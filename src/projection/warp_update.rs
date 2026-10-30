//! Bounded, output-local update builds. Completion is not installation.
//!
//! Retains the r3 full-quality table evaluator and sequential-output row
//! splitting. The control reader and build completion wake the Wayland poll;
//! neither reads files nor waits on stdin in the render thread.
use super::{
    blend::{
        pack_dynamic_shape, pixel_transfer, with_marker, with_packed_marker, Coverage, SliceSpec,
        SlicerSpec,
    },
    control::{ControlEvent, ControlEventKind, ControlUpdate, CONTROL_VERSION},
    warp::Warp,
};
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    sync::{mpsc, Arc, Mutex},
    time::Instant,
};

#[derive(Clone, Debug, PartialEq)]
struct Key {
    slice: SliceSpec,
    gamma: f64,
    lift: f64,
    layout: Option<super::layout::LocalKey>,
    coverage: Vec<crate::model::Rect>,
    dynamic: bool,
    /// Whether this generation's table carries the seam-boundary marker
    /// tags. Held here, not in [`super::layout::LocalKey`], because the
    /// marker POSITIONS depend on nothing `LocalKey` does not already
    /// record (`Evaluator::row_bands` reads the same tables `raw_weight`
    /// does, and only while the layout is blending, which is exactly when
    /// `LocalKey` records the neighbor rectangles). All that is left is the
    /// toggle itself, and it invalidates every output's table.
    ///
    /// The marker WIDTHS also depend on the output size
    /// (`Evaluator::marker_widths`). That needs no field here: a
    /// [`Controller`]'s sizes are fixed for its lifetime, and a presenter
    /// whose size changes fails the install check and restarts the slicer.
    /// The source rectangle they scale from is in `slice` and `layout`.
    markers: bool,
}

pub struct Output {
    pub index: usize,
    pub name: String,
    /// The presenter's configured (logical, as-viewed) size: what the spec
    /// was validated against and what the install check compares.
    pub size: (u32, u32),
    /// The raster `table`, `dynamic_table` and `warp` are built on: `size`
    /// through the slice's output transform, which the GPU presenter owns.
    pub buffer_size: (u32, u32),
    /// The warp the transfer table was built through. A presenter that owns
    /// a non-normal transform always has one, even when the shader takes
    /// the exact copy; `sampling` says which path the shader takes.
    pub warp: Option<Warp>,
    /// How the shader samples this output; see [`sampling_path`].
    pub sampling: SamplingPath,
    pub table: Vec<(u16, u8)>,
    /// Tagged dynamic shape entries. This is populated only for adaptive
    /// transfer; fixed mode retains the exact `(a, b)` table above.
    pub dynamic_table: Option<Vec<u32>>,
    pub slice: crate::model::Rect,
    key: Key,
}

impl Output {
    /// Identity crop/scale uses a [`Warp`] as its sampler too, but only an
    /// explicit nonidentity destination geometry needs GPU Warp capability.
    pub(crate) fn has_geometric_correction(&self) -> bool {
        self.key.slice.geometry.as_ref().is_some_and(|geometry| {
            geometry
                .warp(self.size.0, self.size.1)
                .map_or(true, |warp| warp.is_some())
        })
    }
}

pub struct Prepared {
    pub generation: u64,
    /// Accepted settings for this generation, including adaptive controller
    /// configuration even when no geometry table had to be rebuilt.
    pub spec: SlicerSpec,
    pub outputs: Vec<Output>,
    pub build_ms: f64,
}

struct Request {
    generation: u64,
    spec: SlicerSpec,
    keys: Vec<Key>,
}

#[derive(Default)]
struct Inbox {
    newest: Option<ControlUpdate>,
    failure: Option<(u64, String)>,
    closed: bool,
    restart_required: bool,
}

pub struct Controller {
    spec: SlicerSpec,
    sizes: Vec<(u32, u32)>,
    installed: Vec<Option<Key>>,
    installed_generation: u64,
    requested_generation: u64,
    pending: Option<Request>,
    building: Option<mpsc::Receiver<Result<Prepared, String>>>,
    building_generation: u64,
    inbox: Arc<Mutex<Inbox>>,
    wake: UnixStream,
    signal: Arc<UnixStream>,
    workers: usize,
    closed_reported: bool,
    restart_required: bool,
    /// `None` writes lifecycle lines directly with `println!`, which every
    /// existing unit test still relies on (this is off the render thread in
    /// tests, so blocking/panicking there is not the hazard `StdoutWriter`
    /// exists for). Production sets this once, right after `new`, from
    /// `state.stdout` — see `initialize_warp_control`.
    stdout: Option<super::control::StdoutWriter>,
}

fn signal(stream: &UnixStream) {
    // A full socket already supplies a wakeup. The bounded mailbox is truth.
    let _ = (&*stream).write(&[1]);
}

impl Controller {
    pub fn new(spec: &SlicerSpec, sizes: Vec<(u32, u32)>, workers: usize) -> Result<Self, String> {
        let (wake, signal) = UnixStream::pair().map_err(|e| e.to_string())?;
        wake.set_nonblocking(true).map_err(|e| e.to_string())?;
        signal.set_nonblocking(true).map_err(|e| e.to_string())?;
        let keys = validate(spec, &sizes)?;
        let mut controller = Self {
            spec: spec.clone(),
            sizes,
            installed: vec![None; keys.len()],
            installed_generation: 0,
            requested_generation: 0,
            pending: Some(Request {
                generation: 0,
                spec: spec.clone(),
                keys,
            }),
            building: None,
            building_generation: 0,
            inbox: Arc::new(Mutex::new(Inbox::default())),
            wake,
            signal: Arc::new(signal),
            workers: workers.clamp(1, 8),
            closed_reported: false,
            restart_required: false,
            stdout: None,
        };
        // Initial data is built off the render thread, like every later edit.
        controller.start_build();
        Ok(controller)
    }

    pub fn read_stdin(&self) -> std::io::Result<()> {
        self.read_from(std::io::BufReader::new(std::io::stdin()))
    }

