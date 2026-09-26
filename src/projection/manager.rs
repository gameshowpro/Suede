//! Keeps one blend-overlay process running per output that has seams.
//!
//! Deliberately simpler than the app supervisor: overlays are stateless and
//! instant to start, so there is no backoff, no window tracking, and no
//! restart policy — just "the set of running overlays matches the set of
//! specs". A dead overlay is respawned on the next reconciliation pass,
//! which is also what bounds the respawn rate.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use crate::events::{EventHub, ServerEvent};
use crate::model::{
    Divergence, ProjectionControlFailure, ProjectionControlOutputStatus, ProjectionControlStatus,
};
use crate::snapshot::Snapshot;

use super::blend::{OverlaySpec, SlicerSpec};
use super::control::{
    encode_control_update, read_bounded_line, ControlEvent, ControlEventKind, ControlUpdate,
    NewestMailbox, MAX_CONTROL_LINE_BYTES,
};

struct RunningOverlay {
    child: Child,
    fingerprint: u64,
}

struct RunningSlicer {
    child: Child,
    /// Only fields which require a new Wayland/capture topology are hashed.
    fingerprint: u64,
    session: String,
    desired: SlicerSpec,
    /// Whether this child can receive a complete snapshot over stdin. Auto
    /// starts optimistically, then becomes false if its capability event
    /// reports the CPU fallback; future edits then replace the child.
    live_control: bool,
    writer: Option<ControlWriter>,
    /// Translation from this child session's control sequence to the
    /// StateStore working-copy generation that produced its complete
    /// snapshot. These are different namespaces: config previews can change
    /// without a new child, and a child starts its sequence at zero.
    config_generations: Arc<Mutex<BTreeMap<u64, u64>>>,
    protocol_mismatch: Arc<std::sync::atomic::AtomicBool>,
}

/// Session-scoped state owned by the stdout reader. Keeping it together makes
/// the reader's epoch, session and config-generation gate one coherent
/// boundary instead of a loose collection of arguments.
#[derive(Clone)]
struct SlicerReaderState {
    current_epoch: Arc<Mutex<u64>>,
    epoch: u64,
    session: String,
    config_generations: Arc<Mutex<BTreeMap<u64, u64>>>,
    protocol_mismatch: Arc<std::sync::atomic::AtomicBool>,
}

/// A dedicated writer owns the blocking stdin pipe.  Reconciliation merely
/// swaps the one pending serialized snapshot in its mailbox.
struct ControlWriter {
    mailbox: NewestMailbox<Vec<u8>>,
    failures: Receiver<String>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ControlWriter {
    fn spawn(stdin: ChildStdin) -> Self {
        Self::spawn_writer(stdin)
    }

    fn spawn_writer<W: Write + Send + 'static>(mut stdin: W) -> Self {
        let mailbox: NewestMailbox<Vec<u8>> = NewestMailbox::new();
        let writer_mailbox = mailbox.clone();
        let (failure_tx, failures) = mpsc::sync_channel(1);
        let spawn_failure = failure_tx.clone();
        let thread = match std::thread::Builder::new()
            .name("slicer-control-writer".to_string())
            .spawn(move || {
                while let Some(line) = writer_mailbox.take() {
                    if let Err(error) = stdin.write_all(&line).and_then(|()| stdin.flush()) {
                        let _ = failure_tx.try_send(error.to_string());
                        writer_mailbox.close();
                        return;
                    }
                }
            }) {
            Ok(thread) => Some(thread),
            Err(error) => {
                mailbox.close();
                let _ = spawn_failure.try_send(error.to_string());
                None
            }
        };
        Self {
            mailbox,
            failures,
            thread,
        }
    }

