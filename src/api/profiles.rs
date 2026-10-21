//! Named configuration snapshots, independent of the live working copy.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use utoipa::ToSchema;

use super::config_routes::{precondition, VersionedConfig};
use super::json::Json;
use super::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::model::OutputMatch;
use crate::profiles::{
    ProfileError, ProfileIncludes, ProfileSummary, SystemProfile, PROFILE_FORMAT_VERSION,
};

impl From<ProfileError> for ApiError {
    fn from(error: ProfileError) -> Self {
        match error {
            ProfileError::NotFound(_) => Self::NotFound(error.to_string()),
            ProfileError::AlreadyExists(_) | ProfileError::NameAlreadyExists(_) => {
                Self::Conflict(error.to_string())
            }
            ProfileError::Io(_) | ProfileError::Json(_) => Self::Internal(error.to_string()),
            _ => Self::Validation(error.to_string()),
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptureProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub includes: ProfileIncludes,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub includes: ProfileIncludes,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportProfile {
    pub id: String,
    pub profile: SystemProfile,
}

#[derive(Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ActivateProfile {
    /// Saved output match key to the destination connector name.
    pub output_mappings: BTreeMap<String, String>,
    /// Saved PipeWire sink name to the destination sink name.
    pub audio_mappings: BTreeMap<String, String>,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
}

#[utoipa::path(get, path = "/api/v1/profiles", tag = "profiles",
    responses((status = 200, description = "Saved system profiles", body = Vec<ProfileSummary>)))]
pub async fn list(State(state): State<ApiState>) -> ApiResult<Json<Vec<ProfileSummary>>> {
    blocking(move || Ok(Json(state.profiles.list()?))).await
}

fn capture(state: &ApiState, request: UpdateProfile) -> ApiResult<SystemProfile> {
    let mut profile = SystemProfile {
        format_version: PROFILE_FORMAT_VERSION,
        name: request.name,
        description: request.description,
        includes: request.includes,
        config: state.store.effective(),
        assets: BTreeMap::new(),
    };
    profile.normalize();
    for id in profile.referenced_wallpapers() {
        let (bytes, _) = state.wallpapers.read(&id)?;
        profile.assets.insert(id, bytes);
    }
    profile.validate()?;
    Ok(profile)
}

#[utoipa::path(post, path = "/api/v1/profiles", tag = "profiles", request_body = CaptureProfile,
    responses((status = 201, description = "Captured the effective configuration", body = SystemProfile),
        (status = 409, description = "Profile identifier or name already exists")))]
pub async fn create(
    State(state): State<ApiState>,
    Json(request): Json<CaptureProfile>,
) -> ApiResult<(StatusCode, Json<SystemProfile>)> {
    blocking(move || {
        let profile = capture(
            &state,
            UpdateProfile {
                name: request.name,
                description: request.description,
                includes: request.includes,
            },
        )?;
        state.profiles.create(&request.id, profile.clone())?;
        Ok((StatusCode::CREATED, Json(profile)))
    })
    .await
}

#[utoipa::path(put, path = "/api/v1/profiles/{id}", tag = "profiles",
    params(("id" = String, Path)), request_body = UpdateProfile,
    responses((status = 200, description = "Updated from the effective configuration", body = SystemProfile),
        (status = 409, description = "Another profile already uses this name")))]
pub async fn update(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateProfile>,
) -> ApiResult<Json<SystemProfile>> {
    blocking(move || {
        let profile = capture(&state, request)?;
        state.profiles.update(&id, profile.clone())?;
        Ok(Json(profile))
    })
    .await
}

#[utoipa::path(get, path = "/api/v1/profiles/{id}", tag = "profiles",
    params(("id" = String, Path)),
    responses((status = 200, description = "Portable profile with embedded wallpaper assets", body = SystemProfile)))]
pub async fn download(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SystemProfile>> {
    blocking(move || Ok(Json(state.profiles.get(&id)?))).await
}

#[utoipa::path(post, path = "/api/v1/profiles/import", tag = "profiles", request_body = ImportProfile,
    responses((status = 201, description = "Stored without activation", body = ProfileSummary),
        (status = 409, description = "Profile identifier or name already exists"),
        (status = 422, description = "Unsupported or invalid profile")))]
