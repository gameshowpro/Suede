//! Slice units at document level: converting a document's slices between
//! `percent` and `pixels`, and the report of slices that extend past the
//! canvas.
//!
//! The rectangle-level rules (edge rounding, exact division, the one place
//! pixels are derived) live in [`super::geometry`]; this module applies them
//! to every output of a document. The write policy that decides *when* a
//! document is converted is [`crate::projection_policy::settle_slice_units`].

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::desired::{ArrangeOffset, DesiredState};
use super::geometry::{
    convert_offset, convert_rect, effective_slice_px, CanvasConfig, OutputGeometry, SliceUnits,
};

/// Convert one output's stored geometry from `from` to `to` on `canvas`.
///
/// The slice follows [`convert_rect`]: percent to pixels rounds its edges,
/// pixels to percent divides exactly. The raster footprint is a measurement,
/// not a crop, so it is converted without rounding, except that a footprint
/// equal to the slice (what every conversion and reset writes until a
/// calibrated footprint is supplied) stays equal to it.
pub fn convert_geometry(
    geometry: &mut OutputGeometry,
    from: SliceUnits,
    to: SliceUnits,
    canvas: &CanvasConfig,
) -> Result<(), String> {
    if from == to {
        return Ok(());
    }
    let slice = convert_rect(geometry.slice, from, to, canvas)?;
    geometry.raster_footprint = if geometry.raster_footprint == geometry.slice {
        slice
    } else {
        convert_measurement(geometry.raster_footprint, from, to, canvas)?
    };
    geometry.slice = slice;
    Ok(())
}

/// A rectangle converted by exact multiplication or division, never
/// rounded.
fn convert_measurement(
    rect: super::geometry::CanvasRect,
    from: SliceUnits,
    to: SliceUnits,
    canvas: &CanvasConfig,
) -> Result<super::geometry::CanvasRect, String> {
    match (from, to) {
        (SliceUnits::Percent, SliceUnits::Percent) | (SliceUnits::Pixels, SliceUnits::Pixels) => {
            Ok(rect)
        }
        (SliceUnits::Pixels, SliceUnits::Percent) => {
            super::geometry::to_canvas_units(rect, from, canvas)
        }
        (SliceUnits::Percent, SliceUnits::Pixels) => {
            let (x_density, y_density) = super::geometry::canvas_densities(canvas)?;
            Ok(super::geometry::CanvasRect {
                x: rect.x * x_density,
                y: rect.y * y_density,
                width: rect.width * x_density,
                height: rect.height * y_density,
            })
        }
    }
}

/// Convert one output's `arrangeOffset` from `from` to `to` on `canvas`.
pub fn convert_arrange_offset(
    offset: &mut ArrangeOffset,
    from: SliceUnits,
    to: SliceUnits,
    canvas: &CanvasConfig,
) -> Result<(), String> {
    let [x, y] = convert_offset([offset.x, offset.y], from, to, canvas)?;
    offset.x = x;
    offset.y = y;
    Ok(())
}

/// Convert every output's geometry and `arrangeOffset` in `document` to
/// `to`, on the document's own canvas, and record `to` as
/// `projection.sliceUnits`.
///
/// A document already in `to` is left as it is. Fails, changing nothing,
/// when a conversion is needed and the document has no valid canvas to
/// measure it against.
pub fn convert_document(document: &mut DesiredState, to: SliceUnits) -> Result<(), String> {
    let from = document.slice_units();
    if from == to {
        return Ok(());
    }
    let canvas = document
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas)
        .ok_or_else(|| {
            format!(
                "converting the slices from {} to {} needs projection.canvas",
                from.name(),
                to.name()
            )
        })?;
    canvas
        .dimensions()
        .map_err(|error| format!("projection.canvas {error}"))?;
    let mut converted = document.outputs.clone();
    for output in &mut converted {
        if let Some(geometry) = output.geometry.as_mut() {
            convert_geometry(geometry, from, to, &canvas)?;
        }
        if let Some(offset) = output.arrange_offset.as_mut() {
            convert_arrange_offset(offset, from, to, &canvas)?;
        }
    }
    document.outputs = converted;
    if let Some(projection) = document.projection.as_mut() {
        projection.set_slice_units(to);
    }
    Ok(())
}

