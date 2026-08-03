//! Application status and control.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::Json;

use super::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::model::AppStatus;

#[utoipa::path(
    get, path = "/api/v1/apps", tag = "apps",
    responses((status = 200, description = "Status of every managed app", body = Vec<AppStatus>))
)]
pub async fn list_apps(State(state): State<ApiState>) -> Json<Vec<AppStatus>> {
    Json(state.supervisor.statuses().await)
}

#[utoipa::path(
    get, path = "/api/v1/apps/{id}/status", tag = "apps",
    params(("id" = String, Path, description = "App identifier")),
    responses(
        (status = 200, description = "Runtime status", body = AppStatus),
        (status = 404, description = "No such app"),
    )
)]
pub async fn get_app_status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<AppStatus>> {
    state
        .supervisor
        .status(&id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no app named {id}")))
}

#[utoipa::path(
    post, path = "/api/v1/apps/{id}/restart", tag = "apps",
    params(("id" = String, Path, description = "App identifier")),
    responses(
        (status = 200, description = "Status after the restart", body = AppStatus),
        (status = 404, description = "No such app"),
    )
)]
pub async fn restart_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<AppStatus>> {
    if !state.supervisor.restart(&id).await {
        return Err(ApiError::NotFound(format!("no app named {id}")));
    }
    tracing::info!(app = %id, "restarted on request");
    state
        .supervisor
        .status(&id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no app named {id}")))
}

#[utoipa::path(
    post, path = "/api/v1/apps/{id}/activate", tag = "apps",
    params(("id" = String, Path, description = "App identifier")),
    responses(
        (status = 200, description = "The persisted document", body = crate::model::DesiredState),
        (status = 404, description = "No such app"),
    )
)]
pub async fn activate_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::model::DesiredState>> {
    let mut next = state.store.get();
    if !next.apps.iter().any(|app| app.id == id) {
        return Err(ApiError::NotFound(format!("no app named {id}")));
    }
    // One pointer, one active app: activating B deactivates A atomically.
    next.active_app = Some(id.clone());
    tracing::info!(app = %id, "activated");
    state.commit(next, "apps", None).await.map(Json)
}

#[utoipa::path(
    post, path = "/api/v1/apps/{id}/deactivate", tag = "apps",
    params(("id" = String, Path, description = "App identifier")),
    responses(
        (status = 200, description = "The persisted document", body = crate::model::DesiredState),
        (status = 404, description = "No such app"),
        (status = 409, description = "A different app is active"),
    )
)]
pub async fn deactivate_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::model::DesiredState>> {
    let mut next = state.store.get();
    if !next.apps.iter().any(|app| app.id == id) {
        return Err(ApiError::NotFound(format!("no app named {id}")));
    }
    // Deactivating an app someone else already replaced would silently kill
    // *their* app; say so instead.
    match next.active_app.as_deref() {
        Some(active) if active == id => next.active_app = None,
        Some(active) => {
            return Err(ApiError::Conflict(format!(
                "{active} is the active app, not {id}"
            )))
        }
        None => {}
    }
    tracing::info!(app = %id, "deactivated");
    state.commit(next, "apps", None).await.map(Json)
}

