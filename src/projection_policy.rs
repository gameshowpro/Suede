//! Public geometry capability policy. Saved parameters are never the fallback.

use crate::model::observed::ProjectionGeometryStatus;
use crate::model::slice_units::{convert_arrange_offset, convert_geometry};
use crate::model::{
    exact, CanvasConfig, DesiredState, ProjectionControlStatus, ProjectionMode, Renderer,
    SliceUnits,
};

pub fn status(
    desired: &DesiredState,
    control: &ProjectionControlStatus,
    running: bool,
    allow_overlaps: bool,
    previous: Option<&ProjectionGeometryStatus>,
) -> ProjectionGeometryStatus {
    let projection = desired.projection.clone().unwrap_or_default();
    let retained =
        projection.canvas.is_some() && desired.outputs.iter().any(|o| o.geometry.is_some());
    let (available, reason) = if !cfg!(feature = "projection") {
        (
            Some(false),
            Some(
                "this build has no projection machinery; install a projection-enabled build".into(),
            ),
        )
    } else if !allow_overlaps {
        (
            Some(false),
            Some("warping requires allow_overlaps=true and the canvas slicer".into()),
        )
    } else if projection.renderer == Renderer::Cpu {
        (Some(false), Some("the selected CPU renderer supports simple rectangles only; select Auto or GPU to probe warp support".into()))
    } else if running && control.requested_renderer == Some(projection.renderer) {
        (
            control.warp_available,
            control.warp_reason.clone().or_else(|| {
                control
                    .warp_available
                    .is_none()
                    .then(|| "waiting for the current capture/presentation capability probe".into())
            }),
        )
    } else {
        (None, Some("warp capability is not verified; run content or a calibration pattern to probe the selected pipeline".into()))
    };
    // Exact is the canvas slicer's contract; `exact::unavailable_reason` is
    // the one statement of when an appliance cannot have it.
    let exact_reason = if !cfg!(feature = "projection") {
        Some(
            "this build has no projection machinery; install a projection-enabled build"
                .to_string(),
        )
    } else {
        exact::unavailable_reason(allow_overlaps).map(str::to_string)
    };
    let exact_available = exact_reason.is_none();
    ProjectionGeometryStatus {
        requested_renderer: projection.renderer,
        requested_mode: projection.mode,
        // Keep an already selected warp through its child replacement probe.
        // A pending report is not a negative capability result. Controls still
        // require available=true; an actual startup false selects simple.
        effective_mode: match projection.mode {
            ProjectionMode::Warp
                if available == Some(true)
                    || (available.is_none()
                        && previous.is_some_and(|p| {
                            p.effective_mode == ProjectionMode::Warp
                                && p.requested_renderer == projection.renderer
                        })) =>
            {
                ProjectionMode::Warp
            }
            ProjectionMode::Warp | ProjectionMode::Simple => ProjectionMode::Simple,
            // Exact needs no capability probe beyond what Simple needs, so
            // it is in effect whenever it is requested and available. A
            // build or appliance that cannot run it renders Simple.
            ProjectionMode::Exact if exact_available => ProjectionMode::Exact,
            ProjectionMode::Exact => ProjectionMode::Simple,
        },
        warp_available: available,
        reason,
        retained_warp: retained,
        exact_available: Some(exact_available),
        exact_reason,
        out_of_canvas: crate::model::slice_units::out_of_canvas(desired),
    }
}

/// Settle `projection.sliceUnits` for a working-copy replacement, before
/// [`preserve_retained`] and [`transition`], so every later step sees one
/// unit for the whole document.
///
/// * **The unit.** An explicit `sliceUnits` in the write wins. An omitted
///   one keeps `previous`'s unit, so a client that does not know the field
///   (a bare `{"mode":"exact"}`, a projection section built from scratch)
///   never changes it. Except: a write that creates the canvas where
///   `previous` had none (a fresh document has none) stores `pixels`, the
///   default for a new layout.
/// * **The slices.** Each output's geometry and `arrangeOffset` is in one
///   of two units: one the write left exactly as `previous` had it is in
///   `previous`'s unit; one the write changed is in the unit the write
///   names, or `previous`'s when it names none. Whatever is not already in
///   the settled unit is converted to it on the document's canvas (the
///   write's, else `previous`'s): percent to pixels rounds the edges, pixels
///   to percent divides exactly. So naming the other unit and changing
///   nothing else converts the layout, a whole document written in its own
///   unit is taken literally, and a new canvas's fractional slices become
///   pixels.
/// * **Without a canvas** there is nothing to measure a conversion against,
///   so slices that would need one keep their numbers (a corner case: a
///   retained geometry whose canvas was deleted).
///
/// Geometry the write omits is left for the refill, which converts it
/// (see [`refill_omitted`]). Exact's own rule, pixels only, is applied by
/// [`transition`].
pub fn settle_slice_units(next: &mut DesiredState, previous: &DesiredState) {
    let before = previous.slice_units();
    let had_canvas = previous
        .projection
        .as_ref()
        .is_some_and(|projection| projection.canvas.is_some());
    let (written, created, target) = match next.projection.as_ref() {
        // No projection section: the document is percent by definition.
        None => (Some(SliceUnits::Percent), false, SliceUnits::Percent),
        Some(projection) => {
            let created = !had_canvas && projection.canvas.is_some();
            let written = projection.slice_units;
            let target = written.unwrap_or(if created { SliceUnits::Pixels } else { before });
            (written, created, target)
        }
    };
    // The unit a changed geometry is written in. A write that creates the
    // canvas without naming a unit has no layout to keep a unit from: its
    // slices are what any client without the field writes, fractions.
    let changed_unit = written.unwrap_or(if created { SliceUnits::Percent } else { before });
    let canvas: Option<CanvasConfig> = next
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas)
        .or_else(|| previous.projection.as_ref().and_then(|p| p.canvas))
        .filter(|canvas| canvas.dimensions().is_ok());
    if let Some(canvas) = canvas {
        for output in &mut next.outputs {
            let old = previous
                .outputs
                .iter()
                .find(|o| o.r#match == output.r#match);
            if let Some(geometry) = output.geometry.as_mut() {
                let unchanged = old.is_some_and(|o| o.geometry.as_ref() == Some(&*geometry));
                let from = if unchanged { before } else { changed_unit };
                if let Err(error) = convert_geometry(geometry, from, target, &canvas) {
                    tracing::warn!(%error, "could not convert a slice between units");
                }
            }
            if let Some(offset) = output.arrange_offset.as_mut() {
                let unchanged = old.is_some_and(|o| o.arrange_offset.as_ref() == Some(&*offset));
                let from = if unchanged { before } else { changed_unit };
                if let Err(error) = convert_arrange_offset(offset, from, target, &canvas) {
                    tracing::warn!(%error, "could not convert an arrangement offset between units");
                }
            }
        }
    }
    if let Some(projection) = next.projection.as_mut() {
        projection.set_slice_units(target);
    }
}

