//! The slicer: Suede's own compositor for the overlap, run as `suede slice`.
//!
//! Sway cannot blend overlapping outputs — its global coordinate space gives
//! both projectors the same pixels wherever their boxes intersect. So the
//! overlap is managed here instead. The outputs sit edge to edge in sway
//! (nothing overlaps, nothing bleeds); the app renders once into a headless
//! canvas of `Σwidths − (n−1)·overlap`; and this process captures that
//! canvas each frame, cuts it into per-projector slices whose neighbours
//! *repeat* the seam columns, applies the gamma-shaped blend ramps and black
//! lift, and presents each slice fullscreen on its own physical output.
//! Each projector gets its own buffer, so the two sides of a seam can carry
//! opposite fades — the thing the compositor's shared space can never do.
//!
//! One process for the whole installation: capture happens once per frame no matter
//! how many projectors consume it. The frame loop is damage-driven — a
//! static page costs nothing per second.
//!
//! ## Presentation gating
//!
//! Filming two outputs at once with a fast shutter used to show the frame
//! counters off by one much of the time, for two independent reasons. First,
//! outputs on a GPU without genlock hardware have independent vblank phase,
//! fixed at mode-set: a commit that lands between output A's render and
//! output B's render puts frame N on A and N+1 on B. Second, this process
//! never used to learn when an output had actually taken a commit — it just
//! committed the next frame whenever the next capture arrived. Both are
//! addressed by gating commits on `wl_surface.frame` callbacks (see
//! [`Presenter::frame_pending`] and [`State::can_present`]): the next commit
//! goes out only once every output has reported taking the previous one, so
//! it always lands just after the slower output rendered, and both outputs
//! pick up the same frame at their next render. The canvas keeps rendering on
//! its own timer regardless; the slicer presents whatever the newest
//! completed capture is, dropping or repeating frames symmetrically across
//! every output when the clocks beat against each other.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::AsFd;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_output::{self, WlOutput},
    wl_region::WlRegion,
    wl_registry::WlRegistry,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::{self, WpPresentation},
    wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use super::blend::{pixel_transfer, Coverage, OverlaySpec, SlicerSpec};
use crate::model::{FrameCost, OutputTiming, PresentationOffset, ProjectionStats};

/// Consecutive capture failures tolerated before giving up. The daemon
/// respawns the slicer on its next pass, which is the retry policy.
const MAX_FAILURES: u32 = 3;

/// How long an output may leave a `wl_surface.frame` callback unanswered
/// before it is dropped from the gate. Long enough that a normal compositor
/// hiccup or a momentarily busy GPU never trips it; short enough that a
/// DPMS-off or otherwise unrendering output does not freeze the rest of the
/// wall for more than a third of a second.
const STALL_TIMEOUT: Duration = Duration::from_millis(300);

/// How to read one pixel of the captured canvas: bytes per pixel and where
/// red, green, and blue live within them.
///
/// wl_shm names 0 and 1 for ARGB/XRGB; everything else is a DRM fourcc.
/// wlroots offers whatever the renderer holds — the headless output on this
/// hardware arrives as `BG24`, three bytes per pixel, which is exactly why
/// this cannot be assumed to be 32-bit.
#[derive(Clone, Copy)]
struct PixelFormat {
    bytes: usize,
    red: usize,
    green: usize,
    blue: usize,
}

fn pixel_format(raw: u32) -> Option<PixelFormat> {
    let f = |bytes, red, green, blue| {
        Some(PixelFormat {
            bytes,
            red,
            green,
            blue,
        })
    };
    match raw {
        // wl_shm: ARGB8888 / XRGB8888, little-endian bytes B,G,R,A.
        0 | 1 => f(4, 2, 1, 0),
        // DRM 'AB24'/'XB24': ABGR8888/XBGR8888, bytes R,G,B,A.
        0x34324241 | 0x34324258 => f(4, 0, 1, 2),
        // DRM 'BG24': BGR888, bytes R,G,B.
        0x34324742 => f(3, 0, 1, 2),
        // DRM 'RG24': RGB888, bytes B,G,R.
        0x34324752 => f(3, 2, 1, 0),
        _ => None,
    }
}

struct Presenter {
    surface: WlSurface,
    #[allow(dead_code)]
    layer_surface: ZwlrLayerSurfaceV1,
    configured: Option<(u32, u32)>,
    /// Output name, carried on the presenter so stats and stall messages can
    /// name it without threading the spec through every call.
    name: String,
    /// Two buffers, alternated so we never write one the compositor reads.
    buffers: Vec<(WlBuffer, memmap2::MmapMut)>,
    /// Parallel to `buffers`: true while the compositor holds that buffer,
    /// from attach+commit until its `wl_buffer.release`.
    busy: Vec<bool>,
    next_buffer: usize,
    /// Fixed-point per-pixel transfer `(a, b)`: `out = (a·in)>>8 + b`,
    /// row-major at the configured size. Two-dimensional because seams can
    /// run on any edge — a grid corner is the product of two ramps.
    transfer: Vec<(u16, u8)>,
    /// This presenter's region of the canvas.
    source: crate::model::Rect,
    /// A `wl_surface.frame` callback is outstanding: the compositor has not
    /// yet told us this output took the last commit. Presenting again before
    /// this clears is exactly the race that puts frame N on one output and
    /// N+1 on another — see the module comment.
    frame_pending: bool,
    /// When `frame_pending` was set, for the stall timeout.
    pending_since: Option<Instant>,
    /// This output stopped answering its frame callback — most often DPMS
    /// off, or a monitor that was unplugged without sway noticing yet. It is
    /// still sent commits, so it has fresh content the moment it comes back,
    /// but it no longer holds up the other outputs' gate.
    stalled: bool,
    /// The latest snapshot has not been committed to this output yet.
    stale: bool,
}

#[derive(Default)]
struct Capture {
    /// Offered shm layout: (format, width, height, stride).
    offered: Option<(u32, u32, u32, u32)>,
    format: Option<PixelFormat>,
    buffer: Option<(WlBuffer, memmap2::MmapMut, u32, u32, u32)>,
    buffer_done: bool,
    ready: bool,
    failed: bool,
    y_invert: bool,
    first_copy_done: bool,
    /// The finished frame, copied out of the shared buffer so the next
    /// capture can be requested before this one has been drawn.
    ///
    /// The copy is what makes the pipelining safe. Handing the compositor
    /// the same buffer keeps `copy_with_damage` honest — damage is reported
    /// against the previous copy, so alternating between two buffers would
    /// paint fresh damage onto a frame older than it, leaving stale regions.
    /// One full-canvas memcpy costs a millisecond or so and buys back a
    /// frame interval.
    snapshot: Vec<u8>,
    /// Geometry of what is in `snapshot`: (width, height, stride).
    snapshot_geometry: Option<(u32, u32, u32)>,
}