/// One enabled output whose whole-pixel slice extends past the canvas.
///
/// The slice is still rendered (its part off the canvas shows black); this
/// is a report, not an error. It arises in `pixels` when the canvas shrinks
/// under fixed slices, and in either unit when a slice is placed or
/// arranged past an edge. A slice with no area on the canvas at all is
/// refused by validation instead, and Exact mode refuses any overhang.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SliceOverhang {
    /// The output's configuration key (its connector name, e.g. `DP-2`, for
    /// a plain name match), as `PUT /config/outputs/{key}` takes it.
    pub output: String,
    /// Whole canvas pixels of the slice past the canvas's left edge; 0 when
    /// it does not extend past it.
    pub left: u32,
    /// Whole canvas pixels past the canvas's top edge.
    pub top: u32,
    /// Whole canvas pixels past the canvas's right edge.
    pub right: u32,
    /// Whole canvas pixels past the canvas's bottom edge.
    pub bottom: u32,
    /// The same, as a sentence for a person, e.g. "DP-2 extends 960 px past
    /// the canvas's right edge".
    pub message: String,
}

/// Every enabled output whose whole-pixel slice ([`effective_slice_px`])
/// extends past `document`'s canvas, in document order, with the overhang
/// in canvas pixels per edge. Empty without a valid canvas, and for outputs
/// without geometry.
pub fn out_of_canvas(document: &DesiredState) -> Vec<SliceOverhang> {
    let Some(projection) = document.projection.as_ref() else {
        return Vec::new();
    };
    let Some(canvas) = projection.canvas.as_ref() else {
        return Vec::new();
    };
    let Ok((width, height)) = canvas.dimensions() else {
        return Vec::new();
    };
    let units = projection.slice_units();
    let (width, height) = (i64::from(width), i64::from(height));
    let past = |value: i64| u32::try_from(value.max(0)).unwrap_or(u32::MAX);
    document
        .outputs
        .iter()
        .filter(|output| output.enable)
        .filter_map(|output| {
            let geometry = output.geometry.as_ref()?;
            let pixels = effective_slice_px(geometry.slice, units, canvas).ok()?;
            let key = output.r#match.key();
            let (left, top) = (past(-pixels.x), past(-pixels.y));
            let (right, bottom) = (past(pixels.right() - width), past(pixels.bottom() - height));
            let message = overhang_message(&key, [left, top, right, bottom])?;
            Some(SliceOverhang {
                output: key,
                left,
                top,
                right,
                bottom,
                message,
            })
        })
        .collect()
}

