//! GPU telemetry from `nvidia-smi`, for the System tab's GPU panel.
//!
//! Three invocations per sample: `--query-gpu` for per-GPU utilization, memory,
//! temperature, power, clocks and PCIe link, `pmon -c 1 -s um` for the
//! per-process table, and `dmon -c 1 -s t` for PCIe throughput. Measured on an
//! appliance with a Quadro RTX 8000 they take about 23 ms, 54 ms and 68 ms of
//! wall time and negligible CPU, so sampling every few seconds is cheap. Each call has its own short timeout so a wedged driver
//! cannot stall the telemetry task.
//!
//! Parsing is separated from process execution so it can be tested against
//! captured output. Fields the GPU does not support print as `[N/A]`,
//! `[Not Supported]` or `-`; all of these become `None` rather than errors.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How long each `nvidia-smi` invocation may run before it is abandoned.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// The `--query-gpu` fields, in the order [`parse_query_gpu`] expects them.
const QUERY_FIELDS: &str = "name,driver_version,pstate,utilization.gpu,utilization.memory,\
utilization.decoder,utilization.encoder,memory.used,memory.total,temperature.gpu,\
power.draw,power.limit,clocks.sm,clocks.mem,pcie.link.gen.current,\
pcie.link.width.current,fan.speed";

/// What `nvidia-smi` reported on the last poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GpuTelemetry {
    /// `nvidia-smi` was found and answered on the last poll.
    pub available: bool,
    /// Why not, when `available` is false ("nvidia-smi is not installed",
    /// "nvidia-smi exited with status 9: ...", "nvidia-smi timed out"). Also
    /// carries the process-table failure when only that part failed.
    pub reason: Option<String>,
    /// Unix seconds of the last successful sample.
    pub sampled_at: Option<u64>,
    /// One entry per GPU.
    pub gpus: Vec<GpuSample>,
    /// One entry per process using a GPU.
    pub processes: Vec<GpuProcessSample>,
    /// Samples from the last [`crate::diagnostics::HISTORY_WINDOW`], oldest
    /// first, at most [`crate::diagnostics::history_capacity`] points: one per successful `nvidia-smi` poll. Kept
    /// when a later poll fails, so the graph survives a blip. Empty from the
    /// parser; filled by the telemetry task.
    pub history: Vec<GpuHistoryPoint>,
}

impl Default for GpuTelemetry {
    /// Nothing sampled yet: unavailable, with no reason.
    fn default() -> Self {
        Self {
            available: false,
            reason: None,
            sampled_at: None,
            gpus: Vec::new(),
            processes: Vec::new(),
            history: Vec::new(),
        }
    }
}

impl GpuTelemetry {
    /// A result for when sampling was not possible.
    fn unavailable(reason: String) -> Self {
        Self {
            available: false,
            reason: Some(reason),
            sampled_at: None,
            gpus: Vec::new(),
            processes: Vec::new(),
            history: Vec::new(),
        }
    }
}

/// Every GPU's readings at one successful poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GpuHistoryPoint {
    /// Unix seconds of the sample.
    pub at: u64,
    /// One entry per GPU.
    pub gpus: Vec<GpuHistorySample>,
}

/// The readings worth graphing from one GPU at one poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GpuHistorySample {
    /// Position in `nvidia-smi`'s output, as in [`GpuSample::index`].
    pub index: u32,
    /// Graphics engine utilization.
    pub utilization_gpu_percent: Option<u32>,
    /// Memory controller utilization.
    pub utilization_memory_percent: Option<u32>,
    /// Video decoder utilization.
    pub utilization_decoder_percent: Option<u32>,
    /// Video encoder utilization.
    pub utilization_encoder_percent: Option<u32>,
    /// Framebuffer memory in use.
    pub memory_used_mib: Option<u64>,
    /// Total framebuffer memory.
    pub memory_total_mib: Option<u64>,
    /// Core temperature.
    pub temperature_c: Option<u32>,
    /// Current power draw.
    pub power_draw_w: Option<f64>,
    /// Shader clock.
    pub clock_sm_mhz: Option<u32>,
    /// PCIe generation in use when the sample was taken, so a point can be
    /// read against the link it was measured on.
    pub pcie_generation: Option<u32>,
    /// PCIe lane count in use when the sample was taken.
    pub pcie_width: Option<u32>,
    /// MB/s received by the GPU (host to GPU), from `dmon -s t`.
    pub pcie_rx_mb_s: Option<u32>,
    /// MB/s sent by the GPU (GPU to host).
    pub pcie_tx_mb_s: Option<u32>,
    /// `pcie_rx_mb_s` as a percent of the link as currently trained
    /// (`pcie_generation` x `pcie_width`), one decimal; null when any input is.
    pub pcie_rx_percent: Option<f64>,
    /// `pcie_tx_mb_s` as a percent of the link as currently trained.
    pub pcie_tx_percent: Option<f64>,
}