pub async fn import(
    State(state): State<ApiState>,
    Json(request): Json<ImportProfile>,
) -> ApiResult<(StatusCode, Json<ProfileSummary>)> {
    blocking(move || {
        Ok((
            StatusCode::CREATED,
            Json(state.profiles.create(&request.id, request.profile)?),
        ))
    })
    .await
}

#[utoipa::path(delete, path = "/api/v1/profiles/{id}", tag = "profiles",
    params(("id" = String, Path)), responses((status = 204, description = "Deleted the saved profile; live configuration is unchanged")))]
pub async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    blocking(move || {
        state.profiles.delete(&id)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

impl ActivateProfile {
    fn apply(self, profile: &mut SystemProfile) -> ApiResult<()> {
        for (source, target) in &self.output_mappings {
            if target.trim().is_empty()
                || !profile
                    .config
                    .outputs
                    .iter()
                    .any(|output| output.r#match.key() == *source)
            {
                return Err(ApiError::Validation(format!(
                    "invalid output mapping {source:?} to {target:?}"
                )));
            }
        }
        for output in &mut profile.config.outputs {
            if let Some(target) = self.output_mappings.get(&output.r#match.key()) {
                output.r#match = OutputMatch::by_name(target);
                // Observed values belong to the source display.
                output.adopted = None;
            }
        }
        for (source, target) in &self.audio_mappings {
            if target.trim().is_empty()
                || !profile.config.apps.iter().any(|app| {
                    app.audio.as_ref().and_then(|audio| audio.output.as_ref()) == Some(source)
                })
            {
                return Err(ApiError::Validation(format!(
                    "invalid audio mapping {source:?} to {target:?}"
                )));
            }
        }
        for app in &mut profile.config.apps {
            if let Some(audio) = app.audio.as_mut() {
                if let Some(target) = audio
                    .output
                    .as_ref()
                    .and_then(|source| self.audio_mappings.get(source))
                {
                    audio.output = Some(target.clone());
                }
            }
        }
        Ok(())
    }
}

#[utoipa::path(post, path = "/api/v1/profiles/{id}/activate", tag = "profiles",
    params(("id" = String, Path),
        ("If-Match" = Option<String>, Header, description = "Persisted revision"),
        ("If-Config-Generation" = Option<u64>, Header, description = "Working-copy generation"),
        ("If-Config-Epoch" = Option<String>, Header, description = "Store instance identity")),
    request_body = ActivateProfile,
    responses((status = 200, description = "Merged configuration accepted and saved; reconciliation is asynchronous", body = crate::model::DesiredState),
        (status = 409, description = "Unsaved configuration, stale precondition, or conflicting wallpaper"),
        (status = 422, description = "The combined configuration is invalid")))]
pub async fn activate(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ActivateProfile>,
) -> ApiResult<VersionedConfig> {
    let mut expected = precondition(&headers)?;
    let store = state.profiles.clone();
    let mut profile = blocking(move || Ok(store.get(&id)?)).await?;
    request.apply(&mut profile)?;
    let (basis, version) = state.store.effective_with_version();
    if expected
        .revision
        .is_some_and(|revision| revision != version.revision)
        || expected
            .generation
            .is_some_and(|generation| generation != version.generation)
        || expected
            .epoch
            .as_ref()
            .is_some_and(|epoch| epoch != &version.epoch)
    {
        return Err(ApiError::Conflict(
            "Configuration changed; reload before activating a profile".into(),
        ));
    }
    if !basis.committed {
        return Err(ApiError::Conflict(
            "Save or Revert unsaved configuration before activating a profile".into(),
        ));
    }
    // Validation and asset preparation happen before the atomic transition.
    // Even callers without preconditions cannot overwrite an intervening edit.
    expected.revision.get_or_insert(version.revision);
    expected.generation.get_or_insert(version.generation);
    expected.epoch.get_or_insert(version.epoch);
    let mut merged = profile.merge(&basis);
    super::validate_configuration_literal(&mut merged, &basis, &state.validation_context())?;
    let wallpapers = state.wallpapers.clone();
    let assets = profile.assets.clone();
    blocking(move || {
        wallpapers.install_profile_assets(&assets)?;
        Ok(())
    })
    .await?;
    state
        .commit_literal_if(expected, "all", None, move |current| {
            if !current.committed {
                return Err(ApiError::Conflict(
                    "Save or Revert unsaved configuration before activating a profile".into(),
                ));
            }
            Ok(profile.merge(current))
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}
