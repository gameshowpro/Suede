//! Detecting the NVIDIA kernel module and its GSP firmware state straight
//! from `/proc/driver/nvidia` — no `nvidia-smi`, which is not guaranteed
//! installed and would mean shelling out where a plain read suffices.
//!
//! Used by `GET /system` (`nvidiaDriver`) and by the `gsp-firmware` and
//! `nvidia-driver-version` health checks in [`crate::checks`]. Any of the
//! files being absent reads as "not an NVIDIA machine": [`detect`] returns
//! `None`, never an error.

use std::cmp::Ordering;

use crate::model::{GspFirmware, NvidiaDriverStatus, NvidiaKernelModule};

const VERSION_PATH: &str = "/proc/driver/nvidia/version";
const GPUS_DIR: &str = "/proc/driver/nvidia/gpus";
const PARAMS_PATH: &str = "/proc/driver/nvidia/params";

/// The newest NVIDIA driver release Suede's direct and Wayland presentation
/// paths have been validated on. Bump this after a successful test on a
/// newer release — the `nvidia-driver-version` health check warns when the
/// running driver is older than this. 615.71.09 (open kernel module) passed
/// the System B campaign on 2026-10-01: functional pass, fallback, drain
/// tests, and matched Wayland and direct matrices.
pub const NEWEST_TESTED_NVIDIA_DRIVER: &str = "615.71.09";

/// Detect the NVIDIA driver this machine is running.
///
/// `None` when `/proc/driver/nvidia/version` does not exist (no NVIDIA
/// module loaded) or when no GPU's `information` file could be read (an
/// unreadable `/proc/driver/nvidia/gpus` — in practice this only happens
/// alongside the module not being loaded either).
pub fn detect() -> Option<NvidiaDriverStatus> {
    let version_text = std::fs::read_to_string(VERSION_PATH).ok()?;
    let information_text = first_gpu_information()?;
    let status = detect_from(&version_text, &information_text)?;

    // `EnableGpuFirmware` is what modprobe was asked for; `GPU Firmware:` in
    // the information file (folded into `status.gsp_firmware` above) is what
    // is actually running. They can legitimately differ — the driver falls
    // back when firmware is unsupported — so only the actual state is
    // reported over the API; the configured value is logged for whoever is
    // correlating a warning against `/etc/modprobe.d/`.
    if let Ok(params_text) = std::fs::read_to_string(PARAMS_PATH) {
        if let Some(configured) = parse_configured_gsp_firmware(&params_text) {
            tracing::debug!(
                configured,
                actual = ?status.gsp_firmware,
                "GSP firmware: modprobe-configured vs. actual"
            );
        }
    }

    Some(status)
}

/// The first `/proc/driver/nvidia/gpus/*/information` file that can be read,
/// in directory order. Suede's direct-mode limits already rule out more than
/// one GPU, so "first" is "the only one" in practice.
fn first_gpu_information() -> Option<String> {
    let entries = std::fs::read_dir(GPUS_DIR).ok()?;
    for entry in entries.flatten() {
        if let Ok(text) = std::fs::read_to_string(entry.path().join("information")) {
            return Some(text);
        }
    }
    None
}

/// Pure combination of the two files' text into a [`NvidiaDriverStatus`].
fn detect_from(version_text: &str, information_text: &str) -> Option<NvidiaDriverStatus> {
    let (version, kernel_module) = parse_version(version_text)?;
    let gsp_firmware = parse_gsp_firmware(information_text);
    Some(NvidiaDriverStatus {
        version,
        gsp_optional: kernel_module == NvidiaKernelModule::Proprietary,
        kernel_module,
        gsp_firmware,
        newest_tested: NEWEST_TESTED_NVIDIA_DRIVER.to_string(),
    })
}

/// Parse `/proc/driver/nvidia/version`'s `NVRM version:` line for the driver
/// version and whether the loaded module is the proprietary binary
/// (`... Kernel Module ...`) or the open-source one NVIDIA ships alongside
/// it (`... Open Kernel Module ...`).
fn parse_version(text: &str) -> Option<(String, NvidiaKernelModule)> {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("NVRM version:"))?;
    let kernel_module = if line.contains("Open Kernel Module") {
        NvidiaKernelModule::Open
    } else {
        NvidiaKernelModule::Proprietary
    };
    let version = line
        .split_whitespace()
        .find(|token| is_dotted_version(token))?;
    Some((version.to_string(), kernel_module))
}

