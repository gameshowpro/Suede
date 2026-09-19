//! Persistence of the desired-state document.
//!
//! The document is the *only* persisted state; observed state is always
//! re-derived from Sway. Writes are atomic (temp file, fsync, rename) and the
//! previous version is retained as a `.bak` fallback.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

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
    #[error("state document has unsupported schema version ({found}, supported {supported})")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("state document at {path} is invalid: {detail}")]
    InvalidDocument { path: PathBuf, detail: String },
}

/// The saved document and the unsaved one, under a single lock.
///
/// They were two locks once, and that deadlocked an appliance. `replace` took
/// the committed document and then the preview; `effective` read the preview
/// and then — still holding it, because the temporary guard lives to the end
/// of the statement — the committed one. Opposite orders, so two requests
/// crossing was enough to stop the daemon dead, and a client with a slider
/// bound to a display offset crossed them within seconds.
///
/// One lock cannot be taken out of order. That is the whole reason these live
/// together rather than apart: it is not a tidier arrangement of the same
/// risk, it is the removal of the risk.
struct Documents {
    current: DesiredState,
    /// An unsaved document being tried out live. Never persisted: a daemon
    /// restart or any committed write discards it, so disk state stays the
    /// only durable truth.
    preview: Option<DesiredState>,
    /// In-memory basis for read-only previews/recommendations. This changes
    /// for every working-copy transition, including revert, while persisted
    /// document revisions retain their existing meaning.
    generation: u64,
}

pub struct StateStore {
    dir: PathBuf,
    /// Identifies this in-memory store instance. It is not a secret: it only
    /// prevents a generation counter reset at daemon restart from looking like
    /// the same working copy to an old client.
    epoch: String,
    documents: RwLock<Documents>,
}

/// The two identities of a live configuration document.
///
/// `revision` identifies the persisted document. `generation` additionally
/// identifies the in-memory working copy, so two previews of the same saved
/// revision remain distinguishable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateVersion {
    pub revision: u64,
    pub generation: u64,
    pub epoch: String,
}

/// Optional compare-and-swap conditions for a document transition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatePrecondition {
    pub revision: Option<u64>,
    pub generation: Option<u64>,
    pub epoch: Option<String>,
}

