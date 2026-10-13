//! GPU, CPU and Chromium telemetry for the System tab's panels.
//!
//! One background task, [`Telemetry`], every [`POLL`], gathers only sources
//! that cost next to nothing: a `/proc` walk of every app's process tree
//! ([`crate::procs`]), the machine's own counters in `/proc` and `/sys`
//! ([`crate::cpu`]), one `nvidia-smi` trio of invocations
//! ([`crate::nvidia_smi`], about 70 ms of wall time and negligible CPU), and
//! the state the per-app DevTools sessions already hold in memory
//! ([`crate::devtools`]). Nothing here talks to a browser.
//!
//! # Loop shape
//!
//! A tick takes the clock once and stamps every sample with it, so the
//! charts line up point for point. The `/proc` and `/sys` samplers are
//! synchronous reads of a few small files (about a millisecond together), so
//! they run inline: spawning a task per sampler would cost more in
//! scheduling and channel traffic than it saves, and would add nothing to
//! alignment. The one slow source is `nvidia-smi`, three child processes
//! that mostly wait; [`nvidia_smi::sample`] runs those concurrently with
//! `tokio::join!` (each command keeping its own timeout), so the tick waits
//! for the slowest, about 70 ms, instead of the sum, about 150 ms, for three
//! lines of code. Everything else is a snapshot or lock read.
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

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use utoipa::ToSchema;

use crate::browser_gpu::{AppGpuReport, Rendering};
use crate::cpu::{CpuSampler, SOLO_PREFIX};
use crate::devtools::{
    BrowserGpuInfo, BrowserVersion, DevtoolsState, DevtoolsStatus, Internals, MediaPlayer, Playhead,
};
use crate::diagnostics::{history_capacity, stale_after, HISTORY_WINDOW};
use crate::events::{EventHub, ServerEvent};
use crate::model::{AppState, AppStatus, Output, ProjectionStats};
use crate::nvidia_smi::{self, GpuHistoryPoint, GpuHistorySample, GpuTelemetry};
use crate::procs::{BrowserProcess, ProcessSampleSet, ProcessSampler};
use crate::snapshot::Snapshot;
use crate::supervisor::Supervisor;
use crate::sway::SwayClient;

/// How often [`Telemetry::run`] samples.
pub const POLL: Duration = crate::diagnostics::SAMPLE_INTERVAL;

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
    /// Media elements in the page that no player in `media_players` matched.
    pub unmatched_elements: Vec<Playhead>,
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

/// Append one point built from `sample.gpus` and drop what is too old or over
/// capacity.
///
/// Trimming by age as well as count keeps the graph honest after a long gap
/// (for example `nvidia-smi` missing, then back): stale points would otherwise
/// sit beside fresh ones as if they were adjacent.
pub fn push_history(buffer: &mut VecDeque<GpuHistoryPoint>, sample: &GpuTelemetry, now: u64) {
    buffer.push_back(GpuHistoryPoint {
        at: now,
        gpus: sample
            .gpus
            .iter()
            .map(|gpu| GpuHistorySample {
                index: gpu.index,
                utilization_gpu_percent: gpu.utilization_gpu_percent,
                utilization_memory_percent: gpu.utilization_memory_percent,
                utilization_decoder_percent: gpu.utilization_decoder_percent,
                utilization_encoder_percent: gpu.utilization_encoder_percent,
                memory_used_mib: gpu.memory_used_mib,
                memory_total_mib: gpu.memory_total_mib,
                temperature_c: gpu.temperature_c,
                power_draw_w: gpu.power_draw_w,
                clock_sm_mhz: gpu.clock_sm_mhz,
                pcie_generation: gpu.pcie_generation,
                pcie_width: gpu.pcie_width,
                pcie_rx_mb_s: gpu.pcie_rx_mb_s,
                pcie_tx_mb_s: gpu.pcie_tx_mb_s,
                pcie_rx_percent: gpu.pcie_rx_percent,
                pcie_tx_percent: gpu.pcie_tx_percent,
            })
            .collect(),
    });
    trim_history(buffer, now, |point| point.at);
}

