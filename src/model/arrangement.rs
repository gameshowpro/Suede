//! Grid arrangement: one authoritative solver for "arrange the outputs in a
//! rows x columns grid with a given overlap".
//!
//! The arithmetic used to live in the reference UI's JavaScript, where every
//! client had to reimplement it to agree. It is arithmetic over the document
//! and the canvas, so it belongs here: [`solve`] turns a request into the
//! source rectangles it implies, [`apply`] writes them into a document, and
//! [`in_effect`] says whether a persisted record still describes what the
//! document actually holds.
//!
//! Two things differ from the JavaScript this replaces:
//!
//! * Overlap is per axis — `overlapX` and `overlapY` — and both are honored
//!   **exactly**: the resolved overlaps always equal the requested ones, or
//!   the request is refused. Full canvas coverage comes from the content
//!   scale instead. With per-axis fits `fitX = requestedW / canvasW` and
//!   `fitY = requestedH / canvasH` (the grid's raster extents at the
//!   requested overlaps, over the canvas's), a scale `s` shows the grid at
//!   `extent / s` canvas pixels, so covering an axis needs `s <= fit`, and
//!   the least wasteful scale that covers both is `s = min(fitX, fitY)`: the
//!   axis with the smaller fit fills the canvas exactly and the other
//!   **overhangs** it, centered, by [`ArrangementSolution::overhang`]. Slice
//!   pixels that fall outside the canvas simply render black — unused slice
//!   pixels at the edge of the wall, which is acceptable, unlike an unused
//!   band of canvas that no projector shows. The rule is uniform: it needs
//!   no case analysis of which axes have a seam, and an axis without one (a
//!   single row or column) just has an inert overlap.
//!   [`ArrangementSolution::implied_aspect`] reports the canvas aspect at
//!   which the requested overlaps would fill both axes with no overhang. The
//!   canvas aspect is the operator's; this never changes it.
//!
//!   This supersedes an interim (never released) rule that treated the
//!   requested overlaps as *floors* and grew the overlap of the axis with
//!   room to spare until it too filled the canvas. Growing fed back into
//!   slider-driven clients: a client that adopted the grown value into its
//!   other slider re-sent it as the next request's floor, so changing one
//!   axis's overlap ratcheted up, and effectively overwrote, the other's.
//!   Exact overlaps make the response echo the request, and overhang absorbs
//!   the mismatch the growth used to.
//! * Grid metrics and placement are otherwise reproduced exactly, including
//!   the centering of a single row or single column — now also when it
//!   overhangs the canvas rather than falling short of it.
//!
//! Three more rules, added once operators hit the aspect-mismatch cases
//! above in practice:
//!
//! * An arrangement that would push an output's slice **entirely** off the
//!   canvas (it can happen to an edge slice under a large enough aspect
//!   mismatch with three or more rows or columns, since the overhang is
//!   split between the two ends) is refused, in every mode, naming the
//!   output. [`limits`] reports, per axis, the range of overlaps that stays
//!   clear of that and every other hard rule, so a client can clamp its
//!   controls and never send a request that would be refused.
//! * [`ArrangementRequest::allow_unused_canvas`] keeps its meaning: `true`
//!   fits the whole grid *inside* the canvas instead, at
//!   `scale = max(fitX, fitY)`, and reports the band of canvas that leaves
//!   uncovered. Default `false`: overlap requests overhang rather than leave
//!   a band, and a content-scale solve that would leave a band (an unseamed
//!   axis falling short of the canvas at the asked-for scale) is refused
//!   with a `422` naming the axis, the band as a percentage of the canvas,
//!   and the aspect that would close it.
//! * [`super::desired::OutputConfig::arrange_offset`] lets one output's
//!   placement be nudged after the solve, in normalized canvas units.
//!   [`solve`] adds it to that output's source *after* [`place`] and after
//!   the coverage and off-canvas gates above, so an offset is never mistaken
//!   for an uncovered band or a lost slice — it is the caller's deliberate
//!   shift, e.g. correcting for a projector that is not quite where the grid
//!   puts it.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::desired::{DesiredState, OutputConfig, Transform};
use super::geometry::{CanvasConfig, CanvasRect, OutputGeometry};

/// Two source rectangles are the same arrangement when every edge agrees to
/// within this many normalized canvas units.
const SOURCE_EPS: f64 = 1.0e-9;

/// Overlap above this fraction is legal but almost always a mistake, so the
/// solution carries a warning a client can show.
const HIGH_OVERLAP: f64 = 0.8;

/// Unit destination pins, in TL, TR, BR, BL order.
const UNIT_CORNERS: [[f64; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

/// The resolved grid arrangement last applied to a document.
///
/// All five values are resolved: whichever of overlap or content scale the
/// client supplied, this records what the solver settled on. When overlaps
/// were requested they come back exactly as requested and only the content
/// scale is derived; when a content scale was requested the overlaps are
/// derived from it. It is a record of *intent*, not canonical geometry — the source
/// rectangles remain the truth, and a later manual geometry edit leaves this
/// in place. See [`in_effect`] for the test of whether it still describes the
/// document.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Arrangement {
    /// Grid rows; at least 1.
    pub rows: u32,
    /// Grid columns; at least 1.
    pub columns: u32,
    /// Fraction of the smaller of two adjacent columns' raster widths that
    /// they share; `0 <= overlapX < 1`. Exactly the requested value in
    /// overlap mode.
    pub overlap_x: f64,
    /// As `overlapX`, for adjacent rows' raster heights.
    pub overlap_y: f64,
    /// Output pixels per canvas pixel; `1.0` samples the canvas 1:1.
    pub content_scale: f64,
}

/// What a client asks for: the grid, plus *either* the overlaps *or* the
/// content scale.
///
/// The overlaps are exact: the solver either honors them as given, choosing
/// the content scale so the grid covers the whole canvas (overhanging it on
/// the axis whose fit is larger), or refuses the request. It never changes an
/// overlap. Supplying both overlaps and a content scale is over-determined
/// and is an error; supplying neither means both overlaps are zero.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementRequest {
    pub rows: u32,
    pub columns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap_x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap_y: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_scale: Option<f64>,
    /// Accept a solution that leaves part of the canvas uncovered.
    ///
    /// Default `false`: overlap requests cover the whole canvas, overhanging
    /// it on the axis whose fit is larger (`contentScale = min(fitX, fitY)`),
    /// so they never leave a band; a content-scale solve that would leave
    /// one (an unseamed axis falling short of the canvas at the asked-for
    /// scale) is refused with a `422` naming the axis, the band as a
    /// percentage of the canvas, and the aspect that would close it. `true`
    /// instead fits the whole grid *inside* the canvas at the requested
    /// overlaps — `contentScale = max(fitX, fitY)`, since a smaller scale
    /// would push a source past the canvas edge and a larger one would cover
    /// less on both axes — and reports the band that leaves.
    ///
    /// **Behavior change from Suede 0.1.14**: a request that used to succeed
    /// with a band left on the canvas (any single-row or single-column grid
    /// on a mismatched canvas aspect, for instance) now overhangs the canvas
    /// instead; set this to `true` to get the old fit-inside answer and its
    /// band back.
    #[serde(default)]
    pub allow_unused_canvas: bool,
    /// As on `PUT /config`: `false` applies to the outputs and leaves disk
    /// untouched, `true` persists. The solver itself ignores this.
    #[serde(default)]
    pub committed: bool,
}

/// One enabled output's placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArrangedOutput {
    /// [`super::desired::OutputMatch::key`] of the output this places.
    pub key: String,
    /// The normalized canvas rectangle this output samples.
    pub source: CanvasRect,
}

/// The fraction of the canvas left uncovered on each axis, measured before
/// any [`super::desired::OutputConfig::arrange_offset`] is applied.
///
/// Zero in overlap mode unless [`ArrangementRequest::allow_unused_canvas`]
/// asked for the fit-inside scale: the default `min`-of-fits scale fills the
/// axis with the smaller fit exactly (up to float dust) and overhangs the
/// other, so neither falls short. With `allowUnusedCanvas`, the
/// `max`-of-fits scale fills the axis with the larger fit instead and leaves
/// the other short. In content-scale mode an axis without a seam (a single
/// row or column) can fall short at the asked-for scale, which [`solve`]
/// refuses unless `allowUnusedCanvas` says to accept it. Whichever axis is
/// short is centered on the canvas, so half of its value is left before the
/// first slot and half after the last.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct UnusedCanvas {
    pub x: f64,
    pub y: f64,
}

/// The fraction of the canvas extent by which the grid overshoots the canvas
/// on each axis, measured before any
/// [`super::desired::OutputConfig::arrange_offset`] is applied.
///
/// The total for the axis, split evenly between its two ends: the grid is
/// centered, so half of it hangs off before the canvas's first edge and half
/// past its last. Slice pixels out there render black. In overlap mode this
/// is non-zero on the axis whose fit is the larger (the other fills the
/// canvas exactly); in content-scale mode, on an unseamed axis whose single
/// row or column is taller or wider than the canvas at the asked-for scale.
/// Advisory: the only overhang [`solve`] refuses is one that pushes a whole
/// slice off the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct Overhang {
    pub x: f64,
    pub y: f64,
}

/// A solved arrangement: what would be written, without writing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementSolution {
    /// The resolved five values, exactly as [`apply`] records them.
    pub arrangement: Arrangement,
    /// The uncovered fraction of the canvas on each axis; see
    /// [`UnusedCanvas`] for when this is non-zero.
    pub unused_canvas: UnusedCanvas,
    /// How far the grid overshoots the canvas on each axis, as a fraction of
    /// the canvas extent; see [`Overhang`].
    #[serde(default)]
    pub overhang: Overhang,
    /// The canvas aspect at which the requested overlaps would fill both
    /// axes exactly, with no overhang and no band. Reported, never applied:
    /// changing the aspect resizes the headless canvas and the browser, so
    /// it is the operator's decision.
    pub implied_aspect: f64,
    /// Every enabled output, in document order.
    pub outputs: Vec<ArrangedOutput>,
    /// Advisory notes about the solution; never a reason to refuse it.
    pub warnings: Vec<String>,
}

/// The overlaps one axis can take, holding the other axis at its requested
/// value; see [`limits`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AxisLimits {
    /// The smallest valid overlap; `0` unless an edge slice would otherwise
    /// fall off the canvas.
    pub min: f64,
    /// The largest valid overlap found. Always below `1`, since an overlap
    /// of `1` is never valid: the bound is exclusive, so this reports the
    /// largest value that was actually checked and found valid.
    pub max: f64,
    /// Whether the axis has a seam at all. `false` for a single row
    /// (`overlapY`) or a single column (`overlapX`): the overlap is then
    /// inert — it changes nothing — and a client should disable its control.
    /// An inert axis reports the whole `[0, 1)` range.
    pub has_seam: bool,
}

/// Per-axis overlap limits for one grid on one canvas; see [`limits`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementLimits {
    pub overlap_x: AxisLimits,
    pub overlap_y: AxisLimits,
}

/// [`limits`] scans each axis's `[0, 1)` range at this many even steps
/// before bisecting the boundaries it brackets.
const LIMIT_SCAN_STEPS: u32 = 64;

/// [`limits`] bisects each boundary until the valid and invalid ends of its
/// bracket are at most this far apart. From a `1/64` scan step that is eight
/// halvings, leaving brackets `1/16384` wide.
const LIMIT_RESOLUTION: f64 = 1.0e-4;

