//! Exact mode: the strictly one-to-one projection mode.
//!
//! In Exact mode every enabled output shows exactly its own raster's worth
//! of canvas pixels, placed at whole-pixel canvas coordinates, at output
//! scale 1, without correction. The canvas may be larger than the union of
//! the outputs. These are pure functions over a [`DesiredState`]:
//!
//! * [`rule_problems`] holds the Exact-only rules, which
//!   [`DesiredState::validate`] applies on top of the shared-canvas checks
//!   whenever `projection.mode` is `exact`.
//! * [`conforms`] answers whether a document would be valid in Exact mode as
//!   it stands, [`strip`] removes the correction Exact does not carry,
//!   [`defaults`] derives the Exact layout from the outputs' integer
//!   positions, and [`enter`] combines the three into the switching rule:
//!   keep a conforming document once stripped, otherwise reset it to the
//!   defaults.
//! * [`leave`] is the way out. An Exact document already carries identity
//!   pins, a centered remap, scale 1 and whole-pixel slices, which Simple and
//!   Warp both render pixel for pixel the same way, so leaving changes only
//!   the mode. That is what makes an immediate round trip back into Exact
//!   reproduce the starting document exactly: the document that comes back
//!   conforms, [`strip`] has nothing to change, and it is kept as it is.
//!
//! [`crate::projection_policy::transition`] applies [`enter`] and [`leave`]
//! to every working-copy replacement that changes the mode.
//!
//! Exact needs the canvas slicer. On a tiled appliance
//! (`allow_overlaps = false`) it is unavailable, for the reason
//! [`unavailable_reason`] gives; that one function is the policy, so
//! changing how a tiled appliance treats Exact starts there.

use super::arrangement::seam_extents;
use super::desired::{DesiredState, OutputConfig};
use super::geometry::{validate_slices, CanvasConfig, CanvasRect, OutputGeometry, ProjectionMode};

/// Unit destination pins, in TL, TR, BR, BL order: no corner correction.
pub const UNIT_CORNERS: [[f64; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

/// The centered remap: no center correction.
pub const NEUTRAL_CENTER: [f64; 2] = [0.5, 0.5];

/// How far, in pixels, a slice edge or seam may sit from a whole pixel and
/// still count as one. Normalized canvas coordinates are fractions, so a
/// whole-pixel rectangle comes back from `x * density` with a few ulps of
/// rounding error; nothing a person or a solver means is this close to an
/// integer without being it.
pub const PIXEL_EPS: f64 = 1.0e-9;

/// Why Exact mode is unavailable on a tiled appliance.
pub const TILED_REASON: &str = "exact mode needs the canvas slicer: this appliance tiles \
                                (allow_overlaps = false); tiled output is already one-to-one at \
                                output scale 1";

/// Why Exact mode cannot be used on this appliance, or `None` when it can.
///
/// The one statement of the tiled-appliance policy: validation rejects an
/// Exact document with this reason, and the mode transition does not try to
/// reset a document into a mode it cannot have. Exact needs no capability
/// probe beyond what Simple needs, so the slicer path is the only condition.
pub fn unavailable_reason(allow_overlaps: bool) -> Option<&'static str> {
    (!allow_overlaps).then_some(TILED_REASON)
}

/// What entering Exact mode did to a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// The document conformed once its correction was stripped, and was
    /// kept.
    Kept,
    /// The document did not conform and was reset to the Exact-mode
    /// defaults; `reasons` are the rules it broke.
    Reset { reasons: Vec<String> },
}

/// The raster an output covers on the canvas: its effective mode, swapped
/// for 90/270 transforms. `None` without a positive effective mode.
fn logical_raster(output: &OutputConfig) -> Option<(u32, u32)> {
    let mode = output.effective_mode()?;
    let width = u32::try_from(mode.width).ok().filter(|w| *w > 0)?;
    let height = u32::try_from(mode.height).ok().filter(|h| *h > 0)?;
    Some(
        output
            .effective_transform()
            .unwrap_or_default()
            .physical_size((width, height)),
    )
}

/// The unrotated mode raster, the shape [`seam_extents`] expects.
fn mode_raster(output: &OutputConfig) -> Option<(u32, u32)> {
    let mode = output.effective_mode()?;
    Some((
        u32::try_from(mode.width).ok()?,
        u32::try_from(mode.height).ok()?,
    ))
}

fn whole(value: f64) -> bool {
    (value - value.round()).abs() <= PIXEL_EPS
}

