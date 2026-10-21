//! Portable, named snapshots of selected desired-state sections.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    model::{BackgroundRef, DesiredState},
    wallpapers::{detect_format, MAX_BYTES},
};

pub const PROFILE_FORMAT_VERSION: u32 = 1;
pub const MAX_PROFILE_BYTES: usize = 128 * 1024 * 1024;
const MAX_PROFILE_DOCUMENT_BYTES: u64 = MAX_PROFILE_BYTES as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ProfileIncludes {
    pub displays: bool,
    pub backgrounds: bool,
    pub apps: bool,
    pub active_app: bool,
    pub settings: bool,
}

impl Default for ProfileIncludes {
    fn default() -> Self {
        Self {
            displays: true,
            backgrounds: true,
            apps: true,
            active_app: true,
            settings: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SystemProfile {
    pub format_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub includes: ProfileIncludes,
    pub config: DesiredState,
    #[serde(default)]
    pub assets: BTreeMap<String, Vec<u8>>,
}

impl SystemProfile {
    /// Remove sections that the profile declares it does not contain and
    /// strip live/document metadata that must never be restored from a profile.
    pub fn normalize(&mut self) {
        if !self.includes.displays {
            self.config.outputs.clear();
            self.config.projection = None;
        }
        if !self.includes.backgrounds {
            self.config.backgrounds.clear();
        }
        if !self.includes.apps {
            self.config.apps.clear();
        }
        if !self.includes.active_app {
            self.config.active_app = None;
        }
        if !self.includes.settings {
            self.config.settings = Default::default();
        }
        self.config.revision = 0;
        self.config.committed = false;
        if let Some(projection) = self.config.projection.as_mut() {
            projection.temporary = Default::default();
        }
        let referenced = self.referenced_wallpapers();
        self.assets.retain(|id, _| referenced.contains(id));
    }

    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.format_version != PROFILE_FORMAT_VERSION {
            return Err(ProfileError::UnsupportedVersion(self.format_version));
        }
        validate_name(&self.name)?;
        if self.config.schema_version != crate::model::SCHEMA_VERSION {
            return Err(ProfileError::InvalidProfile(format!(
                "config schemaVersion must be {}",
                crate::model::SCHEMA_VERSION
            )));
        }
        if self.assets.len() > 4096 {
            return Err(ProfileError::TooLarge {
                size: self.assets.len(),
            });
        }
        let mut total = 0usize;
        for (id, bytes) in &self.assets {
            validate_id(id)?;
            total = total
                .checked_add(bytes.len())
                .ok_or(ProfileError::TooLarge { size: usize::MAX })?;
            if bytes.len() > MAX_BYTES {
                return Err(ProfileError::TooLarge { size: bytes.len() });
            }
            if detect_format(bytes).is_none() {
                return Err(ProfileError::UnsupportedAsset(id.clone()));
            }
        }
        if total > MAX_PROFILE_BYTES {
            return Err(ProfileError::TooLarge { size: total });
        }
        for id in self.referenced_wallpapers() {
            if !self.assets.contains_key(&id) {
                return Err(ProfileError::InvalidProfile(format!(
                    "included wallpaper {id:?} has no embedded asset"
                )));
            }
        }
        Ok(())
    }

    /// Wallpaper identifiers referenced by the selected background section.
    pub fn referenced_wallpapers(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        if self.includes.backgrounds {
            for preset in &self.config.backgrounds {
                if let Some(id) = &preset.background.wallpaper {
                    ids.insert(id.clone());
                }
            }
        }
        if self.includes.displays {
            for output in &self.config.outputs {
                match &output.background {
                    Some(BackgroundRef::Inline(background)) => {
                        if let Some(id) = &background.wallpaper {
                            ids.insert(id.clone());
                        }
                    }
                    Some(BackgroundRef::Preset(_)) | None => {}
                }
            }
        }
        // Output references can name presets; collect their image references too.
        if self.includes.displays {
            for output in &self.config.outputs {
                if let Some(BackgroundRef::Preset(name)) = &output.background {
                    if !self.includes.backgrounds {
                        continue;
                    }
                    if let Some(preset) = self.config.backgrounds.iter().find(|p| &p.id == name) {
                        if let Some(id) = &preset.background.wallpaper {
                            ids.insert(id.clone());
                        }
                    }
                }
            }
        }
        ids
    }

    /// Apply the selected sections over a live document, preserving excluded sections.
    pub fn merge(&self, current: &DesiredState) -> DesiredState {
        let mut merged = current.clone();
        if self.includes.displays {
            merged.outputs = self.config.outputs.clone();
            merged.projection = self.config.projection.clone();
        }
        if self.includes.backgrounds {
            merged.backgrounds = self.config.backgrounds.clone();
        }
        if self.includes.apps {
            merged.apps = self.config.apps.clone();
        }
        if self.includes.active_app {
            merged.active_app = self.config.active_app.clone();
        }
        if self.includes.settings {
            merged.settings = self.config.settings;
        }
        merged
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub includes: ProfileIncludes,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("invalid profile id {0:?}")]
    InvalidId(String),
    #[error("profile name must contain 1 to 120 non-whitespace characters")]
    InvalidName,
    #[error("profile {0:?} already exists")]
    AlreadyExists(String),
    #[error("profile name {0:?} is already in use")]
    NameAlreadyExists(String),
    #[error("profile {0:?} was not found")]
    NotFound(String),
    #[error("unsupported profile format version {0}")]
    UnsupportedVersion(u32),
    #[error("profile is invalid: {0}")]
    InvalidProfile(String),
    #[error("unsupported or invalid image asset {0:?}; PNG and JPEG are supported")]
    UnsupportedAsset(String),
    #[error("profile is {size} bytes, over the {MAX_PROFILE_BYTES} byte limit")]
    TooLarge { size: usize },
    #[error("profile storage I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("profile JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct ProfileStore {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl ProfileStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            lock: Mutex::new(()),
        }
    }