impl Capture {
    /// Take the completed frame out of the shared buffer.
    fn take_snapshot(&mut self) {
        let Some((_, map, width, height, stride)) = self.buffer.as_ref() else {
            return;
        };
        self.snapshot.clear();
        self.snapshot.extend_from_slice(&map[..]);
        self.snapshot_geometry = Some((*width, *height, *stride));
    }
}

/// Where each frame's time went, and what presentation looked like, reported
/// periodically.
///
/// Judder on a video wall is a question about frames per second, and it took
/// an evening of indirect measurement — context-switch counts, CPU time,
/// arithmetic on both — to establish a number the slicer could simply have
/// stated. `waiting` against `blending` also says *which* half to attack: a
/// loop that is mostly waiting is not short of CPU. The presentation figures
/// added alongside it answer the newer question this module exists to fix:
/// not "how fast", but "how together".
struct FrameStats {
    since: Instant,
    /// Canvas frames captured this interval — the denominator for every
    /// `per_frame_ms` figure, and for `canvas_fps`.
    captured: u32,
    waiting: Duration,
    /// Copying the finished canvas out of the shared buffer.
    snapshot: Duration,
    /// The handshake for the next capture.
    requesting: Duration,
    /// Cutting, shading and committing the slices. Accumulated across every
    /// call to `present_frame`, including the extra ones free-run mode makes
    /// when a lagging output catches up between captures — so this is the
    /// total blending cost per canvas frame, not per present cycle.
    blending: Duration,
    /// Present cycles that committed to at least one output: one per
    /// all-output commit when locked, one per snapshot that reached any
    /// output when free-running.
    presented: u32,
    /// Captured frames replaced by a newer one before any output showed
    /// them at all.
    superseded: u32,
    stalls: u32,
    /// Every buffer in a presenter's rotation was still busy at commit time,
    /// forcing a reuse. Should stay zero outside a stall — see the
    /// buffer-selection comment in `present_frame`.
    buffer_reuse: u32,
}

impl FrameStats {
    /// Long enough that the line is rare, short enough to watch a change land.
    const INTERVAL: Duration = Duration::from_secs(10);

    fn new() -> Self {
        Self {
            since: Instant::now(),
            captured: 0,
            waiting: Duration::ZERO,
            snapshot: Duration::ZERO,
            requesting: Duration::ZERO,
            blending: Duration::ZERO,
            presented: 0,
            superseded: 0,
            stalls: 0,
            buffer_reuse: 0,
        }
    }

    /// Whether an interval's worth of data has accumulated. Nothing is
    /// reported while no frame has been captured yet, so a stalled-at-start
    /// slicer (no canvas damage) never prints an empty line.
    fn due(&self) -> bool {
        self.since.elapsed() >= Self::INTERVAL && self.captured > 0
    }
}

/// Bookkeeping for `wp_presentation` feedback: which snapshot each output has
/// reported on, and the running per-interval tallies once every outstanding
/// presenter for a snapshot has answered.
///
/// Kept in its own type, with the settling arithmetic in the free function
/// [`settle`], so that arithmetic is unit-testable without a compositor.
struct Timing {
    /// Identifies the snapshot a presentation-feedback request was made for.
    /// Increments once per canvas capture, in `State::new_snapshot`.
    snapshot_id: u64,
    /// One entry per snapshot with at least one outstanding or answered
    /// feedback request, oldest first (`BTreeMap` over an incrementing key).
    pending: BTreeMap<u64, Vec<Slot>>,
    /// Running per-output tallies for the current report interval; `refresh`
    /// is the latest known rate and is *not* reset between intervals, since
    /// it is a measurement, not a count.
    per_output: Vec<OutputAccum>,
    offset_sum_ms: f64,
    offset_max_ms: f64,
    offset_count: u32,
    straddles: u32,
}

/// One presenter's answer for one snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
    /// No feedback has been requested for this presenter on this snapshot
    /// (it was not due, or was gated out at commit time).
    NotCommitted,
    /// Feedback requested; the compositor has not answered yet.
    Waiting,
    /// The compositor showed this update to the user.
    Presented { at_ns: u64, refresh_ns: u32 },
    /// The compositor never showed this update (superseded, surface
    /// destroyed, ...). The feedback protocol destroys the object either
    /// way, so there is nothing further to clean up here.
    Discarded,
}

#[derive(Debug, Clone, Copy, Default)]
struct OutputAccum {
    presented: u32,
    discarded: u32,
    /// `None` until a `Presented` event carries a non-zero refresh.
    refresh_ns: Option<u32>,
}

/// What settling a fully-answered snapshot's slots works out to. Pure and
/// independent of `Timing`'s bookkeeping so it can be tested directly.
#[derive(Debug, Clone, PartialEq)]
struct Settled {
    /// One entry per slot that resolved to `Presented` or `Discarded`,
    /// keyed by its position in the slot list (the presenter index).
    outcomes: Vec<(usize, SettledOutcome)>,
    /// Spread between the earliest and latest `Presented` timestamp, in
    /// nanoseconds — `None` when fewer than two presenters presented.
    offset_ns: Option<u64>,
    /// Whether that spread exceeds half the smallest known refresh period
    /// among the presenting outputs — i.e. they landed on different
    /// refreshes rather than merely at different points in the same one.
    /// `false` whenever no presenting output reported a non-zero refresh.
    straddle: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SettledOutcome {
    Presented { refresh_ns: Option<u32> },
    Discarded,
}

/// Work out what a snapshot's fully-answered slots mean, in isolation from
/// how they got there.
fn settle(slots: &[Slot]) -> Settled {
    let mut outcomes = Vec::new();
    let mut presented_at: Vec<u64> = Vec::new();
    let mut known_refreshes: Vec<u32> = Vec::new();

    for (index, slot) in slots.iter().enumerate() {
        match *slot {
            Slot::Presented { at_ns, refresh_ns } => {
                let refresh_ns = (refresh_ns != 0).then_some(refresh_ns);
                outcomes.push((index, SettledOutcome::Presented { refresh_ns }));
                presented_at.push(at_ns);
                if let Some(refresh_ns) = refresh_ns {
                    known_refreshes.push(refresh_ns);
                }
            }
            Slot::Discarded => outcomes.push((index, SettledOutcome::Discarded)),
            Slot::NotCommitted | Slot::Waiting => {}
        }
    }

    let offset_ns = (presented_at.len() >= 2).then(|| {
        let min = presented_at.iter().min().copied().unwrap_or_default();
        let max = presented_at.iter().max().copied().unwrap_or_default();
        max - min
    });
    let straddle = match (offset_ns, known_refreshes.iter().min()) {
        (Some(offset_ns), Some(&min_refresh_ns)) => offset_ns > u64::from(min_refresh_ns) / 2,
        _ => false,
    };

    Settled {
        outcomes,
        offset_ns,
        straddle,
    }
}

/// What one report interval's presentation figures worked out to.
struct IntervalTiming {
    /// Mean and max offset in ms, `None` when fewer than two presentations
    /// were matched up this interval (including "no feedback at all").
    offset: Option<(f64, f64)>,
    straddles: u32,
    /// Same order as `State::presenters`.
    outputs: Vec<OutputAccum>,
}

impl Timing {
    fn new(presenter_count: usize) -> Self {
        Self {
            snapshot_id: 0,
            pending: BTreeMap::new(),
            per_output: vec![OutputAccum::default(); presenter_count],
            offset_sum_ms: 0.0,
            offset_max_ms: 0.0,
            offset_count: 0,
            straddles: 0,
        }
    }

