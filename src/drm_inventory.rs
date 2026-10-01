//! The physical outputs, read straight from the kernel, for direct
//! presentation.
//!
//! In direct mode the compositor is a headless-only Sway: it has never seen
//! the displays, so it cannot report them. The daemon builds the same
//! picture itself, with no prior Wayland boot and nothing recorded:
//!
//! - **identity** (make, model, serial) from each connector's EDID in sysfs
//!   (`/sys/class/drm/cardN-<name>/edid`, world-readable), parsed with the
//!   exact rules wlroots applies, so an output is named and matched the same
//!   way on both paths;
//! - **connection state and `connector_id`** from the same sysfs directory;
//! - **modes with exact timings** from `DRM_IOCTL_MODE_GETCONNECTOR` on the
//!   card, a read-only query that needs no DRM master, with the refresh rate
//!   computed from clock/htotal/vtotal exactly as wlroots does — so a mode is
//!   59.939 Hz here precisely when Sway calls it 59.939 Hz, which is what
//!   saved configurations and the slicer's refresh matcher rely on.
//!
//! Sysfs `modes` lacks refresh rates, and Vulkan display enumeration costs
//! seconds of driver time, so neither is used.
//!
//! Like [`crate::ports`], this is read-only and never acts on a display.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::Output;
use crate::sway::raw::{RawMode, RawOutput, RawRect};

/// Where the kernel exposes DRM connectors.
pub const DRM_CLASS: &str = "/sys/class/drm";
/// Where the card nodes live.
pub const DEV_DRI: &str = "/dev/dri";

/// `DRM_MODE_FLAG_INTERLACE`.
const FLAG_INTERLACE: u32 = 1 << 4;
/// `DRM_MODE_FLAG_DBLSCAN`.
const FLAG_DBLSCAN: u32 = 1 << 5;
/// `DRM_MODE_TYPE_PREFERRED`.
const TYPE_PREFERRED: u32 = 1 << 3;

/// Places a PNP manufacturer table may be installed. wlroots compiles
/// hwdata's `pnp.ids` into itself at build time; reading the installed copy
/// gives the same names on any machine that carries hwdata.
const PNP_IDS_PATHS: [&str; 2] = ["/usr/share/hwdata/pnp.ids", "/usr/share/misc/pnp.ids"];
/// systemd's copy of the same registry, present wherever udev is. Used only
/// when hwdata is not installed: it agrees with `pnp.ids` on all but a
/// handful of the ~2,550 entries (checked on System A, 2026-09-29), which is
/// much closer than the bare three-letter code would be.
const HWDB_ACPI_VENDOR_PATHS: [&str; 2] = [
    "/usr/lib/udev/hwdb.d/20-acpi-vendor.hwdb",
    "/lib/udev/hwdb.d/20-acpi-vendor.hwdb",
];

/// What an EDID says about the display, formatted as Sway reports it.
///
/// wlroots (0.19, `backend/drm/util.c: parse_edid`) sets all three to
/// nothing when libdisplay-info rejects the EDID, and otherwise always sets
/// `make` and `model`; `serial` may still be absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EdidIdentity {
    pub make: String,
    pub model: String,
    pub serial: Option<String>,
}

/// A PNP manufacturer-id to company-name table.
#[derive(Debug, Clone, Default)]
pub struct PnpNames {
    names: HashMap<String, String>,
    /// Which file the table came from, for diagnostics.
    pub source: Option<String>,
}

impl PnpNames {
    /// The installed table: hwdata's `pnp.ids` if present, else systemd's
    /// ACPI vendor hwdb, else empty (every make is then the three-letter
    /// code, which is also what wlroots falls back to for an unknown id).
    pub fn load_system() -> Self {
        for path in PNP_IDS_PATHS {
            if let Ok(text) = std::fs::read_to_string(path) {
                let mut table = Self::parse_pnp_ids(&text);
                table.source = Some(path.to_string());
                return table;
            }
        }
        for path in HWDB_ACPI_VENDOR_PATHS {
            if let Ok(text) = std::fs::read_to_string(path) {
                let mut table = Self::parse_hwdb(&text);
                table.source = Some(path.to_string());
                return table;
            }
        }
        Self::default()
    }

