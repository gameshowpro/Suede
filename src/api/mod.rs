//! The HTTP API: REST for state, SSE for change notification.

pub mod apps;
pub mod config_routes;
pub mod docs;
pub mod events;
pub mod observed;
pub mod ui;
pub mod wallpapers;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;

use crate::audio::AudioMonitor;
use crate::checks::CheckRunner;
use crate::config::BootstrapConfig;
use crate::error::{ApiError, ApiResult};
use crate::events::{EventHub, ServerEvent};
use crate::model::{ConfigChange, DesiredState};
use crate::reconciler::{ReconcileTrigger, Reconciler};
use crate::snapshot::Snapshot;
use crate::state::StateStore;
use crate::supervisor::Supervisor;
use crate::sway::SwayClient;

/// Everything a request handler needs.
#[derive(Clone)]
pub struct ApiState {
    pub bootstrap: Arc<BootstrapConfig>,
    pub store: Arc<StateStore>,
    pub snapshot: Arc<Snapshot>,
    pub events: EventHub,
    pub sway: Arc<dyn SwayClient>,
    pub audio: Arc<dyn AudioMonitor>,
    pub supervisor: Arc<Supervisor>,
    pub reconciler: Arc<Reconciler>,
    pub trigger: ReconcileTrigger,
    pub checks: Arc<CheckRunner>,
    pub wallpapers: Arc<crate::wallpapers::WallpaperStore>,
    pub started_at: Instant,
}

impl ApiState {
    /// Persist a validated document, then reconcile.
    ///
    /// Writes return once *saved*, not once applied: application is
    /// asynchronous and may currently be impossible. `wait` opts into blocking
    /// until reconciliation settles.
    pub async fn commit(
        &self,
        next: DesiredState,
        section: &str,
        wait: Option<u64>,
    ) -> ApiResult<DesiredState> {
        next.validate()
            .map_err(|errors| ApiError::Validation(errors.join("; ")))?;

        let saved = self
            .store
            .replace(next)
            .map_err(|error| ApiError::Internal(error.to_string()))?;

        self.events
            .publish(ServerEvent::ConfigChanged(ConfigChange {
                revision: saved.revision,
                section: section.to_string(),
            }));

        match wait {
            Some(seconds) => {
                let timeout = std::time::Duration::from_secs(seconds.clamp(1, 120));
                if tokio::time::timeout(timeout, self.reconciler.reconcile())
                    .await
                    .is_err()
                {
                    tracing::warn!(section, "reconciliation did not settle within the wait");
                }
            }
            None => self.trigger.request("config write"),
        }

        Ok(saved)
    }

    /// Reject a write whose `If-Match` revision is stale.
    pub fn check_precondition(&self, if_match: Option<&str>) -> ApiResult<()> {
        let Some(value) = if_match else {
            return Ok(());
        };
        let expected: u64 = value
            .trim()
            .trim_matches('"')
            .parse()
            .map_err(|_| ApiError::BadRequest("If-Match must be a revision number".into()))?;
        let current = self.store.revision();
        if expected != current {
            return Err(ApiError::Conflict(format!(
                "document is at revision {current}, not {expected}"
            )));
        }
        Ok(())
    }
}

