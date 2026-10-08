//! PCIe link health for display adapters, read from sysfs.
//!
//! A graphics card on a marginal link (a loose card, a bad riser, a slot with
//! a dirty contact) keeps working. The link layer notices corrupted packets,
//! rejects them and asks for them again, which the kernel counts as AER
//! *correctable* errors, and the only symptom is lost throughput and extra
//! latency, which on a show machine looks like an unexplained slow wall.
//! Nothing else in the stack reports it, so the `pcie-link` health check reads
//! the counters directly.
//!
//! The reading ([`read_links`]) and the judgment ([`judge`]) are separate so
//! the judgment is table-testable. Both take their inputs as parameters (a
//! sysfs root, an uptime) instead of looking at the real machine.
//!
//! # Where the errors are counted
//!
//! AER counters live on the device that *received* the bad packet, and a link
//! has two ends. On a machine with a sick link the errors were counted on the
//! GPU while the root port's own counters stayed at 0, so reading only the
//! port would have missed it. Each graphics card is therefore summed with its
//! upstream port (the parent directory of its resolved sysfs path, when that
//! is itself a PCI device). Sibling functions of the card, such as its HDMI
//! audio, are other devices and are ignored.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Correctable errors since boot that make [`judge`] warn, together with
/// [`BOOT_MIN_PER_HOUR`]. A handful over a long uptime is normal.
const BOOT_MIN_COUNT: u64 = 100;
/// Average correctable errors per hour since boot that make [`judge`] warn,
/// together with [`BOOT_MIN_COUNT`].
const BOOT_MIN_PER_HOUR: f64 = 10.0;
/// Correctable errors within the recent window that make [`judge`] warn,
/// together with [`RECENT_MIN_PER_HOUR`].
const RECENT_MIN_COUNT: u64 = 5;
/// Correctable errors per hour over the recent window that make [`judge`]
/// warn, together with [`RECENT_MIN_COUNT`].
const RECENT_MIN_PER_HOUR: f64 = 60.0;
/// How far back, in seconds, samples are kept for the recent rule.
const RECENT_WINDOW_SECS: f64 = 600.0;

/// AER error totals for one device. Each is `None` when the kernel exposes no
/// such file for it (AER not enabled, or firmware handles errors itself).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub correctable: Option<u64>,
    pub nonfatal: Option<u64>,
    pub fatal: Option<u64>,
}

impl Counters {
    /// Whether any counter could be read at all.
    fn measurable(&self) -> bool {
        self.correctable.is_some() || self.nonfatal.is_some() || self.fatal.is_some()
    }
}

/// Link speed and width as the kernel reports them, for information only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkInfo {
    pub speed: Option<String>,
    pub width: Option<String>,
    pub max_speed: Option<String>,
    pub max_width: Option<String>,
}

/// One graphics card and its upstream port, as read from sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReading {
    /// The card's PCI address, e.g. `0000:01:00.0`.
    pub address: String,
    /// Counters summed over the card and its upstream port, `None` per field
    /// when neither has the file.
    pub counters: Counters,
    pub link: LinkInfo,
}

/// Previous correctable-error samples per card, kept by the caller between
/// runs so the recent-rate rule has something to compare against.
#[derive(Debug, Default)]
pub struct History {
    /// Per address: `(uptime seconds, correctable total)`, oldest first.
    samples: HashMap<String, Vec<(f64, u64)>>,
}

impl History {
    /// Record a sample and return the errors counted and seconds elapsed
    /// between the oldest sample still inside the window and this one, or
    /// `None` when there is nothing older to compare with.
    fn record(&mut self, address: &str, uptime: f64, total: u64) -> Option<(u64, f64)> {
        let samples = self.samples.entry(address.to_string()).or_default();
        // A counter that went backwards means the driver was reloaded or the
        // device reset; the old samples describe something else.
        if samples.last().is_some_and(|&(_, last)| total < last) {
            samples.clear();
        }
        samples.retain(|&(at, _)| uptime - at <= RECENT_WINDOW_SECS && at <= uptime);
        let recent = samples
            .first()
            .map(|&(at, count)| (total - count, uptime - at))
            .filter(|&(_, secs)| secs > 0.0);
        samples.push((uptime, total));
        recent
    }
}