    /// hwdata's format: `<ID>\t<name>`. Mirrors wlroots' `gen_pnpids.sh`
    /// (`read -r id vendor`): the name is the rest of the line with
    /// surrounding whitespace trimmed, and ids that are not three characters
    /// are not part of the table.
    pub fn parse_pnp_ids(text: &str) -> Self {
        let mut names = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            let Some((id, name)) = line.split_once(|c: char| c.is_whitespace()) else {
                continue;
            };
            if id.chars().count() != 3 {
                continue;
            }
            // wlroots keys its table by the low five bits of each letter,
            // which makes it case-insensitive; EDID codes are upper case.
            names
                .entry(id.to_ascii_uppercase())
                .or_insert_with(|| name.trim().to_string());
        }
        Self {
            names,
            source: None,
        }
    }

    /// systemd hwdb format: `acpi:ABC*:` followed by an indented
    /// `ID_VENDOR_FROM_DATABASE=<name>` line. Four-letter ACPI ids are not
    /// PNP ids and are skipped.
    pub fn parse_hwdb(text: &str) -> Self {
        let mut names = HashMap::new();
        let mut current: Option<String> = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("acpi:") {
                current = rest
                    .strip_suffix("*:")
                    .filter(|id| id.chars().count() == 3)
                    .map(str::to_ascii_uppercase);
                continue;
            }
            if let (Some(id), Some(name)) = (
                current.as_ref(),
                line.trim_start().strip_prefix("ID_VENDOR_FROM_DATABASE="),
            ) {
                names.entry(id.clone()).or_insert_with(|| name.to_string());
                current = None;
            }
        }
        Self {
            names,
            source: None,
        }
    }

    pub fn get(&self, id: &str) -> Option<&str> {
        self.names.get(id).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

const EDID_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
const EDID_BLOCK: usize = 128;
const TAG_PRODUCT_SERIAL: u8 = 0xFF;
const TAG_PRODUCT_NAME: u8 = 0xFC;

/// Parse the identity out of an EDID exactly as Sway reports it.
///
/// The rules are wlroots 0.19's `parse_edid` over libdisplay-info 0.3:
///
/// - the EDID is rejected outright (no identity at all) when it is shorter
///   than one block, lacks the fixed header, is not version 1, or block 0's
///   checksum is wrong;
/// - `make` is the PNP id's company name, or the bare three-letter id;
/// - `model` is the first non-empty product-name descriptor (0xFC), or the
///   product code as `0x%04X`;
/// - `serial` is the first non-empty serial descriptor (0xFF), or the
///   numeric serial as `0x%08X` when it is nonzero, or nothing;
/// - a descriptor string is its 13 bytes up to the first newline (and, as a
///   C string, the first NUL), with no other trimming, and any byte outside
///   printable ASCII escaped as `\xNN`.
pub fn parse_edid(data: &[u8], pnp: &PnpNames) -> Option<EdidIdentity> {
    if data.len() < EDID_BLOCK || data[..8] != EDID_HEADER || data[0x12] != 1 {
        return None;
    }
    if data[..EDID_BLOCK]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        != 0
    {
        return None;
    }

    let packed = u16::from_be_bytes([data[0x08], data[0x09]]);
    let letter = |shift: u16| char::from(((packed >> shift) & 0x1F) as u8 + b'@');
    let pnp_id: String = [letter(10), letter(5), letter(0)].iter().collect();
    let make = pnp
        .get(&pnp_id)
        .map(str::to_string)
        .unwrap_or_else(|| pnp_id.clone());

    let product = u16::from_le_bytes([data[0x0A], data[0x0B]]);
    let serial_number = u32::from_le_bytes([data[0x0C], data[0x0D], data[0x0E], data[0x0F]]);

    let descriptor_string = |wanted: u8| {
        (0..4)
            .map(|index| &data[0x36 + index * 18..0x36 + (index + 1) * 18])
            // A display descriptor starts with a zero pixel clock; anything
            // else is a detailed timing.
            .filter(|block| block[0] == 0 && block[1] == 0 && block[3] == wanted)
            .map(|block| descriptor_text(&block[5..18]))
            .find(|text| !text.is_empty())
    };

    let model = descriptor_string(TAG_PRODUCT_NAME).unwrap_or_else(|| format!("0x{product:04X}"));
    let serial = descriptor_string(TAG_PRODUCT_SERIAL)
        .or_else(|| (serial_number != 0).then(|| format!("0x{serial_number:08X}")));

    Some(EdidIdentity {
        make,
        model,
        serial,
    })
}

fn descriptor_text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n' || *byte == 0)
        .unwrap_or(bytes.len());
    let mut text = String::new();
    for byte in &bytes[..end] {
        if *byte < 0x20 || *byte >= 0x7F {
            text.push_str(&format!("\\x{byte:02x}"));
        } else {
            text.push(char::from(*byte));
        }
    }
    text
}

/// A mode's refresh rate in millihertz, computed exactly as wlroots'
/// `calculate_refresh_rate` does, integer rounding included.
pub fn refresh_millihertz(clock_khz: u32, htotal: u16, vtotal: u16, flags: u32, vscan: u16) -> i32 {
    if htotal == 0 || vtotal == 0 {
        return 0;
    }
    let vtotal = i64::from(vtotal);
    let mut refresh =
        ((i64::from(clock_khz) * 1_000_000 / i64::from(htotal) + vtotal / 2) / vtotal) as i32;
    if flags & FLAG_INTERLACE != 0 {
        refresh *= 2;
    }
    if flags & FLAG_DBLSCAN != 0 {
        refresh /= 2;
    }
    if vscan > 1 {
        refresh /= i32::from(vscan);
    }
    refresh
}

/// The timing fields of one kernel mode that the inventory needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelMode {
    pub clock_khz: u32,
    pub hdisplay: u16,
    pub htotal: u16,
    pub vdisplay: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub flags: u32,
    pub mode_type: u32,
}