/// Drop points older than [`HISTORY_WINDOW`], then the oldest over
/// [`history_capacity`]. Shared by the GPU and pipeline histories so the two
/// charts always cover the same window.
pub(crate) fn trim_history<T>(buffer: &mut VecDeque<T>, now: u64, at: impl Fn(&T) -> u64) {
    while buffer
        .front()
        .is_some_and(|point| now.saturating_sub(at(point)) > HISTORY_WINDOW.as_secs())
    {
        buffer.pop_front();
    }
    while buffer.len() > history_capacity() {
        buffer.pop_front();
    }
}

/// Record a fresh poll in `buffer` and publish the buffer on `gpu`.
///
/// Only a poll that actually ran (`probed`) and was answered adds a point; a
/// reused value or a failed poll adds nothing, but the history still goes out
/// on `gpu` so it survives the failure.
fn apply_history(
    buffer: &mut VecDeque<GpuHistoryPoint>,
    gpu: &mut GpuTelemetry,
    probed: bool,
    now: u64,
) {
    if probed && gpu.available {
        push_history(buffer, gpu, now);
    }
    gpu.history = buffer.iter().cloned().collect();
}

/// The slicer pipeline's history for the System tab's pipeline chart.
///
/// Built from the interval the slicer already reports into the snapshot, so
/// it adds nothing to the slicer, the capture path or presentation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PipelineTelemetry {
    /// Whether a slicer was running at the last tick.
    pub running: bool,
    /// One point per telemetry tick while a slicer was running, oldest
    /// first; same `at` clock, capacity and age limit as the GPU history.
    pub history: Vec<PipelinePoint>,
}

/// One telemetry tick of the slicer pipeline.
///
/// Slicer intervals and ticks share one length ([`crate::diagnostics`]), but
/// they are not phase-locked, so a tick can see a new interval, the same one
/// again, or (rarely) skip one. Counters are therefore reported on the fresh
/// point only, so a sum over the window is right.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PipelinePoint {
    /// Unix seconds of the tick.
    pub at: u64,
    /// This tick saw a new slicer interval (`measured_at` changed).
    pub fresh: bool,
    /// The interval these values come from; null when stale.
    pub measured_at: Option<u64>,
    pub interval_seconds: Option<f64>,
    /// The output refresh rate that is 100 % on the chart.
    pub output_hz: Option<f64>,
    pub canvas_fps: Option<f64>,
    pub presented_fps: Option<f64>,
    /// Null without presentation feedback or with fewer than two outputs.
    pub offset_mean_ms: Option<f64>,
    pub offset_max_ms: Option<f64>,
    pub free_run: Option<bool>,
    /// Counters of the interval; 0 on a point that repeats it.
    pub straddles: u32,
    pub gate_holds: u32,
    pub stalls: u32,
    pub frames_superseded: u32,
}

/// The output rate the chart calls 100 %: the fastest refresh rate the
/// slicer measured, else the fastest current mode of an active output.
fn output_rate(stats: &ProjectionStats, outputs: &[Output]) -> Option<f64> {
    let fastest = |rates: Vec<f64>| {
        rates
            .into_iter()
            .filter(|hz| hz.is_finite() && *hz > 0.0)
            .reduce(f64::max)
    };
    fastest(stats.outputs.iter().filter_map(|o| o.refresh_hz).collect()).or_else(|| {
        fastest(
            outputs
                .iter()
                .filter(|o| o.active)
                .filter_map(|o| o.current_mode.as_ref().map(|mode| mode.refresh_hz))
                .collect(),
        )
    })
}

