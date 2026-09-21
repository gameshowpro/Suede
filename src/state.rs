//! Persistence of the desired-state document.
//!
//! The document is the *only* persisted state; observed state is always
//! re-derived from Sway. Writes are atomic (temp file, fsync, rename) and the
//! previous version is retained as a `.bak` fallback.
//!
//! # Two locks, and the order they are taken in
//!
//! [`StateStore::documents`] holds what is live; [`StateStore::writer`] holds
//! everything about the file on disk. A transition swaps the document in
//! memory under `documents` alone and persists afterwards, so the two are
//! never nested: [`StateStore::flush`] copies the newest document out under a
//! *read* guard, drops it, and only then takes `writer`.
//!
//! Nothing else may be reached for while `documents` is held — no disk, no
//! [`crate::snapshot::Snapshot`], no capability probe. A `prepare` closure
//! runs under that guard, so everything it needs (capability status, the
//! bootstrap's overlap policy) is read *before* the store is entered. The
//! deadlock recorded on [`Documents`] came from exactly this kind of nesting,
//! and persisting under the write guard had put the fsync of a 4 kB file,
//! plus every lock a validator touches, inside it.
//!
//! # When a save fails
//!
//! Memory wins, loudly. The document stays live — the reconciler is already
//! driving the outputs toward it and other clients have been told it was
//! accepted — the write's own response is a 500 saying it was not saved, and
//! [`StateStore::persist_failure`] keeps reporting the failure (as a
//! `state_not_persisted` divergence in `/status`) until a later write reaches
//! disk. Disk therefore lags memory rather than contradicting it: what is on
//! disk is always some earlier revision of the same document, and a restart
//! loses the unsaved changes instead of applying half of them. Rolling memory
//! back instead was rejected because by then the change may already be on the
//! wall, and a concurrent writer may already have built on it.
//!
//! Saves are serialized by `writer` and ordered by revision: each flush writes
//! whatever is newest at the moment it runs, and one that finds a newer
//! revision already on disk does nothing. An earlier revision can therefore
//! never overwrite a later one, however the executor schedules the flushes.
//!
//! # An old or damaged document must never stop the daemon
//!
//! Suede is alpha and does not migrate documents, but refusing to boot on one
//! is never the right answer: an appliance with an obsolete `state.json` has
//! to come up with *something*. [`read_document`] therefore repairs instead of
//! refusing — fields this build no longer knows about are dropped, a section
//! that cannot be read at all falls back to its default, and a document that
//! fails validation is degraded piece by piece until it passes. Every repair
//! is logged at error level and surfaced through
//! [`StateStore::load_repairs`]. The original file is copied to
//! `state.json.rejected` first, so nothing is silently rewritten.
//!
//! The one refusal left is a document from a *newer* Suede: that is not an
//! old file to repair, it is a file this build cannot understand, and
//! starting from defaults would quietly discard a working configuration.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::model::{
    AppConfig, BackgroundPreset, DesiredState, OutputConfig, ProjectionConfig, Settings,
    SCHEMA_VERSION,
};

const FILE_NAME: &str = "state.json";
const BACKUP_NAME: &str = "state.json.bak";
const TEMP_NAME: &str = "state.json.tmp";
/// Where a document that had to be repaired is kept, verbatim.
const REJECTED_NAME: &str = "state.json.rejected";

/// How many unknown fields one value may shed before it is called unreadable.
const MAX_DROPPED_FIELDS: usize = 64;

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
    #[error(
        "state document at {path} was written by a newer Suede (schema {found}; this build \
         understands {supported}). Upgrade Suede, or move the file aside to start from defaults."
    )]
    SchemaTooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("state document at {path} is not readable as JSON: {detail}")]
    Unreadable { path: PathBuf, detail: String },
    #[error(
        "a working copy is live: this write must carry the If-Config-Generation of the working \
         copy it replaces, or discard it first with POST /config/revert"
    )]
    WorkingCopyLive,
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

/// Everything about the file on disk, behind its own mutex.
///
/// Held only by [`StateStore::flush`], and never while `documents` is held.
struct Writer {
    /// The newest revision known to have reached disk. `None` means the file
    /// does not hold what this store holds — nothing saved yet, or a document
    /// that was repaired at load — so the next flush writes whatever it finds.
    persisted_revision: Option<u64>,
    /// The last save that failed, while memory is still ahead of disk.
    failure: Option<PersistFailure>,
}

/// A document that is live in memory but did not reach disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistFailure {
    /// The revision that could not be saved.
    pub revision: u64,
    pub detail: String,
}

