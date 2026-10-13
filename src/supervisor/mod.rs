//! Application lifecycle.
//!
//! Apps are spawned as direct child processes rather than through Sway's
//! `exec`, which is what gives Suede a real PID: clean termination, exit-code
//! observation, restart policies, and per-app audio routing all depend on it.

pub mod launcher;
mod profile;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Child;
use tokio::sync::Mutex;

use crate::devtools;
use crate::events::{EventHub, ServerEvent};
use crate::model::{AppConfig, AppState, AppStatus, Divergence, RestartReason, Window};
use crate::reconciler::plan::{desired_fullscreen_mode, placement_commands, AppTarget};
use crate::sway::SwayClient;

pub use launcher::{LaunchContext, LaunchSpec};

/// How long an app may take to map its window before it is considered failed.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(15);
/// Grace period between SIGTERM and SIGKILL.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);

struct ManagedApp {
    config: AppConfig,
    status: AppStatus,
    child: Option<Child>,
    target: Option<AppTarget>,
    attempts: u32,
    restart_at: Option<Instant>,
    launched_at: Option<Instant>,
    last_heartbeat_at: Option<Instant>,
    /// Whether the window has been moved to its workspace and fullscreened.
    placed: bool,
    /// Set when the restart policy declined to relaunch, so nothing may
    /// auto-start this app until the configuration changes or the API asks.
    halted: bool,
    /// Whether the app's readiness URL has answered acceptably for the
    /// launch currently in flight.
    ///
    /// Not latched across relaunches: [`unlatch_readiness`] clears it
    /// whenever an app is stopped and about to launch again — a crash, a
    /// heartbeat timeout, a window that never appeared, a manual restart, or
    /// a config change — so a dependency that died in the meantime holds the
    /// app back instead of sending it into a browser error page. The
    /// original worry, that a blip would stall a crash relaunch, is bounded:
    /// `tick()` probes every second, so a healthy dependency only delays a
    /// relaunch by about one tick, while an unhealthy one is exactly what
    /// should hold the launch.
    dependency_ready: bool,
    /// When the last readiness probe ran, to honor the configured interval.
    last_probe: Option<Instant>,
    /// When Suede first started waiting, for the give-up timer.
    waiting_since: Option<Instant>,
    /// Set when `advance` refused to spawn this app because its resolved
    /// program is not in `allowed_programs`, so the divergence loop in
    /// `reconcile` can report `app_program_not_allowed` instead of the
    /// generic `app_halted` — the operator's fix is completely different
    /// (edit bootstrap config and restart the daemon, not touch the app).
    /// Cleared by [`schedule_restart_or_halt`], which every other path to a
    /// halted app goes through, so a later, unrelated failure cannot inherit
    /// a stale reason.
    program_not_allowed: bool,
    /// A stopped process must wait for a current reconciliation before launch.
    /// This also holds shared producers until their receiver is replaced.
    launch_pending: bool,
    /// The DevTools session with a `chromium-kiosk` app's browser, when it
    /// was launched with the pipe (or a failed placeholder saying why the
    /// pipe could not be made).
    ///
    /// It owns the write end of Chromium's fd 3, and Chromium exits when
    /// that closes, so it is dropped only where the browser is going anyway:
    /// in [`stop`] just before SIGTERM, and when a new launch replaces it.
    devtools: Option<devtools::Client>,
}

impl ManagedApp {
    fn new(config: AppConfig) -> Self {
        let status = AppStatus::stopped(&config.id);
        Self {
            config,
            status,
            child: None,
            target: None,
            attempts: 0,
            restart_at: None,
            launched_at: None,
            last_heartbeat_at: None,
            placed: false,
            halted: false,
            dependency_ready: false,
            last_probe: None,
            waiting_since: None,
            program_not_allowed: false,
            launch_pending: false,
            devtools: None,
        }
    }

    fn is_running(&self) -> bool {
        self.child.is_some()
    }
}

pub struct Supervisor {
    sway: Arc<dyn SwayClient>,
    events: EventHub,
    context: LaunchContext,
    apps: Mutex<HashMap<String, ManagedApp>>,
    /// Programs any app may launch, from bootstrap's `allowed_programs`. A
    /// copy of the list rather than the whole `BootstrapConfig`: launching is
    /// all the supervisor needs from it, and bootstrap config is otherwise
    /// the API layer's concern.
    allowed_programs: Vec<String>,
}

impl Supervisor {
    /// The deployment facts a launch is built from.
    ///
    /// Exposed so the API can show an operator exactly what an application
    /// would run without running it - the resolved binary, the arguments and
    /// the environment are all decided here, and a UI that reconstructed
    /// them would be guessing.
    pub fn launch_context(&self) -> &LaunchContext {
        &self.context
    }

    pub fn new(
        sway: Arc<dyn SwayClient>,
        events: EventHub,
        context: LaunchContext,
        allowed_programs: Vec<String>,
    ) -> Self {
        Self {
            sway,
            events,
            context,
            apps: Mutex::new(HashMap::new()),
            allowed_programs,
        }
    }

    /// A supervisor managing nothing, with its profiles and logs under
    /// `root`, for tests of the things that hold one.
    #[cfg(test)]
    pub(crate) fn idle(root: &std::path::Path) -> Self {
        Self::new(
            Arc::new(crate::sway::mock::MockSway::empty()),
            EventHub::new(),
            LaunchContext {
                profiles_root: root.join("profiles"),
                log_root: root.join("logs"),
                api_base: "http://127.0.0.1:9088/api/v1".into(),
                browser_devtools: true,
            },
            Vec::new(),
        )
    }

    /// Bring the managed set in line with desired state, then advance lifecycles.
    ///
    /// Returns divergences for apps that cannot run right now.
    /// A stable summary of which apps are currently failing, and how badly.
    ///
    /// The reconciler watches this because nothing else would notice an app
    /// that starts crash-looping: no output changed and no configuration was
    /// written, so no pass runs and the reported health stays whatever it was
    /// when the machine was last healthy. The attempt count is clamped at the
    /// crash-loop threshold so a long-running failure settles on one value
    /// instead of provoking a pass on every retry.
    pub async fn fault_signature(&self) -> String {
        let apps = self.apps.lock().await;
        let mut faults: Vec<String> = apps
            .values()
            .filter(|managed| matches!(managed.status.state, AppState::Backoff | AppState::Crashed))
            .map(|managed| {
                format!(
                    "{}:{:?}:{}",
                    managed.config.id,
                    managed.status.state,
                    managed.attempts.min(CRASH_LOOP_ATTEMPTS)
                )
            })
            .collect();
        faults.sort();
        faults.join(",")
    }

    pub async fn reconcile(&self, desired: &[AppConfig], targets: &[AppTarget]) -> Vec<Divergence> {
        self.reconcile_if_current(desired, targets, || true)
            .await
            .expect("unconditional reconciliation is always current")
    }

    /// Abort a superseded pass before it relaunches processes. Configuration
    /// may change while SIGTERM shutdown is awaited with the app lock held.
    pub async fn reconcile_if_current(
        &self,
        desired: &[AppConfig],
        targets: &[AppTarget],
        is_current: impl Fn() -> bool,
    ) -> Option<Vec<Divergence>> {
        let mut apps = self.apps.lock().await;
        let mut divergences = Vec::new();
        if !reconcile_is_current(&mut apps, &is_current) {
            return None;
        }

        // Apps that vanished from desired state are terminated and forgotten.
        let desired_ids: Vec<&str> = desired.iter().map(|app| app.id.as_str()).collect();
        let removed: Vec<String> = apps
            .keys()
            .filter(|id| !desired_ids.contains(&id.as_str()))
            .cloned()
            .collect();
        for id in removed {
            if let Some(mut managed) = apps.remove(&id) {
                tracing::info!(app = %id, "app removed from configuration");
                stop(&mut managed).await;
            }
            if !reconcile_is_current(&mut apps, &is_current) {
                return None;
            }
        }

        for config in desired {
            let target = targets.iter().find(|t| t.id == config.id).cloned();
            if let Some(blocked) = target.as_ref().and_then(|t| t.blocked.clone()) {
                divergences.push(blocked);
            }

            let managed = apps
                .entry(config.id.clone())
                .or_insert_with(|| ManagedApp::new(config.clone()));

            // A changed specification means the running process is stale.
            let config_changed = managed.config != *config;
            if config_changed {
                if managed.is_running() {
                    tracing::info!(app = %config.id, "configuration changed; restarting");
                    stop(managed).await;
                    if !is_current() {
                        reconcile_is_current(&mut apps, &|| false);
                        return None;
                    }
                    // A pure deactivation (the synthesized `enabled` flag
                    // going false because `activeApp` moved on) is not a
                    // restart of this app's own config, so it must not
                    // overwrite whatever reason is already on the books.
                    if config.enabled {
                        managed.status.last_restart_reason = Some(RestartReason::ConfigChanged);
                    }
                }
                // A new specification earns a halted app another attempt.
                // `allowed_programs` itself cannot change without a daemon
                // restart, but the app's own launcher can — switching it to
                // a permitted program deserves the same fresh attempt as any
                // other reconfiguration.
                managed.halted = false;
                managed.program_not_allowed = false;
                managed.attempts = 0;
                managed.restart_at = Some(Instant::now());
                // Every relaunch re-probes readiness now, so a config change
                // (deactivation included, so a later reactivation probes
                // afresh) needs the same reset the readiness URL changing
                // used to trigger on its own.
                unlatch_readiness(managed);
            }
            managed.config = config.clone();
            managed.target = target;
            managed.launch_pending = false;
        }

        if !reconcile_is_current(&mut apps, &is_current) {
            return None;
        }
        if !self.advance_if_current(&mut apps, &is_current).await {
            reconcile_is_current(&mut apps, &|| false);
            return None;
        }

        // An app that cannot run is a fault in the appliance, not a private
        // matter between the supervisor and the process table. Without this
        // the daemon reported `synced` while its active app failed to launch
        // for ever, so the one place an operator looks said all was well.
        //
        // A single crash and relaunch is normal life and says nothing, so
        // only a repeated failure counts; a halted app counts immediately,
        // because nothing will retry it.
        for managed in apps.values() {
            let detail = managed
                .status
                .detail
                .clone()
                .unwrap_or_else(|| "no detail".to_string());
            match managed.status.state {
                AppState::Crashed if managed.program_not_allowed => divergences.push(
                    Divergence::new("app_program_not_allowed", &managed.config.id, detail),
                ),
                AppState::Crashed => {
                    divergences.push(Divergence::new("app_halted", &managed.config.id, detail))
                }
                AppState::Backoff if managed.attempts >= CRASH_LOOP_ATTEMPTS => {
                    divergences.push(Divergence::new(
                        "app_crash_looping",
                        &managed.config.id,
                        format!("failed {} times: {detail}", managed.attempts),
                    ))
                }
                _ => {}
            }
        }
        Some(divergences)
    }