/// The pipeline point for one tick.
///
/// `previous_measured_at` is the interval the previous tick saw. A missing or
/// stale interval yields a point with no values rather than a repeat of old
/// ones, so a quiet slicer reads as no data. An interval older than
/// [`stale_after`] (which tolerates two late reports) is no data: a static
/// source produces no intervals at all, and a flat line of the last values
/// would read as a healthy pipeline.
pub fn pipeline_point(
    last: Option<&ProjectionStats>,
    previous_measured_at: Option<u64>,
    outputs: &[Output],
    now: u64,
) -> PipelinePoint {
    let empty = PipelinePoint {
        at: now,
        fresh: false,
        measured_at: None,
        interval_seconds: None,
        output_hz: None,
        canvas_fps: None,
        presented_fps: None,
        offset_mean_ms: None,
        offset_max_ms: None,
        free_run: None,
        straddles: 0,
        gate_holds: 0,
        stalls: 0,
        frames_superseded: 0,
    };
    let Some(stats) = last else { return empty };
    if Duration::from_secs(now.saturating_sub(stats.measured_at)) > stale_after() {
        return empty;
    }
    let fresh = previous_measured_at != Some(stats.measured_at);
    let counted = |value: u32| if fresh { value } else { 0 };
    PipelinePoint {
        fresh,
        measured_at: Some(stats.measured_at),
        interval_seconds: Some(stats.interval_seconds),
        output_hz: output_rate(stats, outputs),
        canvas_fps: Some(stats.canvas_fps),
        presented_fps: Some(stats.presented_fps),
        offset_mean_ms: stats.offset_ms.as_ref().map(|offset| offset.mean),
        offset_max_ms: stats.offset_ms.as_ref().map(|offset| offset.max),
        free_run: Some(stats.free_run),
        straddles: counted(stats.straddles),
        gate_holds: counted(stats.gate_holds),
        stalls: counted(stats.stalls),
        frames_superseded: counted(stats.frames_superseded),
        ..empty
    }
}

/// Add this tick's point while a slicer runs, trim, and return what to
/// publish. Nothing is added when no slicer runs, so the chart shows a gap.
fn apply_pipeline(
    buffer: &mut VecDeque<PipelinePoint>,
    previous_measured_at: &mut Option<u64>,
    snapshot: &Snapshot,
    now: u64,
) -> PipelineTelemetry {
    let running = snapshot.slicer_running();
    if running {
        let last = snapshot.projection_stats();
        buffer.push_back(pipeline_point(
            last.as_ref(),
            *previous_measured_at,
            &snapshot.outputs(),
            now,
        ));
        *previous_measured_at = last.map(|stats| stats.measured_at);
    }
    trim_history(buffer, now, |point| point.at);
    PipelineTelemetry {
        running,
        history: buffer.iter().cloned().collect(),
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
            unmatched_elements: internals.unmatched_elements,
        });
    }
    reports
}

/// The background sampler. See the module documentation.
pub struct Telemetry {
    supervisor: Arc<Supervisor>,
    snapshot: Arc<Snapshot>,
    events: EventHub,
    sway: Arc<dyn SwayClient>,
    browser_devtools: bool,
    sampler: ProcessSampler,
    cpu: CpuSampler,
    /// When `nvidia-smi` was last found missing, to rate-limit re-probing.
    missing_since: Option<Instant>,
    /// Recent successful GPU polls, published as [`GpuTelemetry::history`].
    history: VecDeque<GpuHistoryPoint>,
    /// Recent slicer intervals, published as [`PipelineTelemetry::history`].
    pipeline: VecDeque<PipelinePoint>,
    /// The slicer interval the previous tick saw, to tell a fresh one.
    previous_measured_at: Option<u64>,
}

impl Telemetry {
    /// A sampler reading the real `/proc`.
    pub fn new(
        supervisor: Arc<Supervisor>,
        snapshot: Arc<Snapshot>,
        events: EventHub,
        sway: Arc<dyn SwayClient>,
        browser_devtools: bool,
    ) -> Self {
        Self {
            supervisor,
            snapshot,
            events,
            sway,
            browser_devtools,
            sampler: ProcessSampler::new(),
            cpu: CpuSampler::new(),
            missing_since: None,
            history: VecDeque::new(),
            pipeline: VecDeque::new(),
            previous_measured_at: None,
        }
    }

    /// Sample once and publish what changed.
    pub async fn step(&mut self) {
        let apps = self.supervisor.statuses_with_chromium().await;
        let roots: Vec<(String, u32)> = apps
            .iter()
            .filter_map(|(status, _)| status.pid.map(|pid| (status.id.clone(), pid)))
            .collect();
        let now = crate::util::unix_now();
        // The daemon, the compositor and the slicer are sampled alone: the
        // daemon's tree would otherwise contain every app.
        let mut solo = vec![(format!("{SOLO_PREFIX}suede"), std::process::id())];
        solo.extend(
            self.sway
                .pid()
                .map(|pid| (format!("{SOLO_PREFIX}sway"), pid)),
        );
        solo.extend(
            self.snapshot
                .slicer_pid()
                .map(|pid| (format!("{SOLO_PREFIX}slicer"), pid)),
        );
        let mut processes = self.sampler.sample_with_solo(&roots, &solo);
        let solo_processes = crate::cpu::take_solo(&mut processes);
        let cpu = self.cpu.sample(&solo_processes, &processes, now);

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
        apply_history(&mut self.history, &mut gpu, probe, now);

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
            now,
        );