    fn queue(&self, update: ControlUpdate) -> std::io::Result<()> {
        let line = encode_control_update(&update)?;
        self.mailbox.push(line).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "slicer control writer has stopped",
            )
        })?;
        Ok(())
    }

    fn take_failure(&self) -> Option<String> {
        self.failures.try_recv().ok()
    }

    fn close(&mut self) {
        self.mailbox.close();
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct BlendManager {
    overlays: HashMap<String, RunningOverlay>,
    /// The one slicer process for the whole installation, in canvas mode.
    slicer: Option<RunningSlicer>,
    /// Where the slicer's stdout JSON lines land, and where its absence is
    /// announced — shared with the reconciler and the API.
    snapshot: Arc<Snapshot>,
    events: EventHub,
    /// Each reader records its epoch before publishing.  Replacing this
    /// value before killing a child prevents its last stdout bytes from
    /// changing status after a fresh child has begun.
    slicer_epoch: Arc<Mutex<u64>>,
    next_generation: u64,
}

/// What a sync pass decided, before any process is touched. Pure, so the
/// decision rules are testable without spawning anything.
#[derive(Debug, PartialEq, Eq)]
struct SyncPlan {
    kill: Vec<String>,
    spawn: Vec<String>,
}

fn plan_sync(running: &HashMap<String, u64>, wanted: &HashMap<String, u64>) -> SyncPlan {
    let mut kill: Vec<String> = running
        .iter()
        .filter(|(output, fingerprint)| wanted.get(*output) != Some(fingerprint))
        .map(|(output, _)| output.clone())
        .collect();
    let mut spawn: Vec<String> = wanted
        .iter()
        .filter(|(output, fingerprint)| running.get(*output) != Some(fingerprint))
        .map(|(output, _)| output.clone())
        .collect();
    kill.sort();
    spawn.sort();
    SyncPlan { kill, spawn }
}

fn fingerprint(spec: &OverlaySpec) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(spec)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

impl BlendManager {
    pub fn new(snapshot: Arc<Snapshot>, events: EventHub) -> Self {
        Self {
            overlays: HashMap::new(),
            slicer: None,
            snapshot,
            events,
            slicer_epoch: Arc::new(Mutex::new(0)),
            next_generation: 1,
        }
    }

    /// Whether the slicer child is alive right now.
    ///
    /// `sync_slicer` already reaps it with `try_wait` before touching
    /// anything else, so this is never a guess — which is the whole point:
    /// a bench on 2026-09-15 had `GET /projection/stats` and the
    /// `output-phase` check both call a running slicer "not running"
    /// because neither had anything better than "have stats arrived" to go
    /// on, and a damage-driven frame loop reports nothing for a static
    /// page. See `ProjectionReport`.
    pub fn slicer_running(&self) -> bool {
        self.slicer.is_some()
    }

    /// The running slicer's process id, if any — only ever needed to tell
    /// "the same process" from "a fresh one" in a test, since no production
    /// caller has a use for the number itself.
    #[cfg(test)]
    pub(crate) fn slicer_pid(&self) -> Option<u32> {
        self.slicer.as_ref().map(|running| running.child.id())
    }

    fn publish_projection_report(&self) {
        self.events
            .publish(ServerEvent::ProjectionStatsChanged(Box::new(
                self.snapshot.projection_report(),
            )));
    }

    fn mark_control_requested(
        &self,
        session: &str,
        generation: u64,
        config_generation: u64,
        spec: &SlicerSpec,
    ) {
        if self.snapshot.update_projection_control(|status| {
            status.session = Some(session.to_string());
            highest_generation(&mut status.child_generation.requested, generation);
            status.config_generation.requested = Some(config_generation);
            // If the same complete control snapshot was already installed,
            // a later StateStore generation that resolves to it is applied
            // too. This is the only no-control-message fast path; an
            // in-flight newer control generation cannot satisfy it.
            if status.child_generation.applied == status.child_generation.requested {
                status.config_generation.applied = Some(config_generation);
            }
            status.requested_mode = Some(if spec_requests_warp(spec) {
                crate::model::ProjectionMode::Warp
            } else {
                crate::model::ProjectionMode::Simple
            });
        }) {
            self.publish_projection_report();
        }
    }

    fn next_control_generation(&mut self) -> Result<u64, String> {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.checked_add(1).ok_or_else(|| {
            "slicer control generation exhausted; restart the daemon to establish a new session"
                .to_string()
        })?;
        Ok(generation)
    }

    fn queue_update(&mut self, spec: SlicerSpec, config_generation: u64) -> Result<(), String> {
        let generation = self.next_control_generation()?;
        let Some(running) = self.slicer.as_ref() else {
            return Err("slicer is not running".to_string());
        };
        let session = running.session.clone();
        record_config_generation(&running.config_generations, generation, config_generation);
        running
            .writer
            .as_ref()
            .ok_or_else(|| "this slicer mode does not support live control".to_string())?
            .queue(ControlUpdate::new(
                session.clone(),
                generation,
                spec.clone(),
            ))
            .map_err(|error| error.to_string())?;
        self.mark_control_requested(&session, generation, config_generation, &spec);
        Ok(())
    }

    /// Detect a writer or protocol failure. Lifecycle status itself is
    /// recorded by the bounded stdout reader before this method runs, so a
    /// burst cannot discard accepted/applied/presented stages.
    fn poll_slicer(&mut self) -> bool {
        let Some(running) = self.slicer.as_ref() else {
            return false;
        };
        running
            .writer
            .as_ref()
            .and_then(ControlWriter::take_failure)
            .is_some()
            || running.protocol_mismatch.swap(false, Ordering::SeqCst)
    }

    fn stop_slicer(&mut self, reason: &'static str) {
        *self.slicer_epoch.lock().unwrap() += 1;
        if let Some(mut old) = self.slicer.take() {
            tracing::info!(reason, "stopping the slicer");
            if let Some(writer) = &mut old.writer {
                writer.close();
            }
            let _ = old.child.kill();
            let _ = old.child.wait();
            if let Some(writer) = &mut old.writer {
                writer.join();
            }
            self.clear_projection_stats();
        }
    }

    /// Clear the last-reported projection stats and liveness together, and
    /// tell any SSE listener, but only if something actually changed — a
    /// manager that never ran a slicer must not spam a `false`/`null` on
    /// every reconciliation pass.
    ///
    /// The two are cleared from this one place rather than separately,
    /// because a stopped slicer's last interval is stale the instant it
    /// stops: a reader that saw `running: true` with an interval left over
    /// from ten seconds ago, while the process was already dead, would be
    /// exactly the ambiguity this file exists to remove.
    fn clear_projection_stats(&self) {
        let stats_changed = self.snapshot.set_projection_stats(None);
        let running_changed = self.snapshot.set_slicer_running(false);
        // Retain diagnostic control failures, but never advertise an old
        // child's measurement or applied lift after it stops rendering.
        let lift_changed = self.snapshot.update_projection_control(|control| {
            control.black_lift = None;
        });
        if stats_changed || running_changed || lift_changed {
            self.publish_projection_report();
        }
    }

    /// Make the running overlays match `specs`. Empty specs — blending
    /// disabled, no seams, or no projection section at all — tears every
    /// overlay down and reports nothing.
    pub fn sync(&mut self, specs: &[OverlaySpec]) -> Vec<Divergence> {
        let mut divergences = Vec::new();

        // Reap exits first, so a crashed overlay counts as "not running" and
        // is respawned below rather than trusted.
        self.overlays
            .retain(|output, running| match running.child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    tracing::warn!(output, %status, "blend overlay exited; will respawn");
                    false
                }
                Err(error) => {
                    tracing::warn!(output, %error, "could not check blend overlay");
                    false
                }
            });

        let wanted: HashMap<String, u64> = specs
            .iter()
            .map(|spec| (spec.output.clone(), fingerprint(spec)))
            .collect();
        let running: HashMap<String, u64> = self
            .overlays
            .iter()
            .map(|(output, overlay)| (output.clone(), overlay.fingerprint))
            .collect();
        let plan = plan_sync(&running, &wanted);

        for output in &plan.kill {
            if let Some(mut overlay) = self.overlays.remove(output) {
                tracing::info!(output, "stopping blend overlay");
                let _ = overlay.child.kill();
                let _ = overlay.child.wait();
            }
        }

        for output in &plan.spawn {
            let spec = specs
                .iter()
                .find(|spec| &spec.output == output)
                .expect("planned spawns come from specs");
            match spawn_overlay(spec) {
                Ok(child) => {
                    tracing::info!(output, gamma = spec.gamma, "started blend overlay");
                    self.overlays.insert(
                        output.clone(),
                        RunningOverlay {
                            child,
                            fingerprint: wanted[output],
                        },
                    );
                }
                Err(error) => divergences.push(Divergence::new(
                    "blend_overlay_failed",
                    output,
                    format!("could not start the blend overlay: {error}"),
                )),
            }
        }

        divergences
    }

    /// Force the next `sync_slicer` call to respawn the slicer even if its
    /// spec is unchanged. A no-op when none is running.
    ///
    /// Cheap insurance for a teardown `sync_slicer`'s own fingerprint diff
    /// cannot see: when an output disappears and comes back under the same
    /// name, the geometry `SlicerSpec` is built from ends up identical to
    /// before, so its fingerprint does not change and an unmodified
    /// `sync_slicer` would leave the existing (dead) slicer running —
    /// exactly the four-projector-bench defect this exists to close. The
    /// reconciler calls this before `sync_slicer` on any pass whose
    /// `OutputPlan::topology_changed` is true, i.e. one that actually issued
    /// enable/disable commands, so a compositor that does not remove the
    /// `wl_output` global on its own (this one's own self-heal in
    /// `projection::slicer` is the other route, for one that does) still
    /// gets a fresh slicer bound against the outputs that exist now.
    pub fn restart_slicer(&mut self) {
        self.stop_slicer("output topology changed");
    }

    /// Make the running slicer match `spec`. `None` tears it down.
    pub fn sync_slicer(
        &mut self,
        spec: Option<&SlicerSpec>,
        config_generation: u64,
    ) -> Vec<Divergence> {
        if self.poll_slicer() {
            tracing::warn!("slicer control pipe closed; will respawn");
            self.stop_slicer("control pipe closed");
        }

        // Reap an exit first so a crashed slicer is respawned, not trusted.
        if let Some(running) = &mut self.slicer {
            let exited = match running.child.try_wait() {
                Ok(None) => false,
                Ok(Some(status)) => {
                    tracing::warn!(%status, "slicer exited; will respawn");
                    true
                }
                Err(error) => {
                    tracing::warn!(%error, "could not check the slicer");
                    true
                }
            };
            if exited {
                self.stop_slicer("slicer exited");
            }
        }

        // A capability event is published by the stdout reader, so apply its
        // Auto->CPU decision at the next reconciliation boundary. Rehash the
        // installed desired state at the same time: an unchanged spec keeps
        // its child, while the next edit has the full restart fingerprint.
        let mut restart_for_auto_cpu_update = false;
        if let Some(running) = &mut self.slicer {
            if running.desired.renderer == crate::model::Renderer::Auto
                && self.snapshot.projection_control().effective_renderer
                    == Some(crate::model::Renderer::Cpu)
                && running.live_control
            {
                restart_for_auto_cpu_update = self
                    .snapshot
                    .projection_control()
                    .child_generation
                    .requested
                    .is_some_and(|generation| generation > 0);
                running.live_control = false;
                running.fingerprint = slicer_fingerprint(&running.desired, false);
            }
        }
        if restart_for_auto_cpu_update {
            // The CPU child has no controller, so a revision queued before
            // its capability report cannot have been read. Replace it with
            // the latest desired snapshot rather than treating that queued
            // revision as installed.
            self.stop_slicer("auto selected CPU after a queued live update");
        }

        let wanted = spec.map(|spec| {
            let live = self
                .slicer
                .as_ref()
                .filter(|running| running.desired.renderer == spec.renderer)
                .map(|running| running.live_control)
                .unwrap_or_else(|| slicer_requests_live_candidate(spec));
            slicer_fingerprint(spec, live)
        });
        let running = self.slicer.as_ref().map(|s| s.fingerprint);
        if wanted == running {
            if let Some(spec) = spec {
                if !self
                    .slicer
                    .as_ref()
                    .is_some_and(|running| running.live_control)
                {
                    return Vec::new();
                }
                let session = self
                    .slicer
                    .as_ref()
                    .expect("matching slicer exists")
                    .session
                    .clone();
                let mut desired = spec.clone();
                desired.control_session = session.clone();
                let changed = self
                    .slicer
                    .as_ref()
                    .is_some_and(|current| current.desired != desired);
                if changed {
                    if let Err(error) = self.queue_update(desired.clone(), config_generation) {
                        tracing::warn!(%error, "could not queue slicer control update; will respawn");
                        self.stop_slicer("control writer stopped");
                    } else if let Some(running) = &mut self.slicer {
                        running.desired = desired;
                    }
                } else {
                    // A state transition can leave the effective projection
                    // snapshot byte-for-byte unchanged (for example, an
                    // unrelated configuration edit). Correlate that accepted
                    // working copy without inventing a control generation.
                    let control_generation = self
                        .snapshot
                        .projection_control()
                        .child_generation
                        .requested
                        .unwrap_or(0);
                    if let Some(running) = self.slicer.as_ref() {
                        // This generation may still be building. Associate
                        // its eventual applied event with the newest
                        // equivalent StateStore snapshot, rather than the
                        // earlier preview that happened to start the build.
                        record_config_generation(
                            &running.config_generations,
                            control_generation,
                            config_generation,
                        );
                    }
                    self.mark_control_requested(
                        &session,
                        control_generation,
                        config_generation,
                        spec,
                    );
                }
            }
            return Vec::new();
        }

        self.stop_slicer("configuration changed");
        let (Some(spec), Some(_)) = (spec, wanted) else {
            return Vec::new();
        };
        let live_control = slicer_requests_live_candidate(spec);
        // A replacement re-probes Auto; do not carry the old CPU child's
        // complete restart fingerprint into this live GPU candidate.
        let fingerprint = slicer_fingerprint(spec, live_control);
        // Every child needs a session so its initial capability report is
        // attributable, even when CPU deliberately has no stdin controller.
        let session = fresh_control_session();
        let mut desired = spec.clone();
        desired.control_session = session.clone();
        match spawn_internal(
            "slice",
            &serde_json::to_string(&desired).unwrap_or_default(),
            live_control,
            true,
        ) {
            Ok(mut child) => {
                tracing::info!(
                    canvas = format!("{}x{}", spec.canvas_width, spec.canvas_height),
                    slices = spec.slices.len(),
                    "started the slicer"
                );
                // Piped because `pipe_stdout` was true above; `take()` still
                // returns `None` if the platform ever refuses a pipe, in
                // which case the slicer just runs without reported stats.
                let stdin = if live_control {
                    match child.stdin.take() {
                        Some(stdin) => Some(stdin),
                        None => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return vec![Divergence::new(
                                "blend_overlay_failed",
                                "slicer",
                                "could not create the slicer control pipe",
                            )];
                        }
                    }
                } else {
                    None
                };
                let protocol_mismatch = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let config_generations =
                    Arc::new(Mutex::new(BTreeMap::from([(0, config_generation)])));
                let (epoch, control_reset) = {
                    let mut current = self.slicer_epoch.lock().unwrap();
                    *current += 1;
                    (
                        *current,
                        self.snapshot
                            .set_projection_control(ProjectionControlStatus::default()),
                    )
                };
                if control_reset {
                    self.publish_projection_report();
                }
                if let Some(stdout) = child.stdout.take() {
                    spawn_slicer_stdout_reader(
                        stdout,
                        self.snapshot.clone(),
                        self.events.clone(),
                        SlicerReaderState {
                            current_epoch: self.slicer_epoch.clone(),
                            epoch,
                            session: session.clone(),
                            config_generations: config_generations.clone(),
                            protocol_mismatch: protocol_mismatch.clone(),
                        },
                    );
                }
                self.slicer = Some(RunningSlicer {
                    child,
                    fingerprint,
                    session,
                    desired: desired.clone(),
                    live_control,
                    writer: stdin.map(ControlWriter::spawn),
                    config_generations,
                    protocol_mismatch,
                });
                // --spec is generation zero in this new session. Do not
                // enqueue a duplicate no-op revision: it would be applied
                // without a repaint and obscure the initial presentation.
                if live_control {
                    self.mark_control_requested(
                        &self.slicer.as_ref().expect("child just started").session,
                        0,
                        config_generation,
                        &desired,
                    );
                }
                Vec::new()
            }
            Err(error) => vec![Divergence::new(
                "blend_overlay_failed",
                "slicer",
                format!("could not start the slicer: {error}"),
            )],
        }
    }

    /// Kill everything. Called at daemon shutdown so no overlay outlives the
    /// configuration that asked for it.
    pub fn shutdown(&mut self) {
        for (output, mut overlay) in self.overlays.drain() {
            tracing::debug!(output, "stopping blend overlay");
            let _ = overlay.child.kill();
            let _ = overlay.child.wait();
        }
        if let Some(mut slicer) = self.slicer.take() {
            tracing::debug!("stopping the slicer");
            *self.slicer_epoch.lock().unwrap() += 1;
            if let Some(writer) = &mut slicer.writer {
                writer.close();
            }
            let _ = slicer.child.kill();
            let _ = slicer.child.wait();
            if let Some(writer) = &mut slicer.writer {
                writer.join();
            }
            self.clear_projection_stats();
        }
    }
}