pub struct StateStore {
    dir: PathBuf,
    /// Identifies this in-memory store instance. It is not a secret: it only
    /// prevents a generation counter reset at daemon restart from looking like
    /// the same working copy to an old client.
    epoch: String,
    documents: RwLock<Documents>,
    writer: Mutex<Writer>,
    /// What had to be repaired to load the document, in the operator's words.
    /// Fixed at construction, so it needs no lock.
    repairs: Vec<String>,
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
    Precondition {
        current: StateVersion,
    },
    /// A working copy is live and the transition did not name it. Every
    /// transition is conditional while somebody is mid-edit, so that one
    /// client's unconditional save cannot silently discard another's preview.
    WorkingCopy {
        current: StateVersion,
    },
    Rejected(E),
    State(StateError),
}

impl StateStore {
    /// Load persisted state, falling back to the backup and then to an empty
    /// document, repairing whatever stands between the file and a usable one.
    ///
    /// A corrupt, obsolete or invalid file is never fatal: the appliance must
    /// still boot. Only a document from a *newer* Suede is refused.
    pub fn load(dir: PathBuf) -> Result<Self, StateError> {
        std::fs::create_dir_all(&dir).map_err(|source| StateError::CreateDir {
            path: dir.clone(),
            source,
        })?;

        let primary = dir.join(FILE_NAME);
        let backup = dir.join(BACKUP_NAME);
        let mut repairs: Vec<String> = Vec::new();

        let loaded = match read_document(&primary) {
            Ok(Some(loaded)) => {
                tracing::info!(
                    revision = loaded.state.revision,
                    outputs = loaded.state.outputs.len(),
                    apps = loaded.state.apps.len(),
                    "loaded desired state"
                );
                Some(loaded)
            }
            Ok(None) => {
                tracing::info!(path = %primary.display(), "no persisted state; starting empty");
                None
            }
            Err(error @ StateError::SchemaTooNew { .. }) => return Err(error),
            Err(error) => {
                tracing::error!(%error, path = %primary.display(), "state file unreadable; trying backup");
                repairs.push(format!(
                    "{} could not be read ({error}); the backup was tried instead",
                    primary.display()
                ));
                match read_document(&backup) {
                    Ok(Some(loaded)) => {
                        tracing::warn!(
                            revision = loaded.state.revision,
                            "recovered desired state from backup"
                        );
                        Some(loaded)
                    }
                    Err(error @ StateError::SchemaTooNew { .. }) => return Err(error),
                    Ok(None) => None,
                    Err(error) => {
                        tracing::error!(%error, "backup unusable; starting from empty desired state");
                        repairs.push(format!(
                            "{} could not be read either ({error}); started from defaults",
                            backup.display()
                        ));
                        None
                    }
                }
            }
        };

        let (mut state, verbatim) = match loaded {
            Some(loaded) => {
                let verbatim = loaded.verbatim && repairs.is_empty();
                repairs.extend(loaded.repairs);
                (loaded.state, verbatim)
            }
            None => (DesiredState::new(), false),
        };

        // The original is kept whenever what is now in memory is not what the
        // file holds, so a repair can be inspected — and undone — afterwards.
        if !verbatim && primary.exists() {
            let rejected = dir.join(REJECTED_NAME);
            match std::fs::copy(&primary, &rejected) {
                Ok(_) => tracing::warn!(
                    path = %rejected.display(),
                    "kept the document as found before repairing it"
                ),
                Err(error) => {
                    tracing::error!(%error, path = %rejected.display(), "could not keep a copy of the document as found")
                }
            }
        }
        for repair in &repairs {
            tracing::error!(repair, "repaired the saved configuration while loading it");
        }

        // Whatever we booted from is the committed baseline, even a pre-flag
        // document or the empty default.
        state.committed = true;
        let revision = state.revision;

        Ok(Self {
            dir,
            epoch: fresh_epoch(),
            documents: RwLock::new(Documents {
                current: state,
                preview: None,
                generation: 0,
            }),
            writer: Mutex::new(Writer {
                persisted_revision: verbatim.then_some(revision),
                failure: None,
            }),
            repairs,
        })
    }