    /// Mark presenter `index` as awaiting feedback for snapshot `id`. Called
    /// just before the `feedback` request goes out, so a reply that somehow
    /// arrives before `commit` returns still has somewhere to land.
    fn request(&mut self, id: u64, index: usize) {
        let count = self.per_output.len();
        let slots = self
            .pending
            .entry(id)
            .or_insert_with(|| vec![Slot::NotCommitted; count]);
        if let Some(slot) = slots.get_mut(index) {
            *slot = Slot::Waiting;
        }
        // A compositor that drops a feedback request without ever answering
        // it (seen on nothing so far, but nothing rules it out) must not
        // grow this map without bound. Sixteen snapshots is comfortably more
        // than the pipeline ever has in flight at once.
        while self.pending.len() > 16 {
            let Some(&oldest) = self.pending.keys().next() else {
                break;
            };
            self.finalize(oldest);
        }
    }

    fn presented(&mut self, id: u64, index: usize, at_ns: u64, refresh_ns: u32) {
        if let Some(slot) = self.pending.get_mut(&id).and_then(|s| s.get_mut(index)) {
            *slot = Slot::Presented { at_ns, refresh_ns };
        }
        self.finalize_if_settled(id);
    }

    fn discarded(&mut self, id: u64, index: usize) {
        if let Some(slot) = self.pending.get_mut(&id).and_then(|s| s.get_mut(index)) {
            *slot = Slot::Discarded;
        }
        self.finalize_if_settled(id);
    }

    fn finalize_if_settled(&mut self, id: u64) {
        let settled = self
            .pending
            .get(&id)
            .is_some_and(|slots| !slots.contains(&Slot::Waiting));
        if settled {
            self.finalize(id);
        }
    }

    /// Fold a snapshot's slots into the running tallies and drop it,
    /// whether every presenter answered or it was evicted as-is.
    fn finalize(&mut self, id: u64) {
        let Some(slots) = self.pending.remove(&id) else {
            return;
        };
        let settled = settle(&slots);
        for (index, outcome) in settled.outcomes {
            let Some(accum) = self.per_output.get_mut(index) else {
                continue;
            };
            match outcome {
                SettledOutcome::Presented { refresh_ns } => {
                    accum.presented += 1;
                    if refresh_ns.is_some() {
                        accum.refresh_ns = refresh_ns;
                    }
                }
                SettledOutcome::Discarded => accum.discarded += 1,
            }
        }
        if let Some(offset_ns) = settled.offset_ns {
            let offset_ms = offset_ns as f64 / 1_000_000.0;
            self.offset_sum_ms += offset_ms;
            self.offset_max_ms = self.offset_max_ms.max(offset_ms);
            self.offset_count += 1;
        }
        if settled.straddle {
            self.straddles += 1;
        }
    }

    /// Snapshot the interval's figures and reset the counters (but not the
    /// latest-known refresh rates, which are not counts).
    fn drain_interval(&mut self) -> IntervalTiming {
        let offset = (self.offset_count > 0).then(|| {
            (
                self.offset_sum_ms / f64::from(self.offset_count),
                self.offset_max_ms,
            )
        });
        let outputs = self.per_output.clone();
        let straddles = self.straddles;

        for accum in &mut self.per_output {
            accum.presented = 0;
            accum.discarded = 0;
        }
        self.offset_sum_ms = 0.0;
        self.offset_max_ms = 0.0;
        self.offset_count = 0;
        self.straddles = 0;

        IntervalTiming {
            offset,
            straddles,
            outputs,
        }
    }
}

struct State {
    outputs: Vec<(WlOutput, Option<String>)>,
    presenters: Vec<Presenter>,
    capture: Capture,
    closed: bool,
    /// From `SlicerSpec.free_run`; see the type it lives on for the tradeoff.
    free_run: bool,
    /// `None` when the compositor does not offer `wp_presentation` — the
    /// gating logic works identically either way, only the measurements it
    /// is possible to report differ.
    presentation: Option<WpPresentation>,
    timing: Timing,
    stats: FrameStats,
}

impl State {
    fn all_configured(&self) -> bool {
        self.presenters.iter().all(|p| p.configured.is_some())
    }

    /// Whether there is a stale snapshot at least one presentation policy
    /// permits showing right now.
    ///
    /// Locked (`!free_run`): every presenter must be ready before any of
    /// them may show a newer frame than its neighbours — a partial commit
    /// here is exactly the race the gate exists to close. Free-run: an
    /// output takes the newest frame the instant it can, without regard for
    /// its neighbours' pace, because it was configured that way precisely
    /// because it cannot share one.
    fn can_present(&self) -> bool {
        if self.free_run {
            self.presenters
                .iter()
                .any(|p| p.stale && (!p.frame_pending || p.stalled))
        } else {
            self.presenters.iter().any(|p| p.stale)
                && self
                    .presenters
                    .iter()
                    .all(|p| !p.frame_pending || p.stalled)
        }
    }

