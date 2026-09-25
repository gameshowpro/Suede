//! Application status and control.

use std::net::SocketAddr;

use crate::api::config_routes::WaitQuery;
use crate::api::json::Json;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;

use super::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::model::AppStatus;

/// One argument or variable, and where it came from.
///
/// The distinction is the point of the preview: a preset contributes most of
/// what is launched, and an operator needs to see which parts are theirs to
/// change and which arrive automatically.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewItem {
    /// For an argument this is the argument; for a variable, its value.
    pub value: String,
    /// Variable name. Absent for arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `preset`, `app`, or `uri` — the expanded URI the preset appends last.
    pub source: &'static str,
}

/// Exactly what an application would be launched as.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchPreview {
    /// The binary that would run, resolved on this machine. `null` when none
    /// of the candidates is installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// What was looked for, in order. A single entry when the application
    /// names its own program.
    pub searched: Vec<String>,
    pub args: Vec<PreviewItem>,
    pub env: Vec<PreviewItem>,
    /// Browser profile directory, when the launcher manages one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<String>,
    /// Whether that directory is emptied before each launch.
    pub wipe_profile: bool,
    /// The origin that will be granted persistent camera/microphone access
    /// in the profile above, before launch. Present only when a grant will
    /// actually be written — `grantCapture` is on and the URI has an
    /// `http`/`https` origin — not merely when the launcher is chromium.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_grant_origin: Option<String>,
}

#[utoipa::path(
    post, path = "/api/v1/apps/preview", tag = "apps",
    request_body = crate::model::AppConfig,
    responses((
        status = 200,
        description = "What this application would be launched as, resolved                        against this machine. Nothing is started.",
        body = LaunchPreview,
    ))
)]
pub async fn preview_app(
    State(state): State<ApiState>,
    Json(app): Json<crate::model::AppConfig>,
) -> Json<LaunchPreview> {
    use crate::supervisor::launcher;
    let context = state.supervisor.launch_context();
    let chosen = launcher::choose_program(&app);
    let spec = launcher::build(&app, context, chosen.as_deref());

    // Provenance is worked out by comparing the built specification against
    // what the document asked for, rather than threading tags through the
    // builder: the specification stays the single source of truth, and the
    // preview cannot drift from what would actually be spawned.
    let extra: Vec<String> = match &app.launcher {
        crate::model::Launcher::ChromiumKiosk { extra_args, .. }
        | crate::model::Launcher::FirefoxKiosk { extra_args, .. } => extra_args.clone(),
        crate::model::Launcher::Exec { args, .. } => args.clone(),
    };
    let uri = match &app.launcher {
        crate::model::Launcher::ChromiumKiosk { uri, .. }
        | crate::model::Launcher::FirefoxKiosk { uri, .. } => {
            Some(launcher::expand_uri(uri, &app.id, context))
        }
        crate::model::Launcher::Exec { .. } => None,
    };

    let args = spec
        .args
        .iter()
        .map(|arg| PreviewItem {
            value: arg.clone(),
            name: None,
            source: if uri.as_deref() == Some(arg.as_str()) {
                "uri"
            } else if extra.contains(arg) {
                "app"
            } else {
                "preset"
            },
        })
        .collect();

    let env = spec
        .env
        .iter()
        .map(|(name, value)| PreviewItem {
            value: value.clone(),
            name: Some(name.clone()),
            source: if app.env.contains_key(name) {
                "app"
            } else {
                "preset"
            },
        })
        .collect();

    Json(LaunchPreview {
        program: chosen.map(|p| p.display().to_string()),
        searched: launcher::candidates_for(&app),
        args,
        env,
        profile_dir: spec.profile_dir.map(|p| p.display().to_string()),
        wipe_profile: spec.wipe_profile,
        capture_grant_origin: spec.grant_capture.then_some(spec.capture_origin).flatten(),
    })
}

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
    params(("id" = String, Path, description = "App identifier"), WaitQuery),
    responses(
        (status = 200, description = "The persisted document", body = crate::model::DesiredState),
        (status = 404, description = "No such app"),
    )
)]
pub async fn activate_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
) -> ApiResult<Json<crate::model::DesiredState>> {
    let mut next = state.store.get();
    if !next.apps.iter().any(|app| app.id == id) {
        return Err(ApiError::NotFound(format!("no app named {id}")));
    }
    // One pointer, one active app: activating B deactivates A atomically.
    next.active_app = Some(id.clone());
    tracing::info!(app = %id, "activated");
    state.commit(next, "apps", query.wait).await.map(Json)
}

