//! Persistence of the desired-state document.
//!
//! The document is the *only* persisted state; observed state is always
//! re-derived from Sway. Writes are atomic (temp file, fsync, rename) and the
//! previous version is retained as a `.bak` fallback.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::model::{DesiredState, SCHEMA_VERSION};

const FILE_NAME: &str = "state.json";
const BACKUP_NAME: &str = "state.json.bak";
const TEMP_NAME: &str = "state.json.tmp";

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("failed to write state to {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create state directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("state document is from a newer schema version ({found}, supported {supported})")]
    UnsupportedSchema { found: u32, supported: u32 },
}

pub struct StateStore {
    dir: PathBuf,
    current: RwLock<DesiredState>,
}

impl StateStore {
    /// Load persisted state, falling back to the backup and then to an empty document.
    ///
    /// A corrupt primary file is never fatal: the appliance must still boot.
    pub fn load(dir: PathBuf) -> Result<Self, StateError> {
        std::fs::create_dir_all(&dir).map_err(|source| StateError::CreateDir {
            path: dir.clone(),
            source,
        })?;

        let primary = dir.join(FILE_NAME);
        let backup = dir.join(BACKUP_NAME);

        let state = match read_document(&primary) {
            Ok(Some(state)) => {
                tracing::info!(
                    revision = state.revision,
                    outputs = state.outputs.len(),
                    apps = state.apps.len(),
                    "loaded desired state"
                );
                state
            }
            Ok(None) => {
                tracing::info!(path = %primary.display(), "no persisted state; starting empty");
                DesiredState::new()
            }
            Err(error) => {
                tracing::error!(%error, path = %primary.display(), "state file unreadable; trying backup");
                match read_document(&backup) {
                    Ok(Some(state)) => {
                        tracing::warn!(
                            revision = state.revision,
                            "recovered desired state from backup"
                        );
                        state
                    }
                    _ => {
                        tracing::error!("backup unusable; starting from empty desired state");
                        DesiredState::new()
                    }
                }
            }
        };

        Ok(Self {
            dir,
            current: RwLock::new(state),
        })
    }

    /// An in-memory store, for tests and `--mock`.
    pub fn ephemeral(dir: PathBuf) -> Self {
        Self {
            dir,
            current: RwLock::new(DesiredState::new()),
        }
    }

    pub fn get(&self) -> DesiredState {
        self.current.read().unwrap().clone()
    }

    pub fn revision(&self) -> u64 {
        self.current.read().unwrap().revision
    }

    /// Replace the document, bump its revision, and persist it.
    ///
    /// The caller is responsible for having validated `next`.
    pub fn replace(&self, mut next: DesiredState) -> Result<DesiredState, StateError> {
        let mut guard = self.current.write().unwrap();
        next.schema_version = SCHEMA_VERSION;
        next.revision = guard.revision.saturating_add(1);
        self.persist(&next)?;
        *guard = next.clone();
        Ok(next)
    }

    /// Apply `edit` to a copy of the document, then persist the result.
    pub fn update<F>(&self, edit: F) -> Result<DesiredState, StateError>
    where
        F: FnOnce(&mut DesiredState),
    {
        let mut next = self.get();
        edit(&mut next);
        self.replace(next)
    }

    fn persist(&self, state: &DesiredState) -> Result<(), StateError> {
        let target = self.dir.join(FILE_NAME);
        let temp = self.dir.join(TEMP_NAME);
        let backup = self.dir.join(BACKUP_NAME);

        let body = serde_json::to_vec_pretty(state).expect("desired state is serializable");

        let write = || -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&body)?;
            file.write_all(b"\n")?;
            // fsync before the rename, so a power cut cannot leave a truncated file.
            file.sync_all()?;
            drop(file);

            if target.exists() {
                std::fs::copy(&target, &backup)?;
            }
            std::fs::rename(&temp, &target)?;

            // Also fsync the directory, so the rename itself is durable.
            if let Ok(dir) = std::fs::File::open(&self.dir) {
                let _ = dir.sync_all();
            }
            Ok(())
        };

        write().map_err(|source| StateError::Write {
            path: target,
            source,
        })
    }
}

/// Read and migrate a document. `Ok(None)` means the file simply does not exist.
fn read_document(path: &Path) -> Result<Option<DesiredState>, StateError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StateError::Write {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    let mut state: DesiredState =
        serde_json::from_str(&text).map_err(|error| StateError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        })?;

    if state.schema_version > SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchema {
            found: state.schema_version,
            supported: SCHEMA_VERSION,
        });
    }
    // A document written before versioning is treated as version 1.
    if state.schema_version == 0 {
        state.schema_version = SCHEMA_VERSION;
    }

    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppConfig, Launcher, OutputConfig, OutputMatch, RestartPolicy};

    fn sample_app(id: &str) -> AppConfig {
        AppConfig {
            id: id.into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "true".into(),
                args: vec![],
            },
            output: None,
            fullscreen: true,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio: None,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        assert_eq!(store.revision(), 0);

        let mut next = store.get();
        next.outputs
            .push(OutputConfig::new(OutputMatch::by_name("HDMI-A-1")));
        next.apps.push(sample_app("renderer-1"));
        let saved = store.replace(next).unwrap();
        assert_eq!(saved.revision, 1);

        let reopened = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = reopened.get();
        assert_eq!(state.revision, 1);
        assert_eq!(state.outputs.len(), 1);
        assert_eq!(state.apps[0].id, "renderer-1");
    }

    #[test]
    fn revision_increments_on_every_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        for expected in 1..=3 {
            let state = store.update(|_| {}).unwrap();
            assert_eq!(state.revision, expected);
        }
    }

    #[test]
    fn corrupt_primary_falls_back_to_backup() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();

        // First write establishes the file; second write creates the backup.
        store
            .update(|state| state.apps.push(sample_app("first")))
            .unwrap();
        store
            .update(|state| state.apps.push(sample_app("second")))
            .unwrap();

        std::fs::write(dir.path().join(FILE_NAME), b"{ this is not json").unwrap();

        let recovered = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = recovered.get();
        // The backup holds the state as of the previous write.
        assert_eq!(state.revision, 1);
        assert_eq!(state.apps.len(), 1);
        assert_eq!(state.apps[0].id, "first");
    }

    #[test]
    fn corrupt_primary_without_backup_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"nonsense").unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        assert_eq!(store.get(), DesiredState::new());
    }

    #[test]
    fn missing_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let store = StateStore::load(nested.clone()).unwrap();
        store.update(|_| {}).unwrap();
        assert!(nested.join(FILE_NAME).exists());
    }

    #[test]
    fn newer_schema_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"schemaVersion": 999, "revision": 4}"#,
        )
        .unwrap();
        // Refused, so the daemon starts empty rather than silently downgrading.
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        assert_eq!(store.get().revision, 0);
    }

    #[test]
    fn unversioned_document_is_treated_as_version_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), br#"{"revision": 7}"#).unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = store.get();
        assert_eq!(state.schema_version, SCHEMA_VERSION);
        assert_eq!(state.revision, 7);
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        store.update(|_| {}).unwrap();
        assert!(!dir.path().join(TEMP_NAME).exists());
    }
}