    /// A store that starts empty, for tests and `--mock`.
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
            writer: Mutex::new(Writer {
                persisted_revision: None,
                failure: None,
            }),
            repairs: Vec::new(),
        }
    }

    /// What had to be repaired to load this document, if anything. The
    /// reconciler turns each entry into a divergence, so a degraded
    /// configuration is visible in `/status` rather than only in the journal.
    ///
    /// Empty again once the repaired document has been saved: by then the
    /// file matches what is running, and an appliance that stays *degraded*
    /// forever over a repair somebody has already accepted teaches operators
    /// to ignore the light.
    pub fn load_repairs(&self) -> Vec<String> {
        if self.repairs.is_empty() || self.writer.lock().unwrap().persisted_revision.is_some() {
            return Vec::new();
        }
        self.repairs.clone()
    }

    /// The last save that failed while memory moved on without it.
    pub fn persist_failure(&self) -> Option<PersistFailure> {
        self.writer.lock().unwrap().failure.clone()
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

    /// Validate the expected version and swap in a new committed document,
    /// *in memory only*. The caller persists with [`Self::flush`], which is
    /// what lets an HTTP handler put the disk write on a blocking thread.
    ///
    /// `prepare` is handed the one document this transition builds on: the
    /// live working copy if there is one, else the saved document. One basis
    /// for the whole transition, so a save can never be a hybrid of the
    /// committed document and a preview-derived retention.
    ///
    /// It runs under the document write lock. It must not take another lock
    /// or touch the disk; see the module header.
    pub fn stage_replace_if<E, F>(
        &self,
        expected: StatePrecondition,
        prepare: F,
    ) -> Result<(DesiredState, StateVersion), ConditionalWriteError<E>>
    where
        F: FnOnce(&DesiredState) -> Result<DesiredState, E>,
    {
        let mut documents = self.documents.write().unwrap();
        self.check_transition(&documents, &expected)?;
        let basis = documents
            .preview
            .as_ref()
            .unwrap_or(&documents.current)
            .clone();
        let mut next = prepare(&basis).map_err(ConditionalWriteError::Rejected)?;
        next.schema_version = SCHEMA_VERSION;
        next.revision = documents.current.revision.saturating_add(1);
        next.committed = true;
        documents.current = next.clone();
        documents.preview = None;
        documents.generation = documents.generation.saturating_add(1);
        let version = self.version(&documents);
        Ok((next, version))
    }

    /// [`Self::stage_replace_if`] followed by the disk write. Blocking: only
    /// for callers that are not on an async executor.
    pub fn replace_if<E, F>(
        &self,
        expected: StatePrecondition,
        prepare: F,
    ) -> Result<(DesiredState, StateVersion), ConditionalWriteError<E>>
    where
        F: FnOnce(&DesiredState) -> Result<DesiredState, E>,
    {
        let accepted = self.stage_replace_if(expected, prepare)?;
        self.flush().map_err(ConditionalWriteError::State)?;
        Ok(accepted)
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
        self.check_transition(&documents, &expected)?;
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
        self.check_transition(&documents, &expected)?;
        documents.preview = None;
        documents.generation = documents.generation.saturating_add(1);
        let document = documents.current.clone();
        let version = self.version(&documents);
        Ok((document, version))
    }

    /// Replace the document, bump its revision, and persist it.
    ///
    /// The caller is responsible for having validated `next`. Blocking.
    pub fn replace(&self, next: DesiredState) -> Result<DesiredState, StateError> {
        self.unconditional(|_| next)
    }

    /// Apply `edit` to a copy of the document, then persist the result. Blocking.
    pub fn update<F>(&self, edit: F) -> Result<DesiredState, StateError>
    where
        F: FnOnce(&mut DesiredState),
    {
        self.unconditional(|basis| {
            let mut next = basis.clone();
            edit(&mut next);
            next
        })
    }

    fn unconditional<F>(&self, build: F) -> Result<DesiredState, StateError>
    where
        F: FnOnce(&DesiredState) -> DesiredState,
    {
        let result = self.replace_if(StatePrecondition::default(), |basis| {
            Ok::<_, std::convert::Infallible>(build(basis))
        });
        match result {
            Ok((state, _)) => Ok(state),
            Err(ConditionalWriteError::State(error)) => Err(error),
            Err(ConditionalWriteError::WorkingCopy { .. }) => Err(StateError::WorkingCopyLive),
            Err(ConditionalWriteError::Precondition { .. }) => {
                unreachable!("an empty precondition cannot be rejected")
            }
            Err(ConditionalWriteError::Rejected(never)) => match never {},
        }
    }

    /// Write the newest committed document to disk, unless a newer revision
    /// is already there.
    ///
    /// Blocking — file creation, two fsyncs, a copy and a rename. Callers on
    /// an async executor must run it through `spawn_blocking`. It takes no
    /// lock on `documents` beyond the read that copies the document out, so a
    /// slow disk never delays a reader or another transition.
    pub fn flush(&self) -> Result<(), StateError> {
        let document = {
            let documents = self.documents.read().unwrap();
            documents.current.clone()
        };
        let revision = document.revision;

        let mut writer = self.writer.lock().unwrap();
        // Another flush already wrote this document, or a later one: writing
        // again would replace a newer file with an older one.
        if writer
            .persisted_revision
            .is_some_and(|persisted| revision <= persisted)
        {
            return Ok(());
        }
        match self.persist(&document) {
            Ok(()) => {
                writer.persisted_revision = Some(revision);
                writer.failure = None;
                Ok(())
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    revision,
                    "the configuration is live but could not be saved; it will be lost on restart"
                );
                writer.failure = Some(PersistFailure {
                    revision,
                    detail: error.to_string(),
                });
                Err(error)
            }
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

    /// The precondition check every transition shares.
    ///
    /// While a working copy is live, a transition must name it: an
    /// unconditional write would otherwise discard another operator's
    /// unsaved edit without either of them seeing anything. With no working
    /// copy there is nothing to lose, so unconditional writes — a script, a
    /// `curl` — keep working exactly as before.
    fn check_transition<E>(
        &self,
        documents: &Documents,
        expected: &StatePrecondition,
    ) -> Result<(), ConditionalWriteError<E>> {
        self.check_expected(documents, expected)?;
        if documents.preview.is_some() && expected.generation.is_none() {
            return Err(ConditionalWriteError::WorkingCopy {
                current: self.version(documents),
            });
        }
        Ok(())
    }

    fn check_expected<E>(
        &self,
        documents: &Documents,
        expected: &StatePrecondition,
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

/// A document read from disk, and what it cost to read it.
struct Loaded {
    state: DesiredState,
    /// What had to be changed to make the file usable, in plain words.
    repairs: Vec<String>,
    /// True when the file holds exactly what is now in memory, so the next
    /// save may skip writing an identical document back.
    verbatim: bool,
}

/// Read a document, repairing whatever stands between the file and a usable
/// one. `Ok(None)` means the file simply does not exist.
///
/// Only two things are refused: a file that is not JSON at all (the caller
/// falls back to the backup) and a document from a newer Suede.
fn read_document(path: &Path) -> Result<Option<Loaded>, StateError> {
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

    let value: Value = serde_json::from_str(&text).map_err(|error| StateError::Unreadable {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;

    let found = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    if found > SCHEMA_VERSION {
        return Err(StateError::SchemaTooNew {
            path: path.to_path_buf(),
            found,
            supported: SCHEMA_VERSION,
        });
    }

    let mut repairs = Vec::new();
    if found < SCHEMA_VERSION {
        repairs.push(format!(
            "the document is schema {found} and this build writes schema {SCHEMA_VERSION}; \
             settings it no longer understands are replaced with defaults at the next save"
        ));
    }

    let mut state = repair_document(value, &mut repairs);
    repair_until_valid(&mut state, &mut repairs);
    state.schema_version = SCHEMA_VERSION;

    let verbatim = repairs.is_empty();
    Ok(Some(Loaded {
        state,
        repairs,
        verbatim,
    }))
}

/// Turn whatever is in the file into a document, keeping as much of it as the
/// current schema can still hold.
///
/// Three levels of granularity, cheapest first: drop individual fields this
/// build no longer knows about; drop individual list entries that cannot be
/// read; fall back to the default for a whole section. Losing one obsolete
/// output entry is a much smaller loss than losing the apps beside it.
fn repair_document(value: Value, repairs: &mut Vec<String>) -> DesiredState {
    let mut dropped = Vec::new();
    if let Some(state) = deserialize_lenient::<DesiredState>(value.clone(), &mut dropped) {
        note_dropped(dropped, "the document", repairs);
        return state;
    }

    let Value::Object(object) = value else {
        repairs.push("the saved document is not a JSON object; started from defaults".into());
        return DesiredState::new();
    };

    let mut state = DesiredState::new();
    state.revision = object.get("revision").and_then(Value::as_u64).unwrap_or(0);
    state.outputs = repair_list::<OutputConfig>(object.get("outputs"), "outputs", repairs);
    state.apps = repair_list::<AppConfig>(object.get("apps"), "apps", repairs);
    state.backgrounds =
        repair_list::<BackgroundPreset>(object.get("backgrounds"), "backgrounds", repairs);
    state.active_app = object
        .get("activeApp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    state.projection = match object.get("projection") {
        None | Some(Value::Null) => None,
        Some(projection) => {
            repair_section::<ProjectionConfig>(Some(projection), "projection", repairs)
        }
    };
    state.settings =
        repair_section::<Settings>(object.get("settings"), "settings", repairs).unwrap_or_default();
    state
}

/// Read one section, falling back as little as possible: whole, then field by
/// field onto this build's defaults, then the default section.
fn repair_section<T>(value: Option<&Value>, name: &str, repairs: &mut Vec<String>) -> Option<T>
where
    T: DeserializeOwned + serde::Serialize + Default,
{
    let value = value?;
    let mut dropped = Vec::new();
    if let Some(parsed) = deserialize_lenient::<T>(value.clone(), &mut dropped) {
        note_dropped(dropped, name, repairs);
        return Some(parsed);
    }

    // Start from what this build would write and overlay every key the file
    // still has a place for. A section written before a field existed keeps
    // everything it does say, instead of being lost whole for one omission.
    if let (Ok(Value::Object(mut base)), Value::Object(given)) =
        (serde_json::to_value(T::default()), value.clone())
    {
        let mut dropped = Vec::new();
        for (key, entry) in given {
            if base.contains_key(&key) {
                base.insert(key, entry);
            } else {
                dropped.push(key);
            }
        }
        if let Some(parsed) = deserialize_lenient::<T>(Value::Object(base), &mut dropped) {
            repairs.push(format!(
                "`{name}` was rebuilt on this build's defaults; what it still had a place for \
                 was kept"
            ));
            note_dropped(dropped, name, repairs);
            return Some(parsed);
        }
    }

    repairs.push(format!(
        "`{name}` could not be read and was replaced with the default"
    ));
    None
}

/// Read a list entry by entry, so one unreadable entry costs only itself.
fn repair_list<T: DeserializeOwned>(
    value: Option<&Value>,
    name: &str,
    repairs: &mut Vec<String>,
) -> Vec<T> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        repairs.push(format!("`{name}` is not a list; it was dropped"));
        return Vec::new();
    };
    let mut parsed = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let mut dropped = Vec::new();
        match deserialize_lenient::<T>(item.clone(), &mut dropped) {
            Some(entry) => {
                note_dropped(dropped, &format!("{name}[{index}]"), repairs);
                parsed.push(entry);
            }
            None => repairs.push(format!(
                "`{name}[{index}]` could not be read and was dropped"
            )),
        }
    }
    parsed
}

/// Deserialize, shedding fields this build no longer knows about.
///
/// The desired-state types refuse unknown fields, which is what makes a typo
/// in a write a 422 rather than a silent no-op. That strictness is right for
/// a client's write and wrong for a file the daemon has to boot from, so this
/// is the one place it is relaxed — by name, one field at a time, each one
/// reported.
fn deserialize_lenient<T: DeserializeOwned>(
    mut value: Value,
    dropped: &mut Vec<String>,
) -> Option<T> {
    for _ in 0..MAX_DROPPED_FIELDS {
        match serde_json::from_value::<T>(value.clone()) {
            Ok(parsed) => return Some(parsed),
            Err(error) => {
                let field = unknown_field(&error.to_string())?;
                if !remove_field(&mut value, &field) {
                    return None;
                }
                dropped.push(field);
            }
        }
    }
    None
}

/// The field name out of serde's "unknown field `x`, expected one of ..."
fn unknown_field(message: &str) -> Option<String> {
    const MARKER: &str = "unknown field `";
    let start = message.find(MARKER)? + MARKER.len();
    let end = message[start..].find('`')? + start;
    Some(message[start..end].to_owned())
}

/// Remove `field` from every object in the subtree, reporting whether
/// anything went. The error names the field but not where it sits, and the
/// value being repaired here is one section or one list entry, so this is as
/// narrow as the message allows.
fn remove_field(value: &mut Value, field: &str) -> bool {
    match value {
        Value::Object(map) => {
            let mut removed = map.remove(field).is_some();
            for (_, child) in map.iter_mut() {
                removed |= remove_field(child, field);
            }
            removed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |removed, item| removed | remove_field(item, field)),
        _ => false,
    }
}

fn note_dropped(mut dropped: Vec<String>, subject: &str, repairs: &mut Vec<String>) {
    if dropped.is_empty() {
        return;
    }
    dropped.sort();
    dropped.dedup();
    repairs.push(format!(
        "{subject}: dropped {} this build no longer understands: {}",
        if dropped.len() == 1 {
            "a field"
        } else {
            "fields"
        },
        dropped.join(", ")
    ));
}

/// Degrade a document until it validates, keeping as much as possible.
///
/// Validation runs with overlaps allowed, because whether an overlapping
/// layout is legitimate is a fact about how the compositor was started, which
/// this module does not know; the reconciler applies the real policy and
/// reports a divergence.
fn repair_until_valid(state: &mut DesiredState, repairs: &mut Vec<String>) {
    let limit = 16 + 2 * (state.outputs.len() + state.apps.len() + state.backgrounds.len());
    let mut coarse = 0;
    for _ in 0..limit {
        let Err(errors) = state.validate(true) else {
            return;
        };
        let error = errors.first().cloned().unwrap_or_default();
        if repair_one(state, &error, repairs) {
            continue;
        }
        // Nothing in the message named a part to drop, so give up ground in
        // deliberate order: calibration first, then the layout, then the lot.
        coarse += 1;
        match coarse {
            1 => {
                state.projection = None;
                for output in &mut state.outputs {
                    output.geometry = None;
                }
                repairs.push(format!(
                    "the projection settings could not be validated ({error}); \
                     they were reset to defaults"
                ));
            }
            2 => {
                state.outputs.clear();
                repairs.push(format!(
                    "the saved outputs could not be validated ({error}); they were dropped"
                ));
            }
            _ => {
                *state = DesiredState::new();
                repairs.push(format!(
                    "the saved configuration could not be repaired ({error}); \
                     started from defaults"
                ));
                return;
            }
        }
    }
    if state.validate(true).is_err() {
        *state = DesiredState::new();
        repairs.push("the saved configuration could not be repaired; started from defaults".into());
    }
}

/// Undo the smallest thing that could be causing `error`.
fn repair_one(state: &mut DesiredState, error: &str, repairs: &mut Vec<String>) -> bool {
    // A shared canvas the outputs cannot supply is a projection problem,
    // whichever output the message happens to name. Dropping the output
    // instead would answer "this display has no calibration" with "then you
    // have no display".
    let canvas_problem = error.contains("for a shared canvas")
        || error.contains("shared canvas rendering requires")
        || error.contains("is required in warp mode");
    if canvas_problem && state.projection.is_some() {
        state.projection = None;
        repairs.push(format!(
            "the shared canvas was given up and the projection settings reset to defaults: {error}"
        ));
        return true;
    }
    if let Some(index) = indexed(error, "outputs") {
        return repair_output(state, index, error, repairs);
    }
    if let Some(index) = indexed(error, "apps") {
        if index >= state.apps.len() {
            return false;
        }
        let id = state.apps.remove(index).id;
        repairs.push(format!("app {id:?} was dropped: {error}"));
        return true;
    }
    if let Some(index) = indexed(error, "backgrounds") {
        if index >= state.backgrounds.len() {
            return false;
        }
        let id = state.backgrounds.remove(index).id;
        repairs.push(format!("background preset {id:?} was dropped: {error}"));
        return true;
    }
    if error.starts_with("activeApp") && state.active_app.is_some() {
        let app = state.active_app.take();
        repairs.push(format!("no app is active any more ({app:?}): {error}"));
        return true;
    }
    if (error.starts_with("projection") || error.contains("shared canvas"))
        && state.projection.is_some()
    {
        state.projection = None;
        repairs.push(format!(
            "the projection settings were reset to defaults: {error}"
        ));
        return true;
    }
    if error.starts_with("layout is not contiguous")
        && state.outputs.iter().any(|output| output.position.is_some())
    {
        for output in &mut state.outputs {
            output.position = None;
        }
        repairs.push(format!(
            "the saved output positions were cleared and will be planned again: {error}"
        ));
        return true;
    }
    false
}

/// Drop the smallest part of one output entry that could explain `error`,
/// and only drop the entry itself when nothing smaller is left.
fn repair_output(
    state: &mut DesiredState,
    index: usize,
    error: &str,
    repairs: &mut Vec<String>,
) -> bool {
    let Some(output) = state.outputs.get_mut(index) else {
        return false;
    };
    let key = output.r#match.key();
    if error.contains(".background") && output.background.is_some() {
        output.background = None;
        repairs.push(format!("{key} lost its background: {error}"));
        return true;
    }
    if error.contains(".geometry") && output.geometry.is_some() {
        output.geometry = None;
        repairs.push(format!("{key} lost its saved calibration: {error}"));
        return true;
    }
    if error.contains(".scale") && output.scale.is_some() {
        output.scale = None;
        repairs.push(format!("{key} lost its configured scale: {error}"));
        return true;
    }
    if error.contains(".mode") && output.mode.is_some() {
        output.mode = None;
        repairs.push(format!("{key} lost its configured mode: {error}"));
        return true;
    }
    if output.adopted.is_some() {
        output.adopted = None;
        repairs.push(format!(
            "{key} lost the values Suede had pinned for it: {error}"
        ));
        return true;
    }
    if output.geometry.is_some() {
        output.geometry = None;
        repairs.push(format!("{key} lost its saved calibration: {error}"));
        return true;
    }
    state.outputs.remove(index);
    repairs.push(format!("output {key} was dropped: {error}"));
    true
}

/// The `i` of a `section[i]...` validation message.
fn indexed(error: &str, section: &str) -> Option<usize> {
    let rest = error.strip_prefix(section)?.strip_prefix('[')?;
    let end = rest.find(']')?;
    rest[..end].parse().ok()
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

    /// Replaces `invalid_current_schema_primary_falls_back_to_valid_backup`,
    /// which asserted that a document failing validation was thrown away in
    /// favor of the backup. An invalid document is now repaired instead:
    /// discarding a whole configuration because one calibration is degenerate
    /// loses far more than it saves, and the backup is one write older, so it
    /// is not obviously any better.
    #[test]
    fn invalid_calibration_is_repaired_rather_than_costing_the_document() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{
                "schemaVersion": 2,
                "revision": 9,
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

        let recovered = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = recovered.get();
        assert_eq!(state.revision, 9, "the document itself survives");
        assert_eq!(state.outputs.len(), 1, "so does the output entry");
        assert!(
            state.outputs[0].geometry.is_none(),
            "only the calibration that could not be validated is gone"
        );
        state
            .validate(true)
            .expect("what is loaded is always a valid document");
        assert!(
            !recovered.load_repairs().is_empty(),
            "the operator is told what was lost"
        );
        assert!(
            dir.path().join(REJECTED_NAME).exists(),
            "the file as found is kept, so the repair can be inspected"
        );

        recovered.update(|_| {}).unwrap();
        assert!(
            recovered.load_repairs().is_empty(),
            "and it stops being news once the repaired document has been saved"
        );
    }

    /// The document in `tests/fixtures/pre-warp-v1-state.json` was captured
    /// from a four-projector appliance before the warp merge: schema 1,
    /// revision 1736, no canvas and no per-output geometry. A2 in the
    /// September 21 review: upgrading such a machine must not stop the daemon.
    #[test]
    fn a_genuine_pre_warp_v1_document_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            include_str!("../tests/fixtures/pre-warp-v1-state.json"),
        )
        .unwrap();

        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = store.get();
        assert_eq!(state.revision, 1736);
        assert_eq!(state.outputs.len(), 4);
        assert_eq!(state.apps.len(), 5);
        assert_eq!(state.active_app.as_deref(), Some("arena-fx"));
        assert_eq!(
            state.outputs[0].adopted.as_ref().unwrap().mode.unwrap(),
            Mode {
                width: 1920,
                height: 1200,
                refresh_hz: 59.95
            },
            "the pinned modes of the real installation survive the upgrade"
        );
        assert_eq!(
            state.schema_version, SCHEMA_VERSION,
            "it is rewritten at the current version"
        );
        assert!(
            store
                .load_repairs()
                .iter()
                .any(|repair| repair.contains("schema 1")),
            "the version change is reported: {:?}",
            store.load_repairs()
        );
    }

    #[test]
    fn a_setting_this_build_no_longer_understands_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"schemaVersion": 2, "revision": 3,
                 "projection": {"blend": true, "blackOffset": {"mode": "dynamic"}},
                 "settings": {"hideCursor": false, "parkCursorAt": 4}}"#,
        )
        .unwrap();

        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = store.get();
        assert_eq!(state.revision, 3);
        assert!(!state.settings.hide_cursor, "the fields we do know survive");
        assert!(state.projection.is_some());
        let repairs = store.load_repairs().join(" | ");
        assert!(
            repairs.contains("blackOffset") && repairs.contains("parkCursorAt"),
            "each dropped field is named: {repairs}"
        );
    }

    #[test]
    fn a_section_that_cannot_be_read_costs_only_that_section() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"schemaVersion": 2, "revision": 5,
                 "outputs": [{"match": {"name": "HDMI-A-1"}},
                             {"match": 17},
                             {"match": {"name": "HDMI-A-2"}}],
                 "apps": [{"id": "renderer", "launcher": {"kind": "exec", "command": "true"}}],
                 "settings": "not a settings object"}"#,
        )
        .unwrap();

        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        let state = store.get();
        assert_eq!(
            state.outputs.len(),
            2,
            "only the unreadable entry is dropped"
        );
        assert_eq!(state.apps.len(), 1, "a neighboring section is untouched");
        assert_eq!(
            state.settings,
            Settings::default(),
            "the section that could not be read falls back to defaults"
        );
        let repairs = store.load_repairs().join(" | ");
        assert!(repairs.contains("outputs[1]"), "{repairs}");
        assert!(repairs.contains("settings"), "{repairs}");
    }

    #[test]
    fn missing_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let store = StateStore::load(nested.clone()).unwrap();
        store.update(|_| {}).unwrap();
        assert!(nested.join(FILE_NAME).exists());
    }

    /// The only refusal left, and it says something different from every
    /// other message here: an older document is repaired, a newer one cannot
    /// be, because this build has no idea what it would be discarding.
    #[test]
    fn a_document_from_a_newer_suede_is_refused_with_its_own_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"schemaVersion": 999, "revision": 4}"#,
        )
        .unwrap();
        let result = StateStore::load(dir.path().to_path_buf());
        let Err(error @ StateError::SchemaTooNew { found: 999, .. }) = result else {
            panic!("a newer schema must be refused");
        };
        let message = error.to_string();
        assert!(message.contains("newer Suede"), "{message}");
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
            Err(StateError::SchemaTooNew { found: 999, .. })
        ));
    }

    #[test]
    fn an_unversioned_document_loads_as_the_oldest_schema() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), br#"{"revision": 7}"#).unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        assert_eq!(store.get().revision, 7);
        assert_eq!(store.get().schema_version, SCHEMA_VERSION);
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
            |basis| Ok::<_, std::convert::Infallible>(basis.clone()),
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

    /// A3: the document lock is released before anything touches the disk,
    /// so a save that never finishes costs exactly one thread.
    #[test]
    fn a_slow_save_blocks_neither_readers_nor_further_edits() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::load(dir.path().to_path_buf()).unwrap());
        store
            .update(|state| state.settings.hide_cursor = false)
            .unwrap();

        // Stands in for a disk that has gone away mid-write: a save holding
        // everything a save can hold, for longer than anything here waits.
        let blocker = Arc::clone(&store);
        let (started, ready) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let guard = blocker.writer.lock().unwrap();
            started.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            drop(guard);
        });
        ready.recv().unwrap();

        let began = Instant::now();
        assert!(!store.effective().settings.hide_cursor);
        let (staged, _) = store
            .stage_replace_if(StatePrecondition::default(), |basis| {
                let mut next = basis.clone();
                next.settings.output_poll_interval_seconds = 9;
                Ok::<_, std::convert::Infallible>(next)
            })
            .unwrap();
        assert_eq!(staged.settings.output_poll_interval_seconds, 9);
        assert!(
            began.elapsed() < Duration::from_millis(200),
            "a reader waited for the save in flight: {:?}",
            began.elapsed()
        );

        holder.join().unwrap();
    }

    #[test]
    fn an_earlier_revision_never_overwrites_a_later_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();

        // Two accepted writes, neither saved yet: the second client's
        // response does not depend on the first client's disk write.
        for interval in [1, 2] {
            store
                .stage_replace_if(StatePrecondition::default(), |basis| {
                    let mut next = basis.clone();
                    next.settings.output_poll_interval_seconds = interval;
                    Ok::<_, std::convert::Infallible>(next)
                })
                .unwrap();
        }

        // Whichever flush runs first writes what is newest.
        store.flush().unwrap();
        let saved = StateStore::load(dir.path().to_path_buf()).unwrap().get();
        assert_eq!(saved.revision, 2);
        assert_eq!(saved.settings.output_poll_interval_seconds, 2);

        // The flush still owed by the first write must now do nothing at all.
        std::fs::remove_file(dir.path().join(FILE_NAME)).unwrap();
        store.flush().unwrap();
        assert!(
            !dir.path().join(FILE_NAME).exists(),
            "a later revision was already on disk; this flush had nothing to write"
        );
    }

    /// A3's chosen semantics, spelled out: memory wins and says so.
    #[test]
    fn a_save_that_fails_leaves_the_change_live_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        // A state directory that does not exist: every write to it fails.
        let store = StateStore::ephemeral(dir.path().join("no-such-directory"));

        let error = store
            .update(|state| state.settings.hide_cursor = false)
            .expect_err("the save cannot succeed");
        assert!(matches!(error, StateError::Write { .. }), "{error}");

        assert!(
            !store.effective().settings.hide_cursor,
            "the accepted change stays live: the outputs already show it"
        );
        let failure = store
            .persist_failure()
            .expect("an unsaved document is reported until one is saved");
        assert_eq!(failure.revision, 1);
    }

    /// A10: two operators, one of them mid-edit.
    #[test]
    fn an_unconditional_write_cannot_discard_a_live_working_copy() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::ephemeral(dir.path().to_path_buf());
        store
            .set_preview_if(StatePrecondition::default(), |basis| {
                let mut preview = basis.clone();
                preview.settings.hide_cursor = false;
                Ok::<_, std::convert::Infallible>(preview)
            })
            .unwrap();

        let error = store
            .update(|state| state.settings.output_poll_interval_seconds = 9)
            .expect_err("an unconditional write must not clobber somebody's preview");
        assert!(matches!(error, StateError::WorkingCopyLive), "{error}");
        assert!(store.has_preview());
        assert!(!store.effective().settings.hide_cursor);
    }

    #[test]
    fn a_conditional_write_builds_on_the_working_copy_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::ephemeral(dir.path().to_path_buf());
        let (_, version) = store
            .set_preview_if(StatePrecondition::default(), |basis| {
                let mut preview = basis.clone();
                preview.settings.hide_cursor = false;
                Ok::<_, std::convert::Infallible>(preview)
            })
            .unwrap();

        let (saved, _) = store
            .replace_if(
                StatePrecondition {
                    revision: Some(version.revision),
                    generation: Some(version.generation),
                    epoch: Some(version.epoch),
                },
                |basis| {
                    let mut next = basis.clone();
                    next.settings.output_poll_interval_seconds = 9;
                    Ok::<_, std::convert::Infallible>(next)
                },
            )
            .unwrap();

        assert!(
            !saved.settings.hide_cursor,
            "one basis for the whole transition: the working copy, not the saved document"
        );
        assert_eq!(saved.settings.output_poll_interval_seconds, 9);
        assert!(!store.has_preview());
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::load(dir.path().to_path_buf()).unwrap();
        store.update(|_| {}).unwrap();
        assert!(!dir.path().join(TEMP_NAME).exists());
    }
}
