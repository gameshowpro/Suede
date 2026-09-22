//! The slicer: Suede's own compositor for the overlap, run as `suede slice`.
//!
//! Sway cannot blend overlapping outputs — its global coordinate space gives
//! both projectors the same pixels wherever their boxes intersect. So the
//! overlap is managed here instead. The outputs sit edge to edge in sway
//! (nothing overlaps, nothing bleeds); the app renders once into a headless
//! canvas of `Σwidths − (n−1)·overlap`; and this process captures that
//! canvas each frame, cuts it into per-projector slices whose neighbors
//! *repeat* the seam columns, applies the gamma-shaped blend ramps and black
//! lift, and presents each slice fullscreen on its own physical output.
//! Each projector gets its own buffer, so the two sides of a seam can carry
//! opposite fades — the thing the compositor's shared space can never do.
//!
//! One process for the whole installation: capture happens once per frame no matter
//! how many projectors consume it. The frame loop is damage-driven — a
//! static page costs nothing per second.
//!
//! ## Two capture/blend backends
//!
//! What "captures", "cuts", and "applies... blend ramps" above means depends
//! on [`crate::model::Renderer`]. The CPU path is what is described above,
//! literally: screencopy into a shared-memory buffer, a memcpy snapshot so
//! the next capture can be requested before this one is blended, then the
//! blend itself on the CPU in [`Blend::rows`]. Measured on the
//! four-projector rig this task's GPU path was built for: 32 fps, with the
//! compositor's readback of the 3840x2385 canvas plus the CPU blend not
//! fitting in one canvas frame, so the loop took every second one.
//!
//! The GPU path ([`crate::projection::gpu`]) never leaves the GPU: the
//! compositor blits the canvas straight into a Vulkan image this process
//! exported as a dmabuf, a fragment shader blends it into each output's own
//! dmabuf, and the results are committed as `wl_buffer`s — no pixel crosses
//! to system memory, and [`Capture::take_snapshot`] is never called
//! ([`Capture::snapshot`] stays empty). The two matter for how the frame
//! loop is ordered: on the CPU path the snapshot decouples the buffer from
//! the canvas the instant it is copied out, so the next capture can be
//! armed immediately and a free-run straggler can re-blend the same
//! snapshot later with no risk. On the GPU path there is no snapshot — a
//! capture image *is* the buffer the compositor writes into — so a single
//! image forces `arm_copy(next)` to wait for `gpu.blend()`'s fence, which
//! proves this process is done reading it, before it dares hand that same
//! image back to the compositor. That fence wait is real and cannot be
//! shortened — measured on the four-projector rig, 6.8 ms at 45% GPU load
//! from another app, 40 ms at 99% (the blend shader over 9 Mpixels is
//! trivial; the wait is this process's turn on a queue shared with
//! everything else on the GPU — see `gpu.rs`'s "Queue priority" section) —
//! but with one image it also sits squarely on the serial path, so the
//! wall's presented fps followed it down to 12 under load, below the old
//! CPU path's 20. [`GPU_CAPTURE_SLOTS`] is 2 to overlap it instead: two
//! capture images, alternated, so `arm_copy(next)` can target the *other*
//! image and run concurrently with `blend()`'s wait on this one —
//! `ready(A) → arm_copy(next → B) → blend(A) (fence wait) → present →
//! ready(B) → arm_copy(next → A) → blend(B) → ...`, see `run`'s loop.
//! Reordering `arm_copy` ahead of `blend` is safe specifically because
//! arming B never touches the A that `blend` is still reading — the hazard
//! a single image had no way around. It also means A stays intact for a
//! whole extra cycle after being blended, not just until the next capture:
//! a free-run presenter that was not ready when `blend(A)` ran can still
//! pick A up later, from [`Capture::last_blended_slot`], the moment its own
//! frame callback clears — see `present_frame_gpu`'s safety-net call —
//! instead of only ever catching the *next* capture the way one image had
//! to. The compositor's screencopy `ready` event still carries no fence of
//! its own for a dmabuf copy, and the NVIDIA driver adds no implicit dmabuf
//! sync either, so this trusts `ready` at face value for each image exactly
//! as the single-buffered version did for its one.
//!
//! `renderer: auto` (the default) picks the GPU path when the compositor
//! offers dmabuf capture and Vulkan initializes, falling back to the CPU
//! path — logged, with the reason — otherwise; `cpu`/`gpu` force one or the
//! other, `gpu` fatally if it turns out not to be available. See
//! `decide_backend` and the doc on [`crate::model::Renderer`] itself.
//!
//! ## Presentation gating
//!
//! Filming two outputs at once with a fast shutter used to show the frame
//! counters off by one much of the time, for two independent reasons. First,
//! outputs on a GPU without genlock hardware have independent vblank phase,
//! fixed at mode-set: a commit that lands between output A's flip and output
//! B's flip puts frame N on A and N+1 on B. Second, this process never used
//! to learn when an output had actually taken a commit — it just committed
//! the next frame whenever the next capture arrived.
//!
//! The rule that addresses both: **the next commit to the wall goes out only
//! once every gated output has reported `presented` (or `discarded`) for the
//! previous one** — `wp_presentation_feedback`, the flip itself. See
//! [`GateState`], [`State::gate_open`] and [`Presenter::feedback_pending_for`].
//! Anchoring to the flip is what makes the timing work out: every commit
//! then lands a fraction of a millisecond *after* a vblank, which is as far
//! from the next deadline as a commit can be, so a frame has a whole refresh
//! period in which to be rendered and flipped rather than a sliver. When one
//! head's flip does land a refresh late, it holds the gate for that refresh
//! and the other outputs repeat a frame — which is the trade this is for: a
//! repeat on every output beats a mismatch between them.
//!
//! **Why frame callbacks were not enough.** The gate was anchored to
//! `wl_surface.frame` until 2026-09-17, and it never held. wlroots sends
//! frame callbacks when it *commits* an output's frame, before the page flip
//! lands, so a head whose flip misses the driver's deadline and lands a
//! vblank late answers the callback exactly on time and the gate opens
//! anyway. Measured on `brain` (test-log Entry 2) with the `sync` pattern:
//! all three outputs presented every frame at 60 fps — `presented 600` each,
//! no discards, no stalls — while `wp_presentation` reported a mean offset
//! of exactly one refresh period, held for 30–60 seconds at a time, on both
//! the composited and the direct-scanout path. Every output taking every
//! frame while one of them is a whole frame behind is precisely the
//! signature of a gate that is watching the wrong event. Frame callbacks are
//! kept as the fallback for a compositor that offers no `wp_presentation` at
//! all (none of the reference machines is one): pacing on the earlier signal
//! is still better than not pacing.
//!
//! An output that answers neither within [`STALL_TIMEOUT`] is dropped from
//! the gate — it still receives commits, so it is current the moment it
//! comes back, but it stops holding anyone else up. That is what keeps a
//! DPMS-off projector from freezing the wall, and it applies to the feedback
//! wait exactly as it applied to the callback wait.
//!
//! What the gate decides is *when* the newest frame is committed, not which
//! one: the canvas keeps rendering on its own clock regardless, and the
//! slicer presents whatever the newest completed capture is, dropping or
//! repeating frames symmetrically across every output when the clocks beat
//! against each other. `gateHolds` in the periodic report counts the cycles
//! where that wait ran past a canvas period — the wall's own measure of how
//! often it repeated a frame to stay together. `free_run` turns the
//! cross-output half of the rule off entirely (see [`State::gate_open`]);
//! each output still waits for its own answer before taking another frame,
//! which is the same anti-race the gate began as.
//!
//! ## Sync pattern
//!
//! [`crate::model::TestPattern::Sync`] is the one test pattern this module
//! draws itself, per frame, through the present path above rather than once
//! into a static buffer — see `run_sync`. It exists because the question
//! "are these two projectors showing the same frame?" had, until it, no
//! answer that was about Suede. The way it was asked before was to point a
//! high-speed camera at a browser page whose cells all count in step; a
//! photograph of two outputs showing different numbers then implicates the
//! whole chain — Chromium's own render and present, sway's composite, the
//! flip — with no way to say which link is the loose one. A counter drawn
//! here has the browser taken out of it: the picture leaves this process,
//! goes through the compositor, and reaches the plane, and nothing else
//! touches it. So the same photograph, of the same two projectors, becomes a
//! measurement of *this* path, and is comparable frame for frame between
//! direct scanout on and off.
//!
//! Read it against `straddles` in the same interval's report line, which
//! counts how often the compositor's own presentation feedback put two
//! outputs on different refreshes. The two together say more than either
//! alone:
//!
//! - **Same digits on every output, `straddles` 0** — in step, and the
//!   driver's flip reports agree with the light. This is the answer the
//!   gate exists to produce.
//! - **Digits differ, `straddles` 0** — the frames were committed and
//!   reported as one, and the glass says otherwise: the disagreement is
//!   below the compositor, in the flip timing or the panel, not in this
//!   loop's gating. This is the case the pattern was built to be able to
//!   state, because the counters were the only witness to it.
//! - **Digits differ, `straddles` non-zero** — the loop already knew; the
//!   photograph is confirming the count, not adding to it.
//!
//! The binary strip beside the digits carries the low sixteen bits of the
//! same counter, which is what makes a one-frame difference readable when a
//! DLP projector's color wheel has smeared the digits across the exposure —
//! and what distinguishes 99 → 00 from a stall. Photograph two consecutive
//! frames on DLP for the same reason. The pattern needs the slicer, so it
//! needs `allow_overlaps = true`; the tiled path's static overlays show a
//! placeholder saying so rather than a counter frozen at whatever number it
//! started on, which would read as perfect sync.
//!
//! ## Direct scanout
//!
//! Each presenter's buffer already *is* a fullscreen, opaque, output-sized
//! image of exactly what that projector should show, so the compositor's
//! own render pass over it is pure cost: wlroots can instead flip the
//! slicer's dmabuf straight to the KMS plane and skip compositing that
//! output entirely. Doing so needs four things of the client, and the GPU
//! path arranges all four: a fullscreen layer surface (the anchors and
//! `set_exclusive_zone(-1)` at presenter creation), an opaque region
//! covering the whole surface (set from the layer surface's `Configure`,
//! since that is where the size comes from), a buffer that is exactly the
//! output's pixel grid, and a dmabuf whose DRM modifier the display
//! controller accepts — the compositor names those in the `scanout`-flagged
//! tranches of its dmabuf feedback (see [`dmabuf::FeedbackCollector`]), and
//! [`create_present_buffers`] allocates from the intersection of that set
//! with what this device can render to, falling back to the wider set when
//! they share nothing. Nothing here is load-bearing: a buffer that cannot be
//! scanned out is composited exactly as before, and the shm path (whose
//! buffers can never be scanned out) is untouched.
//!
//! Scanning out changes how long the compositor keeps a buffer, and that is
//! a correctness matter, not a tuning one. Compositing releases a buffer as
//! soon as sway has rendered from it; a flipped buffer is being read by the
//! display controller for as long as it is on screen, so it comes back only
//! after the *following* flip has completed. A scanning-out sway therefore
//! holds two of an output's buffers at once — on screen, and queued — and
//! the two-slot rotation this path started with then had nothing free at
//! the next commit and overwrote a buffer still on the display. Hence
//! [`GPU_PRESENT_SLOTS`] = 3 (on screen, queued, being drawn into) against
//! the shm path's [`CPU_PRESENT_SLOTS`] = 2; `buffer reuse` in the periodic
//! report line is the counter that catches getting this wrong.
//!
//! Two things report on scanout, and only one of them is proof. The startup
//! line (`scanout candidate: ...`, one per output) is a *diagnostic*: it
//! says which precondition a wall is failing, and it is trusted only for
//! the conditions this process can check for itself — the buffer's size
//! against the output's mode, and the modifier the driver actually chose.
//! It cannot say "no" from the dmabuf feedback alone. On `brain` (test-log
//! Entry 2) the default feedback named no scanout tranche at all and every
//! presented frame was nonetheless zero-copy, so an absent tranche is
//! reported as `unconfirmed`. The proof is `zeroCopyPresented` in
//! `GET /projection/stats`, counted from the compositor's own
//! `wp_presentation_feedback` flags: that is what the compositor did, not
//! what it advertised.
//!
//! The buffer-size condition is the one this process does not paper over.
//! The slicer never calls `wl_surface.set_buffer_scale`, so its buffers are
//! the surface-local size the compositor configured — which equals the
//! output's pixel size only at scale 1. A scaled output therefore cannot be
//! scanned out, and the startup line says so rather than the slicer
//! silently rescaling: a projector wall is a pixel-exact mapping, and a
//! scaled output would already be wrong for reasons that have nothing to do
//! with scanout.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_output::{self, WlOutput},
    wl_region::WlRegion,
    wl_registry::{self, WlRegistry},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
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

use super::blend::{Coverage, OverlaySpec, SlicerSpec};
use super::pattern::{SyncGroup, SyncRect};
use super::{dmabuf, gpu};
use crate::model::{
    CaptureIntervals, FrameCost, LagFrames, OutputTiming, PresentationOffset, ProjectionStats,
    Renderer, TestPattern,
};

/// DRM fourccs the GPU present path asks for, in preference order — see the
/// "Present buffers" section this mirrors. Duplicated from `gpu.rs`'s own
/// (private) constants rather than importing them: that module must not be
/// edited to export them, and the values are part of the stable DRM fourcc
/// namespace, not something that can drift between the two files.
const FOURCC_XR24: u32 = 0x3432_5258; // XRGB8888
const FOURCC_XB24: u32 = 0x3432_4258; // XBGR8888

/// Capture images kept in flight on the GPU path, alternated by
/// `Capture.gpu_slot` — see the module doc's paragraph on why two, not the
/// single image this started as.
const GPU_CAPTURE_SLOTS: usize = 2;

/// Present buffers each GPU-path presenter owns, rotated by `Presenter`'s
/// `busy`/`next_buffer`.
///
/// Three, because direct scanout changes how long the compositor keeps one.
/// A *compositing* sway releases a buffer as soon as it has rendered from
/// it, so two slots alternate cleanly: one being drawn into, one being read.
/// A sway that flips the buffer straight to the KMS plane cannot let go that
/// early — the display controller reads the on-screen buffer continuously,
/// so that buffer is only released once the *next* flip has completed.
/// Scanning out therefore holds two buffers per output at once (on screen,
/// and queued for the next vblank), leaving the slicer's next commit with no
/// free slot: it reused a busy one and overwrote a buffer still being
/// scanned out. Measured on `brain` (test-log Entry 2): `buffer reuse` went
/// from 0 in 43 consecutive sampled intervals to 609–1781 per 10 s the
/// moment scanout was enabled, with the GPU fence wait doubled and the
/// compositor's CPU up an order of magnitude. Three slots restore the
/// invariant the two-slot pool had under compositing — one free to draw
/// into while the compositor holds the others — at the cost of one more
/// output-sized image per projector.
const GPU_PRESENT_SLOTS: usize = 3;

/// Present buffers each CPU/shm-path presenter owns.
///
/// Two, and deliberately not [`GPU_PRESENT_SLOTS`]: an shm buffer is never
/// scanned out. The compositor has to read it into a buffer of its own and
/// releases it as soon as it has, exactly the compositing lifetime two slots
/// were always enough for, so there is no third holder to cover. Named
/// beside its GPU counterpart so the asymmetry between the paths reads as a
/// decision rather than an oversight.
const CPU_PRESENT_SLOTS: usize = 2;

/// How many Present buffers a presenter gets, given the backend the capture
/// side settled on. The GPU path's dmabufs can be flipped to a plane and so
/// need the extra slot ([`GPU_PRESENT_SLOTS`]); everything else presents
/// through shm and keeps [`CPU_PRESENT_SLOTS`]. `None` — no backend decided
/// yet — reads as the shm path, which is what the callers do with it.
fn present_slots(backend: Option<Backend>) -> usize {
    match backend {
        Some(Backend::Gpu) => GPU_PRESENT_SLOTS,
        _ => CPU_PRESENT_SLOTS,
    }
}

/// Consecutive capture failures tolerated before giving up. The daemon
/// respawns the slicer on its next pass, which is the retry policy.
const MAX_FAILURES: u32 = 3;

/// How long an output may leave the gate's readiness signal unanswered — a
/// `wp_presentation_feedback`, or a `wl_surface.frame` callback where the
/// compositor offers no `wp_presentation` — before it is dropped from the
/// gate. Long enough that a normal compositor hiccup or a momentarily busy
/// GPU never trips it; short enough that a DPMS-off or otherwise
/// unpresenting output does not freeze the rest of the wall for more than a
/// third of a second.
const STALL_TIMEOUT: Duration = Duration::from_millis(300);

/// How long the `sync` loop waits for the gate, in canvas periods, before
/// committing the next frame without it.
///
/// The gate is that loop's clock (see the module doc's "Presentation
/// gating"), and a clock that can stop is not one: an output whose feedback
/// is merely late — a mode-set settling, a compositor hiccup — must not
/// freeze the counter for the third of a second [`STALL_TIMEOUT`] takes to
/// drop it out. One and a half periods is past any healthy wall's gate wait
/// (whose commits land a fraction of a period after the slowest head's
/// flip) and well short of the stall timeout, so the pattern keeps moving
/// at roughly the canvas rate while a straggler is being given its chance.
const GATE_FALLBACK_PERIODS: f64 = 1.5;

/// A commit cycle's floor, in canvas periods: the gate may open sooner, but
/// the wall never commits faster than the canvas produces frames.
const GATE_FLOOR_PERIODS: f64 = 1.0;

/// Feedback answers required in one interval before "none of them were
/// `Presented`" is trusted as a signal rather than noise. On a four-projector
/// bench on 2026-09-15 a dead interval carried 605 answers per output — this
/// is set far below that, just high enough that a couple of stray answers
/// right at startup or during a resize cannot look like the wall going dark.
const MIN_FEEDBACK_ANSWERS_FOR_SELF_HEAL: u32 = 30;

/// Consecutive dead intervals (see [`DeadIntervalTracker`]) required before
/// the slicer gives up on itself and exits. Two, not one, so a single odd
/// interval — a resize settling, a momentary compositor hiccup — cannot
/// trigger a respawn on its own; the condition has to still be true ten
/// seconds later.
const CONSECUTIVE_DEAD_INTERVALS_FOR_SELF_HEAL: u32 = 2;

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
    /// The size the surface's opaque region was last set for, so a
    /// `Configure` that repeats a size it already answered does not build
    /// and destroy a `wl_region` for nothing. See the `Configure` handler.
    opaque_for: Option<(u32, u32)>,
    /// This output's current mode in pixels, as `wl_output` reported it
    /// before the presenters were created. `configured` is surface-local
    /// (logical), so the two differ exactly when the output is scaled — and
    /// a buffer that is not the output's pixel grid can never be scanned
    /// out. Diagnostic only: it decides nothing but the wording of the
    /// startup `scanout candidate:` line. `None` if the compositor never
    /// sent a current mode for it.
    output_mode: Option<(i32, i32)>,
    /// Output name, carried on the presenter so stats and stall messages can
    /// name it without threading the spec through every call.
    name: String,
    /// [`CPU_PRESENT_SLOTS`] buffers, rotated so we never write one the
    /// compositor reads. CPU path only — empty on the GPU path, which uses
    /// `gpu_buffers` instead; the two are never both populated for one
    /// presenter.
    buffers: Vec<(WlBuffer, memmap2::MmapMut)>,
    /// The GPU path's equivalent of `buffers`: the shader renders into
    /// `DmabufImage`, the compositor scans out or samples the `WlBuffer`
    /// wrapping it. Same rotation semantics, same `busy`/`next_buffer`, but
    /// [`GPU_PRESENT_SLOTS`] of them — a scanned-out buffer is held across
    /// the following flip, so the compositor can hold two at once.
    gpu_buffers: Vec<(WlBuffer, gpu::DmabufImage)>,
    /// Parallel to whichever of `buffers`/`gpu_buffers` is in use: true
    /// while the compositor holds that buffer, from attach+commit until its
    /// `wl_buffer.release`.
    busy: Vec<bool>,
    next_buffer: usize,
    /// Fixed-point per-pixel transfer `(a, b)`: `out = (a·in)>>8 + b`,
    /// row-major at the configured size. Two-dimensional because seams can
    /// run on any edge — a grid corner is the product of two ramps.
    transfer: Vec<(u16, u8)>,
    /// Tagged shape table for adaptive mode; fixed tables remain byte exact.
    dynamic_table: Option<Vec<u32>>,
    warp: Option<super::warp::Warp>,
    /// [`AxisSamples`] for `warp`, rebuilt only when `warp_revision` moves
    /// past `sample_revision` — see `present_frame_cpu`. `None` whenever
    /// `warp` is `None`, exactly like the old per-pixel `warp.is_none()`
    /// check.
    sample: Option<AxisSamples>,
    sample_revision: Option<u64>,
    /// Research revision carried by this output, independent of source frames.
    warp_revision: u64,
    warp_reported_revision: Option<u64>,
    warp_submitted_revision: Option<u64>,
    /// This presenter's region of the canvas.
    source: crate::model::Rect,
    /// A `wl_surface.frame` callback is outstanding: the compositor has not
    /// yet told us it *committed* an output frame carrying our last commit.
    /// The gate's fallback signal, used where the compositor offers no
    /// `wp_presentation` — see the module doc's "Presentation gating" for
    /// why it is the fallback and not the primary.
    frame_pending: bool,
    /// When `frame_pending` was set, for the stall timeout.
    pending_since: Option<Instant>,
    /// The snapshot id of the `wp_presentation_feedback` this presenter is
    /// waiting on, `None` when it owes no answer. The gate's primary signal:
    /// it clears on `presented`/`discarded`, i.e. once the flip has actually
    /// landed (or been abandoned), which a frame callback does not wait for.
    ///
    /// Carries the id of the *oldest* unanswered commit rather than a bare
    /// flag: a gated output only ever has one outstanding at a time, but an
    /// output the stall rule has dropped keeps being committed to without
    /// the gate waiting for it, and matching each answer to the wait it
    /// belongs to is what lets such an output rejoin the gate when it
    /// recovers. See `commit_sent`.
    feedback_pending_for: Option<u64>,
    /// When `feedback_pending_for` was set, for the stall timeout.
    feedback_since: Option<Instant>,
    /// This output stopped answering the gate's readiness signal — most
    /// often DPMS off, or a monitor that was unplugged without sway noticing
    /// yet. It is still sent commits, so it has fresh content the moment it
    /// comes back, but it no longer holds up the other outputs' gate.
    stalled: bool,
    /// The latest snapshot has not been committed to this output yet.
    stale: bool,
}

/// Which pipeline the running slicer settled on for this process's whole
/// life — decided once, at the first `arm_copy`, and never revisited (see
/// `decide_backend`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Cpu,
    Gpu,
}

/// One presenter due to take a frame this cycle, and which of its buffer
/// slots it will use — gathered by the GPU path's blend phase and
/// consumed by its present phase; see `gpu_blend_due`/`gpu_present_due`.
struct Due {
    index: usize,
    slot: usize,
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
    /// Fixed the first time `ensure_capture_buffer` runs; see
    /// `decide_backend`.
    backend: Option<Backend>,
    /// GPU path's capture images: the ones the compositor blits the canvas
    /// into and their wl_buffers, indexed by slot. Built up to
    /// `GPU_CAPTURE_SLOTS` (2) lazily, one slot at a time, the first time
    /// each is armed — see `ensure_gpu_capture_buffer` — rather than both at
    /// once, so a mid-run resize touches only the slot currently being
    /// armed and never the other, which may still be waiting to be blended.
    gpu_images: Vec<(gpu::DmabufImage, WlBuffer)>,
    /// The slot `ensure_capture_buffer`/`arm_copy` will next write into —
    /// flipped, right after `Ready`, to the *other* slot before that arm
    /// goes out, so the arm never touches the image about to be blended.
    /// See the module doc.
    gpu_slot: usize,
    /// The slot most recently blended successfully, if any — the image
    /// `present_frame_gpu`'s safety-net call reads from. Always the
    /// complement of `gpu_slot` from the moment a blend sets it until the
    /// *next* `Ready`, which is also the only window anything ever reads it
    /// in: by the time `gpu_slot` is armed to write over this same slot
    /// again, a fresh `blend()` has already moved this on to the slot it
    /// just read instead. `None` before the first successful blend.
    last_blended_slot: Option<usize>,
    /// The slot holding the newest *complete* capture, waiting for the gate
    /// to let it out. Set the moment a `Ready` lands and taken by the blend
    /// that consumes it, so a capture that arrives while the gate is shut is
    /// the one that goes out when the gate opens — rather than the older
    /// frame `last_blended_slot` still points at. Always the complement of
    /// `gpu_slot` (the slot the compositor is writing into), so blending
    /// from it never races the capture in flight. GPU path only.
    pending_slot: Option<usize>,
    /// Offered linux-dmabuf layout, when the compositor advertises one
    /// alongside the shm buffer: (format, width, height).
    dmabuf_offer: Option<(u32, u32, u32)>,
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
    /// buffer-selection comment in `present_frame`. It is also the counter
    /// that catches a pool sized for the wrong buffer lifetime: direct
    /// scanout holds one more buffer per output than compositing does, and
    /// this went from 0 to hundreds per interval on `brain` until the GPU
    /// path grew a third slot ([`GPU_PRESENT_SLOTS`]).
    buffer_reuse: u32,
    /// Commit cycles the gate held past one canvas period waiting for a
    /// straggler — see [`ProjectionStats::gate_holds`].
    gate_holds: u32,
    /// GPU path only: `gpu.blend()`'s fence-wait, summed across every call
    /// this interval. Zero on the CPU path, which never waits on a fence.
    gpu: Duration,
    /// How many canvas periods separated each capture from the previous
    /// one, bucketed — see `crate::model::CaptureIntervals`.
    capture_intervals: CaptureIntervals,
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
            gate_holds: 0,
            gpu: Duration::ZERO,
            capture_intervals: CaptureIntervals::default(),
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
    /// One accumulator per output, indexed the same as `per_output`; index 0
    /// (output 0 relative to itself) is never fed and always empty — its
    /// phase is reported separately, as a trivial 0.0, from `per_output[0]`.
    phase: Vec<PhaseAccum>,
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
    Presented {
        at_ns: u64,
        refresh_ns: u32,
        /// The `zero_copy` bit of the feedback's `flags` — the compositor
        /// scanned this output's buffer out directly, with no compositing
        /// pass, rather than blitting it into its own framebuffer.
        zero_copy: bool,
    },
    /// The compositor never showed this update (superseded, surface
    /// destroyed, ...). The feedback protocol destroys the object either
    /// way, so there is nothing further to clean up here.
    Discarded,
}