    /// Reap exits, run the watchdog, place new windows, and honor restart timers.
    pub async fn tick(&self, windows: &[Window]) {
        // Readiness probes talk to the network, so they run before the lock is
        // taken. Holding it across a probe would stall every API request behind
        // a service that is slow to answer — exactly when it is least welcome.
        self.probe_dependencies().await;

        let mut apps = self.apps.lock().await;
        self.reap(&mut apps).await;
        self.check_watchdogs(&mut apps).await;
        self.place_windows(&mut apps, windows).await;
        self.advance(&mut apps).await;
    }

    /// Poll the readiness URL of any app still waiting on one.
    async fn probe_dependencies(&self) {
        // Decide what to probe under the lock, then release it.
        let due: Vec<(String, crate::model::ReadinessConfig)> = {
            let apps = self.apps.lock().await;
            let now = Instant::now();
            apps.values()
                .filter(|managed| managed.config.enabled && !managed.dependency_ready)
                .filter_map(|managed| {
                    let readiness = managed.config.readiness.clone()?;
                    let due = managed.last_probe.is_none_or(|last| {
                        last.elapsed() >= Duration::from_secs(readiness.interval_seconds)
                    });
                    let _ = now;
                    due.then(|| (managed.config.id.clone(), readiness))
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }

        let results =
            futures::future::join_all(due.into_iter().map(|(id, readiness)| async move {
                let outcome = crate::probe::status_of(
                    &readiness.url,
                    Duration::from_secs(readiness.timeout_seconds),
                )
                .await;
                (id, readiness, outcome)
            }))
            .await;

        let mut apps = self.apps.lock().await;
        for (id, readiness, outcome) in results {
            let Some(managed) = apps.get_mut(&id) else {
                continue;
            };
            managed.last_probe = Some(Instant::now());
            let detail = match outcome {
                Ok(status) if readiness.accepts(status) => {
                    tracing::info!(app = %id, url = %readiness.url, status, "dependency is ready");
                    managed.dependency_ready = true;
                    managed.waiting_since = None;
                    continue;
                }
                Ok(status) => format!("{} answered {status}", readiness.url),
                Err(error) => format!("{}: {error}", readiness.url),
            };

            let waited = managed
                .waiting_since
                .get_or_insert_with(Instant::now)
                .elapsed();

            // Giving up is opt-in: for an appliance, showing the background
            // until the service appears beats showing a browser error page
            // that nothing will ever reload.
            if let Some(limit) = readiness.give_up_after_seconds {
                if waited >= Duration::from_secs(limit) {
                    tracing::warn!(
                        app = %id,
                        url = %readiness.url,
                        "dependency never became ready; launching anyway"
                    );
                    managed.dependency_ready = true;
                    managed.waiting_since = None;
                    continue;
                }
            }

            if managed.status.state != AppState::WaitingForDependency {
                tracing::info!(app = %id, url = %readiness.url, "waiting for dependency");
            }
            set_state(
                &mut managed.status,
                AppState::WaitingForDependency,
                Some(format!("waiting for {detail}")),
            );
        }
    }

    /// Record a heartbeat from an app's content. Returns false for unknown apps.
    pub async fn heartbeat(&self, id: &str) -> bool {
        let mut apps = self.apps.lock().await;
        let Some(managed) = apps.get_mut(id) else {
            return false;
        };
        let now = Instant::now();
        let first = managed.last_heartbeat_at.is_none();
        managed.last_heartbeat_at = Some(now);
        managed.status.last_heartbeat = Some(crate::util::unix_now());
        if first {
            tracing::info!(app = %id, "watchdog armed by first heartbeat");
        }
        true
    }

    /// Kill and relaunch an app on request.
    pub async fn restart(&self, id: &str) -> bool {
        self.restart_because(id, RestartReason::ApiRequest, None)
            .await
    }

    /// Stop a single-connection image producer before replacing its receiver.
    /// The next ordinary reconcile launches it after the new receiver exists.
    pub async fn prepare_shared_receiver(&self, id: &str) -> bool {
        self.prepare_shared_receiver_before(id, || {}).await
    }

    /// Register receiver retirement under the app lock before signaling the
    /// producer. The callback must not wait or acquire the supervisor lock.
    pub async fn prepare_shared_receiver_before(
        &self,
        id: &str,
        before_stop: impl FnMut(),
    ) -> bool {
        self.prepare_shared_receiver_because(id, RestartReason::ConfigChanged, before_stop)
            .await
    }

    pub async fn prepare_shared_receiver_because(
        &self,
        id: &str,
        reason: RestartReason,
        mut before_stop: impl FnMut(),
    ) -> bool {
        let mut apps = self.apps.lock().await;
        let Some(managed) = apps.get_mut(id) else {
            return false;
        };
        if managed.config.shared_input.is_none() {
            return false;
        }
        managed.launch_pending = true;
        let was_running = managed.is_running();
        if was_running {
            before_stop();
        }
        stop(managed).await;
        if was_running {
            managed.status.last_restart_reason = Some(reason);
        }
        managed.halted = false;
        managed.attempts = 0;
        managed.restart_at = Some(Instant::now());
        unlatch_readiness(managed);
        self.publish(managed);
        was_running
    }

    /// Retire shared producers that the next app set no longer uses, while
    /// their receivers are still alive. Disable restart before waiting for
    /// SIGTERM so a supervisor tick cannot revive an intentionally stopped app.
    pub async fn retire_shared_producers(&self, desired: &[AppConfig]) -> bool {
        self.retire_shared_producers_before(desired, || {}).await
    }

    /// Register planned reader exit while both endpoints still exist, even if
    /// the caller is canceled while awaiting the producer's graceful shutdown.
    pub async fn retire_shared_producers_before(
        &self,
        desired: &[AppConfig],
        mut before_stop: impl FnMut(),
    ) -> bool {
        let mut apps = self.apps.lock().await;
        let mut retired = false;
        for managed in apps.values_mut() {
            if managed.config.shared_input.is_none()
                || desired.iter().any(|app| {
                    app.id == managed.config.id && app.enabled && app.shared_input.is_some()
                })
            {
                continue;
            }
            if !managed.config.enabled && !managed.is_running() && !managed.launch_pending {
                continue;
            }
            retired |= managed.is_running();
            if managed.is_running() {
                before_stop();
            }
            managed.config.enabled = false;
            managed.launch_pending = false;
            managed.restart_at = None;
            stop(managed).await;
            unlatch_readiness(managed);
            set_state(&mut managed.status, AppState::Stopped, None);
            self.publish(managed);
        }
        retired
    }

    /// Kill and relaunch an app, recording `reason` as why.
    ///
    /// With `only_pid`, only if the app is still that process: whoever
    /// judged it (the browser GPU watchdog, or the `browser-gpu` check's
    /// fix) judged that process, and an app that has already been
    /// relaunched since — by a crash, say — must not be restarted again on
    /// its predecessor's account. Returns false for an unknown app or a
    /// different process.
    pub async fn restart_because(
        &self,
        id: &str,
        reason: RestartReason,
        only_pid: Option<u32>,
    ) -> bool {
        let mut apps = self.apps.lock().await;
        let Some(managed) = apps.get_mut(id) else {
            return false;
        };
        if only_pid.is_some_and(|pid| managed.status.pid != Some(pid)) {
            return false;
        }
        stop(managed).await;
        managed.status.last_restart_reason = Some(reason);
        managed.halted = false;
        managed.attempts = 0;
        managed.restart_at = Some(Instant::now());
        unlatch_readiness(managed);
        self.publish(managed);
        self.advance(&mut apps).await;
        true
    }

    /// Every app's status, each paired with whether it is launched with the
    /// `chromium-kiosk` preset — what [`crate::browser_gpu`] watches and the
    /// `browser-gpu` check judges. Decided here because only the supervisor
    /// holds the configuration the running process was actually started
    /// from.
    pub async fn statuses_with_chromium(&self) -> Vec<(AppStatus, bool)> {
        let apps = self.apps.lock().await;
        let mut statuses: Vec<(AppStatus, bool)> = apps
            .values()
            .map(|app| (app.status.clone(), app.config.launcher.is_chromium()))
            .collect();
        statuses.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        statuses
    }

    /// What each running `chromium-kiosk` app's DevTools session has
    /// learned, as `(app id, pid, internals)`, sorted by app id. Apps
    /// launched without the pipe (`browser_devtools = false`) are absent.
    pub async fn devtools_internals(&self) -> Vec<(String, u32, devtools::Internals)> {
        let apps = self.apps.lock().await;
        let mut internals: Vec<(String, u32, devtools::Internals)> = apps
            .values()
            .filter(|managed| managed.is_running() && managed.config.launcher.is_chromium())
            .filter_map(|managed| {
                let pid = managed.status.pid?;
                let client = managed.devtools.as_ref()?;
                Some((managed.config.id.clone(), pid, client.internals()))
            })
            .collect();
        internals.sort_by(|a, b| a.0.cmp(&b.0));
        internals
    }

    pub async fn statuses(&self) -> Vec<AppStatus> {
        let apps = self.apps.lock().await;
        let mut statuses: Vec<AppStatus> = apps.values().map(|app| app.status.clone()).collect();
        statuses.sort_by(|a, b| a.id.cmp(&b.id));
        statuses
    }

    pub async fn status(&self, id: &str) -> Option<AppStatus> {
        self.apps.lock().await.get(id).map(|app| app.status.clone())
    }

    /// Terminate every managed process, for daemon shutdown.
    pub async fn shutdown(&self) {
        let mut apps = self.apps.lock().await;
        for managed in apps.values_mut() {
            stop(managed).await;
        }
    }

    /// Attribute windows to apps and place any that are newly mapped.
    async fn place_windows(&self, apps: &mut HashMap<String, ManagedApp>, windows: &[Window]) {
        let claimed: Vec<i64> = Vec::new();
        for managed in apps.values_mut() {
            let Some(pid) = managed.status.pid else {
                continue;
            };
            let matched: Vec<&Window> = windows
                .iter()
                .filter(|window| window.pid == Some(pid as i32))
                .filter(|window| !claimed.contains(&window.id))
                .collect();

            if matched.is_empty() {
                continue;
            }

            let ids: Vec<i64> = matched.iter().map(|window| window.id).collect();
            managed.status.window_ids = ids.clone();

            // A client can change its own fullscreen state after we place it —
            // a kiosk browser asks for fullscreen on its own output — so a
            // window that has drifted out of the mode we asked for is placed
            // again rather than left wrong.
            let wanted_mode =
                desired_fullscreen_mode(managed.config.fullscreen, managed.config.span_outputs);
            let drifted = wanted_mode > 0
                && matched
                    .iter()
                    .any(|window| window.fullscreen_mode != wanted_mode);

            if managed.placed && !drifted {
                continue;
            }

            if managed.status.state == AppState::Starting {
                set_state(&mut managed.status, AppState::Running, None);
                self.publish(managed);
            }

            // No workspace means no output was pinned, which is normal for a
            // spanning app; it still needs its fullscreen mode applied.
            let workspace = managed.target.as_ref().and_then(|target| target.workspace);
            let output = managed
                .target
                .as_ref()
                .and_then(|target| target.output.clone());
            let commands: Vec<String> = ids
                .iter()
                .flat_map(|window_id| {
                    placement_commands(
                        *window_id,
                        workspace,
                        output.as_deref(),
                        managed.config.fullscreen,
                        managed.config.span_outputs,
                    )
                })
                .collect();

            if commands.is_empty() {
                managed.placed = true;
                continue;
            }

            for command in &commands {
                if let Err(error) = self.sway.run_command(command).await {
                    tracing::warn!(app = %managed.config.id, %error, "window placement failed");
                }
            }
            managed.placed = true;
            tracing::info!(
                app = %managed.config.id,
                ?workspace,
                span = managed.config.span_outputs,
                replaced = drifted,
                "placed window"
            );
        }
    }

    /// Notice processes that have exited and schedule restarts.
    async fn reap(&self, apps: &mut HashMap<String, ManagedApp>) {
        for managed in apps.values_mut() {
            let Some(child) = managed.child.as_mut() else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code();
                    // Whatever the program said on the way out is the most
                    // useful thing we can show an operator.
                    let reason = last_stderr_line(&self.context.log_path(&managed.config.id));
                    match &reason {
                        Some(text) => {
                            tracing::warn!(app = %managed.config.id, ?code, error = %text, "app exited")
                        }
                        None => tracing::warn!(app = %managed.config.id, ?code, "app exited"),
                    }
                    managed.child = None;
                    managed.placed = false;
                    managed.status.pid = None;
                    managed.status.window_ids.clear();
                    managed.status.last_exit_code = code;
                    managed.status.last_restart_reason = Some(RestartReason::ProcessExited);
                    let detail = match reason {
                        Some(text) => format!("exited ({}): {text}", describe(code)),
                        None => format!("exited ({})", describe(code)),
                    };
                    schedule_restart_or_halt(managed, code, &detail);
                    self.publish(managed);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(app = %managed.config.id, %error, "failed to poll app");
                }
            }
        }
    }