/// One GPU's current readings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GpuSample {
    /// Position in `nvidia-smi`'s output.
    pub index: u32,
    /// Product name.
    pub name: String,
    /// Driver version string.
    pub driver_version: Option<String>,
    /// Performance state, e.g. "P2".
    pub performance_state: Option<String>,
    /// Graphics engine utilization.
    pub utilization_gpu_percent: Option<u32>,
    /// Memory controller utilization.
    pub utilization_memory_percent: Option<u32>,
    /// Video decoder utilization.
    pub utilization_decoder_percent: Option<u32>,
    /// Video encoder utilization.
    pub utilization_encoder_percent: Option<u32>,
    /// Framebuffer memory in use.
    pub memory_used_mib: Option<u64>,
    /// Total framebuffer memory.
    pub memory_total_mib: Option<u64>,
    /// Core temperature.
    pub temperature_c: Option<u32>,
    /// Current power draw.
    pub power_draw_w: Option<f64>,
    /// Enforced power limit.
    pub power_limit_w: Option<f64>,
    /// Shader clock.
    pub clock_sm_mhz: Option<u32>,
    /// Memory clock.
    pub clock_memory_mhz: Option<u32>,
    /// Current PCIe generation.
    pub pcie_generation: Option<u32>,
    /// Current PCIe lane count.
    pub pcie_width: Option<u32>,
    /// MB/s received by the GPU (host to GPU), from `dmon -s t`.
    pub pcie_rx_mb_s: Option<u32>,
    /// MB/s sent by the GPU (GPU to host).
    pub pcie_tx_mb_s: Option<u32>,
    /// `pcie_rx_mb_s` as a percent of the link as currently trained
    /// (`pcie_generation` x `pcie_width`), one decimal; null when any input is.
    /// NVIDIA retrains the link down at idle, so this can move when traffic
    /// does not.
    pub pcie_rx_percent: Option<f64>,
    /// `pcie_tx_mb_s` as a percent of the link as currently trained.
    pub pcie_tx_percent: Option<f64>,
    /// Fan speed.
    pub fan_percent: Option<u32>,
}

/// What a process does on the GPU, from `pmon`'s `type` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum GpuProcessKind {
    /// Graphics (`G`).
    Graphics,
    /// Compute (`C`).
    Compute,
    /// Both (`C+G`).
    Both,
}

/// One process's use of one GPU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GpuProcessSample {
    /// Which GPU, as in [`GpuSample::index`].
    pub gpu_index: u32,
    /// Process id.
    pub pid: u32,
    /// Graphics, compute or both.
    pub kind: GpuProcessKind,
    /// The `command` column, or "" when absent.
    pub command: String,
    /// Streaming multiprocessor utilization.
    pub sm_percent: Option<u32>,
    /// Memory utilization.
    pub memory_percent: Option<u32>,
    /// Encoder utilization.
    pub encoder_percent: Option<u32>,
    /// Decoder utilization.
    pub decoder_percent: Option<u32>,
    /// Framebuffer memory held.
    pub framebuffer_mib: Option<u64>,
    /// The Suede app whose process tree this PID is in, if any. Filled by the
    /// telemetry task, never by the parser.
    pub app: Option<String>,
    /// Chromium role when the app is a `chromium-kiosk` app and the PID is one
    /// of its processes ("browser", "gpu-process", "renderer", ...). Filled by
    /// the telemetry task, never by the parser.
    pub role: Option<String>,
}

