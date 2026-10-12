//! GPU and Chromium telemetry for the System tab's GPU and Chromium panels.
//!
//! One background task, [`Telemetry`], every [`POLL`], gathers only sources
//! that cost next to nothing: a `/proc` walk of every app's process tree
//! ([`crate::procs`]), one `nvidia-smi` pair of invocations
//! ([`crate::nvidia_smi`], measured at about 80 ms of wall time and
//! negligible CPU), and the state the per-app DevTools sessions already
//! hold in memory ([`crate::devtools`]). Nothing here talks to a browser.
//!
//! [`assemble`] is the pure join of those inputs, so the attribution rules
//! (which Suede app and Chromium role own a GPU process, and what the
//! watchdog and DevTools know about each running `chromium-kiosk` app) can
//! be tested with fake inputs. The task publishes the result on the
//! [`Snapshot`] and sends an event only when a value changed, so an idle
//! appliance produces no event traffic.
//!
//! A machine without `nvidia-smi` is not re-probed every poll: once the
//! binary is known to be missing, the task looks again only every
//! [`REPROBE_MISSING`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use utoipa::ToSchema;

use crate::browser_gpu::{AppGpuReport, Rendering};
use crate::devtools::{
    BrowserGpuInfo, BrowserVersion, DevtoolsState, DevtoolsStatus, Internals, MediaPlayer,
};
use crate::events::{EventHub, ServerEvent};
use crate::model::{AppState, AppStatus};
use crate::nvidia_smi::{self, GpuTelemetry};
use crate::procs::{BrowserProcess, ProcessSampleSet, ProcessSampler};
use crate::snapshot::Snapshot;
use crate::supervisor::Supervisor;

/// How often [`Telemetry::run`] samples.
pub const POLL: Duration = Duration::from_secs(5);

/// How often to look for `nvidia-smi` again once it was found missing.
pub const REPROBE_MISSING: Duration = Duration::from_secs(60);

/// The `reason` [`nvidia_smi::sample`] gives when the binary is absent.
const NOT_INSTALLED: &str = "nvidia-smi is not installed";

/// The `detail` of a DevTools status when the bootstrap key is off.
const DEVTOOLS_OFF: &str = "browser_devtools = false";

/// Whether Chromium's GPU process renders on the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum BrowserRendering {
    /// The GPU process has no software-rendering switch.
    Hardware,
    /// `--use-gl=disabled` or SwiftShader.
    Software,
}

impl From<Rendering> for BrowserRendering {
    fn from(rendering: Rendering) -> Self {
        match rendering {
            Rendering::Hardware => Self::Hardware,
            Rendering::Software => Self::Software,
        }
    }
}

/// Everything known about one running `chromium-kiosk` app's browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserReport {
    /// The app's id.
    pub app: String,
    /// The process Suede launched.
    pub pid: u32,
    /// Unix seconds the process was spawned.
    pub started_at: Option<u64>,
    /// Unix seconds of this sample.
    pub sampled_at: u64,
    /// Whether the GPU process renders in hardware, once the GPU watchdog
    /// has judged it.
    pub rendering: Option<BrowserRendering>,
    /// GPU process crashes since launch.
    pub gpu_process_crashes: u32,
    /// The browser's process tree.
    pub processes: Vec<BrowserProcess>,
    /// State of the DevTools session the next fields come from.
    pub devtools: DevtoolsStatus,
    /// What `Browser.getVersion` answered.
    pub version: Option<BrowserVersion>,
    /// The browser's own view of the GPU.
    pub gpu: Option<BrowserGpuInfo>,
    /// Media players the browser reported.
    pub media_players: Vec<MediaPlayer>,
}

/// Everything one poll needs besides the clock.
pub struct Inputs<'a> {
    /// Every app's status, and whether it is a `chromium-kiosk` app.
    pub apps: &'a [(AppStatus, bool)],
    /// The `/proc` walk of every app's tree.
    pub processes: &'a ProcessSampleSet,
    /// The GPU watchdog's reports.
    pub gpu_reports: &'a [AppGpuReport],
    /// DevTools state per `(app, pid)`.
    pub internals: &'a [(String, u32, Internals)],
    /// The `browser_devtools` bootstrap key.
    pub browser_devtools: bool,
}

/// Attribute each GPU process to the app (and Chromium role) that owns it.
pub fn annotate_gpu(gpu: &mut GpuTelemetry, processes: &ProcessSampleSet) {
    for process in &mut gpu.processes {
        if let Some((app, role)) = processes.owner.get(&process.pid) {
            process.app = Some(app.clone());
            process.role = Some(role.clone());
        }
    }
}