        if self.snapshot.set_cpu(cpu.clone()) {
            self.events.publish(ServerEvent::CpuChanged(Box::new(cpu)));
        }
        if self.snapshot.set_gpu(gpu.clone()) {
            self.events.publish(ServerEvent::GpuChanged(Box::new(gpu)));
        }
        let pipeline = apply_pipeline(
            &mut self.pipeline,
            &mut self.previous_measured_at,
            &self.snapshot,
            now,
        );
        if self.snapshot.set_pipeline(pipeline.clone()) {
            self.events
                .publish(ServerEvent::PipelineChanged(Box::new(pipeline)));
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
            unmatched_elements: Vec::new(),
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
            history: Vec::new(),
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

    fn polled(index: u32, util: u32) -> GpuTelemetry {
        GpuTelemetry {
            available: true,
            reason: None,
            sampled_at: Some(1),
            gpus: vec![crate::nvidia_smi::GpuSample {
                index,
                name: "Quadro".into(),
                driver_version: None,
                performance_state: None,
                utilization_gpu_percent: Some(util),
                utilization_memory_percent: None,
                utilization_decoder_percent: None,
                utilization_encoder_percent: None,
                memory_used_mib: Some(100),
                memory_total_mib: Some(1000),
                temperature_c: None,
                power_draw_w: None,
                power_limit_w: None,
                clock_sm_mhz: None,
                clock_memory_mhz: None,
                pcie_generation: None,
                pcie_width: Some(16),
                pcie_rx_mb_s: Some(344),
                pcie_tx_mb_s: Some(28),
                pcie_rx_percent: Some(2.2),
                pcie_tx_percent: Some(0.2),
                fan_percent: None,
            }],
            processes: Vec::new(),
            history: Vec::new(),
        }
    }

    #[test]
    fn a_pushed_point_carries_the_graphed_readings() {
        let mut buffer = VecDeque::new();
        push_history(&mut buffer, &polled(1, 42), 1000);
        let point = &buffer[0];
        assert_eq!(point.at, 1000);
        assert_eq!(point.gpus[0].index, 1);
        assert_eq!(point.gpus[0].utilization_gpu_percent, Some(42));
        assert_eq!(point.gpus[0].memory_total_mib, Some(1000));
        assert_eq!(point.gpus[0].pcie_width, Some(16));
        assert_eq!(point.gpus[0].pcie_rx_mb_s, Some(344));
        assert_eq!(point.gpus[0].pcie_tx_mb_s, Some(28));
        assert_eq!(point.gpus[0].pcie_rx_percent, Some(2.2));
        assert_eq!(point.gpus[0].pcie_tx_percent, Some(0.2));
    }

    #[test]
    fn history_is_capped_at_capacity() {
        let mut buffer = VecDeque::new();
        for i in 0..=history_capacity() as u64 {
            push_history(&mut buffer, &polled(0, 1), 1000 + i);
        }
        assert_eq!(buffer.len(), history_capacity());
        assert_eq!(buffer.front().map(|p| p.at), Some(1001), "oldest dropped");
    }

    #[test]
    fn history_drops_points_older_than_the_window_even_under_capacity() {
        let mut buffer = VecDeque::new();
        push_history(&mut buffer, &polled(0, 1), 1000);
        push_history(&mut buffer, &polled(0, 1), 1000 + HISTORY_WINDOW.as_secs());
        assert_eq!(buffer.len(), 2, "exactly at the window edge is kept");
        push_history(&mut buffer, &polled(0, 1), 1001 + HISTORY_WINDOW.as_secs());
        assert_eq!(
            buffer.len(),
            2,
            "the point one second past the window is gone"
        );
        assert_eq!(
            buffer.front().map(|p| p.at),
            Some(1000 + HISTORY_WINDOW.as_secs())
        );
    }

    #[test]
    fn a_reused_value_adds_no_point() {
        let mut buffer = VecDeque::new();
        let mut gpu = polled(0, 1);
        apply_history(&mut buffer, &mut gpu, false, 1000);
        assert!(buffer.is_empty());
        assert!(gpu.history.is_empty());
    }

    #[test]
    fn history_survives_a_failed_poll() {
        let mut buffer = VecDeque::new();
        let mut gpu = polled(0, 1);
        apply_history(&mut buffer, &mut gpu, true, 1000);
        assert_eq!(gpu.history.len(), 1);

        let mut failed = GpuTelemetry {
            reason: Some("nvidia-smi timed out".into()),
            ..GpuTelemetry::default()
        };
        apply_history(&mut buffer, &mut failed, true, 1005);
        assert!(!failed.available);
        assert_eq!(failed.history.len(), 1, "no new point, old one kept");
        assert_eq!(failed.history[0].at, 1000);
    }

    fn stats(measured_at: u64) -> ProjectionStats {
        ProjectionStats {
            measured_at,
            interval_seconds: crate::diagnostics::SAMPLE_INTERVAL.as_secs_f64(),
            free_run: false,
            canvas_fps: 59.9,
            presented_fps: 30.0,
            frames_superseded: 4,
            stalls: 1,
            per_frame_ms: crate::model::FrameCost {
                waiting: 1.0,
                snapshot: 1.0,
                requesting: 1.0,
                blending: 1.0,
                gpu: 0.0,
            },
            presentation_feedback: true,
            offset_ms: None,
            straddles: 3,
            gate_holds: 2,
            renderer: "cpu".to_string(),
            presentation_backend: None,
            timestamp_source: None,
            capture_intervals: crate::model::CaptureIntervals::default(),
            outputs: Vec::new(),
            gpu_timing: None,
        }
    }

    fn output(active: bool, hz: f64) -> Output {
        Output {
            name: "HDMI-A-1".into(),
            active,
            make: None,
            model: None,
            serial: None,
            current_mode: Some(crate::model::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: hz,
            }),
            modes: Vec::new(),
            rect: crate::model::Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale: None,
            transform: None,
            adaptive_sync_status: None,
        }
    }