/// Grid metrics, in raster pixels, plus the canvas they are measured against.
struct Metrics {
    /// Widest raster in each column, and tallest in each row. Columns and
    /// rows with no output in them are zero, as in the JavaScript.
    col_width: Vec<f64>,
    row_height: Vec<f64>,
    sum_w: f64,
    sum_h: f64,
    /// Sum over adjacent pairs of the smaller raster extent — the length one
    /// unit of overlap removes from the total.
    div_w: f64,
    div_h: f64,
    /// Canvas raster size in pixels.
    canvas_w: f64,
    canvas_h: f64,
    /// Canvas pixels per normalized unit. `x_density` is the canvas width;
    /// `y_density` is `aspect * height`, because the canvas occupies
    /// `[0, 1]` by `[0, 1/aspect]`. Both come from
    /// [`CanvasRect::pixel_rect`], so this module holds no second copy of
    /// that arithmetic.
    x_density: f64,
    y_density: f64,
    /// Each enabled output's raster, in document order, after the transform
    /// swap.
    rasters: Vec<(f64, f64)>,
}

impl Metrics {
    /// Whether adjacent columns exist to share a seam. A single column has
    /// no seam, so `overlapX` cannot change anything about it.
    fn has_x(&self) -> bool {
        self.col_width.len() > 1 && self.div_w > 0.0
    }

    fn has_y(&self) -> bool {
        self.row_height.len() > 1 && self.div_h > 0.0
    }

    /// Total raster extent the arrangement spans on each axis, in raster
    /// pixels, once the overlaps have been removed.
    fn effective_extent(&self, overlap_x: f64, overlap_y: f64) -> (f64, f64) {
        (
            if self.has_x() {
                self.sum_w - overlap_x * self.div_w
            } else {
                self.sum_w
            },
            if self.has_y() {
                self.sum_h - overlap_y * self.div_h
            } else {
                self.sum_h
            },
        )
    }
}

struct Placement {
    rects: Vec<CanvasRect>,
    unused: UnusedCanvas,
    overhang: Overhang,
}

/// The raster the solver lays out for one output.
///
/// The callback returns the output's **mode** dimensions — the unrotated
/// raster — and this applies the 90/270 swap itself from
/// [`OutputConfig::effective_transform`], so the transform rule lives in one
/// place and a caller cannot forget it. Slice 2's caller therefore only has
/// to answer "effective mode, else adopted, else the observed current mode".
fn raster_of(
    output: &OutputConfig,
    raster: &impl Fn(&OutputConfig) -> Option<(u32, u32)>,
) -> Option<(f64, f64)> {
    let (width, height) = raster(output)?;
    if width == 0 || height == 0 {
        return None;
    }
    let rotated = matches!(
        output.effective_transform(),
        Some(
            Transform::Rotate90
                | Transform::Rotate270
                | Transform::Flipped90
                | Transform::Flipped270
        )
    );
    Some(if rotated {
        (f64::from(height), f64::from(width))
    } else {
        (f64::from(width), f64::from(height))
    })
}