    pub fn list(&self) -> Result<Vec<ProfileSummary>, ProfileError> {
        let _guard = self.lock.lock().unwrap();
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let id = entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned();
            if validate_id(&id).is_err() {
                continue;
            }
            let profile = read_profile(&entry.path())?;
            out.push(summary(id, &profile));
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<SystemProfile, ProfileError> {
        validate_id(id)?;
        let _guard = self.lock.lock().unwrap();
        let path = self.path_for(id);
        if !path.is_file() {
            return Err(ProfileError::NotFound(id.to_owned()));
        }
        let mut profile = read_profile(&path)?;
        profile.validate()?;
        profile.normalize();
        Ok(profile)
    }

    pub fn create(&self, id: &str, profile: SystemProfile) -> Result<ProfileSummary, ProfileError> {
        validate_id(id)?;
        let _guard = self.lock.lock().unwrap();
        fs::create_dir_all(&self.dir)?;
        let path = self.path_for(id);
        if path.exists() {
            return Err(ProfileError::AlreadyExists(id.to_owned()));
        }
        profile.validate()?;
        self.ensure_unique_name(id, &profile.name)?;
        self.write_atomic(&path, profile)?;
        let stored = read_profile(&path)?;
        Ok(summary(id.to_owned(), &stored))
    }

    pub fn update(&self, id: &str, profile: SystemProfile) -> Result<ProfileSummary, ProfileError> {
        validate_id(id)?;
        let _guard = self.lock.lock().unwrap();
        let path = self.path_for(id);
        if !path.is_file() {
            return Err(ProfileError::NotFound(id.to_owned()));
        }
        profile.validate()?;
        self.ensure_unique_name(id, &profile.name)?;
        self.write_atomic(&path, profile)?;
        let stored = read_profile(&path)?;
        Ok(summary(id.to_owned(), &stored))
    }

    pub fn delete(&self, id: &str) -> Result<(), ProfileError> {
        validate_id(id)?;
        let _guard = self.lock.lock().unwrap();
        let path = self.path_for(id);
        match fs::remove_file(path) {
            Ok(()) => {
                if let Ok(dir) = File::open(&self.dir) {
                    let _ = dir.sync_all();
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ProfileError::NotFound(id.to_owned()))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Check names while the caller holds the store lock.
    fn ensure_unique_name(&self, id: &str, name: &str) -> Result<(), ProfileError> {
        let normalized_name = normalize_name(name);
        if !self.dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let stored_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if stored_id == id {
                continue;
            }
            let stored = read_profile(&path)?;
            stored.validate()?;
            if normalize_name(&stored.name) == normalized_name {
                return Err(ProfileError::NameAlreadyExists(name.trim().to_owned()));
            }
        }
        Ok(())
    }

    fn write_atomic(&self, path: &Path, mut profile: SystemProfile) -> Result<(), ProfileError> {
        profile.validate()?;
        profile.name = profile.name.trim().to_owned();
        profile.normalize();
        let bytes = serde_json::to_vec(&profile)?;
        if bytes.len() > MAX_PROFILE_BYTES {
            return Err(ProfileError::TooLarge { size: bytes.len() });
        }
        fs::create_dir_all(&self.dir)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| std::io::Error::other("could not obtain a temporary filename"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temp_path = self.dir.join(format!(".profile-{suffix}.tmp"));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp_path, path)?;
            if let Ok(dir) = File::open(&self.dir) {
                let _ = dir.sync_all();
            }
            Ok::<_, std::io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result?;
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), ProfileError> {
    if id.is_empty()
        || id.len() > 120
        || id == "."
        || id.contains("..")
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(ProfileError::InvalidId(id.to_owned()));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), ProfileError> {
    if name.trim().is_empty() || name.chars().count() > 120 {
        return Err(ProfileError::InvalidName);
    }
    Ok(())
}

fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn read_profile(path: &Path) -> Result<SystemProfile, ProfileError> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_PROFILE_DOCUMENT_BYTES {
        return Err(ProfileError::TooLarge {
            size: metadata.len().min(usize::MAX as u64) as usize,
        });
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn summary(id: String, profile: &SystemProfile) -> ProfileSummary {
    ProfileSummary {
        id,
        name: profile.name.clone(),
        description: profile.description.clone(),
        includes: profile.includes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> SystemProfile {
        let config = DesiredState {
            schema_version: crate::model::SCHEMA_VERSION,
            ..Default::default()
        };
        SystemProfile {
            format_version: 1,
            name: "Stage".into(),
            description: String::new(),
            includes: ProfileIncludes::default(),
            config,
            assets: BTreeMap::new(),
        }
    }

    #[test]
    fn section_merge_replaces_included_empty_values_and_preserves_excluded_values() {
        let mut p = profile();
        p.includes = ProfileIncludes {
            displays: false,
            backgrounds: true,
            apps: true,
            active_app: false,
            settings: false,
        };
        let current = DesiredState {
            active_app: Some("keep".into()),
            ..Default::default()
        };
        let merged = p.merge(&current);
        assert!(merged.apps.is_empty());
        assert_eq!(merged.active_app, Some("keep".into()));
        assert!(merged.outputs.is_empty());
        assert_eq!(merged.settings, current.settings);
    }

    #[test]
    fn normalize_strips_excluded_sections_and_ephemeral_metadata() {
        let mut p = profile();
        p.includes = ProfileIncludes {
            displays: false,
            backgrounds: false,
            apps: false,
            active_app: false,
            settings: false,
        };
        p.config.revision = 99;
        p.config.committed = true;
        p.normalize();
        assert_eq!(p.config.revision, 0);
        assert!(!p.config.committed);
        assert!(p.config.outputs.is_empty());
        assert!(p.config.apps.is_empty());
    }

    #[test]
    fn profile_store_roundtrips_and_enforces_crud() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().join("profiles"));
        store.create("show-1", profile()).unwrap();
        assert_eq!(store.get("show-1").unwrap().name, "Stage");
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(matches!(
            store.create("show-1", profile()),
            Err(ProfileError::AlreadyExists(_))
        ));
        store.delete("show-1").unwrap();
        assert!(matches!(
            store.get("show-1"),
            Err(ProfileError::NotFound(_))
        ));
    }

    #[test]
    fn profile_names_are_unique_across_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().join("profiles"));
        store.create("show-1", profile()).unwrap();

        assert!(matches!(
            store.create("show-2", profile()),
            Err(ProfileError::NameAlreadyExists(name)) if name == "Stage"
        ));
    }

    #[test]
    fn profile_names_are_trimmed_and_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().join("profiles"));
        let mut original = profile();
        original.name = "  Stage  ".into();
        let created = store.create("show-1", original).unwrap();
        assert_eq!(created.name, "Stage");

        let mut collision = profile();
        collision.name = " sTaGe ".into();
        assert!(matches!(
            store.create("show-2", collision),
            Err(ProfileError::NameAlreadyExists(name)) if name == "sTaGe"
        ));
    }

    #[test]
    fn rename_collision_keeps_existing_profile_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().join("profiles"));
        store.create("show-1", profile()).unwrap();
        let mut other = profile();
        other.name = "Lobby".into();
        store.create("show-2", other).unwrap();

        let mut renamed = profile();
        renamed.name = "Lobby".into();
        assert!(matches!(
            store.update("show-1", renamed),
            Err(ProfileError::NameAlreadyExists(name)) if name == "Lobby"
        ));
        assert_eq!(store.get("show-1").unwrap().name, "Stage");
        assert_eq!(store.get("show-2").unwrap().name, "Lobby");
    }

    #[test]
    fn updating_a_profile_with_its_own_name_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(dir.path().join("profiles"));
        store.create("show-1", profile()).unwrap();

        let updated = store.update("show-1", profile()).unwrap();
        assert_eq!(updated.name, "Stage");
    }

    #[test]
    fn rejects_invalid_versions_and_path_ids() {
        let mut p = profile();
        p.format_version = 2;
        assert!(matches!(
            p.validate(),
            Err(ProfileError::UnsupportedVersion(2))
        ));
        assert!(matches!(
            ProfileStore::new(PathBuf::new()).get("../oops"),
            Err(ProfileError::InvalidId(_))
        ));
    }
}