/// Sample every GPU and the processes using them.
///
/// Never fails: a missing binary, a non-zero exit or a timeout yields
/// `available: false` with a reason. If only the process table or the PCIe
/// throughput fails, the GPU readings are still returned with the failure in
/// `reason` (both, joined with "; ", if both fail) and the affected fields
/// empty.
pub async fn sample() -> GpuTelemetry {
    sample_with("nvidia-smi").await
}

/// [`sample`] against `program`, so a test can substitute a script.
///
/// The three commands are independent, so they run concurrently and the
/// call waits for the slowest instead of the sum. Each keeps its own
/// [`COMMAND_TIMEOUT`]. A failed query makes the whole sample unavailable,
/// as before; the other two answers are then simply dropped.
async fn sample_with(program: &str) -> GpuTelemetry {
    let query_args = [
        format!("--query-gpu={QUERY_FIELDS}"),
        "--format=csv,noheader,nounits".to_string(),
    ];
    let pmon_args = ["pmon", "-c", "1", "-s", "um"].map(String::from);
    // `dmon` reports throughput only; the percent needs the link generation
    // and width from this same poll's query, so it is applied below.
    let dmon_args = ["dmon", "-c", "1", "-s", "t"].map(String::from);
    let (gpu_result, pmon_result, dmon_result) = tokio::join!(
        run(program, &query_args),
        run(program, &pmon_args),
        run(program, &dmon_args),
    );
    let gpu_text = match gpu_result {
        Ok(text) => text,
        Err(reason) => return GpuTelemetry::unavailable(reason),
    };
    let mut gpus = parse_query_gpu(&gpu_text);
    let (processes, pmon_reason) = match pmon_result {
        Ok(text) => (parse_pmon(&text), None),
        Err(reason) => (Vec::new(), Some(reason)),
    };
    let dmon_reason = match dmon_result {
        Ok(text) => {
            apply_pcie_throughput(&mut gpus, &parse_dmon_pcie(&text));
            None
        }
        Err(reason) => Some(reason),
    };
    let reason = match (pmon_reason, dmon_reason) {
        (Some(pmon), Some(dmon)) => Some(format!("{pmon}; {dmon}")),
        (one, other) => one.or(other),
    };

    let sampled_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .ok();
    GpuTelemetry {
        available: true,
        reason,
        sampled_at,
        gpus,
        processes,
        history: Vec::new(),
    }
}

/// Fill each GPU's bus throughput and percent from `dmon` rows, matched on
/// GPU index. A GPU with no row keeps its fields `None`.
fn apply_pcie_throughput(gpus: &mut [GpuSample], rows: &[PcieThroughput]) {
    for gpu in gpus {
        let Some(row) = rows.iter().find(|row| row.gpu_index == gpu.index) else {
            continue;
        };
        gpu.pcie_rx_mb_s = row.rx_mb_s;
        gpu.pcie_tx_mb_s = row.tx_mb_s;
        gpu.pcie_rx_percent = row
            .rx_mb_s
            .and_then(|mb_s| pcie_percent(mb_s, gpu.pcie_generation, gpu.pcie_width));
        gpu.pcie_tx_percent = row
            .tx_mb_s
            .and_then(|mb_s| pcie_percent(mb_s, gpu.pcie_generation, gpu.pcie_width));
    }
}

/// Usable MB/s per lane, per direction, after line encoding, for a PCIe
/// generation: Gen 1 250, Gen 2 500, Gen 3 985, Gen 4 1969, Gen 5 3938,
/// Gen 6 7563. `None` for an unknown generation.
pub fn pcie_lane_mb_s(generation: Option<u32>) -> Option<u32> {
    match generation? {
        1 => Some(250),
        2 => Some(500),
        3 => Some(985),
        4 => Some(1969),
        5 => Some(3938),
        6 => Some(7563),
        _ => None,
    }
}

