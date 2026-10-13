//! CPU telemetry for the System tab's CPU panel.
//!
//! Everything comes from `/proc` and `/sys`: `/proc/stat` for whole-machine
//! and per-core use, `/proc/loadavg`, `/proc/meminfo`, and an hwmon package
//! temperature. Nothing is spawned and nothing outside those two trees is
//! read, so sampling costs about a millisecond. Per-process and per-app
//! shares come from the [`crate::procs`] walk the telemetry task already
//! makes; this module only divides them by the core count and adds them up.
//!
//! Percentages are of the whole machine (every core busy is 100) unless a
//! field says `core_percent`, which is the same share as a percent of one
//! core, the figure `top` shows.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::procs::{BrowserProcess, ProcessSampleSet};

/// Prefix of the names the telemetry task gives the daemon, the compositor
/// and the slicer when it asks [`crate::procs::ProcessSampler`] for them, so
/// they can never collide with an app id (ids do not start with `@`).
pub const SOLO_PREFIX: &str = "@";

/// hwmon driver names that report the CPU package temperature, in the order
/// they are preferred.
const TEMPERATURE_DRIVERS: [&str; 4] = ["coretemp", "k10temp", "cpu_thermal", "cpu-thermal"];

/// One of the daemon, the compositor and the slicer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuProcess {
    /// "suede", "sway" or "slicer".
    pub role: String,
    pub pid: u32,
    /// Percent of the whole machine (all cores = 100); null on the first
    /// sample of the process.
    pub percent: Option<f64>,
    /// The same share as a percent of one core, the figure operators know
    /// from `top`.
    pub core_percent: Option<f64>,
    pub rss_mib: u64,
}

/// One Suede app's whole process tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuApp {
    pub app: String,
    /// Percent of the whole machine, summed over the tree.
    pub percent: Option<f64>,
    /// The same share as a percent of one core.
    pub core_percent: Option<f64>,
    pub rss_mib: u64,
    /// Processes in the tree.
    pub processes: u32,
}

/// One app's share in a history point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuAppShare {
    pub app: String,
    /// Percent of the whole machine.
    pub percent: Option<f64>,
}

/// One diagnostics interval of the CPU panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuHistoryPoint {
    /// Unix seconds of the sample.
    pub at: u64,
    pub total_percent: Option<f64>,
    /// The busiest core, so one saturated core shows against a low total.
    pub max_core_percent: Option<f64>,
    pub suede_percent: Option<f64>,
    pub sway_percent: Option<f64>,
    pub slicer_percent: Option<f64>,
    /// Each app's share of the machine.
    pub apps: Vec<CpuAppShare>,
    /// `(total - available) / total` of memory.
    pub memory_used_percent: Option<f64>,
    pub temperature_c: Option<f64>,
}

/// What `GET /system/cpu` returns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuTelemetry {
    /// Unix seconds of this sample; 0 before the first.
    pub sampled_at: u64,
    pub cores: u32,
    /// All cores together, 0 to 100; null on the first sample.
    pub total_percent: Option<f64>,
    /// Per core, 0 to 100, in `/proc/stat` order.
    pub core_percent: Vec<Option<f64>>,
    pub load_1: f64,
    pub load_5: f64,
    pub load_15: f64,
    pub memory_total_mib: u64,
    pub memory_available_mib: u64,
    pub swap_total_mib: u64,
    pub swap_free_mib: u64,
    /// Package temperature from hwmon (`coretemp`, `k10temp`, `cpu_thermal`
    /// or `cpu-thermal`, first found), else null.
    pub temperature_c: Option<f64>,
    /// The daemon, the compositor and the slicer.
    pub processes: Vec<CpuProcess>,
    /// Every Suede app's whole process tree.
    pub apps: Vec<CpuApp>,
    pub history: Vec<CpuHistoryPoint>,
}

/// Busy and total ticks of one `cpu` line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ticks {
    busy: u64,
    total: u64,
}

/// The `cpu` and `cpuN` lines of `/proc/stat`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatSample {
    all: Ticks,
    cores: Vec<Ticks>,
}

