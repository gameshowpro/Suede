//! Experimental direct DRM display target for the shared Vulkan blend renderer.
//! The caller explicitly chooses a primary card, connector IDs, and modes.

pub mod nvkms;
pub mod timing;

use super::{dev_major_minor, missing_requirement, DeviceState, RenderTarget, TargetKind};
use anyhow::{anyhow, bail, Context, Result};
use ash::{vk, Entry};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::CStr,
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::PathBuf,
    time::{Duration, Instant},
};

pub const INSTANCE_EXTENSIONS: [&CStr; 5] = [
    ash::khr::surface::NAME,
    ash::khr::display::NAME,
    ash::ext::acquire_drm_display::NAME,
    ash::ext::direct_mode_display::NAME,
    ash::khr::get_surface_capabilities2::NAME,
];
pub const DEVICE_EXTENSIONS: [&CStr; 6] = [
    ash::khr::swapchain::NAME,
    ash::khr::present_wait::NAME,
    ash::khr::present_id::NAME,
    timing::PRESENT_TIMING_NAME,
    timing::PRESENT_ID2_NAME,
    timing::CALIBRATED_TIMESTAMPS_NAME,
];

// Defined beside the daemon's derivation of it, which also builds on hosts
// without this Vulkan module; re-exported so the slicer's paths are unchanged.
pub use crate::presentation::{DirectDisplayConfig, DirectOutputConfig};

/// One blend job using the same output transfer table and blend shader as a
/// Wayland target. `display` indexes `DirectDisplayConfig::outputs`.
pub struct DirectBlendJob<'a> {
    pub display: usize,
    pub output: usize,
    pub source_x: u32,
    pub source_y: u32,
    pub warp: Option<&'a crate::projection::warp::Warp>,
}

/// A rendered set of images awaiting one batched present call.
pub struct DirectFrame {
    pub gpu_wait: Duration,
    pub(super) owner: vk::Device,
    pub(super) displays: Vec<usize>,
    pub(super) image_indices: Vec<u32>,
}

pub(super) struct AcquiredFrame {
    pub(super) targets: Vec<RenderTarget>,
    pub(super) image_indices: Vec<u32>,
    pub(super) waits: Vec<vk::Semaphore>,
    pub(super) signals: Vec<vk::Semaphore>,
}

#[derive(Clone, Debug)]
pub struct DirectFeedback {
    pub display: usize,
    pub output_name: String,
    pub connector_id: u32,
    pub snapshot_id: u64,
    pub present_id: u64,
    pub stage: u32,
    pub domain: i32,
    pub domain_id: u64,
    pub complete: bool,
    /// Validated CLOCK_MONOTONIC nanoseconds, if this driver exposed them.
    pub monotonic_ns: Option<u64>,
    pub refresh_millihz: u32,
}

/// Optional, aggregate-only diagnostics. Clock reads and serialization are
/// skipped entirely unless SUEDE_DIRECT_PROFILE=1 was set at startup.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    #[serde(rename = "type")]
    kind: &'static str,
    acquire: Vec<HeadMetric>,
    queue_present: Metric,
    timing_poll: Vec<HeadMetric>,
    calibration: Metric,
    shutdown: Metric,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HeadMetric {
    name: String,
    connector_id: u32,
    #[serde(flatten)]
    metric: Metric,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Metric {
    count: u64,
    wall_total_ns: u128,
    wall_max_ns: u128,
    cpu_samples: u64,
    cpu_total_ns: u128,
    cpu_max_ns: u64,
    cpu_failures: u64,
    cpu_last_errno: Option<i32>,
    cpu_regressions: u64,
}

struct ProfileSample {
    wall: Instant,
    cpu_ns: std::result::Result<u64, i32>,
}

impl Profile {
    fn enabled(config: &DirectDisplayConfig) -> Option<Self> {
        (std::env::var_os("SUEDE_DIRECT_PROFILE").as_deref() == Some(std::ffi::OsStr::new("1")))
            .then(|| {
                let heads = || {
                    config
                        .outputs
                        .iter()
                        .map(|output| HeadMetric {
                            name: output.name.clone(),
                            connector_id: output.connector_id,
                            metric: Metric::default(),
                        })
                        .collect()
                };
                Self {
                    kind: "directProfile",
                    acquire: heads(),
                    queue_present: Metric::default(),
                    timing_poll: heads(),
                    calibration: Metric::default(),
                    shutdown: Metric::default(),
                }
            })
    }

    fn report(&self) {
        match serde_json::to_string(self) {
            Ok(json) => eprintln!("{json}"),
            Err(error) => eprintln!("slicer: direct profile serialization failed: {error}"),
        }
    }
}

impl ProfileSample {
    fn start() -> Self {
        Self {
            wall: Instant::now(),
            cpu_ns: thread_cpu_ns(),
        }
    }
}

impl Metric {
    fn record(&mut self, sample: ProfileSample) {
        let wall_ns = sample.wall.elapsed().as_nanos();
        let cpu_end = thread_cpu_ns();
        self.count += 1;
        self.wall_total_ns += wall_ns;
        self.wall_max_ns = self.wall_max_ns.max(wall_ns);
        match (sample.cpu_ns, cpu_end) {
            (Ok(start), Ok(end)) if end >= start => {
                let elapsed = end - start;
                self.cpu_samples += 1;
                self.cpu_total_ns += u128::from(elapsed);
                self.cpu_max_ns = self.cpu_max_ns.max(elapsed);
            }
            (Ok(_), Ok(_)) => self.cpu_regressions += 1,
            (Err(errno), _) | (_, Err(errno)) => {
                self.cpu_failures += 1;
                self.cpu_last_errno = Some(errno);
            }
        }
    }
}

fn thread_cpu_ns() -> std::result::Result<u64, i32> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safety: `time` is initialized writable storage and remains alive for
    // the synchronous system call. CLOCK_THREAD_CPUTIME_ID reads only this
    // calling thread's CPU time, not process-wide or wall-clock time.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    let seconds = u64::try_from(time.tv_sec).map_err(|_| libc::EOVERFLOW)?;
    let nanos = u64::try_from(time.tv_nsec).map_err(|_| libc::EOVERFLOW)?;
    if nanos >= 1_000_000_000 {
        return Err(libc::EOVERFLOW);
    }
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or(libc::EOVERFLOW)
}