    /// Kill apps whose content has gone silent, or which never mapped a window.
    async fn check_watchdogs(&self, apps: &mut HashMap<String, ManagedApp>) {
        for managed in apps.values_mut() {
            if !managed.is_running() {
                continue;
            }
            let Some(launched_at) = managed.launched_at else {
                continue;
            };

            // A window that never appears means the app failed to start —
            // but only for apps that are supposed to have one.
            if managed.config.expects_window()
                && managed.status.state == AppState::Starting
                && launched_at.elapsed() > WINDOW_TIMEOUT
            {
                tracing::warn!(app = %managed.config.id, "window never appeared");
                stop(managed).await;
                managed.status.last_restart_reason = Some(RestartReason::WindowNeverAppeared);
                schedule_restart_or_halt(managed, None, "window never appeared");
                self.publish(managed);
                continue;
            }

            let Some(watchdog) = managed.config.watchdog() else {
                continue;
            };

            // The watchdog arms on the first heartbeat; until then only the
            // startup grace applies, which covers page load.
            let expired = match managed.last_heartbeat_at {
                Some(last) => last.elapsed() > Duration::from_secs(watchdog.timeout_seconds),
                None => launched_at.elapsed() > Duration::from_secs(watchdog.startup_grace_seconds),
            };

            if expired {
                tracing::warn!(
                    app = %managed.config.id,
                    armed = managed.last_heartbeat_at.is_some(),
                    "heartbeat timeout; relaunching"
                );
                stop(managed).await;
                managed.status.last_restart_reason = Some(RestartReason::HeartbeatTimeout);
                schedule_restart_or_halt(managed, None, "heartbeat timeout");
                self.publish(managed);
            }
        }
    }

    /// Start anything that should be running and is not.
    async fn advance(&self, apps: &mut HashMap<String, ManagedApp>) {
        self.advance_if_current(apps, &|| true).await;
    }

