//! Read-only projection planning endpoints.
//!
//! The conversion endpoint deliberately accepts a complete rectangle layout
//! and returns a candidate warp document.  It never writes the working copy,
//! resizes a browser, or asks Sway to probe a limit.  The recommendation
//! endpoint follows the same rule: its answer is advisory and includes the
//! state generation on which it was calculated.

use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use utoipa::{IntoParams, ToSchema};

use crate::api::json::Json;
use crate::error::{ApiError, ApiResult};
use crate::model::{
    validate_slices, ArrangedOutput, Arrangement, ArrangementLimits, ArrangementRequest,
    CanvasConfig, CanvasRect, OutputConfig, OutputGeometry, Overhang, ProjectionMode, Renderer,
    ResampleFilter, Transform, UnusedCanvas,
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

/// Read-only resolution guidance. `approximate` is false only when every
/// enabled output is an axis-aligned rectangle, whose density is computed
/// exactly. Otherwise the sampled Jacobian maximum plus an engineering margin
/// is guidance, not a proof of the exact maximum density.
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
    /// Every enabled output whose whole-pixel slice extends past the
    /// document's current canvas, as in the projection status; each
    /// `message` is also among `warnings`. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_canvas: Vec<crate::model::SliceOverhang>,
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
/// at zero.  The slice and raster-footprint rectangles are intentionally both
/// explicit in the result, even though the initial candidate uses the same
/// rectangle for each; later calibration may change the footprint alone.
///
/// The candidate's rectangles are fractions of the canvas
/// (`sliceUnits: "percent"`). Written as they are onto a document with no
/// canvas, they are stored in pixels (see `projection.sliceUnits`).
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
        // Positions are compositor (logical) coordinates, so the raster
        // each output covers is the as-viewed one: swapped for 90/270.
        let (logical_width, logical_height) = output
            .effective_transform()
            .unwrap_or_default()
            .physical_size((width, height));
        let x = i64::from(position.x);
        let y = i64::from(position.y);
        let right = x
            .checked_add(i64::from(logical_width))
            .ok_or_else(|| format!("output {} has an overflowing x bound", output.r#match.key()))?;
        let bottom = y
            .checked_add(i64::from(logical_height))
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

    let slice = |output: &OutputConfig| -> Result<CanvasRect, String> {
        let mode = output.mode.ok_or_else(|| "missing mode".to_string())?;
        let position = output
            .position
            .ok_or_else(|| "missing position".to_string())?;
        let (logical_width, logical_height) = output
            .effective_transform()
            .unwrap_or_default()
            .physical_size((mode.width as u32, mode.height as u32));
        Ok(CanvasRect {
            x: (i64::from(position.x) - min_x) as f64 / width as f64,
            y: (i64::from(position.y) - min_y) as f64 / width as f64,
            width: f64::from(logical_width) / width as f64,
            height: f64::from(logical_height) / width as f64,
        })
    };

    let outputs = active
        .into_iter()
        .map(|output| {
            let slice = slice(output)?;
            Ok(ConvertedOutput {
                key: output.r#match.key(),
                geometry: OutputGeometry {
                    slice,
                    corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                    center: [0.5, 0.5],
                    raster_footprint: slice,
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    validate_slices(
        &canvas,
        &outputs
            .iter()
            .map(|output| output.geometry.slice)
            .collect::<Vec<_>>(),
    )
    .map_err(|error| format!("converted slice layout is invalid: {error}"))?;

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

/// Query form of [`ArrangementRequest`], for the read-only dry run: the same
/// fields minus `committed`, which has no meaning for a solve that never
/// writes.
#[derive(Debug, Clone, Copy, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementQuery {
    pub rows: u32,
    pub columns: u32,
    #[serde(default)]
    pub overlap_x: Option<f64>,
    #[serde(default)]
    pub overlap_y: Option<f64>,
    #[serde(default)]
    pub content_scale: Option<f64>,
    /// As [`ArrangementRequest::allow_unused_canvas`]: default `false` means
    /// overlaps cover the whole canvas, overhanging it where they must, and
    /// a content-scale band is a `422` here too, not just on the `PUT` that
    /// would write it — a client previewing a rearrangement sees the same
    /// answer or refusal it would get from committing it. `true` fits the
    /// grid inside the canvas instead and reports the band.
    #[serde(default)]
    pub allow_unused_canvas: bool,
}

impl From<ArrangementQuery> for ArrangementRequest {
    fn from(query: ArrangementQuery) -> Self {
        Self {
            rows: query.rows,
            columns: query.columns,
            overlap_x: query.overlap_x,
            overlap_y: query.overlap_y,
            content_scale: query.content_scale,
            allow_unused_canvas: query.allow_unused_canvas,
            committed: false,
        }
    }
}

/// A solved grid arrangement, without writing it: [`crate::model::ArrangementSolution`]'s
/// fields, copied rather than `#[serde(flatten)]`'d — [`Arrangement`] denies
/// unknown fields, and serde cannot combine that with `flatten` — plus the
/// document identity it was solved against, so a client can tell whether its
/// next write should still expect to land cleanly — and, for an overlap
/// request, the overlap range each axis can take from here.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementDryRun {
    /// The resolved five values. In overlap mode `overlapX` and `overlapY`
    /// are exactly the requested ones: the solver never changes an overlap.
    pub arrangement: Arrangement,
    pub unused_canvas: UnusedCanvas,
    #[serde(default)]
    pub overhang: Overhang,
    pub implied_aspect: f64,
    /// Per-axis overlap limits for this grid on this canvas, each holding
    /// the other axis at its requested value; see
    /// [`crate::model::limits`]. Computed against the same document the
    /// solve used. Omitted for a `contentScale` request, which has no
    /// overlaps to limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ArrangementLimits>,
    pub outputs: Vec<ArrangedOutput>,
    pub warnings: Vec<String>,
    /// Exact mode only: the canvas the arrangement derives, which applying
    /// it writes to `projection.canvas`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<crate::model::CanvasConfig>,
    /// Exact mode only: the horizontal overlap in whole pixels after
    /// rounding the requested `overlapX` to the nearest whole pixel (never
    /// below 0) of the seam extent it is a fraction of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap_x_px: Option<u32>,
    /// Exact mode only: as `overlapXPx`, vertically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap_y_px: Option<u32>,
    /// The unit `outputs[].slice` is in: the document's
    /// `projection.sliceUnits` (`pixels` in Exact mode), which applying the
    /// arrangement writes them in.
    #[serde(default)]
    pub slice_units: crate::model::SliceUnits,
    pub revision: u64,
    pub generation: u64,
}

/// `GET /api/v1/projection/arrangement` — solve a grid arrangement without
/// writing it, exactly like [`recommend_resolution`]: reads the effective
/// document and changes nothing. Pure arithmetic over a handful of outputs,
/// so unlike the recommendation endpoint this needs no `spawn_blocking` —
/// the limits scan included, which re-runs the same placement a few hundred
/// times at most.
#[utoipa::path(
    get,
    path = "/api/v1/projection/arrangement",
    tag = "projection",
    params(ArrangementQuery),
    responses(
        (status = 200, description = "The solved arrangement; nothing is written", body = ArrangementDryRun),
        (status = 422, description = "The grid does not fit the outputs, an output \
                       has no known raster, the canvas is missing, the \
                       request is over- or under-determined, an overlap is \
                       outside [0, 1), or an edge slice would fall entirely \
                       outside the canvas"),
    )
)]
pub async fn get_arrangement(
    State(state): State<crate::api::ApiState>,
    Query(query): Query<ArrangementQuery>,
) -> ApiResult<Json<ArrangementDryRun>> {
    let (document, generation) = state.store.effective_with_generation();
    let request: ArrangementRequest = query.into();
    let raster = crate::api::raster_lookup(&state.snapshot);
    let solution =
        crate::model::solve_document(&document, &request, &raster).map_err(ApiError::Validation)?;
    // Only an overlap request has overlaps to limit. Once its solve has
    // succeeded the limits cannot fail for any reason the solve did not
    // already report, but they are advisory either way: a failure omits
    // them rather than turning a good dry run into an error.
    let limits = if request.content_scale.is_none() {
        crate::model::limits_document(&document, &request, &raster).ok()
    } else {
        None
    };
    Ok(Json(ArrangementDryRun {
        arrangement: solution.arrangement,
        unused_canvas: solution.unused_canvas,
        overhang: solution.overhang,
        implied_aspect: solution.implied_aspect,
        limits,
        outputs: solution.outputs,
        warnings: solution.warnings,
        canvas: solution.canvas,
        overlap_x_px: solution.overlap_x_px,
        overlap_y_px: solution.overlap_y_px,
        slice_units: solution.slice_units,
        revision: document.revision,
        generation,
    }))
}

/// What switching to Exact would do to the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum ExactTransitionAction {
    /// The layout is kept (stripped of correction); also reported when the
    /// document is already in Exact and a switch would change nothing.
    Keep,
    /// The layout is replaced by the Exact defaults.
    Reset,
    /// Exact cannot be used on this appliance; the switch would be refused
    /// with `reasons[0]`.
    Unavailable,
}

/// The answer to `GET /api/v1/projection/exact-transition`: what a switch to
/// Exact mode would do, with the document identity it was computed against.
/// No document is returned.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExactTransitionResponse {
    pub action: ExactTransitionAction,
    /// For `reset`, the Exact rules the current layout breaks; for
    /// `unavailable`, the single reason; empty for `keep`.
    pub reasons: Vec<String>,
    /// Revision and generation of the effective document (the preview when
    /// one is active, else the working copy) the prediction was made on,
    /// like the recommendation and arrangement dry runs.
    pub revision: u64,
    pub generation: u64,
}

/// `GET /api/v1/projection/exact-transition` — predict what `PUT
/// /config/projection {"mode":"exact"}` would do to the layout, without
/// writing: keep it or reset it to the Exact defaults. Runs the real switch
/// ([`crate::projection_policy::transition`]) on a clone of the effective
/// document, so the answer cannot drift from the write.
#[utoipa::path(
    get,
    path = "/api/v1/projection/exact-transition",
    tag = "projection",
    responses(
        (status = 200, description = "What switching to exact mode would do; nothing is written", body = ExactTransitionResponse),
        (status = 422, description = "The layout would be reset but the Exact defaults cannot be derived (an enabled output has no mode or position)"),
    )
)]
pub async fn get_exact_transition(
    State(state): State<crate::api::ApiState>,
) -> ApiResult<Json<ExactTransitionResponse>> {
    use crate::projection_policy::{exact_dry_run, ExactDryRun};
    let (document, generation) = state.store.effective_with_generation();
    let (action, reasons) = match exact_dry_run(&document, state.bootstrap.allow_overlaps) {
        Ok(ExactDryRun::AlreadyExact | ExactDryRun::Keep) => {
            (ExactTransitionAction::Keep, Vec::new())
        }
        Ok(ExactDryRun::Reset { reasons }) => (ExactTransitionAction::Reset, reasons),
        Ok(ExactDryRun::Unavailable { reason }) => {
            (ExactTransitionAction::Unavailable, vec![reason])
        }
        Err(errors) => {
            return Err(ApiError::Validation(format!(
                "cannot reset to the exact-mode defaults: {}",
                errors.join("; ")
            )))
        }
    };
    Ok(Json(ExactTransitionResponse {
        action,
        reasons,
        revision: document.revision,
        generation,
    }))
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
            // the Warp candidate. Simple can still estimate its shared slice
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

    if requested_mode == ProjectionMode::Exact {
        return exact_recommendation(document, &converted, requested_aspect, generation);
    }

    // The slices as the slicer will crop them: whole canvas pixels on the
    // document's canvas ([`crate::model::effective_slice_px`]), expressed in
    // canvas units for the density arithmetic below. Without a valid canvas
    // a percent slice is taken as stored; a pixels slice has nothing to be
    // a fraction of, so the converted candidate stands in for it.
    let canvas = document
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas)
        .filter(|canvas| canvas.dimensions().is_ok());
    let units = document.slice_units();
    let in_canvas_units = |geometry: &OutputGeometry| -> Option<OutputGeometry> {
        let Some(canvas) = canvas else {
            return (units == crate::model::SliceUnits::Percent).then(|| geometry.clone());
        };
        let pixels = crate::model::effective_slice_px(geometry.slice, units, &canvas).ok()?;
        Some(OutputGeometry {
            slice: pixels.to_canvas_units(&canvas).ok()?,
            raster_footprint: crate::model::to_canvas_units(
                geometry.raster_footprint,
                units,
                &canvas,
            )
            .ok()?,
            ..geometry.clone()
        })
    };
    let geometries: Vec<(&OutputConfig, OutputGeometry)> = outputs
        .iter()
        .map(|output| {
            let mut geometry = output
                .geometry
                .as_ref()
                .and_then(in_canvas_units)
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
                // Slice placement/content selection is shared between the
                // two modes. Simple contributes no retained corner or center
                // correction to the sampling estimate.
                geometry.corners = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
                geometry.center = [0.5, 0.5];
            }
            Ok((*output, geometry))
        })
        .collect::<Result<Vec<_>, String>>()?;

    // Sampled (isotropic) density for outputs that are not axis-aligned, and
    // exact per-axis densities for those that are. `None` means no output took
    // that path.
    let mut sampled_density: Option<f64> = None;
    let mut exact_density: Option<(f64, f64)> = None;
    let mut warnings = Vec::new();
    // Pixels per canvas pixel come from the document's own canvas width, so
    // the nearest-filter check below judges the scale as configured today.
    let configured_filter = document
        .projection
        .as_ref()
        .map(|projection| projection.filter)
        .unwrap_or_default();
    let configured_render_width = document
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas.as_ref())
        .map(|canvas| canvas.render_width)
        .or_else(|| {
            converted
                .as_ref()
                .map(|candidate| candidate.canvas.render_width)
        })
        .map(f64::from)
        .filter(|width| *width > 0.0);
    let mut nearest_non_whole = false;
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
        let (width, height) = if rotated {
            (height, width)
        } else {
            (width, height)
        };
        if requested_mode == ProjectionMode::Warp
            && output
                .effective_scale()
                .is_some_and(|scale| (scale - 1.0).abs() > f64::EPSILON)
        {
            return Err(format!(
                "output {} uses a scale unsupported by Warp recommendation",
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
        // Validate slice bounds against the requested aspect while keeping
        // an invalid/stale persisted render width from preventing a useful
        // read-only recommendation that can explain its clamp. The geometry
        // is in canvas units by now, and is checked on the largest canvas
        // the aspect admits, so no slice is too small to hold a whole pixel.
        let validation_canvas = CanvasConfig {
            aspect: requested_aspect,
            render_width: admissible_width(requested_aspect)?,
        };
        geometry
            .validate_for_output(
                &validation_canvas,
                crate::model::SliceUnits::Percent,
                width,
                height,
            )
            .map_err(|error| {
                format!(
                    "output {} has invalid geometry: {error}",
                    output.r#match.key()
                )
            })?;
        let slice = geometry.slice;
        if !(slice.width.is_finite()
            && slice.height.is_finite()
            && slice.width > 0.0
            && slice.height > 0.0)
        {
            return Err(format!(
                "output {} has invalid slice dimensions",
                output.r#match.key()
            ));
        }
        if let Some((rw, rh)) = axis_aligned_extent(&geometry.corners) {
            // The mapping is a per-axis scale (piecewise at the center
            // remap), so the density is known exactly: output pixels per
            // canvas-width unit along each axis, taking the steeper half.
            let [cx, cy] = geometry.center;
            let dx = f64::from(width) * rw * (2.0 * cx).max(2.0 * (1.0 - cx)) / slice.width;
            let dy = f64::from(height) * rh * (2.0 * cy).max(2.0 * (1.0 - cy)) / slice.height;
            let (max_dx, max_dy) = exact_density.unwrap_or((0.0, 0.0));
            exact_density = Some((max_dx.max(dx), max_dy.max(dy)));
            if let Some(render_width) = configured_render_width {
                nearest_non_whole |=
                    !is_whole_scale(dx / render_width) || !is_whole_scale(dy / render_width);
            }
        } else {
            // A warped or keystoned mapping has no single whole-number scale.
            nearest_non_whole = true;
            let density = sample_density(&warp, slice.width, slice.height)?;
            sampled_density = Some(sampled_density.map_or(density, |best| best.max(density)));
        }
    }
    if configured_filter == ResampleFilter::Nearest && nearest_non_whole {
        warnings.push(NEAREST_NON_WHOLE_WARNING.to_string());
    }
    let renderer = document
        .projection
        .as_ref()
        .map(|projection| projection.renderer)
        .unwrap_or_default();
    if renderer == Renderer::Cpu {
        let gpu_only = match configured_filter {
            ResampleFilter::Lanczos => Some("lanczos"),
            ResampleFilter::Easu => Some("easu"),
            _ => None,
        };
        if let Some(name) = gpu_only {
            warnings.push(format!(
                "the {name} filter needs the GPU renderer; the renderer is set to cpu, so this filter is not used"
            ));
        }
    }
    let approximate = sampled_density.is_some();
    if approximate {
        warnings.push(
            "approximate density: sampled largest-singular Jacobian with an engineering margin; this is not an exact maximum proof".to_string(),
        );
    }
    // Five percent is an engineering margin for finite-difference truncation
    // and variation between the 33×33 grid points. It is not a mathematical
    // bound on the unsampled maximum, and it applies to the sampled outputs
    // only; axis-aligned outputs carry no margin.
    // The sampled estimator uses canonical isotropic canvas units, so it
    // constrains both axes equally; exact outputs constrain each axis on its
    // own. Account for the actual rounded integer height when choosing the
    // width: for an odd aspect/height pair, `round(width / aspect)` can
    // otherwise leave the y density just below the target.
    let (mut x_density, mut y_density) = exact_density.unwrap_or((0.0, 0.0));
    if let Some(density) = sampled_density {
        let target_density = (density * 1.05).ceil().max(1.0);
        x_density = x_density.max(target_density);
        y_density = y_density.max(target_density);
    }
    if !x_density.is_finite() || !y_density.is_finite() || x_density <= 0.0 || y_density <= 0.0 {
        return Err("geometry has no finite positive sampling density".into());
    }
    let ideal_width_f = width_for_density(x_density, y_density, requested_aspect)?;
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
    let out_of_canvas = crate::model::slice_units::out_of_canvas(document);
    warnings.extend(
        out_of_canvas
            .iter()
            .map(|overhang| overhang.message.clone()),
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
        approximate,
        out_of_canvas,
    })
}