/// A mode as the inventory reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryMode {
    pub width: i32,
    pub height: i32,
    pub refresh_millihz: i32,
    pub preferred: bool,
}

/// Sway's view of a connector's modes: interlaced modes are skipped (as
/// wlroots' `connect_drm_connector` does), everything else kept in kernel
/// order with the wlroots refresh rate.
pub fn modes_from_kernel(modes: &[KernelMode]) -> Vec<InventoryMode> {
    modes
        .iter()
        .filter(|mode| mode.flags & FLAG_INTERLACE == 0)
        .map(|mode| InventoryMode {
            width: i32::from(mode.hdisplay),
            height: i32::from(mode.vdisplay),
            refresh_millihz: refresh_millihertz(
                mode.clock_khz,
                mode.htotal,
                mode.vtotal,
                mode.flags,
                mode.vscan,
            ),
            preferred: mode.mode_type & TYPE_PREFERRED != 0,
        })
        .collect()
}

/// One DRM connector.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryOutput {
    /// Connector name as Sway reports it, e.g. `DP-1`.
    pub name: String,
    /// The card node the connector belongs to, e.g. `/dev/dri/card1`.
    pub card: PathBuf,
    /// Sysfs `status` is `connected`.
    pub connected: bool,
    /// Sysfs `connector_id`, where the kernel exposes it.
    pub connector_id: Option<u32>,
    /// Length of the sysfs EDID blob; zero when there is none.
    pub edid_bytes: usize,
    /// Parsed EDID identity, when the EDID is present and valid.
    pub identity: Option<EdidIdentity>,
    /// Modes from `DRM_IOCTL_MODE_GETCONNECTOR`; empty for a disconnected
    /// connector or when the query failed (see `modes_error`).
    pub modes: Vec<InventoryMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes_error: Option<String>,
}

impl InventoryOutput {
    /// The mode Sway enables an output with when nothing else is asked for.
    pub fn preferred_mode(&self) -> Option<InventoryMode> {
        self.modes
            .iter()
            .find(|mode| mode.preferred)
            .or_else(|| self.modes.first())
            .copied()
    }

    /// The advertised mode's exact refresh rate for a size and an
    /// approximate rate in hertz, as Sway's `get_outputs` rounds it.
    pub fn exact_refresh_millihz(&self, width: i32, height: i32, refresh_hz: f64) -> Option<i32> {
        self.modes
            .iter()
            .filter(|mode| mode.width == width && mode.height == height)
            .map(|mode| mode.refresh_millihz)
            .find(|millihz| (f64::from(*millihz) - refresh_hz * 1000.0).abs() <= 0.5)
    }

    /// This connector as `get_outputs` would describe it on a DRM session
    /// where it is enabled at its preferred mode at `x`, 0. Built through
    /// Sway's own wire shape so modes are deduplicated and ordered exactly as
    /// on the Wayland path.
    pub fn to_output(&self, x: i32) -> Output {
        let preferred = self.preferred_mode();
        let raw_mode = |mode: &InventoryMode| RawMode {
            width: mode.width,
            height: mode.height,
            refresh: mode.refresh_millihz,
        };
        let raw = RawOutput {
            name: self.name.clone(),
            active: preferred.is_some(),
            make: self.identity.as_ref().map(|id| id.make.clone()),
            model: self.identity.as_ref().map(|id| id.model.clone()),
            serial: self.identity.as_ref().and_then(|id| id.serial.clone()),
            current_mode: preferred.as_ref().map(raw_mode),
            modes: self.modes.iter().map(raw_mode).collect(),
            rect: preferred.map(|mode| RawRect {
                x,
                y: 0,
                width: mode.width,
                height: mode.height,
            }),
            scale: Some(1.0),
            transform: Some("normal".to_string()),
            adaptive_sync_status: Some("disabled".to_string()),
        };
        Output::from(raw)
    }
}

/// Every DRM connector the kernel exposes, connected or not.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DrmInventory {
    pub outputs: Vec<InventoryOutput>,
    /// Where make names came from; `None` means three-letter codes only.
    pub pnp_source: Option<String>,
}

impl DrmInventory {
    /// Read the live inventory from the running kernel.
    pub fn read() -> Self {
        let pnp = PnpNames::load_system();
        let mut inventory = read_in(Path::new(DRM_CLASS), Path::new(DEV_DRI), &pnp, query_modes);
        inventory.pnp_source = pnp.source.clone();
        inventory
    }

    /// Connected connectors, in name order.
    pub fn connected(&self) -> impl Iterator<Item = &InventoryOutput> {
        self.outputs.iter().filter(|output| output.connected)
    }

    pub fn get(&self, name: &str) -> Option<&InventoryOutput> {
        self.outputs.iter().find(|output| output.name == name)
    }