#[cfg(test)]
fn update_control_status(snapshot: &Snapshot, event: ControlEvent) -> bool {
    update_control_status_for_config(snapshot, event, None)
}

/// Apply one child lifecycle event. `config_generation` is looked up while
/// the reader is still tied to the child session, so an old child's local
/// generation can never be mistaken for the StateStore generation of its
/// replacement.
fn update_control_status_for_config(
    snapshot: &Snapshot,
    event: ControlEvent,
    config_generation: Option<u64>,
) -> bool {
    snapshot.update_projection_control(|status| {
        status.session = Some(event.session);
        match event.kind {
            ControlEventKind::BlackLift { status: lift } => {
                if status
                    .child_generation
                    .applied
                    .is_none_or(|g| event.generation >= g)
                {
                    status.black_lift = lift;
                }
            }
            ControlEventKind::Capability {
                requested_renderer,
                effective_renderer,
                warp_available,
                reason,
                requested_mode,
                effective_mode,
            } => {
                status.requested_renderer = Some(requested_renderer);
                status.effective_renderer = Some(effective_renderer);
                status.warp_available = Some(warp_available);
                status.warp_reason = reason;
                // The child's own report is the one authoritative source for
                // these two fields: nothing downstream re-derives them from
                // sampling results or anything else (that duplicate
                // computation used to live in the `Applied` arm below, and
                // could disagree with what the child had just reported here).
                status.requested_mode = Some(requested_mode);
                status.effective_mode = Some(effective_mode);
            }
            ControlEventKind::Accepted => {
                highest_generation(&mut status.child_generation.accepted, event.generation);
                clear_failure_through(&mut status.last_failure, event.generation);
            }
            ControlEventKind::Built { outputs, build_ms } => {
                highest_generation(&mut status.child_generation.built, event.generation);
                set_output_generation(
                    &mut status.outputs,
                    &outputs,
                    event.generation,
                    OutputStage::Built,
                );
                status.build_ms = build_ms.or(status.build_ms);
            }
            ControlEventKind::Applied {
                outputs,
                build_ms,
                upload_ms,
                sampling_modes,
            } => {
                let advances_applied = status
                    .child_generation
                    .applied
                    .is_none_or(|previous| event.generation >= previous);
                highest_generation(&mut status.child_generation.applied, event.generation);
                if advances_applied {
                    // A pruned translation is intentionally reported as
                    // unknown rather than leaving an older correlation in
                    // place: the new render revision may differ from that
                    // older snapshot.
                    status.config_generation.applied = config_generation;
                }
                set_output_generation(
                    &mut status.outputs,
                    &outputs,
                    event.generation,
                    OutputStage::Applied,
                );
                set_output_sampling(&mut status.outputs, sampling_modes);
                status.build_ms = build_ms.or(status.build_ms);
                status.upload_ms = upload_ms.or(status.upload_ms);
                clear_failure_through(&mut status.last_failure, event.generation);
            }
            ControlEventKind::Submitted { outputs } => {
                highest_generation(&mut status.child_generation.submitted, event.generation);
                set_output_generation(
                    &mut status.outputs,
                    &outputs,
                    event.generation,
                    OutputStage::Submitted,
                );
            }
            ControlEventKind::Presented { outputs } => {
                highest_generation(&mut status.child_generation.presented, event.generation);
                set_output_generation(
                    &mut status.outputs,
                    &outputs,
                    event.generation,
                    OutputStage::Presented,
                );
            }
            ControlEventKind::Rejected { reason } => {
                if status
                    .last_failure
                    .as_ref()
                    .and_then(|failure| failure.generation)
                    .is_none_or(|generation| event.generation >= generation)
                {
                    status.last_failure = Some(ProjectionControlFailure {
                        generation: Some(event.generation),
                        reason,
                    });
                }
            }
            ControlEventKind::Closed { reason } => {
                status.last_failure = Some(ProjectionControlFailure {
                    generation: Some(event.generation),
                    reason: reason.unwrap_or_else(|| "slicer control channel closed".to_string()),
                });
            }
        }
    })
}

