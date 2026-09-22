//! Desired-state endpoints.
//!
//! Writes are validated synchronously and return once *persisted*. Applying
//! them is asynchronous, because reconciliation may take seconds or be
//! currently impossible; `?wait=<seconds>` opts into blocking until it settles.

use crate::api::json::Json;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use utoipa::IntoParams;

use super::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::model::{
    AppConfig, BackgroundPreset, DesiredState, OutputConfig, OutputMatch, ProjectionConfig,
    Settings,
};
use crate::state::{StatePrecondition, StateVersion};

const CONFIG_GENERATION_HEADER: &str = "x-config-generation";
const IF_CONFIG_GENERATION_HEADER: &str = "if-config-generation";
const CONFIG_EPOCH_HEADER: &str = "x-config-epoch";
const IF_CONFIG_EPOCH_HEADER: &str = "if-config-epoch";

/// A full-document response paired with the exact state identity that
/// produced it. `ETag` remains the persisted revision for established
/// `If-Match` clients; `X-Config-Generation` adds working-copy identity.
pub struct VersionedConfig {
    document: DesiredState,
    version: StateVersion,
}

impl VersionedConfig {
    fn new(document: DesiredState, version: StateVersion) -> Self {
        Self { document, version }
    }
}

impl IntoResponse for VersionedConfig {
    fn into_response(self) -> Response {
        let mut response = Json(self.document).into_response();
        attach_version(&mut response, &self.version);
        response
    }
}

/// One section of the document, paired with the same state identity.
///
/// Subresource GETs answer from the *effective* document, exactly like
/// `GET /api/v1/config`: a `GET /config/outputs` that described the saved
/// document while `GET /config` described the working copy would be two
/// answers to the same question. They carry the same three headers too, so a
/// client that only ever reads one section still has an `If-Match`/
/// `If-Config-Generation`/`If-Config-Epoch` to send if it wants a write
/// rejected under it (optional: see [`precondition`]).
pub struct VersionedSection<T> {
    body: T,
    version: StateVersion,
}

impl<T> VersionedSection<T> {
    fn new(body: T, version: StateVersion) -> Self {
        Self { body, version }
    }
}

impl<T: serde::Serialize> IntoResponse for VersionedSection<T> {
    fn into_response(self) -> Response {
        let mut response = Json(self.body).into_response();
        attach_version(&mut response, &self.version);
        response
    }
}

fn attach_version(response: &mut Response, version: &StateVersion) {
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", version.revision))
            .expect("u64 revision is a valid ETag"),
    );
    response.headers_mut().insert(
        CONFIG_GENERATION_HEADER,
        HeaderValue::from_str(&version.generation.to_string())
            .expect("u64 generation is a valid header value"),
    );
    response.headers_mut().insert(
        CONFIG_EPOCH_HEADER,
        HeaderValue::from_str(&version.epoch).expect("state epoch is a valid header value"),
    );
}

/// Optional blocking behavior for writes.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct WaitQuery {
    /// Block until reconciliation settles, or this many seconds elapse.
    pub wait: Option<u64>,
}

fn precondition(headers: &HeaderMap) -> ApiResult<StatePrecondition> {
    Ok(StatePrecondition {
        revision: parse_condition(headers, header::IF_MATCH.as_str(), "If-Match")?,
        generation: parse_condition(headers, IF_CONFIG_GENERATION_HEADER, "If-Config-Generation")?,
        epoch: parse_opaque_condition(headers, IF_CONFIG_EPOCH_HEADER, "If-Config-Epoch")?,
    })
}

fn parse_condition(headers: &HeaderMap, name: &str, label: &str) -> ApiResult<Option<u64>> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::BadRequest(format!("{label} must be a revision number")))?;
    value
        .trim()
        .trim_matches('"')
        .parse()
        .map(Some)
        .map_err(|_| ApiError::BadRequest(format!("{label} must be a revision number")))
}

fn parse_opaque_condition(
    headers: &HeaderMap,
    name: &str,
    label: &str,
) -> ApiResult<Option<String>> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::BadRequest(format!("{label} must be a valid header value")))?;
    if value.is_empty() {
        return Err(ApiError::BadRequest(format!("{label} must not be empty")));
    }
    Ok(Some(value.to_owned()))
}

#[utoipa::path(
    get, path = "/api/v1/config", tag = "config",
    responses((
        status = 200,
        description = "The current document. `committed: false` means a \
                       working copy is live on the outputs but not saved.",
        body = DesiredState,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
    ))
)]
pub async fn get_config(State(state): State<ApiState>) -> VersionedConfig {
    let (document, version) = state.store.effective_with_version();
    VersionedConfig::new(document, version)
}

