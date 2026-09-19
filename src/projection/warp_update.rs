//! Bounded, output-local update builds. Completion is not installation.
//!
//! Retains the r3 full-quality table evaluator and sequential-output row
//! splitting. The control reader and build completion wake the Wayland poll;
//! neither reads files nor waits on stdin in the render thread.
use super::{
    blend::{transfer_at, Coverage, SliceSpec, SlicerSpec},
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
}

pub struct Output {
    pub index: usize,
    pub name: String,
    pub size: (u32, u32),
    pub warp: Option<Warp>,
    pub table: Vec<(u16, u8)>,
    key: Key,
}

pub struct Prepared {
    pub generation: u64,
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

    pub fn event(&self, generation: u64, kind: ControlEventKind) {
        let event = ControlEvent {
            version: CONTROL_VERSION,
            session: self.session().into(),
            generation,
            kind,
        };
        if let Ok(line) = serde_json::to_string(&event) {
            println!("{line}");
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
            self.restart_required |=
                update.session == self.session() && update.version != CONTROL_VERSION;
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
        && a.slices
            .iter()
            .zip(&b.slices)
            .all(|(a, b)| a.output == b.output && a.source == b.source)
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
    let mut names = std::collections::HashSet::new();
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
            if slice.source.width <= 0
                || slice.source.height <= 0
                || slice.source.x.checked_add(slice.source.width).is_none()
                || slice.source.y.checked_add(slice.source.height).is_none()
            {
                return Err("invalid source size".into());
            }
            let mut slice = slice.clone();
            if let Some(geometry) = &slice.geometry {
                if w != slice.source.width as u32 || h != slice.source.height as u32 {
                    return Err("warp requires unit source density".into());
                }
                if geometry.warp(w, h)?.is_none() {
                    slice.geometry = None;
                }
            }
            if slice.ramps.len() > 64 {
                return Err("at most 64 ramps per output are supported".into());
            }
            if slice.ramps.iter().any(|r| {
                r.rect.width <= 0
                    || r.rect.height <= 0
                    || r.rect.x.checked_add(r.rect.width).is_none()
                    || r.rect.y.checked_add(r.rect.height).is_none()
            }) {
                return Err("invalid ramp rectangle".into());
            }
            Ok(Key {
                slice,
                gamma: spec.gamma,
                lift: spec.black_lift,
            })
        })
        .collect()
}

/// Shared row evaluator for scoped builds and the measured pool candidate.
fn fill_rows(
    spec: &SlicerSpec,
    slice: &SliceSpec,
    warp: Option<&Warp>,
    coverage: &Coverage,
    size: (u32, u32),
    first_row: usize,
    dest: &mut [(u16, u8)],
) {
    let (width, height) = size;
    for (offset, value) in dest.iter_mut().enumerate() {
        let x = (offset % width as usize) as u32;
        let y = (first_row + offset / width as usize) as u32;
        *value = (0, 0);
        let edge = warp.map_or(1.0, |w| w.coverage(x, y));
        if edge == 0.0 {
            continue;
        }
        let Some([sx, sy]) = warp.map_or(Some([x as f64 + 0.5, y as f64 + 0.5]), |w| {
            w.source_at(x as f64 + 0.5, y as f64 + 0.5)
        }) else {
            continue;
        };
        let sx = sx.clamp(0.5, width as f64 - 0.5);
        let sy = sy.clamp(0.5, height as f64 - 0.5);
        let cx = slice.source.x as f64 + sx;
        let cy = slice.source.y as f64 + sy;
        if cx < 0.0 || cy < 0.0 || cx >= spec.canvas_width as f64 || cy >= spec.canvas_height as f64
        {
            continue;
        }
        *value = transfer_at(
            &slice.ramps,
            spec.gamma,
            coverage.lift(spec.black_lift, cx, cy),
            sx,
            sy,
            edge,
        );
    }
}

fn build(
    request: Request,
    sizes: &[(u32, u32)],
    indices: &[usize],
    workers: usize,
) -> Result<Prepared, String> {
    let started = Instant::now();
    let spec = &request.spec;
    let coverage = Coverage::new(spec.slices.iter().map(|s| s.source));
    let mut outputs = Vec::with_capacity(indices.len());
    for &index in indices {
        let slice = &request.keys[index].slice;
        let (width, height) = sizes[index];
        let warp = slice
            .geometry
            .as_ref()
            .map(|g| g.warp(width, height))
            .transpose()?
            .flatten();
        let mut table = Vec::new();
        table
            .try_reserve_exact(width as usize * height as usize)
            .map_err(|e| e.to_string())?;
        table.resize(width as usize * height as usize, (0, 0));
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
                        (width, height),
                        chunk * rows,
                        dest,
                    );
                });
            }
        });
        outputs.push(Output {
            index,
            name: slice.output.clone(),
            size: (width, height),
            warp,
            table,
            key: request.keys[index].clone(),
        });
    }
    Ok(Prepared {
        generation: request.generation,
        outputs,
        build_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{Rect, Renderer},
        projection::{
            blend::{FadeTo, RampSpec},
            warp::Geometry,
        },
    };
    use std::time::Duration;

    fn fixture() -> SlicerSpec {
        SlicerSpec {
            control_session: "test-session".into(),
            source: "canvas".into(),
            canvas_width: 12,
            canvas_height: 8,
            gamma: 2.2,
            black_lift: 0.1,
            pattern: None,
            free_run: false,
            renderer: Renderer::Gpu,
            slices: ["A", "B"]
                .into_iter()
                .enumerate()
                .map(|(i, name)| SliceSpec {
                    output: name.into(),
                    source: Rect {
                        x: i as i32 * 4,
                        y: 0,
                        width: 8,
                        height: 8,
                    },
                    geometry: None,
                    ramps: vec![],
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
    fn initialized() -> Controller {
        let mut c = Controller::new(&fixture(), vec![(8, 8); 2], 4).unwrap();
        let initial = complete(&mut c);
        c.installed(&initial);
        c
    }
    fn send(c: &mut Controller, generation: u64, spec: SlicerSpec) {
        c.request(ControlUpdate::new(c.session().into(), generation, spec));
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
        shared.slices[0].ramps.push(RampSpec {
            rect: Rect {
                x: 4,
                y: 0,
                width: 4,
                height: 8,
            },
            fade_to: FadeTo::Right,
        });
        send(&mut c, 3, shared);
        let local = complete(&mut c);
        assert_eq!(local.outputs.len(), 1);
        assert_eq!(local.outputs[0].index, 0);
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
        spec.slices[0].ramps = vec![
            RampSpec {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 8,
                    height: 8
                },
                fade_to: FadeTo::Right,
            };
            65
        ];
        assert!(Controller::new(&spec, vec![(8, 8); 2], 4).is_err());
        spec.slices[0].ramps.clear();
        spec.slices[0].source.x = i32::MAX;
        assert!(Controller::new(&spec, vec![(8, 8); 2], 4).is_err());
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
                slice.source = Rect {
                    x: (i % 2) * 1760,
                    y: (i / 2) * 920,
                    width: 1920,
                    height: 1080,
                };
                slice.ramps = vec![RampSpec {
                    rect: Rect {
                        x: 1760,
                        y: 0,
                        width: 160,
                        height: 1080,
                    },
                    fade_to: FadeTo::Right,
                }];
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
                    let coverage = Arc::new(Coverage::new(spec.slices.iter().map(|s| s.source)));
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
                gamma: 2.2,
                ..b.clone()
            }
        ));
    }
}