    fn read_from<R: Read + Send + 'static>(&self, mut input: R) -> std::io::Result<()> {
        let inbox = self.inbox.clone();
        let wake = self.signal.clone();
        let session = self.session().to_string();
        std::thread::Builder::new()
            .name("slicer-control".into())
            .spawn(move || {
                let mut newest_generation = 0;
                loop {
                    // Metadata is checked before coalescing: stale or foreign
                    // input must not evict a newer valid pending snapshot.
                    match super::control::read_bounded_json::<_, ControlUpdate>(
                        &mut input,
                        super::control::MAX_CONTROL_LINE_BYTES,
                    ) {
                        Ok(Some(update)) => {
                            let reason = if update.version != CONTROL_VERSION {
                                Some("control version mismatch; restart required")
                            } else if update.session != session {
                                Some("control session mismatch")
                            } else if update.generation <= newest_generation {
                                Some("generation must increase")
                            } else {
                                None
                            };
                            let mut inbox = inbox.lock().unwrap();
                            if update.session == session {
                                newest_generation = newest_generation.max(update.generation);
                            }
                            if let Some(reason) = reason {
                                inbox.restart_required |=
                                    update.session == session && update.version != CONTROL_VERSION;
                                inbox.failure = Some((update.generation, reason.into()));
                            } else {
                                newest_generation = update.generation;
                                inbox.newest = Some(update);
                            }
                        }
                        Ok(None) => {
                            inbox.lock().unwrap().closed = true;
                            signal(&wake);
                            break;
                        }
                        Err(error) => {
                            let terminal = error.kind() != std::io::ErrorKind::InvalidData;
                            let mut inbox = inbox.lock().unwrap();
                            inbox.failure = Some((newest_generation, error.to_string()));
                            if terminal {
                                inbox.closed = true;
                                signal(&wake);
                                break;
                            }
                        }
                    }
                    signal(&wake);
                }
            })
            .map(|_| ())
    }

    pub fn requires_restart(&self) -> bool {
        self.restart_required
    }

    pub fn initialized(&self) -> bool {
        self.installed.iter().all(Option::is_some)
    }

    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }
    pub fn session(&self) -> &str {
        &self.spec.control_session
    }

    /// Route this controller's lifecycle lines through a writer thread
    /// instead of `println!`ing them directly on whichever thread calls
    /// `event`/`poll` — the render thread in production. Additive: leaving
    /// this unset keeps the direct `println!` every existing test exercises.
    pub fn set_stdout(&mut self, stdout: super::control::StdoutWriter) {
        self.stdout = Some(stdout);
    }

    pub fn event(&self, generation: u64, kind: ControlEventKind) {
        let event = ControlEvent {
            version: CONTROL_VERSION,
            session: self.session().into(),
            generation,
            kind,
        };
        if let Ok(line) = serde_json::to_string(&event) {
            // Lifecycle events (`Accepted`, `Built`, `Applied`, `Rejected`,
            // `Closed`, `Submitted`, `Capability`) must not be dropped, so
            // this is the ordered, guaranteed-delivery side of
            // `StdoutWriter`, not the coalescing one.
            match &self.stdout {
                Some(stdout) => stdout.event(line),
                None => println!("{line}"),
            }
        }
    }

    pub fn reject(&self, generation: u64, reason: String) {
        self.event(generation, ControlEventKind::Rejected { reason });
    }

    /// Returns a completed intermediate even when newer input exists. The
    /// caller installs or rejects it before starting another build, so a
    /// pending revert compares against the state actually installed.
    pub fn poll(&mut self) -> Option<Prepared> {
        let mut bytes = [0; 256];
        while self.wake.read(&mut bytes).is_ok_and(|n| n > 0) {}
        let (update, failure, closed, restart_required) = {
            let mut inbox = self.inbox.lock().unwrap();
            (
                inbox.newest.take(),
                inbox.failure.take(),
                inbox.closed,
                inbox.restart_required,
            )
        };
        self.restart_required |= restart_required;
        if let Some((generation, reason)) = failure {
            self.reject(generation, reason);
        }
        if closed && !self.closed_reported {
            self.closed_reported = true;
            self.event(
                self.requested_generation,
                ControlEventKind::Closed { reason: None },
            );
        }
        if let Some(update) = update {
            self.request(update);
        }
        if let Some(rx) = &self.building {
            match rx.try_recv() {
                Ok(Ok(prepared)) => {
                    self.building = None;
                    if prepared.generation < self.installed_generation {
                        self.reject(prepared.generation, "stale build".into());
                    } else {
                        self.event(
                            prepared.generation,
                            ControlEventKind::Built {
                                outputs: prepared.outputs.iter().map(|o| o.name.clone()).collect(),
                                build_ms: Some(prepared.build_ms),
                            },
                        );
                        return Some(prepared);
                    }
                }
                Ok(Err(reason)) => {
                    self.building = None;
                    self.reject(self.building_generation, reason);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.building = None;
                    self.reject(self.building_generation, "table worker terminated".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.start_build();
        None
    }

    fn request(&mut self, update: ControlUpdate) {
        let selection = std::env::var("SUEDE_TEST_OUTPUTS").ok();
        self.request_with_selection(update, selection.as_deref());
    }

    fn request_with_selection(&mut self, mut update: ControlUpdate, selection: Option<&str>) {
        // The testbed selects presenters, while each incoming layout still
        // carries the full configured blend-participant roster.
        if let Some(names) = selection {
            match super::slicer::select_test_outputs(&update.spec, names) {
                Ok(spec) => update.spec = spec,
                Err(error) => {
                    self.reject(update.generation, error.to_string());
                    return;
                }
            }
        }
        let reason = if update.version != CONTROL_VERSION {
            Some("control version mismatch; restart required".to_string())
        } else if update.session != self.session() {
            Some("control session mismatch".into())
        } else if update.generation <= self.requested_generation {
            Some("generation must increase".into())
        } else if !same_topology(&self.spec, &update.spec) {
            Some("topology changed; restart required".into())
        } else {
            None
        };
        if update.session == self.session() {
            self.requested_generation = self.requested_generation.max(update.generation);
        }
        if let Some(reason) = reason {
            // A version or topology mismatch cannot be resolved by a later
            // update in this session: the child was built for a different
            // spec. Flag it so the slicer exits and the daemon respawns it,
            // rather than rejecting every edit until something else restarts
            // it.
            self.restart_required |= update.session == self.session()
                && (update.version != CONTROL_VERSION || reason.starts_with("topology changed"));
            self.reject(update.generation, reason);
            return;
        }
        // Rejected generations cannot later be replayed with a different body.
        self.requested_generation = update.generation;
        match validate(&update.spec, &self.sizes) {
            Ok(keys) => {
                self.event(update.generation, ControlEventKind::Accepted);
                self.pending = Some(Request {
                    generation: update.generation,
                    spec: update.spec,
                    keys,
                });
            }
            Err(reason) => self.reject(update.generation, reason),
        }
    }

    fn start_build(&mut self) {
        if self.building.is_some() {
            return;
        }
        let Some(request) = self.pending.take() else {
            return;
        };
        let indices: Vec<_> = request
            .keys
            .iter()
            .enumerate()
            .filter_map(|(i, key)| (self.installed[i].as_ref() != Some(key)).then_some(i))
            .collect();
        let sizes = self.sizes.clone();
        let workers = self.workers;
        let wake = self.signal.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        self.building_generation = request.generation;
        self.building = Some(rx);
        let result = std::thread::Builder::new()
            .name("warp-build".into())
            .spawn(move || {
                let result = build(request, &sizes, &indices, workers);
                let _ = tx.send(result);
                signal(&wake);
            });
        if let Err(error) = result {
            self.building = None;
            self.reject(self.building_generation, error.to_string());
        }
    }

    pub fn installed(&mut self, prepared: &Prepared) {
        assert!(prepared.generation >= self.installed_generation);
        for output in &prepared.outputs {
            self.installed[output.index] = Some(output.key.clone());
        }
        self.installed_generation = prepared.generation;
        self.start_build();
    }
}

/// Whether two specs describe the same child-lifetime topology, so the
/// difference between them can be rebuilt live instead of needing a restart.
///
/// `highlight_overlaps` is deliberately absent: it changes only the contents
/// of the per-pixel tables, which is exactly what a live update rebuilds, so
/// toggling the lineup aid must never restart the slicer.
pub fn same_topology(a: &SlicerSpec, b: &SlicerSpec) -> bool {
    a.source == b.source
        && a.canvas_width == b.canvas_width
        && a.canvas_height == b.canvas_height
        && a.pattern == b.pattern
        && (a.pattern != Some(crate::model::TestPattern::Gamma)
            || a.gamma.to_bits() == b.gamma.to_bits())
        && a.free_run == b.free_run
        && a.renderer == b.renderer
        && a.slices.len() == b.slices.len()
        && a.slices.iter().zip(&b.slices).all(|(sa, sb)| {
            sa.output == sb.output
                && sa.transform == sb.transform
                && (if a.layout.is_some() && b.layout.is_some() && a.pattern.is_none() {
                    sa.slice.width == sb.slice.width && sa.slice.height == sb.slice.height
                } else {
                    sa.slice == sb.slice && sa.source_rect == sb.source_rect
                })
        })
        && a.layout
            .as_ref()
            .map(|l| l.participants.iter().map(|p| &p.output).collect::<Vec<_>>())
            == b.layout
                .as_ref()
                .map(|l| l.participants.iter().map(|p| &p.output).collect::<Vec<_>>())
}

fn dynamic_mode(spec: &SlicerSpec) -> bool {
    spec.adaptive_lift.is_some() && spec.pattern.is_none()
}

/// Validate startup resources before allocating presenter images or tables.
pub(crate) fn validate_initial(spec: &SlicerSpec, sizes: &[(u32, u32)]) -> Result<(), String> {
    validate(spec, sizes).map(|_| ())
}

fn validate(spec: &SlicerSpec, sizes: &[(u32, u32)]) -> Result<Vec<Key>, String> {
    // The GPU descriptor pool supports eight stable output slots.
    if sizes.len() > 8 {
        return Err("at most eight outputs are supported".into());
    }
    if sizes.len() != spec.slices.len() {
        return Err("output count mismatch".into());
    }
    if spec.canvas_width <= 0
        || spec.canvas_height <= 0
        || !spec.gamma.is_finite()
        || spec.gamma <= 0.0
        || !spec.black_lift.is_finite()
        || !(0.0..=0.5).contains(&spec.black_lift)
    {
        return Err("invalid canvas, gamma or fixed lift".into());
    }
    if let Some(adaptive) = spec.adaptive_lift {
        adaptive.validate()?;
        if adaptive.level.to_bits() != spec.black_lift.to_bits() {
            return Err("adaptive level must match the fixed black-lift fallback".into());
        }
    }
    let mut names = std::collections::HashSet::new();
    let layout = spec
        .layout
        .as_ref()
        .map(|l| super::layout::Evaluator::new(l, spec.canvas_width, spec.canvas_height))
        .transpose()?;
    if dynamic_mode(spec) {
        let maximum = layout.as_ref().map_or_else(
            || Coverage::new(coverage_rects(spec)).max(),
            super::layout::Evaluator::maximum,
        );
        if maximum > 8 {
            return Err("dynamic transfer supports at most eight overlapping footprints".into());
        }
    }
    let coverage = coverage_rects(spec);
    if coverage.iter().any(|r| {
        r.width <= 0
            || r.height <= 0
            || r.x.checked_add(r.width).is_none()
            || r.y.checked_add(r.height).is_none()
    }) {
        return Err("invalid configured coverage rectangle".into());
    }
    spec.slices
        .iter()
        .zip(sizes)
        .map(|(slice, &(w, h))| {
            if !names.insert(&slice.output) {
                return Err("duplicate output".into());
            }
            if w == 0 || h == 0 || u64::from(w) * u64::from(h) > 33_554_432 {
                return Err("outputs must be nonzero and at most 32 megapixels".into());
            }
            if slice.slice.width <= 0
                || slice.slice.height <= 0
                || slice.slice.x.checked_add(slice.slice.width).is_none()
                || slice.slice.y.checked_add(slice.slice.height).is_none()
            {
                return Err("invalid source size".into());
            }
            let mut slice = slice.clone();
            if let Some(geometry) = &slice.geometry {
                if slice.source_rect.is_none()
                    && (w != slice.slice.width as u32 || h != slice.slice.height as u32)
                {
                    return Err("warp requires unit source density".into());
                }
                if geometry.warp(w, h)?.is_none() {
                    slice.geometry = None;
                }
            }
            if slice.source_rect.is_some()
                && (w != slice.slice.width as u32 || h != slice.slice.height as u32)
            {
                return Err("configured output raster differs from presenter dimensions".into());
            }
            sampling_warp(spec, &slice, (w, h), slice.transform)?;
            Ok(Key {
                layout: layout
                    .as_ref()
                    .map(|l| {
                        l.index(&slice.output)
                            .map(|i| l.key(i, spec.black_lift, dynamic_mode(spec)))
                    })
                    .transpose()?,
                coverage: if spec.layout.is_none() && (spec.black_lift > 0.0 || dynamic_mode(spec))
                {
                    coverage.clone()
                } else {
                    Vec::new()
                },
                slice,
                gamma: spec.gamma,
                lift: if dynamic_mode(spec) {
                    0.0
                } else {
                    spec.black_lift
                },
                dynamic: dynamic_mode(spec),
                markers: spec.highlight_overlaps,
            })
        })
        .collect()
}

pub(crate) fn coverage_rects(spec: &SlicerSpec) -> Vec<crate::model::Rect> {
    if spec.coverage_rects.is_empty() {
        spec.slices.iter().map(|s| s.slice).collect()
    } else {
        spec.coverage_rects.clone()
    }
}

/// Shared physical maximum for the dynamic shader uniform. The value is
/// derived from configured footprints, including disconnected outputs.
pub(crate) fn maximum_coverage(spec: &SlicerSpec) -> u32 {
    spec.layout.as_ref().map_or_else(
        || Coverage::new(coverage_rects(spec)).max(),
        |layout| {
            super::layout::Evaluator::new(layout, spec.canvas_width, spec.canvas_height)
                .map(|e| e.maximum())
                .unwrap_or(0)
        },
    )
}

/// The warp the transfer table for `slice` is built through, on a presenter
/// whose configured (logical) size is `size` and which applies `transform`
/// itself (`Normal` when it does not own one). `None` only for an exact
/// integer copy without a transform; otherwise the geometry warp or an
/// identity one, carrying the source rectangle.
///
/// A non-normal `transform` always yields a warp built on the physical
/// raster (`Warp::for_output`), identity pins included, because the table
/// walk ([`fill_generic`]) can only fill a physical raster through one. That
/// is the table's warp, not necessarily the shader's: an identity-pinned,
/// unit-density, integer-origin slice still takes the exact copy, which
/// permutes texels by the transform instead of sampling — see
/// [`exact_copy`], which decides that separately. Its source rectangle is
/// the slice's, or, without one, the unit-density `[x, y, logical width,
/// logical height]` the copy path and `Warp::canvas_at`'s fallback both
/// mean. Every caller in the controller passes `slice.transform`: the
/// controller only runs on the GPU renderer, which owns every output
/// transform.
pub(crate) fn sampling_warp(
    spec: &SlicerSpec,
    slice: &SliceSpec,
    size: (u32, u32),
    transform: crate::model::Transform,
) -> Result<Option<Warp>, String> {
    if !transform.is_normal() {
        let (corners, center) = slice.geometry.as_ref().map_or_else(
            || {
                let (w, h) = (f64::from(size.0), f64::from(size.1));
                ([[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]], [0.5, 0.5])
            },
            |g| (g.corners, g.center),
        );
        let source = slice.source_rect.unwrap_or([
            f64::from(slice.slice.x),
            f64::from(slice.slice.y),
            f64::from(size.0),
            f64::from(size.1),
        ]);
        return Warp::for_output(corners, center, size.0, size.1, transform)?
            .with_source_rect(source)
            .map(Some);
    }
    let warp = slice
        .geometry
        .as_ref()
        .map(|g| g.warp(size.0, size.1))
        .transpose()?
        .flatten();
    let Some(source) = slice.source_rect else {
        return Ok(warp);
    };
    if warp.is_none()
        && source[0] == f64::from(slice.slice.x)
        && source[1] == f64::from(slice.slice.y)
        && super::layout::exact_source(source, size, (spec.canvas_width, spec.canvas_height))
    {
        return Ok(None);
    }
    Ok(Some(
        warp.unwrap_or_else(|| Warp::identity(size.0, size.1))
            .with_source_rect(source)?,
    ))
}

/// How the GPU shader samples one output: the integer copy, which permutes
/// texels by the output transform the presenter owns (`Normal` when it owns
/// none), or bilinear sampling through the output's warp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamplingPath {
    Exact(crate::model::Transform),
    Bilinear,
}

impl SamplingPath {
    /// What the control status reports for this path.
    pub fn mode(self) -> crate::model::SamplingMode {
        match self {
            Self::Exact(_) => crate::model::SamplingMode::Exact,
            Self::Bilinear => crate::model::SamplingMode::Bilinear,
        }
    }
}

/// [`exact_copy`] as a [`SamplingPath`].
pub(crate) fn sampling_path(
    spec: &SlicerSpec,
    slice: &SliceSpec,
    size: (u32, u32),
    transform: crate::model::Transform,
) -> SamplingPath {
    exact_copy(spec, slice, size, transform).map_or(SamplingPath::Bilinear, SamplingPath::Exact)
}

/// Whether the shader may draw `slice` as an integer copy on a presenter
/// whose configured (logical) size is `size` and which applies `transform`
/// itself, and if so the transform the copy permutes texels by.
///
/// Separate from [`sampling_warp`], which keeps building the table's warp:
/// this only decides how the shader samples. It is `Some(transform)` when
/// there is no geometric correction (no geometry, or identity pins and a
/// neutral center on the logical raster) and the source is a unit-density,
/// integer-origin rectangle at the slice's own origin inside the canvas
/// ([`super::layout::exact_source`] on the logical raster). Under those
/// conditions the transform-only table warp maps each physical pixel center
/// to the canvas texel center at `origin + Transform::logical_index(p)`,
/// which is what the shader's exact branch fetches, so table and copy agree.
///
/// For `Transform::Normal` this is exactly the `None` case of
/// [`sampling_warp`], including the slice-origin copy with no source
/// rectangle at all. A non-normal transform without a source rectangle
/// checks its implied unit-density one, so a slice reaching past the canvas
/// edge keeps the warp path it always had.
pub(crate) fn exact_copy(
    spec: &SlicerSpec,
    slice: &SliceSpec,
    size: (u32, u32),
    transform: crate::model::Transform,
) -> Option<crate::model::Transform> {
    let identity = slice
        .geometry
        .as_ref()
        .is_none_or(|g| matches!(g.warp(size.0, size.1), Ok(None)));
    if !identity {
        return None;
    }
    let source = match slice.source_rect {
        Some(source) => source,
        None if transform.is_normal() => return Some(transform),
        None => [
            f64::from(slice.slice.x),
            f64::from(slice.slice.y),
            f64::from(size.0),
            f64::from(size.1),
        ],
    };
    (source[0] == f64::from(slice.slice.x)
        && source[1] == f64::from(slice.slice.y)
        && super::layout::exact_source(source, size, (spec.canvas_width, spec.canvas_height)))
    .then_some(transform)
}

/// Shared row evaluator for scoped builds and the measured pool candidate,
/// and the one place the inverse map, bounds clamp and canvas-bounds test
/// live: [`fill_rows`] and [`fill_dynamic_rows`] were near-duplicates of this
/// same walk, differing only in the empty sentinel and the final per-pixel
/// expression, which is exactly the shape that lets fixed and adaptive modes
/// quietly disagree at a border if one copy is edited and the other is not.
///
/// `layout` is `Some` for every canvas plan the daemon derives, legacy
/// integer layouts included (see `blend::SlicerSpec::layout`'s doc): it is
/// the sole source of blend weight, through [`super::layout::Evaluator`],
/// which also resolves black-lift coverage from the same configured
/// footprints. `None` is only a hand-built spec with no configured layout at
/// all; it still gets `coverage`'s black-lift (physical rectangle coverage
/// alone has no seam-weight rule to fall back to, so every covering source
/// there gets full weight — see [`pixel_transfer`]).
#[allow(clippy::too_many_arguments)]
fn fill_generic<T: Copy>(
    spec: &SlicerSpec,
    slice: &SliceSpec,
    warp: Option<&Warp>,
    size: (u32, u32),
    first_row: usize,
    dest: &mut [T],
    empty: T,
    mut value_of: impl FnMut(f64, f64, f64) -> T,
) {
    let width = size.0;
    // Without a warp each pixel is a unit-density integer copy of the
    // logical raster. `sampling_warp` gives every presenter that owns an
    // output transform a warp, so a physical raster never gets here without
    // one. The shader may still take the exact copy for such a presenter
    // (`exact_copy`); the table is built through the warp either way.
    for (offset, value) in dest.iter_mut().enumerate() {
        let x = (offset % width as usize) as u32;
        let y = (first_row + offset / width as usize) as u32;
        *value = empty;
        let edge = warp.map_or(1.0, |w| w.coverage(x, y));
        if edge == 0.0 {
            continue;
        }
        let Some([cx, cy]) = warp.map_or(
            Some([
                slice.slice.x as f64 + x as f64 + 0.5,
                slice.slice.y as f64 + y as f64 + 0.5,
            ]),
            |w| {
                w.clamped_canvas_at(
                    x as f64 + 0.5,
                    y as f64 + 0.5,
                    [slice.slice.x as f64, slice.slice.y as f64],
                )
            },
        ) else {
            continue;
        };
        // Match the shader's canvas texel-center bounds, including fractional
        // source rectangles. Sync colors use this same transfer table.
        if cx < 0.5
            || cy < 0.5
            || cx > spec.canvas_width as f64 - 0.5
            || cy > spec.canvas_height as f64 - 0.5
        {
            continue;
        }
        *value = value_of(cx, cy, edge);
    }
}

/// [`fill_generic`] for the seam-marker tables only: the same walk, whose
/// `value_of` also gets the pixel center's canvas position BEFORE the
/// source-border texel-center clamp. Markers must be placed from that
/// position. An output pixel can be much smaller than a canvas pixel (about
/// a quarter on brain), and then a blue line, which lies against the
/// source's own edge and is two output pixels wide, falls entirely inside
/// the half-texel border that the clamp moves every sample out of.
///
/// Kept separate so the ordinary tables keep `fill_generic` exactly as it
/// was. `highlight_overlaps_only_adds_tags_and_is_a_no_op_when_off` and
/// `brains_wall_carries_both_marker_colors_through_the_real_pipeline` hold
/// the two walks to the same coverage and transfer values.
#[allow(clippy::too_many_arguments)]
fn fill_marked<T: Copy>(
    spec: &SlicerSpec,
    slice: &SliceSpec,
    warp: Option<&Warp>,
    size: (u32, u32),
    first_row: usize,
    dest: &mut [T],
    empty: T,
    mut value_of: impl FnMut([f64; 2], [f64; 2], f64) -> T,
) {
    let width = size.0;
    let origin = [slice.slice.x as f64, slice.slice.y as f64];
    for (offset, value) in dest.iter_mut().enumerate() {
        let x = (offset % width as usize) as u32;
        let y = (first_row + offset / width as usize) as u32;
        *value = empty;
        let edge = warp.map_or(1.0, |w| w.coverage(x, y));
        if edge == 0.0 {
            continue;
        }
        let center = [x as f64 + 0.5, y as f64 + 0.5];
        let Some((clamped, raw)) = warp.map_or(
            Some((
                [origin[0] + center[0], origin[1] + center[1]],
                [origin[0] + center[0], origin[1] + center[1]],
            )),
            |w| {
                Some((
                    w.clamped_canvas_at(center[0], center[1], origin)?,
                    w.canvas_at(center[0], center[1], origin)?,
                ))
            },
        ) else {
            continue;
        };
        let [cx, cy] = clamped;
        if cx < 0.5
            || cy < 0.5
            || cx > spec.canvas_width as f64 - 0.5
            || cy > spec.canvas_height as f64 - 0.5
        {
            continue;
        }
        *value = value_of(clamped, raw, edge);
    }
}

/// The logical (pre-transform) raster a table of `size` belongs to: a warp
/// knows it, and without one there is no transform, so it is `size`.
fn logical_size(warp: Option<&Warp>, size: (u32, u32)) -> (u32, u32) {
    warp.map_or(size, |w| {
        let [width, height] = w.logical_size();
        (width, height)
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_rows(
    spec: &SlicerSpec,
    slice: &SliceSpec,
    warp: Option<&Warp>,
    coverage: &Coverage,
    layout: Option<(&super::layout::Evaluator, usize)>,
    size: (u32, u32),
    first_row: usize,
    dest: &mut [(u16, u8)],
) {
    // The marker variant is chosen once per table, so the ordinary closure
    // below carries no per-pixel test for it.
    if let Some((layout, index)) = layout.filter(|_| spec.highlight_overlaps) {
        let widths = layout.marker_widths(index, logical_size(warp, size));
        fill_marked(
            spec,
            slice,
            warp,
            size,
            first_row,
            dest,
            (0, 0),
            |[cx, cy], [rx, ry], edge| {
                with_marker(
                    layout.transfer(index, cx, cy, spec.gamma, spec.black_lift, edge),
                    layout.marker_at(index, rx, ry, widths),
                )
            },
        );
        return;
    }
    fill_generic(
        spec,
        slice,
        warp,
        size,
        first_row,
        dest,
        (0, 0),
        |cx, cy, edge| {
            if let Some((layout, index)) = layout {
                layout.transfer(index, cx, cy, spec.gamma, spec.black_lift, edge)
            } else {
                pixel_transfer(coverage.lift(spec.black_lift, cx, cy), edge)
            }
        },
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_dynamic_rows(
    spec: &SlicerSpec,
    slice: &SliceSpec,
    warp: Option<&Warp>,
    coverage: &Coverage,
    layout: Option<(&super::layout::Evaluator, usize)>,
    size: (u32, u32),
    first_row: usize,
    dest: &mut [u32],
) {
    // Chosen once per table, as in `fill_rows`.
    if let Some((layout, index)) = layout.filter(|_| spec.highlight_overlaps) {
        let widths = layout.marker_widths(index, logical_size(warp, size));
        fill_marked(
            spec,
            slice,
            warp,
            size,
            first_row,
            dest,
            0,
            |[cx, cy], [rx, ry], edge| {
                with_packed_marker(
                    layout.dynamic_shape(index, cx, cy, spec.gamma, edge),
                    u32::from(layout.marker_at(index, rx, ry, widths)),
                )
            },
        );
        return;
    }
    fill_generic(
        spec,
        slice,
        warp,
        size,
        first_row,
        dest,
        0,
        |cx, cy, edge| {
            if let Some((layout, index)) = layout {
                layout.dynamic_shape(index, cx, cy, spec.gamma, edge)
            } else {
                pack_dynamic_shape(1.0, edge, coverage.at(cx, cy))
            }
        },
    );
}

fn build(
    request: Request,
    sizes: &[(u32, u32)],
    indices: &[usize],
    workers: usize,
) -> Result<Prepared, String> {
    let started = Instant::now();
    let spec = &request.spec;
    let coverage = Coverage::new(coverage_rects(spec));
    let layout = spec
        .layout
        .as_ref()
        .map(|l| super::layout::Evaluator::new(l, spec.canvas_width, spec.canvas_height))
        .transpose()?;
    let mut outputs = Vec::with_capacity(indices.len());
    for &index in indices {
        let slice = &request.keys[index].slice;
        let logical = sizes[index];
        let warp = sampling_warp(spec, slice, logical, slice.transform)?;
        let sampling = sampling_path(spec, slice, logical, slice.transform);
        // Tables are built on the raster the presenter attaches: physical
        // when it owns an axis-swapping transform.
        let (width, height) = slice.transform.physical_size(logical);
        debug_assert!(slice.transform.is_normal() || warp.is_some());
        debug_assert!(warp
            .as_ref()
            .is_none_or(|w| w.output_size() == [width, height]));
        let layout = layout
            .as_ref()
            .map(|l| l.index(&slice.output).map(|i| (l, i)))
            .transpose()?;
        let dynamic = request.keys[index].dynamic;
        let mut table = Vec::new();
        let mut dynamic_table = None;
        if dynamic {
            let mut values = Vec::new();
            values
                .try_reserve_exact(width as usize * height as usize)
                .map_err(|e| e.to_string())?;
            values.resize(width as usize * height as usize, 0);
            let rows = (height as usize).div_ceil(workers);
            std::thread::scope(|scope| {
                for (chunk, dest) in values.chunks_mut(rows * width as usize).enumerate() {
                    let warp = &warp;
                    let coverage = &coverage;
                    scope.spawn(move || {
                        fill_dynamic_rows(
                            spec,
                            slice,
                            warp.as_ref(),
                            coverage,
                            layout,
                            (width, height),
                            chunk * rows,
                            dest,
                        );
                    });
                }
            });
            dynamic_table = Some(values);
        } else {
            table
                .try_reserve_exact(width as usize * height as usize)
                .map_err(|e| e.to_string())?;
            table.resize(width as usize * height as usize, (0, 0));
        }
        if !dynamic {
            let rows = (height as usize).div_ceil(workers);
            std::thread::scope(|scope| {
                for (chunk, dest) in table.chunks_mut(rows * width as usize).enumerate() {
                    let warp = &warp;
                    let coverage = &coverage;
                    scope.spawn(move || {
                        fill_rows(
                            spec,
                            slice,
                            warp.as_ref(),
                            coverage,
                            layout,
                            (width, height),
                            chunk * rows,
                            dest,
                        );
                    });
                }
            });
        }
        outputs.push(Output {
            slice: slice.slice,
            index,
            name: slice.output.clone(),
            size: logical,
            buffer_size: (width, height),
            warp,
            sampling,
            table,
            dynamic_table,
            key: request.keys[index].clone(),
        });
    }
    Ok(Prepared {
        generation: request.generation,
        spec: request.spec.clone(),
        outputs,
        build_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{AdaptiveBlackLift, Rect, Renderer},
        projection::warp::Geometry,
    };
    use std::time::Duration;

    fn fixture() -> SlicerSpec {
        SlicerSpec {
            shared_input: None,
            layout: None,
            coverage_rects: Vec::new(),
            highlight_overlaps: false,
            control_session: "test-session".into(),
            source: "canvas".into(),
            canvas_width: 12,
            canvas_height: 8,
            gamma: 2.2,
            black_lift: 0.1,
            adaptive_lift: None,
            pattern: None,
            free_run: false,
            renderer: Renderer::Gpu,
            slices: ["A", "B"]
                .into_iter()
                .enumerate()
                .map(|(i, name)| SliceSpec {
                    source_rect: None,
                    output: name.into(),
                    slice: Rect {
                        x: i as i32 * 4,
                        y: 0,
                        width: 8,
                        height: 8,
                    },
                    geometry: None,
                    transform: crate::model::Transform::Normal,
                })
                .collect(),
        }
    }
    fn moved(mut spec: SlicerSpec, index: usize) -> SlicerSpec {
        spec.slices[index].geometry = Some(Geometry {
            corners: [[1.0, 1.0], [7.0, 0.0], [8.0, 7.0], [0.0, 8.0]],
            center: [0.5, 0.5],
        });
        spec
    }
    fn complete(controller: &mut Controller) -> Prepared {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(prepared) = controller.poll() {
                return prepared;
            }
            assert!(Instant::now() < deadline, "worker did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn a_rotated_output_builds_physical_tables_through_a_transform_warp() {
        use crate::model::Transform;
        let mut spec = fixture();
        spec.canvas_width = 1920;
        spec.canvas_height = 1080;
        spec.slices.truncate(1);
        spec.slices[0].slice = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let mut normal = Controller::new(&spec, vec![(1920, 1080)], 4).unwrap();
        let normal = complete(&mut normal);
        spec.slices[0].transform = Transform::Rotate90;
        let mut c = Controller::new(&spec, vec![(1920, 1080)], 4).unwrap();
        let prepared = complete(&mut c);
        let output = &prepared.outputs[0];
        assert_eq!(output.size, (1920, 1080));
        assert_eq!(output.buffer_size, (1080, 1920));
        assert_eq!(output.table.len(), 1080 * 1920);
        // Identity pins are not geometric correction, but the rotated
        // raster still takes the warp path.
        assert!(!output.has_geometric_correction());
        let warp = output.warp.as_ref().expect("rotated outputs always warp");
        assert_eq!(warp.output_size(), [1080, 1920]);
        assert_eq!(warp.logical_size(), [1920, 1080]);
        assert_eq!(warp.transform(), Transform::Rotate90);
        assert_eq!(warp.source_rect(), Some([0.0, 0.0, 1920.0, 1080.0]));
        // The physical table is the logical one, moved by `to_physical`.
        assert!(normal.outputs[0].warp.is_none());
        for (lx, ly) in [
            (0, 0),
            (1919, 0),
            (0, 1079),
            (1919, 1079),
            (960, 540),
            (7, 3),
        ] {
            let [px, py] = Transform::Rotate90
                .to_physical([lx as f64 + 0.5, ly as f64 + 0.5], (1920.0, 1080.0));
            let physical = (py - 0.5) as usize * 1080 + (px - 0.5) as usize;
            assert_eq!(
                output.table[physical],
                normal.outputs[0].table[ly * 1920 + lx],
                "logical pixel ({lx}, {ly})"
            );
        }
        // The same holds for pins: the geometry is logical, the warp physical.
        spec.slices[0].geometry = Some(Geometry {
            corners: [[10.0, 0.0], [1920.0, 20.0], [1900.0, 1080.0], [0.0, 1060.0]],
            center: [0.5, 0.5],
        });
        let mut c = Controller::new(&spec, vec![(1920, 1080)], 4).unwrap();
        let prepared = complete(&mut c);
        let output = &prepared.outputs[0];
        assert!(output.has_geometric_correction());
        assert_eq!(output.buffer_size, (1080, 1920));
        let warp = output.warp.as_ref().unwrap();
        assert_eq!(warp.output_size(), [1080, 1920]);
        let tl = warp.destination_at(0.0, 0.0).unwrap();
        let expected = Transform::Rotate90.to_physical([10.0, 0.0], (1920.0, 1080.0));
        assert!((tl[0] - expected[0]).abs() < 1e-6 && (tl[1] - expected[1]).abs() < 1e-6);
    }

    fn initialized() -> Controller {
        let mut c = Controller::new(&fixture(), vec![(8, 8); 2], 4).unwrap();
        let initial = complete(&mut c);
        c.installed(&initial);
        c
    }
    fn send(c: &mut Controller, generation: u64, spec: SlicerSpec) {
        c.request(ControlUpdate::new(c.session().into(), generation, spec));
    }

    fn public_layout_fixture() -> SlicerSpec {
        let mut spec = fixture();
        spec.canvas_width = 14;
        spec.canvas_height = 8;
        spec.slices = (0..3)
            .map(|i| SliceSpec {
                output: format!("P{i}"),
                slice: Rect {
                    x: i * 4,
                    y: 0,
                    width: 6,
                    height: 8,
                },
                source_rect: Some([i as f64 * 4.0, 0.0, 6.0, 8.0]),
                geometry: None,
                transform: crate::model::Transform::Normal,
            })
            .collect();
        spec.layout = Some(super::super::layout::LayoutSpec {
            aspect: 14.0 / 8.0,
            blend: true,
            participants: spec
                .slices
                .iter()
                .map(|s| {
                    let r = crate::model::CanvasRect {
                        x: s.slice.x as f64 / 14.0,
                        y: 0.0,
                        width: 6.0 / 14.0,
                        height: 8.0 / 14.0,
                    };
                    super::super::layout::LayoutParticipant {
                        output: s.output.clone(),
                        slice: r,
                        raster_footprint: r,
                    }
                })
                .collect(),
        });
        spec
    }

    #[test]
    fn test_output_selection_retains_full_layout_through_live_control() {
        let full = public_layout_fixture();
        let selected = super::super::slicer::select_test_outputs(&full, "P1").unwrap();
        assert_eq!(selected.layout, full.layout);
        assert_eq!(selected.layout.as_ref().unwrap().participants.len(), 3);
        assert_eq!((selected.canvas_width, selected.canvas_height), (14, 8));
        let mut controller = Controller::new(&selected, vec![(6, 8)], 2).unwrap();
        let initial = complete(&mut controller);
        controller.installed(&initial);
        let mut update = full.clone();
        update.gamma = 2.4;
        controller.request_with_selection(
            ControlUpdate::new(controller.session().into(), 1, update.clone()),
            Some("P1"),
        );
        let prepared = complete(&mut controller);
        assert_eq!(prepared.spec.layout, update.layout);
        assert_eq!(prepared.spec.canvas_width, full.canvas_width);
        assert_eq!(prepared.outputs.len(), 1);
        assert_eq!(prepared.outputs[0].name, "P1");
        assert_eq!(prepared.outputs[0].index, 0);
        assert_eq!(prepared.spec.slices[0], update.slices[1]);
        assert_eq!(prepared.spec.gamma, 2.4);
        controller.installed(&prepared);
        assert!(!controller.requires_restart());
    }

    #[test]
    fn explicit_source_origin_is_authoritative_even_with_stale_integer_metadata() {
        let mut spec = fixture();
        spec.slices[0].source_rect = Some([4.0, 0.0, 8.0, 8.0]);
        let mapping = sampling_warp(
            &spec,
            &spec.slices[0],
            (8, 8),
            crate::model::Transform::Normal,
        )
        .unwrap()
        .unwrap();
        assert_eq!(mapping.canvas_at(0.5, 0.5, [0.0, 0.0]), Some([4.5, 0.5]));
        spec.slices[0].slice.x = 4;
        assert!(sampling_warp(
            &spec,
            &spec.slices[0],
            (8, 8),
            crate::model::Transform::Normal
        )
        .unwrap()
        .is_none());
    }

    const ALL_TRANSFORMS: [crate::model::Transform; 8] = {
        use crate::model::Transform::*;
        [
            Normal, Rotate90, Rotate180, Rotate270, Flipped, Flipped90, Flipped180, Flipped270,
        ]
    };

    /// Slice A of [`fixture`] narrowed to a 6×8 logical raster at (2, 0) so
    /// the axis-swapping transforms see a non-square raster.
    fn exact_fixture() -> (SlicerSpec, SliceSpec, (u32, u32)) {
        let spec = fixture();
        let mut slice = spec.slices[0].clone();
        slice.slice = Rect {
            x: 2,
            y: 0,
            width: 6,
            height: 8,
        };
        (spec, slice, (6, 8))
    }

    #[test]
    fn exact_copy_holds_for_every_transform_at_unit_density() {
        let (spec, slice, size) = exact_fixture();
        let identity = Geometry {
            corners: [[0.0, 0.0], [6.0, 0.0], [6.0, 8.0], [0.0, 8.0]],
            center: [0.5, 0.5],
        };
        for transform in ALL_TRANSFORMS {
            for geometry in [None, Some(identity.clone())] {
                for source_rect in [None, Some([2.0, 0.0, 6.0, 8.0])] {
                    let mut slice = slice.clone();
                    slice.geometry = geometry.clone();
                    slice.source_rect = source_rect;
                    slice.transform = transform;
                    assert_eq!(
                        exact_copy(&spec, &slice, size, transform),
                        Some(transform),
                        "{transform:?} {geometry:?} {source_rect:?}"
                    );
                    assert_eq!(
                        sampling_path(&spec, &slice, size, transform),
                        SamplingPath::Exact(transform)
                    );
                    // The table keeps its warp whenever a transform is owned.
                    assert_eq!(
                        sampling_warp(&spec, &slice, size, transform)
                            .unwrap()
                            .is_some(),
                        !transform.is_normal(),
                        "{transform:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn exact_copy_refuses_anything_but_an_integer_unit_density_copy() {
        let (spec, slice, size) = exact_fixture();
        let with = |edit: &dyn Fn(&mut SliceSpec)| {
            let mut slice = slice.clone();
            edit(&mut slice);
            slice
        };
        let cases: [(&str, SliceSpec); 7] = [
            (
                "fractional origin",
                with(&|s| s.source_rect = Some([2.5, 0.0, 6.0, 8.0])),
            ),
            (
                "non-unit size",
                with(&|s| s.source_rect = Some([2.0, 0.0, 5.0, 8.0])),
            ),
            (
                "origin other than the slice's",
                with(&|s| s.source_rect = Some([3.0, 0.0, 6.0, 8.0])),
            ),
            (
                "non-identity pins",
                with(&|s| {
                    s.geometry = Some(Geometry {
                        corners: [[0.5, 0.0], [6.0, 0.0], [6.0, 8.0], [0.0, 8.0]],
                        center: [0.5, 0.5],
                    })
                }),
            ),
            (
                "off-center remap",
                with(&|s| {
                    s.geometry = Some(Geometry {
                        corners: [[0.0, 0.0], [6.0, 0.0], [6.0, 8.0], [0.0, 8.0]],
                        center: [0.4, 0.5],
                    })
                }),
            ),
            (
                "source rect outside the canvas",
                with(&|s| {
                    s.slice.x = 8;
                    s.source_rect = Some([8.0, 0.0, 6.0, 8.0]);
                }),
            ),
            (
                "negative source origin",
                with(&|s| {
                    s.slice.x = -1;
                    s.source_rect = Some([-1.0, 0.0, 6.0, 8.0]);
                }),
            ),
        ];
        for transform in ALL_TRANSFORMS {
            for (name, slice) in &cases {
                assert_eq!(
                    exact_copy(&spec, slice, size, transform),
                    None,
                    "{transform:?}: {name}"
                );
                assert_eq!(
                    sampling_path(&spec, slice, size, transform),
                    SamplingPath::Bilinear,
                    "{transform:?}: {name}"
                );
            }
        }
        // Without a source rectangle, an owned transform checks the implied
        // unit-density one, so a slice past the canvas edge keeps its warp
        // path. `Normal` keeps today's unconditional slice-origin copy.
        let past_edge = with(&|s| s.slice.x = 8);
        for transform in ALL_TRANSFORMS {
            assert_eq!(
                exact_copy(&spec, &past_edge, size, transform).is_some(),
                transform.is_normal(),
                "{transform:?}"
            );
        }
    }

    /// For `Normal`, the shader's exact decision is exactly the table's
    /// "no warp" decision, over every slice the other tests here build.
    #[test]
    fn exact_copy_agrees_with_sampling_warp_for_normal_on_every_fixture() {
        let (exact_spec, exact_slice, exact_size) = exact_fixture();
        let mut cases: Vec<(SlicerSpec, SliceSpec, (u32, u32))> = Vec::new();
        for spec in [fixture(), moved(fixture(), 0), moved(fixture(), 1)] {
            for slice in spec.slices.clone() {
                cases.push((spec.clone(), slice, (8, 8)));
            }
        }
        let public = public_layout_fixture();
        for slice in public.slices.clone() {
            cases.push((public.clone(), slice, (6, 8)));
        }
        for source_rect in [
            None,
            Some([2.0, 0.0, 6.0, 8.0]),
            Some([2.5, 0.0, 6.0, 8.0]),
            Some([2.0, 0.0, 5.0, 8.0]),
            Some([3.0, 0.0, 6.0, 8.0]),
            Some([-0.25, 0.0, 6.0, 8.0]),
            Some([8.0, 0.0, 6.0, 8.0]),
        ] {
            let mut slice = exact_slice.clone();
            slice.source_rect = source_rect;
            cases.push((exact_spec.clone(), slice, exact_size));
        }
        let mut explicit = fixture();
        explicit.slices[0].source_rect = Some([4.0, 0.0, 8.0, 8.0]);
        cases.push((explicit.clone(), explicit.slices[0].clone(), (8, 8)));
        explicit.slices[0].slice.x = 4;
        cases.push((explicit.clone(), explicit.slices[0].clone(), (8, 8)));
        let normal = crate::model::Transform::Normal;
        let mut exact = 0;
        for (spec, slice, size) in &cases {
            let warp = sampling_warp(spec, slice, *size, normal).unwrap();
            let copy = exact_copy(spec, slice, *size, normal);
            assert_eq!(copy.is_some(), warp.is_none(), "{slice:?}");
            exact += usize::from(copy.is_some());
        }
        // Both outcomes are exercised.
        assert!(exact > 0 && exact < cases.len());
    }

    /// The table walk and the shader's exact branch must name the same
    /// canvas texel for every physical pixel: the transform-only table warp
    /// maps physical pixel `p`'s center to the texel center at
    /// `origin + Transform::logical_index(p)`, the texel the shader fetches.
    #[test]
    fn transform_table_warp_maps_each_physical_pixel_to_its_logical_index() {
        let (spec, slice, size) = exact_fixture();
        let origin = [f64::from(slice.slice.x), f64::from(slice.slice.y)];
        for transform in ALL_TRANSFORMS {
            assert!(exact_copy(&spec, &slice, size, transform).is_some());
            let warp = sampling_warp(&spec, &slice, size, transform).unwrap();
            let warp = warp.unwrap_or_else(|| {
                Warp::identity(size.0, size.1)
                    .with_source_rect([origin[0], origin[1], 6.0, 8.0])
                    .unwrap()
            });
            let (pw, ph) = transform.physical_size(size);
            assert_eq!(warp.output_size(), [pw, ph]);
            for py in 0..ph {
                for px in 0..pw {
                    let [lx, ly] = transform.logical_index([px, py], size);
                    let [cx, cy] = warp
                        .clamped_canvas_at(f64::from(px) + 0.5, f64::from(py) + 0.5, origin)
                        .unwrap();
                    let expected = [
                        origin[0] + f64::from(lx) + 0.5,
                        origin[1] + f64::from(ly) + 0.5,
                    ];
                    assert!(
                        (cx - expected[0]).abs() < 1e-9 && (cy - expected[1]).abs() < 1e-9,
                        "{transform:?} physical ({px}, {py}): table samples ({cx}, {cy}), \
                         exact copy fetches {expected:?}"
                    );
                    assert_eq!(warp.coverage(px, py), 1.0, "{transform:?} ({px}, {py})");
                }
            }
        }
    }

    #[test]
    fn built_outputs_carry_the_shader_path_separately_from_the_table_warp() {
        let mut spec = fixture();
        spec.black_lift = 0.0;
        spec.slices[0].transform = crate::model::Transform::Rotate180;
        spec.slices[1].transform = crate::model::Transform::Rotate180;
        spec.slices[1].source_rect = Some([4.5, 0.0, 8.0, 8.0]);
        let sizes = [(8, 8); 2];
        let prepared = build(
            Request {
                generation: 1,
                keys: validate(&spec, &sizes).unwrap(),
                spec,
            },
            &sizes,
            &[0, 1],
            1,
        )
        .unwrap();
        let exact = &prepared.outputs[0];
        assert!(exact.warp.is_some());
        assert_eq!(
            exact.sampling,
            SamplingPath::Exact(crate::model::Transform::Rotate180)
        );
        assert_eq!(exact.sampling.mode(), crate::model::SamplingMode::Exact);
        let crop = &prepared.outputs[1];
        assert!(crop.warp.is_some());
        assert_eq!(crop.sampling, SamplingPath::Bilinear);
        assert_eq!(crop.sampling.mode(), crate::model::SamplingMode::Bilinear);
    }

    #[test]
    fn fractional_canvas_edge_has_zero_transfer_for_content_and_sync() {
        let mut spec = fixture();
        spec.black_lift = 0.0;
        spec.slices[0].source_rect = Some([-0.25, 0.0, 8.0, 8.0]);
        let prepared = build(
            Request {
                generation: 1,
                keys: validate(&spec, &[(8, 8); 2]).unwrap(),
                spec,
            },
            &[(8, 8); 2],
            &[0],
            1,
        )
        .unwrap();
        assert_eq!(prepared.outputs[0].table[0], (0, 0));
        assert_ne!(prepared.outputs[0].table[1], (0, 0));
    }

    #[test]
    fn public_pin_edits_remain_sparse_and_fractional_sources_match_full_build() {
        let spec = public_layout_fixture();
        let sizes = vec![(6, 8); 3];
        let mut c = Controller::new(&spec, sizes.clone(), 2).unwrap();
        let initial = complete(&mut c);
        assert!(initial.outputs.iter().all(|o| o.warp.is_none()));
        c.installed(&initial);
        let mut pin = spec.clone();
        pin.slices[1].geometry = Some(Geometry {
            corners: [[0.5, 0.5], [6.0, 0.0], [6.0, 8.0], [0.0, 8.0]],
            center: [0.5, 0.5],
        });
        assert!(same_topology(&spec, &pin));
        send(&mut c, 1, pin.clone());
        let built = complete(&mut c);
        assert_eq!(
            built.outputs.iter().map(|o| o.index).collect::<Vec<_>>(),
            vec![1]
        );
        c.installed(&built);
        let mut source = pin.clone();
        source.slices[0].source_rect.as_mut().unwrap()[2] = 6.25;
        source.layout.as_mut().unwrap().participants[0].slice.width = 6.25 / 14.0;
        assert!(same_topology(&pin, &source));
        send(&mut c, 2, source.clone());
        let changed = complete(&mut c);
        assert_eq!(
            changed.outputs.iter().map(|o| o.index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(changed.outputs[0].warp.is_some());
        let full = build(
            Request {
                generation: 2,
                keys: validate(&source, &sizes).unwrap(),
                spec: source.clone(),
            },
            &sizes,
            &[0, 1, 2],
            1,
        )
        .unwrap();
        for o in &changed.outputs {
            assert_eq!(o.table, full.outputs[o.index].table);
        }
        assert_eq!(initial.outputs[2].table, full.outputs[2].table);
        c.installed(&changed);
        // A footprint edit that raises N invalidates every lift consumer,
        // including P2, whose source does not overlap P0.
        source.layout.as_mut().unwrap().participants[0]
            .raster_footprint
            .width = 1.0;
        send(&mut c, 3, source);
        let shared = complete(&mut c);
        assert_eq!(
            shared.outputs.iter().map(|o| o.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    /// Two 24-pixel strips overlapping by 8 on a 40x8 canvas, each shown at
    /// 24x8 so one output pixel is one canvas pixel and a marker line is two
    /// canvas pixels. The overlap is wide enough for each of its ends to get
    /// a distinct line; under two line widths the two lines of one ramp
    /// overpaint and orange wins (see `Evaluator::marker_at`).
    fn two_strips() -> SlicerSpec {
        let mut spec = public_layout_fixture();
        spec.canvas_width = 40;
        spec.slices.truncate(2);
        for (i, slice) in spec.slices.iter_mut().enumerate() {
            slice.slice = Rect {
                x: i as i32 * 16,
                y: 0,
                width: 24,
                height: 8,
            };
            slice.source_rect = Some([i as f64 * 16.0, 0.0, 24.0, 8.0]);
        }
        let layout = spec.layout.as_mut().unwrap();
        layout.aspect = 40.0 / 8.0;
        layout.participants.truncate(2);
        for (i, p) in layout.participants.iter_mut().enumerate() {
            let r = crate::model::CanvasRect {
                x: i as f64 * 16.0 / 40.0,
                y: 0.0,
                width: 24.0 / 40.0,
                height: 8.0 / 40.0,
            };
            p.slice = r;
            p.raster_footprint = r;
        }
        spec
    }

    /// The lineup aid is a pure addition, verified rather than asserted by
    /// inspection: with it off no table entry carries a tag at all, and with
    /// it on the tables differ from the off tables ONLY in the tag bits — in
    /// both the fixed `(a, b)` and the packed dynamic representation. Both
    /// colors really appear, and toggling it is a live table rebuild, never
    /// a topology change.
    #[test]
    fn highlight_overlaps_only_adds_tags_and_is_a_no_op_when_off() {
        use crate::projection::blend::{
            gain_of, marker_of_gain, marker_of_packed, MARKER_BLUE, MARKER_ORANGE,
        };
        let off = two_strips();
        let sizes = vec![(24, 8); 2];
        let build_all = |spec: &SlicerSpec| {
            build(
                Request {
                    generation: 0,
                    keys: validate(spec, &sizes).unwrap(),
                    spec: spec.clone(),
                },
                &sizes,
                &[0, 1],
                1,
            )
            .unwrap()
        };
        let mut on = off.clone();
        on.highlight_overlaps = true;
        assert!(same_topology(&off, &on));
        assert_ne!(
            validate(&off, &sizes).unwrap(),
            validate(&on, &sizes).unwrap()
        );

        let (a, b) = (build_all(&off), build_all(&on));
        let mut seen = [false; 3];
        for (o, t) in a.outputs.iter().zip(&b.outputs) {
            assert!(o.table.iter().all(|&(g, _)| marker_of_gain(g) == 0));
            let stripped: Vec<_> = t.table.iter().map(|&(g, l)| (gain_of(g), l)).collect();
            assert_eq!(o.table, stripped, "output {}", o.name);
            for &(g, _) in &t.table {
                seen[usize::from(marker_of_gain(g))] = true;
            }
        }
        assert!(seen[usize::from(MARKER_ORANGE)] && seen[usize::from(MARKER_BLUE)]);

        let mut dynamic_off = off.clone();
        dynamic_off.black_lift = 0.1;
        dynamic_off.adaptive_lift = Some(crate::model::AdaptiveBlackLift {
            level: 0.1,
            dark_threshold: 0.02,
            bright_threshold: 0.2,
            rise_ms: 1_000.0,
            fall_ms: 250.0,
            slew_per_second: 0.1,
        });
        let mut dynamic_on = dynamic_off.clone();
        dynamic_on.highlight_overlaps = true;
        let (a, b) = (build_all(&dynamic_off), build_all(&dynamic_on));
        let mut tagged = 0;
        for (o, t) in a.outputs.iter().zip(&b.outputs) {
            let (o, t) = (
                o.dynamic_table.as_ref().unwrap(),
                t.dynamic_table.as_ref().unwrap(),
            );
            assert!(o.iter().all(|&v| marker_of_packed(v) == 0));
            let stripped: Vec<_> = t.iter().map(|&v| v & !(0x3 << 28)).collect();
            assert_eq!(o, &stripped);
            tagged += t.iter().filter(|&&v| marker_of_packed(v) != 0).count();
        }
        assert!(tagged > 0);
    }

    /// Both renderers take their marker colors from `marker_palette` when a
    /// spec is installed: a gamma change reaches them while the markers are
    /// on, and changes nothing while they are off.
    #[test]
    fn a_gamma_change_reaches_the_marker_palette_only_while_markers_are_on() {
        use crate::projection::blend::MarkerPalette;
        let mut spec = two_strips();
        spec.gamma = 2.2;
        assert_eq!(spec.marker_palette(), None);
        spec.gamma = 2.4;
        assert_eq!(spec.marker_palette(), None);
        spec.highlight_overlaps = true;
        assert_eq!(spec.marker_palette(), Some(MarkerPalette::for_gamma(2.4)));
        assert_eq!(spec.marker_palette().unwrap().green(), 191);
        spec.gamma = 1.8;
        assert_eq!(spec.marker_palette().unwrap().green(), 174);
    }

    /// Brain's 2x2 wall, end to end: the real canvas plan with corner-pin
    /// correction, the real table build and the GPU packing. Every output
    /// must carry both colors. Each output pixel here is about a quarter of
    /// a canvas pixel, so a blue line (half a canvas pixel at the source's
    /// own edge) lies entirely within the half-texel border that sampling
    /// clamps positions into; the markers must be placed from the unclamped
    /// position or blue disappears.
    #[test]
    fn brains_wall_carries_both_marker_colors_through_the_real_pipeline() {
        use crate::model::{CanvasConfig, CanvasRect, Mode, OutputConfig, OutputGeometry};
        use crate::projection::blend::{marker_of_packed, MARKER_BLUE, MARKER_NONE, MARKER_ORANGE};
        let canvas = CanvasConfig {
            aspect: 1.64,
            render_width: 900,
        };
        let (w, h) = (0.52770, 0.29672);
        let outputs = [
            (
                "DP-1",
                0.0,
                0.04303,
                [[0.2551, 0.0707], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            ),
            (
                "DP-2",
                0.47230,
                0.04303,
                [[0.0, 0.0], [0.97, 0.03], [1.0, 1.0], [0.0, 1.0]],
            ),
            (
                "DP-3",
                0.0,
                0.27001,
                [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.02, 0.96]],
            ),
            (
                "DP-4",
                0.47230,
                0.27001,
                [[0.0, 0.0], [1.0, 0.0], [0.98, 0.97], [0.0, 1.0]],
            ),
        ];
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        };
        let mut desired = crate::model::DesiredState::new();
        let mut projection = crate::model::ProjectionConfig {
            mode: crate::model::ProjectionMode::Simple,
            canvas: Some(canvas),
            blend: true,
            ..Default::default()
        };
        projection.temporary.highlight_overlaps = true;
        desired.projection = Some(projection);
        let mut observed = Vec::new();
        for (name, x, y, corners) in outputs {
            let mut config = OutputConfig::new(crate::model::OutputMatch::by_name(name));
            config.mode = Some(mode);
            let source = CanvasRect {
                x,
                y,
                width: w,
                height: h,
            };
            config.geometry = Some(OutputGeometry {
                slice: source,
                corners,
                center: [0.5, 0.5],
                raster_footprint: source,
            });
            desired.outputs.push(config);
            observed.push(crate::model::Output {
                name: name.to_string(),
                active: true,
                make: None,
                model: None,
                serial: None,
                current_mode: Some(mode),
                modes: vec![mode],
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                scale: None,
                transform: None,
                adaptive_sync_status: None,
            });
        }
        let plan =
            super::super::layout::canvas_plan_with_correction(&desired, &observed, true).unwrap();
        let mut spec = two_strips();
        spec.canvas_width = plan.canvas_width;
        spec.canvas_height = plan.canvas_height;
        spec.layout = plan.layout;
        spec.coverage_rects = plan.coverage_rects;
        spec.slices = plan.slices;
        spec.highlight_overlaps = true;
        assert!(spec.slices.iter().all(|s| s.geometry.is_some()));
        let sizes = vec![(1920, 1080); 4];
        let build_all = |spec: &SlicerSpec| {
            build(
                Request {
                    generation: 0,
                    keys: validate(spec, &sizes).unwrap(),
                    spec: spec.clone(),
                },
                &sizes,
                &[0, 1, 2, 3],
                4,
            )
            .unwrap()
        };
        let built = build_all(&spec);
        let mut off = spec.clone();
        off.highlight_overlaps = false;
        let plain = build_all(&off);
        for (output, plain) in built.outputs.iter().zip(&plain.outputs) {
            // The marker walk samples exactly what the ordinary one does.
            let stripped: Vec<_> = output
                .table
                .iter()
                .map(|&(g, l)| (crate::projection::blend::gain_of(g), l))
                .collect();
            assert!(stripped == plain.table, "{}", output.name);
            let mut counts = [0usize; 3];
            for &(a, b) in &output.table {
                let tag = marker_of_packed(crate::projection::gpu::pack_transfer(a, b));
                counts[usize::from(tag)] += 1;
            }
            assert!(counts[usize::from(MARKER_NONE)] > 0);
            assert!(
                counts[usize::from(MARKER_ORANGE)] > 0 && counts[usize::from(MARKER_BLUE)] > 0,
                "{}: orange {} blue {}",
                output.name,
                counts[usize::from(MARKER_ORANGE)],
                counts[usize::from(MARKER_BLUE)]
            );
        }
    }

    /// A marker line is two pixels of the output's own raster, so an output
    /// showing its source at half the canvas resolution gets lines two
    /// canvas pixels wider, not the same two canvas pixels. Each strip is
    /// 24 canvas pixels shown on a 12x4 output. Output 0's overlap is output
    /// columns 8..12: orange at 8 and 9, blue at 10 and 11 (canvas 16..20
    /// and 20..24). A two-canvas-pixel line would cover one column each.
    #[test]
    fn marker_lines_are_two_output_pixels_wide_at_any_scale() {
        use crate::projection::blend::{marker_of_gain, MARKER_BLUE, MARKER_NONE, MARKER_ORANGE};
        let mut spec = two_strips();
        spec.highlight_overlaps = true;
        for (i, slice) in spec.slices.iter_mut().enumerate() {
            slice.slice = Rect {
                x: i as i32 * 8,
                y: 0,
                width: 12,
                height: 4,
            };
        }
        let sizes = vec![(12, 4); 2];
        let built = build(
            Request {
                generation: 0,
                keys: validate(&spec, &sizes).unwrap(),
                spec: spec.clone(),
            },
            &sizes,
            &[0, 1],
            1,
        )
        .unwrap();
        let table = &built.outputs[0].table;
        for row in 0..4 {
            let tags: Vec<u8> = table[row * 12..(row + 1) * 12]
                .iter()
                .map(|&(g, _)| marker_of_gain(g))
                .collect();
            let (n, o, b) = (MARKER_NONE, MARKER_ORANGE, MARKER_BLUE);
            assert_eq!(tags, [n, n, n, n, n, n, n, n, o, o, b, b], "row {row}");
        }
    }

    #[test]
    fn absent_presenters_still_shape_weights_and_black_lift() {
        let all = public_layout_fixture();
        let sizes = vec![(6, 8); 3];
        let full = build(
            Request {
                generation: 0,
                keys: validate(&all, &sizes).unwrap(),
                spec: all.clone(),
            },
            &sizes,
            &[0, 1, 2],
            1,
        )
        .unwrap();
        let mut sparse = all.clone();
        sparse.slices.remove(1);
        let subset = build(
            Request {
                generation: 0,
                keys: validate(&sparse, &[(6, 8); 2]).unwrap(),
                spec: sparse,
            },
            &[(6, 8); 2],
            &[0, 1],
            1,
        )
        .unwrap();
        assert_eq!(full.outputs[0].table, subset.outputs[0].table);
        assert_eq!(full.outputs[2].table, subset.outputs[1].table);
        // A disconnected physical participant keeps N even in simple mode.
        let mut simple = fixture();
        simple.coverage_rects = vec![simple.slices[0].slice, simple.slices[1].slice];
        simple.slices.pop();
        assert_eq!(Coverage::new(coverage_rects(&simple)).max(), 2);
    }

    #[test]
    fn sparse_build_matches_full_oracle_and_preserves_neighbor() {
        let mut c = initialized();
        let initial = c.installed.clone();
        let spec = moved(fixture(), 1);
        send(&mut c, 1, spec.clone());
        let sparse = complete(&mut c);
        assert_eq!(sparse.outputs.len(), 1);
        assert_eq!(sparse.outputs[0].index, 1);
        assert_eq!(sparse.outputs[0].name, "B");
        let keys = validate(&spec, &c.sizes).unwrap();
        let all = build(
            Request {
                generation: 1,
                spec,
                keys,
            },
            &c.sizes,
            &[0, 1],
            1,
        )
        .unwrap();
        assert_eq!(sparse.outputs[0].table, all.outputs[1].table);
        assert_eq!(c.installed, initial, "completion cannot publish keys");
        c.installed(&sparse);
        assert_eq!(c.installed[0], initial[0]);
        assert_ne!(c.installed[1], initial[1]);
        send(&mut c, 2, moved(moved(fixture(), 1), 0));
        let switched = complete(&mut c);
        assert_eq!(
            switched.outputs.iter().map(|o| o.index).collect::<Vec<_>>(),
            vec![0]
        );
    }

    #[test]
    fn sustained_input_installs_intermediate_then_newest_revert() {
        let mut c = initialized();
        let initial = c.installed.clone();
        send(&mut c, 1, moved(fixture(), 1));
        c.start_build();
        for generation in 2..100 {
            send(&mut c, generation, moved(fixture(), 0));
        }
        send(&mut c, 100, fixture());
        let intermediate = complete(&mut c);
        assert_eq!(
            intermediate.generation, 1,
            "new input must not starve a completed build"
        );
        c.installed(&intermediate);
        let latest = complete(&mut c);
        assert_eq!(latest.generation, 100);
        assert_eq!(latest.outputs.len(), 1);
        assert_eq!(
            latest.outputs[0].index, 1,
            "revert must diff against installed intermediate"
        );
        assert!(latest.outputs[0].warp.is_none());
        c.installed(&latest);
        assert_eq!(c.installed, initial);
    }

    #[test]
    fn normalized_identity_is_noop_and_shared_transfer_invalidates_all() {
        let mut c = initialized();
        let mut identity = fixture();
        identity.slices[1].geometry = Some(Geometry {
            corners: [[-0.0, 0.0], [8.0, 0.0], [8.0, 8.0], [0.0, 8.0]],
            center: [0.5, 0.5],
        });
        send(&mut c, 1, identity);
        let noop = complete(&mut c);
        assert!(noop.outputs.is_empty());
        c.installed(&noop);
        let mut shared = fixture();
        shared.gamma = 2.4;
        send(&mut c, 2, shared.clone());
        let all = complete(&mut c);
        assert_eq!(all.outputs.len(), 2);
        c.installed(&all);
        // A per-slice-only, live-updatable field: `same_topology` never
        // compares `geometry` (a destination pin edit is exactly the "live
        // GPU edit" case, never a restart), so this changes slice 0's own
        // key in place without touching slice 1's `SliceSpec` or forcing a
        // restart.
        shared.slices[0].geometry = Some(Geometry {
            corners: [[1.0, 1.0], [7.0, 0.0], [8.0, 7.0], [0.0, 8.0]],
            center: [0.5, 0.5],
        });
        send(&mut c, 3, shared);
        let local = complete(&mut c);
        assert_eq!(local.outputs.len(), 1);
        assert_eq!(local.outputs[0].index, 0);
    }

    #[test]
    fn controller_switches_transfer_modes_and_keeps_parameter_only_updates_sparse() {
        let mut c = initialized();
        let mut adaptive = fixture();
        adaptive.black_lift = 0.2;
        adaptive.adaptive_lift = Some(AdaptiveBlackLift {
            level: 0.2,
            dark_threshold: 0.02,
            bright_threshold: 0.2,
            rise_ms: 1_000.0,
            fall_ms: 250.0,
            slew_per_second: 0.1,
        });
        send(&mut c, 1, adaptive.clone());
        let enabled = complete(&mut c);
        assert_eq!(enabled.spec.adaptive_lift, adaptive.adaptive_lift);
        assert!(enabled
            .outputs
            .iter()
            .all(|output| output.dynamic_table.is_some() && output.table.is_empty()));
        c.installed(&enabled);

        let mut parameters = adaptive.clone();
        parameters.adaptive_lift.as_mut().unwrap().bright_threshold = 0.3;
        send(&mut c, 2, parameters.clone());
        let changed = complete(&mut c);
        assert_eq!(changed.spec.adaptive_lift, parameters.adaptive_lift);
        assert!(changed.outputs.is_empty());
        c.installed(&changed);

        let mut fixed = parameters;
        fixed.adaptive_lift = None;
        fixed.black_lift = 0.1;
        send(&mut c, 3, fixed);
        let disabled = complete(&mut c);
        assert!(disabled.spec.adaptive_lift.is_none());
        assert!(disabled
            .outputs
            .iter()
            .all(|output| output.dynamic_table.is_none() && !output.table.is_empty()));
    }

    #[test]
    fn rejection_preserves_installed_and_valid_inflight_generation() {
        let mut c = initialized();
        let before = c.installed.clone();
        send(&mut c, 1, moved(fixture(), 1));
        c.start_build();
        let mut invalid = moved(fixture(), 0);
        invalid.slices[0].geometry.as_mut().unwrap().center = [0.0, 0.5];
        send(&mut c, 2, invalid);
        let mut resized = fixture();
        resized.canvas_width += 1;
        send(&mut c, 3, resized);
        let mut wrong_session = ControlUpdate::new("old-process".into(), 99, fixture());
        c.request(wrong_session.clone());
        wrong_session.session = c.session().into();
        wrong_session.version = 42;
        c.request(wrong_session);
        assert_eq!(c.installed, before);
        assert!(c.pending.is_none());
        let valid = complete(&mut c);
        assert_eq!(valid.generation, 1);
        // Simulate failed upload by withholding installed(). Next accepted
        // state must rebuild against the original, not this discarded result.
        send(&mut c, 100, moved(fixture(), 1));
        let retry = complete(&mut c);
        assert_eq!(retry.outputs.len(), 1);
        assert_eq!(retry.outputs[0].table, valid.outputs[0].table);
    }
    #[test]
    fn reader_coalesces_burst_without_losing_newest_to_stale_or_foreign_input() {
        let mut c = initialized();
        let mut bytes = Vec::new();
        for generation in 1..=100 {
            let update = ControlUpdate::new(c.session().into(), generation, moved(fixture(), 1));
            bytes.extend(super::super::control::encode_control_update(&update).unwrap());
        }
        for (session, generation) in [(c.session(), 99), ("old-process", 101)] {
            bytes.extend(
                super::super::control::encode_control_update(&ControlUpdate::new(
                    session.into(),
                    generation,
                    fixture(),
                ))
                .unwrap(),
            );
        }
        c.read_from(std::io::Cursor::new(bytes)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !c.inbox.lock().unwrap().closed {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            c.inbox.lock().unwrap().newest.as_ref().unwrap().generation,
            100
        );
        let latest = complete(&mut c);
        assert_eq!(latest.generation, 100);
        c.installed(&latest);
        assert!(c.closed_reported);
        assert!(c.initialized(), "EOF leaves the installed state active");
    }

    #[test]
    fn oversized_work_and_invalid_rectangles_are_rejected_before_build() {
        let mut spec = fixture();
        spec.slices = (0..9)
            .map(|i| {
                let mut slice = fixture().slices[0].clone();
                slice.output = i.to_string();
                slice
            })
            .collect();
        assert!(Controller::new(&spec, vec![(8, 8); 9], 4).is_err());
        let mut spec = fixture();
        spec.slices[0].slice.x = i32::MAX;
        assert!(Controller::new(&spec, vec![(8, 8); 2], 4).is_err());
    }
    #[test]
    fn a_topology_rejection_asks_for_a_restart() {
        let mut c = initialized();
        assert!(!c.requires_restart());
        let mut rotated = fixture();
        rotated.slices[0].transform = crate::model::Transform::Rotate180;
        send(&mut c, 5, rotated);
        assert!(c.pending.is_none());
        assert!(
            c.requires_restart(),
            "a spec the child was not built for cannot become live later"
        );
    }

    #[test]
    fn rejected_same_session_generation_cannot_be_replayed() {
        let mut c = initialized();
        let mut resized = fixture();
        resized.canvas_width += 1;
        send(&mut c, 5, resized);
        send(&mut c, 5, moved(fixture(), 1));
        send(&mut c, 4, moved(fixture(), 0));
        assert!(c.pending.is_none());
        send(&mut c, 6, moved(fixture(), 1));
        assert_eq!(complete(&mut c).generation, 6);
        let mut mismatch = ControlUpdate::new(c.session().into(), 7, fixture());
        mismatch.version = 2;
        c.request(mismatch);
        assert!(c.requires_restart());
        send(&mut c, 7, fixture());
        assert!(c.pending.is_none());
    }

    /// Release-only measurement; each arm can run in a fresh process using
    /// SUEDE_SCHEDULER_ARM=scoped|pool|pool_reused. All compare byte-for-byte
    /// against the production builder, retaining four installed tables.
    #[test]
    #[ignore = "hardware benchmark"]
    fn scheduler_comparison() {
        struct Task {
            spec: Arc<SlicerSpec>,
            coverage: Arc<Coverage>,
            warp: Option<Warp>,
            first: usize,
            table: Vec<(u16, u8)>,
        }
        let (done_tx, done_rx) = mpsc::channel();
        let mut senders = Vec::new();
        let mut threads = Vec::new();
        for _ in 0..4 {
            let (tx, rx) = mpsc::sync_channel::<Task>(1);
            let done = done_tx.clone();
            senders.push(tx);
            threads.push(std::thread::spawn(move || {
                while let Ok(mut task) = rx.recv() {
                    fill_rows(
                        &task.spec,
                        &task.spec.slices[0],
                        task.warp.as_ref(),
                        &task.coverage,
                        None,
                        (1920, 1080),
                        task.first,
                        &mut task.table,
                    );
                    done.send(task).unwrap();
                }
            }));
        }
        let mut spec = fixture();
        spec.canvas_width = 3680;
        spec.canvas_height = 2000;
        spec.slices = (0..4)
            .map(|i| {
                let mut slice = spec.slices[0].clone();
                slice.output = format!("OUT-{i}");
                slice.slice = Rect {
                    x: (i % 2) * 1760,
                    y: (i / 2) * 920,
                    width: 1920,
                    height: 1080,
                };
                slice
            })
            .collect();
        let sizes = [(1920, 1080); 4];
        let request = |spec: &SlicerSpec| Request {
            generation: 1,
            spec: spec.clone(),
            keys: validate(spec, &sizes).unwrap(),
        };
        let installed = build(request(&spec), &sizes, &[0, 1, 2, 3], 4).unwrap();
        let only = std::env::var("SUEDE_SCHEDULER_ARM").ok();
        for arm in ["scoped", "pool", "pool_reused"] {
            if only.as_deref().is_some_and(|wanted| wanted != arm) {
                continue;
            }
            let mut chunks: Vec<Vec<(u16, u8)>> = (0..4).map(|_| Vec::new()).collect();
            let mut assembled = Vec::new();
            let mut times = Vec::new();
            for n in 0..15 {
                let amount = 10.0 + n as f64;
                spec.slices[0].geometry = Some(Geometry {
                    corners: [
                        [amount, amount],
                        [1920.0 - amount, 0.0],
                        [1920.0, 1080.0 - amount],
                        [0.0, 1080.0],
                    ],
                    center: [0.5, 0.5],
                });
                let started = Instant::now();
                if arm == "scoped" {
                    assembled = build(request(&spec), &sizes, &[0], 4)
                        .unwrap()
                        .outputs
                        .remove(0)
                        .table;
                } else {
                    let spec = Arc::new(spec.clone());
                    let coverage = Arc::new(Coverage::new(spec.slices.iter().map(|s| s.slice)));
                    let warp = spec.slices[0]
                        .geometry
                        .as_ref()
                        .unwrap()
                        .warp(1920, 1080)
                        .unwrap();
                    for i in 0..4 {
                        let mut table = if arm == "pool_reused" {
                            std::mem::take(&mut chunks[i])
                        } else {
                            Vec::new()
                        };
                        table.resize(1920 * 270, (0, 0));
                        senders[i]
                            .send(Task {
                                spec: spec.clone(),
                                coverage: coverage.clone(),
                                warp: warp.clone(),
                                first: i * 270,
                                table,
                            })
                            .unwrap();
                    }
                    for _ in 0..4 {
                        let task = done_rx.recv().unwrap();
                        chunks[task.first / 270] = task.table;
                    }
                    if arm != "pool_reused" {
                        assembled = Vec::new();
                    }
                    assembled.clear();
                    assembled.reserve_exact(1920 * 1080);
                    for chunk in &chunks {
                        assembled.extend_from_slice(chunk);
                    }
                }
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                if n >= 3 {
                    times.push(elapsed);
                }
                let oracle = build(request(&spec), &sizes, &[0], 4).unwrap();
                assert_eq!(assembled, oracle.outputs[0].table, "{arm} generation {n}");
            }
            times.sort_by(f64::total_cmp);
            let memory = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            let memory: Vec<_> = memory
                .lines()
                .filter(|l| l.starts_with("VmRSS:") || l.starts_with("VmHWM:"))
                .collect();
            let resident_bytes: usize = installed
                .outputs
                .iter()
                .map(|o| o.table.capacity() * std::mem::size_of::<(u16, u8)>())
                .sum();
            eprintln!(
                "SCHEDULER {}",
                serde_json::json!({"arm":arm,"workers":4,"samples":times,
                "medianMs":times[times.len()/2],"maxMs":times.last(),"installedTableBytes":resident_bytes,
                "candidateChunkBytes":chunks.iter().map(|c|c.capacity()*std::mem::size_of::<(u16,u8)>()).sum::<usize>(),
                "assembledBytes":assembled.capacity()*std::mem::size_of::<(u16,u8)>(),"processMemory":memory})
            );
        }
        drop(senders);
        for worker in threads {
            worker.join().unwrap();
        }
    }

    #[test]
    fn gamma_pattern_gamma_change_requires_restart() {
        let a = fixture();
        let mut b = a.clone();
        b.gamma = 2.4;
        assert!(same_topology(&a, &b));
        b.pattern = Some(crate::model::TestPattern::Gamma);
        assert!(!same_topology(
            &b,
            &SlicerSpec {
                shared_input: None,
                layout: None,
                coverage_rects: Vec::new(),
                gamma: 2.2,
                ..b.clone()
            }
        ));
    }
}