/// A pixel value for a message: whole numbers without a fraction, anything
/// else to six decimals.
fn px(value: f64) -> String {
    let shown = if whole(value) {
        value.round()
    } else {
        (value * 1.0e6).round() / 1.0e6
    };
    // `+ 0.0` turns a negative zero positive, so nothing prints as "-0".
    format!("{}", shown + 0.0)
}

fn has_correction(geometry: &OutputGeometry) -> bool {
    geometry.corners != UNIT_CORNERS || geometry.center != NEUTRAL_CENTER
}

/// The Exact-only rules for a document whose canvas is `canvas`, as
/// distinct, quotable messages. The shared-canvas checks every mode with a
/// canvas gets (canvas present and valid, every enabled output with a mode
/// and geometry, the slice topology) are not repeated here; an output those
/// checks already reject is skipped rather than reported twice.
pub fn rule_problems(desired: &DesiredState, canvas: &CanvasConfig) -> Vec<String> {
    let mut problems = Vec::new();
    let Ok((canvas_width, canvas_height)) = canvas.dimensions() else {
        return problems;
    };
    let (canvas_width, canvas_height) = (f64::from(canvas_width), f64::from(canvas_height));
    for (index, output) in desired.outputs.iter().enumerate() {
        let prefix = format!("outputs[{index}]");
        // Every output, enabled or not: entering Exact strips correction
        // from all of them, so one that carries some was put there by a
        // write in Exact mode, which is refused rather than ignored.
        if output.geometry.as_ref().is_some_and(has_correction) {
            problems.push(format!(
                "{prefix}.geometry has corner or center correction, which exact mode does not \
                 carry"
            ));
        }
        if !output.enable {
            continue;
        }
        if output.effective_scale().is_some_and(|scale| scale != 1.0) {
            problems.push(format!("{prefix}.scale must be 1.0 in exact mode"));
        }
        let (Some((raster_width, raster_height)), Some(geometry)) =
            (logical_raster(output), output.geometry.as_ref())
        else {
            continue;
        };
        let Ok([x, y, width, height]) = geometry.slice.pixel_rect(canvas) else {
            continue;
        };
        let one_to_one = whole(x)
            && whole(y)
            && (width - f64::from(raster_width)).abs() <= PIXEL_EPS
            && (height - f64::from(raster_height)).abs() <= PIXEL_EPS;
        if !one_to_one {
            problems.push(format!(
                "{prefix}.geometry.slice must be a whole-pixel rectangle of exactly \
                 {raster_width}×{raster_height} canvas pixels in exact mode; got {}×{} at ({}, {})",
                px(width),
                px(height),
                px(x),
                px(y)
            ));
        } else if x < -PIXEL_EPS
            || y < -PIXEL_EPS
            || x + width > canvas_width + PIXEL_EPS
            || y + height > canvas_height + PIXEL_EPS
        {
            problems.push(format!(
                "{prefix}.geometry.slice must lie inside the {}×{} canvas in exact mode; got \
                 {}×{} at ({}, {})",
                px(canvas_width),
                px(canvas_height),
                px(width),
                px(height),
                px(x),
                px(y)
            ));
        }
    }
    problems.extend(arrangement_problems(desired, Some(canvas)));
    problems
}

/// The Exact rule for `projection.arrangement`: content scale 1, and every
/// seam's overlap a whole number of pixels of the raster extent it is a
/// fraction of. Without a canvas, or when the grid cannot be measured
/// against the outputs, only the content scale can be judged.
fn arrangement_problems(desired: &DesiredState, canvas: Option<&CanvasConfig>) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(arrangement) = desired
        .projection
        .as_ref()
        .and_then(|projection| projection.arrangement)
    else {
        return problems;
    };
    if arrangement.content_scale != 1.0 {
        problems.push(format!(
            "projection.arrangement.contentScale is {}; exact mode arranges at content scale 1",
            arrangement.content_scale
        ));
    }
    let Some(canvas) = canvas else {
        return problems;
    };
    let Ok((x_seams, y_seams)) = seam_extents(
        &desired.outputs,
        canvas,
        arrangement.rows,
        arrangement.columns,
        mode_raster,
    ) else {
        return problems;
    };
    for (overlap, seams) in [
        (arrangement.overlap_x, x_seams),
        (arrangement.overlap_y, y_seams),
    ] {
        if let Some(pixels) = seams
            .into_iter()
            .filter(|extent| *extent > 0.0)
            .map(|extent| overlap * extent)
            .find(|pixels| !whole(*pixels))
        {
            problems.push(format!(
                "projection.arrangement implies a {} px overlap; exact mode arranges in whole \
                 pixels",
                px(pixels)
            ));
        }
    }
    problems
}

