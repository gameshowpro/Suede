//! Canvas planning and the physical black-lift coverage count.
//!
//! Everything here is deterministic geometry and arithmetic, deliberately free
//! of Wayland, processes, and IO, for the same reason the output planner is
//! pure: the correctness rules — luminance summing to one across a seam —
//! are testable on any machine. The seam blend weight itself lives in
//! exactly one place, [`super::layout::Evaluator`]: every [`CanvasPlan`] this
//! module produces carries a [`super::layout::LayoutSpec`], including the
//! legacy integer-position layouts synthesized by
//! [`canvas_plan_with_warp_activation`] below, so warp geometry (corner
//! pinning) reuses the same weights unchanged — a homography changes where a
//! seam is *sampled*, not what its blend ratio is.

use serde::{Deserialize, Serialize};

use crate::model::{CanvasRect, ProjectionConfig, Rect, Renderer, TestPattern};

use super::layout::{LayoutParticipant, LayoutSpec};

/// An output taking part in seam derivation: its place in the global layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Participant {
    pub name: String,
    pub rect: Rect,
    /// Whether a display is currently attached to this output.
    ///
    /// Absent outputs still take full part in the plan: they shape the
    /// canvas and every seam, because the *configuration* describes the
    /// installation and a dark projector does not change where the light
    /// from its neighbors lands. Only presentation is skipped — the region
    /// simply goes unshown. So unplugging one projector never alters what
    /// the others display, and plugging it back in needs no re-authoring.
    pub connected: bool,
}

/// Everything one overlay process needs. Serialized to the `suede blend`
/// subcommand verbatim — this struct *is* the daemon↔overlay contract.
///
/// This overlay path runs only where nothing in the configured layout
/// overlaps — the reconciler never builds a canvas for it otherwise — so it
/// never needs a seam blend; it exists to show a test pattern (or nothing
/// at all) over content that sway already composites directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlaySpec {
    pub output: String,
    /// Shapes the test pattern's own gradients; see
    /// [`crate::model::ProjectionConfig`].
    pub gamma: f64,
    /// The configured black-level compensation, carried so a change to it
    /// repaints the overlay. It is not *applied* here: this overlay runs
    /// only where nothing overlaps, so there is no raised black floor to
    /// match. The canvas path resolves it per pixel through [`Coverage`].
    #[serde(default)]
    pub black_lift: f64,
    /// This output's rectangle in the global layout, so patterns can draw in
    /// global coordinates and continue exactly across a seam.
    #[serde(default)]
    pub rect: Rect,
    /// This picture's true, possibly fractional and scaled, footprint in
    /// canvas space — the same value `SliceSpec::source_rect` carries for
    /// content. `None` means there is no canvas concept to place this
    /// picture in (the no-canvas bench-alignment overlay), so `rect` itself
    /// is the unit-density footprint. Canvas-anchored features (the grid's
    /// tile lines, warp-alignment's percentage lines and circles) are
    /// computed through this rectangle rather than through `rect` at 1:1, so
    /// they land on the same canvas position regardless of a slice's
    /// raster/canvas scale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_rect: Option<[f64; 4]>,
    /// Test pattern to draw instead of showing the content through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<TestPattern>,
    /// Global canvas dimensions (width, height), for canvas-relative patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas_size: Option<[u32; 2]>,
}

fn intersect(a: &Rect, b: &Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then_some(Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

/// One output's slice of the canvas: which region it shows. Its seam
/// blend weight is not carried here — it is derived from `SlicerSpec.layout`
/// (this output's entry in the shared [`super::layout::LayoutSpec`]) through
/// [`super::layout::Evaluator`], the one place that rule lives. `slice` is
/// in canvas coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SliceSpec {
    pub output: String,
    pub slice: Rect,
    /// Absolute canvas pixel boundaries for generalized slice placement.
    /// `slice` retains the exact crop origin and configured raster dimensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_rect: Option<[f64; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<super::warp::Geometry>,
}

/// Everything the slicer process needs: capture this, cut it up like that.
/// Serialized to the `suede slice` subcommand verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlicerSpec {
    /// Internal process session; never a persisted configuration revision.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub control_session: String,
    /// Output to capture — the headless canvas.
    pub source: String,
    pub canvas_width: i32,
    pub canvas_height: i32,
    pub gamma: f64,
    pub black_lift: f64,
    /// Optional dynamic shape transfer. The numeric `black_lift` remains the
    /// fixed fallback and startup level for adaptive mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_lift: Option<crate::model::AdaptiveBlackLift>,
    /// Render this instead of capturing, for alignment and calibration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<TestPattern>,
    /// Commit to each output as it becomes ready, rather than to all at once.
    #[serde(default)]
    pub free_run: bool,
    /// Which pipeline to blend with; see [`crate::model::Renderer`].
    #[serde(default)]
    pub renderer: Renderer,
    /// Full configured roster, independently of which outputs can present —
    /// the sole source of blend-weight truth (via
    /// [`super::layout::Evaluator`]) for every canvas plan this daemon
    /// builds, `canvas_plan_with_warp_activation`'s legacy integer layouts
    /// included. `None` only for a hand-built spec with no seam blending at
    /// all (every covered pixel full weight, no black lift).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<super::layout::LayoutSpec>,
    /// Configured simple-mode coverage, including unattached outputs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage_rects: Vec<Rect>,
    /// Draw the seam-boundary lineup markers over every output — the
    /// ephemeral `projection.temporary.highlightOverlaps` toggle, threaded
    /// here beside `pattern` and `gamma` because it shapes the per-pixel
    /// transfer table exactly as they do. A live-updatable field: it is
    /// deliberately absent from [`super::warp_update::same_topology`], so
    /// toggling it rebuilds the tables on the running slicer instead of
    /// restarting it. `false` is a complete no-op — see [`MARKER_NONE`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub highlight_overlaps: bool,
    pub slices: Vec<SliceSpec>,
}

impl SlicerSpec {
    /// The marker colors to paint with, or `None` while the markers are off.
    /// Both renderers pick their marker variant from this when a spec is
    /// installed, so a gamma change reaches the colors only while it is on.
    pub fn marker_palette(&self) -> Option<MarkerPalette> {
        self.highlight_overlaps
            .then(|| MarkerPalette::for_gamma(self.gamma))
    }
}

/// Everything the reconciler derives from the configured layout.
#[derive(Debug, Clone, PartialEq)]
pub struct CanvasPlan {
    pub canvas_width: i32,
    pub canvas_height: i32,
    pub layout: Option<super::layout::LayoutSpec>,
    /// Adaptive control is carried with the derived plan so the slicer can
    /// select the tagged shape transfer without changing geometry.
    pub adaptive_lift: Option<crate::model::AdaptiveBlackLift>,
    pub coverage_rects: Vec<Rect>,
    /// Where each output goes in *sway's* layout: a plain edge-to-edge
    /// tiling, row-major by the configured layout. Sway never sees overlaps.
    pub sway_positions: Vec<(String, i32, i32)>,
    /// The slices, ready for a [`SlicerSpec`] once the canvas output exists.
    pub slices: Vec<SliceSpec>,
}

/// Which layouts the slicer is asked to handle — the `allow_overlaps`
/// bootstrap flag, as [`canvas_plan`] sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slicing {
    /// Only when the layout overlaps. Sway tiles everything else natively and
    /// the direct path costs nothing per frame, so there is no reason to
    /// interpose a capture and a blend.
    WhenOverlapping,
    /// Every layout of two or more outputs, overlapping or not.
    ///
    /// A tiled layout then yields slices with no seams and so no blend. What
    /// it buys is that each display scans out one private, output-sized
    /// buffer instead of sharing one window across the lot — the case direct
    /// scanout was built for, and the case a driver cannot mirror by mistake.
    Always,
}