/// `mb_s` as a percent of the link as currently trained (per-lane rate times
/// `width`), rounded to one decimal. `None` when the generation or width is
/// unknown or the width is zero. Never the card's or slot's maximum: NVIDIA
/// drops an idle link to Gen 1, and the percent is of what is trained now.
pub fn pcie_percent(mb_s: u32, generation: Option<u32>, width: Option<u32>) -> Option<f64> {
    let capacity = u64::from(pcie_lane_mb_s(generation)?) * u64::from(width?);
    if capacity == 0 {
        return None;
    }
    let percent = f64::from(mb_s) * 100.0 / capacity as f64;
    Some((percent * 10.0).round() / 10.0)
}

/// Run `nvidia-smi` with a timeout and return its stdout, or a reason it
/// could not be used.
async fn run(program: &str, args: &[String]) -> Result<String, String> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).kill_on_drop(true);
    let output = match tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await {
        Err(_) => return Err("nvidia-smi timed out".to_string()),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("nvidia-smi is not installed".to_string());
        }
        Ok(Err(error)) => return Err(format!("nvidia-smi could not run: {error}")),
        Ok(Ok(output)) => output,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = output
            .status
            .code()
            .map_or_else(|| "a signal".to_string(), |code| format!("status {code}"));
        return Err(format!(
            "nvidia-smi exited with {status}: {}",
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a field is `nvidia-smi`'s way of saying "no value".
fn is_missing(field: &str) -> bool {
    matches!(field, "" | "-" | "[N/A]" | "[Not Supported]" | "N/A")
}

/// A text field, or `None` when missing.
fn text(field: &str) -> Option<String> {
    let field = field.trim();
    (!is_missing(field)).then(|| field.to_string())
}

/// A numeric field, or `None` when missing or unparsable.
fn number<T: std::str::FromStr>(field: &str) -> Option<T> {
    let field = field.trim();
    if is_missing(field) {
        None
    } else {
        field.parse().ok()
    }
}

/// Parse the output of `--query-gpu=<QUERY_FIELDS> --format=csv,noheader,nounits`.
///
/// One GPU per non-empty line; `index` is the line's position. Lines with too
/// few fields are skipped.
pub fn parse_query_gpu(output: &str) -> Vec<GpuSample> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
        .filter_map(|(index, line)| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 17 {
                return None;
            }
            Some(GpuSample {
                index: u32::try_from(index).ok()?,
                name: fields[0].to_string(),
                driver_version: text(fields[1]),
                performance_state: text(fields[2]),
                utilization_gpu_percent: number(fields[3]),
                utilization_memory_percent: number(fields[4]),
                utilization_decoder_percent: number(fields[5]),
                utilization_encoder_percent: number(fields[6]),
                memory_used_mib: number(fields[7]),
                memory_total_mib: number(fields[8]),
                temperature_c: number(fields[9]),
                power_draw_w: number(fields[10]),
                power_limit_w: number(fields[11]),
                clock_sm_mhz: number(fields[12]),
                clock_memory_mhz: number(fields[13]),
                pcie_generation: number(fields[14]),
                pcie_width: number(fields[15]),
                pcie_rx_mb_s: None,
                pcie_tx_mb_s: None,
                pcie_rx_percent: None,
                pcie_tx_percent: None,
                fan_percent: number(fields[16]),
            })
        })
        .collect()
}