    /// A fresh canvas capture is ready to show: mark every presenter due to
    /// take it, and drop any output that has stopped answering its frame
    /// callback so it cannot freeze the rest of the wall.
    fn new_snapshot(&mut self) {
        // If every presenter is still waiting on the snapshot this one is
        // about to replace, no output ever showed it at all.
        if self.presenters.iter().all(|p| p.stale) {
            self.stats.superseded += 1;
        }
        self.timing.snapshot_id += 1;
        for presenter in &mut self.presenters {
            presenter.stale = true;
            if presenter.frame_pending && !presenter.stalled {
                if let Some(elapsed) = presenter.pending_since.map(|since| since.elapsed()) {
                    if elapsed > STALL_TIMEOUT {
                        eprintln!(
                            "slicer: output {} stopped answering frame callbacks after {:.0} ms; \
                             dropping it from the gate until it recovers",
                            presenter.name,
                            elapsed.as_secs_f64() * 1000.0,
                        );
                        presenter.stalled = true;
                        self.stats.stalls += 1;
                    }
                }
            }
        }
    }

    /// Print the human-readable line and, once an interval has elapsed,
    /// the machine-readable one — see the module-level docs on the JSON
    /// shape.
    fn report_stats_if_due(&mut self) {
        if !self.stats.due() {
            return;
        }
        let elapsed = self.stats.since.elapsed();
        let interval = self.timing.drain_interval();
        let presentation_feedback = self.presentation.is_some();
        let captured = f64::from(self.stats.captured);
        let per_ms = |total: Duration| total.as_secs_f64() * 1000.0 / captured;
        let canvas_fps = captured / elapsed.as_secs_f64();
        let presented_fps = f64::from(self.stats.presented) / elapsed.as_secs_f64();

        let offset_text = match (presentation_feedback, interval.offset) {
            (false, _) => "n/a (no wp_presentation)".to_string(),
            (true, None) => "n/a".to_string(),
            (true, Some((mean, max))) => format!("mean {mean:.1} ms max {max:.1} ms"),
        };
        eprintln!(
            "slicer: {canvas_fps:.1} fps captured, {presented_fps:.1} fps presented, over \
             {:.0}s per frame: waiting {:.1} ms, snapshot {:.1} ms, requesting {:.1} ms, \
             blending {:.1} ms; superseded {}, stalls {}, straddles {}, buffer reuse {}, \
             offset {offset_text}",
            elapsed.as_secs_f64(),
            per_ms(self.stats.waiting),
            per_ms(self.stats.snapshot),
            per_ms(self.stats.requesting),
            per_ms(self.stats.blending),
            self.stats.superseded,
            self.stats.stalls,
            interval.straddles,
            self.stats.buffer_reuse,
        );

        let measured_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let stats = ProjectionStats {
            measured_at,
            interval_seconds: elapsed.as_secs_f64(),
            free_run: self.free_run,
            canvas_fps,
            presented_fps,
            frames_superseded: self.stats.superseded,
            stalls: self.stats.stalls,
            per_frame_ms: FrameCost {
                waiting: per_ms(self.stats.waiting),
                snapshot: per_ms(self.stats.snapshot),
                requesting: per_ms(self.stats.requesting),
                blending: per_ms(self.stats.blending),
            },
            presentation_feedback,
            offset_ms: interval
                .offset
                .map(|(mean, max)| PresentationOffset { mean, max }),
            straddles: interval.straddles,
            outputs: self
                .presenters
                .iter()
                .zip(interval.outputs.iter())
                .map(|(presenter, accum)| OutputTiming {
                    name: presenter.name.clone(),
                    presented: accum.presented,
                    discarded: accum.discarded,
                    refresh_hz: accum.refresh_ns.map(|ns| 1_000_000_000.0 / f64::from(ns)),
                })
                .collect(),
        };
        // Rust's stdout is line-buffered, so this flushes on the newline
        // `println!` appends — the manager's reader thread sees it promptly
        // without either side needing to do anything explicit.
        println!("{}", serde_json::to_string(&stats).unwrap_or_default());

        self.stats = FrameStats::new();
    }
}

pub fn run(spec: &SlicerSpec) -> anyhow::Result<()> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let handle = queue.handle();