fn grid(
    enabled: &[&OutputConfig],
    canvas: &CanvasConfig,
    rows: u32,
    columns: u32,
    raster: &impl Fn(&OutputConfig) -> Option<(u32, u32)>,
) -> Result<Metrics, String> {
    if rows == 0 || columns == 0 {
        return Err("arrangement rows and columns must be at least 1".into());
    }
    if enabled.is_empty() {
        return Err("at least one enabled output is required to arrange".into());
    }
    let slots = u64::from(rows) * u64::from(columns);
    if enabled.len() as u64 > slots {
        return Err(format!(
            "{} enabled outputs do not fit a {rows}x{columns} grid",
            enabled.len()
        ));
    }
    let (canvas_w, canvas_h) = canvas
        .dimensions()
        .map_err(|error| format!("canvas {error}"))?;
    // The one conversion from normalized units to canvas pixels, borrowed
    // from the rectangle type rather than rewritten: the unit rectangle's
    // pixel size is exactly the pair of densities.
    let [_, _, x_density, y_density] = CanvasRect {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    }
    .pixel_rect(canvas)?;

    let columns_usize = columns as usize;
    let mut col_width = vec![0.0_f64; columns_usize];
    let mut row_height = vec![0.0_f64; rows as usize];
    let mut rasters = Vec::with_capacity(enabled.len());
    for (index, output) in enabled.iter().enumerate() {
        let (width, height) = raster_of(output, raster)
            .ok_or_else(|| format!("output {} has no known raster size", output.r#match.key()))?;
        let row = index / columns_usize;
        let column = index % columns_usize;
        col_width[column] = col_width[column].max(width);
        row_height[row] = row_height[row].max(height);
        rasters.push((width, height));
    }

    let sum_w: f64 = col_width.iter().sum();
    let sum_h: f64 = row_height.iter().sum();
    let div_w = col_width
        .windows(2)
        .map(|pair| pair[0].min(pair[1]))
        .sum::<f64>();
    let div_h = row_height
        .windows(2)
        .map(|pair| pair[0].min(pair[1]))
        .sum::<f64>();

    Ok(Metrics {
        col_width,
        row_height,
        sum_w,
        sum_h,
        div_w,
        div_h,
        canvas_w: f64::from(canvas_w),
        canvas_h: f64::from(canvas_h),
        x_density,
        y_density,
        rasters,
    })
}

/// Lay the grid out from fully resolved values.
///
/// Origins accumulate in canvas pixels: each step advances by the previous
/// slot's raster extent less the seam it shares with the next, divided by
/// the content scale. Because overlap is below 1, every step is positive,
/// so the JavaScript's defensive `max` against a backwards step is not
/// needed. The grid is centered on each axis: the slack — canvas extent less
/// grid extent — is *signed*, so whatever the grid does not cover is split
/// evenly before and after it (which also reproduces the old centering of a
/// single row or single column), and whatever it overshoots hangs off both
/// ends equally. Before overlap mode could overhang, the slack was clamped
/// at zero, which left content-scale mode's only overhang (an unseamed axis
/// larger than the canvas) pinned to the canvas's first edge instead of
/// centered.
fn place(metrics: &Metrics, arrangement: &Arrangement) -> Placement {
    let columns = metrics.col_width.len();
    let rows = metrics.row_height.len();
    let scale = arrangement.content_scale;
    let (effective_w, effective_h) =
        metrics.effective_extent(arrangement.overlap_x, arrangement.overlap_y);
    let slack_x = metrics.canvas_w - effective_w / scale;
    let slack_y = metrics.canvas_h - effective_h / scale;

    let mut x_origin = Vec::with_capacity(columns);
    x_origin.push(slack_x / 2.0);
    for column in 1..columns {
        let seam =
            arrangement.overlap_x * metrics.col_width[column - 1].min(metrics.col_width[column]);
        let advance = (metrics.col_width[column - 1] - seam) / scale;
        x_origin.push(x_origin[column - 1] + advance);
    }
    let mut y_origin = Vec::with_capacity(rows);
    y_origin.push(slack_y / 2.0);
    for row in 1..rows {
        let seam = arrangement.overlap_y * metrics.row_height[row - 1].min(metrics.row_height[row]);
        let advance = (metrics.row_height[row - 1] - seam) / scale;
        y_origin.push(y_origin[row - 1] + advance);
    }

    let rects = metrics
        .rasters
        .iter()
        .enumerate()
        .map(|(index, &(width, height))| CanvasRect {
            x: x_origin[index % columns] / metrics.x_density,
            y: y_origin[index / columns] / metrics.y_density,
            width: width / (scale * metrics.x_density),
            height: height / (scale * metrics.y_density),
        })
        .collect();

    Placement {
        rects,
        unused: UnusedCanvas {
            x: slack_x.max(0.0) / metrics.canvas_w,
            y: slack_y.max(0.0) / metrics.canvas_h,
        },
        overhang: Overhang {
            x: (-slack_x).max(0.0) / metrics.canvas_w,
            y: (-slack_y).max(0.0) / metrics.canvas_h,
        },
    }
}

/// The index of the first placed slice that shows nothing of the canvas.
///
/// The canvas occupies `[0, 1]` by `[0, 1/aspect]` in normalized units; its
/// extents here come from the same densities [`place`] divided by, so the
/// two agree exactly. A slice whose intersection with it is empty — or no
/// wider or taller than [`SOURCE_EPS`], i.e. merely touching an edge — would
/// render nothing but black.
fn first_black_slice(metrics: &Metrics, rects: &[CanvasRect]) -> Option<usize> {
    let canvas_width = metrics.canvas_w / metrics.x_density;
    let canvas_height = metrics.canvas_h / metrics.y_density;
    rects.iter().position(|rect| {
        let visible_w = (rect.x + rect.width).min(canvas_width) - rect.x.max(0.0);
        let visible_h = (rect.y + rect.height).min(canvas_height) - rect.y.max(0.0);
        visible_w <= SOURCE_EPS || visible_h <= SOURCE_EPS
    })
}

/// Overlap mode's content scale, and the aspect the requested overlaps
/// imply.
///
/// With per-axis fits `fitX = effectiveW / canvasW` and
/// `fitY = effectiveH / canvasH` (the grid's raster extents at these
/// overlaps, over the canvas's), a scale `s` shows an axis at
/// `effective / s` canvas pixels, which covers the canvas only while
/// `s <= fit`. `min(fitX, fitY)` is therefore the largest — least wasteful —
/// scale that covers both axes: the axis with the smaller fit fills exactly
/// and the other overhangs. With `allow_unused_canvas`, `max(fitX, fitY)` is
/// the smallest scale that keeps the grid inside the canvas on both axes:
/// the axis with the larger fit fills exactly and the other is left short.
/// Neither rule needs to know which axes have a seam; an unseamed axis just
/// has an extent the overlap does not change.
///
/// The implied aspect is `effectiveW / effectiveH`: the canvas aspect at
/// which both fits agree, so both rules fill both axes.
fn overlap_scale(
    metrics: &Metrics,
    overlap_x: f64,
    overlap_y: f64,
    allow_unused_canvas: bool,
) -> Result<(f64, f64), String> {
    let (effective_w, effective_h) = metrics.effective_extent(overlap_x, overlap_y);
    let fit_x = effective_w / metrics.canvas_w;
    let fit_y = effective_h / metrics.canvas_h;
    let scale = if allow_unused_canvas {
        fit_x.max(fit_y)
    } else {
        fit_x.min(fit_y)
    };
    if !(scale.is_finite() && scale > 0.0) {
        return Err("this grid has no positive content scale".into());
    }
    Ok((scale, effective_w / effective_h))
}

/// The gates every placement must pass before offsets are applied: the
/// coverage gate, then the off-canvas gate.
///
/// Both run on [`place`]'s output and before any
/// [`super::desired::OutputConfig::arrange_offset`], because an offset is a
/// deliberate nudge, never a coverage failure or a lost slice. [`limits`]
/// runs exactly this too, so every value it reports is one [`solve`]
/// accepts.
fn placement_gates(
    enabled: &[&OutputConfig],
    metrics: &Metrics,
    placement: &Placement,
    allow_unused_canvas: bool,
    implied_aspect: f64,
) -> Result<(), String> {
    unused_canvas_gate(allow_unused_canvas, placement.unused, implied_aspect)?;
    if let Some(index) = first_black_slice(metrics, &placement.rects) {
        return Err(format!(
            "output {}'s slice falls entirely outside the canvas at these overlaps; the canvas \
             aspect that fits these overlaps exactly is {implied_aspect:.4}",
            enabled[index].r#match.key()
        ));
    }
    Ok(())
}

/// Solve an arrangement against one canvas.
///
/// `outputs` is the whole document's roster; disabled entries are ignored
/// and the enabled ones are placed in document order. See [`raster_of`] for
/// what the `raster` callback must return.
///
/// Every error is a client error: the string is suitable as a 422 detail.
pub fn solve(
    outputs: &[OutputConfig],
    canvas: &CanvasConfig,
    request: &ArrangementRequest,
    raster: impl Fn(&OutputConfig) -> Option<(u32, u32)>,
) -> Result<ArrangementSolution, String> {
    let enabled: Vec<&OutputConfig> = outputs.iter().filter(|output| output.enable).collect();
    let metrics = grid(&enabled, canvas, request.rows, request.columns, &raster)?;

    let asked_overlap = request.overlap_x.is_some() || request.overlap_y.is_some();
    let (overlap_x, overlap_y, content_scale, implied_aspect) =
        match (asked_overlap, request.content_scale) {
            (true, Some(_)) => {
                return Err(
                    "arrangement is over-determined: give overlapX and overlapY, or contentScale, \
                     but not both"
                        .into(),
                )
            }
            // Scale mode: the canvas fills every seamed axis and the
            // overlaps follow. The derived overlaps are the ones
            // `impliedAspect` is measured from.
            (false, Some(scale)) => {
                if !(scale.is_finite() && scale > 0.0) {
                    return Err(format!(
                        "contentScale must be a finite positive number, not {scale}"
                    ));
                }
                let overlap_x = if metrics.has_x() {
                    (metrics.sum_w - scale * metrics.canvas_w) / metrics.div_w
                } else {
                    0.0
                };
                let overlap_y = if metrics.has_y() {
                    (metrics.sum_h - scale * metrics.canvas_h) / metrics.div_h
                } else {
                    0.0
                };
                if overlap_x >= 1.0 || overlap_y >= 1.0 {
                    return Err(
                        "content scale is too small for this grid: overlap cannot reach 100%"
                            .into(),
                    );
                }
                let (effective_w, effective_h) = metrics.effective_extent(overlap_x, overlap_y);
                (overlap_x, overlap_y, scale, effective_w / effective_h)
            }
            // Overlap mode: the requested overlaps are exact, and only the
            // content scale is derived — `min(fitX, fitY)` to cover the
            // canvas (overhanging it on the other axis), or `max(fitX, fitY)`
            // to fit inside it when a band is allowed. See `overlap_scale`.
            (_, None) => {
                let overlap_x = checked_overlap("overlapX", request.overlap_x)?;
                let overlap_y = checked_overlap("overlapY", request.overlap_y)?;
                let (scale, implied_aspect) =
                    overlap_scale(&metrics, overlap_x, overlap_y, request.allow_unused_canvas)?;
                (overlap_x, overlap_y, scale, implied_aspect)
            }
        };

    let arrangement = Arrangement {
        rows: request.rows,
        columns: request.columns,
        overlap_x,
        overlap_y,
        content_scale,
    };
    let placement = place(&metrics, &arrangement);
    placement_gates(
        &enabled,
        &metrics,
        &placement,
        request.allow_unused_canvas,
        implied_aspect,
    )?;

    let mut warnings = Vec::new();
    for (label, value, neighbors) in [
        ("overlapX", overlap_x, "columns"),
        ("overlapY", overlap_y, "rows"),
    ] {
        if value < 0.0 {
            warnings.push(format!(
                "{label} is {:.1}%: adjacent {neighbors} leave a gap, which the document will \
                 reject as disconnected sources",
                value * 100.0
            ));
        } else if value > HIGH_OVERLAP {
            warnings.push(format!(
                "{label} of {:.0}% leaves little unique content in each of the {neighbors}",
                value * 100.0
            ));
        }
    }

    Ok(ArrangementSolution {
        arrangement,
        unused_canvas: placement.unused,
        overhang: placement.overhang,
        implied_aspect,
        outputs: enabled
            .iter()
            .zip(placement.rects)
            .map(|(output, mut source)| {
                if let Some(offset) = output.arrange_offset {
                    source.x += offset.x;
                    source.y += offset.y;
                }
                ArrangedOutput {
                    key: output.r#match.key(),
                    source,
                }
            })
            .collect(),
        warnings,
    })
}

fn checked_overlap(label: &str, value: Option<f64>) -> Result<f64, String> {
    let value = value.unwrap_or(0.0);
    if !(0.0..1.0).contains(&value) {
        return Err(format!(
            "{label} must be at least 0 and less than 1, not {value}"
        ));
    }
    Ok(value)
}

/// The strict full-coverage gate: refuse a solution that leaves either axis
/// uncovered by more than float dust, unless the caller said that was fine.
///
/// Applied uniformly by [`placement_gates`] regardless of which mode
/// produced the arrangement. Overlap mode's default `min`-of-fits scale
/// covers both axes by construction (its `unused` is zero up to
/// [`SOURCE_EPS`]-scale float dust), and its fit-inside scale only runs when
/// `allowUnusedCanvas` already waived this gate, so in overlap mode it is a
/// safety net. What can trip it is content-scale mode, which fills every
/// *seamed* axis exactly but can leave an unseamed axis's single row or
/// column short of the canvas at the asked-for scale.
fn unused_canvas_gate(
    allow_unused_canvas: bool,
    unused: UnusedCanvas,
    implied_aspect: f64,
) -> Result<(), String> {
    if allow_unused_canvas {
        return Ok(());
    }
    for (axis, fraction) in [("X", unused.x), ("Y", unused.y)] {
        if fraction > SOURCE_EPS {
            return Err(format!(
                "the canvas {axis} axis is left {:.1}% uncovered at this aspect; the canvas \
                 aspect that would close the gap is {implied_aspect:.4}, or pass \
                 allowUnusedCanvas=true to accept the band",
                fraction * 100.0
            ));
        }
    }
    Ok(())
}

/// Solve against a document's own canvas.
///
/// The one place that turns "this document has no canvas" into the error a
/// client sees; [`solve`] itself is given a canvas and cannot report it.
pub fn solve_document(
    document: &DesiredState,
    request: &ArrangementRequest,
    raster: impl Fn(&OutputConfig) -> Option<(u32, u32)>,
) -> Result<ArrangementSolution, String> {
    let canvas = document
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas.as_ref())
        .ok_or_else(|| "projection.canvas is required to arrange outputs".to_string())?;
    solve(&document.outputs, canvas, request, raster)
}

/// Per-axis overlap limits for an overlap-mode request.
///
/// For each axis, the closed range of overlaps [`solve`] accepts for this
/// grid on this canvas while the *other* axis's overlap stays at its
/// requested value, under every hard rule: `0 <= overlap < 1`, the scale
/// rule the request's `allowUnusedCanvas` selects, the coverage gate and the
/// off-canvas gate. A client that clamps each overlap control to its range
/// can never send an overlap request that is refused for its overlaps.
/// Offsets play no part: they are applied after the gates, so they never
/// make an overlap invalid.
///
/// Validity is decided by running the solver's own pipeline — [`grid`]'s
/// metrics, [`overlap_scale`], [`place`] and [`placement_gates`] — at
/// candidate values, so the limits cannot disagree with [`solve`]. No closed
/// form is attempted: with heterogeneous rasters the boundary is a piecewise
/// function of both overlaps, and a hand-derived formula would be a second
/// copy of the solver to keep in step. Instead each axis is scanned at
/// `k / 64` for `k = 0..=64` (the last sample, `1`, is invalid by the
/// exclusive bound), outward in both directions from an anchor — the
/// requested value when it is valid, else the valid sample nearest it —
/// until the first invalid sample, and each boundary so bracketed is
/// bisected until the bracket is at most [`LIMIT_RESOLUTION`] wide. The
/// valid end of each bracket is reported, so both bounds are values `solve`
/// accepts, while a value more than `LIMIT_RESOLUTION` past either is
/// refused. The range is the contiguous valid run containing the anchor. A
/// hole narrower than a scan step inside it would go unseen; for
/// homogeneous rasters validity is a single interval anyway (an edge slice
/// only loses ground as the overhang that swallows it grows), and the
/// scan-then-bisect approach exists so that any non-monotonic behavior from
/// heterogeneous rasters still yields a range whose ends were both checked,
/// rather than a formula that is quietly wrong.
///
/// An axis without a seam reports `has_seam: false` and, because its
/// overlap changes nothing, the whole `[0, 1)` range (as the largest value
/// the bisection toward `1` checks). Errors: everything [`solve`] refuses
/// before choosing a scale (grid, capacity, raster, canvas and overlap-range
/// errors), a request carrying a `contentScale` (it has no overlaps to
/// limit), and — only when no overlap on an axis is valid at all with the
/// other held where it is — the error `solve` gives at the requested values.
pub fn limits(
    outputs: &[OutputConfig],
    canvas: &CanvasConfig,
    request: &ArrangementRequest,
    raster: impl Fn(&OutputConfig) -> Option<(u32, u32)>,
) -> Result<ArrangementLimits, String> {
    if request.content_scale.is_some() {
        return Err(
            "overlap limits are computed for overlapX and overlapY, not contentScale".into(),
        );
    }
    let enabled: Vec<&OutputConfig> = outputs.iter().filter(|output| output.enable).collect();
    let metrics = grid(&enabled, canvas, request.rows, request.columns, &raster)?;
    let requested_x = checked_overlap("overlapX", request.overlap_x)?;
    let requested_y = checked_overlap("overlapY", request.overlap_y)?;

    // `solve`'s overlap arm and gates, without the offsets or warnings.
    let check = |overlap_x: f64, overlap_y: f64| -> Result<(), String> {
        let overlap_x = checked_overlap("overlapX", Some(overlap_x))?;
        let overlap_y = checked_overlap("overlapY", Some(overlap_y))?;
        let (content_scale, implied_aspect) =
            overlap_scale(&metrics, overlap_x, overlap_y, request.allow_unused_canvas)?;
        let placement = place(
            &metrics,
            &Arrangement {
                rows: request.rows,
                columns: request.columns,
                overlap_x,
                overlap_y,
                content_scale,
            },
        );
        placement_gates(
            &enabled,
            &metrics,
            &placement,
            request.allow_unused_canvas,
            implied_aspect,
        )
    };
    // `axis_range` only comes back empty when the requested value itself is
    // invalid (otherwise it is the anchor), so this is `solve`'s own refusal.
    let refusal = || {
        check(requested_x, requested_y)
            .err()
            .unwrap_or_else(|| "no valid overlap was found for this grid".into())
    };

    let (min_x, max_x) =
        axis_range(requested_x, &|value| check(value, requested_y).is_ok()).ok_or_else(refusal)?;
    let (min_y, max_y) =
        axis_range(requested_y, &|value| check(requested_x, value).is_ok()).ok_or_else(refusal)?;
    Ok(ArrangementLimits {
        overlap_x: AxisLimits {
            min: min_x,
            max: max_x,
            has_seam: metrics.has_x(),
        },
        overlap_y: AxisLimits {
            min: min_y,
            max: max_y,
            has_seam: metrics.has_y(),
        },
    })
}

/// [`limits`] against a document's own canvas, as [`solve_document`] is to
/// [`solve`].
pub fn limits_document(
    document: &DesiredState,
    request: &ArrangementRequest,
    raster: impl Fn(&OutputConfig) -> Option<(u32, u32)>,
) -> Result<ArrangementLimits, String> {
    let canvas = document
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas.as_ref())
        .ok_or_else(|| "projection.canvas is required to arrange outputs".to_string())?;
    limits(&document.outputs, canvas, request, raster)
}

