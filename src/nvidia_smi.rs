//! GPU telemetry from `nvidia-smi`, for the System tab's GPU panel.
//!
//! Two invocations per sample: `--query-gpu` for per-GPU utilization, memory,
//! temperature, power, clocks and PCIe link, and `pmon -c 1 -s um` for the
//! per-process table. Measured on an appliance with a Quadro RTX 8000 they take
//! about 23 ms and 54 ms of wall time and negligible CPU, so sampling every few
//! seconds is cheap. Each call has its own short timeout so a wedged driver
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
        }
    }
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
/// `available: false` with a reason. If only the process table fails, the GPU
/// readings are still returned with the failure in `reason`.
pub async fn sample() -> GpuTelemetry {
    let query_args = [
        format!("--query-gpu={QUERY_FIELDS}"),
        "--format=csv,noheader,nounits".to_string(),
    ];
    let gpu_text = match run(&query_args).await {
        Ok(text) => text,
        Err(reason) => return GpuTelemetry::unavailable(reason),
    };
    let gpus = parse_query_gpu(&gpu_text);

    let pmon_args = ["pmon", "-c", "1", "-s", "um"].map(String::from);
    let (processes, reason) = match run(&pmon_args).await {
        Ok(text) => (parse_pmon(&text), None),
        Err(reason) => (Vec::new(), Some(reason)),
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
    }
}

/// Run `nvidia-smi` with a timeout and return its stdout, or a reason it
/// could not be used.
async fn run(args: &[String]) -> Result<String, String> {
    let mut command = tokio::process::Command::new("nvidia-smi");
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
                fan_percent: number(fields[16]),
            })
        })
        .collect()
}

/// Parse the output of `nvidia-smi pmon -c 1 -s um`.
///
/// Columns: gpu, pid, type, sm, mem, enc, dec, jpg, ofa, fb, ccpm, then an
/// optional command (which may contain spaces). Comment lines start with `#`.
/// `app` and `role` are left `None`.
pub fn parse_pmon(output: &str) -> Vec<GpuProcessSample> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 11 {
                return None;
            }
            let kind = match fields[2] {
                "G" => GpuProcessKind::Graphics,
                "C" => GpuProcessKind::Compute,
                "C+G" | "G+C" => GpuProcessKind::Both,
                _ => return None,
            };
            Some(GpuProcessSample {
                gpu_index: fields[0].parse().ok()?,
                pid: fields[1].parse().ok()?,
                kind,
                command: fields[11..].join(" "),
                sm_percent: number(fields[3]),
                memory_percent: number(fields[4]),
                encoder_percent: number(fields[5]),
                decoder_percent: number(fields[6]),
                framebuffer_mib: number(fields[9]),
                app: None,
                role: None,
            })
        })
        .collect()
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
        let output = "    1    42   C+G     12     3     4     5      -      -    100      0    chrome --type=gpu\n";
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
    fn pmon_without_command_column() {
        let processes = parse_pmon(
            "    0    7     G      -      -      -      -      -      -      3      0\n",
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
}