fn has_extension(properties: &[vk::ExtensionProperties], name: &CStr) -> bool {
    properties
        .iter()
        .any(|property| unsafe { CStr::from_ptr(property.extension_name.as_ptr()) } == name)
}

pub(super) fn check_instance_extensions(entry: &Entry) -> Result<()> {
    let properties = unsafe { entry.enumerate_instance_extension_properties(None) }?;
    for name in INSTANCE_EXTENSIONS {
        if !has_extension(&properties, name) {
            bail!(
                "direct display requires instance extension {}",
                name.to_string_lossy()
            );
        }
    }
    Ok(())
}

/// Index of the best-matching entry in `refreshes` (already filtered to the
/// requested output's width/height) for `requested_millihz`.
///
/// An exact match wins outright. Otherwise the nearest entry within ±5 mHz
/// is used — Sway reports 59.939 Hz as 59939 mHz where the saved
/// configuration (built from Vulkan's own enumeration) says 59940, and the
/// two must resolve to the same mode. 60.000 Hz (60000) is 60 mHz away from
/// 59.940 Hz (59940), well outside that tolerance, and must never be
/// accepted as a match for it. `None` means no candidate is close enough.
fn nearest_refresh_index(refreshes: &[u32], requested_millihz: u32) -> Option<usize> {
    if let Some(index) = refreshes
        .iter()
        .position(|&refresh| refresh == requested_millihz)
    {
        return Some(index);
    }
    refreshes
        .iter()
        .enumerate()
        .filter(|(_, &refresh)| refresh.abs_diff(requested_millihz) <= 5)
        .min_by_key(|(_, &refresh)| refresh.abs_diff(requested_millihz))
        .map(|(index, _)| index)
}

fn validate_config(config: &DirectDisplayConfig) -> Result<()> {
    if config.outputs.is_empty() || config.outputs.len() > super::MAX_OUTPUTS {
        bail!("direct display requires 1..={} outputs", super::MAX_OUTPUTS);
    }
    let mut connectors = HashSet::new();
    let mut names = HashSet::new();
    for output in &config.outputs {
        if output.name.is_empty() || !names.insert(&output.name) {
            bail!("direct display output names must be nonempty and unique");
        }
        if output.connector_id == 0 || !connectors.insert(output.connector_id) {
            bail!("direct display connector IDs must be nonzero and unique");
        }
        if output.width == 0 || output.height == 0 || output.refresh_millihz == 0 {
            bail!("direct display output dimensions and refresh must be positive");
        }
    }
    Ok(())
}

/// `suede display-reset`: clear a stale NVKMS sub-ownership grant left by a
/// crashed direct-mode slicer, on every DRM card, so a later compositor is
/// not blocked from committing. Never fails the process: opening a card,
/// checking its driver, or revoking a grant can each fail independently
/// (most commonly because another process already holds DRM master, which
/// makes the revoke ioctl fail every time — see `nvkms::revoke_sub_ownership`),
/// and each of those is reported per card rather than aborting the scan.
pub fn display_reset() {
    let entries = match std::fs::read_dir("/dev/dri") {
        Ok(entries) => entries,
        Err(error) => {
            println!("display-reset: cannot list /dev/dri: {error}");
            return;
        }
    };
    let mut cards: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("card"))
        })
        .collect();
    cards.sort();
    if cards.is_empty() {
        println!("display-reset: no /dev/dri/card* devices found");
        return;
    }
    for card in cards {
        match OpenOptions::new().read(true).write(true).open(&card) {
            Ok(file) => {
                let fd = file.as_raw_fd();
                if !nvkms::is_nvidia_drm(fd) {
                    println!("{}: not nvidia-drm, skipped", card.display());
                    continue;
                }
                match nvkms::clear_stale_sub_ownership(fd) {
                    // nvidia-drm reports success on any revoke, whether or not
                    // a grant was actually held — see
                    // `clear_stale_sub_ownership`'s own doc comment — so this
                    // must not claim a stale grant existed, only that the
                    // sub-ownership state was reset and any earlier grant is
                    // now cleared.
                    Ok(true) => println!(
                        "{}: sub-ownership reset; any earlier grant cleared",
                        card.display()
                    ),
                    Ok(false) => {
                        println!("{}: no stale NVKMS sub-ownership grant", card.display())
                    }
                    Err(error) => println!(
                        "{}: NVKMS sub-ownership clear skipped (no DRM master, or another error): {error:#}",
                        card.display()
                    ),
                }
            }
            Err(error) => println!("{}: open failed: {error}", card.display()),
        }
    }
}

pub(super) fn open_card(config: &DirectDisplayConfig) -> Result<(File, (i64, i64))> {
    validate_config(config)?;
    let card = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&config.card)
        .with_context(|| format!("opening direct display card {}", config.card.display()))?;
    let metadata = card.metadata()?;
    if !metadata.file_type().is_char_device() {
        bail!("{} is not a character device", config.card.display());
    }
    let major = libc::major(metadata.rdev()) as i64;
    let minor = libc::minor(metadata.rdev()) as i64;
    Ok((card, (major, minor)))
}