/// Build the complete application router.
pub fn router(state: ApiState) -> Router {
    let api = Router::new()
        // Observed state.
        .route("/outputs", get(observed::list_outputs))
        .route("/outputs/{name}", get(observed::get_output))
        .route("/windows", get(observed::list_windows))
        .route("/audio/outputs", get(observed::list_audio_outputs))
        .route("/status", get(observed::get_status))
        .route("/system", get(observed::get_system))
        .route("/system/checks", get(observed::list_checks))
        .route("/system/checks/{id}/fix", post(observed::fix_check))
        // Applications.
        .route("/apps", get(apps::list_apps))
        .route("/apps/{id}/status", get(apps::get_app_status))
        .route("/apps/{id}/restart", post(apps::restart_app))
        .route("/apps/{id}/activate", post(apps::activate_app))
        .route("/apps/{id}/deactivate", post(apps::deactivate_app))
        .route("/apps/{id}/heartbeat", post(apps::heartbeat))
        // Desired state.
        .route(
            "/config",
            get(config_routes::get_config).put(config_routes::put_config),
        )
        .route(
            "/config/outputs",
            get(config_routes::get_outputs).put(config_routes::put_outputs),
        )
        .route("/config/outputs/{key}", get(config_routes::get_output))
        .route("/config/outputs/{key}", put(config_routes::put_output))
        .route(
            "/config/outputs/{key}",
            delete(config_routes::delete_output),
        )
        .route(
            "/config/backgrounds",
            get(config_routes::get_backgrounds).put(config_routes::put_backgrounds),
        )
        .route(
            "/config/backgrounds/{id}",
            put(config_routes::put_background).delete(config_routes::delete_background),
        )
        .route(
            "/config/apps",
            get(config_routes::get_apps).put(config_routes::put_apps),
        )
        .route("/config/apps/{id}", get(config_routes::get_app))
        .route("/config/apps/{id}", put(config_routes::put_app))
        .route("/config/apps/{id}", delete(config_routes::delete_app))
        .route(
            "/config/settings",
            get(config_routes::get_settings).put(config_routes::put_settings),
        )
        .route(
            "/config/projection",
            get(config_routes::get_projection).put(config_routes::put_projection),
        )
        .route(
            "/config/preview",
            put(config_routes::put_preview).delete(config_routes::delete_preview),
        )
        // Imperative escape hatches.
        .route("/reconcile", post(observed::reconcile_now))
        .route("/sway/command", post(observed::run_sway_command))
        // Wallpapers.
        .route("/wallpapers", get(wallpapers::list))
        .route(
            "/wallpapers/{id}",
            get(wallpapers::download)
                .put(wallpapers::upload)
                .delete(wallpapers::delete),
        )
        // Uploads are images, well over axum's default 2 MB body limit.
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::wallpapers::MAX_BYTES,
        ))
        // Events.
        .route("/events", get(events::stream));

    let mut app = Router::new()
        .nest("/api/v1", api)
        .route("/healthz", get(health))
        .route("/api-docs/openapi.json", get(docs::openapi_json));

    // A web UI would have to embed the token to work, which would defeat it.
    if !state.bootstrap.auth_enabled() {
        app = app.merge(docs::scalar_router()).merge(ui::router());
    }

    app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        authenticate,
    ))
    .layer(tower_http::trace::TraceLayer::new_for_http())
    .with_state(state)
}

/// Liveness. Unversioned and never authenticated.
async fn health(State(state): State<ApiState>) -> Response {
    let sway_up = state.sway.is_connected();
    let body = serde_json::json!({
        "status": if sway_up { "ok" } else { "degraded" },
        "sway": sway_up,
        "version": crate::VERSION,
    });
    // Still 200 when sway is down: the daemon is alive and will reconnect.
    (StatusCode::OK, axum::Json(body)).into_response()
}

/// Paths reachable without a bearer token even when one is configured.
fn is_public(path: &str) -> bool {
    path == "/healthz"
        // Heartbeats come from page content, which cannot hold the API token;
        // that endpoint is restricted to loopback callers instead.
        || (path.starts_with("/api/v1/apps/") && path.ends_with("/heartbeat"))
}

async fn authenticate(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // A request that arrived from off-box is proof the port is reachable —
    // something the reachability check cannot establish any other way, since
    // reading the host firewall's rules needs root. Recorded before the token
    // is examined: a rejected request crossed the network just as well.
    if let Some(peer) = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
    {
        state.checks.note_client(peer.0.ip());
    }

    let Some(expected) = state.bootstrap.token.as_deref() else {
        return Ok(next.run(request).await);
    };
    if is_public(request.uri().path()) {
        return Ok(next.run(request).await);
    }

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);

    match presented {
        Some(token) if constant_time_eq(token, expected) => Ok(next.run(request).await),
        _ => Err(ApiError::Unauthorized),
    }
}

