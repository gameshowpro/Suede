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
    AppConfig, Arrangement, ArrangementLimits, ArrangementRequest, BackgroundPreset, DesiredState,
    OutputConfig, OutputMatch, ProjectionConfig, Settings,
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

/// The persisted grid-arrangement record from the effective document, and
/// whether re-solving it still reproduces the current sources. See
/// [`crate::model::in_effect`]: the record is intent, not canonical geometry,
/// so a later manual geometry edit leaves it in place and turns `inEffect`
/// false rather than reverting or dropping it.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArrangementStatus {
    pub arrangement: Option<Arrangement>,
    pub in_effect: bool,
    /// Per-axis overlap limits for the recorded grid on the current canvas
    /// and outputs, each holding the other axis at its recorded overlap —
    /// what the dry run would report for an overlap request at the recorded
    /// values, with `allowUnusedCanvas` at its default `false` (the record
    /// does not keep it). Lets a client seed its overlap controls and their
    /// ranges in one read. Omitted when there is no record, or when the
    /// limits cannot be computed against the current document (no canvas,
    /// a missing raster, too many enabled outputs for the recorded grid, and
    /// so on); never a reason to fail this read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<ArrangementLimits>,
}

#[utoipa::path(
    get, path = "/api/v1/config/projection/arrangement", tag = "config",
    responses((
        status = 200,
        description = "The persisted grid-arrangement record from the effective \
                       document, whether re-solving it still reproduces the \
                       current sources, and (when computable) the overlap \
                       limits at the recorded values",
        body = ArrangementStatus,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        ),
    ))
)]
pub async fn get_projection_arrangement(
    State(state): State<ApiState>,
) -> VersionedSection<ArrangementStatus> {
    let (document, version) = state.store.effective_with_version();
    let raster = super::raster_lookup(&state.snapshot);
    let in_effect = crate::model::in_effect(&document, &raster);
    let arrangement = document
        .projection
        .as_ref()
        .and_then(|projection| projection.arrangement);
    let limits = arrangement.and_then(|recorded| {
        let request = ArrangementRequest {
            rows: recorded.rows,
            columns: recorded.columns,
            overlap_x: Some(recorded.overlap_x),
            overlap_y: Some(recorded.overlap_y),
            content_scale: None,
            allow_unused_canvas: false,
            committed: false,
        };
        crate::model::limits_document(&document, &request, &raster).ok()
    });
    VersionedSection::new(
        ArrangementStatus {
            arrangement,
            in_effect,
            limits,
        },
        version,
    )
}

