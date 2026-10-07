//! Chromium's software-rendering fallback: detection and automatic restart.
//!
//! When Chromium's GPU process dies, Chromium relaunches it. Measured on a
//! test appliance (Google Chrome 151 with Suede's `chromium-kiosk`
//! arguments, killing the GPU process by hand; Chromium 153 behaved the
//! same): every death logs `GPU process exited unexpectedly` on the
//! browser's stderr, which the supervisor captures in the app's log; the
//! first two relaunches keep the GPU; from the third on, the GPU process
//! is relaunched with `--use-gl=disabled` — software compositing — and
//! never returns to the GPU without a browser restart; the sixth death ends
//! the browser, which the supervisor already restarts as `processExited`.
//! On a 3909x2327 canvas, software compositing was a 1–3 fps wall. Frame
//! rate itself is no signal: a static page legitimately shows 0 fps for
//! minutes. The GPU process's own command line is, and renderers that
//! already existed gain no flag, so this reads only that.
//!
//! [`gpu_rendering`] reads the process tree, [`CrashCounter`] the app's
//! log, and [`Policy::evaluate`] decides, pure so every rule is
//! table-testable: act on software rendering seen on [`SOFTWARE_POLLS`]
//! polls in a row of the same process, at most [`MAX_RESTARTS_PER_HOUR`]
//! times per app per rolling hour, and never when the bootstrap key is
//! off, the app is not running, it is not `chromium-kiosk`, or its GPU
//! process never crashed (software from launch is not a fallback, and a
//! restart would only start it the same way). [`Watchdog`] feeds it from
//! the supervisor and `/proc`, runs the restart it decides on, and
//! publishes what it saw for the `browser-gpu` check.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::checks::CheckRunner;
use crate::model::{AppState, CheckStatus, RestartReason};
use crate::snapshot::Snapshot;
use crate::supervisor::Supervisor;

/// How often [`Watchdog::run`] looks.
pub const POLL: Duration = Duration::from_secs(5);

/// Polls in a row that must see software rendering, for the same process,
/// before a restart. Two, so the decision always rests on two separate
/// readings of the process tree rather than one.
pub const SOFTWARE_POLLS: u32 = 2;

/// Automatic restarts allowed per app in any [`BUDGET_WINDOW`]. A GPU that
/// keeps failing is a hardware or driver fault a restart cannot cure, and
/// each restart blanks the app's displays for a few seconds.
pub const MAX_RESTARTS_PER_HOUR: u32 = 3;

/// The rolling window [`MAX_RESTARTS_PER_HOUR`] counts over.
pub const BUDGET_WINDOW: Duration = Duration::from_secs(60 * 60);

/// The GPU process death after which Chromium relaunches it in software.
pub const FALLBACK_AFTER_CRASHES: u32 = 3;

/// What Chromium writes to stderr each time its GPU process dies, e.g.
/// `ERROR:content/browser/gpu/gpu_process_host.cc:1234] GPU process exited
/// unexpectedly: exit_code=8704`.
pub const CRASH_MARKER: &str = "GPU process exited unexpectedly";

/// The command-line switch that marks Chromium's GPU process.
const GPU_PROCESS_SWITCH: &str = "--type=gpu-process";

/// How Chromium's GPU process is rendering, from its command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rendering {
    Hardware,
    /// `--use-gl=disabled` (software compositing), or SwiftShader.
    Software,
}

// --- the process tree ------------------------------------------------------

/// How the GPU process under `pid` is rendering, or `None` when there is no
/// GPU process among its descendants (between relaunches, or still
/// starting). `proc_root` is `/proc` outside tests.
///
/// The whole tree is walked, not only direct children: the `google-chrome`
/// wrapper script and Chromium's zygotes can sit between the process Suede
/// launched and the GPU process.
pub fn gpu_rendering(proc_root: &Path, pid: u32) -> Option<Rendering> {
    let children = children_by_parent(proc_root);
    let mut found = None;
    for descendant in descendants(&children, pid) {
        let Ok(raw) = std::fs::read(proc_root.join(descendant.to_string()).join("cmdline")) else {
            continue;
        };
        // Chrome rewrites its process title, so a child's cmdline is one
        // space-joined string ending in a single NUL, not one NUL-separated
        // argument each (measured on Chrome 151). Splitting on both reads
        // either form; the switches matched here never contain a space.
        let args: Vec<&str> = raw
            .split(|byte| *byte == 0 || byte.is_ascii_whitespace())
            .filter_map(|arg| std::str::from_utf8(arg).ok())
            .collect();
        if !args.contains(&GPU_PROCESS_SWITCH) {
            continue;
        }
        if is_software(&args) {
            return Some(Rendering::Software);
        }
        found = Some(Rendering::Hardware);
    }
    found
}

/// Whether a GPU process's arguments say it renders in software.
fn is_software(args: &[&str]) -> bool {
    args.iter().any(|arg| {
        *arg == "--use-gl=disabled"
            || arg.starts_with("--use-gl=swiftshader")
            || arg.starts_with("--use-angle=swiftshader")
    })
}

/// Every process's children, from each `/proc/<pid>/stat`.
fn children_by_parent(proc_root: &Path) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return children;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some(parent) = parent_pid(&stat) {
            children.entry(parent).or_default().push(pid);
        }
    }
    children
}

/// The parent pid from a `/proc/<pid>/stat` line: the second field after
/// the command name. The name is parenthesized and may itself contain
/// spaces and parentheses, so the fields are counted from the last `)`.
fn parent_pid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    fields.next()?; // state
    fields.next()?.parse().ok()
}

/// Every descendant of `root`, breadth first, each once.
fn descendants(children: &HashMap<u32, Vec<u32>>, root: u32) -> Vec<u32> {
    let mut seen = HashSet::from([root]);
    let mut queue = VecDeque::from([root]);
    let mut found = Vec::new();
    while let Some(pid) = queue.pop_front() {
        for &child in children.get(&pid).into_iter().flatten() {
            if seen.insert(child) {
                found.push(child);
                queue.push_back(child);
            }
        }
    }
    found
}

// --- the crash count -------------------------------------------------------

/// Lines in `text` recording a GPU process death.
pub fn count_crashes(text: &str) -> u32 {
    text.lines()
        .filter(|line| line.contains(CRASH_MARKER))
        .count() as u32
}