/// Preserve omitted calibration for Simple clients, without overwriting explicit
/// shared canvas or slice edits. Explicit output deletion remains deletion.
///
/// Simple only. Warp writes are literal, and so are writes that stay in
/// Exact; a write that switches *into* Exact gets the same refill from
/// [`transition`], so it is judged as the one merged document it produces.
pub fn preserve_retained(next: &mut DesiredState, previous: &DesiredState) {
    let simple = next.projection.as_ref().is_none_or(|p| match p.mode {
        ProjectionMode::Simple => true,
        ProjectionMode::Exact | ProjectionMode::Warp => false,
    });
    if !simple {
        return;
    }
    refill_omitted(next, previous);
}

/// Fill an omitted `projection.canvas` and each output's omitted `geometry`
/// from `previous`, matching outputs by their `match` rule. Present values
/// are never touched, and an output `next` no longer lists stays gone.
///
/// A refilled geometry is in `previous`'s slice unit; when `next` settled on
/// the other one ([`settle_slice_units`]) it is converted on `next`'s
/// canvas (else `previous`'s), so the refill never mixes units.
///
/// Shared by [`preserve_retained`] and [`transition`], and through the
/// latter by [`exact_dry_run`].
pub(crate) fn refill_omitted(next: &mut DesiredState, previous: &DesiredState) {
    if let Some(old) = &previous.projection {
        if old.canvas.is_some() {
            // A section the refill creates is percent: with no section,
            // `settle_slice_units` has already put the write's slices in
            // percent, and the refill below converts what it copies.
            let projection = next.projection.get_or_insert_with(Default::default);
            if projection.canvas.is_none() {
                projection.canvas = old.canvas;
            }
        }
    }
    let (from, to) = (previous.slice_units(), next.slice_units());
    let canvas = next
        .projection
        .as_ref()
        .and_then(|projection| projection.canvas)
        .or_else(|| previous.projection.as_ref().and_then(|p| p.canvas))
        .filter(|canvas| canvas.dimensions().is_ok());
    for output in &mut next.outputs {
        if let Some(old) = previous
            .outputs
            .iter()
            .find(|o| o.r#match == output.r#match)
        {
            if output.geometry.is_none() {
                output.geometry = old.geometry.clone();
                if let (Some(geometry), Some(canvas)) = (output.geometry.as_mut(), canvas) {
                    if let Err(error) = convert_geometry(geometry, from, to, &canvas) {
                        tracing::warn!(%error, "could not convert a refilled slice between units");
                    }
                }
            }
        }
    }
}

/// Apply the projection-mode switching rules to a working-copy
/// replacement, after [`preserve_retained`] and before validation.
///
/// Runs only when `next`'s mode differs from `previous`'s (a missing
/// projection section counts as Simple):
///
/// * **Into Exact** (from Simple or Warp): the write is one merged document
///   with the previous working copy. An omitted canvas or output geometry
///   is first refilled from `previous`, exactly as [`preserve_retained`]
///   does for Simple, so a bare `{"mode":"exact"}` is judged on the layout
///   it would actually switch. [`exact::enter`] then keeps that document
///   once stripped of correction if it conforms, and otherwise resets its
///   geometry to the Exact defaults. Skipped where Exact is
///   unavailable ([`exact::unavailable_reason`]), so validation refuses the
///   write with that reason rather than with whatever a reset would trip
///   over.
/// * **Out of Exact** (to Simple or Warp): [`exact::leave`], which changes
///   only the mode; the Exact document already renders identically in the
///   new mode, which is what makes an immediate round trip byte-identical.
/// * **Simple and Warp**: nothing here; their retained-calibration
///   behavior is [`preserve_retained`]'s.
///
/// Returns what entering Exact did, or `None` when no Exact transition ran.
/// Fails with the reasons when a reset was needed and the defaults could not
/// be derived.
pub fn transition(
    next: &mut DesiredState,
    previous: &DesiredState,
    allow_overlaps: bool,
) -> Result<Option<exact::Transition>, Vec<String>> {
    let mode = |document: &DesiredState| {
        document
            .projection
            .as_ref()
            .map(|projection| projection.mode)
            .unwrap_or_default()
    };
    let (from, to) = (mode(previous), mode(next));
    match (from, to) {
        // Exact stores pixels. A write that stays in Exact is literal, but
        // one that leaves it in percent (an Exact document saved before
        // slice units, or a write naming percent) is converted; its slices
        // are whole pixels within `PIXEL_EPS` if it conforms at all, so
        // nothing moves.
        (ProjectionMode::Exact, ProjectionMode::Exact) => {
            if next.slice_units() == SliceUnits::Percent {
                if let Err(error) =
                    crate::model::slice_units::convert_document(next, SliceUnits::Pixels)
                {
                    tracing::warn!(%error, "could not convert an exact document to pixels");
                }
            }
            Ok(None)
        }
        (ProjectionMode::Simple, ProjectionMode::Simple)
        | (ProjectionMode::Warp, ProjectionMode::Warp)
        | (ProjectionMode::Simple, ProjectionMode::Warp)
        | (ProjectionMode::Warp, ProjectionMode::Simple) => Ok(None),
        (ProjectionMode::Simple | ProjectionMode::Warp, ProjectionMode::Exact) => {
            if exact::unavailable_reason(allow_overlaps).is_some() {
                return Ok(None);
            }
            refill_omitted(next, previous);
            let (entered, transition) = exact::enter(next)?;
            if let exact::Transition::Reset { reasons } = &transition {
                tracing::info!(
                    reasons = %reasons.join("; "),
                    "entering exact mode reset the projection layout to the exact defaults"
                );
            }
            *next = entered;
            Ok(Some(transition))
        }
        (ProjectionMode::Exact, ProjectionMode::Simple | ProjectionMode::Warp) => {
            *next = exact::leave(next, to);
            Ok(None)
        }
    }
}

/// What switching to Exact would do to a document, without doing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactDryRun {
    /// The document is already in Exact; a write changes nothing here.
    AlreadyExact,
    /// The layout conforms (once its correction is stripped) and is kept.
    Keep,
    /// The layout does not conform and is reset to the Exact defaults;
    /// `reasons` are the rules it breaks.
    Reset { reasons: Vec<String> },
    /// Exact cannot be used here; the write would be refused with this
    /// reason.
    Unavailable { reason: String },
}

