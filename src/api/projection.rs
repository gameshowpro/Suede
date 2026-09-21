//! Read-only projection planning endpoints.
//!
//! The conversion endpoint deliberately accepts a complete rectangle layout
//! and returns a candidate warp document.  It never writes the working copy,
//! resizes a browser, or asks Sway to probe a limit.  The recommendation
//! endpoint follows the same rule: its answer is advisory and includes the
//! state generation on which it was calculated.

use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use utoipa::ToSchema;

use crate::api::json::Json;
use crate::error::{ApiError, ApiResult};
use crate::model::{
    validate_sources, CanvasConfig, CanvasRect, OutputConfig, OutputGeometry, ProjectionMode,
    Transform,
};

/// One converted output, addressed by its stable configuration key.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConvertedOutput {
    pub key: String,
    pub geometry: OutputGeometry,
}

/// Candidate returned by the conversion endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConvertLayoutResponse {
    pub canvas: CanvasConfig,
    pub outputs: Vec<ConvertedOutput>,
    pub warnings: Vec<String>,
}

/// Known allocation limits used by the recommendation calculation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolutionLimits {
    pub max_dimension: u32,
    pub max_canvas_pixels: u64,
    pub max_output_pixels: u64,
}

/// A UI-adoptable resolution preset.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScalePreset {
    pub scale: f64,
    pub width: u64,
    pub height: u64,
    pub achieved_scale: f64,
    /// Whether this target can be allocated within the known canvas limits.
    /// Unavailable targets remain in the response so the UI can explain and
    /// disable them instead of relabeling a clamped width as 100%.
    pub available: bool,
}

/// Read-only resolution guidance. `approximate` is always true: the sampled
/// Jacobian maximum plus an engineering margin is guidance, not a proof of the
/// exact maximum density.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecommendationResponse {
    pub revision: u64,
    pub generation: u64,
    pub requested_aspect: f64,
    pub ideal_width: u64,
    pub ideal_height: u64,
    pub admissible_width: u32,
    pub admissible_height: u32,
    pub presets: Vec<ScalePreset>,
    pub known_limits: ResolutionLimits,
    pub unknown_limits: Vec<String>,
    pub warnings: Vec<String>,
    pub approximate: bool,
}

const LIMITS: ResolutionLimits = ResolutionLimits {
    max_dimension: crate::model::limits::MAX_DIMENSION,
    max_canvas_pixels: crate::model::limits::MAX_CANVAS_PIXELS,
    max_output_pixels: crate::model::limits::MAX_OUTPUT_PIXELS,
};

