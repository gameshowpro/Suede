//! Desired-state endpoints.
//!
//! Writes are validated synchronously and return once *persisted*. Applying
//! them is asynchronous, because reconciliation may take seconds or be
//! currently impossible; `?wait=<seconds>` opts into blocking until it settles.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use utoipa::IntoParams;

use super::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::model::{AppConfig, DesiredState, OutputConfig, OutputMatch, Settings};

/// Optional blocking behaviour for writes.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct WaitQuery {
    /// Block until reconciliation settles, or this many seconds elapse.
    pub wait: Option<u64>,
}

fn if_match(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
}

#[utoipa::path(
    get, path = "/api/v1/config", tag = "config",
    responses((status = 200, description = "The desired-state document", body = DesiredState))
)]
pub async fn get_config(State(state): State<ApiState>) -> Json<DesiredState> {
    Json(state.store.get())
}

#[utoipa::path(
    put, path = "/api/v1/config", tag = "config",
    params(WaitQuery), request_body = DesiredState,
    responses(
        (status = 200, description = "The persisted document", body = DesiredState),
        (status = 409, description = "If-Match revision is stale"),
        (status = 422, description = "Validation failed"),
    )
)]
pub async fn put_config(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(body): Json<DesiredState>,
) -> ApiResult<Json<DesiredState>> {
    state.check_precondition(if_match(&headers))?;
    state.commit(body, "all", query.wait).await.map(Json)
}

#[utoipa::path(
    get, path = "/api/v1/config/outputs", tag = "config",
    responses((status = 200, description = "Configured outputs", body = Vec<OutputConfig>))
)]
pub async fn get_outputs(State(state): State<ApiState>) -> Json<Vec<OutputConfig>> {
    Json(state.store.get().outputs)
}

#[utoipa::path(
    put, path = "/api/v1/config/outputs", tag = "config",
    params(WaitQuery), request_body = Vec<OutputConfig>,
    responses((status = 200, description = "The persisted document", body = DesiredState))
)]
pub async fn put_outputs(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(body): Json<Vec<OutputConfig>>,
) -> ApiResult<Json<DesiredState>> {
    state.check_precondition(if_match(&headers))?;
    let mut next = state.store.get();
    next.outputs = body;
    state.commit(next, "outputs", query.wait).await.map(Json)
}

#[utoipa::path(
    get, path = "/api/v1/config/outputs/{key}", tag = "config",
    params(("key" = String, Path, description = "Match key, e.g. HDMI-A-1")),
    responses(
        (status = 200, description = "The output configuration", body = OutputConfig),
        (status = 404, description = "No such entry"),
    )
)]
pub async fn get_output(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> ApiResult<Json<OutputConfig>> {
    state
        .store
        .get()
        .outputs
        .into_iter()
        .find(|output| output.r#match.key() == key)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no output configuration for {key}")))
}

#[utoipa::path(
    put, path = "/api/v1/config/outputs/{key}", tag = "config",
    params(("key" = String, Path, description = "Match key"), WaitQuery),
    request_body = OutputConfig,
    responses((status = 200, description = "The persisted document", body = DesiredState))
)]
pub async fn put_output(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(mut body): Json<OutputConfig>,
) -> ApiResult<Json<DesiredState>> {
    state.check_precondition(if_match(&headers))?;
    // The path is authoritative, so a body that disagrees cannot create a duplicate.
    if body.r#match.key() != key {
        body.r#match = OutputMatch::parse_key(&key);
    }

    let mut next = state.store.get();
    match next
        .outputs
        .iter_mut()
        .find(|output| output.r#match.key() == key)
    {
        Some(existing) => *existing = body,
        None => next.outputs.push(body),
    }
    state.commit(next, "outputs", query.wait).await.map(Json)
}