/// How bad a [`judge`]ment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Pass,
    Warn,
    Fail,
}

/// Read the first uptime field of a `/proc/uptime` style file, in seconds.
pub fn read_uptime(path: &Path) -> Option<f64> {
    fs::read_to_string(path)
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The value after `key` on its own line of an `aer_dev_*` file.
fn aer_total(path: &Path, key: &str) -> Option<u64> {
    fs::read_to_string(path)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(key)?.trim().parse().ok())
}

fn read_counters(device: &Path) -> Counters {
    Counters {
        correctable: aer_total(&device.join("aer_dev_correctable"), "TOTAL_ERR_COR"),
        nonfatal: aer_total(&device.join("aer_dev_nonfatal"), "TOTAL_ERR_NONFATAL"),
        fatal: aer_total(&device.join("aer_dev_fatal"), "TOTAL_ERR_FATAL"),
    }
}

/// Add two optional counters, `None` only when both are.
fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// Every display-class PCI device under `sysfs_root` (normally `/sys`), with
/// its upstream port's counters added in. Sorted by address.
pub fn read_links(sysfs_root: &Path) -> Vec<LinkReading> {
    let Ok(entries) = fs::read_dir(sysfs_root.join("bus/pci/devices")) else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Class 0x03xxxx is a display controller. The card's HDMI audio is
        // 0x0403xx, a different device, and is not looked at.
        let is_display = read_trimmed(&path.join("class")).is_some_and(|c| c.starts_with("0x03"));
        if !is_display {
            continue;
        }
        let mut counters = read_counters(&path);
        // The parent of the resolved path is the upstream port when it is a
        // PCI device itself; for a card on the root complex it is a bus
        // directory such as `pci0000:00`, which has no `class`.
        let upstream = fs::canonicalize(&path)
            .ok()
            .and_then(|real| real.parent().map(Path::to_path_buf))
            .filter(|parent| parent.join("class").is_file());
        if let Some(upstream) = upstream {
            let port = read_counters(&upstream);
            counters = Counters {
                correctable: add(counters.correctable, port.correctable),
                nonfatal: add(counters.nonfatal, port.nonfatal),
                fatal: add(counters.fatal, port.fatal),
            };
        }
        let link = LinkInfo {
            speed: link_speed(&path.join("current_link_speed")),
            width: link_width(&path.join("current_link_width")),
            max_speed: link_speed(&path.join("max_link_speed")),
            max_width: link_width(&path.join("max_link_width")),
        };
        // An integrated GPU sits on the root complex with no PCIe link: the
        // kernel reports its speed as `Unknown` and its width as 0 or 255,
        // and it has no error counters. There is no link to judge, and
        // listing it as "cannot be read" would only be noise beside the
        // discrete card that does have one.
        if !counters.measurable() && link.speed.is_none() && link.max_speed.is_none() {
            continue;
        }
        links.push(LinkReading {
            address: entry.file_name().to_string_lossy().into_owned(),
            counters,
            link,
        });
    }
    links.sort_by(|a, b| a.address.cmp(&b.address));
    links
}

/// A link speed, `None` when absent or `Unknown` (no PCIe link).
fn link_speed(path: &Path) -> Option<String> {
    read_trimmed(path).filter(|speed| !speed.starts_with("Unknown"))
}

/// A link width, `None` when absent, 0 (no link trained) or 255 (the
/// kernel's "not a PCIe link" value).
fn link_width(path: &Path) -> Option<String> {
    read_trimmed(path).filter(|width| width != "0" && width != "255")
}