/// One report per running `chromium-kiosk` app with a process.
pub fn assemble(inputs: &Inputs<'_>, now: u64) -> Vec<BrowserReport> {
    let mut reports = Vec::new();
    for (status, chromium) in inputs.apps {
        let Some(pid) = status.pid else { continue };
        if !*chromium || !matches!(status.state, AppState::Running | AppState::Starting) {
            continue;
        }
        let watchdog = inputs
            .gpu_reports
            .iter()
            .find(|report| report.app == status.id && report.pid == pid);
        let internals = if !inputs.browser_devtools {
            Internals {
                status: DevtoolsStatus::new(
                    DevtoolsState::Disabled,
                    Some(DEVTOOLS_OFF.to_string()),
                ),
                ..Internals::default()
            }
        } else {
            inputs
                .internals
                .iter()
                .find(|(app, internals_pid, _)| *app == status.id && *internals_pid == pid)
                .map(|(_, _, internals)| internals.clone())
                .unwrap_or_else(|| Internals {
                    status: DevtoolsStatus::new(
                        DevtoolsState::Connecting,
                        Some("waiting for the DevTools session".to_string()),
                    ),
                    ..Internals::default()
                })
        };
        reports.push(BrowserReport {
            app: status.id.clone(),
            pid,
            started_at: status.started_at,
            sampled_at: now,
            rendering: watchdog.and_then(|report| report.rendering).map(Into::into),
            gpu_process_crashes: watchdog.map_or(0, |report| report.crashes),
            processes: inputs
                .processes
                .by_app
                .get(&status.id)
                .cloned()
                .unwrap_or_default(),
            devtools: internals.status,
            version: internals.version,
            gpu: internals.gpu,
            media_players: internals.players,
        });
    }
    reports
}

/// The background sampler. See the module documentation.
pub struct Telemetry {
    supervisor: Arc<Supervisor>,
    snapshot: Arc<Snapshot>,
    events: EventHub,
    browser_devtools: bool,
    sampler: ProcessSampler,
    /// When `nvidia-smi` was last found missing, to rate-limit re-probing.
    missing_since: Option<Instant>,
}

impl Telemetry {
    /// A sampler reading the real `/proc`.
    pub fn new(
        supervisor: Arc<Supervisor>,
        snapshot: Arc<Snapshot>,
        events: EventHub,
        browser_devtools: bool,
    ) -> Self {
        Self {
            supervisor,
            snapshot,
            events,
            browser_devtools,
            sampler: ProcessSampler::new(),
            missing_since: None,
        }
    }

    /// Sample once and publish what changed.
    pub async fn step(&mut self) {
        let apps = self.supervisor.statuses_with_chromium().await;
        let roots: Vec<(String, u32)> = apps
            .iter()
            .filter_map(|(status, _)| status.pid.map(|pid| (status.id.clone(), pid)))
            .collect();
        let processes = self.sampler.sample(&roots);

        let probe = match self.missing_since {
            Some(since) => since.elapsed() >= REPROBE_MISSING,
            None => true,
        };
        let mut gpu = if probe {
            let sample = nvidia_smi::sample().await;
            self.missing_since = (!sample.available
                && sample.reason.as_deref() == Some(NOT_INSTALLED))
            .then(Instant::now);
            sample
        } else {
            self.snapshot.gpu()
        };
        annotate_gpu(&mut gpu, &processes);

        let gpu_reports = self.snapshot.browser_gpu();
        let internals = self.supervisor.devtools_internals().await;
        let browsers = assemble(
            &Inputs {
                apps: &apps,
                processes: &processes,
                gpu_reports: &gpu_reports,
                internals: &internals,
                browser_devtools: self.browser_devtools,
            },
            crate::util::unix_now(),
        );

        if self.snapshot.set_gpu(gpu.clone()) {
            self.events.publish(ServerEvent::GpuChanged(Box::new(gpu)));
        }
        if self.snapshot.set_browsers(browsers.clone()) {
            self.events.publish(ServerEvent::BrowsersChanged(browsers));
        }
    }