#[utoipa::path(
    delete, path = "/api/v1/config/outputs/{key}", tag = "config",
    params(("key" = String, Path, description = "Match key"), WaitQuery),
    responses(
        (status = 200, description = "The persisted document", body = DesiredState),
        (status = 404, description = "No such entry"),
    )
)]
pub async fn delete_output(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<DesiredState>> {
    state.check_precondition(if_match(&headers))?;
    let mut next = state.store.get();
    let before = next.outputs.len();
    next.outputs.retain(|output| output.r#match.key() != key);
    if next.outputs.len() == before {
        return Err(ApiError::NotFound(format!(
            "no output configuration for {key}"
        )));
    }
    state.commit(next, "outputs", query.wait).await.map(Json)
}

#[utoipa::path(
    get, path = "/api/v1/config/apps", tag = "config",
    responses((status = 200, description = "Configured apps", body = Vec<AppConfig>))
)]
pub async fn get_apps(State(state): State<ApiState>) -> Json<Vec<AppConfig>> {
    Json(state.store.get().apps)
}

#[utoipa::path(
    put, path = "/api/v1/config/apps", tag = "config",
    params(WaitQuery), request_body = Vec<AppConfig>,
    responses((status = 200, description = "The persisted document", body = DesiredState))
)]
pub async fn put_apps(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(body): Json<Vec<AppConfig>>,
) -> ApiResult<Json<DesiredState>> {
    state.check_precondition(if_match(&headers))?;
    let mut next = state.store.get();
    next.apps = body;
    state.commit(next, "apps", query.wait).await.map(Json)
}

#[utoipa::path(
    get, path = "/api/v1/config/apps/{id}", tag = "config",
    params(("id" = String, Path, description = "App identifier")),
    responses(
        (status = 200, description = "The app configuration", body = AppConfig),
        (status = 404, description = "No such app"),
    )
)]
pub async fn get_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<AppConfig>> {
    state
        .store
        .get()
        .apps
        .into_iter()
        .find(|app| app.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no app configuration for {id}")))
}

#[utoipa::path(
    put, path = "/api/v1/config/apps/{id}", tag = "config",
    params(("id" = String, Path, description = "App identifier"), WaitQuery),
    request_body = AppConfig,
    responses((status = 200, description = "The persisted document", body = DesiredState))
)]
pub async fn put_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(mut body): Json<AppConfig>,
) -> ApiResult<Json<DesiredState>> {
    state.check_precondition(if_match(&headers))?;
    body.id = id.clone();

    let mut next = state.store.get();
    match next.apps.iter_mut().find(|app| app.id == id) {
        Some(existing) => *existing = body,
        None => next.apps.push(body),
    }
    state.commit(next, "apps", query.wait).await.map(Json)
}

#[utoipa::path(
    delete, path = "/api/v1/config/apps/{id}", tag = "config",
    params(("id" = String, Path, description = "App identifier"), WaitQuery),
    responses(
        (status = 200, description = "The persisted document", body = DesiredState),
        (status = 404, description = "No such app"),
    )
)]
pub async fn delete_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<DesiredState>> {
    state.check_precondition(if_match(&headers))?;
    let mut next = state.store.get();
    let before = next.apps.len();
    next.apps.retain(|app| app.id != id);
    if next.apps.len() == before {
        return Err(ApiError::NotFound(format!("no app configuration for {id}")));
    }
    state.commit(next, "apps", query.wait).await.map(Json)
}

#[utoipa::path(
    get, path = "/api/v1/config/settings", tag = "config",
    responses((status = 200, description = "Daemon settings", body = Settings))
)]
pub async fn get_settings(State(state): State<ApiState>) -> Json<Settings> {
    Json(state.store.get().settings)
}

#[utoipa::path(
    put, path = "/api/v1/config/settings", tag = "config",
    params(WaitQuery), request_body = Settings,
    responses((status = 200, description = "The persisted document", body = DesiredState))
)]
pub async fn put_settings(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(body): Json<Settings>,
) -> ApiResult<Json<DesiredState>> {
    state.check_precondition(if_match(&headers))?;
    let mut next = state.store.get();
    next.settings = body;
    state.commit(next, "settings", query.wait).await.map(Json)
}