/// The contiguous valid range around `current` on one axis, scanned and
/// bisected as [`limits`] describes; `None` when neither `current` nor any
/// scan sample is valid.
fn axis_range(current: f64, valid: &impl Fn(f64) -> bool) -> Option<(f64, f64)> {
    let sample = |step: u32| f64::from(step) / f64::from(LIMIT_SCAN_STEPS);
    let anchor = if valid(current) {
        current
    } else {
        (0..=LIMIT_SCAN_STEPS)
            .map(sample)
            .filter(|&value| valid(value))
            .min_by(|a, b| (a - current).abs().total_cmp(&(b - current).abs()))?
    };

    // Walk outward one scan step at a time, remembering the last valid value
    // reached; the first invalid sample closes the bracket to bisect. Walking
    // down can run out at 0 with everything valid, which makes 0 the bound;
    // walking up always meets the invalid sample at 1.
    let edge = |steps: &mut dyn Iterator<Item = f64>| {
        let mut inner = anchor;
        for value in steps {
            if valid(value) {
                inner = value;
            } else {
                return bisect(inner, value, valid);
            }
        }
        inner
    };
    let min = edge(
        &mut (0..=LIMIT_SCAN_STEPS)
            .rev()
            .map(sample)
            .filter(|&value| value < anchor),
    );
    let max = edge(
        &mut (0..=LIMIT_SCAN_STEPS)
            .map(sample)
            .filter(|&value| value > anchor),
    );
    Some((min, max))
}

/// Narrow a bracket with a valid end and an invalid end until the two are at
/// most [`LIMIT_RESOLUTION`] apart, and return the valid end.
fn bisect(mut valid_end: f64, mut invalid_end: f64, valid: &impl Fn(f64) -> bool) -> f64 {
    while (invalid_end - valid_end).abs() > LIMIT_RESOLUTION {
        let middle = (valid_end + invalid_end) / 2.0;
        if valid(middle) {
            valid_end = middle;
        } else {
            invalid_end = middle;
        }
    }
    valid_end
}