    fn counters(point: &PipelinePoint) -> (u32, u32, u32, u32) {
        (
            point.straddles,
            point.gate_holds,
            point.stalls,
            point.frames_superseded,
        )
    }

    #[test]
    fn a_new_interval_is_fresh_and_carries_its_counters() {
        let point = pipeline_point(Some(&stats(1000)), Some(990), &[output(true, 60.0)], 1003);
        assert!(point.fresh);
        assert_eq!(point.measured_at, Some(1000));
        assert_eq!(point.canvas_fps, Some(59.9));
        assert_eq!(point.output_hz, Some(60.0));
        assert_eq!(counters(&point), (3, 2, 1, 4));
    }

    #[test]
    fn a_repeated_interval_keeps_values_but_zeroes_counters() {
        let point = pipeline_point(Some(&stats(1000)), Some(1000), &[], 1008);
        assert!(!point.fresh);
        assert_eq!(point.presented_fps, Some(30.0));
        assert_eq!(counters(&point), (0, 0, 0, 0));
    }

    #[test]
    fn the_first_interval_ever_seen_is_fresh() {
        assert!(pipeline_point(Some(&stats(1000)), None, &[], 1001).fresh);
    }

    #[test]
    fn a_stale_interval_is_no_data() {
        let at_limit = pipeline_point(
            Some(&stats(1000)),
            Some(1000),
            &[],
            1000 + stale_after().as_secs(),
        );
        assert_eq!(
            at_limit.canvas_fps,
            Some(59.9),
            "exactly at the stale limit is still data"
        );
        let stale = pipeline_point(
            Some(&stats(1000)),
            Some(990),
            &[],
            1001 + stale_after().as_secs(),
        );
        assert!(!stale.fresh);
        assert_eq!(stale.measured_at, None);
        assert_eq!(stale.canvas_fps, None);
        assert_eq!(stale.output_hz, None);
        assert_eq!(counters(&stale), (0, 0, 0, 0));
    }

