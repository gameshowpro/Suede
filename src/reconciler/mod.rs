//! The reconciliation engine.
//!
//! One task owns "make live match desired". It is triggered by startup, config
//! writes, compositor events, and app exits, and it always runs the same pass —
//! which is why boot-restore, hotplug recovery, and API writes need no separate
//! code paths.

pub mod plan;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch, Mutex};

use crate::audio::AudioMonitor;
use crate::events::{EventHub, ServerEvent};
use crate::model::{Divergence, Status, SyncState, Window};
use crate::snapshot::Snapshot;
use crate::state::StateStore;
use crate::supervisor::Supervisor;
use crate::sway::{SwayClient, SwayEvent};
use crate::wallpapers::WallpaperStore;

pub use plan::{
    cursor_commands, placement_commands, plan_outputs, resolve_app_targets, AppTarget,
    AppliedOutput, Capabilities, OutputPlan,
};

/// Coalescing window for reconciliation triggers.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// How long to let Sway settle after an output is enabled or disabled.
const SETTLE: Duration = Duration::from_secs(3);
/// Cadence of supervisor housekeeping: exits, watchdogs, window placement.
const TICK: Duration = Duration::from_secs(1);

/// Handle used to ask for a reconciliation pass.
#[derive(Clone)]
pub struct ReconcileTrigger {
    sender: mpsc::Sender<&'static str>,
}

impl ReconcileTrigger {
    /// Request a pass. Never blocks: a pending request already covers this one.
    pub fn request(&self, reason: &'static str) {
        if self.sender.try_send(reason).is_err() {
            tracing::trace!(reason, "reconciliation already pending");
        }
    }
}

pub struct Reconciler {
    sway: Arc<dyn SwayClient>,
    audio: Arc<dyn AudioMonitor>,
    store: Arc<StateStore>,
    snapshot: Arc<Snapshot>,
    supervisor: Arc<Supervisor>,
    events: EventHub,
    /// Serializes passes, so two triggers cannot interleave commands.
    pass: Mutex<()>,
    /// What the last pass applied, for settings Sway does not report back.
    applied: Mutex<HashMap<String, AppliedOutput>>,
    capabilities: Mutex<Capabilities>,
    cursor_parked_at: Mutex<Option<i32>>,
    wallpapers: Arc<WallpaperStore>,
    /// Base for the documentation links attached to divergences.
    docs_base_url: String,
    /// Blend-overlay processes, one per projector output with seams.
    #[cfg(feature = "projection")]
    blend: Mutex<crate::projection::BlendManager>,
}

/// Everything a [`Reconciler`] collaborates with.
///
/// Named fields rather than a long positional argument list: four of these are
/// `Arc<dyn …>` and two are plain strings, so a transposition would compile.
pub struct ReconcilerDeps {
    pub sway: Arc<dyn SwayClient>,
    pub audio: Arc<dyn AudioMonitor>,
    pub store: Arc<StateStore>,
    pub snapshot: Arc<Snapshot>,
    pub supervisor: Arc<Supervisor>,
    pub events: EventHub,
    pub wallpapers: Arc<WallpaperStore>,
    /// Base for the documentation links attached to divergences.
    pub docs_base_url: String,
}

impl Reconciler {
    pub fn new(deps: ReconcilerDeps) -> Self {
        let ReconcilerDeps {
            sway,
            audio,
            store,
            snapshot,
            supervisor,
            events,
            wallpapers,
            docs_base_url,
        } = deps;
        Self {
            sway,
            audio,
            store,
            snapshot,
            supervisor,
            events,
            wallpapers,
            docs_base_url,
            pass: Mutex::new(()),
            applied: Mutex::new(HashMap::new()),
            capabilities: Mutex::new(Capabilities::default()),
            cursor_parked_at: Mutex::new(None),
            #[cfg(feature = "projection")]
            blend: Mutex::new(crate::projection::BlendManager::new()),
        }
    }