#[derive(Debug, Clone, Copy, Default)]
struct OutputAccum {
    presented: u32,
    discarded: u32,
    /// How many of `presented` carried the `zero_copy` feedback flag — the
    /// compositor scanned the slicer's buffer straight to the display
    /// controller with no compositing pass.
    zero_copy_presented: u32,
    /// `None` until a `Presented` event carries a non-zero refresh.
    refresh_ns: Option<u32>,
    /// Histogram of this output's lag behind the earliest output to present
    /// the same snapshot, in whole refresh periods — see
    /// [`crate::model::LagFrames`].
    lag_frames: LagFrames,
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
    /// One entry per non-zero output that presented alongside output 0:
    /// `(index, reduced_d_ns, period_ns)`, where `reduced_d_ns` is already
    /// folded into `(-period/2, period/2]` by `reduce_phase`. Empty whenever
    /// output 0 itself did not present this snapshot, since there is nothing
    /// to measure the others against.
    phase_samples: Vec<(usize, i64, u32)>,
    /// One entry per presented output whose own refresh is known:
    /// `(index, lag_frames)`, where `lag_frames` is that output's timestamp
    /// minus the earliest presenting output's, divided by its own refresh
    /// and rounded to the nearest whole period, clamped to 3 ("three or
    /// more"). Empty whenever fewer than two outputs presented this
    /// snapshot — a lone presenter has nothing to lag behind.
    lag_frames: Vec<(usize, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SettledOutcome {
    Presented {
        refresh_ns: Option<u32>,
        zero_copy: bool,
    },
    Discarded,
}

/// Reduce a signed nanosecond difference into `(-period/2, period/2]` — the
/// representative closest to zero for a value that really lives on a circle
/// of length `period_ns` (a vblank phase, not a linear duration: an output 15
/// ms behind at 60 Hz is 1.67 ms *ahead* on the next vblank, not 15 ms
/// behind). Pure so the wrap arithmetic can be tested without a compositor.
fn reduce_phase(d_ns: i64, period_ns: i64) -> i64 {
    if period_ns <= 0 {
        return d_ns;
    }
    // rem_euclid always lands in [0, period_ns); fold the top half back
    // negative so the result sits either side of zero, whichever is closer.
    let wrapped = d_ns.rem_euclid(period_ns);
    if wrapped * 2 > period_ns {
        wrapped - period_ns
    } else {
        wrapped
    }
}

/// Circular mean of one output's per-frame vblank phase relative to output
/// 0, accumulated over a report interval by `Timing::finalize`.
///
/// A plain mean of the reduced offsets fails exactly where this matters: two
/// outputs sitting right at the wrap boundary report values near +period/2
/// and -period/2 on alternating frames, and a linear average of those
/// collapses toward zero — reporting "locked in phase" for a pair that is
/// actually locked at the far edge. The circular mean (average the unit
/// vectors at angle `2π·d/period`, then take the angle back) does not have
/// that failure: see `circular_mean_of_a_symmetric_pair_does_not_cancel_to_zero`.
#[derive(Debug, Clone, Copy, Default)]
struct PhaseAccum {
    sum_cos: f64,
    sum_sin: f64,
    count: u32,
    min_ms: f64,
    max_ms: f64,
    /// The period the most recent sample was reduced against, in ns, reused
    /// to turn the circular mean's angle back into milliseconds. Refresh
    /// barely moves frame to frame on real hardware, so one interval's
    /// samples share close enough to the same period for this to matter.
    period_ns: u32,
}

impl PhaseAccum {
    fn add(&mut self, reduced_ns: i64, period_ns: u32) {
        let fraction = reduced_ns as f64 / f64::from(period_ns);
        let angle = std::f64::consts::TAU * fraction;
        self.sum_cos += angle.cos();
        self.sum_sin += angle.sin();
        let ms = reduced_ns as f64 / 1_000_000.0;
        if self.count == 0 {
            self.min_ms = ms;
            self.max_ms = ms;
        } else {
            self.min_ms = self.min_ms.min(ms);
            self.max_ms = self.max_ms.max(ms);
        }
        self.count += 1;
        self.period_ns = period_ns;
    }

    /// The interval's circular-mean phase in ms, `None` until at least one
    /// sample has landed.
    fn phase_ms(&self) -> Option<f64> {
        (self.count > 0).then(|| {
            let angle = self.sum_sin.atan2(self.sum_cos);
            angle / std::f64::consts::TAU * (f64::from(self.period_ns) / 1_000_000.0)
        })
    }

    /// Max minus min of the per-frame reduced value, ms — near zero for a
    /// locked pair, near a period for one that is wandering across it.
    fn spread_ms(&self) -> Option<f64> {
        (self.count > 0).then_some(self.max_ms - self.min_ms)
    }
}

/// Work out what a snapshot's fully-answered slots mean, in isolation from
/// how they got there.
fn settle(slots: &[Slot]) -> Settled {
    let mut outcomes = Vec::new();
    let mut presented_at: Vec<u64> = Vec::new();
    let mut known_refreshes: Vec<u32> = Vec::new();
    // Kept alongside `outcomes` (which erases the at_ns/refresh once turned
    // into an outcome) because the phase pass below needs to find output 0
    // specifically and compare every other presenter's timestamp against it.
    let mut presented: Vec<(usize, u64, Option<u32>)> = Vec::new();

    for (index, slot) in slots.iter().enumerate() {
        match *slot {
            Slot::Presented {
                at_ns,
                refresh_ns,
                zero_copy,
            } => {
                let refresh_ns = (refresh_ns != 0).then_some(refresh_ns);
                outcomes.push((
                    index,
                    SettledOutcome::Presented {
                        refresh_ns,
                        zero_copy,
                    },
                ));
                presented_at.push(at_ns);
                if let Some(refresh_ns) = refresh_ns {
                    known_refreshes.push(refresh_ns);
                }
                presented.push((index, at_ns, refresh_ns));
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

    // Per-output vblank phase relative to output 0 (see the module docs on
    // stable-versus-drifting phase). Nothing to measure when output 0 itself
    // did not present this snapshot.
    let mut phase_samples = Vec::new();
    if let Some(&(_, at0, refresh0)) = presented.iter().find(|&&(index, ..)| index == 0) {
        for &(index, at_ns, refresh_ns) in &presented {
            if index == 0 {
                continue;
            }
            // The smaller of the two known refreshes: reducing against the
            // faster output's period is the more conservative choice when
            // they disagree, and it is the only one either side actually
            // measured.
            let period_ns = match (refresh_ns, refresh0) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            };
            if let Some(period_ns) = period_ns {
                let d_ns = at_ns as i64 - at0 as i64;
                let reduced = reduce_phase(d_ns, i64::from(period_ns));
                phase_samples.push((index, reduced, period_ns));
            }
        }
    }

    // Per-output lag behind the earliest output to present this snapshot, in
    // whole refresh periods. A lone presenter has nothing to lag behind, so
    // this only fires alongside `offset_ns` (both require two-plus
    // presenters); unlike phase, it is not relative to output 0 specifically
    // — it is relative to whichever output happened to present first.
    let mut lag_frames = Vec::new();
    if presented.len() >= 2 {
        let earliest_at_ns = presented
            .iter()
            .map(|&(_, at_ns, _)| at_ns)
            .min()
            .unwrap_or(0);
        for &(index, at_ns, refresh_ns) in &presented {
            if let Some(refresh_ns) = refresh_ns {
                let lag = ((at_ns - earliest_at_ns) as f64 / f64::from(refresh_ns)).round() as u32;
                lag_frames.push((index, lag.min(3)));
            }
        }
    }

    Settled {
        outcomes,
        offset_ns,
        straddle,
        phase_samples,
        lag_frames,
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
    /// Same order as `outputs`: `(phase_ms, phase_spread_ms)`.
    phases: Vec<(Option<f64>, Option<f64>)>,
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
            phase: vec![PhaseAccum::default(); presenter_count],
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

    fn presented(&mut self, id: u64, index: usize, at_ns: u64, refresh_ns: u32, zero_copy: bool) {
        if let Some(slot) = self.pending.get_mut(&id).and_then(|s| s.get_mut(index)) {
            *slot = Slot::Presented {
                at_ns,
                refresh_ns,
                zero_copy,
            };
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
                SettledOutcome::Presented {
                    refresh_ns,
                    zero_copy,
                } => {
                    accum.presented += 1;
                    if zero_copy {
                        accum.zero_copy_presented += 1;
                    }
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
        for (index, reduced_ns, period_ns) in settled.phase_samples {
            if let Some(accum) = self.phase.get_mut(index) {
                accum.add(reduced_ns, period_ns);
            }
        }
        for (index, lag) in settled.lag_frames {
            if let Some(accum) = self.per_output.get_mut(index) {
                match lag {
                    0 => accum.lag_frames.zero += 1,
                    1 => accum.lag_frames.one += 1,
                    2 => accum.lag_frames.two += 1,
                    _ => accum.lag_frames.more += 1,
                }
            }
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
        // Output 0's phase relative to itself is trivially zero, and never
        // fed into `self.phase[0]` — report it as 0.0 whenever it presented
        // at all this interval, `None` otherwise, rather than via the
        // (always-empty) accumulator.
        let phases: Vec<(Option<f64>, Option<f64>)> = outputs
            .iter()
            .enumerate()
            .map(|(index, accum)| {
                if index == 0 {
                    let presented = accum.presented > 0;
                    (presented.then_some(0.0), presented.then_some(0.0))
                } else {
                    let accum = &self.phase[index];
                    (accum.phase_ms(), accum.spread_ms())
                }
            })
            .collect();

        for accum in &mut self.per_output {
            accum.presented = 0;
            accum.discarded = 0;
            accum.zero_copy_presented = 0;
            accum.lag_frames = LagFrames::default();
        }
        self.offset_sum_ms = 0.0;
        self.offset_max_ms = 0.0;
        self.offset_count = 0;
        self.straddles = 0;
        for accum in &mut self.phase {
            *accum = PhaseAccum::default();
        }

        IntervalTiming {
            offset,
            straddles,
            outputs,
            phases,
        }
    }
}

/// Whether one report interval, taken alone, is the "every frame is going
/// nowhere" condition: presentation feedback was available, a meaningful
/// number of answers came back, and not one of them was `Presented`.
///
/// Without feedback (`presentation_feedback` false) this never fires — a
/// compositor that offers no `wp_presentation` gives no grounds to conclude
/// anything about where frames landed, so silence is not evidence. Zero
/// answers must not count as "none presented" either: that is the ordinary
/// case before the compositor has answered anything at all, not the failure
/// this exists to catch, which is why `answers` is checked against
/// [`MIN_FEEDBACK_ANSWERS_FOR_SELF_HEAL`] rather than merely `> 0`. See
/// [`DeadIntervalTracker`] for why one dead interval is not enough on its own
/// to act on.
fn interval_is_dead(presentation_feedback: bool, answers: u32, presented: u32) -> bool {
    presentation_feedback && answers >= MIN_FEEDBACK_ANSWERS_FOR_SELF_HEAL && presented == 0
}

/// Counts consecutive dead intervals (see [`interval_is_dead`]) and says when
/// enough of them in a row have been seen to act on.
///
/// Exists because of a four-projector bench on 2026-09-15: the
/// `output-phase` check's fix disables and re-enables every output together,
/// which destroys and recreates them in the compositor, and the
/// already-running slicer kept presenting to the layer surfaces it had built
/// against the old ones. `GET /api/v1/projection/stats` read `presented 0
/// discarded 605` on all four outputs for a full ten-second interval while
/// the same report claimed `presentedFps 60.4` throughout — the wall was
/// black and the daemon insisted it was fine. A `systemctl --user restart
/// suede` fixed it instantly. This is not specific to that one fix: any event
/// that destroys and recreates an output does the same thing, including a
/// projector simply being unplugged and replugged, so this reasons from the
/// symptom rather than any one cause.
///
/// A single dead interval is deliberately not enough — a resize settling or a
/// momentary compositor hiccup could plausibly produce one — so this waits
/// for a second consecutive interval before reporting the condition as real.
/// Any interval with at least one `Presented` answer resets the count to
/// zero, because that proves frames are still reaching somewhere.
#[derive(Default)]
struct DeadIntervalTracker {
    consecutive: u32,
}

impl DeadIntervalTracker {
    /// Record one interval's outcome. Returns true once
    /// [`CONSECUTIVE_DEAD_INTERVALS_FOR_SELF_HEAL`] consecutive dead
    /// intervals have been seen (and keeps returning true for as long as
    /// that stays true, though the caller only needs to act on it once).
    fn record(&mut self, presentation_feedback: bool, answers: u32, presented: u32) -> bool {
        if interval_is_dead(presentation_feedback, answers, presented) {
            self.consecutive += 1;
        } else {
            self.consecutive = 0;
        }
        self.consecutive >= CONSECUTIVE_DEAD_INTERVALS_FOR_SELF_HEAL
    }
}

// --- the presentation gate ------------------------------------------------

/// Which signal the gate is anchored to. See the module doc's "Presentation
/// gating" section for why the two are not interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateSignal {
    /// The compositor offers `wp_presentation`: the gate waits for
    /// `presented`/`discarded`, which is the flip itself.
    Presentation,
    /// No `wp_presentation` at all: the gate falls back to
    /// `wl_surface.frame`, which is the compositor's output *commit* and
    /// therefore earlier — and blind to a flip that lands a vblank late.
    FrameCallback,
}

impl GateSignal {
    /// What to call this signal in an operator-facing message.
    fn answer_name(self) -> &'static str {
        match self {
            GateSignal::Presentation => "presentation feedback",
            GateSignal::FrameCallback => "frame callbacks",
        }
    }
}

/// Which signal the gate uses, given whether the compositor offers
/// `wp_presentation`. Trivial, and named anyway so the fallback rule has one
/// definition and one test rather than an `is_some()` at each use.
fn gate_signal(presentation_feedback: bool) -> GateSignal {
    if presentation_feedback {
        GateSignal::Presentation
    } else {
        GateSignal::FrameCallback
    }
}

/// One presenter's outstanding answer for the previous commit, as the gate
/// sees it — lifted off [`Presenter`] so the rule below can be exercised
/// without a compositor.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GateEntry {
    /// This presenter still owes an answer for the commit it was last sent.
    waiting: bool,
    /// How long it has owed it; `Duration::ZERO` when `waiting` is false.
    waited: Duration,
    /// Already dropped out of the gate by an earlier stall decision.
    stalled: bool,
}

impl GateEntry {
    /// Nothing outstanding, as far as the gate is concerned: either the
    /// answer came back, or this presenter has been dropped out of the gate
    /// — already (`stalled`) or by this very call, because it has owed its
    /// answer for longer than `limit`.
    fn answered(&self, limit: Duration) -> bool {
        !self.waiting || self.stalled || self.waited > limit
    }
}

/// Build one presenter's gate entry from both signals' waits, picking
/// whichever the gate is anchored to. `frame`/`feedback` are `Some(waited)`
/// while that signal's answer is outstanding and `None` once it is in.
fn gate_entry(
    signal: GateSignal,
    frame: Option<Duration>,
    feedback: Option<Duration>,
    stalled: bool,
) -> GateEntry {
    let waited = match signal {
        GateSignal::Presentation => feedback,
        GateSignal::FrameCallback => frame,
    };
    GateEntry {
        waiting: waited.is_some(),
        waited: waited.unwrap_or_default(),
        stalled,
    }
}

/// The locked-mode gate: every presenter's outstanding answer for the
/// previous commit, plus how long one of them may go unanswered before the
/// wall stops waiting for it.
///
/// Kept as a value rather than a method on [`State`] so the rule — which is
/// the whole of this module's pacing policy — is one testable thing.
#[derive(Debug, Clone, PartialEq)]
struct GateState {
    entries: Vec<GateEntry>,
    /// [`STALL_TIMEOUT`] in the running slicer.
    limit: Duration,
}

impl GateState {
    fn new(entries: Vec<GateEntry>, limit: Duration) -> Self {
        Self { entries, limit }
    }

    /// Whether the wall may commit its next frame: every presenter has
    /// answered for the previous one, or has been waited on long enough.
    ///
    /// A wall with no presenters at all is trivially open — there is nobody
    /// to wait for, and reporting it shut would spin the loop.
    fn all_answered(&self) -> bool {
        self.entries.iter().all(|entry| entry.answered(self.limit))
    }

    /// The presenters that have just run past `limit` without answering and
    /// are not yet marked stalled, with how long each has been waiting —
    /// the ones `State::new_snapshot` drops out of the gate and counts as
    /// stalls.
    fn newly_stalled(&self) -> Vec<(usize, Duration)> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.waiting && !entry.stalled && entry.waited > self.limit)
            .map(|(index, entry)| (index, entry.waited))
            .collect()
    }
}

impl Presenter {
    /// This presenter's outstanding answer under `signal`, as of `now`.
    fn gate_entry(&self, signal: GateSignal, now: Instant) -> GateEntry {
        let since = |set: bool, at: Option<Instant>| {
            set.then(|| {
                at.map(|at| now.saturating_duration_since(at))
                    .unwrap_or_default()
            })
        };
        gate_entry(
            signal,
            since(self.frame_pending, self.pending_since),
            since(self.feedback_pending_for.is_some(), self.feedback_since),
            self.stalled,
        )
    }

    /// Whether this presenter alone is ready for another commit. The gate's
    /// per-output half: free-run asks only this, locked mode asks it of
    /// every presenter at once through [`GateState::all_answered`].
    fn gate_ready(&self, signal: GateSignal) -> bool {
        self.gate_entry(signal, Instant::now())
            .answered(STALL_TIMEOUT)
    }

    /// Record that a commit carrying `snapshot_id` has just gone out to this
    /// presenter, with `feedback` true when a `wp_presentation_feedback` was
    /// requested alongside its `wl_surface.frame` callback.
    ///
    /// The feedback wait tracks the *oldest* unanswered commit, not the
    /// newest: a presenter the stall rule has dropped keeps being committed
    /// to without the gate waiting for it, so advancing the wait on every
    /// commit would leave its answers permanently one id behind the wait
    /// they are matched against — and an output that had recovered would
    /// never be let back into the gate.
    fn commit_sent(&mut self, snapshot_id: u64, feedback: bool) {
        let now = Instant::now();
        self.frame_pending = true;
        self.pending_since = Some(now);
        if feedback && self.feedback_pending_for.is_none() {
            self.feedback_pending_for = Some(snapshot_id);
            self.feedback_since = Some(now);
        }
    }
}

struct State {
    /// Third element: this output's current-mode refresh in mHz, from
    /// `wl_output`'s `Mode` event — used only to size the canvas period for
    /// the capture-interval histogram (see `report_stats_if_due`). Fourth:
    /// the registry `name` this output's `wl_output` global was bound under —
    /// not to be confused with the output's own advertised name string in
    /// the second element — kept so a later `wl_registry::GlobalRemove` can
    /// be matched back to it. See `used_outputs` and the
    /// `Dispatch<WlRegistry, GlobalListContents>` impl.
    outputs: Vec<(WlOutput, Option<String>, Option<i32>, u32)>,
    /// Registry names (the same numbering as `outputs`' fourth element, and
    /// what `wl_registry::Event::GlobalRemove` names) of the outputs this
    /// slicer actually captures from or presents to — a subset of `outputs`,
    /// which also holds every other `wl_output` global the compositor
    /// advertises. Populated once in `run`, after `source` and every
    /// presenter's target are resolved.
    used_outputs: Vec<u32>,
    /// Registry name -> the current mode's size in *pixels*, from the same
    /// `wl_output::Mode` event the refresh comes from. Compared against a
    /// presenter's configured (surface-local) size to tell whether the
    /// buffer it commits is exactly the output's pixel grid — the
    /// precondition for direct scanout that nothing else here checks. See
    /// `Presenter.output_mode`.
    output_modes: HashMap<u32, (i32, i32)>,
    /// Kept so the layer-surface `Configure` handler can build a new opaque
    /// region at the size it was just given. `None` only in unit tests,
    /// which construct a `State` with no live Wayland objects at all.
    compositor: Option<WlCompositor>,
    presenters: Vec<Presenter>,
    /// Set once at startup by `negotiate_gpu`, when `renderer != Cpu` and
    /// dmabuf feedback completed and `Gpu::new` succeeded. `None` either
    /// because the renderer is forced to `Cpu`, or because something in
    /// that chain failed — `gpu_error` says what, for `decide_backend`'s
    /// fallback message.
    ///
    /// Declared ahead of `capture` so it is also *dropped* ahead of it:
    /// `Gpu::drop` waits the device idle, and an asynchronous source
    /// measurement may still be reading a capture image when the slicer
    /// stops (on any of `run`'s error paths, not only the clean one). The
    /// images live in `capture` and destroy themselves when dropped, so the
    /// wait has to come first.
    gpu: Option<gpu::Gpu>,
    capture: Capture,
    closed: bool,
    /// Counts consecutive intervals where feedback came back for nobody; see
    /// `DeadIntervalTracker`.
    dead_intervals: DeadIntervalTracker,
    /// From `SlicerSpec.free_run`; see the type it lives on for the tradeoff.
    free_run: bool,
    /// From `SlicerSpec.renderer`; see `decide_backend` for how this turns
    /// into `Capture.backend`.
    renderer: Renderer,
    /// `None` when the compositor does not offer `wp_presentation`. This is
    /// what [`gate_signal`] reads: with it the gate waits for the flip,
    /// without it for the compositor's output commit. Every reference
    /// machine offers it; the fallback exists so a compositor that does not
    /// still paces rather than free-runs.
    presentation: Option<WpPresentation>,
    timing: Timing,
    stats: FrameStats,
    /// What the compositor's main device advertises, and the subset of it
    /// flagged for direct scanout; both empty until `negotiate_gpu` runs
    /// (or if it never does).
    gpu_formats: DmabufFormats,
    /// Why `gpu` is `None`, when it is — the step `negotiate_gpu` gave up at.
    gpu_error: Option<String>,
    /// Fourcc and modifiers `create_present_buffers` settled on for every
    /// output's Present images (every output shares one format — see
    /// `gpu.rs`'s note on `ensure_pipeline`), reused if a presenter's
    /// surface is reconfigured to a new size mid-run.
    present_format: Option<(u32, PresentModifiers)>,
    /// Staging area for one `zwp_linux_dmabuf_feedback_v1` exchange; see
    /// `negotiate_gpu` and the `Dispatch` impl below.
    dmabuf_feedback: Option<dmabuf::FeedbackCollector>,
    /// `1000 / canvas refresh`, or an assumed 60 Hz's 16.667 ms when the
    /// canvas output reports no refresh — the bucket width for
    /// `FrameStats.capture_intervals`.
    canvas_period_ms: f64,
    /// When the previous capture's `Ready` was handled, for the interval
    /// histogram.
    last_capture_at: Option<Instant>,
    /// When the gate was first seen holding a frame the wall was otherwise
    /// ready to commit, since the last commit cycle. `None` between the
    /// commit that cleared it and the next time the gate is found shut with
    /// something to show — which, on a healthy wall, is never. See
    /// [`ProjectionStats::gate_holds`].
    gate_blocked_since: Option<Instant>,
    warp_updates: Option<super::warp_update::Controller>,
    warp_available: bool,
    /// Exact unit-density copies can use an unfilterable capture image. A
    /// fractional shared crop needs linear filtering and falls back to CPU
    /// under `renderer: auto` when the selected modifier cannot provide it.
    requires_linear_sampling: bool,
    /// Immutable diagnostic sources; each output retains its connector labels.
    static_canvases: Vec<gpu::StaticCanvas>,
    /// A failed draw keeps the retained source dirty and retries on a bounded timer.
    gpu_retry_after: Option<Instant>,
    adaptive: Option<LiftRuntime>,
    /// Advances only on completed source captures, never on repaint ticks.
    capture_id: u64,
    /// The most recent measurement the GPU has handed back, or the most
    /// recent failure to obtain one. Deliberately outlives the capture it
    /// describes: the pass that produced it was submitted some captures ago
    /// and the controller's time constants make that irrelevant.
    capture_measurement: Option<CapturedLuminance>,
    /// When the next measurement may be submitted, and which capture slot
    /// one already submitted is still reading. See
    /// [`super::adaptive::MeasurementSchedule`].
    measurement: super::adaptive::MeasurementSchedule,
    /// Owns the process's stdout so lifecycle events and periodic telemetry
    /// go through a writer thread instead of this (render) thread blocking
    /// on, or panicking from, `println!`. See `control::StdoutWriter`.
    stdout: super::control::StdoutWriter,
}

/// Kept across transfer-mode/settings changes so a static capture can be
/// reused without a second GPU measurement or a new screencopy request.
#[derive(Clone)]
struct CapturedLuminance {
    capture_id: u64,
    result: Result<(f64, u32), String>,
    measured_at_unix_ms: u64,
    /// Submission to collection, in milliseconds — see
    /// [`super::gpu::LuminanceSample::latency_ms`]. Nothing waits for it, so
    /// it is a pipeline latency rather than a cost the frame loop paid.
    latency_ms: f64,
}

/// One controller and one uniform value for the entire wall. The measurement
/// identity is independent of presentation feedback and configuration IDs.
struct LiftRuntime {
    controller: super::adaptive::AdaptiveController,
    maximum: u32,
    canvas_size: (u32, u32),
    session: String,
    generation: u64,
    next_tick: Option<Instant>,
    next_report: Instant,
    status: crate::model::observed::ProjectionBlackLiftStatus,
    /// A `BlackLift` report is periodic telemetry (rate-limited to 10 Hz
    /// below), so it goes through the coalescing side of `StdoutWriter`: a
    /// slow reader loses only staleness, never correctness.
    stdout: super::control::StdoutWriter,
}

impl LiftRuntime {
    /// Publish the current adaptive status when its 100 ms reporting window
    /// permits it. Returns whether an event was emitted, which keeps callers
    /// and tests from confusing an updated local status with a reported one.
    fn report(&mut self, now: Instant, force: bool) -> bool {
        if !force && now < self.next_report {
            return false;
        }
        self.next_report = now + Duration::from_millis(100);
        self.status.target = self.controller.target();
        self.status.applied = self.controller.level();
        self.status.sample_age_ms = self.status.measured_at_unix_ms.map(|at| {
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64)
                .saturating_sub(at)
        });
        let event = super::control::ControlEvent::new(
            self.session.clone(),
            self.generation,
            super::control::ControlEventKind::BlackLift {
                status: Some(self.status.clone()),
            },
        );
        if let Ok(line) = serde_json::to_string(&event) {
            self.stdout.telemetry(line);
            true
        } else {
            false
        }
    }
}

fn configure_adaptive(state: &mut State, spec: &SlicerSpec, generation: u64) {
    let now = Instant::now();
    let Some(config) = spec.adaptive_lift else {
        if state.adaptive.take().is_some() {
            let event = super::control::ControlEvent::new(
                spec.control_session.clone(),
                generation,
                super::control::ControlEventKind::BlackLift { status: None },
            );
            if let Ok(line) = serde_json::to_string(&event) {
                // A one-time mode transition (adaptive turned off), not
                // periodic telemetry: guaranteed delivery, not coalescing.
                state.stdout.event(line);
            }
        }
        return;
    };
    let maximum = super::warp_update::maximum_coverage(spec);
    if let Some(runtime) = &mut state.adaptive {
        if runtime.controller.config() == config {
            runtime.generation = generation;
            runtime.maximum = maximum;
            runtime.report(now, true);
            return;
        }
    }
    let mut controller = super::adaptive::AdaptiveController::new(config, now);
    let paused = spec.pattern.is_some();
    let reason = if paused {
        controller.set_paused(true, now);
        Some("Calibration pattern suspends source measurement and adaptation".into())
    } else if state.capture.backend != Some(Backend::Gpu) {
        controller.unavailable();
        Some(
            "Source measurement requires the GPU capture path; using configured fixed level".into(),
        )
    } else {
        Some("Waiting for the first source measurement".into())
    };
    state.adaptive = Some(LiftRuntime {
        controller, maximum,
        canvas_size: (spec.canvas_width as u32, spec.canvas_height as u32),
        session: spec.control_session.clone(), generation, next_tick: None,
        next_report: now,
        stdout: state.stdout.clone(),
        status: crate::model::observed::ProjectionBlackLiftStatus {
            metric: "Mean linear Rec.709 luminance after sRGB decoding; nearest cell centers on a source-canvas grid up to 256x256, including unused canvas, excluding allocation padding".into(),
            available: false, paused, stale: false, reason,
            capture_id: None, sample_count: 0, luminance: None,
            measured_at_unix_ms: None, sample_age_ms: None,
            target: config.level, applied: config.level,
            logical_generation: state.timing.snapshot_id, measurement_ms: None,
        },
    });
    state.adaptive.as_mut().unwrap().report(now, true);
    if let Some(slot) = state
        .capture
        .pending_slot
        .or(state.capture.last_blended_slot)
    {
        measure_adaptive(state, slot);
    }
}

/// Unix milliseconds now, for the measurement timestamps in the status.
fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Take delivery of a measurement the GPU has already finished. Never waits:
/// an unsignalled fence simply means the answer arrives on a later capture.
fn poll_measurement(state: &mut State) {
    let Some(gpu) = state.gpu.as_mut() else {
        return;
    };
    match gpu.poll_luminance_measurement() {
        Ok(None) => {}
        Ok(Some(sample)) => {
            state.measurement.completed();
            store_measurement(state, sample);
        }
        Err(error) => {
            // The pass is unusable; nothing is in flight to wait for and the
            // schedule's rate limit keeps the retry off the frame path.
            state.measurement.completed();
            let capture_id = state.capture_id;
            store_measurement_error(state, capture_id, &error);
        }
    }
}

/// Record a collected sample against the capture it actually measured.
fn store_measurement(state: &mut State, sample: super::gpu::LuminanceSample) {
    state.capture_measurement = Some(CapturedLuminance {
        capture_id: sample.tag,
        result: Ok((sample.luminance, sample.sample_count)),
        // The sample is of the frame that was captured when the pass was
        // submitted, not of the moment its fence was noticed, so the age the
        // status reports counts from there.
        measured_at_unix_ms: unix_ms_now().saturating_sub(sample.latency_ms as u64),
        latency_ms: sample.latency_ms,
    });
}

fn store_measurement_error(state: &mut State, capture_id: u64, error: &anyhow::Error) {
    state.capture_measurement = Some(CapturedLuminance {
        capture_id,
        result: Err(format!("{error:#}")),
        measured_at_unix_ms: unix_ms_now(),
        latency_ms: 0.0,
    });
}

/// Ask the GPU for a measurement of the capture just completed into `slot`,
/// if the schedule allows one. Returns without waiting for any result.
fn submit_measurement(state: &mut State, slot: usize, canvas: (u32, u32), now: Instant) {
    if !state
        .measurement
        .should_submit(now, slot, state.capture.gpu_slot)
    {
        return;
    }
    let capture_id = state.capture_id;
    let y_invert = state.capture.y_invert;
    let submitted = match (state.gpu.as_mut(), state.capture.gpu_images.get(slot)) {
        (Some(gpu), Some((image, _))) => {
            gpu.submit_luminance_measurement(image, y_invert, canvas.0, canvas.1, capture_id)
        }
        _ => Err(anyhow::anyhow!("Completed GPU source image unavailable")),
    };
    match submitted {
        Ok(()) => state
            .measurement
            .submitted(super::adaptive::MeasurementFlight {
                capture_id,
                slot,
                submitted_at: now,
            }),
        Err(error) => {
            state.measurement.refused(now);
            store_measurement_error(state, capture_id, &error);
        }
    }
}

/// Make sure no measurement is still reading `slot`'s capture image, because
/// it is about to be handed back to the compositor (or destroyed).
///
/// The GPU-side read is ordered ahead of the blend of the same capture, and
/// `blend()` waits on its own fence, so on a presenting wall this finds the
/// pass long finished and costs one `vkGetFenceStatus`. The wait exists for
/// the case where the gate withheld the frame and no blend ran at all: the
/// compositor writes into these images with no fence of its own, so the
/// alternative to waiting is sampling a frame while it is overwritten.
fn retire_measurement_for_slot(state: &mut State, slot: usize) {
    if !state.measurement.reads_slot(slot) {
        return;
    }
    state.measurement.completed();
    let Some(gpu) = state.gpu.as_mut() else {
        return;
    };
    match gpu.finish_luminance_measurement() {
        Ok(Some((sample, waited))) => {
            if waited {
                eprintln!(
                    "slicer: waited for the source measurement of capture {} before reusing \
                     its capture slot",
                    sample.tag
                );
            }
            store_measurement(state, sample);
        }
        Ok(None) => {}
        Err(error) => {
            let capture_id = state.capture_id;
            store_measurement_error(state, capture_id, &error);
        }
    }
}

/// Retire whatever measurement is in flight, whichever slot it reads. Used
/// where the capture stream is being restarted and neither slot's contents
/// or identity can be relied on any longer.
fn retire_measurement(state: &mut State) {
    if let Some(flight) = state.measurement.flight() {
        retire_measurement_for_slot(state, flight.slot);
    }
}

/// Called once for each completed capture, while its slot is still owned by
/// the slicer. No presenter or retained-capture repaint calls measurement.
///
/// Nothing here blocks: a measurement already finished by the GPU is taken
/// delivery of, a new one may be queued, and the controller is advanced with
/// whatever the most recent answer was. The value applied is therefore
/// typically a capture or two old, which the controller's 250 ms-1 s time
/// constants make immaterial, and the status reports which capture it came
/// from so that staleness is visible rather than implied.
fn measure_adaptive(state: &mut State, slot: usize) -> bool {
    let Some(runtime) = state.adaptive.as_ref() else {
        return false;
    };
    if runtime.controller.paused() || state.capture.backend != Some(Backend::Gpu) {
        return false;
    }
    let was_available = runtime.status.available;
    let canvas_size = runtime.canvas_size;
    let now = Instant::now();
    poll_measurement(state);
    submit_measurement(state, slot, canvas_size, now);
    let (Some(runtime), Some(measurement)) =
        (state.adaptive.as_mut(), state.capture_measurement.as_ref())
    else {
        // Measurement was requested and nothing has come back yet: the
        // controller stays at its configured fixed level, reported as
        // startup, until the first pass is collected.
        return false;
    };
    runtime.status.measurement_ms = Some(measurement.latency_ms);
    // A settled source has no timer. Do not apply minutes of idle time to
    // a target that arrived only with this capture's scene cut.
    if !runtime.controller.needs_tick() {
        runtime.controller.tick(now);
    }
    match measurement.result.clone() {
        Ok((luminance, count)) => {
            // Tagged with the capture the pass sampled, which is the one
            // this value describes — not whichever capture was current when
            // its fence happened to be noticed.
            runtime.controller.measure_with_capture(
                luminance,
                u64::from(count),
                Some(measurement.capture_id),
            );
            if count > 0 && luminance.is_finite() {
                runtime.status.available = true;
                runtime.status.stale = false;
                runtime.status.reason = None;
                runtime.status.capture_id = Some(measurement.capture_id);
                runtime.status.sample_count = count;
                runtime.status.luminance = Some(luminance);
                runtime.status.measured_at_unix_ms = Some(measurement.measured_at_unix_ms);
                // Age is derived from that timestamp wherever the status is
                // published (`LiftRuntime::report`, `snapshot.rs`). Claiming
                // zero here would have been honest only while the readback
                // happened inside this call; a collected sample describes a
                // capture that is already a frame or two old.
            } else {
                runtime.status.available = false;
                runtime.status.stale = true;
                runtime.status.reason =
                    Some("Zero samples: retaining the last valid target".into());
            }
        }
        Err(error) => {
            runtime.controller.unavailable();
            runtime.status.available = false;
            runtime.status.stale = false;
            runtime.status.reason = Some(format!("Source measurement unavailable: {error:#}"));
        }
    }
    runtime.status.logical_generation = state.timing.snapshot_id;
    // The first valid sample must clear "waiting" even when it already puts
    // the controller at its target and therefore schedules no controller
    // tick. Once a valid status has been reported, later moving-source
    // captures retain the normal 100 ms report bound.
    let report_now = !runtime.status.available || !was_available;
    let reported = runtime.report(Instant::now(), report_now);
    runtime.next_tick = if runtime.controller.needs_tick() {
        let controller_tick = Instant::now() + super::adaptive::CONTROLLER_TICK;
        Some(
            runtime
                .next_tick
                .map_or(controller_tick, |scheduled| scheduled.min(controller_tick)),
        )
    } else if reported {
        None
    } else {
        // A settled controller has no adaptation tick to wake it after a
        // throttled report. Reuse the report deadline as a report-only wakeup
        // so a later static-source sample cannot remain invisible forever.
        Some(runtime.next_report)
    };
    reported
}

fn tick_adaptive(state: &mut State) {
    tick_adaptive_at(state, Instant::now());
}

fn tick_adaptive_at(state: &mut State, now: Instant) {
    let Some(runtime) = &mut state.adaptive else {
        return;
    };
    if runtime.next_tick.is_none_or(|at| now < at) {
        return;
    }
    let tick = runtime.controller.tick(now);
    runtime.next_tick = runtime
        .controller
        .needs_tick()
        .then_some(now + super::adaptive::CONTROLLER_TICK);
    if tick.changed {
        // Presentation identity advances, but the completed capture ID and
        // measurement timestamp stay unchanged while static content settles.
        state.new_snapshot();
    }
    let runtime = state.adaptive.as_mut().unwrap();
    runtime.status.logical_generation = state.timing.snapshot_id;
    runtime.report(now, tick.settled);
}

/// Never draw into a buffer still owned by the compositor.
fn free_present_slot(busy: &[bool], next: usize) -> Option<usize> {
    (0..busy.len())
        .map(|step| (next + step) % busy.len())
        .find(|&slot| !busy[slot])
}

impl State {
    fn all_configured(&self) -> bool {
        self.presenters.iter().all(|p| p.configured.is_some())
    }

    /// Which readiness signal this run's gate is anchored to — presentation
    /// feedback wherever the compositor offers `wp_presentation`, frame
    /// callbacks otherwise.
    fn gate_signal(&self) -> GateSignal {
        gate_signal(self.presentation.is_some())
    }

    /// The locked-mode gate as it stands right now.
    fn gate_state(&self, now: Instant) -> GateState {
        let signal = self.gate_signal();
        GateState::new(
            self.presenters
                .iter()
                .map(|p| p.gate_entry(signal, now))
                .collect(),
            STALL_TIMEOUT,
        )
    }

    /// Whether the gate permits a commit right now, regardless of whether
    /// there is anything new to show.
    ///
    /// Locked (`!free_run`): every presenter must have answered for the
    /// previous commit before any of them may take a newer frame than its
    /// neighbors — a partial commit here is exactly the race the gate
    /// exists to close. Free-run: one ready presenter is enough, because
    /// each takes the newest frame the instant it can, without regard for
    /// its neighbors' pace.
    fn gate_open(&self) -> bool {
        if self.free_run {
            let signal = self.gate_signal();
            self.presenters.iter().any(|p| p.gate_ready(signal))
        } else {
            self.gate_state(Instant::now()).all_answered()
        }
    }

    /// Whether there is a stale snapshot at least one presentation policy
    /// permits showing right now: the gate is open *and* somebody has
    /// something newer to show.
    fn can_present(&self) -> bool {
        if self.gpu_retry_after.is_some_and(|at| Instant::now() < at) {
            return false;
        }
        if self.free_run {
            let signal = self.gate_signal();
            self.presenters.iter().any(|p| {
                p.stale
                    && p.gate_ready(signal)
                    && free_present_slot(&p.busy, p.next_buffer).is_some()
            })
        } else {
            self.presenters
                .iter()
                .any(|p| p.stale && free_present_slot(&p.busy, p.next_buffer).is_some())
                && self
                    .presenters
                    .iter()
                    .filter(|p| p.stale && !p.stalled)
                    .all(|p| free_present_slot(&p.busy, p.next_buffer).is_some())
                && self.gate_open()
        }
    }

    /// A fresh frame is ready to show: mark every presenter due to take it,
    /// and drop any output that has stopped answering the gate's readiness
    /// signal so it cannot freeze the rest of the wall.
    fn new_snapshot(&mut self) {
        // If every presenter is still waiting on the snapshot this one is
        // about to replace, no output ever showed it at all.
        if self.presenters.iter().all(|p| p.stale) {
            self.stats.superseded += 1;
        }
        self.timing.snapshot_id += 1;
        let answer = self.gate_signal().answer_name();
        let newly_stalled = self.gate_state(Instant::now()).newly_stalled();
        for presenter in &mut self.presenters {
            presenter.stale = true;
        }
        for (index, waited) in newly_stalled {
            let Some(presenter) = self.presenters.get_mut(index) else {
                continue;
            };
            eprintln!(
                "slicer: output {} stopped answering {answer} after {:.0} ms; \
                 dropping it from the gate until it recovers",
                presenter.name,
                waited.as_secs_f64() * 1000.0,
            );
            presenter.stalled = true;
            self.stats.stalls += 1;
        }
    }

    /// Note that the wall has a frame it would commit right now but for the
    /// gate. Idempotent within one commit cycle: the *first* such
    /// observation is the one [`ProjectionStats::gate_holds`] measures from.
    fn note_gate_blocked(&mut self) {
        if self.gate_blocked_since.is_none() {
            self.gate_blocked_since = Some(Instant::now());
        }
    }

    /// Close off a commit cycle: if the gate had been holding a ready frame,
    /// decide whether that wait was long enough to count as a hold.
    ///
    /// `forced` is the `sync` loop's fallback commit (see
    /// [`GATE_FALLBACK_PERIODS`]) — the gate never opened at all, which is a
    /// hold however briefly it was measured.
    fn note_commit_cycle(&mut self, forced: bool) {
        let Some(since) = self.gate_blocked_since.take() else {
            return;
        };
        let held_ms = since.elapsed().as_secs_f64() * 1000.0;
        if forced || held_ms > self.canvas_period_ms {
            self.stats.gate_holds += 1;
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

        // Built once, then reused for both the human-readable line below and
        // the JSON `outputs` field, so the two never drift apart.
        let outputs: Vec<OutputTiming> = self
            .presenters
            .iter()
            .zip(interval.outputs.iter())
            .zip(interval.phases.iter())
            .map(
                |((presenter, accum), &(phase_ms, phase_spread_ms))| OutputTiming {
                    name: presenter.name.clone(),
                    presented: accum.presented,
                    discarded: accum.discarded,
                    zero_copy_presented: accum.zero_copy_presented,
                    refresh_hz: accum.refresh_ns.map(|ns| 1_000_000_000.0 / f64::from(ns)),
                    phase_ms,
                    phase_spread_ms,
                    lag_frames: accum.lag_frames,
                },
            )
            .collect();

        // The self-heal check (see `DeadIntervalTracker`): reuses this
        // interval's per-output tallies rather than accumulating a separate
        // pair, since the pass/fail signal is already sitting right here in
        // `outputs`.
        let interval_presented: u32 = outputs.iter().map(|o| o.presented).sum();
        let interval_answers: u32 = outputs.iter().map(|o| o.presented + o.discarded).sum();
        if self
            .dead_intervals
            .record(presentation_feedback, interval_answers, interval_presented)
        {
            eprintln!(
                "slicer: two consecutive {:.0}s intervals answered {interval_answers} \
                 presentation-feedback requests and none were `Presented` — every frame is \
                 going nowhere, most likely because the outputs this slicer was built against \
                 were destroyed and recreated by the compositor (measured on a four-projector \
                 bench on 2026-09-15: `presented 0 discarded 605` on every output while this \
                 process kept reporting 60.4 fps). Exiting so the daemon's sync_slicer respawns \
                 it against whatever exists now.",
                elapsed.as_secs_f64(),
            );
            self.closed = true;
        }

        // Per-output vblank phase, name and value only — see the `phase_ms`
        // doc on `OutputTiming` for what stable versus wandering looks like
        // here. The half-spread is folded in only when it is non-zero, which
        // is why output 0 (always exactly 0.0 against itself) prints with no
        // `±` at all.
        let phase_parts: Vec<String> = outputs
            .iter()
            .filter_map(|o| {
                o.phase_ms.map(|phase| {
                    let sign = if phase > 0.0 { "+" } else { "" };
                    match o.phase_spread_ms {
                        Some(spread) if spread > 0.0 => {
                            format!("{} {sign}{phase:.1}±{:.1}", o.name, spread / 2.0)
                        }
                        _ => format!("{} {sign}{phase:.1}", o.name),
                    }
                })
            })
            .collect();
        let phase_text = match (presentation_feedback, phase_parts.is_empty()) {
            (false, _) => "n/a (no wp_presentation)".to_string(),
            (true, true) => "n/a".to_string(),
            (true, false) => format!("[{}]", phase_parts.join(", ")),
        };

        let renderer_name = match self.capture.backend {
            Some(Backend::Gpu) => "gpu",
            _ => "cpu",
        };
        let ci = self.stats.capture_intervals;

        eprintln!(
            "slicer: {canvas_fps:.1} fps captured, {presented_fps:.1} fps presented, over \
             {:.0}s per frame: waiting {:.1} ms, snapshot {:.1} ms, requesting {:.1} ms, \
             blending {:.1} ms; superseded {}, stalls {}, straddles {}, gate holds {}, \
             buffer reuse {}, offset {offset_text}, phase {phase_text}, \
             renderer {renderer_name}, gpu {:.1} ms, intervals 1:{} 2:{} 3:{} 4+:{}",
            elapsed.as_secs_f64(),
            per_ms(self.stats.waiting),
            per_ms(self.stats.snapshot),
            per_ms(self.stats.requesting),
            per_ms(self.stats.blending),
            self.stats.superseded,
            self.stats.stalls,
            interval.straddles,
            self.stats.gate_holds,
            self.stats.buffer_reuse,
            per_ms(self.stats.gpu),
            ci.one,
            ci.two,
            ci.three,
            ci.more,
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
                gpu: per_ms(self.stats.gpu),
            },
            presentation_feedback,
            offset_ms: interval
                .offset
                .map(|(mean, max)| PresentationOffset { mean, max }),
            straddles: interval.straddles,
            gate_holds: self.stats.gate_holds,
            renderer: renderer_name.to_string(),
            capture_intervals: ci,
            outputs,
        };
        // Periodic telemetry, coalesced through `StdoutWriter`'s writer
        // thread rather than blocking this (render) thread on the pipe; the
        // writer flushes every line it writes, so the manager's reader
        // thread still sees each one promptly.
        self.stdout
            .telemetry(serde_json::to_string(&stats).unwrap_or_default());

        self.stats = FrameStats::new();
    }
}

pub fn run(spec: &SlicerSpec) -> anyhow::Result<()> {
    let nonexact = requested_warp(spec)?;
    let requires_linear_sampling = requires_linear_sampling(spec).map_err(anyhow::Error::msg)?;
    if nonexact && spec.renderer == Renderer::Cpu {
        report_capability(
            spec,
            Renderer::Cpu,
            false,
            Some("renderer:cpu does not support geometric warp correction".into()),
            true,
        );
        anyhow::bail!("warp_unavailable: renderer:cpu does not support geometric warp correction");
    }
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
    // Optional: version 4 is what carries `get_default_feedback` (see the
    // module doc); absent, or an older version, means the GPU path is
    // simply never available and `negotiate_gpu` says so.
    let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&handle, 4..=4, ()).ok();

    let mut state = State {
        outputs: Vec::new(),
        used_outputs: Vec::new(),
        output_modes: HashMap::new(),
        compositor: Some(compositor.clone()),
        presenters: Vec::new(),
        capture: Capture::default(),
        closed: false,
        dead_intervals: DeadIntervalTracker::default(),
        free_run: spec.free_run,
        renderer: spec.renderer,
        presentation,
        timing: Timing::new(spec.slices.len()),
        stats: FrameStats::new(),
        gpu: None,
        gpu_formats: DmabufFormats::default(),
        gpu_error: None,
        present_format: None,
        dmabuf_feedback: None,
        canvas_period_ms: 1000.0 / 60.0,
        last_capture_at: None,
        gate_blocked_since: None,
        warp_updates: None,
        warp_available: false,
        requires_linear_sampling,
        static_canvases: Vec::new(),
        gpu_retry_after: None,
        adaptive: None,
        capture_id: 0,
        capture_measurement: None,
        measurement: super::adaptive::MeasurementSchedule::default(),
        stdout: super::control::StdoutWriter::spawn(),
    };
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" && global.version >= 4 {
            let output: WlOutput = globals.registry().bind(global.name, 4, &handle, ());
            // The registry `name` is discarded nowhere near here any more:
            // kept alongside the proxy so a later `GlobalRemove` — the event
            // this same registry sends when the compositor destroys this
            // output, as it does on every enable/disable-together commit and
            // on every unplug — can be matched back to it. See `used_outputs`
            // and the `Dispatch<WlRegistry, GlobalListContents>` impl.
            state.outputs.push((output, None, None, global.name));
        }
    }
    queue.roundtrip(&mut state)?;

    let find = |state: &State, name: &str| -> Option<WlOutput> {
        state
            .outputs
            .iter()
            .find(|(_, n, ..)| n.as_deref() == Some(name))
            .map(|(o, ..)| o.clone())
    };
    let source = find(&state, &spec.source)
        .ok_or_else(|| anyhow::anyhow!("no output named {} to capture", spec.source))?;
    if let Some(name) = state
        .outputs
        .iter()
        .find(|(o, ..)| *o == source)
        .map(|(.., name)| *name)
    {
        state.used_outputs.push(name);
    }

    // The canvas's own refresh, for the capture-interval histogram's bucket
    // width (see `FrameStats.capture_intervals`); an assumed 60 Hz stands in
    // when the compositor never reported one (a virtual/headless output can
    // report zero — see wl_output's own doc on that).
    let canvas_refresh_mhz = state
        .outputs
        .iter()
        .find(|(o, ..)| *o == source)
        .and_then(|(_, _, refresh, _)| *refresh);
    state.canvas_period_ms = canvas_refresh_mhz
        .filter(|&mhz| mhz > 0)
        .map(|mhz| 1_000_000.0 / f64::from(mhz))
        .unwrap_or(1000.0 / 60.0);

    // Negotiate for content and every pattern; forced GPU failures stay explicit.
    if spec.renderer != Renderer::Cpu {
        match negotiate_gpu(dmabuf.as_ref(), &mut state, &mut queue, &handle) {
            Ok((gpu, formats)) => {
                state.gpu = Some(gpu);
                state.gpu_formats = formats;
            }
            Err(reason) => {
                if spec.renderer == Renderer::Gpu {
                    report_capability(spec, Renderer::Gpu, false, Some(reason.clone()), nonexact);
                    anyhow::bail!("renderer gpu was forced but is unavailable: {reason}");
                }
                state.gpu_error = Some(reason);
            }
        }
    }

    // One presenter per slice, covering its physical output entirely.
    for (index, slice) in spec.slices.iter().enumerate() {
        let target = find(&state, &slice.output)
            .ok_or_else(|| anyhow::anyhow!("no output named {}", slice.output))?;
        // The mode is recorded on the presenter as it stood after the
        // roundtrip above, so the `scanout candidate:` line can say whether
        // the buffer will be the output's own pixel grid. See
        // `Presenter.output_mode`.
        let mut output_mode = None;
        if let Some(name) = state
            .outputs
            .iter()
            .find(|(o, ..)| *o == target)
            .map(|(.., name)| *name)
        {
            state.used_outputs.push(name);
            output_mode = state.output_modes.get(&name).copied();
        }
        let surface = compositor.create_surface(&handle, ());
        let region: WlRegion = compositor.create_region(&handle, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        // The opaque region itself cannot be set yet — it wants the size the
        // compositor has not configured us with — so the layer-surface
        // `Configure` handler sets it, and resets it on any later resize.
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
            opaque_for: None,
            output_mode,
            name: slice.output.clone(),
            buffers: Vec::new(),
            gpu_buffers: Vec::new(),
            busy: Vec::new(),
            next_buffer: 0,
            transfer: Vec::new(),
            dynamic_table: None,
            warp: None,
            sample: None,
            sample_revision: None,
            warp_revision: 0,
            warp_reported_revision: None,
            warp_submitted_revision: None,
            source: slice.source,
            frame_pending: false,
            pending_since: None,
            feedback_pending_for: None,
            feedback_since: None,
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
    if state.gpu.is_some() {
        super::warp_update::validate_initial(
            spec,
            &state
                .presenters
                .iter()
                .map(|p| p.configured.unwrap())
                .collect::<Vec<_>>(),
        )
        .map_err(anyhow::Error::msg)?;
    }
    // How much black each region of the canvas is receiving. Derived from
    // every slice, because how much a projector must lift depends on how many
    // *others* light the same pixel — a four-way grid center needs none while
    // its two-way seams still do. Used only as the fallback for a hand-built
    // spec with no configured layout at all; every canvas plan the daemon
    // itself derives resolves both the seam weight and this lift through
    // `layout::Evaluator` instead (`blend::SlicerSpec::layout`'s doc).
    let coverage = Coverage::new(super::warp_update::coverage_rects(spec));
    let layout = spec
        .layout
        .as_ref()
        .map(|l| super::layout::Evaluator::new(l, spec.canvas_width, spec.canvas_height))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    for (presenter, slice) in state.presenters.iter_mut().zip(spec.slices.iter()) {
        let (width, height) = presenter.configured.unwrap();
        let layout = layout
            .as_ref()
            .map(|l| l.index(&slice.output).map(|i| (l, i)))
            .transpose()
            .map_err(anyhow::Error::msg)?;
        // Built at the *presented* size, with no warp yet (this is the
        // initial bootstrap table, before any control update has built one):
        // any warp/size mismatch later shows up as identity pixels, not a
        // panic. Single-threaded here — this runs once at startup, not per
        // frame — through the same row evaluator `warp_update`'s ongoing
        // rebuilds use, so there is exactly one place this math lives.
        let mut transfer = vec![(0u16, 0u8); width as usize * height as usize];
        super::warp_update::fill_rows(
            spec,
            slice,
            None,
            &coverage,
            layout,
            (width, height),
            0,
            &mut transfer,
        );
        presenter.transfer = transfer;
    }

    if let Some(pattern) = spec.pattern {
        state.capture.backend = Some(
            decide_sync_backend(&mut state, dmabuf.as_ref(), !animated(pattern)).inspect_err(
                |error| {
                    report_capability(
                        spec,
                        spec.renderer,
                        false,
                        Some(error.to_string()),
                        nonexact,
                    )
                },
            )?,
        );
        create_present_buffers(&mut state, &shm, dmabuf.as_ref(), &handle)?;
        if !animated(pattern) && state.capture.backend == Some(Backend::Gpu) {
            state.static_canvases = build_static_canvases(&mut state, spec, pattern)?;
        }
        initialize_warp_control(spec, &connection, &mut queue, &mut state)?;
        if animated(pattern) {
            return run_sync(
                &connection,
                &mut queue,
                &mut state,
                dmabuf.as_ref(),
                &handle,
            );
        }
        if state.capture.backend == Some(Backend::Gpu) {
            state.new_snapshot();
            loop {
                apply_warp_updates(&mut state)?;
                if state.closed {
                    return Ok(());
                }
                if state.can_present() {
                    present_frame_gpu(&mut state, &handle);
                }
                dispatch_until(
                    &connection,
                    &mut queue,
                    &mut state,
                    Instant::now() + Duration::from_secs(60),
                )?;
            }
        }
        present_pattern(&mut state, spec, pattern)?;
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
    loop {
        if !arm_copy(
            &mut state,
            &mut queue,
            &shm,
            dmabuf.as_ref(),
            &handle,
            &current,
        )
        .inspect_err(|error| {
            report_capability(
                spec,
                spec.renderer,
                false,
                Some(error.to_string()),
                nonexact,
            )
        })? {
            return Ok(());
        }
        if !state.capture.failed {
            break;
        }
        current.destroy();
        failures += 1;
        anyhow::ensure!(
            failures < MAX_FAILURES,
            "initial screencopy failed {failures} times"
        );
        state.capture.failed = false;
        current = request_capture(&mut state, &screencopy, &source, &handle);
    }
    // The backend (and, on the GPU path, the capture image) is decided as
    // of the `arm_copy` above — now create presenter buffers of the right
    // kind for it, before the first capture can possibly complete.
    create_present_buffers(&mut state, &shm, dmabuf.as_ref(), &handle)?;
    initialize_warp_control(spec, &connection, &mut queue, &mut state)?;
    let mut next = request_capture(&mut state, &screencopy, &source, &handle);

    loop {
        let waiting_from = Instant::now();
        apply_warp_updates(&mut state)?;
        tick_adaptive(&mut state);
        while !state.capture.ready && !state.capture.failed && !state.can_present() {
            if state.warp_updates.is_some() || state.gpu_retry_after.is_some() {
                dispatch_until(
                    &connection,
                    &mut queue,
                    &mut state,
                    Instant::now() + Duration::from_secs(60),
                )?;
                apply_warp_updates(&mut state)?;
                tick_adaptive(&mut state);
            } else {
                queue.blocking_dispatch(&mut state)?;
            }
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
            // with a single capture rather than guessing. Both capture
            // slots are about to be rearmed from scratch, so anything still
            // measuring one of them is retired first.
            retire_measurement(&mut state);
            state.capture.failed = false;
            state.capture.ready = false;
            current = request_capture(&mut state, &screencopy, &source, &handle);
            if !arm_copy(
                &mut state,
                &mut queue,
                &shm,
                dmabuf.as_ref(),
                &handle,
                &current,
            )? {
                return Ok(());
            }
            next = request_capture(&mut state, &screencopy, &source, &handle);
            continue;
        }
        failures = 0;

        if state.capture.ready {
            state.capture.ready = false;
            state.capture.first_copy_done = true;
            record_capture_interval(&mut state);
            state.stats.captured += 1;
            state.capture_id += 1;
            // Marks every presenter `stale` (both backends' due-presenter
            // selection reads this) and runs the stall-callback timeout
            // check — needed on the GPU path too, not just CPU.
            state.new_snapshot();

            if state.capture.backend == Some(Backend::Gpu) {
                // Two capture images, alternated — see the module doc. This
                // `Ready` filled `gpu_slot` (call it image A); flip the slot
                // to the *other* image (B) and arm the next capture into it
                // *before* blending A, so the compositor's copy into B runs
                // concurrently with `blend()`'s fence wait on A instead of
                // waiting for that wait to finish first. Safe specifically
                // because B is a different image than the one `blend` is
                // about to read — arming it can never race that read the
                // way re-arming a single shared image used to. The
                // present-side fence wait inside `blend()` is unavoidable
                // either way (the compositor samples the Present buffers
                // the moment we commit, and NVIDIA gives dmabufs no
                // implicit sync), but it no longer sits ahead of the next
                // capture — that capture is already in flight by the time
                // this call blocks on it.
                current.destroy();
                let filled_slot = state.capture.gpu_slot;
                state.capture.gpu_slot = (filled_slot + 1) % GPU_CAPTURE_SLOTS;
                // Flipped first so measurement knows which slot is about to
                // be armed and never samples that one. This takes delivery
                // of whatever the GPU has already measured and may queue a
                // pass over the capture just completed; it waits for
                // nothing. The pass's read of `filled_slot` is ordered
                // ahead of the `blend()` below, which waits on its own
                // fence — see `Gpu::submit_luminance_measurement`.
                measure_adaptive(&mut state, filled_slot);
                // The newest complete frame, held for the gate rather than
                // blended here: whether it goes out now or in a moment is
                // the gate's decision, made once at the bottom of the loop
                // for both backends. Arming the *next* capture still
                // happens right away — that is what keeps the pipeline
                // full, and it is independent of when this frame is shown.
                state.capture.pending_slot = Some(filled_slot);

                // The arm below hands `gpu_slot` back to the compositor,
                // which writes into it with no fence of its own. That slot
                // carries the capture measured one iteration ago, so this
                // is where an unobserved measurement of it has to be
                // retired; on a presenting wall it has been signaled since
                // that iteration's `blend()`.
                let arming_slot = state.capture.gpu_slot;
                retire_measurement_for_slot(&mut state, arming_slot);
                let requesting_from = Instant::now();
                if !arm_copy(
                    &mut state,
                    &mut queue,
                    &shm,
                    dmabuf.as_ref(),
                    &handle,
                    &next,
                )? {
                    return Ok(());
                }
                state.stats.requesting += requesting_from.elapsed();
                current = next;
                next = request_capture(&mut state, &screencopy, &source, &handle);

                // Present images follow a mid-run resize on every capture,
                // not only on the ones the gate lets out — `gpu_blend_due`
                // used to be the only place this happened, and it no longer
                // runs on a gated cycle.
                if let Some(dmabuf) = dmabuf.as_ref() {
                    for index in 0..state.presenters.len() {
                        ensure_gpu_present_buffers(&mut state, dmabuf, &handle, index);
                    }
                }
            } else {
                // Copy the frame out and hand the buffer straight back, so the
                // compositor is already drawing the next one while this one is
                // being cut into slices and committed.
                let snapshot_from = Instant::now();
                state.capture.take_snapshot();
                current.destroy();
                state.stats.snapshot += snapshot_from.elapsed();

                // Usually already satisfied: the handshake ran during the wait above.
                let requesting_from = Instant::now();
                if !arm_copy(
                    &mut state,
                    &mut queue,
                    &shm,
                    dmabuf.as_ref(),
                    &handle,
                    &next,
                )? {
                    return Ok(());
                }
                state.stats.requesting += requesting_from.elapsed();
                current = next;
                next = request_capture(&mut state, &screencopy, &source, &handle);
            }
        }

        if state.can_present() {
            let blending_from = Instant::now();
            let before = state.stats.presented;
            present_frame(&mut state, &handle);
            state.stats.blending += blending_from.elapsed();
            if state.stats.presented > before {
                state.note_commit_cycle(false);
            }
        } else if state.presenters.iter().any(|p| p.stale) {
            // A frame is waiting and the gate is shut: from here to the
            // commit that eventually goes out is what `gateHolds` measures.
            state.note_gate_blocked();
        }

        state.report_stats_if_due();
        // A dead-interval trip (see `DeadIntervalTracker`) sets `closed`
        // from inside that call, same as a `GlobalRemove` or the layer
        // surface's own `Closed` — checked here so the process actually
        // exits on the interval it was decided, rather than drifting on for
        // another cycle or more before the next `blocking_dispatch` above
        // happens to re-check it.
        if state.closed {
            current.destroy();
            next.destroy();
            return Ok(());
        }
    }
}

fn requested_warp(spec: &SlicerSpec) -> anyhow::Result<bool> {
    let mut requested = false;
    for slice in &spec.slices {
        anyhow::ensure!(
            slice.source.width > 0 && slice.source.height > 0,
            "source dimensions must be positive"
        );
        // A source rectangle can crop and scale through the identity mapper
        // on either renderer.  Capability is only about retained geometric
        // correction, whose malformed form must still be rejected here.
        requested |= slice.geometry.as_ref().is_some_and(|geometry| {
            geometry
                .warp(slice.source.width as u32, slice.source.height as u32)
                .map_or(true, |warp| warp.is_some())
        });
    }
    Ok(requested)
}

/// Whether the source mapping reaches the shader's linearly filtered path.
/// This includes identity geometry carrying a fractional or scaled shared
/// crop, but excludes the exact integer-copy proof.
fn requires_linear_sampling(spec: &SlicerSpec) -> Result<bool, String> {
    spec.slices.iter().try_fold(false, |required, slice| {
        Ok(required
            || super::warp_update::sampling_warp(
                spec,
                slice,
                (slice.source.width as u32, slice.source.height as u32),
            )?
            .is_some())
    })
}

fn report_capability(
    spec: &SlicerSpec,
    effective_renderer: Renderer,
    available: bool,
    reason: Option<String>,
    requested: bool,
) {
    let event = super::control::ControlEvent::new(
        spec.control_session.clone(),
        0,
        super::control::ControlEventKind::Capability {
            requested_renderer: spec.renderer,
            effective_renderer,
            warp_available: available,
            reason,
            requested_mode: if requested {
                crate::model::ProjectionMode::Warp
            } else {
                crate::model::ProjectionMode::Simple
            },
            effective_mode: if requested && available {
                crate::model::ProjectionMode::Warp
            } else {
                crate::model::ProjectionMode::Simple
            },
        },
    );
    println!("{}", serde_json::to_string(&event).unwrap());
    let _ = std::io::stdout().flush();
}

/// Startup capability reflects the selected source and allocated image, not
/// merely a successfully created Vulkan sampler. Public mode persistence is
/// intentionally separate from this internal slicer contract.
fn initialize_warp_control(
    spec: &SlicerSpec,
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
) -> anyhow::Result<()> {
    let requested = requested_warp(spec)?;
    let effective = if state.capture.backend == Some(Backend::Gpu) {
        Renderer::Gpu
    } else {
        Renderer::Cpu
    };
    let reason = if effective == Renderer::Cpu {
        Some(state.gpu_error.clone().unwrap_or_else(|| {
            "selected CPU pipeline does not support geometric warp correction".into()
        }))
    } else if spec.pattern.is_none()
        && state
            .capture
            .gpu_images
            .first()
            .is_none_or(|(image, _)| !image.linear_filter_supported())
    {
        Some("capture format/modifier does not support linear filtering".into())
    } else {
        None
    };
    state.warp_available = reason.is_none();
    report_capability(
        spec,
        effective,
        state.warp_available,
        reason.clone(),
        requested,
    );
    if requested && !state.warp_available {
        anyhow::bail!("warp_unavailable: {}", reason.unwrap());
    }
    if effective == Renderer::Cpu {
        configure_adaptive(state, spec, 0);
        return Ok(());
    }
    let mut controller = super::warp_update::Controller::new(
        spec,
        state
            .presenters
            .iter()
            .map(|p| p.configured.unwrap())
            .collect(),
        blend_workers(u32::MAX) as usize,
    )
    .map_err(anyhow::Error::msg)?;
    controller.set_stdout(state.stdout.clone());
    controller.read_stdin()?;
    state.warp_updates = Some(controller);
    // Initial matrices and tables must exist before any submission. Later
    // edits build off-thread and retain the current complete generation.
    while !state.warp_updates.as_ref().unwrap().initialized() {
        apply_warp_updates(state)?;
        if state.warp_updates.as_ref().unwrap().initialized() {
            break;
        }
        dispatch_until(
            connection,
            queue,
            state,
            Instant::now() + Duration::from_secs(60),
        )?;
        anyhow::ensure!(!state.closed, "output closed during initial warp build");
    }
    Ok(())
}

/// Each diagnostic canvas retains the legacy per-output labels/chart placement.
/// It is uploaded once, then sampled through the same warp/transfer shader as
/// content. Pins never regenerate these sources. Gamma chart gamma changes are
/// source changes and therefore require a topology restart.
fn build_static_canvases(
    state: &mut State,
    spec: &SlicerSpec,
    pattern: TestPattern,
) -> anyhow::Result<Vec<gpu::StaticCanvas>> {
    anyhow::ensure!(
        spec.canvas_width > 0 && spec.canvas_height > 0,
        "invalid canvas dimensions"
    );
    let pixels = spec.canvas_width as u64 * spec.canvas_height as u64;
    anyhow::ensure!(
        pixels > 0 && pixels.saturating_mul(spec.slices.len() as u64) <= 64_000_000,
        "static pattern canvases exceed the 64 megapixel resource limit"
    );
    let gpu = state.gpu.as_mut().expect("GPU pattern backend");
    let mut canvases = Vec::with_capacity(spec.slices.len());
    for slice in &spec.slices {
        let rgba = static_pattern_rgba(spec, slice, pattern)?;
        canvases.push(gpu.upload_static_canvas_rgba(
            spec.canvas_width as u32,
            spec.canvas_height as u32,
            &rgba,
        )?);
    }
    Ok(canvases)
}

fn static_pattern_rgba(
    spec: &SlicerSpec,
    slice: &super::blend::SliceSpec,
    pattern: TestPattern,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        spec.canvas_width > 0
            && spec.canvas_height > 0
            && spec.canvas_width as u64 * spec.canvas_height as u64 <= 64_000_000,
        "invalid static canvas dimensions"
    );
    let rect = slice.source;
    anyhow::ensure!(
        rect.width > 0 && rect.height > 0,
        "invalid static pattern source rectangle"
    );
    let (width, height) = (rect.width as u32, rect.height as u32);
    anyhow::ensure!(
        u64::from(width) * u64::from(height) <= 32_000_000,
        "static pattern output exceeds the 32 megapixel limit"
    );
    let source = slice.source_rect.unwrap_or([
        f64::from(rect.x),
        f64::from(rect.y),
        f64::from(rect.width),
        f64::from(rect.height),
    ]);
    anyhow::ensure!(
        source.iter().all(|v| v.is_finite())
            && source[2] > 0.0
            && source[3] > 0.0
            && (source[0] + source[2]).is_finite()
            && (source[1] + source[3]).is_finite(),
        "invalid static pattern source rectangle"
    );
    let fake = OverlaySpec {
        output: slice.output.clone(),
        gamma: spec.gamma,
        black_lift: 0.0,
        rect,
        // The picture's true canvas footprint — the same fractional,
        // possibly-scaled `source` this function resamples into below — so
        // canvas-anchored features (the grid's tile lines, warp-alignment's
        // percentage lines and circles) are computed at their real canvas
        // position rather than assuming raster pixel x is canvas pixel
        // `rect.x + x`, which only holds when Content scale is 100%.
        source_rect: Some(source),
        pattern: Some(pattern),
        canvas_size: Some([spec.canvas_width as u32, spec.canvas_height as u32]),
    };
    let rgb = super::pattern::render(width, height, &fake);
    let mut rgba = vec![0; spec.canvas_width as usize * spec.canvas_height as usize * 4];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    if rect.x >= 0
        && rect.y >= 0
        && source
            == [
                f64::from(rect.x),
                f64::from(rect.y),
                f64::from(rect.width),
                f64::from(rect.height),
            ]
    {
        // Preserve every existing integer-source diagnostic byte, including
        // its per-output labels and the unpainted surrounding canvas.
        for y in 0..height.min((spec.canvas_height as u32).saturating_sub(rect.y as u32)) {
            for x in 0..width.min((spec.canvas_width as u32).saturating_sub(rect.x as u32)) {
                let src = (y as usize * width as usize + x as usize) * 3;
                let dst = ((y as usize + rect.y as usize) * spec.canvas_width as usize
                    + x as usize
                    + rect.x as usize)
                    * 4;
                rgba[dst..dst + 3].copy_from_slice(&rgb[src..src + 3]);
            }
        }
        return Ok(rgba);
    }

    // Paint the fixed diagnostic picture into continuous canvas space. The
    // shader clamps to source-border texel centers, so include the neighboring
    // canvas texels used by bilinear filtering and extend picture-edge colors
    // into that support. This avoids black fringes at fractional source edges
    // and also gives a subpixel source rectangle a defined midpoint sample.
    let canvas_size = [spec.canvas_width, spec.canvas_height];
    let bounds: [(i32, i32); 2] = std::array::from_fn(|axis| {
        let inset = (source[axis + 2] * 0.5).min(0.5);
        let first_center = source[axis] + inset;
        let last_center = source[axis] + (source[axis + 2] - inset).max(inset);
        (
            (first_center - 0.5)
                .floor()
                .max(0.0)
                .min(f64::from(canvas_size[axis])) as i32,
            ((last_center - 0.5).ceil() + 1.0)
                .max(0.0)
                .min(f64::from(canvas_size[axis])) as i32,
        )
    });
    for y in bounds[1].0..bounds[1].1 {
        for x in bounds[0].0..bounds[0].1 {
            // Both spaces use pixel boundaries; subtract 0.5 only when
            // converting the local diagnostic position to sample indices.
            let px = if source[2] < 1.0 {
                f64::from(width - 1) * 0.5
            } else {
                (((f64::from(x) + 0.5 - source[0]) / source[2]) * f64::from(width) - 0.5)
                    .clamp(0.0, f64::from(width - 1))
            };
            let py = if source[3] < 1.0 {
                f64::from(height - 1) * 0.5
            } else {
                (((f64::from(y) + 0.5 - source[1]) / source[3]) * f64::from(height) - 0.5)
                    .clamp(0.0, f64::from(height - 1))
            };
            let x0 = px.floor() as u32;
            let y0 = py.floor() as u32;
            let x1 = (x0 + 1).min(width - 1);
            let y1 = (y0 + 1).min(height - 1);
            let tx = px - f64::from(x0);
            let ty = py - f64::from(y0);
            let dst = (y as usize * spec.canvas_width as usize + x as usize) * 4;
            for channel in 0..3 {
                let at = |sx: u32, sy: u32| {
                    f64::from(rgb[(sy as usize * width as usize + sx as usize) * 3 + channel])
                };
                rgba[dst + channel] = ((1.0 - ty) * ((1.0 - tx) * at(x0, y0) + tx * at(x1, y0))
                    + ty * ((1.0 - tx) * at(x0, y1) + tx * at(x1, y1)))
                .round() as u8;
            }
        }
    }
    Ok(rgba)
}

/// Install only between synchronous GPU submissions: Gpu::blend/sync have
/// already waited their fence, so no SSBO reader remains in flight.
/// Stage the complete affected set before changing any active matrix/table.
fn apply_warp_updates(state: &mut State) -> anyhow::Result<()> {
    // Checked here because every path through the main and startup loops
    // already calls this once per iteration regardless of backend: a broken
    // pipe (the daemon restarted, or stopped reading) makes the stdout
    // writer thread give up instead of blocking or panicking on the render
    // thread (see `StdoutWriter`); noticing it here ends the child cleanly,
    // the same way stdin EOF already does, rather than rendering into a pipe
    // nobody drains until some other error surfaces.
    if state.stdout.closed() {
        state.closed = true;
        return Ok(());
    }
    let Some(controller) = state.warp_updates.as_mut() else {
        return Ok(());
    };
    let prepared = controller.poll();
    anyhow::ensure!(
        !controller.requires_restart(),
        "control version mismatch; restarting slicer"
    );
    let Some(mut prepared) = prepared else {
        return Ok(());
    };
    let validated = prepared.outputs.iter().all(|output| {
        state
            .presenters
            .get(output.index)
            .is_some_and(|p| p.name == output.name && p.configured == Some(output.size))
    });
    let filtering_restart = prepared.outputs.iter().any(|output| output.warp.is_some())
        && state
            .capture
            .gpu_images
            .first()
            .is_some_and(|(image, _)| !image.linear_filter_supported());
    let upload = Instant::now();
    let result = if !validated {
        Err(anyhow::anyhow!("output topology changed; restart required"))
    } else if filtering_restart {
        Err(anyhow::anyhow!(
            "capture format/modifier does not support linear filtering required by the source crop; restarting with CPU"
        ))
    } else if prepared
        .outputs
        .iter()
        .any(super::warp_update::Output::has_geometric_correction)
        && !state.warp_available
    {
        Err(anyhow::anyhow!(
            "warp_unavailable: negotiated pipeline does not support geometric correction"
        ))
    } else if let Some(gpu) = state.gpu.as_mut() {
        if prepared.outputs.iter().any(|o| o.dynamic_table.is_some()) {
            let updates: Vec<_> = prepared
                .outputs
                .iter()
                .map(|o| gpu::PackedTransferUpdate {
                    index: o.index,
                    width: o.size.0,
                    height: o.size.1,
                    table: o
                        .dynamic_table
                        .as_deref()
                        .expect("one transfer mode per generation"),
                })
                .collect();
            gpu.replace_packed_transfers(&updates)
        } else {
            let updates: Vec<_> = prepared
                .outputs
                .iter()
                .map(|o| gpu::TransferUpdate {
                    index: o.index,
                    width: o.size.0,
                    height: o.size.1,
                    table: &o.table,
                })
                .collect();
            gpu.replace_transfers(&updates)
        }
    } else {
        Err(anyhow::anyhow!("GPU unavailable"))
    };
    let upload_ms = upload.elapsed().as_secs_f64() * 1000.0;
    if let Err(error) = result {
        state
            .warp_updates
            .as_ref()
            .unwrap()
            .reject(prepared.generation, error.to_string());
        if !validated || filtering_restart || !state.warp_updates.as_ref().unwrap().initialized() {
            return Err(error);
        }
        return Ok(());
    }
    let mut changed: Vec<_> = prepared.outputs.iter().map(|o| o.name.clone()).collect();
    let sampling_modes = prepared
        .outputs
        .iter()
        .map(|o| {
            (
                o.name.clone(),
                if o.warp.is_some() {
                    crate::model::SamplingMode::Bilinear
                } else {
                    crate::model::SamplingMode::Exact
                },
            )
        })
        .collect();
    for output in &mut prepared.outputs {
        let presenter = &mut state.presenters[output.index];
        presenter.transfer = std::mem::take(&mut output.table);
        presenter.dynamic_table = output.dynamic_table.take();
        presenter.warp = output.warp.take();
        presenter.source = output.source;
        presenter.warp_revision = prepared.generation;
        presenter.stale = true;
    }
    let uniform_changed = state
        .adaptive
        .as_ref()
        .map(|runtime| runtime.controller.config())
        != prepared.spec.adaptive_lift;
    if prepared.spec.adaptive_lift.is_some() && (prepared.outputs.is_empty() || uniform_changed) {
        for presenter in &mut state.presenters {
            presenter.warp_revision = prepared.generation;
            if !changed.contains(&presenter.name) {
                changed.push(presenter.name.clone());
            }
        }
        state.new_snapshot();
    }
    let controller = state.warp_updates.as_mut().unwrap();
    controller.installed(&prepared);
    controller.event(
        prepared.generation,
        super::control::ControlEventKind::Applied {
            outputs: changed,
            sampling_modes,
            build_ms: Some(prepared.build_ms),
            upload_ms: Some(upload_ms),
        },
    );
    configure_adaptive(state, &prepared.spec, prepared.generation);
    Ok(())
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
    dmabuf: Option<&ZwpLinuxDmabufV1>,
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
        if let Some((format, width, height)) = state.capture.dmabuf_offer {
            eprintln!("slicer: dmabuf offer {width}x{height} format {format:#x}");
        }
    }
    ensure_capture_buffer(state, shm, dmabuf, handle)?;
    let first_copy_done = state.capture.first_copy_done;
    if !first_copy_done {
        print_renderer_decision(state);
    }
    match state.capture.backend {
        Some(Backend::Gpu) => {
            let (_, buffer) = &state.capture.gpu_images[state.capture.gpu_slot];
            // Damage is reported against the previous copy into this
            // buffer, which is why the same (single, today) capture image
            // is reused every time — see `Capture.gpu_images`'s doc.
            if first_copy_done {
                frame.copy_with_damage(buffer);
            } else {
                frame.copy(buffer);
            }
        }
        _ => {
            let buffer = &state.capture.buffer.as_ref().unwrap().0;
            // Damage is reported against the previous copy into this buffer,
            // which is why the same buffer is reused every time.
            if first_copy_done {
                frame.copy_with_damage(buffer);
            } else {
                frame.copy(buffer);
            }
        }
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

/// Decide the capture backend, once, and ensure this frame has a buffer of
/// the right kind. `Capture.backend` is fixed after the first call — see
/// `decide_backend`.
fn ensure_capture_buffer(
    state: &mut State,
    shm: &WlShm,
    dmabuf: Option<&ZwpLinuxDmabufV1>,
    handle: &QueueHandle<State>,
) -> anyhow::Result<()> {
    if state.capture.backend.is_none() {
        state.capture.backend = Some(decide_backend(state, dmabuf)?);
    }
    match state.capture.backend {
        Some(Backend::Gpu) => {
            let dmabuf =
                dmabuf.expect("decide_backend only ever returns Backend::Gpu when dmabuf is Some");
            ensure_gpu_capture_buffer(state, dmabuf, handle)?;
            let linear_filter_supported = state
                .capture
                .gpu_images
                .get(state.capture.gpu_slot)
                .is_some_and(|(image, _)| image.linear_filter_supported());
            if !state.requires_linear_sampling || linear_filter_supported {
                return Ok(());
            }
            let reason =
                "capture format/modifier does not support linear filtering required by the source crop";
            if state.renderer == Renderer::Gpu {
                anyhow::bail!("renderer gpu was forced but is unavailable: {reason}");
            }
            eprintln!("slicer: renderer gpu unavailable ({reason}); falling back to cpu (shm)");
            // Every capture image goes with this drain, including one an
            // asynchronous measurement may still be reading, so that pass
            // has to be retired first whichever slot it is on.
            if let Some(gpu) = state.gpu.as_mut() {
                let _ = gpu.finish_luminance_measurement();
            }
            state.measurement.completed();
            for (_, buffer) in state.capture.gpu_images.drain(..) {
                buffer.destroy();
            }
            state.capture.backend = Some(Backend::Cpu);
            state.gpu_error = Some(reason.into());
            ensure_shm_capture_buffer(&mut state.capture, shm, handle)
        }
        _ => ensure_shm_capture_buffer(&mut state.capture, shm, handle),
    }
}

/// The backend decision itself, run once at the first `arm_copy` — see the
/// module doc's negotiation notes. `Ok` names the winner; `Err` is only
/// possible when `Renderer::Gpu` was forced and the GPU path turns out not
/// to be available, which is fatal (the slicer exits, the daemon respawns
/// it on its next reconcile).
fn decide_backend(state: &mut State, dmabuf: Option<&ZwpLinuxDmabufV1>) -> anyhow::Result<Backend> {
    if state.renderer == Renderer::Cpu {
        return Ok(Backend::Cpu);
    }
    match gpu_availability(state, dmabuf) {
        Ok(()) => Ok(Backend::Gpu),
        Err(reason) => {
            if state.renderer == Renderer::Gpu {
                anyhow::bail!("renderer gpu was forced but is unavailable: {reason}");
            }
            eprintln!("slicer: renderer gpu unavailable ({reason}); falling back to cpu (shm)");
            state.gpu_error = Some(reason);
            Ok(Backend::Cpu)
        }
    }
}

/// `Ok` when every condition for the GPU path holds this frame; `Err` names
/// the first one that does not, reused for both `decide_backend`'s
/// `Auto`-fallback log line and its `Gpu`-forced fatal error. The present
/// format check is folded in here — rather than left until presenter
/// buffers are actually created — deliberately: by the time this function
/// returns `Ok`, `arm_copy` is about to tell the compositor to copy into a
/// GPU capture image, and backing out of that after the fact (the
/// compositor already mid-write) is not something to attempt. Checking
/// every requirement up front means a missing Present format can only ever
/// produce a clean `Cpu` fallback, never a half-committed GPU capture.
fn gpu_availability(state: &State, dmabuf: Option<&ZwpLinuxDmabufV1>) -> Result<(), String> {
    if dmabuf.is_none() {
        return Err("compositor does not offer zwp_linux_dmabuf_v1 version 4".to_string());
    }
    let Some((format, ..)) = state.capture.dmabuf_offer else {
        return Err(
            "compositor's screencopy offer for this frame carried no dmabuf option".to_string(),
        );
    };
    let Some(gpu) = state.gpu.as_ref() else {
        return Err(state
            .gpu_error
            .clone()
            .unwrap_or_else(|| "no Vulkan device available".to_string()));
    };
    let capture_modifiers = state
        .gpu_formats
        .all
        .get(&format)
        .cloned()
        .unwrap_or_default();
    if gpu
        .supported_modifiers(format, &capture_modifiers, gpu::Usage::Capture)
        .is_empty()
    {
        return Err(format!(
            "capture format {format:#010x} has no Vulkan-importable modifier"
        ));
    }
    present_format_available(gpu, &state.gpu_formats)
}

/// Everything [`gpu_availability`] checks except the parts about capture —
/// the requirements of a run that *draws* its own frames rather than
/// capturing them. See `decide_sync_backend`.
fn sync_gpu_availability(state: &State, dmabuf: Option<&ZwpLinuxDmabufV1>) -> Result<(), String> {
    if dmabuf.is_none() {
        return Err("compositor does not offer zwp_linux_dmabuf_v1 version 4".to_string());
    }
    let Some(gpu) = state.gpu.as_ref() else {
        return Err(state
            .gpu_error
            .clone()
            .unwrap_or_else(|| "no Vulkan device available".to_string()));
    };
    present_format_available(gpu, &state.gpu_formats)
}

/// Whether a Present image can be allocated at all — the half of the GPU
/// path's requirements that has nothing to do with where the pixels came
/// from, and so the only half the `sync` pattern needs.
fn present_format_available(gpu: &gpu::Gpu, formats: &DmabufFormats) -> Result<(), String> {
    let present_ok = [FOURCC_XR24, FOURCC_XB24].into_iter().any(|fourcc| {
        let modifiers = formats.all.get(&fourcc).cloned().unwrap_or_default();
        !gpu.supported_modifiers(fourcc, &modifiers, gpu::Usage::Present)
            .is_empty()
    });
    if !present_ok {
        return Err("neither XR24 nor XB24 is importable as a Present image".to_string());
    }
    Ok(())
}

/// Print, once, which renderer this run settled on — the summary line the
/// coordinator's note on debugging the first on-hardware run is asking for.
/// Gated by the same `!first_copy_done` the offer-diagnostics lines above it
/// use, so it prints exactly once, right after the decision is final.
fn print_renderer_decision(state: &State) {
    match state.capture.backend {
        Some(Backend::Gpu) => {
            let (image, _) = &state.capture.gpu_images[state.capture.gpu_slot];
            let gpu = state
                .gpu
                .as_ref()
                .expect("Backend::Gpu implies state.gpu is Some");
            eprintln!(
                "slicer: renderer gpu ({}), capture {}x{} {:#010x} modifier {:#x}",
                gpu.describe(),
                image.width,
                image.height,
                image.fourcc,
                image.modifier,
            );
        }
        Some(Backend::Cpu) | None => {
            eprintln!("slicer: renderer cpu (shm)");
        }
    }
}

fn ensure_shm_capture_buffer(
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

/// GPU path's `ensure_capture_buffer`: create (or, on a mid-run resize
/// offer, recreate) `Capture.gpu_slot`'s capture image and its wl_buffer at
/// whatever size/format screencopy most recently offered. Only ever touches
/// that one slot — never the other, which may still hold an image nothing
/// has blended yet (the primary blend later this cycle, or a straggler's
/// safety-net one) — see `Capture.gpu_images`'s doc.
fn ensure_gpu_capture_buffer(
    state: &mut State,
    dmabuf_proxy: &ZwpLinuxDmabufV1,
    handle: &QueueHandle<State>,
) -> anyhow::Result<()> {
    let State {
        gpu,
        gpu_formats,
        capture,
        ..
    } = state;
    let gpu = gpu
        .as_ref()
        .expect("Backend::Gpu implies state.gpu is Some");
    let (format, width, height) = capture
        .dmabuf_offer
        .ok_or_else(|| anyhow::anyhow!("screencopy offered no dmabuf buffer"))?;
    let slot = capture.gpu_slot;
    let stale = match capture.gpu_images.get(slot) {
        Some((image, _)) => (image.width, image.height, image.fourcc) != (width, height, format),
        None => true,
    };
    if stale {
        let modifiers = gpu_formats.all.get(&format).cloned().unwrap_or_default();
        let mut supported = gpu.supported_capture_modifiers(format, &modifiers, true);
        if supported.is_empty() && !state.warp_available {
            supported = gpu.supported_capture_modifiers(format, &modifiers, false);
        }
        if supported.is_empty() {
            anyhow::bail!(
                "no Vulkan-importable modifier for capture format {format:#010x} \
                 (mid-run resize to a format the device cannot import?)"
            );
        }
        let image = gpu
            .create_image(width, height, format, &supported, gpu::Usage::Capture)
            .context("gpu.create_image (capture)")?;
        let buffer = dmabuf::dmabuf_wl_buffer(dmabuf_proxy, &image, handle, ());
        if slot < capture.gpu_images.len() {
            let (_old_image, old_buffer) =
                std::mem::replace(&mut capture.gpu_images[slot], (image, buffer));
            old_buffer.destroy();
        } else {
            debug_assert_eq!(
                slot,
                capture.gpu_images.len(),
                "gpu_slot alternates 0, 1, 0, 1, ...; each slot's first use must be the next \
                 index in order"
            );
            capture.gpu_images.push((image, buffer));
        }
        debug_assert!(capture.gpu_images.len() <= GPU_CAPTURE_SLOTS);
    }
    Ok(())
}

/// What one `zwp_linux_dmabuf_feedback_v1` exchange told us, beside which
/// device to render on: every fourcc -> modifier pair the compositor's main
/// device advertises, and the subset it flagged as scan-out capable. Both
/// are wanted when a Present image is allocated — the scanout subset to
/// allocate from so wlroots can flip the buffer straight to the KMS plane,
/// the full list to fall back to when the two share nothing.
#[derive(Default)]
struct DmabufFormats {
    /// fourcc -> modifiers, in the compositor's advertised order.
    all: HashMap<u32, Vec<u64>>,
    /// The subset of `all` that came from tranches carrying the protocol's
    /// `scanout` tranche flag. Empty on a compositor that flags none.
    scanout: HashMap<u32, Vec<u64>>,
}

/// The two modifier lists `create_present_buffers` settled on for the fourcc
/// it chose, kept on `State.present_format` so a mid-run resize reallocates
/// from exactly the same choice.
#[derive(Clone)]
struct PresentModifiers {
    /// Every modifier this device can render a Present image with that the
    /// compositor also advertises. Always non-empty (a `Backend::Gpu` run
    /// got here only because `gpu_availability` found one).
    all: Vec<u64>,
    /// The subset of `all` the compositor flagged for scanout, in the same
    /// order. Empty when there is no such subset, and then `all` is simply
    /// what gets allocated.
    scanout: Vec<u64>,
}

/// The modifiers to allocate an output's Present image from if the buffer is
/// to be eligible for direct scanout: those the device supports *and* the
/// compositor listed in a scanout tranche, in `supported`'s order — which is
/// the compositor's advertised order, since `Gpu::supported_modifiers` keeps
/// its caller's. Empty when the two share nothing (or the compositor flagged
/// no scanout tranche for this fourcc at all), and the caller then allocates
/// from `supported` whole: a buffer that cannot be scanned out still
/// composites correctly, which is exactly today's behavior.
fn scanout_modifiers(supported: &[u64], scanout: &[u64]) -> Vec<u64> {
    supported
        .iter()
        .copied()
        .filter(|modifier| scanout.contains(modifier))
        .collect()
}

/// Allocate one Present image, preferring the narrowed scanout modifier
/// list. Narrowing it is the whole point — `VkImageDrmFormatModifierList`
/// lets the *driver* pick any entry, so offering the non-scanout ones
/// alongside would simply let it choose one — but a failure with the narrow
/// list must not end the run: the wider list is tried before giving up,
/// because a picture that costs a compositing pass beats no picture.
fn create_present_image(
    gpu: &gpu::Gpu,
    width: u32,
    height: u32,
    fourcc: u32,
    modifiers: &PresentModifiers,
) -> anyhow::Result<gpu::DmabufImage> {
    if !modifiers.scanout.is_empty() {
        match gpu.create_image(
            width,
            height,
            fourcc,
            &modifiers.scanout,
            gpu::Usage::Present,
        ) {
            Ok(image) => return Ok(image),
            Err(error) => eprintln!(
                "slicer: allocating a {width}x{height} present image from the compositor's \
                 scanout modifiers failed ({error:#}); falling back to every importable \
                 modifier, which costs a compositing pass per frame"
            ),
        }
    }
    gpu.create_image(width, height, fourcc, &modifiers.all, gpu::Usage::Present)
}

/// A DRM fourcc as the four characters it spells ("XR24"), for log lines.
/// Anything unprintable becomes `?` — this is never parsed back.
fn fourcc_name(fourcc: u32) -> String {
    fourcc
        .to_le_bytes()
        .iter()
        .map(|&byte| {
            let c = char::from(byte);
            if c.is_ascii_graphic() {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// What the startup line can honestly say about one output's Present buffer.
///
/// Three answers, not two, because the default dmabuf feedback is not
/// evidence of a negative: on `brain` (test-log Entry 2) wlroots' default
/// feedback carried no scanout-flagged tranche for the chosen fourcc, this
/// function said "no", and every single presented frame came back
/// zero-copy anyway. The tranche flag is a hint about what the compositor
/// *prefers*; the real gate is the buffer's format, modifier and size
/// against the KMS plane at commit time, which only the compositor can
/// evaluate. So an absent tranche is [`ScanoutVerdict::Unconfirmed`] — this
/// line cannot tell — while the conditions that are checkable from here
/// stay definite.
#[derive(Debug, PartialEq, Eq)]
enum ScanoutVerdict {
    /// Every precondition this process can check is met: the buffer is the
    /// output's pixel grid and carries a modifier the compositor flagged for
    /// scanout.
    Yes { fourcc: u32, modifier: u64 },
    /// A precondition that is decidable from here fails, so no flip is
    /// possible however the compositor feels about it.
    No(String),
    /// Nothing disqualifies this buffer, but the compositor's default
    /// feedback did not say it prefers the modifier for scanout either.
    /// `zeroCopyPresented` is the only authority.
    Unconfirmed(String),
}

/// Whether the buffer an output is about to commit is something wlroots can
/// flip straight to the plane, the first reason it definitely is not, or
/// that this process cannot tell. Ordered so the most fundamental obstacle
/// wins: a buffer that is not the output's pixel grid is disqualified
/// whatever modifier it carries. Nothing here changes what is allocated —
/// the allocation already happened, and `chosen_modifier` is what the driver
/// picked for it; this only words the log line.
fn scanout_verdict(
    configured: (u32, u32),
    output_mode: Option<(i32, i32)>,
    fourcc: u32,
    modifiers: &PresentModifiers,
    compositor_flagged_scanout: bool,
    chosen_modifier: u64,
) -> ScanoutVerdict {
    let (width, height) = configured;
    if let Some((mode_width, mode_height)) = output_mode {
        // The slicer never calls `set_buffer_scale`, so the buffer is the
        // surface-local (logical) size; on a scaled output that is smaller
        // than the mode and the compositor must scale it, which no plane
        // here is asked to do. A rotated output reads as a mismatch too —
        // a false negative in this line only, since the mode is reported
        // before transform.
        let matches = (i64::from(width), i64::from(height))
            == (i64::from(mode_width), i64::from(mode_height));
        if !matches {
            return ScanoutVerdict::No(format!(
                "buffer is {width}x{height} but the output's mode is \
                 {mode_width}x{mode_height} pixels; direct scanout needs scale 1"
            ));
        }
    }
    if !compositor_flagged_scanout {
        // Deliberately not a "no": see [`ScanoutVerdict`]. The compositor
        // never named a scanout tranche for this fourcc, which says nothing
        // about whether it will flip the buffer.
        return ScanoutVerdict::Unconfirmed(
            "default feedback carries no scanout tranche; zeroCopyPresented in \
             GET /projection/stats is authoritative"
                .to_string(),
        );
    }
    // From here the compositor *did* name a tranche, so what it did or did
    // not name is real evidence about this buffer.
    if modifiers.scanout.is_empty() {
        return ScanoutVerdict::No("no common modifier".to_string());
    }
    if !modifiers.scanout.contains(&chosen_modifier) {
        return ScanoutVerdict::No(format!(
            "allocated with modifier {chosen_modifier:#x}, which the compositor did not flag \
             for scanout"
        ));
    }
    ScanoutVerdict::Yes {
        fourcc,
        modifier: chosen_modifier,
    }
}

/// The startup line, one per output, saying whether the buffer this
/// presenter will commit can be flipped straight to the display controller,
/// definitely cannot, or cannot be judged from here. Purely informational —
/// everything it reports was already decided — but it is the only way to
/// tell, without a compositor debug build, *which* of the several
/// preconditions a wall is failing. It is a diagnostic and never a proof
/// that scanout happened: `zeroCopyPresented` in `GET /projection/stats`,
/// read from the compositor's own presentation feedback, is that.
fn log_scanout_candidate(name: &str, verdict: ScanoutVerdict) {
    match verdict {
        ScanoutVerdict::Yes { fourcc, modifier } => eprintln!(
            "slicer: output {name}: scanout candidate: yes (fourcc {}, modifier {modifier:#x})",
            fourcc_name(fourcc)
        ),
        ScanoutVerdict::No(reason) => {
            eprintln!("slicer: output {name}: scanout candidate: no ({reason})")
        }
        ScanoutVerdict::Unconfirmed(reason) => {
            eprintln!("slicer: output {name}: scanout candidate: unconfirmed ({reason})")
        }
    }
}

/// Once the backend is decided (right after the first `arm_copy`), create
/// each presenter's rotation of Present buffers — shm as always, or a Vulkan
/// image exported as a dmabuf and wrapped as a wl_buffer on the GPU path,
/// picking XR24 and falling back to XB24 (see the module doc's "Present
/// buffers" section) — and, on the GPU path, upload every output's transfer
/// table. How many per presenter is [`present_slots`]: the GPU path needs
/// one more than the shm path because a scanned-out buffer is held across
/// the following flip.
fn create_present_buffers(
    state: &mut State,
    shm: &WlShm,
    dmabuf_proxy: Option<&ZwpLinuxDmabufV1>,
    handle: &QueueHandle<State>,
) -> anyhow::Result<()> {
    match state.capture.backend {
        Some(Backend::Gpu) => {
            let dmabuf_proxy = dmabuf_proxy
                .expect("Backend::Gpu implies decide_backend already confirmed dmabuf is Some");
            let (fourcc, modifiers) = {
                let State {
                    gpu, gpu_formats, ..
                } = state;
                let gpu = gpu
                    .as_ref()
                    .expect("Backend::Gpu implies state.gpu is Some");
                // Both candidate fourccs worked out in full, rather than
                // stopping at the first importable one, so a format the
                // compositor can scan out can be preferred over one it can
                // only composite. XR24 still leads when neither (or both)
                // can be — see the module doc's "Present buffers".
                let candidates: Vec<(u32, PresentModifiers)> = [FOURCC_XR24, FOURCC_XB24]
                    .into_iter()
                    .filter_map(|fourcc| {
                        let advertised = gpu_formats.all.get(&fourcc)?;
                        let all = gpu.supported_modifiers(fourcc, advertised, gpu::Usage::Present);
                        if all.is_empty() {
                            return None;
                        }
                        let scanout = scanout_modifiers(
                            &all,
                            gpu_formats
                                .scanout
                                .get(&fourcc)
                                .map(Vec::as_slice)
                                .unwrap_or_default(),
                        );
                        Some((fourcc, PresentModifiers { all, scanout }))
                    })
                    .collect();
                candidates
                    .iter()
                    .find(|(_, modifiers)| !modifiers.scanout.is_empty())
                    .or_else(|| candidates.first())
                    .map(|(fourcc, modifiers)| (*fourcc, modifiers.clone()))
                    // `decide_backend`/`gpu_availability` already confirmed
                    // one of these works before committing to Backend::Gpu.
                    .expect("gpu_availability already confirmed a Present format is importable")
            };
            state.present_format = Some((fourcc, modifiers.clone()));
            let flagged = state.gpu_formats.scanout.contains_key(&fourcc);

            let State {
                presenters, gpu, ..
            } = state;
            let gpu = gpu
                .as_mut()
                .expect("Backend::Gpu implies state.gpu is Some");
            for (index, presenter) in presenters.iter_mut().enumerate() {
                let (width, height) = presenter.configured.unwrap();
                for slot in 0..present_slots(Some(Backend::Gpu)) {
                    let image = create_present_image(gpu, width, height, fourcc, &modifiers)
                        .with_context(|| format!("gpu.create_image (present, output {index})"))?;
                    let buffer =
                        dmabuf::dmabuf_wl_buffer(dmabuf_proxy, &image, handle, (index, slot));
                    presenter.gpu_buffers.push((buffer, image));
                }
                presenter.busy = vec![false; presenter.gpu_buffers.len()];
                // The modifier the driver settled on for the images just
                // allocated — `last`, not `[0]`, so this reports what this
                // call produced whatever else the presenter is holding.
                let chosen = presenter
                    .gpu_buffers
                    .last()
                    .map(|(_, image)| image.modifier)
                    .expect("the loop above pushed GPU_PRESENT_SLOTS buffers");
                log_scanout_candidate(
                    &presenter.name,
                    scanout_verdict(
                        (width, height),
                        presenter.output_mode,
                        fourcc,
                        &modifiers,
                        flagged,
                        chosen,
                    ),
                );
                gpu.set_transfer(index, width, height, &presenter.transfer)
                    .with_context(|| format!("gpu.set_transfer (output {index})"))?;
            }
            Ok(())
        }
        _ => {
            let slots = present_slots(state.capture.backend);
            for (index, presenter) in state.presenters.iter_mut().enumerate() {
                let (width, height) = presenter.configured.unwrap();
                for slot in 0..slots {
                    presenter
                        .buffers
                        .push(shm_buffer(shm, handle, width, height, (index, slot))?);
                }
                presenter.busy = vec![false; presenter.buffers.len()];
                // Shared memory is never scanned out: the compositor has to
                // read it into a buffer of its own. Said once per output so
                // the line is present on both paths and the answer is never
                // ambiguous silence.
                log_scanout_candidate(&presenter.name, ScanoutVerdict::No("cpu path".to_string()));
            }
            Ok(())
        }
    }
}

/// GPU path only: recreate presenter `index`'s [`GPU_PRESENT_SLOTS`] Present
/// images if its
/// surface's configured size no longer matches them — a mid-run resize (a
/// changed output mode) rather than the initial configure `create_present_buffers`
/// already handled. Best-effort: any failure is logged (naming the output
/// and the step) and leaves the previous, still-valid buffers in place
/// rather than leaving the presenter with none at all.
fn ensure_gpu_present_buffers(
    state: &mut State,
    dmabuf_proxy: &ZwpLinuxDmabufV1,
    handle: &QueueHandle<State>,
    index: usize,
) {
    let Some((fourcc, modifiers)) = state.present_format.clone() else {
        return;
    };
    let flagged = state.gpu_formats.scanout.contains_key(&fourcc);
    let Some(presenter) = state.presenters.get(index) else {
        return;
    };
    let Some((width, height)) = presenter.configured else {
        return;
    };
    let stale = match presenter.gpu_buffers.first() {
        Some((_, image)) => (image.width, image.height) != (width, height),
        None => true,
    };
    if !stale {
        return;
    }
    let name = presenter.name.clone();
    let old_transfer_matches = presenter
        .dynamic_table
        .as_ref()
        .map_or(presenter.transfer.len(), Vec::len)
        == width as usize * height as usize;

    let State {
        presenters, gpu, ..
    } = state;
    let Some(gpu) = gpu.as_mut() else { return };
    let presenter = &mut presenters[index];
    // The whole rotation is rebuilt, so the new size lands on every slot and
    // a resize can never leave a presenter with a mix of sizes.
    let slots = present_slots(Some(Backend::Gpu));
    let mut rebuilt = Vec::with_capacity(slots);
    let mut failed = false;
    for _ in 0..slots {
        match create_present_image(gpu, width, height, fourcc, &modifiers) {
            Ok(image) => {
                let buffer =
                    dmabuf::dmabuf_wl_buffer(dmabuf_proxy, &image, handle, (index, rebuilt.len()));
                rebuilt.push((buffer, image));
            }
            Err(error) => {
                eprintln!(
                    "slicer: output {name} resized but its GPU present buffers could not be \
                     recreated ({error:#}); keeping the previous size until the slicer restarts"
                );
                failed = true;
                break;
            }
        }
    }
    if failed {
        for (buffer, _) in rebuilt {
            buffer.destroy();
        }
        return;
    }
    for (old_buffer, _) in presenter.gpu_buffers.drain(..) {
        old_buffer.destroy();
    }
    presenter.gpu_buffers = rebuilt;
    presenter.busy = vec![false; presenter.gpu_buffers.len()];
    presenter.next_buffer = 0;
    // Said again here, and only here, because a resize is the one thing
    // that can change the answer after startup: a new size against the same
    // output mode is exactly the "buffer is not the output's pixel grid"
    // case. Rare enough (a mode change) not to be noise.
    let chosen = presenter
        .gpu_buffers
        .last()
        .map(|(_, image)| image.modifier)
        .expect("`rebuilt`, just moved in, holds GPU_PRESENT_SLOTS buffers");
    log_scanout_candidate(
        &name,
        scanout_verdict(
            (width, height),
            presenter.output_mode,
            fourcc,
            &modifiers,
            flagged,
            chosen,
        ),
    );
    if old_transfer_matches {
        let result = if let Some(table) = &presenter.dynamic_table {
            gpu.set_packed_transfer(index, width, height, table)
        } else {
            gpu.set_transfer(index, width, height, &presenter.transfer)
        };
        if let Err(error) = result {
            eprintln!("slicer: output {name}: set_transfer after resize failed: {error:#}");
        }
    } else {
        eprintln!(
            "slicer: output {name} resized to {width}x{height} but its blend ramps were \
             computed for a different size; the picture will be untransformed until the slicer \
             restarts"
        );
    }
}

/// Negotiate the GPU path once at startup: bind dmabuf feedback, wait for it
/// to complete, and open a Vulkan device on the compositor's preferred
/// device. `Ok` carries both halves of what that feedback said — every
/// format the main device advertises, and the subset flagged for scanout
/// (see [`DmabufFormats`]). `Err` names the step that failed, for `Auto`'s
/// log line and `Gpu`'s fatal error alike — see `run`'s call site.
fn negotiate_gpu(
    dmabuf: Option<&ZwpLinuxDmabufV1>,
    state: &mut State,
    queue: &mut wayland_client::EventQueue<State>,
    handle: &QueueHandle<State>,
) -> Result<(gpu::Gpu, DmabufFormats), String> {
    let Some(dmabuf) = dmabuf else {
        return Err("compositor does not offer zwp_linux_dmabuf_v1 version 4".to_string());
    };
    state.dmabuf_feedback = Some(dmabuf::FeedbackCollector::default());
    let feedback = dmabuf.get_default_feedback(handle, ());
    // The compositor processes requests in order, so every event this
    // feedback object will ever send for this exchange — including `done`
    // — is already queued behind our own `get_default_feedback` and ahead
    // of whatever the roundtrip's sync reply waits on; one roundtrip is
    // enough to have seen `done` by the time it returns.
    let roundtrip = queue
        .roundtrip(state)
        .map_err(|error| format!("dmabuf feedback roundtrip: {error}"));
    feedback.destroy();
    roundtrip?;
    let collected = state.dmabuf_feedback.take().unwrap_or_default();
    if !collected.done {
        return Err("compositor never sent zwp_linux_dmabuf_feedback_v1.done".to_string());
    }
    gpu::Gpu::new(collected.main_device)
        .map(|gpu| {
            (
                gpu,
                DmabufFormats {
                    all: collected.formats,
                    scanout: collected.scanout_formats,
                },
            )
        })
        .map_err(|error| format!("Gpu::new: {error:#}"))
}

/// Bump `FrameStats.capture_intervals` for the gap since the previous
/// capture reached the slicer — both backends call this, right after
/// detecting `Ready`. See `crate::model::CaptureIntervals`'s doc for the
/// bucket edges.
fn record_capture_interval(state: &mut State) {
    let now = Instant::now();
    if let Some(previous) = state.last_capture_at {
        let elapsed_ms = now.duration_since(previous).as_secs_f64() * 1000.0;
        let periods = elapsed_ms / state.canvas_period_ms;
        let bucket = if periods <= 1.5 {
            &mut state.stats.capture_intervals.one
        } else if periods <= 2.5 {
            &mut state.stats.capture_intervals.two
        } else if periods <= 3.5 {
            &mut state.stats.capture_intervals.three
        } else {
            &mut state.stats.capture_intervals.more
        };
        *bucket += 1;
    }
    state.last_capture_at = Some(now);
}

/// Backend dispatch for the two present_frame implementations below. The
/// single `can_present()`-driven site in `run`'s loop — both backends go
/// through it, so the gate decides when a capture is blended and committed
/// whichever renderer produced it.
fn present_frame(state: &mut State, handle: &QueueHandle<State>) {
    match state.capture.backend {
        Some(Backend::Gpu) => present_frame_gpu(state, handle),
        _ => present_frame_cpu(state, handle),
    }
}

/// GPU path: blend the newest complete capture into whichever presenters are
/// due and commit them.
///
/// The image blended from is `Capture.pending_slot` — the capture a `Ready`
/// most recently completed and the gate has been holding — falling back to
/// `Capture.last_blended_slot` when there is no fresh capture waiting, which
/// is the free-run case of a straggler whose readiness cleared after this
/// cycle's frame already went out to its neighbors. Either is guaranteed
/// not to be the image the compositor is currently writing into: that is
/// always the *other* slot (see `Capture.gpu_slot`'s doc).
fn present_frame_gpu(state: &mut State, handle: &QueueHandle<State>) {
    let slot = state
        .capture
        .pending_slot
        .take()
        .or(state.capture.last_blended_slot)
        .or_else(|| (!state.static_canvases.is_empty()).then_some(0));
    let Some(slot) = slot else {
        // Nothing has been captured yet this run: clear `stale` the same way
        // `gpu_blend_due` would, so a presenter that somehow raced ahead of
        // the very first capture cannot spin `can_present()` forever.
        for presenter in &mut state.presenters {
            presenter.stale = false;
        }
        return;
    };
    let due = gpu_blend_due(state, slot);
    if !due.is_empty() {
        gpu_present_due(state, handle, &due);
    }
}

/// GPU path, blend phase: build the jobs for whichever presenters are due
/// right now and hand them to `gpu.blend()` in one call, which blocks until
/// the GPU is done reading `capture_slot`'s image — see the module's
/// frame-loop comment on why the caller must arm the *other* slot, not this
/// one, before calling this.
///
/// A free-run presenter that is not yet ready (still owing the gate's
/// readiness signal for its last commit) is left `stale`, not cleared:
/// `capture_slot` stays intact for a whole extra cycle after this call
/// returns (see `Capture.last_blended_slot`), so `present_frame_gpu`'s next
/// call can still pick it up the moment that presenter answers, instead of
/// only ever catching the *next* capture the way a single image had to.
/// Every other presenter's `stale` is cleared here, due or not — locked
/// mode only ever calls this once every presenter has answered (see
/// `can_present`), so a configured presenter that was not due simply had
/// nothing new to show and is not owed a re-blend later.
fn gpu_blend_due(state: &mut State, capture_slot: usize) -> Vec<Due> {
    if let (Some(runtime), Some(gpu)) = (&state.adaptive, &mut state.gpu) {
        // Validated configured coverage and controller bounds make this
        // update infallible. One value is shared by every job in this draw.
        if let Err(error) = gpu.set_dynamic_lift(runtime.controller.level(), runtime.maximum) {
            eprintln!("slicer: invalid adaptive uniform: {error:#}");
            return Vec::new();
        }
    }
    let signal = state.gate_signal();
    let State {
        capture,
        presenters,
        free_run,
        stats,
        gpu,
        gpu_retry_after,
        static_canvases,
        ..
    } = state;
    let canvas_image = capture.gpu_images.get(capture_slot).map(|(image, _)| image);
    if canvas_image.is_none() && static_canvases.is_empty() {
        for presenter in presenters.iter_mut() {
            presenter.stale = false;
        }
        return Vec::new();
    }
    let y_invert = capture.y_invert;

    let mut due = Vec::new();
    for (index, presenter) in presenters.iter_mut().enumerate() {
        let was_stale = presenter.stale;
        if presenter.configured.is_none() || !was_stale {
            presenter.stale = false;
            continue;
        }
        if *free_run && !presenter.gate_ready(signal) {
            // Left `stale` — see the doc above.
            continue;
        }
        presenter.stale = false;
        let len = presenter.gpu_buffers.len();
        if len == 0 {
            continue;
        }
        let Some(slot) = free_present_slot(&presenter.busy, presenter.next_buffer) else {
            // A released slot is required even when the presentation gate is
            // open. Keep this generation dirty until wl_buffer.release wakes us.
            presenter.stale = true;
            continue;
        };
        presenter.next_buffer = (slot + 1) % len;
        presenter.busy[slot] = true;
        due.push(Due { index, slot });
    }
    if due.is_empty() {
        return due;
    }

    let jobs: Vec<gpu::BlendJob<'_>> = due
        .iter()
        .map(|d| {
            let presenter = &presenters[d.index];
            let (_, image) = &presenter.gpu_buffers[d.slot];
            gpu::BlendJob {
                target: image,
                output: d.index,
                source_x: presenter.source.x.max(0) as u32,
                source_y: presenter.source.y.max(0) as u32,
                warp: presenter.warp.as_ref(),
            }
        })
        .collect();

    let gpu = gpu
        .as_mut()
        .expect("Backend::Gpu implies state.gpu is Some");
    let result = if static_canvases.is_empty() {
        gpu.blend(canvas_image.unwrap(), y_invert, &jobs)
    } else {
        jobs.iter().try_fold(Duration::ZERO, |total, job| {
            gpu.blend_static(&static_canvases[job.output], std::slice::from_ref(job))
                .map(|duration| total + duration)
        })
    };
    match result {
        Ok(duration) => {
            *gpu_retry_after = None;
            stats.gpu += duration;
            capture.last_blended_slot = Some(capture_slot);
            due
        }
        Err(error) => {
            eprintln!("slicer: gpu.blend failed: {error:#}");
            for d in &due {
                presenters[d.index].busy[d.slot] = false;
                presenters[d.index].stale = true;
            }
            // Retain the complete source even if this was its first draw.
            // This slot remains protected by the existing capture ownership.
            capture.last_blended_slot = Some(capture_slot);
            *gpu_retry_after = Some(Instant::now() + Duration::from_millis(10));
            Vec::new()
        }
    }
}

/// GPU path, present phase: attach each blended image, damage, request the
/// frame callback, ask for presentation feedback, and commit — the same
/// per-presenter tail `present_frame_cpu` ends with, just reading the GPU
/// buffer/image `gpu_blend_due` just wrote instead of a `MmapMut`.
fn gpu_present_due(state: &mut State, handle: &QueueHandle<State>, due: &[Due]) {
    let State {
        presenters,
        timing,
        presentation,
        stats,
        warp_updates,
        ..
    } = state;
    let snapshot_id = timing.snapshot_id;
    let mut committed_any = false;
    for d in due {
        let presenter = &mut presenters[d.index];
        let (buffer, image) = &presenter.gpu_buffers[d.slot];
        let (width, height) = (image.width, image.height);
        presenter.surface.attach(Some(buffer), 0, 0);
        presenter
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        // Requested before `commit`, per wl_surface.frame: the callback
        // fires no earlier than the *next* commit's contents are shown, so
        // asking after commit would describe the wrong frame.
        presenter.surface.frame(handle, d.index);
        if let Some(presentation) = presentation.as_ref() {
            timing.request(snapshot_id, d.index);
            presentation.feedback(
                &presenter.surface,
                handle,
                (d.index, snapshot_id, presenter.warp_revision),
            );
        }
        // Both waits armed together, right before the commit they describe:
        // whichever of them the gate is anchored to is what holds the next
        // commit back until this one has landed.
        presenter.commit_sent(snapshot_id, presentation.is_some());
        presenter.surface.commit();
        if presenter
            .warp_submitted_revision
            .is_none_or(|r| presenter.warp_revision > r)
        {
            presenter.warp_submitted_revision = Some(presenter.warp_revision);
            if let Some(controller) = warp_updates.as_ref() {
                controller.event(
                    presenter.warp_revision,
                    super::control::ControlEventKind::Submitted {
                        outputs: vec![presenter.name.clone()],
                    },
                );
            }
        }
        committed_any = true;
    }
    if committed_any {
        stats.presented += 1;
    }
}

/// Cut the captured canvas into slices, apply each column's transfer, and
/// commit whichever presenters the gating policy says are due a frame.
///
/// Locked mode is only ever called once `can_present` has confirmed every
/// presenter is ready, so every `stale` presenter is committed together —
/// the whole point being that no output can show a newer frame than its
/// neighbors. Free-run commits presenter by presenter as each becomes
/// ready, which is exactly what makes a wall that cannot share a rate keep
/// moving at all. CPU path only — see `present_frame_gpu` for the GPU path's
/// equivalent.
fn present_frame_cpu(state: &mut State, handle: &QueueHandle<State>) {
    let signal = state.gate_signal();
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
        let due = presenter.stale && (!*free_run || presenter.gate_ready(signal));
        if !due {
            continue;
        }
        let len = presenter.buffers.len();
        // Rotate only through released buffers.
        let Some(slot) = free_present_slot(&presenter.busy, presenter.next_buffer) else {
            // A released slot is required even when the presentation gate is
            // open. Keep this generation dirty until wl_buffer.release wakes us.
            presenter.stale = true;
            continue;
        };
        presenter.next_buffer = (slot + 1) % len;
        presenter.busy[slot] = true;

        // Rebuild the axis-sample table only when the warp actually moved to
        // a new generation, not every frame: `warp_revision` only advances in
        // `apply_warp_updates`, so a run of frames on an unchanged warp reuses
        // the same `AxisSamples` and never re-solves a single homography.
        if presenter.sample_revision != Some(presenter.warp_revision) {
            // Matches `source_x`/`source_y` below exactly: the fallback
            // origin `Warp::clamped_canvas_at` would use if this warp had no
            // `source_rect` (every warp built by `sampling_warp` sets one, so
            // in practice this is never read, but it must still match the
            // old per-pixel call's argument to stay byte-identical).
            let origin = [
                f64::from(presenter.source.x.max(0) as u32),
                f64::from(presenter.source.y.max(0) as u32),
            ];
            presenter.sample = presenter
                .warp
                .as_ref()
                .map(|w| AxisSamples::build(w, origin));
            presenter.sample_revision = Some(presenter.warp_revision);
        }

        {
            // Split-borrow again: the transfer table is read while this
            // presenter's buffer is written.
            let Presenter {
                transfer,
                buffers,
                sample,
                source,
                ..
            } = presenter;
            let (_, map) = &mut buffers[slot];
            let blend = Blend {
                canvas,
                transfer,
                format,
                stride,
                y_invert,
                usable_width,
                usable_height,
                source_x: source.x.max(0) as u32,
                source_y: source.y.max(0) as u32,
                width,
                sample: sample.as_ref(),
            };

            let row_bytes = width as usize * 4;
            // A crop can extend beyond the captured canvas.  Clear every
            // output-sized destination first so those clipped pixels are
            // opaque black rather than stale bytes from a reused buffer.
            for pixel in map[..height as usize * row_bytes].chunks_exact_mut(4) {
                pixel.copy_from_slice(&[0, 0, 0, 255]);
            }
            let painted = &mut map[..height as usize * row_bytes];
            let workers = blend_workers(height);
            if workers <= 1 {
                blend.rows(painted, 0, height);
            } else {
                // Rows are independent, so each worker owns a disjoint band of
                // the destination and nothing needs synchronizing. Scoped
                // threads borrow the canvas and the table directly, which is
                // why this needs no channel and no Arc.
                let band = height.div_ceil(workers);
                std::thread::scope(|scope| {
                    for (index, chunk) in painted.chunks_mut(band as usize * row_bytes).enumerate()
                    {
                        let blend = &blend;
                        let first = index as u32 * band;
                        let count = band.min(height - first);
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
        presenter.stale = false;
        if let Some(presentation) = presentation.as_ref() {
            timing.request(snapshot_id, index);
            presentation.feedback(
                &presenter.surface,
                handle,
                (index, snapshot_id, presenter.warp_revision),
            );
        }
        presenter.commit_sent(snapshot_id, presentation.is_some());
        presenter.surface.commit();
        committed_any = true;
    }

    if committed_any {
        stats.presented += 1;
    }
}

/// Precomputed per-axis canvas sample positions for [`Blend::sample`]'s
/// bilinear path, built once per warp generation instead of solved with a
/// homography (matrix multiply plus divide, [`super::warp::Warp::source_at`])
/// on every destination pixel of every frame.
///
/// The CPU renderer never receives a genuine corner warp: `requested_warp`
/// rejects one before the slicer connects to the compositor (see
/// `cpu_rejects_warp_before_connecting_and_preserves_identity_support`), and
/// every `Warp` this struct is ever built from — the content path's
/// `presenter.warp` and `present_pattern`'s local warp alike — is
/// [`super::warp::Warp::identity`] plus, at most, a `source_rect` crop/scale.
/// For that shape `Warp::canvas_at`'s mapping is `rect[axis] +
/// local[axis]/output_dim[axis] * rect[axis+2]` with `local` equal to the
/// output coordinate itself, and `clamped_canvas_at`'s border clamp is
/// applied per axis too — so the whole mapping is two independent 1-D
/// functions, not one 2-D one. A full `width * height` table would waste
/// memory for no benefit; two 1-D tables are exact and far smaller.
///
/// Memory at 4K (3840x2160, the largest single output `warp_update`
/// allows): `(3840 + 2160)` entries of `Option<f64>` (16 bytes, no niche for
/// `f64`) is about 94 KiB per output — negligible, and roughly three orders
/// of magnitude below a naive `3840 * 2160` per-pixel table (~66 MiB).
struct AxisSamples {
    x: Vec<Option<f64>>,
    y: Vec<Option<f64>>,
}

impl AxisSamples {
    /// Builds by calling the same `clamped_canvas_at` the per-pixel path
    /// used to call every frame, just once per column and once per row, so
    /// every entry is the exact value the old code computed — not an
    /// approximation of it. `warp` must be [`super::warp::Warp::is_identity`];
    /// see the struct docs for why that is the only shape that ever reaches
    /// here, and the assert below for what happens if that ever stops being
    /// true (a wrong per-axis table would otherwise fail silently). The
    /// table size is `warp`'s own output size, not a separately passed one,
    /// so it can never disagree with the geometry `warp` itself encodes.
    fn build(warp: &super::warp::Warp, origin: [f64; 2]) -> Self {
        assert!(
            warp.is_identity(),
            "CPU Blend only ever samples an identity warp (source-rectangle \
             crop/scale); geometric correction is rejected before it reaches \
             the CPU renderer, so a per-axis table is exact here"
        );
        let [width, height] = warp.output_size();
        let x = (0..width)
            .map(|i| {
                warp.clamped_canvas_at(f64::from(i) + 0.5, 0.5, origin)
                    .map(|p| p[0])
            })
            .collect();
        let y = (0..height)
            .map(|i| {
                warp.clamped_canvas_at(0.5, f64::from(i) + 0.5, origin)
                    .map(|p| p[1])
            })
            .collect();
        Self { x, y }
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
    usable_width: u32,
    usable_height: u32,
    source_x: u32,
    source_y: u32,
    width: u32,
    /// `Some` is the common fractional crop/scale path. `None` is the
    /// exact integer-copy proof and deliberately retains its old sampling.
    sample: Option<&'a AxisSamples>,
}

impl Blend<'_> {
    fn shade(a: u16, b: u8, value: u8) -> u8 {
        (((a as u32 * value as u32) >> 8) + b as u32).min(255) as u8
    }

    fn write_black(dst: &mut [u8]) {
        dst.copy_from_slice(&[0, 0, 0, 255]);
    }

    fn sample(&self, x: u32, y: u32) -> Option<[u8; 3]> {
        let Some(axes) = self.sample else {
            let source_x = self.source_x.checked_add(x)?;
            let source_y = self.source_y.checked_add(y)?;
            if source_x >= self.usable_width || source_y >= self.usable_height {
                return None;
            }
            let source_y = if self.y_invert {
                self.usable_height - 1 - source_y
            } else {
                source_y
            };
            let offset =
                source_y as usize * self.stride as usize + source_x as usize * self.format.bytes;
            return Some([
                self.canvas[offset + self.format.blue],
                self.canvas[offset + self.format.green],
                self.canvas[offset + self.format.red],
            ]);
        };
        // Table lookup, not a homography solve: `axes` was built once for
        // this warp generation (`AxisSamples::build`), so this is exactly
        // the value `Warp::clamped_canvas_at(x+0.5, y+0.5, ...)` would have
        // returned every frame, at the cost of two array reads.
        let source_x = axes.x.get(x as usize).copied().flatten()?;
        let source_y = axes.y.get(y as usize).copied().flatten()?;
        // Source positions are texel centers.  This check is intentionally
        // before clamping to the captured image: a crop outside the canvas
        // is black, including when an old frame's buffer is being reused.
        if source_x < 0.5
            || source_y < 0.5
            || source_x > f64::from(self.usable_width) - 0.5
            || source_y > f64::from(self.usable_height) - 0.5
        {
            return None;
        }
        let source_y = if self.y_invert {
            f64::from(self.usable_height) - source_y
        } else {
            source_y
        };
        // The exact path remains a byte-for-byte texel copy.  Fractional
        // source rectangles use bilinear texel-center sampling, matching the
        // GPU's linearly filtered source mapping.
        let x0 = (source_x - 0.5).floor() as u32;
        let y0 = (source_y - 0.5).floor() as u32;
        let x1 = (x0 + 1).min(self.usable_width - 1);
        let y1 = (y0 + 1).min(self.usable_height - 1);
        let tx = source_x - (f64::from(x0) + 0.5);
        let ty = source_y - (f64::from(y0) + 0.5);
        let channel = |pixel_x: u32, pixel_y: u32, index: usize| {
            self.canvas[pixel_y as usize * self.stride as usize
                + pixel_x as usize * self.format.bytes
                + index] as f64
        };
        let interpolate = |index| {
            ((1.0 - ty) * ((1.0 - tx) * channel(x0, y0, index) + tx * channel(x1, y0, index))
                + ty * ((1.0 - tx) * channel(x0, y1, index) + tx * channel(x1, y1, index)))
            .round() as u8
        };
        Some([
            interpolate(self.format.blue),
            interpolate(self.format.green),
            interpolate(self.format.red),
        ])
    }

    /// Shade `count` destination rows, `dst` starting at row `first`.
    fn rows(&self, dst: &mut [u8], first: u32, count: u32) {
        let row_bytes = self.width as usize * 4;
        for y in 0..count {
            let target = first + y;
            let dst_row = y as usize * row_bytes;
            let transfer_row = target as usize * self.width as usize;
            for x in 0..self.width as usize {
                let (a, b) = self
                    .transfer
                    .get(transfer_row + x)
                    .copied()
                    .unwrap_or((256, 0));
                let out = &mut dst[dst_row + x * 4..dst_row + x * 4 + 4];
                if a == 0 && b == 0 {
                    Self::write_black(out);
                    continue;
                }
                let Some(sample) = self.sample(x as u32, target) else {
                    Self::write_black(out);
                    continue;
                };
                // Presenters are always BGRA, opaque.
                out[0] = Self::shade(a, b, sample[0]);
                out[1] = Self::shade(a, b, sample[1]);
                out[2] = Self::shade(a, b, sample[2]);
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
///
/// Builds this slice's picture in canvas space (the same picture, via the
/// same `static_pattern_rgba`, that the GPU path uploads as a
/// `StaticCanvas`), then samples it into the presenter's buffer through
/// exactly the same `Blend::sample` the CPU *content* path uses — a
/// source-rectangle-only `Warp` standing in for the real geometric
/// correction the CPU renderer does not support. A 1:1 local copy here — as
/// this used to be — silently assumed raster pixel x was canvas pixel
/// `slice.source.x + x`, which only holds when Content scale is 100%; at any
/// other scale it drew canvas-anchored features at the wrong canvas
/// position, so a CPU-rendered wall calibrated on the pattern would not
/// merge with a GPU-rendered one on real content.
fn present_pattern(
    state: &mut State,
    spec: &SlicerSpec,
    pattern: crate::model::TestPattern,
) -> anyhow::Result<()> {
    for (slice, presenter) in spec.slices.iter().zip(state.presenters.iter_mut()) {
        let Some((width, height)) = presenter.configured else {
            continue;
        };
        let canvas = static_pattern_rgba(spec, slice, pattern)?;
        let source = slice.source_rect.unwrap_or([
            f64::from(slice.source.x),
            f64::from(slice.source.y),
            f64::from(slice.source.width),
            f64::from(slice.source.height),
        ]);
        let warp = super::warp::Warp::identity(width, height)
            .with_source_rect(source)
            .map_err(anyhow::Error::msg)?;
        // One-shot per pattern activation, not per frame, but built through
        // the same `AxisSamples` the content path uses so there is exactly
        // one place this table-building logic lives.
        let sample = AxisSamples::build(
            &warp,
            [
                f64::from(slice.source.x.max(0) as u32),
                f64::from(slice.source.y.max(0) as u32),
            ],
        );
        let blend = Blend {
            canvas: &canvas,
            transfer: &presenter.transfer,
            // `static_pattern_rgba`'s output is packed R,G,B,A, unlike a
            // captured canvas' native BGRA/RGBA negotiated format.
            format: PixelFormat {
                bytes: 4,
                red: 0,
                green: 1,
                blue: 2,
            },
            stride: spec.canvas_width as u32 * 4,
            y_invert: false,
            usable_width: spec.canvas_width as u32,
            usable_height: spec.canvas_height as u32,
            source_x: slice.source.x.max(0) as u32,
            source_y: slice.source.y.max(0) as u32,
            width,
            sample: Some(&sample),
        };
        let (_, map) = &mut presenter.buffers[0];
        let row_bytes = width as usize * 4;
        // Clear first: a crop can extend beyond the canvas, same as content.
        for pixel in map[..height as usize * row_bytes].chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 0, 255]);
        }
        blend.rows(&mut map[..height as usize * row_bytes], 0, height);
        let (buffer, _) = &presenter.buffers[0];
        presenter.surface.attach(Some(buffer), 0, 0);
        presenter
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        presenter.surface.commit();
    }
    Ok(())
}

// --- the `sync` test pattern ----------------------------------------------

/// Whether a pattern is drawn per frame by the slicer rather than once into
/// a static buffer. See the module doc's "Sync pattern" section.
fn animated(pattern: TestPattern) -> bool {
    match pattern {
        TestPattern::Sync => true,
        TestPattern::Grid
        | TestPattern::White
        | TestPattern::Black
        | TestPattern::Gamma
        | TestPattern::Identify
        | TestPattern::WarpAlignment => false,
    }
}

/// The `sync` pattern's frame loop: the content loop with the capture taken
/// out and the gate left to set the pace.
///
/// Everything that decides *when* a frame reaches an output is deliberately
/// unchanged — the all-outputs gate, `free_run`, the buffer rotation,
/// `wl_surface.frame`, `wp_presentation_feedback`, `snapshot_id`, the
/// ten-second report and `GET /projection/stats` — because that machinery is
/// exactly what this pattern exists to photograph. What is gone is the
/// canvas: there is nothing to capture, so nothing stands in for the
/// screencopy `Ready` that would otherwise mark a new frame's arrival.
///
/// What marks it instead is the gate itself: a cycle commits as soon as
/// every output has reported presenting the previous one, which puts the
/// commit a fraction of a millisecond after the slowest head's vblank and
/// therefore a whole refresh short of the next deadline. The canvas period
/// (`State.canvas_period_ms`, which falls back to 60 Hz for an output that
/// reports none — a headless canvas legitimately may) survives only at the
/// two edges: as a floor, so the pattern never runs faster than the canvas
/// rate it claims to run at, and as a deadline ([`GATE_FALLBACK_PERIODS`]),
/// so an output whose feedback is merely late cannot stop the counter. A
/// free-running timer was the primary clock until 2026-09-17 and is not any
/// more: see the module doc's "Presentation gating" for the measurement
/// that moved it. `canvasFps` is now the commit rate, which on a healthy
/// wall is the refresh.
fn run_sync(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    dmabuf: Option<&ZwpLinuxDmabufV1>,
    handle: &QueueHandle<State>,
) -> anyhow::Result<()> {
    match state.capture.backend {
        Some(Backend::Gpu) => {
            let gpu = state
                .gpu
                .as_ref()
                .expect("Backend::Gpu implies state.gpu is Some");
            eprintln!("slicer: renderer gpu ({}), sync pattern", gpu.describe());
        }
        Some(Backend::Cpu) | None => eprintln!("slicer: renderer cpu (shm), sync pattern"),
    }

    let period = Duration::from_secs_f64(state.canvas_period_ms / 1000.0);
    let floor_step = period.mul_f64(GATE_FLOOR_PERIODS);
    let fallback_step = period.mul_f64(GATE_FALLBACK_PERIODS);
    eprintln!(
        "slicer: sync pattern paced by the gate ({}), floored at {:.2} Hz ({:.2} ms, the \
         canvas output's refresh) and forced after {:.2} ms",
        state.gate_signal().answer_name(),
        1000.0 / state.canvas_period_ms,
        state.canvas_period_ms,
        fallback_step.as_secs_f64() * 1000.0,
    );

    // One value per present cycle, shared by every output committed in it —
    // the whole measurement rests on that, so it is incremented here and
    // nowhere else.
    let mut frame: u32 = 0;
    // Nothing has been committed yet, so the first frame goes out at once.
    let mut floor = Instant::now();
    let mut force = floor;
    loop {
        apply_warp_updates(state)?;
        // Sleep to whichever the loop is actually waiting for: the floor
        // when the gate is already open (there is nothing to wait for but
        // the canvas rate), the fallback deadline when it is not (an event
        // will almost always arrive first and cut the wait short).
        let deadline = if state.gate_open() { floor } else { force };
        let waiting_from = Instant::now();
        dispatch_until(connection, queue, state, deadline)?;
        state.stats.waiting += waiting_from.elapsed();
        if state.closed {
            return Ok(());
        }

        let now = Instant::now();
        let open = state.gate_open();
        if now >= floor && !open {
            // Past the floor with the gate still shut: the wall would commit
            // right now if a straggler had answered. See `gateHolds`.
            state.note_gate_blocked();
        }
        // The gate is this loop's clock; the fallback is what keeps a clock
        // that has stopped from stopping the pattern with it.
        let forced = now >= force && !open;
        if now >= floor && (open || forced) {
            record_capture_interval(state);
            state.stats.captured += 1;
            state.new_snapshot();
            // Once a cycle, the same place the content loop does it: an
            // output whose mode changed under us needs Present images of
            // the new size before anything is drawn into them.
            if state.capture.backend == Some(Backend::Gpu) {
                if let Some(dmabuf) = dmabuf {
                    for index in 0..state.presenters.len() {
                        ensure_gpu_present_buffers(state, dmabuf, handle, index);
                    }
                }
            }
            let blending_from = Instant::now();
            if present_sync(state, handle, frame) {
                frame = frame.wrapping_add(1);
            }
            state.stats.blending += blending_from.elapsed();
            state.note_commit_cycle(forced);
            // Both deadlines run from the commit, not from a fixed grid: the
            // whole point is that the commits follow the flips rather than a
            // clock of this loop's own.
            floor = now + floor_step;
            force = now + fallback_step;
        }

        state.report_stats_if_due();
        if state.closed {
            return Ok(());
        }
    }
}

/// `decide_backend` for a run with no capture: the same `Auto`/`Cpu`/`Gpu`
/// policy and the same fatal case, against `sync_gpu_availability`'s
/// shorter list of requirements.
fn decide_sync_backend(
    state: &mut State,
    dmabuf: Option<&ZwpLinuxDmabufV1>,
    static_canvas: bool,
) -> anyhow::Result<Backend> {
    if state.renderer == Renderer::Cpu {
        return Ok(Backend::Cpu);
    }
    let availability = sync_gpu_availability(state, dmabuf).and_then(|()| {
        if static_canvas && !state.gpu.as_ref().unwrap().static_canvas_supported() {
            Err("GPU static canvas format lacks sampled linear filtering or upload support".into())
        } else {
            Ok(())
        }
    });
    match availability {
        Ok(()) => Ok(Backend::Gpu),
        Err(reason) => {
            if state.renderer == Renderer::Gpu {
                anyhow::bail!("renderer gpu was forced but is unavailable: {reason}");
            }
            eprintln!("slicer: renderer gpu unavailable ({reason}); falling back to cpu (shm)");
            state.gpu_error = Some(reason);
            Ok(Backend::Cpu)
        }
    }
}

/// Dispatch Wayland events until `deadline`, or until something arrives,
/// whichever is first.
///
/// `blocking_dispatch`, which the content loop uses, has no deadline — the
/// canvas's own `Ready` is what wakes it. This loop has no canvas, so it has
/// to wake itself: flush, arm a read, and poll the connection's fd with the
/// time left on the tick. Returning early on any event is deliberate and
/// costs nothing: the caller re-checks both the clock and the gate on every
/// pass, and events on this connection are frame callbacks and presentation
/// feedback — a handful per output per frame, not a stream.
fn dispatch_until(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    deadline: Instant,
) -> anyhow::Result<()> {
    queue.flush()?;
    // `None` means events are already queued; there is nothing to wait for.
    let Some(guard) = queue.prepare_read() else {
        queue.dispatch_pending(state)?;
        return Ok(());
    };
    let deadline = state
        .gpu_retry_after
        .filter(|at| *at > Instant::now())
        .map_or(deadline, |retry| deadline.min(retry));
    let deadline = state
        .adaptive
        .as_ref()
        .and_then(|runtime| runtime.next_tick)
        .map_or(deadline, |tick| deadline.min(tick));
    let millis = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(i32::MAX as u128) as i32;
    let mut poll_fds = [
        libc::pollfd {
            fd: connection.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: state.warp_updates.as_ref().map_or(-1, |c| c.fd()),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // Safety: both initialized descriptors are owned for the duration of
    // the call. A negative control fd is ignored by poll.
    let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, millis) };
    if ready > 0 && poll_fds[0].revents != 0 {
        // A spurious wakeup (or a racing dispatch from elsewhere) leaves
        // nothing to read; that is not an error, just an empty pass.
        if let Err(error) = guard.read() {
            if !matches!(
                &error,
                wayland_client::backend::WaylandError::Io(io)
                    if io.kind() == std::io::ErrorKind::WouldBlock
            ) {
                return Err(error.into());
            }
        }
    } else {
        // Timed out, or interrupted by a signal. Either way the guard must
        // be released before anything else touches the queue.
        drop(guard);
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

/// Draw and commit one `sync` frame. `true` when at least one output took
/// it, which is what advances the counter.
fn present_sync(state: &mut State, handle: &QueueHandle<State>, frame: u32) -> bool {
    let due = sync_due(state);
    if due.is_empty() {
        return false;
    }
    // Sampled once, here, and handed to every output of this cycle: the
    // clock is there to be read off one video frame of the whole wall, so
    // two outputs showing different milliseconds would be a defect of the
    // pattern rather than a measurement. A clock stepped before 1970 is not
    // worth a branch further down, so it reads as midnight.
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    match state.capture.backend {
        Some(Backend::Gpu) => present_sync_gpu(state, handle, frame, unix_ms, due),
        _ => present_sync_cpu(state, handle, frame, unix_ms, due),
    }
}

/// Which presenters are owed this cycle's frame, and which buffer slot each
/// takes — the selection `present_frame_cpu` and `gpu_blend_due` both make,
/// in one place because with no canvas there is nothing else for the two
/// paths to differ about.
///
/// Locked mode reaches here only once every presenter is ready (see
/// `can_present`), so every stale presenter is committed together. Free-run
/// leaves a presenter that is still holding a frame callback `stale`, so it
/// is picked up on the pass where its callback clears — and then shows that
/// pass's counter, not this one's. That is free-run behaving as documented,
/// and it is why a sync photograph is taken with the gate on.
fn sync_due(state: &mut State) -> Vec<Due> {
    if !state.free_run
        && state
            .presenters
            .iter()
            .any(|p| p.stale && !p.stalled && free_present_slot(&p.busy, p.next_buffer).is_none())
    {
        return Vec::new();
    }
    let signal = state.gate_signal();
    let State {
        presenters,
        free_run,
        capture,
        ..
    } = state;
    let gpu = capture.backend == Some(Backend::Gpu);
    let mut due = Vec::new();
    for (index, presenter) in presenters.iter_mut().enumerate() {
        if presenter.configured.is_none() || !presenter.stale {
            presenter.stale = false;
            continue;
        }
        if *free_run && !presenter.gate_ready(signal) {
            continue;
        }
        presenter.stale = false;
        let len = if gpu {
            presenter.gpu_buffers.len()
        } else {
            presenter.buffers.len()
        };
        if len == 0 {
            continue;
        }
        let Some(slot) = free_present_slot(&presenter.busy, presenter.next_buffer) else {
            // A released slot is required even when the presentation gate is
            // open. Keep this generation dirty until wl_buffer.release wakes us.
            presenter.stale = true;
            continue;
        };
        presenter.next_buffer = (slot + 1) % len;
        presenter.busy[slot] = true;
        due.push(Due { index, slot });
    }
    due
}

/// CPU path: rasterize the rect list into each due presenter's next free shm
/// slot and commit it exactly as `present_frame_cpu` commits a slice of the
/// canvas — the same rotation, the same frame callback, the same
/// presentation-feedback request.
fn present_sync_cpu(
    state: &mut State,
    handle: &QueueHandle<State>,
    frame: u32,
    unix_ms: u64,
    due: Vec<Due>,
) -> bool {
    let State {
        presenters,
        timing,
        presentation,
        ..
    } = state;
    let snapshot_id = timing.snapshot_id;
    let mut committed_any = false;
    for d in &due {
        let presenter = &mut presenters[d.index];
        let Some((width, height)) = presenter.configured else {
            continue;
        };
        let groups =
            super::pattern::sync_rects(width, height, frame, &presenter.name, snapshot_id, unix_ms);
        let rects: Vec<SyncRect> = groups
            .iter()
            .flat_map(|group| group.rects.iter().copied())
            .collect();
        {
            // Split-borrow: the transfer table is read while this
            // presenter's buffer is written.
            let Presenter {
                transfer, buffers, ..
            } = presenter;
            let (_, map) = &mut buffers[d.slot];
            let paint = SyncPaint {
                transfer,
                rects: &rects,
                width,
            };
            let row_bytes = width as usize * 4;
            // From the buffer, not only from `configured`: a surface that
            // was reconfigured larger mid-run still has its old shm buffer
            // (the CPU path never reallocates one), and painting the
            // configured height into it would be an out-of-bounds slice.
            // The picture is wrong for that one frame either way; it must
            // not also be a panic.
            let height = height.min((map.len() / row_bytes.max(1)) as u32);
            let painted = &mut map[..height as usize * row_bytes];
            let workers = blend_workers(height);
            if workers <= 1 {
                paint.rows(painted, 0, height);
            } else {
                let band = height.div_ceil(workers);
                std::thread::scope(|scope| {
                    for (index, chunk) in painted.chunks_mut(band as usize * row_bytes).enumerate()
                    {
                        let paint = &paint;
                        let first = index as u32 * band;
                        let count = band.min(height - first);
                        scope.spawn(move || paint.rows(chunk, first, count));
                    }
                });
            }
        }

        let (buffer, _) = &presenter.buffers[d.slot];
        presenter.surface.attach(Some(buffer), 0, 0);
        presenter
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        // Requested before `commit`, per wl_surface.frame — same reasoning
        // as `present_frame_cpu`.
        presenter.surface.frame(handle, d.index);
        if let Some(presentation) = presentation.as_ref() {
            timing.request(snapshot_id, d.index);
            presentation.feedback(
                &presenter.surface,
                handle,
                (d.index, snapshot_id, presenter.warp_revision),
            );
        }
        presenter.commit_sent(snapshot_id, presentation.is_some());
        presenter.surface.commit();
        committed_any = true;
    }
    if committed_any {
        state.stats.presented += 1;
    }
    committed_any
}

/// GPU path: upload each due output's shapes, draw them into the Present
/// dmabufs with one submit, and commit through `gpu_present_due` — the same
/// images and the same commit path content uses, which is what keeps
/// `zeroCopyPresented` at n/n while the pattern is showing.
fn present_sync_gpu(
    state: &mut State,
    handle: &QueueHandle<State>,
    frame: u32,
    unix_ms: u64,
    due: Vec<Due>,
) -> bool {
    let snapshot_id = state.timing.snapshot_id;
    let State {
        presenters, gpu, ..
    } = state;
    let Some(gpu) = gpu.as_mut() else {
        return false;
    };
    for d in &due {
        let presenter = &presenters[d.index];
        let Some((width, height)) = presenter.configured else {
            continue;
        };
        let groups =
            super::pattern::sync_rects(width, height, frame, &presenter.name, snapshot_id, unix_ms);
        let (count, items) = sync_shape_items(&groups);
        if let Err(error) = gpu.set_sync_shapes(d.index, count, &items) {
            eprintln!(
                "slicer: output {}: uploading the sync pattern failed: {error:#}",
                presenter.name
            );
        }
    }
    let jobs: Vec<gpu::BlendJob<'_>> = due
        .iter()
        .filter_map(|d| {
            let presenter = &presenters[d.index];
            let (_, image) = presenter.gpu_buffers.get(d.slot)?;
            Some(gpu::BlendJob {
                target: image,
                output: d.index,
                // Unused in sync mode — the shader takes its color from the
                // shape list, not from a canvas — but carried so a job is
                // one thing whichever mode built it.
                source_x: presenter.source.x.max(0) as u32,
                source_y: presenter.source.y.max(0) as u32,
                warp: presenter.warp.as_ref(),
            })
        })
        .collect();
    match gpu.sync(&jobs) {
        Ok(duration) => {
            state.stats.gpu += duration;
            gpu_present_due(state, handle, &due);
            true
        }
        Err(error) => {
            eprintln!("slicer: gpu.sync failed: {error:#}");
            for d in &due {
                state.presenters[d.index].busy[d.slot] = false;
            }
            false
        }
    }
}

/// Pack [`SyncGroup`]s into the flat `uvec4` array `blend.frag`'s `Shapes`
/// block walks: per group, the bounding box `(x0, y0, x1, y1)`, then
/// `(rect_count, next_group_index, 0, 0)`, then the rectangles. Returns the
/// group count alongside, for the draw's push constants.
///
/// The `next` link is what lets groups vary in length without the shader
/// needing a second array or a stride: it reads the header, decides whether
/// the pixel is in the box at all, and jumps.
fn sync_shape_items(groups: &[SyncGroup]) -> (u32, Vec<[u32; 4]>) {
    let mut items =
        Vec::with_capacity(groups.len() * 2 + groups.iter().map(|g| g.rects.len()).sum::<usize>());
    for group in groups {
        items.push([
            group.bounds.x0,
            group.bounds.y0,
            group.bounds.x1,
            group.bounds.y1,
        ]);
        let header = items.len();
        items.push([group.rects.len() as u32, 0, 0, 0]);
        for rect in &group.rects {
            items.push([rect.x0, rect.y0, rect.x1, rect.y1]);
        }
        items[header][1] = items.len() as u32;
    }
    (groups.len() as u32, items)
}

/// Everything a `sync` rasterizing worker needs that does not vary between
/// rows — [`Blend`]'s counterpart for a pattern with no canvas behind it.
struct SyncPaint<'a> {
    transfer: &'a [(u16, u8)],
    rects: &'a [SyncRect],
    width: u32,
}

impl SyncPaint<'_> {
    /// Paint `count` destination rows, `dst` starting at row `first`.
    ///
    /// Black everywhere, then white inside every rectangle that crosses the
    /// row — with the same fixed-point transfer `Blend::rows` applies, so
    /// ramps and black lift shape the counter exactly as they shape content
    /// and the two renderers produce the same bytes. Black is not zero once
    /// the lift is on: `out = ((a*0)>>8) + b` is `b`.
    fn rows(&self, dst: &mut [u8], first: u32, count: u32) {
        let row_bytes = self.width as usize * 4;
        for y in 0..count {
            let target = first + y;
            let dst_row = y as usize * row_bytes;
            let transfer_row = target as usize * self.width as usize;
            for x in 0..self.width as usize {
                let (_, b) = self
                    .transfer
                    .get(transfer_row + x)
                    .copied()
                    .unwrap_or((256, 0));
                dst[dst_row + x * 4..dst_row + x * 4 + 4].copy_from_slice(&[b, b, b, 255]);
            }
            for rect in self.rects {
                if target < rect.y0 || target >= rect.y1 {
                    continue;
                }
                let from = rect.x0 as usize;
                let to = (rect.x1 as usize).min(self.width as usize);
                for x in from..to {
                    let (a, b) = self
                        .transfer
                        .get(transfer_row + x)
                        .copied()
                        .unwrap_or((256, 0));
                    let v = (((u32::from(a) * 255) >> 8) + u32::from(b)).min(255) as u8;
                    dst[dst_row + x * 4..dst_row + x * 4 + 4].copy_from_slice(&[v, v, v, 255]);
                }
            }
        }
    }
}

impl State {
    /// A `wl_registry` global named `removed` just went away. If it is one
    /// of the outputs this slicer captures from or presents to, this is the
    /// slicer's own outputs being torn down under it — end the process the
    /// same way the layer surface's own `Closed` event already does, and say
    /// so. Returns whether it was.
    ///
    /// Split out from the `Dispatch<WlRegistry, GlobalListContents>` impl
    /// below (its only real caller) so the decision is testable on its own,
    /// without the live Wayland connection that trait's other parameters
    /// require.
    ///
    /// A compositor that destroys and recreates an output removes its
    /// `wl_output` global and later advertises a new one — under a new
    /// registry name, even when the output keeps the same name string. That
    /// is exactly what happens on every enable/disable-together commit (see
    /// the `output-phase` fix's note in `reconciler::mod`) and exactly what
    /// unplugging and replugging a projector does. Measured on a
    /// four-projector bench on 2026-09-15: without this, the already-running
    /// slicer kept presenting to the layer surfaces it had built against the
    /// old outputs, and `GET /api/v1/projection/stats` read `presented 0
    /// discarded 605` on all four for a full ten-second interval while the
    /// process itself kept reporting 60.4 fps — the wall was black and
    /// nothing said so. Closing here lets the daemon's `sync_slicer` reap
    /// this exit and respawn against whatever exists now.
    fn handle_output_removed(&mut self, removed: u32) -> bool {
        if !self.used_outputs.contains(&removed) {
            return false;
        }
        let label = self
            .outputs
            .iter()
            .find(|(.., global_name)| *global_name == removed)
            .and_then(|(_, output_name, ..)| output_name.clone())
            .unwrap_or_else(|| format!("registry name {removed}"));
        eprintln!(
            "slicer: output {label} was removed by the compositor; exiting so the daemon's \
             sync_slicer respawns against whatever replaces it"
        );
        self.closed = true;
        true
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::GlobalRemove { name } = event {
            state.handle_output_removed(name);
        }
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
        match event {
            wl_output::Event::Name { name } => {
                for (candidate, stored, ..) in &mut state.outputs {
                    if candidate == output {
                        *stored = Some(name);
                        break;
                    }
                }
            }
            wl_output::Event::Mode {
                flags,
                refresh,
                width,
                height,
            } => {
                // Only the current mode — a compositor may still advertise
                // deprecated non-current ones (see wl_output's own doc).
                let current = flags
                    .into_result()
                    .map(|f| f.contains(wl_output::Mode::Current))
                    .unwrap_or(false);
                if current {
                    // The registry name, so the mode can be filed in
                    // `output_modes` once this loop's borrow of `outputs`
                    // has ended.
                    let mut global = None;
                    for (candidate, _, stored_refresh, global_name) in &mut state.outputs {
                        if candidate == output {
                            *stored_refresh = Some(refresh);
                            global = Some(*global_name);
                            break;
                        }
                    }
                    if let Some(global) = global {
                        state.output_modes.insert(global, (width, height));
                    }
                }
            }
            _ => {}
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
        handle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                let compositor = state.compositor.clone();
                if let Some(presenter) = state.presenters.get_mut(*index) {
                    if state.warp_updates.is_some()
                        && presenter
                            .configured
                            .is_some_and(|size| size != (width, height))
                    {
                        eprintln!(
                            "slicer: output {} resized; restarting geometry topology",
                            presenter.name
                        );
                        state.closed = true;
                        return;
                    }
                    presenter.configured = Some((width, height));
                    // A surface wlroots knows is fully opaque is one it may
                    // flip straight to the plane instead of compositing:
                    // nothing can show through, so nothing needs blending.
                    // The slicer's slices are opaque by construction (an
                    // alpha-less XR24/XB24 buffer covering the whole
                    // output), but the compositor has no way to know that
                    // until it is told. Set here rather than at creation
                    // because it needs the size the configure just brought,
                    // and reset on any later resize; it is pending state,
                    // applied by the next commit, which is the one that
                    // will attach a buffer of this size anyway.
                    if presenter.opaque_for != Some((width, height)) {
                        if let Some(compositor) = compositor {
                            let region: WlRegion = compositor.create_region(handle, ());
                            region.add(0, 0, width as i32, height as i32);
                            presenter.surface.set_opaque_region(Some(&region));
                            region.destroy();
                            presenter.opaque_for = Some((width, height));
                        }
                    }
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
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                state.capture.dmabuf_offer = Some((format, width, height));
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
        // `Done` is the callback's only event: the compositor committed an
        // output frame carrying the commit this callback was requested for.
        // Not the flip — see the module doc's "Presentation gating" — so
        // this only opens the gate where there is no `wp_presentation` to
        // anchor it to, and only then does it clear `stalled` (the flag that
        // lets an output rejoin the gate once it answers again): an output
        // that has stopped presenting while still answering frame callbacks
        // must not bounce in and out of the gate every frame.
        if let wl_callback::Event::Done { .. } = event {
            let gated_on_callbacks = state.presentation.is_none();
            if let Some(presenter) = state.presenters.get_mut(*index) {
                presenter.frame_pending = false;
                presenter.pending_since = None;
                if gated_on_callbacks {
                    presenter.stalled = false;
                }
            }
        }
    }
}

/// Presenter buffers only — every capture buffer (both GPU-path capture
/// images and the shm capture buffer) keeps the `()` behavior below via
/// `delegate_noop!`: nothing needs to know which one a `wl_buffer.release`
/// belongs to, since a capture buffer's readiness is learned from
/// screencopy's own `Ready`/`Failed` events, not from `wl_buffer.release`.
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

impl Dispatch<WpPresentationFeedback, (usize, u64, u64)> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        data: &(usize, u64, u64),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let (index, snapshot_id, warp_revision) = *data;
        // Either answer ends this presenter's wait — the protocol promises
        // exactly one of them per feedback object, and the gate cares that
        // the commit is resolved, not that it was shown. `>=` rather than
        // `==` because a *stalled* output is committed to without the gate
        // waiting for it, so it can have more than one answer outstanding;
        // an answer older than the wait in hand must not end it.
        if matches!(
            &event,
            wp_presentation_feedback::Event::Presented { .. }
                | wp_presentation_feedback::Event::Discarded
        ) {
            let gated_on_feedback = state.presentation.is_some();
            if let Some(presenter) = state.presenters.get_mut(index) {
                if presenter
                    .feedback_pending_for
                    .is_some_and(|pending| snapshot_id >= pending)
                {
                    presenter.feedback_pending_for = None;
                    presenter.feedback_since = None;
                    if gated_on_feedback {
                        presenter.stalled = false;
                    }
                }
            }
        }
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                flags,
                ..
            } => {
                let at_ns = ((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo)) * 1_000_000_000
                    + u64::from(tv_nsec);
                // Only the `zero_copy` bit (3) is counted today. `flags` is
                // decoded as the full `Kind` bitfield rather than masked by
                // hand, so `vsync` (bit 0) and `hw_completion` (bit 2) stay
                // one `.contains(...)` away without revisiting this decode.
                let zero_copy = flags
                    .into_result()
                    .map(|f| f.contains(wp_presentation_feedback::Kind::ZeroCopy))
                    .unwrap_or(false);
                if let Some(presenter) = state.presenters.get_mut(index) {
                    if presenter
                        .warp_reported_revision
                        .is_none_or(|r| warp_revision > r)
                    {
                        presenter.warp_reported_revision = Some(warp_revision);
                        // Journal time is feedback receipt, not the compositor's
                        // clock domain or camera-measured optical latency.
                        if let Some(controller) = &state.warp_updates {
                            controller.event(
                                warp_revision,
                                super::control::ControlEventKind::Presented {
                                    outputs: vec![presenter.name.clone()],
                                },
                            );
                        }
                    }
                }
                state
                    .timing
                    .presented(snapshot_id, index, at_ns, refresh, zero_copy);
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

/// The only place `zwp_linux_dmabuf_feedback_v1`'s events land — forwarded
/// into whichever `dmabuf::FeedbackCollector` `negotiate_gpu` is currently
/// running (`None` once that call has already taken it and moved on, in
/// which case a late event is simply dropped: harmless, since the decision
/// this feedback informs is made once and not revisited).
impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(feedback) = state.dmabuf_feedback.as_mut() else {
            return;
        };
        match event {
            zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } => {
                feedback.main_device(&device);
            }
            zwp_linux_dmabuf_feedback_v1::Event::FormatTable { fd, size } => {
                feedback.format_table(fd, size);
            }
            zwp_linux_dmabuf_feedback_v1::Event::TrancheTargetDevice { device } => {
                feedback.tranche_target_device(&device);
            }
            zwp_linux_dmabuf_feedback_v1::Event::TrancheFlags { flags } => {
                feedback.tranche_flags(flags.into())
            }
            zwp_linux_dmabuf_feedback_v1::Event::TrancheFormats { indices } => {
                feedback.tranche_formats(&indices);
            }
            zwp_linux_dmabuf_feedback_v1::Event::TrancheDone => feedback.tranche_done(),
            zwp_linux_dmabuf_feedback_v1::Event::Done => feedback.done(),
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
// The deprecated-since-4 `format`/`modifier` events are the only ones this
// interface itself can send, and a compositor must not send them once we
// bind version 4 (see the protocol doc) — nothing to react to either way.
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
// `create_params` objects are always finished with `create_immed`, which
// sends no event of its own on success (see `dmabuf.rs`'s doc on
// `dmabuf_wl_buffer`) — `created`/`failed` only fire for the non-immediate
// `create` request, which this module never uses.
delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);

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
            usable_width: 10,
            usable_height: 9,
            source_x: 2,
            source_y: 1,
            width: 6,
            sample: None,
        }
    }

    /// Splitting the work across workers must produce byte-identical output.
    ///
    /// The band arithmetic is the whole risk of parallelizing this: a worker
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
    fn y_invert_is_honored_per_band() {
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

    fn sampled_blend<'a>(
        canvas: &'a [u8],
        transfer: &'a [(u16, u8)],
        sample: &'a AxisSamples,
        width: u32,
    ) -> Blend<'a> {
        Blend {
            canvas,
            transfer,
            format: PixelFormat {
                bytes: 4,
                red: 2,
                green: 1,
                blue: 0,
            },
            stride: 16,
            y_invert: false,
            usable_width: 4,
            usable_height: 1,
            source_x: 0,
            source_y: 0,
            width,
            sample: Some(sample),
        }
    }

    #[test]
    fn cpu_simple_downscale_uses_bilinear_sampling() {
        let mut canvas = vec![0u8; 16];
        for (index, value) in [0u8, 100, 200, 255].into_iter().enumerate() {
            canvas[index * 4..index * 4 + 4].copy_from_slice(&[value, value, value, 255]);
        }
        let transfer = vec![(256, 0); 2];
        let warp = super::super::warp::Warp::identity(2, 1)
            .with_source_rect([0.0, 0.0, 4.0, 1.0])
            .unwrap();
        let sample = AxisSamples::build(&warp, [0.0, 0.0]);
        let blend = sampled_blend(&canvas, &transfer, &sample, 2);
        let mut output = vec![0u8; 8];
        blend.rows(&mut output, 0, 1);

        assert_eq!(&output[..4], &[50, 50, 50, 255]);
        assert_eq!(&output[4..], &[228, 228, 228, 255]);
    }

    #[test]
    fn cpu_simple_upscale_uses_bilinear_sampling() {
        let canvas = vec![0, 0, 0, 255, 200, 200, 200, 255];
        let transfer = vec![(256, 0); 4];
        let warp = super::super::warp::Warp::identity(4, 1)
            .with_source_rect([0.0, 0.0, 2.0, 1.0])
            .unwrap();
        let sample = AxisSamples::build(&warp, [0.0, 0.0]);
        let blend = Blend {
            canvas: &canvas,
            transfer: &transfer,
            format: PixelFormat {
                bytes: 4,
                red: 2,
                green: 1,
                blue: 0,
            },
            stride: 8,
            y_invert: false,
            usable_width: 2,
            usable_height: 1,
            source_x: 0,
            source_y: 0,
            width: 4,
            sample: Some(&sample),
        };
        let mut output = vec![0u8; 16];
        blend.rows(&mut output, 0, 1);

        assert_eq!(
            output
                .chunks_exact(4)
                .map(|pixel| pixel[0])
                .collect::<Vec<_>>(),
            vec![0, 50, 150, 200]
        );
        assert!(output.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn cpu_simple_fractional_crop_honors_y_inversion() {
        let mut canvas = vec![0u8; 4 * 3 * 4];
        for (y, row) in canvas.chunks_exact_mut(16).enumerate() {
            for pixel in row.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[(y * 100) as u8, (y * 100) as u8, (y * 100) as u8, 255]);
            }
        }
        let transfer = vec![(256, 0); 4];
        let warp = super::super::warp::Warp::identity(2, 2)
            .with_source_rect([0.5, 0.25, 2.0, 2.0])
            .unwrap();
        let sample = AxisSamples::build(&warp, [0.0, 0.0]);
        let blend = Blend {
            canvas: &canvas,
            transfer: &transfer,
            format: PixelFormat {
                bytes: 4,
                red: 2,
                green: 1,
                blue: 0,
            },
            stride: 16,
            y_invert: true,
            usable_width: 4,
            usable_height: 3,
            source_x: 0,
            source_y: 0,
            width: 2,
            sample: Some(&sample),
        };
        let mut output = vec![0u8; 16];
        blend.rows(&mut output, 0, 2);

        assert_eq!(&output[..4], &[175, 175, 175, 255]);
        assert_eq!(&output[8..12], &[75, 75, 75, 255]);
    }

    #[test]
    fn cpu_simple_unit_density_copy_preserves_source_bytes() {
        let pixels = canvas(9, 40);
        let transfer = vec![(256, 0); 6];
        let blend = blend_for(&pixels, &transfer);
        let mut output = vec![0u8; 6 * 4];
        blend.rows(&mut output, 0, 1);

        for x in 0..6usize {
            let source = 40usize + (2 + x) * 4;
            assert_eq!(
                &output[x * 4..x * 4 + 4],
                &[pixels[source], pixels[source + 1], pixels[source + 2], 255]
            );
        }
    }

    #[test]
    fn cpu_simple_crop_clips_outside_canvas_to_opaque_black() {
        let mut canvas = vec![0u8; 16];
        for pixel in canvas.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[80, 80, 80, 255]);
        }
        let transfer = vec![(256, 0); 4];
        let warp = super::super::warp::Warp::identity(4, 1)
            .with_source_rect([-0.25, 0.0, 4.0, 1.0])
            .unwrap();
        let sample = AxisSamples::build(&warp, [0.0, 0.0]);
        let blend = sampled_blend(&canvas, &transfer, &sample, 4);
        let mut output = vec![0u8; 16];
        blend.rows(&mut output, 0, 1);

        assert_eq!(&output[..4], &[0, 0, 0, 255]);
        assert_eq!(&output[4..8], &[80, 80, 80, 255]);
    }

    #[test]
    fn fractional_shared_crop_requires_linear_capture_filtering() {
        let mut spec = pattern_spec();
        assert!(!requires_linear_sampling(&spec).unwrap());
        spec.slices[0].source_rect = Some([30.25, 20.0, 240.0, 180.0]);
        assert!(requires_linear_sampling(&spec).unwrap());
    }

    // --- presentation-feedback settling --------------------------------

    /// Defaults `zero_copy` to `false` — every existing test here is about
    /// offset/straddle/phase arithmetic, which does not care about it. Tests
    /// that do care use [`presented_zero_copy`] instead.
    fn presented(at_ms: u64, refresh_hz: f64) -> Slot {
        presented_with(at_ms, refresh_hz, false)
    }

    fn presented_zero_copy(at_ms: u64, refresh_hz: f64) -> Slot {
        presented_with(at_ms, refresh_hz, true)
    }

    fn presented_with(at_ms: u64, refresh_hz: f64, zero_copy: bool) -> Slot {
        let refresh_ns = if refresh_hz == 0.0 {
            0
        } else {
            (1_000_000_000.0 / refresh_hz).round() as u32
        };
        Slot::Presented {
            at_ns: at_ms * 1_000_000,
            refresh_ns,
            zero_copy,
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
                        refresh_ns: Some(16_666_667),
                        zero_copy: false,
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

    // --- zero-copy tally --------------------------------------------------

    #[test]
    fn a_zero_copy_presentation_increments_the_output_s_counter() {
        let mut timing = Timing::new(1);
        timing.request(1, 0);
        timing.presented(1, 0, 1_000_000, 16_666_667, true);
        assert_eq!(timing.per_output[0].presented, 1);
        assert_eq!(timing.per_output[0].zero_copy_presented, 1);
    }

    #[test]
    fn a_non_zero_copy_presentation_leaves_the_counter_at_zero() {
        let mut timing = Timing::new(1);
        timing.request(1, 0);
        timing.presented(1, 0, 1_000_000, 16_666_667, false);
        assert_eq!(timing.per_output[0].presented, 1);
        assert_eq!(timing.per_output[0].zero_copy_presented, 0);
    }

    #[test]
    fn settle_carries_the_zero_copy_flag_into_the_outcome() {
        let settled = settle(&[presented_zero_copy(1000, 60.0), presented(1003, 60.0)]);
        assert_eq!(
            settled.outcomes,
            vec![
                (
                    0,
                    SettledOutcome::Presented {
                        refresh_ns: Some(16_666_667),
                        zero_copy: true,
                    }
                ),
                (
                    1,
                    SettledOutcome::Presented {
                        refresh_ns: Some(16_666_667),
                        zero_copy: false,
                    }
                ),
            ]
        );
    }

    // --- per-output vblank phase ----------------------------------------

    /// 60 Hz's refresh period as `presented()` above rounds it, ns.
    const REFRESH_60HZ_NS: i64 = 16_666_667;

    #[test]
    fn reduce_phase_keeps_a_value_already_within_half_a_period() {
        assert_eq!(reduce_phase(3_000_000, REFRESH_60HZ_NS), 3_000_000);
    }

    #[test]
    fn reduce_phase_wraps_a_value_past_half_a_period() {
        // 15 ms is on the far side of half a 60 Hz period (8.33 ms), so the
        // representative closer to zero is one period earlier: -1.67 ms.
        let reduced = reduce_phase(15_000_000, REFRESH_60HZ_NS);
        assert!(
            (reduced as f64 / 1_000_000.0 - (-1.666_667)).abs() < 0.001,
            "got {reduced} ns"
        );
    }

    #[test]
    fn circular_mean_of_a_symmetric_pair_does_not_cancel_to_zero() {
        let period_ns = REFRESH_60HZ_NS as u32;
        let mut accum = PhaseAccum::default();
        accum.add(reduce_phase(8_000_000, REFRESH_60HZ_NS), period_ns);
        accum.add(reduce_phase(-8_000_000, REFRESH_60HZ_NS), period_ns);
        let phase = accum.phase_ms().expect("two samples were added");
        // A plain mean of +8.0 and -8.0 would report 0.0 ms - dead in phase.
        // The pair actually sits right at the wrap boundary, on the far edge
        // from zero, so the circular mean must land near +-8.33 ms instead.
        assert!(
            (phase.abs() - 8.333).abs() < 0.05,
            "wrap-around pair averaged to {phase} ms, not +-8.33 ms"
        );
    }

    #[test]
    fn a_consistent_offset_over_many_frames_reports_cleanly() {
        let period_ns = REFRESH_60HZ_NS as u32;
        let mut accum = PhaseAccum::default();
        for _ in 0..100 {
            accum.add(reduce_phase(5_000_000, REFRESH_60HZ_NS), period_ns);
        }
        let phase = accum.phase_ms().expect("100 samples were added");
        assert!((phase - 5.0).abs() < 0.01, "got {phase} ms");
        assert_eq!(accum.spread_ms(), Some(0.0));
    }

    #[test]
    fn an_accumulator_with_no_samples_reports_nothing() {
        let accum = PhaseAccum::default();
        assert_eq!(accum.phase_ms(), None);
        assert_eq!(accum.spread_ms(), None);
    }

    #[test]
    fn settle_reports_phase_relative_to_output_zero() {
        let settled = settle(&[presented(1000, 60.0), presented(1003, 60.0)]);
        assert_eq!(
            settled.phase_samples,
            vec![(1, 3_000_000, REFRESH_60HZ_NS as u32)]
        );
    }

    #[test]
    fn settle_has_no_phase_samples_when_output_zero_did_not_present() {
        // Only presenter 1 presented; there is nothing to measure it against.
        let settled = settle(&[Slot::Discarded, presented(1003, 60.0)]);
        assert!(settled.phase_samples.is_empty());
    }

    // --- per-output lag in frames ----------------------------------------

    /// Builds a `Presented` slot at an exact nanosecond timestamp, unlike
    /// `presented`/`presented_with` above which only offer millisecond
    /// precision — needed here to land exactly on refresh-period multiples.
    fn presented_at_ns(at_ns: u64, refresh_ns: u32) -> Slot {
        Slot::Presented {
            at_ns,
            refresh_ns,
            zero_copy: false,
        }
    }

    #[test]
    fn two_outputs_one_refresh_apart_lag_one_on_the_later_output() {
        let refresh_ns = REFRESH_60HZ_NS as u32;
        let settled = settle(&[
            presented_at_ns(0, refresh_ns),
            presented_at_ns(u64::from(refresh_ns), refresh_ns),
        ]);
        assert_eq!(settled.lag_frames, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn three_outputs_one_two_refreshes_behind() {
        let refresh_ns = REFRESH_60HZ_NS as u32;
        let settled = settle(&[
            presented_at_ns(0, refresh_ns),
            presented_at_ns(u64::from(refresh_ns), refresh_ns),
            presented_at_ns(2 * u64::from(refresh_ns), refresh_ns),
        ]);
        assert_eq!(settled.lag_frames, vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn a_lone_presenter_contributes_no_lag_samples() {
        let settled = settle(&[presented(1000, 60.0)]);
        assert!(settled.lag_frames.is_empty());
    }

    #[test]
    fn a_lag_of_three_or_more_refreshes_clamps_to_the_more_bucket() {
        let refresh_ns = REFRESH_60HZ_NS as u32;
        let settled = settle(&[
            presented_at_ns(0, refresh_ns),
            presented_at_ns(5 * u64::from(refresh_ns), refresh_ns),
        ]);
        assert_eq!(settled.lag_frames, vec![(0, 0), (1, 3)]);
    }

    #[test]
    fn the_lag_histogram_resets_between_report_intervals() {
        let refresh_ns = REFRESH_60HZ_NS as u32;
        let mut timing = Timing::new(2);
        timing.request(1, 0);
        timing.request(1, 1);
        timing.presented(1, 0, 0, refresh_ns, false);
        timing.presented(1, 1, u64::from(refresh_ns), refresh_ns, false);
        assert_eq!(timing.per_output[1].lag_frames.one, 1);

        let interval = timing.drain_interval();
        assert_eq!(interval.outputs[1].lag_frames.one, 1);
        assert_eq!(timing.per_output[1].lag_frames, LagFrames::default());

        // A second interval with no lagging presentation must not carry the
        // first interval's `one` tally forward.
        timing.request(2, 0);
        timing.request(2, 1);
        timing.presented(2, 0, 0, refresh_ns, false);
        timing.presented(2, 1, 0, refresh_ns, false);
        let interval = timing.drain_interval();
        assert_eq!(
            interval.outputs[1].lag_frames,
            LagFrames {
                zero: 1,
                ..LagFrames::default()
            }
        );
    }

    // --- self-heal: the dead-interval predicate -------------------------

    #[test]
    fn two_consecutive_dead_intervals_trip_the_tracker() {
        let mut tracker = DeadIntervalTracker::default();
        assert!(
            !tracker.record(true, 40, 0),
            "one bad interval is not enough"
        );
        assert!(
            tracker.record(true, 40, 0),
            "a second consecutive bad interval must trip it"
        );
    }

    #[test]
    fn a_single_dead_interval_does_not_trip_it() {
        let mut tracker = DeadIntervalTracker::default();
        assert!(!tracker.record(true, 40, 0));
        // A good interval in between resets the count, so a run of isolated
        // bad ones — a resize here, a hiccup there — never accumulates.
        assert!(!tracker.record(true, 40, 5));
        assert!(!tracker.record(true, 40, 0));
    }

    #[test]
    fn zero_answers_never_trips_it() {
        // The ordinary case for an idle canvas or a compositor that has not
        // answered anything yet: no evidence either way, not "none presented".
        let mut tracker = DeadIntervalTracker::default();
        for _ in 0..5 {
            assert!(!tracker.record(true, 0, 0));
        }
    }

    #[test]
    fn any_presented_frame_resets_the_count() {
        let mut tracker = DeadIntervalTracker::default();
        assert!(!tracker.record(true, 40, 0));
        assert!(
            !tracker.record(true, 40, 1),
            "a presented frame is not itself dead"
        );
        // Back to square one: this alone must not trip it.
        assert!(!tracker.record(true, 40, 0));
    }

    #[test]
    fn without_feedback_the_predicate_never_fires() {
        // `presentation_feedback` false means no `wp_presentation` at all —
        // there is nothing to conclude from, so this must never act blind.
        let mut tracker = DeadIntervalTracker::default();
        assert!(!tracker.record(false, 1000, 0));
        assert!(!tracker.record(false, 1000, 0));
    }

    #[test]
    fn fewer_than_the_threshold_of_answers_does_not_count_as_dead() {
        assert!(!interval_is_dead(true, 29, 0));
        assert!(interval_is_dead(true, 30, 0));
    }

    // --- the presentation gate -------------------------------------------

    /// An entry with nothing outstanding: this presenter has answered.
    fn answered_entry() -> GateEntry {
        GateEntry {
            waiting: false,
            waited: Duration::ZERO,
            stalled: false,
        }
    }

    /// An entry that has owed its answer for `waited`.
    fn waiting_entry(waited: Duration) -> GateEntry {
        GateEntry {
            waiting: true,
            waited,
            stalled: false,
        }
    }

    #[test]
    fn the_gate_is_open_once_every_output_has_answered() {
        let gate = GateState::new(vec![answered_entry(); 3], STALL_TIMEOUT);
        assert!(gate.all_answered(), "nobody owes an answer");
        assert!(gate.newly_stalled().is_empty());
    }

    #[test]
    fn one_outstanding_output_holds_the_gate_shut() {
        // The whole point: two heads have flipped, the third has not, and
        // the wall waits rather than letting the two run a frame ahead.
        let gate = GateState::new(
            vec![
                answered_entry(),
                answered_entry(),
                waiting_entry(Duration::from_millis(8)),
            ],
            STALL_TIMEOUT,
        );
        assert!(!gate.all_answered());
        assert!(
            gate.newly_stalled().is_empty(),
            "8 ms is an ordinary wait, not a stall"
        );
    }

    #[test]
    fn an_output_that_never_answers_is_dropped_from_the_gate_and_counted() {
        // A DPMS-off projector must not freeze the rest of the wall: past
        // the timeout it stops being waited for, and it is named so the
        // stall shows up in the stats.
        let waited = STALL_TIMEOUT + Duration::from_millis(1);
        let gate = GateState::new(vec![answered_entry(), waiting_entry(waited)], STALL_TIMEOUT);
        assert!(
            gate.all_answered(),
            "the straggler no longer holds the gate"
        );
        assert_eq!(gate.newly_stalled(), vec![(1, waited)]);
    }

    #[test]
    fn an_already_stalled_output_is_neither_waited_for_nor_counted_again() {
        // It is still being committed to, so it still owes an answer — but
        // the stall was counted when it was first dropped, and counting it
        // once per frame afterwards would say the wall is failing sixty
        // times a second.
        let entry = GateEntry {
            waiting: true,
            waited: Duration::from_secs(5),
            stalled: true,
        };
        let gate = GateState::new(vec![answered_entry(), entry], STALL_TIMEOUT);
        assert!(gate.all_answered());
        assert!(gate.newly_stalled().is_empty());
    }

    #[test]
    fn an_empty_wall_never_holds_the_gate_shut() {
        // No presenters, nobody to wait for. Reporting this shut would spin
        // the frame loop at full CPU presenting nothing.
        assert!(GateState::new(Vec::new(), STALL_TIMEOUT).all_answered());
    }

    #[test]
    fn without_wp_presentation_the_gate_falls_back_to_frame_callbacks() {
        // The signal the compositor offers decides which wait is consulted,
        // and the other one is not merely preferred — it is ignored.
        assert_eq!(gate_signal(true), GateSignal::Presentation);
        assert_eq!(gate_signal(false), GateSignal::FrameCallback);

        let frame = Some(Duration::from_millis(4));
        let feedback = None;
        assert!(
            !gate_entry(GateSignal::FrameCallback, frame, feedback, false).answered(STALL_TIMEOUT),
            "a compositor with no wp_presentation still waits for the callback"
        );
        assert!(
            gate_entry(GateSignal::Presentation, frame, feedback, false).answered(STALL_TIMEOUT),
            "with feedback in hand an outstanding callback is not what the gate waits on"
        );

        // And the other way round: the callback came back, the flip has not.
        // This is the case `brain` was failing — every output answering its
        // callback while one was a whole refresh behind.
        let late_flip = Some(Duration::from_millis(12));
        assert!(
            !gate_entry(GateSignal::Presentation, None, late_flip, false).answered(STALL_TIMEOUT),
            "the flip has not landed, so the gate holds"
        );
        assert!(
            gate_entry(GateSignal::FrameCallback, None, late_flip, false).answered(STALL_TIMEOUT),
            "the callback gate sees nothing outstanding — which is why it never held"
        );
    }

    // --- self-heal: noticing an output's global disappear ----------------

    /// A `State` with no real Wayland objects at all — every field that
    /// would otherwise need a live connection is empty or `None`.
    /// `handle_output_removed` never dereferences a `WlOutput` proxy, so an
    /// empty `outputs` costs it nothing but the human-readable label (it
    /// falls back to printing the registry name instead).
    fn bare_state(used_outputs: Vec<u32>) -> State {
        State {
            outputs: Vec::new(),
            used_outputs,
            output_modes: HashMap::new(),
            compositor: None,
            presenters: Vec::new(),
            capture: Capture::default(),
            closed: false,
            dead_intervals: DeadIntervalTracker::default(),
            free_run: false,
            renderer: Renderer::Cpu,
            presentation: None,
            timing: Timing::new(0),
            stats: FrameStats::new(),
            gpu: None,
            gpu_formats: DmabufFormats::default(),
            gpu_error: None,
            present_format: None,
            dmabuf_feedback: None,
            canvas_period_ms: 1000.0 / 60.0,
            last_capture_at: None,
            gate_blocked_since: None,
            warp_updates: None,
            warp_available: false,
            requires_linear_sampling: false,
            static_canvases: Vec::new(),
            gpu_retry_after: None,
            adaptive: None,
            capture_id: 0,
            capture_measurement: None,
            measurement: super::super::adaptive::MeasurementSchedule::default(),
            stdout: super::super::control::StdoutWriter::spawn(),
        }
    }

    fn adaptive_spec() -> SlicerSpec {
        let mut spec = pattern_spec();
        spec.adaptive_lift = Some(crate::model::AdaptiveBlackLift {
            level: 0.2,
            dark_threshold: 0.02,
            bright_threshold: 0.2,
            rise_ms: 1000.0,
            fall_ms: 250.0,
            slew_per_second: 0.1,
        });
        spec.black_lift = 0.2;
        spec
    }

    #[test]
    fn adaptive_static_source_converges_without_recapture_or_table_builds() {
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        configure_adaptive(&mut state, &adaptive_spec(), 7);
        state.capture_id = 41;
        let start = Instant::now();
        let runtime = state.adaptive.as_mut().unwrap();
        runtime
            .controller
            .measure_with_capture(1.0, 65536, Some(41));
        runtime.status.capture_id = Some(41);
        runtime.next_tick = Some(start + super::super::adaptive::CONTROLLER_TICK);
        for n in 1..=500 {
            tick_adaptive_at(&mut state, start + Duration::from_millis(n * 20));
        }
        let runtime = state.adaptive.as_ref().unwrap();
        assert_eq!(runtime.controller.level(), 0.0);
        assert!(runtime.next_tick.is_none());
        assert_eq!(runtime.status.capture_id, Some(41));
        assert_eq!(state.capture_id, 41);
        assert_eq!(state.stats.captured, 0);
        assert!(state.timing.snapshot_id > 1);
        assert!(
            state.warp_updates.is_none(),
            "lift does not enlist a table worker"
        );
        let settled_generation = state.timing.snapshot_id;
        tick_adaptive_at(&mut state, start + Duration::from_secs(30));
        assert_eq!(state.timing.snapshot_id, settled_generation);
    }

    #[test]
    fn first_settled_measurement_is_reported_without_removing_the_rate_bound() {
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        configure_adaptive(&mut state, &adaptive_spec(), 7);
        state.capture_id = 41;
        state.capture_measurement = Some(CapturedLuminance {
            capture_id: 41,
            result: Ok((0.0, 65536)),
            measured_at_unix_ms: 1000,
            latency_ms: 2.0,
        });
        // Keep the report window shut so this specifically exercises the
        // first-valid-sample transition, rather than an ordinary timer tick.
        state.adaptive.as_mut().unwrap().next_report = Instant::now() + Duration::from_secs(1);

        assert!(measure_adaptive(&mut state, 0));
        let runtime = state.adaptive.as_ref().unwrap();
        assert!(runtime.status.available);
        assert!(runtime.status.reason.is_none());
        assert!(
            runtime.next_tick.is_none(),
            "the sample is already at target"
        );

        // A subsequent settled capture remains subject to the 100 ms report
        // bound even though it also needs no controller tick.
        state.capture_id = 42;
        state.capture_measurement = Some(CapturedLuminance {
            capture_id: 42,
            result: Ok((0.0, 65536)),
            measured_at_unix_ms: 1001,
            latency_ms: 2.0,
        });
        state.adaptive.as_mut().unwrap().next_report = Instant::now() + Duration::from_secs(1);
        assert!(!measure_adaptive(&mut state, 0));
        let report_at = state.adaptive.as_ref().unwrap().next_tick.unwrap();
        tick_adaptive_at(&mut state, report_at);
        assert!(state.adaptive.as_ref().unwrap().next_tick.is_none());
    }

    #[test]
    fn startup_holds_the_fixed_level_until_the_first_pass_is_collected() {
        // Nothing has been measured yet and, with measurement asynchronous,
        // nothing can have been: the first capture queues a pass and the
        // controller keeps reporting the configured fixed level rather than
        // stalling the render thread to obtain a value on the spot.
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        let spec = adaptive_spec();
        configure_adaptive(&mut state, &spec, 7);
        state.capture_id = 2;
        state.capture.gpu_slot = 1;
        // The first pass is queued and its fence has not signaled yet.
        state
            .measurement
            .submitted(super::super::adaptive::MeasurementFlight {
                capture_id: 1,
                slot: 0,
                submitted_at: Instant::now(),
            });
        assert!(!measure_adaptive(&mut state, 1));
        let runtime = state.adaptive.as_ref().unwrap();
        assert_eq!(runtime.controller.level(), spec.black_lift);
        assert_eq!(
            runtime.controller.status(),
            super::super::adaptive::ControllerStatus::Startup
        );
    }

    #[test]
    fn the_slot_the_next_capture_will_fill_is_never_measured() {
        // The compositor writes into the armed slot with no fence of its
        // own, so a pass reading it would be sampling a frame as it is
        // overwritten. Attempting the submission at all would be visible
        // here as a refusal recorded against the capture.
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        configure_adaptive(&mut state, &adaptive_spec(), 7);
        state.capture_id = 5;
        state.capture.gpu_slot = 0;
        measure_adaptive(&mut state, 0);
        assert!(
            state.capture_measurement.is_none(),
            "no submission should have been attempted for the armed slot"
        );
        assert!(state.measurement.flight().is_none());
    }

    #[test]
    fn a_measurement_in_flight_defers_the_next_submission_and_survives_the_capture() {
        // Every completed capture runs `measure_adaptive`; only one pass may
        // be outstanding, and the one already submitted must still be there
        // afterwards — it is what the next capture collects, and what the
        // arm of its slot retires.
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        configure_adaptive(&mut state, &adaptive_spec(), 7);
        let flight = super::super::adaptive::MeasurementFlight {
            capture_id: 5,
            slot: 0,
            submitted_at: Instant::now(),
        };
        state.measurement.submitted(flight);
        state.capture_id = 6;
        state.capture.gpu_slot = 0;
        measure_adaptive(&mut state, 1);
        assert_eq!(state.measurement.flight(), Some(flight));
        assert!(
            state.capture_measurement.is_none(),
            "a second submission must not even be attempted"
        );
    }

    #[test]
    fn a_collected_measurement_is_applied_tagged_with_the_capture_it_sampled() {
        // The readback is deferred, so the value that arrives describes a
        // capture that is no longer the current one. Reporting it against
        // the current capture id would make every sample look fresh.
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        configure_adaptive(&mut state, &adaptive_spec(), 7);
        state.capture_id = 9;
        state.capture.gpu_slot = 1;
        // Inside the rate limit, so this capture collects and applies
        // without queueing a pass of its own.
        state.measurement.refused(Instant::now());
        state.capture_measurement = Some(CapturedLuminance {
            capture_id: 6,
            result: Ok((1.0, 65536)),
            measured_at_unix_ms: 1000,
            latency_ms: 17.0,
        });
        measure_adaptive(&mut state, 0);
        let runtime = state.adaptive.as_ref().unwrap();
        assert_eq!(runtime.status.capture_id, Some(6));
        assert_eq!(runtime.controller.last_capture_id(), Some(6));
        assert_eq!(runtime.status.measurement_ms, Some(17.0));
        assert!(runtime.status.available);
    }

    #[test]
    fn retiring_a_measured_slot_releases_it_even_with_no_gpu_to_ask() {
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        state
            .measurement
            .submitted(super::super::adaptive::MeasurementFlight {
                capture_id: 3,
                slot: 1,
                submitted_at: Instant::now(),
            });
        // The other slot is untouched: retiring is per image, not global.
        retire_measurement_for_slot(&mut state, 0);
        assert!(state.measurement.reads_slot(1));
        retire_measurement_for_slot(&mut state, 1);
        assert!(state.measurement.flight().is_none());
    }

    #[test]
    fn repeated_moving_measurements_do_not_postpone_controller_tick() {
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        configure_adaptive(&mut state, &adaptive_spec(), 7);
        state.capture_id = 41;
        state.capture_measurement = Some(CapturedLuminance {
            capture_id: 41,
            result: Ok((1.0, 65536)),
            measured_at_unix_ms: 1000,
            latency_ms: 2.0,
        });
        assert!(measure_adaptive(&mut state, 0));
        let deadline = Instant::now() + Duration::from_millis(5);
        state.adaptive.as_mut().unwrap().next_tick = Some(deadline);
        state.capture_id = 42;
        state.capture_measurement = Some(CapturedLuminance {
            capture_id: 42,
            result: Ok((1.0, 65536)),
            measured_at_unix_ms: 1001,
            latency_ms: 2.0,
        });
        let _ = measure_adaptive(&mut state, 0);
        assert_eq!(state.adaptive.as_ref().unwrap().next_tick, Some(deadline));
    }

    #[test]
    fn adaptive_cpu_and_patterns_report_fixed_fallback_without_timer() {
        let mut state = bare_state(vec![]);
        let mut spec = adaptive_spec();
        state.capture.backend = Some(Backend::Cpu);
        configure_adaptive(&mut state, &spec, 0);
        let runtime = state.adaptive.as_ref().unwrap();
        assert!(!runtime.status.available);
        assert!(runtime
            .status
            .reason
            .as_ref()
            .unwrap()
            .contains("GPU capture"));
        assert_eq!(runtime.controller.level(), spec.black_lift);
        assert!(runtime.next_tick.is_none());
        // Patterns are a new child lifetime, as enforced by topology checks.
        state.adaptive = None;
        state.capture.backend = Some(Backend::Gpu);
        spec.pattern = Some(TestPattern::White);
        configure_adaptive(&mut state, &spec, 0);
        let runtime = state.adaptive.as_ref().unwrap();
        assert!(runtime.status.paused);
        assert_eq!(runtime.controller.level(), spec.black_lift);
        assert!(runtime.next_tick.is_none());
    }

    #[test]
    fn geometry_config_install_preserves_measurement_and_controller_progress() {
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        let spec = adaptive_spec();
        configure_adaptive(&mut state, &spec, 7);
        let now = Instant::now();
        let runtime = state.adaptive.as_mut().unwrap();
        runtime.controller.measure_with_capture(1.0, 100, Some(8));
        runtime.controller.tick(now + Duration::from_millis(100));
        let prior = runtime.controller.level();
        configure_adaptive(&mut state, &spec, 9);
        let runtime = state.adaptive.as_ref().unwrap();
        assert_eq!(runtime.generation, 9);
        assert_eq!(runtime.controller.level(), prior);
        assert_eq!(runtime.controller.last_capture_id(), Some(8));
    }

    #[test]
    fn adaptive_settings_and_mode_changes_reuse_the_static_capture_measurement() {
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        state.capture.last_blended_slot = Some(0);
        state.capture_id = 42;
        state.capture_measurement = Some(CapturedLuminance {
            capture_id: 42,
            result: Ok((1.0, 65536)),
            measured_at_unix_ms: 1000,
            latency_ms: 2.0,
        });
        let mut spec = adaptive_spec();
        configure_adaptive(&mut state, &spec, 1);
        assert_eq!(state.adaptive.as_ref().unwrap().controller.target(), 0.0);
        assert!(state.adaptive.as_ref().unwrap().next_tick.is_some());
        spec.adaptive_lift.as_mut().unwrap().level = 0.3;
        spec.black_lift = 0.3;
        configure_adaptive(&mut state, &spec, 2);
        let runtime = state.adaptive.as_ref().unwrap();
        assert_eq!(runtime.controller.target(), 0.0);
        assert_eq!(runtime.status.capture_id, Some(42));
        assert_eq!(runtime.status.measured_at_unix_ms, Some(1000));
        spec.adaptive_lift = None;
        configure_adaptive(&mut state, &spec, 3);
        assert!(state.adaptive.is_none());
        configure_adaptive(&mut state, &adaptive_spec(), 4);
        assert_eq!(state.adaptive.as_ref().unwrap().controller.target(), 0.0);
        assert_eq!(state.capture_id, 42);
        assert!(
            state.gpu.is_none(),
            "all transitions used the one cached measurement"
        );
    }

    #[test]
    fn scene_cut_after_idle_uses_elapsed_time_since_the_measurement() {
        let mut state = bare_state(vec![]);
        state.capture.backend = Some(Backend::Gpu);
        let spec = adaptive_spec();
        configure_adaptive(&mut state, &spec, 0);
        state.adaptive.as_mut().unwrap().controller =
            super::super::adaptive::AdaptiveController::new(
                spec.adaptive_lift.unwrap(),
                Instant::now() - Duration::from_secs(3600),
            );
        state.capture_id = 1;
        state.capture_measurement = Some(CapturedLuminance {
            capture_id: 1,
            result: Ok((1.0, 65536)),
            measured_at_unix_ms: 1000,
            latency_ms: 0.0,
        });
        measure_adaptive(&mut state, 0);
        let next = state.adaptive.as_ref().unwrap().next_tick.unwrap();
        tick_adaptive_at(&mut state, next);
        let level = state.adaptive.as_ref().unwrap().controller.level();
        assert!(
            level > 0.19 && level < 0.2,
            "an hour of idle time must not bypass the scene-cut slew: {level}"
        );
    }

    #[test]
    fn a_global_remove_naming_a_used_output_closes_the_slicer() {
        let mut state = bare_state(vec![42]);
        assert!(state.handle_output_removed(42));
        assert!(state.closed, "the output this slicer presents to went away");
    }

    #[test]
    fn a_global_remove_naming_an_unrelated_global_is_ignored() {
        let mut state = bare_state(vec![42]);
        assert!(!state.handle_output_removed(7));
        assert!(
            !state.closed,
            "a global the slicer neither captures from nor presents to must not stop it"
        );
    }

    // --- direct scanout: which modifiers a Present image is allocated from ---

    #[test]
    fn the_scanout_subset_is_what_the_device_and_the_scanout_tranche_share() {
        assert_eq!(scanout_modifiers(&[1, 2, 3], &[3, 1]), vec![1, 3]);
    }

    #[test]
    fn the_scanout_subset_keeps_the_advertised_order_not_the_tranche_s() {
        // `supported` is the compositor's advertised order (filtered by the
        // device); the tranche listing them the other way round must not
        // reorder what gets offered to `vkCreateImage`.
        assert_eq!(
            scanout_modifiers(
                &[0x0100_0000_0000_0001, 0, 7],
                &[7, 0, 0x0100_0000_0000_0001]
            ),
            vec![0x0100_0000_0000_0001, 0, 7]
        );
    }

    #[test]
    fn nothing_in_common_means_an_empty_subset_and_the_caller_falls_back() {
        assert!(scanout_modifiers(&[1, 2], &[3, 4]).is_empty());
        // The degenerate case: a compositor that flagged no scanout tranche
        // at all for this fourcc.
        assert!(scanout_modifiers(&[1, 2], &[]).is_empty());
    }

    // --- direct scanout: the per-output verdict the log line reports ------

    fn modifiers(all: &[u64], scanout: &[u64]) -> PresentModifiers {
        PresentModifiers {
            all: all.to_vec(),
            scanout: scanout.to_vec(),
        }
    }

    /// The reason text out of a verdict that carries one, so a test can
    /// assert on the wording without unwrapping a specific variant.
    fn verdict_reason(verdict: &ScanoutVerdict) -> String {
        match verdict {
            ScanoutVerdict::Yes { .. } => panic!("expected a reason, got a yes"),
            ScanoutVerdict::No(reason) | ScanoutVerdict::Unconfirmed(reason) => reason.clone(),
        }
    }

    #[test]
    fn a_pixel_exact_buffer_on_a_flagged_modifier_is_a_candidate() {
        assert_eq!(
            scanout_verdict(
                (1920, 1080),
                Some((1920, 1080)),
                FOURCC_XR24,
                &modifiers(&[0, 7], &[7]),
                true,
                7,
            ),
            ScanoutVerdict::Yes {
                fourcc: FOURCC_XR24,
                modifier: 7
            }
        );
    }

    #[test]
    fn an_unknown_output_mode_does_not_disqualify_a_buffer() {
        // A compositor that never sent a current mode (a headless output,
        // say) leaves nothing to compare against; the modifier still decides.
        assert_eq!(
            scanout_verdict(
                (1920, 1080),
                None,
                FOURCC_XR24,
                &modifiers(&[7], &[7]),
                true,
                7,
            ),
            ScanoutVerdict::Yes {
                fourcc: FOURCC_XR24,
                modifier: 7
            }
        );
    }

    #[test]
    fn a_scaled_output_is_disqualified_whatever_modifier_it_got() {
        let verdict = scanout_verdict(
            (1920, 1080),
            Some((3840, 2160)),
            FOURCC_XR24,
            &modifiers(&[7], &[7]),
            true,
            7,
        );
        assert!(
            matches!(verdict, ScanoutVerdict::No(_)),
            "a buffer that is not the output's pixel grid is a definite no, got {verdict:?}"
        );
        let reason = verdict_reason(&verdict);
        assert!(
            reason.contains("1920x1080") && reason.contains("3840x2160"),
            "the reason must name both sizes, got {reason:?}"
        );
        assert!(reason.contains("scale 1"), "got {reason:?}");
    }

    #[test]
    fn a_compositor_that_flagged_no_scanout_tranche_is_unconfirmed_not_a_no() {
        // Regression guard for test-log Entry 2: on `brain` the default
        // feedback named no scanout tranche and 100 % of presented frames
        // were zero-copy regardless, so this branch must not claim a
        // negative it cannot know.
        let verdict = scanout_verdict(
            (1920, 1080),
            Some((1920, 1080)),
            FOURCC_XR24,
            &modifiers(&[0, 7], &[]),
            false,
            7,
        );
        assert!(
            matches!(verdict, ScanoutVerdict::Unconfirmed(_)),
            "an absent tranche is not evidence of a negative, got {verdict:?}"
        );
        let reason = verdict_reason(&verdict);
        assert!(
            reason.contains("no scanout tranche"),
            "the reason must still name what was missing, got {reason:?}"
        );
        assert!(
            reason.contains("zeroCopyPresented"),
            "the reason must point at the authoritative stat, got {reason:?}"
        );
    }

    #[test]
    fn a_flagged_tranche_the_device_cannot_use_says_no_common_modifier() {
        assert_eq!(
            scanout_verdict(
                (1920, 1080),
                Some((1920, 1080)),
                FOURCC_XR24,
                &modifiers(&[0], &[]),
                true,
                0,
            ),
            ScanoutVerdict::No("no common modifier".to_string())
        );
    }

    #[test]
    fn a_driver_that_picked_outside_the_scanout_subset_is_not_a_candidate() {
        // Only reachable through `create_present_image`'s fallback, but that
        // fallback is exactly when the honest answer is "no".
        let verdict = scanout_verdict(
            (1920, 1080),
            Some((1920, 1080)),
            FOURCC_XR24,
            &modifiers(&[0, 7], &[7]),
            true,
            0,
        );
        assert!(
            matches!(verdict, ScanoutVerdict::No(_)),
            "the compositor named a tranche and this modifier is not in it, got {verdict:?}"
        );
        assert!(verdict_reason(&verdict).contains("0x0"), "got {verdict:?}");
    }

    // --- direct scanout: how many Present buffers each path keeps ---------

    #[test]
    fn the_gpu_path_keeps_one_more_present_buffer_than_the_shm_path() {
        // A scanning-out compositor holds an output's on-screen buffer until
        // the next flip completes, so it can hold two at once; the pool needs
        // a third for the frame loop to draw into. shm is never scanned out
        // and keeps the compositing-lifetime pair.
        assert_eq!(present_slots(Some(Backend::Gpu)), 3);
        assert_eq!(present_slots(Some(Backend::Cpu)), 2);
        assert_eq!(
            present_slots(Some(Backend::Gpu)),
            present_slots(Some(Backend::Cpu)) + 1
        );
    }

    #[test]
    fn an_undecided_backend_sizes_the_pool_as_the_shm_path() {
        // `create_present_buffers` reaches its shm branch for `None` as well
        // as for `Backend::Cpu`, and a test pattern is poked into a mapped
        // buffer whatever the backend — both are the two-slot lifetime.
        assert_eq!(present_slots(None), CPU_PRESENT_SLOTS);
    }

    #[test]
    fn a_pool_rotates_through_every_slot_before_reusing_one() {
        // The slot search in `present_frame`/`gpu_blend_due` in miniature:
        // start at `next_buffer`, take the first free slot, advance. With
        // three slots and a compositor holding two, one is always free.
        let len = GPU_PRESENT_SLOTS;
        let mut busy = vec![false; len];
        let mut next_buffer = 0;
        let mut taken = Vec::new();
        for frame in 0..6 {
            let slot = (0..len)
                .map(|step| (next_buffer + step) % len)
                .find(|&candidate| !busy[candidate])
                .expect("three slots against two held by the compositor always leave one free");
            next_buffer = (slot + 1) % len;
            busy[slot] = true;
            taken.push(slot);
            // The compositor releases the buffer from two flips ago, which
            // is the lifetime direct scanout imposes.
            if frame >= 2 {
                busy[taken[frame - 2]] = false;
            }
        }
        assert_eq!(taken, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn a_fourcc_prints_as_the_four_characters_it_spells() {
        assert_eq!(fourcc_name(FOURCC_XR24), "XR24");
        assert_eq!(fourcc_name(FOURCC_XB24), "XB24");
        assert_eq!(fourcc_name(0), "????");
    }

    // --- the `sync` test pattern -----------------------------------------

    /// `blend.frag`'s `Shapes` walk, in Rust: read a group's bounding box,
    /// test it, walk that group's rectangles only if the pixel is inside,
    /// then follow the header's `next` link. Written from the shader rather
    /// than from `sync_shape_items`, so the two have to agree for the tests
    /// below to pass.
    fn shader_walk(items: &[[u32; 4]], groups: u32, x: u32, y: u32) -> bool {
        let mut index = 0usize;
        for _ in 0..groups {
            let box_ = items[index];
            let head = items[index + 1];
            if x >= box_[0] && x < box_[2] && y >= box_[1] && y < box_[3] {
                for rect in 0..head[0] as usize {
                    let r = items[index + 2 + rect];
                    if x >= r[0] && x < r[2] && y >= r[1] && y < r[3] {
                        return true;
                    }
                }
            }
            index = head[1] as usize;
        }
        false
    }

    #[test]
    fn the_packed_shape_list_lights_exactly_the_rectangles_it_was_built_from() {
        // Small enough to check every pixel, large enough that the pattern
        // still has all four of its features.
        let (width, height) = (320u32, 200u32);
        let groups =
            super::super::pattern::sync_rects(width, height, 57, "DP-1", 9_000, 1_700_000_000_000);
        let (count, items) = sync_shape_items(&groups);
        assert_eq!(count as usize, groups.len());
        for y in 0..height {
            for x in 0..width {
                let expected = groups
                    .iter()
                    .any(|group| group.rects.iter().any(|rect| rect.contains(x, y)));
                assert_eq!(
                    shader_walk(&items, count, x, y),
                    expected,
                    "pixel ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn the_last_groups_next_link_points_past_the_end() {
        // The walk reads `next` on every group including the last, so the
        // link has to be the length rather than anything clever.
        let groups = super::super::pattern::sync_rects(640, 480, 3, "DP-2", 1, 1_700_000_000_000);
        let (count, items) = sync_shape_items(&groups);
        let mut index = 0usize;
        for _ in 0..count {
            index = items[index + 1][1] as usize;
        }
        assert_eq!(index, items.len());
    }

    #[test]
    fn the_cpu_rasterizer_and_the_shader_produce_the_same_bytes() {
        // The measurement is only comparable between renderers if they draw
        // the identical frame, so this checks the two halves of that claim
        // against each other with a transfer that is neither identity nor
        // uniform: a ramp across the width with a black lift under it.
        let (width, height) = (96u32, 64u32);
        let transfer: Vec<(u16, u8)> = (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    let a = 64 + (x as u16 * 192) / width as u16;
                    let b = ((y * 24) / height) as u8;
                    (a, b)
                })
            })
            .collect();
        let groups =
            super::super::pattern::sync_rects(width, height, 88, "DP-9", 42, 1_700_000_000_000);
        let rects: Vec<SyncRect> = groups
            .iter()
            .flat_map(|group| group.rects.iter().copied())
            .collect();
        let (count, items) = sync_shape_items(&groups);

        let mut painted = vec![0u8; width as usize * height as usize * 4];
        SyncPaint {
            transfer: &transfer,
            rects: &rects,
            width,
        }
        .rows(&mut painted, 0, height);

        for y in 0..height {
            for x in 0..width {
                let (a, b) = transfer[(y * width + x) as usize];
                // What `blend.frag` computes: white or black, then the same
                // fixed-point transfer.
                let input = if shader_walk(&items, count, x, y) {
                    255u32
                } else {
                    0
                };
                let expected = (((u32::from(a) * input) >> 8) + u32::from(b)).min(255) as u8;
                let offset = ((y * width + x) * 4) as usize;
                assert_eq!(
                    &painted[offset..offset + 4],
                    &[expected, expected, expected, 255],
                    "pixel ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn a_banded_rasterize_matches_a_single_pass_one() {
        // The CPU path splits the rows across scoped threads exactly as the
        // content blend does, so a rect that straddles a band boundary must
        // come out the same either way.
        let (width, height) = (64u32, 48u32);
        let transfer = vec![(256u16, 0u8); (width * height) as usize];
        let rects: Vec<SyncRect> =
            super::super::pattern::sync_rects(width, height, 7, "DP-1", 5, 1_700_000_000_000)
                .iter()
                .flat_map(|group| group.rects.iter().copied())
                .collect();
        let paint = SyncPaint {
            transfer: &transfer,
            rects: &rects,
            width,
        };
        let mut whole = vec![0u8; width as usize * height as usize * 4];
        paint.rows(&mut whole, 0, height);
        let mut banded = vec![0u8; whole.len()];
        let band = 7u32;
        let row_bytes = width as usize * 4;
        for (index, chunk) in banded.chunks_mut(band as usize * row_bytes).enumerate() {
            let first = index as u32 * band;
            paint.rows(chunk, first, band.min(height - first));
        }
        assert_eq!(whole, banded);
    }

    #[test]
    fn only_the_sync_pattern_is_animated() {
        assert!(animated(TestPattern::Sync));
        for pattern in [
            TestPattern::Grid,
            TestPattern::White,
            TestPattern::Black,
            TestPattern::Gamma,
            TestPattern::Identify,
            TestPattern::WarpAlignment,
        ] {
            assert!(!animated(pattern), "{pattern:?} must stay one-shot");
        }
    }
    #[test]
    fn slow_buffer_release_never_selects_an_owned_slot() {
        let mut busy = vec![true; GPU_PRESENT_SLOTS];
        for next in 0..12 {
            assert_eq!(free_present_slot(&busy, next), None);
        }
        busy[1] = false;
        assert_eq!(free_present_slot(&busy, 2), Some(1));
        busy[1] = true;
        busy[0] = false;
        assert_eq!(free_present_slot(&busy, 2), Some(0));
        assert_eq!(free_present_slot(&[], 0), None);
    }

    fn pattern_spec() -> SlicerSpec {
        serde_json::from_value(serde_json::json!({
            "source":"HEADLESS-1", "canvasWidth":320, "canvasHeight":240,
            "gamma":2.2, "blackLift":0.1, "renderer":"auto",
            "slices":[{"output":"DP-1","source":{"x":30,"y":20,"width":240,"height":180},"ramps":[]}]
        })).unwrap()
    }

    #[test]
    fn static_canvases_preserve_legacy_pattern_pixels_and_source_selection() {
        let spec = pattern_spec();
        let slice = &spec.slices[0];
        for pattern in [
            TestPattern::Grid,
            TestPattern::White,
            TestPattern::Black,
            TestPattern::Gamma,
            TestPattern::Identify,
        ] {
            let canvas = static_pattern_rgba(&spec, slice, pattern).unwrap();
            let legacy = super::super::pattern::render(
                240,
                180,
                &OverlaySpec {
                    output: slice.output.clone(),
                    gamma: spec.gamma,
                    black_lift: 0.0,
                    rect: slice.source,
                    source_rect: None,
                    pattern: Some(pattern),
                    canvas_size: None,
                },
            );
            for y in 0..180usize {
                for x in 0..240usize {
                    let dst = ((y + 20) * 320 + x + 30) * 4;
                    let src = (y * 240 + x) * 3;
                    assert_eq!(
                        &canvas[dst..dst + 3],
                        &legacy[src..src + 3],
                        "{pattern:?} at {x},{y}"
                    );
                    assert_eq!(canvas[dst + 3], 255);
                }
            }
            assert_eq!(&canvas[..4], &[0, 0, 0, 255]);
            let mut warped = slice.clone();
            warped.geometry = Some(super::super::warp::Geometry {
                corners: [[10.0, 5.0], [230.0, 0.0], [240.0, 175.0], [0.0, 180.0]],
                center: [0.4, 0.6],
            });
            assert_eq!(
                canvas,
                static_pattern_rgba(&spec, &warped, pattern).unwrap(),
                "pins must not move diagnostic source pixels"
            );
        }
    }

    #[test]
    fn static_white_covers_larger_fractional_sources_and_clips_negative_origins() {
        let spec = pattern_spec();
        let mut slice = spec.slices[0].clone();
        slice.source.x = 20;
        slice.source.y = 10;
        slice.source_rect = Some([20.25, 10.75, 290.5, 220.5]);
        let canvas = static_pattern_rgba(&spec, &slice, TestPattern::White).unwrap();
        let pixel = |bytes: &[u8], x: usize, y: usize| {
            let offset = (y * spec.canvas_width as usize + x) * 4;
            <[u8; 4]>::try_from(&bytes[offset..offset + 4]).unwrap()
        };
        // This lies beyond the old output-sized paint rectangle but inside
        // the configured source, and must not turn black when sampled.
        assert_eq!(pixel(&canvas, 300, 220), [255, 255, 255, 255]);
        assert_eq!(pixel(&canvas, 0, 0), [0, 0, 0, 255]);
        slice.source.x = -31;
        slice.source.y = -21;
        slice.source_rect = Some([-30.25, -20.75, 300.5, 220.5]);
        let clipped = static_pattern_rgba(&spec, &slice, TestPattern::White).unwrap();
        assert_eq!(pixel(&clipped, 0, 0), [255, 255, 255, 255]);
        assert_eq!(pixel(&clipped, 268, 198), [255, 255, 255, 255]);
        assert_eq!(pixel(&clipped, 319, 239), [0, 0, 0, 255]);
        let black = static_pattern_rgba(&spec, &slice, TestPattern::Black).unwrap();
        assert!(black.chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
    }

    #[test]
    fn static_diagnostics_scale_into_fractional_sources_without_following_pins() {
        let mut spec = pattern_spec();
        spec.canvas_width = 640;
        spec.canvas_height = 480;
        let mut slice = spec.slices[0].clone();
        slice.source.x = 0;
        slice.source.y = 0;
        slice.source_rect = Some([0.5, 0.5, 480.0, 360.0]);
        for pattern in [TestPattern::Grid, TestPattern::Gamma, TestPattern::Identify] {
            let canvas = static_pattern_rgba(&spec, &slice, pattern).unwrap();
            // Built with the same footprint `static_pattern_rgba` resolves
            // internally (`slice.source_rect`), not the raster `rect` at
            // 1:1: a canvas-anchored feature (the grid's tile lines) is now
            // computed from the true canvas footprint, so a comparison
            // picture that omitted it would show tiles at the pre-fix,
            // wrong canvas position and this coincidence would no longer
            // hold. Gamma and Identify's own drawing does not depend on the
            // footprint, so this is a no-op for them.
            let picture = super::super::pattern::render(
                240,
                180,
                &OverlaySpec {
                    output: slice.output.clone(),
                    gamma: spec.gamma,
                    black_lift: 0.0,
                    rect: slice.source,
                    source_rect: slice.source_rect,
                    pattern: Some(pattern),
                    canvas_size: None,
                },
            );
            // At 2x density with a half-pixel source origin, these canvas
            // centers coincide exactly with diagnostic picture centers.
            for y in (0..180usize).step_by(13) {
                for x in (0..239usize).step_by(17) {
                    let src = (y * 240 + x) * 3;
                    let dst = ((2 * y + 1) * 640 + 2 * x + 1) * 4;
                    assert_eq!(
                        &canvas[dst..dst + 3],
                        &picture[src..src + 3],
                        "{pattern:?} {x},{y}"
                    );
                    for c in 0..3 {
                        let midpoint = (u16::from(picture[src + c])
                            + u16::from(picture[src + 3 + c]))
                        .div_ceil(2) as u8;
                        // +/-1: `source[3] = 360.0` over `height = 180` does
                        // not divide back to an exactly representable f64,
                        // so the resample's `py` lands a sub-ULP epsilon off
                        // 104.0 rather than exactly on it, pulling in an
                        // infinitesimal, content-dependent contribution from
                        // the next texel row. This one-row check only
                        // predicts the pure-1D average, so a tie can round
                        // either way; it is pre-existing floating-point
                        // fragility in the resample this criterion-1 rework
                        // does not touch, made visible now only because the
                        // grid's corrected canvas-anchored content differs
                        // pixel-for-pixel from before at this sample point.
                        let actual = canvas[dst + 4 + c];
                        assert!(
                            actual.abs_diff(midpoint) <= 1,
                            "{pattern:?} midpoint {x},{y}: {actual} vs {midpoint}"
                        );
                    }
                }
            }
            let mut pinned = slice.clone();
            pinned.geometry = Some(super::super::warp::Geometry {
                corners: [[12.0, 8.0], [240.0, 0.0], [230.0, 170.0], [0.0, 180.0]],
                center: [0.3, 0.7],
            });
            assert_eq!(
                canvas,
                static_pattern_rgba(&spec, &pinned, pattern).unwrap()
            );
        }
    }

    #[test]
    fn static_source_support_prevents_fractional_white_fringes_and_preserves_exact_bytes() {
        let spec = pattern_spec();
        let mut slice = spec.slices[0].clone();
        for pattern in [
            TestPattern::White,
            TestPattern::Grid,
            TestPattern::Gamma,
            TestPattern::Identify,
        ] {
            let legacy = static_pattern_rgba(&spec, &slice, pattern).unwrap();
            slice.source_rect = Some([30.0, 20.0, 240.0, 180.0]);
            assert_eq!(legacy, static_pattern_rgba(&spec, &slice, pattern).unwrap());
            slice.source_rect = None;
        }
        slice.source.x = 30;
        slice.source.y = 20;
        slice.source_rect = Some([30.9, 20.9, 0.25, 0.25]);
        let canvas = static_pattern_rgba(&spec, &slice, TestPattern::White).unwrap();
        // The midpoint (31.025,21.025) uses all four of these support texels.
        for y in 20..=21usize {
            for x in 30..=31usize {
                let offset = (y * 320 + x) * 4;
                assert_eq!(&canvas[offset..offset + 4], &[255, 255, 255, 255]);
            }
        }
    }

    /// The September 21 review bug, directly: `warp_alignment`'s percentage
    /// lines and `grid`'s tile fill must classify the very same canvas
    /// position the very same way regardless of a slice's raster density
    /// relative to its canvas footprint (what "Content scale" changes), and
    /// regardless of the footprint's origin being fractional. Before this
    /// fix, `static_pattern_rgba` rendered the picture as if raster pixel x
    /// were canvas pixel `slice.source.x + x` at 1:1, so only the 1x case
    /// below would have landed correctly.
    #[test]
    fn warp_alignment_and_grid_canvas_features_hold_position_across_slice_scale() {
        let mut spec = pattern_spec();
        spec.canvas_width = 1000;
        spec.canvas_height = 1000;
        // A fractional origin: exactly what `canonical_source`'s exact-
        // integer fast path does not cover, and every Content scale other
        // than one that happens to divide the canvas evenly produces.
        let footprint: [f64; 4] = [120.25, 80.75, 700.0, 700.0];
        let pixel = |canvas: &[u8], x: usize, y: usize| -> [u8; 4] {
            let offset = (y * 1000 + x) * 4;
            <[u8; 4]>::try_from(&canvas[offset..offset + 4]).unwrap()
        };
        let mut grid_samples = Vec::new();
        for raster in [350i32, 700, 1400] {
            // raster/footprint = 0.5x, 1x, 2x.
            let mut slice = spec.slices[0].clone();
            slice.source = crate::model::Rect {
                x: footprint[0].floor() as i32,
                y: footprint[1].floor() as i32,
                width: raster,
                height: raster,
            };
            slice.source_rect = Some(footprint);

            let alignment = static_pattern_rgba(&spec, &slice, TestPattern::WarpAlignment)
                .unwrap_or_else(|e| panic!("raster {raster}: {e}"));
            // The 50% line of a 1000-wide canvas sits at columns 499/500.
            // y=650 is inside the footprint and clear of every 10% line and
            // every alignment circle (the nearest, the 350px-radius
            // inscribed circle, is 100px away there).
            let on_line = (495..=505).any(|x| pixel(&alignment, x, 650)[0] > 200);
            assert!(on_line, "raster {raster}: no lit pixel near canvas x=500");
            // x=350 (not a multiple of 100, so not on any 10% line) and
            // 212px from the circles' shared center, clear of all three.
            let off = pixel(&alignment, 350, 650);
            assert!(
                off[0] < 100,
                "raster {raster}: canvas (350,650) should stay background, got {off:?}"
            );

            let grid = static_pattern_rgba(&spec, &slice, TestPattern::Grid).unwrap();
            // (220,260): 20,60 into the tile at (200,200) — clear of that
            // tile's diagonal cross and corner triangle, and (with a raster
            // pixel spanning at most 2 canvas px here) clear of its edges.
            grid_samples.push(pixel(&grid, 220, 260));
        }
        assert!(
            grid_samples.windows(2).all(|w| w[0] == w[1]),
            "the same canvas tile position must classify identically at every scale: {grid_samples:?}"
        );
    }

    /// Two outputs whose canvas footprints overlap — a real seam — must
    /// agree, pixel for pixel, on every canvas-anchored grid feature inside
    /// that overlap, even when the two slices have different raster
    /// densities (as two projectors of different native resolution sharing
    /// a wall commonly would). This is the same guarantee two aligned
    /// projectors rely on to superimpose the pattern physically.
    #[test]
    fn two_overlapping_slices_agree_on_grid_features_inside_the_overlap() {
        let mut spec = pattern_spec();
        spec.canvas_width = 1000;
        spec.canvas_height = 1000;
        let mut left = spec.slices[0].clone();
        left.output = "LEFT".into();
        left.source = crate::model::Rect {
            x: 100,
            y: 100,
            width: 300,
            height: 300,
        };
        left.source_rect = Some([100.0, 100.0, 600.0, 600.0]);
        let mut right = spec.slices[0].clone();
        right.output = "RIGHT".into();
        right.source = crate::model::Rect {
            x: 400,
            y: 100,
            width: 1200,
            height: 1200,
        };
        right.source_rect = Some([400.0, 100.0, 600.0, 600.0]);

        let canvas_left = static_pattern_rgba(&spec, &left, TestPattern::Grid).unwrap();
        let canvas_right = static_pattern_rgba(&spec, &right, TestPattern::Grid).unwrap();
        // (570,320): inside both footprints ([100,700] and [400,1000] on x,
        // both [100,700] on y) and 70,20 into its tile — clear of the
        // diagonal cross and corner triangle.
        let offset = (320 * 1000 + 570) * 4;
        assert_eq!(
            &canvas_left[offset..offset + 4],
            &canvas_right[offset..offset + 4],
            "two overlapping slices at different raster densities disagreed \
             on a canvas-anchored grid feature inside their overlap"
        );
    }

    /// Criterion 2: the CPU (no-GPU) `present_pattern` path, for a slice
    /// whose raster is not 1:1 with its canvas footprint, must produce
    /// exactly what sampling the canvas-space picture through the content
    /// CPU blend (`Blend::sample`) would — not a 1:1 local copy, which is
    /// what this function used to do and which only agreed with the content
    /// path at Content scale 100%.
    #[test]
    fn cpu_present_pattern_matches_sampling_the_canvas_picture_through_the_content_blend() {
        let (socket, _server) = std::os::unix::net::UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let backend = connection.backend().downgrade();

        let mut spec = pattern_spec();
        spec.canvas_width = 320;
        spec.canvas_height = 240;
        // Raster at 2x the canvas footprint's density.
        spec.slices[0].source = crate::model::Rect {
            x: 10,
            y: 10,
            width: 480,
            height: 360,
        };
        spec.slices[0].source_rect = Some([10.0, 10.0, 240.0, 180.0]);
        let slice = spec.slices[0].clone();

        let transfer = vec![(200u16, 3u8); 480 * 360];
        let mut state = bare_state(vec![]);
        state.presenters = vec![Presenter {
            surface: WlSurface::inert(backend.clone()),
            layer_surface: ZwlrLayerSurfaceV1::inert(backend.clone()),
            configured: Some((480, 360)),
            opaque_for: None,
            output_mode: None,
            name: slice.output.clone(),
            buffers: vec![(
                WlBuffer::inert(backend.clone()),
                memmap2::MmapMut::map_anon(480 * 360 * 4).unwrap(),
            )],
            gpu_buffers: vec![],
            busy: vec![false],
            next_buffer: 0,
            transfer: transfer.clone(),
            dynamic_table: None,
            warp: None,
            sample: None,
            sample_revision: None,
            warp_revision: 0,
            warp_reported_revision: None,
            warp_submitted_revision: None,
            source: slice.source,
            frame_pending: false,
            pending_since: None,
            feedback_pending_for: None,
            feedback_since: None,
            stalled: false,
            stale: true,
        }];

        present_pattern(&mut state, &spec, TestPattern::Grid).unwrap();
        let (_, map) = &state.presenters[0].buffers[0];
        let actual = map[..480 * 360 * 4].to_vec();

        // Independently reproduce the content CPU blend's sampling of this
        // slice's canvas-space picture.
        let canvas = static_pattern_rgba(&spec, &slice, TestPattern::Grid).unwrap();
        let warp = super::super::warp::Warp::identity(480, 360)
            .with_source_rect([10.0, 10.0, 240.0, 180.0])
            .unwrap();
        let sample = AxisSamples::build(&warp, [10.0, 10.0]);
        let expected_blend = Blend {
            canvas: &canvas,
            transfer: &transfer,
            format: PixelFormat {
                bytes: 4,
                red: 0,
                green: 1,
                blue: 2,
            },
            stride: 320 * 4,
            y_invert: false,
            usable_width: 320,
            usable_height: 240,
            source_x: 10,
            source_y: 10,
            width: 480,
            sample: Some(&sample),
        };
        let mut expected = vec![0u8; 480 * 360 * 4];
        expected_blend.rows(&mut expected, 0, 360);
        assert_eq!(actual, expected);

        // And this must actually differ from the old 1:1 local copy: proof
        // the fix changes behavior for a scaled slice, not just that the
        // new implementation is internally consistent with itself.
        let legacy_picture = super::super::pattern::render(
            480,
            360,
            &OverlaySpec {
                output: slice.output.clone(),
                gamma: spec.gamma,
                black_lift: 0.0,
                rect: slice.source,
                source_rect: None,
                pattern: Some(TestPattern::Grid),
                canvas_size: Some([320, 240]),
            },
        );
        let mut legacy = vec![0u8; 480 * 360 * 4];
        for y in 0..360usize {
            for x in 0..480usize {
                let (a, b) = transfer[y * 480 + x];
                let src = &legacy_picture[(y * 480 + x) * 3..(y * 480 + x) * 3 + 3];
                let out = |v: u8| (((a as u32 * v as u32) >> 8) + b as u32).min(255) as u8;
                let dst = &mut legacy[(y * 480 + x) * 4..(y * 480 + x) * 4 + 4];
                dst[0] = out(src[2]);
                dst[1] = out(src[1]);
                dst[2] = out(src[0]);
                dst[3] = 255;
            }
        }
        assert_ne!(
            actual, legacy,
            "the fix must actually change output for a scaled slice"
        );
    }

    #[test]
    fn cpu_rejects_warp_before_connecting_and_preserves_identity_support() {
        let mut spec = pattern_spec();
        spec.renderer = Renderer::Cpu;
        assert!(!requested_warp(&spec).unwrap());
        spec.slices[0].geometry = Some(super::super::warp::Geometry {
            corners: [[10.0, 5.0], [230.0, 0.0], [240.0, 175.0], [0.0, 180.0]],
            center: [0.5, 0.5],
        });
        assert!(run(&spec)
            .unwrap_err()
            .to_string()
            .contains("warp_unavailable"));
    }
    fn lifecycle_state() -> (Connection, std::os::unix::net::UnixStream, State) {
        let (socket, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let backend = connection.backend().downgrade();
        let mut state = bare_state(vec![]);
        state.presenters = (0..2)
            .map(|i| Presenter {
                surface: WlSurface::inert(backend.clone()),
                layer_surface: ZwlrLayerSurfaceV1::inert(backend.clone()),
                configured: Some((4, 4)),
                opaque_for: None,
                output_mode: None,
                name: format!("OUT-{i}"),
                buffers: (0..2)
                    .map(|_| {
                        (
                            WlBuffer::inert(backend.clone()),
                            memmap2::MmapMut::map_anon(64).unwrap(),
                        )
                    })
                    .collect(),
                gpu_buffers: vec![],
                busy: vec![false; 2],
                next_buffer: 0,
                transfer: vec![(256, 0); 16],
                dynamic_table: None,
                warp: None,
                sample: None,
                sample_revision: None,
                warp_revision: 7,
                warp_reported_revision: None,
                warp_submitted_revision: None,
                source: crate::model::Rect {
                    x: i * 4,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                frame_pending: false,
                pending_since: None,
                feedback_pending_for: None,
                feedback_since: None,
                stalled: false,
                stale: true,
            })
            .collect();
        (connection, server, state)
    }

    #[test]
    fn slow_buffers_hold_locked_generation_and_free_run_keeps_other_output_moving() {
        let (_connection, _server, mut state) = lifecycle_state();
        state.presenters[0].busy.fill(true);
        assert!(!state.can_present());
        assert!(sync_due(&mut state).is_empty());
        assert!(state
            .presenters
            .iter()
            .all(|p| p.stale && p.warp_revision == 7));
        state.free_run = true;
        assert!(state.can_present());
        let due = sync_due(&mut state);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].index, 1);
        assert!(state.presenters[0].stale);
        state.presenters[0].warp_revision = 9;
        state.presenters[0].busy[1] = false;
        let due = sync_due(&mut state);
        assert_eq!((due[0].index, due[0].slot), (0, 1));
        assert_eq!(state.presenters[0].warp_revision, 9);
        assert!(!state.presenters[0].stale);
    }

    #[test]
    fn installed_generation_survives_until_first_complete_capture() {
        let (connection, _server, mut state) = lifecycle_state();
        state.capture.backend = Some(Backend::Gpu);
        let queue = connection.new_event_queue::<State>();
        present_frame_gpu(&mut state, &queue.handle());
        assert!(state
            .presenters
            .iter()
            .all(|p| !p.stale && p.warp_revision == 7));
        assert!(state
            .presenters
            .iter()
            .all(|p| p.warp_submitted_revision.is_none()));
        assert!(!state.can_present());
        state.timing = Timing::new(2);
        state.new_snapshot();
        assert!(state.can_present());
        assert!(state
            .presenters
            .iter()
            .all(|p| p.stale && p.warp_revision == 7));
    }
}

/// End-to-end reconstruction: the slices the CPU pipeline actually renders,
/// placed back on the canvas at their offsets and summed in linear light,
/// must reproduce the original canvas picture. Built through the real
/// pipeline entry points, not a re-implementation of the blend math:
/// `layout::canvas_plan_with_correction` (the plan the daemon builds),
/// `warp_update::fill_rows` (the transfer table the slicer builds at
/// startup), and `Blend::rows` (the CPU content path).
///
/// Why summing shaded 8-bit values as `(v/255)^gamma` reconstructs the
/// original: `Evaluator::transfer` (layout.rs) returns a gain
/// `round(weight^(1/gamma) * 256)` applied to the raw sampled byte in the
/// *gamma-encoded* domain (`Blend::shade`: `(gain * value) >> 8`). Since
/// `weight` is the linear-light seam share and encoded values are
/// (approximately) `linear^(1/gamma)`, shading in the encoded domain by
/// `weight^(1/gamma)` is exactly weighting the corresponding linear-light
/// value by `weight`. Every source's `weight` sums to 1 across the sources
/// covering a point (see the `Evaluator` doc comment in layout.rs), so
/// summing the shaded slices' `(v/255)^gamma` back together reconstructs
/// `(original/255)^gamma`, up to the gain's 1/256 quantization and
/// `Blend::shade`'s integer-floor shading.
///
/// That sum-to-one composite is necessary but not sufficient: the current
/// (pre-fix) `Evaluator` rule always makes covering weights sum to one, so
/// it cannot see a defect that is a wrong SPLIT between two slices showing
/// the same content rather than a wrong total. `reconstruction::share_at`
/// and the `row_independence_*`/`grid_share_separability` tests below
/// isolate one slice's own share of a pixel (not the composite) and check
/// it against what the geometry says it should be, independent of the
/// other covering slices' particular values.
///
/// See `.claude/plans/warp-fixes.md`, "Round 2 ... Slice SEAM": layout `d`
/// (`brain_scaled_slices(0.07)`, brain's real 0.07-canvas-pixel row sliver,
/// as opposed to `c`'s exact touch) reproduces the wall geometry that
/// produced a wedge-shaped error at the row boundary under the old
/// minimum-distance seam rule. `row_independence_brain_scaled_rows_overlap_sliver`
/// is that slice's acceptance test: it currently FAILS (left failing, not
/// `#[ignore]`d, per instructions), and its measured per-row shares are the
/// evidence of the wedge.
#[cfg(test)]
mod reconstruction {
    use super::super::{layout, warp_update};
    use super::{Blend, PixelFormat};

    const CANVAS_WIDTH: i32 = 400;
    const CANVAS_HEIGHT: i32 = 260;
    const GAMMA: f64 = 2.2;
    /// Tolerance for the per-slice SHARE assertions below: 2 parts in 256,
    /// matching the gain table's own quantization
    /// (`Evaluator::transfer`'s `* 256.0`) rather than the coarser 8-bit
    /// (`/255`) rounding the composite assertions use — a share is computed
    /// directly from one rendered slice's byte, without the extra summation
    /// step that the composite assertions' wider 3-code-value tolerance
    /// accounts for.
    const SHARE_TOLERANCE: f64 = 2.0 / 256.0;
    /// The separability check multiplies two measured shares and compares
    /// with a third. Each share comes from one floored 8-bit byte
    /// (`Blend::shade` floors `(gain * value) >> 8`) of a gain that is itself
    /// rounded to 1/256, so each carries up to about one code value of error;
    /// at gamma 2.2 on a byte around 100 that is a little over 2% of the
    /// share, and three of them compound to about 6/256. The weights
    /// themselves are proven to factor exactly in
    /// `layout::tests::a_grid_gives_the_product_of_two_one_dimensional_ramps_everywhere`;
    /// this bound only has to be tight enough to catch a rule that does not
    /// factor, which the minimum-distance rule missed by 0.13.
    const SEPARABILITY_TOLERANCE: f64 = 6.0 / 256.0;

    /// One output's placement: a canvas-space source rectangle (fractional
    /// canvas pixels, as `geometry.source` stores it) and an independent
    /// destination raster size (`mode.width`/`mode.height`, whatever the
    /// output's own resolution is). The two are equal, integer values for
    /// every slice except `brain_scaled_slices`' bottom row when it carries
    /// a sub-pixel sliver: the destination is still an integer raster (it
    /// has to be), but the source that raster is placed against in the
    /// `Evaluator`'s canvas-space geometry can be fractional.
    #[derive(Clone, Copy)]
    struct SliceGeom {
        source_x: f64,
        source_y: f64,
        source_width: f64,
        source_height: f64,
        mode_width: i32,
        mode_height: i32,
    }

    impl SliceGeom {
        /// Integer placement at 100% content scale: raster and canvas-space
        /// source rectangle are pixel-identical, so sampling is exact (no
        /// warp, no bilinear interpolation).
        fn exact(x: i32, y: i32, width: i32, height: i32) -> Self {
            Self {
                source_x: f64::from(x),
                source_y: f64::from(y),
                source_width: f64::from(width),
                source_height: f64::from(height),
                mode_width: width,
                mode_height: height,
            }
        }
    }

    /// A smooth diagonal gradient, two hard edges (a vertical step at 1/3
    /// width, a horizontal step at 2/3 height), and a flat mid-gray field in
    /// the canvas center. Values span the full 0..255 range, including near
    /// both ends within every overlap band exercised below, so the
    /// blend-off "doubled overlap" assertion is never vacuous.
    fn content_value(x: i32, y: i32) -> u8 {
        let gx = f64::from(x) / f64::from(CANVAS_WIDTH - 1);
        let gy = f64::from(y) / f64::from(CANVAS_HEIGHT - 1);
        let mut v = 0.5 * (gx + gy) * 255.0;
        if x >= CANVAS_WIDTH / 3 {
            v += 60.0;
        }
        if y >= 2 * CANVAS_HEIGHT / 3 {
            v -= 40.0;
        }
        v = v.clamp(0.0, 255.0);
        let (cx0, cx1) = (CANVAS_WIDTH * 3 / 8, CANVAS_WIDTH * 5 / 8);
        let (cy0, cy1) = (CANVAS_HEIGHT * 3 / 8, CANVAS_HEIGHT * 5 / 8);
        if x >= cx0 && x < cx1 && y >= cy0 && y < cy1 {
            v = 128.0;
        }
        v.round() as u8
    }

    /// The canvas picture, packed BGRA to match `PixelFormat` below.
    /// R = G = B at every pixel, so one channel's arithmetic proves all
    /// three; a captured canvas is never anything but 8-bit RGB(A) here.
    fn build_canvas() -> Vec<u8> {
        build_canvas_with(content_value)
    }

    /// A flat field for the share checks. A share is read back out of 8-bit
    /// slice bytes, and `Blend::shade` floors `(gain * value) >> 8`, so with
    /// content that changes from row to row the floor lands differently on
    /// neighboring rows and reads as a share difference of about 1.5/256 that
    /// has nothing to do with geometry. On a flat field the only noise left is
    /// the quantization of the gain itself and of the one output byte.
    fn build_flat_canvas() -> Vec<u8> {
        build_canvas_with(|_, _| 200)
    }

    fn build_canvas_with(value: impl Fn(i32, i32) -> u8) -> Vec<u8> {
        let stride = CANVAS_WIDTH as usize * 4;
        let mut canvas = vec![0u8; stride * CANVAS_HEIGHT as usize];
        for y in 0..CANVAS_HEIGHT {
            for x in 0..CANVAS_WIDTH {
                let v = value(x, y);
                let offset = y as usize * stride + x as usize * 4;
                canvas[offset] = v; // blue
                canvas[offset + 1] = v; // green
                canvas[offset + 2] = v; // red
                canvas[offset + 3] = 255;
            }
        }
        canvas
    }

    fn canvas_config() -> crate::model::CanvasConfig {
        crate::model::CanvasConfig {
            aspect: f64::from(CANVAS_WIDTH) / f64::from(CANVAS_HEIGHT),
            render_width: CANVAS_WIDTH as u32,
        }
    }

    /// A shared-canvas output: identity corners, center `[0.5, 0.5]`,
    /// `source == raster_footprint`.
    fn output_config(name: &str, geom: SliceGeom) -> crate::model::OutputConfig {
        let mut config = crate::model::OutputConfig::new(crate::model::OutputMatch::by_name(name));
        config.mode = Some(crate::model::Mode {
            width: geom.mode_width,
            height: geom.mode_height,
            refresh_hz: 60.0,
        });
        let source = crate::model::CanvasRect {
            x: geom.source_x / f64::from(CANVAS_WIDTH),
            y: geom.source_y / f64::from(CANVAS_WIDTH),
            width: geom.source_width / f64::from(CANVAS_WIDTH),
            height: geom.source_height / f64::from(CANVAS_WIDTH),
        };
        config.geometry = Some(crate::model::OutputGeometry {
            source,
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: source,
        });
        config
    }

    /// The attached display `canvas_plan_with_correction` needs to actually
    /// emit a slice for this output (an output absent from `observed` only
    /// shapes the layout, contributing no slice of its own).
    fn observed_output(name: &str, geom: SliceGeom) -> crate::model::Output {
        let mode = crate::model::Mode {
            width: geom.mode_width,
            height: geom.mode_height,
            refresh_hz: 60.0,
        };
        crate::model::Output {
            name: name.to_string(),
            active: true,
            make: None,
            model: None,
            serial: None,
            current_mode: Some(mode),
            modes: vec![mode],
            rect: crate::model::Rect {
                x: 0,
                y: 0,
                width: geom.mode_width,
                height: geom.mode_height,
            },
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }
    }

    fn desired_state(slices: &[(&str, SliceGeom)], blend: bool) -> crate::model::DesiredState {
        let mut desired = crate::model::DesiredState::new();
        desired.projection = Some(crate::model::ProjectionConfig {
            mode: crate::model::ProjectionMode::Simple,
            canvas: Some(canvas_config()),
            blend,
            ..Default::default()
        });
        for (name, geom) in slices {
            desired.outputs.push(output_config(name, *geom));
        }
        desired
    }

    /// Builds the `SlicerSpec` the reconciler hands the slicer, through the
    /// real `canvas_plan_with_correction` entry point. `apply_correction:
    /// false` so no warp stage is involved.
    fn slicer_spec(slices: &[(&str, SliceGeom)], blend: bool) -> super::SlicerSpec {
        let desired = desired_state(slices, blend);
        let observed: Vec<crate::model::Output> = slices
            .iter()
            .map(|(name, geom)| observed_output(name, *geom))
            .collect();
        let plan = layout::canvas_plan_with_correction(&desired, &observed, false)
            .expect("a shared canvas with identity corners and a valid layout plans");
        let projection = desired.projection.as_ref().unwrap();
        super::SlicerSpec {
            control_session: String::new(),
            source: "canvas".to_string(),
            canvas_width: plan.canvas_width,
            canvas_height: plan.canvas_height,
            gamma: projection.gamma,
            black_lift: projection.black_lift.level(),
            adaptive_lift: None,
            pattern: None,
            free_run: false,
            renderer: crate::model::Renderer::Cpu,
            layout: plan.layout,
            coverage_rects: plan.coverage_rects,
            slices: plan.slices,
        }
    }

    // ---- shared layout geometry --------------------------------------

    fn two_strip_slices() -> Vec<(&'static str, SliceGeom)> {
        let overlap = (f64::from(CANVAS_HEIGHT) * 0.2).round() as i32; // 52
        let strip_height = (CANVAS_HEIGHT + overlap) / 2; // 156
        let y2 = CANVAS_HEIGHT - strip_height; // 104
        vec![
            ("TOP", SliceGeom::exact(0, 0, CANVAS_WIDTH, strip_height)),
            (
                "BOTTOM",
                SliceGeom::exact(0, y2, CANVAS_WIDTH, strip_height),
            ),
        ]
    }

    /// `(slices, x2, col_width, y2, strip_height)`: the last four values are
    /// the column/row overlap-band bounds, returned so callers (the sum
    /// test and the separability test) never recompute them independently
    /// and risk disagreeing.
    fn grid_slices() -> (Vec<(&'static str, SliceGeom)>, i32, i32, i32, i32) {
        let overlap_y = (f64::from(CANVAS_HEIGHT) * 0.2).round() as i32; // 52
        let strip_height = (CANVAS_HEIGHT + overlap_y) / 2; // 156
        let y2 = CANVAS_HEIGHT - strip_height; // 104
        let overlap_x = (f64::from(CANVAS_WIDTH) * 0.2).round() as i32; // 80
        let col_width = (CANVAS_WIDTH + overlap_x) / 2; // 240
        let x2 = CANVAS_WIDTH - col_width; // 160
        let slices = vec![
            ("TL", SliceGeom::exact(0, 0, col_width, strip_height)),
            ("TR", SliceGeom::exact(x2, 0, col_width, strip_height)),
            ("BL", SliceGeom::exact(0, y2, col_width, strip_height)),
            ("BR", SliceGeom::exact(x2, y2, col_width, strip_height)),
        ];
        (slices, x2, col_width, y2, strip_height)
    }

    /// brain's real wall geometry, scaled to this 400x260 canvas: two
    /// columns overlapping 5.75% of the canvas width (nearest integer pixel
    /// to brain's measured 5.8%), two rows separated by `row_sliver` canvas
    /// pixels of overlap — `0.0` is an exact touch (`c`, as before),
    /// `0.07` is brain's real sub-pixel sliver (`d`), the value that
    /// produces the wedge. Returns `(slices, row_boundary)`.
    ///
    /// The destination (raster) height of the bottom row is derived from
    /// its floored source origin so it always reaches the canvas edge
    /// (`CANVAS_HEIGHT - floor(row_height - row_sliver)`): the raster is an
    /// integer buffer regardless of the sliver, but the canvas-space source
    /// rectangle the `Evaluator` sees for it is not, which is exactly what
    /// lets a sub-pixel sliver exist in the first place.
    fn brain_scaled_slices(row_sliver: f64) -> (Vec<(&'static str, SliceGeom)>, i32) {
        let col1_width = 212;
        let overlap_x = 23;
        let col2_x = col1_width - overlap_x; // 189
        let col2_width = CANVAS_WIDTH - col2_x; // 211
        let row_height = CANVAS_HEIGHT / 2; // 130

        let row2_source_y = f64::from(row_height) - row_sliver;
        let row2_dest_y = row2_source_y.floor() as i32;
        let row2_dest_height = CANVAS_HEIGHT - row2_dest_y;
        let row2_source_height = f64::from(CANVAS_HEIGHT) - row2_source_y;

        let row2_geom = |x: i32, width: i32| SliceGeom {
            source_x: f64::from(x),
            source_y: row2_source_y,
            source_width: f64::from(width),
            source_height: row2_source_height,
            mode_width: width,
            mode_height: row2_dest_height,
        };

        let slices = vec![
            ("C1R1", SliceGeom::exact(0, 0, col1_width, row_height)),
            ("C2R1", SliceGeom::exact(col2_x, 0, col2_width, row_height)),
            ("C1R2", row2_geom(0, col1_width)),
            ("C2R2", row2_geom(col2_x, col2_width)),
        ];
        (slices, row_height)
    }

    // ---- rendering the real CPU content path --------------------------

    /// One slice's rendered raster (BGRA, matching `PixelFormat` below) and
    /// where it sits on the canvas.
    struct RenderedSlice {
        output: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        /// The canvas the slice was rendered from, so a share is measured
        /// against what the pipeline actually saw, whatever content a test
        /// chose.
        canvas: Vec<u8>,
    }

    impl RenderedSlice {
        fn original_at(&self, x: i32, y: i32) -> u8 {
            if x < 0 || y < 0 || x >= CANVAS_WIDTH || y >= CANVAS_HEIGHT {
                return 0;
            }
            self.canvas[(y as usize * CANVAS_WIDTH as usize + x as usize) * 4 + 1]
        }
    }

    impl RenderedSlice {
        /// The shaded red-channel byte (R = G = B in this canvas) at canvas
        /// pixel `(cx, cy)`, or `None` if this slice's raster does not
        /// cover that pixel.
        fn value_at(&self, cx: i32, cy: i32) -> Option<u8> {
            let lx = cx - self.x;
            let ly = cy - self.y;
            if lx < 0 || ly < 0 || lx as u32 >= self.width || ly as u32 >= self.height {
                return None;
            }
            let offset = (ly as u32 * self.width + lx as u32) as usize * 4;
            Some(self.pixels[offset + 2])
        }
    }

    /// Renders every slice `spec` describes with the real CPU content path:
    /// `warp_update::fill_rows` builds the transfer table exactly as the
    /// slicer's startup bootstrap does, and `Blend::rows` is the same
    /// per-pixel shading `present_frame_cpu` calls.
    fn render_slices(spec: &super::SlicerSpec, canvas: &[u8]) -> Vec<RenderedSlice> {
        let coverage = super::Coverage::new(warp_update::coverage_rects(spec));
        let evaluator = spec
            .layout
            .as_ref()
            .map(|l| layout::Evaluator::new(l, spec.canvas_width, spec.canvas_height).unwrap());

        spec.slices
            .iter()
            .map(|slice| {
                let width = slice.source.width as u32;
                let height = slice.source.height as u32;
                let layout_index = evaluator
                    .as_ref()
                    .map(|e| (e, e.index(&slice.output).unwrap()));

                let mut transfer = vec![(0u16, 0u8); (width * height) as usize];
                warp_update::fill_rows(
                    spec,
                    slice,
                    None,
                    &coverage,
                    layout_index,
                    (width, height),
                    0,
                    &mut transfer,
                );

                let blend = Blend {
                    canvas,
                    transfer: &transfer,
                    format: PixelFormat {
                        bytes: 4,
                        red: 2,
                        green: 1,
                        blue: 0,
                    },
                    stride: spec.canvas_width as u32 * 4,
                    y_invert: false,
                    usable_width: spec.canvas_width as u32,
                    usable_height: spec.canvas_height as u32,
                    source_x: slice.source.x.max(0) as u32,
                    source_y: slice.source.y.max(0) as u32,
                    width,
                    sample: None,
                };
                let mut output = vec![0u8; (width * height * 4) as usize];
                blend.rows(&mut output, 0, height);

                RenderedSlice {
                    output: slice.output.clone(),
                    x: slice.source.x,
                    y: slice.source.y,
                    width,
                    height,
                    pixels: output,
                    canvas: canvas.to_vec(),
                }
            })
            .collect()
    }

    /// `(slice_v/255)^gamma / (original/255)^gamma` at canvas pixel
    /// `(x, y)` for one rendered slice — the linear-light fraction of the
    /// original that this slice alone is showing there. `None` when the
    /// original is too dark to keep quantization noise small (below 64),
    /// or when the slice's shaded byte is exactly 0. An exact 0 is the
    /// `fill_rows` "not covered" sentinel (`(0, 0)` in the transfer table,
    /// written as black by `Blend::shade`), not a very small real share —
    /// it shows up at a slice's own raster edge when that edge's floored
    /// integer origin extends slightly beyond its true (possibly
    /// fractional) canvas-space source rectangle, which is not part of the
    /// geometry this function is measuring.
    fn share_at(rs: &RenderedSlice, x: i32, y: i32, gamma: f64) -> Option<f64> {
        let original = rs.original_at(x, y);
        if original < 64 {
            return None;
        }
        let shaded = rs.value_at(x, y)?;
        if shaded == 0 {
            return None;
        }
        let original_linear = (f64::from(original) / 255.0).powf(gamma);
        let shaded_linear = (f64::from(shaded) / 255.0).powf(gamma);
        Some(shaded_linear / original_linear)
    }

    // ---- assertion set 1: sum-to-one composite -------------------------

    struct Reconstruction {
        max_error: f64,
        mean_error: f64,
        max_error_near_row_boundary: Option<f64>,
        covered_everywhere: bool,
    }

    /// Places every rendered slice back on the canvas at its offset and
    /// sums in linear light. `row_boundary`, when given, is a canvas row
    /// index to track a separate max error within 30 rows of.
    fn reconstruct(
        spec: &super::SlicerSpec,
        canvas: &[u8],
        row_boundary: Option<i32>,
    ) -> Reconstruction {
        let rendered = render_slices(spec, canvas);

        let mut composite = vec![0.0f64; (spec.canvas_width * spec.canvas_height) as usize];
        let mut coverage_count = vec![0u32; composite.len()];

        for rs in &rendered {
            for ly in 0..rs.height {
                for lx in 0..rs.width {
                    let cx = rs.x + lx as i32;
                    let cy = rs.y + ly as i32;
                    if cx < 0 || cy < 0 || cx >= spec.canvas_width || cy >= spec.canvas_height {
                        continue;
                    }
                    let Some(shaded) = rs.value_at(cx, cy) else {
                        continue;
                    };
                    let idx = (cy * spec.canvas_width + cx) as usize;
                    composite[idx] += (f64::from(shaded) / 255.0).powf(GAMMA);
                    coverage_count[idx] += 1;
                }
            }
        }

        let mut max_error = 0.0f64;
        let mut max_error_near_boundary = row_boundary.map(|_| 0.0f64);
        let mut sum_error = 0.0f64;
        let mut n = 0usize;
        let mut covered_everywhere = true;

        for y in 0..spec.canvas_height {
            for x in 0..spec.canvas_width {
                let idx = (y * spec.canvas_width + x) as usize;
                if coverage_count[idx] == 0 {
                    covered_everywhere = false;
                    continue;
                }
                let original = f64::from(content_value(x, y));
                let composite_code = 255.0 * composite[idx].powf(1.0 / GAMMA);
                let error = (composite_code - original).abs();
                max_error = max_error.max(error);
                sum_error += error;
                n += 1;
                if let (Some(boundary), Some(max_boundary)) =
                    (row_boundary, max_error_near_boundary.as_mut())
                {
                    if (y - boundary).abs() <= 30 {
                        *max_boundary = max_boundary.max(error);
                    }
                }
            }
        }

        Reconstruction {
            max_error,
            mean_error: sum_error / n.max(1) as f64,
            max_error_near_row_boundary: max_error_near_boundary,
            covered_everywhere,
        }
    }

    /// Runs both the blend-on and blend-off composites for one layout,
    /// prints every measurement (so a failure still reports the numbers),
    /// then checks them.
    fn assert_reconstructs(name: &str, slices: &[(&str, SliceGeom)], row_boundary: Option<i32>) {
        let canvas = build_canvas();

        let on_spec = slicer_spec(slices, true);
        let on = reconstruct(&on_spec, &canvas, row_boundary);
        let off_spec = slicer_spec(slices, false);
        let off = reconstruct(&off_spec, &canvas, None);

        eprintln!(
            "{name}: blend-on max {:.3}, mean {:.3}{}; blend-off max {:.3}",
            on.max_error,
            on.mean_error,
            on.max_error_near_row_boundary
                .map(|e| format!(", boundary-band max {e:.3}"))
                .unwrap_or_default(),
            off.max_error,
        );

        assert!(
            on.covered_everywhere,
            "{name}: every canvas pixel must be covered by at least one slice"
        );

        // The gain `Evaluator::transfer` writes is quantized to 1/256
        // (`round(weight^(1/gamma) * 256.0)`), and `Blend::shade` then
        // floors `(gain * value) >> 8` rather than rounding it, so a single
        // covering source can already be off by close to one code value.
        // A seam pixel sums two (or more) such shaded values, each with its
        // own independent rounding, so 3 code values covers the worst case
        // without being a loose margin; tighten it if the measurement above
        // comes in lower.
        assert!(
            on.max_error <= 3.0,
            "{name}: blend-on max error {} exceeds the 3 code value bound",
            on.max_error
        );
        assert!(
            on.mean_error <= 0.5,
            "{name}: blend-on mean error {} exceeds the 0.5 code value bound",
            on.mean_error
        );
        if let Some(boundary_error) = on.max_error_near_row_boundary {
            assert!(
                boundary_error <= 3.0,
                "{name}: max error within 30 rows of the row boundary is {boundary_error}, \
                 exceeding the 3 code value bound that holds everywhere else"
            );
        }

        // Blend off (`projection.blend = false`) gives every covering
        // source weight 1 (`Evaluator::weight_and_coverage`), so an overlap
        // is the sum of `n` full-strength copies instead of one: proof the
        // comparison above is not vacuous.
        assert!(
            off.max_error > 60.0,
            "{name}: blend-off composite should double overlaps (error over 60 code values \
             somewhere), got {}",
            off.max_error
        );
    }

    #[test]
    fn two_horizontal_strips_20_percent_overlap() {
        let slices = two_strip_slices();
        assert_reconstructs("two horizontal strips, 20% overlap", &slices, None);
    }

    #[test]
    fn two_by_two_grid_20_percent_overlaps() {
        let (slices, ..) = grid_slices();
        assert_reconstructs("2x2 grid, 20% overlaps both ways", &slices, None);
    }

    #[test]
    fn brain_scaled_columns_overlap_rows_touch() {
        // brain's real wall (see the SEAM slice notes in
        // .claude/plans/warp-fixes.md): two columns overlapping 5.8% of the
        // canvas width, two rows that touch with essentially no overlap.
        // Scaled to this 400x260 canvas at integer pixels: the nearest
        // integer overlap to 5.8% of 400 (23.2px) is 23px, i.e. 5.75%.
        let (slices, row_boundary) = brain_scaled_slices(0.0);
        assert_reconstructs(
            "brain-scaled: columns overlap 5.75%, rows touch",
            &slices,
            Some(row_boundary),
        );
    }

    #[test]
    fn brain_scaled_columns_overlap_rows_overlap_sliver() {
        // As above, but with brain's real measured 0.07-canvas-pixel row
        // sliver instead of an exact touch. The sum-to-one composite still
        // passes here (see the module doc comment: it cannot see a wrong
        // SPLIT, only a wrong total) — `row_independence_brain_scaled_rows_overlap_sliver`
        // below is the test that catches the wedge this sliver produces.
        let (slices, row_boundary) = brain_scaled_slices(0.07);
        assert_reconstructs(
            "brain-scaled: columns overlap 5.75%, rows overlap by a 0.07px sliver",
            &slices,
            Some(row_boundary),
        );
    }

    // ---- assertion set 2a: row independence of the column split -------

    struct RowIndependenceReport {
        max_deviation: f64,
        worst: Option<(String, i32, i32, f64, f64)>,
        /// `(row, share)` at one representative column, sorted by row, for
        /// reporting a by-row profile near the boundary.
        samples: Vec<(i32, f64)>,
    }

    /// For every output in `outputs` and every canvas column, compares that
    /// output's share at each of its own rows (skipping `reference_row`
    /// itself and any row whose pixel center falls inside
    /// `[exclude_y_lo, exclude_y_hi]`, the row-overlap band) against its
    /// share at `reference_row` in that same column. `sample_x` is recorded
    /// at every row for the by-row report.
    #[allow(clippy::too_many_arguments)]
    fn check_row_independence(
        rendered: &[RenderedSlice],
        outputs: &[&str],
        reference_row: i32,
        exclude_y_lo: f64,
        exclude_y_hi: f64,
        canvas_width: i32,
        sample_x: i32,
    ) -> RowIndependenceReport {
        let mut max_deviation = 0.0f64;
        let mut worst = None;
        let mut samples = Vec::new();

        for name in outputs {
            let rs = rendered
                .iter()
                .find(|s| s.output == *name)
                .expect("named output is among the rendered slices");
            for x in 0..canvas_width {
                let Some(reference_share) = share_at(rs, x, reference_row, GAMMA) else {
                    continue;
                };
                for ly in 0..rs.height {
                    let y = rs.y + ly as i32;
                    if y == reference_row {
                        continue;
                    }
                    let yc = f64::from(y) + 0.5;
                    if yc >= exclude_y_lo && yc <= exclude_y_hi {
                        continue;
                    }
                    let Some(share) = share_at(rs, x, y, GAMMA) else {
                        continue;
                    };
                    if x == sample_x {
                        samples.push((y, share));
                    }
                    let deviation = (share - reference_share).abs();
                    if deviation > max_deviation {
                        max_deviation = deviation;
                        worst = Some(((*name).to_string(), x, y, share, reference_share));
                    }
                }
            }
        }
        samples.sort_by_key(|(y, _)| *y);
        RowIndependenceReport {
            max_deviation,
            worst,
            samples,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn assert_row_independence(
        name: &str,
        spec: &super::SlicerSpec,
        canvas: &[u8],
        top: &[&str],
        bottom: &[&str],
        row_boundary: i32,
        band_lo: f64,
        band_hi: f64,
        sample_x: i32,
    ) {
        let rendered = render_slices(spec, canvas);
        let top_report =
            check_row_independence(&rendered, top, 0, band_lo, band_hi, CANVAS_WIDTH, sample_x);
        let bottom_report = check_row_independence(
            &rendered,
            bottom,
            CANVAS_HEIGHT - 1,
            band_lo,
            band_hi,
            CANVAS_WIDTH,
            sample_x,
        );

        eprintln!(
            "{name}: top-half (reference row 0) max deviation {:.4} (tolerance {:.4})",
            top_report.max_deviation, SHARE_TOLERANCE
        );
        if let Some((output, x, y, share, reference)) = &top_report.worst {
            eprintln!(
                "  worst: {output} at ({x},{y}) share {share:.4} vs row-0 reference {reference:.4}"
            );
        }
        eprintln!(
            "{name}: bottom-half (reference row {}) max deviation {:.4} (tolerance {:.4})",
            CANVAS_HEIGHT - 1,
            bottom_report.max_deviation,
            SHARE_TOLERANCE
        );
        if let Some((output, x, y, share, reference)) = &bottom_report.worst {
            eprintln!(
                "  worst: {output} at ({x},{y}) share {share:.4} vs row-{}-reference {reference:.4}",
                CANVAS_HEIGHT - 1
            );
        }
        eprintln!(
            "{name}: sample column x={sample_x}, bottom-half share by row near the boundary:"
        );
        for (y, share) in bottom_report
            .samples
            .iter()
            .filter(|(y, _)| (*y - row_boundary).abs() <= 30)
        {
            eprintln!(
                "    row {y} (boundary+{}): share {share:.4}",
                y - row_boundary
            );
        }

        assert!(
            top_report.max_deviation <= SHARE_TOLERANCE,
            "{name}: top-half share depends on row (max deviation {} > {})",
            top_report.max_deviation,
            SHARE_TOLERANCE
        );
        assert!(
            bottom_report.max_deviation <= SHARE_TOLERANCE,
            "{name}: bottom-half share depends on row (max deviation {} > {}) — a wrong SPLIT \
             between the two column slices, not a wrong total",
            bottom_report.max_deviation,
            SHARE_TOLERANCE
        );
    }

    #[test]
    fn row_independence_two_horizontal_strips() {
        // No column split at all in this layout (a single output per row),
        // so every column has exactly one covering row-slice outside the
        // row-overlap band: a trivial sanity check that the harness itself
        // is not the source of any deviation seen in the brain-scaled
        // layouts below.
        let canvas = build_flat_canvas();
        let slices = two_strip_slices();
        let spec = slicer_spec(&slices, true);
        assert_row_independence(
            "two horizontal strips",
            &spec,
            &canvas,
            &["TOP"],
            &["BOTTOM"],
            130,
            104.0,
            156.0,
            200,
        );
    }

    #[test]
    fn row_independence_brain_scaled_rows_touch() {
        let canvas = build_flat_canvas();
        let (slices, row_boundary) = brain_scaled_slices(0.0);
        let spec = slicer_spec(&slices, true);
        // Sample column: 10% into the column-overlap band from its left
        // edge (189 + 10% of 23px).
        assert_row_independence(
            "brain-scaled, rows touch exactly",
            &spec,
            &canvas,
            &["C1R1", "C2R1"],
            &["C1R2", "C2R2"],
            row_boundary,
            130.0,
            130.0,
            191,
        );
    }

    #[test]
    fn row_independence_brain_scaled_rows_overlap_sliver() {
        let canvas = build_flat_canvas();
        let (slices, row_boundary) = brain_scaled_slices(0.07);
        let spec = slicer_spec(&slices, true);
        // Expected to FAIL under the current minimum-distance seam rule:
        // the 0.07px row sliver spuriously activates the row edge across
        // the whole column-overlap band (`build_edge_masks`'s per-column
        // tables do not depend on the query row), pinning the C1/C2 column
        // split near 0.5 for a wedge below the row boundary before it
        // "snaps" to the correct ramp. See .claude/plans/warp-fixes.md,
        // "Round 2 ... Slice SEAM" — this is that slice's acceptance test.
        // Left failing on purpose: do not add #[ignore].
        assert_row_independence(
            "brain-scaled, rows overlap by a 0.07px sliver",
            &spec,
            &canvas,
            &["C1R1", "C2R1"],
            &["C1R2", "C2R2"],
            row_boundary,
            129.93,
            130.0,
            191,
        );
    }

    // ---- assertion set 2b: grid separability ---------------------------

    /// Each grid corner's share should be the product of a pure horizontal
    /// ramp (measured along a row far from the row-overlap band, where only
    /// that corner's own column-neighbor covers) and a pure vertical ramp
    /// (measured along a column far from the column-overlap band, where
    /// only its own row-neighbor covers). Reports and asserts this for all
    /// four corners; "report whether this holds" per the brief — this is
    /// not something the harness assumes, it is measured.
    fn assert_grid_share_separability(
        spec: &super::SlicerSpec,
        canvas: &[u8],
        x2: i32,
        col_width: i32,
        y2: i32,
        strip_height: i32,
    ) {
        let rendered = render_slices(spec, canvas);

        // Reference lines 80px in from each edge: comfortably outside both
        // the [x2, col_width) column-overlap band and the [y2, strip_height)
        // row-overlap band used below.
        let far_left = 80;
        let far_right = CANVAS_WIDTH - 1 - 80;
        let top_row = 0;
        let bottom_row = CANVAS_HEIGHT - 1;

        struct Corner {
            name: &'static str,
            ramp_x_row: i32,
            ramp_y_col: i32,
        }
        let corners = [
            Corner {
                name: "TL",
                ramp_x_row: top_row,
                ramp_y_col: far_left,
            },
            Corner {
                name: "TR",
                ramp_x_row: top_row,
                ramp_y_col: far_right,
            },
            Corner {
                name: "BL",
                ramp_x_row: bottom_row,
                ramp_y_col: far_left,
            },
            Corner {
                name: "BR",
                ramp_x_row: bottom_row,
                ramp_y_col: far_right,
            },
        ];

        struct CornerResult {
            name: &'static str,
            checked: usize,
            max_deviation: f64,
            worst: Option<(i32, i32, f64, f64)>,
        }

        let mut results = Vec::new();
        for corner in &corners {
            let rs = rendered
                .iter()
                .find(|s| s.output == corner.name)
                .expect("corner output is among the rendered slices");

            let ramp_x: Vec<(i32, Option<f64>)> = (x2..col_width)
                .map(|x| (x, share_at(rs, x, corner.ramp_x_row, GAMMA)))
                .collect();
            let ramp_y: Vec<(i32, Option<f64>)> = (y2..strip_height)
                .map(|y| (y, share_at(rs, corner.ramp_y_col, y, GAMMA)))
                .collect();

            let mut max_deviation = 0.0f64;
            let mut worst = None;
            let mut checked = 0usize;
            for &(x, rx) in &ramp_x {
                let Some(rx) = rx else { continue };
                for &(y, ry) in &ramp_y {
                    let Some(ry) = ry else { continue };
                    let Some(actual) = share_at(rs, x, y, GAMMA) else {
                        continue;
                    };
                    let expected = rx * ry;
                    let deviation = (actual - expected).abs();
                    checked += 1;
                    if deviation > max_deviation {
                        max_deviation = deviation;
                        worst = Some((x, y, actual, expected));
                    }
                }
            }
            results.push(CornerResult {
                name: corner.name,
                checked,
                max_deviation,
                worst,
            });
        }

        for result in &results {
            eprintln!(
                "grid separability, {}: checked {} points, max deviation {:.4} (tolerance {:.4})",
                result.name, result.checked, result.max_deviation, SEPARABILITY_TOLERANCE
            );
            if let Some((x, y, actual, expected)) = result.worst {
                eprintln!(
                    "  worst: ({x},{y}) actual share {actual:.4} vs ramp_x*ramp_y {expected:.4}"
                );
            }
        }

        for result in &results {
            assert!(
                result.checked > 0,
                "grid separability: {} had no comparable points (bright-enough overlap)",
                result.name
            );
            assert!(
                result.max_deviation <= SEPARABILITY_TOLERANCE,
                "grid separability failed for {}: max deviation {} > {}",
                result.name,
                result.max_deviation,
                SEPARABILITY_TOLERANCE
            );
        }
    }

    #[test]
    fn grid_share_separability() {
        let canvas = build_flat_canvas();
        let (slices, x2, col_width, y2, strip_height) = grid_slices();
        let spec = slicer_spec(&slices, true);
        assert_grid_share_separability(&spec, &canvas, x2, col_width, y2, strip_height);
    }
}