/// Derive the canvas from the configured layout.
///
/// **The layout is the projection configuration.** Each participant's
/// rectangle is where its beam lands in canvas space; wherever two
/// rectangles intersect, both projectors show that region of the canvas, and
/// with `blend` on each fades its own copy toward the neighbor. Every seam
/// carries its own width because every seam *is* its own intersection —
/// a top row may overlap differently from a bottom row, grids included.
///
/// The plan is a function of the *configuration alone*. Whether a display is
/// currently attached decides only which slices get presented — never the
/// canvas size and never a seam's weight. That is what keeps a failure
/// local: pull the
/// cable on one projector and the others carry on showing exactly the pixels
/// they showed a moment earlier, with the dead projector's region simply
/// unlit. Deriving any of the geometry from what happens to be plugged in
/// would make one loose connector reflow the whole installation.
///
/// A single identity output is never sliced: there is nothing to cut up, and
/// a canvas the size of one display bought with a capture and a blend pass is
/// pure loss. The internal warp activation seam below is the deliberate
/// exception: a nonidentity output needs the slicer even with blend off.
pub fn canvas_plan(
    participants: &[Participant],
    config: Option<&ProjectionConfig>,
    slicing: Slicing,
) -> Option<CanvasPlan> {
    canvas_plan_with_warp_activation(participants, config, slicing, false)
}

/// Derive a canvas plan while an internal caller knows that at least one
/// destination mapping is nonidentity. Geometry is intentionally only a
/// boolean here: Phase 1 keeps it out of the public desired-state schema,
/// while still giving the reconciler a testable activation seam for a direct
/// slicer fixture. A nonidentity mapping requires slicing even for one output
/// and even when blend is disabled; identity retains the direct path.
pub fn canvas_plan_with_warp_activation(
    participants: &[Participant],
    config: Option<&ProjectionConfig>,
    slicing: Slicing,
    has_nonidentity_warp: bool,
) -> Option<CanvasPlan> {
    if participants.is_empty()
        || participants
            .iter()
            .any(|participant| participant.rect.width <= 0 || participant.rect.height <= 0)
        || (participants.len() < 2 && !has_nonidentity_warp)
    {
        return None;
    }
    if slicing == Slicing::WhenOverlapping && !has_nonidentity_warp {
        let any_overlap = participants.iter().enumerate().any(|(i, a)| {
            participants[i + 1..]
                .iter()
                .any(|b| intersect(&a.rect, &b.rect).is_some())
        });
        if !any_overlap {
            return None;
        }
    }

    // Normalize so the canvas starts at 0,0 wherever the user drew it.
    let min_x = participants.iter().map(|p| p.rect.x).min().unwrap_or(0);
    let min_y = participants.iter().map(|p| p.rect.y).min().unwrap_or(0);
    let rects: Vec<(String, Rect)> = participants
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                Rect {
                    x: p.rect.x - min_x,
                    y: p.rect.y - min_y,
                    width: p.rect.width,
                    height: p.rect.height,
                },
            )
        })
        .collect();

    let canvas_width = rects.iter().map(|(_, r)| r.x + r.width).max().unwrap_or(0);
    let canvas_height = rects.iter().map(|(_, r)| r.y + r.height).max().unwrap_or(0);

    // Legacy integer layouts have no separate canvas-unit configuration: the
    // integer positions/sizes above (already normalized to start at 0,0)
    // *are* the whole layout. Synthesize the same `LayoutSpec` a warp canvas
    // would carry, normalizing by the bounding box's own width — canvas
    // units are isotropic (`crate::model::geometry`'s module doc), so one
    // divisor serves both axes. `layout::Evaluator` then derives seams,
    // stacking and black-lift coverage exactly as it does for a configured
    // warp canvas: this is the one blend-weight rule production has.
    let aspect = f64::from(canvas_width) / f64::from(canvas_height);
    let layout = LayoutSpec {
        aspect,
        blend: config.is_some_and(|p| p.blend),
        participants: rects
            .iter()
            .map(|(name, rect)| {
                let source = CanvasRect {
                    x: f64::from(rect.x) / f64::from(canvas_width),
                    y: f64::from(rect.y) / f64::from(canvas_width),
                    width: f64::from(rect.width) / f64::from(canvas_width),
                    height: f64::from(rect.height) / f64::from(canvas_width),
                };
                LayoutParticipant {
                    output: name.clone(),
                    slice: source,
                    // A legacy layout has no separate physical-footprint
                    // concept: the configured rectangle is both the crop and
                    // the coverage a black-lift shortfall is computed from.
                    raster_footprint: source,
                }
            })
            .collect(),
    };
    // Validates the topology a legacy layout still requires — nonempty,
    // non-mixed stacking, in-bounds — but not that every slice is reachable
    // from every other (`layout::Evaluator::new` no longer requires that; see
    // its own doc): a legacy layout that fails this is not a layout
    // `layout::Evaluator` — and so nothing downstream — can give one seam
    // rule to, so there is no canvas plan for it. That is rarer now, so it
    // is worth a line explaining why, rather than a silent `None` that looks
    // identical to "nothing overlaps" or "one output".
    if let Err(reason) = super::layout::Evaluator::new(&layout, canvas_width, canvas_height) {
        eprintln!(
            "blend: rejecting the synthesized layout for {} participant(s): {reason}",
            participants.len()
        );
        return None;
    }

    // Only connected outputs can be presented on. Everything above — the
    // canvas size and the synthesized layout — was computed from the full
    // configured roster, so a disconnected output leaves its region unshown
    // without altering a single pixel of its neighbors'.
    let slices: Vec<SliceSpec> = rects
        .iter()
        .zip(participants)
        .filter(|(_, participant)| participant.connected)
        .map(|((name, rect), _)| SliceSpec {
            source_rect: None,
            geometry: None,
            output: name.clone(),
            slice: *rect,
        })
        .collect();

    // Sway's layout: row-major order of the configured layout, tiled edge to
    // edge in one row. Purely internal — presenters cover every output.
    let mut order: Vec<usize> = (0..rects.len())
        .filter(|&i| participants[i].connected)
        .collect();
    order.sort_by_key(|&i| (rects[i].1.y, rects[i].1.x));
    let mut sway_positions = Vec::new();
    let mut x = 0;
    for index in order {
        sway_positions.push((rects[index].0.clone(), x, 0));
        x += rects[index].1.width;
    }

    Some(CanvasPlan {
        layout: Some(layout),
        adaptive_lift: config.and_then(|p| p.black_lift.adaptive_settings()),
        coverage_rects: rects.iter().map(|(_, rect)| *rect).collect(),
        canvas_width,
        canvas_height,
        sway_positions,
        slices,
    })
}

/// How many projectors light each point of the canvas.
///
/// Projector black is additive and cannot be subtracted: a region lit by `n`
/// projectors sits at `n` times one projector's black floor, so the only way
/// to make an installation *look* even is to raise everywhere else to meet
/// its worst spot. That makes the shortfall a function of coverage, and
/// coverage a function of the whole layout — which is why this is derived
/// from every slice's rectangle rather than from one projector's seams.
///
/// A plain two-projector blend has coverage 1 and 2 only, and reduces to the
/// original rule exactly: lift outside the seam, none inside it. A 2×2 grid
/// has three regimes — 1, 2 and 4 — and needs all three.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    rects: Vec<Rect>,
    max: u32,
}

