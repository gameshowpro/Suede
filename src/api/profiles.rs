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
    ActivateSections, ProfileError, ProfileIncludes, ProfileSummary, SectionSelection,
    SystemProfile, PROFILE_FORMAT_VERSION,
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
    pub includes: CaptureIncludes,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub includes: CaptureIncludes,
}

/// The sections to capture. Omitted switches default to `true`. The stored
/// profile always records `apps` as a boolean: a nonempty subset stores as
/// `true` with only the chosen apps in its catalog.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct CaptureIncludes {
    pub displays: bool,
    pub backgrounds: bool,
    /// `true` for every current app, `false` or `[]` for none, or the ids of
    /// apps in the current configuration.
    pub apps: SectionSelection,
    pub active_app: bool,
    pub settings: bool,
}

impl Default for CaptureIncludes {
    fn default() -> Self {
        let all = ProfileIncludes::default();
        Self {
            displays: all.displays,
            backgrounds: all.backgrounds,
            apps: SectionSelection::Flag(all.apps),
            active_app: all.active_app,
            settings: all.settings,
        }
    }
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
    /// Sections to apply. Omitted, every section the profile includes.
    pub sections: ActivateSections,
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
    let selection = request.includes;
    let mut config = state.store.effective();
    if let Some(ids) = selection.apps.ids() {
        if let Some(unknown) = ids
            .iter()
            .find(|id| !config.apps.iter().any(|app| app.id == **id))
        {
            return Err(ApiError::Validation(format!(
                "app {unknown:?} is not in the current configuration"
            )));
        }
        // Keep the catalog's order, not the request's, so a stored subset is
        // stable however the client assembled the list.
        config.apps.retain(|app| ids.contains(app.id.as_str()));
        if selection.active_app {
            if let Some(active) = config
                .active_app
                .as_deref()
                .filter(|active| !ids.contains(active))
            {
                return Err(ApiError::Validation(format!(
                    "the active app {active:?} is not among the selected apps; \
                     select it or leave out the Active app section"
                )));
            }
        }
    }
    let mut profile = SystemProfile {
        format_version: PROFILE_FORMAT_VERSION,
        name: request.name,
        description: request.description,
        includes: ProfileIncludes {
            displays: selection.displays,
            backgrounds: selection.backgrounds,
            apps: !selection.apps.is_none(),
            active_app: selection.active_app,
            settings: selection.settings,
        },
        config,
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
        (status = 409, description = "Profile identifier or name already exists"),
        (status = 422, description = "Invalid name, an app id not in the current configuration, or an active app outside the selected apps")))]
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
        (status = 409, description = "Another profile already uses this name"),
        (status = 422, description = "Invalid name, an app id not in the current configuration, or an active app outside the selected apps")))]
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
    /// Validate the request against the stored profile, then rewrite its
    /// device names. Mappings are checked against the whole stored profile,
    /// even for sections this activation leaves out.
    fn apply(&self, profile: &mut SystemProfile) -> ApiResult<()> {
        self.check_sections(profile)?;
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

    fn check_sections(&self, profile: &SystemProfile) -> ApiResult<()> {
        let sections = &self.sections;
        let includes = profile.includes;
        let requested = [
            (
                "displays",
                sections.displays == Some(true),
                includes.displays,
            ),
            (
                "backgrounds",
                sections.backgrounds == Some(true),
                includes.backgrounds,
            ),
            (
                "apps",
                sections
                    .apps
                    .as_ref()
                    .is_some_and(|selection| !selection.is_none()),
                includes.apps,
            ),
            (
                "activeApp",
                sections.active_app == Some(true),
                includes.active_app,
            ),
            (
                "settings",
                sections.settings == Some(true),
                includes.settings,
            ),
        ];
        for (section, selected, included) in requested {
            if selected && !included {
                return Err(ApiError::Validation(format!(
                    "profile {:?} does not include {section}",
                    profile.name
                )));
            }
        }
        if let Some(ids) = sections.apps.as_ref().and_then(SectionSelection::ids) {
            if let Some(unknown) = ids
                .iter()
                .find(|id| !profile.config.apps.iter().any(|app| app.id == **id))
            {
                return Err(ApiError::Validation(format!(
                    "app {unknown:?} is not in this profile"
                )));
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
        (status = 422, description = "A section or app the profile does not include, an invalid mapping, or an invalid combined configuration")))]
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
    let sections = request.sections;
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
    // Both merges must take the same sections, or the document validated here
    // is not the one committed below.
    let mut merged = profile.merge_selected(&basis, &sections);
    super::validate_configuration_literal(&mut merged, &basis, &state.validation_context())?;
    let wallpapers = state.wallpapers.clone();
    let assets = profile.assets_for(&sections);
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
            Ok(profile.merge_selected(current, &sections))
        })
        .await
        .map(|(document, version)| VersionedConfig::new(document, version))
}
