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
//! offers dmabuf capture and Vulkan initialises, falling back to the CPU
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
//! DLP projector's colour wheel has smeared the digits across the exposure —
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

use super::blend::{pixel_transfer, Coverage, OverlaySpec, SlicerSpec};
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
    /// Set once at startup by `negotiate_gpu`, when `renderer != Cpu` and
    /// dmabuf feedback completed and `Gpu::new` succeeded. `None` either
    /// because the renderer is forced to `Cpu`, or because something in
    /// that chain failed — `gpu_error` says what, for `decide_backend`'s
    /// fallback message.
    gpu: Option<gpu::Gpu>,
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
    /// neighbours — a partial commit here is exactly the race the gate
    /// exists to close. Free-run: one ready presenter is enough, because
    /// each takes the newest frame the instant it can, without regard for
    /// its neighbours' pace.
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
        if self.free_run {
            let signal = self.gate_signal();
            self.presenters
                .iter()
                .any(|p| p.stale && p.gate_ready(signal))
        } else {
            self.presenters.iter().any(|p| p.stale) && self.gate_open()
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

    // GPU/dmabuf negotiation, once, before anything else needs to know the
    // backend — see the module doc's negotiation notes. Skipped entirely
    // for a *static* test pattern, which never captures at all and draws
    // straight into shm-backed presenter buffers regardless of `renderer`.
    // The `sync` pattern is not static: it presents every frame, through
    // whichever backend content would have used, because what it measures
    // is that backend's path to the glass.
    if !spec.pattern.is_some_and(|pattern| !animated(pattern)) && spec.renderer != Renderer::Cpu {
        match negotiate_gpu(dmabuf.as_ref(), &mut state, &mut queue, &handle) {
            Ok((gpu, formats)) => {
                state.gpu = Some(gpu);
                state.gpu_formats = formats;
            }
            Err(reason) => {
                if spec.renderer == Renderer::Gpu {
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
    // How much black each region of the canvas is receiving. Derived from
    // every slice, because how much a projector must lift depends on how many
    // *others* light the same pixel — a four-way grid centre needs none while
    // its two-way seams still do.
    let coverage = Coverage::new(spec.slices.iter().map(|slice| slice.source));
    for (presenter, slice) in state.presenters.iter_mut().zip(spec.slices.iter()) {
        let (width, height) = presenter.configured.unwrap();
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
        if animated(pattern) {
            // Presents every frame through the normal path, so it shares
            // nothing below but the presenters themselves.
            return run_sync(
                &connection,
                &mut queue,
                &mut state,
                &shm,
                dmabuf.as_ref(),
                &handle,
            );
        }
        // A test pattern is drawn by poking pixels directly (see
        // `present_pattern`), so it always needs a CPU-mapped buffer — the
        // GPU path never enters into it regardless of `renderer` (and
        // `negotiate_gpu` was skipped above for exactly this reason).
        for (index, presenter) in state.presenters.iter_mut().enumerate() {
            let (width, height) = presenter.configured.unwrap();
            // Always the shm count, whatever `renderer` says: a pattern is
            // poked pixel by pixel into a mapped buffer, so this is the CPU
            // path even when the GPU one was available.
            for slot in 0..CPU_PRESENT_SLOTS {
                presenter
                    .buffers
                    .push(shm_buffer(&shm, &handle, width, height, (index, slot))?);
            }
            presenter.busy = vec![false; presenter.buffers.len()];
        }
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
    // The backend (and, on the GPU path, the capture image) is decided as
    // of the `arm_copy` above — now create presenter buffers of the right
    // kind for it, before the first capture can possibly complete.
    create_present_buffers(&mut state, &shm, dmabuf.as_ref(), &handle)?;
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
                // The newest complete frame, held for the gate rather than
                // blended here: whether it goes out now or in a moment is
                // the gate's decision, made once at the bottom of the loop
                // for both backends. Arming the *next* capture still
                // happens right away — that is what keeps the pipeline
                // full, and it is independent of when this frame is shown.
                state.capture.pending_slot = Some(filled_slot);

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
            ensure_gpu_capture_buffer(state, dmabuf, handle)
        }
        _ => ensure_shm_capture_buffer(&mut state.capture, shm, handle),
    }
}

/// The backend decision itself, run once at the first `arm_copy` — see the
/// module doc's negotiation notes. `Ok` names the winner; `Err` is only
/// possible when `Renderer::Gpu` was forced and the GPU path turns out not
/// to be available, which is fatal (the slicer exits, the daemon respawns
/// it on its next reconcile).
fn decide_backend(state: &State, dmabuf: Option<&ZwpLinuxDmabufV1>) -> anyhow::Result<Backend> {
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
        let supported = gpu.supported_modifiers(format, &modifiers, gpu::Usage::Capture);
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
/// composites correctly, which is exactly today's behaviour.
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
    let old_transfer_matches = presenter.transfer.len() == width as usize * height as usize;

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
        if let Err(error) = gpu.set_transfer(index, width, height, &presenter.transfer) {
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
/// cycle's frame already went out to its neighbours. Either is guaranteed
/// not to be the image the compositor is currently writing into: that is
/// always the *other* slot (see `Capture.gpu_slot`'s doc).
fn present_frame_gpu(state: &mut State, handle: &QueueHandle<State>) {
    let slot = state
        .capture
        .pending_slot
        .take()
        .or(state.capture.last_blended_slot);
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
    let signal = state.gate_signal();
    let State {
        capture,
        presenters,
        free_run,
        stats,
        gpu,
        ..
    } = state;
    let Some((canvas_image, _)) = capture.gpu_images.get(capture_slot) else {
        for presenter in presenters.iter_mut() {
            presenter.stale = false;
        }
        return Vec::new();
    };
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
        let slot = (0..len)
            .map(|step| (presenter.next_buffer + step) % len)
            .find(|&candidate| !presenter.busy[candidate])
            .unwrap_or_else(|| {
                stats.buffer_reuse += 1;
                presenter.next_buffer % len
            });
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
            }
        })
        .collect();

    let gpu = gpu
        .as_mut()
        .expect("Backend::Gpu implies state.gpu is Some");
    match gpu.blend(canvas_image, y_invert, &jobs) {
        Ok(duration) => {
            stats.gpu += duration;
            capture.last_blended_slot = Some(capture_slot);
            due
        }
        Err(error) => {
            eprintln!("slicer: gpu.blend failed: {error:#}");
            for d in &due {
                presenters[d.index].busy[d.slot] = false;
            }
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
            presentation.feedback(&presenter.surface, handle, (d.index, snapshot_id));
        }
        // Both waits armed together, right before the commit they describe:
        // whichever of them the gate is anchored to is what holds the next
        // commit back until this one has landed.
        presenter.commit_sent(snapshot_id, presentation.is_some());
        presenter.surface.commit();
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
/// neighbours. Free-run commits presenter by presenter as each becomes
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
        // the search at `next_buffer` so the slots keep rotating in the
        // common case. Falling back to reusing a busy one only happens when
        // an output has fallen behind on releases — a stall, in practice —
        // and is counted so it shows up as non-zero if it ever happens
        // without one. The loop is written for any pool length; the two
        // paths size their pools differently (see [`CPU_PRESENT_SLOTS`] and
        // [`GPU_PRESENT_SLOTS`]).
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
        presenter.stale = false;
        if let Some(presentation) = presentation.as_ref() {
            timing.request(snapshot_id, index);
            presentation.feedback(&presenter.surface, handle, (index, snapshot_id));
        }
        presenter.commit_sent(snapshot_id, presentation.is_some());
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
        | TestPattern::Identify => false,
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
    shm: &WlShm,
    dmabuf: Option<&ZwpLinuxDmabufV1>,
    handle: &QueueHandle<State>,
) -> anyhow::Result<()> {
    state.capture.backend = Some(decide_sync_backend(state, dmabuf)?);
    create_present_buffers(state, shm, dmabuf, handle)?;
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
    state: &State,
    dmabuf: Option<&ZwpLinuxDmabufV1>,
) -> anyhow::Result<Backend> {
    if state.renderer == Renderer::Cpu {
        return Ok(Backend::Cpu);
    }
    match sync_gpu_availability(state, dmabuf) {
        Ok(()) => Ok(Backend::Gpu),
        Err(reason) => {
            if state.renderer == Renderer::Gpu {
                anyhow::bail!("renderer gpu was forced but is unavailable: {reason}");
            }
            eprintln!("slicer: renderer gpu unavailable ({reason}); falling back to cpu (shm)");
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
    let millis = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(i32::MAX as u128) as i32;
    let mut poll_fd = libc::pollfd {
        fd: connection.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // Safety: one fully initialised `pollfd` naming a socket this
    // `Connection` owns and outlives the call; `poll` writes `revents` and
    // nothing else.
    let ready = unsafe { libc::poll(&mut poll_fd, 1, millis) };
    if ready > 0 {
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
    match state.capture.backend {
        Some(Backend::Gpu) => present_sync_gpu(state, handle, frame, due),
        _ => present_sync_cpu(state, handle, frame, due),
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
    let signal = state.gate_signal();
    let State {
        presenters,
        free_run,
        stats,
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
        let slot = (0..len)
            .map(|step| (presenter.next_buffer + step) % len)
            .find(|&candidate| !presenter.busy[candidate])
            .unwrap_or_else(|| {
                stats.buffer_reuse += 1;
                presenter.next_buffer % len
            });
        presenter.next_buffer = (slot + 1) % len;
        presenter.busy[slot] = true;
        due.push(Due { index, slot });
    }
    due
}

/// CPU path: rasterise the rect list into each due presenter's next free shm
/// slot and commit it exactly as `present_frame_cpu` commits a slice of the
/// canvas — the same rotation, the same frame callback, the same
/// presentation-feedback request.
fn present_sync_cpu(
    state: &mut State,
    handle: &QueueHandle<State>,
    frame: u32,
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
        let groups = super::pattern::sync_rects(width, height, frame, &presenter.name, snapshot_id);
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
            presentation.feedback(&presenter.surface, handle, (d.index, snapshot_id));
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
        let groups = super::pattern::sync_rects(width, height, frame, &presenter.name, snapshot_id);
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
                // Unused in sync mode — the shader takes its colour from the
                // shape list, not from a canvas — but carried so a job is
                // one thing whichever mode built it.
                source_x: presenter.source.x.max(0) as u32,
                source_y: presenter.source.y.max(0) as u32,
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

/// Everything a `sync` rasterising worker needs that does not vary between
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
/// images and the shm capture buffer) keeps the `()` behaviour below via
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
        }
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
        let groups = super::super::pattern::sync_rects(width, height, 57, "DP-1", 9_000);
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
        let groups = super::super::pattern::sync_rects(640, 480, 3, "DP-2", 1);
        let (count, items) = sync_shape_items(&groups);
        let mut index = 0usize;
        for _ in 0..count {
            index = items[index + 1][1] as usize;
        }
        assert_eq!(index, items.len());
    }

    #[test]
    fn the_cpu_rasteriser_and_the_shader_produce_the_same_bytes() {
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
        let groups = super::super::pattern::sync_rects(width, height, 88, "DP-9", 42);
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
    fn a_banded_rasterise_matches_a_single_pass_one() {
        // The CPU path splits the rows across scoped threads exactly as the
        // content blend does, so a rect that straddles a band boundary must
        // come out the same either way.
        let (width, height) = (64u32, 48u32);
        let transfer = vec![(256u16, 0u8); (width * height) as usize];
        let rects: Vec<SyncRect> = super::super::pattern::sync_rects(width, height, 7, "DP-1", 5)
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
        ] {
            assert!(!animated(pattern), "{pattern:?} must stay one-shot");
        }
    }
}