/// Shape-transfer format used by adaptive black lift. Bit 31 tags this as a
/// dynamic entry. Bits 0..15 store the gamma-shaped ramp, bits 16..23 store
/// output-pixel picture coverage, bits 24..27 store physical coverage count,
/// bits 28..29 carry the seam-boundary marker tag (see [`MARKER_NONE`]) and
/// bit 30 is reserved and zero. All floating values are clamped before
/// positive-ties-up rounding; coverage counts above eight are rejected by the
/// update validator and saturate here for defensive callers.
pub const DYNAMIC_TRANSFER_TAG: u32 = 1 << 31;

pub fn pack_dynamic_shape(ramp: f64, edge: f64, coverage: u32) -> u32 {
    let r = (ramp.clamp(0.0, 1.0) * 65_535.0).round() as u32;
    let e = (edge.clamp(0.0, 1.0) * 255.0).round() as u32;
    let n = coverage.min(8);
    DYNAMIC_TRANSFER_TAG | r | (e << 16) | (n << 24)
}

/// The ramp, border coverage and coverage count of a dynamic entry, or
/// `None` if it is not one. The marker tag in bits 28..29 is deliberately
/// ignored here rather than rejected: a marker overrides the shade this
/// triple produces, it does not change it, so the two are independent. Bit
/// 30 remains reserved and a set one is still not a valid entry.
pub fn unpack_dynamic_shape(value: u32) -> Option<(u16, u8, u8)> {
    (value & DYNAMIC_TRANSFER_TAG != 0 && value & 0x4000_0000 == 0).then_some((
        (value & 0xffff) as u16,
        ((value >> 16) & 0xff) as u8,
        ((value >> 24) & 0xf) as u8,
    ))
}

/// The marker tag carried by a packed transfer word, fixed or dynamic.
pub fn marker_of_packed(value: u32) -> u8 {
    ((value >> MARKER_PACKED_SHIFT) & 0x3) as u8
}

/// Reference arithmetic for the tagged dynamic transfer. The ramp and edge
/// inputs are the unpacked quantized values, matching the shader's input.
pub fn dynamic_shade(value: u32, input: u8, level: f64, maximum: u32) -> u8 {
    let Some((r16, e8, n8)) = unpack_dynamic_shape(value) else {
        return 0;
    };
    let n = u32::from(n8);
    if n == 0 || e8 == 0 || maximum == 0 {
        return 0;
    }
    let r = f64::from(r16) / 65_535.0;
    let e = f64::from(e8) / 255.0;
    let l = (level.clamp(0.0, 1.0) * f64::from(maximum.saturating_sub(n)) / f64::from(n))
        .clamp(0.0, 1.0);
    (255.0 * e * ((1.0 - l) * r * (f64::from(input) / 255.0) + l))
        .round()
        .clamp(0.0, 255.0) as u8
}

fn covering(rects: &[Rect], x: f64, y: f64) -> u32 {
    rects
        .iter()
        .filter(|r| {
            x >= r.x as f64
                && x < (r.x + r.width) as f64
                && y >= r.y as f64
                && y < (r.y + r.height) as f64
        })
        .count() as u32
}

impl Coverage {
    /// Every projector's rectangle, in one shared coordinate space.
    pub fn new(rects: impl IntoIterator<Item = Rect>) -> Self {
        let rects: Vec<Rect> = rects
            .into_iter()
            .filter(|r| r.width > 0 && r.height > 0)
            .collect();
        // Coverage only changes where a rectangle begins or ends, so the
        // true maximum is found by sampling the center of every cell of the
        // grid those edges induce — a few dozen probes, not megapixels.
        let mut xs: Vec<i32> = rects.iter().flat_map(|r| [r.x, r.x + r.width]).collect();
        let mut ys: Vec<i32> = rects.iter().flat_map(|r| [r.y, r.y + r.height]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        let mut max = 0;
        for xw in xs.windows(2) {
            for yw in ys.windows(2) {
                let x = f64::from(xw[0] + xw[1]) / 2.0;
                let y = f64::from(yw[0] + yw[1]) / 2.0;
                max = max.max(covering(&rects, x, y));
            }
        }
        Self { rects, max }
    }

    /// The highest coverage anywhere — the black floor everything must match.
    pub fn max(&self) -> u32 {
        self.max
    }

    /// How many projectors light this point.
    pub fn at(&self, x: f64, y: f64) -> u32 {
        covering(&self.rects, x, y)
    }

    /// The lift *one* projector applies at this point.
    ///
    /// `black_lift` is calibrated as the lift that adds one projector's worth
    /// of black, so a point short by `max − n` projectors needs that much
    /// added — divided by `n`, because every projector lighting the point
    /// applies the overlay and their contributions add just as their black
    /// does. At full coverage the shortfall is zero and nothing is applied.
    pub fn lift(&self, black_lift: f64, x: f64, y: f64) -> f64 {
        let n = self.at(x, y);
        if n == 0 || n >= self.max {
            return 0.0;
        }
        (black_lift.clamp(0.0, 1.0) * f64::from(self.max - n) / f64::from(n)).clamp(0.0, 1.0)
    }
}

/// Seam-boundary marker tags — the `highlightOverlaps` lineup aid.
///
/// A tagged destination pixel is painted a flat, full-strength palette
/// color, **overriding** whatever the transfer above would have produced.
/// That override is not a stylistic choice, it is the only thing that can
/// work: the blue line marks the point where this source's own blend weight
/// reaches exactly zero, so a marker composited as ordinary pre-transfer
/// content would be multiplied away precisely where it is drawn. See
/// [`super::layout::Evaluator::marker_at`] for where the two boundaries are.
///
/// **Why a spare-bit tag rather than a second buffer.** The tag has to reach
/// two consumers that already carry one per-destination-pixel word each, and
/// both words have unused range:
/// - the fixed `(a, b)` pair's gain is `0..=256` ([`pixel_transfer`],
///   [`super::layout::Evaluator::transfer`]), so bits 9..15 of its `u16` are
///   always zero — this uses the top two, 14..15, as
///   [`MARKER_GAIN_SHIFT`];
/// - the packed `u32` the GPU reads is `a << 8 | b` for a fixed entry (bits
///   17..31 zero) and [`pack_dynamic_shape`] for a dynamic one (bits 28..30
///   documented reserved zero), so bits 28..29 are free in *both* — this
///   uses them, as [`MARKER_PACKED_SHIFT`], so the shader has exactly one
///   place to look regardless of transfer mode.
///
/// Adding a parallel buffer instead would have meant a fourth SSBO binding,
/// a descriptor-layout and pipeline change, a per-output allocation and
/// upload path and a push constant to say whether it is bound — all to carry
/// two bits that the existing word already has room for.
///
/// Nothing is spent when the aid is off. An untagged entry is bit-for-bit the
/// value this daemon has always produced, and only the marker variants read
/// the tag: `blend_markers.frag` on the GPU (see `Gpu::set_markers`) and
/// `Blend::marked_rows` / `SyncPaint::marked_rows` on the CPU. Each is chosen
/// when a spec is installed, so the plain shader, row loops and table
/// closures run unchanged while the aid is off.
pub const MARKER_NONE: u8 = 0;
/// Orange (see [`MarkerPalette`]), at this source's own gradient START — the
/// boundary between the last full-weight pixel and the first attenuated one.
pub const MARKER_ORANGE: u8 = 1;
/// Blue (see [`MarkerPalette`]), at the OUTSIDE edge of the same overlap,
/// where this source's own blend weight reaches exactly zero.
pub const MARKER_BLUE: u8 = 2;

/// Where a tag lives in the fixed `(a, b)` pair's `u16` gain.
pub const MARKER_GAIN_SHIFT: u32 = 14;
/// Everything below [`MARKER_GAIN_SHIFT`]: the gain itself, `0..=256`.
pub const MARKER_GAIN_MASK: u16 = (1 << MARKER_GAIN_SHIFT) - 1;
/// Where a tag lives in the packed `u32` both transfer formats share.
pub const MARKER_PACKED_SHIFT: u32 = 28;

/// The green both marker colors share at `gamma`: `round(255 × 0.5^(1/gamma))`,
/// the signal that emits exactly half the light. 186 (`0xba`) at 2.2.
pub fn marker_green(gamma: f64) -> u8 {
    (255.0 * 0.5f64.powf(1.0 / gamma)).round() as u8
}

/// The two marker colors, orange `(255, g, 0)` and blue `(0, g, 255)` with
/// `g` from [`marker_green`] at the configured gamma.
///
/// The two are complementary in LIGHT, not in signal: projectors add light,
/// so each green emits half and the pair sums to `(1, 1, 1)`. Where a wall is
/// lined up correctly one output's orange lands on its neighbor's blue and
/// the pair reads as a white line; any misalignment shows as an orange or
/// blue fringe instead. That also makes an aligned line a gamma check: if it
/// reads green or magenta rather than white, the configured gamma does not
/// match the projectors'.
///
/// Computed only when a spec is installed with markers on (see
/// [`SlicerSpec::marker_palette`]), never per frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarkerPalette {
    green: u8,
}