pub(super) fn pick_direct_physical_device(
    instance: &ash::Instance,
    render_node: Option<u64>,
    primary: (i64, i64),
) -> Result<vk::PhysicalDevice> {
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    for physical in devices {
        if missing_requirement(instance, physical).is_some() {
            continue;
        }
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
        unsafe { instance.get_physical_device_properties2(physical, &mut props) };
        if drm.has_primary == 0 || (drm.primary_major, drm.primary_minor) != primary {
            continue;
        }
        if let Some(render_node) = render_node {
            let requested = dev_major_minor(render_node);
            if !((drm.has_render != 0 && (drm.render_major, drm.render_minor) == requested)
                || (drm.has_primary != 0 && (drm.primary_major, drm.primary_minor) == requested))
            {
                continue;
            }
        }
        let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }?;
        // The one failure this hardware actually hits (550 on System B: see
        // docs/developer/vk-khr.md) gets a message that names the missing
        // capability and the device up front, rather than the generic "lacks
        // extension X" below, which only ever named the first of the two it
        // happened to check — leaving an operator to go find the other one
        // in a journal.
        let missing_present_timing = !has_extension(&extensions, timing::PRESENT_TIMING_NAME);
        let missing_present_id2 = !has_extension(&extensions, timing::PRESENT_ID2_NAME);
        if missing_present_timing || missing_present_id2 {
            let (name, driver_version) = device_name_and_driver(instance, physical);
            bail!(
                "{}",
                describe_missing_present_capabilities(
                    &name,
                    &driver_version,
                    missing_present_timing,
                    missing_present_id2,
                )
            );
        }
        for name in DEVICE_EXTENSIONS {
            if !has_extension(&extensions, name) {
                bail!(
                    "matching GPU lacks direct display extension {}",
                    name.to_string_lossy()
                );
            }
        }
        let mut present_wait = vk::PhysicalDevicePresentWaitFeaturesKHR::default();
        let mut present_id = vk::PhysicalDevicePresentIdFeaturesKHR::default();
        let mut present_timing = timing::Features::default();
        let mut present_id2 = timing::Id2Features::default();
        let mut features = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut present_wait)
            .push_next(&mut present_id);
        present_timing.p_next = features.p_next;
        present_id2.p_next = (&mut present_timing as *mut timing::Features).cast();
        features.p_next = (&mut present_id2 as *mut timing::Id2Features).cast();
        unsafe { instance.get_physical_device_features2(physical, &mut features) };
        if present_wait.present_wait == 0
            || present_id.present_id == 0
            || present_timing.present_timing == 0
            || present_id2.present_id2 == 0
        {
            bail!("matching GPU lacks present wait, present ID, or present timing features");
        }
        return Ok(physical);
    }
    bail!(
        "no Vulkan GPU matches primary DRM card {}:{} and render node {:?}",
        primary.0,
        primary.1,
        render_node.map(dev_major_minor)
    )
}

