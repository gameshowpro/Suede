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
//!   exactly. The content scale is therefore `max` of the two per-axis
//!   scales, not `min`: the binding axis fills the canvas and the other is
//!   centered, leaving a visible unused band rather than silently absorbing
//!   extra overlap on the slack axis (which would make one slider inert and
//!   would show a doubled band where the projectors do not physically
//!   overlap). [`ArrangementSolution::unused_canvas`] reports the band and
//!   [`ArrangementSolution::implied_aspect`] the canvas aspect that would
//!   remove it. The canvas aspect is the operator's; this never changes it.
//! * Grid metrics and placement are otherwise reproduced exactly, including
//!   the centering of a single row or single column.

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
/// client supplied, this records what the solver settled on. It is a record
/// of *intent*, not canonical geometry — the source rectangles remain the
/// truth, and a later manual geometry edit leaves this in place. See
/// [`in_effect`] for the test of whether it still describes the document.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Arrangement {
    /// Grid rows; at least 1.
    pub rows: u32,
    /// Grid columns; at least 1.
    pub columns: u32,
    /// Fraction of the smaller of two adjacent columns' raster widths that
    /// they share; `0 <= overlapX < 1`.
    pub overlap_x: f64,
    /// As `overlapX`, for adjacent rows' raster heights.
    pub overlap_y: f64,
    /// Output pixels per canvas pixel; `1.0` samples the canvas 1:1.
    pub content_scale: f64,
}

/// What a client asks for: the grid, plus *either* the overlaps *or* the
/// content scale.
///
/// Supplying both is over-determined and is an error; supplying neither
/// means both overlaps are zero.
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

/// The fraction of the canvas left uncovered on each axis.
///
/// At most one of the two is non-zero: the binding axis fills the canvas
/// exactly and the slack axis is centered, so half of its value is left
/// before the first slot and half after the last.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct UnusedCanvas {
    pub x: f64,
    pub y: f64,
}