/// Parse one `cpu...` line's counters. Busy is user, nice, system, irq,
/// softirq and steal; idle time is idle and iowait. Guest time is already
/// inside user and nice.
fn parse_ticks(fields: &[&str]) -> Option<Ticks> {
    let value = |index: usize| -> Option<u64> {
        match fields.get(index) {
            Some(field) => field.parse().ok(),
            // Old kernels stop after iowait, or earlier.
            None if index >= 4 => Some(0),
            None => None,
        }
    };
    let (user, nice, system, idle) = (value(0)?, value(1)?, value(2)?, value(3)?);
    let (iowait, irq, softirq, steal) = (value(4)?, value(5)?, value(6)?, value(7)?);
    let busy = user + nice + system + irq + softirq + steal;
    Some(Ticks {
        busy,
        total: busy + idle + iowait,
    })
}

fn parse_stat(text: &str) -> Option<StatSample> {
    let mut all = None;
    let mut cores = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(name) = words.next() else { continue };
        let Some(suffix) = name.strip_prefix("cpu") else {
            continue;
        };
        let fields: Vec<&str> = words.collect();
        let Some(ticks) = parse_ticks(&fields) else {
            continue;
        };
        if suffix.is_empty() {
            all = Some(ticks);
        } else if suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            cores.push(ticks);
        }
    }
    Some(StatSample { all: all?, cores })
}

/// Percent busy between two counter readings, one decimal; null when no
/// time passed or a counter went backwards.
fn busy_percent(before: Ticks, after: Ticks) -> Option<f64> {
    let total = after.total.checked_sub(before.total)?;
    let busy = after.busy.checked_sub(before.busy)?;
    if total == 0 {
        return None;
    }
    Some(round(busy as f64 * 100.0 / total as f64, 1))
}

fn round(value: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (value * scale).round() / scale
}

/// The three load averages of `/proc/loadavg`.
fn parse_loadavg(text: &str) -> (f64, f64, f64) {
    let mut numbers = text
        .split_whitespace()
        .map(|word| word.parse::<f64>().unwrap_or(0.0));
    (
        numbers.next().unwrap_or(0.0),
        numbers.next().unwrap_or(0.0),
        numbers.next().unwrap_or(0.0),
    )
}

/// MiB figures of `/proc/meminfo`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Memory {
    total: u64,
    available: u64,
    swap_total: u64,
    swap_free: u64,
}

fn parse_meminfo(text: &str) -> Memory {
    let mut memory = Memory::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let kib: u64 = rest
            .split_whitespace()
            .next()
            .and_then(|word| word.parse().ok())
            .unwrap_or(0);
        let mib = kib / 1024;
        match key {
            "MemTotal" => memory.total = mib,
            "MemAvailable" => memory.available = mib,
            "SwapTotal" => memory.swap_total = mib,
            "SwapFree" => memory.swap_free = mib,
            _ => {}
        }
    }
    memory
}