/// This physical device's name and driver version, best-effort, for the
/// capability refusal message below. `driverInfo` is queried rather than the
/// packed `driverVersion` integer because NVIDIA reports the same version
/// string there as `nvidia-smi`/`/proc/driver/nvidia` (e.g. "550.163.01"),
/// which is what an operator comparing against
/// [`crate::nvidia_driver`](../../nvidia_driver/index.html)'s own report, or
/// the campaign notes in docs/developer/vk-khr.md, actually recognizes.
fn device_name_and_driver(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> (String, String) {
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
    // Safety: read-only query; `props`/`driver` are fully written by the
    // driver before use.
    unsafe { instance.get_physical_device_properties2(physical, &mut props) };
    // Safety: `device_name` is a NUL-terminated byte array the driver fills
    // in as part of `props` above.
    let name = unsafe { CStr::from_ptr(props.properties.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    // `props`'s last use: its `push_next` chain borrows `driver`, so this
    // packed fallback is read out before `driver` is borrowed again below.
    let packed_version = props.properties.driver_version;
    let driver_version = driver
        .driver_info_as_c_str()
        .ok()
        .map(|info| info.to_string_lossy().into_owned())
        .filter(|info| !info.is_empty())
        .unwrap_or_else(|| format!("driver {packed_version:#x}"));
    (name, driver_version)
}

/// A single clear line for the one direct-presentation capability refusal
/// this hardware actually hits (550 on System B lacks both extensions; see
/// docs/developer/vk-khr.md), rather than the generic "lacks extension X"
/// that only ever names the first of the two a scan happened to check. Pure,
/// so the wording is unit-testable without a Vulkan instance.
fn describe_missing_present_capabilities(
    name: &str,
    driver_version: &str,
    missing_present_timing: bool,
    missing_present_id2: bool,
) -> String {
    let timing_name = timing::PRESENT_TIMING_NAME.to_string_lossy();
    let id2_name = timing::PRESENT_ID2_NAME.to_string_lossy();
    let exposes = match (missing_present_timing, missing_present_id2) {
        (true, true) => "exposes neither".to_string(),
        (true, false) => format!("exposes {id2_name} but not {timing_name}"),
        (false, true) => format!("exposes {timing_name} but not {id2_name}"),
        (false, false) => {
            unreachable!("called only when at least one of the two is missing")
        }
    };
    format!(
        "direct presentation needs {timing_name} and {id2_name}; this device/driver \
         ({name}, {driver_version}) {exposes}"
    )
}

struct Head {
    config: DirectOutputConfig,
    display: vk::DisplayKHR,
    surface: vk::SurfaceKHR,
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    format: vk::Format,
    acquired: vk::Semaphore,
    ready: Vec<vk::Semaphore>,
    presented: Vec<bool>,
    timing_stage: u32,
    timing_domain: i32,
    timing_domain_id: u64,
    next_present_id: u64,
    last_present_id: u64,
    pending_snapshots: HashMap<u64, u64>,
}

impl Head {
    fn empty(config: DirectOutputConfig, display: vk::DisplayKHR) -> Self {
        Self {
            config,
            display,
            surface: vk::SurfaceKHR::null(),
            swapchain: vk::SwapchainKHR::null(),
            images: Vec::new(),
            views: Vec::new(),
            format: vk::Format::UNDEFINED,
            acquired: vk::Semaphore::null(),
            ready: Vec::new(),
            presented: Vec::new(),
            timing_stage: 0,
            timing_domain: 0,
            timing_domain_id: 0,
            next_present_id: 1,
            last_present_id: 0,
            pending_snapshots: HashMap::new(),
        }
    }
}

pub(super) struct DirectDisplay {
    card: Option<File>,
    device: std::sync::Arc<DeviceState>,
    display_api: ash::khr::display::Instance,
    acquire_api: ash::ext::acquire_drm_display::Instance,
    direct_api: ash::ext::direct_mode_display::Instance,
    surface_api: ash::khr::surface::Instance,
    surface_caps2_api: ash::khr::get_surface_capabilities2::Instance,
    swapchain_api: ash::khr::swapchain::Device,
    present_wait_api: ash::khr::present_wait::Device,
    timing_api: timing::Api,
    calibration_api: timing::CalibrationApi,
    calibration_samples: VecDeque<timing::CalibrationSample>,
    profile: Option<Profile>,
    heads: Vec<Head>,
    frame_pending: bool,
    poisoned: bool,
    closed: bool,
    /// NVKMS sub-ownership was granted to the driver handle; see [`nvkms`].
    sub_owner: bool,
}

impl DirectDisplay {
    pub(super) fn target_size(&self, display: usize) -> Result<(u32, u32)> {
        let head = self
            .heads
            .get(display)
            .ok_or_else(|| anyhow!("unknown direct display index {display}"))?;
        Ok((head.config.width, head.config.height))
    }

    pub(super) fn new(
        device: &std::sync::Arc<DeviceState>,
        card: File,
        config: DirectDisplayConfig,
    ) -> Result<Self> {
        let entry = &device._entry;
        let instance = &device.instance;
        let vk_device = &device.device;
        let profile = Profile::enabled(&config);
        let mut direct = Self {
            card: Some(card),
            device: device.clone(),
            display_api: ash::khr::display::Instance::new(entry, instance),
            acquire_api: ash::ext::acquire_drm_display::Instance::new(entry, instance),
            direct_api: ash::ext::direct_mode_display::Instance::new(entry, instance),
            surface_api: ash::khr::surface::Instance::new(entry, instance),
            surface_caps2_api: ash::khr::get_surface_capabilities2::Instance::new(entry, instance),
            swapchain_api: ash::khr::swapchain::Device::new(instance, vk_device),
            present_wait_api: ash::khr::present_wait::Device::new(instance, vk_device),
            timing_api: unsafe { timing::Api::new(instance, vk_device)? },
            calibration_api: unsafe {
                timing::CalibrationApi::new(entry, instance, device.physical_device, vk_device)?
            },
            calibration_samples: VecDeque::new(),
            profile: None,
            heads: Vec::new(),
            frame_pending: false,
            poisoned: false,
            closed: false,
            sub_owner: false,
        };
        let card_fd = direct
            .card
            .as_ref()
            .expect("card held until shutdown")
            .as_raw_fd();
        let nvidia = nvkms::is_nvidia_drm(card_fd);
        if nvidia {
            match nvkms::clear_stale_sub_ownership(card_fd) {
                Ok(true) => eprintln!(
                    "slicer: {}: sub-ownership reset; any earlier grant cleared",
                    config.card.display()
                ),
                Ok(false) => {}
                Err(error) => eprintln!(
                    "slicer: checking for a stale NVKMS sub-ownership grant failed: {error:#}"
                ),
            }
        }
        direct.create_heads(config)?;
        if nvidia {
            // Only after every display is acquired: with full permissions
            // already held, vkAcquireDrmDisplayEXT fails. Nothing has set a
            // mode yet, so nvidia-drm's disable-all on grant costs nothing.
            match nvkms::grant_sub_ownership_to_driver(card_fd) {
                Ok(fd) => {
                    direct.sub_owner = true;
                    eprintln!("slicer: NVKMS sub-ownership granted to the NVIDIA driver (fd {fd}); nvidia-drm flip bookkeeping and atomic commits are suspended until shutdown");
                }
                Err(error @ nvkms::GrantFailure::NotGranted(_)) => eprintln!("slicer: NVKMS sub-ownership unavailable ({error}); continuing with per-head modeset permission"),
                Err(error @ nvkms::GrantFailure::RevokedAfterGrant(_)) => bail!("NVKMS sub-ownership could not be acquired ({error}); revoking the grant removed the driver's per-head modeset permission, so direct presentation cannot start"),
            }
        }
        direct.profile = profile;
        if let Ok(sample) = unsafe { direct.calibration_api.sample_device_monotonic() } {
            direct.calibration_samples.push_back(sample);
        }
        Ok(direct)
    }

    fn create_heads(&mut self, config: DirectDisplayConfig) -> Result<()> {
        let physical = self.device.physical_device;
        let card_fd = self
            .card
            .as_ref()
            .expect("card held until shutdown")
            .as_raw_fd();
        let planes = unsafe {
            self.display_api
                .get_physical_device_display_plane_properties(physical)
        }?;
        let displays = unsafe {
            self.display_api
                .get_physical_device_display_properties(physical)
        }?;
        let mut used_planes = HashSet::new();
        let mut used_displays = HashSet::new();
        for output in config.outputs {
            let connector = output.connector_id;
            let display = unsafe {
                self.acquire_api
                    .get_drm_display(physical, card_fd, connector)
            }
            .with_context(|| format!("connector {connector}: vkGetDrmDisplayEXT"))?;
            if display == vk::DisplayKHR::null() || !used_displays.insert(display) {
                bail!("connector {connector}: no distinct Vulkan display");
            }
            let props = displays
                .iter()
                .find(|item| item.display == display)
                .ok_or_else(|| anyhow!("connector {connector}: display absent from inventory"))?;
            if !props
                .supported_transforms
                .contains(vk::SurfaceTransformFlagsKHR::IDENTITY)
            {
                bail!("connector {connector}: identity display transform unsupported");
            }
            let modes = unsafe {
                self.display_api
                    .get_display_mode_properties(physical, display)
            }?;
            let same_size: Vec<_> = modes
                .into_iter()
                .filter(|mode| {
                    mode.parameters.visible_region.width == output.width
                        && mode.parameters.visible_region.height == output.height
                })
                .collect();
            let refreshes: Vec<u32> = same_size
                .iter()
                .map(|mode| mode.parameters.refresh_rate)
                .collect();
            let mode = nearest_refresh_index(&refreshes, output.refresh_millihz)
                .map(|index| same_size[index])
                .ok_or_else(|| {
                    anyhow!(
                        "connector {connector}: requested {}x{} @ {} mHz mode absent (no mode within \u{b1}5 mHz)",
                        output.width,
                        output.height,
                        output.refresh_millihz
                    )
                })?;
            let mut chosen_plane = None;
            for plane in 0..planes.len() as u32 {
                if used_planes.contains(&plane) {
                    continue;
                }
                let supported = unsafe {
                    self.display_api
                        .get_display_plane_supported_displays(physical, plane)
                }?;
                if !supported.contains(&display)
                    || (planes[plane as usize].current_display != vk::DisplayKHR::null()
                        && planes[plane as usize].current_display != display)
                {
                    continue;
                }
                let caps = unsafe {
                    self.display_api.get_display_plane_capabilities(
                        physical,
                        mode.display_mode,
                        plane,
                    )
                }?;
                let size = mode.parameters.visible_region;
                if caps
                    .supported_alpha
                    .contains(vk::DisplayPlaneAlphaFlagsKHR::OPAQUE)
                    && size.width >= caps.min_src_extent.width
                    && size.height >= caps.min_src_extent.height
                    && size.width <= caps.max_src_extent.width
                    && size.height <= caps.max_src_extent.height
                    && size.width >= caps.min_dst_extent.width
                    && size.height >= caps.min_dst_extent.height
                    && size.width <= caps.max_dst_extent.width
                    && size.height <= caps.max_dst_extent.height
                {
                    chosen_plane = Some(plane);
                    break;
                }
            }
            let plane = chosen_plane
                .ok_or_else(|| anyhow!("connector {connector}: no distinct opaque plane"))?;
            used_planes.insert(plane);
            unsafe {
                self.acquire_api
                    .acquire_drm_display(physical, card_fd, display)
            }
            .with_context(|| {
                format!("connector {connector}: vkAcquireDrmDisplayEXT requires DRM master")
            })?;
            self.heads.push(Head::empty(output, display));
            let head = self.heads.last_mut().unwrap();
            let size = mode.parameters.visible_region;
            let surface_info = vk::DisplaySurfaceCreateInfoKHR::default()
                .display_mode(mode.display_mode)
                .plane_index(plane)
                .plane_stack_index(planes[plane as usize].current_stack_index)
                .transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
                .global_alpha(1.0)
                .alpha_mode(vk::DisplayPlaneAlphaFlagsKHR::OPAQUE)
                .image_extent(size);
            head.surface = unsafe {
                self.display_api
                    .create_display_plane_surface(&surface_info, None)
            }
            .with_context(|| format!("connector {connector}: create display surface"))?;
            if !unsafe {
                self.surface_api.get_physical_device_surface_support(
                    physical,
                    self.device.queue_family,
                    head.surface,
                )
            }? {
                bail!("connector {connector}: blend queue cannot present to surface");
            }
            let surface_info2 = vk::PhysicalDeviceSurfaceInfo2KHR::default().surface(head.surface);
            let mut timing_caps = timing::SurfaceCaps::default();
            let mut id2_caps = timing::Id2SurfaceCaps::default();
            let mut capabilities2 = vk::SurfaceCapabilities2KHR::default();
            timing_caps.p_next = capabilities2.p_next;
            capabilities2.p_next = (&mut timing_caps as *mut timing::SurfaceCaps).cast();
            id2_caps.p_next = capabilities2.p_next;
            capabilities2.p_next = (&mut id2_caps as *mut timing::Id2SurfaceCaps).cast();
            unsafe {
                self.surface_caps2_api
                    .get_physical_device_surface_capabilities2(
                        physical,
                        &surface_info2,
                        &mut capabilities2,
                    )
            }?;
            if timing_caps.present_timing_supported == 0 || id2_caps.present_id2_supported == 0 {
                bail!("connector {connector}: present timing unsupported on surface");
            }
            head.timing_stage = [
                timing::STAGE_FIRST_PIXEL_VISIBLE,
                timing::STAGE_FIRST_PIXEL_OUT,
            ]
            .into_iter()
            .find(|stage| timing_caps.present_stage_queries & stage != 0)
            .ok_or_else(|| {
                anyhow!("connector {connector}: no visible/output pixel timing stage")
            })?;
            let caps = unsafe {
                self.surface_api
                    .get_physical_device_surface_capabilities(physical, head.surface)
            }?;
            let formats = unsafe {
                self.surface_api
                    .get_physical_device_surface_formats(physical, head.surface)
            }?;
            let format = formats
                .into_iter()
                .find(|format| {
                    format.format != vk::Format::UNDEFINED
                        && format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                        && matches!(
                            format.format,
                            vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM
                        )
                        && unsafe {
                            self.device
                                .instance
                                .get_physical_device_format_properties(physical, format.format)
                        }
                        .optimal_tiling_features
                        .contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT)
                })
                .ok_or_else(|| {
                    anyhow!("connector {connector}: no supported color attachment surface format")
                })?;
            let modes = unsafe {
                self.surface_api
                    .get_physical_device_surface_present_modes(physical, head.surface)
            }?;
            if !modes.contains(&vk::PresentModeKHR::FIFO) {
                bail!("connector {connector}: FIFO present mode absent");
            }
            if !caps
                .supported_usage_flags
                .contains(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                || !caps
                    .supported_transforms
                    .contains(vk::SurfaceTransformFlagsKHR::IDENTITY)
                || !caps
                    .supported_composite_alpha
                    .contains(vk::CompositeAlphaFlagsKHR::OPAQUE)
            {
                bail!("connector {connector}: color attachment, identity, or opaque surface capability absent");
            }
            let count = caps.min_image_count.max(2);
            if caps.max_image_count != 0 && count > caps.max_image_count {
                bail!("connector {connector}: fewer than two swapchain images allowed");
            }
            let extent = if caps.current_extent.width == u32::MAX {
                size
            } else {
                caps.current_extent
            };
            if extent != size {
                bail!("connector {connector}: surface extent differs from requested mode");
            }
            let info = vk::SwapchainCreateInfoKHR::default()
                .flags(timing::SWAPCHAIN_TIMING_FLAG | timing::SWAPCHAIN_ID2_FLAG)
                .surface(head.surface)
                .min_image_count(count)
                .image_format(format.format)
                .image_color_space(format.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
                .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                .present_mode(vk::PresentModeKHR::FIFO)
                .clipped(true);
            head.swapchain = unsafe { self.swapchain_api.create_swapchain(&info, None) }
                .with_context(|| format!("connector {connector}: create swapchain"))?;
            head.images = unsafe { self.swapchain_api.get_swapchain_images(head.swapchain) }?;
            head.format = format.format;
            unsafe {
                self.timing_api
                    .set_queue_size(head.swapchain, head.images.len())
            }?;
            let domains = unsafe { self.timing_api.time_domains(head.swapchain) }?;
            let (domain, domain_id) = domains.iter().copied()
                .find(|(domain, _)| *domain == timing::MONOTONIC_DOMAIN)
                .or_else(|| domains.iter().copied().find(|(domain, _)| *domain == timing::DEVICE_DOMAIN))
                .ok_or_else(|| anyhow!("connector {connector}: neither monotonic nor calibratable device time domain"))?;
            head.timing_domain = domain;
            head.timing_domain_id = domain_id;
            if domain == timing::DEVICE_DOMAIN {
                let calibration_domains = unsafe { self.calibration_api.available_domains() }?;
                if !calibration_domains.contains(&timing::DEVICE_DOMAIN)
                    || !calibration_domains.contains(&timing::MONOTONIC_DOMAIN)
                    || self.calibration_api.timestamp_period_ns != 1.0
                {
                    bail!("connector {connector}: device timing cannot be calibrated to CLOCK_MONOTONIC");
                }
            }
            for image in &head.images {
                let view_info = vk::ImageViewCreateInfo::default()
                    .image(*image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format.format)
                    .subresource_range(super::color_subresource_range());
                head.views
                    .push(unsafe { self.device.device.create_image_view(&view_info, None) }?);
                head.ready.push(unsafe {
                    self.device
                        .device
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                }?);
            }
            head.acquired = unsafe {
                self.device
                    .device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?;
            head.presented = vec![false; head.images.len()];
        }
        if self
            .heads
            .iter()
            .any(|head| head.format != self.heads[0].format)
        {
            bail!("direct displays expose different color formats; one blend pipeline cannot serve them");
        }
        Ok(())
    }

    pub(super) fn acquire(&mut self, displays: &[usize]) -> Result<AcquiredFrame> {
        if self.closed || self.poisoned || self.frame_pending {
            bail!("direct display unavailable or prior frame not presented");
        }
        if displays.is_empty() {
            bail!("direct display frame needs at least one output");
        }
        let mut unique = HashSet::new();
        let mut targets = Vec::new();
        let mut indices = Vec::new();
        let mut waits = Vec::new();
        let mut signals = Vec::new();
        for &display in displays {
            if !unique.insert(display) {
                bail!("duplicate direct display in frame");
            }
            let head = self
                .heads
                .get_mut(display)
                .ok_or_else(|| anyhow!("unknown direct display index {display}"))?;
            self.poisoned = true;
            let profile_sample = self.profile.as_ref().map(|_| ProfileSample::start());
            let acquired = unsafe {
                self.swapchain_api.acquire_next_image(
                    head.swapchain,
                    100_000_000,
                    head.acquired,
                    vk::Fence::null(),
                )
            };
            if let (Some(profile), Some(sample)) = (&mut self.profile, profile_sample) {
                profile.acquire[display].metric.record(sample);
            }
            let (index, suboptimal) = acquired.with_context(|| {
                format!(
                    "connector {}: swapchain acquire failed or timed out",
                    head.config.connector_id
                )
            })?;
            if suboptimal {
                bail!(
                    "connector {}: swapchain suboptimal",
                    head.config.connector_id
                );
            }
            let index_usize = index as usize;
            targets.push(RenderTarget {
                width: head.config.width,
                height: head.config.height,
                format: head.format,
                image: head.images[index_usize],
                view: head.views[index_usize],
                kind: TargetKind::Swapchain {
                    old_layout: if head.presented[index_usize] {
                        vk::ImageLayout::PRESENT_SRC_KHR
                    } else {
                        vk::ImageLayout::UNDEFINED
                    },
                },
            });
            indices.push(index);
            waits.push(head.acquired);
            signals.push(head.ready[index_usize]);
        }
        self.frame_pending = true;
        self.poisoned = false;
        Ok(AcquiredFrame {
            targets,
            image_indices: indices,
            waits,
            signals,
        })
    }

    pub(super) fn render_failed(&mut self) {
        self.frame_pending = false;
        self.poisoned = true;
    }

    pub(super) fn present(&mut self, frame: &DirectFrame, snapshot_id: u64) -> Result<()> {
        if frame.owner != self.device.device.handle() {
            bail!("direct frame belongs to another Vulkan device");
        }
        if !self.frame_pending || self.poisoned {
            bail!("direct display has no rendered frame");
        }
        if frame
            .displays
            .iter()
            .any(|&display| self.heads[display].pending_snapshots.len() >= 256)
        {
            self.poisoned = true;
            bail!("direct display timing feedback backlog exceeded 256 presents");
        }
        let swapchains: Vec<_> = frame
            .displays
            .iter()
            .map(|&index| self.heads[index].swapchain)
            .collect();
        let semaphores: Vec<_> = frame
            .displays
            .iter()
            .zip(&frame.image_indices)
            .map(|(&index, &image)| self.heads[index].ready[image as usize])
            .collect();
        let ids: Vec<_> = frame
            .displays
            .iter()
            .map(|&index| self.heads[index].next_present_id)
            .collect();
        let timing_infos: Vec<_> = frame
            .displays
            .iter()
            .map(|&index| {
                let head = &self.heads[index];
                timing::PresentTimingInfo::query(head.timing_stage, head.timing_domain_id)
            })
            .collect();
        let mut present_ids = vk::PresentIdKHR::default().present_ids(&ids);
        let mut results = vec![vk::Result::ERROR_UNKNOWN; frame.displays.len()];
        let mut info = vk::PresentInfoKHR::default()
            .wait_semaphores(&semaphores)
            .swapchains(&swapchains)
            .image_indices(&frame.image_indices)
            .results(&mut results)
            .push_next(&mut present_ids);
        let mut timing_request = timing::PresentTimingsInfo::new(&timing_infos);
        let mut id2_request = timing::PresentId2::new(&ids);
        timing_request.p_next = info.p_next;
        id2_request.p_next = (&timing_request as *const timing::PresentTimingsInfo).cast();
        info.p_next = (&id2_request as *const timing::PresentId2).cast();
        self.frame_pending = false;
        self.poisoned = true;
        let profile_sample = self.profile.as_ref().map(|_| ProfileSample::start());
        let overall = unsafe { self.swapchain_api.queue_present(self.device.queue, &info) };
        if let (Some(profile), Some(sample)) = (&mut self.profile, profile_sample) {
            profile.queue_present.record(sample);
        }
        for ((&display, &image), (&id, &result)) in frame
            .displays
            .iter()
            .zip(&frame.image_indices)
            .zip(ids.iter().zip(&results))
        {
            if result == vk::Result::SUCCESS || result == vk::Result::SUBOPTIMAL_KHR {
                let head = &mut self.heads[display];
                head.presented[image as usize] = true;
                head.last_present_id = id;
                head.next_present_id += 1;
                head.pending_snapshots.insert(id, snapshot_id);
            }
        }
        if overall == Err(timing::QUEUE_FULL) {
            bail!("direct present timing queue full");
        }
        let suboptimal = overall.context("batched vkQueuePresentKHR")?;
        if suboptimal || results.iter().any(|result| *result != vk::Result::SUCCESS) {
            bail!(
                "direct display present returned suboptimal or per-swapchain failure: {results:?}"
            );
        }
        self.poisoned = false;
        Ok(())
    }

    /// Poll `VK_EXT_present_timing` for every head, taking a fresh
    /// calibration sample first, every call. This is deliberately not
    /// throttled to the render loop's wake or present rate: gating the poll
    /// to at most twice the present rate let the presentation gate open up
    /// to half a refresh period later and cost 4–7 fps at 4500×2679 on System A
    /// with no measurable CPU saving (see "Attribution check" in
    /// `docs/plans/display-component-results.md`), so it was reverted.
    pub(super) fn poll(&mut self) -> Result<Vec<DirectFeedback>> {
        if self.closed {
            return Ok(Vec::new());
        }
        self.sample_calibration();

        let mut feedback = Vec::new();
        for (index, head) in self.heads.iter_mut().enumerate() {
            let profile_sample = self.profile.as_ref().map(|_| ProfileSample::start());
            let polled = unsafe { self.timing_api.poll(head.swapchain) };
            if let (Some(profile), Some(sample)) = (&mut self.profile, profile_sample) {
                profile.timing_poll[index].metric.record(sample);
            }
            let records = polled.with_context(|| {
                format!(
                    "connector {}: poll present timing",
                    head.config.connector_id
                )
            })?;
            for record in records {
                let snapshot_id = match head.pending_snapshots.get(&record.present_id) {
                    Some(&id) => id,
                    None => continue,
                };
                let matching_record = record.stage == head.timing_stage
                    && record.time_domain == head.timing_domain
                    && record.time_domain_id == head.timing_domain_id;
                let monotonic_ns = matching_record
                    .then(|| {
                        record.common_monotonic_ns().or_else(|| {
                            if !record.complete || record.time_domain != timing::DEVICE_DOMAIN {
                                return None;
                            }
                            self.calibration_samples
                                .iter()
                                .zip(self.calibration_samples.iter().skip(1))
                                .find_map(|(&start, &end)| {
                                    self.calibration_api
                                        .pair(start, end)
                                        .estimate_present_device_ns(record.time_ns)
                                        .map(|time| time.monotonic_ns)
                                })
                        })
                    })
                    .flatten();
                feedback.push(DirectFeedback {
                    display: index,
                    output_name: head.config.name.clone(),
                    connector_id: head.config.connector_id,
                    snapshot_id,
                    present_id: record.present_id,
                    stage: record.stage,
                    domain: record.time_domain,
                    domain_id: record.time_domain_id,
                    complete: record.complete,
                    monotonic_ns,
                    refresh_millihz: head.config.refresh_millihz,
                });
                if record.complete {
                    head.pending_snapshots.remove(&record.present_id);
                }
            }
        }
        Ok(feedback)
    }

    /// Take one calibration sample and, on success, push it as the newest
    /// entry in the bounded history (capped at 32 entries, comfortably more
    /// than any plausible feedback latency).
    fn sample_calibration(&mut self) {
        let profile_sample = self.profile.as_ref().map(|_| ProfileSample::start());
        let calibrated = unsafe { self.calibration_api.sample_device_monotonic() };
        if let (Some(profile), Some(sample)) = (&mut self.profile, profile_sample) {
            profile.calibration.record(sample);
        }
        if let Ok(sample) = calibrated {
            self.calibration_samples.push_back(sample);
            while self.calibration_samples.len() > 32 {
                self.calibration_samples.pop_front();
            }
        }
    }

    pub(super) fn shutdown(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let profile_sample = self.profile.as_ref().map(|_| ProfileSample::start());
        for head in &mut self.heads {
            if head.last_present_id != 0 {
                unsafe {
                    self.present_wait_api.wait_for_present(
                        head.swapchain,
                        head.last_present_id,
                        2_000_000_000,
                    )
                }
                .with_context(|| {
                    format!(
                        "connector {}: final present completion unverified",
                        head.config.connector_id
                    )
                })?;
                head.last_present_id = 0;
            }
        }
        // The renderer waits its fence after every direct submit. A failed
        // submit leaves `poisoned` set and requires the caller to quarantine.
        if self.poisoned || self.frame_pending {
            // The Vulkan objects stay alive, but the kernel grant must not:
            // while it stands, no compositor can light these outputs.
            self.revoke_sub_owner();
            bail!("direct display has unverified work");
        }
        unsafe {
            for head in &mut self.heads {
                for view in head.views.drain(..) {
                    self.device.device.destroy_image_view(view, None);
                }
                for semaphore in head.ready.drain(..) {
                    self.device.device.destroy_semaphore(semaphore, None);
                }
                if head.acquired != vk::Semaphore::null() {
                    self.device.device.destroy_semaphore(head.acquired, None);
                    head.acquired = vk::Semaphore::null();
                }
                if head.swapchain != vk::SwapchainKHR::null() {
                    self.swapchain_api.destroy_swapchain(head.swapchain, None);
                    head.swapchain = vk::SwapchainKHR::null();
                }
            }
        }
        // Hand sub-ownership back before the displays are released: the
        // driver's own connector disable on release is a DRM atomic commit,
        // which nvidia-drm rejects while the grant stands.
        self.revoke_sub_owner();
        unsafe {
            for head in &mut self.heads {
                if head.surface != vk::SurfaceKHR::null() {
                    self.surface_api.destroy_surface(head.surface, None);
                    head.surface = vk::SurfaceKHR::null();
                }
                if head.display != vk::DisplayKHR::null() {
                    (self.direct_api.fp().release_display_ext)(
                        self.device.physical_device,
                        head.display,
                    )
                    .result()
                    .with_context(|| {
                        format!("connector {}: release display", head.config.connector_id)
                    })?;
                    head.display = vk::DisplayKHR::null();
                }
            }
        }
        self.heads.clear();
        drop(self.card.take());
        self.closed = true;
        if let (Some(profile), Some(sample)) = (&mut self.profile, profile_sample) {
            profile.shutdown.record(sample);
            profile.report();
        }
        Ok(())
    }
}

impl DirectDisplay {
    fn revoke_sub_owner(&mut self) {
        if !self.sub_owner {
            return;
        }
        self.sub_owner = false;
        let Some(card) = &self.card else {
            eprintln!("slicer: NVKMS sub-ownership revoke skipped; DRM card already closed");
            return;
        };
        match nvkms::revoke_sub_ownership(card.as_raw_fd()) {
            Ok(()) => eprintln!("slicer: NVKMS sub-ownership revoked"),
            Err(error) => eprintln!("slicer: NVKMS sub-ownership revoke failed: {error:#}"),
        }
    }
}

impl Drop for DirectDisplay {
    fn drop(&mut self) {
        if !self.closed && self.shutdown().is_err() {
            // Keep the DRM master FD alive when a final present cannot be
            // verified. `Gpu::drop` also retains the Vulkan device in this case.
            if let Some(card) = self.card.take() {
                std::mem::forget(card);
            }
            std::mem::forget(self.device.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_thread_clock_is_reported_as_missing() {
        let mut metric = Metric::default();
        metric.record(ProfileSample {
            wall: Instant::now(),
            cpu_ns: Err(libc::ENOSYS),
        });
        assert_eq!(metric.count, 1);
        assert_eq!(metric.cpu_samples, 0);
        assert_eq!(metric.cpu_failures, 1);
        assert_eq!(metric.cpu_last_errno, Some(libc::ENOSYS));
    }

    #[test]
    fn nearest_refresh_prefers_an_exact_match() {
        // Even when a within-tolerance neighbor would also match, an exact
        // hit wins and its index is returned, not the neighbor's.
        let refreshes = [59940, 60000];
        assert_eq!(nearest_refresh_index(&refreshes, 60000), Some(1));
        assert_eq!(nearest_refresh_index(&refreshes, 59940), Some(0));
    }

    #[test]
    fn nearest_refresh_matches_within_five_millihz() {
        // Sway's 59.939 Hz against Vulkan's own 59940 mHz enumeration.
        let refreshes = [59940];
        assert_eq!(nearest_refresh_index(&refreshes, 59939), Some(0));
    }

    #[test]
    fn nearest_refresh_never_conflates_60000_with_59940() {
        let refreshes = [59940];
        assert_eq!(nearest_refresh_index(&refreshes, 60000), None);
    }

    #[test]
    fn nearest_refresh_none_when_nothing_is_close() {
        let refreshes: [u32; 0] = [];
        assert_eq!(nearest_refresh_index(&refreshes, 60000), None);
    }

    #[test]
    fn the_present_capability_refusal_is_one_clear_line() {
        let message =
            describe_missing_present_capabilities("NVIDIA RTX A1000", "550.163.01", true, true);
        assert!(message.contains("VK_EXT_present_timing"), "{message}");
        assert!(message.contains("VK_KHR_present_id2"), "{message}");
        assert!(message.contains("NVIDIA RTX A1000"), "{message}");
        assert!(message.contains("550.163.01"), "{message}");
        assert!(message.contains("exposes neither"), "{message}");
        assert!(!message.contains('\n'), "a single line: {message}");
    }

    #[test]
    fn the_refusal_names_whichever_one_capability_is_missing() {
        let timing_only = describe_missing_present_capabilities("GPU", "1.0", true, false);
        assert!(timing_only.contains("exposes VK_KHR_present_id2 but not VK_EXT_present_timing"));

        let id2_only = describe_missing_present_capabilities("GPU", "1.0", false, true);
        assert!(id2_only.contains("exposes VK_EXT_present_timing but not VK_KHR_present_id2"));
    }

    #[test]
    fn rejects_duplicate_connectors_and_zero_mode() {
        let output = DirectOutputConfig {
            name: "left".into(),
            connector_id: 1,
            width: 1920,
            height: 1200,
            refresh_millihz: 60000,
        };
        let config = DirectDisplayConfig {
            card: "/dev/dri/card0".into(),
            outputs: vec![output.clone(), output],
        };
        assert!(validate_config(&config).is_err());
        let bad = DirectDisplayConfig {
            outputs: vec![DirectOutputConfig {
                height: 0,
                ..config.outputs[0].clone()
            }],
            ..config
        };
        assert!(validate_config(&bad).is_err());
    }
}