/// Compare without leaking length or position through timing.
fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use crate::audio::mock::MockAudio;
    use crate::reconciler::ReconcilerDeps;
    use crate::supervisor::LaunchContext;
    use crate::sway::mock::MockSway;

    pub struct Harness {
        pub state: ApiState,
        pub router: Router,
        pub sway: Arc<MockSway>,
        pub audio: Arc<MockAudio>,
        pub _dir: tempfile::TempDir,
    }

    /// A fully wired API backed by mocks, with no background tasks running.
    pub fn harness(token: Option<&str>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = Arc::new(BootstrapConfig {
            token: token.map(str::to_string),
            state_dir: dir.path().to_path_buf(),
            ..BootstrapConfig::default()
        });
        let sway = Arc::new(MockSway::with_fixtures());
        let audio = Arc::new(MockAudio::with_sinks());
        let store = Arc::new(StateStore::ephemeral(dir.path().to_path_buf()));
        let snapshot = Arc::new(Snapshot::new());
        let hub = EventHub::new();
        let wallpapers = Arc::new(crate::wallpapers::WallpaperStore::new(
            dir.path().join("wallpapers"),
        ));
        let supervisor = Arc::new(Supervisor::new(
            sway.clone(),
            hub.clone(),
            LaunchContext {
                profiles_root: dir.path().join("profiles"),
                log_root: dir.path().join("logs"),
                api_base: "http://127.0.0.1:9088/api/v1".into(),
            },
        ));
        let reconciler = Arc::new(Reconciler::new(ReconcilerDeps {
            sway: sway.clone(),
            audio: audio.clone(),
            store: store.clone(),
            snapshot: snapshot.clone(),
            supervisor: supervisor.clone(),
            events: hub.clone(),
            wallpapers: wallpapers.clone(),
            docs_base_url: bootstrap.docs_base_url.clone(),
        }));
        let checks = Arc::new(CheckRunner::new(
            bootstrap.clone(),
            sway.clone(),
            audio.clone(),
            store.clone(),
            hub.clone(),
        ));
        let (trigger, _receiver) = Reconciler::channel();

        let state = ApiState {
            bootstrap,
            store,
            snapshot,
            events: hub,
            sway: sway.clone(),
            audio: audio.clone(),
            supervisor,
            reconciler,
            trigger,
            checks,
            wallpapers,
            started_at: Instant::now(),
        };

        Harness {
            router: router(state.clone()),
            state,
            sway,
            audio,
            _dir: dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::harness;
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn healthz_is_open() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["version"], crate::VERSION);
    }

    #[tokio::test]
    async fn healthz_stays_open_in_token_mode() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn no_token_configured_means_no_authentication() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_mode_rejects_an_anonymous_request() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_mode_accepts_the_right_token() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_mode_rejects_a_wrong_token() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs")
                    .header("authorization", "Bearer wrong!")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn errors_are_problem_json() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/v1/outputs/NOPE")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/problem+json"
        );
        let body = body_json(response).await;
        assert_eq!(body["status"], 404);
        assert!(body["type"].as_str().unwrap().contains("not-found"));
    }

    #[tokio::test]
    async fn scalar_docs_are_served_without_a_token() {
        let harness = harness(None);
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/docs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn scalar_docs_are_absent_in_token_mode() {
        let harness = harness(Some("secret"));
        let response = harness
            .router
            .oneshot(
                HttpRequest::builder()
                    .uri("/docs")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn heartbeat_paths_are_public() {
        assert!(is_public("/api/v1/apps/renderer-1/heartbeat"));
        assert!(is_public("/healthz"));
        assert!(!is_public("/api/v1/apps/renderer-1/restart"));
        assert!(!is_public("/api/v1/config"));
    }

    #[test]
    fn constant_time_comparison_is_correct() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(constant_time_eq("", ""));
    }

    #[tokio::test]
    async fn stale_if_match_is_a_conflict() {
        let harness = harness(None);
        harness.state.store.update(|_| {}).unwrap();
        assert!(harness.state.check_precondition(Some("1")).is_ok());
        assert!(matches!(
            harness.state.check_precondition(Some("0")),
            Err(ApiError::Conflict(_))
        ));
        assert!(harness.state.check_precondition(None).is_ok());
    }

    #[tokio::test]
    async fn non_numeric_if_match_is_rejected() {
        let harness = harness(None);
        assert!(matches!(
            harness.state.check_precondition(Some("banana")),
            Err(ApiError::BadRequest(_))
        ));
    }
}
