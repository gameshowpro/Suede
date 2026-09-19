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
use crate::model::{validate_sources, CanvasConfig, CanvasRect, OutputConfig, OutputGeometry};
use crate::state::StateStore;

/// Request body for `POST /api/v1/projection/convert`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConvertLayoutRequest {
    /// The complete configured participant roster. Disabled entries are
    /// retained in the request but do not participate in the candidate.
    pub outputs: Vec<OutputConfig>,
}

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
    pub width: u32,
    pub height: u32,
    pub achieved_scale: f64,
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
    max_dimension: 32_768,
    max_canvas_pixels: 64_000_000,
    max_output_pixels: 32_000_000,
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
        scale: 1.0,
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

/// `POST /api/v1/projection/convert` — derive a warp candidate without saving.
#[utoipa::path(
    post,
    path = "/api/v1/projection/convert",
    tag = "projection",
    request_body = ConvertLayoutRequest,
    responses(
        (status = 200, description = "Converted candidate", body = ConvertLayoutResponse),
        (status = 422, description = "The layout is incomplete or invalid")
    )
)]
pub async fn convert_layout(
    Json(body): Json<ConvertLayoutRequest>,
) -> ApiResult<Json<ConvertLayoutResponse>> {
    convert_layout_candidate(&body.outputs)
        .map(Json)
        .map_err(ApiError::Validation)
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
    recommend_for_state(&state.store, &document, generation)
        .map(Json)
        .map_err(ApiError::Validation)
}

/// Pure recommendation implementation, kept separate so tests can prove it
/// does not mutate the store or adopt a stale recommendation.
pub fn recommend_for_state(
    _store: &StateStore,
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

    let converted = convert_layout_candidate(&document.outputs).ok();
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
            let geometry = output
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
            scale: 1.0,
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
    let ideal_width_f = round_up_eight((density * 1.05).ceil().max(1.0));
    if !ideal_width_f.is_finite() || ideal_width_f >= u64::MAX as f64 {
        return Err("geometry density is too ill-conditioned to represent safely".into());
    }
    let ideal_width = ideal_width_f as u64;
    let ideal_height_f = (ideal_width_f / requested_aspect).round().max(1.0);
    if !ideal_height_f.is_finite() || ideal_height_f >= u64::MAX as f64 {
        return Err("geometry density is too ill-conditioned to represent safely".into());
    }
    let ideal_height = ideal_height_f as u64;
    let admissible_seed = ideal_width.min(u64::from(LIMITS.max_dimension)) as u32;
    let admissible_width = admissible_width(admissible_seed, requested_aspect)?;
    let admissible_height = canvas_height(admissible_width, requested_aspect)?;
    if ideal_width > u64::from(LIMITS.max_dimension) {
        warnings.push(format!(
            "ideal width was clamped to {} by the known dimension limit",
            LIMITS.max_dimension
        ));
    }
    if u64::from(admissible_width) < ideal_width {
        warnings.push(format!(
            "ideal width was clamped to {} by known canvas limits",
            admissible_width
        ));
    }
    warnings.push(
        "device and compositor maximum dimensions are unknown; no live probe was performed".into(),
    );

    let presets = [1.0, 0.75, 0.5]
        .into_iter()
        .map(|scale| {
            let width = round_up_eight((admissible_width as f64 * scale).ceil())
                .min(admissible_width as f64) as u32;
            let height = canvas_height(width, requested_aspect).unwrap_or(1);
            ScalePreset {
                scale,
                width,
                height,
                achieved_scale: width as f64 / ideal_width.max(1) as f64,
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
    if !(aspect.is_finite() && aspect > 0.0) {
        return Err("canvas aspect must be finite and positive".into());
    }
    let height = (f64::from(width) / aspect).round().max(1.0);
    if height > f64::from(LIMITS.max_dimension) {
        return Err("canvas height exceeds the known dimension limit".into());
    }
    let height = height as u32;
    if u64::from(width) * u64::from(height) > LIMITS.max_canvas_pixels {
        return Err("canvas exceeds the known pixel limit".into());
    }
    Ok(height)
}

fn admissible_width(ideal: u32, aspect: f64) -> Result<u32, String> {
    let mut width = (ideal.min(LIMITS.max_dimension) / 8) * 8;
    while width >= 8 {
        let height = canvas_height(width, aspect).unwrap_or_default();
        if height > 0 && u64::from(width) * u64::from(height) <= LIMITS.max_canvas_pixels {
            return Ok(width);
        }
        width -= 8;
    }
    Err("canvas aspect cannot fit within the known limits at an 8-pixel width".into())
}

fn round_up_eight(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 8.0;
    }
    (value.ceil() / 8.0).ceil() * 8.0
}

fn sample_density(
    warp: &crate::warp_math::Warp,
    source_width: f64,
    source_height: f64,
) -> Result<f64, String> {
    const GRID: usize = 32;
    const STEP: f64 = 1.0 / 1024.0;
    let mut max_density: f64 = 0.0;
    for iy in 0..=GRID {
        for ix in 0..=GRID {
            let u = ix as f64 / GRID as f64;
            let v = iy as f64 / GRID as f64;
            for (u, v) in [
                (u, v),
                (0.5 - 2.0 * STEP, v),
                (0.5 + 2.0 * STEP, v),
                (u, 0.5 - 2.0 * STEP),
                (u, 0.5 + 2.0 * STEP),
            ] {
                if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
                    continue;
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
                let largest =
                    ((trace + (trace * trace - 4.0 * determinant).max(0.0).sqrt()) * 0.5).sqrt();
                if largest.is_finite() {
                    max_density = max_density.max(largest);
                }
            }
        }
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
        assert_eq!(round_up_eight(100.1), 104.0);
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
                scale: 1.0,
            }),
            ..Default::default()
        });
        let identity_rec = recommend_for_state(
            &StateStore::ephemeral(std::env::temp_dir()),
            &identity_state,
            0,
        )
        .unwrap();

        let mut keystone_state = identity_state.clone();
        keystone_state.outputs = vec![keystone];
        let keystone_rec = recommend_for_state(
            &StateStore::ephemeral(std::env::temp_dir()),
            &keystone_state,
            0,
        )
        .unwrap();
        assert!(keystone_rec.ideal_width > identity_rec.ideal_width);
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
                scale: 1.0,
            }),
            ..Default::default()
        });
        let response =
            recommend_for_state(&StateStore::ephemeral(std::env::temp_dir()), &state, 9).unwrap();
        assert!(response.admissible_width <= 32768);
        assert!(u64::from(response.admissible_width) < response.ideal_width);
        assert!(response.admissible_height >= 1);
        assert!(
            u64::from(response.admissible_width) * u64::from(response.admissible_height)
                <= LIMITS.max_canvas_pixels
        );
        assert!(response.presets.iter().all(|preset| {
            preset.width <= response.admissible_width
                && preset.achieved_scale.is_finite()
                && preset.achieved_scale > 0.0
        }));
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
                scale: 1.0,
            }),
            ..Default::default()
        });
        let store = StateStore::ephemeral(std::env::temp_dir());
        let initial_generation = store.generation();
        store.set_preview(Some(preview));
        let (effective, generation) = store.effective_with_generation();
        let response = recommend_for_state(&store, &effective, generation).unwrap();
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
                scale: 1.0,
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