#[utoipa::path(
    put, path = "/api/v1/config", tag = "config",
    params(
        WaitQuery,
        ("If-Match" = Option<String>, Header, description = "Optional persisted revision precondition from ETag"),
        ("If-Config-Generation" = Option<u64>, Header, description = "Optional working-copy generation precondition"),
        ("If-Config-Epoch" = Option<String>, Header, description = "Optional store-instance precondition from X-Config-Epoch"),
    ), request_body = DesiredState,
    responses(
        (status = 200, description = "The accepted document", body = DesiredState,
            headers(
                ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
                ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
                ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
            )
        ),
        (status = 409, description = "If-Match, If-Config-Generation, or If-Config-Epoch is stale"),
        (status = 422, description = "Validation failed"),
    )
)]
pub async fn put_config(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(body): Json<DesiredState>,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    if body.committed {
        return state
            .commit_if(expected, "all", query.wait, move |_| Ok(body))
            .await
            .map(|(document, version)| VersionedConfig::new(document, version));
    }
    // Not committed: everything except persistence. The document reaches the
    // outputs immediately; disk keeps the last saved state.
    state
        .preview_if(expected, body)
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    get, path = "/api/v1/config/outputs", tag = "config",
    responses((
        status = 200,
        description = "Configured outputs, from the effective document",
        body = Vec<OutputConfig>,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
    ))
)]
pub async fn get_outputs(State(state): State<ApiState>) -> VersionedSection<Vec<OutputConfig>> {
    let (document, version) = state.store.effective_with_version();
    VersionedSection::new(document.outputs, version)
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
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_if(expected, "outputs", query.wait, move |current| {
            let mut next = current.clone();
            next.outputs = body;
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    get, path = "/api/v1/config/outputs/{key}", tag = "config",
    params(("key" = String, Path, description = "Match key, e.g. HDMI-A-1")),
    responses(
        (status = 200, description = "The output configuration, from the effective document",
            body = OutputConfig,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
        ),
        (status = 404, description = "No such entry"),
    )
)]
pub async fn get_output(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> ApiResult<VersionedSection<OutputConfig>> {
    let (document, version) = state.store.effective_with_version();
    document
        .outputs
        .into_iter()
        .find(|output| output.r#match.key() == key)
        .map(|output| VersionedSection::new(output, version))
        .ok_or_else(|| ApiError::NotFound(format!("no output configuration for {key}")))
}

#[utoipa::path(
    put, path = "/api/v1/config/outputs/{key}", tag = "config",
    params(("key" = String, Path, description = "Match key"), WaitQuery),
    request_body = OutputConfig,
    responses((status = 200,
        description = "The persisted document. Retention note: while the \
                       effective projection mode is Simple, omitting \
                       `geometry` here (or leaving it out of a full PUT \
                       /config) keeps the previously saved geometry rather \
                       than clearing it, so a Simple-mode save cannot \
                       accidentally discard retained Warp calibration. PUT \
                       is otherwise literal. To actually clear one output's \
                       geometry, use DELETE on this same path with an \
                       additional /geometry segment.",
        body = DesiredState))
)]
pub async fn put_output(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(mut body): Json<OutputConfig>,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    // The path is authoritative, so a body that disagrees cannot create a duplicate.
    if body.r#match.key() != key {
        body.r#match = OutputMatch::parse_key(&key);
    }

    state
        .commit_if(expected, "outputs", query.wait, move |current| {
            let mut next = current.clone();
            match next
                .outputs
                .iter_mut()
                .find(|output| output.r#match.key() == key)
            {
                Some(existing) => *existing = body,
                None => next.outputs.push(body),
            }
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
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
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_if(expected, "outputs", query.wait, move |current| {
            let mut next = current.clone();
            let before = next.outputs.len();
            next.outputs.retain(|output| output.r#match.key() != key);
            if next.outputs.len() == before {
                return Err(ApiError::NotFound(format!(
                    "no output configuration for {key}"
                )));
            }
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

/// Explicit calibration clear. A full-document or section PUT cannot do
/// this while the effective mode is Simple: `preserve_retained` treats an
/// omitted `geometry` as "leave it", precisely so an ordinary Simple-mode
/// save cannot discard retained Warp correction by accident (see
/// [`crate::projection_policy::preserve_retained`]). This route bypasses
/// that retention deliberately and is the only way to clear one output's
/// geometry. Idempotent: a second call on an output that already has no
/// geometry still succeeds, since the end state (no geometry) is what was
/// asked for either way.
#[utoipa::path(
    delete, path = "/api/v1/config/outputs/{key}/geometry", tag = "config",
    params(("key" = String, Path, description = "Match key"), WaitQuery),
    responses(
        (status = 200, description = "The persisted document, with this output's geometry cleared", body = DesiredState),
        (status = 404, description = "No such output"),
    )
)]
pub async fn delete_output_geometry(
    State(state): State<ApiState>,
    Path(key): Path<String>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_literal_if(expected, "outputs", query.wait, move |current| {
            let mut next = current.clone();
            let output = next
                .outputs
                .iter_mut()
                .find(|output| output.r#match.key() == key)
                .ok_or_else(|| ApiError::NotFound(format!("no output configuration for {key}")))?;
            output.geometry = None;
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    get, path = "/api/v1/config/backgrounds", tag = "config",
    responses((
        status = 200,
        description = "Defined background presets, from the effective document",
        body = Vec<BackgroundPreset>,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
    ))
)]
pub async fn get_backgrounds(
    State(state): State<ApiState>,
) -> VersionedSection<Vec<BackgroundPreset>> {
    let (document, version) = state.store.effective_with_version();
    VersionedSection::new(document.backgrounds, version)
}

#[utoipa::path(
    put, path = "/api/v1/config/backgrounds", tag = "config",
    params(WaitQuery), request_body = Vec<BackgroundPreset>,
    responses(
        (status = 200, description = "The persisted document", body = DesiredState),
        (status = 422, description = "A preset is invalid, or an output refers to one that is gone"),
    )
)]
pub async fn put_backgrounds(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(body): Json<Vec<BackgroundPreset>>,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    // Validation rejects the write if this removed a preset an output still
    // refers to, so the two collections cannot drift out of agreement.
    state
        .commit_if(expected, "backgrounds", query.wait, move |current| {
            let mut next = current.clone();
            next.backgrounds = body;
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    put, path = "/api/v1/config/backgrounds/{id}", tag = "config",
    params(("id" = String, Path, description = "Preset id"), WaitQuery),
    request_body = BackgroundPreset,
    responses((status = 200, description = "The persisted document", body = DesiredState))
)]
pub async fn put_background(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(mut body): Json<BackgroundPreset>,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    // The path wins, so a mismatched body cannot silently create a second one.
    body.id = id;
    state
        .commit_if(expected, "backgrounds", query.wait, move |current| {
            let mut next = current.clone();
            match next.backgrounds.iter_mut().find(|p| p.id == body.id) {
                Some(existing) => *existing = body,
                None => next.backgrounds.push(body),
            }
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    delete, path = "/api/v1/config/backgrounds/{id}", tag = "config",
    params(("id" = String, Path, description = "Preset id"), WaitQuery),
    responses(
        (status = 200, description = "The persisted document", body = DesiredState),
        (status = 404, description = "No such preset"),
        (status = 409, description = "An output still uses this preset"),
    )
)]
pub async fn delete_background(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_if(expected, "backgrounds", query.wait, move |current| {
            let mut next = current.clone();
            // Refused rather than cascaded: deleting a preset would otherwise blank
            // every screen using it, which is a lot of damage for one click.
            let users: Vec<String> = next
                .outputs
                .iter()
                .filter(|output| {
                    output
                        .background
                        .as_ref()
                        .and_then(|background| background.preset_id())
                        == Some(id.as_str())
                })
                .map(|output| output.r#match.key())
                .collect();
            if !users.is_empty() {
                return Err(ApiError::Conflict(format!(
                    "background preset {id:?} is still used by {}",
                    users.join(", ")
                )));
            }
            let before = next.backgrounds.len();
            next.backgrounds.retain(|preset| preset.id != id);
            if next.backgrounds.len() == before {
                return Err(ApiError::NotFound(format!("no background preset {id}")));
            }
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    get, path = "/api/v1/config/apps", tag = "config",
    responses((
        status = 200,
        description = "Configured apps, from the effective document",
        body = Vec<AppConfig>,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
    ))
)]
pub async fn get_apps(State(state): State<ApiState>) -> VersionedSection<Vec<AppConfig>> {
    let (document, version) = state.store.effective_with_version();
    VersionedSection::new(document.apps, version)
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
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_if(expected, "apps", query.wait, move |current| {
            let mut next = current.clone();
            next.apps = body;
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    get, path = "/api/v1/config/apps/{id}", tag = "config",
    params(("id" = String, Path, description = "App identifier")),
    responses(
        (status = 200, description = "The app configuration, from the effective document",
            body = AppConfig,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
        ),
        (status = 404, description = "No such app"),
    )
)]
pub async fn get_app(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<VersionedSection<AppConfig>> {
    let (document, version) = state.store.effective_with_version();
    document
        .apps
        .into_iter()
        .find(|app| app.id == id)
        .map(|app| VersionedSection::new(app, version))
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
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    body.id = id.clone();
    state
        .commit_if(expected, "apps", query.wait, move |current| {
            let mut next = current.clone();
            match next.apps.iter_mut().find(|app| app.id == id) {
                Some(existing) => *existing = body,
                None => next.apps.push(body),
            }
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
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
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_if(expected, "apps", query.wait, move |current| {
            let mut next = current.clone();
            let before = next.apps.len();
            next.apps.retain(|app| app.id != id);
            if next.apps.len() == before {
                return Err(ApiError::NotFound(format!("no app configuration for {id}")));
            }
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    get, path = "/api/v1/config/settings", tag = "config",
    responses((
        status = 200,
        description = "Daemon settings, from the effective document",
        body = Settings,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
    ))
)]
pub async fn get_settings(State(state): State<ApiState>) -> VersionedSection<Settings> {
    let (document, version) = state.store.effective_with_version();
    VersionedSection::new(document.settings, version)
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
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_if(expected, "settings", query.wait, move |current| {
            let mut next = current.clone();
            next.settings = body;
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    get, path = "/api/v1/config/projection", tag = "config",
    responses((
        status = 200,
        description = "The projection configuration from the effective document; \
                       null when none is set",
        body = Option<ProjectionConfig>,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
    ))
)]
pub async fn get_projection(
    State(state): State<ApiState>,
) -> VersionedSection<Option<ProjectionConfig>> {
    let (document, version) = state.store.effective_with_version();
    VersionedSection::new(document.projection, version)
}

#[utoipa::path(
    put, path = "/api/v1/config/projection", tag = "config",
    params(WaitQuery), request_body = Option<ProjectionConfig>,
    responses(
        (status = 200,
        description = "The persisted document. `null` removes the whole \
                       projection section. Retention note: while the \
                       effective mode is Simple, an otherwise-present \
                       projection body that omits `canvas` keeps the \
                       previously saved canvas rather than clearing it, so \
                       a Simple-mode save cannot accidentally discard a \
                       retained Warp canvas. PUT is otherwise literal. To \
                       actually clear the shared canvas, use DELETE \
                       /config/projection/canvas.",
        body = DesiredState),
        (status = 422, description = "Validation failed"),
    )
)]
pub async fn put_projection(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    // `null` removes the section entirely — projection off, no trace left.
    Json(body): Json<Option<ProjectionConfig>>,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_if(expected, "projection", query.wait, move |current| {
            let mut next = current.clone();
            next.projection = body;
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

/// Explicit canvas clear, for the same reason `DELETE
/// /config/outputs/{key}/geometry` exists: `preserve_retained` deliberately
/// refills an omitted `canvas` while the effective mode is Simple, so
/// ordinary section and full-document PUTs cannot clear it. A document with
/// no `projection` section at all, or one whose canvas is already absent,
/// is left as it is — clearing an already-clear canvas is still success.
#[utoipa::path(
    delete, path = "/api/v1/config/projection/canvas", tag = "config",
    params(WaitQuery),
    responses((status = 200, description = "The persisted document, with the shared canvas cleared", body = DesiredState))
)]
pub async fn delete_projection_canvas(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    state
        .commit_literal_if(expected, "projection", query.wait, move |current| {
            let mut next = current.clone();
            if let Some(projection) = next.projection.as_mut() {
                projection.canvas = None;
            }
            Ok(next)
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}

#[utoipa::path(
    post, path = "/api/v1/config/revert", tag = "config",
    params(
        ("If-Match" = Option<String>, Header, description = "Optional persisted revision precondition from ETag"),
        ("If-Config-Generation" = Option<u64>, Header, description = "Optional working-copy generation precondition"),
        ("If-Config-Epoch" = Option<String>, Header, description = "Optional store-instance precondition from X-Config-Epoch"),
    ),
    responses(
        (status = 200,
        description = "Working copy discarded; the saved document is re-applied and returned",
        body = DesiredState,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        )),
        (status = 409, description = "If-Match, If-Config-Generation, or If-Config-Epoch is stale"),
    )
)]
pub async fn revert_config(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<VersionedConfig> {
    state
        .revert_if(precondition(&headers)?)
        .map(|(document, version)| VersionedConfig::new(document, version))
}

/// Re-exported for the OpenAPI document.
pub const OK: StatusCode = StatusCode::OK;

#[cfg(test)]
mod tests {
    use super::{CONFIG_EPOCH_HEADER, CONFIG_GENERATION_HEADER};
    use crate::api::test_support::{harness, Harness};
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode};
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

    async fn call_with_headers(
        harness: &Harness,
        method: &str,
        uri: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, serde_json::Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or(Body::empty()),
            )
            .unwrap();
        let response = harness.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let response_headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            response_headers,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    const OUTPUT: &str = r#"{"match":{"name":"HDMI-A-1"},"enable":true,
        "mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":0,"y":0}}"#;

    const APP: &str = r#"{"id":"renderer-1",
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
    async fn full_config_responses_identify_the_exact_working_copy() {
        let harness = harness(None);
        let (status, headers, body) =
            call_with_headers(&harness, "GET", "/api/v1/config", None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("etag").unwrap(), "\"0\"");
        assert_eq!(headers.get(CONFIG_GENERATION_HEADER).unwrap(), "0");
        let epoch = headers
            .get(CONFIG_EPOCH_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(!epoch.is_empty());
        assert_eq!(body["revision"], 0);

        let mut preview = harness.state.store.get();
        preview.committed = false;
        preview.settings.hide_cursor = false;
        let (status, headers, _) = call_with_headers(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&preview).unwrap()),
            &[
                ("if-match", "0"),
                ("if-config-generation", "0"),
                ("if-config-epoch", &epoch),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("etag").unwrap(), "\"0\"");
        assert_eq!(headers.get(CONFIG_GENERATION_HEADER).unwrap(), "1");
        assert_eq!(headers.get(CONFIG_EPOCH_HEADER).unwrap(), epoch.as_str());

        let (status, headers, body) = call_with_headers(
            &harness,
            "POST",
            "/api/v1/config/revert",
            None,
            &[
                ("if-match", "0"),
                ("if-config-generation", "1"),
                ("if-config-epoch", &epoch),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("etag").unwrap(), "\"0\"");
        assert_eq!(headers.get(CONFIG_GENERATION_HEADER).unwrap(), "2");
        assert_eq!(headers.get(CONFIG_EPOCH_HEADER).unwrap(), epoch.as_str());
        assert_eq!(body["committed"], true);
    }

    #[tokio::test]
    async fn competing_preview_save_and_revert_transitions_have_one_winner() {
        let harness = harness(None);
        let mut initial = harness.state.store.get();
        initial.committed = false;
        initial.settings.hide_cursor = false;
        let (status, _, _) = call_with_headers(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&initial).unwrap()),
            &[("if-match", "0"), ("if-config-generation", "0")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let mut replacement = harness.state.store.effective();
        replacement.committed = false;
        replacement.settings.hide_cursor = true;
        let replacement_json = serde_json::to_string(&replacement).unwrap();
        let preview = call_with_headers(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&replacement_json),
            &[("if-match", "0"), ("if-config-generation", "1")],
        );
        let revert = call_with_headers(
            &harness,
            "POST",
            "/api/v1/config/revert",
            None,
            &[("if-match", "0"), ("if-config-generation", "1")],
        );
        let (preview, revert) = tokio::join!(preview, revert);
        assert_eq!(
            [preview.0, revert.0]
                .into_iter()
                .filter(|status| *status == StatusCode::OK)
                .count(),
            1,
            "only one same-generation preview/revert transition can apply"
        );
        assert_eq!(
            [preview.0, revert.0]
                .into_iter()
                .filter(|status| *status == StatusCode::CONFLICT)
                .count(),
            1
        );

        let (_, headers, document) =
            call_with_headers(&harness, "GET", "/api/v1/config", None, &[]).await;
        let revision = headers.get("etag").unwrap().to_str().unwrap().to_owned();
        let generation = headers
            .get(CONFIG_GENERATION_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let mut saved = document.clone();
        saved["committed"] = serde_json::Value::Bool(true);
        let mut next_preview = document;
        next_preview["committed"] = serde_json::Value::Bool(false);
        next_preview["settings"]["hideCursor"] = serde_json::Value::Bool(false);
        let saved_json = saved.to_string();
        let next_preview_json = next_preview.to_string();
        let write_headers = [
            ("if-match", revision.as_str()),
            ("if-config-generation", generation.as_str()),
        ];
        let save = call_with_headers(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&saved_json),
            &write_headers,
        );
        let preview = call_with_headers(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&next_preview_json),
            &write_headers,
        );
        let (save, preview) = tokio::join!(save, preview);
        assert_eq!(
            [save.0, preview.0]
                .into_iter()
                .filter(|status| *status == StatusCode::OK)
                .count(),
            1,
            "only one same-generation preview/save transition can apply"
        );
        assert_eq!(
            [save.0, preview.0]
                .into_iter()
                .filter(|status| *status == StatusCode::CONFLICT)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn putting_the_whole_document_bumps_the_revision() {
        let harness = harness(None);
        let document = format!(r#"{{"outputs":[{OUTPUT}],"apps":[],"committed":true}}"#);
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
    async fn a_field_suede_does_not_know_is_refused_not_ignored() {
        let harness = harness(None);
        // The shape apps had before one active app covered the whole canvas.
        // Silently ignoring it is the worst outcome: the write succeeds and
        // the appliance runs nothing, with nothing to say why.
        let stale = r#"{"apps":[{"id":"renderer-1","enabled":true,"output":{"name":"HDMI-A-1"},
            "launcher":{"kind":"chromium-kiosk","uri":"http://example.com"}}]}"#;
        let (status, body) = call(&harness, "PUT", "/api/v1/config", Some(stale)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let detail = body.to_string();
        assert!(
            detail.contains("enabled"),
            "the message must name the field: {detail}"
        );
        assert!(harness.state.store.get().apps.is_empty());
    }

    #[tokio::test]
    async fn a_typo_in_a_settings_key_is_refused() {
        let harness = harness(None);
        // hideCursor mistyped. Dropping it silently would leave a pointer on
        // screen with the configuration insisting it had been hidden.
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(r#"{"settings":{"hideCursors":true}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
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

    const OUTPUT_WITH_GEOMETRY: &str = r#"{"match":{"name":"HDMI-A-1"},"enable":true,
        "mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":0,"y":0},
        "geometry":{"source":{"x":0,"y":0,"width":1,"height":1},
            "corners":[[0,0],[1,0],[1,1],[0,1]],
            "rasterFootprint":{"x":0,"y":0,"width":1,"height":1}}}"#;

    /// A11: `preserve_retained` refills an omitted `geometry` while the
    /// document stays Simple, so PUT alone can never clear retained
    /// calibration — this is the explicit route that can, and a PUT that
    /// follows it must not resurrect what it cleared.
    #[tokio::test]
    async fn deleting_output_geometry_really_clears_it_and_a_later_put_does_not_resurrect_it() {
        let harness = harness(None);
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(OUTPUT_WITH_GEOMETRY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(&harness, "GET", "/api/v1/config/outputs/HDMI-A-1", None).await;
        assert!(!body["geometry"].is_null(), "{body}");

        let (status, body) = call(
            &harness,
            "DELETE",
            "/api/v1/config/outputs/HDMI-A-1/geometry",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, body) = call(&harness, "GET", "/api/v1/config/outputs/HDMI-A-1", None).await;
        assert!(
            body["geometry"].is_null(),
            "DELETE must really clear it: {body}"
        );

        // A plain Simple-mode PUT that omits geometry entirely — the
        // ordinary shape a client sends when it never touched calibration —
        // must not bring the deleted geometry back from the basis document.
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(OUTPUT),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(&harness, "GET", "/api/v1/config/outputs/HDMI-A-1", None).await;
        assert!(
            body["geometry"].is_null(),
            "a later PUT without geometry must not resurrect cleared calibration: {body}"
        );

        // Deleting an output's geometry twice (or an output whose geometry
        // is already absent) is idempotent success, not a 404 — the DELETE
        // asks for an end state, not that something specific be removed.
        let (status, _) = call(
            &harness,
            "DELETE",
            "/api/v1/config/outputs/HDMI-A-1/geometry",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // The output itself not existing is still a 404, exactly like the
        // parent DELETE route.
        let (status, _) = call(
            &harness,
            "DELETE",
            "/api/v1/config/outputs/ghost/geometry",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A11: the same clearing/no-resurrection contract, for the shared
    /// canvas.
    #[tokio::test]
    async fn deleting_projection_canvas_really_clears_it_and_a_later_put_does_not_resurrect_it() {
        let mut harness = harness(None);
        // A shared canvas requires allow_overlaps and at least one enabled
        // output with a complete geometry; set both up first so the PUTs
        // below are validated on the canvas field alone. The router was
        // built against the original bootstrap, so it is rebuilt from the
        // mutated state rather than relying on the one `harness()` returned.
        std::sync::Arc::make_mut(&mut harness.state.bootstrap).allow_overlaps = true;
        harness.router = crate::api::router(harness.state.clone());
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(OUTPUT_WITH_GEOMETRY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection",
            // Aspect 1.0 to match OUTPUT_WITH_GEOMETRY's unit-square source.
            Some(r#"{"canvas":{"aspect":1.0,"renderWidth":1600}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, body) = call(&harness, "GET", "/api/v1/config/projection", None).await;
        assert!(!body["canvas"].is_null(), "{body}");

        let (status, body) =
            call(&harness, "DELETE", "/api/v1/config/projection/canvas", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, body) = call(&harness, "GET", "/api/v1/config/projection", None).await;
        assert!(
            body["canvas"].is_null(),
            "DELETE must really clear it: {body}"
        );

        // An ordinary Simple-mode projection edit that never mentions canvas
        // must not bring the deleted one back.
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection",
            Some(r#"{"gamma":2.4}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(&harness, "GET", "/api/v1/config/projection", None).await;
        assert!(
            body["canvas"].is_null(),
            "a later PUT without canvas must not resurrect a cleared one: {body}"
        );

        // No projection section at all, or one whose canvas is already
        // clear: still success, not an error.
        let (status, _) = call(&harness, "DELETE", "/api/v1/config/projection/canvas", None).await;
        assert_eq!(status, StatusCode::OK);
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
        let data = event.data();
        assert_eq!(data["section"], "apps");
        assert_eq!(data["revision"], 1);
        // A commit always ends with no working copy live, so it always
        // reports `committed: true`, mirroring `config.committed`.
        assert_eq!(data["committed"], true);
        assert_eq!(data["config"]["committed"], true);
        assert_eq!(data["config"]["revision"], 1);
        assert!(data["generation"].as_u64().is_some());
        assert!(!data["epoch"].as_str().unwrap().is_empty());
    }

    // --- background presets ----------------------------------------------

    const PRESET: &str = r##"{"id":"lobby","color":"#101820","mode":"fit"}"##;
    const USES_PRESET: &str = r#"{"match":{"name":"HDMI-A-1"},"background":"lobby"}"#;

    #[tokio::test]
    async fn a_preset_can_be_defined_and_then_named_by_an_output() {
        let harness = harness(None);
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/backgrounds/lobby",
            Some(PRESET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(USES_PRESET),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The shorthand survives the round trip rather than being expanded.
        assert_eq!(body["outputs"][0]["background"], "lobby");
        assert_eq!(body["backgrounds"][0]["color"], "#101820");
    }

    #[tokio::test]
    async fn naming_a_preset_that_does_not_exist_is_refused() {
        // Rejected at the write, where the author can still see their typo.
        let harness = harness(None);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(r#"{"match":{"name":"HDMI-A-1"},"background":"typo"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("not a defined background preset"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_preset_in_use_cannot_be_deleted() {
        // Deleting it would blank every screen using it, which is a lot of
        // damage for one click.
        let harness = harness(None);
        call(
            &harness,
            "PUT",
            "/api/v1/config/backgrounds/lobby",
            Some(PRESET),
        )
        .await;
        call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(USES_PRESET),
        )
        .await;

        let (status, body) =
            call(&harness, "DELETE", "/api/v1/config/backgrounds/lobby", None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["detail"].as_str().unwrap().contains("HDMI-A-1"),
            "the operator needs to know which display is holding it: {body}"
        );
    }

    #[tokio::test]
    async fn an_unused_preset_deletes_cleanly() {
        let harness = harness(None);
        call(
            &harness,
            "PUT",
            "/api/v1/config/backgrounds/spare",
            Some(r##"{"id":"spare","color":"#000000"}"##),
        )
        .await;
        let (status, body) =
            call(&harness, "DELETE", "/api/v1/config/backgrounds/spare", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["backgrounds"].as_array().unwrap().len(), 0);

        let (status, _) = call(&harness, "DELETE", "/api/v1/config/backgrounds/spare", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn replacing_the_whole_set_cannot_orphan_an_output() {
        let harness = harness(None);
        call(
            &harness,
            "PUT",
            "/api/v1/config/backgrounds/lobby",
            Some(PRESET),
        )
        .await;
        call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(USES_PRESET),
        )
        .await;

        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/backgrounds",
            Some(r##"[{"id":"other","color":"#000000"}]"##),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "dropping a preset an output still names must not be accepted"
        );
    }

    #[tokio::test]
    async fn an_output_may_still_carry_its_properties_directly() {
        // A script driving the API should not have to create a preset to
        // paint one screen.
        let harness = harness(None);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/outputs/HDMI-A-1",
            Some(
                r##"{"match":{"name":"HDMI-A-1"},
                     "background":{"color":"#223344","mode":"fill"}}"##,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["outputs"][0]["background"]["color"], "#223344");
    }

    // --- projection -------------------------------------------------------

    #[tokio::test]
    async fn projection_round_trips_and_null_clears_it() {
        let harness = harness(None);
        let (status, body) = call(&harness, "GET", "/api/v1/config/projection", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_null(), "no projection by default");

        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection",
            Some(r#"{"blend":true,"gamma":2.4,"blackLift":0.04}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["projection"]["gamma"], 2.4);
        assert_eq!(body["projection"]["blackLift"], 0.04);

        let (status, body) = call(&harness, "PUT", "/api/v1/config/projection", Some("null")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.get("projection").is_none_or(|p| p.is_null()),
            "null must remove the section: {body}"
        );
    }

    #[tokio::test]
    async fn an_empty_projection_section_gets_sensible_defaults() {
        // `{}` is the whole intended configuration for an installation of typical
        // projectors: blending on, gamma 2.2, no black lift.
        let harness = harness(None);
        let (status, body) = call(&harness, "PUT", "/api/v1/config/projection", Some("{}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["projection"]["blend"], true);
        assert_eq!(body["projection"]["gamma"], 2.2);
        assert_eq!(body["projection"]["blackLift"], 0.0);
    }

    /// The preconditions a client that has just read the state would send.
    fn conditions(harness: &Harness) -> Vec<(String, String)> {
        let (_, version) = harness.state.store.effective_with_version();
        vec![
            ("if-match".into(), version.revision.to_string()),
            (
                "if-config-generation".into(),
                version.generation.to_string(),
            ),
            ("if-config-epoch".into(), version.epoch),
        ]
    }

    async fn call_conditionally(
        harness: &Harness,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let owned = conditions(harness);
        let headers: Vec<(&str, &str)> = owned
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let (status, _, body) = call_with_headers(harness, method, uri, body, &headers).await;
        (status, body)
    }

    #[tokio::test]
    async fn an_uncommitted_write_reaches_the_outputs_but_never_the_disk() {
        let harness = harness(None);
        let mut working = harness.state.store.get();
        // Any settings bool would do here; `hideCursor` is used because,
        // unlike `allowRawSwayCommands`, it still serializes — the retired
        // field's `skip_serializing` would make this write silently omit it
        // and the test would stop proving anything.
        working.settings.hide_cursor = false;
        working.committed = false;

        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&working).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // Reads now serve the working copy, honestly flagged.
        assert_eq!(body["committed"], false);
        assert!(!harness.state.store.effective().settings.hide_cursor);
        // Disk keeps the saved state - a restart would return to it.
        assert!(harness.state.store.get().settings.hide_cursor);
        assert!(harness.state.store.get().committed);

        // Revert discards the working copy and returns the saved document.
        // Naming it is optional now, but this client happens to have just
        // read it, so it sends the precondition anyway.
        let (status, body) =
            call_conditionally(&harness, "POST", "/api/v1/config/revert", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["committed"], true);
        assert!(!harness.state.store.has_preview());
    }

    #[tokio::test]
    async fn committing_the_same_document_persists_it() {
        let harness = harness(None);
        let mut document = harness.state.store.get();
        document.settings.hide_cursor = false;
        document.committed = false;
        call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&document).unwrap()),
        )
        .await;

        // Save: the same document with the flag set, naming the working copy
        // it is committing.
        document.committed = true;
        let (status, body) = call_conditionally(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&document).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!harness.state.store.get().settings.hide_cursor);
        assert!(!harness.state.store.has_preview());
    }

    #[tokio::test]
    async fn an_invalid_working_copy_is_refused_like_any_write() {
        let harness = harness(None);
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(
                r#"{"committed":false,
                    "apps":[{"id":"x","launcher":{"kind":"exec","command":"true"}},
                            {"id":"x","launcher":{"kind":"exec","command":"true"}}]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!harness.state.store.has_preview());
    }

    /// Round 4 (September 21, evening): the server holds at most one shared
    /// working copy, and nothing stops one client from committing another's
    /// unsaved edit. Replaces
    /// `a_section_write_cannot_discard_a_working_copy_unless_it_names_it`,
    /// which asserted the withdrawn rule: a section PUT with no
    /// preconditions used to get a 409 while a working copy was live. Now it
    /// folds the live working copy in — one basis, the preview, exactly as
    /// a conditional write already did — and publishes the result once.
    #[tokio::test]
    async fn a_section_write_commits_another_operators_live_working_copy_and_publishes_once() {
        let harness = harness(None);

        // A is mid-edit: a working copy on the outputs, not saved.
        let mut previewed = harness.state.store.get();
        previewed.settings.hide_cursor = false;
        previewed.committed = false;
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&previewed).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // B section-PUTs without naming any version at all.
        let mut receiver = harness.state.events.subscribe();
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/settings",
            Some(r#"{"hideCursor":false,"outputPollIntervalSeconds":11}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["settings"]["outputPollIntervalSeconds"], 11);
        assert!(
            !body["settings"]["hideCursor"].as_bool().unwrap(),
            "A's unsaved change was committed along with B's, not silently dropped: {body}"
        );
        assert!(
            !harness.state.store.has_preview(),
            "the write folded the working copy into the commit rather than leaving it live"
        );
        assert!(harness.state.store.get().committed);

        let event = receiver.try_recv().unwrap();
        assert_eq!(event.name(), "config_changed");
        let data = event.data();
        assert_eq!(data["committed"], true);
        assert_eq!(data["config"]["settings"]["outputPollIntervalSeconds"], 11);
        assert!(!data["config"]["settings"]["hideCursor"].as_bool().unwrap());
        assert!(
            receiver.try_recv().is_err(),
            "the combined change is published exactly once, not once per contributor"
        );
    }

    /// Replaces `a_stale_revert_cannot_discard_a_newer_working_copy`'s sibling
    /// scenario: an unconditional revert — nobody named a version — used to
    /// get a 409 while a working copy was live. Now it discards whatever is
    /// live, whoever made it, and publishes the saved document. A stale
    /// *named* precondition still 409s; see `if_match_guards_concurrent_writes`
    /// and `stale_conditional_commit_cannot_clear_a_newer_preview` in
    /// `src/state.rs`.
    #[tokio::test]
    async fn an_unconditional_revert_discards_whatever_working_copy_is_live_and_publishes_it() {
        let harness = harness(None);
        let mut previewed = harness.state.store.get();
        previewed.settings.hide_cursor = false;
        previewed.committed = false;
        call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&previewed).unwrap()),
        )
        .await;
        assert!(harness.state.store.has_preview());

        let mut receiver = harness.state.events.subscribe();
        let (status, body) = call(&harness, "POST", "/api/v1/config/revert", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!harness.state.store.has_preview());
        assert!(
            harness.state.store.get().settings.hide_cursor,
            "the saved document is back, not the discarded preview"
        );

        let event = receiver.try_recv().unwrap();
        assert_eq!(event.name(), "config_changed");
        let data = event.data();
        assert_eq!(data["committed"], true);
        assert_eq!(
            data["config"]["settings"]["hideCursor"], true,
            "the saved document, not the discarded preview, is what was published"
        );
    }

    /// The other half of the same event contract: a preview PUT publishes
    /// `committed: false` with the working copy itself as `config`.
    #[tokio::test]
    async fn preview_publishes_committed_false_with_the_working_copy() {
        let harness = harness(None);
        let mut receiver = harness.state.events.subscribe();
        let mut working = harness.state.store.get();
        working.settings.hide_cursor = false;
        working.committed = false;
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&working).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let event = receiver.try_recv().unwrap();
        assert_eq!(event.name(), "config_changed");
        let data = event.data();
        assert_eq!(data["committed"], false);
        assert_eq!(data["section"], "all");
        assert_eq!(data["config"]["committed"], false);
        assert_eq!(data["config"]["settings"]["hideCursor"], false);
    }

    /// A10 acceptance 5: one answer to "what is the configuration", whichever
    /// route asks.
    #[tokio::test]
    async fn subresource_reads_answer_from_the_effective_document() {
        let harness = harness(None);
        let mut previewed = harness.state.store.get();
        previewed.settings.hide_cursor = false;
        previewed.committed = false;
        call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&previewed).unwrap()),
        )
        .await;

        let (status, headers, body) =
            call_with_headers(&harness, "GET", "/api/v1/config/settings", None, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["hideCursor"], false,
            "the working copy is what is on the outputs, so it is what a read describes"
        );
        // And the identity a write now has to quote comes back with it.
        let (_, version) = harness.state.store.effective_with_version();
        assert_eq!(
            headers[CONFIG_GENERATION_HEADER],
            version.generation.to_string()
        );
        assert_eq!(headers[CONFIG_EPOCH_HEADER], version.epoch);
        assert_eq!(headers[axum::http::header::ETAG], "\"0\"");
    }

    #[tokio::test]
    async fn projection_gamma_is_range_checked() {
        // 22 instead of 2.2 is the likely slip, and it would produce ramps so
        // wrong they look like a broken projector rather than a typo.
        let harness = harness(None);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection",
            Some(r#"{"gamma":22}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("between 1.0 and 4.0"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_test_pattern_round_trips_and_rejects_nonsense() {
        let harness = harness(None);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection",
            Some(r#"{"testPattern":"grid"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["projection"]["testPattern"], "grid");

        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection",
            Some(r#"{"testPattern":"plaid"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn projection_black_lift_is_range_checked() {
        let harness = harness(None);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection",
            Some(r#"{"blackLift":0.9}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("between 0.0 and 0.5"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn projection_black_lift_tagged_modes_roundtrip_and_validate() {
        let harness = harness(None);
        for value in [
            serde_json::json!(0.04),
            serde_json::json!({"mode":"fixed","level":0.04}),
            serde_json::json!({"mode":"adaptive","level":0.2,"darkThreshold":0.03,
                "brightThreshold":0.3,"riseMs":800.0,"fallMs":400.0,"slewPerSecond":0.2}),
        ] {
            let input = serde_json::json!({"blackLift":value}).to_string();
            let (status, body) =
                call(&harness, "PUT", "/api/v1/config/projection", Some(&input)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["projection"]["blackLift"], value);
        }
        for value in [
            serde_json::json!({"mode":"adaptive","level":0.6}),
            serde_json::json!({"mode":"adaptive","level":0.2,"darkThreshold":0.3,"brightThreshold":0.2}),
            serde_json::json!({"mode":"adaptive","level":0.2,"riseMs":0}),
            serde_json::json!({"mode":"adaptive","level":0.2,"slewPerSecond":0}),
        ] {
            let input = serde_json::json!({"blackLift":value}).to_string();
            let (status, _) =
                call(&harness, "PUT", "/api/v1/config/projection", Some(&input)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{value}");
        }
    }
}
