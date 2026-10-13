//! Process-tree sampling for the apps Suede runs.
//!
//! For each running app's root pid this reads `/proc/<pid>/stat` and
//! `/proc/<pid>/cmdline` of the root and all of its descendants, and reports
//! each process's role (browser, gpu-process, renderer, ...), CPU percent and
//! resident memory. It also builds a pid-to-app map so other telemetry (for
//! example the GPU process table) can attribute a pid to the app it belongs to.
//!
//! Cost: one `/proc` directory walk plus two small file reads per sampled
//! process, every diagnostics interval. The GPU watchdog already performs the
//! same walk at that cadence, so this adds no new kind of load.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::browser_gpu::{children_by_parent, descendants};

/// One process in an app's tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserProcess {
    pub pid: u32,
    /// "browser" for the root pid; otherwise the `--type=` value
    /// ("gpu-process", "renderer", "utility", "zygote", "broker", ...);
    /// "unknown" when there is none.
    pub role: String,
    /// `--utility-sub-type=` value, when present (e.g.
    /// "network.mojom.NetworkService").
    pub subtype: Option<String>,
    /// CPU percent of one core over the last poll interval (utime+stime
    /// delta / wall delta), null on the first sample of a pid.
    pub cpu_percent: Option<f64>,
    pub rss_mib: u64,
}

/// The result of one sampling pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessSampleSet {
    /// Processes per app name, root first.
    pub by_app: HashMap<String, Vec<BrowserProcess>>,
    /// Every sampled pid, mapped to its (app, role).
    pub owner: HashMap<u32, (String, String)>,
}

/// Samples process trees and keeps the previous CPU tick counts so it can
/// report CPU percent across consecutive samples.
#[derive(Debug)]
pub struct ProcessSampler {
    proc_root: PathBuf,
    previous: HashMap<u32, (u64, Instant)>,
    ticks_per_second: u64,
    page_size: u64,
}

impl Default for ProcessSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampler {
    /// A sampler over the real `/proc`.
    pub fn new() -> Self {
        // SAFETY: sysconf has no preconditions; it returns -1 on error.
        let (ticks, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        Self {
            proc_root: PathBuf::from("/proc"),
            previous: HashMap::new(),
            ticks_per_second: if ticks > 0 { ticks as u64 } else { 100 },
            page_size: if page > 0 { page as u64 } else { 4096 },
        }
    }

    /// A sampler over a fake `/proc` (100 ticks per second, 4 KiB pages).
    #[cfg(test)]
    pub(crate) fn with_proc_root(proc_root: PathBuf) -> Self {
        Self {
            proc_root,
            previous: HashMap::new(),
            ticks_per_second: 100,
            page_size: 4096,
        }
    }

    /// Sample the trees under each `(app, root pid)`.
    pub fn sample(&mut self, roots: &[(String, u32)]) -> ProcessSampleSet {
        self.sample_at(roots, Instant::now())
    }

    pub(crate) fn sample_at(&mut self, roots: &[(String, u32)], now: Instant) -> ProcessSampleSet {
        let children = children_by_parent(&self.proc_root);
        let mut set = ProcessSampleSet::default();
        let mut current: HashMap<u32, (u64, Instant)> = HashMap::new();
        for (app, root) in roots {
            let mut pids = vec![*root];
            pids.extend(descendants(&children, *root));
            for pid in pids {
                if set.owner.contains_key(&pid) {
                    continue;
                }
                let dir = self.proc_root.join(pid.to_string());
                let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
                    continue;
                };
                let Some((ticks, rss_pages)) = parse_stat(&stat) else {
                    continue;
                };
                let cmdline = std::fs::read(dir.join("cmdline")).unwrap_or_default();
                let (role, subtype) = classify(&cmdline, pid == *root);
                let cpu_percent = self.previous.get(&pid).and_then(|(before, then)| {
                    let wall = now.checked_duration_since(*then)?.as_secs_f64();
                    if wall <= 0.0 {
                        return None;
                    }
                    let used = ticks.saturating_sub(*before) as f64 / self.ticks_per_second as f64;
                    Some((used / wall * 1000.0).round() / 10.0)
                });
                current.insert(pid, (ticks, now));
                set.owner.insert(pid, (app.clone(), role.clone()));
                set.by_app
                    .entry(app.clone())
                    .or_default()
                    .push(BrowserProcess {
                        pid,
                        role,
                        subtype,
                        cpu_percent,
                        rss_mib: rss_pages * self.page_size / (1024 * 1024),
                    });
            }
        }
        // Pids no longer seen are dropped.
        self.previous = current;
        set
    }
}