/// Parse the output of `nvidia-smi pmon -c 1 -s um`.
///
/// The column set depends on the driver release: 615 prints `gpu, pid,
/// type, sm, mem, enc, dec, jpg, ofa, fb, ccpm, command`, older releases
/// have no `jpg`/`ofa` (or `ccpm`) columns at all. So the first `# gpu ...`
/// header line decides where each value sits, and a column the header does
/// not name is simply `None`. Rows before any header are skipped. `command`
/// is everything after the last named numeric column, joined with spaces,
/// or empty. `app` and `role` are left `None`.
pub fn parse_pmon(output: &str) -> Vec<GpuProcessSample> {
    let mut columns: Option<PmonColumns> = None;
    let mut samples = Vec::new();
    for line in output.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('#') {
            let names: Vec<&str> = header.split_whitespace().collect();
            // The first header line names the columns; the second holds
            // units (`Idx`, `#`, `C/G`, `%`, ...), which must not replace it.
            if names.first() == Some(&"gpu") {
                columns = PmonColumns::from_names(&names);
            }
            continue;
        }
        let Some(columns) = &columns else {
            continue;
        };
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() <= columns.last_numeric {
            continue;
        }
        let at = |index: Option<usize>| index.and_then(|i| fields.get(i).copied());
        let kind = match at(columns.kind) {
            Some("G") => GpuProcessKind::Graphics,
            Some("C") => GpuProcessKind::Compute,
            Some("C+G") | Some("G+C") => GpuProcessKind::Both,
            _ => continue,
        };
        let (Some(gpu_index), Some(pid)) = (
            at(columns.gpu).and_then(|f| f.parse().ok()),
            at(columns.pid).and_then(|f| f.parse().ok()),
        ) else {
            continue;
        };
        samples.push(GpuProcessSample {
            gpu_index,
            pid,
            kind,
            command: fields[columns.last_numeric + 1..].join(" "),
            sm_percent: at(columns.sm).and_then(number),
            memory_percent: at(columns.mem).and_then(number),
            encoder_percent: at(columns.enc).and_then(number),
            decoder_percent: at(columns.dec).and_then(number),
            framebuffer_mib: at(columns.fb).and_then(number),
            app: None,
            role: None,
        });
    }
    samples
}

/// One GPU's PCIe throughput from `dmon -s t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcieThroughput {
    /// The `gpu` column.
    pub gpu_index: u32,
    /// `rxpci`: MB/s the GPU receives (host to GPU); `None` for `-`.
    pub rx_mb_s: Option<u32>,
    /// `txpci`: MB/s the GPU sends (GPU to host); `None` for `-`.
    pub tx_mb_s: Option<u32>,
}

/// Parse the output of `nvidia-smi dmon -c 1 -s t`.
///
/// Measured on 615:
///
/// ```text
/// # gpu  rxpci  txpci
/// # Idx   MB/s   MB/s
///     0    344     28
/// ```
///
/// As with [`parse_pmon`], the first `# gpu ...` header line decides which
/// column is which, a column the header does not name is `None`, and rows
/// before a header are skipped. `-` means no sample. NVML measures these over
/// about 20 ms, so each value is a snapshot rather than a mean over the poll.
pub fn parse_dmon_pcie(output: &str) -> Vec<PcieThroughput> {
    let mut columns: Option<(Option<usize>, Option<usize>, Option<usize>)> = None;
    let mut rows = Vec::new();
    for line in output.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('#') {
            let names: Vec<&str> = header.split_whitespace().collect();
            // The second header line holds units (`Idx`, `MB/s`) and must not
            // replace the names.
            if names.first() == Some(&"gpu") {
                let find = |name: &str| names.iter().position(|n| *n == name);
                columns = Some((find("gpu"), find("rxpci"), find("txpci")));
            }
            continue;
        }
        let Some((gpu, rx, tx)) = columns else {
            continue;
        };
        let fields: Vec<&str> = line.split_whitespace().collect();
        let at = |index: Option<usize>| index.and_then(|i| fields.get(i).copied());
        let Some(gpu_index) = at(gpu).and_then(|f| f.parse().ok()) else {
            continue;
        };
        rows.push(PcieThroughput {
            gpu_index,
            rx_mb_s: at(rx).and_then(number),
            tx_mb_s: at(tx).and_then(number),
        });
    }
    rows
}

/// Where each `pmon` value sits on a row, from the `# gpu pid type ...`
/// header line.
#[derive(Debug, Clone, Copy)]
struct PmonColumns {
    gpu: Option<usize>,
    pid: Option<usize>,
    kind: Option<usize>,
    sm: Option<usize>,
    mem: Option<usize>,
    enc: Option<usize>,
    dec: Option<usize>,
    fb: Option<usize>,
    /// Index of the last column that is not `command`, so the command (which
    /// may contain spaces) is everything after it.
    last_numeric: usize,
}