/// Write a solution into a document.
///
/// Only `geometry.source` is replaced: an output that was already
/// calibrated keeps its `corners`, `center` and `rasterFootprint`. An output
/// with no geometry at all is seeded with identity geometry, exactly as the
/// layout conversion does. Outputs the solution does not name — the
/// disabled ones — are untouched.
///
/// The caller validates the result; a solution can be arithmetically fine
/// and still produce a document validation rejects (a gap between sources,
/// for instance).
pub fn apply(document: &mut DesiredState, solution: &ArrangementSolution) {
    for arranged in &solution.outputs {
        let Some(output) = document
            .outputs
            .iter_mut()
            .find(|output| output.r#match.key() == arranged.key)
        else {
            continue;
        };
        match output.geometry.as_mut() {
            Some(geometry) => geometry.source = arranged.source,
            None => {
                output.geometry = Some(OutputGeometry {
                    source: arranged.source,
                    corners: UNIT_CORNERS,
                    center: [0.5, 0.5],
                    raster_footprint: arranged.source,
                })
            }
        }
    }
    if let Some(projection) = document.projection.as_mut() {
        projection.arrangement = Some(solution.arrangement);
    }
}

/// Whether the recorded arrangement still describes the document's sources.
///
/// The record is intent, so it survives a manual geometry edit; this is how
/// a client learns that the edit happened. Laying the recorded values out
/// again and comparing every enabled output's source rectangle is the whole
/// test — within [`SOURCE_EPS`] normalized units, because the numbers made a
/// round trip through JSON.
pub fn in_effect(
    document: &DesiredState,
    raster: impl Fn(&OutputConfig) -> Option<(u32, u32)>,
) -> bool {
    let Some(projection) = document.projection.as_ref() else {
        return false;
    };
    let (Some(arrangement), Some(canvas)) = (projection.arrangement, projection.canvas.as_ref())
    else {
        return false;
    };
    if !(arrangement.content_scale.is_finite() && arrangement.content_scale > 0.0) {
        return false;
    }
    let enabled: Vec<&OutputConfig> = document
        .outputs
        .iter()
        .filter(|output| output.enable)
        .collect();
    let Ok(metrics) = grid(
        &enabled,
        canvas,
        arrangement.rows,
        arrangement.columns,
        &raster,
    ) else {
        return false;
    };
    let placement = place(&metrics, &arrangement);
    enabled
        .iter()
        .zip(placement.rects)
        .all(|(output, mut expected)| {
            // Apply the document's *current* offset before comparing, the
            // same way `solve` applies it after placement — an offset edited
            // since the arrangement was applied is exactly the manual
            // geometry edit this function exists to detect.
            if let Some(offset) = output.arrange_offset {
                expected.x += offset.x;
                expected.y += offset.y;
            }
            output
                .geometry
                .as_ref()
                .is_some_and(|geometry| same_rect(geometry.source, expected))
        })
}

fn same_rect(left: CanvasRect, right: CanvasRect) -> bool {
    (left.x - right.x).abs() < SOURCE_EPS
        && (left.y - right.y).abs() < SOURCE_EPS
        && (left.width - right.width).abs() < SOURCE_EPS
        && (left.height - right.height).abs() < SOURCE_EPS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::desired::{ArrangeOffset, OutputMatch, ProjectionConfig};
    use crate::model::observed::Mode;

    /// 3840 x 2160: `dimensions()` rounds 3840 / (16/9) to 2160 exactly, and
    /// the y density is `aspect * height = (16/9) * 2160 = 3840` canvas
    /// pixels per normalized unit — the same number as the x density here,
    /// which is what makes a 16:9 canvas the easy case to check by hand.
    fn canvas_16_9() -> CanvasConfig {
        CanvasConfig {
            aspect: 16.0 / 9.0,
            render_width: 3840,
        }
    }

    fn output(name: &str, width: i32, height: i32) -> OutputConfig {
        let mut output = OutputConfig::new(OutputMatch::by_name(name));
        output.mode = Some(Mode {
            width,
            height,
            refresh_hz: 60.0,
        });
        output
    }

    /// The mode raster, unrotated: exactly what slice 2's lookup hands the
    /// solver. The 90/270 swap is the solver's own job.
    fn mode_raster(output: &OutputConfig) -> Option<(u32, u32)> {
        let mode = output.effective_mode()?;
        Some((
            u32::try_from(mode.width).ok()?,
            u32::try_from(mode.height).ok()?,
        ))
    }

    fn hd_outputs(count: usize) -> Vec<OutputConfig> {
        (0..count)
            .map(|index| output(&format!("HDMI-{}", index + 1), 1920, 1080))
            .collect()
    }

    /// `allowUnusedCanvas: true`, so every round-1 test below keeps the
    /// fit-inside `max(fitX, fitY)` scale and the band it was written against
    /// verbatim: the default `false` would instead cover the canvas and
    /// overhang it, turning every one-seamed-axis "centered band" test into
    /// an overhang test. Tests of the default rule use [`exact_request`], and
    /// tests of the coverage gate flip the field back to `false` on the
    /// value this returns.
    fn request(
        rows: u32,
        columns: u32,
        overlap_x: Option<f64>,
        overlap_y: Option<f64>,
        content_scale: Option<f64>,
    ) -> ArrangementRequest {
        ArrangementRequest {
            rows,
            columns,
            overlap_x,
            overlap_y,
            content_scale,
            allow_unused_canvas: true,
            committed: false,
        }
    }

    /// An overlap request with the API's own default,
    /// `allowUnusedCanvas: false`: the exact-overlap, cover-the-canvas rule
    /// (`scale = min(fitX, fitY)`, overhanging the other axis) that every
    /// client gets unless it asks for a band.
    fn exact_request(
        rows: u32,
        columns: u32,
        overlap_x: f64,
        overlap_y: f64,
    ) -> ArrangementRequest {
        let mut asked = request(rows, columns, Some(overlap_x), Some(overlap_y), None);
        asked.allow_unused_canvas = false;
        asked
    }

    fn document(enabled: usize) -> DesiredState {
        let mut document = DesiredState::new();
        document.outputs = hd_outputs(enabled);
        document.projection = Some(ProjectionConfig {
            canvas: Some(canvas_16_9()),
            ..ProjectionConfig::default()
        });
        document
    }

    #[track_caller]
    fn close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1.0e-12,
            "expected {expected}, got {actual}"
        );
    }

    #[track_caller]
    fn close_rect(actual: CanvasRect, expected: [f64; 4]) {
        close(actual.x, expected[0]);
        close(actual.y, expected[1]);
        close(actual.width, expected[2]);
        close(actual.height, expected[3]);
    }

    /// The case the JavaScript solver was driven with: 2x2 of 1920x1080 on a
    /// 3840x2160 canvas at 20% overlap on both axes.
    ///
    /// colWidth = [1920, 1920] so sumW = 3840 and divW = min(1920, 1920) = 1920;
    /// rowHeight = [1080, 1080] so sumH = 2160 and divH = 1080.
    /// sx = (3840 − 0.2·1920)/3840 = 3456/3840 = 0.9
    /// sy = (2160 − 0.2·1080)/2160 = 1944/2160 = 0.9, so max and min agree
    /// and the canvas fills on both axes: unused = 0, scale = 0.9.
    /// x origins: 0 and (1920 − 384)/0.9 = 1706.666… px → /3840 = 4/9.
    /// width = 1920/(0.9·3840) = 1920/3456 = 5/9, and 4/9 + 5/9 = 1.
    /// y origins: 0 and (1080 − 216)/0.9 = 960 px → /3840 = 0.25.
    /// height = 1080/3456 = 0.3125, and 0.25 + 0.3125 = 0.5625 = 9/16 = 1/aspect.
    #[test]
    fn two_by_two_at_twenty_percent_reproduces_the_legacy_result() {
        let outputs = hd_outputs(4);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, Some(0.2), Some(0.2), None),
            mode_raster,
        )
        .unwrap();

        close(solution.arrangement.content_scale, 0.9);
        close(solution.unused_canvas.x, 0.0);
        close(solution.unused_canvas.y, 0.0);
        close(solution.overhang.x, 0.0);
        close(solution.overhang.y, 0.0);
        close(solution.implied_aspect, 16.0 / 9.0);
        assert!(solution.warnings.is_empty(), "{:?}", solution.warnings);

        let keys: Vec<&str> = solution
            .outputs
            .iter()
            .map(|arranged| arranged.key.as_str())
            .collect();
        assert_eq!(keys, ["HDMI-1", "HDMI-2", "HDMI-3", "HDMI-4"]);
        close_rect(solution.outputs[0].source, [0.0, 0.0, 5.0 / 9.0, 0.3125]);
        close_rect(
            solution.outputs[1].source,
            [4.0 / 9.0, 0.0, 5.0 / 9.0, 0.3125],
        );
        close_rect(solution.outputs[2].source, [0.0, 0.25, 5.0 / 9.0, 0.3125]);
        close_rect(
            solution.outputs[3].source,
            [4.0 / 9.0, 0.25, 5.0 / 9.0, 0.3125],
        );
    }

    /// The requested overlaps are exact: the axis whose fit is larger no
    /// longer grows its overlap to close its slack, it overhangs the canvas
    /// instead, centered, and its overlap comes back exactly as asked.
    ///
    /// fit_x = (3840 − 0.2·1920)/3840 = 3456/3840 = 0.9
    /// fit_y = (2160 − 0.05·1080)/2160 = 2106/2160 = 0.975
    /// scale = min(fit_x, fit_y) = 0.9, so the columns fill exactly and are
    /// laid out as in the 20%/20% legacy case: x origins 0 and 4/9, width
    /// 1920/(0.9·3840) = 5/9.
    /// The rows span 2106/0.9 = 2340 canvas px against a 2160 px canvas:
    /// slack = 2160 − 2340 = −180 px, so overhang.y = 180/2160 = 1/12
    /// (equivalently fit_y/scale − 1 = 0.975/0.9 − 1), and unused is 0 on
    /// both axes.
    /// Row origins: −180/2 = −90 px → −90/3840 = −0.0234375, then
    /// −90 + (1080 − 0.05·1080)/0.9 = −90 + 1026/0.9 = −90 + 1140 = 1050 px
    /// → 1050/3840 = 0.2734375. Height = 1080/(0.9·3840) = 0.3125, so the
    /// bottom row ends at 0.2734375 + 0.3125 = 0.5859375, which is the same
    /// 0.0234375 past the canvas bottom (0.5625) as the top row starts above
    /// the canvas top: centered.
    #[test]
    fn a_slack_axis_overhangs_instead_of_growing_its_overlap() {
        let outputs = hd_outputs(4);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &exact_request(2, 2, 0.2, 0.05),
            mode_raster,
        )
        .unwrap();

        assert_eq!(solution.arrangement.overlap_x, 0.2);
        assert_eq!(solution.arrangement.overlap_y, 0.05);
        close(solution.arrangement.content_scale, 0.9);
        close(solution.unused_canvas.x, 0.0);
        close(solution.unused_canvas.y, 0.0);
        close(solution.overhang.x, 0.0);
        close(solution.overhang.y, 1.0 / 12.0);
        // impliedAspect is the requested extents' ratio, 3456/2106: the
        // aspect at which neither axis would overhang.
        close(solution.implied_aspect, 3456.0 / 2106.0);
        assert!(solution.warnings.is_empty(), "{:?}", solution.warnings);

        close_rect(
            solution.outputs[0].source,
            [0.0, -0.0234375, 5.0 / 9.0, 0.3125],
        );
        close_rect(
            solution.outputs[1].source,
            [4.0 / 9.0, -0.0234375, 5.0 / 9.0, 0.3125],
        );
        close_rect(
            solution.outputs[2].source,
            [0.0, 0.2734375, 5.0 / 9.0, 0.3125],
        );
        close_rect(
            solution.outputs[3].source,
            [4.0 / 9.0, 0.2734375, 5.0 / 9.0, 0.3125],
        );
    }

    /// The product contract the exact rule keeps: whatever the overlaps,
    /// the union of source rectangles covers the whole canvas — no gap
    /// between neighbors, and every canvas edge reached or passed.
    #[test]
    fn exact_overlaps_still_cover_the_whole_canvas() {
        let outputs = hd_outputs(4);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &exact_request(2, 2, 0.2, 0.05),
            mode_raster,
        )
        .unwrap();
        let eps = 1.0e-9;
        let rects: Vec<CanvasRect> = solution
            .outputs
            .iter()
            .map(|output| output.source)
            .collect();

        // The grid's left edge sits at or before the canvas's left edge, and
        // the two columns overlap or touch rather than leaving a gap between
        // them.
        assert!(rects[0].x <= eps);
        assert!(rects[2].x <= eps);
        assert!(rects[0].x + rects[0].width >= rects[1].x - eps);
        assert!(rects[2].x + rects[2].width >= rects[3].x - eps);
        // The grid's right edge reaches the canvas's right edge (x == 1).
        assert!(rects[1].x + rects[1].width >= 1.0 - eps);
        assert!(rects[3].x + rects[3].width >= 1.0 - eps);

        // Likewise vertically: the top row starts at or above the canvas
        // top, the two rows overlap or touch, and the bottom row reaches the
        // canvas bottom, which is `1 / aspect` in normalized units.
        assert!(rects[0].y <= eps);
        assert!(rects[1].y <= eps);
        assert!(rects[0].y + rects[0].height >= rects[2].y - eps);
        assert!(rects[1].y + rects[1].height >= rects[3].y - eps);
        let bottom = 1.0 / (16.0 / 9.0);
        assert!(rects[2].y + rects[2].height >= bottom - eps);
        assert!(rects[3].y + rects[3].height >= bottom - eps);
    }

    /// At the reported `impliedAspect`, the requested overlaps fill both
    /// axes exactly: no overhang and no band.
    ///
    /// aspect = 3456/2106 with renderWidth 3840 gives height
    /// round(3840·2106/3456) = 2340, and then fit_y = 2106/2340 = 0.9 = fit_x,
    /// so scale = 0.9 and both axes fill at the requested (0.2, 0.05).
    #[test]
    fn implied_aspect_removes_the_overhang() {
        let outputs = hd_outputs(4);
        let asked = exact_request(2, 2, 0.2, 0.05);
        let first = solve(&outputs, &canvas_16_9(), &asked, mode_raster).unwrap();
        assert!(first.overhang.y > 0.0);

        let squarer = CanvasConfig {
            aspect: first.implied_aspect,
            render_width: 3840,
        };
        assert_eq!(squarer.dimensions().unwrap(), (3840, 2340));
        let second = solve(&outputs, &squarer, &asked, mode_raster).unwrap();

        assert_eq!(second.arrangement.overlap_x, 0.2);
        assert_eq!(second.arrangement.overlap_y, 0.05);
        close(second.unused_canvas.x, 0.0);
        close(second.unused_canvas.y, 0.0);
        close(second.overhang.x, 0.0);
        close(second.overhang.y, 0.0);
        close(second.arrangement.content_scale, 0.9);
        close(second.implied_aspect, first.implied_aspect);
    }

    /// Scale mode derives what `overlapsFromScale` did: ox = (sumW − s·Wc)/divW
    /// = (3840 − 0.9·3840)/1920 = 384/1920 = 0.2, and oy = (2160 − 0.9·2160)/1080
    /// = 216/1080 = 0.2 — the same arrangement as asking for 20% directly.
    #[test]
    fn scale_mode_derives_the_legacy_overlaps() {
        let outputs = hd_outputs(4);
        let from_scale = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, None, None, Some(0.9)),
            mode_raster,
        )
        .unwrap();
        let from_overlap = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, Some(0.2), Some(0.2), None),
            mode_raster,
        )
        .unwrap();

        close(from_scale.arrangement.overlap_x, 0.2);
        close(from_scale.arrangement.overlap_y, 0.2);
        close(from_scale.unused_canvas.x, 0.0);
        close(from_scale.unused_canvas.y, 0.0);
        assert_eq!(from_scale.outputs, from_overlap.outputs);
    }

    /// A single row has no horizontal seam of its own to fill the canvas
    /// with, so it is centered, exactly as the JavaScript's
    /// `yOrigins[0] = (Hc − rowHeight/scale)/2` did.
    ///
    /// sx = (3840 − 0.2·1920)/3840 = 0.9; divH = 0 so sy = 1080/2160 = 0.5;
    /// scale = max = 0.9. y extent = 1080/0.9 = 1200 px, leaving 960 px
    /// unused (960/2160 = 4/9), so the row starts at 480 px = 0.125 and ends
    /// at 0.125 + 0.3125 = 0.4375, which is 0.125 above the canvas bottom
    /// (0.5625).
    #[test]
    fn a_single_row_is_centered_vertically() {
        let outputs = hd_outputs(2);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &request(1, 2, Some(0.2), Some(0.2), None),
            mode_raster,
        )
        .unwrap();

        close(solution.arrangement.content_scale, 0.9);
        close(solution.unused_canvas.x, 0.0);
        close(solution.unused_canvas.y, 4.0 / 9.0);
        close_rect(solution.outputs[0].source, [0.0, 0.125, 5.0 / 9.0, 0.3125]);
        close_rect(
            solution.outputs[1].source,
            [4.0 / 9.0, 0.125, 5.0 / 9.0, 0.3125],
        );
    }

    /// The same single-row request under the default rule
    /// (`allowUnusedCanvas: false`): instead of leaving the 4/9 band above,
    /// the row covers the canvas's height and overhangs its width, centered.
    ///
    /// fit_x = (3840 − 0.2·1920)/3840 = 0.9; divH = 0, so the Y extent is a
    /// single row's 1080 and fit_y = 1080/2160 = 0.5; scale = min = 0.5.
    /// Y fills: 1080/0.5 = 2160 px. X spans 3456/0.5 = 6912 px against 3840:
    /// slack = −3072 px, overhang.x = 3072/3840 = 0.8.
    /// Each slice is 1920/(0.5·3840) = 1.0 wide and 1080/(0.5·3840) = 0.5625
    /// tall. Column origins: −3072/2 = −1536 px → −0.4, then
    /// −1536 + (1920 − 384)/0.5 = −1536 + 3072 = 1536 px → 0.4; the right
    /// slice ends at 1.4, the same 0.4 past the canvas's right edge as the
    /// left one starts before its left edge. impliedAspect is
    /// 3456/1080 = 3.2.
    #[test]
    fn a_single_row_overhangs_instead_of_leaving_a_band() {
        let outputs = hd_outputs(2);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &exact_request(1, 2, 0.2, 0.2),
            mode_raster,
        )
        .unwrap();

        assert_eq!(solution.arrangement.overlap_x, 0.2);
        assert_eq!(solution.arrangement.overlap_y, 0.2);
        close(solution.arrangement.content_scale, 0.5);
        close(solution.unused_canvas.x, 0.0);
        close(solution.unused_canvas.y, 0.0);
        close(solution.overhang.x, 0.8);
        close(solution.overhang.y, 0.0);
        close(solution.implied_aspect, 3.2);
        close_rect(solution.outputs[0].source, [-0.4, 0.0, 1.0, 0.5625]);
        close_rect(solution.outputs[1].source, [0.4, 0.0, 1.0, 0.5625]);
    }

    /// The coverage gate still guards content-scale mode, where an unseamed
    /// axis can fall short of the canvas at the asked-for scale.
    ///
    /// One row of two at contentScale 0.9: ox = (3840 − 0.9·3840)/1920 = 0.2,
    /// so X fills; the single row is 1080/0.9 = 1200 px tall against 2160,
    /// leaving 960/2160 = 4/9 (44.4%) of Y uncovered. impliedAspect is
    /// effectiveW/effectiveH = 3456/1080 = 3.2.
    #[test]
    fn a_content_scale_band_is_refused_without_allow_unused_canvas() {
        let outputs = hd_outputs(2);
        let mut asked = request(1, 2, None, None, Some(0.9));
        asked.allow_unused_canvas = false;
        let error = solve(&outputs, &canvas_16_9(), &asked, mode_raster).unwrap_err();
        assert!(error.contains('Y'), "{error}");
        assert!(error.contains("44.4%"), "{error}");
        assert!(error.contains("3.2"), "{error}");
        assert!(error.contains("allowUnusedCanvas"), "{error}");
    }

    /// Content-scale mode's overhang on an unseamed axis is now centered,
    /// like every other overhang, rather than pinned to the canvas's first
    /// edge as the old clamped slack left it.
    ///
    /// One row of two portrait 1080x1920 outputs at contentScale 0.5:
    /// colWidth = [1080, 1080], sumW = 2160, divW = 1080, so
    /// ox = (2160 − 0.5·3840)/1080 = 240/1080 = 2/9 and X fills exactly.
    /// Y has no seam: the row is 1920/0.5 = 3840 px tall against 2160,
    /// slack = −1680 px, overhang.y = 1680/2160 = 7/9, and the row starts
    /// at −840 px → −840/3840 = −0.21875 (it used to start at 0).
    /// Width = 1080/(0.5·3840) = 0.5625, height = 1920/1920 = 1.0; the
    /// second column starts at (1080 − 240)/0.5 = 1680 px → 0.4375, and
    /// 0.4375 + 0.5625 = 1. The row ends at −0.21875 + 1 = 0.78125, the same
    /// 0.21875 past the canvas bottom (0.5625) as it starts above the top.
    #[test]
    fn a_content_scale_overhang_is_centered() {
        let outputs: Vec<OutputConfig> = (1..=2)
            .map(|index| output(&format!("HDMI-{index}"), 1080, 1920))
            .collect();
        let mut asked = request(1, 2, None, None, Some(0.5));
        asked.allow_unused_canvas = false;
        let solution = solve(&outputs, &canvas_16_9(), &asked, mode_raster).unwrap();

        close(solution.arrangement.overlap_x, 2.0 / 9.0);
        close(solution.unused_canvas.x, 0.0);
        close(solution.unused_canvas.y, 0.0);
        close(solution.overhang.x, 0.0);
        close(solution.overhang.y, 7.0 / 9.0);
        close_rect(solution.outputs[0].source, [0.0, -0.21875, 0.5625, 1.0]);
        close_rect(solution.outputs[1].source, [0.4375, -0.21875, 0.5625, 1.0]);
    }

    /// The mirror case: divW = 0, so sx = 1920/3840 = 0.5, sy = 0.9 and the
    /// column is centered in 3840 − 1920/0.9 = 1706.66… px of slack, half of
    /// it (853.33… px = 2/9 of the canvas) on the left.
    #[test]
    fn a_single_column_is_centered_horizontally() {
        let outputs = hd_outputs(2);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 1, Some(0.2), Some(0.2), None),
            mode_raster,
        )
        .unwrap();

        close(solution.arrangement.content_scale, 0.9);
        close(solution.unused_canvas.x, 4.0 / 9.0);
        close(solution.unused_canvas.y, 0.0);
        close_rect(
            solution.outputs[0].source,
            [2.0 / 9.0, 0.0, 5.0 / 9.0, 0.3125],
        );
        close_rect(
            solution.outputs[1].source,
            [2.0 / 9.0, 0.25, 5.0 / 9.0, 0.3125],
        );
    }

    /// A 90-degree transform swaps the raster the grid is measured in.
    ///
    /// Portrait 1080x1920 beside landscape 1920x1080, one row, no overlap:
    /// colWidth = [1080, 1920] so sumW = 3000; rowHeight = [1920] so
    /// sumH = 1920 and divH = 0. sx = 3000/3840 = 0.78125,
    /// sy = 1920/2160 = 8/9 = 0.888…, scale = 8/9.
    /// x extent = 3000·9/8 = 3375 px, leaving 465 px unused and 232.5 px on
    /// the left: 232.5/3840 = 0.060546875.
    /// widths: 1080·9/(8·3840) = 0.31640625 and 1920·9/30720 = 0.5625;
    /// second origin = 232.5 + 1080·9/8 = 1447.5 px → 0.376953125, and
    /// 0.376953125 + 0.5625 = 0.939453125 = 1 − 0.060546875.
    #[test]
    fn a_rotated_output_swaps_its_raster() {
        let mut outputs = hd_outputs(2);
        outputs[0].transform = Some(Transform::Rotate90);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &request(1, 2, Some(0.0), Some(0.0), None),
            mode_raster,
        )
        .unwrap();

        close(solution.arrangement.content_scale, 8.0 / 9.0);
        close(solution.unused_canvas.x, 465.0 / 3840.0);
        close(solution.unused_canvas.y, 0.0);
        close_rect(
            solution.outputs[0].source,
            [0.060546875, 0.0, 0.31640625, 0.5625],
        );
        close_rect(
            solution.outputs[1].source,
            [0.376953125, 0.0, 0.5625, 0.31640625],
        );
    }

    #[test]
    fn too_many_outputs_for_the_grid_are_refused() {
        let outputs = hd_outputs(5);
        let error = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, Some(0.2), Some(0.2), None),
            mode_raster,
        )
        .unwrap_err();
        assert!(error.contains("do not fit a 2x2 grid"), "{error}");
    }

    #[test]
    fn a_missing_raster_names_the_output() {
        let mut outputs = hd_outputs(2);
        outputs[1].mode = None;
        let error = solve(
            &outputs,
            &canvas_16_9(),
            &request(1, 2, Some(0.2), Some(0.0), None),
            mode_raster,
        )
        .unwrap_err();
        assert_eq!(error, "output HDMI-2 has no known raster size");
    }

    #[test]
    fn a_missing_canvas_is_refused() {
        let mut state = document(4);
        state.projection.as_mut().unwrap().canvas = None;
        let error = solve_document(
            &state,
            &request(2, 2, Some(0.2), Some(0.2), None),
            mode_raster,
        )
        .unwrap_err();
        assert_eq!(error, "projection.canvas is required to arrange outputs");
    }

    #[test]
    fn asking_for_overlap_and_scale_together_is_over_determined() {
        let outputs = hd_outputs(4);
        let error = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, Some(0.2), None, Some(0.9)),
            mode_raster,
        )
        .unwrap_err();
        assert!(error.contains("over-determined"), "{error}");
    }

    #[test]
    fn an_overlap_of_one_is_refused() {
        let outputs = hd_outputs(4);
        let error = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, Some(1.0), Some(0.2), None),
            mode_raster,
        )
        .unwrap_err();
        assert_eq!(error, "overlapX must be at least 0 and less than 1, not 1");
    }

    /// An aspect far from the outputs' own proportions used to demand more
    /// overlap on the slack axis than a seam had raster left to give, and
    /// was refused. Under the exact rule it just overhangs — heavily — and
    /// with only two rows no slice can fall off the canvas, since both rows
    /// always contain the canvas's center line.
    ///
    /// Canvas 3840x480 (aspect 8.0; y density = 8·480 = 3840):
    /// fit_x = (3840 − 0.2·1920)/3840 = 3456/3840 = 0.9;
    /// fit_y = (2160 − 0.2·1080)/480 = 1944/480 = 4.05; scale = min = 0.9.
    /// Y spans 1944/0.9 = 2160 px against 480: slack = −1680 px,
    /// overhang.y = 1680/480 = 3.5. Row origins: −840 px → −840/3840
    /// = −0.21875, then −840 + (1080 − 216)/0.9 = −840 + 960 = 120 px
    /// → 0.03125. Each row is 1080/(0.9·3840) = 0.3125 tall, so the top row
    /// shows 0 to 0.09375 of the canvas's 0 to 0.125 and the bottom row
    /// 0.03125 to 0.125. impliedAspect is the outputs' own 16:9 grid
    /// proportion, 3456/1944 = 16/9.
    #[test]
    fn an_extreme_aspect_overhangs_rather_than_growing_the_overlap() {
        let outputs = hd_outputs(4);
        let canvas = CanvasConfig {
            aspect: 8.0,
            render_width: 3840,
        };
        assert_eq!(canvas.dimensions().unwrap(), (3840, 480));
        let solution = solve(
            &outputs,
            &canvas,
            &exact_request(2, 2, 0.2, 0.2),
            mode_raster,
        )
        .unwrap();

        assert_eq!(solution.arrangement.overlap_x, 0.2);
        assert_eq!(solution.arrangement.overlap_y, 0.2);
        close(solution.arrangement.content_scale, 0.9);
        close(solution.unused_canvas.x, 0.0);
        close(solution.unused_canvas.y, 0.0);
        close(solution.overhang.x, 0.0);
        close(solution.overhang.y, 3.5);
        close(solution.implied_aspect, 16.0 / 9.0);
        close_rect(
            solution.outputs[0].source,
            [0.0, -0.21875, 5.0 / 9.0, 0.3125],
        );
        close_rect(
            solution.outputs[3].source,
            [4.0 / 9.0, 0.03125, 5.0 / 9.0, 0.3125],
        );
    }

    /// With three rows, a big enough overhang swallows an edge row whole,
    /// which is refused, naming the first output it happens to.
    ///
    /// 3x2 of 1920x1080 on 3840x1440 (aspect 8/3; y density = (8/3)·1440
    /// = 3840) at (0.6, 0): fit_x = (3840 − 0.6·1920)/3840 = 2688/3840 = 0.7;
    /// fit_y = 3240/1440 = 2.25; scale = min = 0.7. Y spans 3240/0.7
    /// = 4628.57… px against 1440, so the top row starts at
    /// (1440 − 3240/0.7)/2 = 720 − 1620/0.7 px and ends 1080/0.7 px later,
    /// at 720 − 540/0.7 = 720 − 771.43… < 0: above the canvas entirely.
    /// impliedAspect = 2688/3240 = 0.8296….
    #[test]
    fn a_slice_pushed_entirely_off_the_canvas_is_refused() {
        let outputs = hd_outputs(6);
        let canvas = CanvasConfig {
            aspect: 8.0 / 3.0,
            render_width: 3840,
        };
        assert_eq!(canvas.dimensions().unwrap(), (3840, 1440));
        let error = solve(
            &outputs,
            &canvas,
            &exact_request(3, 2, 0.6, 0.0),
            mode_raster,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "output HDMI-1's slice falls entirely outside the canvas at these overlaps; the \
             canvas aspect that fits these overlaps exactly is 0.8296"
        );
    }

    /// The off-canvas gate is on the edge rows themselves, not the grid: a
    /// row whose only contact with the canvas is its edge is as lost as one
    /// clear of it.
    ///
    /// One row of three 1920x1080 at no overlap on 16:9: fit_x = 5760/3840
    /// = 1.5, fit_y = 1080/2160 = 0.5, scale = 0.5, so each column is
    /// 1920/0.5 = 3840 px — the whole canvas width — and X spans 11520 px:
    /// slack = −7680 px, the first column starts at −3840 px and ends at
    /// exactly 0. It touches the canvas's left edge and shows nothing of it.
    #[test]
    fn a_slice_only_touching_the_canvas_edge_is_refused() {
        let outputs = hd_outputs(3);
        let error = solve(
            &outputs,
            &canvas_16_9(),
            &exact_request(1, 3, 0.0, 0.0),
            mode_raster,
        )
        .unwrap_err();
        assert!(
            error.starts_with("output HDMI-1's slice falls entirely outside the canvas"),
            "{error}"
        );
        // implied aspect = 5760/1080 = 5.3333…
        assert!(error.contains("5.3333"), "{error}");
    }

    /// The same extreme 8:1 aspect with `allowUnusedCanvas: true`: the
    /// requested overlaps (unchanged, as always) at
    /// `scale = max(fit_x, fit_y) = 4.05` — the smallest scale that keeps
    /// every source inside the canvas — with the resulting band reported.
    ///
    /// unused_x = 1 − fit_x/scale = 1 − 0.9/4.05 = 1 − 2/9 = 7/9: column x has
    /// the smaller fit, so it is the axis left short. unused_y = 1 −
    /// fit_y/scale = 1 − 4.05/4.05 = 0: row y set the scale, so it fills
    /// exactly. Nothing overhangs.
    #[test]
    fn allowing_a_band_fits_the_grid_inside_the_canvas_at_the_extreme_aspect() {
        let outputs = hd_outputs(4);
        let canvas = CanvasConfig {
            aspect: 8.0,
            render_width: 3840,
        };
        let solution = solve(
            &outputs,
            &canvas,
            &request(2, 2, Some(0.2), Some(0.2), None),
            mode_raster,
        )
        .unwrap();
        close(solution.arrangement.overlap_x, 0.2);
        close(solution.arrangement.overlap_y, 0.2);
        close(solution.arrangement.content_scale, 4.05);
        close(solution.unused_canvas.x, 7.0 / 9.0);
        close(solution.unused_canvas.y, 0.0);
        close(solution.overhang.x, 0.0);
        close(solution.overhang.y, 0.0);
        close(solution.implied_aspect, 16.0 / 9.0);
    }

    /// At scale 0.5 the derived overlap is (3840 − 0.5·3840)/1920 = 1.0, which
    /// is a column with nothing of its own left.
    #[test]
    fn a_content_scale_below_the_grid_is_refused() {
        let outputs = hd_outputs(4);
        let error = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, None, None, Some(0.5)),
            mode_raster,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "content scale is too small for this grid: overlap cannot reach 100%"
        );
    }

    /// No overlap and no scale is the zero-overlap arrangement, not an error.
    #[test]
    fn neither_overlap_nor_scale_means_no_overlap() {
        let outputs = hd_outputs(4);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, None, None, None),
            mode_raster,
        )
        .unwrap();
        close(solution.arrangement.overlap_x, 0.0);
        close(solution.arrangement.overlap_y, 0.0);
        // Edge to edge: 2·1920 = 3840 = the canvas width, so scale is 1.0.
        close(solution.arrangement.content_scale, 1.0);
    }

    /// Warnings are about the requested overlaps, which are now also the
    /// resolved ones: overlapX of 90% warns, and overlapY stays the 0% that
    /// was asked for — it no longer grows (to (2160 − 0.55·2160)/1080 = 0.9
    /// under the old floors rule) into a second warning. The mismatch goes
    /// into overhang instead: fit_x = (3840 − 0.9·1920)/3840 = 0.55,
    /// fit_y = 2160/2160 = 1.0, scale = min = 0.55, and
    /// overhang.y = fit_y/scale − 1 = 1/0.55 − 1 = 9/11.
    #[test]
    fn a_very_high_overlap_warns() {
        let outputs = hd_outputs(4);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &exact_request(2, 2, 0.9, 0.0),
            mode_raster,
        )
        .unwrap();
        assert_eq!(solution.arrangement.overlap_y, 0.0);
        close(solution.overhang.y, 9.0 / 11.0);
        assert_eq!(solution.warnings.len(), 1, "{:?}", solution.warnings);
        assert!(
            solution.warnings[0].contains("overlapX"),
            "{:?}",
            solution.warnings
        );
    }

    #[test]
    fn apply_replaces_sources_and_keeps_calibration() {
        let mut state = document(4);
        // One output is already calibrated; only its source may change.
        let corners = [[0.01, 0.02], [0.99, 0.0], [1.0, 0.98], [0.0, 1.0]];
        state.outputs[1].geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.9,
                y: 0.1,
                width: 0.1,
                height: 0.1,
            },
            corners,
            center: [0.4, 0.6],
            raster_footprint: CanvasRect {
                x: 0.5,
                y: 0.25,
                width: 0.5,
                height: 0.3,
            },
        });
        // A disabled output takes no part and is left exactly as it was.
        let mut spare = output("HDMI-9", 1920, 1080);
        spare.enable = false;
        state.outputs.push(spare.clone());

        let solution = solve_document(
            &state,
            &request(2, 2, Some(0.2), Some(0.2), None),
            mode_raster,
        )
        .unwrap();
        apply(&mut state, &solution);

        assert_eq!(state.outputs[4], spare);
        assert_eq!(
            state.projection.as_ref().unwrap().arrangement,
            Some(solution.arrangement)
        );
        let calibrated = state.outputs[1].geometry.as_ref().unwrap();
        assert_eq!(calibrated.corners, corners);
        assert_eq!(calibrated.center, [0.4, 0.6]);
        close(calibrated.raster_footprint.x, 0.5);
        close_rect(calibrated.source, [4.0 / 9.0, 0.0, 5.0 / 9.0, 0.3125]);
        // The rest were seeded with identity geometry.
        let seeded = state.outputs[0].geometry.as_ref().unwrap();
        assert_eq!(seeded.corners, UNIT_CORNERS);
        assert_eq!(seeded.center, [0.5, 0.5]);
        assert_eq!(seeded.raster_footprint, seeded.source);
        close_rect(seeded.source, [0.0, 0.0, 5.0 / 9.0, 0.3125]);

        state.validate(true).unwrap();
    }

    #[test]
    fn in_effect_follows_the_sources() {
        let mut state = document(4);
        assert!(!in_effect(&state, mode_raster));

        let solution = solve_document(
            &state,
            &request(2, 2, Some(0.2), Some(0.05), None),
            mode_raster,
        )
        .unwrap();
        apply(&mut state, &solution);
        assert!(in_effect(&state, mode_raster));

        // Well under the 1e-9 tolerance: a JSON round trip must not flip it.
        state.outputs[0].geometry.as_mut().unwrap().source.x += 1.0e-12;
        assert!(in_effect(&state, mode_raster));

        // A real nudge is a real difference.
        state.outputs[2].geometry.as_mut().unwrap().source.y += 1.0e-6;
        assert!(!in_effect(&state, mode_raster));
    }

    /// `arrangeOffset` shifts exactly the output that carries it, applied
    /// after placement: every other output's source, and the coverage
    /// report, come out identical to the same request with no offset at all.
    #[test]
    fn arrange_offset_shifts_exactly_that_outputs_source() {
        let plain = hd_outputs(4);
        let mut offset_outputs = plain.clone();
        offset_outputs[1].arrange_offset = Some(ArrangeOffset { x: 0.01, y: -0.02 });

        let asked = request(2, 2, Some(0.2), Some(0.05), None);
        let base = solve(&plain, &canvas_16_9(), &asked, mode_raster).unwrap();
        let offset = solve(&offset_outputs, &canvas_16_9(), &asked, mode_raster).unwrap();

        close(offset.unused_canvas.x, base.unused_canvas.x);
        close(offset.unused_canvas.y, base.unused_canvas.y);

        assert_eq!(offset.outputs[0].source, base.outputs[0].source);
        assert_eq!(offset.outputs[2].source, base.outputs[2].source);
        assert_eq!(offset.outputs[3].source, base.outputs[3].source);

        close(offset.outputs[1].source.x, base.outputs[1].source.x + 0.01);
        close(offset.outputs[1].source.y, base.outputs[1].source.y - 0.02);
        close(offset.outputs[1].source.width, base.outputs[1].source.width);
        close(
            offset.outputs[1].source.height,
            base.outputs[1].source.height,
        );
    }

    /// `inEffect` applies the document's *current* `arrangeOffset` before
    /// comparing, so an arrangement applied with an offset in place reads as
    /// in effect; editing the offset afterward — without touching the source
    /// at all — is exactly the manual-geometry-edit case `inEffect` exists to
    /// catch, so it turns `false`.
    #[test]
    fn in_effect_applies_the_current_offset_before_comparing() {
        let mut state = document(4);
        state.outputs[1].arrange_offset = Some(ArrangeOffset { x: 0.01, y: -0.02 });

        let solution = solve_document(
            &state,
            &request(2, 2, Some(0.2), Some(0.05), None),
            mode_raster,
        )
        .unwrap();
        apply(&mut state, &solution);
        assert!(in_effect(&state, mode_raster));

        state.outputs[1].arrange_offset = Some(ArrangeOffset { x: 0.02, y: -0.02 });
        assert!(!in_effect(&state, mode_raster));
    }

    #[test]
    fn the_record_is_camel_case_and_refuses_unknown_fields() {
        let arrangement = Arrangement {
            rows: 2,
            columns: 2,
            overlap_x: 0.2,
            overlap_y: 0.05,
            content_scale: 0.975,
        };
        let value = serde_json::to_value(arrangement).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "rows": 2,
                "columns": 2,
                "overlapX": 0.2,
                "overlapY": 0.05,
                "contentScale": 0.975
            })
        );
        assert_eq!(
            serde_json::from_value::<Arrangement>(value.clone()).unwrap(),
            arrangement
        );
        let mut extra = value;
        extra["overlap"] = serde_json::json!(0.2);
        assert!(serde_json::from_value::<Arrangement>(extra).is_err());
    }

    #[test]
    fn an_out_of_range_record_fails_document_validation() {
        let mut state = document(1);
        state.outputs[0].geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 0.5625,
            },
            corners: UNIT_CORNERS,
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 0.5625,
            },
        });
        state.validate(true).unwrap();

        state.projection.as_mut().unwrap().arrangement = Some(Arrangement {
            rows: 0,
            columns: 2,
            overlap_x: 1.0,
            overlap_y: -0.1,
            content_scale: 0.0,
        });
        let errors = state.validate(true).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error == "projection.arrangement rows and columns must be at least 1"),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|error| error
                .starts_with("projection.arrangement.overlapX must be at least 0 and less than 1")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|error| error
                .starts_with("projection.arrangement.overlapY must be at least 0 and less than 1")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.starts_with("projection.arrangement.contentScale")),
            "{errors:?}"
        );
    }

    #[test]
    fn arrange_offset_out_of_range_fails_document_validation() {
        let mut state = document(1);
        state.outputs[0].geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 0.5625,
            },
            corners: UNIT_CORNERS,
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 0.5625,
            },
        });
        state.validate(true).unwrap();

        // Non-finite on one axis, oversized on the other: both are caught.
        state.outputs[0].arrange_offset = Some(ArrangeOffset {
            x: f64::NAN,
            y: 17.0,
        });
        let errors = state.validate(true).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.starts_with("outputs[0].arrangeOffset.x")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.starts_with("outputs[0].arrangeOffset.y")),
            "{errors:?}"
        );

        // Exactly at the bound is fine; the record is a floor, not a
        // strictly-less-than limit.
        state.outputs[0].arrange_offset = Some(ArrangeOffset { x: -16.0, y: 16.0 });
        state.validate(true).unwrap();
    }

    #[test]
    fn arrange_offset_is_camel_case_and_optional() {
        let mut output = OutputConfig::new(OutputMatch::by_name("HDMI-1"));
        output.arrange_offset = Some(ArrangeOffset { x: 0.25, y: -0.5 });
        let value = serde_json::to_value(&output).unwrap();
        assert_eq!(
            value["arrangeOffset"],
            serde_json::json!({ "x": 0.25, "y": -0.5 })
        );
        let round_tripped: OutputConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(round_tripped, output);

        // A document written before this field existed has no `arrangeOffset`
        // key at all, and must keep parsing exactly as before.
        let mut without = value;
        without.as_object_mut().unwrap().remove("arrangeOffset");
        let parsed: OutputConfig = serde_json::from_value(without).unwrap();
        assert_eq!(parsed.arrange_offset, None);
    }

    /// A document arranged before the exact rule — the 20%/20% legacy
    /// record at scale 0.9, with the legacy sources from
    /// [`two_by_two_at_twenty_percent_reproduces_the_legacy_result`] written
    /// by hand rather than by this solver — still reads as in effect: its
    /// slack is zero on both axes, so centering the signed slack changes
    /// nothing about where `place` lays it out.
    #[test]
    fn in_effect_still_recognizes_a_previously_arranged_document() {
        let mut state = document(4);
        let legacy = [[0.0, 0.0], [4.0 / 9.0, 0.0], [0.0, 0.25], [4.0 / 9.0, 0.25]];
        for (output, [x, y]) in state.outputs.iter_mut().zip(legacy) {
            let source = CanvasRect {
                x,
                y,
                width: 5.0 / 9.0,
                height: 0.3125,
            };
            output.geometry = Some(OutputGeometry {
                source,
                corners: UNIT_CORNERS,
                center: [0.5, 0.5],
                raster_footprint: source,
            });
        }
        state.projection.as_mut().unwrap().arrangement = Some(Arrangement {
            rows: 2,
            columns: 2,
            overlap_x: 0.2,
            overlap_y: 0.2,
            content_scale: 0.9,
        });
        assert!(in_effect(&state, mode_raster));
    }

    /// On the 16:9 2x2 rig every overlap in `[0, 1)` is valid on both axes:
    /// with two rows and two columns, whichever axis overhangs still has
    /// both of its slices straddling the canvas's center line, so neither
    /// can fall off.
    ///
    /// Scanning up from the requested 0.2, every sample through 63/64 is
    /// valid and the sample at 1 is refused by the exclusive bound, so the
    /// bracket [63/64, 1] is bisected. Every midpoint is valid, and eight
    /// halvings take the bracket from 1/64 to 1/16384 (≤ 1e-4, where 1/8192
    /// is not), leaving the valid end at 1 − 1/16384. Scanning down reaches
    /// 0 with every sample valid, so the minimum is exactly 0. All these
    /// values are dyadic, so they compare exactly.
    #[test]
    fn limits_on_the_two_by_two_rig_span_the_whole_range() {
        let limits = limits(
            &hd_outputs(4),
            &canvas_16_9(),
            &exact_request(2, 2, 0.2, 0.2),
            mode_raster,
        )
        .unwrap();
        let whole = AxisLimits {
            min: 0.0,
            max: 1.0 - 1.0 / 16384.0,
            has_seam: true,
        };
        assert_eq!(limits.overlap_x, whole);
        assert_eq!(limits.overlap_y, whole);
    }

    /// A single row has no Y seam: `overlapY` is inert, reports
    /// `hasSeam: false`, and spans the whole range, since no value of it
    /// changes anything. `overlapX` spans the whole range too: at
    /// fit_y = 1080/2160 = 0.5 the scale is 0.5 for every overlapX (fit_x
    /// = 1 − overlapX/2 never drops below 0.5), and two overhanging columns
    /// both straddle the canvas's center line. The same bisection toward 1
    /// as on the 2x2 rig gives 1 − 1/16384 on both axes.
    #[test]
    fn a_single_row_reports_an_inert_y_axis() {
        let limits = limits(
            &hd_outputs(2),
            &canvas_16_9(),
            &exact_request(1, 2, 0.2, 0.2),
            mode_raster,
        )
        .unwrap();
        assert_eq!(
            limits.overlap_x,
            AxisLimits {
                min: 0.0,
                max: 1.0 - 1.0 / 16384.0,
                has_seam: true,
            }
        );
        assert_eq!(
            limits.overlap_y,
            AxisLimits {
                min: 0.0,
                max: 1.0 - 1.0 / 16384.0,
                has_seam: false,
            }
        );
    }

    /// An extreme aspect tightens the maximum: more overlapX shrinks the
    /// scale, which grows the Y overhang until the top and bottom of three
    /// rows fall off the canvas.
    ///
    /// 3x2 of 1920x1080 on 3840x1440 (aspect 8/3) holding overlapY at 0:
    /// fit_x = (3840 − 1920·ox)/3840 = 1 − ox/2 ≤ 1 and fit_y = 3240/1440
    /// = 2.25, so the scale is s = 1 − ox/2 and X fills. Y spans 3240/s px,
    /// so the top row starts at (1440 − 3240/s)/2 = 720 − 1620/s px and ends
    /// 1080/s px later, at 720 − 540/s: on the canvas iff s > 540/720 = 0.75,
    /// i.e. ox < 0.5. The scan sample 32/64 = 0.5 is the first invalid one
    /// (the row's bottom lands exactly on the canvas's top edge), so the
    /// bracket [31/64, 32/64] is bisected eight times; every midpoint is
    /// below 0.5 and valid, leaving max = 0.5 − 1/16384.
    ///
    /// overlapY holding overlapX at 0.2 stays whole: s = min(0.9, fit_y)
    /// and, while fit_y > 0.9 (oy < 0.9), the top row ends at
    /// 720 − (540 − 1080·oy)/0.9 = 120 + 1200·oy px > 0; above that Y fills
    /// and two columns overhang harmlessly.
    #[test]
    fn an_extreme_aspect_tightens_the_maximum() {
        let outputs = hd_outputs(6);
        let canvas = CanvasConfig {
            aspect: 8.0 / 3.0,
            render_width: 3840,
        };
        let limits = limits(
            &outputs,
            &canvas,
            &exact_request(3, 2, 0.2, 0.0),
            mode_raster,
        )
        .unwrap();
        assert_eq!(
            limits.overlap_x,
            AxisLimits {
                min: 0.0,
                max: 0.5 - 1.0 / 16384.0,
                has_seam: true,
            }
        );
        assert_eq!(
            limits.overlap_y,
            AxisLimits {
                min: 0.0,
                max: 1.0 - 1.0 / 16384.0,
                has_seam: true,
            }
        );

        // Monotonic sanity: the reported maximum solves, and a step of the
        // limit resolution past it is refused by the off-canvas gate.
        let at_max = limits.overlap_x.max;
        solve(
            &outputs,
            &canvas,
            &exact_request(3, 2, at_max, 0.0),
            mode_raster,
        )
        .unwrap();
        let error = solve(
            &outputs,
            &canvas,
            &exact_request(3, 2, at_max + LIMIT_RESOLUTION, 0.0),
            mode_raster,
        )
        .unwrap_err();
        assert!(
            error.contains("falls entirely outside the canvas"),
            "{error}"
        );
    }

    /// Three columns in one row make *small* overlaps invalid: every column
    /// is the full canvas width at the only possible scale, so the edge
    /// columns show exactly as much of the canvas as they overlap their
    /// neighbor.
    ///
    /// 1x3 on 16:9: fit_y = 1080/2160 = 0.5 and fit_x = (5760 − 3840·ox)/3840
    /// = 1.5 − ox > 0.5, so s = 0.5 and each column is 1920/0.5 = 3840 px.
    /// X spans (5760 − 3840·ox)/0.5 px, so the first column starts at
    /// (3840 − 11520 + 7680·ox)/2 = −3840·(1 − ox) px and ends at 3840·ox:
    /// on the canvas iff ox > 0. Scanning down from 0.2, every sample down
    /// to 1/64 is valid and 0 is not, so [0, 1/64] is bisected eight times
    /// with every midpoint valid, leaving min = 1/16384. overlapY is inert.
    #[test]
    fn a_third_column_makes_zero_overlap_invalid() {
        let outputs = hd_outputs(3);
        let limits = limits(
            &outputs,
            &canvas_16_9(),
            &exact_request(1, 3, 0.2, 0.0),
            mode_raster,
        )
        .unwrap();
        assert_eq!(
            limits.overlap_x,
            AxisLimits {
                min: 1.0 / 16384.0,
                max: 1.0 - 1.0 / 16384.0,
                has_seam: true,
            }
        );
        assert!(!limits.overlap_y.has_seam);

        let at_min = limits.overlap_x.min;
        solve(
            &outputs,
            &canvas_16_9(),
            &exact_request(1, 3, at_min, 0.0),
            mode_raster,
        )
        .unwrap();
        solve(
            &outputs,
            &canvas_16_9(),
            &exact_request(1, 3, 0.0, 0.0),
            mode_raster,
        )
        .unwrap_err();
    }

    /// Limits still answer for a request that is itself refused, as long as
    /// each axis has some valid value with the other held where it is: the
    /// range is then the one around the valid scan sample nearest the
    /// requested value.
    ///
    /// The refused (0.6, 0) on the 3x2, 8/3 rig of
    /// [`a_slice_pushed_entirely_off_the_canvas_is_refused`]: overlapX,
    /// holding overlapY at 0, is [0, 0.5 − 1/16384] exactly as in
    /// [`an_extreme_aspect_tightens_the_maximum`] (anchored at 31/64, the
    /// valid sample nearest 0.6).
    /// overlapY, holding overlapX at 0.6: s = min(0.7, fit_y) = 0.7 (fit_y =
    /// (3240 − 2160·oy)/1440 > 0.7 for every oy < 1), and the top row ends
    /// at 720 − (540 − 1080·oy)/0.7 px: on the canvas iff oy > 36/1080 =
    /// 1/30 = 0.0333…. The samples 1/64 and 2/64 = 0.03125 are invalid and
    /// 3/64 is the nearest valid one to 0; bisecting [2/64, 3/64] converges
    /// on the first multiple of 1/16384 above 1/30 = 546.13…/16384, so
    /// min = 547/16384.
    #[test]
    fn limits_answer_around_a_refused_request() {
        let outputs = hd_outputs(6);
        let canvas = CanvasConfig {
            aspect: 8.0 / 3.0,
            render_width: 3840,
        };
        let limits = limits(
            &outputs,
            &canvas,
            &exact_request(3, 2, 0.6, 0.0),
            mode_raster,
        )
        .unwrap();
        assert_eq!(
            limits.overlap_x,
            AxisLimits {
                min: 0.0,
                max: 0.5 - 1.0 / 16384.0,
                has_seam: true,
            }
        );
        assert_eq!(
            limits.overlap_y,
            AxisLimits {
                min: 547.0 / 16384.0,
                max: 1.0 - 1.0 / 16384.0,
                has_seam: true,
            }
        );
        solve(
            &outputs,
            &canvas,
            &exact_request(3, 2, 0.6, limits.overlap_y.min),
            mode_raster,
        )
        .unwrap();
    }

    /// When no value of an axis is valid with the other held where it is,
    /// there is no range to report, and the error is `solve`'s own at the
    /// requested values: with overlapX at 0, the 1x3 row of
    /// [`a_third_column_makes_zero_overlap_invalid`] is refused whatever
    /// the inert overlapY.
    #[test]
    fn limits_with_no_valid_value_report_the_solve_error() {
        let outputs = hd_outputs(3);
        let asked = exact_request(1, 3, 0.0, 0.0);
        let error = limits(&outputs, &canvas_16_9(), &asked, mode_raster).unwrap_err();
        assert_eq!(
            error,
            solve(&outputs, &canvas_16_9(), &asked, mode_raster).unwrap_err()
        );
    }

    /// Limits are about overlaps; a content-scale request has none to
    /// limit. Grid errors come through as `solve` gives them.
    #[test]
    fn limits_refuse_a_content_scale_request_and_grid_errors() {
        let error = limits(
            &hd_outputs(4),
            &canvas_16_9(),
            &request(2, 2, None, None, Some(0.9)),
            mode_raster,
        )
        .unwrap_err();
        assert!(error.contains("contentScale"), "{error}");

        let error = limits(
            &hd_outputs(5),
            &canvas_16_9(),
            &exact_request(2, 2, 0.2, 0.2),
            mode_raster,
        )
        .unwrap_err();
        assert!(error.contains("do not fit a 2x2 grid"), "{error}");

        let mut state = document(4);
        state.projection.as_mut().unwrap().canvas = None;
        let error =
            limits_document(&state, &exact_request(2, 2, 0.2, 0.2), mode_raster).unwrap_err();
        assert_eq!(error, "projection.canvas is required to arrange outputs");
    }

    /// The new solution field and the limits serialize camelCase, as the
    /// API contract spells them. (Zero overlap on the 16:9 rig fits both
    /// axes exactly — fit_x = 3840/3840 = fit_y = 2160/2160 = 1 — so the
    /// overhang is exactly zero, with no float dust to compare.)
    #[test]
    fn overhang_and_limits_serialize_camel_case() {
        let solution = solve(
            &hd_outputs(4),
            &canvas_16_9(),
            &exact_request(2, 2, 0.0, 0.0),
            mode_raster,
        )
        .unwrap();
        let value = serde_json::to_value(&solution).unwrap();
        assert_eq!(value["overhang"], serde_json::json!({ "x": 0.0, "y": 0.0 }));

        let limits = ArrangementLimits {
            overlap_x: AxisLimits {
                min: 0.0,
                max: 0.5,
                has_seam: true,
            },
            overlap_y: AxisLimits {
                min: 0.0,
                max: 0.25,
                has_seam: false,
            },
        };
        assert_eq!(
            serde_json::to_value(limits).unwrap(),
            serde_json::json!({
                "overlapX": { "min": 0.0, "max": 0.5, "hasSeam": true },
                "overlapY": { "min": 0.0, "max": 0.25, "hasSeam": false }
            })
        );
    }
}