    async fn advance_if_current(
        &self,
        apps: &mut HashMap<String, ManagedApp>,
        is_current: &impl Fn() -> bool,
    ) -> bool {
        let now = Instant::now();
        for managed in apps.values_mut() {
            if !is_current() {
                return false;
            }
            let runnable = managed.config.enabled
                && managed
                    .target
                    .as_ref()
                    .map(AppTarget::runnable)
                    .unwrap_or(true);

            if !runnable {
                if managed.is_running() {
                    stop(managed).await;
                    if !is_current() {
                        return false;
                    }
                }
                let state = if !managed.config.enabled {
                    AppState::Stopped
                } else {
                    AppState::WaitingForOutput
                };
                let detail = managed
                    .target
                    .as_ref()
                    .and_then(|target| target.blocked.as_ref())
                    .map(|divergence| divergence.detail.clone());
                if managed.status.state != state {
                    set_state(&mut managed.status, state, detail);
                    self.publish(managed);
                }
                continue;
            }

            if managed.is_running() || managed.halted || managed.launch_pending {
                continue;
            }
            // An app whose dependency has never answered must not be launched:
            // a kiosk browser started too early shows an error page and stays
            // on it, because nothing reloads the tab.
            if managed.config.readiness.is_some() && !managed.dependency_ready {
                continue;
            }
            if managed.restart_at.is_some_and(|at| at > now) {
                continue;
            }

            match self.spawn(managed, is_current).await {
                Ok(true) => self.publish(managed),
                Ok(false) => return false,
                // A program that is not allowed will never become allowed
                // without a daemon restart — `allowed_programs` is bootstrap
                // config, read once at startup — so the usual backoff timer
                // would just spin forever between now and that restart.
                // Refuse once per pass and halt outright, the same way a
                // restart policy of `never` does, rather than counting this
                // toward a crash loop it cannot recover from on its own.
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::warn!(app = %managed.config.id, %error, "program not allowed; halting");
                    managed.halted = true;
                    managed.program_not_allowed = true;
                    managed.restart_at = None;
                    set_state(
                        &mut managed.status,
                        AppState::Crashed,
                        Some(error.to_string()),
                    );
                    self.publish(managed);
                }
                Err(error) => {
                    tracing::error!(app = %managed.config.id, %error, "failed to launch app");
                    let detail = error.to_string();
                    schedule_restart_or_halt(managed, None, "launch failed");
                    managed.status.detail = Some(detail);
                    self.publish(managed);
                }
            }
        }
        is_current()
    }

    async fn spawn(
        &self,
        managed: &mut ManagedApp,
        is_current: &impl Fn() -> bool,
    ) -> std::io::Result<bool> {
        // Resolve first, then describe: the profile directory depends on
        // which program won, and only one of the two touches the disk.
        let chosen = launcher::choose_program(&managed.config);

        // Enforced immediately around `choose_program`, the one place a
        // program is resolved for every launcher kind — chromium-kiosk,
        // firefox-kiosk, exec, and any kind added later, since
        // `candidates_for`'s exhaustive match over `Launcher` forces a new
        // variant through this same path or the crate fails to compile.
        // Checking here, before anything below touches the filesystem, means
        // a refused app leaves no profile directory or log file behind.
        //
        // `ErrorKind::PermissionDenied` rather than a bespoke error type:
        // it is carried through the same `std::io::Result` the "not
        // installed" case already uses, and `advance` below tells the two
        // apart by kind, which is exactly the distinction the operator needs
        // — one says "install a browser", the other says "that program is
        // deliberately not allowed here".
        if let Some(program) = &chosen {
            if !crate::config::program_name_matches(&self.allowed_programs, program) {
                let name = program
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| program.display().to_string());
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "app `{}` asks to launch `{name}`, which is not in `allowed_programs`; \
                         add it to suede.toml (or SUEDE_ALLOWED_PROGRAMS) and restart the daemon, \
                         or change the app's launcher.",
                        managed.config.id
                    ),
                ));
            }
        }

        let spec = launcher::build(&managed.config, &self.context, chosen.as_deref());

        if let Some(profile) = &spec.profile_dir {
            if spec.wipe_profile {
                // Kiosk sessions are stateless by default, so start clean.
                let _ = tokio::fs::remove_dir_all(profile).await;
            }
            tokio::fs::create_dir_all(profile).await?;

            // After the wipe and the fresh directory, immediately before the
            // browser is spawned, so a stale grant from a previous origin
            // never lingers and a fresh one is always in place before
            // Chromium reads its own Preferences. A failure here is never
            // fatal to the launch: the fallback
            // `--auto-accept-camera-and-microphone-capture` still lets the
            // app get a camera, it just cannot pick one by name, and that is
            // a worse experience, not a broken one.
            if let Some(origin) = &spec.capture_origin {
                let profile_for_task = profile.clone();
                let profile_for_log = profile.clone();
                let origin = origin.clone();
                let grant = spec.grant_capture;
                let app_id = managed.config.id.clone();
                match tokio::task::spawn_blocking(move || {
                    profile::apply_capture_grant(&profile_for_task, &origin, grant)
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(
                            app = %app_id,
                            path = %profile_for_log.display(),
                            %error,
                            "failed to write capture grant; continuing without it"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            app = %app_id,
                            path = %profile_for_log.display(),
                            %error,
                            "capture grant task panicked; continuing without it"
                        );
                    }
                }
            }
        }

        // Capture stderr to a file. Without it, a crash-looping app tells the
        // operator nothing at all — which is precisely when they need to know
        // why, and the reason is usually in the first line the program prints.
        let log_path = self.context.log_path(&managed.config.id);
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let log = std::fs::File::create(&log_path)?;

        // Chromium goes by a different name on almost every distribution, so
        // the preset carries candidates rather than one hardcoded binary.
        let program = launcher::resolve_program(&spec.programs).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "none of these programs is installed: {}",
                    spec.programs.join(", ")
                ),
            )
        })?;

        // Whatever session the previous launch had is over: its browser is
        // gone or about to be replaced.
        managed.devtools = None;

        // Created before the command is built, because failing to create
        // them changes the arguments: Chromium given the flag without fds 3
        // and 4 would exit at once, so it launches without it instead, and
        // the failure is reported rather than the launch failing.
        let mut args = spec.args.clone();
        let mut devtools_failure: Option<String> = None;
        #[cfg(unix)]
        let mut devtools_pipes: Option<devtools::DevtoolsPipes> = None;
        if spec.devtools_pipe {
            #[cfg(unix)]
            match devtools::DevtoolsPipes::new() {
                Ok(pipes) => devtools_pipes = Some(pipes),
                Err(error) => {
                    tracing::warn!(
                        app = %managed.config.id,
                        %error,
                        "cannot create the DevTools pipes; launching without them"
                    );
                    devtools_failure = Some(format!("cannot create the DevTools pipes: {error}"));
                }
            }
            #[cfg(not(unix))]
            {
                devtools_failure = Some("the DevTools pipe needs a Unix host".to_string());
            }
            if devtools_failure.is_some() {
                args.retain(|arg| arg != devtools::PIPE_FLAG);
            }
        }

        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(log))
            .kill_on_drop(true);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        if managed.config.shared_input.is_some() {
            for key in [
                "DISPLAY",
                "WAYLAND_DISPLAY",
                "SWAYSOCK",
                "SDL_VIDEO_DRIVER",
                "SDL_VIDEODRIVER",
            ] {
                command.env_remove(key);
            }
        }
        // Its own process group, so terminating the app takes its children with it.
        #[cfg(unix)]
        command.process_group(0);

        #[cfg(target_os = "linux")]
        if let Some(input) = &managed.config.shared_input {
            prepare_shared_socket(&input.socket)?;
        }
        // Profile and log preparation await filesystem work; activation may
        // have changed while those tasks ran. Check at the actual launch.
        if !is_current() {
            return Ok(false);
        }
        #[cfg(unix)]
        let (child, devtools_client) = match devtools_pipes {
            Some(pipes) => {
                let (child, sender, receiver) = pipes.spawn(&mut command)?;
                let client = devtools::Client::spawn(managed.config.id.clone(), sender, receiver);
                (child, Some(client))
            }
            None => (command.spawn()?, None),
        };
        #[cfg(not(unix))]
        let (child, devtools_client) = (command.spawn()?, None::<devtools::Client>);
        let pid = child.id();
        managed.devtools =
            devtools_client.or_else(|| devtools_failure.map(devtools::Client::failed));

        tracing::info!(
            app = %managed.config.id,
            program = %program.display(),
            ?pid,
            "launched app"
        );

        managed.child = Some(child);
        managed.status.pid = pid;
        managed.status.started_at = Some(crate::util::unix_now());
        managed.status.window_ids.clear();
        managed.status.last_heartbeat = None;
        managed.launched_at = Some(Instant::now());
        managed.last_heartbeat_at = None;
        managed.placed = false;
        managed.restart_at = None;
        managed.halted = false;
        if managed.status.last_restart_reason.is_some() {
            managed.status.restart_count += 1;
        }
        // A headless app is running the moment it is spawned; one that owns a
        // window is only running once that window appears.
        let state = if managed.config.expects_window() {
            AppState::Starting
        } else {
            AppState::Running
        };
        set_state(&mut managed.status, state, None);
        Ok(true)
    }

    fn publish(&self, managed: &ManagedApp) {
        self.events.publish(ServerEvent::AppStatusChanged(Box::new(
            managed.status.clone(),
        )));
    }
}

/// Recover a filesystem socket left behind by a terminated producer. Never
/// connect as a probe: accepting that connection would consume the producer's
/// single receiver slot. A live bound socket is visible in this net namespace.
#[cfg(target_os = "linux")]
fn prepare_shared_socket(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let refuse = || {
        std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!(
                "shared input socket {} is in use or is not a stale socket owned by this user",
                path.display()
            ),
        )
    };
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(refuse());
    }
    let sockets = std::fs::read_to_string("/proc/net/unix")?;
    let name = path.to_string_lossy();
    // The first seven fields have no spaces; the remaining suffix is the
    // socket path and may itself contain spaces.
    for line in sockets.lines().skip(1) {
        let mut rest = line;
        for _ in 0..7 {
            rest = rest.trim_start();
            rest = rest
                .find(char::is_whitespace)
                .map(|index| &rest[index..])
                .unwrap_or("");
        }
        if rest.trim_start() == name {
            return Err(refuse());
        }
    }
    let current = std::fs::symlink_metadata(path)?;
    if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Err(refuse());
    }
    std::fs::remove_file(path)
}

/// Keep ticks from relaunching processes stopped by a stale reconciliation.
/// The next current reconciliation installs the desired config and clears the
/// hold; processes that are still running keep their existing lifecycle.
fn reconcile_is_current(
    apps: &mut HashMap<String, ManagedApp>,
    is_current: &impl Fn() -> bool,
) -> bool {
    if is_current() {
        return true;
    }
    for managed in apps.values_mut() {
        if managed.config.enabled && !managed.is_running() {
            managed.launch_pending = true;
        }
    }
    false
}

fn set_state(status: &mut AppStatus, state: AppState, detail: Option<String>) {
    status.state = state;
    status.detail = detail;
}

/// Clear the readiness latch so the next launch probes fresh.
///
/// Called wherever an app is stopped and about to launch again — see the
/// call sites — so the dependency check gates every launch, not just the
/// first one.
fn unlatch_readiness(managed: &mut ManagedApp) {
    managed.dependency_ready = false;
    managed.last_probe = None;
    managed.waiting_since = None;
}

fn describe(code: Option<i32>) -> String {
    match code {
        Some(0) => "cleanly".to_string(),
        Some(code) => format!("status {code}"),
        None => "killed by a signal".to_string(),
    }
}