    let compositor: WlCompositor = globals.bind(&handle, 4..=4, ())?;
    let shm: WlShm = globals.bind(&handle, 1..=1, ())?;
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&handle, 1..=4, ())?;
    let screencopy: ZwlrScreencopyManagerV1 = globals.bind(&handle, 1..=3, ())?;
    // Optional: without it, gating still works exactly the same way (it
    // never depended on presentation feedback), only `offset_ms` and the
    // per-output presented/discarded/refreshHz figures cannot be measured.
    let presentation: Option<WpPresentation> = globals.bind(&handle, 1..=1, ()).ok();

    let mut state = State {
        outputs: Vec::new(),
        presenters: Vec::new(),
        capture: Capture::default(),
        closed: false,
        free_run: spec.free_run,
        presentation,
        timing: Timing::new(spec.slices.len()),
        stats: FrameStats::new(),
    };
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" && global.version >= 4 {
            let output: WlOutput = globals.registry().bind(global.name, 4, &handle, ());
            state.outputs.push((output, None));
        }
    }
    queue.roundtrip(&mut state)?;

    let find = |state: &State, name: &str| -> Option<WlOutput> {
        state
            .outputs
            .iter()
            .find(|(_, n)| n.as_deref() == Some(name))
            .map(|(o, _)| o.clone())
    };
    let source = find(&state, &spec.source)
        .ok_or_else(|| anyhow::anyhow!("no output named {} to capture", spec.source))?;

    // One presenter per slice, covering its physical output entirely.
    for (index, slice) in spec.slices.iter().enumerate() {
        let target = find(&state, &slice.output)
            .ok_or_else(|| anyhow::anyhow!("no output named {}", slice.output))?;
        let surface = compositor.create_surface(&handle, ());
        let region: WlRegion = compositor.create_region(&handle, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            Some(&target),
            Layer::Overlay,
            "suede-slice".to_string(),
            &handle,
            index,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_size(0, 0);
        surface.commit();

        state.presenters.push(Presenter {
            surface,
            layer_surface,
            configured: None,
            name: slice.output.clone(),
            buffers: Vec::new(),
            busy: Vec::new(),
            next_buffer: 0,
            transfer: Vec::new(),
            source: slice.source,
            frame_pending: false,
            pending_since: None,
            stalled: false,
            stale: false,
        });
    }

    while !state.all_configured() {
        queue.blocking_dispatch(&mut state)?;
        if state.closed {
            return Ok(());
        }
    }
    // How much black each region of the canvas is receiving. Derived from
    // every slice, because how much a projector must lift depends on how many
    // *others* light the same pixel — a four-way grid centre needs none while
    // its two-way seams still do.
    let coverage = Coverage::new(spec.slices.iter().map(|slice| slice.source));
    for (index, (presenter, slice)) in state
        .presenters
        .iter_mut()
        .zip(spec.slices.iter())
        .enumerate()
    {
        let (width, height) = presenter.configured.unwrap();
        for slot in 0..2 {
            presenter
                .buffers
                .push(shm_buffer(&shm, &handle, width, height, (index, slot))?);
        }
        presenter.busy = vec![false; presenter.buffers.len()];
        // Built at the *presented* size: ramps are defined against the
        // slice, and any mismatch shows up as identity pixels, not a panic.
        let mut transfer = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                // Coverage is a property of the canvas, so the slice-local
                // pixel is looked up at its place in the layout.
                let lift = coverage.lift(
                    spec.black_lift,
                    f64::from(slice.source.x + x) + 0.5,
                    f64::from(slice.source.y + y) + 0.5,
                );
                transfer.push(pixel_transfer(&slice.ramps, spec.gamma, lift, x, y));
            }
        }
        presenter.transfer = transfer;
    }

    if let Some(pattern) = spec.pattern {
        present_pattern(&mut state, spec, pattern);
        // Static image: nothing further to do but stay alive.
        loop {
            queue.blocking_dispatch(&mut state)?;
            if state.closed {
                return Ok(());
            }
        }
    }

    // The capture loop, pipelined. Each iteration asks the compositor for the
    // canvas's next damaged frame, so an idle canvas still parks us in
    // blocking_dispatch — but the request for the *next* frame goes out
    // before this one is blended, rather than after.
    //
    // Measured on the two-projector rig before this: a strictly serial
    // request-wait-blend-present loop delivered 49 frames a second from a
    // 60 fps camera and 52 from a shader the GPU rendered effortlessly. The
    // ceiling did not move with the content because it was never the
    // content: presenting took the loop past the compositor's next frame, so
    // every cycle waited for the one after. Only about 6 ms of each 20 ms
    // cycle was work.
    //
    // Presentation gating (see the module docs) sits on top of that
    // pipeline without disturbing it: the wait below also ends the moment
    // every presenter is ready to show what has already been captured, so a
    // wall that is only waiting on its own outputs — not on the canvas —
    // never waits for a capture that has not even been requested yet.
    let mut failures = 0u32;

    // Prime: one capture requested and handed a buffer to fill, and the
    // *next* one already requested so its buffer negotiation can run
    // concurrently with the first wait, exactly as it does every loop
    // iteration below.
    let mut current = request_capture(&mut state, &screencopy, &source, &handle);
    if !arm_copy(&mut state, &mut queue, &shm, &handle, &current)? {
        return Ok(());
    }
    let mut next = request_capture(&mut state, &screencopy, &source, &handle);

    loop {
        let waiting_from = Instant::now();
        while !state.capture.ready && !state.capture.failed && !state.can_present() {
            queue.blocking_dispatch(&mut state)?;
            if state.closed {
                current.destroy();
                next.destroy();
                return Ok(());
            }
        }
        state.stats.waiting += waiting_from.elapsed();

        if state.capture.failed {
            current.destroy();
            next.destroy();
            failures += 1;
            if failures >= MAX_FAILURES {
                anyhow::bail!("screencopy failed {failures} times; giving up");
            }
            // Either frame may have been the one that failed, so start over
            // with a single capture rather than guessing.
            state.capture.failed = false;
            state.capture.ready = false;
            current = request_capture(&mut state, &screencopy, &source, &handle);
            if !arm_copy(&mut state, &mut queue, &shm, &handle, &current)? {
                return Ok(());
            }
            next = request_capture(&mut state, &screencopy, &source, &handle);
            continue;
        }
        failures = 0;

        if state.capture.ready {
            state.capture.ready = false;
            state.capture.first_copy_done = true;

            // Copy the frame out and hand the buffer straight back, so the
            // compositor is already drawing the next one while this one is
            // being cut into slices and committed.
            let snapshot_from = Instant::now();
            state.capture.take_snapshot();
            current.destroy();
            state.stats.snapshot += snapshot_from.elapsed();

            // Usually already satisfied: the handshake ran during the wait above.
            let requesting_from = Instant::now();
            if !arm_copy(&mut state, &mut queue, &shm, &handle, &next)? {
                return Ok(());
            }
            state.stats.requesting += requesting_from.elapsed();
            current = next;
            next = request_capture(&mut state, &screencopy, &source, &handle);

            state.new_snapshot();
            state.stats.captured += 1;
        }

        if state.can_present() {
            let blending_from = Instant::now();
            present_frame(&mut state, &handle);
            state.stats.blending += blending_from.elapsed();
        }

        state.report_stats_if_due();
    }
}

/// Ask the compositor for the canvas's next damaged frame.
///
/// Sends only. The compositor answers with the buffer layout it wants, which
/// [`arm_copy`] waits for — deliberately separate, so that round trip can be
/// left running while the caller gets on with something else.
fn request_capture(
    state: &mut State,
    screencopy: &ZwlrScreencopyManagerV1,
    source: &WlOutput,
    handle: &QueueHandle<State>,
) -> ZwlrScreencopyFrameV1 {
    state.capture.offered = None;
    state.capture.buffer_done = false;
    screencopy.capture_output(0, source, handle, ())
}

/// Give a requested frame the buffer to fill. `false` means the compositor
/// went away.
fn arm_copy(
    state: &mut State,
    queue: &mut wayland_client::EventQueue<State>,
    shm: &WlShm,
    handle: &QueueHandle<State>,
    frame: &ZwlrScreencopyFrameV1,
) -> anyhow::Result<bool> {
    while !state.capture.buffer_done && !state.capture.failed {
        queue.blocking_dispatch(state)?;
        if state.closed {
            return Ok(false);
        }
    }
    if state.capture.failed {
        return Ok(true);
    }
    if !state.capture.first_copy_done {
        if let Some((format, width, height, stride)) = state.capture.offered {
            eprintln!("slicer: capture offer {width}x{height} stride {stride} format {format:#x}");
        }
    }
    ensure_capture_buffer(&mut state.capture, shm, handle)?;
    let buffer = &state.capture.buffer.as_ref().unwrap().0;
    // Damage is reported against the previous copy into this buffer, which is
    // why the same buffer is reused every time.
    if state.capture.first_copy_done {
        frame.copy_with_damage(buffer);
    } else {
        frame.copy(buffer);
    }
    Ok(true)
}