/// A rejected conditional transition, or an error returned while preparing it.
#[derive(Debug)]
pub enum ConditionalWriteError<E> {
    Precondition { current: StateVersion },
    Rejected(E),
    State(StateError),
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
            Err(error @ StateError::UnsupportedSchema { .. }) => return Err(error),
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
                    Err(error @ StateError::UnsupportedSchema { .. }) => return Err(error),
                    _ => {
                        tracing::error!("backup unusable; starting from empty desired state");
                        DesiredState::new()
                    }
                }
            }
        };

        Ok(Self {
            dir,
            epoch: fresh_epoch(),
            documents: RwLock::new(Documents {
                current: {
                    let mut state = state;
                    // Whatever we booted from is the committed baseline, even
                    // a pre-flag document or the empty default.
                    state.committed = true;
                    state
                },
                preview: None,
                generation: 0,
            }),
        })
    }

    /// An in-memory store, for tests and `--mock`.
    pub fn ephemeral(dir: PathBuf) -> Self {
        Self {
            dir,
            epoch: fresh_epoch(),
            documents: RwLock::new(Documents {
                current: {
                    let mut state = DesiredState::new();
                    state.committed = true;
                    state
                },
                preview: None,
                generation: 0,
            }),
        }
    }

    pub fn get(&self) -> DesiredState {
        self.documents.read().unwrap().current.clone()
    }

    /// What the reconciler should realize: the preview if one is being tried
    /// out, else the persisted document.
    pub fn effective(&self) -> DesiredState {
        // One acquisition, both fields. Reading the preview and then calling
        // `get()` for the committed one is what deadlocked against `replace`,
        // because the first guard was still alive when the second was taken.
        let documents = self.documents.read().unwrap();
        documents
            .preview
            .clone()
            .unwrap_or_else(|| documents.current.clone())
    }

    /// Set or clear the working copy. The caller validates.
    pub fn set_preview(&self, preview: Option<DesiredState>) {
        let preview = preview.map(|mut document| {
            // A working copy always reads back honestly: not committed, and
            // carrying the revision of the saved document it shadows.
            document.committed = false;
            document.revision = self.revision();
            document
        });
        let mut documents = self.documents.write().unwrap();
        documents.preview = preview;
        documents.generation = documents.generation.saturating_add(1);
    }

    pub fn has_preview(&self) -> bool {
        self.documents.read().unwrap().preview.is_some()
    }

    pub fn revision(&self) -> u64 {
        self.documents.read().unwrap().current.revision
    }

    /// Return the effective document and the in-memory working-copy basis that
    /// produced it. Two previews can share a persisted revision while still
    /// needing distinct recommendation/cache identities.
    pub fn effective_with_generation(&self) -> (DesiredState, u64) {
        let documents = self.documents.read().unwrap();
        (
            documents
                .preview
                .clone()
                .unwrap_or_else(|| documents.current.clone()),
            documents.generation,
        )
    }

    pub fn generation(&self) -> u64 {
        self.documents.read().unwrap().generation
    }

    /// Read the effective document and both version identities in one lock
    /// acquisition. HTTP responses use this to avoid advertising a revision
    /// from one transition with a generation from another.
    pub fn effective_with_version(&self) -> (DesiredState, StateVersion) {
        let documents = self.documents.read().unwrap();
        (
            documents
                .preview
                .clone()
                .unwrap_or_else(|| documents.current.clone()),
            StateVersion {
                revision: documents.current.revision,
                generation: documents.generation,
                epoch: self.epoch.clone(),
            },
        )
    }

    /// Atomically validate the expected version, build a saved document from
    /// the current one, and persist it. `prepare` also receives the effective
    /// document so retained projection settings keep the same semantics when a
    /// working copy is live.
    pub fn replace_if<E, F>(
        &self,
        expected: StatePrecondition,
        prepare: F,
    ) -> Result<(DesiredState, StateVersion), ConditionalWriteError<E>>
    where
        F: FnOnce(&DesiredState, &DesiredState) -> Result<DesiredState, E>,
    {
        let mut documents = self.documents.write().unwrap();
        self.check_expected(&documents, expected)?;
        let effective = documents
            .preview
            .as_ref()
            .unwrap_or(&documents.current)
            .clone();
        let mut next =
            prepare(&documents.current, &effective).map_err(ConditionalWriteError::Rejected)?;
        next.schema_version = SCHEMA_VERSION;
        next.revision = documents.current.revision.saturating_add(1);
        next.committed = true;
        self.persist(&next).map_err(ConditionalWriteError::State)?;
        documents.current = next.clone();
        documents.preview = None;
        documents.generation = documents.generation.saturating_add(1);
        let version = self.version(&documents);
        Ok((next, version))
    }

    /// Atomically validate the expected version and replace the live working
    /// copy. The supplied document is never persisted.
    pub fn set_preview_if<E, F>(
        &self,
        expected: StatePrecondition,
        prepare: F,
    ) -> Result<(DesiredState, StateVersion), ConditionalWriteError<E>>
    where
        F: FnOnce(&DesiredState) -> Result<DesiredState, E>,
    {
        let mut documents = self.documents.write().unwrap();
        self.check_expected(&documents, expected)?;
        let effective = documents
            .preview
            .as_ref()
            .unwrap_or(&documents.current)
            .clone();
        let mut preview = prepare(&effective).map_err(ConditionalWriteError::Rejected)?;
        preview.committed = false;
        preview.revision = documents.current.revision;
        documents.preview = Some(preview.clone());
        documents.generation = documents.generation.saturating_add(1);
        let version = self.version(&documents);
        Ok((preview, version))
    }

    /// Atomically discard a working copy when its version still matches.
    pub fn clear_preview_if(
        &self,
        expected: StatePrecondition,
    ) -> Result<(DesiredState, StateVersion), ConditionalWriteError<std::convert::Infallible>> {
        let mut documents = self.documents.write().unwrap();
        self.check_expected(&documents, expected)?;
        documents.preview = None;
        documents.generation = documents.generation.saturating_add(1);
        let document = documents.current.clone();
        let version = self.version(&documents);
        Ok((document, version))
    }

    /// Replace the document, bump its revision, and persist it.
    ///
    /// The caller is responsible for having validated `next`.
    pub fn replace(&self, mut next: DesiredState) -> Result<DesiredState, StateError> {
        let mut documents = self.documents.write().unwrap();
        next.schema_version = SCHEMA_VERSION;
        next.revision = documents.current.revision.saturating_add(1);
        // What reaches disk is by definition the committed document.
        next.committed = true;
        self.persist(&next)?;
        documents.current = next.clone();
        // A committed write supersedes whatever was being previewed. Same
        // guard, so this can no longer race the readers.
        documents.preview = None;
        documents.generation = documents.generation.saturating_add(1);
        Ok(next)
    }

    /// Apply `edit` to a copy of the document, then persist the result.
    pub fn update<F>(&self, edit: F) -> Result<DesiredState, StateError>
    where
        F: FnOnce(&mut DesiredState),
    {
        let result = self.replace_if(StatePrecondition::default(), |current, _| {
            let mut next = current.clone();
            edit(&mut next);
            Ok::<_, std::convert::Infallible>(next)
        });
        match result {
            Ok((state, _)) => Ok(state),
            Err(ConditionalWriteError::State(error)) => Err(error),
            Err(ConditionalWriteError::Precondition { .. }) => {
                unreachable!("an empty precondition cannot be rejected")
            }
            Err(ConditionalWriteError::Rejected(never)) => match never {},
        }
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

    fn version(&self, documents: &Documents) -> StateVersion {
        StateVersion {
            revision: documents.current.revision,
            generation: documents.generation,
            epoch: self.epoch.clone(),
        }
    }

    fn check_expected<E>(
        &self,
        documents: &Documents,
        expected: StatePrecondition,
    ) -> Result<(), ConditionalWriteError<E>> {
        let current = self.version(documents);
        if expected
            .revision
            .is_some_and(|revision| revision != current.revision)
            || expected
                .generation
                .is_some_and(|generation| generation != current.generation)
            || expected
                .epoch
                .as_ref()
                .is_some_and(|epoch| epoch != &current.epoch)
        {
            return Err(ConditionalWriteError::Precondition { current });
        }
        Ok(())
    }
}

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(0);