/// `16.0 GT/s PCIe` becomes `16.0 GT/s`.
fn speed_text(speed: &str) -> &str {
    speed.strip_suffix(" PCIe").unwrap_or(speed)
}

/// `link x8, card supports 16.0 GT/s x16`, with whatever part the kernel did
/// not report left out. Information only. The current speed is deliberately
/// absent: it drops to the slowest rate at idle and climbs back under load,
/// and a detail that changed with it would be republished to every client on
/// nearly every run. The current width is stable, and an x8 board can carry
/// an x16-capable chip, so neither is judged.
fn link_text(link: &LinkInfo) -> Option<String> {
    let now = link.width.as_ref().map(|w| format!("link x{w}"));
    let max = match (&link.max_speed, &link.max_width) {
        (Some(s), Some(w)) => Some(format!("{} x{w}", speed_text(s))),
        _ => None,
    };
    match (now, max) {
        (Some(now), Some(max)) => Some(format!("{now}, card supports {max}")),
        (Some(now), None) => Some(now),
        (None, Some(max)) => Some(format!("card supports {max}")),
        (None, None) => None,
    }
}

/// What [`judge_detailed`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgment {
    pub severity: Severity,
    /// For the health check. Carries no number that drifts while the verdict
    /// holds, so an unchanged machine does not republish every run.
    pub detail: String,
    /// The live counts and rates, for the journal line logged when the
    /// severity changes.
    pub numbers: String,
}

/// Judge the readings. Pure apart from recording this sample into `history`.
///
/// - Any nonfatal or fatal count above 0 fails.
/// - Correctable errors warn when either of two rules holds. Since boot: at
///   least 100 and an average of at least 10 per hour. Recent: at least 5 and
///   at least 60 per hour across the samples kept from the last 10 minutes.
///   The first keeps the verdict stable through idle stretches, so the check
///   does not flap; the second catches a link that just went bad on a machine
///   with a long uptime.
/// - No display-class device passes, as does a card with no AER files.
///
/// `uptime_secs` is `None` when it could not be read; the time-based rules
/// are then skipped, but any nonfatal or fatal count still fails.
pub fn judge(
    readings: &[LinkReading],
    uptime_secs: Option<f64>,
    history: &mut History,
) -> (Severity, String) {
    let judgment = judge_detailed(readings, uptime_secs, history);
    (judgment.severity, judgment.detail)
}

/// [`judge`], also returning the live numbers behind the verdict.
pub fn judge_detailed(
    readings: &[LinkReading],
    uptime_secs: Option<f64>,
    history: &mut History,
) -> Judgment {
    if readings.is_empty() {
        return Judgment {
            severity: Severity::Pass,
            detail: "no PCIe graphics card".to_string(),
            numbers: String::new(),
        };
    }
    let mut worst = Severity::Pass;
    let mut lines = Vec::new();
    let mut numbers = Vec::new();
    for reading in readings {
        let (severity, line, live) = judge_one(reading, uptime_secs, history);
        worst = worst.max(severity);
        lines.push(line);
        numbers.push(live);
    }
    let mut detail = lines.join("; ");
    match worst {
        Severity::Pass => {}
        Severity::Warn => detail.push_str(
            ". Errors this frequent mean the PCIe link is marginal: reseat the card, \
             try another slot, or remove any riser",
        ),
        Severity::Fail => detail.push_str(
            ". Uncorrectable errors on the link can corrupt data: reseat the card, \
             try another slot, or remove any riser",
        ),
    }
    Judgment {
        severity: worst,
        detail,
        numbers: numbers.join("; "),
    }
}