impl MarkerPalette {
    pub fn for_gamma(gamma: f64) -> Self {
        MarkerPalette {
            green: marker_green(gamma),
        }
    }

    pub fn green(self) -> u8 {
        self.green
    }

    /// The color a tag paints, or `None` for [`MARKER_NONE`] and any value
    /// outside the palette. RGB, in the order a `#rrggbb` literal reads.
    pub fn color(self, tag: u8) -> Option<[u8; 3]> {
        match tag {
            MARKER_ORANGE => Some([0xff, self.green, 0x00]),
            MARKER_BLUE => Some([0x00, self.green, 0xff]),
            _ => None,
        }
    }
}

/// The tag carried by a fixed `(a, b)` pair's gain.
pub fn marker_of_gain(a: u16) -> u8 {
    (a >> MARKER_GAIN_SHIFT) as u8
}

/// That gain with its tag removed — the `0..=256` the transfer arithmetic
/// wants. Every consumer of a `(u16, u8)` table entry must go through this,
/// or through [`marker_of_gain`], rather than reading the `u16` raw.
pub fn gain_of(a: u16) -> u16 {
    a & MARKER_GAIN_MASK
}

/// Tag a fixed `(a, b)` pair. `MARKER_NONE` returns it unchanged, which is
/// what makes the aid a byte-for-byte no-op while it is off.
pub fn with_marker((a, b): (u16, u8), tag: u8) -> (u16, u8) {
    (a | (u16::from(tag) << MARKER_GAIN_SHIFT), b)
}

/// Tag a packed dynamic-shape word.
pub fn with_packed_marker(value: u32, tag: u32) -> u32 {
    value | (tag << MARKER_PACKED_SHIFT)
}

/// The signal transfer for one pixel with no seam weighting — full own-source
/// signal (weight 1) — as fixed-point `(a, b)` where
/// `out = min(((a * input) >> 8) + b, 255)` for an 8-bit channel. Combines
/// output-pixel picture coverage `edge` with the black-level rescale `lift`
/// — [`Coverage::lift`], not the configured `blackLift` directly — so the
/// slicer applies one table in one pass.
///
/// This is what every seam-weighted transfer collapses to once a source has
/// no neighbor to blend against — the degenerate case of
/// [`super::layout::Evaluator::transfer`]'s own formula with weight fixed at
/// one. It exists as its own function for the "no configured layout at all"
/// fallback (see `SlicerSpec::layout`'s doc) where there is no seam rule to
/// evaluate in the first place. Lift is clamped to `[0, 1]`; positive ties
/// round upward. GPU storage packs this pair as
/// `(u32::from(a) << 8) | u32::from(b)`: bits 0..7 are `b`, bits 8..16 are
/// `a`, and bits 17..31 are zero.
pub fn pixel_transfer(lift: f64, edge: f64) -> (u16, u8) {
    let lift = lift.clamp(0.0, 1.0);
    (
        (edge * (1.0 - lift) * 256.0).round() as u16,
        (edge * lift * 255.0).round() as u8,
    )
}

/// One pattern overlay per output, for bench alignment when no canvas runs.
///
/// This path only ever runs where nothing in the layout overlaps — the
/// canvas is where seams exist — so these overlays carry only test
/// patterns, drawn in layout coordinates so two physically-aligned
/// projectors superimpose them.
pub fn overlay_specs(participants: &[Participant], config: &ProjectionConfig) -> Vec<OverlaySpec> {
    if config.test_pattern.is_none() {
        return Vec::new();
    }
    let mut specs: Vec<OverlaySpec> = participants
        .iter()
        .filter(|participant| participant.connected)
        .map(|participant| OverlaySpec {
            output: participant.name.clone(),
            gamma: config.gamma,
            black_lift: config.black_lift.level(),
            rect: participant.rect,
            source_rect: None,
            pattern: config.test_pattern,
            canvas_size: None,
        })
        .collect();
    specs.sort_by(|a, b| a.output.cmp(&b.output));
    specs
}

/// The complete overlay image: premultiplied BGRA bytes, row-major.
///
/// This is the *no-canvas* path, which the reconciler runs only when
/// nothing in the layout overlaps: every pixel is lit by exactly one
/// projector, so there is no seam to shape and no black-lift shortfall —
/// see [`Coverage`], which resolves that wherever seams do exist instead.
pub fn pixel_map(width: u32, height: u32, spec: &OverlaySpec) -> Vec<u8> {
    match spec.pattern {
        None => transparent_map(width, height),
        Some(_) => pattern_map(width, height, spec),
    }
}

/// The normal overlay: fully transparent, content shows through everywhere.
fn transparent_map(width: u32, height: u32) -> Vec<u8> {
    vec![0u8; width as usize * height as usize * 4]
}