/// Keep only a bounded translation window. The slicer can still finish one
/// older build after a burst of coalesced updates, so preserve a generous
/// tail rather than retaining every drag sample forever. A missing entry
/// simply withholds config correlation; it never guesses a wrong one.
const MAX_CONFIG_GENERATION_MAPPINGS: usize = 256;

fn record_config_generation(
    mappings: &Arc<Mutex<BTreeMap<u64, u64>>>,
    control_generation: u64,
    config_generation: u64,
) {
    let mut mappings = mappings.lock().unwrap();
    mappings.insert(control_generation, config_generation);
    while mappings.len() > MAX_CONFIG_GENERATION_MAPPINGS {
        let Some(oldest) = mappings.keys().copied().find(|generation| *generation != 0) else {
            break;
        };
        mappings.remove(&oldest);
    }
}

fn config_generation_for(
    mappings: &Arc<Mutex<BTreeMap<u64, u64>>>,
    control_generation: u64,
) -> Option<u64> {
    mappings.lock().unwrap().get(&control_generation).copied()
}

#[derive(Clone, Copy)]
enum OutputStage {
    Built,
    Applied,
    Submitted,
    Presented,
}

fn highest_generation(slot: &mut Option<u64>, generation: u64) {
    *slot = Some(slot.unwrap_or(0).max(generation));
}

fn clear_failure_through(failure: &mut Option<ProjectionControlFailure>, generation: u64) {
    if failure
        .as_ref()
        .and_then(|failure| failure.generation)
        .is_none_or(|failed| failed <= generation)
    {
        *failure = None;
    }
}

fn set_output_generation(
    statuses: &mut Vec<ProjectionControlOutputStatus>,
    outputs: &[String],
    generation: u64,
    stage: OutputStage,
) {
    for name in outputs {
        let index = match statuses.iter().position(|status| status.name == *name) {
            Some(index) => index,
            None => {
                statuses.push(ProjectionControlOutputStatus {
                    name: name.clone(),
                    built_generation: None,
                    applied_generation: None,
                    submitted_generation: None,
                    presented_generation: None,
                    sampling_mode: None,
                });
                statuses.len() - 1
            }
        };
        let slot = match stage {
            OutputStage::Built => &mut statuses[index].built_generation,
            OutputStage::Applied => &mut statuses[index].applied_generation,
            OutputStage::Submitted => &mut statuses[index].submitted_generation,
            OutputStage::Presented => &mut statuses[index].presented_generation,
        };
        highest_generation(slot, generation);
    }
}

fn set_output_sampling(
    statuses: &mut Vec<ProjectionControlOutputStatus>,
    sampling_modes: std::collections::BTreeMap<String, crate::model::SamplingMode>,
) {
    for (name, sampling_mode) in sampling_modes {
        let index = match statuses.iter().position(|status| status.name == name) {
            Some(index) => index,
            None => {
                statuses.push(ProjectionControlOutputStatus {
                    name,
                    built_generation: None,
                    applied_generation: None,
                    submitted_generation: None,
                    presented_generation: None,
                    sampling_mode: None,
                });
                statuses.len() - 1
            }
        };
        statuses[index].sampling_mode = Some(sampling_mode);
    }
}

impl Drop for BlendManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The overlay is this same binary: one ELF on disk, per the packaging story.
fn spawn_overlay(spec: &OverlaySpec) -> std::io::Result<Child> {
    spawn_internal("blend", &serde_json::to_string(spec)?, false, false)
}