/// Create an shm-backed buffer with the given Wayland user data attached, so
/// its owner can be told apart from every other buffer's events.
///
/// Generic rather than fixed to one user-data type because the two callers
/// need different things: the capture path tracks nothing per buffer (`()`,
/// via `delegate_noop!`), while presenter buffers need to know which
/// presenter and which of its two slots a `wl_buffer.release` belongs to.
fn shm_buffer<U: Send + Sync + 'static>(
    shm: &WlShm,
    handle: &QueueHandle<State>,
    width: u32,
    height: u32,
    user_data: U,
) -> anyhow::Result<(WlBuffer, memmap2::MmapMut)>
where
    State: Dispatch<WlBuffer, U>,
{
    let stride = width as i32 * 4;
    let size = stride as usize * height as usize;
    let mut file = tempfile::tempfile()?;
    file.write_all(&vec![0u8; size])?;
    let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
    let pool: WlShmPool = shm.create_pool(file.as_fd(), size as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride,
        wl_shm::Format::Argb8888,
        handle,
        user_data,
    );
    pool.destroy();
    Ok((buffer, map))
}

fn ensure_capture_buffer(
    capture: &mut Capture,
    shm: &WlShm,
    handle: &QueueHandle<State>,
) -> anyhow::Result<()> {
    let (format, width, height, stride) = capture
        .offered
        .ok_or_else(|| anyhow::anyhow!("screencopy offered no shm buffer"))?;
    capture.format = Some(
        pixel_format(format)
            .ok_or_else(|| anyhow::anyhow!("unsupported capture format {format:#x}"))?,
    );
    if let Some((_, _, w, h, s)) = &capture.buffer {
        if (*w, *h, *s) == (width, height, stride) {
            return Ok(());
        }
    }
    let size = stride as usize * height as usize;
    let mut file = tempfile::tempfile()?;
    file.write_all(&vec![0u8; size])?;
    let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
    let pool: WlShmPool = shm.create_pool(file.as_fd(), size as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        // Echo whatever the compositor offered; 4-byte formats only.
        WEnum::<wl_shm::Format>::from(format)
            .into_result()
            .map_err(|_| anyhow::anyhow!("unknown shm format {format}"))?,
        handle,
        (),
    );
    pool.destroy();
    if let Some((old, ..)) = capture.buffer.take() {
        old.destroy();
    }
    capture.buffer = Some((buffer, map, width, height, stride));
    Ok(())
}

/// Cut the captured canvas into slices, apply each column's transfer, and
/// commit whichever presenters the gating policy says are due a frame.
///
/// Locked mode is only ever called once `can_present` has confirmed every
/// presenter is ready, so every `stale` presenter is committed together —
/// the whole point being that no output can show a newer frame than its
/// neighbours. Free-run commits presenter by presenter as each becomes
/// ready, which is exactly what makes a wall that cannot share a rate keep
/// moving at all.
fn present_frame(state: &mut State, handle: &QueueHandle<State>) {
    // Split-borrow: the canvas is read while presenter buffers are written,
    // and the presentation bookkeeping is independent of both.
    let State {
        capture,
        presenters,
        timing,
        presentation,
        free_run,
        stats,
        ..
    } = state;
    // The snapshot, not the shared buffer: by now the compositor has been
    // given that buffer back and may already be drawing the next frame into
    // it. Blending from underneath it would tear.
    let Some((canvas_width, canvas_height, stride)) = capture.snapshot_geometry else {
        // Nothing to show, so nothing is owed: a presenter left `stale`
        // with no commit to clear it would keep `can_present` true and
        // turn the event loop into a spin.
        for presenter in presenters.iter_mut() {
            presenter.stale = false;
        }
        return;
    };
    let canvas = &capture.snapshot;
    let y_invert = capture.y_invert;
    let format = capture.format.unwrap_or(PixelFormat {
        bytes: 4,
        red: 2,
        green: 1,
        blue: 0,
    });
    // The offer is not gospel: a scale or a mid-resize race can make width,
    // stride, and buffer length disagree. The buffer length is the only hard
    // truth, so derive the usable geometry from it.
    let usable_width = canvas_width.min(stride / format.bytes as u32);
    let usable_height = canvas_height.min((canvas.len() as u32) / stride.max(1));
    let snapshot_id = timing.snapshot_id;
    // One present cycle for this call, regardless of how many presenters it
    // reaches: locked commits to all of them together, free-run to whichever
    // are ready, and either way it is a single event for `presented_fps`.
    let mut committed_any = false;

    for (index, presenter) in presenters.iter_mut().enumerate() {
        let Some((width, height)) = presenter.configured else {
            presenter.stale = false;
            continue;
        };
        // In locked mode this reduces to `presenter.stale`, since the caller
        // only reaches here once every presenter is already ready; free-run
        // needs the check here too, because presenters arrive at this loop
        // at different points in their own cycle.
        let ready = !presenter.frame_pending || presenter.stalled;
        let due = presenter.stale && (!*free_run || ready);
        if !due {
            continue;
        }
        let source_x = presenter.source.x.max(0) as u32;
        let source_y = presenter.source.y.max(0) as u32;
        let rows = height.min(usable_height.saturating_sub(source_y));
        let copy_width = width.min(usable_width.saturating_sub(source_x)) as usize;
        if copy_width == 0 || rows == 0 {
            // A slice the snapshot does not reach (a mid-resize race) gets
            // no commit, so it must be marked shown by hand: otherwise it
            // stays `stale` with nothing pending, `can_present` never goes
            // false, and the loop spins at full CPU presenting nothing.
            presenter.stale = false;
            continue;
        }

        // Choose a buffer the compositor is not currently holding, starting
        // the search at `next_buffer` so the two slots keep alternating in
        // the common case. Falling back to reusing a busy one only happens
        // when an output has fallen behind on releases — a stall, in
        // practice — and is counted so it shows up as non-zero if it ever
        // happens without one.
        let len = presenter.buffers.len();
        let slot = (0..len)
            .map(|step| (presenter.next_buffer + step) % len)
            .find(|&candidate| !presenter.busy[candidate])
            .unwrap_or_else(|| {
                stats.buffer_reuse += 1;
                presenter.next_buffer % len
            });
        presenter.next_buffer = (slot + 1) % len;
        presenter.busy[slot] = true;

        {
            // Split-borrow again: the transfer table is read while this
            // presenter's buffer is written.
            let Presenter {
                transfer, buffers, ..
            } = presenter;
            let (_, map) = &mut buffers[slot];
            let blend = Blend {
                canvas,
                transfer,
                format,
                stride,
                y_invert,
                usable_height,
                source_x,
                source_y,
                width,
                copy_width,
            };

            let row_bytes = width as usize * 4;
            let painted = &mut map[..rows as usize * row_bytes];
            let workers = blend_workers(rows);
            if workers <= 1 {
                blend.rows(painted, 0, rows);
            } else {
                // Rows are independent, so each worker owns a disjoint band of
                // the destination and nothing needs synchronising. Scoped
                // threads borrow the canvas and the table directly, which is
                // why this needs no channel and no Arc.
                let band = rows.div_ceil(workers);
                std::thread::scope(|scope| {
                    for (index, chunk) in painted.chunks_mut(band as usize * row_bytes).enumerate()
                    {
                        let blend = &blend;
                        let first = index as u32 * band;
                        let count = band.min(rows - first);
                        scope.spawn(move || blend.rows(chunk, first, count));
                    }
                });
            }
        }

        let (buffer, _) = &presenter.buffers[slot];
        presenter.surface.attach(Some(buffer), 0, 0);
        presenter
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        // Requested before `commit`, per wl_surface.frame: the callback
        // fires no earlier than the *next* commit's contents are shown, so
        // asking after commit would describe the wrong frame.
        presenter.surface.frame(handle, index);
        presenter.frame_pending = true;
        presenter.pending_since = Some(Instant::now());
        presenter.stale = false;
        if let Some(presentation) = presentation.as_ref() {
            timing.request(snapshot_id, index);
            presentation.feedback(&presenter.surface, handle, (index, snapshot_id));
        }
        presenter.surface.commit();
        committed_any = true;
    }

    if committed_any {
        stats.presented += 1;
    }
}