/// A solved arrangement: what would be written, without writing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementSolution {
    /// The resolved five values, exactly as [`apply`] records them.
    pub arrangement: Arrangement,
    /// The uncovered fraction of the canvas on each axis.
    pub unused_canvas: UnusedCanvas,
    /// The canvas aspect at which both axes would fill exactly, leaving no
    /// unused band. Reported, never applied: changing the aspect resizes the
    /// headless canvas and the browser, so it is the operator's decision.
    pub implied_aspect: f64,
    /// Every enabled output, in document order.
    pub outputs: Vec<ArrangedOutput>,
    /// Advisory notes about the solution; never a reason to refuse it.
    pub warnings: Vec<String>,
}

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
/// needed. Whatever the grid does not cover is split evenly before and
/// after it, which also reproduces the old centering of a single row or
/// single column.
fn place(metrics: &Metrics, arrangement: &Arrangement) -> Placement {
    let columns = metrics.col_width.len();
    let rows = metrics.row_height.len();
    let scale = arrangement.content_scale;
    let (effective_w, effective_h) =
        metrics.effective_extent(arrangement.overlap_x, arrangement.overlap_y);
    let unused_x = (metrics.canvas_w - effective_w / scale).max(0.0);
    let unused_y = (metrics.canvas_h - effective_h / scale).max(0.0);

    let mut x_origin = Vec::with_capacity(columns);
    x_origin.push(unused_x / 2.0);
    for column in 1..columns {
        let seam =
            arrangement.overlap_x * metrics.col_width[column - 1].min(metrics.col_width[column]);
        let advance = (metrics.col_width[column - 1] - seam) / scale;
        x_origin.push(x_origin[column - 1] + advance);
    }
    let mut y_origin = Vec::with_capacity(rows);
    y_origin.push(unused_y / 2.0);
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
            x: unused_x / metrics.canvas_w,
            y: unused_y / metrics.canvas_h,
        },
    }
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
    let (overlap_x, overlap_y, content_scale) = match (asked_overlap, request.content_scale) {
        (true, Some(_)) => {
            return Err(
                "arrangement is over-determined: give overlapX and overlapY, or contentScale, \
                 but not both"
                    .into(),
            )
        }
        // Scale mode: the canvas fills both axes and the overlaps follow.
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
                    "content scale is too small for this grid: overlap cannot reach 100%".into(),
                );
            }
            (overlap_x, overlap_y, scale)
        }
        // Overlap mode: both overlaps are honored exactly, so the content
        // scale is the larger of the two axes' requirements and the smaller
        // axis is left with an unused band.
        (_, None) => {
            let overlap_x = checked_overlap("overlapX", request.overlap_x)?;
            let overlap_y = checked_overlap("overlapY", request.overlap_y)?;
            let (effective_w, effective_h) = metrics.effective_extent(overlap_x, overlap_y);
            let scale_x = effective_w / metrics.canvas_w;
            let scale_y = effective_h / metrics.canvas_h;
            let scale = scale_x.max(scale_y);
            if !(scale.is_finite() && scale > 0.0) {
                return Err("this grid has no positive content scale".into());
            }
            (overlap_x, overlap_y, scale)
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
    let (effective_w, effective_h) = metrics.effective_extent(overlap_x, overlap_y);

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
        // Both axes fill exactly when scale_x == scale_y, that is when
        // canvasHeight = canvasWidth * effectiveH / effectiveW; the aspect
        // is width / height, so the canvas pixels cancel out.
        implied_aspect: effective_w / effective_h,
        outputs: enabled
            .iter()
            .zip(placement.rects)
            .map(|(output, source)| ArrangedOutput {
                key: output.r#match.key(),
                source,
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
        .all(|(output, expected)| {
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
    use crate::model::desired::{OutputMatch, ProjectionConfig};
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
            committed: false,
        }
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

    /// Both overlaps are honored exactly, so the axis that needs the smaller
    /// scale is the one left with slack.
    ///
    /// sx = (3840 − 0.2·1920)/3840 = 3456/3840 = 0.9
    /// sy = (2160 − 0.05·1080)/2160 = 2106/2160 = 0.975
    /// scale = max = 0.975 (the old solver took min = 0.9 and let the rows
    /// overlap by more than asked).
    /// x extent = 3456/0.975 = 3544.615… px, so 3840 − 3544.615… = 295.384… px
    /// is unused: 295.384…/3840 = 1/13, half of it (1/26) before the grid.
    /// x origins: 1/26 and 1/26 + (1536/0.975)/3840 = 1/26 + 16/39 = 35/78.
    /// width = 1920/(0.975·3840) = 1920/3744 = 20/39, and 35/78 + 40/78 = 75/78
    /// = 25/26 = 1 − 1/26: centered.
    /// y extent = 2106/0.975 = 2160 px exactly, so y is the binding axis.
    /// y origins: 0 and (1026/0.975)/3840 = 1052.307…/3840 = 57/208.
    /// height = 1080/3744 = 15/52, and 57/208 + 60/208 = 117/208 = 0.5625.
    #[test]
    fn independent_overlaps_are_exact_and_the_slack_axis_is_centered() {
        let outputs = hd_outputs(4);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, Some(0.2), Some(0.05), None),
            mode_raster,
        )
        .unwrap();

        close(solution.arrangement.overlap_x, 0.2);
        close(solution.arrangement.overlap_y, 0.05);
        close(solution.arrangement.content_scale, 0.975);
        close(solution.unused_canvas.x, 1.0 / 13.0);
        close(solution.unused_canvas.y, 0.0);
        // 3456/2106 = 1.6410…: at that aspect the two per-axis scales agree.
        close(solution.implied_aspect, 3456.0 / 2106.0);

        close_rect(
            solution.outputs[0].source,
            [1.0 / 26.0, 0.0, 20.0 / 39.0, 15.0 / 52.0],
        );
        close_rect(
            solution.outputs[1].source,
            [35.0 / 78.0, 0.0, 20.0 / 39.0, 15.0 / 52.0],
        );
        close_rect(
            solution.outputs[2].source,
            [1.0 / 26.0, 57.0 / 208.0, 20.0 / 39.0, 15.0 / 52.0],
        );
        close_rect(
            solution.outputs[3].source,
            [35.0 / 78.0, 57.0 / 208.0, 20.0 / 39.0, 15.0 / 52.0],
        );
    }

    /// Setting the canvas to the reported `impliedAspect` removes the band.
    ///
    /// aspect = 3456/2106 with renderWidth 3840 gives height
    /// round(3840·2106/3456) = 2340, and then sy = 2106/2340 = 0.9 = sx.
    #[test]
    fn implied_aspect_removes_the_unused_band() {
        let outputs = hd_outputs(4);
        let asked = request(2, 2, Some(0.2), Some(0.05), None);
        let first = solve(&outputs, &canvas_16_9(), &asked, mode_raster).unwrap();

        let squarer = CanvasConfig {
            aspect: first.implied_aspect,
            render_width: 3840,
        };
        assert_eq!(squarer.dimensions().unwrap(), (3840, 2340));
        let second = solve(&outputs, &squarer, &asked, mode_raster).unwrap();

        close(second.unused_canvas.x, 0.0);
        close(second.unused_canvas.y, 0.0);
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

    #[test]
    fn a_very_high_overlap_warns() {
        let outputs = hd_outputs(4);
        let solution = solve(
            &outputs,
            &canvas_16_9(),
            &request(2, 2, Some(0.9), Some(0.0), None),
            mode_raster,
        )
        .unwrap();
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
}
