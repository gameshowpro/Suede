//! Gate 0 display path probe. Run `cargo run --example display_probe -- --help`.
//!
//! Enumeration opens the explicit DRM card read-only. Presentation requires
//! `--present --connectors ID[,ID...]` and an already available DRM master.
//! It does not set master, change services, perform DRM commits, or read DRM
//! events. Run only during a planned display outage.

#[cfg(not(all(feature = "projection", target_os = "linux")))]
fn main() {
    eprintln!("display_probe requires Linux and the projection feature");
    std::process::exit(2);
}

#[cfg(all(feature = "projection", target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    supported::run()
}

#[cfg(all(feature = "projection", target_os = "linux"))]
mod supported {
    use anyhow::{anyhow, bail, Context, Result};
    use ash::{vk, Entry};
    use clap::Parser;
    use std::{
        collections::HashSet,
        ffi::{c_void, CStr},
        fs::{File, OpenOptions},
        os::fd::{AsRawFd, RawFd},
        os::unix::fs::{FileTypeExt, MetadataExt},
        path::PathBuf,
        time::{Duration, Instant},
    };
    use suede::projection::gpu::display::{nvkms, timing};

    #[derive(Parser)]
    #[command(
        about = "Read-only Vulkan display inventory, or explicitly present a Gate 0 test pattern"
    )]
    struct Args {
        /// Explicit DRM primary card node, for example /dev/dri/card0.
        #[arg(long)]
        card: PathBuf,
        /// Acquire displays and present test colors. Requires --connectors.
        #[arg(long)]
        present: bool,
        /// Request VK_NV_present_barrier on every display swapchain. Requires --present.
        #[arg(long, requires = "present")]
        present_barrier: bool,
        /// Request per-display VK_EXT_present_timing feedback. Requires --present.
        #[arg(long, requires = "present")]
        present_timing: bool,
        /// Grant NVKMS sub-ownership to the NVIDIA driver's own NVKMS handle
        /// before acquiring displays, and revoke it at teardown. NVKMS only
        /// lets the modeset owner or sub-owner allocate the swap group that
        /// implements VK_NV_present_barrier; a client that acquired displays
        /// through nvidia-drm otherwise holds per-head modeset permission only.
        /// While granted, nvidia-drm rejects every DRM atomic commit, so a
        /// crashed run must be followed by --revoke-sub-owner. Requires --present.
        #[arg(long, requires = "present")]
        nvkms_sub_owner: bool,
        /// Recovery: revoke a stale NVKMS sub-ownership grant on the card and
        /// exit. Requires being the DRM master (no compositor running).
        #[arg(long, conflicts_with = "present")]
        revoke_sub_owner: bool,
        /// Comma-separated DRM connector IDs. Use `modetest -c` to identify them.
        #[arg(long, value_delimiter = ',', num_args = 1..)]
        connectors: Vec<u32>,
        /// Duration of the presentation run.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=600))]
        seconds: u64,
        /// Required visible mode width when presenting.
        #[arg(long)]
        width: Option<u32>,
        /// Required visible mode height when presenting.
        #[arg(long)]
        height: Option<u32>,
        /// Required refresh rate in millihertz when presenting (59939 = 59.939 Hz).
        #[arg(long)]
        refresh_millihz: Option<u32>,
        /// Minimum swapchain image count per display (the surface minimum
        /// still applies). Three images let vkQueuePresentKHR return before
        /// the previous flip completes, at one frame of extra latency.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(2..=4))]
        images: u32,
        /// Do not attach VK_KHR_present_id to presents. The run then ends with
        /// a device idle wait and a short sleep instead of vkWaitForPresentKHR.
        /// Conflicts with --present-timing, which needs present ids.
        #[arg(long, requires = "present", conflicts_with = "present_timing")]
        no_present_id: bool,
    }

    struct Head {
        connector: u32,
        display: vk::DisplayKHR,
        surface: vk::SurfaceKHR,
        swapchain: vk::SwapchainKHR,
        images: Vec<vk::Image>,
        commands: Vec<vk::CommandBuffer>,
        ready: Vec<vk::Semaphore>,
        acquired: vk::Semaphore,
        used: Vec<bool>,
        frames: u64,
        timing_stage: u32,
        timing_domain: i32,
        timing_domain_id: u64,
        last_timing_complete_id: u64,
        first_device_ns: Option<u64>,
        last_device_ns: Option<u64>,
    }

    struct PresentAvailability {
        barrier_extension: bool,
        barrier_feature: bool,
        timing: bool,
    }

    struct Probe {
        card: Option<File>,
        instance: ash::Instance,
        physical: vk::PhysicalDevice,
        display_api: ash::khr::display::Instance,
        acquire_api: ash::ext::acquire_drm_display::Instance,
        direct_api: ash::ext::direct_mode_display::Instance,
        surface_api: ash::khr::surface::Instance,
        surface_capabilities2_api: ash::khr::get_surface_capabilities2::Instance,
        device: Option<ash::Device>,
        swapchain_api: Option<ash::khr::swapchain::Device>,
        present_wait_api: Option<ash::khr::present_wait::Device>,
        timing_api: Option<timing::Api>,
        calibration_api: Option<timing::CalibrationApi>,
        present_attempted: bool,
        use_present_id: bool,
        presentation_complete: bool,
        last_present_id: u64,
        pool: vk::CommandPool,
        heads: Vec<Head>,
        sub_owner_granted: bool,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            if self.present_attempted && !self.presentation_complete {
                eprintln!("last presentation completion unverified; exiting without Vulkan teardown to avoid destroying resources still used by the presentation engine");
                self.revoke_sub_owner_if_granted();
                std::process::exit(3);
            }
            let teardown_start = Instant::now();
            unsafe {
                if let Some(device) = &self.device {
                    // A failed/removed driver may return an error; continue best effort.
                    eprintln!("teardown: device_wait_idle begin");
                    let idle_result = device.device_wait_idle();
                    eprintln!("teardown: device_wait_idle end: {idle_result:?}");
                    for head in &self.heads {
                        eprintln!("teardown: connector {} semaphores begin", head.connector);
                        for &semaphore in &head.ready {
                            device.destroy_semaphore(semaphore, None);
                        }
                        if head.acquired != vk::Semaphore::null() {
                            device.destroy_semaphore(head.acquired, None);
                        }
                        eprintln!("teardown: connector {} semaphores end", head.connector);
                        if head.swapchain != vk::SwapchainKHR::null() {
                            if let Some(api) = &self.swapchain_api {
                                eprintln!(
                                    "teardown: connector {} destroy_swapchain begin",
                                    head.connector
                                );
                                api.destroy_swapchain(head.swapchain, None);
                                eprintln!(
                                    "teardown: connector {} destroy_swapchain end",
                                    head.connector
                                );
                            }
                        }
                    }
                    if self.pool != vk::CommandPool::null() {
                        eprintln!("teardown: destroy_command_pool begin");
                        device.destroy_command_pool(self.pool, None);
                        eprintln!("teardown: destroy_command_pool end");
                    }
                    eprintln!("teardown: destroy_device begin");
                    device.destroy_device(None);
                    eprintln!("teardown: destroy_device end");
                }
                // Sub-ownership must be handed back before the displays are
                // released and before the DRM master FD closes: nvidia-drm
                // refuses every atomic commit (the driver's own connector
                // disable on release, the fbdev console restore at lastclose)
                // while it is granted.
                self.revoke_sub_owner_if_granted();
                let mut release_failed = false;
                for head in &self.heads {
                    if head.surface != vk::SurfaceKHR::null() {
                        eprintln!(
                            "teardown: connector {} destroy_surface begin",
                            head.connector
                        );
                        self.surface_api.destroy_surface(head.surface, None);
                        eprintln!("teardown: connector {} destroy_surface end", head.connector);
                    }
                    if head.display != vk::DisplayKHR::null() {
                        eprintln!(
                            "teardown: connector {} release_display begin",
                            head.connector
                        );
                        let result =
                            (self.direct_api.fp().release_display_ext)(self.physical, head.display)
                                .result();
                        eprintln!(
                            "teardown: connector {} release_display end: {result:?}",
                            head.connector
                        );
                        if let Err(err) = result {
                            release_failed = true;
                            eprintln!("release connector {}: {err:?}", head.connector);
                        }
                    }
                }
                // vkAcquireDrmDisplayEXT requires the DRM FD to remain open
                // until every acquired display has been released. Close it
                // while the Vulkan instance and loader are still alive.
                if !release_failed {
                    eprintln!("teardown: drop(card) begin");
                    drop(self.card.take());
                    eprintln!("teardown: drop(card) end");
                }
                eprintln!("teardown: destroy_instance begin");
                self.instance.destroy_instance(None);
                eprintln!("teardown: destroy_instance end");
                if release_failed {
                    eprintln!(
                        "teardown: display release failed; drop(card) after destroy_instance begin"
                    );
                    drop(self.card.take());
                    eprintln!(
                        "teardown: display release failed; drop(card) after destroy_instance end"
                    );
                }
            }
            eprintln!(
                "teardown: complete elapsed_ms={:.3}",
                teardown_start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    impl Probe {
        fn revoke_sub_owner_if_granted(&mut self) {
            if !self.sub_owner_granted {
                return;
            }
            self.sub_owner_granted = false;
            let Some(card) = &self.card else {
                eprintln!("teardown: sub-ownership revoke skipped; DRM card already closed");
                return;
            };
            eprintln!("teardown: revoke NVKMS sub-ownership begin");
            match nvkms::revoke_sub_ownership(card.as_raw_fd()) {
                Ok(()) => eprintln!("teardown: revoke NVKMS sub-ownership end: ok"),
                Err(err) => eprintln!("teardown: revoke NVKMS sub-ownership end: {err:#}"),
            }
        }
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();
        if args.present && args.connectors.is_empty() {
            bail!("--present requires --connectors ID[,ID...]");
        }
        if !args.present && !args.connectors.is_empty() {
            bail!("--connectors requires --present");
        }
        if args.present
            && (args.width.is_none() || args.height.is_none() || args.refresh_millihz.is_none())
        {
            bail!("--present requires --width, --height, and --refresh-millihz");
        }
        if args.present
            && (args.width == Some(0) || args.height == Some(0) || args.refresh_millihz == Some(0))
        {
            bail!("requested width, height, and refresh must be positive");
        }
        if args
            .connectors
            .iter()
            .copied()
            .collect::<HashSet<_>>()
            .len()
            != args.connectors.len()
        {
            bail!("connector IDs must be distinct");
        }
        let card = OpenOptions::new()
            .read(true)
            .write(args.present || args.revoke_sub_owner)
            .open(&args.card)
            .with_context(|| format!("opening {}", args.card.display()))?;
        let metadata = card.metadata()?;
        if !metadata.file_type().is_char_device() {
            bail!("{} is not a character device", args.card.display());
        }
        let major = libc::major(metadata.rdev()) as i64;
        let minor = libc::minor(metadata.rdev()) as i64;
        let card_fd = card.as_raw_fd();
        if args.revoke_sub_owner {
            if !nvkms::is_nvidia_drm(card_fd) {
                bail!("{} is not driven by nvidia-drm", args.card.display());
            }
            return nvkms::clear_stale_sub_ownership(card_fd).map(|revoked| {
                if revoked {
                    println!(
                        "NVKMS sub-ownership revoke accepted on {} (any earlier grant is cleared)",
                        args.card.display()
                    );
                } else {
                    println!(
                        "no NVKMS sub-ownership grant to revoke on {}",
                        args.card.display()
                    );
                }
            });
        }
        println!(
            "DRM card {} (major {major}, minor {minor}); mode={}",
            args.card.display(),
            if args.present {
                "PRESENT"
            } else {
                "enumerate only"
            }
        );

        let entry = unsafe { Entry::load().context("loading Vulkan loader")? };
        let available = unsafe { entry.enumerate_instance_extension_properties(None)? };
        for required in [
            ash::khr::surface::NAME,
            ash::khr::display::NAME,
            ash::ext::acquire_drm_display::NAME,
            ash::ext::direct_mode_display::NAME,
            ash::khr::get_surface_capabilities2::NAME,
        ] {
            if !has_extension(&available, required) {
                bail!(
                    "instance extension {} unavailable",
                    required.to_string_lossy()
                );
            }
        }
        let names = [
            ash::khr::surface::NAME.as_ptr(),
            ash::khr::display::NAME.as_ptr(),
            ash::ext::acquire_drm_display::NAME.as_ptr(),
            ash::ext::direct_mode_display::NAME.as_ptr(),
            ash::khr::get_surface_capabilities2::NAME.as_ptr(),
        ];
        let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&names);
        let instance = unsafe { entry.create_instance(&info, None)? };
        let display_api = ash::khr::display::Instance::new(&entry, &instance);
        let acquire_api = ash::ext::acquire_drm_display::Instance::new(&entry, &instance);
        let direct_api = ash::ext::direct_mode_display::Instance::new(&entry, &instance);
        let surface_api = ash::khr::surface::Instance::new(&entry, &instance);
        let surface_capabilities2_api =
            ash::khr::get_surface_capabilities2::Instance::new(&entry, &instance);
        let physical = unsafe {
        instance.enumerate_physical_devices()?.into_iter().find(|&gpu| {
            let extensions = instance.enumerate_device_extension_properties(gpu).unwrap_or_default();
            if !has_extension(&extensions, ash::ext::physical_device_drm::NAME) { return false; }
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            instance.get_physical_device_properties2(gpu, &mut props);
            drm.has_primary != 0 && drm.primary_major == major && drm.primary_minor == minor
        })
    }.ok_or_else(|| anyhow!("no Vulkan physical device reports primary DRM node {major}:{minor} via VK_EXT_physical_device_drm"))?;
        let mut probe = Probe {
            card: Some(card),
            instance,
            physical,
            display_api,
            acquire_api,
            direct_api,
            surface_api,
            surface_capabilities2_api,
            device: None,
            swapchain_api: None,
            present_wait_api: None,
            timing_api: None,
            calibration_api: None,
            present_attempted: false,
            use_present_id: true,
            presentation_complete: false,
            last_present_id: 0,
            pool: vk::CommandPool::null(),
            heads: Vec::new(),
            sub_owner_granted: false,
        };
        let props = unsafe { probe.instance.get_physical_device_properties(physical) };
        println!(
            "Vulkan GPU: {}",
            unsafe { CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy()
        );
        let device_extensions = unsafe {
            probe
                .instance
                .enumerate_device_extension_properties(physical)?
        };
        let barrier_extension = has_extension(&device_extensions, ash::nv::present_barrier::NAME);
        let mut barrier_features = vk::PhysicalDevicePresentBarrierFeaturesNV::default();
        if barrier_extension {
            let mut features =
                vk::PhysicalDeviceFeatures2::default().push_next(&mut barrier_features);
            unsafe {
                probe
                    .instance
                    .get_physical_device_features2(physical, &mut features)
            };
        }
        let barrier_feature = barrier_features.present_barrier != 0;
        println!(
            "VK_NV_present_barrier: extension {}, device feature {}",
            if barrier_extension {
                "advertised"
            } else {
                "unavailable"
            },
            if barrier_feature {
                "supported"
            } else {
                "unavailable"
            }
        );
        let timing_extensions = [
            timing::PRESENT_TIMING_NAME,
            timing::PRESENT_ID2_NAME,
            timing::CALIBRATED_TIMESTAMPS_NAME,
        ];
        let timing_extensions_available = timing_extensions
            .iter()
            .all(|name| has_extension(&device_extensions, name));
        let mut timing_features = timing::Features::default();
        let mut id2_features = timing::Id2Features::default();
        if has_extension(&device_extensions, timing::PRESENT_TIMING_NAME)
            || has_extension(&device_extensions, timing::PRESENT_ID2_NAME)
        {
            let mut features = vk::PhysicalDeviceFeatures2::default();
            if has_extension(&device_extensions, timing::PRESENT_TIMING_NAME) {
                timing_features.p_next = features.p_next;
                features.p_next = (&mut timing_features as *mut timing::Features).cast::<c_void>();
            }
            if has_extension(&device_extensions, timing::PRESENT_ID2_NAME) {
                id2_features.p_next = features.p_next;
                features.p_next = (&mut id2_features as *mut timing::Id2Features).cast::<c_void>();
            }
            unsafe {
                probe
                    .instance
                    .get_physical_device_features2(physical, &mut features)
            };
        }
        println!(
            "VK_EXT_present_timing: extension {}, feature {}; VK_KHR_present_id2: extension {}, feature {}; VK_KHR_calibrated_timestamps: extension {}",
            if has_extension(&device_extensions, timing::PRESENT_TIMING_NAME) { "advertised" } else { "unavailable" },
            if timing_features.present_timing != 0 { "supported" } else { "unavailable" },
            if has_extension(&device_extensions, timing::PRESENT_ID2_NAME) { "advertised" } else { "unavailable" },
            if id2_features.present_id2 != 0 { "supported" } else { "unavailable" },
            if has_extension(&device_extensions, timing::CALIBRATED_TIMESTAMPS_NAME) { "advertised" } else { "unavailable" },
        );
        let timing_available = timing_extensions_available
            && timing_features.present_timing != 0
            && id2_features.present_id2 != 0;
        let displays = unsafe {
            probe
                .display_api
                .get_physical_device_display_properties(physical)?
        };
        let planes = unsafe {
            probe
                .display_api
                .get_physical_device_display_plane_properties(physical)?
        };
        println!(
            "{} Vulkan displays; {} display planes",
            displays.len(),
            planes.len()
        );
        for (index, item) in displays.iter().enumerate() {
            let modes = unsafe {
                probe
                    .display_api
                    .get_display_mode_properties(physical, item.display)?
            };
            let name = if item.display_name.is_null() {
                "<unnamed>".into()
            } else {
                unsafe { CStr::from_ptr(item.display_name) }
                    .to_string_lossy()
                    .into_owned()
            };
            println!("display {index}: {name}, {} modes", modes.len());
            for mode in modes {
                let p = mode.parameters;
                println!(
                    "  {}x{} @ {:.3} Hz",
                    p.visible_region.width,
                    p.visible_region.height,
                    p.refresh_rate as f64 / 1000.0
                );
            }
        }
        if !args.present {
            return Ok(());
        }
        let result = present(
            &args,
            &entry,
            card_fd,
            &mut probe,
            &planes,
            PresentAvailability {
                barrier_extension,
                barrier_feature,
                timing: timing_available,
            },
        );
        // Drop may exit when presentation completion is unknown. Preserve the
        // actual driver failure before that conservative cleanup path runs.
        if let Err(error) = &result {
            eprintln!("presentation probe failed: {error:#}");
        }
        eprintln!("teardown: drop(probe) begin");
        drop(probe);
        eprintln!("teardown: drop(probe) end");
        // Keep the loader alive until after the DRM FD and Vulkan instance
        // have been closed and destroyed, respectively.
        eprintln!("teardown: drop(entry) begin");
        drop(entry);
        eprintln!("teardown: drop(entry) end");
        result
    }

    fn has_extension(properties: &[vk::ExtensionProperties], name: &CStr) -> bool {
        properties
            .iter()
            .any(|p| unsafe { CStr::from_ptr(p.extension_name.as_ptr()) } == name)
    }

    fn present(
        args: &Args,
        entry: &Entry,
        card_fd: RawFd,
        p: &mut Probe,
        planes: &[vk::DisplayPlanePropertiesKHR],
        availability: PresentAvailability,
    ) -> Result<()> {
        if args.present_barrier && !(availability.barrier_extension && availability.barrier_feature)
        {
            bail!("--present-barrier requires VK_NV_present_barrier and its device feature");
        }
        if args.present_timing && !availability.timing {
            bail!("--present-timing requires VK_EXT_present_timing, VK_KHR_present_id2, VK_KHR_calibrated_timestamps, and both device features");
        }
        let extensions = unsafe {
            p.instance
                .enumerate_device_extension_properties(p.physical)?
        };
        if !has_extension(&extensions, ash::khr::swapchain::NAME) {
            bail!(
                "device extension {} unavailable",
                ash::khr::swapchain::NAME.to_string_lossy()
            );
        }
        println!(
            "VK_GOOGLE_display_timing: {}",
            if has_extension(&extensions, ash::google::display_timing::NAME) {
                "advertised; no timestamp query in this probe"
            } else {
                "unavailable"
            }
        );
        let families = unsafe {
            p.instance
                .get_physical_device_queue_family_properties(p.physical)
        };
        let queue_family = families
            .iter()
            .position(|f| {
                f.queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER)
            })
            .ok_or_else(|| anyhow!("no graphics and transfer-capable queue family"))?
            as u32;

        // Resolve every requested DRM connector before touching any display.
        let mut selections = Vec::new();
        let mut used_planes = HashSet::new();
        let mut used_displays = HashSet::new();
        let display_properties = unsafe {
            p.display_api
                .get_physical_device_display_properties(p.physical)?
        };
        for &connector in &args.connectors {
            let display = unsafe {
                p.acquire_api
                    .get_drm_display(p.physical, card_fd, connector)
            }
            .with_context(|| format!("vkGetDrmDisplayEXT connector {connector}"))?;
            if display == vk::DisplayKHR::null() {
                bail!("connector {connector}: vkGetDrmDisplayEXT returned no Vulkan display");
            }
            if !used_displays.insert(display) {
                bail!(
                "connector {connector}: driver mapped multiple connector IDs to one VkDisplayKHR"
            );
            }
            let properties = display_properties.iter().find(|item| item.display == display)
                .ok_or_else(|| anyhow!("connector {connector}: mapped display is absent from Vulkan display inventory"))?;
            if !properties
                .supported_transforms
                .contains(vk::SurfaceTransformFlagsKHR::IDENTITY)
            {
                bail!("connector {connector}: display does not support identity transform");
            }
            let modes = unsafe {
                p.display_api
                    .get_display_mode_properties(p.physical, display)?
            };
            let mode = modes
            .into_iter()
            .find(|m| m.parameters.visible_region.width == args.width.unwrap()
                && m.parameters.visible_region.height == args.height.unwrap()
                && m.parameters.refresh_rate == args.refresh_millihz.unwrap())
            .ok_or_else(|| anyhow!("connector {connector}: requested {}x{} @ {} mHz mode absent; inspect read-only inventory", args.width.unwrap(), args.height.unwrap(), args.refresh_millihz.unwrap()))?;
            let mut selected = None;
            for plane in 0..planes.len() as u32 {
                if used_planes.contains(&plane) {
                    continue;
                }
                let supported = unsafe {
                    p.display_api
                        .get_display_plane_supported_displays(p.physical, plane)?
                };
                if supported.contains(&display)
                    && (planes[plane as usize].current_display == vk::DisplayKHR::null()
                        || planes[plane as usize].current_display == display)
                {
                    let caps = unsafe {
                        p.display_api.get_display_plane_capabilities(
                            p.physical,
                            mode.display_mode,
                            plane,
                        )?
                    };
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
                        selected = Some(plane);
                        break;
                    }
                }
            }
            let plane = selected.ok_or_else(|| {
                anyhow!(
                    "connector {connector}: no distinct supported opaque plane at selected mode"
                )
            })?;
            used_planes.insert(plane);
            selections.push((connector, display, mode, plane));
        }
        let priority = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priority)];
        let wait_extensions = has_extension(&extensions, ash::khr::present_wait::NAME)
            && has_extension(&extensions, ash::khr::present_id::NAME);
        let mut query_wait = vk::PhysicalDevicePresentWaitFeaturesKHR::default();
        let mut query_id = vk::PhysicalDevicePresentIdFeaturesKHR::default();
        if wait_extensions {
            let mut query = vk::PhysicalDeviceFeatures2::default()
                .push_next(&mut query_wait)
                .push_next(&mut query_id);
            unsafe {
                p.instance
                    .get_physical_device_features2(p.physical, &mut query);
            }
        }
        let present_wait =
            wait_extensions && query_wait.present_wait != 0 && query_id.present_id != 0;
        if args.present_timing && !present_wait {
            bail!("--present-timing requires VK_KHR_present_wait and VK_KHR_present_id for verified final completion");
        }
        println!(
            "VK_KHR_present_wait + present_id features: {}",
            if present_wait {
                "enabled for final-present drain"
            } else {
                "unavailable; clean presentation teardown cannot be verified"
            }
        );
        let mut device_extensions = vec![ash::khr::swapchain::NAME.as_ptr()];
        if present_wait {
            device_extensions.push(ash::khr::present_wait::NAME.as_ptr());
            device_extensions.push(ash::khr::present_id::NAME.as_ptr());
        }
        if args.present_barrier {
            device_extensions.push(ash::nv::present_barrier::NAME.as_ptr());
        }
        if args.present_timing {
            device_extensions.extend([
                timing::PRESENT_TIMING_NAME.as_ptr(),
                timing::PRESENT_ID2_NAME.as_ptr(),
                timing::CALIBRATED_TIMESTAMPS_NAME.as_ptr(),
            ]);
        }
        let mut enable_wait =
            vk::PhysicalDevicePresentWaitFeaturesKHR::default().present_wait(true);
        let mut enable_id = vk::PhysicalDevicePresentIdFeaturesKHR::default().present_id(true);
        let mut enable_barrier =
            vk::PhysicalDevicePresentBarrierFeaturesNV::default().present_barrier(true);
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&device_extensions);
        let device_info = if present_wait {
            device_info
                .push_next(&mut enable_wait)
                .push_next(&mut enable_id)
        } else {
            device_info
        };
        let device_info = if args.present_barrier {
            device_info.push_next(&mut enable_barrier)
        } else {
            device_info
        };
        let mut device_info = device_info;
        let mut enable_timing = timing::Features::default();
        let mut enable_id2 = timing::Id2Features::default();
        if args.present_timing {
            enable_timing.present_timing = vk::TRUE;
            enable_id2.present_id2 = vk::TRUE;
            enable_timing.p_next = device_info.p_next as *mut c_void;
            enable_id2.p_next = (&mut enable_timing as *mut timing::Features).cast();
            device_info.p_next = (&mut enable_id2 as *mut timing::Id2Features).cast();
        }
        let device = unsafe { p.instance.create_device(p.physical, &device_info, None)? };
        p.swapchain_api = Some(ash::khr::swapchain::Device::new(&p.instance, &device));
        if present_wait {
            p.present_wait_api = Some(ash::khr::present_wait::Device::new(&p.instance, &device));
        }
        p.device = Some(device);
        let device = p.device.as_ref().unwrap();
        if args.present_timing {
            p.timing_api = Some(unsafe { timing::Api::new(&p.instance, device)? });
            match unsafe { timing::CalibrationApi::new(entry, &p.instance, p.physical, device) } {
                Ok(api) => match unsafe { api.available_domains() } {
                    Ok(domains) => {
                        println!(
                            "calibration available_domains={}",
                            domains
                                .iter()
                                .map(|domain| format!("{}({domain})", timing::domain_name(*domain)))
                                .collect::<Vec<_>>()
                                .join(",")
                        );
                        println!(
                            "calibration timestamp_period_ns={}",
                            api.timestamp_period_ns
                        );
                        if domains.contains(&timing::DEVICE_DOMAIN)
                            && domains.contains(&timing::MONOTONIC_DOMAIN)
                        {
                            p.calibration_api = Some(api);
                        } else {
                            println!("calibration unavailable: DEVICE and CLOCK_MONOTONIC domains are both required");
                        }
                    }
                    Err(err) => {
                        eprintln!("calibration unavailable: querying domains failed: {err:#}")
                    }
                },
                Err(err) => eprintln!("calibration unavailable: initializing API failed: {err:#}"),
            }
        }
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        p.pool = unsafe { device.create_command_pool(&pool_info, None)? };

        for (connector, display, mode, plane) in selections {
            unsafe { p.acquire_api.acquire_drm_display(p.physical, card_fd, display) }
            .with_context(|| format!("vkAcquireDrmDisplayEXT connector {connector}; card must already be available as DRM master"))?;
            p.heads.push(Head {
                connector,
                display,
                surface: vk::SurfaceKHR::null(),
                swapchain: vk::SwapchainKHR::null(),
                images: vec![],
                commands: vec![],
                ready: vec![],
                acquired: vk::Semaphore::null(),
                used: vec![],
                frames: 0,
                timing_stage: 0,
                timing_domain: 0,
                timing_domain_id: 0,
                last_timing_complete_id: 0,
                first_device_ns: None,
                last_device_ns: None,
            });
            let head = p.heads.last_mut().unwrap();
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
                p.display_api
                    .create_display_plane_surface(&surface_info, None)
            }
            .with_context(|| format!("connector {connector}: creating display surface"))?;
            if availability.barrier_extension || availability.timing {
                let surface_info =
                    vk::PhysicalDeviceSurfaceInfo2KHR::default().surface(head.surface);
                let mut barrier_caps = vk::SurfaceCapabilitiesPresentBarrierNV::default();
                let mut timing_caps = timing::SurfaceCaps::default();
                let mut id2_caps = timing::Id2SurfaceCaps::default();
                let mut capabilities = vk::SurfaceCapabilities2KHR::default();
                if availability.barrier_extension {
                    capabilities = capabilities.push_next(&mut barrier_caps);
                }
                if availability.timing {
                    timing_caps.p_next = capabilities.p_next;
                    capabilities.p_next = (&mut timing_caps as *mut timing::SurfaceCaps).cast();
                    id2_caps.p_next = capabilities.p_next;
                    capabilities.p_next = (&mut id2_caps as *mut timing::Id2SurfaceCaps).cast();
                }
                unsafe {
                    p.surface_capabilities2_api
                        .get_physical_device_surface_capabilities2(
                            p.physical,
                            &surface_info,
                            &mut capabilities,
                        )
                }
                .with_context(|| {
                    format!("connector {connector}: querying display surface capabilities2")
                })?;
                if availability.barrier_extension {
                    let supported = barrier_caps.present_barrier_supported != 0;
                    println!(
                        "connector {connector}: VK_NV_present_barrier surface support: {}",
                        if supported {
                            "supported"
                        } else {
                            "unavailable"
                        }
                    );
                    if args.present_barrier && !supported {
                        bail!(
                            "connector {connector}: present barrier unsupported on display surface"
                        );
                    }
                } else {
                    println!("connector {connector}: VK_NV_present_barrier surface support: unavailable (device extension missing)");
                }
                if availability.timing {
                    println!(
                        "connector {connector}: present timing surface={}, present_id2 surface={}, stages=0x{:x}",
                        timing_caps.present_timing_supported != 0,
                        id2_caps.present_id2_supported != 0,
                        timing_caps.present_stage_queries,
                    );
                    if args.present_timing {
                        if timing_caps.present_timing_supported == 0
                            || id2_caps.present_id2_supported == 0
                        {
                            bail!("connector {connector}: present timing or present_id2 unsupported on display surface");
                        }
                        head.timing_stage = timing_caps.preferred_stage().ok_or_else(|| {
                            anyhow!("connector {connector}: no present timing stage available")
                        })?;
                        println!(
                            "connector {connector}: selected timing stage {}{}",
                            timing::stage_name(head.timing_stage),
                            if head.timing_stage == timing::STAGE_FIRST_PIXEL_VISIBLE
                                || head.timing_stage == timing::STAGE_FIRST_PIXEL_OUT
                            {
                                ""
                            } else {
                                " (not a scanout measurement)"
                            }
                        );
                    }
                }
            } else {
                println!("connector {connector}: VK_NV_present_barrier surface support: unavailable (device extension missing)");
                println!("connector {connector}: present timing surface support: unavailable (device extensions or features missing)");
            }
            if !unsafe {
                p.surface_api.get_physical_device_surface_support(
                    p.physical,
                    queue_family,
                    head.surface,
                )?
            } {
                bail!("connector {connector}: transfer queue family {queue_family} cannot present to display surface");
            }
            let caps = unsafe {
                p.surface_api
                    .get_physical_device_surface_capabilities(p.physical, head.surface)?
            };
            let formats = unsafe {
                p.surface_api
                    .get_physical_device_surface_formats(p.physical, head.surface)?
            };
            let format = formats
                .into_iter()
                .find(|f| {
                    f.format != vk::Format::UNDEFINED
                        && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                })
                .ok_or_else(|| {
                    anyhow!("connector {connector}: no SRGB_NONLINEAR surface format")
                })?;
            let present_modes = unsafe {
                p.surface_api
                    .get_physical_device_surface_present_modes(p.physical, head.surface)?
            };
            if !present_modes.contains(&vk::PresentModeKHR::FIFO) {
                bail!("connector {connector}: FIFO present mode missing");
            }
            if !caps
                .supported_usage_flags
                .contains(vk::ImageUsageFlags::TRANSFER_DST)
            {
                bail!("connector {connector}: swapchain cannot be transfer destination");
            }
            if !caps
                .supported_transforms
                .contains(vk::SurfaceTransformFlagsKHR::IDENTITY)
            {
                bail!("connector {connector}: identity transform unsupported");
            }
            if !caps
                .supported_composite_alpha
                .contains(vk::CompositeAlphaFlagsKHR::OPAQUE)
            {
                bail!("connector {connector}: opaque composite alpha unsupported");
            }
            let image_count = caps.min_image_count.max(args.images);
            if caps.max_image_count != 0 && image_count > caps.max_image_count {
                bail!("connector {connector}: fewer than two swapchain images allowed");
            }
            let extent = if caps.current_extent.width == u32::MAX {
                size
            } else {
                caps.current_extent
            };
            let mut barrier_info =
                vk::SwapchainPresentBarrierCreateInfoNV::default().present_barrier_enable(true);
            let mut swapchain_flags = vk::SwapchainCreateFlagsKHR::empty();
            if args.present_timing {
                swapchain_flags |= timing::SWAPCHAIN_TIMING_FLAG | timing::SWAPCHAIN_ID2_FLAG;
            }
            let info = vk::SwapchainCreateInfoKHR::default()
                .flags(swapchain_flags)
                .surface(head.surface)
                .min_image_count(image_count)
                .image_format(format.format)
                .image_color_space(format.color_space)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
                .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                .present_mode(vk::PresentModeKHR::FIFO)
                .clipped(true);
            let info = if args.present_barrier {
                info.push_next(&mut barrier_info)
            } else {
                info
            };
            let swap = p.swapchain_api.as_ref().unwrap();
            head.swapchain = unsafe { swap.create_swapchain(&info, None) }
                .with_context(|| format!("connector {connector}: creating FIFO swapchain"))?;
            head.images = unsafe { swap.get_swapchain_images(head.swapchain)? };
            if let Some(api) = &p.timing_api {
                let queue_size = unsafe { api.set_queue_size(head.swapchain, head.images.len()) }
                    .with_context(|| {
                    format!("connector {connector}: setting present timing queue size")
                })?;
                let domains = unsafe { api.time_domains(head.swapchain) }.with_context(|| {
                    format!("connector {connector}: enumerating present time domains")
                })?;
                println!(
                    "connector {connector}: present timing queue size {queue_size}; domains: {}",
                    domains
                        .iter()
                        .map(|(domain, id)| format!(
                            "{}({domain}) id={id}",
                            timing::domain_name(*domain)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let (domain, domain_id) = domains
                    .iter()
                    .copied()
                    .find(|(domain, _)| *domain == timing::MONOTONIC_DOMAIN)
                    .or_else(|| domains.first().copied())
                    .ok_or_else(|| {
                        anyhow!("connector {connector}: no present time domain available")
                    })?;
                head.timing_domain = domain;
                head.timing_domain_id = domain_id;
                if domain != timing::MONOTONIC_DOMAIN {
                    println!("connector {connector}: CLOCK_MONOTONIC unavailable; timestamps cannot be directly compared across outputs");
                }
            }
            let allocate = vk::CommandBufferAllocateInfo::default()
                .command_pool(p.pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(head.images.len() as u32);
            head.commands = unsafe { device.allocate_command_buffers(&allocate)? };
            head.acquired =
                unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
            for _ in &head.images {
                head.ready.push(unsafe {
                    device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?
                });
            }
            head.used = vec![false; head.images.len()];
            println!(
                "connector {connector}: plane {plane}, {}x{} @ {:.3} Hz, {} images",
                extent.width,
                extent.height,
                mode.parameters.refresh_rate as f64 / 1000.0,
                head.images.len()
            );
        }
        p.use_present_id = !args.no_present_id;
        if args.nvkms_sub_owner {
            // Grant only after every display is acquired: with full
            // permissions already held, vkAcquireDrmDisplayEXT fails with
            // VK_ERROR_INITIALIZATION_FAILED. Nothing has set a mode yet, so
            // nvidia-drm's disable-all on grant costs nothing.
            let fd = nvkms::grant_sub_ownership_to_driver(card_fd)
                .context("--nvkms-sub-owner: granting NVKMS sub-ownership to the driver")?;
            p.sub_owner_granted = true;
            println!("NVKMS sub-ownership granted to the NVIDIA driver (fd {fd}); nvidia-drm atomic commits are suspended until teardown");
            println!("NVKMS sub-ownership acquired by driver fd {fd}");
        }
        run_frames(p, queue_family, Duration::from_secs(args.seconds))
    }

    fn run_frames(p: &mut Probe, family: u32, duration: Duration) -> Result<()> {
        let device = p.device.as_ref().unwrap();
        let swap = p.swapchain_api.as_ref().unwrap();
        let queue = unsafe { device.get_device_queue(family, 0) };
        let mut calibration_samples = Vec::new();
        let mut calibration_misses = 0_u32;
        if let Some(api) = &p.calibration_api {
            collect_calibration_sample(
                api,
                "start",
                &mut calibration_samples,
                &mut calibration_misses,
            );
        }
        let start = Instant::now();
        let mut next_calibration = start + Duration::from_millis(250);
        let mut batches = 0_u64;
        while start.elapsed() < duration {
            let mut indices = Vec::with_capacity(p.heads.len());
            for head in &p.heads {
                let (index, suboptimal) = unsafe {
                    swap.acquire_next_image(
                        head.swapchain,
                        2_000_000_000,
                        head.acquired,
                        vk::Fence::null(),
                    )
                }
                .with_context(|| {
                    format!(
                        "connector {}: image acquire timed out or failed",
                        head.connector
                    )
                })?;
                if suboptimal {
                    bail!("connector {}: swapchain suboptimal", head.connector);
                }
                indices.push(index);
            }
            // A fence ensures the acquisition semaphore can be reused next frame.
            // Each render-complete semaphore belongs to its image and is only
            // reused after that image is reacquired from the presentation engine.
            let mut submits = Vec::with_capacity(p.heads.len());
            let mut stages = Vec::with_capacity(p.heads.len());
            for (head, &index) in p.heads.iter_mut().zip(&indices) {
                record_clear(device, head, index as usize, batches)?;
                stages.push([vk::PipelineStageFlags::TRANSFER]);
            }
            for ((head, &index), stage) in p.heads.iter().zip(&indices).zip(&stages) {
                submits.push(
                    vk::SubmitInfo::default()
                        .wait_semaphores(std::slice::from_ref(&head.acquired))
                        .wait_dst_stage_mask(stage)
                        .command_buffers(std::slice::from_ref(&head.commands[index as usize]))
                        .signal_semaphores(std::slice::from_ref(&head.ready[index as usize])),
                );
            }
            let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None)? };
            let submit_result = unsafe { device.queue_submit(queue, &submits, fence) };
            if let Err(err) = submit_result {
                unsafe { device.destroy_fence(fence, None) };
                return Err(err.into());
            }
            let wait_result = unsafe { device.wait_for_fences(&[fence], true, 2_000_000_000) };
            if wait_result.is_err() {
                // Do not destroy an in-flight fence or resources on a GPU hang.
                eprintln!("GPU fence wait failed: {wait_result:?}; process exit may not release driver resources cleanly");
                std::process::abort();
            }
            unsafe { device.destroy_fence(fence, None) };
            let swapchains: Vec<_> = p.heads.iter().map(|h| h.swapchain).collect();
            let semaphores: Vec<_> = p
                .heads
                .iter()
                .zip(&indices)
                .map(|(h, &i)| h.ready[i as usize])
                .collect();
            let mut per_swapchain = vec![vk::Result::SUCCESS; p.heads.len()];
            let ids = vec![batches + 1; p.heads.len()];
            let mut present_ids = vk::PresentIdKHR::default().present_ids(&ids);
            let info = vk::PresentInfoKHR::default()
                .wait_semaphores(&semaphores)
                .swapchains(&swapchains)
                .image_indices(&indices)
                .results(&mut per_swapchain);
            let info = if p.present_wait_api.is_some() && p.use_present_id {
                info.push_next(&mut present_ids)
            } else {
                info
            };
            let timing_infos: Vec<_> = if p.timing_api.is_some() {
                p.heads
                    .iter()
                    .map(|head| {
                        timing::PresentTimingInfo::query(head.timing_stage, head.timing_domain_id)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let mut timing_request = timing::PresentTimingsInfo::new(&timing_infos);
            let mut id2_request = timing::PresentId2::new(&ids);
            let mut info = info;
            if p.timing_api.is_some() {
                timing_request.p_next = info.p_next;
                id2_request.p_next = (&timing_request as *const timing::PresentTimingsInfo).cast();
                info.p_next = (&id2_request as *const timing::PresentId2).cast();
            }
            p.present_attempted = true;
            let overall = unsafe { swap.queue_present(queue, &info) };
            for (head, result) in p.heads.iter().zip(&per_swapchain) {
                if *result != vk::Result::SUCCESS {
                    eprintln!(
                        "connector {}: vkQueuePresentKHR pResults={result:?}",
                        head.connector
                    );
                }
            }
            if overall == Err(timing::QUEUE_FULL) {
                bail!("VK_EXT_present_timing feedback queue is full; the probe could not drain timing results fast enough");
            }
            let suboptimal = overall.context("batched vkQueuePresentKHR")?;
            if suboptimal || per_swapchain.iter().any(|r| *r != vk::Result::SUCCESS) {
                bail!("one or more swapchains reported a present error or suboptimal result");
            }
            p.last_present_id = batches + 1;
            for head in &mut p.heads {
                head.frames += 1;
            }
            batches += 1;
            if let Some(api) = &p.timing_api {
                log_timing_feedback(api, &mut p.heads)?;
            }
            if let Some(api) = &p.calibration_api {
                if Instant::now() >= next_calibration {
                    // The run is bounded to 600 seconds: at most 2400 periodic
                    // samples plus its start and end samples.
                    if calibration_samples.len() < 2401 {
                        collect_calibration_sample(
                            api,
                            "periodic",
                            &mut calibration_samples,
                            &mut calibration_misses,
                        );
                    }
                    next_calibration = Instant::now() + Duration::from_millis(250);
                }
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        println!(
            "submitted {batches} batched present calls in {elapsed:.3} s ({:.3} batches/s)",
            batches as f64 / elapsed
        );
        for head in &p.heads {
            println!(
                "connector {}: {} present submissions, {:.3} submissions/s",
                head.connector,
                head.frames,
                head.frames as f64 / elapsed
            );
        }
        println!("These are host submission rates, not actual scanout timestamps or proof of simultaneous vblank. Check the wall and external capture; record recovery separately after normal exit and forced kill.");
        if !p.use_present_id {
            // Without present ids there is nothing to wait on by identity;
            // drain the queue and give the last flip a few refreshes.
            unsafe { p.device.as_ref().unwrap().device_wait_idle() }?;
            std::thread::sleep(Duration::from_millis(100));
            p.presentation_complete = true;
            println!("final present drained without present ids (device idle plus 100 ms; no scanout timestamp)");
        } else if let Some(wait) = &p.present_wait_api {
            for head in &p.heads {
                if let Err(err) = unsafe {
                    wait.wait_for_present(head.swapchain, p.last_present_id, 5_000_000_000)
                } {
                    eprintln!(
                        "connector {}: final present {} completion wait failed: {err:?}",
                        head.connector, p.last_present_id
                    );
                    bail!("final presentation completion unverified");
                }
            }
            p.presentation_complete = true;
            println!("final present completed on every connector before Vulkan teardown (host wait; no scanout timestamp)");
            if let Some(timing_api) = p.timing_api.as_ref() {
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    log_timing_feedback(timing_api, &mut p.heads)?;
                    if p.heads
                        .iter()
                        .all(|head| head.last_timing_complete_id >= p.last_present_id)
                    {
                        println!("final present timing feedback complete on every connector");
                        break;
                    }
                    if Instant::now() >= deadline {
                        let missing = p
                            .heads
                            .iter()
                            .filter(|head| head.last_timing_complete_id < p.last_present_id)
                            .map(|head| head.connector.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        eprintln!("final present timing feedback incomplete: present_id={} missing connectors=[{}]; timestamp series is incomplete", p.last_present_id, missing);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            if let Some(api) = &p.calibration_api {
                collect_calibration_sample(
                    api,
                    "end",
                    &mut calibration_samples,
                    &mut calibration_misses,
                );
            }
            report_calibration(p, &calibration_samples, calibration_misses);
        } else {
            eprintln!("no present_wait/present_id feature; final present completion cannot be verified, so Vulkan teardown will be skipped");
        }
        Ok(())
    }

    fn log_timing_feedback(api: &timing::Api, heads: &mut [Head]) -> Result<()> {
        for head in heads {
            for feedback in unsafe { api.poll(head.swapchain) }
                .with_context(|| format!("connector {}: polling present timing", head.connector))?
            {
                if feedback.complete {
                    head.last_timing_complete_id =
                        head.last_timing_complete_id.max(feedback.present_id);
                    if feedback.time_domain == timing::DEVICE_DOMAIN && feedback.time_ns != 0 {
                        head.first_device_ns = Some(
                            head.first_device_ns
                                .map_or(feedback.time_ns, |value| value.min(feedback.time_ns)),
                        );
                        head.last_device_ns = Some(
                            head.last_device_ns
                                .map_or(feedback.time_ns, |value| value.max(feedback.time_ns)),
                        );
                    }
                }
                println!(
                        "timing connector={} present_id={} stage={}({:#x}) time_ns={} domain={}({}) domain_id={} complete={} comparable_monotonic_ns={:?}",
                        head.connector,
                        feedback.present_id,
                        timing::stage_name(feedback.stage),
                        feedback.stage,
                        feedback.time_ns,
                        timing::domain_name(feedback.time_domain),
                        feedback.time_domain,
                        feedback.time_domain_id,
                        feedback.complete,
                        feedback.common_monotonic_ns(),
                );
            }
        }
        Ok(())
    }

    fn collect_calibration_sample(
        api: &timing::CalibrationApi,
        position: &str,
        samples: &mut Vec<timing::CalibrationSample>,
        misses: &mut u32,
    ) {
        match unsafe { api.sample_device_monotonic() } {
            Ok(sample) => {
                println!(
                    "calibration position={} device_ticks={} monotonic_ns={} max_deviation_ns={} timestamp_period_ns={}",
                    position, sample.device_ticks, sample.monotonic_ns, sample.max_deviation_ns, api.timestamp_period_ns,
                );
                samples.push(sample);
            }
            Err(err) => {
                *misses += 1;
                eprintln!("calibration position={position} missing: {err:#}; raw present timing remains available");
            }
        }
    }

    fn report_calibration(p: &Probe, samples: &[timing::CalibrationSample], misses: u32) {
        if p.timing_api.is_some() {
            println!("calibration samples={} missing={misses}", samples.len());
        }
        if samples.len() < 2 {
            if p.timing_api.is_some() {
                println!("calibration mapping=unavailable reason=fewer_than_two_samples; raw present timing records remain available");
            }
            return;
        }
        let (Some(api), Some(&start), Some(&end)) =
            (&p.calibration_api, samples.first(), samples.last())
        else {
            if p.timing_api.is_some() {
                println!("calibration mapping=unavailable reason=missing_api_or_sample; raw present timing records remain available");
            }
            return;
        };
        let device_span_ticks = end.device_ticks.wrapping_sub(start.device_ticks);
        let host_span_ns = end.monotonic_ns.checked_sub(start.monotonic_ns);
        if let Some(host_span_ns) = host_span_ns {
            if device_span_ticks <= i64::MAX as u64 {
                let device_span_ns = device_span_ticks as f64 * api.timestamp_period_ns as f64;
                println!(
                    "calibration overall_interval diagnostic_only=true device_ticks={} device_scaled_ns={:.3} monotonic_ns={} mismatch_ns={:.3} max_deviation_ns={}",
                    device_span_ticks,
                    device_span_ns,
                    host_span_ns,
                    (device_span_ns - host_span_ns as f64).abs(),
                    start.max_deviation_ns.max(end.max_deviation_ns),
                );
            } else {
                println!("calibration overall_interval diagnostic_only=true mismatch=unavailable reason=device_counter_span_ambiguous");
            }
        } else {
            println!("calibration overall_interval diagnostic_only=true mismatch=unavailable reason=monotonic_counter_reversed");
        }
        for head in &p.heads {
            for (boundary, value) in [
                ("first_min", head.first_device_ns),
                ("last_max", head.last_device_ns),
            ] {
                let mapped = value.and_then(|device_ns| {
                    samples
                        .windows(2)
                        .enumerate()
                        .find_map(|(pair_index, window)| {
                            api.pair(window[0], window[1])
                                .estimate_present_device_ns(device_ns)
                                .map(|estimate| (device_ns, pair_index, estimate))
                        })
                });
                match mapped {
                    Some((device_ns, pair_index, estimate)) => println!(
                        "calibration connector={} boundary={} device_ns={} monotonic_ns={} max_deviation_ns={} interval_mismatch_ns={:.3} pair_index={} mapping=available",
                        head.connector, boundary, device_ns, estimate.monotonic_ns, estimate.max_deviation_ns, estimate.interval_mismatch_ns, pair_index,
                    ),
                    None => println!(
                        "calibration connector={} boundary={} device_ns={:?} mapping=unavailable reason=missing_or_not_bracketed_by_valid_adjacent_samples_or_ambiguous_period",
                        head.connector, boundary, value,
                    ),
                }
            }
        }
    }

    fn record_clear(device: &ash::Device, head: &mut Head, index: usize, frame: u64) -> Result<()> {
        let command = head.commands[index];
        unsafe {
            device.reset_command_buffer(command, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            let old = if head.used[index] {
                vk::ImageLayout::PRESENT_SRC_KHR
            } else {
                vk::ImageLayout::UNDEFINED
            };
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let before = vk::ImageMemoryBarrier::default()
                .old_layout(old)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(head.images[index])
                .subresource_range(range)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[before],
            );
            let phase = ((frame / 15 + head.connector as u64) % 6) as usize;
            let palette = [
                [1.0, 0.0, 0.0, 1.0],
                [0.0, 1.0, 0.0, 1.0],
                [0.0, 0.0, 1.0, 1.0],
                [1.0, 1.0, 0.0, 1.0],
                [1.0, 0.0, 1.0, 1.0],
                [0.0, 1.0, 1.0, 1.0],
            ];
            device.cmd_clear_color_image(
                command,
                head.images[index],
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: palette[phase],
                },
                &[range],
            );
            let after = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(head.images[index])
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[after],
            );
            device.end_command_buffer(command)?;
        }
        head.used[index] = true;
        Ok(())
    }
}