/// GPU process deaths in one app's log since its current launch.
///
/// The supervisor truncates the log at every launch, so the count is "since
/// this launch" by construction. Read incrementally, complete lines only,
/// because a browser that runs for days can write a long log.
#[derive(Debug, Clone, Default)]
pub struct CrashCounter {
    pid: Option<u32>,
    offset: u64,
    count: u32,
}

impl CrashCounter {
    /// The count for the process `pid`, reading whatever `path` gained.
    pub fn read(&mut self, path: &Path, pid: u32) -> u32 {
        if self.pid != Some(pid) {
            *self = Self {
                pid: Some(pid),
                ..Self::default()
            };
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            return self.count;
        };
        let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        if len < self.offset {
            // Truncated under us: a launch this counter has not seen yet.
            self.offset = 0;
            self.count = 0;
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return self.count;
        }
        let mut gained = Vec::new();
        if file.read_to_end(&mut gained).is_err() {
            return self.count;
        }
        // A line still being written is read again, whole, next time.
        let complete = gained
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |end| end + 1);
        self.count += count_crashes(&String::from_utf8_lossy(&gained[..complete]));
        self.offset += complete as u64;
        self.count
    }
}

// --- the policy ------------------------------------------------------------

/// Everything [`Policy::evaluate`] reads about one app, gathered at one
/// moment.
#[derive(Debug, Clone)]
pub struct Observation<'a> {
    pub now: Instant,
    /// Unix seconds, for the wall-clock time a fallback was first seen.
    pub now_unix: u64,
    pub app: &'a str,
    /// Whether the app is launched with the `chromium-kiosk` preset.
    pub chromium: bool,
    /// Whether the supervisor says the app is `running`.
    pub running: bool,
    pub pid: Option<u32>,
    /// What the GPU process's command line says; `None` when there is none.
    pub gpu: Option<Rendering>,
    /// GPU process deaths in the app's log since this launch.
    pub crashes: u32,
}

/// What [`Policy::evaluate`] concluded. Everything but `Wait` is logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do now.
    Wait,
    /// Restart the app's process `pid`; `attempt` counts this hour's
    /// automatic restarts including this one.
    Restart {
        pid: u32,
        attempt: u32,
        crashes: u32,
    },
    /// Fallen back with every automatic restart this hour used. Reported
    /// once per process.
    Exhausted { crashes: u32 },
    /// Fallen back with automatic restart switched off. Reported once per
    /// process.
    Off { crashes: u32 },
    /// Rendering in software without a GPU process crash since launch, so
    /// it started that way. Reported once per process.
    NotAFallback,
}

/// What the policy knows about one app.
#[derive(Debug, Clone, Default)]
struct Track {
    /// The process the fields below describe.
    pid: Option<u32>,
    /// The last GPU process rendering seen for it; kept while no GPU process
    /// is found.
    rendering: Option<Rendering>,
    /// Polls in a row that saw software.
    software_polls: u32,
    /// Unix second software rendering was first seen for this process.
    software_since: Option<u64>,
    crashes: u32,
    /// Whether this process was ever seen rendering on the GPU. Software
    /// after that is a fallback whatever the log says, so a future Chromium
    /// that rewords its crash line cannot make one look like a browser that
    /// started in software.
    seen_hardware: bool,
    /// Whether this process's `Exhausted`/`Off`/`NotAFallback` was reported.
    held_reported: bool,
    /// When each automatic restart in the current window happened. Per app,
    /// not per process: a restart is what makes a new process.
    restarts: VecDeque<Instant>,
}

/// The restart policy's state for one daemon. See the module docs for the
/// rules.
#[derive(Debug, Clone)]
pub struct Policy {
    enabled: bool,
    apps: HashMap<String, Track>,
}

impl Policy {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            apps: HashMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Decide what to do about `observation`.
    pub fn evaluate(&mut self, observation: &Observation<'_>) -> Decision {
        if !observation.chromium {
            self.apps.remove(observation.app);
            return Decision::Wait;
        }
        let track = self.apps.entry(observation.app.to_string()).or_default();
        while track
            .restarts
            .front()
            .is_some_and(|at| observation.now.saturating_duration_since(*at) >= BUDGET_WINDOW)
        {
            track.restarts.pop_front();
        }
        if track.pid != observation.pid {
            *track = Track {
                pid: observation.pid,
                restarts: std::mem::take(&mut track.restarts),
                ..Track::default()
            };
        }
        track.crashes = observation.crashes;
        // No GPU process right now (between relaunches) says nothing either
        // way: the last reading stands and the count neither grows nor resets.
        match observation.gpu {
            Some(Rendering::Software) => {
                track.rendering = Some(Rendering::Software);
                track.software_polls += 1;
                track.software_since.get_or_insert(observation.now_unix);
            }
            Some(Rendering::Hardware) => {
                track.rendering = Some(Rendering::Hardware);
                track.seen_hardware = true;
                track.software_polls = 0;
                track.software_since = None;
                track.held_reported = false;
            }
            None => {}
        }

        let Some(pid) = observation.pid.filter(|_| observation.running) else {
            track.software_polls = 0;
            return Decision::Wait;
        };
        if track.rendering != Some(Rendering::Software) || track.software_polls < SOFTWARE_POLLS {
            return Decision::Wait;
        }
        let crashes = track.crashes;
        let held = if crashes == 0 && !track.seen_hardware {
            Decision::NotAFallback
        } else if !self.enabled {
            Decision::Off { crashes }
        } else if track.restarts.len() as u32 >= MAX_RESTARTS_PER_HOUR {
            Decision::Exhausted { crashes }
        } else {
            track.restarts.push_back(observation.now);
            track.software_polls = 0;
            return Decision::Restart {
                pid,
                attempt: track.restarts.len() as u32,
                crashes,
            };
        };
        if std::mem::replace(&mut track.held_reported, true) {
            Decision::Wait
        } else {
            held
        }
    }

    /// Give back the restart [`Self::evaluate`] just counted for `app`, when
    /// the supervisor did not carry it out (the process had already gone).
    pub fn refund(&mut self, app: &str) {
        if let Some(track) = self.apps.get_mut(app) {
            track.restarts.pop_back();
        }
    }