/// Solve a grid arrangement and write it — each enabled output's
/// `geometry.source`, and the resolved five values at
/// `projection.arrangement` — exactly like any other config write:
/// `committed: false` (the default) replaces the shared working copy and
/// applies to the outputs; `committed: true` persists it. See
/// [`crate::model::arrangement`] for the solver itself and
/// `docs/configuration.md`'s "Grid arrangement" section for the contract.
#[utoipa::path(
    put, path = "/api/v1/config/projection/arrangement", tag = "config",
    params(
        WaitQuery,
        ("If-Match" = Option<String>, Header, description = "Optional persisted revision precondition from ETag"),
        ("If-Config-Generation" = Option<u64>, Header, description = "Optional working-copy generation precondition"),
        ("If-Config-Epoch" = Option<String>, Header, description = "Optional store-instance precondition from X-Config-Epoch"),
    ), request_body = ArrangementRequest,
    responses(
        (status = 200,
        description = "The accepted document; the resolved five values are at \
                       `projection.arrangement` and every enabled output's \
                       `geometry.source` reflects the solve",
        body = DesiredState,
        headers(
            ("ETag" = String, description = "Persisted document revision, quoted for If-Match"),
            ("X-Config-Generation" = u64, description = "Effective working-copy generation"),
            ("X-Config-Epoch" = String, description = "Store instance identity for If-Config-Epoch"),
        )),
        (status = 409, description = "If-Match, If-Config-Generation, or If-Config-Epoch is stale"),
        (status = 422, description = "The grid does not fit the outputs, an output \
                       has no known raster, the request is over- or \
                       under-determined, an overlap is outside [0, 1), an edge \
                       slice would fall entirely outside the canvas, or the \
                       result fails document validation"),
    )
)]
pub async fn put_projection_arrangement(
    State(state): State<ApiState>,
    Query(query): Query<WaitQuery>,
    headers: HeaderMap,
    Json(body): Json<ArrangementRequest>,
) -> ApiResult<VersionedConfig> {
    let expected = precondition(&headers)?;
    let raster = super::raster_lookup(&state.snapshot);

    if body.committed {
        state
            .commit_if(expected, "projection", query.wait, move |current| {
                let solution = crate::model::solve_document(current, &body, raster)
                    .map_err(ApiError::Validation)?;
                let mut next = current.clone();
                crate::model::apply(&mut next, &solution);
                Ok(next)
            })
            .await
            .map(|(document, version)| VersionedConfig::new(document, version))
    } else {
        // The working copy is shared and owned by nobody: an uncommitted
        // write always builds on the effective document — the live working
        // copy if one exists, else the saved one — exactly like any other
        // preview, so another client's unrelated edit survives underneath
        // this one. The solve runs inside `preview_with_if`'s closure, under
        // the store's write lock, so a concurrent preview from another
        // client cannot land between reading the basis and staging this one.
        state
            .preview_with_if(expected, move |basis| {
                let solution = crate::model::solve_document(basis, &body, raster)
                    .map_err(ApiError::Validation)?;
                let mut next = basis.clone();
                crate::model::apply(&mut next, &solution);
                Ok(next)
            })
            .map(|(document, version)| VersionedConfig::new(document, version))
    }
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
    use crate::model::{
        CanvasConfig, DesiredState, Mode, OutputConfig, OutputMatch, ProjectionConfig,
    };
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

    /// A preview (uncommitted) working copy carries `highlightOverlaps`
    /// exactly like any other field — it is not committed here, so slice 1's
    /// structural reset (which only fires from `commit_if`/`commit_literal_if`)
    /// must not touch it.
    #[tokio::test]
    async fn highlight_overlaps_previews_uncommitted() {
        let harness = harness(None);
        let mut previewed = harness.state.store.get();
        previewed.committed = false;
        previewed.projection = Some(ProjectionConfig::default());
        previewed
            .projection
            .as_mut()
            .unwrap()
            .temporary
            .highlight_overlaps = true;
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&previewed).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["committed"], false);
        assert_eq!(body["projection"]["temporary"]["highlightOverlaps"], true);

        // Visible from a plain read of the effective (working-copy) document too.
        let (status, body) = call(&harness, "GET", "/api/v1/config", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["projection"]["temporary"]["highlightOverlaps"], true);
    }

    /// The structural half of the "temporary settings" guarantee: a document
    /// committed with `highlightOverlaps: true` comes back — and is actually
    /// written to disk — as `false`, regardless of what was asked for. Reads
    /// back the persisted file through a fresh `StateStore` rather than
    /// trusting only the in-memory return value, so a bug that reset the
    /// return value but not what `flush()` wrote would still be caught.
    #[tokio::test]
    async fn highlight_overlaps_is_reset_to_false_on_commit_and_never_persisted() {
        let harness = harness(None);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection",
            Some(r#"{"temporary":{"highlightOverlaps":true}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["projection"]["temporary"]["highlightOverlaps"], false,
            "commit_if resets projection.temporary before the document is even returned"
        );

        let reloaded = crate::state::StateStore::load(harness._dir.path().to_path_buf()).unwrap();
        assert!(
            !reloaded
                .get()
                .projection
                .unwrap()
                .temporary
                .highlight_overlaps,
            "what actually reached disk must also be false, not just the response"
        );
    }

    /// Slice 7: "unsaved edits" is server-driven off
    /// `StateStore::has_unsaved_edits`, which ignores
    /// `projection.temporary`. A preview that flips `highlightOverlaps` and
    /// changes nothing else reads back `committed: true` — from the PUT's
    /// own response and from `/api/v1/status` — even though the preview is
    /// still stored, because the renderer reads it.
    #[tokio::test]
    async fn a_preview_that_only_toggles_highlight_overlaps_is_not_unsaved() {
        let harness = harness(None);
        // A projection section already on the saved document, so the
        // preview below can differ from `current` in nothing but
        // `temporary`.
        harness
            .state
            .store
            .update(|state| state.projection = Some(ProjectionConfig::default()))
            .unwrap();

        let mut previewed = harness.state.store.get();
        previewed.committed = false;
        previewed
            .projection
            .as_mut()
            .unwrap()
            .temporary
            .highlight_overlaps = true;
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&previewed).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["committed"], true,
            "only an ephemeral toggle changed; nothing is unsaved: {body}"
        );
        assert!(
            harness.state.store.has_preview(),
            "the preview is still stored — the renderer reads it"
        );

        let (status, status_body) = call(&harness, "GET", "/api/v1/status", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(status_body["committed"], true, "{status_body}");
    }

    /// A real edit alongside the ephemeral toggle is still unsaved: the
    /// helper compares the whole document, not just `temporary`.
    #[tokio::test]
    async fn a_real_edit_alongside_highlight_overlaps_is_still_unsaved() {
        let harness = harness(None);
        harness
            .state
            .store
            .update(|state| state.projection = Some(ProjectionConfig::default()))
            .unwrap();

        let mut previewed = harness.state.store.get();
        previewed.committed = false;
        previewed
            .projection
            .as_mut()
            .unwrap()
            .temporary
            .highlight_overlaps = true;
        previewed.settings.hide_cursor = !previewed.settings.hide_cursor;
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&previewed).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["committed"], false,
            "a real edit is present alongside the toggle: {body}"
        );

        let (status, status_body) = call(&harness, "GET", "/api/v1/status", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(status_body["committed"], false, "{status_body}");
    }

    /// Undoing the real edit while leaving `highlightOverlaps` on reads back
    /// `committed: true` again — the preview is re-evaluated against
    /// `current` on every `set_preview_if`, not stuck at whatever the first
    /// preview decided.
    #[tokio::test]
    async fn removing_the_real_edit_while_keeping_highlight_overlaps_on_reads_back_committed() {
        let harness = harness(None);
        harness
            .state
            .store
            .update(|state| state.projection = Some(ProjectionConfig::default()))
            .unwrap();

        let mut previewed = harness.state.store.get();
        previewed.committed = false;
        previewed
            .projection
            .as_mut()
            .unwrap()
            .temporary
            .highlight_overlaps = true;
        previewed.settings.hide_cursor = !previewed.settings.hide_cursor;
        call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&previewed).unwrap()),
        )
        .await;
        assert!(!harness.state.store.effective().committed);

        let mut reverted = harness.state.store.effective();
        reverted.committed = false;
        reverted.settings.hide_cursor = !reverted.settings.hide_cursor;
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&reverted).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["committed"], true,
            "the real edit is gone; only the ephemeral toggle remains: {body}"
        );
        assert_eq!(body["projection"]["temporary"]["highlightOverlaps"], true);
    }

    /// `POST /config/revert` still discards the whole preview, ephemeral
    /// toggle included, even though the toggle alone no longer makes the
    /// preview read as unsaved.
    #[tokio::test]
    async fn reverting_a_working_copy_still_clears_highlight_overlaps() {
        let harness = harness(None);
        harness
            .state
            .store
            .update(|state| state.projection = Some(ProjectionConfig::default()))
            .unwrap();

        let mut previewed = harness.state.store.get();
        previewed.committed = false;
        previewed
            .projection
            .as_mut()
            .unwrap()
            .temporary
            .highlight_overlaps = true;
        call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&previewed).unwrap()),
        )
        .await;
        assert!(
            harness
                .state
                .store
                .effective()
                .projection
                .unwrap()
                .temporary
                .highlight_overlaps,
            "the preview carries the toggle before revert"
        );

        let (status, body) = call(&harness, "POST", "/api/v1/config/revert", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!harness.state.store.has_preview());
        assert!(
            !harness
                .state
                .store
                .get()
                .projection
                .unwrap()
                .temporary
                .highlight_overlaps,
            "revert discards the whole preview, including the ephemeral toggle"
        );
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

    // --- grid arrangement --------------------------------------------------

    /// `count` enabled outputs at 1920x1080, plus a 3840-wide 16:9 shared
    /// canvas — no geometry yet, which is exactly the state the arrangement
    /// endpoint is for. This document would fail `DesiredState::validate`
    /// as it stands (a shared canvas requires every enabled output to
    /// already have geometry), but that is fine: it is written straight into
    /// the store with `StateStore::update`, which does not validate, and the
    /// arrangement PUT under test is what is expected to make it valid by
    /// seeding geometry for every output it places.
    fn arrangement_document(count: usize) -> DesiredState {
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
        let mut document = DesiredState::new();
        document.outputs = outputs;
        document.projection = Some(ProjectionConfig {
            canvas: Some(CanvasConfig {
                aspect: 16.0 / 9.0,
                render_width: 3840,
            }),
            ..ProjectionConfig::default()
        });
        document
    }

    /// A shared canvas requires `allow_overlaps = true` (see
    /// `validate_projection`), so every arrangement test needs it, and the
    /// router has to be rebuilt from the mutated bootstrap for the change to
    /// take effect, exactly as
    /// `deleting_projection_canvas_really_clears_it_and_a_later_put_does_not_resurrect_it`
    /// does.
    fn arrangement_harness(count: usize) -> Harness {
        let mut harness = harness(None);
        std::sync::Arc::make_mut(&mut harness.state.bootstrap).allow_overlaps = true;
        harness.router = crate::api::router(harness.state.clone());
        harness
            .state
            .store
            .update(|document| *document = arrangement_document(count))
            .unwrap();
        harness
    }

    const ARRANGE_2X2_20: &str = r#"{"rows":2,"columns":2,"overlapX":0.2,"overlapY":0.2}"#;

    #[tokio::test]
    async fn uncommitted_arrangement_reaches_the_outputs_but_not_the_disk() {
        let harness = arrangement_harness(4);
        let before = harness.state.store.get();
        let mut receiver = harness.state.events.subscribe();

        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some(ARRANGE_2X2_20),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["committed"], false);
        assert_eq!(body["projection"]["arrangement"]["rows"], 2);
        assert_eq!(body["projection"]["arrangement"]["columns"], 2);
        assert!(
            (body["projection"]["arrangement"]["contentScale"]
                .as_f64()
                .unwrap()
                - 0.9)
                .abs()
                < 1e-9,
            "{body}"
        );

        // Every enabled output's source matches the dry run's.
        let (dry_status, dry_body) = call(
            &harness,
            "GET",
            "/api/v1/projection/arrangement?rows=2&columns=2&overlapX=0.2&overlapY=0.2",
            None,
        )
        .await;
        assert_eq!(dry_status, StatusCode::OK, "{dry_body}");
        let dry_outputs = dry_body["outputs"].as_array().unwrap();
        assert_eq!(dry_outputs.len(), 4);
        for (index, arranged) in dry_outputs.iter().enumerate() {
            assert_eq!(
                body["outputs"][index]["geometry"]["source"], arranged["source"],
                "output {index}: {body}"
            );
        }

        // Disk is untouched by a preview.
        assert_eq!(
            harness.state.store.get(),
            before,
            "a preview must not reach disk"
        );

        // `config_changed` carries the working copy, honestly flagged.
        let event = receiver.try_recv().unwrap();
        assert_eq!(event.name(), "config_changed");
        let data = event.data();
        assert_eq!(data["committed"], false);
        assert_eq!(data["config"], body);
    }

    #[tokio::test]
    async fn uncommitted_arrangement_builds_on_another_clients_live_working_copy() {
        let harness = arrangement_harness(4);

        // A is mid-edit: an unrelated setting, previewed but not saved. Set
        // directly on the store (as `dry_run_does_not_change_generation...`
        // in `api::projection::tests` also does) rather than through the
        // ordinary `PUT /config`, which would itself be refused here: the
        // fixture's canvas requires every enabled output to already have
        // geometry, and seeding that would only obscure what this test is
        // about.
        let mut previewed = harness.state.store.get();
        previewed.settings.hide_cursor = false;
        previewed.committed = false;
        harness.state.store.set_preview(Some(previewed));
        assert!(harness.state.store.has_preview());

        // B arranges without naming any version.
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some(ARRANGE_2X2_20),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["projection"]["arrangement"]["rows"], 2);
        assert!(
            !body["settings"]["hideCursor"].as_bool().unwrap(),
            "A's unrelated edit survives underneath B's arrangement: {body}"
        );
    }

    #[tokio::test]
    async fn committed_arrangement_persists_and_clears_the_working_copy() {
        let harness = arrangement_harness(4);
        let before_revision = harness.state.store.get().revision;
        let mut receiver = harness.state.events.subscribe();

        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some(r#"{"rows":2,"columns":2,"overlapX":0.2,"overlapY":0.2,"committed":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["committed"], true);
        assert_eq!(harness.state.store.get().revision, before_revision + 1);
        assert!(harness.state.store.get().committed);
        assert!(!harness.state.store.has_preview());

        let event = receiver.try_recv().unwrap();
        assert_eq!(event.name(), "config_changed");
        let data = event.data();
        assert_eq!(data["committed"], true);
        assert_eq!(data["section"], "projection");
    }

    #[tokio::test]
    async fn a_stale_generation_is_a_conflict() {
        let harness = arrangement_harness(4);
        let (status, _, body) = call_with_headers(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some(ARRANGE_2X2_20),
            &[("if-config-generation", "999")],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    #[tokio::test]
    async fn a_missing_canvas_is_a_422_with_the_solvers_message() {
        // No canvas at all: `allow_overlaps` never even comes into it.
        let harness = harness(None);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some(ARRANGE_2X2_20),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(
            body["detail"], "projection.canvas is required to arrange outputs",
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_grid_too_small_for_the_outputs_is_a_422_with_the_solvers_message() {
        let harness = arrangement_harness(5);
        let (status, body) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some(r#"{"rows":2,"columns":2}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("do not fit a 2x2 grid"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn malformed_arrangement_json_is_rejected() {
        let harness = arrangement_harness(4);
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some("{ not json"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn arrangement_status_reports_in_effect_until_a_source_is_moved_by_hand() {
        let harness = arrangement_harness(4);
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some(ARRANGE_2X2_20),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            &harness,
            "GET",
            "/api/v1/config/projection/arrangement",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["inEffect"], true, "{body}");
        assert_eq!(body["arrangement"]["rows"], 2);

        // Move one source by hand, well beyond the solver's own 1e-9
        // tolerance but too small to change the layout's topology.
        let mut moved = harness.state.store.effective();
        moved.outputs[0].geometry.as_mut().unwrap().source.x += 1.0e-6;
        moved.committed = false;
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config",
            Some(&serde_json::to_string(&moved).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (_, body) = call(
            &harness,
            "GET",
            "/api/v1/config/projection/arrangement",
            None,
        )
        .await;
        assert_eq!(body["inEffect"], false, "{body}");
        assert_eq!(
            body["arrangement"]["rows"], 2,
            "the record survives the manual edit: {body}"
        );
    }

    /// With a record, the status read carries the overlap limits at the
    /// recorded values, exactly as a dry run at those values would.
    ///
    /// 2x2 of 1920x1080 at (0.2, 0.2) on 16:9: two slices per axis put the
    /// seam on the canvas's center line at every overlap, so no slice can
    /// leave the canvas and each axis's range is the whole of `[0, 1)` — `0`
    /// exactly, up to the bisected bound just below `1`.
    #[tokio::test]
    async fn arrangement_status_carries_limits_at_the_recorded_overlaps() {
        let harness = arrangement_harness(4);
        let (status, _) = call(
            &harness,
            "PUT",
            "/api/v1/config/projection/arrangement",
            Some(ARRANGE_2X2_20),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            &harness,
            "GET",
            "/api/v1/config/projection/arrangement",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        for axis in ["overlapX", "overlapY"] {
            let limits = &body["limits"][axis];
            assert_eq!(limits["hasSeam"], true, "{axis}: {body}");
            assert_eq!(limits["min"].as_f64().unwrap(), 0.0, "{axis}: {body}");
            let max = limits["max"].as_f64().unwrap();
            assert!(max > 0.999 && max < 1.0, "{axis}: {body}");
        }

        let (dry_status, dry_body) = call(
            &harness,
            "GET",
            "/api/v1/projection/arrangement?rows=2&columns=2&overlapX=0.2&overlapY=0.2",
            None,
        )
        .await;
        assert_eq!(dry_status, StatusCode::OK, "{dry_body}");
        assert_eq!(body["limits"], dry_body["limits"], "{body} vs {dry_body}");
    }

    /// No record, no limits: the field is omitted rather than null, and the
    /// read still succeeds.
    #[tokio::test]
    async fn arrangement_status_omits_limits_without_a_record() {
        let harness = arrangement_harness(4);
        let (status, body) = call(
            &harness,
            "GET",
            "/api/v1/config/projection/arrangement",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["arrangement"].is_null(), "{body}");
        assert_eq!(body["inEffect"], false, "{body}");
        assert!(body.get("limits").is_none(), "{body}");
    }

    /// A record the current document can no longer honor — here a 1x1 grid
    /// with four enabled outputs — still reads back, `inEffect` false; its
    /// limits cannot be computed, so they are omitted rather than failing
    /// the read.
    #[tokio::test]
    async fn arrangement_status_omits_limits_it_cannot_compute() {
        let harness = arrangement_harness(4);
        harness
            .state
            .store
            .update(|document| {
                document.projection.as_mut().unwrap().arrangement = Some(super::Arrangement {
                    rows: 1,
                    columns: 1,
                    overlap_x: 0.2,
                    overlap_y: 0.2,
                    content_scale: 1.0,
                });
            })
            .unwrap();

        let (status, body) = call(
            &harness,
            "GET",
            "/api/v1/config/projection/arrangement",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["arrangement"]["rows"], 1, "{body}");
        assert_eq!(body["inEffect"], false, "{body}");
        assert!(body.get("limits").is_none(), "{body}");
    }

    /// The smoothness claim in the plan's "Rapid change" section: a burst of
    /// uncommitted arrangement writes is pure in-memory arithmetic under the
    /// store lock, wakes the projection `Notify` (not the debounced ordinary
    /// queue), and never touches disk. Fifty PUTs land in a tight loop —
    /// nothing here awaits a debounce or a reconciler pass, so if this test
    /// were slow it would mean a write had started blocking on something it
    /// should not.
    #[tokio::test]
    async fn fifty_uncommitted_arrangement_puts_coalesce_onto_the_projection_trigger() {
        let mut harness = arrangement_harness(4);
        let saved_before = harness.state.store.get();
        let projection = harness.requests.projection_handle();
        // Nothing queued yet: the fixture seeds the store directly, not
        // through the API.
        assert!(harness.requests.try_recv().is_err());

        let mut last_overlap_x = 0.0_f64;
        for step in 0..50 {
            let overlap_x = 0.02 + (0.5 - 0.02) * (step as f64) / 49.0;
            last_overlap_x = overlap_x;
            let body = serde_json::json!({
                "rows": 2,
                "columns": 2,
                "overlapX": overlap_x,
                "overlapY": 0.1,
            })
            .to_string();
            let (status, response_body) = call(
                &harness,
                "PUT",
                "/api/v1/config/projection/arrangement",
                Some(&body),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "step {step}: {response_body}");
        }

        // The effective document reflects the last request sent, not some
        // earlier one that a race let land out of order.
        let effective = harness.state.store.effective();
        let arrangement = effective
            .projection
            .as_ref()
            .and_then(|projection| projection.arrangement)
            .expect("an arrangement was applied");
        assert!(
            (arrangement.overlap_x - last_overlap_x).abs() < 1e-9,
            "expected overlapX {last_overlap_x}, got {}",
            arrangement.overlap_x
        );

        // Every PUT was a preview: the saved document on disk never moved.
        assert_eq!(
            harness.state.store.get(),
            saved_before,
            "an uncommitted burst must not reach disk"
        );

        // The burst woke the projection fast path...
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), projection.notified())
                .await
                .is_ok(),
            "a burst of uncommitted arrangement writes must signal the projection trigger"
        );
        // ...and never the debounced ordinary queue: every one of the fifty
        // writes changed only the projection, so each took the
        // `request_projection` branch, not `request(\"working copy\")`.
        assert!(
            harness.requests.try_recv().is_err(),
            "an arrangement-only burst must not enqueue an ordinary debounced request"
        );
    }
}