/// Everything a blend worker needs that does not vary between rows.
///
/// Cutting a slice out of the canvas and shading it is per-pixel independent
/// work, so it is the one part of the frame that can simply be divided up.
/// Measured before this: 10 ms a frame, single-threaded, on a machine with
/// twenty idle cores.
struct Blend<'a> {
    canvas: &'a [u8],
    transfer: &'a [(u16, u8)],
    format: PixelFormat,
    stride: u32,
    y_invert: bool,
    usable_height: u32,
    source_x: u32,
    source_y: u32,
    width: u32,
    copy_width: usize,
}

impl Blend<'_> {
    /// Shade `count` destination rows, `dst` starting at row `first`.
    fn rows(&self, dst: &mut [u8], first: u32, count: u32) {
        let row_bytes = self.width as usize * 4;
        for y in 0..count {
            let target = first + y;
            let canvas_row = self.source_y + target;
            let canvas_row = if self.y_invert {
                self.usable_height - 1 - canvas_row
            } else {
                canvas_row
            };
            let src_row = canvas_row as usize * self.stride as usize;
            let dst_row = y as usize * row_bytes;
            let transfer_row = target as usize * self.width as usize;
            for x in 0..self.copy_width {
                let src = src_row + (self.source_x as usize + x) * self.format.bytes;
                let (a, b) = self
                    .transfer
                    .get(transfer_row + x)
                    .copied()
                    .unwrap_or((256, 0));
                let shade = |v: u8| (((a as u32 * v as u32) >> 8) + b as u32).min(255) as u8;
                let out = &mut dst[dst_row + x * 4..dst_row + x * 4 + 4];
                // Presenters are always BGRA, opaque.
                out[0] = shade(self.canvas[src + self.format.blue]);
                out[1] = shade(self.canvas[src + self.format.green]);
                out[2] = shade(self.canvas[src + self.format.red]);
                out[3] = 255;
            }
        }
    }
}

/// How many workers to split a slice of `rows` across.
///
/// Capped rather than taking every core: the work is a few milliseconds, and
/// past a handful of threads the spawn cost starts to eat the saving. Never
/// more workers than rows, so a tiny slice stays on one thread.
fn blend_workers(rows: u32) -> u32 {
    const MAX: u32 = 8;
    static AVAILABLE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let available = *AVAILABLE.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
    });
    available.min(MAX).min(rows).max(1)
}

/// Test patterns, drawn once in canvas coordinates so they continue exactly
/// across the seams — with the same ramps and lift content would get.
fn present_pattern(state: &mut State, spec: &SlicerSpec, pattern: crate::model::TestPattern) {
    for (slice, presenter) in spec.slices.iter().zip(state.presenters.iter_mut()) {
        let Some((width, height)) = presenter.configured else {
            continue;
        };
        let fake = OverlaySpec {
            output: slice.output.clone(),
            gamma: spec.gamma,
            black_lift: 0.0,
            rect: slice.source,
            pattern: Some(pattern),
            ramps: Vec::new(),
        };
        let rgb = super::pattern::render(width, height, &fake);
        let (_, map) = &mut presenter.buffers[0];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let (a, b) = presenter
                    .transfer
                    .get(y * width as usize + x)
                    .copied()
                    .unwrap_or((256, 0));
                let src = &rgb[(y * width as usize + x) * 3..(y * width as usize + x) * 3 + 3];
                let out = |v: u8| (((a as u32 * v as u32) >> 8) + b as u32).min(255) as u8;
                let dst = &mut map[(y * width as usize + x) * 4..(y * width as usize + x) * 4 + 4];
                dst[0] = out(src[2]);
                dst[1] = out(src[1]);
                dst[2] = out(src[0]);
                dst[3] = 255;
            }
        }
        let (buffer, _) = &presenter.buffers[0];
        presenter.surface.attach(Some(buffer), 0, 0);
        presenter
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        presenter.surface.commit();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            for (candidate, stored) in &mut state.outputs {
                if candidate == output {
                    *stored = Some(name);
                    break;
                }
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                if let Some(presenter) = state.presenters.get_mut(*index) {
                    presenter.configured = Some((width, height));
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                // Only 4-byte formats are handled; anything else is refused
                // at copy time by never storing an offer.
                let raw = match format {
                    WEnum::Value(value) => value as u32,
                    WEnum::Unknown(value) => value,
                };
                state.capture.offered = Some((raw, width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.capture.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.capture.y_invert = flags
                    .into_result()
                    .map(|f| f.contains(zwlr_screencopy_frame_v1::Flags::YInvert))
                    .unwrap_or(false);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.capture.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.capture.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `Done` is the callback's only event: the output took the commit
        // this callback was requested for. Clearing `stalled` here as well
        // as `frame_pending` is what lets an output rejoin the gate the
        // moment it starts answering again.
        if let wl_callback::Event::Done { .. } = event {
            if let Some(presenter) = state.presenters.get_mut(*index) {
                presenter.frame_pending = false;
                presenter.stalled = false;
                presenter.pending_since = None;
            }
        }
    }
}

/// Presenter buffers only — the capture buffer keeps the `()` behaviour
/// below via `delegate_noop!`, since nothing needs to know which capture
/// buffer was released (there is only ever one in play).
impl Dispatch<WlBuffer, (usize, usize)> for State {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        data: &(usize, usize),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            let (presenter_index, buffer_index) = *data;
            if let Some(busy) = state
                .presenters
                .get_mut(presenter_index)
                .and_then(|presenter| presenter.busy.get_mut(buffer_index))
            {
                *busy = false;
            }
        }
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpPresentation,
        _: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The only event (`ClockId`) says which clock domain the feedback
        // timestamps use. They are only ever compared against each other
        // here (the inter-output offset), never against wall time, so which
        // clock it is does not change anything this module does with it.
    }
}