    /// What the `browser-gpu` check is told about `app`, if it has a process.
    pub fn report(&self, app: &str, log_path: PathBuf) -> Option<AppGpuReport> {
        let track = self.apps.get(app)?;
        Some(AppGpuReport {
            app: app.to_string(),
            pid: track.pid?,
            rendering: track.rendering,
            crashes: track.crashes,
            software_since: track.software_since,
            seen_hardware: track.seen_hardware,
            restarts_this_hour: track.restarts.len() as u32,
            log_path,
        })
    }

    /// Forget every app `keep` rejects (removed from the configuration).
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.apps.retain(|app, _| keep(app));
    }
}

// --- what the check reads --------------------------------------------------

/// What the watchdog saw of one Chromium app's process, published on the
/// [`Snapshot`] for the `browser-gpu` check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppGpuReport {
    pub app: String,
    /// The process this describes; a report for any other is stale.
    pub pid: u32,
    /// `None` until a GPU process has been seen.
    pub rendering: Option<Rendering>,
    /// GPU process deaths since this launch.
    pub crashes: u32,
    /// Unix second software rendering was first seen for this process.
    pub software_since: Option<u64>,
    /// Whether this process was ever seen rendering on the GPU, which makes
    /// software now a fallback even with no crash line in the log.
    pub seen_hardware: bool,
    /// Automatic restarts of this app in the last hour.
    pub restarts_this_hour: u32,
    /// The app's log, where the crashes are recorded.
    pub log_path: PathBuf,
}

/// The `browser-gpu` check's verdict on the running Chromium apps, each
/// with the watchdog's report on its current process if there is one, and
/// whether automatic restart is on.
///
/// The detail must stay the same between runs on an unchanged machine, so
/// it carries no elapsed time: the fallback is dated by the wall-clock time
/// it was first seen, and every count in it changes only on a real event.
pub fn check_verdict(
    apps: &[(&str, Option<&AppGpuReport>)],
    automatic: bool,
) -> (CheckStatus, String) {
    if apps.is_empty() {
        return (
            CheckStatus::Pass,
            "not applicable: no Chromium application is running".to_string(),
        );
    }
    let mut worst = CheckStatus::Pass;
    let mut details = Vec::new();
    for (app, report) in apps {
        let (status, detail) = app_verdict(app, *report, automatic);
        if rank(status) > rank(worst) {
            worst = status;
        }
        details.push(detail);
    }
    (worst, details.join("; "))
}

/// Why software rendering counts as a fallback: the crashes the log
/// recorded, or, when it recorded none, the hardware rendering the watchdog
/// saw earlier in the same launch.
fn fallback_cause(crashes: u32) -> String {
    if crashes > 0 {
        format!("after its GPU process crashed {}", times(crashes))
    } else {
        "after rendering on the GPU earlier in this launch".to_string()
    }
}

fn app_verdict(app: &str, report: Option<&AppGpuReport>, automatic: bool) -> (CheckStatus, String) {
    let Some(report) = report else {
        return (CheckStatus::Pass, format!("{app}: no GPU process seen yet"));
    };
    let log = report.log_path.display();
    match report.rendering {
        Some(Rendering::Software) => {
            let since = report
                .software_since
                .map(|unix| format!(" since {}", format_utc(unix)))
                .unwrap_or_default();
            let used = format!(
                "{} of {MAX_RESTARTS_PER_HOUR} automatic restarts used this hour",
                report.restarts_this_hour
            );
            let detail = if report.crashes == 0 && !report.seen_hardware {
                format!(
                    "{app}: Chromium has been rendering in software{since}, without a GPU \
                     process crash since launch, so it started that way and a restart would \
                     most likely start it the same way; check the app's extraArgs and the \
                     graphics driver ({used}; log: {log})"
                )
            } else {
                let next = if !automatic {
                    "automatic restart is off (restart_on_gpu_fallback = false), so restart \
                     the app to recover"
                        .to_string()
                } else if report.restarts_this_hour >= MAX_RESTARTS_PER_HOUR {
                    "every automatic restart this hour is used, so Suede will not restart it \
                     again until the oldest is an hour old; restart the app to recover"
                        .to_string()
                } else {
                    "Suede restarts it automatically once a second poll confirms it".to_string()
                };
                format!(
                    "{app}: Chromium fell back to software rendering {cause}, and has rendered \
                     in software{since}, so its displays may run at a few frames per second; \
                     {next} ({used}; log: {log})",
                    cause = fallback_cause(report.crashes),
                )
            };
            (CheckStatus::Fail, detail)
        }
        rendering if report.crashes > 0 => (
            CheckStatus::Warn,
            format!(
                "{app}: {}, but its GPU process has crashed {} since launch; Chromium drops \
                 to software rendering after the {} crash (log: {log})",
                if rendering.is_some() {
                    "still rendering on the GPU"
                } else {
                    "no GPU process seen yet"
                },
                times(report.crashes),
                ordinal(FALLBACK_AFTER_CRASHES),
            ),
        ),
        Some(Rendering::Hardware) => (
            CheckStatus::Pass,
            format!("{app}: rendering on the GPU, with no GPU process crash since launch"),
        ),
        None => (CheckStatus::Pass, format!("{app}: no GPU process seen yet")),
    }
}

fn rank(status: CheckStatus) -> u8 {
    match status {
        CheckStatus::Pass => 0,
        CheckStatus::Warn => 1,
        CheckStatus::Fail => 2,
    }
}

fn times(count: u32) -> String {
    if count == 1 {
        "once".to_string()
    } else {
        format!("{count} times")
    }
}

fn ordinal(count: u32) -> String {
    match count {
        1 => "first".to_string(),
        2 => "second".to_string(),
        3 => "third".to_string(),
        other => format!("{other}th"),
    }
}