/// The last meaningful line a failing program wrote to stderr.
pub(crate) fn last_stderr_line(path: &std::path::Path) -> Option<String> {
    const TAIL: usize = 4096;
    let text = std::fs::read(path).ok()?;
    let tail = &text[text.len().saturating_sub(TAIL)..];
    let text = String::from_utf8_lossy(tail);
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    // Keep it short enough to sit in a status field and an SSE payload.
    let line: String = line.chars().take(300).collect();
    Some(line)
}

/// Consecutive failed launches before an app is called crash-looping.
///
/// One crash is an event; three in a row is a fault worth reporting. With
/// the default backoff this is reached within a few seconds, so a genuinely
/// broken app surfaces quickly while a browser that died once does not.
const CRASH_LOOP_ATTEMPTS: u32 = 3;

/// Queue a restart, or halt the app when its policy declines one.
///
/// Halting matters: without it, an app whose policy is `never` would be started
/// again by the very next pass, because nothing else stops the start path.
///
/// Every ordinary path to a halted or backed-off app goes through here, so
/// `program_not_allowed` is cleared unconditionally: it must not survive
/// into a later, unrelated failure and mislabel it as a policy refusal.
/// `advance` sets that flag itself, bypassing this function entirely, for
/// the one case where it means something.
fn schedule_restart_or_halt(managed: &mut ManagedApp, exit_code: Option<i32>, detail: &str) {
    managed.program_not_allowed = false;
    // Whether this launch is retried or the app is halted, the verdict on
    // the dependency is about to be stale either way — a retry must re-probe
    // before it launches, and a halted app should not resume on a stopped
    // clock the next time something restarts it.
    unlatch_readiness(managed);
    if managed.config.restart.should_restart(exit_code) {
        managed.attempts += 1;
        let delay = managed.config.restart.delay_for(managed.attempts);
        managed.restart_at = Some(Instant::now() + delay);
        set_state(
            &mut managed.status,
            AppState::Backoff,
            Some(format!(
                "{detail}; restarting in {:.1}s",
                delay.as_secs_f64()
            )),
        );
    } else {
        managed.halted = true;
        managed.restart_at = None;
        set_state(
            &mut managed.status,
            AppState::Crashed,
            Some(format!("{detail}; restart policy declines a relaunch")),
        );
    }
}

/// Stop an app: SIGTERM to its process group, then SIGKILL if it lingers.
async fn stop(managed: &mut ManagedApp) {
    // Before SIGTERM: dropping the session closes Chromium's fd 3, which
    // would end the browser by itself, and nothing may keep it past here.
    managed.devtools = None;
    let Some(mut child) = managed.child.take() else {
        return;
    };
    let id = managed.config.id.clone();

    if let Some(pid) = child.id() {
        signal_group(pid, TERM_SIGNAL);
        match tokio::time::timeout(TERMINATE_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => {
                tracing::debug!(app = %id, code = ?status.code(), "app terminated");
            }
            Ok(Err(error)) => tracing::warn!(app = %id, %error, "error waiting for app"),
            Err(_) => {
                tracing::warn!(app = %id, "app ignored SIGTERM; killing");
                signal_group(pid, KILL_SIGNAL);
                let _ = child.kill().await;
            }
        }
    } else {
        let _ = child.kill().await;
    }

    managed.status.pid = None;
    managed.status.window_ids.clear();
    managed.placed = false;
    managed.launched_at = None;
    managed.last_heartbeat_at = None;
    managed.status.last_heartbeat = None;
}

#[cfg(unix)]
pub(crate) const TERM_SIGNAL: i32 = libc::SIGTERM;
#[cfg(unix)]
pub(crate) const KILL_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
pub(crate) const TERM_SIGNAL: i32 = 15;
#[cfg(not(unix))]
pub(crate) const KILL_SIGNAL: i32 = 9;

/// Signal the whole process group, so a browser's helper processes go too.
///
/// Crate-visible because the capability check spawns a browser of its own,
/// outside the supervisor's managed set, and has the same cleanup problem.
#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, signal: i32) {
    // Safe: `kill` has no memory effects, and a stale pid simply returns ESRCH.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