/// `pipe_stdout` is true only for the slicer: it reports `ProjectionStats` as
/// JSON lines on stdout (see `crate::projection::slicer`), which this
/// process reads back with [`spawn_stats_reader`]. Blend overlays have
/// nothing to say there, so theirs keeps inheriting the daemon's stdout —
/// piping it for no reason would just make it silently vanish.
fn spawn_internal(
    subcommand: &str,
    spec: &str,
    pipe_stdin: bool,
    pipe_stdout: bool,
) -> std::io::Result<Child> {
    let program = std::env::current_exe()?;
    let mut command = Command::new(program);
    command
        .arg(subcommand)
        .arg("--spec")
        .arg(spec)
        .stdin(if pipe_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    if pipe_stdout {
        command.stdout(Stdio::piped());
    }
    // Stderr flows to the daemon's own journal, tagged per process.
    command.spawn()
}

/// Read bounded slicer stdout records on a dedicated thread.  The epoch gate
/// is checked immediately before publishing, so a reader left briefly alive
/// by an old process can never overwrite a new child's status.
fn spawn_slicer_stdout_reader(
    stdout: ChildStdout,
    snapshot: Arc<Snapshot>,
    events: EventHub,
    state: SlicerReaderState,
) {
    let build = std::thread::Builder::new()
        .name("slicer-stats".to_string())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let line = match read_bounded_line(&mut reader, MAX_CONTROL_LINE_BYTES) {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    // An oversize line is recoverable: `read_bounded_line`
                    // already drained it up to the next newline, so the
                    // stream is realigned and the next line can be read
                    // normally. Any other error means the pipe itself is in
                    // a state this reader cannot recover from (closed,
                    // reset, or similar) — looping on it here spun the
                    // thread at 100% CPU instead of ending it; the child
                    // side of this same protocol (`warp_update.rs`) already
                    // treats only `InvalidData` as non-terminal.
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        tracing::debug!(%error, "slicer stdout reader recovered from a malformed line");
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "slicer stdout reader stopped");
                        break;
                    }
                };
                if let Ok(event) = serde_json::from_slice::<ControlEvent>(&line) {
                    if event_belongs_to_session(&event, &state.session) {
                        let current = state.current_epoch.lock().unwrap();
                        if *current != state.epoch {
                            break;
                        }
                        if event.version == super::control::CONTROL_VERSION {
                            let config_generation =
                                config_generation_for(&state.config_generations, event.generation);
                            if update_control_status_for_config(&snapshot, event, config_generation)
                            {
                                events.publish(ServerEvent::ProjectionStatsChanged(Box::new(
                                    snapshot.projection_report(),
                                )));
                            }
                        } else {
                            state.protocol_mismatch.store(true, Ordering::SeqCst);
                        }
                    } else {
                        tracing::debug!("ignored a control event from another slicer session");
                    }
                    continue;
                }
                match serde_json::from_slice::<crate::model::ProjectionStats>(&line) {
                    Ok(stats) => {
                        // A stats line only ever arrives from a live
                        // process, so this is as good a place as any to
                        // affirm liveness too — the reconciler sets it after
                        // every `sync_slicer` call, but a client that only
                        // watches events should not have to wait for the
                        // next reconciliation pass to hear it.
                        // Keep the epoch check and publication in one mutex
                        // critical section.  An old reader may publish just
                        // before a stop clears status, never after a new
                        // child has replaced it.
                        let current = state.current_epoch.lock().unwrap();
                        if *current != state.epoch {
                            break;
                        }
                        snapshot.set_projection_stats(Some(stats));
                        snapshot.set_slicer_running(true);
                        events.publish(ServerEvent::ProjectionStatsChanged(Box::new(
                            snapshot.projection_report(),
                        )));
                        drop(current);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "could not parse a slicer stdout line");
                    }
                }
            }
        });
    if let Err(error) = build {
        tracing::warn!(%error, "could not start the slicer-stats reader thread");
    }
}

fn event_belongs_to_session(event: &ControlEvent, session: &str) -> bool {
    event.session == session
}

/// CPU never has a controller. GPU and Auto start with one, including static
/// patterns; an Auto child that negotiates CPU is switched to restart-on-edit
/// at the next reconciliation boundary. `free_run` stays in the topology
/// fingerprint because it changes presentation policy.
fn slicer_requests_live_candidate(spec: &SlicerSpec) -> bool {
    spec.renderer != crate::model::Renderer::Cpu
}

/// A malformed internal geometry is still a Warp request: the slicer must
/// reject it, never advertise the resulting rectangle fallback as requested
/// Simple mode. Source rectangles are shared crop/scale mappings and work in
/// Simple, including the CPU renderer.
fn spec_requests_warp(spec: &SlicerSpec) -> bool {
    spec.slices.iter().any(|slice| {
        slice.geometry.as_ref().is_some_and(|geometry| {
            geometry
                .warp(slice.slice.width as u32, slice.slice.height as u32)
                .map_or(true, |warp| warp.is_some())
        })
    })
}

fn slicer_fingerprint(spec: &SlicerSpec, live_control: bool) -> u64 {
    if !live_control {
        let mut complete = spec.clone();
        complete.control_session.clear();
        return fingerprint_slicer_json(&complete);
    }
    slicer_topology_fingerprint(spec)
}