    /// Sample every [`POLL`] until shutdown, skipping missed ticks.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut poll = tokio::time::interval(POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = poll.tick() => self.step().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvidia_smi::{GpuProcessKind, GpuProcessSample};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn gpu_process(pid: u32) -> GpuProcessSample {
        GpuProcessSample {
            gpu_index: 0,
            pid,
            kind: GpuProcessKind::Graphics,
            command: "chrome".into(),
            sm_percent: Some(3),
            memory_percent: None,
            encoder_percent: None,
            decoder_percent: Some(12),
            framebuffer_mib: Some(300),
            app: None,
            role: None,
        }
    }

    fn browser_process(pid: u32, role: &str) -> BrowserProcess {
        BrowserProcess {
            pid,
            role: role.into(),
            subtype: None,
            cpu_percent: Some(1.5),
            rss_mib: 100,
        }
    }

    fn sample_set() -> ProcessSampleSet {
        let mut by_app = HashMap::new();
        by_app.insert(
            "wall".to_string(),
            vec![
                browser_process(100, "browser"),
                browser_process(101, "gpu-process"),
            ],
        );
        let mut owner = HashMap::new();
        owner.insert(100, ("wall".to_string(), "browser".to_string()));
        owner.insert(101, ("wall".to_string(), "gpu-process".to_string()));
        ProcessSampleSet { by_app, owner }
    }

    fn running(id: &str, pid: u32) -> AppStatus {
        let mut status = AppStatus::stopped(id);
        status.state = AppState::Running;
        status.pid = Some(pid);
        status.started_at = Some(1000);
        status
    }

    fn watchdog(app: &str, pid: u32) -> AppGpuReport {
        AppGpuReport {
            app: app.into(),
            pid,
            rendering: Some(Rendering::Software),
            crashes: 3,
            software_since: Some(1),
            seen_hardware: true,
            restarts_this_hour: 0,
            log_path: PathBuf::from("/tmp/log"),
        }
    }

    fn fake_internals() -> Internals {
        Internals {
            status: DevtoolsStatus::new(DevtoolsState::Connected, None),
            version: Some(BrowserVersion {
                product: "Chrome/151.0.7922.71".into(),
                protocol_version: "1.3".into(),
                js_version: None,
                user_agent: None,
            }),
            gpu: None,
            players: Vec::new(),
        }
    }

    #[test]
    fn gpu_processes_are_attributed_to_apps_and_roles() {
        let mut gpu = GpuTelemetry {
            available: true,
            reason: None,
            sampled_at: Some(5),
            gpus: Vec::new(),
            processes: vec![gpu_process(101), gpu_process(999)],
        };
        annotate_gpu(&mut gpu, &sample_set());
        assert_eq!(gpu.processes[0].app.as_deref(), Some("wall"));
        assert_eq!(gpu.processes[0].role.as_deref(), Some("gpu-process"));
        assert_eq!(gpu.processes[1].app, None, "a stranger stays unattributed");
        assert_eq!(gpu.processes[1].role, None);
    }

    #[test]
    fn a_browser_report_joins_processes_watchdog_and_devtools() {
        let apps = vec![
            (running("wall", 100), true),
            (running("tool", 200), false),
            (AppStatus::stopped("idle"), true),
        ];
        let processes = sample_set();
        let gpu_reports = vec![watchdog("wall", 100), watchdog("wall", 55)];
        let internals = vec![("wall".to_string(), 100, fake_internals())];
        let reports = assemble(
            &Inputs {
                apps: &apps,
                processes: &processes,
                gpu_reports: &gpu_reports,
                internals: &internals,
                browser_devtools: true,
            },
            42,
        );
        assert_eq!(reports.len(), 1, "only the running chromium app");
        let report = &reports[0];
        assert_eq!(report.app, "wall");
        assert_eq!(report.pid, 100);
        assert_eq!(report.started_at, Some(1000));
        assert_eq!(report.sampled_at, 42);
        assert_eq!(report.rendering, Some(BrowserRendering::Software));
        assert_eq!(report.gpu_process_crashes, 3);
        assert_eq!(report.processes.len(), 2);
        assert_eq!(report.devtools.state, DevtoolsState::Connected);
        assert_eq!(
            report.version.as_ref().map(|v| v.product.as_str()),
            Some("Chrome/151.0.7922.71")
        );
    }

    #[test]
    fn a_stale_watchdog_report_for_another_pid_is_ignored() {
        let apps = vec![(running("wall", 100), true)];
        let processes = sample_set();
        let gpu_reports = vec![watchdog("wall", 55)];
        let reports = assemble(
            &Inputs {
                apps: &apps,
                processes: &processes,
                gpu_reports: &gpu_reports,
                internals: &[],
                browser_devtools: true,
            },
            1,
        );
        assert_eq!(reports[0].rendering, None);
        assert_eq!(reports[0].gpu_process_crashes, 0);
        assert_eq!(reports[0].devtools.state, DevtoolsState::Connecting);
    }

    #[test]
    fn devtools_off_reports_disabled() {
        let apps = vec![(running("wall", 100), true)];
        let processes = sample_set();
        let reports = assemble(
            &Inputs {
                apps: &apps,
                processes: &processes,
                gpu_reports: &[],
                internals: &[],
                browser_devtools: false,
            },
            1,
        );
        assert_eq!(
            reports[0].devtools,
            DevtoolsStatus::new(
                DevtoolsState::Disabled,
                Some("browser_devtools = false".into())
            )
        );
        assert_eq!(
            reports[0].processes.len(),
            2,
            "the process table still appears"
        );
        assert!(reports[0].media_players.is_empty());
    }
}