#[utoipa::path(
    post, path = "/api/v1/apps/{id}/deactivate", tag = "apps",
    params(("id" = String, Path, description = "App identifier"), WaitQuery),
    responses(
        (status = 200, description = "The persisted document", body = crate::model::DesiredState),
        (status = 404, description = "No such app"),
        (status = 409, description = "A different app is active"),
    )
)]
pub async fn deactivate_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
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
    state.commit(next, "apps", query.wait).await.map(Json)
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

    /// Like [`send`], but keeps the response headers — the CORS tests need
    /// to see `access-control-allow-*`, which `send` discards.
    async fn send_with_headers(
        harness: &Harness,
        request: Request<Body>,
        peer: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut request = request;
        let address: SocketAddr = peer.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(address));
        let response = harness.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
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

    /// Page content posting a heartbeat is cross-origin (another host or
    /// port than the API, sometimes `file://`), so Chromium sends a
    /// preflight, including its private-network variant when the page
    /// itself is treated as a public address. The route must answer it
    /// directly rather than falling through to a plain 405.
    #[tokio::test]
    async fn heartbeat_preflight_gets_cors_headers() {
        let harness = with_app("renderer").await;
        let request = Request::builder()
            .method("OPTIONS")
            .uri("/api/v1/apps/renderer/heartbeat")
            .header("Origin", "http://example.test")
            .header("Access-Control-Request-Method", "POST")
            .header("Access-Control-Request-Private-Network", "true")
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send_with_headers(&harness, request, "127.0.0.1:1").await;
        assert!(status.is_success());
        assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
        assert!(headers
            .get("access-control-allow-methods")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("POST"));
        assert_eq!(
            headers.get("access-control-allow-private-network").unwrap(),
            "true"
        );
        harness.state.supervisor.shutdown().await;
    }

    /// The real POST, not just the preflight, needs the header: it is what
    /// the page's `fetch` promise actually checks.
    #[tokio::test]
    async fn heartbeat_post_carries_cors_header_on_success_and_404() {
        let harness = with_app("renderer").await;

        let ok = Request::builder()
            .method("POST")
            .uri("/api/v1/apps/renderer/heartbeat")
            .header("Origin", "http://example.test")
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send_with_headers(&harness, ok, "127.0.0.1:1").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");

        let unknown = Request::builder()
            .method("POST")
            .uri("/api/v1/apps/nope/heartbeat")
            .header("Origin", "http://example.test")
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send_with_headers(&harness, unknown, "127.0.0.1:1").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");

        harness.state.supervisor.shutdown().await;
    }

    /// The CORS layer is scoped to the heartbeat route only; every other
    /// endpoint must stay same-origin.
    #[tokio::test]
    async fn other_routes_carry_no_cors_headers() {
        let harness = harness(None);
        let request = Request::builder()
            .uri("/api/v1/status")
            .header("Origin", "http://example.test")
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send_with_headers(&harness, request, "127.0.0.1:1").await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("access-control-allow-origin").is_none());
    }

    /// Without `?wait=`, a status read right after activation can land
    /// before the pass that launches the app; with it, the response only
    /// comes back once that pass has run, so the read that follows is not a
    /// race.
    #[tokio::test]
    async fn activating_with_wait_blocks_until_the_app_is_launched() {
        let harness = harness(None);
        harness
            .state
            .store
            .update(|state| state.apps.push(sleeper("renderer")))
            .unwrap();

        let (status, _) = send(
            &harness,
            post("/api/v1/apps/renderer/activate?wait=5"),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = send(
            &harness,
            Request::builder()
                .uri("/api/v1/apps/renderer/status")
                .body(Body::empty())
                .unwrap(),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let app: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // A headless exec app owns no window, so it is running immediately
        // once the pass that `wait` blocked for has completed.
        assert_eq!(app["state"], "running");
        harness.state.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn activating_without_wait_still_succeeds() {
        let harness = harness(None);
        harness
            .state
            .store
            .update(|state| state.apps.push(sleeper("renderer")))
            .unwrap();

        let (status, _) = send(
            &harness,
            post("/api/v1/apps/renderer/activate"),
            "127.0.0.1:1",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        harness.state.supervisor.shutdown().await;
    }
}