fn fingerprint_slicer_json(spec: &SlicerSpec) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(spec)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Hash only child-lifetime inputs on a controlled GPU path. Gamma is an
/// exception for the Gamma pattern because its canvas source is rebuilt from
/// that value; lift and geometry remain live snapshots.
fn slicer_topology_fingerprint(spec: &SlicerSpec) -> u64 {
    let mut hasher = DefaultHasher::new();
    spec.source.hash(&mut hasher);
    spec.canvas_width.hash(&mut hasher);
    spec.canvas_height.hash(&mut hasher);
    serde_json::to_string(&spec.pattern)
        .unwrap_or_default()
        .hash(&mut hasher);
    if spec.pattern == Some(crate::model::TestPattern::Gamma) {
        spec.gamma.to_bits().hash(&mut hasher);
    }
    spec.free_run.hash(&mut hasher);
    serde_json::to_string(&spec.renderer)
        .unwrap_or_default()
        .hash(&mut hasher);
    for slice in &spec.slices {
        slice.output.hash(&mut hasher);
        if spec.layout.is_some() && spec.pattern.is_none() {
            slice.slice.width.hash(&mut hasher);
            slice.slice.height.hash(&mut hasher);
        } else {
            serde_json::to_string(&slice.slice)
                .unwrap_or_default()
                .hash(&mut hasher);
            serde_json::to_string(&slice.source_rect)
                .unwrap_or_default()
                .hash(&mut hasher);
        }
    }
    if let Some(layout) = &spec.layout {
        for participant in &layout.participants {
            participant.output.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn fresh_control_session() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
    let nonce = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos:x}-{nonce:x}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, u64)]) -> HashMap<String, u64> {
        entries
            .iter()
            .map(|(name, fingerprint)| (name.to_string(), *fingerprint))
            .collect()
    }

    #[test]
    fn an_empty_want_kills_everything_and_spawns_nothing() {
        let plan = plan_sync(&map(&[("DP-1", 1), ("DP-2", 2)]), &map(&[]));
        assert_eq!(plan.kill, vec!["DP-1", "DP-2"]);
        assert!(plan.spawn.is_empty());
    }

    #[test]
    fn a_matching_state_is_left_alone() {
        let plan = plan_sync(&map(&[("DP-1", 1)]), &map(&[("DP-1", 1)]));
        assert!(plan.kill.is_empty() && plan.spawn.is_empty());
    }

    #[test]
    fn a_changed_spec_is_killed_and_respawned() {
        // The overlay draws a static image, so a new spec means a new process.
        let plan = plan_sync(&map(&[("DP-1", 1)]), &map(&[("DP-1", 2)]));
        assert_eq!(plan.kill, vec!["DP-1"]);
        assert_eq!(plan.spawn, vec!["DP-1"]);
    }

    #[test]
    fn fingerprints_track_the_spec_content() {
        use crate::model::Rect;
        let mut spec = OverlaySpec {
            output: "DP-1".into(),
            gamma: 2.2,
            black_lift: 0.0,
            rect: Rect::default(),
            source_rect: None,
            pattern: None,
            canvas_size: None,
        };
        let original = fingerprint(&spec);
        assert_eq!(original, fingerprint(&spec), "stable for equal specs");
        spec.gamma = 2.4;
        assert_ne!(original, fingerprint(&spec), "gamma changes must repaint");
    }

    // --- restart_slicer: forcing a respawn the fingerprint cannot see ----
    //
    // `sync_slicer` re-execs this very test binary as `suede slice`, which
    // does not understand that subcommand and exits (with an error)
    // virtually as soon as it starts — real enough to exercise process
    // bookkeeping, but too short-lived to stand in for a genuinely running
    // slicer across any real time gap. Every test below therefore makes its
    // two `sync_slicer`/`restart_slicer` calls back-to-back with no `.await`
    // or sleep in between, so they run in the same thread well inside the
    // minimum time a freshly exec'd process needs to even start, let alone
    // exit — unlike a reconciler-level test spanning a whole extra pass (or
    // the 3-second output-settle sleep), which cannot tell a forced restart
    // apart from the fake process simply having crashed on its own by then.

    fn minimal_slicer_spec() -> SlicerSpec {
        SlicerSpec {
            adaptive_lift: None,
            layout: None,
            coverage_rects: Vec::new(),
            highlight_overlaps: false,
            control_session: String::new(),
            source: "HEADLESS-1".to_string(),
            canvas_width: 100,
            canvas_height: 100,
            gamma: 2.2,
            black_lift: 0.0,
            pattern: None,
            free_run: false,
            renderer: crate::model::Renderer::Cpu,
            slices: Vec::new(),
        }
    }

    fn manager_for_test() -> BlendManager {
        BlendManager::new(Arc::new(Snapshot::new()), EventHub::new())
    }

    #[test]
    fn an_unforced_call_with_an_unchanged_spec_leaves_the_slicer_alone() {
        let mut manager = manager_for_test();
        let spec = minimal_slicer_spec();

        manager.sync_slicer(Some(&spec), 0);
        let pid = manager.slicer_pid().expect("must have spawned a slicer");

        manager.sync_slicer(Some(&spec), 0);
        assert_eq!(
            manager.slicer_pid(),
            Some(pid),
            "an unchanged spec must not be restarted"
        );
    }

    #[test]
    fn restart_slicer_forces_a_respawn_even_with_an_unchanged_spec() {
        // This is exactly what the reconciler calls before `sync_slicer` on
        // any pass whose `OutputPlan::topology_changed` is true — see
        // `restart_slicer`'s own doc for why the spec alone cannot be
        // trusted to notice an output that disappeared and came back under
        // the same name.
        let mut manager = manager_for_test();
        let spec = minimal_slicer_spec();

        manager.sync_slicer(Some(&spec), 0);
        let pid = manager.slicer_pid().expect("must have spawned a slicer");

        manager.restart_slicer();
        manager.sync_slicer(Some(&spec), 0);
        assert_ne!(
            manager.slicer_pid(),
            Some(pid),
            "a forced restart must respawn even an unchanged spec"
        );
    }

    #[test]
    fn restart_slicer_is_a_no_op_when_nothing_is_running() {
        let mut manager = manager_for_test();
        manager.restart_slicer();
        assert!(!manager.slicer_running());
    }

    #[test]
    fn live_gpu_edits_keep_the_child_but_cpu_edits_restart_it() {
        use super::super::blend::SliceSpec;
        use crate::model::Rect;

        let mut gpu = minimal_slicer_spec();
        gpu.renderer = crate::model::Renderer::Gpu;
        gpu.slices.push(SliceSpec {
            source_rect: None,
            output: "DP-1".to_string(),
            slice: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            geometry: None,
        });
        let gpu_fingerprint = slicer_fingerprint(&gpu, true);
        gpu.gamma = 2.4;
        gpu.black_lift = 0.1;
        assert_eq!(slicer_fingerprint(&gpu, true), gpu_fingerprint);
        gpu.slices[0].geometry = Some(crate::projection::warp::Geometry {
            corners: [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
            center: [0.5, 0.5],
        });
        assert_eq!(slicer_fingerprint(&gpu, true), gpu_fingerprint);
        gpu.slices[0].slice.x = 1;
        assert_ne!(slicer_fingerprint(&gpu, true), gpu_fingerprint);
        gpu.slices[0].slice.x = 0;
        gpu.free_run = true;
        assert_ne!(slicer_fingerprint(&gpu, true), gpu_fingerprint);
        assert!(slicer_requests_live_candidate(&gpu));
        gpu.pattern = Some(crate::model::TestPattern::Sync);
        assert!(slicer_requests_live_candidate(&gpu));
        gpu.pattern = Some(crate::model::TestPattern::Grid);
        assert!(slicer_requests_live_candidate(&gpu));

        let mut cpu = minimal_slicer_spec();
        let cpu_fingerprint = slicer_fingerprint(&cpu, false);
        cpu.gamma = 2.4;
        assert_ne!(slicer_fingerprint(&cpu, false), cpu_fingerprint);
    }

    /// With a layout set (the arrangement endpoint's shared-canvas path) and
    /// no pattern, `slicer_topology_fingerprint` hashes only each slice's
    /// raster width/height, not its `source_rect` or `source.x`/`source.y`.
    /// So moving a source — exactly what a grid arrangement PUT does — is a
    /// live control update on the GPU path (`live_control: true`), never a
    /// restart, even though the CPU path (`live_control: false`, which
    /// always restarts on any geometry change) still sees it.
    #[test]
    fn a_layout_source_move_is_live_on_gpu_but_restarts_the_cpu_path() {
        use crate::model::{CanvasRect, Rect};
        use crate::projection::blend::SliceSpec;
        use crate::projection::layout::{LayoutParticipant, LayoutSpec};

        fn spec_at(x: i32, y: i32, source_rect: [f64; 4]) -> SlicerSpec {
            let mut spec = minimal_slicer_spec();
            spec.slices.push(SliceSpec {
                output: "DP-1".into(),
                slice: Rect {
                    x,
                    y,
                    width: 100,
                    height: 100,
                },
                source_rect: Some(source_rect),
                geometry: None,
            });
            let participant_source = CanvasRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            };
            spec.layout = Some(LayoutSpec {
                aspect: 1.0,
                blend: true,
                participants: vec![LayoutParticipant {
                    output: "DP-1".into(),
                    slice: participant_source,
                    raster_footprint: participant_source,
                }],
            });
            spec
        }

        let a = spec_at(0, 0, [0.0, 0.0, 100.0, 100.0]);
        let b = spec_at(10, 20, [10.0, 20.0, 100.0, 100.0]);

        assert_eq!(
            slicer_fingerprint(&a, true),
            slicer_fingerprint(&b, true),
            "a source move under a layout is a live control update, not a restart"
        );
        assert_ne!(
            slicer_fingerprint(&a, false),
            slicer_fingerprint(&b, false),
            "the CPU path restarts on any geometry change, by design"
        );
    }

    #[test]
    fn generation_exhaustion_never_repeats_a_generation() {
        let mut manager = manager_for_test();
        manager.next_generation = u64::MAX;
        assert!(manager.next_control_generation().is_err());
        assert_eq!(manager.next_generation, u64::MAX);
    }

    #[test]
    fn broken_control_pipe_is_reported_by_the_writer() {
        struct BrokenPipe;
        impl Write for BrokenPipe {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = ControlWriter::spawn_writer(BrokenPipe);
        writer.mailbox.push(vec![b'x', b'\n']).unwrap();
        let failure = writer
            .failures
            .recv_timeout(std::time::Duration::from_secs(5));
        writer.join();
        assert!(failure.is_ok(), "broken pipe must report a failure");
        assert!(
            writer.mailbox.push(vec![b'y']).is_err(),
            "failed writer must reject future updates"
        );
    }

    #[test]
    fn blocked_writer_keeps_only_the_newest_complete_pending_line() {
        struct BlockedWriter {
            lines: mpsc::SyncSender<Vec<u8>>,
            permits: Receiver<()>,
        }
        impl Write for BlockedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.lines.send(bytes.to_vec()).unwrap();
                self.permits.recv().unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (lines, received) = mpsc::sync_channel(1);
        let (release, permits) = mpsc::sync_channel(1);
        let mut writer = ControlWriter::spawn_writer(BlockedWriter { lines, permits });
        writer.mailbox.push(b"first\n".to_vec()).unwrap();
        assert_eq!(
            received
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            b"first\n"
        );
        // The writer is blocked in write_all. These calls must still return,
        // and each replaces the previous whole line rather than a byte suffix.
        for i in 0..100 {
            writer.mailbox.push(format!("{i}\n").into_bytes()).unwrap();
        }
        release.send(()).unwrap();
        assert_eq!(
            received
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            b"99\n"
        );
        writer.close();
        release.send(()).unwrap();
        writer.join();
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn newer_control_status_replaces_an_older_failure() {
        let manager = manager_for_test();
        update_control_status(
            &manager.snapshot,
            ControlEvent::new(
                "current".to_string(),
                3,
                ControlEventKind::Rejected {
                    reason: "bad geometry".to_string(),
                },
            ),
        );
        assert!(manager.snapshot.projection_control().last_failure.is_some());
        update_control_status(
            &manager.snapshot,
            ControlEvent::new(
                "current".to_string(),
                4,
                ControlEventKind::Applied {
                    outputs: vec!["DP-1".to_string()],
                    build_ms: Some(2.0),
                    upload_ms: Some(1.0),
                    sampling_modes: std::collections::BTreeMap::new(),
                },
            ),
        );
        let status = manager.snapshot.projection_control();
        assert_eq!(status.child_generation.applied, Some(4));
        assert!(status.last_failure.is_none());
    }

    #[test]
    fn config_generation_stays_pending_until_the_current_session_applies_it() {
        let manager = manager_for_test();
        let spec = minimal_slicer_spec();

        manager.mark_control_requested("current", 7, 41, &spec);
        update_control_status_for_config(
            &manager.snapshot,
            ControlEvent::new("current".into(), 7, ControlEventKind::Accepted),
            Some(41),
        );
        let pending = manager.snapshot.projection_control();
        assert_eq!(pending.config_generation.requested, Some(41));
        assert_eq!(pending.config_generation.applied, None);

        update_control_status_for_config(
            &manager.snapshot,
            ControlEvent::new(
                "current".into(),
                7,
                ControlEventKind::Applied {
                    outputs: vec!["DP-1".into()],
                    build_ms: None,
                    upload_ms: None,
                    sampling_modes: std::collections::BTreeMap::new(),
                },
            ),
            Some(41),
        );
        assert_eq!(
            manager
                .snapshot
                .projection_control()
                .config_generation
                .applied,
            Some(41)
        );

        // A new StateStore generation with the same installed control
        // snapshot is applied immediately, without pretending that its
        // working-copy number is a slicer control number.
        manager.mark_control_requested("current", 7, 42, &spec);
        let unchanged = manager.snapshot.projection_control();
        assert_eq!(unchanged.config_generation.requested, Some(42));
        assert_eq!(unchanged.config_generation.applied, Some(42));
    }

    #[test]
    fn stale_session_cannot_advance_config_correlation() {
        let manager = manager_for_test();
        manager.snapshot.update_projection_control(|status| {
            status.session = Some("current".into());
            status.config_generation.requested = Some(12);
        });
        let old = ControlEvent::new(
            "old".into(),
            7,
            ControlEventKind::Applied {
                outputs: vec!["DP-1".into()],
                build_ms: None,
                upload_ms: None,
                sampling_modes: std::collections::BTreeMap::new(),
            },
        );
        if event_belongs_to_session(&old, "current") {
            update_control_status_for_config(&manager.snapshot, old, Some(99));
        }
        let status = manager.snapshot.projection_control();
        assert_eq!(status.config_generation.requested, Some(12));
        assert_eq!(status.config_generation.applied, None);
    }

    #[test]
    fn old_control_generation_cannot_regress_config_correlation() {
        let manager = manager_for_test();
        manager.mark_control_requested("current", 8, 80, &minimal_slicer_spec());
        update_control_status_for_config(
            &manager.snapshot,
            ControlEvent::new(
                "current".into(),
                8,
                ControlEventKind::Applied {
                    outputs: vec!["DP-1".into()],
                    build_ms: None,
                    upload_ms: None,
                    sampling_modes: std::collections::BTreeMap::new(),
                },
            ),
            Some(80),
        );
        manager.mark_control_requested("current", 9, 90, &minimal_slicer_spec());

        update_control_status_for_config(
            &manager.snapshot,
            ControlEvent::new(
                "current".into(),
                7,
                ControlEventKind::Applied {
                    outputs: vec!["DP-1".into()],
                    build_ms: None,
                    upload_ms: None,
                    sampling_modes: std::collections::BTreeMap::new(),
                },
            ),
            Some(70),
        );
        let status = manager.snapshot.projection_control();
        assert_eq!(status.config_generation.requested, Some(90));
        assert_eq!(status.config_generation.applied, Some(80));
    }

    #[test]
    fn equivalent_pending_snapshot_uses_its_newest_config_generation() {
        let mappings = Arc::new(Mutex::new(BTreeMap::from([(0, 0)])));
        record_config_generation(&mappings, 7, 41);
        // A no-op reconcile can observe a newer working-copy generation
        // before control generation 7 has reached `applied`.
        record_config_generation(&mappings, 7, 42);
        assert_eq!(config_generation_for(&mappings, 7), Some(42));
    }

    #[test]
    fn capability_status_records_the_negotiated_cpu_fallback() {
        let manager = manager_for_test();
        update_control_status(
            &manager.snapshot,
            ControlEvent::new(
                "cpu-session".into(),
                0,
                ControlEventKind::Capability {
                    requested_renderer: crate::model::Renderer::Auto,
                    effective_renderer: crate::model::Renderer::Cpu,
                    warp_available: false,
                    reason: Some(
                        "dmabuf linear filtering is unavailable; install the driver update".into(),
                    ),
                    requested_mode: crate::model::ProjectionMode::Warp,
                    effective_mode: crate::model::ProjectionMode::Simple,
                },
            ),
        );
        let status = manager.snapshot.projection_control();
        assert_eq!(status.session.as_deref(), Some("cpu-session"));
        assert_eq!(
            status.requested_renderer,
            Some(crate::model::Renderer::Auto)
        );
        assert_eq!(status.effective_renderer, Some(crate::model::Renderer::Cpu));
        assert_eq!(status.warp_available, Some(false));
        assert_eq!(
            status.requested_mode,
            Some(crate::model::ProjectionMode::Warp)
        );
        assert_eq!(
            status.effective_mode,
            Some(crate::model::ProjectionMode::Simple)
        );
        assert!(status
            .warp_reason
            .as_deref()
            .unwrap()
            .contains("driver update"));
    }

    #[test]
    fn gamma_pattern_changes_topology_but_other_live_patterns_do_not() {
        let mut spec = minimal_slicer_spec();
        spec.renderer = crate::model::Renderer::Gpu;
        spec.pattern = Some(crate::model::TestPattern::Grid);
        let grid = slicer_fingerprint(&spec, true);
        spec.gamma = 2.4;
        assert_eq!(slicer_fingerprint(&spec, true), grid);
        spec.pattern = Some(crate::model::TestPattern::Gamma);
        let gamma = slicer_fingerprint(&spec, true);
        spec.gamma = 2.2;
        assert_ne!(slicer_fingerprint(&spec, true), gamma);
    }

    #[test]
    fn bilinear_simple_crop_does_not_change_effective_mode() {
        let manager = manager_for_test();
        update_control_status(
            &manager.snapshot,
            ControlEvent::new(
                "session".into(),
                0,
                ControlEventKind::Capability {
                    requested_renderer: crate::model::Renderer::Gpu,
                    effective_renderer: crate::model::Renderer::Gpu,
                    warp_available: true,
                    reason: Some("filtering verified".into()),
                    requested_mode: crate::model::ProjectionMode::Simple,
                    effective_mode: crate::model::ProjectionMode::Simple,
                },
            ),
        );
        update_control_status(
            &manager.snapshot,
            ControlEvent::new(
                "session".into(),
                1,
                ControlEventKind::Applied {
                    outputs: vec!["DP-1".into()],
                    build_ms: None,
                    upload_ms: None,
                    sampling_modes: std::collections::BTreeMap::from([(
                        "DP-1".into(),
                        crate::model::SamplingMode::Bilinear,
                    )]),
                },
            ),
        );
        let status = manager.snapshot.projection_control();
        assert_eq!(
            status.effective_mode,
            Some(crate::model::ProjectionMode::Simple)
        );
        assert_eq!(status.warp_reason.as_deref(), Some("filtering verified"));
    }

    #[test]
    fn shared_simple_layout_remains_simple_when_every_applied_sampler_is_exact() {
        use crate::model::Rect;
        use crate::projection::blend::SliceSpec;

        let manager = manager_for_test();
        let mut spec = minimal_slicer_spec();
        spec.renderer = crate::model::Renderer::Gpu;
        spec.slices.push(SliceSpec {
            output: "DP-1".into(),
            slice: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            source_rect: Some([0.0, 0.0, 100.0, 100.0]),
            geometry: None,
        });
        let source = crate::model::CanvasRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        spec.layout = Some(crate::projection::layout::LayoutSpec {
            aspect: 1.0,
            blend: true,
            participants: vec![crate::projection::layout::LayoutParticipant {
                output: "DP-1".into(),
                slice: source,
                raster_footprint: source,
            }],
        });
        assert!(
            super::super::warp_update::sampling_warp(&spec, &spec.slices[0], (100, 100))
                .unwrap()
                .is_none()
        );
        manager.mark_control_requested("public-layout", 0, 0, &spec);
        assert_eq!(
            manager.snapshot.projection_control().requested_mode,
            Some(crate::model::ProjectionMode::Simple)
        );
        update_control_status(
            &manager.snapshot,
            ControlEvent::new(
                "public-layout".into(),
                0,
                ControlEventKind::Capability {
                    requested_renderer: crate::model::Renderer::Gpu,
                    effective_renderer: crate::model::Renderer::Gpu,
                    warp_available: true,
                    reason: None,
                    requested_mode: crate::model::ProjectionMode::Simple,
                    effective_mode: crate::model::ProjectionMode::Simple,
                },
            ),
        );
        update_control_status(
            &manager.snapshot,
            ControlEvent::new(
                "public-layout".into(),
                0,
                ControlEventKind::Applied {
                    outputs: vec!["DP-1".into()],
                    build_ms: None,
                    upload_ms: None,
                    sampling_modes: std::collections::BTreeMap::from([(
                        "DP-1".into(),
                        crate::model::SamplingMode::Exact,
                    )]),
                },
            ),
        );
        let status = manager.snapshot.projection_control();
        assert_eq!(
            status.effective_mode,
            Some(crate::model::ProjectionMode::Simple)
        );
        assert_eq!(
            status.requested_mode,
            Some(crate::model::ProjectionMode::Simple)
        );
        assert_eq!(status.warp_available, Some(true));
        assert_eq!(
            status.outputs[0].sampling_mode,
            Some(crate::model::SamplingMode::Exact)
        );
    }

    #[cfg(unix)]
    #[test]
    fn auto_cpu_fallback_replays_an_edit_queued_before_capability() {
        let mut manager = manager_for_test();
        let mut spec = minimal_slicer_spec();
        spec.renderer = crate::model::Renderer::Auto;
        spec.control_session = "before-capability".into();
        // A controlled pipe consumer stays alive until the manager kills it;
        // no shell timeout or scheduling assumption stands in for a child.
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let writer = ControlWriter::spawn(child.stdin.take().unwrap());
        manager.slicer = Some(RunningSlicer {
            child,
            fingerprint: slicer_fingerprint(&spec, true),
            session: spec.control_session.clone(),
            desired: spec.clone(),
            live_control: true,
            writer: Some(writer),
            config_generations: Arc::new(Mutex::new(BTreeMap::from([(0, 0)]))),
            protocol_mismatch: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        manager.snapshot.update_projection_control(|status| {
            status.session = Some(spec.control_session.clone());
            status.child_generation.requested = Some(0);
        });
        spec.gamma = 2.4;
        assert!(manager.sync_slicer(Some(&spec), 1).is_empty());
        assert_eq!(manager.slicer_pid(), Some(pid));
        assert_eq!(
            manager
                .snapshot
                .projection_control()
                .child_generation
                .requested,
            Some(1)
        );
        manager.snapshot.update_projection_control(|status| {
            status.effective_renderer = Some(crate::model::Renderer::Cpu);
        });
        assert!(manager.sync_slicer(Some(&spec), 1).is_empty());
        let replacement = manager.slicer.as_ref().unwrap();
        assert_ne!(replacement.child.id(), pid);
        assert_eq!(replacement.desired.gamma, 2.4);
        assert!(
            replacement.live_control && replacement.writer.is_some(),
            "new Auto child must be able to recover GPU live control"
        );
        assert_eq!(replacement.fingerprint, slicer_fingerprint(&spec, true));
        assert_eq!(
            manager
                .snapshot
                .projection_control()
                .child_generation
                .requested,
            Some(0)
        );
    }

    #[test]
    fn burst_lifecycle_reports_retain_every_stage() {
        let manager = manager_for_test();
        for kind in [
            ControlEventKind::Accepted,
            ControlEventKind::Built {
                outputs: vec!["DP-1".to_string()],
                build_ms: Some(3.0),
            },
            ControlEventKind::Applied {
                outputs: vec!["DP-1".to_string()],
                build_ms: Some(3.0),
                upload_ms: Some(1.0),
                sampling_modes: std::collections::BTreeMap::from([(
                    "DP-1".to_string(),
                    crate::model::SamplingMode::Exact,
                )]),
            },
            ControlEventKind::Submitted {
                outputs: vec!["DP-1".to_string()],
            },
            ControlEventKind::Presented {
                outputs: vec!["DP-1".to_string()],
            },
        ] {
            update_control_status(
                &manager.snapshot,
                ControlEvent::new("session".into(), 7, kind),
            );
        }
        let status = manager.snapshot.projection_control();
        assert_eq!(status.child_generation.accepted, Some(7));
        assert_eq!(status.child_generation.built, Some(7));
        assert_eq!(status.child_generation.applied, Some(7));
        assert_eq!(status.child_generation.submitted, Some(7));
        assert_eq!(status.child_generation.presented, Some(7));
        assert_eq!(
            status.outputs[0].sampling_mode,
            Some(crate::model::SamplingMode::Exact)
        );
    }

    #[test]
    fn stale_session_events_are_ignored_before_status_mutation() {
        let manager = manager_for_test();
        let old = ControlEvent::new("old".into(), 9, ControlEventKind::Accepted);
        assert!(!event_belongs_to_session(&old, "current"));
        if event_belongs_to_session(&old, "current") {
            update_control_status(&manager.snapshot, old);
        }
        assert!(manager
            .snapshot
            .projection_control()
            .child_generation
            .accepted
            .is_none());
    }

    #[test]
    fn a_new_session_does_not_inherit_old_output_status() {
        let manager = manager_for_test();
        update_control_status(
            &manager.snapshot,
            ControlEvent::new(
                "old".into(),
                4,
                ControlEventKind::Presented {
                    outputs: vec!["removed-output".into()],
                },
            ),
        );
        assert!(!manager.snapshot.projection_control().outputs.is_empty());
        manager
            .snapshot
            .set_projection_control(ProjectionControlStatus::default());
        let reset = manager.snapshot.projection_control();
        assert!(reset.outputs.is_empty());
        assert!(reset.child_generation.presented.is_none());
    }
}