/// Shared by handlers that need to report an unexpected state error.
#[allow(dead_code)]
fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(error.to_string())
}

/// Re-exported for the OpenAPI document.
pub const OK: StatusCode = StatusCode::OK;

#[cfg(test)]
mod tests {
    use crate::api::test_support::{harness, Harness};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn call(
        harness: &Harness,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or(Body::empty()),
            )
            .unwrap();
        let response = harness.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    const OUTPUT: &str = r#"{"match":{"name":"HDMI-A-1"},"enable":true,
        "mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":0,"y":0}}"#;

    const APP: &str = r#"{"id":"renderer-1","enabled":true,
        "launcher":{"kind":"chromium-kiosk","uri":"http://example.com"}}"#;

    #[tokio::test]
    async fn empty_config_is_served_initially() {
        let harness = harness(None);
        let (status, body) = call(&harness, "GET", "/api/v1/config", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["revision"], 0);
        assert_eq!(body["outputs"].as_array().unwrap().len(), 0);
        assert_eq!(body["settings"]["hideCursor"], true);
    }

    #[tokio::test]
    async fn putting_the_whole_document_bumps_the_revision() {
        let harness = harness(None);
        let document = format!(r#"{{"outputs":[{OUTPUT}],"apps":[]}}"#);
        let (status, body) = call(&harness, "PUT", "/api/v1/config", Some(&document)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["revision"], 1);
        assert_eq!(body["outputs"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_write_survives_a_reload_of_the_store() {
        let harness = harness(None);
        call(
            &harness,
            "PUT",
            "/api/v1/config/apps",
            Some(&format!("[{APP}]")),
        )
        .await;
        let (_, body) = call(&harness, "GET", "/api/v1/config/apps", None).await;
        assert_eq!(body[0]["id"], "renderer-1");
    }

    #[tokio::test]
    async fn invalid_configuration_is_rejected_with_422() {
        let harness = harness(None);
        // Two apps sharing an id.
        let document = format!(r#"{{"apps":[{APP},{APP}]}}"#);
        let (status, body) = call(&harness, "PUT", "/api/v1/config", Some(&document)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["detail"].as_str().unwrap().contains("not unique"));
    }

    #[tokio::test]
    async fn malformed_json_is_rejected() {
        let harness = harness(None);
        let (status, _) = call(&harness, "PUT", "/api/v1/config", Some("{ not json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejected_writes_do_not_change_the_revision() {
        let harness = harness(None);
        let document = format!(r#"{{"apps":[{APP},{APP}]}}"#);
        call(&harness, "PUT", "/api/v1/config", Some(&document)).await;
        let (_, body) = call(&harness, "GET", "/api/v1/config", None).await;
        assert_eq!(body["revision"], 0);
    }

    #[tokio::test]
    async fn section_write_leaves_other_sections_alone() {
        let harness = harness(None);
        call(
            &harness,
            "PUT",
            "/api/v1/config/outputs",
            Some(&format!("[{OUTPUT}]")),
        )
        .await;
        call(
            &harness,
            "PUT",
            "/api/v1/config/apps",
            Some(&format!("[{APP}]")),
        )
        .await;

        let (_, body) = call(&harness, "GET", "/api/v1/config", None).await;
        assert_eq!(body["outputs"].as_array().unwrap().len(), 1);
        assert_eq!(body["apps"].as_array().unwrap().len(), 1);
        assert_eq!(body["revision"], 2);
    }

    #[tokio::test]
    async fn single_output_can_be_created_read_and_deleted() {
        let harness = harness(None);
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(OUTPUT),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(&harness, "GET", "/api/v1/config/outputs/HDMI-A-1", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["match"]["name"], "HDMI-A-1");

        let (status, _) = call(&harness, "DELETE", "/api/v1/config/outputs/HDMI-A-1", None).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = call(&harness, "GET", "/api/v1/config/outputs/HDMI-A-1", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn putting_the_same_output_twice_updates_rather_than_duplicates() {
        let harness = harness(None);
        call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(OUTPUT),
        )
        .await;
        call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(OUTPUT),
        )
        .await;
        let (_, body) = call(&harness, "GET", "/api/v1/config/outputs", None).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_path_wins_over_a_disagreeing_body() {
        let harness = harness(None);
        // Body says HDMI-A-1, path says HDMI-A-2.
        call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-2",
            Some(OUTPUT),
        )
        .await;
        let (_, body) = call(&harness, "GET", "/api/v1/config/outputs", None).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["match"]["name"], "HDMI-A-2");
    }

    #[tokio::test]
    async fn single_app_can_be_created_and_deleted() {
        let harness = harness(None);
        let (status, _) = call(&harness, "PUT", "/api/v1/config/apps/renderer-1", Some(APP)).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(&harness, "GET", "/api/v1/config/apps/renderer-1", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["launcher"]["kind"], "chromium-kiosk");

        let (status, _) = call(&harness, "DELETE", "/api/v1/config/apps/renderer-1", None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(&harness, "GET", "/api/v1/config/apps/renderer-1", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn deleting_an_absent_entry_is_404() {
        let harness = harness(None);
        let (status, _) = call(&harness, "DELETE", "/api/v1/config/apps/ghost", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(&harness, "DELETE", "/api/v1/config/outputs/ghost", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn settings_round_trip() {
        let harness = harness(None);
        let settings = r#"{"hideCursor":false,"outputPollIntervalSeconds":9,
            "allowRawSwayCommands":true}"#;
        let (status, _) = call(&harness, "PUT", "/api/v1/config/settings", Some(settings)).await;
        assert_eq!(status, StatusCode::OK);

        let (_, body) = call(&harness, "GET", "/api/v1/config/settings", None).await;
        assert_eq!(body["hideCursor"], false);
        assert_eq!(body["outputPollIntervalSeconds"], 9);
    }

    #[tokio::test]
    async fn zero_poll_interval_is_rejected() {
        let harness = harness(None);
        let settings = r#"{"hideCursor":true,"outputPollIntervalSeconds":0,
            "allowRawSwayCommands":false}"#;
        let (status, _) = call(&harness, "PUT", "/api/v1/config/settings", Some(settings)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn if_match_guards_concurrent_writes() {
        let harness = harness(None);
        call(
            &harness,
            "PUT",
            "/api/v1/config/apps",
            Some(&format!("[{APP}]")),
        )
        .await;

        let stale = Request::builder()
            .method("PUT")
            .uri("/api/v1/config/apps")
            .header("content-type", "application/json")
            .header("if-match", "0")
            .body(Body::from("[]"))
            .unwrap();
        let response = harness.router.clone().oneshot(stale).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let current = Request::builder()
            .method("PUT")
            .uri("/api/v1/config/apps")
            .header("content-type", "application/json")
            .header("if-match", "1")
            .body(Body::from("[]"))
            .unwrap();
        let response = harness.router.clone().oneshot(current).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wait_query_returns_after_reconciliation() {
        let harness = harness(None);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/outputs?wait=5",
            Some(&format!("[{OUTPUT}]")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["revision"], 1);
        // The pass has run, so the snapshot is populated.
        assert_eq!(harness.state.snapshot.outputs().len(), 3);
    }

    #[tokio::test]
    async fn writes_publish_a_config_changed_event() {
        let harness = harness(None);
        let mut receiver = harness.state.events.subscribe();
        call(&harness, "PUT", "/api/v1/config/apps", Some("[]")).await;

        let event = receiver.try_recv().unwrap();
        assert_eq!(event.name(), "config_changed");
        assert_eq!(event.data()["section"], "apps");
        assert_eq!(event.data()["revision"], 1);
    }
}