/// The recommendation in Exact mode: there is nothing to recommend, because
/// the canvas is whatever the output placements make it. The ideal width is
/// the canvas's own, never approximate, with no presets.
fn exact_recommendation(
    document: &crate::model::DesiredState,
    converted: &Option<ConvertLayoutResponse>,
    requested_aspect: f64,
    generation: u64,
) -> Result<RecommendationResponse, String> {
    let canvas = document
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas)
        .or_else(|| converted.as_ref().map(|candidate| candidate.canvas))
        .ok_or_else(|| "projection canvas is unknown".to_string())?;
    let (ideal_width, ideal_height) = canvas.dimensions()?;
    let admissible_width = admissible_width(requested_aspect)?;
    let admissible_height = canvas_height(admissible_width, requested_aspect)?;
    let out_of_canvas = crate::model::slice_units::out_of_canvas(document);
    let mut warnings = vec!["exact mode: the canvas is defined by the output placements".into()];
    warnings.extend(
        out_of_canvas
            .iter()
            .map(|overhang| overhang.message.clone()),
    );
    Ok(RecommendationResponse {
        revision: document.revision,
        generation,
        requested_aspect,
        ideal_width: u64::from(ideal_width),
        ideal_height: u64::from(ideal_height),
        admissible_width,
        admissible_height,
        presets: Vec::new(),
        known_limits: LIMITS,
        unknown_limits: vec!["device/compositor allocation limit".into()],
        warnings,
        approximate: false,
        out_of_canvas,
    })
}

