//! OpenAPI generation and the Scalar reference UI.
//!
//! The document is generated from the code, so `suede openapi` (used by CI to
//! build the published API reference) and the runtime `/docs` page can never
//! drift apart.

use axum::response::IntoResponse;
use axum::Router;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_scalar::{Scalar, Servable};

use super::ApiState;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .description(Some(
                            "Optional. When a token is configured, every endpoint except \
                             /healthz and the heartbeat endpoint requires it.",
                        ))
                        .build(),
                ),
            );
        }
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Suede",
        description = "Remote management for Sway-based display appliances. \
                       Clients write desired state; a reconciler drives the live \
                       session toward it.",
        license(name = "MIT", url = "https://github.com/gameshowpro/Suede/blob/main/LICENSE"),
    ),
    paths(
        super::observed::list_outputs,
        super::observed::get_output,
        super::observed::list_windows,
        super::observed::list_audio_outputs,
        super::observed::get_status,
        super::observed::get_system,
        super::observed::list_checks,
        super::observed::fix_check,
        super::observed::reconcile_now,
        super::observed::run_sway_command,
        super::apps::list_apps,
        super::apps::get_app_status,
        super::apps::restart_app,
        super::apps::heartbeat,
        super::config_routes::get_config,
        super::config_routes::put_config,
        super::config_routes::get_outputs,
        super::config_routes::put_outputs,
        super::config_routes::get_backgrounds,
        super::config_routes::put_backgrounds,
        super::config_routes::put_background,
        super::config_routes::delete_background,
        super::config_routes::get_output,
        super::config_routes::put_output,
        super::config_routes::delete_output,
        super::config_routes::get_apps,
        super::config_routes::put_apps,
        super::config_routes::get_app,
        super::config_routes::put_app,
        super::config_routes::delete_app,
        super::config_routes::get_settings,
        super::config_routes::put_settings,
        super::config_routes::get_projection,
        super::config_routes::put_projection,
        super::events::stream,
        super::wallpapers::list,
        super::wallpapers::upload,
        super::wallpapers::download,
        super::wallpapers::delete,
    ),
    components(schemas(
        crate::model::Mode,
        crate::model::Position,
        crate::model::Rect,
        crate::model::Output,
        crate::model::Window,
        crate::model::AppState,
        crate::model::AppStatus,
        crate::model::RestartReason,
        crate::model::AudioSink,
        crate::model::SyncState,
        crate::model::Divergence,
        crate::model::Status,
        crate::model::PackageVersion,
        crate::model::SystemInfo,
        crate::model::CheckStatus,
        crate::model::Check,
        crate::model::WindowChange,
        crate::model::ConfigChange,
        crate::model::DesiredState,
        crate::model::OutputMatch,
        crate::model::Transform,
        crate::model::OutputConfig,
        crate::model::Launcher,
        crate::model::RestartPolicyKind,
        crate::model::RestartPolicy,
        crate::model::AudioConfig,
        crate::model::HeartbeatConfig,
        crate::model::AppConfig,
        crate::model::Settings,
        crate::model::ProjectionConfig,
        crate::model::TestPattern,
        crate::model::Background,
        crate::model::BackgroundMode,
        crate::model::BackgroundPreset,
        crate::model::BackgroundRef,
        crate::model::ReadinessConfig,
        crate::wallpapers::Wallpaper,
        crate::error::Problem,
        super::observed::FixOutcome,
        super::observed::SwayCommand,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "observed", description = "Live state reported by sway and PipeWire"),
        (name = "config", description = "Desired state, persisted and reconciled"),
        (name = "apps", description = "Managed application status and control"),
        (name = "control", description = "Imperative escape hatches"),
        (name = "events", description = "Server-sent change notification"),
        (name = "wallpapers", description = "Images shown when no window covers an output"),
    )
)]
pub struct ApiDoc;

/// The generated document, pretty-printed.
pub fn openapi_document() -> String {
    ApiDoc::openapi()
        .to_pretty_json()
        .expect("the OpenAPI document is serializable")
}

pub async fn openapi_json() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        openapi_document(),
    )
}

/// The Scalar reference UI, served at `/docs`.
pub fn scalar_router() -> Router<ApiState> {
    Scalar::with_url("/docs", ApiDoc::openapi()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> serde_json::Value {
        serde_json::from_str(&openapi_document()).unwrap()
    }

    #[test]
    fn document_is_openapi_3_1() {
        let document = document();
        assert!(document["openapi"].as_str().unwrap().starts_with("3.1"));
        assert_eq!(document["info"]["title"], "Suede");
    }

    #[test]
    fn every_route_family_is_documented() {
        let document = document();
        let paths = document["paths"].as_object().unwrap();
        for path in [
            "/api/v1/outputs",
            "/api/v1/outputs/{name}",
            "/api/v1/windows",
            "/api/v1/audio/outputs",
            "/api/v1/status",
            "/api/v1/system",
            "/api/v1/system/checks",
            "/api/v1/apps",
            "/api/v1/apps/{id}/status",
            "/api/v1/apps/{id}/heartbeat",
            "/api/v1/config",
            "/api/v1/config/outputs/{key}",
            "/api/v1/config/apps/{id}",
            "/api/v1/config/settings",
            "/api/v1/events",
        ] {
            assert!(paths.contains_key(path), "missing path {path}");
        }
    }

    #[test]
    fn schemas_use_camel_case_properties() {
        let document = document();
        let output = &document["components"]["schemas"]["Output"]["properties"];
        assert!(output.get("currentMode").is_some());
        assert!(output.get("current_mode").is_none());
    }

    #[test]
    fn launcher_is_a_tagged_union() {
        let document = document();
        let launcher = &document["components"]["schemas"]["Launcher"];
        let text = launcher.to_string();
        assert!(text.contains("chromium-kiosk"), "got: {text}");
        assert!(text.contains("firefox-kiosk"));
        assert!(text.contains("exec"));
    }

    #[test]
    fn bearer_security_scheme_is_declared() {
        let document = document();
        let scheme = &document["components"]["securitySchemes"]["bearer"];
        assert_eq!(scheme["scheme"], "bearer");
    }

    #[test]
    fn generation_needs_no_compositor() {
        // `suede openapi` runs in CI, where there is no sway and no network.
        assert!(!openapi_document().is_empty());
    }
}