    /// Check that direct presentation can own these outputs, returning the
    /// one card they all hang off.
    ///
    /// Refused when nothing is connected, when connected outputs span more
    /// than one card (direct presentation drives exactly one), or when any
    /// connected output lacks a `connector_id`, an EDID, or any modes — each
    /// of those is information direct mode has no other way to learn.
    pub fn preflight(&self) -> Result<PathBuf, String> {
        let connected: Vec<&InventoryOutput> = self.connected().collect();
        let Some(first) = connected.first() else {
            return Err("no DRM connector reports a connected display".to_string());
        };
        if let Some(other) = connected.iter().find(|output| output.card != first.card) {
            return Err(format!(
                "connected displays span more than one card ({} on {}, {} on {}); \
                 direct presentation drives exactly one",
                first.name,
                first.card.display(),
                other.name,
                other.card.display()
            ));
        }
        for output in &connected {
            if output.connector_id.is_none() {
                return Err(format!(
                    "{} has no connector_id in sysfs; direct presentation cannot address it",
                    output.name
                ));
            }
            if output.edid_bytes == 0 {
                return Err(format!(
                    "{} has no EDID; direct presentation cannot identify it",
                    output.name
                ));
            }
            if output.modes.is_empty() {
                return Err(format!(
                    "{} reports no modes{}",
                    output.name,
                    output
                        .modes_error
                        .as_ref()
                        .map(|error| format!(" ({error})"))
                        .unwrap_or_default()
                ));
            }
        }
        Ok(first.card.clone())
    }

    /// The connected outputs as `get_outputs` would report them on a DRM
    /// session that enabled them all at their preferred modes, left to
    /// right in name order — the state a direct-mode session starts from.
    pub fn simulated_outputs(&self) -> Vec<Output> {
        let mut x = 0;
        self.connected()
            .map(|output| {
                let simulated = output.to_output(x);
                x += simulated.rect.width;
                simulated
            })
            .collect()
    }
}

/// Build the inventory from a sysfs class directory, with the mode query
/// injected so the scan is testable without a GPU.
pub fn read_in<F>(sysfs: &Path, dev: &Path, pnp: &PnpNames, mut query: F) -> DrmInventory
where
    F: FnMut(&Path, u32) -> std::io::Result<Vec<KernelMode>>,
{
    let Ok(entries) = std::fs::read_dir(sysfs) else {
        return DrmInventory::default();
    };
    let mut outputs: Vec<InventoryOutput> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            let name = crate::ports::connector_name(file_name)?;
            let card_name = file_name.split_once('-')?.0;
            let directory = entry.path();
            let status = std::fs::read_to_string(directory.join("status")).ok()?;
            let connected = status.trim() == "connected";
            let connector_id = std::fs::read_to_string(directory.join("connector_id"))
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .filter(|id| *id != 0);
            let edid = std::fs::read(directory.join("edid")).unwrap_or_default();
            let card = dev.join(card_name);
            let (modes, modes_error) = match (connected, connector_id) {
                (true, Some(id)) => match query(&card, id) {
                    Ok(modes) => (modes_from_kernel(&modes), None),
                    Err(error) => (Vec::new(), Some(error.to_string())),
                },
                _ => (Vec::new(), None),
            };
            Some(InventoryOutput {
                name,
                card,
                connected,
                connector_id,
                edid_bytes: edid.len(),
                identity: parse_edid(&edid, pnp),
                modes,
                modes_error,
            })
        })
        .collect();
    outputs.sort_by(|a, b| a.card.cmp(&b.card).then_with(|| a.name.cmp(&b.name)));
    DrmInventory {
        outputs,
        pnp_source: pnp.source.clone(),
    }
}

/// `DRM_IOCTL_MODE_GETCONNECTOR` on `card`, read-only.
///
/// Never asks the kernel to probe: a zero `count_modes` would request a
/// forced probe (honored only for the DRM master, and slow), so the first
/// call already offers room for 64 modes and only grows when the kernel
/// reports more — the approach libdrm's `drmModeGetConnectorCurrent` takes.
///
/// Opening a primary node when nobody holds DRM master makes the opener
/// master, so the handle drops it straight away: this query must never
/// stand between the slicer (or a compositor) and the card.
#[cfg(target_os = "linux")]
pub fn query_modes(card: &Path, connector_id: u32) -> std::io::Result<Vec<KernelMode>> {
    use std::os::fd::AsRawFd;

    let file = std::fs::OpenOptions::new().read(true).open(card)?;
    let fd = file.as_raw_fd();
    // Harmless when not master (EINVAL), which is the usual case.
    // SAFETY: an argument-less ioctl on a descriptor this function owns.
    unsafe {
        libc::ioctl(fd, DRM_IOCTL_DROP_MASTER as _);
    }

    let mut capacity = 64usize;
    for _ in 0..8 {
        let mut modes = vec![DrmModeInfo::default(); capacity];
        let mut request = DrmModeGetConnector {
            modes_ptr: modes.as_mut_ptr() as u64,
            count_modes: capacity as u32,
            connector_id,
            ..Default::default()
        };
        // SAFETY: `request` is the kernel's `struct drm_mode_get_connector`
        // (size asserted below), and `modes_ptr` points at `capacity`
        // writable `struct drm_mode_modeinfo` entries that outlive the call.
        // Encoder and property pointers are null with zero counts, so the
        // kernel writes nothing through them.
        let result = unsafe {
            libc::ioctl(
                fd,
                DRM_IOCTL_MODE_GETCONNECTOR as _,
                &mut request as *mut DrmModeGetConnector,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::EINTR) | Some(libc::EAGAIN)) {
                continue;
            }
            return Err(error);
        }
        let count = request.count_modes as usize;
        if count > capacity {
            // More modes than room: the kernel copied nothing. Retry bigger.
            capacity = count;
            continue;
        }
        modes.truncate(count);
        return Ok(modes
            .iter()
            .map(|mode| KernelMode {
                clock_khz: mode.clock,
                hdisplay: mode.hdisplay,
                htotal: mode.htotal,
                vdisplay: mode.vdisplay,
                vtotal: mode.vtotal,
                vscan: mode.vscan,
                flags: mode.flags,
                mode_type: mode.mode_type,
            })
            .collect());
    }
    Err(std::io::Error::other(
        "the connector's mode list kept changing while it was read",
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn query_modes(_card: &Path, _connector_id: u32) -> std::io::Result<Vec<KernelMode>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "DRM connector queries need Linux",
    ))
}