#[utoipa::path(
    post, path = "/api/v1/apps/{id}/heartbeat", tag = "apps",
    params(("id" = String, Path, description = "App identifier")),
    responses(
        (status = 204, description = "Heartbeat recorded"),
        (status = 403, description = "Only loopback callers may post heartbeats"),
        (status = 404, description = "No such app"),
    )
)]
pub async fn heartbeat(
    State(state): State<ApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    // Deliberately unauthenticated so page content can call it without holding
    // the API token — restricted to the local machine instead.
    if !peer.ip().is_loopback() {
        return Err(ApiError::Forbidden(
            "heartbeats are only accepted from the local machine".into(),
        ));
    }
    if !state.supervisor.heartbeat(&id).await {
        return Err(ApiError::NotFound(format!("no app named {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::api::test_support::{harness, Harness};
    use crate::model::{AppConfig, HeartbeatConfig, Launcher, RestartPolicy};
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    fn sleeper(id: &str) -> AppConfig {
        AppConfig {
            id: id.into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "sleep".into(),
                args: vec!["30".into()],
            },
            output: None,
            fullscreen: true,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio: None,
            heartbeat: Some(HeartbeatConfig {
                enabled: true,
                ..Default::default()
            }),
            restart: RestartPolicy::default(),
            persist_profile: false,
        }
    }

    /// Start one managed app, as a reconciliation pass would.
    async fn with_app(id: &str) -> Harness {
        let harness = harness(None);
        harness
            .state
            .store
            .update(|state| {
                state.apps.push(sleeper(id));
                // Only the active app runs; there is no per-app enable.
                state.active_app = Some(id.to_string());
            })
            .unwrap();
        harness.state.reconciler.reconcile().await;
        harness
    }

    /// Send a request from a given peer address, as the real server would.
    async fn send(harness: &Harness, request: Request<Body>, peer: &str) -> (StatusCode, Vec<u8>) {
        let mut request = request;
        let address: SocketAddr = peer.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(address));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    fn post(uri: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn lists_managed_apps() {
        let harness = with_app("renderer").await;
        let (status, body) = send(
            &harness,
            Request::builder()
                .uri("/api/v1/apps")
                .body(Body::empty())
                .unwrap(),
            "127.0.0.1:1234",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let apps: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(apps.as_array().unwrap().len(), 1);
        assert_eq!(apps[0]["id"], "renderer");
        harness.state.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn reports_one_app_status() {
        let harness = with_app("renderer").await;
        let (status, body) = send(
            &harness,
            Request::builder()
                .uri("/api/v1/apps/renderer/status")
                .body(Body::empty())
                .unwrap(),
            "127.0.0.1:1234",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let app: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(app["id"], "renderer");
        assert!(app["pid"].is_number());
        harness.state.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_app_status_is_404() {
        let harness = harness(None);
        let (status, _) = send(
            &harness,
            Request::builder()
                .uri("/api/v1/apps/nope/status")
                .body(Body::empty())
                .unwrap(),
            "127.0.0.1:1234",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn restart_replaces_the_process() {
        let harness = with_app("renderer").await;
        let before = harness
            .state
            .supervisor
            .status("renderer")
            .await
            .unwrap()
            .pid;

        let (status, _) = send(
            &harness,
            post("/api/v1/apps/renderer/restart"),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let after = harness
            .state
            .supervisor
            .status("renderer")
            .await
            .unwrap()
            .pid;
        assert_ne!(before, after);
        harness.state.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn restarting_an_unknown_app_is_404() {
        let harness = harness(None);
        let (status, _) = send(&harness, post("/api/v1/apps/nope/restart"), "127.0.0.1:1").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn loopback_heartbeat_is_accepted() {
        let harness = with_app("renderer").await;
        let (status, _) = send(
            &harness,
            post("/api/v1/apps/renderer/heartbeat"),
            "127.0.0.1:54321",
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(harness
            .state
            .supervisor
            .status("renderer")
            .await
            .unwrap()
            .last_heartbeat
            .is_some());
        harness.state.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn remote_heartbeat_is_refused() {
        let harness = with_app("renderer").await;
        let (status, _) = send(
            &harness,
            post("/api/v1/apps/renderer/heartbeat"),
            "192.168.1.50:54321",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(harness
            .state
            .supervisor
            .status("renderer")
            .await
            .unwrap()
            .last_heartbeat
            .is_none());
        harness.state.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeat_for_an_unknown_app_is_404() {
        let harness = harness(None);
        let (status, _) = send(&harness, post("/api/v1/apps/nope/heartbeat"), "127.0.0.1:1").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn heartbeat_needs_no_token_even_in_token_mode() {
        let harness = harness(Some("secret"));
        harness
            .state
            .store
            .update(|state| state.apps.push(sleeper("renderer")))
            .unwrap();
        harness.state.reconciler.reconcile().await;

        let (status, _) = send(
            &harness,
            post("/api/v1/apps/renderer/heartbeat"),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        harness.state.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn ipv6_loopback_is_accepted() {
        let harness = with_app("renderer").await;
        let (status, _) = send(
            &harness,
            post("/api/v1/apps/renderer/heartbeat"),
            "[::1]:9999",
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        harness.state.supervisor.shutdown().await;
    }
}