/// Predict a switch of `document`'s projection to Exact by making it: the
/// mode is set on a clone, and [`transition`] (with its refill of omitted
/// canvas and geometry, its strip and its tiled skip) runs on that clone
/// against `document` as the previous working copy, exactly as a write does
/// through `api::validate_configuration_against`. Fails with the reasons the
/// write would be refused with when the reset itself cannot be derived.
pub fn exact_dry_run(
    document: &DesiredState,
    allow_overlaps: bool,
) -> Result<ExactDryRun, Vec<String>> {
    if document
        .projection
        .as_ref()
        .is_some_and(|projection| projection.mode == ProjectionMode::Exact)
    {
        return Ok(ExactDryRun::AlreadyExact);
    }
    if let Some(reason) = exact::unavailable_reason(allow_overlaps) {
        return Ok(ExactDryRun::Unavailable {
            reason: reason.to_string(),
        });
    }
    let mut next = document.clone();
    next.projection.get_or_insert_with(Default::default).mode = ProjectionMode::Exact;
    Ok(match transition(&mut next, document, allow_overlaps)? {
        Some(exact::Transition::Reset { reasons }) => ExactDryRun::Reset { reasons },
        Some(exact::Transition::Kept) | None => ExactDryRun::Keep,
    })
}

pub fn validate_activation(
    next: &DesiredState,
    previous: &DesiredState,
    control: &ProjectionControlStatus,
    running: bool,
    allow_overlaps: bool,
) -> Result<(), String> {
    let next_mode = next.projection.as_ref().map(|p| p.mode).unwrap_or_default();
    match next_mode {
        ProjectionMode::Warp => {}
        // Neither needs a capability probe: Exact's integer copy and Simple's
        // rectangles run on every slicer pipeline.
        ProjectionMode::Exact | ProjectionMode::Simple => return Ok(()),
    }
    let before_mode = previous
        .projection
        .as_ref()
        .map(|p| p.mode)
        .unwrap_or_default();
    // Shared slice selection and resolution remain editable during fallback.
    // Only activation or changes to retained correction require verified Warp.
    let correction_changed = next.outputs.iter().any(|o| {
        let old = previous
            .outputs
            .iter()
            .find(|p| p.r#match == o.r#match)
            .and_then(|p| p.geometry.as_ref());
        match (old, o.geometry.as_ref()) {
            (Some(old), Some(new)) => {
                old.corners != new.corners
                    || old.center != new.center
                    || old.raster_footprint != new.raster_footprint
            }
            (None, None) => false,
            _ => true,
        }
    });
    let capability = status(next, control, running, allow_overlaps, None);
    if capability.warp_available != Some(true)
        && (before_mode != ProjectionMode::Warp || correction_changed)
    {
        return Err(format!("warp_unavailable: {}; retained warp settings remain saved; use simple mode until capability is verified", capability.reason.unwrap_or_else(|| "the selected pipeline cannot warp".into())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CanvasConfig, CanvasRect, OutputConfig, OutputGeometry, OutputMatch, ProjectionConfig,
    };

    fn saved() -> DesiredState {
        let mut d = DesiredState::new();
        d.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Warp,
            canvas: Some(CanvasConfig {
                aspect: 1.0,
                render_width: 100,
            }),
            ..Default::default()
        });
        let mut output = OutputConfig::new(OutputMatch::by_name("A"));
        output.mode = Some(crate::model::Mode {
            width: 100,
            height: 100,
            refresh_hz: 60.0,
        });
        output.position = Some(crate::model::Position { x: 0, y: 0 });
        let r = CanvasRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        output.geometry = Some(OutputGeometry {
            slice: r,
            raster_footprint: r,
            corners: [[0.1, 0.1], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.4, 0.6],
        });
        d.outputs.push(output);
        d
    }

    #[test]
    fn simple_edits_cannot_replace_retained_geometry_or_canvas() {
        let before = saved();
        let mut next = before.clone();
        next.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        next.projection.as_mut().unwrap().canvas = None;
        next.outputs[0].geometry = None;
        next.outputs[0].position = Some(crate::model::Position { x: 13, y: 17 });
        preserve_retained(&mut next, &before);
        assert_eq!(next.outputs[0].geometry, before.outputs[0].geometry);
        assert_eq!(
            next.projection.as_ref().unwrap().canvas,
            before.projection.as_ref().unwrap().canvas
        );
        assert_eq!(next.outputs[0].position.unwrap().x, 13);
    }

    #[test]
    fn simple_shared_edits_survive_retention_and_do_not_need_integer_positions() {
        let before = saved();
        let mut next = before.clone();
        next.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        next.projection
            .as_mut()
            .unwrap()
            .canvas
            .as_mut()
            .unwrap()
            .render_width = 75;
        next.outputs[0].position = None;
        next.outputs[0].geometry.as_mut().unwrap().slice.x = -0.125;
        preserve_retained(&mut next, &before);
        assert_eq!(
            next.projection
                .as_ref()
                .unwrap()
                .canvas
                .unwrap()
                .render_width,
            75
        );
        let geometry = next.outputs[0].geometry.as_ref().unwrap();
        let old = before.outputs[0].geometry.as_ref().unwrap();
        assert_eq!(geometry.slice.x, -0.125);
        assert_eq!(geometry.corners, old.corners);
        assert_eq!(geometry.center, old.center);
        assert_eq!(geometry.raster_footprint, old.raster_footprint);
        assert!(next.validate(true).is_ok());
    }

    #[test]
    fn fallback_allows_shared_edits_but_still_guards_correction() {
        let before = saved();
        let mut next = before.clone();
        next.projection
            .as_mut()
            .unwrap()
            .canvas
            .as_mut()
            .unwrap()
            .render_width = 50;
        next.outputs[0].geometry.as_mut().unwrap().slice.x = 0.125;
        let cpu = ProjectionControlStatus {
            requested_renderer: Some(Renderer::Auto),
            effective_renderer: Some(Renderer::Cpu),
            warp_available: Some(false),
            ..Default::default()
        };
        assert!(validate_activation(&next, &before, &cpu, true, true).is_ok());
        assert_eq!(next.projection.as_ref().unwrap().mode, ProjectionMode::Warp);
        next.outputs[0].geometry.as_mut().unwrap().center[0] = 0.625;
        assert!(validate_activation(&next, &before, &cpu, true, true).is_err());
    }

    #[test]
    fn shared_simple_validates_sources_instead_of_stale_integer_positions() {
        let mut next = saved();
        next.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        let mut second = next.outputs[0].clone();
        second.r#match = OutputMatch::by_name("B");
        second.position = Some(crate::model::Position { x: 10000, y: 10000 });
        second.geometry.as_mut().unwrap().slice.x = 0.5;
        next.outputs[0].geometry.as_mut().unwrap().slice.width = 0.5;
        second.geometry.as_mut().unwrap().slice.width = 0.5;
        next.outputs.push(second);
        assert!(next.validate(true).is_ok());
        next.outputs[1].geometry.as_mut().unwrap().slice.x = 0.6;
        assert!(next
            .validate(true)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("disconnected")));
        next.outputs[1].geometry = None;
        assert!(next
            .validate(true)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("geometry is required")));
    }

    #[test]
    fn shared_simple_rejects_unallocatable_compositor_scale() {
        let mut next = saved();
        next.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        next.outputs[0].scale = Some(0.01);
        assert!(next
            .validate(true)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("presentation allocation")));
        next.outputs[0].scale = Some(1e-10);
        assert!(next
            .validate(true)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("presentation dimensions")));
        next.outputs[0].scale = Some(2.0);
        next.outputs[0].transform = Some(crate::model::Transform::Rotate90);
        assert!(next.validate(true).is_ok());
        next.projection.as_mut().unwrap().mode = ProjectionMode::Warp;
        assert!(next
            .validate(true)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("scale must be 1.0")));
        assert!(!next
            .validate(true)
            .unwrap_err()
            .iter()
            .any(|e| e.contains("transform")));
        // Scale 1 with a 90 degree transform is valid in Warp and yields a
        // swapped (logical) destination.
        next.outputs[0].scale = Some(1.0);
        next.outputs[0].mode = Some(crate::model::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        });
        assert!(next.validate(true).is_ok());
        let mode = next.outputs[0].effective_mode().unwrap();
        assert_eq!(
            next.outputs[0].presentation_dimensions(mode, true),
            Ok((1080, 1920))
        );
    }

    #[test]
    fn unsupported_or_unresolved_activation_rejects_without_replacing_saved_state() {
        let requested = saved();
        let mut simple = requested.clone();
        simple.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        assert!(
            validate_activation(&requested, &simple, &Default::default(), false, true).is_err()
        );
        let cpu = ProjectionControlStatus {
            requested_renderer: Some(Renderer::Auto),
            effective_renderer: Some(Renderer::Cpu),
            warp_available: Some(false),
            ..Default::default()
        };
        assert!(validate_activation(&requested, &simple, &cpu, true, true).is_err());
        assert_eq!(
            status(&requested, &cpu, true, true, None).effective_mode,
            ProjectionMode::Simple
        );
        assert_eq!(simple.outputs[0].geometry, requested.outputs[0].geometry);
    }

    #[cfg(feature = "projection")]
    #[test]
    fn capability_recovery_restores_saved_pins_and_pending_restart_does_not_oscillate() {
        let requested = saved();
        let mut simple = requested.clone();
        simple.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        let gpu = ProjectionControlStatus {
            requested_renderer: Some(Renderer::Auto),
            effective_renderer: Some(Renderer::Gpu),
            warp_available: Some(true),
            ..Default::default()
        };
        assert!(validate_activation(&requested, &simple, &gpu, true, true).is_ok());
        let restored = status(&requested, &gpu, true, true, None);
        assert_eq!(restored.effective_mode, ProjectionMode::Warp);
        let pending = status(&requested, &Default::default(), true, true, Some(&restored));
        assert_eq!(pending.effective_mode, ProjectionMode::Warp);
        assert_eq!(pending.warp_available, None);
        let failed = ProjectionControlStatus {
            warp_available: Some(false),
            ..gpu
        };
        assert_eq!(
            status(&requested, &failed, true, true, Some(&pending)).effective_mode,
            ProjectionMode::Simple
        );
        let mut changed = requested.clone();
        changed.projection.as_mut().unwrap().renderer = Renderer::Gpu;
        assert_eq!(
            status(&changed, &Default::default(), true, true, Some(&restored)).effective_mode,
            ProjectionMode::Simple
        );
    }

    #[cfg(feature = "projection")]
    #[tokio::test]
    async fn api_preview_save_revert_and_recovery_preserve_shared_settings() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let mut h = crate::api::test_support::harness(None);
        std::sync::Arc::make_mut(&mut h.state.bootstrap).allow_overlaps = true;
        let app = crate::api::router(h.state.clone());
        let mut baseline = saved();
        baseline.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        let baseline = h.state.store.replace(baseline).unwrap();
        let mut requested = baseline.clone();
        requested.committed = false;
        requested.projection.as_mut().unwrap().mode = ProjectionMode::Warp;
        // Every write names the document it read, the way a real client does:
        // once a working copy is live, an unconditional write is refused so
        // that it cannot discard a second operator's unsaved edit.
        let store = h.state.store.clone();
        let put = move |d: &DesiredState| {
            let (_, version) = store.effective_with_version();
            Request::builder()
                .method("PUT")
                .uri("/api/v1/config")
                .header("content-type", "application/json")
                .header("if-config-generation", version.generation.to_string())
                .header("if-config-epoch", version.epoch)
                .body(Body::from(serde_json::to_vec(d).unwrap()))
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(put(&requested)).await.unwrap().status(),
            422
        );
        assert_eq!(h.state.store.effective(), baseline);
        h.state.snapshot.set_slicer_running(true);
        h.state
            .snapshot
            .set_projection_control(ProjectionControlStatus {
                requested_renderer: Some(Renderer::Auto),
                effective_renderer: Some(Renderer::Gpu),
                warp_available: Some(true),
                ..Default::default()
            });
        assert_eq!(
            app.clone().oneshot(put(&requested)).await.unwrap().status(),
            200
        );
        let first_generation = h.state.store.generation();
        requested.outputs[0].geometry.as_mut().unwrap().corners[0] = [0.2, 0.15];
        assert_eq!(
            app.clone().oneshot(put(&requested)).await.unwrap().status(),
            200
        );
        assert!(h.state.store.generation() > first_generation);
        let before = h.state.store.effective_with_generation();
        let recommendation = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projection/recommendation")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(recommendation.status(), 200);
        assert_eq!(h.state.store.effective_with_generation(), before);
        assert_eq!(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/config/revert")
                        .header(
                            "if-config-generation",
                            h.state.store.generation().to_string(),
                        )
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            h.state.store.effective().outputs[0].geometry,
            baseline.outputs[0].geometry
        );

        // A full simple-mode save omits both retained fields; the server keeps
        // them while accepting the independent rectangle position edit.
        let mut simple = baseline.clone();
        simple.committed = true;
        simple.projection.as_mut().unwrap().renderer = Renderer::Cpu;
        simple.projection.as_mut().unwrap().canvas = None;
        simple.outputs[0].geometry = None;
        simple.outputs[0].position = Some(crate::model::Position { x: 10, y: 20 });
        assert_eq!(
            app.clone().oneshot(put(&simple)).await.unwrap().status(),
            200
        );
        let saved = h.state.store.get();
        assert_eq!(saved.outputs[0].geometry, baseline.outputs[0].geometry);
        assert_eq!(
            saved.projection.as_ref().unwrap().canvas,
            baseline.projection.as_ref().unwrap().canvas
        );
        let reloaded = crate::state::StateStore::load(h._dir.path().to_path_buf()).unwrap();
        assert_eq!(reloaded.get(), saved);
        let mut restore = saved.clone();
        restore.committed = false;
        restore.projection.as_mut().unwrap().renderer = Renderer::Auto;
        restore.projection.as_mut().unwrap().mode = ProjectionMode::Warp;
        assert_eq!(
            app.clone().oneshot(put(&restore)).await.unwrap().status(),
            200
        );
        assert_eq!(
            h.state.store.effective().outputs[0].geometry,
            baseline.outputs[0].geometry
        );

        // A real capability loss keeps Warp intent, but does not make shared
        // framing read-only. Commit and disk reload must retain both edits and
        // correction; invalid crops must leave the accepted generation intact.
        h.state
            .snapshot
            .set_projection_control(ProjectionControlStatus {
                requested_renderer: Some(Renderer::Auto),
                effective_renderer: Some(Renderer::Cpu),
                warp_available: Some(false),
                ..Default::default()
            });
        let mut fallback = h.state.store.effective();
        fallback.committed = true;
        fallback
            .projection
            .as_mut()
            .unwrap()
            .canvas
            .as_mut()
            .unwrap()
            .render_width = 75;
        fallback.outputs[0].geometry.as_mut().unwrap().slice.x = -0.125;
        assert_eq!(
            app.clone().oneshot(put(&fallback)).await.unwrap().status(),
            200
        );
        let accepted = h.state.store.effective_with_generation();
        let reloaded = crate::state::StateStore::load(h._dir.path().to_path_buf()).unwrap();
        assert_eq!(reloaded.get(), accepted.0);
        assert_eq!(
            reloaded.get().projection.unwrap().mode,
            ProjectionMode::Warp
        );
        assert_eq!(
            accepted.0.outputs[0].geometry.as_ref().unwrap().corners,
            baseline.outputs[0].geometry.as_ref().unwrap().corners
        );
        fallback.outputs[0].geometry.as_mut().unwrap().slice.x = 2.0;
        assert_eq!(app.oneshot(put(&fallback)).await.unwrap().status(), 422);
        assert_eq!(h.state.store.effective_with_generation(), accepted);
    }

    /// Two 100x100 outputs overlapping by 10 px, reset into Exact from their
    /// positions alone.
    fn exact_saved() -> DesiredState {
        let mut d = DesiredState::new();
        for (name, x) in [("A", 0), ("B", 90)] {
            let mut output = OutputConfig::new(OutputMatch::by_name(name));
            output.mode = Some(crate::model::Mode {
                width: 100,
                height: 100,
                refresh_hz: 60.0,
            });
            output.position = Some(crate::model::Position { x, y: 0 });
            d.outputs.push(output);
        }
        d.projection = Some(ProjectionConfig {
            gamma: 2.4,
            ..Default::default()
        });
        let (entered, transition) = exact::enter(&d).unwrap();
        assert!(matches!(transition, exact::Transition::Reset { .. }));
        assert!(
            entered.validate(true).is_ok(),
            "{:?}",
            entered.validate(true)
        );
        entered
    }

    /// What `api::validate_configuration_against` does to a replacement
    /// before validating it, on a slicer appliance.
    fn replace(mut next: DesiredState, previous: &DesiredState) -> DesiredState {
        settle_slice_units(&mut next, previous);
        preserve_retained(&mut next, previous);
        transition(&mut next, previous, true).unwrap();
        assert!(next.validate(true).is_ok(), "{:?}", next.validate(true));
        next
    }

    fn with_mode(document: &DesiredState, mode: ProjectionMode) -> DesiredState {
        let mut next = document.clone();
        next.projection.as_mut().unwrap().mode = mode;
        next
    }

    #[test]
    fn exact_round_trips_through_simple_and_warp_byte_identically() {
        let exact = exact_saved();
        let start = serde_json::to_string(&exact).unwrap();
        for to in [ProjectionMode::Simple, ProjectionMode::Warp] {
            let left = replace(with_mode(&exact, to), &exact);
            // Leaving changes the mode and nothing else.
            assert_eq!(left, exact::leave(&exact, to), "{to:?}");
            let back = replace(with_mode(&left, ProjectionMode::Exact), &left);
            assert_eq!(serde_json::to_string(&back).unwrap(), start, "{to:?}");
        }
    }

    #[test]
    fn entering_exact_resets_a_calibrated_warp_document() {
        let warp = saved();
        let mut next = with_mode(&warp, ProjectionMode::Exact);
        // The canvas the warp document used is not the positions' box.
        next.projection
            .as_mut()
            .unwrap()
            .canvas
            .as_mut()
            .unwrap()
            .render_width = 150;
        let outcome = transition(&mut next, &warp, true).unwrap();
        assert!(
            matches!(outcome, Some(exact::Transition::Reset { .. })),
            "{outcome:?}"
        );
        let geometry = next.outputs[0].geometry.as_ref().unwrap();
        assert_eq!(geometry.corners, exact::UNIT_CORNERS);
        assert_eq!(geometry.center, exact::NEUTRAL_CENTER);
        assert_eq!(
            next.projection
                .as_ref()
                .unwrap()
                .canvas
                .unwrap()
                .render_width,
            100
        );
        assert!(next.validate(true).is_ok(), "{:?}", next.validate(true));
    }

    #[test]
    fn a_switch_into_exact_is_judged_with_the_omitted_layout_refilled() {
        let simple = with_mode(&exact_saved(), ProjectionMode::Simple);
        // A bare `{"mode":"exact"}`: no canvas, and an output edit that
        // dropped a geometry.
        let mut next = with_mode(&simple, ProjectionMode::Exact);
        next.projection.as_mut().unwrap().canvas = None;
        next.outputs[1].geometry = None;
        let outcome = transition(&mut next, &simple, true).unwrap();
        assert_eq!(outcome, Some(exact::Transition::Kept));
        assert_eq!(next, with_mode(&simple, ProjectionMode::Exact));
    }

    #[test]
    fn a_write_that_stays_in_exact_is_literal() {
        let exact = exact_saved();
        let mut next = exact.clone();
        next.projection.as_mut().unwrap().canvas = None;
        preserve_retained(&mut next, &exact);
        assert_eq!(transition(&mut next, &exact, true).unwrap(), None);
        assert!(next.projection.as_ref().unwrap().canvas.is_none());
        assert!(next
            .validate(true)
            .unwrap_err()
            .contains(&"projection.canvas is required in exact mode".to_string()));
    }

    #[test]
    fn a_write_that_stays_in_exact_is_not_transitioned() {
        let exact = exact_saved();
        let mut next = exact.clone();
        next.outputs[0].scale = Some(2.0);
        assert_eq!(transition(&mut next, &exact, true).unwrap(), None);
        assert_eq!(next.outputs[0].scale, Some(2.0));
        assert_eq!(
            next.validate(true).unwrap_err(),
            ["outputs[0].scale must be 1.0 in exact mode"]
        );
    }

    #[test]
    fn simple_and_warp_switches_are_left_to_retention() {
        let warp = saved();
        let mut simple = with_mode(&warp, ProjectionMode::Simple);
        let untouched = simple.clone();
        assert_eq!(transition(&mut simple, &warp, true).unwrap(), None);
        assert_eq!(simple, untouched);
        let mut back = with_mode(&simple, ProjectionMode::Warp);
        let untouched = back.clone();
        assert_eq!(transition(&mut back, &simple, true).unwrap(), None);
        assert_eq!(back, untouched);
    }

    #[test]
    fn a_tiled_appliance_is_not_reset_into_exact() {
        let mut simple = exact_saved();
        simple.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        simple.projection.as_mut().unwrap().canvas = None;
        for output in &mut simple.outputs {
            output.geometry = None;
        }
        // Edge to edge, as a tiled appliance needs them.
        simple.outputs[1].position = Some(crate::model::Position { x: 100, y: 0 });
        assert!(simple.validate(false).is_ok());
        let mut next = with_mode(&simple, ProjectionMode::Exact);
        assert_eq!(transition(&mut next, &simple, false).unwrap(), None);
        assert_eq!(next, with_mode(&simple, ProjectionMode::Exact));
        assert_eq!(next.validate(false).unwrap_err(), [exact::TILED_REASON]);
    }

    #[test]
    fn a_reset_that_cannot_be_derived_fails_with_its_reasons() {
        let mut simple = exact_saved();
        simple.projection.as_mut().unwrap().mode = ProjectionMode::Simple;
        simple.projection.as_mut().unwrap().canvas = None;
        for output in &mut simple.outputs {
            output.geometry = None;
        }
        simple.outputs[1].position = None;
        let mut next = with_mode(&simple, ProjectionMode::Exact);
        assert_eq!(
            transition(&mut next, &simple, true).unwrap_err(),
            ["outputs[1].position is required for a shared canvas"]
        );
    }

    #[test]
    fn exact_needs_no_warp_capability() {
        let simple = with_mode(&exact_saved(), ProjectionMode::Simple);
        let exact = with_mode(&simple, ProjectionMode::Exact);
        assert!(validate_activation(&exact, &simple, &Default::default(), false, true).is_ok());
        let status = status(&exact, &Default::default(), false, true, None);
        assert_eq!(status.requested_mode, ProjectionMode::Exact);
        assert_eq!(status.effective_mode, ProjectionMode::Exact);
    }

    #[cfg(feature = "projection")]
    #[test]
    fn exact_availability_follows_the_tiled_policy() {
        let exact = exact_saved();
        let slicer = status(&exact, &Default::default(), false, true, None);
        assert_eq!(slicer.exact_available, Some(true));
        assert_eq!(slicer.exact_reason, None);
        assert_eq!(slicer.effective_mode, ProjectionMode::Exact);

        let tiled = status(&exact, &Default::default(), false, false, None);
        assert_eq!(tiled.exact_available, Some(false));
        assert_eq!(tiled.exact_reason.as_deref(), Some(exact::TILED_REASON));
        assert_eq!(tiled.effective_mode, ProjectionMode::Simple);
        assert_eq!(tiled.requested_mode, ProjectionMode::Exact);

        // Reported whatever the requested mode is, so a client can disable
        // the toggle before anyone asks for Exact.
        let simple = with_mode(&exact, ProjectionMode::Simple);
        assert_eq!(
            status(&simple, &Default::default(), false, false, None).exact_available,
            Some(false)
        );
    }

    #[cfg(feature = "projection")]
    #[test]
    fn exact_with_the_cpu_renderer_is_effective_exact() {
        let mut exact = exact_saved();
        exact.projection.as_mut().unwrap().renderer = Renderer::Cpu;
        let status = status(&exact, &Default::default(), true, true, None);
        assert_eq!(status.effective_mode, ProjectionMode::Exact);
        assert_eq!(status.exact_available, Some(true));
    }

    #[test]
    fn the_dry_run_predicts_each_outcome_of_the_real_transition() {
        let exact = exact_saved();
        let simple = with_mode(&exact, ProjectionMode::Simple);
        assert_eq!(exact_dry_run(&simple, true), Ok(ExactDryRun::Keep));
        assert_eq!(exact_dry_run(&exact, true), Ok(ExactDryRun::AlreadyExact));
        assert!(matches!(
            exact_dry_run(&simple, false),
            Ok(ExactDryRun::Unavailable { .. })
        ));
        let mut fractional = simple.clone();
        fractional.outputs[0].geometry.as_mut().unwrap().slice.x += 0.0005;
        let Ok(ExactDryRun::Reset { reasons }) = exact_dry_run(&fractional, true) else {
            panic!("a fractional slice must reset");
        };
        assert!(!reasons.is_empty());
        // The prediction is the real transition's own outcome.
        let mut next = with_mode(&fractional, ProjectionMode::Exact);
        let real = transition(&mut next, &fractional, true).unwrap();
        assert_eq!(real, Some(exact::Transition::Reset { reasons }));
    }

    #[cfg(feature = "projection")]
    #[tokio::test]
    async fn api_previews_round_trip_exact_through_simple_and_warp() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let mut h = crate::api::test_support::harness(None);
        std::sync::Arc::make_mut(&mut h.state.bootstrap).allow_overlaps = true;
        let app = crate::api::router(h.state.clone());
        // Warp activation needs a verified capability; Exact does not.
        h.state.snapshot.set_slicer_running(true);
        h.state
            .snapshot
            .set_projection_control(ProjectionControlStatus {
                requested_renderer: Some(Renderer::Auto),
                effective_renderer: Some(Renderer::Gpu),
                warp_available: Some(true),
                ..Default::default()
            });
        let saved = h.state.store.replace(exact_saved()).unwrap();
        let store = h.state.store.clone();
        let put = move |d: &DesiredState| {
            let (_, version) = store.effective_with_version();
            Request::builder()
                .method("PUT")
                .uri("/api/v1/config")
                .header("content-type", "application/json")
                .header("if-config-generation", version.generation.to_string())
                .header("if-config-epoch", version.epoch)
                .body(Body::from(serde_json::to_vec(d).unwrap()))
                .unwrap()
        };
        let mut start = saved.clone();
        start.committed = false;
        // Coming back to the saved document byte for byte leaves a working
        // copy with no edit in it: the store reports it as committed, so it
        // serializes exactly as the saved document does.
        let saved_json = serde_json::to_string(&saved).unwrap();
        for to in [ProjectionMode::Simple, ProjectionMode::Warp] {
            let response = app
                .clone()
                .oneshot(put(&with_mode(&start, to)))
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{to:?}");
            let left = h.state.store.effective();
            assert_eq!(left, with_mode(&start, to), "{to:?}");
            let response = app
                .clone()
                .oneshot(put(&with_mode(&left, ProjectionMode::Exact)))
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{to:?}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let returned: DesiredState = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                serde_json::to_string(&returned).unwrap(),
                saved_json,
                "{to:?}"
            );
            assert_eq!(
                serde_json::to_string(&h.state.store.effective()).unwrap(),
                saved_json,
                "{to:?}"
            );
            assert!(!h.state.store.has_unsaved_edits(), "{to:?}");
        }
    }

    // --- slice units --------------------------------------------------------

    /// Two 100x100 outputs side by side on a 200x100 canvas, in percent,
    /// with A's slice deliberately off whole pixels (x at 0.3 px).
    fn percent_pair() -> DesiredState {
        let mut d = DesiredState::new();
        for (name, x) in [("A", 0.003 / 2.0), ("B", 0.5)] {
            let mut output = OutputConfig::new(OutputMatch::by_name(name));
            output.mode = Some(crate::model::Mode {
                width: 100,
                height: 100,
                refresh_hz: 60.0,
            });
            let slice = CanvasRect {
                x,
                y: 0.0,
                width: 0.5,
                height: 0.5,
            };
            output.geometry = Some(OutputGeometry {
                slice,
                raster_footprint: slice,
                corners: exact::UNIT_CORNERS,
                center: exact::NEUTRAL_CENTER,
            });
            d.outputs.push(output);
        }
        d.projection = Some(ProjectionConfig {
            canvas: Some(CanvasConfig {
                aspect: 2.0,
                render_width: 200,
            }),
            ..Default::default()
        });
        d
    }

    fn slices(document: &DesiredState) -> Vec<CanvasRect> {
        document
            .outputs
            .iter()
            .map(|output| output.geometry.as_ref().unwrap().slice)
            .collect()
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CanvasRect {
        CanvasRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn naming_the_other_unit_converts_the_unchanged_slices() {
        let previous = percent_pair();
        let mut next = previous.clone();
        next.projection.as_mut().unwrap().slice_units = Some(SliceUnits::Pixels);
        let next = replace(next, &previous);
        assert_eq!(next.slice_units(), SliceUnits::Pixels);
        // A's edges at 0.3 and 100.3 px round to 0 and 100.
        assert_eq!(
            slices(&next),
            [rect(0.0, 0.0, 100.0, 100.0), rect(100.0, 0.0, 100.0, 100.0)]
        );

        // And back: exact division, the stored unit absent again.
        let mut back = next.clone();
        back.projection.as_mut().unwrap().slice_units = Some(SliceUnits::Percent);
        let back = replace(back, &next);
        assert_eq!(back.projection.as_ref().unwrap().slice_units, None);
        assert_eq!(
            slices(&back),
            [rect(0.0, 0.0, 0.5, 0.5), rect(0.5, 0.0, 0.5, 0.5)]
        );
    }

    #[test]
    fn a_write_in_the_other_unit_is_taken_literally_where_it_changes_a_slice() {
        let previous = percent_pair();
        let mut next = previous.clone();
        next.projection.as_mut().unwrap().slice_units = Some(SliceUnits::Pixels);
        // B rewritten in pixels; A left as it was, so it is converted.
        next.outputs[1].geometry.as_mut().unwrap().slice = rect(90.0, 0.0, 100.0, 100.0);
        next.outputs[1].geometry.as_mut().unwrap().raster_footprint = rect(90.0, 0.0, 100.0, 100.0);
        let next = replace(next, &previous);
        assert_eq!(
            slices(&next),
            [rect(0.0, 0.0, 100.0, 100.0), rect(90.0, 0.0, 100.0, 100.0)]
        );
    }

    #[test]
    fn an_omitted_unit_keeps_the_previous_one() {
        let mut previous = percent_pair();
        crate::model::slice_units::convert_document(&mut previous, SliceUnits::Pixels).unwrap();
        // A client that does not know the field: a projection section built
        // from scratch, and a bare switch to Exact.
        let mut next = previous.clone();
        next.projection.as_mut().unwrap().slice_units = None;
        next.projection.as_mut().unwrap().gamma = 2.4;
        let next = replace(next, &previous);
        assert_eq!(next.slice_units(), SliceUnits::Pixels);
        assert_eq!(slices(&next), slices(&previous));

        let mut bare = previous.clone();
        bare.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Exact,
            ..Default::default()
        });
        let entered = replace(bare, &previous);
        assert_eq!(
            entered.projection.as_ref().unwrap().mode,
            ProjectionMode::Exact
        );
        assert_eq!(entered.slice_units(), SliceUnits::Pixels);
        assert_eq!(slices(&entered), slices(&previous));
    }

    #[test]
    fn a_canvas_created_where_there_was_none_stores_pixels() {
        // A fresh document, then a write that brings a canvas and the
        // fractional slices any client without the field sends.
        let previous = DesiredState::new();
        let next = replace(percent_pair(), &previous);
        assert_eq!(
            next.projection.as_ref().unwrap().slice_units,
            Some(SliceUnits::Pixels)
        );
        assert_eq!(
            slices(&next),
            [rect(0.0, 0.0, 100.0, 100.0), rect(100.0, 0.0, 100.0, 100.0)]
        );
        // Naming percent keeps percent.
        let mut explicit = percent_pair();
        explicit.projection.as_mut().unwrap().slice_units = Some(SliceUnits::Percent);
        let kept = replace(explicit, &previous);
        assert_eq!(kept.projection.as_ref().unwrap().slice_units, None);
        assert_eq!(slices(&kept), slices(&percent_pair()));
        // A loaded document without the field is percent, and an ordinary
        // write to it stays percent.
        let loaded = percent_pair();
        let mut edit = loaded.clone();
        edit.projection.as_mut().unwrap().gamma = 2.4;
        let edited = replace(edit, &loaded);
        assert_eq!(edited.slice_units(), SliceUnits::Percent);
        assert_eq!(slices(&edited), slices(&loaded));
    }

    /// A write with no projection section is percent by definition; the
    /// Simple refill that brings the canvas back keeps the picture.
    #[test]
    fn dropping_the_projection_section_keeps_the_picture() {
        let mut previous = percent_pair();
        crate::model::slice_units::convert_document(&mut previous, SliceUnits::Pixels).unwrap();
        let mut next = previous.clone();
        next.projection = None;
        let next = replace(next, &previous);
        assert_eq!(next.slice_units(), SliceUnits::Percent);
        assert_eq!(
            next.projection.as_ref().unwrap().canvas,
            previous.projection.as_ref().unwrap().canvas
        );
        assert_eq!(
            slices(&next),
            [rect(0.0, 0.0, 0.5, 0.5), rect(0.5, 0.0, 0.5, 0.5)]
        );
    }

    #[test]
    fn a_refilled_slice_follows_the_settled_unit() {
        let previous = percent_pair();
        let mut next = previous.clone();
        next.projection.as_mut().unwrap().slice_units = Some(SliceUnits::Pixels);
        next.outputs[0].geometry = None;
        let next = replace(next, &previous);
        assert_eq!(
            slices(&next),
            [rect(0.0, 0.0, 100.0, 100.0), rect(100.0, 0.0, 100.0, 100.0)]
        );
    }

    /// An Exact document saved before slice units is percent; the first
    /// write that stays in Exact stores it in pixels, moving nothing.
    #[test]
    fn a_write_that_stays_in_an_exact_percent_document_converts_it() {
        let mut legacy = exact_saved();
        crate::model::slice_units::convert_document(&mut legacy, SliceUnits::Percent).unwrap();
        assert_eq!(legacy.slice_units(), SliceUnits::Percent);
        assert!(legacy.validate(true).is_ok());
        let mut next = legacy.clone();
        next.projection.as_mut().unwrap().gamma = 2.2;
        let next = replace(next, &legacy);
        assert_eq!(next.slice_units(), SliceUnits::Pixels);
        assert_eq!(slices(&next), slices(&exact_saved()));
    }

    #[test]
    fn status_reports_slices_past_the_canvas() {
        let mut document = percent_pair();
        crate::model::slice_units::convert_document(&mut document, SliceUnits::Pixels).unwrap();
        document
            .projection
            .as_mut()
            .unwrap()
            .canvas
            .as_mut()
            .unwrap()
            .render_width = 150;
        let reported = status(
            &document,
            &ProjectionControlStatus::default(),
            false,
            true,
            None,
        );
        // The canvas is 150x75 now: A hangs 25 px below it, B also 50 px
        // past its right edge.
        assert_eq!(reported.out_of_canvas.len(), 2);
        assert_eq!(
            reported.out_of_canvas[0].message,
            "A extends 25 px past the canvas's bottom edge"
        );
        let overhang = &reported.out_of_canvas[1];
        assert_eq!(
            (overhang.output.as_str(), overhang.right, overhang.bottom),
            ("B", 50, 25)
        );
        assert_eq!(
            overhang.message,
            "B extends 50 px past the canvas's right edge and 25 px past its bottom edge"
        );
        let json = serde_json::to_value(&reported).unwrap();
        assert_eq!(json["outOfCanvas"][1]["right"], 50);
        // Nothing past the canvas: the field is omitted.
        let clear = status(
            &percent_pair(),
            &ProjectionControlStatus::default(),
            false,
            true,
            None,
        );
        assert!(serde_json::to_value(&clear)
            .unwrap()
            .get("outOfCanvas")
            .is_none());
    }
}