impl PmonColumns {
    fn from_names(names: &[&str]) -> Option<Self> {
        let find = |name: &str| names.iter().position(|n| *n == name);
        let (gpu, pid, kind) = (find("gpu"), find("pid"), find("type"));
        if gpu.is_none() || pid.is_none() || kind.is_none() {
            return None;
        }
        let last_numeric = match find("command") {
            Some(index) if index > 0 => index - 1,
            // No command column: every column is a value.
            _ => names.len() - 1,
        };
        Some(Self {
            gpu,
            pid,
            kind,
            sm: find("sm"),
            mem: find("mem"),
            enc: find("enc"),
            dec: find("dec"),
            fb: find("fb"),
            last_numeric,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: &str = "Quadro RTX 8000, 615.71.09, P2, 0, 0, 0, 0, 395, 49152, 52, 65.22, 260.00, 1020, 6500, 3, 16, 33\n";

    const PMON: &str = "\
# gpu         pid   type     sm    mem    enc    dec    jpg    ofa     fb   ccpm    command
# Idx           #    C/G      %      %      %      %      %      %     MB     MB    name
    0       3049     G      -      -      -      -      -      -      3      0    sway
    0    1181783     C      -      -      -      -      -      -    270      0    python
";

    #[test]
    fn query_gpu_fixture() {
        let gpus = parse_query_gpu(QUERY);
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.index, 0);
        assert_eq!(gpu.name, "Quadro RTX 8000");
        assert_eq!(gpu.driver_version.as_deref(), Some("615.71.09"));
        assert_eq!(gpu.performance_state.as_deref(), Some("P2"));
        assert_eq!(gpu.utilization_gpu_percent, Some(0));
        assert_eq!(gpu.memory_used_mib, Some(395));
        assert_eq!(gpu.memory_total_mib, Some(49152));
        assert_eq!(gpu.temperature_c, Some(52));
        assert_eq!(gpu.power_draw_w, Some(65.22));
        assert_eq!(gpu.power_limit_w, Some(260.0));
        assert_eq!(gpu.clock_sm_mhz, Some(1020));
        assert_eq!(gpu.clock_memory_mhz, Some(6500));
        assert_eq!(gpu.pcie_generation, Some(3));
        assert_eq!(gpu.pcie_width, Some(16));
        assert_eq!(gpu.fan_percent, Some(33));
    }

    #[test]
    fn query_gpu_unsupported_fields_are_none() {
        let line = "Tesla T4, 550.1, [N/A], [Not Supported], 0, 0, 0, 10, 100, 40, [N/A], [N/A], 300, 500, 3, 16, [N/A]\nTesla T4, 550.1, P8, 1, 2, 3, 4, 5, 6, 7, 8.5, 9.5, 10, 11, 1, 4, 12\n";
        let gpus = parse_query_gpu(line);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].performance_state, None);
        assert_eq!(gpus[0].utilization_gpu_percent, None);
        assert_eq!(gpus[0].power_draw_w, None);
        assert_eq!(gpus[0].fan_percent, None);
        assert_eq!(gpus[0].temperature_c, Some(40));
        assert_eq!(gpus[1].index, 1);
        assert_eq!(gpus[1].fan_percent, Some(12));
    }

    #[test]
    fn query_gpu_empty_and_short_lines() {
        assert!(parse_query_gpu("").is_empty());
        assert!(parse_query_gpu("\n\n").is_empty());
        assert!(parse_query_gpu("a, b, c\n").is_empty());
    }