/// Warning text for the nearest filter at a scale that is not a whole number.
pub const NEAREST_NON_WHOLE_WARNING: &str = "nearest filter at a non-whole scale will alias";

/// Whether an output-pixels-per-canvas-pixel ratio is a whole number (at
/// least 1), within rounding noise.
fn is_whole_scale(ratio: f64) -> bool {
    ratio >= 1.0 - 1e-6 && (ratio - ratio.round()).abs() < 1e-6
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

/// The corners' rectangle extent `(width, height)` in normalized output
/// fractions when they (TL, TR, BR, BL) form a positive axis-aligned
/// rectangle, otherwise `None`.
fn axis_aligned_extent(corners: &[[f64; 2]; 4]) -> Option<(f64, f64)> {
    const TOLERANCE: f64 = 1e-12;
    let [tl, tr, br, bl] = *corners;
    let aligned = (tl[1] - tr[1]).abs() <= TOLERANCE
        && (bl[1] - br[1]).abs() <= TOLERANCE
        && (tl[0] - bl[0]).abs() <= TOLERANCE
        && (tr[0] - br[0]).abs() <= TOLERANCE;
    let (rw, rh) = (tr[0] - tl[0], bl[1] - tl[1]);
    (aligned && rw.is_finite() && rh.is_finite() && rw > TOLERANCE && rh > TOLERANCE)
        .then_some((rw, rh))
}

/// Round up while ignoring float noise (e.g. 3840.0000000004 stays 3840).
/// The relative slack also absorbs an aspect stored to eight places:
/// 3840 / 1.77777777 is 2160.0000036 rows, a few thousandths of a pixel.
fn ceil_tol(value: f64) -> f64 {
    (value - value.abs() * 1e-6).ceil()
}

/// Smallest integer canvas width that meets `x_density` horizontally and
/// `y_density` vertically (both in output pixels per canvas-width unit; the
/// canvas height in pixels is `round(width / aspect)`).
fn width_for_density(x_density: f64, y_density: f64, aspect: f64) -> Result<f64, String> {
    if !(x_density.is_finite() && x_density > 0.0 && y_density.is_finite() && y_density > 0.0) {
        return Err("geometry density is not finite and positive".into());
    }
    if !(aspect.is_finite() && aspect > 0.0) {
        return Err("canvas aspect must be finite and positive".into());
    }
    // For positive half-up rounding, round(width / aspect) >= h exactly when
    // width >= aspect * (h - 0.5). Calculate the minimum required integer
    // height directly instead of incrementing width: at very large values an
    // f64 cannot represent width + 1, and an incremental loop could stall.
    let required_height = ceil_tol(y_density / aspect).max(1.0);
    // Canvas height is at least one even below the first rounding boundary.
    let y_width = if required_height == 1.0 {
        1.0
    } else {
        (aspect * (required_height - 0.5)).ceil().max(1.0)
    };
    let width = ceil_tol(x_density).max(y_width).max(1.0);
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
        assert_eq!(response.outputs[0].geometry.slice.x, 0.0);
        assert_eq!(response.outputs[1].geometry.slice.x, 0.5);
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
    fn conversion_rejects_duplicate_keys_and_disconnected_slices() {
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
        assert_eq!(width_for_density(105.84, 105.84, 1.6).unwrap(), 107.0);
        assert!(width_for_density(1.0e20, 1.0e20, 1.6).is_err());
        assert_eq!(width_for_density(100.0, 100.0, 1.0e9).unwrap(), 100.0);
        // Per-axis requirements stay separate: 3840 px wide and 2160 rows
        // at aspect 1.77777777 need exactly width 3840.
        assert_eq!(
            width_for_density(3840.0000000004, 3839.99998, 1.77777777).unwrap(),
            3840.0
        );
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
            slice: CanvasRect {
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
    fn simple_recommendation_uses_identity_correction_with_shared_slice() {
        let mut configured = output("simple", 0, 0, 160, 90, true);
        configured.geometry = Some(OutputGeometry {
            slice: CanvasRect {
                x: 0.125,
                y: 0.0,
                width: 0.5,
                height: 0.5,
            },
            // Retained Warp calibration must not affect a requested Simple
            // recommendation. The slice rectangle remains shared.
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

    /// Four 1920x1080 outputs in a 2x2 grid on a 3840-wide canvas, as on
    /// System A, with the given pins and mode.
    fn grid_state(mode: ProjectionMode, corners: [[f64; 2]; 4]) -> crate::model::DesiredState {
        let slice_height = 0.281_250_001_230_468_8;
        let mut state = crate::model::DesiredState::new();
        state.outputs = [
            (0.0, 0.0),
            (0.5, 0.0),
            (0.0, slice_height),
            (0.5, slice_height),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (x, y))| {
            let slice = CanvasRect {
                x,
                y,
                width: 0.5,
                height: slice_height,
            };
            let mut configured = output(&format!("grid-{index}"), 0, 0, 1920, 1080, true);
            configured.geometry = Some(OutputGeometry {
                slice,
                corners,
                center: [0.5, 0.5],
                raster_footprint: slice,
            });
            configured
        })
        .collect();
        state.projection = Some(crate::model::ProjectionConfig {
            mode,
            canvas: Some(CanvasConfig {
                aspect: 1.777_777_77,
                render_width: 3840,
            }),
            ..Default::default()
        });
        state
    }

    const IDENTITY_PINS: [[f64; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    const KEYSTONE_PINS: [[f64; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.1, 1.0]];

    #[test]
    fn axis_aligned_grid_recommendation_is_exact() {
        for mode in [ProjectionMode::Simple, ProjectionMode::Warp] {
            let rec = recommend_for_state(&grid_state(mode, IDENTITY_PINS), 0).unwrap();
            assert_eq!(rec.ideal_width, 3840, "{mode:?}");
            assert_eq!(rec.ideal_height, 2160, "{mode:?}");
            assert!(!rec.approximate, "{mode:?}");
            assert!(rec.warnings.iter().all(|w| !w.contains("approximate")));
            assert_eq!(rec.presets[3].width, 3840);
        }
        // Simple ignores non-identity pins, so it stays exact and unchanged.
        let rec =
            recommend_for_state(&grid_state(ProjectionMode::Simple, KEYSTONE_PINS), 0).unwrap();
        assert_eq!((rec.ideal_width, rec.ideal_height), (3840, 2160));
        assert!(!rec.approximate);
    }

    #[test]
    fn exact_recommendation_is_the_canvas_with_no_presets() {
        // Pins are irrelevant: Exact carries none, and nothing is sampled.
        for pins in [IDENTITY_PINS, KEYSTONE_PINS] {
            let state = grid_state(ProjectionMode::Exact, pins);
            let canvas = state.projection.as_ref().unwrap().canvas.unwrap();
            let rec = recommend_for_state(&state, 7).unwrap();
            let (width, height) = canvas.dimensions().unwrap();
            assert_eq!(rec.ideal_width, u64::from(width));
            assert_eq!(rec.ideal_height, u64::from(height));
            assert!(!rec.approximate);
            assert!(rec.presets.is_empty());
            assert_eq!(
                rec.warnings,
                ["exact mode: the canvas is defined by the output placements"]
            );
            assert_eq!(rec.generation, 7);
        }
    }

    #[test]
    fn recommendation_warns_about_filters_that_do_not_fit() {
        let with = |render_width: u32, filter: ResampleFilter, renderer: Renderer| {
            let mut state = grid_state(ProjectionMode::Simple, IDENTITY_PINS);
            let projection = state.projection.as_mut().unwrap();
            projection.canvas.as_mut().unwrap().render_width = render_width;
            projection.filter = filter;
            projection.renderer = renderer;
            recommend_for_state(&state, 0).unwrap().warnings
        };
        let aliases = |warnings: &[String]| warnings.iter().any(|w| w == NEAREST_NON_WHOLE_WARNING);
        // 3840 wide gives 1:1 and 1920 wide gives 2x: both whole.
        assert!(!aliases(&with(
            3840,
            ResampleFilter::Nearest,
            Renderer::Auto
        )));
        assert!(!aliases(&with(
            1920,
            ResampleFilter::Nearest,
            Renderer::Auto
        )));
        // 1920 / 1440 is 1.33x.
        assert!(aliases(&with(
            2880,
            ResampleFilter::Nearest,
            Renderer::Auto
        )));
        // Other filters never raise it.
        assert!(!aliases(&with(
            2880,
            ResampleFilter::Bilinear,
            Renderer::Auto
        )));
        // GPU-only filters with the CPU renderer requested.
        let gpu_only = |warnings: &[String]| {
            warnings
                .iter()
                .any(|w| w.contains("needs the GPU renderer"))
        };
        assert!(gpu_only(&with(
            3840,
            ResampleFilter::Lanczos,
            Renderer::Cpu
        )));
        assert!(gpu_only(&with(3840, ResampleFilter::Easu, Renderer::Cpu)));
        assert!(!gpu_only(&with(
            3840,
            ResampleFilter::Lanczos,
            Renderer::Auto
        )));
        assert!(!gpu_only(&with(
            3840,
            ResampleFilter::Bicubic,
            Renderer::Cpu
        )));
    }

    #[test]
    fn rectangular_pins_scale_the_exact_density() {
        // A half-width rectangle halves the density the 1920 raster needs.
        let half = [[0.0, 0.0], [0.5, 0.0], [0.5, 1.0], [0.0, 1.0]];
        let mut state = grid_state(ProjectionMode::Warp, half);
        state.outputs.truncate(1);
        state
            .projection
            .as_mut()
            .unwrap()
            .canvas
            .as_mut()
            .unwrap()
            .aspect = 16.0 / 9.0;
        state.outputs[0].geometry.as_mut().unwrap().slice = CanvasRect {
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.28125,
        };
        let rec = recommend_for_state(&state, 0).unwrap();
        assert!(!rec.approximate);
        // dx = 1920 * 0.5 / 0.5; dy = 1080 / 0.28125 = 3840 rows * aspect.
        assert_eq!(rec.ideal_width, 3840);

        // Without the height requirement dominating, the width is the x density.
        state.outputs[0].geometry.as_mut().unwrap().slice.height = 0.5625;
        let rec = recommend_for_state(&state, 0).unwrap();
        assert_eq!(rec.ideal_width, 1920);
    }

    #[test]
    fn off_center_scales_the_exact_density() {
        let mut state = grid_state(ProjectionMode::Warp, IDENTITY_PINS);
        state.outputs.truncate(1);
        state.outputs[0].geometry.as_mut().unwrap().center = [0.25, 0.5];
        let rec = recommend_for_state(&state, 0).unwrap();
        assert!(!rec.approximate);
        // dx = 1920 * 1.5 / 0.5 = 5760; dy = 3840 is the lesser requirement.
        assert_eq!(rec.ideal_width, 5760);
    }

    #[test]
    fn keystoned_pins_keep_the_sampled_margin() {
        let rec = recommend_for_state(&grid_state(ProjectionMode::Warp, KEYSTONE_PINS), 0).unwrap();
        assert!(rec.approximate);
        assert!(rec.warnings.iter().any(|w| w.contains("approximate")));
        assert!(rec.ideal_width > 3840);
    }

    #[test]
    fn mixed_layout_meets_both_requirements() {
        let rectangular =
            recommend_for_state(&grid_state(ProjectionMode::Warp, IDENTITY_PINS), 0).unwrap();
        let mut mixed = grid_state(ProjectionMode::Warp, IDENTITY_PINS);
        mixed.outputs[1].geometry.as_mut().unwrap().corners = KEYSTONE_PINS;
        let rec = recommend_for_state(&mixed, 0).unwrap();
        assert!(rec.approximate);
        assert!(rec.ideal_width >= rectangular.ideal_width);
        assert!(rec.ideal_width > 3840);
    }

    /// A `cols`x`rows` grid of `raster_w`x`raster_h` (logical) outputs, each
    /// slice sized so the output samples `ratio_x` / `ratio_y` physical pixels
    /// per canvas pixel at render width `canvas_w`.
    struct RatioCase {
        name: &'static str,
        cols: u32,
        rows: u32,
        raster: (i32, i32),
        transform: Transform,
        canvas_w: f64,
        ratio: (f64, f64),
        aspect: f64,
        pins: [[f64; 2]; 4],
        center: [f64; 2],
        /// Expected `(ideal_width, ideal_height)`.
        expect: (u64, u64),
    }

    impl RatioCase {
        fn new(
            name: &'static str,
            (cols, rows): (u32, u32),
            raster: (i32, i32),
            canvas_w: f64,
            ratio: (f64, f64),
            aspect: f64,
            expect: (u64, u64),
        ) -> Self {
            Self {
                name,
                cols,
                rows,
                raster,
                transform: Transform::Normal,
                canvas_w,
                ratio,
                aspect,
                pins: IDENTITY_PINS,
                center: [0.5, 0.5],
                expect,
            }
        }

        fn state(&self) -> crate::model::DesiredState {
            let (w, h) = self.raster;
            let slice_w = f64::from(w) / (self.ratio.0 * self.canvas_w);
            let slice_h = f64::from(h) / (self.ratio.1 * self.canvas_w);
            let rotated = matches!(
                self.transform,
                Transform::Rotate90
                    | Transform::Rotate270
                    | Transform::Flipped90
                    | Transform::Flipped270
            );
            let mode = if rotated { (h, w) } else { (w, h) };
            let mut state = crate::model::DesiredState::new();
            for row in 0..self.rows {
                for col in 0..self.cols {
                    let slice = CanvasRect {
                        // Slices that would overhang (ratios below 1) are
                        // pulled back onto the canvas; they may coincide.
                        x: (f64::from(col) * slice_w).min(1.0 - slice_w),
                        y: (f64::from(row) * slice_h).min(1.0 / self.aspect - slice_h),
                        width: slice_w,
                        height: slice_h,
                    };
                    let mut configured =
                        output(&format!("o-{row}-{col}"), 0, 0, mode.0, mode.1, true);
                    configured.transform = Some(self.transform);
                    configured.geometry = Some(OutputGeometry {
                        slice,
                        corners: self.pins,
                        center: self.center,
                        raster_footprint: slice,
                    });
                    state.outputs.push(configured);
                }
            }
            state.projection = Some(crate::model::ProjectionConfig {
                mode: ProjectionMode::Warp,
                canvas: Some(CanvasConfig {
                    aspect: self.aspect,
                    render_width: self.canvas_w as u32,
                }),
                ..Default::default()
            });
            state
        }
    }

    #[test]
    fn whole_ratio_layouts_hit_exact_dimensions() {
        const A: f64 = 16.0 / 9.0;
        let hd = (1920, 1080);
        let mut cases = vec![
            RatioCase::new("1:1 grid", (2, 2), hd, 3840.0, (1.0, 1.0), A, (3840, 2160)),
            RatioCase::new("2:1 grid", (2, 2), hd, 3840.0, (2.0, 2.0), A, (7680, 4320)),
            RatioCase::new("1:2 grid", (2, 2), hd, 3840.0, (0.5, 0.5), A, (1920, 1080)),
            RatioCase::new(
                "2:1 x, 1:1 y",
                (2, 2),
                hd,
                3840.0,
                (2.0, 1.0),
                A,
                (7680, 4320),
            ),
            RatioCase::new(
                "1:1 x, 2:1 y",
                (2, 2),
                hd,
                3840.0,
                (1.0, 2.0),
                A,
                (7680, 4320),
            ),
            RatioCase::new(
                "single full canvas",
                (1, 1),
                hd,
                1920.0,
                (1.0, 1.0),
                A,
                (1920, 1080),
            ),
            RatioCase::new(
                "5:4",
                (1, 1),
                (1280, 1024),
                1280.0,
                (1.0, 1.0),
                1.25,
                (1280, 1024),
            ),
            RatioCase::new(
                "1366x768",
                (1, 1),
                (1366, 768),
                1366.0,
                (1.0, 1.0),
                1366.0 / 768.0,
                (1366, 768),
            ),
            RatioCase::new(
                "3x1",
                (3, 1),
                hd,
                5760.0,
                (1.0, 1.0),
                5760.0 / 1080.0,
                (5760, 1080),
            ),
            RatioCase::new(
                "1x3",
                (1, 3),
                hd,
                1920.0,
                (1.0, 1.0),
                1920.0 / 3240.0,
                (1920, 3240),
            ),
            // 3x2 on a 2.5 canvas: W / aspect = 2304 is already whole.
            RatioCase::new(
                "3x2 aspect 2.5",
                (3, 2),
                hd,
                5760.0,
                (1.0, 1.0),
                2.5,
                (5760, 2304),
            ),
            // W / aspect = 2215.38, so the canvas is 5760x2215 and its
            // vertical density 5759 px per unit. The stored fractions alone
            // would ask for a 2216th row, but the recommendation reads the
            // whole-pixel slices the slicer crops, which on this canvas are
            // exactly 1080 rows each (edges at 1079.8 and 2159.6 round to
            // 1080 and 2160), so no row is added.
            RatioCase::new(
                "3x2 aspect 2.6",
                (3, 2),
                hd,
                5760.0,
                (1.0, 1.0),
                2.6,
                (5760, 2215),
            ),
        ];
        let mut half_pins = RatioCase::new(
            "half-raster pins",
            (1, 1),
            hd,
            1920.0,
            (1.0, 1.0),
            A,
            (960, 540),
        );
        half_pins.pins = [[0.0, 0.0], [0.5, 0.0], [0.5, 0.5], [0.0, 0.5]];
        cases.push(half_pins);
        let mut off_center = RatioCase::new(
            "center 0.25",
            (1, 1),
            hd,
            3840.0,
            (1.0, 1.0),
            A,
            (5760, 3240),
        );
        off_center.center = [0.25, 0.5];
        cases.push(off_center);
        for (name, ratio, expect) in [
            ("rotated 1:1", (1.0, 1.0), (1080, 1920)),
            ("rotated 2:1", (2.0, 2.0), (2160, 3840)),
        ] {
            for transform in [Transform::Rotate90, Transform::Rotate270] {
                // Logical raster is portrait; canvas is portrait too.
                let mut rotated = RatioCase::new(
                    name,
                    (1, 1),
                    (1080, 1920),
                    1080.0,
                    ratio,
                    9.0 / 16.0,
                    expect,
                );
                rotated.transform = transform;
                cases.push(rotated);
            }
        }
        for case in &cases {
            let rec = recommend_for_state(&case.state(), 0).unwrap();
            assert!(!rec.approximate, "{}", case.name);
            assert_eq!(
                (rec.ideal_width, rec.ideal_height),
                case.expect,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn stored_aspect_and_slice_noise_do_not_add_pixels() {
        // System A: aspect stored to eight places, slice height carrying the
        // matching float dust.
        let mut noisy = RatioCase::new(
            "noisy",
            (2, 2),
            (1920, 1080),
            3840.0,
            (1.0, 1.0),
            1.777_777_77,
            (3840, 2160),
        );
        let mut state = noisy.state();
        for configured in &mut state.outputs {
            let geometry = configured.geometry.as_mut().unwrap();
            geometry.slice.height = 0.281_250_001_230_468_8;
            geometry.raster_footprint = geometry.slice;
        }
        let rec = recommend_for_state(&state, 0).unwrap();
        assert_eq!((rec.ideal_width, rec.ideal_height), (3840, 2160));
        // Stored aspect with an exact slice height, as a typed 1080 px crop
        // produces.
        let rec = recommend_for_state(&noisy.state(), 0).unwrap();
        assert_eq!((rec.ideal_width, rec.ideal_height), (3840, 2160));
        // Exact aspect with exact and with dusty slice heights.
        noisy.aspect = 16.0 / 9.0;
        let rec = recommend_for_state(&noisy.state(), 0).unwrap();
        assert_eq!((rec.ideal_width, rec.ideal_height), (3840, 2160));
        let mut state = noisy.state();
        for configured in &mut state.outputs {
            let geometry = configured.geometry.as_mut().unwrap();
            geometry.slice.height = 0.281_250_001_230_468_8;
            geometry.raster_footprint = geometry.slice;
        }
        let rec = recommend_for_state(&state, 0).unwrap();
        assert_eq!((rec.ideal_width, rec.ideal_height), (3840, 2160));
        let widths: Vec<u64> = rec.presets.iter().map(|preset| preset.width).collect();
        assert_eq!(widths, [960, 1920, 2880, 3840]);
    }

    #[test]
    fn arrangement_round_trip_hits_the_content_scale_exactly() {
        let raster = |_: &OutputConfig| Some((1920u32, 1080u32));
        for (columns, rows) in [(1, 1), (2, 1), (1, 2), (2, 2), (3, 2), (3, 3)] {
            for scale in [0.5, 1.0, 2.0, 4.0] {
                let mut document = arrangement_document((columns * rows) as usize);
                // A canvas the grid fills exactly at this scale, so zero
                // overlap and the requested scale agree.
                let canvas_w = f64::from(columns) * 1920.0 / scale;
                let canvas_h = f64::from(rows) * 1080.0 / scale;
                if canvas_w * canvas_h > LIMITS.max_canvas_pixels as f64 {
                    // 3x3 at 0.5 needs an 11520x6480 canvas, past the 64 MP
                    // planning limit, so the solver rightly refuses it.
                    continue;
                }
                document.projection.as_mut().unwrap().canvas = Some(CanvasConfig {
                    aspect: canvas_w / canvas_h,
                    render_width: canvas_w as u32,
                });
                let request = crate::model::ArrangementRequest {
                    rows,
                    columns,
                    overlap_x: None,
                    overlap_y: None,
                    content_scale: Some(scale),
                    allow_unused_canvas: false,
                    committed: false,
                };
                let solution = crate::model::solve_document(&document, &request, raster)
                    .unwrap_or_else(|error| panic!("{columns}x{rows} @ {scale}: {error}"));
                crate::model::apply(&mut document, &solution);
                let rec = recommend_for_state(&document, 0).unwrap();
                let label = format!("{columns}x{rows} @ {scale}");
                assert!(!rec.approximate, "{label}");
                assert_eq!(rec.ideal_width, (canvas_w * scale) as u64, "{label}");
                assert_eq!(rec.ideal_height, (canvas_h * scale) as u64, "{label}");
            }
        }
    }

    #[test]
    fn arrangement_round_trip_with_overlap_hits_the_content_scale_exactly() {
        let raster = |_: &OutputConfig| Some((1920u32, 1080u32));
        // (columns, rows, overlapX, overlapY): the covered extent is
        // 2*1920 - 0.25*1920 = 3360 wide and 2*1080 - 0.5*1080 = 1620 tall.
        for (columns, rows, overlap_x, overlap_y, covered) in [
            (2, 1, 0.25, 0.0, (3360.0, 1080.0)),
            (2, 2, 0.25, 0.5, (3360.0, 1620.0)),
        ] {
            for scale in [1.0, 2.0] {
                let mut document = arrangement_document((columns * rows) as usize);
                let canvas_w = covered.0 / scale;
                document.projection.as_mut().unwrap().canvas = Some(CanvasConfig {
                    aspect: covered.0 / covered.1,
                    render_width: canvas_w as u32,
                });
                let request = crate::model::ArrangementRequest {
                    rows,
                    columns,
                    overlap_x: Some(overlap_x),
                    overlap_y: Some(overlap_y),
                    content_scale: None,
                    allow_unused_canvas: false,
                    committed: false,
                };
                let solution = crate::model::solve_document(&document, &request, raster).unwrap();
                let label = format!("{columns}x{rows} @ {scale}");
                assert_eq!(solution.arrangement.content_scale, scale, "{label}");
                crate::model::apply(&mut document, &solution);
                let rec = recommend_for_state(&document, 0).unwrap();
                assert!(!rec.approximate, "{label}");
                assert_eq!(rec.ideal_width, (canvas_w * scale) as u64, "{label}");
            }
        }
    }

    #[test]
    fn recommendation_is_invariant_to_render_width_with_rounded_height() {
        let mut configured = output("odd", 0, 0, 160, 63, true);
        configured.geometry = Some(OutputGeometry {
            slice: CanvasRect {
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
            slice: CanvasRect {
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

        // Converting from positions lays the rotated output out at its
        // logical (as-viewed) 80x160 rect, as sway does, so the converted
        // canvas is portrait: it matches an explicit full-canvas portrait
        // geometry, not the landscape slice above.
        let mut converted_rotated = rotated_state.clone();
        converted_rotated.outputs[0].geometry = None;
        let converted = recommend_for_state(&converted_rotated, 0).unwrap();
        let mut explicit_portrait = rotated_state.clone();
        explicit_portrait.projection.as_mut().unwrap().canvas = Some(CanvasConfig {
            aspect: 0.5,
            render_width: 80,
        });
        let portrait_slice = CanvasRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 2.0,
        };
        let geometry = explicit_portrait.outputs[0].geometry.as_mut().unwrap();
        geometry.slice = portrait_slice;
        geometry.raster_footprint = portrait_slice;
        let portrait = recommend_for_state(&explicit_portrait, 0).unwrap();
        assert_eq!(portrait.ideal_width, converted.ideal_width);
        assert_ne!(rotated_without_scale.ideal_width, converted.ideal_width);
    }

    #[test]
    fn recommendation_clamps_dimensions_and_reports_achieved_presets() {
        let mut wide = output("wide", 0, 0, 32768, 900, true);
        wide.geometry = Some(OutputGeometry {
            slice: CanvasRect {
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
            slice: CanvasRect {
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
            slice: CanvasRect {
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

    // --- grid arrangement dry run ------------------------------------------

    /// `count` enabled outputs at 1920x1080 on a 3840-wide 16:9 canvas, with
    /// no geometry: a dry run does not write, so unlike the config-route
    /// tests this never needs `allow_overlaps` or document validation to
    /// pass — it is pure arithmetic over the document as given.
    fn arrangement_document(count: usize) -> crate::model::DesiredState {
        let mut outputs = Vec::new();
        for index in 0..count {
            let mut output = OutputConfig::new(OutputMatch::by_name(format!("HDMI-{}", index + 1)));
            output.enable = true;
            output.mode = Some(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0,
            });
            outputs.push(output);
        }
        let mut document = crate::model::DesiredState::new();
        document.outputs = outputs;
        document.projection = Some(crate::model::ProjectionConfig {
            canvas: Some(CanvasConfig {
                aspect: 16.0 / 9.0,
                render_width: 3840,
            }),
            ..Default::default()
        });
        document
    }

    #[tokio::test]
    async fn dry_run_solves_without_writing() {
        let harness = crate::api::test_support::harness(None);
        harness
            .state
            .store
            .update(|document| *document = arrangement_document(4))
            .unwrap();
        let before = harness.state.store.get();

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(
                        "/api/v1/projection/arrangement?rows=2&columns=2&overlapX=0.2&overlapY=0.2",
                    )
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
        assert_eq!(body["arrangement"]["rows"], 2);
        assert_eq!(body["arrangement"]["columns"], 2);
        assert_eq!(body["outputs"].as_array().unwrap().len(), 4);
        assert!(
            (body["arrangement"]["contentScale"].as_f64().unwrap() - 0.9).abs() < 1e-9,
            "{body}"
        );

        assert_eq!(
            harness.state.store.get(),
            before,
            "a dry run must not write"
        );
    }

    #[tokio::test]
    async fn dry_run_reports_the_solvers_error_as_a_422() {
        let harness = crate::api::test_support::harness(None);
        harness
            .state
            .store
            .update(|document| *document = arrangement_document(5))
            .unwrap();

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projection/arrangement?rows=2&columns=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("do not fit a 2x2 grid"),
            "{body}"
        );
    }

    /// A harness whose saved document is [`arrangement_document`]`(count)`.
    fn arrangement_harness(count: usize) -> crate::api::test_support::Harness {
        let harness = crate::api::test_support::harness(None);
        harness
            .state
            .store
            .update(|document| *document = arrangement_document(count))
            .unwrap();
        harness
    }

    /// `GET` `uri` on the harness's router: the status and the JSON body.
    async fn get_json(
        harness: &crate::api::test_support::Harness,
        uri: &str,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        let response = harness
            .router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// A single row has no vertical seam, and on 16:9 its fits differ, so
    /// the default overlap-mode solve overhangs rather than leaving a band:
    /// 1x2 of 1920x1080 at (0.2, 0.2) on 3840x2160 has fit_x = (3840 −
    /// 0.2·1920)/3840 = 3456/3840 = 0.9 and fit_y = 1080/2160 = 0.5, so
    /// scale = min = 0.5. Y fills exactly; X spans 3456/0.5 = 6912 px
    /// against 3840, an overhang of 6912/3840 − 1 = 0.8. Each column is
    /// 1920/0.5 = 3840 px, the first starting at (3840 − 6912)/2 = −1536 and
    /// ending at 2304, so it is still on the canvas and the solve succeeds.
    ///
    /// With `allowUnusedCanvas=true` the fit-inside scale is max = 0.9
    /// instead: X fills exactly, Y spans 1080/0.9 = 1200 px of 2160, leaving
    /// a band of 1 − 1200/2160 = 4/9 and no overhang.
    #[tokio::test]
    async fn dry_run_overhangs_a_single_row_unless_allow_unused_canvas_asks_for_a_band() {
        let harness = arrangement_harness(2);

        let (status, body) = get_json(
            &harness,
            "/api/v1/projection/arrangement?rows=1&columns=2&overlapX=0.2&overlapY=0.2",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let close = |value: &serde_json::Value, expected: f64| {
            (value.as_f64().unwrap() - expected).abs() < 1e-9
        };
        assert!(close(&body["arrangement"]["contentScale"], 0.5), "{body}");
        assert!(close(&body["overhang"]["x"], 0.8), "{body}");
        assert!(close(&body["overhang"]["y"], 0.0), "{body}");
        assert!(close(&body["unusedCanvas"]["x"], 0.0), "{body}");
        assert!(close(&body["unusedCanvas"]["y"], 0.0), "{body}");

        let (status, body) = get_json(
            &harness,
            "/api/v1/projection/arrangement?rows=1&columns=2&overlapX=0.2&overlapY=0.2&allowUnusedCanvas=true",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert!(close(&body["arrangement"]["contentScale"], 0.9), "{body}");
        assert!(close(&body["unusedCanvas"]["x"], 0.0), "{body}");
        assert!(close(&body["unusedCanvas"]["y"], 4.0 / 9.0), "{body}");
        assert!(close(&body["overhang"]["x"], 0.0), "{body}");
        assert!(close(&body["overhang"]["y"], 0.0), "{body}");
    }

    /// The overlaps in the answer are the overlaps in the query, bit for
    /// bit — including an inert one on an axis with no seam — so a client
    /// that adopts the echo never sees its own slider move.
    #[tokio::test]
    async fn dry_run_echoes_the_requested_overlaps_exactly() {
        for (count, rows, columns, overlap_x, overlap_y) in [
            (4, 2, 2, 0.137_f64, 0.0421_f64),
            (4, 2, 2, 0.0, 0.3333333333333333),
            // One row: overlapY is inert, and still echoed as sent.
            (2, 1, 2, 0.2718281828, 0.61),
        ] {
            let harness = arrangement_harness(count);
            let (status, body) = get_json(
                &harness,
                &format!(
                    "/api/v1/projection/arrangement?rows={rows}&columns={columns}&overlapX={overlap_x}&overlapY={overlap_y}"
                ),
            )
            .await;
            assert_eq!(status, axum::http::StatusCode::OK, "{body}");
            assert_eq!(
                body["arrangement"]["overlapX"].as_f64().unwrap(),
                overlap_x,
                "{body}"
            );
            assert_eq!(
                body["arrangement"]["overlapY"].as_f64().unwrap(),
                overlap_y,
                "{body}"
            );
        }
    }

    /// An overlap dry run carries `overhang` and per-axis `limits`.
    ///
    /// 2x2 of 1920x1080 at (0.2, 0.2) on 16:9: fit_x = 3456/3840 = 0.9 and
    /// fit_y = 1944/2160 = 0.9, so both axes fill exactly and neither
    /// overhangs. With two slices per axis the seam sits on the canvas's
    /// center line at every overlap, so no slice can leave the canvas and
    /// each axis's range is the whole of `[0, 1)`: `0` exactly (the scan
    /// reaches it with everything valid) up to the bisected bound just
    /// below `1`.
    ///
    /// 1x2 on the same canvas: `overlapY` has no seam (one row), so it
    /// reports `hasSeam: false` and the whole range; `overlapX` is `[0, 1)`
    /// too — scale stays at fit_y = 0.5 (fit_x = 1 − overlapX/2 ≥ 0.5), each
    /// column is the canvas's full width, and the first one ends at
    /// 1920·(1 + overlapX) > 0 px, always on the canvas.
    #[tokio::test]
    async fn dry_run_reports_overhang_and_limits_for_an_overlap_request() {
        let harness = arrangement_harness(4);
        let (status, body) = get_json(
            &harness,
            "/api/v1/projection/arrangement?rows=2&columns=2&overlapX=0.2&overlapY=0.2",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert!(
            body["overhang"]["x"].as_f64().unwrap().abs() < 1e-9,
            "{body}"
        );
        assert!(
            body["overhang"]["y"].as_f64().unwrap().abs() < 1e-9,
            "{body}"
        );
        for axis in ["overlapX", "overlapY"] {
            let limits = &body["limits"][axis];
            assert_eq!(limits["hasSeam"], true, "{axis}: {body}");
            assert_eq!(limits["min"].as_f64().unwrap(), 0.0, "{axis}: {body}");
            let max = limits["max"].as_f64().unwrap();
            assert!(max > 0.999 && max < 1.0, "{axis}: {body}");
        }

        let harness = arrangement_harness(2);
        let (status, body) = get_json(
            &harness,
            "/api/v1/projection/arrangement?rows=1&columns=2&overlapX=0.2&overlapY=0.2",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert_eq!(body["limits"]["overlapX"]["hasSeam"], true, "{body}");
        assert_eq!(body["limits"]["overlapY"]["hasSeam"], false, "{body}");
        for axis in ["overlapX", "overlapY"] {
            let limits = &body["limits"][axis];
            assert_eq!(limits["min"].as_f64().unwrap(), 0.0, "{axis}: {body}");
            let max = limits["max"].as_f64().unwrap();
            assert!(max > 0.999 && max < 1.0, "{axis}: {body}");
        }
    }

    /// A content-scale request has no overlaps to limit, so its dry run
    /// omits `limits` altogether; `overhang` is still reported.
    #[tokio::test]
    async fn dry_run_omits_limits_for_a_content_scale_request() {
        let harness = arrangement_harness(4);
        let (status, body) = get_json(
            &harness,
            "/api/v1/projection/arrangement?rows=2&columns=2&contentScale=0.9",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert!(body.get("limits").is_none(), "{body}");
        assert!(body["overhang"].is_object(), "{body}");
    }

    /// The off-canvas gate reaches the client as an ordinary `422` problem,
    /// with no limits attached.
    ///
    /// One row of three 1920x1080 at no overlap on 16:9: fit_x = 5760/3840
    /// = 1.5, fit_y = 1080/2160 = 0.5, scale = 0.5, so each column is the
    /// canvas's full 3840 px width and X spans 11520 px: the first column
    /// runs from −3840 to exactly 0, touching the canvas's left edge and
    /// showing none of it. impliedAspect = 5760/1080 = 5.3333….
    #[tokio::test]
    async fn dry_run_refuses_a_slice_pushed_off_the_canvas_with_a_422() {
        let harness = arrangement_harness(3);
        let (status, body) = get_json(
            &harness,
            "/api/v1/projection/arrangement?rows=1&columns=3&overlapX=0&overlapY=0",
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        let detail = body["detail"].as_str().unwrap();
        assert!(
            detail.starts_with("output HDMI-1's slice falls entirely outside the canvas"),
            "{detail}"
        );
        assert!(detail.contains("5.3333"), "{detail}");
        assert!(body.get("limits").is_none(), "{body}");
    }

    #[tokio::test]
    async fn dry_run_does_not_change_generation_and_reports_the_working_copys_identity() {
        let harness = crate::api::test_support::harness(None);
        harness
            .state
            .store
            .update(|document| *document = arrangement_document(4))
            .unwrap();

        // A live working copy, so the effective document differs from the
        // persisted one and bumps the generation.
        let mut previewed = harness.state.store.get();
        previewed.settings.hide_cursor = false;
        previewed.committed = false;
        harness.state.store.set_preview(Some(previewed));
        let revision_before = harness.state.store.revision();
        let generation_before = harness.state.store.generation();

        let response = harness
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(
                        "/api/v1/projection/arrangement?rows=2&columns=2&overlapX=0.2&overlapY=0.2",
                    )
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
        assert_eq!(body["revision"], revision_before);
        assert_eq!(body["generation"], generation_before);
        assert_eq!(
            harness.state.store.generation(),
            generation_before,
            "a dry run must not change the generation"
        );
    }
}