/// `(utime + stime, rss pages)` from a `/proc/<pid>/stat` line. Fields are
/// counted from the last `)` so a command name with spaces or parentheses
/// cannot shift them.
fn parse_stat(stat: &str) -> Option<(u64, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `fields[0]` is field 3 (state), so field N is `fields[N - 3]`.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let rss: i64 = fields.get(21)?.parse().ok()?;
    Some((utime + stime, rss.max(0) as u64))
}

/// Role and utility subtype from a raw cmdline, which is either NUL-separated
/// or (after Chromium rewrites its process title) one space-joined string.
fn classify(cmdline: &[u8], is_root: bool) -> (String, Option<String>) {
    let mut role = None;
    let mut subtype = None;
    for arg in cmdline
        .split(|byte| *byte == 0 || byte.is_ascii_whitespace())
        .filter_map(|arg| std::str::from_utf8(arg).ok())
    {
        if let Some(value) = arg.strip_prefix("--type=") {
            role = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--utility-sub-type=") {
            subtype = Some(value.to_string());
        }
    }
    let role = match role {
        Some(role) => role,
        None if is_root => "browser".to_string(),
        None => "unknown".to_string(),
    };
    (role, subtype)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    /// Write one fake process. `ticks` is utime+stime (split across both
    /// fields), `rss` is in pages, `cmdline` is written verbatim.
    fn write_proc(
        root: &Path,
        pid: u32,
        parent: u32,
        comm: &str,
        ticks: u64,
        rss: u64,
        cmdline: &str,
    ) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        // Fields 3..=24: state, ppid, pgrp, session, tty, tpgid, flags,
        // minflt, cminflt, majflt, cmajflt, utime, stime, cutime, cstime,
        // priority, nice, threads, itrealvalue, starttime, vsize, rss.
        let stat = format!(
            "{pid} ({comm}) S {parent} {pid} {pid} 0 -1 4194560 0 0 0 0 {} {} 0 0 20 0 1 0 100 1000000 {rss} 0\n",
            ticks - ticks / 2,
            ticks / 2
        );
        std::fs::write(dir.join("stat"), stat).unwrap();
        std::fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    fn app(pid: u32) -> Vec<(String, u32)> {
        vec![("kiosk".to_string(), pid)]
    }

    #[test]
    fn roles_subtypes_and_rss() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        write_proc(p, 100, 1, "chrome", 0, 256, "/opt/chrome\0--kiosk\0");
        write_proc(
            p,
            101,
            100,
            "chrome",
            0,
            512,
            "/opt/chrome\0--type=gpu-process\0",
        );
        write_proc(
            p,
            102,
            100,
            "chrome",
            0,
            0,
            "/opt/chrome\0--type=utility\0--utility-sub-type=network.mojom.NetworkService\0",
        );
        write_proc(p, 103, 101, "sh", 0, 0, "sh\0");
        write_proc(p, 999, 1, "other", 0, 0, "other\0--type=renderer\0");
        let mut sampler = ProcessSampler::with_proc_root(p.to_path_buf());
        let set = sampler.sample(&app(100));
        let procs = &set.by_app["kiosk"];
        assert_eq!(procs.len(), 4);
        let by_pid = |pid: u32| procs.iter().find(|p| p.pid == pid).unwrap();
        assert_eq!(by_pid(100).role, "browser");
        assert_eq!(by_pid(100).rss_mib, 1);
        assert_eq!(by_pid(101).role, "gpu-process");
        assert_eq!(by_pid(101).rss_mib, 2);
        assert_eq!(by_pid(102).role, "utility");
        assert_eq!(
            by_pid(102).subtype.as_deref(),
            Some("network.mojom.NetworkService")
        );
        assert_eq!(by_pid(103).role, "unknown");
        assert_eq!(by_pid(103).subtype, None);
        assert_eq!(by_pid(100).cpu_percent, None);
        assert_eq!(
            set.owner[&101],
            ("kiosk".to_string(), "gpu-process".to_string())
        );
        assert!(!set.owner.contains_key(&999));
    }

    #[test]
    fn a_space_joined_cmdline_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        write_proc(p, 100, 1, "chrome", 0, 0, "/opt/chrome --kiosk\0");
        write_proc(
            p,
            101,
            100,
            "chrome",
            0,
            0,
            "/opt/chrome --type=utility --utility-sub-type=audio.mojom.AudioService\0",
        );
        let mut sampler = ProcessSampler::with_proc_root(p.to_path_buf());
        let set = sampler.sample(&app(100));
        let child = set.by_app["kiosk"].iter().find(|p| p.pid == 101).unwrap();
        assert_eq!(child.role, "utility");
        assert_eq!(child.subtype.as_deref(), Some("audio.mojom.AudioService"));
    }

    #[test]
    fn cpu_percent_comes_from_two_samples_and_vanished_pids_are_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        write_proc(p, 100, 1, "chrome", 1000, 0, "chrome\0");
        write_proc(p, 101, 100, "chrome", 50, 0, "chrome\0--type=renderer\0");
        let mut sampler = ProcessSampler::with_proc_root(p.to_path_buf());
        let start = Instant::now();
        sampler.sample_at(&app(100), start);

        // 5 s later: the root used 50 ticks (0.5 s, so 10 %), the child
        // vanished, and a new process appeared.
        std::fs::remove_dir_all(p.join("101")).unwrap();
        write_proc(p, 100, 1, "chrome", 1050, 0, "chrome\0");
        write_proc(p, 102, 100, "chrome", 10, 0, "chrome\0--type=renderer\0");
        let set = sampler.sample_at(&app(100), start + Duration::from_secs(5));
        let procs = &set.by_app["kiosk"];
        assert_eq!(procs.len(), 2);
        assert_eq!(procs[0].pid, 100);
        assert_eq!(procs[0].cpu_percent, Some(10.0));
        assert_eq!(procs[1].pid, 102);
        assert_eq!(procs[1].cpu_percent, None);

        // The vanished pid is forgotten: if it returns it has no history.
        write_proc(p, 101, 100, "chrome", 60, 0, "chrome\0--type=renderer\0");
        let set = sampler.sample_at(&app(100), start + Duration::from_secs(10));
        let back = set.by_app["kiosk"].iter().find(|p| p.pid == 101).unwrap();
        assert_eq!(back.cpu_percent, None);
    }

    #[test]
    fn a_command_name_with_parentheses_does_not_shift_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        write_proc(p, 100, 1, "Web Content (chrome) x", 200, 2560, "chrome\0");
        let mut sampler = ProcessSampler::with_proc_root(p.to_path_buf());
        let start = Instant::now();
        let first = sampler.sample_at(&app(100), start);
        assert_eq!(first.by_app["kiosk"][0].rss_mib, 10);
        write_proc(p, 100, 1, "Web Content (chrome) x", 300, 2560, "chrome\0");
        let second = sampler.sample_at(&app(100), start + Duration::from_secs(2));
        assert_eq!(second.by_app["kiosk"][0].cpu_percent, Some(50.0));
    }

    #[test]
    fn non_chromium_roots_are_walked_too() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        write_proc(p, 10, 1, "ue", 0, 0, "ue\0");
        write_proc(p, 11, 10, "helper", 0, 0, "helper\0");
        write_proc(p, 20, 1, "chrome", 0, 0, "chrome\0");
        let mut sampler = ProcessSampler::with_proc_root(p.to_path_buf());
        let roots = vec![("ue".to_string(), 10), ("kiosk".to_string(), 20)];
        let set = sampler.sample(&roots);
        assert_eq!(set.owner[&10], ("ue".to_string(), "browser".to_string()));
        assert_eq!(set.owner[&11], ("ue".to_string(), "unknown".to_string()));
        assert_eq!(set.owner[&20].0, "kiosk");
    }
}