/// Convert a complete simple rectangle layout into the canonical warp model.
///
/// Coordinates are first bounded in the original integer layout, so negative
/// origins and large positive origins have the same result as a layout rooted
/// at zero.  Source and raster-footprint rectangles are intentionally both
/// explicit in the result, even though the initial candidate uses the same
/// rectangle for each; later calibration may change the footprint alone.
pub fn convert_layout_candidate(outputs: &[OutputConfig]) -> Result<ConvertLayoutResponse, String> {
    let mut keys = HashSet::new();
    for output in outputs {
        let key = output.r#match.key();
        if !keys.insert(key.clone()) {
            return Err(format!("duplicate output key {key:?}"));
        }
    }
    let active: Vec<&OutputConfig> = outputs.iter().filter(|output| output.enable).collect();
    if active.is_empty() {
        return Err("at least one enabled output is required".into());
    }
    if active.len() > 8 {
        return Err("at most eight enabled outputs are supported".into());
    }

    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for output in &active {
        let mode = output.mode.ok_or_else(|| {
            format!(
                "output {} is missing an explicit mode",
                output.r#match.key()
            )
        })?;
        let position = output.position.ok_or_else(|| {
            format!(
                "output {} is missing an explicit position",
                output.r#match.key()
            )
        })?;
        if mode.width <= 0
            || mode.height <= 0
            || !mode.refresh_hz.is_finite()
            || mode.refresh_hz <= 0.0
        {
            return Err(format!(
                "output {} has invalid mode dimensions or refresh rate",
                output.r#match.key()
            ));
        }
        let width = u32::try_from(mode.width)
            .map_err(|_| format!("output {} width exceeds u32", output.r#match.key()))?;
        let height = u32::try_from(mode.height)
            .map_err(|_| format!("output {} height exceeds u32", output.r#match.key()))?;
        if width > LIMITS.max_dimension
            || height > LIMITS.max_dimension
            || u64::from(width) * u64::from(height) > LIMITS.max_output_pixels
        {
            return Err(format!(
                "output {} dimensions exceed the supported raster limits",
                output.r#match.key()
            ));
        }
        if output
            .effective_scale()
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > f64::EPSILON)
        {
            return Err(format!(
                "output {} uses a scale unsupported by warp conversion",
                output.r#match.key()
            ));
        }
        if output
            .effective_transform()
            .is_some_and(|transform| transform != crate::model::Transform::Normal)
        {
            return Err(format!(
                "output {} uses a transform unsupported by warp conversion",
                output.r#match.key()
            ));
        }
        let x = i64::from(position.x);
        let y = i64::from(position.y);
        let right = x
            .checked_add(i64::from(mode.width))
            .ok_or_else(|| format!("output {} has an overflowing x bound", output.r#match.key()))?;
        let bottom = y
            .checked_add(i64::from(mode.height))
            .ok_or_else(|| format!("output {} has an overflowing y bound", output.r#match.key()))?;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(right);
        max_y = max_y.max(bottom);
    }
    let width = max_x
        .checked_sub(min_x)
        .ok_or_else(|| "layout width overflows".to_string())?;
    let height = max_y
        .checked_sub(min_y)
        .ok_or_else(|| "layout height overflows".to_string())?;
    if width <= 0 || height <= 0 {
        return Err("layout bounds must have positive width and height".into());
    }
    let width_u32 = u32::try_from(width).map_err(|_| "layout width exceeds u32".to_string())?;
    u32::try_from(height).map_err(|_| "layout height exceeds u32".to_string())?;
    let aspect = width as f64 / height as f64;
    let canvas = CanvasConfig {
        aspect,
        render_width: width_u32,
    };
    canvas
        .dimensions()
        .map_err(|error| format!("converted canvas is invalid: {error}"))?;

    let source = |output: &OutputConfig| -> Result<CanvasRect, String> {
        let mode = output.mode.ok_or_else(|| "missing mode".to_string())?;
        let position = output
            .position
            .ok_or_else(|| "missing position".to_string())?;
        Ok(CanvasRect {
            x: (i64::from(position.x) - min_x) as f64 / width as f64,
            y: (i64::from(position.y) - min_y) as f64 / width as f64,
            width: f64::from(mode.width) / width as f64,
            height: f64::from(mode.height) / width as f64,
        })
    };

    let outputs = active
        .into_iter()
        .map(|output| {
            let source = source(output)?;
            Ok(ConvertedOutput {
                key: output.r#match.key(),
                geometry: OutputGeometry {
                    source,
                    corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                    center: [0.5, 0.5],
                    raster_footprint: source,
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    validate_sources(
        &canvas,
        &outputs
            .iter()
            .map(|output| output.geometry.source)
            .collect::<Vec<_>>(),
    )
    .map_err(|error| format!("converted source layout is invalid: {error}"))?;

    Ok(ConvertLayoutResponse {
        canvas,
        outputs,
        warnings: Vec::new(),
    })
}

/// `GET /api/v1/projection/recommendation` — estimate a useful canvas density.
#[utoipa::path(
    get,
    path = "/api/v1/projection/recommendation",
    tag = "projection",
    responses((status = 200, description = "Approximate render-resolution guidance", body = RecommendationResponse))
)]
pub async fn recommend_resolution(
    State(state): State<crate::api::ApiState>,
) -> ApiResult<Json<RecommendationResponse>> {
    let (document, generation) = state.store.effective_with_generation();
    // The Jacobian sampling below is pure CPU work (33x33 grid, a handful of
    // finite differences per point) with no I/O, but it is real work all
    // the same; keeping it off the async executor means one slow
    // recommendation request cannot stall unrelated requests sharing the
    // same worker thread.
    tokio::task::spawn_blocking(move || recommend_for_state(&document, generation))
        .await
        .map_err(|error| ApiError::Internal(format!("recommendation task panicked: {error}")))?
        .map(Json)
        .map_err(ApiError::Validation)
}

/// Pure recommendation implementation, kept separate so tests can call it
/// synchronously without going through the executor or an `ApiState`.
pub fn recommend_for_state(
    document: &crate::model::DesiredState,
    generation: u64,
) -> Result<RecommendationResponse, String> {
    let outputs: Vec<&OutputConfig> = document
        .outputs
        .iter()
        .filter(|output| output.enable)
        .collect();
    if outputs.is_empty() {
        return Err("at least one enabled output is required".into());
    }

    // The requested mode is the recommendation basis. A capability fallback
    // is deliberately not represented in DesiredState, so retaining Warp here
    // keeps the answer useful while the effective renderer is temporarily
    // Simple. A missing projection section means the shared rectangular path.
    let requested_mode = document
        .projection
        .as_ref()
        .map(|projection| projection.mode)
        .unwrap_or(ProjectionMode::Simple);

    let converted = convert_layout_candidate(&document.outputs)
        .ok()
        .or_else(|| {
            // Conversion intentionally rejects transformed/scaled outputs for
            // the Warp candidate. Simple can still estimate its shared source
            // rectangles, so retry the rectangular conversion with those
            // output-only compositor properties normalized away.
            if requested_mode != ProjectionMode::Simple {
                return None;
            }
            let mut simple_outputs = document.outputs.clone();
            for output in &mut simple_outputs {
                output.scale = Some(1.0);
                output.transform = Some(Transform::Normal);
            }
            convert_layout_candidate(&simple_outputs).ok()
        });
    let requested_aspect = document
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas.as_ref())
        .map(|canvas| canvas.aspect)
        .or_else(|| converted.as_ref().map(|candidate| candidate.canvas.aspect))
        .ok_or_else(|| "projection canvas aspect is unknown".to_string())?;
    if !(requested_aspect.is_finite() && requested_aspect > 0.0) {
        return Err("projection canvas aspect must be finite and positive".into());
    }

    let geometries: Vec<(&OutputConfig, OutputGeometry)> = outputs
        .iter()
        .map(|output| {
            let mut geometry = output
                .geometry
                .clone()
                .or_else(|| {
                    converted.as_ref().and_then(|candidate| {
                        candidate
                            .outputs
                            .iter()
                            .find(|item| item.key == output.r#match.key())
                            .map(|item| item.geometry.clone())
                    })
                })
                .ok_or_else(|| {
                    format!("output {} has no complete geometry", output.r#match.key())
                })?;
            if requested_mode == ProjectionMode::Simple {
                // Source placement/content selection is shared between the
                // two modes. Simple contributes no retained corner or center
                // correction to the sampling estimate.
                geometry.corners = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
                geometry.center = [0.5, 0.5];
            }
            Ok((*output, geometry))
        })
        .collect::<Result<Vec<_>, String>>()?;

    let mut density = 0.0_f64;
    let mut warnings = vec![
        "approximate density: sampled largest-singular Jacobian with an engineering margin; this is not an exact maximum proof".to_string(),
    ];
    for (output, geometry) in geometries {
        let mode = output.effective_mode().ok_or_else(|| {
            format!(
                "output {} is missing an effective mode",
                output.r#match.key()
            )
        })?;
        if mode.width <= 0 || mode.height <= 0 {
            return Err(format!(
                "output {} has non-positive mode dimensions",
                output.r#match.key()
            ));
        }
        let width =
            u32::try_from(mode.width).map_err(|_| "output width exceeds u32".to_string())?;
        let height =
            u32::try_from(mode.height).map_err(|_| "output height exceeds u32".to_string())?;
        let rotated = matches!(
            output.effective_transform(),
            Some(
                Transform::Rotate90
                    | Transform::Rotate270
                    | Transform::Flipped90
                    | Transform::Flipped270
            )
        );
        let (width, height) = if requested_mode == ProjectionMode::Simple && rotated {
            (height, width)
        } else {
            (width, height)
        };
        if requested_mode == ProjectionMode::Warp
            && (output
                .effective_scale()
                .is_some_and(|scale| (scale - 1.0).abs() > f64::EPSILON)
                || output
                    .effective_transform()
                    .is_some_and(|transform| transform != Transform::Normal))
        {
            return Err(format!(
                "output {} uses a scale or transform unsupported by Warp recommendation",
                output.r#match.key()
            ));
        }
        if u64::from(width) * u64::from(height) > LIMITS.max_output_pixels {
            return Err(format!(
                "output {} exceeds the known 32 megapixel output limit",
                output.r#match.key()
            ));
        }
        let corners = geometry
            .corners
            .map(|corner| [corner[0] * f64::from(width), corner[1] * f64::from(height)]);
        let warp = crate::warp_math::Warp::new(corners, geometry.center, width, height).map_err(
            |error| {
                format!(
                    "output {} has invalid geometry: {error}",
                    output.r#match.key()
                )
            },
        )?;
        // Validate source bounds against the requested aspect while keeping
        // an invalid/stale persisted render width from preventing a useful
        // read-only recommendation that can explain its clamp.
        let validation_canvas = CanvasConfig {
            aspect: requested_aspect,
            render_width: 1,
        };
        geometry
            .validate_for_output(&validation_canvas, width, height)
            .map_err(|error| {
                format!(
                    "output {} has invalid geometry: {error}",
                    output.r#match.key()
                )
            })?;
        let source = geometry.source;
        if !(source.width.is_finite()
            && source.height.is_finite()
            && source.width > 0.0
            && source.height > 0.0)
        {
            return Err(format!(
                "output {} has invalid source dimensions",
                output.r#match.key()
            ));
        }
        density = density.max(sample_density(&warp, source.width, source.height)?);
    }
    if !density.is_finite() || density <= 0.0 {
        return Err("geometry has no finite positive sampling density".into());
    }

    // Five percent is an engineering margin for finite-difference truncation
    // and variation between the 33×33 grid points. It is not a mathematical
    // bound on the unsampled maximum.
    // The density estimator uses canonical isotropic canvas units. Account
    // for the actual rounded integer height when choosing the width: for an
    // odd aspect/height pair, `round(width / aspect)` can otherwise leave the
    // y density just below the sampled target.
    let target_density = (density * 1.05).ceil().max(1.0);
    let ideal_width_f = width_for_density(target_density, requested_aspect)?;
    if !ideal_width_f.is_finite() || ideal_width_f >= u64::MAX as f64 {
        return Err("geometry density is too ill-conditioned to represent safely".into());
    }
    let ideal_width = ideal_width_f as u64;
    let ideal_height_f = round_positive(ideal_width_f / requested_aspect).max(1.0);
    if !ideal_height_f.is_finite() || ideal_height_f >= u64::MAX as f64 {
        return Err("geometry density is too ill-conditioned to represent safely".into());
    }
    let ideal_height = ideal_height_f as u64;
    let admissible_width = admissible_width(requested_aspect)?;
    let admissible_height = canvas_height(admissible_width, requested_aspect)?;
    if ideal_width > u64::from(LIMITS.max_dimension) {
        warnings.push(format!(
            "ideal width exceeds the known dimension limit of {}",
            LIMITS.max_dimension
        ));
    }
    if u64::from(admissible_width) < ideal_width {
        warnings.push(format!(
            "ideal width exceeds the known admissible canvas width of {}",
            admissible_width
        ));
    }
    warnings.push(
        "device and compositor maximum dimensions are unknown; no live probe was performed".into(),
    );

    let presets = [0.25, 0.5, 0.75, 1.0]
        .into_iter()
        .map(|scale| {
            let width = round_positive(ideal_width as f64 * scale).max(1.0) as u64;
            let height = round_positive(width as f64 / requested_aspect).max(1.0) as u64;
            let available = width <= u64::from(LIMITS.max_dimension)
                && height <= u64::from(LIMITS.max_dimension)
                && width.saturating_mul(height) <= LIMITS.max_canvas_pixels;
            ScalePreset {
                scale,
                width,
                height,
                achieved_scale: width as f64 / ideal_width.max(1) as f64,
                available,
            }
        })
        .collect();

    Ok(RecommendationResponse {
        revision: document.revision,
        generation,
        requested_aspect,
        ideal_width,
        ideal_height,
        admissible_width,
        admissible_height,
        presets,
        known_limits: LIMITS,
        unknown_limits: vec!["device/compositor allocation limit".into()],
        warnings,
        approximate: true,
    })
}

fn canvas_height(width: u32, aspect: f64) -> Result<u32, String> {
    let height = canvas_height_unbounded(u64::from(width), aspect)?;
    if height > u64::from(LIMITS.max_dimension) {
        return Err("canvas height exceeds the known dimension limit".into());
    }
    if u64::from(width).saturating_mul(height) > LIMITS.max_canvas_pixels {
        return Err("canvas exceeds the known pixel limit".into());
    }
    u32::try_from(height).map_err(|_| "canvas height exceeds u32".into())
}

fn canvas_height_unbounded(width: u64, aspect: f64) -> Result<u64, String> {
    if !(aspect.is_finite() && aspect > 0.0) {
        return Err("canvas aspect must be finite and positive".into());
    }
    let height = round_positive(width as f64 / aspect).max(1.0);
    if !height.is_finite() || height >= u64::MAX as f64 {
        return Err("canvas height is too large to represent safely".into());
    }
    Ok(height as u64)
}

fn admissible_width(aspect: f64) -> Result<u32, String> {
    // `canvas_height(width, aspect)` is monotonic in `width` for a fixed
    // positive aspect: the derived height is non-decreasing, so both the
    // dimension-limit and pixel-count checks it applies, once tripped, stay
    // tripped for every larger width. The admissible widths are therefore a
    // prefix of `1..=MAX_DIMENSION`, and its end can be found with a binary
    // search instead of the up-to-32768-step linear scan this replaces.
    let ok = |width: u32| canvas_height(width, aspect).is_ok();
    if !ok(1) {
        return Err("canvas aspect cannot fit within the known limits".into());
    }
    let (mut lo, mut hi) = (1u32, LIMITS.max_dimension);
    while lo < hi {
        // Bias the midpoint up so `lo` always advances, even when
        // `hi == lo + 1`.
        let mid = lo + (hi - lo).div_ceil(2);
        if ok(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Ok(lo)
}

fn width_for_density(target_density: f64, aspect: f64) -> Result<f64, String> {
    if !(target_density.is_finite() && target_density > 0.0) {
        return Err("geometry density is not finite and positive".into());
    }
    if !(aspect.is_finite() && aspect > 0.0) {
        return Err("canvas aspect must be finite and positive".into());
    }
    // For positive half-up rounding, round(width / aspect) >= h exactly when
    // width >= aspect * (h - 0.5). Calculate the minimum required integer
    // height directly instead of incrementing width: at very large values an
    // f64 cannot represent width + 1, and an incremental loop could stall.
    let required_height = (target_density / aspect).ceil().max(1.0);
    // Canvas height is at least one even below the first rounding boundary.
    let y_width = if required_height == 1.0 {
        1.0
    } else {
        (aspect * (required_height - 0.5)).ceil().max(1.0)
    };
    let width = target_density.ceil().max(y_width).max(1.0);
    if !width.is_finite() || width >= u64::MAX as f64 {
        return Err("geometry density is too ill-conditioned to represent safely".into());
    }
    let height = round_positive(width / aspect).max(1.0);
    if height < required_height {
        // This guard handles a boundary lost to floating-point rounding. It
        // is a fixed correction, with a representability check, rather than a
        // potentially unbounded search.
        let corrected = (aspect * (required_height + 0.5)).ceil().max(width);
        if !corrected.is_finite()
            || corrected >= u64::MAX as f64
            || round_positive(corrected / aspect) < required_height
        {
            return Err("geometry density is too ill-conditioned to represent safely".into());
        }
        return Ok(corrected);
    }
    Ok(width)
}

/// Round a positive value to the nearest integer, with exact half values
/// rounded upward. The recommendation uses this for preset dimensions and
/// derived canvas heights; it never imposes an 8-pixel allocation alignment.
fn round_positive(value: f64) -> f64 {
    // Preserve overflow so callers reject it; infinity is not a one-pixel
    // canvas. Rust's positive round implements ties upward without addition.
    value.round().max(1.0)
}

fn sample_density(
    warp: &crate::warp_math::Warp,
    source_width: f64,
    source_height: f64,
) -> Result<f64, String> {
    const GRID: usize = 32;
    const STEP: f64 = 1.0 / 1024.0;
    let mut max_density: f64 = 0.0;
    // A single point's contribution to `max_density`, shared by the main
    // grid and the center-seam probes below so neither has to repeat the
    // other's work.
    let mut probe = |u: f64, v: f64| -> Result<(), String> {
        if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
            return Ok(());
        }
        let du = derivative_axis(warp, u, v, true)?;
        let dv = derivative_axis(warp, u, v, false)?;
        let j00 = du[0] / source_width;
        let j10 = du[1] / source_width;
        let j01 = dv[0] / source_height;
        let j11 = dv[1] / source_height;
        let a = j00 * j00 + j10 * j10;
        let b = j00 * j01 + j10 * j11;
        let c = j01 * j01 + j11 * j11;
        let trace = a + c;
        let determinant = (a * c - b * b).max(0.0);
        let largest = ((trace + (trace * trace - 4.0 * determinant).max(0.0).sqrt()) * 0.5).sqrt();
        if largest.is_finite() {
            max_density = max_density.max(largest);
        }
        Ok(())
    };
    for iy in 0..=GRID {
        let v = iy as f64 / GRID as f64;
        // The horizontal center-seam neighborhood only depends on this row's
        // `v`, so it is sampled once per row here rather than once per
        // (row, column) grid point below.
        probe(0.5 - 2.0 * STEP, v)?;
        probe(0.5 + 2.0 * STEP, v)?;
        for ix in 0..=GRID {
            let u = ix as f64 / GRID as f64;
            probe(u, v)?;
        }
    }
    for ix in 0..=GRID {
        let u = ix as f64 / GRID as f64;
        // Likewise, the vertical center-seam neighborhood only depends on
        // this column's `u`: once per column, not once per grid point.
        probe(u, 0.5 - 2.0 * STEP)?;
        probe(u, 0.5 + 2.0 * STEP)?;
    }
    (max_density.is_finite() && max_density > 0.0)
        .then_some(max_density)
        .ok_or_else(|| "geometry has no finite Jacobian samples".into())
}

fn derivative_axis(
    warp: &crate::warp_math::Warp,
    u: f64,
    v: f64,
    horizontal: bool,
) -> Result<[f64; 2], String> {
    // Evaluate both sides of the center-remap break. At the endpoints use a
    // one-sided difference; elsewhere use a centered finite difference.
    let h = 1.0 / 1024.0;
    let mut left = if horizontal { u - h } else { v - h };
    let mut right = if horizontal { u + h } else { v + h };
    if horizontal && (u - 0.5).abs() <= h {
        left = 0.5 - h;
        right = 0.5 + h;
    }
    if !horizontal && (v - 0.5).abs() <= h {
        left = 0.5 - h;
        right = 0.5 + h;
    }
    if left < 0.0 {
        left = 0.0;
    }
    if right > 1.0 {
        right = 1.0;
    }
    if (right - left).abs() < f64::EPSILON {
        return Err("finite-difference sample collapsed".into());
    }
    let a = if horizontal {
        warp.destination_at(left, v)
    } else {
        warp.destination_at(u, left)
    };
    let b = if horizontal {
        warp.destination_at(right, v)
    } else {
        warp.destination_at(u, right)
    };
    let (a, b) = a
        .zip(b)
        .ok_or_else(|| "warp Jacobian sample is undefined".to_string())?;
    Ok([
        (b[0] - a[0]) / (right - left),
        (b[1] - a[1]) / (right - left),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mode, OutputMatch, Position, ProjectionMode};
    use crate::state::StateStore;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn output(key: &str, x: i32, y: i32, width: i32, height: i32, enable: bool) -> OutputConfig {
        let mut output = OutputConfig::new(OutputMatch::by_name(key));
        output.enable = enable;
        output.mode = Some(Mode {
            width,
            height,
            refresh_hz: 60.0,
        });
        output.position = Some(Position { x, y });
        output
    }

    #[test]
    fn conversion_handles_negative_origins_and_disabled_outputs() {
        let response = convert_layout_candidate(&[
            output("A", -100, -20, 1000, 1000, true),
            output("B", 900, -20, 1000, 1000, true),
            output("disabled", 0, 0, 200, 200, false),
        ])
        .unwrap();
        assert_eq!(response.canvas.render_width, 2000);
        assert_eq!(response.outputs.len(), 2);
        assert_eq!(response.outputs[0].geometry.source.x, 0.0);
        assert_eq!(response.outputs[1].geometry.source.x, 0.5);
        assert_eq!(
            response.outputs[0].geometry.corners,
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
        );
    }

    #[test]
    fn conversion_requires_complete_enabled_participants() {
        let mut missing = output("A", 0, 0, 100, 100, true);
        missing.position = None;
        assert!(convert_layout_candidate(&[missing]).is_err());
        assert!(convert_layout_candidate(&[output("disabled", 0, 0, 0, 0, false)]).is_err());
    }

    #[test]
    fn conversion_rejects_duplicate_keys_and_disconnected_sources() {
        let a = output("A", 0, 0, 100, 100, true);
        let duplicate = output("A", 100, 0, 100, 100, false);
        assert!(convert_layout_candidate(&[a.clone(), duplicate]).is_err());
        let gap = output("B", 110, 0, 100, 100, true);
        assert!(convert_layout_candidate(&[a, gap]).is_err());
    }

    #[test]
    fn canvas_height_uses_positive_rounding_for_fractional_aspect() {
        assert_eq!(canvas_height(100, 1.6).unwrap(), 63);
        assert_eq!(canvas_height(100, 3.0).unwrap(), 33);
        assert_eq!(round_positive(100.4), 100.0);
        assert_eq!(round_positive(100.5), 101.0);
        assert_eq!(width_for_density(105.84, 1.6).unwrap(), 107.0);
        assert!(width_for_density(1.0e20, 1.6).is_err());
        assert_eq!(width_for_density(100.0, 1.0e9).unwrap(), 100.0);
        assert!(canvas_height_unbounded(100, 1.0e-320).is_err());
    }

    #[test]
    fn center_remap_samples_both_side_slopes() {
        let warp = crate::warp_math::Warp::new(
            [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
            [0.25, 0.75],
            100,
            100,
        )
        .unwrap();
        let h = 1.0 / 1024.0;
        let left = derivative_axis(&warp, 0.5 - 2.0 * h, 0.5, true).unwrap();
        let right = derivative_axis(&warp, 0.5 + 2.0 * h, 0.5, true).unwrap();
        assert!((left[0] - 50.0).abs() < 1e-6, "left slope: {left:?}");
        assert!((right[0] - 150.0).abs() < 1e-6, "right slope: {right:?}");
        let left_y = derivative_axis(&warp, 0.5, 0.5 - 2.0 * h, false).unwrap();
        let right_y = derivative_axis(&warp, 0.5, 0.5 + 2.0 * h, false).unwrap();
        assert!((left_y[1] - 150.0).abs() < 1e-6, "left y slope: {left_y:?}");
        assert!(
            (right_y[1] - 50.0).abs() < 1e-6,
            "right y slope: {right_y:?}"
        );
    }

    #[test]
    fn recommendation_detects_a_densest_keystone_region() {
        let mut identity = output("identity", 0, 0, 100, 100, true);
        identity.geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        });
        let mut keystone = identity.clone();
        keystone.r#match = OutputMatch::by_name("keystone");
        keystone.geometry.as_mut().unwrap().corners =
            [[0.0, 0.0], [1.0, 0.0], [1.45, 1.0], [0.0, 1.0]];

        let mut identity_state = crate::model::DesiredState::new();
        identity_state.outputs = vec![identity];
        identity_state.projection = Some(crate::model::ProjectionConfig {
            mode: ProjectionMode::Warp,
            canvas: Some(CanvasConfig {
                aspect: 1.0,
                render_width: 100,
            }),
            ..Default::default()
        });
        let identity_rec = recommend_for_state(&identity_state, 0).unwrap();

        let mut keystone_state = identity_state.clone();
        keystone_state.outputs = vec![keystone];
        let keystone_rec = recommend_for_state(&keystone_state, 0).unwrap();
        assert!(keystone_rec.ideal_width > identity_rec.ideal_width);
    }

    #[test]
    fn simple_recommendation_uses_identity_correction_with_shared_source() {
        let mut configured = output("simple", 0, 0, 160, 90, true);
        configured.geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.125,
                y: 0.0,
                width: 0.5,
                height: 0.5,
            },
            // Retained Warp calibration must not affect a requested Simple
            // recommendation. The source rectangle remains shared.
            corners: [[0.0, 0.0], [1.0, 0.0], [1.4, 1.0], [0.0, 1.0]],
            center: [0.25, 0.75],
            raster_footprint: CanvasRect {
                x: 0.125,
                y: 0.0,
                width: 0.5,
                height: 0.5,
            },
        });
        let mut state = crate::model::DesiredState::new();
        state.outputs = vec![configured.clone()];
        state.projection = Some(crate::model::ProjectionConfig {
            mode: ProjectionMode::Simple,
            canvas: Some(CanvasConfig {
                aspect: 16.0 / 9.0,
                render_width: 101,
            }),
            ..Default::default()
        });
        let simple = recommend_for_state(&state, 0).unwrap();

        configured.geometry.as_mut().unwrap().corners =
            [[0.0, 0.0], [1.0, 0.0], [1.4, 1.0], [0.0, 1.0]];
        state.outputs[0] = configured;
        state.projection.as_mut().unwrap().mode = ProjectionMode::Warp;
        let warp = recommend_for_state(&state, 0).unwrap();
        assert!(warp.ideal_width > simple.ideal_width);
    }

    #[test]
    fn recommendation_is_invariant_to_render_width_with_rounded_height() {
        let mut configured = output("odd", 0, 0, 160, 63, true);
        configured.geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0 / 1.6,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0 / 1.6,
            },
        });
        let mut state = crate::model::DesiredState::new();
        state.outputs = vec![configured];
        state.projection = Some(crate::model::ProjectionConfig {
            mode: ProjectionMode::Simple,
            canvas: Some(CanvasConfig {
                aspect: 1.6,
                render_width: 100,
            }),
            ..Default::default()
        });
        let first = recommend_for_state(&state, 1).unwrap();
        state
            .projection
            .as_mut()
            .unwrap()
            .canvas
            .as_mut()
            .unwrap()
            .render_width = 101;
        let second = recommend_for_state(&state, 2).unwrap();
        assert_eq!(first.ideal_width, second.ideal_width);
        assert_eq!(first.ideal_height, second.ideal_height);
    }

    #[test]
    fn simple_recommendation_uses_rotated_physical_raster_dimensions() {
        let mut normal = output("normal", 0, 0, 160, 80, true);
        normal.geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 0.5,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 0.5,
            },
        });
        let mut rotated = normal.clone();
        rotated.r#match = OutputMatch::by_name("rotated");
        rotated.scale = Some(2.0);
        rotated.transform = Some(Transform::Rotate90);
        let mut normal_state = crate::model::DesiredState::new();
        normal_state.outputs = vec![normal];
        normal_state.projection = Some(crate::model::ProjectionConfig {
            mode: ProjectionMode::Simple,
            canvas: Some(CanvasConfig {
                aspect: 2.0,
                render_width: 100,
            }),
            ..Default::default()
        });
        let mut rotated_state = crate::model::DesiredState {
            outputs: vec![rotated],
            ..normal_state.clone()
        };
        let normal_rec = recommend_for_state(&normal_state, 0).unwrap();
        let rotated_rec = recommend_for_state(&rotated_state, 0).unwrap();
        assert!(rotated_rec.ideal_width > normal_rec.ideal_width);

        rotated_state.outputs[0].scale = Some(1.0);
        let rotated_without_scale = recommend_for_state(&rotated_state, 0).unwrap();
        assert_eq!(rotated_rec.ideal_width, rotated_without_scale.ideal_width);

        let mut converted_rotated = rotated_state.clone();
        converted_rotated.outputs[0].geometry = None;
        let converted = recommend_for_state(&converted_rotated, 0).unwrap();
        assert_eq!(rotated_without_scale.ideal_width, converted.ideal_width);
    }

    #[test]
    fn recommendation_clamps_dimensions_and_reports_achieved_presets() {
        let mut wide = output("wide", 0, 0, 32768, 900, true);
        wide.geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 900.0 / 32768.0,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 900.0 / 32768.0,
            },
        });
        let mut state = crate::model::DesiredState::new();
        state.outputs = vec![wide];
        state.projection = Some(crate::model::ProjectionConfig {
            mode: ProjectionMode::Warp,
            canvas: Some(CanvasConfig {
                aspect: 1.0,
                render_width: 32768,
            }),
            ..Default::default()
        });
        let response = recommend_for_state(&state, 9).unwrap();
        assert!(response.admissible_width <= 32768);
        assert!(u64::from(response.admissible_width) < response.ideal_width);
        assert!(response.admissible_height >= 1);
        assert!(
            u64::from(response.admissible_width) * u64::from(response.admissible_height)
                <= LIMITS.max_canvas_pixels
        );
        assert!(response
            .presets
            .iter()
            .all(|preset| { preset.achieved_scale.is_finite() && preset.achieved_scale > 0.0 }));
        assert_eq!(
            response
                .presets
                .iter()
                .map(|preset| preset.scale)
                .collect::<Vec<_>>(),
            vec![0.25, 0.5, 0.75, 1.0]
        );
        assert!(response.presets.iter().any(|preset| !preset.available));
        assert_eq!(response.generation, 9);
    }

    #[test]
    fn recommendation_uses_preview_generation_without_mutating_store() {
        let mut preview = crate::model::DesiredState::new();
        preview.outputs = vec![output("preview", 0, 0, 100, 100, true)];
        preview.outputs[0].geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        });
        preview.projection = Some(crate::model::ProjectionConfig {
            mode: ProjectionMode::Warp,
            canvas: Some(CanvasConfig {
                aspect: 1.0,
                render_width: 100,
            }),
            ..Default::default()
        });
        let store = StateStore::ephemeral(std::env::temp_dir());
        let initial_generation = store.generation();
        store.set_preview(Some(preview));
        let (effective, generation) = store.effective_with_generation();
        let response = recommend_for_state(&effective, generation).unwrap();
        assert_eq!(response.revision, 0);
        assert!(response.generation > initial_generation);
        assert!(store.get().projection.is_none());
        assert_eq!(store.generation(), response.generation);
    }

    #[tokio::test]
    async fn recommendation_handler_is_read_only_and_reports_preview_basis() {
        let harness = crate::api::test_support::harness(None);
        let mut preview = crate::model::DesiredState::new();
        let mut configured = OutputConfig::new(OutputMatch::by_name("preview"));
        configured.mode = Some(Mode {
            width: 100,
            height: 100,
            refresh_hz: 60.0,
        });
        configured.position = Some(Position { x: 0, y: 0 });
        configured.geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        });
        preview.outputs = vec![configured];
        preview.projection = Some(crate::model::ProjectionConfig {
            mode: ProjectionMode::Warp,
            canvas: Some(CanvasConfig {
                aspect: 1.0,
                render_width: 100,
            }),
            ..Default::default()
        });
        harness.state.store.set_preview(Some(preview));
        let revision = harness.state.store.revision();
        let generation = harness.state.store.generation();
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projection/recommendation")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["revision"], revision);
        assert_eq!(body["generation"], generation);
        assert_eq!(harness.state.store.revision(), revision);
        assert_eq!(harness.state.store.generation(), generation);
    }
}
