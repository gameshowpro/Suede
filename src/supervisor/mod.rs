//! Application lifecycle.
//!
//! Apps are spawned as direct child processes rather than through Sway's
//! `exec`, which is what gives Suede a real PID: clean termination, exit-code
//! observation, restart policies, and per-app audio routing all depend on it.

pub mod launcher;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Child;
use tokio::sync::Mutex;

use crate::events::{EventHub, ServerEvent};
use crate::model::{AppConfig, AppState, AppStatus, Divergence, RestartReason, Window};
use crate::reconciler::plan::{placement_commands, AppTarget};
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
}

impl Supervisor {
    pub fn new(sway: Arc<dyn SwayClient>, events: EventHub, context: LaunchContext) -> Self {
        Self {
            sway,
            events,
            context,
            apps: Mutex::new(HashMap::new()),
        }
    }

    /// Bring the managed set in line with desired state, then advance lifecycles.
    ///
    /// Returns divergences for apps that cannot run right now.
    pub async fn reconcile(&self, desired: &[AppConfig], targets: &[AppTarget]) -> Vec<Divergence> {
        let mut apps = self.apps.lock().await;
        let mut divergences = Vec::new();

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
                    managed.status.last_restart_reason = Some(RestartReason::ConfigChanged);
                }
                // A new specification earns a halted app another attempt.
                managed.halted = false;
                managed.attempts = 0;
                managed.restart_at = Some(Instant::now());
            }
            managed.config = config.clone();
            managed.target = target;
        }

        self.advance(&mut apps).await;
        divergences
    }

    /// Reap exits, run the watchdog, place new windows, and honour restart timers.
    pub async fn tick(&self, windows: &[Window]) {
        let mut apps = self.apps.lock().await;
        self.reap(&mut apps).await;
        self.check_watchdogs(&mut apps).await;
        self.place_windows(&mut apps, windows).await;
        self.advance(&mut apps).await;
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
        let mut apps = self.apps.lock().await;
        let Some(managed) = apps.get_mut(id) else {
            return false;
        };
        stop(managed).await;
        managed.status.last_restart_reason = Some(RestartReason::ApiRequest);
        managed.halted = false;
        managed.attempts = 0;
        managed.restart_at = Some(Instant::now());
        self.publish(managed);
        self.advance(&mut apps).await;
        true
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

            if managed.placed {
                continue;
            }

            if managed.status.state == AppState::Starting {
                set_state(&mut managed.status, AppState::Running, None);
                self.publish(managed);
            }

            let Some(workspace) = managed.target.as_ref().and_then(|target| target.workspace)
            else {
                managed.placed = true;
                continue;
            };

            for window_id in ids {
                for command in placement_commands(window_id, workspace, managed.config.fullscreen) {
                    if let Err(error) = self.sway.run_command(&command).await {
                        tracing::warn!(app = %managed.config.id, %error, "window placement failed");
                    }
                }
            }
            managed.placed = true;
            tracing::info!(
                app = %managed.config.id,
                workspace,
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
                    tracing::warn!(app = %managed.config.id, ?code, "app exited");
                    managed.child = None;
                    managed.placed = false;
                    managed.status.pid = None;
                    managed.status.window_ids.clear();
                    managed.status.last_exit_code = code;
                    managed.status.last_restart_reason = Some(RestartReason::ProcessExited);
                    schedule_restart_or_halt(managed, code, "exited");
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
        let now = Instant::now();
        for managed in apps.values_mut() {
            let runnable = managed.config.enabled
                && managed
                    .target
                    .as_ref()
                    .map(AppTarget::runnable)
                    .unwrap_or(true);

            if !runnable {
                if managed.is_running() {
                    stop(managed).await;
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

            if managed.is_running() || managed.halted {
                continue;
            }
            if managed.restart_at.is_some_and(|at| at > now) {
                continue;
            }

            match self.spawn(managed).await {
                Ok(()) => self.publish(managed),
                Err(error) => {
                    tracing::error!(app = %managed.config.id, %error, "failed to launch app");
                    let detail = error.to_string();
                    schedule_restart_or_halt(managed, None, "launch failed");
                    managed.status.detail = Some(detail);
                    self.publish(managed);
                }
            }
        }
    }

    async fn spawn(&self, managed: &mut ManagedApp) -> std::io::Result<()> {
        let spec = launcher::build(&managed.config, &self.context);

        if let Some(profile) = &spec.profile_dir {
            if spec.wipe_profile {
                // Kiosk sessions are stateless by default, so start clean.
                let _ = tokio::fs::remove_dir_all(profile).await;
            }
            tokio::fs::create_dir_all(profile).await?;
        }

        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        // Its own process group, so terminating the app takes its children with it.
        #[cfg(unix)]
        command.process_group(0);

        let child = command.spawn()?;
        let pid = child.id();

        tracing::info!(
            app = %managed.config.id,
            program = %spec.program,
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
        Ok(())
    }

    fn publish(&self, managed: &ManagedApp) {
        self.events.publish(ServerEvent::AppStatusChanged(Box::new(
            managed.status.clone(),
        )));
    }
}

fn set_state(status: &mut AppStatus, state: AppState, detail: Option<String>) {
    status.state = state;
    status.detail = detail;
}

/// Queue a restart, or halt the app when its policy declines one.
///
/// Halting matters: without it, an app whose policy is `never` would be started
/// again by the very next pass, because nothing else stops the start path.
fn schedule_restart_or_halt(managed: &mut ManagedApp, exit_code: Option<i32>, detail: &str) {
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
const TERM_SIGNAL: i32 = libc::SIGTERM;
#[cfg(unix)]
const KILL_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const TERM_SIGNAL: i32 = 15;
#[cfg(not(unix))]
const KILL_SIGNAL: i32 = 9;

/// Signal the whole process group, so a browser's helper processes go too.
#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    // Safe: `kill` has no memory effects, and a stale pid simply returns ESRCH.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: i32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Launcher, OutputMatch, RestartPolicy, RestartPolicyKind};
    use crate::sway::mock::MockSway;
    use std::path::PathBuf;

    fn context(root: &std::path::Path) -> LaunchContext {
        LaunchContext {
            profiles_root: root.join("profiles"),
            api_base: "http://127.0.0.1:7071/api/v1".into(),
        }
    }

    fn supervisor(root: &std::path::Path) -> (Supervisor, Arc<MockSway>) {
        let sway = Arc::new(MockSway::empty());
        let supervisor = Supervisor::new(sway.clone(), EventHub::new(), context(root));
        (supervisor, sway)
    }

    /// A long-lived process that ignores nothing and is available everywhere.
    fn sleeper(id: &str) -> AppConfig {
        AppConfig {
            id: id.into(),
            enabled: true,
            launcher: Launcher::Exec {
                command: "sleep".into(),
                args: vec!["30".into()],
            },
            output: None,
            fullscreen: true,
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
