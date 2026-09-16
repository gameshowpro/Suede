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
    adoption_for, cursor_commands, placement_commands, plan_outputs, resolve_app_targets,
    AppTarget, AppliedOutput, Capabilities, OutputPlan,
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
    /// What the last pass observed on each live output, keyed the same way as
    /// `applied`. Adoption only pins a value once it has been observed twice
    /// in a row — a display still negotiating its link can report something
    /// on the first pass that is not its final answer — and this is that
    /// memory. See [`plan::adoption_for`].
    previous_observations: Mutex<HashMap<String, crate::model::Output>>,
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
        // Built before the struct literal below moves `snapshot` and
        // `events` into their own fields.
        #[cfg(feature = "projection")]
        let blend = Mutex::new(crate::projection::BlendManager::new(
            snapshot.clone(),
            events.clone(),
        ));
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
            previous_observations: Mutex::new(HashMap::new()),
            capabilities: Mutex::new(Capabilities::default()),
            cursor_parked_at: Mutex::new(None),
            #[cfg(feature = "projection")]
            blend,
        }
    }

    /// Stop everything this reconciler spawned. Called at daemon shutdown so
    /// no blend overlay outlives the daemon that configured it.
    pub async fn shutdown(&self) {
        #[cfg(feature = "projection")]
        self.blend.lock().await.shutdown();
    }

    /// The running slicer's process id, for tests that need to tell whether
    /// a pass restarted it — see `BlendManager::slicer_pid`.
    #[cfg(all(test, feature = "projection"))]
    async fn slicer_pid(&self) -> Option<u32> {
        self.blend.lock().await.slicer_pid()
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

        // The effective document: a live preview when one is being tried out
        // in the UI, otherwise the persisted state.
        let desired = self.store.effective();
        let capabilities = *self.capabilities.lock().await;
        let mut divergences: Vec<Divergence> = Vec::new();

        let previous = self.snapshot.status();
        self.publish_status(Status {
            state: SyncState::Reconciling,
            divergences: previous.divergences,
            last_reconciled: previous.last_reconciled,
            revision: desired.revision,
            committed: !self.store.has_preview(),
            current_revision: self.store.revision(),
            // The reconciler holds no `CheckRunner` — checks shell out to
            // other programs, and a pass must stay cheap — so it cannot
            // honestly fill this in. `GET /status` folds in the runner's
            // last results on top of whatever this publishes; SSE
            // subscribers only ever see `None` from this path.
            checks: None,
        });

        self.refresh_outputs().await;

        // --- the canvas plan ---
        // The configured layout lives in canvas space and may overlap; sway
        // must only ever see a plain tiling. Decide the mapping first, so the
        // output planner below works on what sway is actually given.
        let canvas_plan = self.plan_canvas(&desired);
        let sway_outputs = self.outputs_for_sway(&desired, canvas_plan.as_ref());

        // --- outputs ---
        let output_plan = {
            let applied = self.applied.lock().await;
            crate::reconciler::plan::plan_outputs_with(
                &self.snapshot.outputs(),
                &sway_outputs,
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

        // Sent as one sway IPC message, not one `run_command` per line: on a
        // four-projector NVIDIA rig (RTX A1000, sway 1.10.1, 2026-09-15),
        // outputs enabled one command at a time left the first head 7.1 ms
        // out of phase with the other three, which locked to each other —
        // about 145 straddled frames (shown on different refreshes) per 330
        // captured. The same four, disabled then re-enabled together in one
        // sway command, landed within 0.03 ms of each other — about 20
        // straddles per 330 at the same rate. A mode-set delivered in one
        // IPC message is applied by sway in one backend commit, which is
        // what keeps the heads on the same clock; issuing it one command at
        // a time, as this loop did before, is precisely the case that
        // measured 2-7 ms out of phase. This also makes the very first
        // application after the daemon starts batched, since every pass
        // goes through here.
        for (command, result) in output_plan
            .commands
            .iter()
            .zip(self.sway.run_commands(&output_plan.commands).await)
        {
            if let Err(error) = result {
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
        } else if !output_plan.commands.is_empty() {
            // Even a plain move must be re-observed before anything downstream
            // derives geometry from the snapshot — blend seams computed from
            // pre-move rectangles paint ramps on the wrong edges.
            self.refresh_outputs().await;
        }

        // --- projection ---
        // After the outputs have settled: geometry is derived from observed
        // rectangles. In canvas mode this creates and sizes the headless
        // canvas and runs the slicer; it returns which output the active app
        // must render into.
        //
        // `output_plan.topology_changed` is passed through so the slicer can
        // be forced to restart even when its spec turns out unchanged — see
        // `sync_projection` and `BlendManager::restart_slicer`. It is true
        // only for the pass that actually issued the enable/disable commands
        // (freshly computed per call in `plan_outputs_with`, never carried
        // over), so this cannot restart the slicer on every ordinary pass.
        let (canvas_output, projection_divergences) = self
            .sync_projection(&desired, canvas_plan, output_plan.topology_changed)
            .await;
        divergences.extend(projection_divergences);

        // --- apps ---
        // Exactly one app runs — desired.activeApp — and it always covers the
        // whole canvas. The per-app placement fields are synthesized here
        // rather than configured, which is what "remove per-display apps"
        // means mechanically.
        let apps = Self::effective_apps(&desired, canvas_output.as_deref());
        let targets = resolve_app_targets(&apps, &self.snapshot.outputs(), &output_plan.workspaces);
        divergences.extend(self.supervisor.reconcile(&apps, &targets).await);

        // --- audio ---
        if apps.iter().any(|app| {
            app.enabled
                && app
                    .audio
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
        divergences.extend(self.apply_audio_gains(&apps).await);
        divergences.extend(self.audio_divergences(&apps));

        // --- cursor ---
        if desired.settings.hide_cursor {
            self.park_cursor().await;
        }

        let windows = self.refresh_windows().await;
        self.supervisor.tick(&windows).await;

        // --- adoption ---
        // Pin whatever each output settled on for a mode, scale or transform
        // the configuration left unset, so a reboot keeps today's picture
        // rather than trusting Sway's own default pick again. On 2026-09-15
        // a four-projector bench came back at 3840x2160@60 instead of the
        // 1920x1200@59.95 it had been running — nothing was misconfigured,
        // nothing had been configured at all, and the machine had simply
        // been lucky until then, which changed the projection canvas from
        // 9.2 to 19.2 megapixels and invalidated a set of performance
        // measurements. A pass that cannot reach sway has verified nothing,
        // so it adopts nothing either — the outputs below are stale.
        if self.sway.is_connected() {
            self.adopt_settled_outputs(&desired, &divergences).await;
        }

        // A pass that could not reach sway has not verified anything: the
        // outputs it reports are whatever was last seen, the commands it
        // planned went nowhere, and finding no divergences means only that
        // it could not look. Reporting that as "synced" is the worst answer
        // available - healthz already says the connection is down, so the
        // two would disagree, and the operator watching sync state would be
        // told the appliance was fine while it was blind.
        if !self.sway.is_connected() {
            divergences.push(Divergence::new(
                "sway_unreachable",
                "sway",
                concat!(
                    "the compositor's IPC socket is not answering, so this pass ",
                    "verified nothing and the outputs below are the last ones ",
                    "seen. A compositor that restarted has a new socket path, ",
                    "which a running daemon cannot pick up: restart Suede."
                )
                .to_string(),
            ));
        }

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
            // Read fresh, not carried over from `desired` above: a write
            // that lands mid-pass (the store is shared with every API
            // handler) must show up as `currentRevision` outrunning the
            // `revision` this pass actually applied, which is exactly the
            // "have you caught up with my last write" signal the field
            // exists for.
            committed: !self.store.has_preview(),
            current_revision: self.store.revision(),
            checks: None,
        };
        self.publish_status(status.clone());
        status
    }

    /// Run the projection machinery for this pass's canvas plan.
    ///
    /// With a plan (the configured layout overlaps somewhere): ensure the
    /// headless canvas exists at the right size, run the slicer with the
    /// plan's slices, and report which output the active app must render
    /// into. Without one: retire any lingering canvas and show test patterns
    /// per output if asked.
    ///
    /// `restart_slicer` is this pass's `OutputPlan::topology_changed`: true
    /// when the compositor was just told to enable or disable an output,
    /// which destroys and recreates it. A running slicer's spec fingerprint
    /// does not change when an output disappears and comes back under the
    /// same name, so without this it would be left alone, still bound to the
    /// layer surfaces it built against the output that no longer exists —
    /// see `BlendManager::restart_slicer`.
    #[cfg(feature = "projection")]
    async fn sync_projection(
        &self,
        desired: &crate::model::DesiredState,
        plan: Option<crate::projection::CanvasPlan>,
        restart_slicer: bool,
    ) -> (Option<String>, Vec<Divergence>) {
        use crate::checks::is_synthetic_output;
        use crate::projection::{overlay_specs, Participant, SlicerSpec};

        let mut divergences = Vec::new();
        let mut overlay = Vec::new();
        let mut slicer = None;
        let mut canvas: Option<String> = None;
        let projection = desired.projection.clone().unwrap_or_default();

        if let Some(plan) = plan {
            // The canvas is a headless output: create it if the backend is
            // there, size it to the plan, park it away from the tiling.
            let find_headless = |outputs: &[crate::model::Output]| {
                outputs
                    .iter()
                    .find(|output| output.name.starts_with("HEADLESS-"))
                    .map(|output| {
                        (
                            output.name.clone(),
                            output.rect,
                            output.active,
                            output.current_mode.map(|mode| mode.refresh_hz),
                        )
                    })
            };
            if find_headless(&self.snapshot.outputs()).is_none() {
                if let Err(error) = self.sway.run_command("create_output").await {
                    tracing::warn!(%error, "create_output failed");
                }
                self.refresh_outputs().await;
            }
            match find_headless(&self.snapshot.outputs()) {
                None => divergences.push(Divergence::new(
                    "headless_unavailable",
                    "projection",
                    "the overlapping layout needs sway's headless backend \
                     (WLR_BACKENDS=drm,libinput,headless) and no canvas output \
                     could be created; the layout is tiled without slicing \
                     meanwhile"
                        .to_string(),
                )),
                Some((name, rect, active, current_refresh_hz)) => {
                    let (width, height) = (plan.canvas_width, plan.canvas_height);
                    // Give the canvas the rate its participating outputs are
                    // actually running at, rather than leaving sway's
                    // headless backend at its own default (which reports 0
                    // Hz back, and paces requestAnimationFrame at nothing in
                    // particular). wlroots' headless backend times its
                    // frames in whole milliseconds (1000 / refresh), so 60
                    // Hz actually ticks at 16 ms — close enough that it does
                    // not matter here, but for a 50 Hz wall 20 ms is exact,
                    // and getting it right is free.
                    let observed = self.snapshot.outputs();
                    let participant_rates: Vec<f64> = plan
                        .sway_positions
                        .iter()
                        .filter_map(|(participant, _, _)| {
                            observed
                                .iter()
                                .find(|output| &output.name == participant)
                                .and_then(|output| output.current_mode)
                                .map(|mode| mode.refresh_hz)
                        })
                        .collect();
                    let wanted_rate =
                        crate::reconciler::plan::canvas_refresh_hz(&participant_rates);
                    let rate_needs_reissue = match (wanted_rate, current_refresh_hz) {
                        (Some(wanted), Some(current)) => (wanted - current).abs() > 0.01,
                        (Some(_), None) => true,
                        (None, _) => false,
                    };
                    if !active
                        || rect.width != width
                        || rect.height != height
                        || rect.y != 20000
                        || rate_needs_reissue
                    {
                        let mode_command = match wanted_rate {
                            Some(rate) => format!(
                                "output {name} mode --custom {width}x{height}@{}Hz",
                                crate::model::format_refresh(rate)
                            ),
                            None => format!("output {name} mode --custom {width}x{height}"),
                        };
                        let canvas_commands = vec![
                            format!("output {name} enable"),
                            mode_command,
                            // An implicit scale would make the canvas's pixel
                            // buffer differ from its logical size, and the
                            // slicer cuts pixels, not logical units.
                            format!("output {name} scale 1"),
                            format!("output {name} pos 0 20000"),
                        ];
                        // One output, but batching costs nothing here and
                        // keeps this in step with the same one-commit
                        // reasoning as the output plan above.
                        for (command, result) in canvas_commands
                            .iter()
                            .zip(self.sway.run_commands(&canvas_commands).await)
                        {
                            if let Err(error) = result {
                                divergences.push(Divergence::new(
                                    "command_failed",
                                    command.clone(),
                                    error.to_string(),
                                ));
                            }
                        }
                        self.refresh_outputs().await;
                    }
                    // The slicer's surfaces sit on the overlay layer, above
                    // everything sway draws on these outputs — including the
                    // backgrounds. That is right while an app is producing
                    // frames, and wrong the moment it is not: a canvas with
                    // nothing on it would paint black over every configured
                    // background. So the slicer runs only when there is
                    // something for it to show. Standing it down uncovers
                    // the outputs and their backgrounds appear, which is the
                    // whole point of having configured one.
                    //
                    // Test patterns are drawn rather than captured, so they
                    // are a reason to run with no app at all — that is the
                    // bench-alignment case.
                    let anything_to_show =
                        desired.active_app.is_some() || projection.test_pattern.is_some();
                    // With nothing attached there is nowhere to present, but
                    // the canvas stays exactly as configured so the app is
                    // never resized — displays coming back find the frame
                    // they left.
                    if anything_to_show && !plan.slices.is_empty() {
                        slicer = Some(SlicerSpec {
                            source: name.clone(),
                            canvas_width: width,
                            canvas_height: height,
                            gamma: projection.gamma,
                            black_lift: projection.black_lift,
                            pattern: projection.test_pattern,
                            free_run: projection.free_run,
                            renderer: projection.renderer,
                            slices: plan.slices,
                        });
                    }
                    canvas = Some(name);
                }
            }
        } else {
            // Leaving canvas mode: a lingering headless output would be
            // spanned by fullscreen-global apps, so retire it.
            if let Some(name) = self
                .snapshot
                .outputs()
                .iter()
                .find(|output| output.name.starts_with("HEADLESS-"))
                .map(|output| output.name.clone())
            {
                if let Err(error) = self
                    .sway
                    .run_command(&format!("output {name} unplug"))
                    .await
                {
                    tracing::warn!(%error, output = %name, "could not unplug the canvas output");
                }
                self.refresh_outputs().await;
            }

            // Test patterns without a canvas: per-output, as on a bench.
            if desired.projection.is_some() {
                let participants: Vec<Participant> = self
                    .snapshot
                    .outputs()
                    .iter()
                    .filter(|output| {
                        output.active && output.rect.width > 0 && !is_synthetic_output(&output.name)
                    })
                    .map(|output| Participant {
                        name: output.name.clone(),
                        rect: output.rect,
                        connected: true,
                    })
                    .collect();
                overlay = overlay_specs(&participants, &projection);
            }
        }

        let mut manager = self.blend.lock().await;
        divergences.extend(manager.sync(&overlay));
        if restart_slicer {
            manager.restart_slicer();
        }
        divergences.extend(manager.sync_slicer(slicer.as_ref()));
        // `sync_slicer` above already reaped a dead child before deciding
        // whether to respawn, so the manager knows definitively whether one
        // is alive now — publish that rather than leaving the API and the
        // `output-phase` check to infer it from whether stats have arrived,
        // which stays "no" for as long as a static page produces no frames
        // to report (see `ProjectionReport`).
        self.publish_slicer_running(manager.slicer_running());
        (canvas, divergences)
    }

    /// Without the `projection` feature there is nothing to run — but a
    /// layout that needs slicing must be surfaced, not silently mis-tiled.
    #[cfg(not(feature = "projection"))]
    async fn sync_projection(
        &self,
        desired: &crate::model::DesiredState,
        _plan: Option<()>,
        _restart_slicer: bool,
    ) -> (Option<String>, Vec<Divergence>) {
        // No slicer ever runs in this build, so there is exactly one
        // liveness fact to publish, once.
        self.publish_slicer_running(false);
        if desired.projection.is_some() {
            (
                None,
                vec![Divergence::new(
                    "projection_unavailable",
                    "projection",
                    "projection is configured, but this build of suede was compiled \
                     without the 'projection' feature",
                )],
            )
        } else {
            (None, Vec::new())
        }
    }

    /// Publish the slicer's liveness, telling any SSE listener when it
    /// actually changed — mirrors `publish_status`, and exists so both
    /// `sync_projection` variants above share one place that decides
    /// whether to say something rather than each reimplementing the
    /// change check.
    fn publish_slicer_running(&self, running: bool) {
        if self.snapshot.set_slicer_running(running) {
            self.events
                .publish(ServerEvent::ProjectionStatsChanged(Box::new(
                    self.snapshot.projection_report(),
                )));
        }
    }

    /// The configured layout, in canvas space, resolved to output names.
    ///
    /// Every enabled output the configuration pins down takes part, whether
    /// or not a display is attached: the installation is described by the
    /// configuration, so a projector that is unplugged (or not yet
    /// delivered) still holds its place in the canvas. Sizes come from the
    /// configured mode, falling back to what a connected display currently
    /// runs.
    #[cfg(feature = "projection")]
    fn plan_canvas(
        &self,
        desired: &crate::model::DesiredState,
    ) -> Option<crate::projection::CanvasPlan> {
        use crate::projection::Participant;
        let observed = self.snapshot.outputs();
        let mut participants = Vec::new();
        for config in desired.outputs.iter().filter(|output| output.enable) {
            let matched = observed
                .iter()
                .find(|output| config.r#match.matches(output));
            // Geometry comes from the configuration first — the operator's
            // own choice, or one already pinned because none was given.
            // Only an output that is both connected *and* still unpinned
            // falls back to what it currently runs — a disconnected output
            // has no "currently".
            let size = config
                .effective_mode()
                .map(|mode| (mode.width, mode.height))
                .or_else(|| matched?.current_mode.map(|mode| (mode.width, mode.height)));
            let position = config.position.or_else(|| {
                matched.map(|output| crate::model::Position {
                    x: output.rect.x,
                    y: output.rect.y,
                })
            });
            // Without a size and a place, the entry describes no rectangle
            // and cannot take part in a layout at all.
            let (Some((width, height)), Some(position)) = (size, position) else {
                continue;
            };
            participants.push(Participant {
                // Absent outputs are named by their match key, which never
                // collides with a connector name sway would report.
                name: matched
                    .map(|output| output.name.clone())
                    .unwrap_or_else(|| config.r#match.key()),
                rect: crate::model::Rect {
                    x: position.x,
                    y: position.y,
                    width,
                    height,
                },
                connected: matched.is_some(),
            });
        }
        crate::projection::canvas_plan(&participants, desired.projection.as_ref())
    }

    #[cfg(not(feature = "projection"))]
    fn plan_canvas(&self, _desired: &crate::model::DesiredState) -> Option<()> {
        None
    }

    /// What sway is told about the outputs. With a canvas plan, the
    /// configured (possibly overlapping) positions are replaced by the
    /// plan's synthesized edge-to-edge tiling; without one, the configured
    /// layout goes to sway verbatim.
    #[cfg(feature = "projection")]
    fn outputs_for_sway(
        &self,
        desired: &crate::model::DesiredState,
        plan: Option<&crate::projection::CanvasPlan>,
    ) -> Vec<crate::model::OutputConfig> {
        let Some(plan) = plan else {
            return desired.outputs.clone();
        };
        let observed = self.snapshot.outputs();
        desired
            .outputs
            .iter()
            .cloned()
            .map(|mut output| {
                // Resolve the config to its connector name the same way the
                // canvas plan did, then take that name's synthesized slot.
                let name = observed
                    .iter()
                    .find(|candidate| output.r#match.matches(candidate))
                    .map(|candidate| candidate.name.clone());
                if let Some(name) = name {
                    if let Some((_, x, y)) = plan.sway_positions.iter().find(|(n, _, _)| *n == name)
                    {
                        output.position = Some(crate::model::Position { x: *x, y: *y });
                    }
                }
                output
            })
            .collect()
    }

    #[cfg(not(feature = "projection"))]
    fn outputs_for_sway(
        &self,
        desired: &crate::model::DesiredState,
        _plan: Option<&()>,
    ) -> Vec<crate::model::OutputConfig> {
        desired.outputs.clone()
    }

    /// The apps as the supervisor should see them: exactly one enabled (the
    /// active one), always fullscreen, covering either the headless canvas or
    /// the whole physical span.
    fn effective_apps(
        desired: &crate::model::DesiredState,
        canvas_output: Option<&str>,
    ) -> Vec<crate::model::AppConfig> {
        desired
            .apps
            .iter()
            .cloned()
            .map(|mut app| {
                app.enabled = desired.active_app.as_deref() == Some(app.id.as_str());
                app.fullscreen = true;
                match canvas_output {
                    // Canvas mode: fill the headless canvas; the slicer takes
                    // it to the projectors.
                    Some(name) => {
                        app.output = Some(crate::model::OutputMatch::by_name(name));
                        app.span_outputs = false;
                    }
                    // Plain mode: one window across every physical output.
                    None => {
                        app.output = None;
                        app.span_outputs = true;
                    }
                }
                app
            })
            .collect()
    }

    /// Bring each configured sink to the gain its app asks for.
    ///
    /// Level is desired state like anything else here: an appliance whose
    /// output sits whereever the last session left it is passing signal at a
    /// level nobody knows, and the symptom — everything works, quietly — is a
    /// wretched one to chase. Only sinks an app actually names are touched;
    /// the rest of the machine's audio is not Suede's business.
    async fn apply_audio_gains(&self, apps: &[crate::model::AppConfig]) -> Vec<Divergence> {
        let available = self.audio.sinks();
        if available.is_empty() {
            return Vec::new();
        }

        let mut divergences = Vec::new();
        let mut applied: Vec<&str> = Vec::new();
        for app in apps.iter().filter(|app| app.enabled) {
            let Some(audio) = app.audio.as_ref() else {
                continue;
            };
            // A silent app is routed to the null sink, whose level means
            // nothing: it discards the signal either way.
            let Some(wanted) = audio.output.as_deref() else {
                continue;
            };
            let Some(sink) = available.iter().find(|sink| sink.id == wanted) else {
                // audio_divergences reports the missing sink; do not say it twice.
                continue;
            };
            // Two apps naming one sink with different levels is a
            // contradiction the configuration cannot resolve, so say so
            // rather than letting the last one to be visited win silently.
            if applied.contains(&wanted) {
                continue;
            }

            // PipeWire's own figure is quantised, and a comparison of floats
            // that came back through a decibel conversion needs slack: a
            // hundredth of a dB is far below anything audible or meaningful.
            if sink
                .gain_db
                .is_some_and(|current| (current - audio.gain_db).abs() < 0.01)
            {
                applied.push(wanted);
                continue;
            }

            match self.audio.set_sink_gain(wanted, audio.gain_db).await {
                Ok(()) => applied.push(wanted),
                Err(error) => {
                    tracing::warn!(%error, sink = wanted, "could not set the sink gain");
                    divergences.push(Divergence::new(
                        "audio_gain_not_applied",
                        wanted,
                        format!("could not set {wanted} to {} dB: {error}", audio.gain_db),
                    ));
                }
            }
        }
        divergences
    }

    /// Report apps whose configured audio sink is not currently present.
    fn audio_divergences(&self, apps: &[crate::model::AppConfig]) -> Vec<Divergence> {
        let available = self.audio.sinks();
        if available.is_empty() {
            // Nothing known about audio yet; do not cry wolf.
            return Vec::new();
        }
        apps.iter()
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

    /// Pin, in the persisted document, whatever each configured output
    /// settled on for a mode/scale/transform the operator left unset. See
    /// [`plan::adoption_for`] for the decision and why each guard exists;
    /// this only orchestrates it — matching each config entry to its live
    /// output, keeping the pass-to-pass observation memory, and writing the
    /// result through [`StateStore::update`] so it lands as a normal
    /// committed change (a new revision, an SSE event) like any other.
    ///
    /// Skipped entirely while a working copy is live: an operator mid-edit
    /// must not have the document rewritten underneath them. The
    /// observation memory is still refreshed even then, so adoption is not
    /// left thinking a display just changed the instant the preview clears.
    async fn adopt_settled_outputs(
        &self,
        desired: &crate::model::DesiredState,
        divergences: &[Divergence],
    ) {
        let observed = self.snapshot.outputs();
        let mut history = self.previous_observations.lock().await;

        let mut pins: Vec<(crate::model::OutputMatch, crate::model::AdoptedOutput)> = Vec::new();
        if !self.store.has_preview() {
            for config in &desired.outputs {
                let Some(output) = observed
                    .iter()
                    .find(|candidate| config.r#match.matches(candidate))
                else {
                    continue;
                };
                // Divergence subjects name either the output itself
                // (`mode_unsupported`, `output_not_connected`, …) or, for
                // `command_failed`, the sway command that failed — which
                // always names its output — so a substring match catches
                // both without the caller needing to know which shape it is.
                let this_output_diverged = divergences
                    .iter()
                    .any(|d| d.subject.contains(output.name.as_str()));
                let previous = history.get(&output.name);
                if let Some(adopted) =
                    plan::adoption_for(config, output, previous, this_output_diverged)
                {
                    pins.push((config.r#match.clone(), adopted));
                }
            }
        }

        // Recorded for every observed output, not only ones with a config
        // entry today, so one added later already has a pass of history to
        // compare against.
        for output in &observed {
            history.insert(output.name.clone(), output.clone());
        }
        drop(history);

        if pins.is_empty() {
            return;
        }

        // What each pin is replacing, captured before the write, so the log
        // can say whether this was a first pin or a re-adopt after the
        // display on that connector changed.
        let previous_adopted: HashMap<String, Option<crate::model::AdoptedOutput>> = pins
            .iter()
            .map(|(rule, _)| {
                let previous = desired
                    .outputs
                    .iter()
                    .find(|output| output.r#match.key() == rule.key())
                    .and_then(|output| output.adopted.clone());
                (rule.key(), previous)
            })
            .collect();

        match self.store.update(|state| {
            for output in &mut state.outputs {
                if let Some((_, adopted)) = pins
                    .iter()
                    .find(|(rule, _)| rule.key() == output.r#match.key())
                {
                    output.adopted = Some(adopted.clone());
                }
            }
        }) {
            Ok(_) => {
                for (rule, adopted) in &pins {
                    let previous = previous_adopted.get(&rule.key()).cloned().flatten();
                    let event = match &previous {
                        None => "first pin",
                        Some(previous) if previous.display != adopted.display => {
                            "re-adopt: display changed"
                        }
                        Some(_) => "adopted a field the operator freed",
                    };
                    tracing::info!(
                        output = %rule.key(),
                        mode = ?adopted.mode.map(|m| m.to_sway()),
                        scale = adopted.scale,
                        transform = ?adopted.transform.map(|t| t.as_sway()),
                        event,
                        "adopted the output value the configuration left unset"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, "failed to persist adopted output values");
            }
        }
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

        // An app that begins failing changes the appliance's health without
        // changing anything a pass is normally triggered by, so its state is
        // watched explicitly.
        let mut faults = self.supervisor.fault_signature().await;

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
                    let current = self.supervisor.fault_signature().await;
                    if current != faults {
                        faults = current;
                        self.reconcile().await;
                    }
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
        AppConfig, AudioConfig, Launcher, Mode, Output, OutputConfig, OutputMatch, Position,
        RestartPolicy,
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
            // Unrestricted: these tests exercise reconciliation, not the
            // allowlist, and launch stand-ins like "sleep" that a real
            // appliance's browser-only default would refuse.
            vec!["*".to_string()],
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
    async fn a_pass_that_cannot_reach_sway_is_not_synced() {
        // The pass finds nothing wrong because it cannot look. Reporting
        // that as healthy is how an operator ends up trusting a dashboard
        // while the screens show something else entirely.
        let harness = harness();
        harness.sway.set_connected(false);

        let status = harness.reconciler.reconcile().await;
        assert_eq!(status.state, SyncState::Degraded);
        let found = status
            .divergences
            .iter()
            .find(|d| d.kind == "sway_unreachable")
            .expect("must say the compositor is unreachable");
        assert!(found.detail.contains("restart"), "{}", found.detail);
        assert!(found.docs_url.is_some(), "and lead somewhere");

        // And it clears by itself once the socket answers again.
        harness.sway.set_connected(true);
        let status = harness.reconciler.reconcile().await;
        assert!(!status
            .divergences
            .iter()
            .any(|d| d.kind == "sway_unreachable"));
        harness.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn a_configured_gain_is_applied_to_its_sink_and_then_left_alone() {
        // Level is desired state: the pass brings the sink to it, and having
        // done so stops touching it. A reconciler that re-sets an unchanged
        // value on every pass is a reconciler that fights the operator.
        let harness = harness();
        harness
            .store
            .update(|state| {
                state.apps.push(app(
                    "renderer",
                    None,
                    Some(AudioConfig {
                        output: Some("alsa_output.hdmi-stereo".into()),
                        gain_db: -6.0,
                    }),
                ));
                state.active_app = Some("renderer".into());
            })
            .unwrap();

        harness.reconciler.reconcile().await;
        assert_eq!(
            harness.audio.gains_set(),
            vec![("alsa_output.hdmi-stereo".to_string(), -6.0)]
        );

        harness.reconciler.reconcile().await;
        assert_eq!(
            harness.audio.gains_set().len(),
            1,
            "an unchanged level must not be written again"
        );
        harness.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn a_silent_app_has_no_level_to_set() {
        // The null sink discards the signal whatever its volume, so setting
        // one would be theatre.
        let harness = harness();
        harness
            .store
            .update(|state| {
                state.apps.push(app(
                    "renderer",
                    None,
                    Some(AudioConfig {
                        output: None,
                        gain_db: -6.0,
                    }),
                ));
                state.active_app = Some("renderer".into());
            })
            .unwrap();

        harness.reconciler.reconcile().await;
        assert!(harness.audio.gains_set().is_empty());
        harness.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn status_carries_the_applied_revision() {
        let harness = harness();
        harness.store.update(|_| {}).unwrap();
        let status = harness.reconciler.reconcile().await;
        assert_eq!(status.revision, 1);
    }

    #[tokio::test]
    async fn status_reports_committed_and_current_revision_from_the_store() {
        let harness = harness();
        let status = harness.reconciler.reconcile().await;
        assert!(status.committed, "no working copy is live");
        assert_eq!(status.current_revision, 0);
        // The reconciler holds no check runner, so it never claims to know.
        assert!(status.checks.is_none());

        harness.store.set_preview(Some(harness.store.get()));
        let status = harness.reconciler.reconcile().await;
        assert!(
            !status.committed,
            "a live working copy is not what is saved on disk"
        );

        harness.store.set_preview(None);
        let status = harness.reconciler.reconcile().await;
        assert!(status.committed);
    }

    #[tokio::test]
    async fn a_missing_canvas_is_a_divergence() {
        let harness = harness();
        harness
            .store
            .update(|state| {
                state.apps.push(app("renderer", None, None));
                state.active_app = Some("renderer".into());
                // An overlapping layout wants a canvas, but the mock
                // compositor has no headless backend, so it can never
                // materialise.
                state.outputs.push(configured_output("HDMI-A-1", 0));
                state.outputs.push(configured_output("HDMI-A-2", 1760));
            })
            .unwrap();

        let status = harness.reconciler.reconcile().await;
        assert!(
            status
                .divergences
                .iter()
                .any(|d| d.kind == "headless_unavailable"),
            "the operator must hear that the canvas is missing: {:?}",
            status.divergences
        );
        harness.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn the_canvas_is_created_with_the_shared_refresh_rate() {
        // Both physical outputs run at 60 Hz, so the headless canvas that
        // carries the overlap between them should be told to run at 60 Hz
        // too, rather than being left at sway's own default (which reports
        // back as 0 Hz — the very thing that made this worth fixing).
        let harness = harness();
        let mut outputs = harness.sway.get_outputs().await.unwrap();
        outputs.push(Output {
            name: "HEADLESS-1".into(),
            active: false,
            make: None,
            model: None,
            serial: None,
            current_mode: None,
            modes: vec![],
            rect: Default::default(),
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        });
        harness.sway.set_outputs(outputs);

        harness
            .store
            .update(|state| {
                state.apps.push(app("renderer", None, None));
                state.active_app = Some("renderer".into());
                // Overlapping x-positions, same as a_missing_canvas_is_a_divergence,
                // so the canvas plan actually needs a headless output.
                state.outputs.push(configured_output("HDMI-A-1", 0));
                state.outputs.push(configured_output("HDMI-A-2", 1760));
            })
            .unwrap();

        harness.reconciler.reconcile().await;
        assert!(
            harness
                .sway
                .ran_command_containing("mode --custom 3680x1080@60Hz"),
            "canvas mode command did not carry the shared rate: {:?}",
            harness.sway.commands()
        );
        harness.supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn topology_change_restarts_the_slicer_even_with_an_unchanged_spec() {
        // Mirrors the 2026-09-15 bench measurement: the `output-phase`
        // check's fix disables and re-enables every output together, which
        // destroys and recreates them in the compositor, but an output that
        // disappears and comes back under the same name and geometry leaves
        // the `SlicerSpec` — and so its fingerprint — completely unchanged.
        // Without `OutputPlan::topology_changed` forcing a restart, the
        // already-running slicer would be left alone, still bound to layer
        // surfaces built against outputs that no longer exist.
        let harness = harness();
        let mut outputs = harness.sway.get_outputs().await.unwrap();
        outputs.push(Output {
            name: "HEADLESS-1".into(),
            active: false,
            make: None,
            model: None,
            serial: None,
            current_mode: None,
            modes: vec![],
            rect: Default::default(),
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        });
        harness.sway.set_outputs(outputs);

        harness
            .store
            .update(|state| {
                state.apps.push(app("renderer", None, None));
                state.active_app = Some("renderer".into());
                // Overlapping x-positions, so the canvas plan needs a
                // headless output and the slicer actually runs.
                state.outputs.push(configured_output("HDMI-A-1", 0));
                state.outputs.push(configured_output("HDMI-A-2", 1760));
            })
            .unwrap();

        harness.reconciler.reconcile().await;
        let pid_after_first_pass = harness
            .reconciler
            .slicer_pid()
            .await
            .expect("the overlap must have started a slicer");

        // An ordinary pass, nothing changed: left alone.
        harness.reconciler.reconcile().await;
        assert_eq!(
            harness.reconciler.slicer_pid().await,
            Some(pid_after_first_pass),
            "an unchanged pass must not restart the slicer"
        );

        // Simulate the output going away and coming back under the same
        // name and geometry: the mock reports it inactive without touching
        // its mode or rect, so the next pass issues an `enable` command
        // (topology_changed) even though the geometry the slicer's spec is
        // built from — and so the spec's fingerprint — ends up identical.
        let mut outputs = harness.sway.get_outputs().await.unwrap();
        for output in outputs.iter_mut() {
            if output.name == "HDMI-A-1" {
                output.active = false;
            }
        }
        harness.sway.set_outputs(outputs);

        harness.reconciler.reconcile().await;
        assert!(
            harness
                .sway
                .ran_command_containing("output HDMI-A-1 enable"),
            "the simulated teardown must have been re-enabled: {:?}",
            harness.sway.commands()
        );
        let pid_after_topology_change = harness
            .reconciler
            .slicer_pid()
            .await
            .expect("the slicer must still be running after the output came back");
        assert_ne!(
            pid_after_topology_change, pid_after_first_pass,
            "a pass that changed output topology must restart the slicer even \
             though its spec is unchanged"
        );
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
                        gain_db: 0.0,
                    }),
                ));
                state.active_app = Some("renderer".into());
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
                state.apps.push(app(
                    "silent",
                    None,
                    Some(AudioConfig {
                        output: None,
                        gain_db: 0.0,
                    }),
                ));
                state.active_app = Some("silent".into());
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
    async fn a_settled_mode_is_adopted_after_two_passes_then_left_alone() {
        // Mirrors the 2026-09-15 incident: mode left unset, sway settles on
        // whatever the display already reports, and after two agreeing
        // passes that value must be pinned so it survives a reboot.
        let harness = harness();
        harness.reconciler.detect_capabilities().await;
        harness
            .store
            .update(|state| {
                // No mode: left to sway's own preferred pick, exactly the
                // case adoption exists for.
                state
                    .outputs
                    .push(OutputConfig::new(OutputMatch::by_name("HDMI-A-1")));
            })
            .unwrap();

        // First pass: nothing to compare against yet, so nothing is pinned.
        harness.reconciler.reconcile().await;
        assert!(
            harness.store.get().outputs[0].adopted.is_none(),
            "must not adopt from a single sighting"
        );

        // Second pass: the same mode was observed twice in a row.
        let revision_after_first = harness.store.get().revision;
        harness.reconciler.reconcile().await;
        let after_second = harness.store.get();
        let adopted = after_second.outputs[0]
            .adopted
            .clone()
            .expect("a value settled on two consecutive passes must be pinned");
        assert_eq!(adopted.mode.unwrap().width, 1920);
        assert_eq!(adopted.mode.unwrap().height, 1080);
        assert!(after_second.revision > revision_after_first);

        // Third pass: already pinned and nothing changed, so nothing is
        // written — a flapping cable must not churn the revision.
        harness.reconciler.reconcile().await;
        let after_third = harness.store.get();
        assert_eq!(
            after_third.revision, after_second.revision,
            "an unchanged adoption must not write again"
        );
        assert_eq!(after_third.outputs[0].adopted, Some(adopted));
        harness.supervisor.shutdown().await;
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
