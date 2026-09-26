//! Public geometry capability policy. Saved parameters are never the fallback.

use crate::model::observed::ProjectionGeometryStatus;
use crate::model::{DesiredState, ProjectionControlStatus, ProjectionMode, Renderer};

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
    ProjectionGeometryStatus {
        requested_renderer: projection.renderer,
        requested_mode: projection.mode,
        // Keep an already selected warp through its child replacement probe.
        // A pending report is not a negative capability result. Controls still
        // require available=true; an actual startup false selects simple.
        effective_mode: if projection.mode == ProjectionMode::Warp
            && (available == Some(true)
                || (available.is_none()
                    && previous.is_some_and(|p| {
                        p.effective_mode == ProjectionMode::Warp
                            && p.requested_renderer == projection.renderer
                    }))) {
            ProjectionMode::Warp
        } else {
            ProjectionMode::Simple
        },
        warp_available: available,
        reason,
        retained_warp: retained,
    }
}

/// Preserve omitted calibration for Simple clients, without overwriting explicit
/// shared canvas or slice edits. Explicit output deletion remains deletion.
pub fn preserve_retained(next: &mut DesiredState, previous: &DesiredState) {
    let simple = next
        .projection
        .as_ref()
        .is_none_or(|p| p.mode == ProjectionMode::Simple);
    if !simple {
        return;
    }
    if let Some(old) = &previous.projection {
        if old.canvas.is_some() {
            let projection = next.projection.get_or_insert_with(Default::default);
            if projection.canvas.is_none() {
                projection.canvas = old.canvas;
            }
        }
    }
    for output in &mut next.outputs {
        if let Some(old) = previous
            .outputs
            .iter()
            .find(|o| o.r#match == output.r#match)
        {
            if output.geometry.is_none() {
                output.geometry = old.geometry.clone();
            }
        }
    }
}

pub fn validate_activation(
    next: &DesiredState,
    previous: &DesiredState,
    control: &ProjectionControlStatus,
    running: bool,
    allow_overlaps: bool,
) -> Result<(), String> {
    let next_mode = next.projection.as_ref().map(|p| p.mode).unwrap_or_default();
    if next_mode != ProjectionMode::Warp {
        return Ok(());
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
            .any(|e| e.contains("transform must be normal")));
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
}