/// The package temperature: the first hwmon device (in directory order)
/// whose `name` is one of [`TEMPERATURE_DRIVERS`] and whose `temp1_input`
/// reads, in degrees Celsius to one decimal.
fn read_temperature(sys_root: &Path) -> Option<f64> {
    let mut devices: Vec<PathBuf> = std::fs::read_dir(sys_root.join("class/hwmon"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    devices.sort();
    devices.into_iter().find_map(|device| {
        let name = std::fs::read_to_string(device.join("name")).ok()?;
        if !TEMPERATURE_DRIVERS.contains(&name.trim()) {
            return None;
        }
        let millidegrees: f64 = std::fs::read_to_string(device.join("temp1_input"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some(round(millidegrees / 1000.0, 1))
    })
}

/// Remove the solo roots (named with [`SOLO_PREFIX`]) from `set`, so they
/// neither appear as apps nor attribute GPU processes to a fake app.
pub fn take_solo(set: &mut ProcessSampleSet) -> Vec<(String, BrowserProcess)> {
    set.owner
        .retain(|_, (app, _)| !app.starts_with(SOLO_PREFIX));
    let names: Vec<String> = set
        .by_app
        .keys()
        .filter(|name| name.starts_with(SOLO_PREFIX))
        .cloned()
        .collect();
    names
        .into_iter()
        .filter_map(|name| {
            let process = set.by_app.remove(&name)?.into_iter().next()?;
            Some((name[SOLO_PREFIX.len()..].to_string(), process))
        })
        .collect()
}

/// Samples the machine and keeps the CPU history. See the module
/// documentation.
#[derive(Debug)]
pub struct CpuSampler {
    proc_root: PathBuf,
    sys_root: PathBuf,
    previous: Option<StatSample>,
    history: VecDeque<CpuHistoryPoint>,
}

impl Default for CpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuSampler {
    /// A sampler over the real `/proc` and `/sys`.
    pub fn new() -> Self {
        Self::with_roots(PathBuf::from("/proc"), PathBuf::from("/sys"))
    }

    /// A sampler over other roots, for tests.
    pub fn with_roots(proc_root: PathBuf, sys_root: PathBuf) -> Self {
        Self {
            proc_root,
            sys_root,
            previous: None,
            history: VecDeque::new(),
        }
    }

    /// Sample the machine at `now` (Unix seconds). `solo` is the daemon,
    /// the compositor and the slicer from [`take_solo`]; `apps` is every
    /// app's tree.
    pub fn sample(
        &mut self,
        solo: &[(String, BrowserProcess)],
        apps: &ProcessSampleSet,
        now: u64,
    ) -> CpuTelemetry {
        let current = std::fs::read_to_string(self.proc_root.join("stat"))
            .ok()
            .and_then(|text| parse_stat(&text));
        let cores = current
            .as_ref()
            .map(|stat| stat.cores.len())
            .filter(|count| *count > 0)
            .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
            .unwrap_or(1) as u32;
        let (total_percent, core_percent) = match (&self.previous, &current) {
            (Some(before), Some(after)) => (
                busy_percent(before.all, after.all),
                after
                    .cores
                    .iter()
                    .enumerate()
                    .map(|(index, ticks)| {
                        before
                            .cores
                            .get(index)
                            .and_then(|earlier| busy_percent(*earlier, *ticks))
                    })
                    .collect(),
            ),
            (_, Some(after)) => (None, vec![None; after.cores.len()]),
            _ => (None, Vec::new()),
        };
        if current.is_some() {
            self.previous = current;
        }

        let (load_1, load_5, load_15) = std::fs::read_to_string(self.proc_root.join("loadavg"))
            .map(|text| parse_loadavg(&text))
            .unwrap_or_default();
        let memory = std::fs::read_to_string(self.proc_root.join("meminfo"))
            .map(|text| parse_meminfo(&text))
            .unwrap_or_default();
        let temperature_c = read_temperature(&self.sys_root);

        let machine = |core: Option<f64>| core.map(|value| round(value / f64::from(cores), 2));
        let processes: Vec<CpuProcess> = ["suede", "sway", "slicer"]
            .into_iter()
            .filter_map(|role| {
                let (_, process) = solo.iter().find(|(name, _)| name == role)?;
                Some(CpuProcess {
                    role: role.to_string(),
                    pid: process.pid,
                    percent: machine(process.cpu_percent),
                    core_percent: process.cpu_percent,
                    rss_mib: process.rss_mib,
                })
            })
            .collect();
        let mut app_rows: Vec<CpuApp> = apps
            .by_app
            .iter()
            .map(|(app, tree)| {
                // A process new this tick has no figure yet: sum the ones
                // that do, and report none only when no process has one.
                let known: Vec<f64> = tree
                    .iter()
                    .filter_map(|process| process.cpu_percent)
                    .collect();
                let core_percent = (!known.is_empty()).then(|| round(known.iter().sum::<f64>(), 1));
                CpuApp {
                    app: app.clone(),
                    percent: machine(core_percent),
                    core_percent,
                    rss_mib: tree.iter().map(|process| process.rss_mib).sum(),
                    processes: tree.len() as u32,
                }
            })
            .collect();
        app_rows.sort_by(|a, b| a.app.cmp(&b.app));

        let share_of = |role: &str| {
            processes
                .iter()
                .find(|process| process.role == role)
                .and_then(|process| process.percent)
        };
        let point = CpuHistoryPoint {
            at: now,
            total_percent,
            max_core_percent: core_percent.iter().flatten().copied().reduce(f64::max),
            suede_percent: share_of("suede"),
            sway_percent: share_of("sway"),
            slicer_percent: share_of("slicer"),
            apps: app_rows
                .iter()
                .map(|app| CpuAppShare {
                    app: app.app.clone(),
                    percent: app.percent,
                })
                .collect(),
            memory_used_percent: (memory.total > 0).then(|| {
                round(
                    memory.total.saturating_sub(memory.available) as f64 * 100.0
                        / memory.total as f64,
                    1,
                )
            }),
            temperature_c,
        };
        self.history.push_back(point);
        crate::telemetry::trim_history(&mut self.history, now, |point| point.at);

        CpuTelemetry {
            sampled_at: now,
            cores,
            total_percent,
            core_percent,
            load_1,
            load_5,
            load_15,
            memory_total_mib: memory.total,
            memory_available_mib: memory.available,
            swap_total_mib: memory.swap_total,
            swap_free_mib: memory.swap_free,
            temperature_c,
            processes,
            apps: app_rows,
            history: self.history.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A `/proc/stat` with the aggregate line and one line per `(busy, idle)`.
    fn write_stat(root: &Path, all: (u64, u64), cores: &[(u64, u64)]) {
        let mut text = format!("cpu  {} 0 0 {} 0 0 0 0 0 0\n", all.0, all.1);
        for (index, (busy, idle)) in cores.iter().enumerate() {
            text.push_str(&format!("cpu{index} {busy} 0 0 {idle} 0 0 0 0 0 0\n"));
        }
        text.push_str("intr 1 2 3\nctxt 4\n");
        std::fs::write(root.join("stat"), text).unwrap();
    }

    fn fake_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("proc")).unwrap();
        std::fs::create_dir_all(dir.path().join("sys/class/hwmon")).unwrap();
        dir
    }

    fn sampler(dir: &tempfile::TempDir) -> CpuSampler {
        CpuSampler::with_roots(dir.path().join("proc"), dir.path().join("sys"))
    }

    fn process(pid: u32, cpu: Option<f64>, rss: u64) -> BrowserProcess {
        BrowserProcess {
            pid,
            role: "browser".into(),
            subtype: None,
            cpu_percent: cpu,
            rss_mib: rss,
        }
    }

    #[test]
    fn stat_deltas_give_total_and_per_core_percent() {
        let dir = fake_root();
        let proc = dir.path().join("proc");
        write_stat(&proc, (100, 900), &[(100, 300), (0, 600)]);
        let mut sampler = sampler(&dir);
        let first = sampler.sample(&[], &ProcessSampleSet::default(), 10);
        assert_eq!(first.cores, 2);
        assert_eq!(first.total_percent, None, "no delta yet");
        assert_eq!(first.core_percent, vec![None, None]);

        write_stat(&proc, (200, 1800), &[(200, 600), (0, 1200)]);
        let second = sampler.sample(&[], &ProcessSampleSet::default(), 15);
        // 100 busy of 1000 ticks overall; core 0: 100 of 400; core 1: idle.
        assert_eq!(second.total_percent, Some(10.0));
        assert_eq!(second.core_percent, vec![Some(25.0), Some(0.0)]);
        assert_eq!(second.history.len(), 2);
        assert_eq!(second.history[1].max_core_percent, Some(25.0));
    }

    #[test]
    fn iowait_counts_as_idle_and_a_still_clock_is_null() {
        let before = parse_stat("cpu 10 0 10 70 10 0 0 0\n").unwrap();
        let after = parse_stat("cpu 20 0 20 140 20 0 0 0\n").unwrap();
        // 20 busy of 100 more ticks.
        assert_eq!(busy_percent(before.all, after.all), Some(20.0));
        assert_eq!(busy_percent(after.all, after.all), None);
        assert_eq!(busy_percent(after.all, before.all), None, "counter reset");
    }

    #[test]
    fn loadavg_and_meminfo_parse() {
        assert_eq!(
            parse_loadavg("0.95 0.66 0.74 2/1234 5678\n"),
            (0.95, 0.66, 0.74)
        );
        assert_eq!(parse_loadavg(""), (0.0, 0.0, 0.0));
        let memory = parse_meminfo(
            "MemTotal:       32768000 kB\nMemFree: 1 kB\nMemAvailable:   16384000 kB\n\
             SwapTotal:       2048000 kB\nSwapFree:        1024000 kB\n",
        );
        assert_eq!(
            memory,
            Memory {
                total: 32000,
                available: 16000,
                swap_total: 2000,
                swap_free: 1000
            }
        );
    }

    fn write_hwmon(sys: &Path, dir: &str, name: &str, temp: Option<&str>) {
        let device = sys.join("class/hwmon").join(dir);
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("name"), format!("{name}\n")).unwrap();
        if let Some(temp) = temp {
            std::fs::write(device.join("temp1_input"), temp).unwrap();
        }
    }

    #[test]
    fn temperature_comes_from_the_first_known_hwmon_driver() {
        let dir = fake_root();
        let sys = dir.path().join("sys");
        write_hwmon(&sys, "hwmon0", "acpitz", Some("99000\n"));
        write_hwmon(&sys, "hwmon1", "nvme", Some("30000\n"));
        write_hwmon(&sys, "hwmon2", "coretemp", Some("44000\n"));
        assert_eq!(read_temperature(&sys), Some(44.0));
    }

    #[test]
    fn temperature_is_null_when_absent_or_unreadable() {
        let dir = fake_root();
        let sys = dir.path().join("sys");
        assert_eq!(read_temperature(&sys), None, "no hwmon devices");
        write_hwmon(&sys, "hwmon0", "r8169", Some("50000"));
        assert_eq!(read_temperature(&sys), None, "no CPU driver");
        write_hwmon(&sys, "hwmon1", "coretemp", None);
        assert_eq!(read_temperature(&sys), None, "temp1_input missing");
        write_hwmon(&sys, "hwmon2", "k10temp", Some("not a number"));
        assert_eq!(read_temperature(&sys), None, "unreadable value");
        write_hwmon(&sys, "hwmon3", "cpu-thermal", Some("51234"));
        assert_eq!(read_temperature(&sys), Some(51.2), "a later device is used");
        assert_eq!(
            read_temperature(&dir.path().join("missing")),
            None,
            "no /sys at all"
        );
    }

    #[test]
    fn shares_are_of_the_machine_and_of_a_core() {
        let dir = fake_root();
        let proc = dir.path().join("proc");
        write_stat(&proc, (0, 0), &[(0, 0), (0, 0), (0, 0), (0, 0)]);
        let mut apps = ProcessSampleSet::default();
        apps.by_app.insert(
            "wall".into(),
            vec![process(1, Some(100.0), 50), process(2, Some(60.0), 70)],
        );
        apps.by_app
            .insert("fresh".into(), vec![process(3, None, 10)]);
        let solo = vec![
            ("sway".to_string(), process(9, Some(40.0), 71)),
            ("suede".to_string(), process(8, Some(8.0), 30)),
        ];
        let telemetry = sampler(&dir).sample(&solo, &apps, 1);
        assert_eq!(telemetry.cores, 4);
        let roles: Vec<&str> = telemetry
            .processes
            .iter()
            .map(|p| p.role.as_str())
            .collect();
        assert_eq!(
            roles,
            ["suede", "sway"],
            "fixed order, absent slicer left out"
        );
        let sway = &telemetry.processes[1];
        assert_eq!(sway.core_percent, Some(40.0));
        assert_eq!(sway.percent, Some(10.0), "40 % of a core is 10 % of four");
        assert_eq!(sway.rss_mib, 71);
        let wall = telemetry.apps.iter().find(|a| a.app == "wall").unwrap();
        assert_eq!(wall.core_percent, Some(160.0));
        assert_eq!(wall.percent, Some(40.0));
        assert_eq!(wall.rss_mib, 120);
        assert_eq!(wall.processes, 2);
        let fresh = telemetry.apps.iter().find(|a| a.app == "fresh").unwrap();
        assert_eq!(fresh.percent, None, "no figure on a first sample");
        let point = telemetry.history.last().unwrap();
        assert_eq!(point.sway_percent, Some(10.0));
        assert_eq!(point.suede_percent, Some(2.0));
        assert_eq!(point.slicer_percent, None);
        assert_eq!(point.apps.len(), 2);
    }

    #[test]
    fn history_is_bounded_by_the_window() {
        let dir = fake_root();
        let mut sampler = sampler(&dir);
        let capacity = crate::diagnostics::history_capacity();
        let step = crate::diagnostics::SAMPLE_INTERVAL.as_secs();
        for tick in 0..(capacity as u64 + 10) {
            sampler.sample(&[], &ProcessSampleSet::default(), 1000 + tick * step);
        }
        let last = sampler.sample(
            &[],
            &ProcessSampleSet::default(),
            1000 + (capacity as u64 + 10) * step,
        );
        assert_eq!(last.history.len(), capacity);
    }

    #[test]
    fn solo_roots_are_taken_out_of_the_app_view() {
        let mut set = ProcessSampleSet {
            by_app: HashMap::new(),
            owner: HashMap::new(),
        };
        set.by_app
            .insert("@sway".into(), vec![process(9, Some(1.0), 5)]);
        set.owner.insert(9, ("@sway".into(), "browser".into()));
        set.by_app
            .insert("wall".into(), vec![process(1, Some(1.0), 5)]);
        set.owner.insert(1, ("wall".into(), "browser".into()));
        let solo = take_solo(&mut set);
        assert_eq!(solo.len(), 1);
        assert_eq!(solo[0].0, "sway");
        assert!(!set.by_app.contains_key("@sway"));
        assert!(!set.owner.contains_key(&9));
        assert!(set.owner.contains_key(&1));
    }
}