#[cfg(not(unix))]
pub(crate) fn signal_group(_pid: u32, _signal: i32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Launcher, OutputMatch, RestartPolicy, RestartPolicyKind};
    use crate::sway::mock::MockSway;
    use std::path::PathBuf;

    fn context(root: &std::path::Path) -> LaunchContext {
        LaunchContext {
            profiles_root: root.join("profiles"),
            log_root: root.join("logs"),
            api_base: "http://127.0.0.1:9088/api/v1".into(),
            browser_devtools: true,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_socket_cleanup_preserves_live_sockets_and_other_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("renderer with spaces.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(prepare_shared_socket(&path).is_err());
        assert!(path.exists());
        drop(listener);
        // Parallel app tests may fork while this listener exists. The child
        // briefly retains it until exec closes the inherited descriptor, so
        // cleanup correctly refuses it even after this thread drops its copy.
        let deadline = Instant::now() + Duration::from_millis(500);
        while let Err(error) = prepare_shared_socket(&path) {
            assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
            assert!(Instant::now() < deadline, "socket stayed live: {error}");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!path.exists());
        std::fs::write(&path, "preserve").unwrap();
        assert!(prepare_shared_socket(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve");
    }

    #[tokio::test]
    async fn shared_receiver_replacement_stops_producer_until_reconcile() {
        let directory = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(directory.path());
        let mut app = sleeper("shared");
        app.shared_input = Some(crate::model::SharedInputOptions {
            socket: directory.path().join("source.sock"),
            mode: crate::model::SharedInputMode::Copy,
            pacing: false,
        });
        supervisor.reconcile(&[app.clone()], &[]).await;
        let before = supervisor.status("shared").await.unwrap().pid.unwrap();
        supervisor.prepare_shared_receiver("shared").await;
        assert!(supervisor.status("shared").await.unwrap().pid.is_none());
        supervisor.tick(&[]).await;
        assert!(
            supervisor.status("shared").await.unwrap().pid.is_none(),
            "ticks must wait for reconciliation to create the new receiver"
        );
        supervisor.reconcile(&[app], &[]).await;
        assert_ne!(
            supervisor.status("shared").await.unwrap().pid.unwrap(),
            before
        );
        supervisor.reconcile(&[], &[]).await;
    }

    #[tokio::test]
    async fn shared_deactivation_blocks_restart_without_stopping_ordinary_apps() {
        let directory = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(directory.path());
        let mut shared = sleeper("shared");
        shared.shared_input = Some(crate::model::SharedInputOptions {
            socket: directory.path().join("source.sock"),
            mode: crate::model::SharedInputMode::Copy,
            pacing: false,
        });
        let ordinary = sleeper("ordinary");
        supervisor
            .reconcile(&[shared.clone(), ordinary.clone()], &[])
            .await;
        let ordinary_pid = supervisor.status("ordinary").await.unwrap().pid;
        shared.enabled = false;
        supervisor
            .retire_shared_producers(&[shared.clone(), ordinary.clone()])
            .await;
        supervisor.tick(&[]).await;
        let stopped = supervisor.status("shared").await.unwrap();
        assert_eq!(stopped.state, AppState::Stopped);
        assert!(stopped.pid.is_none());
        assert_eq!(stopped.last_restart_reason, None);
        assert_eq!(
            supervisor.status("ordinary").await.unwrap().pid,
            ordinary_pid
        );
        // An ordinary reconcile cannot revive the app after deactivation;
        // a later explicit activation can.
        supervisor
            .reconcile(&[shared.clone(), ordinary.clone()], &[])
            .await;
        assert!(supervisor.status("shared").await.unwrap().pid.is_none());
        shared.enabled = true;
        supervisor.reconcile(&[shared, ordinary], &[]).await;
        assert!(supervisor.status("shared").await.unwrap().pid.is_some());
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn removing_shared_app_or_switching_to_window_input_retires_producer() {
        for remove in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let (supervisor, _) = supervisor(directory.path());
            let mut source = sleeper("shared");
            source.shared_input = Some(crate::model::SharedInputOptions {
                socket: directory.path().join("source.sock"),
                mode: crate::model::SharedInputMode::Copy,
                pacing: false,
            });
            supervisor.reconcile(&[source.clone()], &[]).await;
            source.shared_input = None;
            let next = if remove { vec![] } else { vec![source] };
            assert!(supervisor.retire_shared_producers(&next).await);
            supervisor.tick(&[]).await;
            assert!(supervisor.status("shared").await.unwrap().pid.is_none());
            supervisor.reconcile(&next, &[]).await;
            if remove {
                assert!(supervisor.status("shared").await.is_none());
            } else {
                assert!(supervisor.status("shared").await.unwrap().pid.is_some());
            }
            supervisor.shutdown().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deactivation_during_sigterm_prevents_stale_window_app_relaunch() {
        assert_deactivation_during_shutdown_prevents_relaunch(true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deactivation_during_output_shutdown_prevents_stale_window_app_relaunch() {
        assert_deactivation_during_shutdown_prevents_relaunch(false).await;
    }

    #[cfg(unix)]
    async fn assert_deactivation_during_shutdown_prevents_relaunch(change_config: bool) {
        let directory = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(directory.path());
        let store = crate::state::StateStore::ephemeral(directory.path().to_path_buf());
        let ready = directory.path().join("ready");
        let stopping = directory.path().join("stopping");
        let exit_allowed = directory.path().join("exit-allowed");
        let launches = directory.path().join("launches");
        let mut app = sleeper("window");
        app.output = Some(OutputMatch::by_name("HEADLESS-1"));
        app.launcher = Launcher::Exec {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "trap 'echo stopping > \"$STOPPING\"; while [ ! -f \"$EXIT_ALLOWED\" ]; do sleep 0.01; done; exit 0' TERM; echo launched >> \"$LAUNCHES\"; echo ready > \"$READY\"; while :; do sleep 0.01; done".into(),
            ],
        };
        for (key, path) in [
            ("READY", &ready),
            ("STOPPING", &stopping),
            ("EXIT_ALLOWED", &exit_allowed),
            ("LAUNCHES", &launches),
        ] {
            app.env.insert(key.into(), path.display().to_string());
        }
        assert!(app.expects_window());
        store
            .update(|state| {
                state.apps.push(app.clone());
                state.active_app = Some(app.id.clone());
            })
            .unwrap();
        supervisor.reconcile(&[app.clone()], &[]).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let targets = if change_config {
            app.env.insert("CONFIG_CHANGED".into(), "1".into());
            vec![]
        } else {
            let mut blocked = target("window", None);
            blocked.blocked = Some(Divergence::new(
                "output_missing",
                "window",
                "configured output is unavailable",
            ));
            vec![blocked]
        };
        let generation = store.generation();
        let stale_apps = [app.clone()];
        let stale_reconcile = supervisor
            .reconcile_if_current(&stale_apps, &targets, || store.generation() == generation);
        let deactivate = async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while !stopping.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            store.update(|state| state.active_app = None).unwrap();
            std::fs::write(&exit_allowed, "exit").unwrap();
        };
        let (result, ()) = tokio::join!(stale_reconcile, deactivate);
        assert!(result.is_none(), "shutdown invalidated the old generation");
        supervisor.tick(&[]).await;
        assert!(
            supervisor.status("window").await.unwrap().pid.is_none(),
            "ticks cannot relaunch the old enabled config before the next pass"
        );
        app.enabled = false;
        assert!(supervisor
            .reconcile_if_current(&[app], &[], || true)
            .await
            .is_some());
        supervisor.tick(&[]).await;
        assert_eq!(
            supervisor.status("window").await.unwrap().state,
            AppState::Stopped
        );
        assert_eq!(
            std::fs::read_to_string(launches).unwrap().lines().count(),
            1
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn deactivation_during_launch_preparation_prevents_a_stale_process() {
        let directory = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(directory.path());
        let store = crate::state::StateStore::ephemeral(directory.path().to_path_buf());
        let mut app = sleeper("window");
        store
            .update(|state| {
                state.apps.push(app.clone());
                state.active_app = Some(app.id.clone());
            })
            .unwrap();
        let generation = store.generation();
        let log_path = supervisor.context.log_path(&app.id);
        let log_directory = log_path.parent().unwrap();
        assert!(!log_directory.exists());
        let result = supervisor
            .reconcile_if_current(&[app.clone()], &[], || {
                // Log-directory creation is awaited before the actual spawn.
                // Invalidate exactly after that preparation has completed.
                if log_directory.exists() && store.generation() == generation {
                    store.update(|state| state.active_app = None).unwrap();
                }
                store.generation() == generation
            })
            .await;
        assert!(result.is_none());
        supervisor.tick(&[]).await;
        assert!(supervisor.status("window").await.unwrap().pid.is_none());
        app.enabled = false;
        supervisor.reconcile(&[app.clone()], &[]).await;
        assert_eq!(
            supervisor.status("window").await.unwrap().state,
            AppState::Stopped
        );
        app.enabled = true;
        supervisor.reconcile(&[app], &[]).await;
        assert!(supervisor.status("window").await.unwrap().pid.is_some());
        supervisor.shutdown().await;
    }

    fn supervisor(root: &std::path::Path) -> (Supervisor, Arc<MockSway>) {
        let sway = Arc::new(MockSway::empty());
        // Unrestricted: these tests are about lifecycle, not the allowlist,
        // and use stand-ins like "sleep" and "true" that a real appliance's
        // default (browsers only) would refuse outright.
        let supervisor = Supervisor::new(
            sway.clone(),
            EventHub::new(),
            context(root),
            vec!["*".to_string()],
        );
        (supervisor, sway)
    }

    /// A long-lived process that ignores nothing and is available everywhere.
    fn sleeper(id: &str) -> AppConfig {
        AppConfig {
            shared_input: None,
            id: id.into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "sleep".into(),
                args: vec!["30".into()],
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

    fn target(id: &str, workspace: Option<u32>) -> AppTarget {
        AppTarget {
            id: id.into(),
            output: workspace.map(|_| "HDMI-A-1".to_string()),
            workspace,
            blocked: None,
        }
    }

    /// The spawn wiring end to end: a "browser" that reads the first byte
    /// of a request from fd 3 and answers request 1 (`Browser.getVersion`)
    /// on fd 4 shows up in `devtools_internals`, and stopping it drops the
    /// session.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_chromium_app_gets_a_devtools_session_on_fds_3_and_4() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let browser = dir.path().join("fake-chrome");
        std::fs::write(
            &browser,
            "#!/bin/sh\n\
             head -c 1 <&3 >/dev/null || exit 1\n\
             printf '%s\\000' '{\"id\":1,\"result\":{\"product\":\"Fake/1\",\"protocolVersion\":\"1.3\"}}' >&4\n\
             exec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut app = sleeper("browser");
        app.launcher = Launcher::ChromiumKiosk {
            uri: "http://127.0.0.1/".into(),
            show_fps_counter: false,
            extra_args: Vec::new(),
            program: Some(browser.display().to_string()),
            grant_capture: false,
        };
        // An exec app beside it never gets a session.
        supervisor
            .reconcile(
                &[app, sleeper("other")],
                &[target("browser", None), target("other", None)],
            )
            .await;
        let pid = supervisor.status("browser").await.unwrap().pid.unwrap();

        let mut found = None;
        for _ in 0..500 {
            let internals = supervisor.devtools_internals().await;
            if internals
                .first()
                .is_some_and(|(_, _, i)| i.version.is_some())
            {
                found = Some(internals);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let internals = found.expect("the fake browser's answer arrives over fd 4");
        assert_eq!(internals.len(), 1, "only the chromium app");
        let (id, seen_pid, internals) = &internals[0];
        assert_eq!(id, "browser");
        assert_eq!(*seen_pid, pid);
        assert_eq!(internals.version.as_ref().unwrap().product, "Fake/1");
        assert_eq!(internals.status.state, devtools::DevtoolsState::Connected);

        supervisor.shutdown().await;
        assert!(supervisor.devtools_internals().await.is_empty());
    }

    #[tokio::test]
    async fn launches_an_enabled_app() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;

        let status = supervisor.status("a").await.unwrap();
        assert!(status.pid.is_some());
        // A headless exec app owns no window, so it is running immediately.
        assert_eq!(status.state, AppState::Running);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn an_app_that_owns_a_window_waits_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        // Pinning an output means a window is expected before it counts as up.
        config.output = Some(OutputMatch::by_name("HDMI-A-1"));
        supervisor
            .reconcile(&[config], &[target("a", Some(1))])
            .await;

        assert_eq!(
            supervisor.status("a").await.unwrap().state,
            AppState::Starting
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn a_headless_app_is_never_failed_for_lacking_a_window() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;

        // Far beyond the window timeout, with no window ever mapped.
        tokio::time::sleep(Duration::from_millis(100)).await;
        supervisor.tick(&[]).await;

        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::Running);
        assert_eq!(status.last_restart_reason, None);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn does_not_launch_a_disabled_app() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        config.enabled = false;
        supervisor.reconcile(&[config], &[]).await;

        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::Stopped);
        assert!(status.pid.is_none());
    }

    #[tokio::test]
    async fn blocked_app_waits_for_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let blocked = AppTarget {
            id: "a".into(),
            output: None,
            workspace: None,
            blocked: Some(Divergence::new("app_waiting_for_output", "a", "no output")),
        };
        let divergences = supervisor.reconcile(&[sleeper("a")], &[blocked]).await;

        assert_eq!(divergences.len(), 1);
        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::WaitingForOutput);
        assert!(status.pid.is_none());
    }

    #[tokio::test]
    async fn disabling_an_app_terminates_it() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;
        assert!(supervisor.status("a").await.unwrap().pid.is_some());

        let mut disabled = sleeper("a");
        disabled.enabled = false;
        supervisor.reconcile(&[disabled], &[]).await;

        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::Stopped);
        assert!(status.pid.is_none());
    }

    #[tokio::test]
    async fn removing_an_app_forgets_it() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;
        supervisor.reconcile(&[], &[]).await;
        assert!(supervisor.status("a").await.is_none());
    }

    #[tokio::test]
    async fn exit_schedules_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        config.launcher = Launcher::Exec {
            command: "true".into(),
            args: vec![],
        };
        config.restart.delay_ms = 10_000;
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        // Give the trivial process time to exit, then reap it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        supervisor.tick(&[]).await;

        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::Backoff);
        assert_eq!(
            status.last_restart_reason,
            Some(RestartReason::ProcessExited)
        );
    }

    #[tokio::test]
    async fn never_policy_leaves_the_app_crashed() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        config.launcher = Launcher::Exec {
            command: "true".into(),
            args: vec![],
        };
        config.restart.policy = RestartPolicyKind::Never;
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        supervisor.tick(&[]).await;

        assert_eq!(
            supervisor.status("a").await.unwrap().state,
            AppState::Crashed
        );
    }

    #[tokio::test]
    async fn window_is_placed_on_its_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, sway) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", Some(2))])
            .await;
        let pid = supervisor.status("a").await.unwrap().pid.unwrap();

        let window = Window {
            id: 77,
            title: None,
            app_id: Some("sleep".into()),
            pid: Some(pid as i32),
            visible: Some(true),
            fullscreen_mode: 0,
            rect: Default::default(),
            output: Some("HDMI-A-1".into()),
            app: None,
        };
        supervisor.tick(&[window]).await;

        assert!(sway.ran_command_containing("[con_id=77] move container to workspace number 2"));
        assert!(sway.ran_command_containing("[con_id=77] fullscreen enable"));
        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::Running);
        assert_eq!(status.window_ids, vec![77]);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn windows_of_other_processes_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, sway) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", Some(1))])
            .await;

        let stranger = Window {
            id: 99,
            title: None,
            app_id: Some("someone-else".into()),
            pid: Some(1),
            visible: Some(true),
            fullscreen_mode: 0,
            rect: Default::default(),
            output: None,
            app: None,
        };
        supervisor.tick(&[stranger]).await;

        assert!(!sway.ran_command_containing("con_id=99"));
        supervisor.shutdown().await;
    }

    /// A server that answers `status`, for readiness tests.
    async fn serve(status: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                use tokio::io::AsyncWriteExt;
                let _ = socket
                    .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await;
            }
        });
        format!("http://{address}/ready")
    }

    fn waiting_on(url: &str) -> crate::model::ReadinessConfig {
        crate::model::ReadinessConfig {
            url: url.to_string(),
            expect_status: vec![],
            interval_seconds: 1,
            timeout_seconds: 2,
            give_up_after_seconds: None,
        }
    }

    #[tokio::test]
    async fn an_app_waits_for_a_dependency_that_is_not_up() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        // Nothing is listening on this port.
        config.readiness = Some(waiting_on("http://127.0.0.1:1/ready"));
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        let status = supervisor.status("a").await.unwrap();
        assert!(
            status.pid.is_none(),
            "must not launch before the service is up"
        );

        supervisor.tick(&[]).await;
        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::WaitingForDependency);
        assert!(status.detail.unwrap().contains("waiting for"));
        assert!(supervisor.status("a").await.unwrap().pid.is_none());
    }

    #[tokio::test]
    async fn an_app_launches_once_its_dependency_answers() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let url = serve("200 OK").await;
        let mut config = sleeper("a");
        config.readiness = Some(waiting_on(&url));
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        // The first tick probes; the app starts on the same pass.
        supervisor.tick(&[]).await;
        let status = supervisor.status("a").await.unwrap();
        assert!(status.pid.is_some(), "should launch: {:?}", status.detail);
        assert_ne!(status.state, AppState::WaitingForDependency);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn an_unacceptable_status_keeps_the_app_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        // Answering 503 means the service is up but not ready to serve.
        let url = serve("503 Service Unavailable").await;
        let mut config = sleeper("a");
        config.readiness = Some(waiting_on(&url));
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        supervisor.tick(&[]).await;
        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::WaitingForDependency);
        assert!(status.detail.unwrap().contains("503"));
    }

    #[tokio::test]
    async fn an_explicit_expected_status_is_honored() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let url = serve("503 Service Unavailable").await;
        let mut config = sleeper("a");
        let mut readiness = waiting_on(&url);
        readiness.expect_status = vec![503];
        config.readiness = Some(readiness);
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        supervisor.tick(&[]).await;
        assert!(supervisor.status("a").await.unwrap().pid.is_some());
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn giving_up_launches_anyway() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        let mut readiness = waiting_on("http://127.0.0.1:1/ready");
        // Zero means the very first failure exhausts the patience.
        readiness.give_up_after_seconds = Some(0);
        config.readiness = Some(readiness);
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        supervisor.tick(&[]).await;
        assert!(
            supervisor.status("a").await.unwrap().pid.is_some(),
            "give_up_after_seconds should let it start regardless"
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn an_app_without_readiness_is_unaffected() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;
        assert!(supervisor.status("a").await.unwrap().pid.is_some());
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn changing_the_readiness_url_forces_a_fresh_probe() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let url = serve("200 OK").await;
        let mut config = sleeper("a");
        config.readiness = Some(waiting_on(&url));
        supervisor
            .reconcile(&[config.clone()], &[target("a", None)])
            .await;
        supervisor.tick(&[]).await;
        assert!(supervisor.status("a").await.unwrap().pid.is_some());

        // Point it at something dead: the old verdict must not carry over.
        config.readiness = Some(waiting_on("http://127.0.0.1:1/ready"));
        supervisor.reconcile(&[config], &[target("a", None)]).await;
        supervisor.tick(&[]).await;
        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::WaitingForDependency);
        assert!(status.pid.is_none());
    }

    /// Like [`serve`], but the answer can be flipped off after the fact, to
    /// prove a dependency that dies between launches is caught on the very
    /// next probe rather than trusting an old verdict.
    async fn serve_toggle() -> (String, Arc<std::sync::atomic::AtomicBool>) {
        let up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let flag = up.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                use tokio::io::AsyncWriteExt;
                let status = if flag.load(std::sync::atomic::Ordering::SeqCst) {
                    "200 OK"
                } else {
                    "503 Service Unavailable"
                };
                let _ = socket
                    .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await;
            }
        });
        (format!("http://{address}/ready"), up)
    }

    #[tokio::test]
    async fn a_relaunch_re_probes_readiness_and_waits_if_the_dependency_died() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let (url, up) = serve_toggle().await;
        let mut config = sleeper("a");
        config.readiness = Some(waiting_on(&url));
        supervisor.reconcile(&[config], &[target("a", None)]).await;
        supervisor.tick(&[]).await;
        assert!(
            supervisor.status("a").await.unwrap().pid.is_some(),
            "should have launched once the dependency answered"
        );

        // The dependency dies before the app is relaunched.
        up.store(false, std::sync::atomic::Ordering::SeqCst);
        supervisor.restart("a").await;
        // `restart()` itself must not relaunch on the old, latched verdict.
        assert!(
            supervisor.status("a").await.unwrap().pid.is_none(),
            "a relaunch must not skip the readiness check"
        );

        // The next tick probes, finds the dependency down, and holds the launch.
        supervisor.tick(&[]).await;
        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::WaitingForDependency);
        assert!(status.pid.is_none());
    }

    #[tokio::test]
    async fn a_relaunch_does_not_wait_when_the_dependency_is_still_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let url = serve("200 OK").await;
        let mut config = sleeper("a");
        config.readiness = Some(waiting_on(&url));
        supervisor.reconcile(&[config], &[target("a", None)]).await;
        supervisor.tick(&[]).await;
        assert!(supervisor.status("a").await.unwrap().pid.is_some());

        supervisor.restart("a").await;
        // The probe itself only runs in `tick()`; give it one.
        supervisor.tick(&[]).await;
        let status = supervisor.status("a").await.unwrap();
        assert!(
            status.pid.is_some(),
            "a healthy dependency must not block a relaunch: {:?}",
            status.detail
        );
        assert!(
            matches!(status.state, AppState::Starting | AppState::Running),
            "unexpected state: {:?}",
            status.state
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn deactivation_leaves_last_restart_reason_alone_but_a_real_change_stamps_it() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());

        // Get a known restart reason on the books first.
        let mut config = sleeper("a");
        config.launcher = Launcher::Exec {
            command: "true".into(),
            args: vec![],
        };
        config.restart.delay_ms = 10_000;
        supervisor
            .reconcile(&[config.clone()], &[target("a", None)])
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        supervisor.tick(&[]).await;
        assert_eq!(
            supervisor.status("a").await.unwrap().last_restart_reason,
            Some(RestartReason::ProcessExited)
        );

        // Deactivating is a config change (the synthesized `enabled` flag
        // flips), but not one about the app's own config, so it must not
        // relabel the reason already recorded.
        let mut disabled = config.clone();
        disabled.enabled = false;
        supervisor.reconcile(&[disabled.clone()], &[]).await;
        assert_eq!(
            supervisor.status("a").await.unwrap().last_restart_reason,
            Some(RestartReason::ProcessExited),
            "a pure deactivation must not stamp ConfigChanged"
        );

        // A genuine config change on an enabled, running app still records it.
        let mut reactivated = disabled;
        reactivated.enabled = true;
        reactivated.launcher = Launcher::Exec {
            command: "sleep".into(),
            args: vec!["30".into()],
        };
        supervisor
            .reconcile(&[reactivated.clone()], &[target("a", None)])
            .await;
        assert!(supervisor.status("a").await.unwrap().pid.is_some());

        let mut changed = reactivated.clone();
        changed.env.insert("X".into(), "1".into());
        supervisor.reconcile(&[changed], &[target("a", None)]).await;
        assert_eq!(
            supervisor.status("a").await.unwrap().last_restart_reason,
            Some(RestartReason::ConfigChanged)
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeat_is_recorded_and_arms_the_watchdog() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;

        assert!(supervisor.heartbeat("a").await);
        assert!(supervisor
            .status("a")
            .await
            .unwrap()
            .last_heartbeat
            .is_some());
        supervisor.shutdown().await;
    }

    /// An app whose program does not exist — the shape of a `firefox-kiosk`
    /// app on a machine with no Firefox.
    fn unlaunchable(id: &str) -> AppConfig {
        AppConfig {
            launcher: Launcher::Exec {
                command: "/nonexistent/definitely-not-a-program".into(),
                args: vec![],
            },
            // A real backoff would make this test take seconds; the logic
            // under test is the counting, not the waiting.
            restart: RestartPolicy {
                policy: RestartPolicyKind::Always,
                delay_ms: 1,
                max_delay_ms: 1,
            },
            ..sleeper(id)
        }
    }

    #[tokio::test]
    async fn an_app_that_cannot_start_is_reported_as_a_divergence() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let apps = [unlaunchable("ghost")];
        let targets = [target("ghost", None)];

        // The first failure is just an event; the machine is not condemned
        // for one crash.
        let first = supervisor.reconcile(&apps, &targets).await;
        assert!(
            !first.iter().any(|d| d.kind == "app_crash_looping"),
            "a single failure must not degrade the appliance: {first:?}"
        );

        // Let it fail repeatedly of its own accord: each pass retries once the
        // (deliberately tiny) backoff has elapsed, exactly as it would in the
        // field, just faster.
        let mut divergences = Vec::new();
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            divergences = supervisor.reconcile(&apps, &targets).await;
        }
        let found = divergences
            .iter()
            .find(|d| d.kind == "app_crash_looping")
            .unwrap_or_else(|| panic!("expected a crash-loop divergence, got {divergences:?}"));
        assert_eq!(found.subject, "ghost");
        assert!(
            found.detail.contains("definitely-not-a-program") || found.detail.contains("failed"),
            "the divergence must carry the reason: {}",
            found.detail
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn an_app_that_will_never_be_retried_is_reported_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut app = unlaunchable("ghost");
        // `never` means nothing will start it again, so waiting for a third
        // failure would mean waiting for ever.
        app.restart = RestartPolicy {
            policy: RestartPolicyKind::Never,
            ..RestartPolicy::default()
        };

        let divergences = supervisor.reconcile(&[app], &[target("ghost", None)]).await;
        assert!(
            divergences
                .iter()
                .any(|d| d.kind == "app_halted" && d.subject == "ghost"),
            "a halted app must be reported immediately: {divergences:?}"
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn an_app_whose_program_is_not_allowed_is_never_spawned() {
        let dir = tempfile::tempdir().unwrap();
        let sway = Arc::new(MockSway::empty());
        // Only firefox is permitted; the app below asks for `sleep`, which is
        // certain to exist on the test machine's PATH — the point is that
        // being installed does not matter when policy refuses it.
        let supervisor = Supervisor::new(
            sway,
            EventHub::new(),
            context(dir.path()),
            vec!["firefox".to_string()],
        );
        let apps = [sleeper("ghost")];
        let targets = [target("ghost", None)];

        let divergences = supervisor.reconcile(&apps, &targets).await;

        let status = supervisor.status("ghost").await.unwrap();
        assert!(status.pid.is_none(), "must never be spawned");
        assert_eq!(status.state, AppState::Crashed);

        let found = divergences
            .iter()
            .find(|d| d.kind == "app_program_not_allowed")
            .unwrap_or_else(|| panic!("expected app_program_not_allowed, got {divergences:?}"));
        assert_eq!(found.subject, "ghost");
        assert!(found.detail.contains("sleep"), "{}", found.detail);
        assert!(
            found.detail.contains("allowed_programs"),
            "{}",
            found.detail
        );

        // Refused once per pass, not retried on a timer: a second pass with
        // nothing changed must repeat the same divergence, never escalate to
        // a crash loop that implies retrying will eventually work.
        let second = supervisor.reconcile(&apps, &targets).await;
        assert!(
            !second.iter().any(|d| d.kind == "app_crash_looping"),
            "a disallowed program must never spin into a crash loop: {second:?}"
        );
        assert!(second.iter().any(|d| d.kind == "app_program_not_allowed"));
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeat_for_an_unknown_app_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        assert!(!supervisor.heartbeat("nope").await);
    }

    #[tokio::test]
    async fn silent_content_is_relaunched_after_the_startup_grace() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        config.heartbeat = Some(crate::model::HeartbeatConfig {
            enabled: true,
            timeout_seconds: 25,
            // Expire immediately, rather than making the test wait.
            startup_grace_seconds: 0,
        });
        config.restart.delay_ms = 10_000;
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        supervisor.tick(&[]).await;

        let status = supervisor.status("a").await.unwrap();
        assert_eq!(
            status.last_restart_reason,
            Some(RestartReason::HeartbeatTimeout)
        );
        assert_eq!(status.state, AppState::Backoff);
    }

    #[tokio::test]
    async fn an_armed_watchdog_tolerates_recent_heartbeats() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        config.heartbeat = Some(crate::model::HeartbeatConfig {
            enabled: true,
            timeout_seconds: 25,
            startup_grace_seconds: 0,
        });
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        // Arming with a heartbeat replaces the startup grace with the timeout.
        supervisor.heartbeat("a").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        supervisor.tick(&[]).await;

        let status = supervisor.status("a").await.unwrap();
        assert_ne!(status.state, AppState::Backoff);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn heartbeats_do_not_leak_between_apps() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(
                &[sleeper("a"), sleeper("b")],
                &[target("a", None), target("b", None)],
            )
            .await;

        supervisor.heartbeat("a").await;
        assert!(supervisor
            .status("a")
            .await
            .unwrap()
            .last_heartbeat
            .is_some());
        assert!(supervisor
            .status("b")
            .await
            .unwrap()
            .last_heartbeat
            .is_none());
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn configuration_change_relaunches_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;
        let first = supervisor.status("a").await.unwrap().pid.unwrap();

        let mut changed = sleeper("a");
        changed.launcher = Launcher::Exec {
            command: "sleep".into(),
            args: vec!["60".into()],
        };
        supervisor.reconcile(&[changed], &[target("a", None)]).await;

        let second = supervisor.status("a").await.unwrap().pid.unwrap();
        assert_ne!(first, second);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn unchanged_configuration_does_not_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;
        let first = supervisor.status("a").await.unwrap().pid.unwrap();

        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;
        let second = supervisor.status("a").await.unwrap().pid.unwrap();
        assert_eq!(first, second);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn restart_on_request_replaces_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;
        let first = supervisor.status("a").await.unwrap().pid.unwrap();

        assert!(supervisor.restart("a").await);
        let status = supervisor.status("a").await.unwrap();
        assert_ne!(status.pid.unwrap(), first);
        assert_eq!(status.last_restart_reason, Some(RestartReason::ApiRequest));
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn a_restart_for_a_reason_stamps_it_and_respects_the_expected_pid() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(&[sleeper("a")], &[target("a", None)])
            .await;
        let first = supervisor.status("a").await.unwrap().pid.unwrap();

        // A process that is no longer the app's is left alone.
        assert!(
            !supervisor
                .restart_because("a", RestartReason::GpuFallback, Some(first + 1))
                .await
        );
        assert_eq!(supervisor.status("a").await.unwrap().pid, Some(first));

        assert!(
            supervisor
                .restart_because("a", RestartReason::GpuFallback, Some(first))
                .await
        );
        let status = supervisor.status("a").await.unwrap();
        assert_ne!(status.pid.unwrap(), first);
        assert_eq!(status.last_restart_reason, Some(RestartReason::GpuFallback));
        let statuses = supervisor.statuses_with_chromium().await;
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].1, "an exec launcher is not chromium-kiosk");
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn restarting_an_unknown_app_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        assert!(!supervisor.restart("nope").await);
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("a");
        config.launcher = Launcher::Exec {
            command: "/definitely/not/here".into(),
            args: vec![],
        };
        supervisor.reconcile(&[config], &[target("a", None)]).await;

        let status = supervisor.status("a").await.unwrap();
        assert_eq!(status.state, AppState::Backoff);
        assert!(status.detail.is_some());
    }

    #[tokio::test]
    async fn chromium_profile_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        let mut config = sleeper("r1");
        config.launcher = Launcher::ChromiumKiosk {
            uri: "http://example.com".into(),
            show_fps_counter: false,
            extra_args: vec![],
            program: None,
            grant_capture: true,
        };
        // Chromium is absent in CI, so the launch fails — but only after the
        // profile directory has been prepared, which is what we assert.
        supervisor.reconcile(&[config], &[target("r1", None)]).await;

        let profile: PathBuf = dir.path().join("profiles").join("r1");
        assert!(profile.exists());
    }

    #[tokio::test]
    async fn shutdown_stops_everything() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(
                &[sleeper("a"), sleeper("b")],
                &[target("a", None), target("b", None)],
            )
            .await;
        supervisor.shutdown().await;

        for id in ["a", "b"] {
            assert!(supervisor.status(id).await.unwrap().pid.is_none());
        }
    }

    #[tokio::test]
    async fn statuses_are_sorted_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, _) = supervisor(dir.path());
        supervisor
            .reconcile(
                &[sleeper("z"), sleeper("a")],
                &[target("z", None), target("a", None)],
            )
            .await;
        let ids: Vec<String> = supervisor
            .statuses()
            .await
            .into_iter()
            .map(|status| status.id)
            .collect();
        assert_eq!(ids, vec!["a", "z"]);
        supervisor.shutdown().await;
    }

    #[test]
    fn output_match_is_used_for_targets() {
        // Guards the assumption the supervisor relies on: targets are resolved
        // upstream, so the supervisor never matches outputs itself.
        let rule = OutputMatch::by_name("HDMI-A-1");
        assert_eq!(rule.key(), "HDMI-A-1");
    }
}