    /// Stop everything this reconciler spawned. Called at daemon shutdown so
    /// no blend overlay outlives the daemon that configured it.
    pub async fn shutdown(&self) {
        #[cfg(feature = "projection")]
        self.blend.lock().await.shutdown();
    }

    /// Detect version-gated compositor features. Safe to call repeatedly.
    pub async fn detect_capabilities(&self) {
        match self.sway.get_version().await {
            Ok(version) => {
                let capabilities = Capabilities {
                    supports_tearing: version.supports_tearing(),
                };
                tracing::info!(
                    version = %version.display(),
                    tearing = capabilities.supports_tearing,
                    "detected sway capabilities"
                );
                *self.capabilities.lock().await = capabilities;
            }
            Err(error) => tracing::warn!(%error, "could not determine sway version"),
        }
    }

    /// Re-query outputs, publishing an event when they changed.
    pub async fn refresh_outputs(&self) -> bool {
        match self.sway.get_outputs().await {
            Ok(outputs) => {
                if self.snapshot.set_outputs(outputs.clone()) {
                    tracing::info!(count = outputs.len(), "outputs changed");
                    self.events.publish(ServerEvent::OutputsChanged(outputs));
                    return true;
                }
                false
            }
            Err(error) => {
                tracing::warn!(%error, "failed to query outputs");
                false
            }
        }
    }

    /// Re-query windows, attributing each to the app that owns it.
    pub async fn refresh_windows(&self) -> Vec<Window> {
        let mut windows = match self.sway.get_windows().await {
            Ok(windows) => windows,
            Err(error) => {
                tracing::debug!(%error, "failed to query windows");
                return self.snapshot.windows();
            }
        };

        let ownership: HashMap<i64, String> = self
            .supervisor
            .statuses()
            .await
            .into_iter()
            .flat_map(|status| {
                let app_id = status.id.clone();
                status
                    .window_ids
                    .into_iter()
                    .map(move |id| (id, app_id.clone()))
            })
            .collect();
        for window in &mut windows {
            window.app = ownership.get(&window.id).cloned();
        }

        self.snapshot.set_windows(windows.clone());
        windows
    }

    /// Run one full reconciliation pass and return the resulting status.
    pub async fn reconcile(&self) -> Status {
        let _guard = self.pass.lock().await;

        let desired = self.store.get();
        let capabilities = *self.capabilities.lock().await;
        let mut divergences: Vec<Divergence> = Vec::new();

        let previous = self.snapshot.status();
        self.publish_status(Status {
            state: SyncState::Reconciling,
            divergences: previous.divergences,
            last_reconciled: previous.last_reconciled,
            revision: desired.revision,
        });

        self.refresh_outputs().await;

        // --- outputs ---
        let output_plan = {
            let applied = self.applied.lock().await;
            crate::reconciler::plan::plan_outputs_with(
                &self.snapshot.outputs(),
                &desired.outputs,
                &desired.backgrounds,
                &applied,
                capabilities,
                |id| {
                    self.wallpapers
                        .resolve(id)
                        .ok()
                        .map(|path| path.display().to_string())
                },
            )
        };
        divergences.extend(output_plan.divergences.iter().cloned());

        for command in &output_plan.commands {
            if let Err(error) = self.sway.run_command(command).await {
                tracing::warn!(%command, %error, "output command failed");
                divergences.push(Divergence::new(
                    "command_failed",
                    command.clone(),
                    error.to_string(),
                ));
            }
        }

        *self.applied.lock().await = output_plan.applied.clone();

        // Enabling or disabling an output rearranges the layout; let it settle
        // before anything reads geometry or places a window.
        if output_plan.topology_changed {
            tracing::debug!("waiting for output topology to settle");
            tokio::time::sleep(SETTLE).await;
            self.refresh_outputs().await;
        }

        // --- projection ---
        // After the outputs have settled: seams are derived from the observed
        // rectangles, so blending must see the layout it will actually cover.
        divergences.extend(self.sync_projection(&desired).await);

        // --- apps ---
        let targets = resolve_app_targets(
            &desired.apps,
            &self.snapshot.outputs(),
            &output_plan.workspaces,
        );
        divergences.extend(self.supervisor.reconcile(&desired.apps, &targets).await);

        // --- audio ---
        if desired.apps.iter().any(|app| {
            app.audio
                .as_ref()
                .is_some_and(|audio| audio.output.is_none())
        }) {
            if let Err(error) = self.audio.ensure_null_sink().await {
                tracing::warn!(%error, "could not create the null audio sink");
                divergences.push(Divergence::new(
                    "null_sink_unavailable",
                    crate::audio::NULL_SINK_NAME,
                    error.to_string(),
                ));
            }
        }
        divergences.extend(self.audio_divergences(&desired));

        // --- cursor ---
        if desired.settings.hide_cursor {
            self.park_cursor().await;
        }

        let windows = self.refresh_windows().await;
        self.supervisor.tick(&windows).await;

        // Every divergence gains a documentation link here, so the pure
        // planner stays free of deployment concerns and no producer can forget.
        for divergence in &mut divergences {
            divergence.docs_url = Divergence::docs_path(&divergence.kind)
                .map(|path| format!("{}/{}", self.docs_base_url.trim_end_matches('/'), path));
        }

        let status = Status {
            state: if divergences.is_empty() {
                SyncState::Synced
            } else {
                SyncState::Degraded
            },
            divergences,
            last_reconciled: Some(crate::util::unix_now()),
            revision: desired.revision,
        };
        self.publish_status(status.clone());
        status
    }