fn judge_one(
    reading: &LinkReading,
    uptime_secs: Option<f64>,
    history: &mut History,
) -> (Severity, String, String) {
    let address = &reading.address;
    let info = link_text(&reading.link);
    let suffix = info.map(|i| format!(" ({i})")).unwrap_or_default();
    let counters = reading.counters;
    if !counters.measurable() {
        return (
            Severity::Pass,
            format!(
                "{address}: error counters cannot be read here (no aer_dev_* files: AER is \
                 not enabled for this device, or the firmware handles PCIe errors itself){suffix}"
            ),
            String::new(),
        );
    }

    let correctable = counters.correctable.unwrap_or(0);
    let nonfatal = counters.nonfatal.unwrap_or(0);
    let fatal = counters.fatal.unwrap_or(0);

    let hours = uptime_secs.map(|s| s / 3600.0).filter(|h| *h > 0.0);
    let boot_rate = hours.map(|h| correctable as f64 / h);
    let recent = uptime_secs.and_then(|up| history.record(address, up, correctable));

    let mut numbers = format!(
        "{address}: {correctable} correctable, {nonfatal} nonfatal, {fatal} fatal since boot"
    );
    if let Some(rate) = boot_rate {
        numbers.push_str(&format!(", {rate:.1} correctable per hour since boot"));
    }
    if let Some((count, secs)) = recent {
        numbers.push_str(&format!(
            ", {count} correctable in the last {:.0} s",
            secs.round()
        ));
    }

    // Each wording below depends only on the verdict, or on a count that
    // changes only when an error actually happens.
    let (severity, text) = if nonfatal > 0 || fatal > 0 {
        (
            Severity::Fail,
            format!("{address}: {nonfatal} nonfatal, {fatal} fatal errors since boot{suffix}"),
        )
    } else {
        let since_boot = correctable >= BOOT_MIN_COUNT
            && boot_rate.is_some_and(|rate| rate >= BOOT_MIN_PER_HOUR);
        let lately = recent.is_some_and(|(count, secs)| {
            count >= RECENT_MIN_COUNT && count as f64 / (secs / 3600.0) >= RECENT_MIN_PER_HOUR
        });
        if since_boot || lately {
            (
                Severity::Warn,
                format!(
                    "{address}: correctable errors are arriving faster than the warning rate \
                     ({BOOT_MIN_PER_HOUR:.0} per hour since boot or {RECENT_MIN_PER_HOUR:.0} \
                     per hour over {} minutes); live counts in \
                     /sys/bus/pci/devices/{address}/aer_dev_correctable{suffix}",
                    (RECENT_WINDOW_SECS / 60.0).round()
                ),
            )
        } else if correctable == 0 {
            (
                Severity::Pass,
                format!("{address}: no errors since boot{suffix}"),
            )
        } else {
            let noun = if correctable == 1 { "error" } else { "errors" };
            (
                Severity::Pass,
                format!(
                    "{address}: {correctable} correctable {noun} since boot, below the \
                     warning rate{suffix}"
                ),
            )
        }
    };
    (severity, text, numbers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const GPU: &str = "0000:01:00.0";
    const AUDIO: &str = "0000:01:00.1";
    const PORT: &str = "0000:00:01.0";

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn aer(dir: &Path, cor: u64, nonfatal: u64, fatal: u64) {
        write(
            &dir.join("aer_dev_correctable"),
            &format!("RxErr {cor}\nBadTLP 0\nTOTAL_ERR_COR {cor}\n"),
        );
        write(
            &dir.join("aer_dev_nonfatal"),
            &format!("Undefined 0\nTOTAL_ERR_NONFATAL {nonfatal}\n"),
        );
        write(
            &dir.join("aer_dev_fatal"),
            &format!("Undefined 0\nTOTAL_ERR_FATAL {fatal}\n"),
        );
    }

    /// A fake `/sys` with a GPU behind a root port, the way the real one is
    /// laid out: the entry under `bus/pci/devices` is a symlink into
    /// `devices/pci0000:00/<port>/<gpu>`. `gpu` and `port` are
    /// `(correctable, nonfatal, fatal)`; `None` leaves the AER files out.
    fn fake_sys(root: &Path, gpu: Option<(u64, u64, u64)>, port: Option<(u64, u64, u64)>) {
        let real = root.join("devices/pci0000:00");
        let port_dir = real.join(PORT);
        let gpu_dir = port_dir.join(GPU);
        let audio_dir = port_dir.join(AUDIO);
        write(&port_dir.join("class"), "0x060400\n");
        write(&gpu_dir.join("class"), "0x030000\n");
        write(&audio_dir.join("class"), "0x040300\n");
        write(&gpu_dir.join("current_link_speed"), "16.0 GT/s PCIe\n");
        write(&gpu_dir.join("current_link_width"), "8\n");
        write(&gpu_dir.join("max_link_speed"), "16.0 GT/s PCIe\n");
        write(&gpu_dir.join("max_link_width"), "16\n");
        if let Some((c, n, f)) = gpu {
            aer(&gpu_dir, c, n, f);
        }
        if let Some((c, n, f)) = port {
            aer(&port_dir, c, n, f);
        }
        // The audio function carries errors that must not be counted.
        aer(&audio_dir, 9999, 9, 9);
        let devices = root.join("bus/pci/devices");
        fs::create_dir_all(&devices).unwrap();
        // Rebuilt in place by tests that change the counters between runs.
        for (name, target) in [(GPU, &gpu_dir), (AUDIO, &audio_dir), (PORT, &port_dir)] {
            let _ = fs::remove_file(devices.join(name));
            symlink(target, devices.join(name)).unwrap();
        }
    }

    const HOUR: f64 = 3600.0;

    fn run(root: &Path, uptime: f64, history: &mut History) -> (Severity, String) {
        judge(&read_links(root), Some(uptime), history)
    }

    #[test]
    fn clean_card_passes_and_shows_link_details() {
        let dir = tempfile::tempdir().unwrap();
        fake_sys(dir.path(), Some((0, 0, 0)), Some((0, 0, 0)));
        let (severity, detail) = run(dir.path(), 5.0 * HOUR, &mut History::default());
        assert_eq!(severity, Severity::Pass);
        assert!(detail.contains(GPU), "{detail}");
        assert!(detail.contains("no errors since boot"), "{detail}");
        assert!(!detail.contains("GT/s PCIe"), "{detail}");
        assert!(
            detail.contains("link x8, card supports 16.0 GT/s x16"),
            "{detail}"
        );
    }

    #[test]
    fn warns_on_the_since_boot_rule() {
        let dir = tempfile::tempdir().unwrap();
        fake_sys(dir.path(), Some((241, 0, 0)), Some((0, 0, 0)));
        let (severity, detail) = run(dir.path(), 2.0 * HOUR, &mut History::default());
        assert_eq!(severity, Severity::Warn, "{detail}");
        assert!(detail.contains("faster than the warning rate"), "{detail}");
        assert!(detail.contains("aer_dev_correctable"), "{detail}");
    }

    #[test]
    fn does_not_warn_on_a_few_errors_over_a_long_uptime() {
        let dir = tempfile::tempdir().unwrap();
        fake_sys(dir.path(), Some((241, 0, 0)), Some((0, 0, 0)));
        // 241 over 100 hours is 2.4 per hour.
        let (severity, _) = run(dir.path(), 100.0 * HOUR, &mut History::default());
        assert_eq!(severity, Severity::Pass);
    }

    #[test]
    fn warns_on_the_recent_rule_when_a_long_uptime_hides_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::default();
        let up = 1000.0 * HOUR;
        fake_sys(dir.path(), Some((3, 0, 0)), Some((0, 0, 0)));
        assert_eq!(run(dir.path(), up, &mut history).0, Severity::Pass);
        // Ten more in five minutes is 120 per hour; since boot it is still
        // 13 errors over 1000 hours.
        fake_sys(dir.path(), Some((13, 0, 0)), Some((0, 0, 0)));
        let (severity, detail) = run(dir.path(), up + 300.0, &mut history);
        assert_eq!(severity, Severity::Warn, "{detail}");
        assert!(detail.contains("faster than the warning rate"), "{detail}");
    }

    #[test]
    fn recent_rule_needs_enough_errors_and_a_high_enough_rate() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::default();
        let up = 1000.0 * HOUR;
        fake_sys(dir.path(), Some((0, 0, 0)), None);
        run(dir.path(), up, &mut history);
        // Four errors: below the count floor however fast.
        fake_sys(dir.path(), Some((4, 0, 0)), None);
        assert_eq!(run(dir.path(), up + 10.0, &mut history).0, Severity::Pass);
        // Five more over an hour and a half, once the first samples have
        // aged out of the window: five errors, but only a few per hour.
        fake_sys(dir.path(), Some((9, 0, 0)), None);
        assert_eq!(run(dir.path(), up + 5400.0, &mut history).0, Severity::Pass);
    }

    #[test]
    fn fails_on_a_nonfatal_error() {
        let dir = tempfile::tempdir().unwrap();
        fake_sys(dir.path(), Some((0, 1, 0)), Some((0, 0, 0)));
        let (severity, detail) = run(dir.path(), HOUR, &mut History::default());
        assert_eq!(severity, Severity::Fail, "{detail}");
        assert!(detail.contains("1 nonfatal, 0 fatal"), "{detail}");
    }

    #[test]
    fn fails_on_a_fatal_error() {
        let dir = tempfile::tempdir().unwrap();
        fake_sys(dir.path(), Some((0, 0, 0)), Some((0, 0, 2)));
        assert_eq!(
            run(dir.path(), HOUR, &mut History::default()).0,
            Severity::Fail
        );
    }

    #[test]
    fn card_without_aer_files_passes_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        fake_sys(dir.path(), None, None);
        let (severity, detail) = run(dir.path(), HOUR, &mut History::default());
        assert_eq!(severity, Severity::Pass);
        assert!(detail.contains("cannot be read"), "{detail}");
        assert!(detail.contains("AER"), "{detail}");
    }

    #[test]
    fn no_graphics_card_passes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bus/pci/devices")).unwrap();
        let (severity, detail) = run(dir.path(), HOUR, &mut History::default());
        assert_eq!(severity, Severity::Pass);
        assert_eq!(detail, "no PCIe graphics card");
        // And a sysfs with no PCI bus at all, as on a Pi.
        let empty = tempfile::tempdir().unwrap();
        assert!(read_links(empty.path()).is_empty());
    }

    #[test]
    fn upstream_port_errors_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        fake_sys(dir.path(), Some((0, 0, 0)), Some((0, 1, 0)));
        let links = read_links(dir.path());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].counters.nonfatal, Some(1));
        assert_eq!(
            run(dir.path(), HOUR, &mut History::default()).0,
            Severity::Fail
        );
    }

    #[test]
    fn upstream_port_alone_makes_a_card_measurable() {
        let dir = tempfile::tempdir().unwrap();
        fake_sys(dir.path(), None, Some((500, 0, 0)));
        let (severity, _) = run(dir.path(), HOUR, &mut History::default());
        assert_eq!(severity, Severity::Warn);
    }

    #[test]
    fn hdmi_audio_sibling_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        // The audio function's fake counters are huge; only the GPU is read.
        fake_sys(dir.path(), Some((0, 0, 0)), Some((0, 0, 0)));
        let links = read_links(dir.path());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].address, GPU);
        assert_eq!(links[0].counters.correctable, Some(0));
        assert_eq!(
            run(dir.path(), HOUR, &mut History::default()).0,
            Severity::Pass
        );
    }

    /// An integrated GPU on the root complex, as the kernel shows one: no
    /// PCIe link and no error counters.
    fn fake_integrated(root: &Path) {
        let igpu = root.join("devices/pci0000:00/0000:00:02.0");
        write(&igpu.join("class"), "0x030000\n");
        write(&igpu.join("current_link_speed"), "Unknown\n");
        write(&igpu.join("current_link_width"), "0\n");
        write(&igpu.join("max_link_speed"), "Unknown\n");
        write(&igpu.join("max_link_width"), "255\n");
        let devices = root.join("bus/pci/devices");
        fs::create_dir_all(&devices).unwrap();
        symlink(&igpu, devices.join("0000:00:02.0")).unwrap();
    }

    #[test]
    fn an_integrated_gpu_without_a_link_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        fake_integrated(dir.path());
        assert!(read_links(dir.path()).is_empty());
        let (severity, detail) = run(dir.path(), HOUR, &mut History::default());
        assert_eq!(severity, Severity::Pass);
        assert_eq!(detail, "no PCIe graphics card");

        fake_sys(dir.path(), Some((0, 0, 0)), Some((0, 0, 0)));
        let (_, detail) = run(dir.path(), HOUR, &mut History::default());
        assert!(!detail.contains("0000:00:02.0"), "{detail}");
        assert!(!detail.contains("Unknown"), "{detail}");
        assert!(detail.contains(GPU), "{detail}");
    }

    #[test]
    fn counter_reset_clears_the_recent_history() {
        let mut history = History::default();
        assert_eq!(history.record("a", 100.0, 50), None);
        assert_eq!(history.record("a", 160.0, 80), Some((30, 60.0)));
        // The counter went backwards: nothing to compare with.
        assert_eq!(history.record("a", 220.0, 2), None);
    }

    #[test]
    fn reads_uptime_from_the_first_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uptime");
        write(&path, "12345.67 54321.00\n");
        assert_eq!(read_uptime(&path), Some(12345.67));
        assert_eq!(read_uptime(&dir.path().join("missing")), None);
    }

    #[test]
    fn detail_is_byte_identical_while_the_verdict_holds() {
        let dir = tempfile::tempdir().unwrap();
        let speed = dir
            .path()
            .join("devices/pci0000:00")
            .join(PORT)
            .join(GPU)
            .join("current_link_speed");

        // Warn: the count keeps rising and the link speed changes, so the
        // recent rate and the running total differ on every run.
        let mut history = History::default();
        let up = 1000.0 * HOUR;
        let mut details = Vec::new();
        for (i, count) in [3u64, 400, 900, 1700, 2600].into_iter().enumerate() {
            fake_sys(dir.path(), Some((count, 0, 0)), Some((0, 0, 0)));
            fs::write(
                &speed,
                if i % 2 == 0 {
                    "2.5 GT/s PCIe\n"
                } else {
                    "16.0 GT/s PCIe\n"
                },
            )
            .unwrap();
            let (severity, detail) = run(dir.path(), up + 60.0 * i as f64, &mut history);
            if i > 0 {
                assert_eq!(severity, Severity::Warn, "{detail}");
                details.push(detail);
            }
        }
        assert!(
            details.windows(2).all(|pair| pair[0] == pair[1]),
            "{details:?}"
        );

        // Pass with a nonzero count below the threshold that does not change.
        let mut history = History::default();
        let mut details = Vec::new();
        for i in 0..4 {
            fake_sys(dir.path(), Some((7, 0, 0)), Some((0, 0, 0)));
            fs::write(
                &speed,
                if i % 2 == 0 {
                    "2.5 GT/s PCIe\n"
                } else {
                    "16.0 GT/s PCIe\n"
                },
            )
            .unwrap();
            let (severity, detail) = run(dir.path(), up + 60.0 * i as f64, &mut history);
            assert_eq!(severity, Severity::Pass, "{detail}");
            details.push(detail);
        }
        assert!(
            details.windows(2).all(|pair| pair[0] == pair[1]),
            "{details:?}"
        );
        assert!(
            details[0].contains("7 correctable errors since boot, below the warning rate"),
            "{details:?}"
        );
    }
}
