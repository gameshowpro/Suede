//! Read-only endpoints, plus the imperative escape hatches.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::model::{AudioSink, Check, Output, Status, SystemInfo, Window};

#[utoipa::path(
    get, path = "/api/v1/outputs", tag = "observed",
    responses((status = 200, description = "Outputs reported by sway", body = Vec<Output>))
)]
pub async fn list_outputs(State(state): State<ApiState>) -> Json<Vec<Output>> {
    Json(state.snapshot.outputs())
}

#[utoipa::path(
    get, path = "/api/v1/outputs/{name}", tag = "observed",
    params(("name" = String, Path, description = "Connector name, e.g. HDMI-A-1")),
    responses(
        (status = 200, description = "The output", body = Output),
        (status = 404, description = "No such output"),
    )
)]
pub async fn get_output(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Output>> {
    state
        .snapshot
        .output(&name)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no output named {name}")))
}

#[utoipa::path(
    get, path = "/api/v1/windows", tag = "observed",
    responses((status = 200, description = "Windows in sway's tree", body = Vec<Window>))
)]
pub async fn list_windows(State(state): State<ApiState>) -> Json<Vec<Window>> {
    Json(state.snapshot.windows())
}

#[utoipa::path(
    get, path = "/api/v1/audio/outputs", tag = "observed",
    responses((status = 200, description = "Audio sinks reported by PipeWire", body = Vec<AudioSink>))
)]
pub async fn list_audio_outputs(State(state): State<ApiState>) -> Json<Vec<AudioSink>> {
    Json(state.audio.sinks())
}

#[utoipa::path(
    get, path = "/api/v1/status", tag = "observed",
    responses((status = 200, description = "Reconciliation status", body = Status))
)]
pub async fn get_status(State(state): State<ApiState>) -> Json<Status> {
    Json(state.snapshot.status())
}

#[utoipa::path(
    get, path = "/api/v1/system", tag = "observed",
    responses((status = 200, description = "Daemon and environment information", body = SystemInfo))
)]
pub async fn get_system(State(state): State<ApiState>) -> Json<SystemInfo> {
    let version = state.sway.get_version().await.ok();
    Json(SystemInfo {
        suede_version: crate::VERSION.to_string(),
        sway_version: version.as_ref().map(|v| v.display()),
        hostname: hostname(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        packages: state.checks.package_versions().await,
        supports_tearing: version.as_ref().is_some_and(|v| v.supports_tearing()),
        web_ui_enabled: !state.bootstrap.auth_enabled(),
    })
}

#[utoipa::path(
    get, path = "/api/v1/system/checks", tag = "observed",
    responses((status = 200, description = "Environment health checks", body = Vec<Check>))
)]
pub async fn list_checks(State(state): State<ApiState>) -> Json<Vec<Check>> {
    Json(state.checks.run_all().await)
}

/// What a remediation did.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FixOutcome {
    pub id: String,
    pub detail: String,
}

#[utoipa::path(
    post, path = "/api/v1/system/checks/{id}/fix", tag = "observed",
    params(("id" = String, Path, description = "Check identifier")),
    responses(
        (status = 200, description = "What the fix did", body = FixOutcome),
        (status = 404, description = "No automated fix exists for this check"),
    )
)]
pub async fn fix_check(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<FixOutcome>> {
    let detail = state.checks.fix(&id).await?;
    tracing::info!(check = %id, %detail, "applied environment fix");
    Ok(Json(FixOutcome { id, detail }))
}

#[utoipa::path(
    post, path = "/api/v1/reconcile", tag = "control",
    responses((status = 200, description = "Status after the pass", body = Status))
)]
pub async fn reconcile_now(State(state): State<ApiState>) -> Json<Status> {
    Json(state.reconciler.reconcile().await)
}

/// A raw Sway command, for debugging.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SwayCommand {
    pub command: String,
}

#[utoipa::path(
    post, path = "/api/v1/sway/command", tag = "control",
    request_body = SwayCommand,
    responses(
        (status = 204, description = "Sway accepted the command"),
        (status = 403, description = "Raw commands are disabled"),
        (status = 503, description = "Sway rejected the command"),
    )
)]
pub async fn run_sway_command(
    State(state): State<ApiState>,
    Json(body): Json<SwayCommand>,
) -> ApiResult<axum::http::StatusCode> {
    if !state.store.get().settings.allow_raw_sway_commands {
        return Err(ApiError::Forbidden(
            "raw sway commands are disabled; set settings.allowRawSwayCommands to enable".into(),
        ));
    }
    tracing::warn!(command = %body.command, "running raw sway command");
    state
        .sway
        .run_command(&body.command)
        .await
        .map_err(|error| ApiError::Unavailable(error.to_string()))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use crate::api::test_support::harness;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
        let harness = harness(None);
        // Populate the snapshot the way a reconciliation pass would.
        harness.state.reconciler.refresh_outputs().await;
        harness.state.reconciler.refresh_windows().await;

        let response = harness
            .router
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn lists_outputs() {
        let (status, body) = get_json("/api/v1/outputs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 3);
        assert_eq!(body[0]["name"], "HDMI-A-1");
        // camelCase is the API convention.
        assert!(body[0].get("currentMode").is_some());
    }

    #[tokio::test]
    async fn gets_one_output() {
        let (status, body) = get_json("/api/v1/outputs/HDMI-A-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "HDMI-A-1");
    }

    #[tokio::test]
    async fn unknown_output_is_404() {
        let (status, _) = get_json("/api/v1/outputs/HDMI-A-99").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn lists_windows() {
        let (status, body) = get_json("/api/v1/windows").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn lists_audio_outputs() {
        let (status, body) = get_json("/api/v1/audio/outputs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reports_status() {
        let (status, body) = get_json("/api/v1/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "synced");
        assert!(body["divergences"].is_array());
    }

    #[tokio::test]
    async fn reports_system_information() {
        let (status, body) = get_json("/api/v1/system").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["suedeVersion"], crate::VERSION);
        assert_eq!(body["webUiEnabled"], true);
        assert!(body["packages"].is_array());
    }

    #[tokio::test]
    async fn runs_health_checks() {
        let (status, body) = get_json("/api/v1/system/checks").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 12);
        assert!(body[0].get("fixAvailable").is_some());
    }

    #[tokio::test]
    async fn raw_sway_commands_are_refused_by_default() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/sway/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"command":"exec danger"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!harness.sway.ran_command_containing("danger"));
    }

    #[tokio::test]
    async fn raw_sway_commands_run_when_enabled() {
        let harness = harness(None);
        harness
            .state
            .store
            .update(|state| state.settings.allow_raw_sway_commands = true)
            .unwrap();

        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/sway/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"command":"reload"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(harness.sway.ran_command_containing("reload"));
    }

    #[tokio::test]
    async fn unknown_check_fix_is_404() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/system/checks/nonsense/fix")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