/// Whether `desired` would be a valid Exact-mode document as it stands,
/// whatever its `projection.mode` says: a valid canvas, every enabled output
/// with a mode and a geometry that validates against its raster, a valid
/// slice topology, and every [`rule_problems`] rule. The errors are the
/// reasons a [`Transition::Reset`] reports.
///
/// Fields Exact leaves alone (blend, gamma, black lift, filter, patterns,
/// renderer and the rest) are not judged here: an invalid one is the
/// write's own problem, which no reset could fix, and document validation
/// reports it either way.
pub fn conforms(desired: &DesiredState) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let canvas = match desired
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas.as_ref())
    {
        None => {
            errors.push("projection.canvas is required in exact mode".to_string());
            None
        }
        Some(canvas) => match canvas.dimensions() {
            Ok(_) => Some(canvas),
            Err(error) => {
                errors.push(format!("projection.canvas {error}"));
                None
            }
        },
    };
    let enabled = desired
        .outputs
        .iter()
        .filter(|output| output.enable)
        .count();
    if enabled == 0 {
        errors.push("shared canvas rendering requires at least one enabled output".into());
    }
    let mut slices = Vec::new();
    for (index, output) in desired.outputs.iter().enumerate() {
        if !output.enable {
            continue;
        }
        let prefix = format!("outputs[{index}]");
        if output.effective_mode().is_none() {
            errors.push(format!("{prefix}.mode is required for a shared canvas"));
        }
        let Some(geometry) = output.geometry.as_ref() else {
            errors.push(format!("{prefix}.geometry is required for a shared canvas"));
            continue;
        };
        let Some(canvas) = canvas else {
            continue;
        };
        match logical_raster(output) {
            Some((width, height)) => {
                if let Err(error) = geometry.validate_for_output(canvas, width, height) {
                    errors.push(format!("{prefix}.geometry {error}"));
                }
            }
            None if output.effective_mode().is_some() => {
                errors.push(format!("{prefix}.mode dimensions must be positive"));
            }
            None => {}
        }
        slices.push(geometry.slice);
    }
    if let Some(canvas) = canvas {
        if slices.len() == enabled && !slices.is_empty() {
            if let Err(error) = validate_slices(canvas, &slices) {
                errors.push(format!("projection slices {error}"));
            }
        }
        errors.extend(rule_problems(desired, canvas));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Remove what Exact mode does not carry: every output's corner pins become
/// [`UNIT_CORNERS`] and its center [`NEUTRAL_CENTER`], and
/// `projection.arrangement` is kept only when it satisfies the Exact
/// arrangement rule (content scale 1, whole-pixel seams). Nothing else
/// changes, so stripping a document that already conforms returns it
/// exactly as it was.
pub fn strip(desired: &DesiredState) -> DesiredState {
    let canvas = desired
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas.as_ref())
        .filter(|canvas| canvas.dimensions().is_ok());
    let keep_arrangement = arrangement_problems(desired, canvas).is_empty();
    let mut stripped = desired.clone();
    for output in &mut stripped.outputs {
        if let Some(geometry) = output.geometry.as_mut() {
            geometry.corners = UNIT_CORNERS;
            geometry.center = NEUTRAL_CENTER;
        }
    }
    if let Some(projection) = stripped.projection.as_mut() {
        if !keep_arrangement {
            projection.arrangement = None;
        }
    }
    stripped
}