    #[test]
    fn no_interval_yet_is_no_data() {
        let point = pipeline_point(None, None, &[], 1000);
        assert!(!point.fresh);
        assert_eq!(point.canvas_fps, None);
    }

    #[test]
    fn the_output_rate_prefers_the_slicers_measurement() {
        let mut measured = stats(1000);
        measured.outputs = vec![timing(Some(50.0)), timing(Some(59.94)), timing(None)];
        let point = pipeline_point(Some(&measured), None, &[output(true, 144.0)], 1001);
        assert_eq!(point.output_hz, Some(59.94));
    }

    #[test]
    fn the_output_rate_falls_back_to_active_output_modes() {
        let outputs = [output(true, 50.0), output(true, 60.0), output(false, 144.0)];
        let point = pipeline_point(Some(&stats(1000)), None, &outputs, 1001);
        assert_eq!(point.output_hz, Some(60.0), "max of the active outputs");
        let bare = pipeline_point(Some(&stats(1000)), None, &[], 1001);
        assert_eq!(bare.output_hz, None);
    }

    #[test]
    fn the_offset_is_copied_when_reported() {
        let mut measured = stats(1000);
        measured.offset_ms = Some(crate::model::PresentationOffset {
            mean: 0.2,
            max: 0.4,
        });
        let point = pipeline_point(Some(&measured), None, &[], 1001);
        assert_eq!(
            (point.offset_mean_ms, point.offset_max_ms),
            (Some(0.2), Some(0.4))
        );
        let without = pipeline_point(Some(&stats(1000)), None, &[], 1001);
        assert_eq!(without.offset_max_ms, None);
    }

    fn timing(refresh_hz: Option<f64>) -> crate::model::OutputTiming {
        crate::model::OutputTiming {
            name: "HDMI-A-1".into(),
            presented: 0,
            discarded: 0,
            zero_copy_presented: 0,
            refresh_hz,
            phase_ms: None,
            phase_spread_ms: None,
            lag_frames: crate::model::LagFrames::default(),
            commit_to_present_ms: None,
        }
    }

    #[test]
    fn a_tick_adds_a_point_only_while_a_slicer_runs() {
        let snapshot = Snapshot::new();
        let mut buffer = VecDeque::new();
        let mut previous = None;
        let idle = apply_pipeline(&mut buffer, &mut previous, &snapshot, 1000);
        assert!(!idle.running);
        assert!(idle.history.is_empty());

        snapshot.set_slicer_running(true);
        snapshot.set_projection_stats(Some(stats(1000)));
        let first = apply_pipeline(&mut buffer, &mut previous, &snapshot, 1001);
        assert!(first.running);
        assert!(first.history[0].fresh);
        let second = apply_pipeline(&mut buffer, &mut previous, &snapshot, 1006);
        assert_eq!(second.history.len(), 2);
        assert!(!second.history[1].fresh, "same interval, a repeat");

        snapshot.set_slicer_running(false);
        let stopped = apply_pipeline(&mut buffer, &mut previous, &snapshot, 1011);
        assert!(!stopped.running);
        assert_eq!(stopped.history.len(), 2, "history kept, no new point");
    }

    #[test]
    fn pipeline_history_is_trimmed_by_age_and_capacity() {
        let mut buffer = VecDeque::new();
        let newest = 1000 + history_capacity() as u64;
        for at in 1000..=newest {
            buffer.push_back(pipeline_point(None, None, &[], at));
        }
        trim_history(&mut buffer, newest, |point| point.at);
        assert_eq!(buffer.len(), history_capacity());
        assert_eq!(buffer.front().map(|p| p.at), Some(1001), "oldest dropped");

        trim_history(&mut buffer, newest + HISTORY_WINDOW.as_secs(), |point| {
            point.at
        });
        assert_eq!(buffer.len(), 1, "only the newest is within the window");
        assert_eq!(buffer.front().map(|p| p.at), Some(newest));
        trim_history(
            &mut buffer,
            newest + HISTORY_WINDOW.as_secs() + 1,
            |point| point.at,
        );
        assert!(buffer.is_empty(), "301 s old is gone");
    }
}