    #[test]
    fn pmon_fixture() {
        let processes = parse_pmon(PMON);
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].gpu_index, 0);
        assert_eq!(processes[0].pid, 3049);
        assert_eq!(processes[0].kind, GpuProcessKind::Graphics);
        assert_eq!(processes[0].command, "sway");
        assert_eq!(processes[0].sm_percent, None);
        assert_eq!(processes[0].decoder_percent, None);
        assert_eq!(processes[0].framebuffer_mib, Some(3));
        assert_eq!(processes[0].app, None);
        assert_eq!(processes[0].role, None);
        assert_eq!(processes[1].pid, 1181783);
        assert_eq!(processes[1].kind, GpuProcessKind::Compute);
        assert_eq!(processes[1].command, "python");
        assert_eq!(processes[1].framebuffer_mib, Some(270));
    }

    #[test]
    fn pmon_percentages_and_both_kind() {
        let output = "\
# gpu         pid   type     sm    mem    enc    dec    jpg    ofa     fb   ccpm    command
# Idx           #    C/G      %      %      %      %      %      %     MB     MB    name
    1    42   C+G     12     3     4     5      -      -    100      0    chrome --type=gpu
";
        let processes = parse_pmon(output);
        assert_eq!(processes.len(), 1);
        let process = &processes[0];
        assert_eq!(process.gpu_index, 1);
        assert_eq!(process.kind, GpuProcessKind::Both);
        assert_eq!(process.sm_percent, Some(12));
        assert_eq!(process.memory_percent, Some(3));
        assert_eq!(process.encoder_percent, Some(4));
        assert_eq!(process.decoder_percent, Some(5));
        assert_eq!(process.framebuffer_mib, Some(100));
        assert_eq!(process.command, "chrome --type=gpu");
    }

    #[test]
    fn pmon_older_driver_without_jpg_ofa_columns() {
        // Pre-535 releases print neither `jpg`/`ofa` nor `ccpm`; the header
        // decides where `dec` and `fb` sit.
        let output = "\
# gpu        pid  type    sm   mem   enc   dec    fb   command
# Idx          #   C/G     %     %     %     %    MB   name
    0      12345     G     7     1     -    12   184   chrome --type=gpu-process
";
        let processes = parse_pmon(output);
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].pid, 12345);
        assert_eq!(processes[0].sm_percent, Some(7));
        assert_eq!(processes[0].decoder_percent, Some(12));
        assert_eq!(processes[0].framebuffer_mib, Some(184));
        assert_eq!(processes[0].command, "chrome --type=gpu-process");
    }

    #[test]
    fn pmon_rows_before_a_header_are_skipped() {
        assert!(parse_pmon(
            "    0      12345     G     7     1     -    12   184   chrome
"
        )
        .is_empty());
    }

    #[test]
    fn pmon_without_command_column() {
        // A release whose header names no `command` column: every column
        // is a value and the command is empty.
        let processes = parse_pmon(
            "# gpu  pid  type  sm  mem  enc  dec  jpg  ofa  fb  ccpm\n\
             # Idx    #   C/G   %    %    %    %    %    %  MB    MB\n\
                 0    7     G   -    -    -    -    -    -   3     0\n",
        );
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].command, "");
        assert_eq!(processes[0].pid, 7);
    }

    #[test]
    fn pmon_empty_and_comment_only() {
        assert!(parse_pmon("").is_empty());
        assert!(parse_pmon("# gpu pid type\n# Idx # C/G\n").is_empty());
    }

    const DMON: &str = "\
# gpu  rxpci  txpci
# Idx   MB/s   MB/s
    0    344     28