/// A test pattern: fully opaque, unshaped — this path has no seams to fade.
fn pattern_map(width: u32, height: u32, spec: &OverlaySpec) -> Vec<u8> {
    let rgb = super::pattern::render(width, height, spec);
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let index = (y * width + x) as usize;
            let source = [rgb[index * 3], rgb[index * 3 + 1], rgb[index * 3 + 2]];
            let offset = index * 4;
            // Opaque and premultiplied: BGRA from RGB, straight through.
            pixels[offset] = source[2];
            pixels[offset + 1] = source[1];
            pixels[offset + 2] = source[0];
            pixels[offset + 3] = 255;
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::layout::Evaluator;

    fn participant(name: &str, x: i32, y: i32, width: i32, height: i32) -> Participant {
        Participant {
            name: name.to_string(),
            rect: Rect {
                x,
                y,
                width,
                height,
            },
            connected: true,
        }
    }

    /// The same output, configured but with nothing plugged into it.
    fn absent(name: &str, x: i32, y: i32, width: i32, height: i32) -> Participant {
        Participant {
            connected: false,
            ..participant(name, x, y, width, height)
        }
    }

    fn blending() -> ProjectionConfig {
        ProjectionConfig::default()
    }

    /// The `layout::Evaluator` a synthesized `CanvasPlan` carries — the one
    /// blend-weight rule every test below checks behavior through, rather
    /// than a deleted ramps abstraction.
    fn evaluator_for(plan: &CanvasPlan) -> Evaluator {
        Evaluator::new(
            plan.layout.as_ref().expect("legacy layouts synthesize one"),
            plan.canvas_width,
            plan.canvas_height,
        )
        .unwrap()
    }

    #[test]
    fn nonidentity_activation_slices_one_output_without_enabling_blend() {
        let config = ProjectionConfig {
            blend: false,
            ..ProjectionConfig::default()
        };
        let output = [participant("DP-1", 40, 20, 1920, 1080)];

        assert!(canvas_plan(&output, Some(&config), Slicing::WhenOverlapping).is_none());
        let plan = canvas_plan_with_warp_activation(
            &output,
            Some(&config),
            Slicing::WhenOverlapping,
            true,
        )
        .expect("a nonidentity output needs a slicer even without blend");
        assert_eq!((plan.canvas_width, plan.canvas_height), (1920, 1080));
        assert_eq!(plan.slices.len(), 1);
        let layout = plan
            .layout
            .as_ref()
            .expect("a layout is always synthesized");
        assert_eq!(layout.participants.len(), 1);
        assert!(!layout.blend);
        assert_eq!(
            plan.slices[0].slice,
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
    }

    // --- the canvas plan: the layout IS the configuration -----------------

    #[test]
    fn an_overlapping_pair_becomes_a_canvas_with_a_synthesized_layout() {
        let plan = canvas_plan(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                participant("DP-1", 1760, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .expect("overlap must produce a plan");

        assert_eq!((plan.canvas_width, plan.canvas_height), (3680, 1080));
        // Sway sees a plain tiling, never the overlap.
        assert_eq!(
            plan.sway_positions,
            vec![("DP-3".into(), 0, 0), ("DP-1".into(), 1920, 0)]
        );

        let left = &plan.slices[0];
        assert_eq!(left.output, "DP-3");
        assert_eq!(
            left.slice,
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
        let right = &plan.slices[1];
        assert_eq!(right.slice.x, 1760);

        // The synthesized layout carries both participants, normalized by
        // the canvas bounding-box width, and every downstream weight comes
        // from `layout::Evaluator` — the one blend-weight rule.
        let layout = plan.layout.as_ref().unwrap();
        assert_eq!(layout.participants.len(), 2);
        assert!(layout.blend);
        let evaluator = evaluator_for(&plan);
        // At the seam's midpoint (x = 1840, the middle of the 160px overlap
        // 1760..1920), both sides split evenly.
        assert_eq!(evaluator.transfer(0, 1840.0, 500.0, 1.0, 0.0, 1.0).0, 128);
        assert_eq!(evaluator.transfer(1, 1840.0, 500.0, 1.0, 0.0, 1.0).0, 128);
        // A quarter of the way in from DP-3's side (x = 1800), DP-3 keeps
        // three quarters of the light and DP-1 a quarter — matching the
        // ordinary strip ratio `layout::Evaluator`'s own tests check.
        assert_eq!(evaluator.transfer(0, 1800.0, 500.0, 1.0, 0.0, 1.0).0, 192);
        assert_eq!(evaluator.transfer(1, 1800.0, 500.0, 1.0, 0.0, 1.0).0, 64);
        // Outside the seam: full weight, unattenuated.
        assert_eq!(evaluator.transfer(0, 800.0, 500.0, 1.0, 0.0, 1.0).0, 256);
    }

    #[test]
    fn losing_a_projector_changes_nothing_the_others_show() {
        // The requirement, stated as a test: take a working three-projector
        // installation, unplug the middle one, and every pixel the survivors
        // were showing must still be the same pixel of the same canvas.
        let configured = [
            participant("DP-3", 0, 0, 1920, 1080),
            participant("DP-1", 1760, 0, 1920, 1080),
            participant("DP-2", 3520, 0, 1920, 1080),
        ];
        let whole =
            canvas_plan(&configured, Some(&blending()), Slicing::WhenOverlapping).expect("plan");

        let mut degraded = configured;
        degraded[1].connected = false;
        let degraded =
            canvas_plan(&degraded, Some(&blending()), Slicing::WhenOverlapping).expect("plan");

        // The canvas is unchanged, so the app renders exactly as before and
        // is never reloaded at a different size.
        assert_eq!(
            (degraded.canvas_width, degraded.canvas_height),
            (whole.canvas_width, whole.canvas_height)
        );
        assert_eq!(
            (degraded.canvas_width, degraded.canvas_height),
            (5440, 1080)
        );
        // The roster the layout is derived from is unchanged too — a
        // disconnected output still shapes its neighbors' seams.
        assert_eq!(whole.layout, degraded.layout);

        // The survivors keep their source rectangles.
        for name in ["DP-3", "DP-2"] {
            let before = whole.slices.iter().find(|s| s.output == name).unwrap();
            let after = degraded.slices.iter().find(|s| s.output == name).unwrap();
            assert_eq!(before, after, "{name} must be untouched");
        }
        // The absent output is simply not presented.
        assert!(!degraded.slices.iter().any(|s| s.output == "DP-1"));
        assert!(!degraded.sway_positions.iter().any(|(n, _, _)| n == "DP-1"));
    }

    #[test]
    fn an_output_that_was_never_plugged_in_still_shapes_the_canvas() {
        // Configuring a rig from the desk before the projectors arrive: the
        // canvas is the full installation, and the one display present shows
        // its own region of it, already blended for the neighbor to come.
        let plan = canvas_plan(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                absent("DP-1", 1760, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .expect("a configured overlap is a plan even with nothing attached");

        assert_eq!((plan.canvas_width, plan.canvas_height), (3680, 1080));
        assert_eq!(plan.slices.len(), 1);
        assert_eq!(plan.slices[0].output, "DP-3");
        assert_eq!(plan.sway_positions, vec![("DP-3".into(), 0, 0)]);
        // The absent DP-1 still takes part in the synthesized layout, and
        // still shapes DP-3's seam — the same ratio as the connected case.
        let layout = plan.layout.as_ref().unwrap();
        assert_eq!(layout.participants.len(), 2);
        let evaluator = evaluator_for(&plan);
        assert_eq!(evaluator.transfer(0, 1800.0, 500.0, 1.0, 0.0, 1.0).0, 192);
    }

    #[test]
    fn a_test_pattern_is_only_sent_to_attached_displays() {
        let config = ProjectionConfig {
            test_pattern: Some(TestPattern::Grid),
            ..ProjectionConfig::default()
        };
        let specs = overlay_specs(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                absent("DP-1", 1920, 0, 1920, 1080),
            ],
            &config,
        );
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].output, "DP-3");
    }

    #[test]
    fn every_seam_carries_its_own_overlap() {
        // A row of three where the rigger got 160 px on one seam and 100 on
        // the other. No single ramp width can describe this; the layout's
        // geometry can — each seam's own rectangles imply its own overlap.
        let plan = canvas_plan(
            &[
                participant("A", 0, 0, 1920, 1080),
                participant("B", 1760, 0, 1920, 1080),
                participant("C", 3580, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .unwrap();

        assert_eq!(plan.canvas_width, 5500);
        let evaluator = evaluator_for(&plan);
        // 40px into the 160px A-B seam (25%) and 40px into the 100px B-C
        // seam (40%) give different ratios, proving each seam is evaluated
        // by its own geometry rather than one shared width.
        let (ab, _) = evaluator.transfer(1, 1800.0, 500.0, 1.0, 0.0, 1.0);
        let (bc, _) = evaluator.transfer(1, 3620.0, 500.0, 1.0, 0.0, 1.0);
        assert_ne!(ab, bc, "the two seams must not share one ratio");
        assert_eq!(ab, 64); // B is 40/160 = 25% of the way in from A's side.
        assert_eq!(bc, 154); // B is 1 - 40/100 = 60% of the way in from C's side.
    }

    #[test]
    fn rows_can_overlap_differently_from_columns() {
        // The 2x2 the redesign asked for: the top pair overlaps 160 in x,
        // the rows overlap 90 in y. The synthesized layout carries all four
        // rectangles, so `layout::Evaluator` derives each seam from them —
        // no diagonal cross-term, since the rule is per-edge distance.
        let plan = canvas_plan(
            &[
                participant("A", 0, 0, 1920, 1080),
                participant("B", 1760, 0, 1920, 1080),
                participant("C", 0, 990, 1920, 1080),
                participant("D", 1760, 990, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .unwrap();

        assert_eq!((plan.canvas_width, plan.canvas_height), (3680, 2070));
        assert_eq!(plan.layout.as_ref().unwrap().participants.len(), 4);
        let evaluator = evaluator_for(&plan);
        // At the symmetric four-way corner (midpoint of both seams), every
        // participant carries an equal quarter of the light.
        let (mid, _) = evaluator.transfer(0, 1840.0, 1035.0, 2.0, 0.0, 1.0);
        let expected = (0.25f64).powf(0.5);
        assert!(
            ((mid as f64 / 256.0) - expected).abs() < 0.02,
            "corner multiplier was {mid}"
        );
    }

    #[test]
    fn a_layout_without_overlaps_needs_no_canvas() {
        // Sway tiles non-overlapping layouts natively; the slicer would be
        // pure overhead.
        assert!(canvas_plan(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                participant("DP-1", 1920, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .is_none());
        assert!(canvas_plan(
            &[participant("DP-1", 0, 0, 1920, 1080)],
            None,
            Slicing::WhenOverlapping,
        )
        .is_none());
    }

    #[test]
    fn a_tiled_layout_is_sliced_anyway_when_the_appliance_asks_for_it() {
        // `allow_overlaps = true`: the point is not the seams (there are
        // none) but that each display scans out its own buffer.
        let plan = canvas_plan(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                participant("DP-1", 1920, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::Always,
        )
        .expect("a forced plan covers a tiled layout too");

        assert_eq!((plan.canvas_width, plan.canvas_height), (3840, 1080));
        assert_eq!(plan.slices.len(), 2);
        assert_eq!(
            plan.slices[0].slice,
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(
            plan.slices[1].slice,
            Rect {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
        // Nothing overlaps (the two only touch), so nothing fades: blending
        // is still on, it simply has no seam to shape either side of.
        let evaluator = evaluator_for(&plan);
        assert_eq!(evaluator.transfer(0, 500.0, 500.0, 1.0, 0.0, 1.0).0, 256);
        assert_eq!(evaluator.transfer(1, 3500.0, 500.0, 1.0, 0.0, 1.0).0, 256);
    }

    #[test]
    fn warp_activation_rejects_empty_or_negative_rasters() {
        for (width, height) in [(0, 1080), (1920, 0), (-1, 1080), (1920, -1)] {
            assert!(canvas_plan_with_warp_activation(
                &[participant("DP-1", 0, 0, width, height)],
                None,
                Slicing::WhenOverlapping,
                true,
            )
            .is_none());
        }
    }

    #[test]
    fn one_output_is_never_sliced_however_the_appliance_is_configured() {
        // Even forced: there is nothing to cut up, and the capture and blend
        // pass would be pure loss.
        assert!(canvas_plan(
            &[participant("DP-1", 0, 0, 1920, 1080)],
            Some(&blending()),
            Slicing::Always,
        )
        .is_none());
    }

    #[test]
    fn the_canvas_normalizes_wherever_the_layout_was_drawn() {
        // An operator who drew the layout starting at 500,300 still gets a
        // canvas anchored at zero.
        let plan = canvas_plan(
            &[
                participant("L", 500, 300, 1920, 1080),
                participant("R", 2260, 300, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .unwrap();
        assert_eq!((plan.canvas_width, plan.canvas_height), (3680, 1080));
        assert_eq!(plan.slices[0].slice.x, 0);
        assert_eq!(plan.slices[0].slice.y, 0);
    }

    #[test]
    fn a_full_mirror_is_duplicated_but_never_blended() {
        // Two projectors stacked for brightness, or a confidence monitor:
        // both show the region at full strength — the synthesized layout's
        // near-total overlap is classified a stack by
        // `crate::model::geometry::validate_sources`, same as a warp canvas.
        let plan = canvas_plan(
            &[
                participant("MAIN", 0, 0, 1920, 1080),
                participant("STACKED", 0, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .unwrap();
        assert_eq!(plan.slices[0].slice, plan.slices[1].slice);
        let evaluator = evaluator_for(&plan);
        assert_eq!(evaluator.transfer(0, 500.0, 500.0, 1.0, 0.0, 1.0).0, 256);
        assert_eq!(evaluator.transfer(1, 500.0, 500.0, 1.0, 0.0, 1.0).0, 256);
    }

    #[test]
    fn blend_off_still_slices_but_does_not_fade() {
        // Physically overlapping beams need the duplication even unblended.
        let plan = canvas_plan(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            Some(&ProjectionConfig {
                blend: false,
                ..Default::default()
            }),
            Slicing::WhenOverlapping,
        )
        .unwrap();
        assert!(!plan.layout.as_ref().unwrap().blend);
        let evaluator = evaluator_for(&plan);
        // Every covered point gets full weight, seam or not.
        assert_eq!(evaluator.transfer(0, 1840.0, 500.0, 1.0, 0.0, 1.0).0, 256);
        assert_eq!(evaluator.transfer(1, 1840.0, 500.0, 1.0, 0.0, 1.0).0, 256);

        // And with no projection section at all, the same.
        let plan = canvas_plan(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            None,
            Slicing::WhenOverlapping,
        )
        .unwrap();
        assert!(!plan.layout.as_ref().unwrap().blend);
    }

    // --- disconnected legacy layouts (P3, review A8/`layout.rs:177`) ------

    #[test]
    fn two_outputs_with_a_gap_still_get_a_canvas_plan() {
        // Before `layout::Evaluator` became the sole blend-weight rule, two
        // outputs with a deliberate gap between them (no overlap, no shared
        // edge) got a canvas plan with no seam to compute. `Slicing::Always`
        // is what a caller building a plan for direct scanout — not only
        // for overlap — uses; `WhenOverlapping` would already reject this
        // pair before reaching `Evaluator::new` (see the "must overlap"
        // check above), so it would not exercise the fix here.
        let plan = canvas_plan_with_warp_activation(
            &[
                participant("L", 0, 0, 1920, 1080),
                // A 100px gap: L ends at x=1920, R starts at x=2020.
                participant("R", 2020, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::Always,
            false,
        )
        .expect("a gap must not cost the pair its canvas plan");
        assert_eq!(plan.slices.len(), 2);
        let layout = plan
            .layout
            .as_ref()
            .expect("a layout is always synthesized");
        assert_eq!(layout.participants.len(), 2);
        let evaluator = evaluator_for(&plan);
        // Disconnected groups simply never blend with each other: each
        // source's own region is untouched, full weight, no seam.
        assert_eq!(evaluator.transfer(0, 900.0, 500.0, 1.0, 0.0, 1.0).0, 256);
        assert_eq!(evaluator.transfer(1, 2900.0, 500.0, 1.0, 0.0, 1.0).0, 256);
        // The gap itself belongs to neither source.
        assert_eq!(evaluator.transfer(0, 1960.0, 500.0, 1.0, 0.0, 1.0).0, 0);
    }

    #[test]
    fn three_outputs_where_only_two_overlap_keep_their_shared_canvas_plan() {
        // A and B overlap (and so are connected to each other); C sits in a
        // separate, disconnected group of its own — a realistic "two walls,
        // one daemon" installation, not merely two isolated pairs.
        let plan = canvas_plan_with_warp_activation(
            &[
                participant("A", 0, 0, 1920, 1080),
                participant("B", 1760, 0, 1920, 1080),
                participant("C", 5000, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::Always,
            false,
        )
        .expect("one disconnected group must not cost the others their plan");
        assert_eq!(plan.slices.len(), 3);
        let layout = plan
            .layout
            .as_ref()
            .expect("a layout is always synthesized");
        assert_eq!(layout.participants.len(), 3);
        let evaluator = evaluator_for(&plan);
        // A and B still share their seam exactly as the connected-pair test
        // above checks (same geometry, same expected split).
        assert_eq!(evaluator.transfer(0, 1840.0, 500.0, 1.0, 0.0, 1.0).0, 128);
        assert_eq!(evaluator.transfer(1, 1840.0, 500.0, 1.0, 0.0, 1.0).0, 128);
        // C, disconnected from both, keeps full weight throughout its own
        // region — nothing about A/B's seam reaches across the gap.
        assert_eq!(evaluator.transfer(2, 5900.0, 500.0, 1.0, 0.0, 1.0).0, 256);
    }

    // --- the per-pixel transfer through the synthesized layout ------------

    #[test]
    fn seam_light_sums_to_one_through_the_layout() {
        let plan = canvas_plan(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .unwrap();
        let gamma = 2.2;
        let evaluator = evaluator_for(&plan);
        for offset in 0..160 {
            let x = 1760.0 + offset as f64;
            let (a_left, _) = evaluator.transfer(0, x, 500.0, gamma, 0.0, 1.0);
            let (a_right, _) = evaluator.transfer(1, x, 500.0, gamma, 0.0, 1.0);
            let light = (a_left as f64 / 256.0).powf(gamma) + (a_right as f64 / 256.0).powf(gamma);
            assert!(
                (light - 1.0).abs() < 0.03,
                "column {offset}: light sums to {light}"
            );
        }
        // Outside the seam: identity.
        assert_eq!(
            evaluator.transfer(0, 800.0, 500.0, gamma, 0.0, 1.0),
            (256, 0)
        );
    }

    #[test]
    fn black_lift_applies_outside_seams_only() {
        // Two projectors: physical coverage is 1 or 2, so the general rule
        // collapses to the original one — full lift outside, none inside —
        // and `layout::Evaluator` resolves both the seam and the lift from
        // the same synthesized layout (legacy layouts get real black-lift
        // coverage counting too, not just a warp canvas).
        let plan = canvas_plan(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .unwrap();
        let evaluator = evaluator_for(&plan);
        assert_eq!(evaluator.maximum(), 2);

        // Outside: out = lift + (1-lift)*in.
        let (a, b) = evaluator.transfer(0, 800.5, 500.5, 2.2, 0.1, 1.0);
        assert_eq!(b, 26, "lift offset should be 0.1*255");
        assert_eq!(a, 230, "multiplier should be (1-0.1)*256");
        // Inside the seam: no lift, the doubled projector black is the lift.
        assert_eq!(evaluator.transfer(0, 1840.5, 500.5, 2.2, 0.1, 1.0).1, 0);
    }

    #[test]
    fn a_grid_lifts_every_region_by_its_own_shortfall() {
        // A 2×2 with 160px overlaps both ways. Three black floors exist —
        // one, two and four projectors — and only the four-way center is
        // already at the worst of them.
        let plan = canvas_plan(
            &[
                participant("TL", 0, 0, 1920, 1080),
                participant("TR", 1760, 0, 1920, 1080),
                participant("BL", 0, 920, 1920, 1080),
                participant("BR", 1760, 920, 1920, 1080),
            ],
            Some(&blending()),
            Slicing::WhenOverlapping,
        )
        .unwrap();
        let evaluator = evaluator_for(&plan);
        assert_eq!(evaluator.maximum(), 4, "the center is lit by all four");

        // And the center is the one region the old binary rule got wrong: it
        // sits inside the blend, so it must still receive no lift while the
        // two-way seams around it — also inside the blend — now do.
        assert_eq!(
            evaluator.transfer(0, 1840.0, 1000.0, 1.0, 0.05, 1.0).1,
            0,
            "four-way center"
        );
        assert_eq!(
            evaluator.transfer(0, 1840.0, 500.0, 1.0, 0.05, 1.0).1,
            13,
            "two-way seam"
        );
        assert_eq!(
            evaluator.transfer(0, 500.0, 500.0, 1.0, 0.05, 1.0).1,
            38,
            "single projector"
        );
    }

    // --- pattern overlays --------------------------------------------------

    #[test]
    fn overlays_exist_only_for_test_patterns() {
        let installation = [
            participant("DP-3", 0, 0, 1920, 1080),
            participant("DP-1", 1920, 0, 1920, 1080),
        ];
        assert!(overlay_specs(&installation, &blending()).is_empty());
        let specs = overlay_specs(
            &installation,
            &ProjectionConfig {
                test_pattern: Some(TestPattern::Grid),
                ..Default::default()
            },
        );
        assert_eq!(specs.len(), 2);
    }

    // --- the overlay pixel paths (no seams: the no-canvas path never has
    //     one) ---------------------------------------------------------

    #[test]
    fn a_plain_overlay_is_fully_transparent() {
        let spec = OverlaySpec {
            output: "DP-1".into(),
            gamma: 2.2,
            black_lift: 0.0,
            rect: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 1,
            },
            source_rect: None,
            pattern: None,
            canvas_size: None,
        };
        let pixels = pixel_map(100, 1, &spec);
        assert!(pixels.iter().all(|&b| b == 0));
    }

    /// Each marker green emits half the light at its gamma, so an aligned
    /// orange and blue pair sums to white; 2.2 keeps the original `0xba`.
    #[test]
    fn marker_green_emits_half_the_light_at_the_configured_gamma() {
        for (gamma, green) in [(1.8, 174), (2.2, 186), (2.4, 191)] {
            assert_eq!(marker_green(gamma), green, "gamma {gamma}");
            let palette = MarkerPalette::for_gamma(gamma);
            assert_eq!(palette.color(MARKER_ORANGE), Some([255, green, 0]));
            assert_eq!(palette.color(MARKER_BLUE), Some([0, green, 255]));
            assert_eq!(palette.color(MARKER_NONE), None);
            let light = |v: u8| (f64::from(v) / 255.0).powf(gamma);
            for i in 0..3 {
                let orange = palette.color(MARKER_ORANGE).unwrap()[i];
                let blue = palette.color(MARKER_BLUE).unwrap()[i];
                let sum = light(orange) + light(blue);
                assert!((sum - 1.0).abs() < 0.01, "gamma {gamma} channel {i}: {sum}");
            }
        }
    }

    #[test]
    fn a_pattern_overlay_is_opaque() {
        let spec = OverlaySpec {
            output: "DP-1".into(),
            gamma: 2.2,
            black_lift: 0.0,
            rect: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 1,
            },
            source_rect: None,
            pattern: Some(TestPattern::White),
            canvas_size: None,
        };
        let pixels = pixel_map(100, 1, &spec);
        assert!((0..100).all(|x| pixels[x * 4 + 3] == 255));
    }

    #[test]
    fn the_slicer_spec_serializes_stably() {
        // The spec crosses a process boundary as JSON; field names are ABI.
        let spec = SlicerSpec {
            // Round binary fractions, deliberately not derived from the
            // canvas dimensions below: serde_json's default (non-
            // `float_roundtrip`) float parser is not guaranteed exact to the
            // last bit for an arbitrary repeating decimal, so this ABI
            // stability check picks values with an exact `f64` JSON
            // round-trip rather than fighting that unrelated precision
            // question.
            layout: Some(LayoutSpec {
                aspect: 2.0,
                blend: true,
                participants: vec![LayoutParticipant {
                    output: "DP-3".into(),
                    slice: CanvasRect {
                        x: 0.0,
                        y: 0.0,
                        width: 0.5,
                        height: 0.25,
                    },
                    raster_footprint: CanvasRect {
                        x: 0.0,
                        y: 0.0,
                        width: 0.5,
                        height: 0.25,
                    },
                }],
            }),
            coverage_rects: Vec::new(),
            control_session: String::new(),
            source: "HEADLESS-1".into(),
            canvas_width: 3680,
            canvas_height: 1080,
            gamma: 2.2,
            black_lift: 0.04,
            adaptive_lift: None,
            pattern: None,
            free_run: false,
            renderer: Renderer::Auto,
            highlight_overlaps: false,
            slices: vec![SliceSpec {
                source_rect: None,
                geometry: None,
                output: "DP-3".into(),
                slice: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            }],
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains(r#""canvasWidth":3680"#), "{json}");
        assert!(json.contains(r#""renderer":"auto""#), "{json}");
        assert!(json.contains(r#""layout":{"#), "{json}");
        assert!(json.contains(r#""participants":["#), "{json}");
        let back: SlicerSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    fn channel((a, b): (u16, u8), input: u32) -> u8 {
        (((u32::from(a) * input) >> 8) + u32::from(b)).min(255) as u8
    }

    #[test]
    fn plain_transfer_matches_the_fixed_point_contract_at_every_lift_and_edge() {
        for (edge, lift, expected) in [
            (0.0, 0.0, (0, 0)),
            (0.0, 0.5, (0, 0)),
            (0.0, 1.0, (0, 0)),
            (0.25, 0.0, (64, 0)),
            (0.25, 0.5, (32, 32)),
            (0.5, 0.0, (128, 0)),
            (0.5, 0.1, (115, 13)),
            (0.5, 0.5, (64, 64)),
            (0.5, 1.0, (0, 128)),
            (1.0, 0.0, (256, 0)),
            (1.0, 0.1, (230, 26)),
            (1.0, 1.0, (0, 255)),
        ] {
            let actual = pixel_transfer(lift, edge);
            assert_eq!(actual, expected, "edge={edge} lift={lift}");
            // D2's ideal white level survives lift changes within the legacy
            // coefficient rounding plus the channel's integer truncation.
            assert!((f64::from(channel(actual, 255)) - 255.0 * edge).abs() <= 1.5);
            if edge == 0.0 {
                for input in 0..=255 {
                    assert_eq!(channel(actual, input), 0);
                }
            }
        }
        // Lift outside [0, 1] clamps rather than under/overflowing.
        assert_eq!(pixel_transfer(-0.1, 1.0), (256, 0));
        assert_eq!(pixel_transfer(1.1, 1.0), (0, 255));
    }

    #[test]
    fn coverage_shortfall_can_exceed_one_and_resolved_lift_saturates() {
        let rects = [(0, 0), (4, 0), (0, 4), (4, 4)].map(|(x, y)| Rect {
            x,
            y,
            width: 8,
            height: 8,
        });
        let coverage = Coverage::new(rects);
        assert_eq!(coverage.max(), 4);
        for (x, y, count, lift) in [
            (1.5, 1.5, 1, 0.75),
            (5.5, 1.5, 2, 0.25),
            (5.5, 5.5, 4, 0.0),
            (20.5, 20.5, 0, 0.0),
        ] {
            assert_eq!(coverage.at(x, y), count);
            assert_eq!(coverage.lift(0.25, x, y), lift);
        }
        let saturated = coverage.lift(0.5, 1.5, 1.5);
        assert_eq!(saturated, 1.0); // k=3, L*k=1.5
        assert_eq!(pixel_transfer(saturated, 0.5), (0, 128));
    }

    #[test]
    fn non_overlapping_topology_forces_lift_to_zero() {
        // When there are no overlaps anywhere (max coverage <= 1), black lift
        // is forced to zero everywhere to avoid unnecessary contrast degradation.
        let single = Coverage::new([Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }]);
        assert_eq!(single.max(), 1);
        assert_eq!(single.lift(0.2, 500.0, 500.0), 0.0);

        let tiled = Coverage::new([
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            Rect {
                x: 1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ]);
        assert_eq!(tiled.max(), 1);
        assert_eq!(tiled.lift(0.2, 500.0, 500.0), 0.0);
        assert_eq!(tiled.lift(0.2, 2500.0, 500.0), 0.0);
    }

    #[test]
    fn dynamic_shape_packing_rounds_saturates_and_reserves_bits() {
        let value = pack_dynamic_shape(0.5, 0.5, 9);
        assert_eq!(value & DYNAMIC_TRANSFER_TAG, DYNAMIC_TRANSFER_TAG);
        assert_eq!(value & 0x0000_ffff, 32_768);
        assert_eq!((value >> 16) & 0xff, 128);
        assert_eq!((value >> 24) & 0xf, 8);
        assert_eq!(value & 0x7000_0000, 0);
        assert_eq!(unpack_dynamic_shape(value), Some((32_768, 128, 8)));
    }

    #[test]
    fn dynamic_shade_handles_k_greater_than_one_and_saturation() {
        let shape = pack_dynamic_shape(1.0, 1.0, 1);
        assert_eq!(dynamic_shade(shape, 255, 0.4, 4), 255); // l saturates
        assert_eq!(dynamic_shade(shape, 0, 0.4, 4), 255);
        let half = pack_dynamic_shape(1.0, 0.5, 1);
        assert_eq!(dynamic_shade(half, 255, 0.0, 4), 128);
        assert_eq!(
            dynamic_shade(pack_dynamic_shape(1.0, 1.0, 0), 255, 0.2, 4),
            0
        );
        assert_eq!(
            dynamic_shade(pack_dynamic_shape(1.0, 0.0, 1), 255, 0.2, 4),
            0
        );
    }
}