/// Whether `token` looks like a dotted-integer version, e.g. `595.91.07`:
/// every dot-separated part is non-empty and all-digit.
fn is_dotted_version(token: &str) -> bool {
    token.contains('.')
        && token
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Parse the `GPU Firmware:` line of
/// `/proc/driver/nvidia/gpus/*/information`: `N/A` means GSP is off; any
/// other value (a firmware version) means it is on. Older drivers (550 on
/// System B) print no such line at all, which reads as
/// [`GspFirmware::Unknown`] rather than as "not an NVIDIA machine": the
/// driver is plainly there, it just does not say.
fn parse_gsp_firmware(text: &str) -> GspFirmware {
    let value = text
        .lines()
        .find(|line| line.trim_start().starts_with("GPU Firmware:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim());
    match value {
        None => GspFirmware::Unknown,
        Some(value) if value.eq_ignore_ascii_case("N/A") => GspFirmware::Off,
        Some(_) => GspFirmware::On,
    }
}

/// Parse the `EnableGpuFirmware` line of `/proc/driver/nvidia/params`: the
/// value modprobe was configured with, exactly as the driver prints it. Not
/// normalized to a bool — the driver's own encoding is not a simple 0/1, so
/// the raw text is what is worth keeping.
fn parse_configured_gsp_firmware(text: &str) -> Option<&str> {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("EnableGpuFirmware:"))?;
    Some(line.split_once(':')?.1.trim())
}

/// Compare two dotted-integer driver version strings component-wise, e.g.
/// `595.91.07` vs `595.104.02`. `None` when either has a non-numeric
/// component — the caller reports that as "cannot be compared" rather than
/// guessing at an ordering.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    Some(dotted(a)?.cmp(&dotted(b)?))
}

fn dotted(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read live from System A 2026-09-29 with GSP firmware
    /// off: `cat /proc/driver/nvidia/version`.
    const SYSTEM_A_VERSION: &str = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.91.07  Wed Jul 29 02:50:21 UTC 2026\nGCC version:  gcc version 15.2.0 (Ubuntu 15.2.0-16ubuntu1) \n";

    /// Read live from System A 2026-09-29, GSP firmware off:
    /// `cat /proc/driver/nvidia/gpus/*/information`.
    const SYSTEM_A_INFORMATION_GSP_OFF: &str = "Model: \t\t Quadro RTX 8000\nIRQ:   \t\t 135\nGPU UUID: \t GPU-97e0449e-f914-403f-3976-34c746092b9c\nVideo BIOS: \t 90.02.30.00.01\nBus Type: \t PCIe\nDMA Size: \t 47 bits\nDMA Mask: \t 0x7fffffffffff\nBus Location: \t 0000:01:00.0\nDevice Minor: \t 0\nGPU Firmware: \t N/A\nGPU Excluded:\t No\n";

    /// Read live from System A 2026-09-29, GSP firmware off:
    /// `grep EnableGpuFirmware /proc/driver/nvidia/params`.
    const SYSTEM_A_PARAMS_GSP_OFF: &str = "EnableGpuFirmware: 0\nEnableGpuFirmwareLogs: 2\n";

    /// The GSP-on variant of the information file, given as a sample for
    /// this test by the orchestrator (`GPU Firmware:` names a version
    /// instead of `N/A`).
    const SYSTEM_A_INFORMATION_GSP_ON: &str = "Model: \t\t Quadro RTX 8000\nIRQ:   \t\t 135\nGPU UUID: \t GPU-97e0449e-f914-403f-3976-34c746092b9c\nVideo BIOS: \t 90.02.30.00.01\nBus Type: \t PCIe\nDMA Size: \t 47 bits\nDMA Mask: \t 0x7fffffffffff\nBus Location: \t 0000:01:00.0\nDevice Minor: \t 0\nGPU Firmware: \t 595.91.07\nGPU Excluded:\t No\n";

    /// The GSP-on variant of the params file, given as a sample for this
    /// test by the orchestrator.
    const SYSTEM_A_PARAMS_GSP_ON: &str = "EnableGpuFirmware: 18\nEnableGpuFirmwareLogs: 2\n";

    /// Read live from System B 2026-09-30 (driver 550.163.01 from Debian
    /// trixie): `cat /proc/driver/nvidia/version`.
    const SYSTEM_B_VERSION_550: &str = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.163.01  Tue Apr  8 12:41:17 UTC 2025\nGCC version:  gcc version 14.2.0 (Debian 14.2.0-19) \n";

    /// Read live from System B 2026-09-30:
    /// `cat /proc/driver/nvidia/gpus/*/information`. Driver 550 prints no
    /// `GPU Firmware:` line at all.
    const SYSTEM_B_INFORMATION_550: &str = "Model: \t\t NVIDIA RTX A1000\nIRQ:   \t\t 164\nGPU UUID: \t GPU-f02fadb9-9b06-3b2e-5102-468bc9d18b45\nVideo BIOS: \t 94.07.96.00.01\nBus Type: \t PCIe\nDMA Size: \t 47 bits\nDMA Mask: \t 0x7fffffffffff\nBus Location: \t 0000:01:00.0\nDevice Minor: \t 0\nGPU Excluded:\t No\n";

    /// Read live from System B 2026-09-30:
    /// `grep EnableGpuFirmware /proc/driver/nvidia/params`.
    const SYSTEM_B_PARAMS_550: &str = "EnableGpuFirmware: 18\nEnableGpuFirmwareLogs: 2\n";

    /// A synthetic open-kernel-module `version` line — no such machine is
    /// on hand, so this follows NVIDIA's own documented phrasing for the
    /// open module.
    const OPEN_MODULE_VERSION: &str = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  550.120.03  Release Build  (dvs-builder@U22-I3-C05-9-3)  Thu Apr 11 22:14:56 UTC 2026\n";

    #[test]
    fn parses_system_a_proprietary_module_and_version() {
        let (version, kernel_module) = parse_version(SYSTEM_A_VERSION).unwrap();
        assert_eq!(version, "595.91.07");
        assert_eq!(kernel_module, NvidiaKernelModule::Proprietary);
    }

    #[test]
    fn parses_the_open_kernel_module() {
        let (version, kernel_module) = parse_version(OPEN_MODULE_VERSION).unwrap();
        assert_eq!(version, "550.120.03");
        assert_eq!(kernel_module, NvidiaKernelModule::Open);
    }

    #[test]
    fn a_missing_nvrm_line_is_not_nvidia() {
        assert!(parse_version("nothing to see here\n").is_none());
    }

    #[test]
    fn system_a_information_reads_gsp_off() {
        assert_eq!(
            parse_gsp_firmware(SYSTEM_A_INFORMATION_GSP_OFF),
            GspFirmware::Off
        );
    }

    #[test]
    fn system_a_information_reads_gsp_on() {
        assert_eq!(
            parse_gsp_firmware(SYSTEM_A_INFORMATION_GSP_ON),
            GspFirmware::On
        );
    }

    #[test]
    fn system_a_params_are_read_as_configured_gsp_firmware() {
        assert_eq!(
            parse_configured_gsp_firmware(SYSTEM_A_PARAMS_GSP_OFF),
            Some("0")
        );
        assert_eq!(
            parse_configured_gsp_firmware(SYSTEM_A_PARAMS_GSP_ON),
            Some("18")
        );
    }

    #[test]
    fn detect_from_combines_both_files_gsp_off() {
        let status = detect_from(SYSTEM_A_VERSION, SYSTEM_A_INFORMATION_GSP_OFF).unwrap();
        assert_eq!(status.version, "595.91.07");
        assert_eq!(status.kernel_module, NvidiaKernelModule::Proprietary);
        assert_eq!(status.gsp_firmware, GspFirmware::Off);
        assert!(status.gsp_optional);
        assert_eq!(status.newest_tested, NEWEST_TESTED_NVIDIA_DRIVER);
    }

    #[test]
    fn detect_from_combines_both_files_gsp_on() {
        let status = detect_from(SYSTEM_A_VERSION, SYSTEM_A_INFORMATION_GSP_ON).unwrap();
        assert_eq!(status.gsp_firmware, GspFirmware::On);
        assert!(status.gsp_optional);
    }

    #[test]
    fn the_open_module_is_never_gsp_optional() {
        let status = detect_from(OPEN_MODULE_VERSION, SYSTEM_A_INFORMATION_GSP_ON).unwrap();
        assert_eq!(status.kernel_module, NvidiaKernelModule::Open);
        assert!(!status.gsp_optional);
    }

    #[test]
    fn information_without_a_gpu_firmware_line_reads_unknown() {
        let status = detect_from(SYSTEM_A_VERSION, "no such line here\n").unwrap();
        assert_eq!(status.gsp_firmware, GspFirmware::Unknown);
    }

    #[test]
    fn system_b_driver_550_reads_gsp_unknown() {
        assert_eq!(
            parse_gsp_firmware(SYSTEM_B_INFORMATION_550),
            GspFirmware::Unknown
        );
        assert_eq!(
            parse_configured_gsp_firmware(SYSTEM_B_PARAMS_550),
            Some("18")
        );
        let status = detect_from(SYSTEM_B_VERSION_550, SYSTEM_B_INFORMATION_550).unwrap();
        assert_eq!(status.version, "550.163.01");
        assert_eq!(status.kernel_module, NvidiaKernelModule::Proprietary);
        assert_eq!(status.gsp_firmware, GspFirmware::Unknown);
        assert!(status.gsp_optional);
        assert_eq!(status.newest_tested, NEWEST_TESTED_NVIDIA_DRIVER);
    }

    #[test]
    fn an_unparseable_version_file_is_still_not_detected() {
        assert!(detect_from("no such line here\n", SYSTEM_B_INFORMATION_550).is_none());
    }

    #[test]
    fn newer_driver_is_greater() {
        assert_eq!(
            compare_versions("610.43.03", "595.91.07"),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn older_minor_release_is_less() {
        assert_eq!(
            compare_versions("595.91.07", "595.104.02"),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn older_major_release_is_less() {
        assert_eq!(
            compare_versions("550.163.01", "595.91.07"),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn equal_versions_are_equal() {
        assert_eq!(
            compare_versions("595.91.07", "595.91.07"),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn a_malformed_version_cannot_be_compared() {
        assert_eq!(compare_versions("not-a-version", "595.91.07"), None);
        assert_eq!(compare_versions("595.91.07", "also-not"), None);
    }
}