";

    #[test]
    fn dmon_fixture() {
        let rows = parse_dmon_pcie(DMON);
        assert_eq!(
            rows,
            vec![PcieThroughput {
                gpu_index: 0,
                rx_mb_s: Some(344),
                tx_mb_s: Some(28),
            }]
        );
    }

    #[test]
    fn dmon_dash_fields_are_none() {
        let rows = parse_dmon_pcie(
            "# gpu  rxpci  txpci\n# Idx   MB/s   MB/s\n    0      -     28\n    1    5      -\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].rx_mb_s, None);
        assert_eq!(rows[0].tx_mb_s, Some(28));
        assert_eq!(rows[1].gpu_index, 1);
        assert_eq!(rows[1].rx_mb_s, Some(5));
        assert_eq!(rows[1].tx_mb_s, None);
    }

    #[test]
    fn dmon_rows_before_a_header_are_skipped() {
        assert!(parse_dmon_pcie("    0    344     28\n").is_empty());
        assert!(parse_dmon_pcie("").is_empty());
        assert!(parse_dmon_pcie("# gpu rxpci txpci\n# Idx MB/s MB/s\n").is_empty());
    }

    #[test]
    fn dmon_header_decides_column_order() {
        let rows = parse_dmon_pcie("# gpu txpci rxpci\n# Idx MB/s MB/s\n 0 7 9\n");
        assert_eq!(rows[0].rx_mb_s, Some(9));
        assert_eq!(rows[0].tx_mb_s, Some(7));
    }

    #[test]
    fn lane_rate_per_generation() {
        for (generation, rate) in [
            (1, 250),
            (2, 500),
            (3, 985),
            (4, 1969),
            (5, 3938),
            (6, 7563),
        ] {
            assert_eq!(pcie_lane_mb_s(Some(generation)), Some(rate));
        }
        assert_eq!(pcie_lane_mb_s(Some(0)), None);
        assert_eq!(pcie_lane_mb_s(Some(7)), None);
        assert_eq!(pcie_lane_mb_s(None), None);
    }

    #[test]
    fn percent_of_the_trained_link() {
        // System A idle: 344 MB/s on Gen 3 x16 (15,760 MB/s) is 2.2 %.
        assert_eq!(pcie_percent(344, Some(3), Some(16)), Some(2.2));
        assert_eq!(pcie_percent(28, Some(3), Some(16)), Some(0.2));
        assert_eq!(pcie_percent(0, Some(1), Some(1)), Some(0.0));
        assert_eq!(pcie_percent(250, Some(1), Some(1)), Some(100.0));
        // The same traffic reads higher on the narrower, slower idle link.
        assert_eq!(pcie_percent(344, Some(1), Some(16)), Some(8.6));
        // 1 / 15760 = 0.0063 % rounds to 0.0; 8 / 15760 = 0.0508 rounds to 0.1.
        assert_eq!(pcie_percent(1, Some(3), Some(16)), Some(0.0));
        assert_eq!(pcie_percent(8, Some(3), Some(16)), Some(0.1));
    }

    #[test]
    fn percent_is_none_when_any_input_is() {
        assert_eq!(pcie_percent(344, None, Some(16)), None);
        assert_eq!(pcie_percent(344, Some(3), None), None);
        assert_eq!(pcie_percent(344, Some(9), Some(16)), None);
        assert_eq!(pcie_percent(344, Some(3), Some(0)), None);
    }

    #[test]
    fn throughput_is_applied_against_the_same_polls_link() {
        let mut gpus = parse_query_gpu(QUERY);
        apply_pcie_throughput(&mut gpus, &parse_dmon_pcie(DMON));
        assert_eq!(gpus[0].pcie_rx_mb_s, Some(344));
        assert_eq!(gpus[0].pcie_tx_mb_s, Some(28));
        assert_eq!(gpus[0].pcie_rx_percent, Some(2.2));
        assert_eq!(gpus[0].pcie_tx_percent, Some(0.2));

        // Unknown link: throughput stays, percent is null.
        let mut gpus = parse_query_gpu(QUERY);
        gpus[0].pcie_width = None;
        apply_pcie_throughput(&mut gpus, &parse_dmon_pcie(DMON));
        assert_eq!(gpus[0].pcie_rx_mb_s, Some(344));
        assert_eq!(gpus[0].pcie_rx_percent, None);

        // No dmon row: everything stays null.
        let mut gpus = parse_query_gpu(QUERY);
        apply_pcie_throughput(&mut gpus, &[]);
        assert_eq!(gpus[0].pcie_rx_mb_s, None);
        assert_eq!(gpus[0].pcie_tx_percent, None);
    }

    #[test]
    fn serializes_camel_case() {
        let json = serde_json::to_value(parse_pmon(PMON)).unwrap();
        assert_eq!(json[0]["gpuIndex"], 0);
        assert_eq!(json[0]["kind"], "graphics");
        assert!(json[0]["app"].is_null());
    }

    #[tokio::test]
    async fn sample_never_errors() {
        // Without nvidia-smi this is "not installed"; with it, real data.
        let telemetry = sample().await;
        if !telemetry.available {
            assert!(telemetry.reason.is_some());
            assert!(telemetry.gpus.is_empty());
        }
    }

    #[tokio::test]
    async fn the_three_commands_run_concurrently() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("nvidia-smi");
        std::fs::write(&script, "#!/bin/sh\nsleep 0.4\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let telemetry = sample_with(script.to_str().unwrap()).await;
        let elapsed = started.elapsed();
        assert!(telemetry.available, "{:?}", telemetry.reason);
        // In sequence the three sleeps take at least 1.2 s.
        assert!(
            elapsed < Duration::from_millis(800),
            "three 0.4 s commands took {elapsed:?}"
        );
    }
}