/// `unix` as `2026-10-07 14:03:12 UTC`.
fn format_utc(unix: u64) -> String {
    // Howard Hinnant's days-to-civil conversion; the proleptic Gregorian
    // calendar, which is all a Unix timestamp needs.
    let days = (unix / 86_400) as i64;
    let seconds = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

// --- the task --------------------------------------------------------------

/// Feeds [`Policy`] from the supervisor, `/proc` and the apps' logs,
/// restarts what it says to, and publishes what it saw.
pub struct Watchdog {
    supervisor: Arc<Supervisor>,
    checks: Arc<CheckRunner>,
    snapshot: Arc<Snapshot>,
    /// `/proc` outside tests.
    proc_root: PathBuf,
    policy: Policy,
    counters: HashMap<String, CrashCounter>,
}

impl Watchdog {
    pub fn new(
        enabled: bool,
        supervisor: Arc<Supervisor>,
        checks: Arc<CheckRunner>,
        snapshot: Arc<Snapshot>,
    ) -> Self {
        Self {
            supervisor,
            checks,
            snapshot,
            proc_root: PathBuf::from("/proc"),
            policy: Policy::new(enabled),
            counters: HashMap::new(),
        }
    }

    /// The same watchdog reading a fake `/proc`, for tests.
    #[cfg(test)]
    pub(crate) fn with_proc_root(mut self, proc_root: PathBuf) -> Self {
        self.proc_root = proc_root;
        self
    }

    /// Look once, restart whatever the policy says to, publish what was seen,
    /// and rerun the checks if anything changed. Returns each app's decision.
    pub async fn step(&mut self) -> Vec<(String, Decision)> {
        let apps = self.supervisor.statuses_with_chromium().await;
        let now = Instant::now();
        let now_unix = crate::util::unix_now();
        let watched = |id: &str| {
            apps.iter()
                .any(|(status, chromium)| *chromium && status.id == id)
        };
        self.policy.retain(watched);
        self.counters.retain(|id, _| watched(id));

        let mut decisions = Vec::new();
        let mut reports = Vec::new();
        let mut acted = false;
        for (status, chromium) in &apps {
            let id = status.id.as_str();
            let log_path = self.supervisor.launch_context().log_path(id);
            let (gpu, crashes) = match (chromium, status.pid) {
                (true, Some(pid)) => (
                    gpu_rendering(&self.proc_root, pid),
                    self.counters
                        .entry(id.to_string())
                        .or_default()
                        .read(&log_path, pid),
                ),
                _ => (None, 0),
            };
            let decision = self.policy.evaluate(&Observation {
                now,
                now_unix,
                app: id,
                chromium: *chromium,
                running: status.state == AppState::Running,
                pid: status.pid,
                gpu,
                crashes,
            });
            match &decision {
                Decision::Wait => {}
                Decision::Restart {
                    pid,
                    attempt,
                    crashes,
                } => {
                    tracing::warn!(
                        app = %id,
                        pid,
                        crashes,
                        attempt,
                        max_restarts = MAX_RESTARTS_PER_HOUR,
                        "app {id}: Chromium fell back to software rendering {}; restarting it \
                         (automatic restart {attempt} of {MAX_RESTARTS_PER_HOUR} this hour)",
                        fallback_cause(*crashes)
                    );
                    if self
                        .supervisor
                        .restart_because(id, RestartReason::GpuFallback, Some(*pid))
                        .await
                    {
                        acted = true;
                    } else {
                        tracing::info!(
                            app = %id,
                            pid,
                            "app {id} was relaunched before it could be restarted; nothing to do"
                        );
                        self.policy.refund(id);
                    }
                }
                Decision::Exhausted { crashes } => tracing::warn!(
                    app = %id,
                    crashes,
                    max_restarts = MAX_RESTARTS_PER_HOUR,
                    "app {id}: Chromium fell back to software rendering {}; not restarting it, \
                     because all {MAX_RESTARTS_PER_HOUR} automatic restarts this hour are used",
                    fallback_cause(*crashes)
                ),
                Decision::Off { crashes } => tracing::warn!(
                    app = %id,
                    crashes,
                    "app {id}: Chromium fell back to software rendering {}; not restarting it, \
                     because automatic restart is off (restart_on_gpu_fallback = false)",
                    fallback_cause(*crashes)
                ),
                Decision::NotAFallback => tracing::warn!(
                    app = %id,
                    "app {id}: Chromium is rendering in software, but its GPU process has not \
                     crashed since launch, so it started that way; not restarting it"
                ),
            }
            if let Some(report) = self.policy.report(id, log_path) {
                reports.push(report);
            }
            decisions.push((id.to_string(), decision));
        }
        let changed = self.snapshot.set_browser_gpu(reports);
        // The check would otherwise wait for its next scheduled run: up to a
        // minute of showing a fallback that has already been dealt with.
        if acted || changed {
            self.checks.run_all().await;
        }
        decisions
    }

    /// Look every [`POLL`] until shutdown. Runs with automatic restart off
    /// too: the `browser-gpu` check still needs to be told what it sees.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        if !self.policy.enabled() {
            tracing::info!(
                "automatic restart on a browser GPU fallback is off \
                 (restart_on_gpu_fallback = false)"
            );
        }
        let mut poll = tokio::time::interval(POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = poll.tick() => {
                    self.step().await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the process tree ---------------------------------------------------

    /// A fake `/proc` with these processes: (pid, parent, comm, argv).
    fn fake_proc(processes: &[(u32, u32, &str, &[&str])]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (pid, parent, comm, argv) in processes {
            let dir = root.path().join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("stat"),
                format!("{pid} ({comm}) S {parent} {pid} {pid} 0 -1 4194560 0 0 0 0\n"),
            )
            .unwrap();
            let mut cmdline = argv.join("\0");
            cmdline.push('\0');
            std::fs::write(dir.join("cmdline"), cmdline).unwrap();
        }
        // Not a process: must be skipped, not trip the reader.
        std::fs::write(root.path().join("uptime"), "100.0 50.0\n").unwrap();
        root
    }

    const CHROME: &[&str] = &["/opt/google/chrome/chrome", "--kiosk"];
    const ZYGOTE: &[&str] = &["/opt/google/chrome/chrome", "--type=zygote"];
    const RENDERER: &[&str] = &["/opt/google/chrome/chrome", "--type=renderer"];
    const GPU: &[&str] = &[
        "/opt/google/chrome/chrome",
        "--type=gpu-process",
        "--ozone-platform=wayland",
    ];

    /// Replace a fake process's cmdline with the form Chrome leaves after
    /// rewriting its process title: one space-joined string and one NUL.
    fn retitle(proc: &tempfile::TempDir, pid: u32, argv: &[&str]) {
        let cmdline = format!("{}\0", argv.join(" "));
        std::fs::write(proc.path().join(pid.to_string()).join("cmdline"), cmdline).unwrap();
    }

    #[test]
    fn a_retitled_cmdline_is_read_like_a_separated_one() {
        let proc = fake_proc(&[(100, 1, "chrome", CHROME), (101, 100, "chrome", GPU)]);
        retitle(&proc, 101, GPU);
        assert_eq!(gpu_rendering(proc.path(), 100), Some(Rendering::Hardware));
        retitle(&proc, 101, &[GPU, &["--use-gl=disabled"]].concat());
        assert_eq!(gpu_rendering(proc.path(), 100), Some(Rendering::Software));
    }

    #[test]
    fn a_gpu_process_without_software_flags_is_hardware() {
        let proc = fake_proc(&[(100, 1, "chrome", CHROME), (101, 100, "chrome", GPU)]);
        assert_eq!(gpu_rendering(proc.path(), 100), Some(Rendering::Hardware));
    }

    #[test]
    fn each_software_flag_is_software() {
        for flag in [
            "--use-gl=disabled",
            "--use-gl=swiftshader",
            "--use-gl=swiftshader-webgl",
            "--use-angle=swiftshader",
            "--use-angle=swiftshader-webgl",
        ] {
            let gpu = [GPU, &[flag]].concat();
            let proc = fake_proc(&[(100, 1, "chrome", CHROME), (101, 100, "chrome", &gpu)]);
            assert_eq!(
                gpu_rendering(proc.path(), 100),
                Some(Rendering::Software),
                "{flag}"
            );
        }
        // The same switches with a hardware value are not.
        let gpu = [GPU, &["--use-gl=angle", "--use-angle=vulkan"]].concat();
        let proc = fake_proc(&[(100, 1, "chrome", CHROME), (101, 100, "chrome", &gpu)]);
        assert_eq!(gpu_rendering(proc.path(), 100), Some(Rendering::Hardware));
    }

    #[test]
    fn no_gpu_process_is_none() {
        let proc = fake_proc(&[
            (100, 1, "chrome", CHROME),
            (101, 100, "chrome", ZYGOTE),
            (102, 101, "chrome", RENDERER),
        ]);
        assert_eq!(gpu_rendering(proc.path(), 100), None);
        assert_eq!(gpu_rendering(proc.path(), 999), None, "an unknown pid");
    }

    #[test]
    fn a_grandchild_through_a_launcher_is_found_and_strangers_are_not() {
        let software = [GPU, &["--use-gl=disabled"]].concat();
        let proc = fake_proc(&[
            // The wrapper script Suede launched, then Chrome under it, then a
            // zygote, then the GPU process; the comm has a space and a paren.
            (
                100,
                1,
                "google-chrome",
                &["/bin/bash", "/usr/bin/google-chrome"],
            ),
            (101, 100, "chrome", CHROME),
            (102, 101, "chrome (zyg)", ZYGOTE),
            (103, 102, "chrome", &software),
            // Another browser's GPU process, not under this app.
            (200, 1, "chrome", CHROME),
            (201, 200, "chrome", GPU),
        ]);
        assert_eq!(gpu_rendering(proc.path(), 100), Some(Rendering::Software));
        assert_eq!(gpu_rendering(proc.path(), 200), Some(Rendering::Hardware));
        // The app's own process is not its GPU process.
        let proc = fake_proc(&[(300, 1, "chrome", GPU)]);
        assert_eq!(gpu_rendering(proc.path(), 300), None);
    }

    #[test]
    fn the_parent_is_read_after_the_last_parenthesis() {
        assert_eq!(parent_pid("42 (a) b) S 7 42 42 0"), Some(7));
        assert_eq!(parent_pid("42 (Web Content) R 1 42"), Some(1));
        assert_eq!(parent_pid("42 (x"), None);
        assert_eq!(parent_pid("42 (x) S"), None);
    }

    // --- the crash count ----------------------------------------------------

    const CRASH: &str =
        "[1:1:1007/120000.000000:ERROR:content/browser/gpu/gpu_process_host.cc:1004] \
                         GPU process exited unexpectedly: exit_code=8704\n";

    #[test]
    fn crash_lines_are_counted() {
        assert_eq!(count_crashes(""), 0);
        let log = format!(
            "[1:1:1007/120000.000000:WARNING:other.cc:1] something else\n{CRASH}\
             Restarting GPU process due to unrecoverable error. Context was lost.\n{CRASH}"
        );
        assert_eq!(count_crashes(&log), 2);
    }

    #[test]
    fn the_counter_reads_incrementally_and_starts_again_for_a_new_launch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena-fx.log");
        let mut counter = CrashCounter::default();
        assert_eq!(counter.read(&path, 10), 0, "no log yet");

        std::fs::write(&path, CRASH).unwrap();
        assert_eq!(counter.read(&path, 10), 1);
        assert_eq!(counter.read(&path, 10), 1, "nothing new is not a new crash");

        // A line still being written is not counted until it is whole.
        let (head, tail) = CRASH.split_at(40);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, head.as_bytes()).unwrap();
        assert_eq!(counter.read(&path, 10), 1);
        std::io::Write::write_all(&mut file, tail.as_bytes()).unwrap();
        assert_eq!(counter.read(&path, 10), 2);

        // The supervisor truncates the log at each launch.
        std::fs::write(&path, "").unwrap();
        assert_eq!(counter.read(&path, 11), 0, "a new process starts from zero");
        std::fs::write(&path, CRASH).unwrap();
        assert_eq!(counter.read(&path, 11), 1);
    }

    // --- the policy ---------------------------------------------------------

    struct Clock {
        start: Instant,
    }

    impl Clock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
            }
        }

        /// A running Chromium app, pid 10, seen at `seconds` after the start
        /// (Unix second 1000 + that).
        fn at(&self, seconds: u64, gpu: Option<Rendering>, crashes: u32) -> Observation<'static> {
            Observation {
                now: self.start + Duration::from_secs(seconds),
                now_unix: 1000 + seconds,
                app: "arena-fx",
                chromium: true,
                running: true,
                pid: Some(10),
                gpu,
                crashes,
            }
        }
    }

    const SW: Option<Rendering> = Some(Rendering::Software);
    const HW: Option<Rendering> = Some(Rendering::Hardware);

    #[test]
    fn software_on_two_polls_in_a_row_restarts_once() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        assert_eq!(policy.evaluate(&clock.at(0, HW, 0)), Decision::Wait);
        assert_eq!(policy.evaluate(&clock.at(5, HW, 2)), Decision::Wait);
        assert_eq!(policy.evaluate(&clock.at(10, SW, 3)), Decision::Wait);
        assert_eq!(
            policy.evaluate(&clock.at(15, SW, 3)),
            Decision::Restart {
                pid: 10,
                attempt: 1,
                crashes: 3
            }
        );
        let report = policy.report("arena-fx", PathBuf::from("x.log")).unwrap();
        assert_eq!(report.software_since, Some(1010), "when it was first seen");
        assert_eq!(report.restarts_this_hour, 1);
        // The same process, still software: confirmed afresh, not at once.
        assert_eq!(policy.evaluate(&clock.at(20, SW, 3)), Decision::Wait);
    }

    #[test]
    fn a_hardware_poll_or_a_missing_gpu_process_between_them() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(0, SW, 3));
        assert_eq!(policy.evaluate(&clock.at(5, HW, 3)), Decision::Wait);
        assert_eq!(
            policy.evaluate(&clock.at(10, SW, 3)),
            Decision::Wait,
            "a hardware reading resets the count"
        );
        // A GPU process between relaunches neither counts nor resets.
        assert_eq!(policy.evaluate(&clock.at(15, None, 4)), Decision::Wait);
        assert!(matches!(
            policy.evaluate(&clock.at(20, SW, 4)),
            Decision::Restart { attempt: 1, .. }
        ));
    }

    #[test]
    fn no_gpu_process_keeps_the_previous_verdict_and_is_unknown_at_first() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(0, None, 0));
        let report = policy.report("arena-fx", PathBuf::new()).unwrap();
        assert_eq!(report.rendering, None);
        policy.evaluate(&clock.at(5, SW, 3));
        policy.evaluate(&clock.at(10, None, 3));
        let report = policy.report("arena-fx", PathBuf::new()).unwrap();
        assert_eq!(report.rendering, Some(Rendering::Software));
        assert_eq!(report.software_since, Some(1005));
    }

    #[test]
    fn a_new_pid_resets_the_confirmation_but_not_the_budget() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(0, SW, 3));
        assert!(matches!(
            policy.evaluate(&clock.at(5, SW, 3)),
            Decision::Restart { attempt: 1, .. }
        ));
        // Relaunched: one software reading of the new process is not two.
        let mut observation = clock.at(10, SW, 3);
        observation.pid = Some(11);
        assert_eq!(policy.evaluate(&observation), Decision::Wait);
        let report = policy.report("arena-fx", PathBuf::new()).unwrap();
        assert_eq!(report.pid, 11);
        assert_eq!(report.software_since, Some(1010));
        assert_eq!(report.restarts_this_hour, 1, "the budget is the app's");

        // And a pid change between two software readings is not two either.
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(0, SW, 3));
        let mut observation = clock.at(5, SW, 3);
        observation.pid = Some(11);
        assert_eq!(policy.evaluate(&observation), Decision::Wait);
    }

    #[test]
    fn three_restarts_per_rolling_hour_then_exhausted_once() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        let mut pid = 10;
        let mut t = 0;
        let mut restarts = Vec::new();
        let mut exhausted = 0;
        // A fallback every few minutes, each on a fresh process.
        for _ in 0..12 {
            for _ in 0..2 {
                let mut observation = clock.at(t, SW, 3);
                observation.pid = Some(pid);
                match policy.evaluate(&observation) {
                    Decision::Restart { attempt, .. } => {
                        restarts.push((t, attempt));
                        pid += 1;
                    }
                    Decision::Exhausted { crashes } => {
                        assert_eq!(crashes, 3);
                        exhausted += 1;
                    }
                    _ => {}
                }
                t += 5;
            }
            t += 300;
        }
        // Restarts at 5, 315 and 625 seconds; then nothing until the first is
        // an hour old.
        assert_eq!(restarts, vec![(5, 1), (315, 2), (625, 3)]);
        assert_eq!(exhausted, 1, "reported once for the process it held");
        assert_eq!(
            policy
                .report("arena-fx", PathBuf::new())
                .unwrap()
                .restarts_this_hour,
            3
        );

        // An hour after the first, one restart is free again.
        let mut observation = clock.at(3605, SW, 3);
        observation.pid = Some(pid);
        assert_eq!(
            policy.evaluate(&observation),
            Decision::Restart {
                pid,
                attempt: 3,
                crashes: 3
            }
        );
    }

    #[test]
    fn a_refunded_restart_does_not_count() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(0, SW, 3));
        policy.evaluate(&clock.at(5, SW, 3));
        policy.refund("arena-fx");
        assert_eq!(
            policy
                .report("arena-fx", PathBuf::new())
                .unwrap()
                .restarts_this_hour,
            0
        );
    }

    #[test]
    fn never_when_the_key_is_off() {
        let clock = Clock::new();
        let mut policy = Policy::new(false);
        policy.evaluate(&clock.at(0, SW, 3));
        assert_eq!(
            policy.evaluate(&clock.at(5, SW, 3)),
            Decision::Off { crashes: 3 }
        );
        for t in [10, 15, 20] {
            assert_eq!(policy.evaluate(&clock.at(t, SW, 3)), Decision::Wait);
        }
        // Still tracked, so the check can say what it sees.
        let report = policy.report("arena-fx", PathBuf::new()).unwrap();
        assert_eq!(report.rendering, Some(Rendering::Software));
        assert_eq!(report.restarts_this_hour, 0);
    }

    #[test]
    fn never_when_the_app_is_not_running() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        for t in [0, 5, 10] {
            let mut observation = clock.at(t, SW, 3);
            observation.running = false;
            assert_eq!(policy.evaluate(&observation), Decision::Wait);
        }
        // Readings while it was not running do not count towards two.
        assert_eq!(policy.evaluate(&clock.at(15, SW, 3)), Decision::Wait);
        assert!(matches!(
            policy.evaluate(&clock.at(20, SW, 3)),
            Decision::Restart { .. }
        ));

        let mut stopped = clock.at(0, SW, 3);
        stopped.pid = None;
        assert_eq!(Policy::new(true).evaluate(&stopped), Decision::Wait);
    }

    #[test]
    fn never_when_the_app_is_not_chromium() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        for t in [0, 5, 10] {
            let mut observation = clock.at(t, SW, 3);
            observation.chromium = false;
            assert_eq!(policy.evaluate(&observation), Decision::Wait);
        }
        assert!(policy.report("arena-fx", PathBuf::new()).is_none());
    }

    #[test]
    fn never_when_it_started_in_software() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(0, SW, 0));
        assert_eq!(policy.evaluate(&clock.at(5, SW, 0)), Decision::NotAFallback);
        assert_eq!(policy.evaluate(&clock.at(10, SW, 0)), Decision::Wait);
    }

    #[test]
    fn software_after_hardware_is_a_fallback_without_a_crash_line() {
        let clock = Clock::new();
        let mut policy = Policy::new(true);
        policy.evaluate(&clock.at(0, HW, 0));
        policy.evaluate(&clock.at(5, SW, 0));
        assert!(matches!(
            policy.evaluate(&clock.at(10, SW, 0)),
            Decision::Restart { crashes: 0, .. }
        ));
    }

    // --- the check's verdict ------------------------------------------------

    fn report(rendering: Option<Rendering>, crashes: u32, restarts: u32) -> AppGpuReport {
        AppGpuReport {
            app: "arena-fx".into(),
            pid: 10,
            rendering,
            crashes,
            // 2026-10-07 14:03:12 UTC.
            software_since: (rendering == SW).then_some(1_791_381_792),
            seen_hardware: false,
            restarts_this_hour: restarts,
            log_path: PathBuf::from("/var/lib/suede/logs/arena-fx.log"),
        }
    }

    #[test]
    fn no_chromium_app_is_not_applicable() {
        let (status, detail) = check_verdict(&[], true);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("not applicable"), "{detail}");
    }

    #[test]
    fn hardware_without_crashes_passes() {
        let hardware = report(HW, 0, 0);
        let (status, detail) = check_verdict(&[("arena-fx", Some(&hardware))], true);
        assert_eq!(status, CheckStatus::Pass);
        assert_eq!(
            detail,
            "arena-fx: rendering on the GPU, with no GPU process crash since launch"
        );
        let (status, detail) = check_verdict(&[("arena-fx", None)], true);
        assert_eq!(status, CheckStatus::Pass);
        assert!(detail.contains("no GPU process seen yet"), "{detail}");
    }

    #[test]
    fn hardware_after_a_crash_warns_with_the_log() {
        let crashed = report(HW, 2, 0);
        let (status, detail) = check_verdict(&[("arena-fx", Some(&crashed))], true);
        assert_eq!(status, CheckStatus::Warn);
        assert!(detail.contains("crashed 2 times since launch"), "{detail}");
        assert!(detail.contains("after the third crash"), "{detail}");
        assert!(
            detail.contains("/var/lib/suede/logs/arena-fx.log"),
            "{detail}"
        );
        let once = report(None, 1, 0);
        let (status, detail) = check_verdict(&[("arena-fx", Some(&once))], true);
        assert_eq!(status, CheckStatus::Warn);
        assert!(detail.contains("crashed once"), "{detail}");
        assert!(detail.contains("no GPU process seen yet"), "{detail}");
    }

    #[test]
    fn software_fails_saying_since_when_and_what_happens_next() {
        let fallen = report(SW, 3, 1);
        let (status, detail) = check_verdict(&[("arena-fx", Some(&fallen))], true);
        assert_eq!(status, CheckStatus::Fail);
        assert!(detail.contains("since 2026-10-07 14:03:12 UTC"), "{detail}");
        assert!(detail.contains("crashed 3 times"), "{detail}");
        assert!(
            detail.contains("1 of 3 automatic restarts used this hour"),
            "{detail}"
        );
        assert!(detail.contains("restarts it automatically"), "{detail}");

        let (_, detail) = check_verdict(&[("arena-fx", Some(&fallen))], false);
        assert!(detail.contains("automatic restart is off"), "{detail}");

        let spent = report(SW, 3, 3);
        let (_, detail) = check_verdict(&[("arena-fx", Some(&spent))], true);
        assert!(
            detail.contains("every automatic restart this hour is used"),
            "{detail}"
        );

        let from_launch = report(SW, 0, 0);
        let (status, detail) = check_verdict(&[("arena-fx", Some(&from_launch))], true);
        assert_eq!(status, CheckStatus::Fail);
        assert!(detail.contains("started that way"), "{detail}");
        let after_hardware = AppGpuReport {
            seen_hardware: true,
            ..report(SW, 0, 0)
        };
        let (status, detail) = check_verdict(&[("arena-fx", Some(&after_hardware))], true);
        assert_eq!(status, CheckStatus::Fail);
        assert!(
            detail.contains("after rendering on the GPU earlier in this launch"),
            "{detail}"
        );
    }

    #[test]
    fn the_worst_app_decides_and_every_app_is_named() {
        let fallen = report(SW, 3, 0);
        let fine = AppGpuReport {
            app: "clock".into(),
            ..report(HW, 0, 0)
        };
        let (status, detail) =
            check_verdict(&[("arena-fx", Some(&fallen)), ("clock", Some(&fine))], true);
        assert_eq!(status, CheckStatus::Fail);
        assert!(detail.starts_with("arena-fx: "), "{detail}");
        assert!(detail.contains("; clock: "), "{detail}");
    }

    // --- the task, end to end ----------------------------------------------

    /// A supervisor running one `chromium-kiosk` app whose "browser" is a
    /// script that sleeps, with its window mapped (so it is `running`), and
    /// a check runner and snapshot sharing it.
    #[cfg(unix)]
    async fn chromium_app(
        dir: &Path,
        automatic: bool,
    ) -> (Arc<Supervisor>, Arc<CheckRunner>, Arc<Snapshot>, u32) {
        use crate::model::{AppConfig, Launcher, RestartPolicy};
        use std::os::unix::fs::PermissionsExt;

        let browser = dir.join("fake-chrome");
        std::fs::write(&browser, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sway = Arc::new(crate::sway::mock::MockSway::empty());
        let supervisor = Arc::new(Supervisor::new(
            sway.clone(),
            crate::events::EventHub::new(),
            crate::supervisor::LaunchContext {
                profiles_root: dir.join("profiles"),
                log_root: dir.join("logs"),
                api_base: "http://127.0.0.1:9088/api/v1".into(),
            },
            vec!["*".to_string()],
        ));
        let app = AppConfig {
            id: "arena-fx".into(),
            enabled: true,
            launcher: Launcher::ChromiumKiosk {
                uri: "http://127.0.0.1/".into(),
                show_fps_counter: false,
                extra_args: Vec::new(),
                program: Some(browser.display().to_string()),
                grant_capture: false,
            },
            output: None,
            fullscreen: false,
            span_outputs: false,
            env: Default::default(),
            readiness: None,
            audio: None,
            heartbeat: None,
            restart: RestartPolicy::default(),
            persist_profile: false,
        };
        let target = crate::reconciler::plan::AppTarget {
            id: "arena-fx".into(),
            output: None,
            workspace: None,
            blocked: None,
        };
        supervisor.reconcile(&[app], &[target]).await;
        let pid = supervisor.status("arena-fx").await.unwrap().pid.unwrap();
        map_window(&supervisor, pid).await;

        let snapshot = Arc::new(Snapshot::new());
        let bootstrap = Arc::new(crate::config::BootstrapConfig {
            state_dir: dir.to_path_buf(),
            sway_config_path: dir.join("sway/config"),
            systemd_user_dir: dir.join("systemd/user"),
            restart_on_gpu_fallback: automatic,
            ..crate::config::BootstrapConfig::default()
        });
        let (trigger, _receiver) = crate::reconciler::Reconciler::channel();
        let checks = Arc::new(CheckRunner::new(crate::checks::CheckRunnerDeps {
            bootstrap,
            sway,
            audio: Arc::new(crate::audio::mock::MockAudio::default()),
            store: Arc::new(crate::state::StateStore::ephemeral(dir.to_path_buf())),
            events: crate::events::EventHub::new(),
            capabilities: Arc::new(crate::capabilities::CapabilityStore::new(dir)),
            snapshot: snapshot.clone(),
            trigger,
            supervisor: supervisor.clone(),
        }));
        (supervisor, checks, snapshot, pid)
    }

    /// Map a window for `pid`, which makes a windowed app `running`.
    #[cfg(unix)]
    async fn map_window(supervisor: &Supervisor, pid: u32) {
        let window = crate::model::Window {
            id: 77,
            title: None,
            app_id: Some("chrome".into()),
            pid: Some(pid as i32),
            visible: Some(true),
            fullscreen_mode: 0,
            rect: Default::default(),
            output: Some("HDMI-A-1".into()),
            app: None,
        };
        supervisor.tick(&[window]).await;
    }

    /// A fake `/proc` in which `pid` has fallen back: a GPU process with
    /// `--use-gl=disabled` under a zygote.
    fn fallen_back(pid: u32) -> tempfile::TempDir {
        let software = [GPU, &["--use-gl=disabled"]].concat();
        fake_proc(&[
            (pid, 1, "chrome", CHROME),
            (pid + 100_000, pid, "chrome", ZYGOTE),
            (pid + 100_001, pid + 100_000, "chrome", &software),
        ])
    }

    fn browser_gpu_check(checks: &CheckRunner) -> crate::model::Check {
        checks
            .results()
            .into_iter()
            .find(|check| check.id == crate::checks::ids::BROWSER_GPU)
            .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_confirmed_fallback_restarts_the_app_once_with_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, checks, snapshot, pid) = chromium_app(dir.path(), true).await;
        assert_eq!(
            supervisor.status("arena-fx").await.unwrap().state,
            AppState::Running
        );
        let log = supervisor.launch_context().log_path("arena-fx");
        std::fs::write(&log, CRASH.repeat(3)).unwrap();
        let proc = fallen_back(pid);
        let mut watchdog =
            Watchdog::new(true, supervisor.clone(), checks.clone(), snapshot.clone())
                .with_proc_root(proc.path().to_path_buf());

        assert_eq!(
            watchdog.step().await,
            vec![("arena-fx".to_string(), Decision::Wait)]
        );
        let check = browser_gpu_check(&checks);
        assert_eq!(check.status, CheckStatus::Fail, "{}", check.detail);
        assert!(check.fix_available);

        assert_eq!(
            watchdog.step().await,
            vec![(
                "arena-fx".to_string(),
                Decision::Restart {
                    pid,
                    attempt: 1,
                    crashes: 3
                }
            )]
        );
        let status = supervisor.status("arena-fx").await.unwrap();
        assert_ne!(status.pid, Some(pid));
        assert_eq!(status.last_restart_reason, Some(RestartReason::GpuFallback));
        // The checks ran again at once, and a report on the old process no
        // longer counts.
        let check = browser_gpu_check(&checks);
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
        assert!(!check.fix_available);
        supervisor.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn with_the_key_off_the_check_fails_and_its_fix_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let (supervisor, checks, snapshot, pid) = chromium_app(dir.path(), false).await;
        let log = supervisor.launch_context().log_path("arena-fx");
        std::fs::write(&log, CRASH.repeat(3)).unwrap();
        let proc = fallen_back(pid);
        let mut watchdog =
            Watchdog::new(false, supervisor.clone(), checks.clone(), snapshot.clone())
                .with_proc_root(proc.path().to_path_buf());

        watchdog.step().await;
        assert_eq!(
            watchdog.step().await,
            vec![("arena-fx".to_string(), Decision::Off { crashes: 3 })]
        );
        assert_eq!(supervisor.status("arena-fx").await.unwrap().pid, Some(pid));
        let check = browser_gpu_check(&checks);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(
            check.detail.contains("automatic restart is off"),
            "{}",
            check.detail
        );
        assert!(check.fix_available);

        let outcome = checks.fix(crate::checks::ids::BROWSER_GPU).await.unwrap();
        assert!(outcome.contains("arena-fx"), "{outcome}");
        let status = supervisor.status("arena-fx").await.unwrap();
        assert_ne!(status.pid, Some(pid));
        assert_eq!(status.last_restart_reason, Some(RestartReason::GpuFallback));
        assert_eq!(browser_gpu_check(&checks).status, CheckStatus::Pass);
        // Nothing left to fix.
        assert!(checks.fix(crate::checks::ids::BROWSER_GPU).await.is_err());
        supervisor.shutdown().await;
    }

    #[test]
    fn utc_times_format_as_calendar_dates() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_utc(1_791_381_792), "2026-10-07 14:03:12 UTC");
    }
}