/// "DP-2 extends 960 px past the canvas's right edge", or with several
/// edges "DP-2 extends 10 px past the canvas's left edge and 960 px past its
/// right edge". `None` when nothing extends past the canvas.
fn overhang_message(key: &str, [left, top, right, bottom]: [u32; 4]) -> Option<String> {
    let parts: Vec<(u32, &str)> = [
        (left, "left"),
        (right, "right"),
        (top, "top"),
        (bottom, "bottom"),
    ]
    .into_iter()
    .filter(|(pixels, _)| *pixels > 0)
    .collect();
    let phrases: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(index, (pixels, edge))| {
            let owner = if index == 0 { "the canvas's" } else { "its" };
            format!("{pixels} px past {owner} {edge} edge")
        })
        .collect();
    let list = match phrases.as_slice() {
        [] => return None,
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    };
    Some(format!("{key} extends {list}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overhang_messages_name_every_edge_in_order() {
        assert_eq!(overhang_message("DP-2", [0, 0, 0, 0]), None);
        assert_eq!(
            overhang_message("DP-2", [0, 0, 960, 0]).unwrap(),
            "DP-2 extends 960 px past the canvas's right edge"
        );
        assert_eq!(
            overhang_message("DP-2", [10, 0, 960, 0]).unwrap(),
            "DP-2 extends 10 px past the canvas's left edge and 960 px past its right edge"
        );
        assert_eq!(
            overhang_message("DP-1", [1, 2, 3, 4]).unwrap(),
            "DP-1 extends 1 px past the canvas's left edge, 3 px past its right edge, 2 px \
             past its top edge and 4 px past its bottom edge"
        );
    }

    use crate::model::{CanvasConfig, CanvasRect, DesiredState, OutputGeometry, SliceUnits};

    const REVISION_2373: &str = include_str!("../../tests/fixtures/system-a-revision-2373.json");

    fn revision_2373() -> DesiredState {
        serde_json::from_str(REVISION_2373).unwrap()
    }

    fn canvas_of(document: &DesiredState) -> CanvasConfig {
        document.projection.as_ref().unwrap().canvas.unwrap()
    }

    fn set_render_width(document: &mut DesiredState, render_width: u32) {
        document
            .projection
            .as_mut()
            .unwrap()
            .canvas
            .as_mut()
            .unwrap()
            .render_width = render_width;
    }

    fn crops(document: &DesiredState) -> Vec<[f64; 4]> {
        let canvas = canvas_of(document);
        let units = document.slice_units();
        document
            .outputs
            .iter()
            .map(|output| {
                effective_slice_px(output.geometry.as_ref().unwrap().slice, units, &canvas)
                    .unwrap()
                    .source()
            })
            .collect()
    }

    const QUADRANTS: [[f64; 4]; 4] = [
        [0.0, 0.0, 1920.0, 1080.0],
        [1920.0, 0.0, 1920.0, 1080.0],
        [0.0, 1080.0, 1920.0, 1080.0],
        [1920.0, 1080.0, 1920.0, 1080.0],
    ];

    /// System A's revision 2373, as saved before slice units existed: no
    /// `sliceUnits`, aspect 1.77777777, slice height 0.2812500012304688.
    /// It loads as percent, validates, re-serializes byte for byte, and its
    /// slices are exactly 1920x1080 whole pixels.
    #[test]
    fn system_a_revision_2373_is_percent_and_round_trips_byte_identically() {
        let document = revision_2373();
        assert_eq!(document.projection.as_ref().unwrap().slice_units, None);
        assert_eq!(document.slice_units(), SliceUnits::Percent);
        assert_eq!(document.validate(true), Ok(()));
        assert_eq!(
            String::from_utf8(serde_json::to_vec_pretty(&document).unwrap()).unwrap(),
            REVISION_2373
        );
        assert!(!REVISION_2373.contains("sliceUnits"));
        assert_eq!(crops(&document), QUADRANTS);
        assert!(out_of_canvas(&document).is_empty());
    }

    /// `percent` is never written out, whether absent or named, so an older
    /// build reads a percent document as it always did; `pixels` is, and an
    /// older build (which denies unknown fields) refuses it rather than
    /// misreading it.
    #[test]
    fn only_pixels_is_written_out() {
        let mut projection = crate::model::ProjectionConfig::default();
        for (units, written) in [
            (None, None),
            (Some(SliceUnits::Percent), None),
            (Some(SliceUnits::Pixels), Some("pixels")),
        ] {
            projection.slice_units = units;
            let value = serde_json::to_value(&projection).unwrap();
            assert_eq!(
                value.get("sliceUnits").and_then(|v| v.as_str()),
                written,
                "{units:?}"
            );
        }
        let named: crate::model::ProjectionConfig =
            serde_json::from_str(r#"{"sliceUnits":"percent"}"#).unwrap();
        assert_eq!(named.slice_units(), SliceUnits::Percent);
    }

    /// Percent: the fractions are the truth, so a render-width change
    /// rescales the crops and coming back restores them exactly, after any
    /// sequence of changes, because nothing rounded is ever stored.
    #[test]
    fn percent_slices_rescale_and_never_drift() {
        let original = revision_2373();
        let mut document = original.clone();
        set_render_width(&mut document, 2880);
        assert_eq!(
            crops(&document),
            [
                [0.0, 0.0, 1440.0, 810.0],
                [1440.0, 0.0, 1440.0, 810.0],
                [0.0, 810.0, 1440.0, 810.0],
                [1440.0, 810.0, 1440.0, 810.0],
            ]
        );
        set_render_width(&mut document, 3840);
        assert_eq!(crops(&document), QUADRANTS);

        // An arbitrary walk of widths, odd ones included, through the write
        // policy each time: the stored slices never change.
        let mut state = 0x2373_u64;
        for _ in 0..200 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let width = 640 + (state % 5000) as u32;
            let previous = document.clone();
            let mut next = document.clone();
            set_render_width(&mut next, width);
            crate::projection_policy::settle_slice_units(&mut next, &previous);
            crate::projection_policy::preserve_retained(&mut next, &previous);
            assert_eq!(next.outputs, original.outputs, "at {width}");
            document = next;
        }
        set_render_width(&mut document, 3840);
        assert_eq!(crops(&document), QUADRANTS);
        assert_eq!(
            String::from_utf8(serde_json::to_vec_pretty(&document).unwrap()).unwrap(),
            REVISION_2373
        );
    }

    /// Pixels: the integers are the truth. A render-width reduction leaves
    /// them alone and reports every output that now extends past the
    /// canvas, per edge, in pixels; validation still accepts it.
    #[test]
    fn pixels_survive_a_render_width_change_and_report_the_overhang() {
        let mut document = revision_2373();
        convert_document(&mut document, SliceUnits::Pixels).unwrap();
        assert_eq!(
            document.projection.as_ref().unwrap().slice_units,
            Some(SliceUnits::Pixels)
        );
        let stored = document.outputs.clone();
        assert_eq!(crops(&document), QUADRANTS);

        let previous = document.clone();
        set_render_width(&mut document, 2880);
        crate::projection_policy::settle_slice_units(&mut document, &previous);
        assert_eq!(document.outputs, stored);
        assert_eq!(crops(&document), QUADRANTS);
        assert_eq!(document.validate(true), Ok(()));
        // The canvas is 2880x1620 now.
        let report = out_of_canvas(&document);
        let summary: Vec<_> = report
            .iter()
            .map(|o| (o.output.as_str(), o.left, o.top, o.right, o.bottom))
            .collect();
        assert_eq!(
            summary,
            [
                ("DP-2", 0, 0, 960, 0),
                ("DP-3", 0, 0, 0, 540),
                ("DP-4", 0, 0, 960, 540),
            ]
        );
        assert_eq!(
            report[0].message,
            "DP-2 extends 960 px past the canvas's right edge"
        );
        assert_eq!(
            report[2].message,
            "DP-4 extends 960 px past the canvas's right edge and 540 px past its bottom edge"
        );

        // Back to 3840: the same pixels, nothing reported.
        set_render_width(&mut document, 3840);
        assert_eq!(document.outputs, stored);
        assert!(out_of_canvas(&document).is_empty());

        // A slice with no area on the canvas at all is still refused.
        set_render_width(&mut document, 1600);
        let errors = document.validate(true).unwrap_err();
        assert!(
            errors.iter().any(|e| e
                == "outputs[3].geometry geometry.slice has no positive visible area \
                            on the canvas"),
            "{errors:?}"
        );
    }

    /// Switching units converts the stored slices: percent to pixels rounds
    /// the edges, pixels to percent divides exactly, and pixels survive the
    /// round trip unchanged.
    #[test]
    fn conversion_rounds_edges_one_way_and_divides_exactly_the_other() {
        let mut document = revision_2373();
        let percent = document.outputs.clone();
        convert_document(&mut document, SliceUnits::Pixels).unwrap();
        let pixels = document.outputs.clone();
        assert_eq!(
            pixels[3].geometry.as_ref().unwrap().slice,
            CanvasRect {
                x: 1920.0,
                y: 1080.0,
                width: 1920.0,
                height: 1080.0
            }
        );
        // The keystone pins are not slice data and are untouched.
        for (before, after) in percent.iter().zip(&pixels) {
            let (before, after) = (before.geometry.as_ref(), after.geometry.as_ref());
            assert_eq!(before.unwrap().corners, after.unwrap().corners);
            assert_eq!(after.unwrap().raster_footprint, after.unwrap().slice);
        }
        convert_document(&mut document, SliceUnits::Percent).unwrap();
        assert_eq!(document.projection.as_ref().unwrap().slice_units, None);
        assert_eq!(crops(&document), QUADRANTS);
        convert_document(&mut document, SliceUnits::Pixels).unwrap();
        assert_eq!(document.outputs, pixels);
    }

    /// A small xorshift generator: the property tests below need
    /// reproducible randomness, not a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// Edge rounding over random grids: neighbors that touch in fractions
    /// still touch in pixels, an overlap is the same whole number of pixels
    /// from both sides, and a pixel layout survives pixels -> percent ->
    /// pixels exactly.
    #[test]
    fn edge_rounding_keeps_random_grids_consistent() {
        let mut rng = Rng(0x5eed_1234_abcd_ef01);
        for _ in 0..500 {
            let render_width = 200 + rng.below(7000) as u32;
            let aspect = 0.4 + rng.below(3000) as f64 / 1000.0;
            let canvas = CanvasConfig {
                aspect,
                render_width,
            };
            let Ok((width, height)) = canvas.dimensions() else {
                continue;
            };
            let (x_density, y_density) = super::super::geometry::canvas_densities(&canvas).unwrap();
            let columns = 1 + rng.below(4) as usize;
            let rows = 1 + rng.below(3) as usize;

            // Random fractional column edges, each column overlapping or
            // touching the next by a random (possibly zero) fraction.
            let axis = |rng: &mut Rng, count: usize, extent: f64| -> Vec<(f64, f64)> {
                let mut spans = Vec::new();
                let mut start = 0.0_f64;
                for _ in 0..count {
                    let size = extent * (0.2 + rng.below(1000) as f64 / 1250.0) / count as f64;
                    spans.push((start, size));
                    let overlap = if rng.below(3) == 0 {
                        0.0
                    } else {
                        size * rng.below(400) as f64 / 1000.0
                    };
                    start = start + size - overlap;
                }
                spans
            };
            let xs = axis(&mut rng, columns, 1.0);
            let ys = axis(&mut rng, rows, 1.0 / aspect);
            for (row, &(y, h)) in ys.iter().enumerate() {
                for (column, &(x, w)) in xs.iter().enumerate() {
                    let rect = CanvasRect {
                        x,
                        y,
                        width: w,
                        height: h,
                    };
                    let px = effective_slice_px(rect, SliceUnits::Percent, &canvas).unwrap();
                    // Each edge is the rounding of that edge alone.
                    assert_eq!(
                        px.x as f64,
                        super::super::geometry::round_half_up(x * x_density)
                    );
                    assert_eq!(
                        px.right() as f64,
                        super::super::geometry::round_half_up((x + w) * x_density)
                    );
                    if column + 1 < xs.len() {
                        let (next_x, next_w) = xs[column + 1];
                        let next = effective_slice_px(
                            CanvasRect {
                                x: next_x,
                                y,
                                width: next_w,
                                height: h,
                            },
                            SliceUnits::Percent,
                            &canvas,
                        )
                        .unwrap();
                        let fractional_overlap = (x + w) - next_x;
                        let pixel_overlap = px.right() - next.x;
                        if fractional_overlap == 0.0 {
                            assert_eq!(pixel_overlap, 0, "touching neighbors must still touch");
                        }
                        assert!(pixel_overlap >= 0, "an overlap never turns into a gap");
                        // Seen from the right-hand slice, the same overlap.
                        assert_eq!(
                            pixel_overlap,
                            super::super::geometry::round_half_up((x + w) * x_density) as i64
                                - super::super::geometry::round_half_up(next_x * x_density) as i64
                        );
                    }
                    if row + 1 < ys.len() {
                        let (next_y, _) = ys[row + 1];
                        let below = effective_slice_px(
                            CanvasRect {
                                x,
                                y: next_y,
                                width: w,
                                height: ys[row + 1].1,
                            },
                            SliceUnits::Percent,
                            &canvas,
                        )
                        .unwrap();
                        assert_eq!(
                            below.y as f64,
                            super::super::geometry::round_half_up(next_y * y_density)
                        );
                        if (y + h) - next_y == 0.0 {
                            assert_eq!(px.bottom(), below.y, "touching rows must still touch");
                        }
                        assert!(px.bottom() >= below.y);
                    }

                    // Pixels -> percent -> pixels is exact.
                    let pixels = px.stored();
                    let fraction =
                        convert_rect(pixels, SliceUnits::Pixels, SliceUnits::Percent, &canvas)
                            .unwrap();
                    let back =
                        convert_rect(fraction, SliceUnits::Percent, SliceUnits::Pixels, &canvas)
                            .unwrap();
                    assert_eq!(back, pixels, "{width}x{height}");
                }
            }
        }
    }

    #[test]
    fn a_footprint_that_is_not_the_slice_is_converted_without_rounding() {
        let canvas = CanvasConfig {
            aspect: 2.0,
            render_width: 1000,
        };
        let mut geometry = OutputGeometry {
            slice: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 0.5004,
                height: 0.25,
            },
            corners: crate::model::exact::UNIT_CORNERS,
            center: crate::model::exact::NEUTRAL_CENTER,
            raster_footprint: CanvasRect {
                x: 0.0001,
                y: 0.0,
                width: 0.5,
                height: 0.25,
            },
        };
        convert_geometry(
            &mut geometry,
            SliceUnits::Percent,
            SliceUnits::Pixels,
            &canvas,
        )
        .unwrap();
        assert_eq!(geometry.slice.width, 500.0);
        assert!((geometry.raster_footprint.x - 0.1).abs() < 1e-9);
        assert_eq!(geometry.raster_footprint.width, 500.0);
    }
}