/// The Exact-mode defaults for `desired`'s outputs.
///
/// The canvas is the whole-pixel bounding box of the enabled outputs'
/// logical rasters at their integer `position`s (aspect = width / height,
/// render width = width); each enabled output's slice is its positioned
/// rectangle, normalized, so it is 1:1 by construction, with unit pins, a
/// centered remap, and a raster footprint equal to the slice. An enabled
/// output whose effective scale is not 1 gets `scale: 1.0`. The arrangement
/// record and every output's `arrangeOffset` are dropped, and correction is
/// stripped from disabled outputs too. Everything else (blend, gamma, black
/// lift, filter, patterns, and the outputs' modes, transforms and
/// backgrounds) is untouched.
///
/// Fails when an enabled output has no mode or no position, or when the
/// bounding box is not a valid canvas.
pub fn defaults(desired: &DesiredState) -> Result<DesiredState, Vec<String>> {
    let mut errors = Vec::new();
    let mut rects = Vec::new();
    for (index, output) in desired.outputs.iter().enumerate() {
        if !output.enable {
            continue;
        }
        let prefix = format!("outputs[{index}]");
        let raster = match output.effective_mode() {
            None => {
                errors.push(format!("{prefix}.mode is required for a shared canvas"));
                None
            }
            Some(_) => {
                let raster = logical_raster(output);
                if raster.is_none() {
                    errors.push(format!("{prefix}.mode dimensions must be positive"));
                }
                raster
            }
        };
        let Some(position) = output.position else {
            errors.push(format!("{prefix}.position is required for a shared canvas"));
            continue;
        };
        if let Some((width, height)) = raster {
            rects.push((
                index,
                i64::from(position.x),
                i64::from(position.y),
                i64::from(width),
                i64::from(height),
            ));
        }
    }
    if rects.is_empty() && errors.is_empty() {
        errors.push("shared canvas rendering requires at least one enabled output".into());
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let min_x = rects.iter().map(|r| r.1).min().unwrap_or_default();
    let min_y = rects.iter().map(|r| r.2).min().unwrap_or_default();
    let max_x = rects.iter().map(|r| r.1 + r.3).max().unwrap_or_default();
    let max_y = rects.iter().map(|r| r.2 + r.4).max().unwrap_or_default();
    let (width, height) = (max_x - min_x, max_y - min_y);
    let render_width = u32::try_from(width)
        .map_err(|_| vec!["projection.canvas the outputs' bounding box is too wide".to_string()])?;
    let canvas = CanvasConfig {
        aspect: width as f64 / height as f64,
        render_width,
    };
    match canvas.dimensions() {
        Ok(dimensions) if dimensions == (render_width, height as u32) => {}
        Ok((w, h)) => {
            return Err(vec![format!(
                "projection.canvas cannot hold the outputs' {width}×{height} bounding box \
                 exactly (it would be {w}×{h})"
            )])
        }
        Err(error) => return Err(vec![format!("projection.canvas {error}")]),
    }

    let mut reset = strip(desired);
    let projection = reset.projection.get_or_insert_with(Default::default);
    projection.canvas = Some(canvas);
    projection.arrangement = None;
    // The same normalization `convert_layout_candidate` and the slicer's
    // canonical-slice recovery use: pixels over the canvas width on both
    // axes, because canvas units are isotropic.
    let unit = width as f64;
    for output in &mut reset.outputs {
        output.arrange_offset = None;
    }
    for (index, x, y, w, h) in rects {
        let output = &mut reset.outputs[index];
        let slice = CanvasRect {
            x: (x - min_x) as f64 / unit,
            y: (y - min_y) as f64 / unit,
            width: w as f64 / unit,
            height: h as f64 / unit,
        };
        output.geometry = Some(OutputGeometry {
            slice,
            corners: UNIT_CORNERS,
            center: NEUTRAL_CENTER,
            raster_footprint: slice,
        });
        if output.effective_scale() != Some(1.0) {
            output.scale = Some(1.0);
        }
    }
    Ok(reset)
}

/// Enter Exact mode: `desired` with `projection.mode` set to `exact` (a
/// projection section is created if there is none), [`strip`]ped, and kept
/// if it then [`conforms`]; otherwise replaced by its [`defaults`]. A write
/// that switches to Exact and edits geometry in the same request is judged
/// as the one merged document it produces.
///
/// Fails only when the document must be reset and the defaults cannot be
/// derived, or do not themselves conform (outputs whose positions do not
/// form one connected layout, for instance).
pub fn enter(desired: &DesiredState) -> Result<(DesiredState, Transition), Vec<String>> {
    let mut candidate = desired.clone();
    candidate
        .projection
        .get_or_insert_with(Default::default)
        .mode = ProjectionMode::Exact;
    let stripped = strip(&candidate);
    match conforms(&stripped) {
        Ok(()) => Ok((stripped, Transition::Kept)),
        Err(reasons) => {
            let reset = defaults(&stripped)?;
            conforms(&reset)?;
            Ok((reset, Transition::Reset { reasons }))
        }
    }
}

/// Leave Exact mode for `to` (Simple or Warp).
///
/// Lands on settings that render exactly as Exact did: identity pins, a
/// centered remap, scale 1, the same whole-pixel slices, and the filter
/// untouched (the integer copy makes it neutral at 1:1). An Exact document
/// already holds every one of those values, so only the mode changes. A
/// document with no projection section is returned unchanged.
pub fn leave(desired: &DesiredState, to: ProjectionMode) -> DesiredState {
    let mut left = desired.clone();
    if let Some(projection) = left.projection.as_mut() {
        projection.mode = to;
    }
    left
}

/// The integer source rectangle a near-whole-pixel slice stands for.
///
/// Validation accepts an Exact slice whose pixel rectangle is within
/// [`PIXEL_EPS`] of whole pixels, because normalized coordinates carry
/// rounding error; the slicer's integer copy needs the integers themselves.
/// The canvas planner applies this to every output that carries no
/// correction, in every mode, so a slice plans the same way in Exact and
/// after leaving it. A rectangle that is not within that tolerance on every
/// component is returned unchanged.
pub fn whole_pixel_source(source: [f64; 4]) -> [f64; 4] {
    if source
        .iter()
        .all(|value| value.is_finite() && whole(*value))
    {
        source.map(|value| value.round() + 0.0)
    } else {
        source
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arrangement, Mode, OutputMatch, Position, ProjectionConfig, Transform};

    const W: f64 = 3680.0;

    fn hd(name: &str, x: i32, y: i32) -> OutputConfig {
        let mut output = OutputConfig::new(OutputMatch::by_name(name));
        output.mode = Some(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        });
        output.position = Some(Position { x, y });
        output
    }

    fn slice_px(x: f64, y: f64, w: f64, h: f64) -> CanvasRect {
        CanvasRect {
            x: x / W,
            y: y / W,
            width: w / W,
            height: h / W,
        }
    }

    fn geometry(slice: CanvasRect) -> OutputGeometry {
        OutputGeometry {
            slice,
            corners: UNIT_CORNERS,
            center: NEUTRAL_CENTER,
            raster_footprint: slice,
        }
    }

    /// A 2x2 grid of 1920x1080 outputs with 160 px overlaps on both axes,
    /// on a 3680x2000 canvas, recorded as an Exact arrangement.
    fn grid_2x2(mode: ProjectionMode) -> DesiredState {
        let mut document = DesiredState::new();
        for (index, (x, y)) in [(0, 0), (1760, 0), (0, 920), (1760, 920)]
            .into_iter()
            .enumerate()
        {
            let mut output = hd(&format!("HDMI-{}", index + 1), x, y);
            output.geometry = Some(geometry(slice_px(
                f64::from(x),
                f64::from(y),
                1920.0,
                1080.0,
            )));
            document.outputs.push(output);
        }
        document.projection = Some(ProjectionConfig {
            mode,
            canvas: Some(CanvasConfig {
                aspect: W / 2000.0,
                render_width: 3680,
            }),
            arrangement: Some(Arrangement {
                rows: 2,
                columns: 2,
                overlap_x: 160.0 / 1920.0,
                overlap_y: 160.0 / 1080.0,
                content_scale: 1.0,
            }),
            ..ProjectionConfig::default()
        });
        document
    }

    fn errors(document: &DesiredState) -> Vec<String> {
        document.validate(true).err().unwrap_or_default()
    }

    fn json(document: &DesiredState) -> String {
        serde_json::to_string(document).unwrap()
    }

    #[test]
    fn mode_order_is_most_to_least_restrictive_with_simple_the_default() {
        let names: Vec<String> = [
            ProjectionMode::Exact,
            ProjectionMode::Simple,
            ProjectionMode::Warp,
        ]
        .iter()
        .map(|mode| {
            serde_json::to_value(mode)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
        assert_eq!(names, ["exact", "simple", "warp"]);
        assert_eq!(ProjectionMode::default(), ProjectionMode::Simple);
        assert!(ProjectionMode::Exact.requires_canvas());
        assert!(!ProjectionMode::Exact.applies_correction());
    }

    #[test]
    fn a_whole_pixel_2x2_grid_with_160_px_overlaps_validates() {
        let document = grid_2x2(ProjectionMode::Exact);
        assert_eq!(errors(&document), Vec::<String>::new());
        assert!(conforms(&document).is_ok());
    }

    #[test]
    fn a_half_pixel_slice_offset_is_refused_with_its_numbers() {
        let mut document = grid_2x2(ProjectionMode::Exact);
        document.outputs[0].geometry.as_mut().unwrap().slice.x = 0.5 / W;
        assert_eq!(
            errors(&document),
            [
                "outputs[0].geometry.slice must be a whole-pixel rectangle of exactly 1920×1080 \
                 canvas pixels in exact mode; got 1920×1080 at (0.5, 0)"
            ]
        );
        // The same rule in Simple is no rule at all.
        document.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        assert!(document.validate(true).is_ok());
    }

    #[test]
    fn a_slice_of_the_wrong_size_is_refused() {
        let mut document = grid_2x2(ProjectionMode::Exact);
        document.outputs[1].geometry.as_mut().unwrap().slice.width = 1919.5 / W;
        assert_eq!(
            errors(&document),
            [
                "outputs[1].geometry.slice must be a whole-pixel rectangle of exactly 1920×1080 \
                 canvas pixels in exact mode; got 1919.5×1080 at (1760, 0)"
            ]
        );
    }

    #[test]
    fn a_slice_hanging_off_the_canvas_is_refused() {
        let mut document = grid_2x2(ProjectionMode::Exact);
        document.outputs[3].geometry.as_mut().unwrap().slice.x = 1800.0 / W;
        assert_eq!(
            errors(&document),
            [
                "outputs[3].geometry.slice must lie inside the 3680×2000 canvas in exact mode; \
                 got 1920×1080 at (1800, 920)"
            ]
        );
    }

    #[test]
    fn a_non_unit_scale_is_refused() {
        let mut document = grid_2x2(ProjectionMode::Exact);
        document.outputs[2].scale = Some(2.0);
        assert_eq!(
            errors(&document),
            ["outputs[2].scale must be 1.0 in exact mode"]
        );
    }

    #[test]
    fn an_arrangement_at_another_content_scale_is_refused() {
        let mut document = grid_2x2(ProjectionMode::Exact);
        document
            .projection
            .as_mut()
            .unwrap()
            .arrangement
            .as_mut()
            .unwrap()
            .content_scale = 0.9;
        assert_eq!(
            errors(&document),
            ["projection.arrangement.contentScale is 0.9; exact mode arranges at content scale 1"]
        );
    }

    #[test]
    fn an_arrangement_implying_a_fractional_overlap_is_refused() {
        let mut document = grid_2x2(ProjectionMode::Exact);
        document
            .projection
            .as_mut()
            .unwrap()
            .arrangement
            .as_mut()
            .unwrap()
            .overlap_x = 160.5 / 1920.0;
        assert_eq!(
            errors(&document),
            [
                "projection.arrangement implies a 160.5 px overlap; exact mode arranges in whole \
                 pixels"
            ]
        );
    }

    #[test]
    fn an_overlap_on_an_axis_without_a_seam_is_inert() {
        let mut document = DesiredState::new();
        let mut output = hd("HDMI-1", 0, 0);
        output.geometry = Some(geometry(CanvasRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1080.0 / 1920.0,
        }));
        document.outputs.push(output);
        document.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Exact,
            canvas: Some(CanvasConfig {
                aspect: 1920.0 / 1080.0,
                render_width: 1920,
            }),
            arrangement: Some(Arrangement {
                rows: 1,
                columns: 1,
                overlap_x: 0.123,
                overlap_y: 0.456,
                content_scale: 1.0,
            }),
            ..ProjectionConfig::default()
        });
        assert_eq!(errors(&document), Vec::<String>::new());
    }

    #[test]
    fn correction_is_refused_rather_than_ignored() {
        let mut document = grid_2x2(ProjectionMode::Exact);
        document.outputs[0].geometry.as_mut().unwrap().center = [0.4, 0.6];
        assert_eq!(
            errors(&document),
            [
                "outputs[0].geometry has corner or center correction, which exact mode does not \
                 carry"
            ]
        );
    }

    #[test]
    fn a_rotated_output_validates_with_a_portrait_slice() {
        let mut document = DesiredState::new();
        let mut output = hd("HDMI-1", 0, 0);
        output.transform = Some(Transform::Rotate90);
        output.geometry = Some(geometry(CanvasRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1920.0 / 1080.0,
        }));
        document.outputs.push(output);
        document.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Exact,
            canvas: Some(CanvasConfig {
                aspect: 1080.0 / 1920.0,
                render_width: 1080,
            }),
            ..ProjectionConfig::default()
        });
        assert_eq!(errors(&document), Vec::<String>::new());

        // The landscape slice the unrotated mode would suggest is not 1:1.
        document.outputs[0].geometry.as_mut().unwrap().slice.height = 1.0;
        assert_eq!(
            errors(&document),
            [
                "outputs[0].geometry.slice must be a whole-pixel rectangle of exactly 1080×1920 \
                 canvas pixels in exact mode; got 1080×1080 at (0, 0)"
            ]
        );
    }

    #[test]
    fn exact_requires_a_canvas_by_name() {
        let mut document = grid_2x2(ProjectionMode::Exact);
        document.projection.as_mut().unwrap().canvas = None;
        assert!(errors(&document).contains(&"projection.canvas is required in exact mode".into()));
    }

    #[test]
    fn a_tiled_appliance_refuses_exact_with_one_reason() {
        let document = grid_2x2(ProjectionMode::Exact);
        assert_eq!(document.validate(false).unwrap_err(), [TILED_REASON]);
        // Even without a canvas, the tiled reason is the whole answer.
        let mut bare = DesiredState::new();
        bare.outputs.push(hd("HDMI-1", 0, 0));
        bare.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Exact,
            ..ProjectionConfig::default()
        });
        assert_eq!(bare.validate(false).unwrap_err(), [TILED_REASON]);
        assert_eq!(unavailable_reason(false), Some(TILED_REASON));
        assert_eq!(unavailable_reason(true), None);
    }

    #[test]
    fn a_conforming_simple_document_enters_as_kept_with_identical_slices() {
        let simple = grid_2x2(ProjectionMode::Simple);
        let (entered, transition) = enter(&simple).unwrap();
        assert_eq!(transition, Transition::Kept);
        assert_eq!(
            entered.projection.as_ref().unwrap().mode,
            ProjectionMode::Exact
        );
        for (before, after) in simple.outputs.iter().zip(&entered.outputs) {
            assert_eq!(before.geometry, after.geometry);
        }
        assert_eq!(
            entered.projection.as_ref().unwrap().arrangement,
            simple.projection.as_ref().unwrap().arrangement
        );
        assert!(entered.validate(true).is_ok());
    }

    #[test]
    fn entering_strips_correction_and_keeps_an_otherwise_conforming_document() {
        let mut warp = grid_2x2(ProjectionMode::Warp);
        warp.outputs[0].geometry.as_mut().unwrap().corners =
            [[0.02, 0.0], [1.0, 0.01], [0.98, 1.0], [0.0, 0.99]];
        warp.outputs[0].geometry.as_mut().unwrap().center = [0.48, 0.52];
        let (entered, transition) = enter(&warp).unwrap();
        assert_eq!(transition, Transition::Kept);
        assert_eq!(json(&entered), json(&grid_2x2(ProjectionMode::Exact)));
    }

    #[test]
    fn a_calibrated_warp_document_resets_to_the_defaults() {
        let mut warp = grid_2x2(ProjectionMode::Warp);
        for output in &mut warp.outputs {
            let geometry = output.geometry.as_mut().unwrap();
            geometry.corners = [[0.02, 0.0], [1.0, 0.01], [0.98, 1.0], [0.0, 0.99]];
            geometry.slice.x += 0.25 / W;
            geometry.slice.width -= 0.5 / W;
        }
        warp.outputs[1].arrange_offset = Some(crate::model::ArrangeOffset { x: 0.001, y: 0.0 });
        warp.projection.as_mut().unwrap().gamma = 2.4;
        let (reset, transition) = enter(&warp).unwrap();
        let Transition::Reset { reasons } = transition else {
            panic!("expected a reset, got {transition:?}");
        };
        assert_eq!(reasons.len(), 4, "{reasons:?}");
        assert!(reasons[0].starts_with("outputs[0].geometry.slice must be a whole-pixel"));
        assert!(reset.validate(true).is_ok(), "{:?}", reset.validate(true));

        let projection = reset.projection.as_ref().unwrap();
        assert_eq!(projection.mode, ProjectionMode::Exact);
        // Non-geometry fields of the write are honored.
        assert_eq!(projection.gamma, 2.4);
        // The positions' bounding box, which this fixture's positions make
        // the same 3680x2000 grid.
        assert_eq!(projection.canvas.unwrap().dimensions(), Ok((3680, 2000)));
        assert_eq!(projection.arrangement, None);
        for (output, (x, y)) in
            reset
                .outputs
                .iter()
                .zip([(0.0, 0.0), (1760.0, 0.0), (0.0, 920.0), (1760.0, 920.0)])
        {
            let geometry = output.geometry.as_ref().unwrap();
            assert_eq!(geometry.slice, slice_px(x, y, 1920.0, 1080.0));
            assert_eq!(geometry.raster_footprint, geometry.slice);
            assert_eq!(geometry.corners, UNIT_CORNERS);
            assert_eq!(geometry.center, NEUTRAL_CENTER);
            assert_eq!(output.arrange_offset, None);
            assert_eq!(output.scale, Some(1.0));
        }
    }

    #[test]
    fn a_fractional_simple_document_resets_and_fixes_its_scale() {
        let mut simple = grid_2x2(ProjectionMode::Simple);
        simple.projection.as_mut().unwrap().canvas = Some(CanvasConfig {
            aspect: W / 2000.0,
            render_width: 3000,
        });
        simple.outputs[0].scale = Some(2.0);
        let (reset, transition) = enter(&simple).unwrap();
        assert!(matches!(transition, Transition::Reset { .. }));
        assert_eq!(
            reset
                .projection
                .as_ref()
                .unwrap()
                .canvas
                .unwrap()
                .dimensions(),
            Ok((3680, 2000))
        );
        assert_eq!(reset.outputs[0].scale, Some(1.0));
        assert!(reset.validate(true).is_ok());
    }

    #[test]
    fn a_reset_without_positions_fails_naming_the_output() {
        let mut simple = grid_2x2(ProjectionMode::Simple);
        simple.projection.as_mut().unwrap().canvas = None;
        simple.outputs[2].position = None;
        assert_eq!(
            enter(&simple).unwrap_err(),
            ["outputs[2].position is required for a shared canvas"]
        );
    }

    #[test]
    fn defaults_place_a_rotated_output_by_its_portrait_raster() {
        let mut document = DesiredState::new();
        let mut portrait = hd("HDMI-1", 0, 0);
        portrait.transform = Some(Transform::Rotate270);
        document.outputs.push(portrait);
        document.outputs.push(hd("HDMI-2", 1080, 0));
        let (reset, transition) = enter(&document).unwrap();
        assert!(matches!(transition, Transition::Reset { .. }));
        let canvas = reset.projection.as_ref().unwrap().canvas.unwrap();
        assert_eq!(canvas.dimensions(), Ok((3000, 1920)));
        let rect = reset.outputs[0]
            .geometry
            .as_ref()
            .unwrap()
            .slice
            .pixel_rect(&canvas)
            .unwrap();
        assert_eq!(whole_pixel_source(rect), [0.0, 0.0, 1080.0, 1920.0]);
        assert!(reset.validate(true).is_ok(), "{:?}", reset.validate(true));
    }

    #[test]
    fn leaving_changes_only_the_mode_and_coming_back_is_byte_identical() {
        let exact = grid_2x2(ProjectionMode::Exact);
        for to in [ProjectionMode::Simple, ProjectionMode::Warp] {
            let left = leave(&exact, to);
            assert_eq!(left.projection.as_ref().unwrap().mode, to);
            assert_eq!(json(&leave(&left, ProjectionMode::Exact)), json(&exact));
            assert!(
                left.validate(true).is_ok(),
                "{to:?}: {:?}",
                left.validate(true)
            );
            let (back, transition) = enter(&left).unwrap();
            assert_eq!(transition, Transition::Kept);
            assert_eq!(json(&back), json(&exact), "{to:?}");
        }
    }

    #[test]
    fn a_reset_document_also_round_trips() {
        let mut simple = grid_2x2(ProjectionMode::Simple);
        simple.outputs[0].geometry.as_mut().unwrap().slice.x = 0.25 / W;
        let (exact, _) = enter(&simple).unwrap();
        for to in [ProjectionMode::Simple, ProjectionMode::Warp] {
            let (back, transition) = enter(&leave(&exact, to)).unwrap();
            assert_eq!(transition, Transition::Kept);
            assert_eq!(json(&back), json(&exact), "{to:?}");
        }
    }

    #[test]
    fn whole_pixel_source_rounds_only_within_tolerance() {
        assert_eq!(
            whole_pixel_source([1759.9999999999998, 0.0, 1920.0000000000002, 1080.0]),
            [1760.0, 0.0, 1920.0, 1080.0]
        );
        let fractional = [0.5, 0.0, 1920.0, 1080.0];
        assert_eq!(whole_pixel_source(fractional), fractional);
    }
}