impl Dispatch<WpPresentationFeedback, (usize, u64)> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        data: &(usize, u64),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let (index, snapshot_id) = *data;
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                ..
            } => {
                let at_ns = ((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo)) * 1_000_000_000
                    + u64::from(tv_nsec);
                state.timing.presented(snapshot_id, index, at_ns, refresh);
            }
            wp_presentation_feedback::Event::Discarded => {
                state.timing.discarded(snapshot_id, index);
            }
            // `sync_output`, sent only ahead of `Presented`, is not needed:
            // the offset and straddle math only cares about timestamps.
            _ => {}
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: WlRegion);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);

#[cfg(test)]
mod tests {
    use super::*;

    /// A canvas whose bytes are all distinct, so a row addressed wrongly
    /// cannot coincidentally match a row addressed rightly.
    fn canvas(height: u32, stride: u32) -> Vec<u8> {
        (0..stride * height)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<u8>>()
    }

    fn blend_for<'a>(canvas: &'a [u8], transfer: &'a [(u16, u8)]) -> Blend<'a> {
        Blend {
            canvas,
            transfer,
            format: PixelFormat {
                bytes: 4,
                red: 2,
                green: 1,
                blue: 0,
            },
            stride: 40,
            y_invert: false,
            usable_height: 9,
            source_x: 2,
            source_y: 1,
            width: 6,
            copy_width: 5,
        }
    }

    /// Splitting the work across workers must produce byte-identical output.
    ///
    /// The band arithmetic is the whole risk of parallelising this: a worker
    /// addresses its destination relative to its own chunk but the canvas and
    /// the transfer table absolutely, and getting that wrong shifts rows in a
    /// way that looks plausible on a photograph of a projector.
    #[test]
    fn splitting_the_blend_changes_nothing() {
        let pixels = canvas(9, 40);
        let transfer: Vec<(u16, u8)> = (0..6 * 8).map(|i| ((i * 7 % 300) as u16, 3)).collect();
        let blend = blend_for(&pixels, &transfer);
        let rows = 7;
        let row_bytes = blend.width as usize * 4;

        let mut whole = vec![0u8; rows as usize * row_bytes];
        blend.rows(&mut whole, 0, rows);

        // Every division, including ones that leave a short final band.
        for band in 1..=rows {
            let mut split = vec![0u8; rows as usize * row_bytes];
            for (index, chunk) in split.chunks_mut(band as usize * row_bytes).enumerate() {
                let first = index as u32 * band;
                blend.rows(chunk, first, band.min(rows - first));
            }
            assert_eq!(split, whole, "band size {band} produced a different image");
        }
    }

    /// The worker count must never exceed the rows, or a band would be empty
    /// and `rows - first` would underflow.
    #[test]
    fn workers_never_outnumber_rows() {
        for rows in 1..4u32 {
            assert!(blend_workers(rows) <= rows, "{rows} rows");
            assert!(blend_workers(rows) >= 1);
        }
    }

    /// A vertically flipped capture must address the canvas from the bottom.
    #[test]
    fn y_invert_is_honoured_per_band() {
        let pixels = canvas(9, 40);
        let transfer: Vec<(u16, u8)> = vec![(256, 0); 6 * 8];
        let mut blend = blend_for(&pixels, &transfer);
        blend.y_invert = true;
        let rows = 6;
        let row_bytes = blend.width as usize * 4;

        let mut whole = vec![0u8; rows as usize * row_bytes];
        blend.rows(&mut whole, 0, rows);

        let mut split = vec![0u8; rows as usize * row_bytes];
        for (index, chunk) in split.chunks_mut(2 * row_bytes).enumerate() {
            blend.rows(chunk, index as u32 * 2, 2);
        }
        assert_eq!(split, whole);
    }

    // --- presentation-feedback settling --------------------------------

    fn presented(at_ms: u64, refresh_hz: f64) -> Slot {
        let refresh_ns = if refresh_hz == 0.0 {
            0
        } else {
            (1_000_000_000.0 / refresh_hz).round() as u32
        };
        Slot::Presented {
            at_ns: at_ms * 1_000_000,
            refresh_ns,
        }
    }

    #[test]
    fn a_small_offset_at_60hz_is_not_a_straddle() {
        let settled = settle(&[presented(1000, 60.0), presented(1003, 60.0)]);
        assert_eq!(settled.offset_ns, Some(3_000_000));
        assert!(
            !settled.straddle,
            "3 ms at 60 Hz is well under half a refresh"
        );
    }

    #[test]
    fn a_twelve_millisecond_gap_at_60hz_is_a_straddle() {
        let settled = settle(&[presented(1000, 60.0), presented(1012, 60.0)]);
        assert_eq!(settled.offset_ns, Some(12_000_000));
        assert!(settled.straddle, "12 ms exceeds half of a ~16.7 ms refresh");
    }

    #[test]
    fn a_discarded_output_is_tallied_but_contributes_no_offset() {
        let settled = settle(&[presented(1000, 60.0), Slot::Discarded]);
        assert_eq!(settled.offset_ns, None);
        assert!(!settled.straddle);
        assert_eq!(
            settled.outcomes,
            vec![
                (
                    0,
                    SettledOutcome::Presented {
                        refresh_ns: Some(16_666_667)
                    }
                ),
                (1, SettledOutcome::Discarded),
            ]
        );
    }

    #[test]
    fn an_unknown_refresh_is_ignored_for_hz_and_for_the_straddle_test() {
        let settled = settle(&[presented(1000, 60.0), presented(1012, 0.0)]);
        assert_eq!(settled.offset_ns, Some(12_000_000));
        // Only one presented slot carries a known refresh, but that is still
        // enough to test the straddle against; zero refreshes known at all
        // is the case that skips the test (covered below).
        assert!(settled.straddle);
        let unknown_only = settle(&[presented(1000, 0.0), presented(1012, 0.0)]);
        assert!(
            !unknown_only.straddle,
            "no known refresh means nothing to compare the offset against"
        );
    }

    #[test]
    fn a_single_presenter_has_no_offset_to_report() {
        let settled = settle(&[presented(1000, 60.0)]);
        assert_eq!(settled.offset_ns, None);
        assert!(!settled.straddle);
    }
}