/// `struct drm_mode_get_connector` from `drm_mode.h`.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // Kernel ABI layout; only some fields are read.
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

/// `struct drm_mode_modeinfo` from `drm_mode.h`.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)] // Kernel ABI layout; only some fields are read.
struct DrmModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    mode_type: u32,
    name: [u8; 32],
}

#[cfg(target_os = "linux")]
const _: () = assert!(std::mem::size_of::<DrmModeGetConnector>() == 80);
#[cfg(target_os = "linux")]
const _: () = assert!(std::mem::size_of::<DrmModeInfo>() == 68);

/// `DRM_IOWR(0xA7, struct drm_mode_get_connector)`.
#[cfg(target_os = "linux")]
const DRM_IOCTL_MODE_GETCONNECTOR: u64 = (3 << 30) | (80 << 16) | (0x64 << 8) | 0xA7;
/// `DRM_IO(0x1f)`.
#[cfg(target_os = "linux")]
const DRM_IOCTL_DROP_MASTER: u64 = (0x64 << 8) | 0x1F;

/// Hand-built inventory entries for tests elsewhere in the crate.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A connected display offering 1080p at 60 (preferred) and 59.939 Hz,
    /// named after its connector so matching rules can tell them apart.
    pub(crate) fn physical(name: &str, card: &str, connector_id: Option<u32>) -> InventoryOutput {
        InventoryOutput {
            name: name.to_string(),
            card: PathBuf::from(card),
            connected: true,
            connector_id,
            edid_bytes: 256,
            identity: Some(EdidIdentity {
                make: "Acme".into(),
                model: name.into(),
                serial: None,
            }),
            modes: vec![
                InventoryMode {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: 60_000,
                    preferred: true,
                },
                InventoryMode {
                    width: 1920,
                    height: 1080,
                    refresh_millihz: 59_939,
                    preferred: false,
                },
            ],
            modes_error: None,
        }
    }

    /// One card, one connected display per name, connector ids as on System A.
    pub(crate) fn wall(names: &[&str]) -> DrmInventory {
        DrmInventory {
            outputs: names
                .iter()
                .enumerate()
                .map(|(index, name)| physical(name, "/dev/dri/card1", Some(129 + 4 * index as u32)))
                .collect(),
            pnp_source: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRAIN_EDIDS: [(&str, &[u8]); 4] = [
        ("DP-1", include_bytes!("fixtures/drm/system-a-DP-1.edid")),
        ("DP-2", include_bytes!("fixtures/drm/system-a-DP-2.edid")),
        ("DP-3", include_bytes!("fixtures/drm/system-a-DP-3.edid")),
        ("DP-4", include_bytes!("fixtures/drm/system-a-DP-4.edid")),
    ];
    /// System A's `get_outputs` under the DRM Sway (sway 1.11, wlroots 0.19.2,
    /// libdisplay-info 0.3.0), reduced to identity and modes.
    const SYSTEM_A_SWAY: &str = include_str!("fixtures/drm/system-a-sway-outputs.json");
    /// `DRM_IOCTL_MODE_GETCONNECTOR` on System A's card1 at the same moment:
    /// per connector, `[clock, hdisplay, htotal, vdisplay, vtotal, vscan,
    /// flags, type, vrefresh]`.
    const SYSTEM_A_GETCONNECTOR: &str = include_str!("fixtures/drm/system-a-getconnector.json");

    /// The four System A manufacturers, as hwdata spells them.
    fn brain_pnp() -> PnpNames {
        PnpNames::parse_pnp_ids(
            "ACI\tAncor Communications Inc\n\
             GSM\tLG Electronics\n\
             SAM\tSamsung Electric Company\n\
             SET\tSendTek Corporation\n",
        )
    }

    #[derive(serde::Deserialize)]
    struct SwayIdentity {
        name: String,
        make: String,
        model: String,
        serial: String,
        modes: Vec<[i32; 3]>,
    }

    fn brain_sway() -> Vec<SwayIdentity> {
        serde_json::from_str(SYSTEM_A_SWAY).unwrap()
    }

    fn brain_kernel_modes() -> HashMap<String, Vec<KernelMode>> {
        #[derive(serde::Deserialize)]
        struct Connector {
            modes: Vec<[u32; 9]>,
        }
        let raw: HashMap<String, Connector> = serde_json::from_str(SYSTEM_A_GETCONNECTOR).unwrap();
        raw.into_iter()
            .map(|(name, connector)| {
                let modes = connector
                    .modes
                    .iter()
                    .map(|m| KernelMode {
                        clock_khz: m[0],
                        hdisplay: m[1] as u16,
                        htotal: m[2] as u16,
                        vdisplay: m[3] as u16,
                        vtotal: m[4] as u16,
                        vscan: m[5] as u16,
                        flags: m[6],
                        mode_type: m[7],
                    })
                    .collect();
                (name, modes)
            })
            .collect()
    }

    #[test]
    fn brain_edids_parse_to_exactly_what_sway_reports() {
        let pnp = brain_pnp();
        let sway = brain_sway();
        for (name, edid) in BRAIN_EDIDS {
            let identity = parse_edid(edid, &pnp).expect("a valid EDID");
            let reported = sway.iter().find(|o| o.name == name).unwrap();
            assert_eq!(identity.make, reported.make, "{name} make");
            assert_eq!(identity.model, reported.model, "{name} model");
            assert_eq!(
                identity.serial.as_deref(),
                Some(reported.serial.as_str()),
                "{name} serial"
            );
        }
    }

    #[test]
    fn edid_fallbacks_cover_every_serial_rule() {
        let pnp = brain_pnp();
        let parse = |index: usize| parse_edid(BRAIN_EDIDS[index].1, &pnp).unwrap();
        // DP-1: a serial descriptor.
        assert_eq!(parse(0).serial.as_deref(), Some("B8LMIB558821"));
        // DP-2: no serial descriptor, so the numeric serial.
        assert_eq!(parse(1).serial.as_deref(), Some("0x00000001"));
        // DP-2's name fills all 13 bytes with no newline.
        assert_eq!(parse(1).model, "Field Monitor");
        // DP-4: an *empty* serial descriptor is skipped, not reported as "".
        assert_eq!(parse(3).serial.as_deref(), Some("0x01010101"));
    }

    #[test]
    fn an_unknown_manufacturer_is_its_three_letter_code() {
        let identity = parse_edid(BRAIN_EDIDS[0].1, &PnpNames::default()).unwrap();
        assert_eq!(identity.make, "ACI");
    }

    #[test]
    fn a_missing_product_name_falls_back_to_the_product_code() {
        let mut edid = BRAIN_EDIDS[0].1.to_vec();
        // Retag DP-1's 0xFC descriptor as a data string, fix the checksum.
        let position = (0..4)
            .map(|i| 0x36 + i * 18)
            .find(|&at| edid[at] == 0 && edid[at + 3] == TAG_PRODUCT_NAME)
            .unwrap();
        edid[position + 3] = 0xFE;
        fix_checksum(&mut edid);
        let identity = parse_edid(&edid, &brain_pnp()).unwrap();
        assert_eq!(identity.model, format!("0x{:04X}", 9208));
    }

    #[test]
    fn a_zero_numeric_serial_with_no_descriptor_is_absent() {
        let mut edid = BRAIN_EDIDS[1].1.to_vec();
        edid[0x0C..0x10].fill(0);
        fix_checksum(&mut edid);
        assert_eq!(parse_edid(&edid, &brain_pnp()).unwrap().serial, None);
    }

    #[test]
    fn unprintable_bytes_are_escaped_like_libdisplay_info() {
        assert_eq!(descriptor_text(b"AB\x01\xffC\n    "), "AB\\x01\\xffC");
        assert_eq!(descriptor_text(b"ABC\0DEF      "), "ABC");
        // Trailing spaces before the newline are part of the string.
        assert_eq!(descriptor_text(b"AB  \n        "), "AB  ");
    }

    #[test]
    fn invalid_edids_have_no_identity() {
        let pnp = brain_pnp();
        assert!(parse_edid(&[], &pnp).is_none());
        assert!(parse_edid(&BRAIN_EDIDS[0].1[..100], &pnp).is_none());
        let mut bad_checksum = BRAIN_EDIDS[0].1.to_vec();
        bad_checksum[0x7F] ^= 1;
        assert!(parse_edid(&bad_checksum, &pnp).is_none());
        let mut bad_header = BRAIN_EDIDS[0].1.to_vec();
        bad_header[0] = 1;
        assert!(parse_edid(&bad_header, &pnp).is_none());
        let mut version_two = BRAIN_EDIDS[0].1.to_vec();
        version_two[0x12] = 2;
        fix_checksum(&mut version_two);
        assert!(parse_edid(&version_two, &pnp).is_none());
    }

    fn fix_checksum(edid: &mut [u8]) {
        let sum = edid[..127]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        edid[127] = 0u8.wrapping_sub(sum);
    }

    #[test]
    fn pnp_tables_parse_both_formats() {
        let pnp = PnpNames::parse_pnp_ids(
            "ACI\tAncor Communications Inc  \nABCD\tNot a PNP id\ninu\tInovatec S.p.A.\n\n",
        );
        assert_eq!(pnp.get("ACI"), Some("Ancor Communications Inc"));
        assert_eq!(pnp.get("ABCD"), None);
        assert_eq!(pnp.get("INU"), Some("Inovatec S.p.A."));

        let hwdb = PnpNames::parse_hwdb(
            "# comment\nacpi:ACI*:\n ID_VENDOR_FROM_DATABASE=Ancor Communications Inc\n\n\
             acpi:AAVA*:\n ID_VENDOR_FROM_DATABASE=Four Letter ACPI\n",
        );
        assert_eq!(hwdb.get("ACI"), Some("Ancor Communications Inc"));
        assert_eq!(hwdb.len(), 1);
    }

    #[test]
    fn refresh_matches_wlroots_rounding() {
        // CEA 1080p60: 148.5 MHz over 2200x1125.
        assert_eq!(refresh_millihertz(148_500, 2200, 1125, 0, 0), 60_000);
        // The NTSC-rate twin, which Sway reports as 59.939 on System A.
        assert_eq!(refresh_millihertz(148_350, 2200, 1125, 0, 0), 59_939);
        // Interlace doubles, double-scan halves, vscan divides.
        assert_eq!(
            refresh_millihertz(74_250, 2200, 1125, FLAG_INTERLACE, 0),
            60_000
        );
        assert_eq!(
            refresh_millihertz(148_500, 2200, 1125, FLAG_DBLSCAN, 0),
            30_000
        );
        assert_eq!(refresh_millihertz(148_500, 2200, 1125, 0, 2), 30_000);
        assert_eq!(refresh_millihertz(148_500, 0, 1125, 0, 0), 0);
    }

    #[test]
    fn brain_kernel_modes_become_exactly_sways_mode_list() {
        let kernel = brain_kernel_modes();
        for reported in brain_sway() {
            let modes = modes_from_kernel(&kernel[&reported.name]);
            let ours: Vec<[i32; 3]> = modes
                .iter()
                .map(|m| [m.width, m.height, m.refresh_millihz])
                .collect();
            assert_eq!(ours, reported.modes, "{}", reported.name);
            assert!(
                ours.contains(&[1920, 1080, 59_939]),
                "{} must offer Sway's 59.939 Hz 1080p",
                reported.name
            );
        }
    }

    #[test]
    fn interlaced_modes_are_skipped_and_the_preferred_one_is_marked() {
        let modes = modes_from_kernel(&[
            KernelMode {
                clock_khz: 74_250,
                hdisplay: 1920,
                htotal: 2200,
                vdisplay: 1080,
                vtotal: 1125,
                vscan: 0,
                flags: FLAG_INTERLACE,
                mode_type: 0,
            },
            KernelMode {
                clock_khz: 148_500,
                hdisplay: 1920,
                htotal: 2200,
                vdisplay: 1080,
                vtotal: 1125,
                vscan: 0,
                flags: 5,
                mode_type: TYPE_PREFERRED,
            },
        ]);
        assert_eq!(
            modes,
            vec![InventoryMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                preferred: true
            }]
        );
    }

    /// A fake sysfs tree shaped like System A's, and a query answering from the
    /// recorded GETCONNECTOR data.
    fn brain_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (index, (name, edid)) in BRAIN_EDIDS.iter().enumerate() {
            let path = dir.path().join(format!("card1-{name}"));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("status"), "connected\n").unwrap();
            std::fs::write(path.join("connector_id"), format!("{}\n", 129 + index * 4)).unwrap();
            std::fs::write(path.join("edid"), edid).unwrap();
        }
        let unknown = dir.path().join("card1-Unknown-2");
        std::fs::create_dir(&unknown).unwrap();
        std::fs::write(unknown.join("status"), "disconnected\n").unwrap();
        std::fs::create_dir(dir.path().join("card1")).unwrap();
        dir
    }

    fn brain_query() -> impl FnMut(&Path, u32) -> std::io::Result<Vec<KernelMode>> {
        let kernel = brain_kernel_modes();
        move |card: &Path, id: u32| {
            assert_eq!(card, Path::new("/dev/dri/card1"));
            let name = format!("DP-{}", (id - 129) / 4 + 1);
            Ok(kernel[&name].clone())
        }
    }

    #[test]
    fn a_brain_shaped_tree_reads_like_sways_outputs() {
        let dir = brain_tree();
        let inventory = read_in(
            dir.path(),
            Path::new("/dev/dri"),
            &brain_pnp(),
            brain_query(),
        );
        assert_eq!(
            inventory
                .outputs
                .iter()
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>(),
            ["DP-1", "DP-2", "DP-3", "DP-4"],
            "Unknown-* is a writeback connector, never an output"
        );
        assert_eq!(inventory.preflight(), Ok(PathBuf::from("/dev/dri/card1")));
        assert_eq!(inventory.get("DP-3").unwrap().connector_id, Some(137));

        let simulated = inventory.simulated_outputs();
        let sway = brain_sway();
        for output in &simulated {
            let reported = sway.iter().find(|o| o.name == output.name).unwrap();
            let raw: Vec<RawMode> = reported
                .modes
                .iter()
                .map(|m| RawMode {
                    width: m[0],
                    height: m[1],
                    refresh: m[2],
                })
                .collect();
            let expected = Output::from(RawOutput {
                name: reported.name.clone(),
                active: true,
                make: Some(reported.make.clone()),
                model: Some(reported.model.clone()),
                serial: Some(reported.serial.clone()),
                current_mode: None,
                modes: raw,
                rect: None,
                scale: None,
                transform: None,
                adaptive_sync_status: None,
            });
            assert_eq!(output.modes, expected.modes, "{}", output.name);
            assert_eq!(output.make, expected.make);
            assert_eq!(output.model, expected.model);
            assert_eq!(output.serial, expected.serial);
            assert!(output.active);
        }
        // Preferred modes, tiled left to right: DP-3 prefers 4K.
        let dp3 = simulated.iter().find(|o| o.name == "DP-3").unwrap();
        assert_eq!(dp3.current_mode.unwrap().width, 3840);
        assert_eq!(dp3.rect.x, 3840);
    }

    #[test]
    fn preflight_refuses_what_direct_mode_cannot_learn() {
        let dir = brain_tree();
        let card = dir.path().join("card1-DP-2");

        std::fs::remove_file(card.join("connector_id")).unwrap();
        let inventory = read_in(
            dir.path(),
            Path::new("/dev/dri"),
            &brain_pnp(),
            brain_query(),
        );
        let error = inventory.preflight().unwrap_err();
        assert!(
            error.contains("DP-2") && error.contains("connector_id"),
            "{error}"
        );
        std::fs::write(card.join("connector_id"), "133\n").unwrap();

        std::fs::write(card.join("edid"), b"").unwrap();
        let inventory = read_in(
            dir.path(),
            Path::new("/dev/dri"),
            &brain_pnp(),
            brain_query(),
        );
        let error = inventory.preflight().unwrap_err();
        assert!(error.contains("DP-2") && error.contains("EDID"), "{error}");
        std::fs::write(card.join("edid"), BRAIN_EDIDS[1].1).unwrap();

        let failing = |_: &Path, _: u32| Err(std::io::Error::other("ioctl refused"));
        let inventory = read_in(dir.path(), Path::new("/dev/dri"), &brain_pnp(), failing);
        let error = inventory.preflight().unwrap_err();
        assert!(error.contains("ioctl refused"), "{error}");
    }

    #[test]
    fn preflight_refuses_two_cards_and_an_empty_wall() {
        let dir = brain_tree();
        std::fs::rename(dir.path().join("card1-DP-4"), dir.path().join("card2-DP-4")).unwrap();
        let kernel = brain_kernel_modes();
        let query =
            move |_: &Path, id: u32| Ok(kernel[&format!("DP-{}", (id - 129) / 4 + 1)].clone());
        let inventory = read_in(dir.path(), Path::new("/dev/dri"), &brain_pnp(), query);
        let error = inventory.preflight().unwrap_err();
        assert!(error.contains("more than one card"), "{error}");

        let empty = DrmInventory::default();
        assert!(empty.preflight().is_err());
    }

    #[test]
    fn preflight_picks_the_card_with_connected_displays_over_an_idle_igpu() {
        // System B's shape: card0 (Intel iGPU) has a connector but nothing
        // plugged into it; card1 (NVIDIA) drives the wall. Direct
        // presentation, and the login profile's own render-node choice
        // (packaging/provision.sh), must agree on card1 — see F1a.
        let dir = brain_tree();
        let igpu = dir.path().join("card0-HDMI-1");
        std::fs::create_dir(&igpu).unwrap();
        std::fs::write(igpu.join("status"), "disconnected\n").unwrap();
        std::fs::create_dir(dir.path().join("card0")).unwrap();
        let inventory = read_in(
            dir.path(),
            Path::new("/dev/dri"),
            &brain_pnp(),
            brain_query(),
        );
        assert_eq!(inventory.preflight(), Ok(PathBuf::from("/dev/dri/card1")));
    }

    #[test]
    fn exact_refresh_recovers_millihertz_from_sways_hertz() {
        let dir = brain_tree();
        let inventory = read_in(
            dir.path(),
            Path::new("/dev/dri"),
            &brain_pnp(),
            brain_query(),
        );
        let dp1 = inventory.get("DP-1").unwrap();
        assert_eq!(dp1.exact_refresh_millihz(1920, 1080, 59.939), Some(59_939));
        assert_eq!(dp1.exact_refresh_millihz(1920, 1080, 60.0), Some(60_000));
        assert_eq!(dp1.exact_refresh_millihz(1920, 1080, 59.94), None);
    }
}