    /// Keep the blend overlays in step with the projection configuration.
    ///
    /// With blending off, absent, or the whole section missing, this passes an
    /// empty spec list — every overlay is torn down and nothing else happens,
    /// so a disabled feature costs exactly nothing per pass.
    #[cfg(feature = "projection")]
    async fn sync_projection(&self, desired: &crate::model::DesiredState) -> Vec<Divergence> {
        use crate::projection::{overlay_specs, Participant};

        let mut divergences = Vec::new();
        let mut specs = Vec::new();

        if let Some(projection) = desired.projection.as_ref().filter(|p| p.blend) {
            let observed = self.snapshot.outputs();
            let mut participants = Vec::new();
            for configured in &projection.outputs {
                match observed
                    .iter()
                    .find(|output| output.active && output.name == configured.name)
                {
                    Some(output) if output.rect.width > 0 => {
                        participants.push(Participant::from_config(configured, output.rect));
                    }
                    _ => divergences.push(Divergence::new(
                        "projection_output_not_found",
                        &configured.name,
                        format!(
                            "{} is listed for edge blending, but no active output has that name",
                            configured.name
                        ),
                    )),
                }
            }
            specs = overlay_specs(&participants);
        }

        divergences.extend(self.blend.lock().await.sync(&specs));
        divergences
    }

    /// Without the `projection` feature there is nothing to run — but asking
    /// for blending anyway must be surfaced, not silently ignored.
    #[cfg(not(feature = "projection"))]
    async fn sync_projection(&self, desired: &crate::model::DesiredState) -> Vec<Divergence> {
        if desired.projection.as_ref().is_some_and(|p| p.blend) {
            vec![Divergence::new(
                "projection_unavailable",
                "projection",
                "edge blending is configured, but this build of suede was compiled \
                 without the 'projection' feature",
            )]
        } else {
            Vec::new()
        }
    }

    /// Report apps whose configured audio sink is not currently present.
    fn audio_divergences(&self, desired: &crate::model::DesiredState) -> Vec<Divergence> {
        let available = self.audio.sinks();
        if available.is_empty() {
            // Nothing known about audio yet; do not cry wolf.
            return Vec::new();
        }
        desired
            .apps
            .iter()
            .filter(|app| app.enabled)
            .filter_map(|app| {
                let wanted = app.audio.as_ref()?.output.as_ref()?;
                if available.iter().any(|sink| sink.id == *wanted) {
                    return None;
                }
                Some(Divergence::new(
                    "audio_sink_not_present",
                    &app.id,
                    format!(
                        "{} requests audio sink {wanted}, which is not present",
                        app.id
                    ),
                ))
            })
            .collect()
    }