fn fresh_epoch() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", std::process::id(), nanos, sequence)
}

/// Read a current-schema document. `Ok(None)` means the file simply does not exist.
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

    let state: DesiredState = serde_json::from_str(&text).map_err(|error| StateError::Write {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
    })?;

    if state.schema_version > SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchema {
            found: state.schema_version,
            supported: SCHEMA_VERSION,
        });
    }
    // Alpha state files must carry this exact schema; older documents are not
    // migrated.
    if state.schema_version != SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchema {
            found: state.schema_version,
            supported: SCHEMA_VERSION,
        });
    }

    if let Err(errors) = state.validate(true) {
        return Err(StateError::InvalidDocument {
            path: path.to_path_buf(),
            detail: errors.join("; "),
        });
    }

    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AppConfig, CanvasConfig, CanvasRect, Launcher, Mode, OutputConfig, OutputGeometry,
        OutputMatch, Position, ProjectionConfig, ProjectionMode, RestartPolicy,
    };

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
        let mut output = OutputConfig::new(OutputMatch::by_name("HDMI-A-1"));
        output.mode = Some(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        });
        output.position = Some(Position { x: 0, y: 0 });
        output.geometry = Some(OutputGeometry {
            source: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            corners: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            center: [0.5, 0.5],
            raster_footprint: CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
        });
        next.outputs.push(output);
        next.projection = Some(ProjectionConfig {
            mode: ProjectionMode::Warp,
            canvas: Some(CanvasConfig {
                aspect: 1.0,
                render_width: 1920,
                scale: 1.0,
            }),
            ..ProjectionConfig::default()
        });
        next.apps.push(sample_app("renderer-1"));
        let saved = store.replace(next).unwrap();
        assert_eq!(saved.revision, 1);

        let reopened = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = reopened.get();
        assert_eq!(state.revision, 1);
        assert_eq!(state.outputs.len(), 1);
        assert_eq!(state.apps[0].id, "renderer-1");
        assert_eq!(
            state.outputs[0].geometry.as_ref().unwrap().center,
            [0.5, 0.5]
        );
        assert_eq!(
            state.projection.as_ref().unwrap().mode,
            ProjectionMode::Warp
        );
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
        let mut expected = DesiredState::new();
        expected.committed = true;
        assert_eq!(store.get(), expected);
    }

    #[test]
    fn invalid_current_schema_primary_falls_back_to_valid_backup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{
                "schemaVersion": 2,
                "outputs": [{
                    "match": {"name": "HDMI-A-1"},
                    "enable": true,
                    "mode": {"width": 100, "height": 100, "refreshHz": 60},
                    "position": {"x": 0, "y": 0},
                    "geometry": {
                        "source": {"x": 0, "y": 0, "width": 1, "height": 1},
                        "corners": [[0,0],[0,0],[1,1],[0,1]],
                        "rasterFootprint": {"x": 0, "y": 0, "width": 1, "height": 1}
                    }
                }],
                "projection": {
                    "mode": "warp",
                    "canvas": {"aspect": 1, "renderWidth": 100}
                }
            }"#,
        )
        .unwrap();
        std::fs::write(dir.path().join(BACKUP_NAME), br#"{"schemaVersion": 2}"#).unwrap();

        let recovered = StateStore::load(dir.path().to_path_buf()).unwrap();
        assert!(recovered.get().outputs.is_empty());
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
        let result = StateStore::load(dir.path().to_path_buf());
        assert!(matches!(
            result,
            Err(StateError::UnsupportedSchema { found: 999, .. })
        ));
    }

    #[test]
    fn newer_backup_schema_is_refused_instead_of_discarded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"not json").unwrap();
        std::fs::write(
            dir.path().join(BACKUP_NAME),
            br#"{"schemaVersion": 999, "revision": 4}"#,
        )
        .unwrap();
        let result = StateStore::load(dir.path().to_path_buf());
        assert!(matches!(
            result,
            Err(StateError::UnsupportedSchema { found: 999, .. })
        ));
    }

    #[test]
    fn unversioned_document_is_not_migrated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), br#"{"revision": 7}"#).unwrap();
        let result = StateStore::load(dir.path().to_path_buf());
        assert!(matches!(
            result,
            Err(StateError::UnsupportedSchema { found: 0, .. })
        ));
    }

    #[test]
    fn preview_generation_changes_without_changing_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        let initial = store.generation();
        let mut preview = store.get();
        preview.revision = 999;
        store.set_preview(Some(preview));
        let (_, preview_generation) = store.effective_with_generation();
        assert!(preview_generation > initial);
        assert_eq!(store.revision(), 0);
        store.set_preview(None);
        let reverted_generation = store.generation();
        assert!(reverted_generation > preview_generation);
        store.update(|_| {}).unwrap();
        assert!(store.generation() > reverted_generation);
    }

    #[test]
    fn stale_epoch_rejects_a_restart_aba_with_the_same_revision_and_generation() {
        let dir = tempfile::tempdir().unwrap();
        let first = StateStore::load(dir.path().to_path_buf()).unwrap();
        let (_, old) = first.effective_with_version();
        drop(first);
        let second = StateStore::load(dir.path().to_path_buf()).unwrap();
        let (_, current) = second.effective_with_version();
        assert_eq!(old.revision, current.revision);
        assert_eq!(old.generation, current.generation);
        assert_ne!(old.epoch, current.epoch);

        let result = second.set_preview_if(
            StatePrecondition {
                revision: Some(old.revision),
                generation: Some(old.generation),
                epoch: Some(old.epoch),
            },
            |document| Ok::<_, std::convert::Infallible>(document.clone()),
        );
        assert!(matches!(
            result,
            Err(ConditionalWriteError::Precondition { .. })
        ));
    }

    #[test]
    fn stale_conditional_commit_cannot_clear_a_newer_preview() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::ephemeral(dir.path().to_path_buf());
        let (_, version) = store.effective_with_version();
        let mut preview = store.get();
        preview.settings.hide_cursor = false;
        store
            .set_preview_if(StatePrecondition::default(), |_| {
                Ok::<_, std::convert::Infallible>(preview)
            })
            .unwrap();

        // This models a reconciler adoption computed from `version` before
        // the operator's preview was accepted. Its CAS must fail rather than
        // persisting the old committed copy and clearing that preview.
        let result = store.replace_if(
            StatePrecondition {
                revision: Some(version.revision),
                generation: Some(version.generation),
                epoch: Some(version.epoch),
            },
            |current, _| Ok::<_, std::convert::Infallible>(current.clone()),
        );
        assert!(matches!(
            result,
            Err(ConditionalWriteError::Precondition { .. })
        ));
        assert!(store.has_preview());
        assert!(!store.effective().settings.hide_cursor);
        assert!(store.get().settings.hide_cursor);
    }

    /// The project's alpha status means the only backwards-compatibility
    /// promise Suede makes is that an upgrade migrates correctly, which makes
    /// `Settings::allow_raw_sway_commands`'s `#[serde(default,
    /// skip_serializing)]` the single load-bearing guarantee behind retiring
    /// the raw-command gate: `Settings` refuses unknown fields and there is
    /// no migration step, so that shim is the only thing standing between an
    /// upgrade and a daemon that cannot read its own saved configuration.
    ///
    /// The document is a string literal, not built from today's `Settings`,
    /// because a document built from the current struct could never exercise
    /// the regression this guards against — it would never have had the
    /// field serialized into it to begin with.
    #[test]
    fn a_document_from_before_the_raw_command_flag_was_retired_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"schemaVersion": 2, "settings": {"hideCursor": true, "outputPollIntervalSeconds": 5,
                 "allowRawSwayCommands": false}}"#,
        )
        .unwrap();

        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = store.get();
        assert!(state.settings.hide_cursor);
        assert_eq!(state.settings.output_poll_interval_seconds, 5);

        // Proves it drops off disk at the next save: `persist` serializes
        // with exactly this `Serialize` impl, so if the key survives here it
        // would survive there too.
        let resaved = serde_json::to_string(&state).unwrap();
        assert!(
            !resaved.contains("allowRawSwayCommands"),
            "the retired field must never be written back: {resaved}"
        );
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        store.update(|_| {}).unwrap();
        assert!(!dir.path().join(TEMP_NAME).exists());
    }
}