    async fn park_cursor(&self) {
        let height = self.snapshot.layout_height();
        let mut parked = self.cursor_parked_at.lock().await;
        if *parked == Some(height) {
            return;
        }
        for command in cursor_commands(height) {
            if let Err(error) = self.sway.run_command(&command).await {
                tracing::debug!(%command, %error, "cursor command failed");
                return;
            }
        }
        *parked = Some(height);
    }

    fn publish_status(&self, status: Status) {
        if self.snapshot.set_status(status.clone()) {
            self.events
                .publish(ServerEvent::StatusChanged(Box::new(status)));
        }
    }

    /// Create the trigger channel and the receiver the task services.
    pub fn channel() -> (ReconcileTrigger, mpsc::Receiver<&'static str>) {
        let (sender, receiver) = mpsc::channel(1);
        (ReconcileTrigger { sender }, receiver)
    }

    /// The reconciliation task: react to triggers, poll as a backstop, and tick
    /// the supervisor.
    pub async fn run(
        self: Arc<Self>,
        mut triggers: mpsc::Receiver<&'static str>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        self.detect_capabilities().await;
        self.reconcile().await;

        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let poll_seconds = self
            .store
            .get()
            .settings
            .output_poll_interval_seconds
            .max(1);
        let mut poll = tokio::time::interval(Duration::from_secs(poll_seconds));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    tracing::info!("reconciler stopping");
                    return;
                }
                Some(reason) = triggers.recv() => {
                    // Coalesce a burst of triggers into a single pass.
                    tokio::time::sleep(DEBOUNCE).await;
                    while triggers.try_recv().is_ok() {}
                    tracing::debug!(reason, "reconciling");
                    self.reconcile().await;
                }
                _ = tick.tick() => {
                    let windows = self.refresh_windows().await;
                    self.supervisor.tick(&windows).await;
                }
                _ = poll.tick() => {
                    // Backstop in case an event was missed or sway restarted.
                    if self.refresh_outputs().await {
                        self.reconcile().await;
                    }
                }
            }
        }
    }

    /// Forward compositor events into reconciliation triggers and SSE.
    pub async fn forward_sway_events(
        sway: Arc<dyn SwayClient>,
        events: EventHub,
        trigger: ReconcileTrigger,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut receiver = sway.subscribe();
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                received = receiver.recv() => match received {
                    Ok(SwayEvent::OutputsMayHaveChanged) => {
                        trigger.request("output event");
                    }
                    Ok(SwayEvent::Window { change, window }) => {
                        events.publish(ServerEvent::WindowsChanged(Box::new(
                            crate::model::WindowChange { change: change.clone(), window },
                        )));
                        // A new window may be one we are waiting to place.
                        if change == "new" || change == "close" {
                            trigger.request("window event");
                        }
                    }
                    Ok(SwayEvent::Shutdown) => {
                        tracing::warn!("sway is shutting down");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "fell behind sway events; resyncing");
                        trigger.request("event lag");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }

    /// Forward audio changes into events and a reconciliation trigger.
    pub async fn forward_audio_events(
        audio: Arc<dyn AudioMonitor>,
        events: EventHub,
        trigger: ReconcileTrigger,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut receiver = audio.subscribe();
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                received = receiver.recv() => match received {
                    Ok(sinks) => {
                        events.publish(ServerEvent::AudioOutputsChanged(sinks));
                        trigger.request("audio change");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mock::MockAudio;
    use crate::model::{
        AppConfig, AudioConfig, Launcher, Mode, OutputConfig, OutputMatch, Position, RestartPolicy,
    };
    use crate::supervisor::LaunchContext;
    use crate::sway::mock::MockSway;

    struct Harness {
        reconciler: Arc<Reconciler>,
        sway: Arc<MockSway>,
        audio: Arc<MockAudio>,
        store: Arc<StateStore>,
        snapshot: Arc<Snapshot>,
        supervisor: Arc<Supervisor>,
        events: EventHub,
        _dir: tempfile::TempDir,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let sway = Arc::new(MockSway::with_fixtures());
        let audio = Arc::new(MockAudio::with_sinks());
        let store = Arc::new(StateStore::ephemeral(dir.path().to_path_buf()));
        let snapshot = Arc::new(Snapshot::new());
        let events = EventHub::new();
        let supervisor = Arc::new(Supervisor::new(
            sway.clone(),
            events.clone(),
            LaunchContext {
                profiles_root: dir.path().join("profiles"),
                log_root: dir.path().join("logs"),
                api_base: "http://127.0.0.1:9088/api/v1".into(),
            },
        ));
        let reconciler = Arc::new(Reconciler::new(ReconcilerDeps {
            sway: sway.clone(),
            audio: audio.clone(),
            store: store.clone(),
            snapshot: snapshot.clone(),
            supervisor: supervisor.clone(),
            events: events.clone(),
            wallpapers: Arc::new(WallpaperStore::new(dir.path().join("wallpapers"))),
            docs_base_url: "https://suede.gameshow.pro/".to_string(),
        }));
        Harness {
            reconciler,
            sway,
            audio,
            store,
            snapshot,
            supervisor,
            events,
            _dir: dir,
        }
    }

    fn configured_output(name: &str, x: i32) -> OutputConfig {
        let mut config = OutputConfig::new(OutputMatch::by_name(name));
        config.mode = Some(Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60.0,
        });
        config.position = Some(Position { x, y: 0 });
        config
    }

    fn app(id: &str, output: Option<&str>, audio: Option<AudioConfig>) -> AppConfig {
        AppConfig {
            id: id.into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "sleep".into(),
                args: vec!["30".into()],
            },
            output: output.map(OutputMatch::by_name),
            fullscreen: true,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        }
    }

    #[tokio::test]
    async fn empty_desired_state_issues_no_output_commands() {
        let harness = harness();
        harness.reconciler.detect_capabilities().await;
        harness
            .store
            .update(|state| state.settings.hide_cursor = false)
            .unwrap();

        let status = harness.reconciler.reconcile().await;
        assert_eq!(status.state, SyncState::Synced);
        assert!(
            harness.sway.commands().is_empty(),
            "unexpected commands: {:?}",
            harness.sway.commands()
        );
    }

    #[tokio::test]
    async fn observed_outputs_populate_the_snapshot() {
        let harness = harness();
        harness.reconciler.reconcile().await;
        assert_eq!(harness.snapshot.outputs().len(), 3);
    }

    #[tokio::test]
    async fn configures_an_inactive_output() {
        let harness = harness();
        harness.reconciler.detect_capabilities().await;
        harness
            .store
            .update(|state| state.outputs.push(configured_output("HDMI-A-3", 0)))
            .unwrap();

        harness.reconciler.reconcile().await;
        assert!(harness
            .sway
            .ran_command_containing("output HDMI-A-3 enable"));
    }

    #[tokio::test]
    async fn a_satisfied_configuration_is_left_alone_on_the_second_pass() {
        let harness = harness();
        harness.reconciler.detect_capabilities().await;
        harness
            .store
            .update(|state| {
                state.outputs.push(configured_output("HDMI-A-1", 0));
                state.settings.hide_cursor = false;
            })
            .unwrap();

        harness.reconciler.reconcile().await;
        harness.sway.clear_commands();
        harness.reconciler.reconcile().await;

        assert!(
            harness.sway.commands().is_empty(),
            "second pass should be a no-op, got {:?}",
            harness.sway.commands()
        );
    }

    #[tokio::test]
    async fn missing_output_degrades_status_without_failing() {
        let harness = harness();
        harness
            .store
            .update(|state| state.outputs.push(configured_output("HDMI-A-9", 0)))
            .unwrap();

        let status = harness.reconciler.reconcile().await;
        assert_eq!(status.state, SyncState::Degraded);
        assert_eq!(status.divergences[0].kind, "output_not_connected");
    }

    #[tokio::test]
    async fn a_failed_command_is_recorded_as_a_divergence() {
        let harness = harness();
        harness.sway.fail_commands_containing("HDMI-A-3");
        harness
            .store
            .update(|state| state.outputs.push(configured_output("HDMI-A-3", 0)))
            .unwrap();

        let status = harness.reconciler.reconcile().await;
        assert_eq!(status.state, SyncState::Degraded);
        assert!(status
            .divergences
            .iter()
            .any(|d| d.kind == "command_failed"));
    }

    #[tokio::test]
    async fn cursor_is_hidden_when_configured() {
        let harness = harness();
        harness.reconciler.reconcile().await;
        assert!(harness.sway.ran_command_containing("hide_cursor"));
    }

    #[tokio::test]
    async fn cursor_is_left_alone_when_disabled() {
        let harness = harness();
        harness
            .store
            .update(|state| state.settings.hide_cursor = false)
            .unwrap();
        harness.reconciler.reconcile().await;
        assert!(!harness.sway.ran_command_containing("hide_cursor"));
    }

    #[tokio::test]
    async fn status_carries_the_applied_revision() {
        let harness = harness();
        harness.store.update(|_| {}).unwrap();
        let status = harness.reconciler.reconcile().await;
        assert_eq!(status.revision, 1);
    }

    #[tokio::test]
    async fn app_targeting_a_missing_output_is_a_divergence() {
        let harness = harness();
        harness
            .store
            .update(|state| state.apps.push(app("renderer", Some("HDMI-A-9"), None)))
            .unwrap();

        let status = harness.reconciler.reconcile().await;
        assert!(status
            .divergences
            .iter()
            .any(|d| d.kind == "app_waiting_for_output"));
        harness.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn missing_audio_sink_is_a_divergence() {
        let harness = harness();
        harness
            .store
            .update(|state| {
                state.apps.push(app(
                    "renderer",
                    None,
                    Some(AudioConfig {
                        output: Some("alsa_output.does-not-exist".into()),
                    }),
                ))
            })
            .unwrap();

        let status = harness.reconciler.reconcile().await;
        assert!(status
            .divergences
            .iter()
            .any(|d| d.kind == "audio_sink_not_present"));
        harness.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn null_routing_creates_the_null_sink_once() {
        let harness = harness();
        harness
            .store
            .update(|state| {
                state
                    .apps
                    .push(app("silent", None, Some(AudioConfig { output: None })))
            })
            .unwrap();

        harness.reconciler.reconcile().await;
        harness.reconciler.reconcile().await;
        assert_eq!(harness.audio.null_sink_creations(), 1);
        harness.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn reconciliation_publishes_status_changes() {
        let harness = harness();
        let mut receiver = harness.events.subscribe();
        harness
            .store
            .update(|state| state.outputs.push(configured_output("HDMI-A-9", 0)))
            .unwrap();
        harness.reconciler.reconcile().await;

        let mut saw_status = false;
        while let Ok(event) = receiver.try_recv() {
            if event.name() == "status_changed" {
                saw_status = true;
            }
        }
        assert!(saw_status);
    }

    #[tokio::test]
    async fn windows_are_attributed_to_their_app() {
        let harness = harness();
        let windows = harness.reconciler.refresh_windows().await;
        assert_eq!(windows.len(), 2);
        // No apps are managed, so no window is claimed.
        assert!(windows.iter().all(|window| window.app.is_none()));
    }

    #[tokio::test]
    async fn trigger_requests_never_block() {
        let (trigger, mut receiver) = Reconciler::channel();
        for _ in 0..100 {
            trigger.request("test");
        }
        assert!(receiver.try_recv().is_ok());
    }
}
