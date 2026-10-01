//! Small FFI shim for VK_EXT_present_timing and VK_KHR_present_id2.
//!
//! Source: Khronos Vulkan-Headers v1.4.351 `include/vulkan/vulkan_core.h`,
//! extension versions VK_EXT_present_timing 3 and VK_KHR_present_id2 1:
//! https://github.com/KhronosGroup/Vulkan-Headers/blob/v1.4.351/include/vulkan/vulkan_core.h
//! Keep these definitions aligned with that header until ash exposes them.
//!
//! Integration order: enumerate both device extensions, chain `Features` and
//! `Id2Features` through PhysicalDeviceFeatures2 and DeviceCreateInfo,
//! query `SurfaceCaps` and `Id2SurfaceCaps` through SurfaceCapabilities2,
//! set both swapchain flags, create the swapchains, then call `Api::new`,
//! `set_queue_size`, and `time_domains`. Chain `PresentTimingsInfo` and
//! `PresentId2` into each PresentInfoKHR, poll every swapchain every batch,
//! and drain once more before reporting. All pNext objects and their arrays
//! must remain alive through the Vulkan call. Existing VkPresentIdKHR can
//! coexist in that chain when it carries the same IDs; the namespace is shared.

use ash::{vk, Device, Entry, Instance};
use std::{
    ffi::{c_void, CStr},
    ptr,
};

pub const PRESENT_TIMING_NAME: &CStr = c"VK_EXT_present_timing";
pub const PRESENT_ID2_NAME: &CStr = c"VK_KHR_present_id2";
pub const CALIBRATED_TIMESTAMPS_NAME: &CStr = c"VK_KHR_calibrated_timestamps";
pub const SWAPCHAIN_TIMING_FLAG: vk::SwapchainCreateFlagsKHR =
    vk::SwapchainCreateFlagsKHR::from_raw(0x0000_0200);
pub const SWAPCHAIN_ID2_FLAG: vk::SwapchainCreateFlagsKHR =
    vk::SwapchainCreateFlagsKHR::from_raw(0x0000_0040);
pub const STAGE_QUEUE_END: u32 = 0x1;
pub const STAGE_DEQUEUED: u32 = 0x2;
pub const STAGE_FIRST_PIXEL_OUT: u32 = 0x4;
pub const STAGE_FIRST_PIXEL_VISIBLE: u32 = 0x8;
pub const MONOTONIC_DOMAIN: i32 = 1;
pub const DEVICE_DOMAIN: i32 = 0;
pub const QUEUE_FULL: vk::Result = vk::Result::from_raw(-1000208000);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Features {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub present_timing: vk::Bool32,
    pub present_at_absolute_time: vk::Bool32,
    pub present_at_relative_time: vk::Bool32,
}
impl Default for Features {
    fn default() -> Self {
        Self {
            s_type: vk::StructureType::from_raw(1000208000),
            p_next: ptr::null_mut(),
            present_timing: 0,
            present_at_absolute_time: 0,
            present_at_relative_time: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Id2Features {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub present_id2: vk::Bool32,
}
impl Default for Id2Features {
    fn default() -> Self {
        Self {
            s_type: vk::StructureType::from_raw(1000479002),
            p_next: ptr::null_mut(),
            present_id2: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SurfaceCaps {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub present_timing_supported: vk::Bool32,
    pub present_at_absolute_time_supported: vk::Bool32,
    pub present_at_relative_time_supported: vk::Bool32,
    pub present_stage_queries: u32,
}
impl Default for SurfaceCaps {
    fn default() -> Self {
        Self {
            s_type: vk::StructureType::from_raw(1000208008),
            p_next: ptr::null_mut(),
            present_timing_supported: 0,
            present_at_absolute_time_supported: 0,
            present_at_relative_time_supported: 0,
            present_stage_queries: 0,
        }
    }
}
impl SurfaceCaps {
    pub fn preferred_stage(&self) -> Option<u32> {
        [
            STAGE_FIRST_PIXEL_VISIBLE,
            STAGE_FIRST_PIXEL_OUT,
            STAGE_DEQUEUED,
            STAGE_QUEUE_END,
        ]
        .into_iter()
        .find(|bit| self.present_stage_queries & bit != 0)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Id2SurfaceCaps {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub present_id2_supported: vk::Bool32,
}
impl Default for Id2SurfaceCaps {
    fn default() -> Self {
        Self {
            s_type: vk::StructureType::from_raw(1000479000),
            p_next: ptr::null_mut(),
            present_id2_supported: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PresentTimingInfo {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub flags: u32,
    pub target_time: u64,
    pub time_domain_id: u64,
    pub present_stage_queries: u32,
    pub target_time_domain_present_stage: u32,
}
impl PresentTimingInfo {
    pub fn query(stage: u32, domain_id: u64) -> Self {
        Self {
            s_type: vk::StructureType::from_raw(1000208004),
            p_next: ptr::null(),
            flags: 0,
            target_time: 0,
            time_domain_id: domain_id,
            present_stage_queries: stage,
            target_time_domain_present_stage: 0,
        }
    }
}

#[repr(C)]
pub struct PresentTimingsInfo {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub swapchain_count: u32,
    pub p_timing_infos: *const PresentTimingInfo,
}
impl PresentTimingsInfo {
    pub fn new(infos: &[PresentTimingInfo]) -> Self {
        Self {
            s_type: vk::StructureType::from_raw(1000208003),
            p_next: ptr::null(),
            swapchain_count: infos.len() as u32,
            p_timing_infos: infos.as_ptr(),
        }
    }
}

#[repr(C)]
pub struct PresentId2 {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub swapchain_count: u32,
    pub p_present_ids: *const u64,
}
impl PresentId2 {
    pub fn new(ids: &[u64]) -> Self {
        Self {
            s_type: vk::StructureType::from_raw(1000479001),
            p_next: ptr::null(),
            swapchain_count: ids.len() as u32,
            p_present_ids: ids.as_ptr(),
        }
    }
}

#[repr(C)]
struct TimeDomainProperties {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    count: u32,
    domains: *mut i32,
    ids: *mut u64,
}
impl Default for TimeDomainProperties {
    fn default() -> Self {
        Self {
            s_type: vk::StructureType::from_raw(1000208002),
            p_next: ptr::null_mut(),
            count: 0,
            domains: ptr::null_mut(),
            ids: ptr::null_mut(),
        }
    }
}

#[repr(C)]
struct PastInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    flags: u32,
    swapchain: vk::SwapchainKHR,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct StageTime {
    stage: u32,
    time: u64,
}
#[repr(C)]
struct PastTiming {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    present_id: u64,
    target_time: u64,
    stage_count: u32,
    stages: *mut StageTime,
    time_domain: i32,
    time_domain_id: u64,
    complete: vk::Bool32,
}
#[repr(C)]
struct PastProperties {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    timing_counter: u64,
    domains_counter: u64,
    count: u32,
    timings: *mut PastTiming,
}

#[derive(Debug, Clone, Copy)]
pub struct Feedback {
    pub present_id: u64,
    pub stage: u32,
    pub time_ns: u64,
    pub time_domain: i32,
    pub time_domain_id: u64,
    pub complete: bool,
}
impl Feedback {
    /// Only CLOCK_MONOTONIC values are directly comparable across swapchains.
    pub fn common_monotonic_ns(&self) -> Option<u64> {
        (self.complete && self.time_domain == MONOTONIC_DOMAIN && self.time_ns != 0)
            .then_some(self.time_ns)
    }
}

type SetQueueSize = unsafe extern "system" fn(vk::Device, vk::SwapchainKHR, u32) -> vk::Result;
type GetDomains = unsafe extern "system" fn(
    vk::Device,
    vk::SwapchainKHR,
    *mut TimeDomainProperties,
    *mut u64,
) -> vk::Result;
type GetPast =
    unsafe extern "system" fn(vk::Device, *const PastInfo, *mut PastProperties) -> vk::Result;

pub struct Api {
    device: vk::Device,
    set_queue_size_fn: SetQueueSize,
    get_domains_fn: GetDomains,
    get_past_fn: GetPast,
}
impl Api {
    /// Call only after enabling VK_EXT_present_timing on this device.
    ///
    /// # Safety
    /// The extension must be enabled on `device`, which must belong to
    /// `instance`. Both Vulkan objects must outlive this API and its calls.
    pub unsafe fn new(instance: &Instance, device: &Device) -> anyhow::Result<Self> {
        let load = |name: &CStr| -> anyhow::Result<unsafe extern "system" fn()> {
            instance
                .get_device_proc_addr(device.handle(), name.as_ptr())
                .ok_or_else(|| anyhow::anyhow!("{} unavailable", name.to_string_lossy()))
        };
        Ok(Self {
            device: device.handle(),
            set_queue_size_fn: std::mem::transmute::<unsafe extern "system" fn(), SetQueueSize>(
                load(c"vkSetSwapchainPresentTimingQueueSizeEXT")?,
            ),
            get_domains_fn: std::mem::transmute::<unsafe extern "system" fn(), GetDomains>(load(
                c"vkGetSwapchainTimeDomainPropertiesEXT",
            )?),
            get_past_fn: std::mem::transmute::<unsafe extern "system" fn(), GetPast>(load(
                c"vkGetPastPresentationTimingEXT",
            )?),
        })
    }

    /// # Safety
    /// `swapchain` must be a live swapchain owned by this API's device and
    /// created with the present-timing flag.
    pub unsafe fn set_queue_size(
        &self,
        swapchain: vk::SwapchainKHR,
        image_count: usize,
    ) -> anyhow::Result<u32> {
        // Enough room for feedback latency; keep polling every batch to free slots.
        let size = (image_count.saturating_mul(16)).clamp(64, 256) as u32;
        (self.set_queue_size_fn)(self.device, swapchain, size).result()?;
        Ok(size)
    }

    /// # Safety
    /// `swapchain` must be a live swapchain owned by this API's device and
    /// created with the present-timing flag.
    pub unsafe fn time_domains(
        &self,
        swapchain: vk::SwapchainKHR,
    ) -> anyhow::Result<Vec<(i32, u64)>> {
        let mut props = TimeDomainProperties::default();
        (self.get_domains_fn)(self.device, swapchain, &mut props, ptr::null_mut()).result()?;
        anyhow::ensure!(props.count <= 64, "unreasonable present time domain count");
        let mut domains = vec![0i32; props.count as usize];
        let mut ids = vec![0u64; props.count as usize];
        props.domains = domains.as_mut_ptr();
        props.ids = ids.as_mut_ptr();
        (self.get_domains_fn)(self.device, swapchain, &mut props, ptr::null_mut()).result()?;
        anyhow::ensure!(
            props.count as usize <= domains.len(),
            "present time domains changed during query"
        );
        Ok(domains
            .into_iter()
            .zip(ids)
            .take(props.count as usize)
            .collect())
    }

    /// Poll after each batch. Complete records release their slots in the driver.
    /// The fixed 256-record bound matches `set_queue_size`'s maximum.
    ///
    /// # Safety
    /// `swapchain` must be a live swapchain owned by this API's device and
    /// created with the present-timing flag.
    pub unsafe fn poll(&self, swapchain: vk::SwapchainKHR) -> anyhow::Result<Vec<Feedback>> {
        let info = PastInfo {
            s_type: vk::StructureType::from_raw(1000208005),
            p_next: ptr::null(),
            flags: 0,
            swapchain,
        };
        let mut stages: Vec<[StageTime; 4]> = (0..256)
            .map(|_| [StageTime { stage: 0, time: 0 }; 4])
            .collect();
        let mut timings: Vec<PastTiming> = stages
            .iter_mut()
            .map(|slots| PastTiming {
                s_type: vk::StructureType::from_raw(1000208007),
                p_next: ptr::null_mut(),
                present_id: 0,
                target_time: 0,
                stage_count: 4,
                stages: slots.as_mut_ptr(),
                time_domain: 0,
                time_domain_id: 0,
                complete: 0,
            })
            .collect();
        let mut props = PastProperties {
            s_type: vk::StructureType::from_raw(1000208006),
            p_next: ptr::null_mut(),
            timing_counter: 0,
            domains_counter: 0,
            count: timings.len() as u32,
            timings: timings.as_mut_ptr(),
        };
        let result = (self.get_past_fn)(self.device, &info, &mut props);
        anyhow::ensure!(
            result == vk::Result::SUCCESS,
            "vkGetPastPresentationTimingEXT: {result:?}"
        );
        anyhow::ensure!(
            props.count as usize <= timings.len(),
            "present timing result overflow"
        );
        let mut out = Vec::new();
        for timing in timings.iter().take(props.count as usize) {
            anyhow::ensure!(timing.stage_count <= 4, "present timing stage overflow");
            let slots = std::slice::from_raw_parts(timing.stages, timing.stage_count as usize);
            for slot in slots {
                out.push(Feedback {
                    present_id: timing.present_id,
                    stage: slot.stage,
                    time_ns: slot.time,
                    time_domain: timing.time_domain,
                    time_domain_id: timing.time_domain_id,
                    complete: timing.complete != 0,
                });
            }
        }
        Ok(out)
    }
}

/// One simultaneous device/host timestamp calibration. DEVICE values are
/// device ticks; CLOCK_MONOTONIC values and max_deviation_ns are nanoseconds.
#[derive(Debug, Clone, Copy)]
pub struct CalibrationSample {
    pub device_ticks: u64,
    pub monotonic_ns: u64,
    pub max_deviation_ns: u64,
}

/// Two samples bracketing a presentation interval. The conversion uses the
/// physical device's timestampPeriod and checks the observed clock rate.
#[derive(Debug, Clone, Copy)]
pub struct CalibrationPair {
    pub start: CalibrationSample,
    pub end: CalibrationSample,
    pub timestamp_period_ns: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct CalibratedTime {
    pub monotonic_ns: u64,
    /// Largest Vulkan-reported sample deviation, in nanoseconds.
    pub max_deviation_ns: u64,
    /// Difference between measured CLOCK_MONOTONIC interval and DEVICE
    /// interval scaled by timestampPeriod, in nanoseconds. This is a measured
    /// consistency check, not a formal error bound between the samples.
    pub interval_mismatch_ns: f64,
}

impl CalibrationPair {
    /// Convert VK_EXT_present_timing feedback reported in the DEVICE domain.
    /// WSI describes present-stage values as nanoseconds, while VkTimeDomainKHR
    /// describes DEVICE timestamps in timestampPeriod-sized ticks. Until that
    /// discrepancy is validated for the running driver, only period == 1 ns
    /// per tick is unambiguous. Other periods retain raw samples for analysis.
    pub fn estimate_present_device_ns(&self, feedback_ns: u64) -> Option<CalibratedTime> {
        (self.timestamp_period_ns == 1.0)
            .then(|| self.estimate_monotonic_ns(feedback_ns))
            .flatten()
    }

    /// Estimate a DEVICE-domain feedback timestamp within this sample pair.
    /// This lower-level method accepts known DEVICE ticks, such as a timestamp
    /// captured by vkCmdWriteTimestamp. For present feedback, use
    /// `estimate_present_device_ns` instead. Returns None for an out-of-range timestamp, implausible clock rate,
    /// zero feedback, or an unrepresentable conversion. Recalibrate often;
    /// maxDeviation does not bound drift outside the sampled interval.
    pub fn estimate_monotonic_ns(&self, device_ticks: u64) -> Option<CalibratedTime> {
        let period = self.timestamp_period_ns as f64;
        if device_ticks == 0 || !period.is_finite() || period <= 0.0 {
            return None;
        }
        // Wrapping differences preserve nearby 64-bit DEVICE timestamps even
        // when the counter wraps. Reject intervals >= 2^63 ticks.
        let span_ticks = self.end.device_ticks.wrapping_sub(self.start.device_ticks);
        let event_ticks = device_ticks.wrapping_sub(self.start.device_ticks);
        if span_ticks == 0 || span_ticks > i64::MAX as u64 || event_ticks > span_ticks {
            return None;
        }
        let host_span = self.end.monotonic_ns.checked_sub(self.start.monotonic_ns)?;
        let scaled_span = (span_ticks as f64) * period;
        let mismatch = (scaled_span - host_span as f64).abs();
        let allowed = (self.start.max_deviation_ns.max(self.end.max_deviation_ns) as f64) * 2.0
            + period.max(1.0) * 2.0;
        if !scaled_span.is_finite() || mismatch > allowed {
            return None;
        }
        let estimate = self.start.monotonic_ns as f64 + (event_ticks as f64) * period;
        if !estimate.is_finite() || estimate < 0.0 || estimate > u64::MAX as f64 {
            return None;
        }
        Some(CalibratedTime {
            monotonic_ns: estimate.round() as u64,
            max_deviation_ns: self.start.max_deviation_ns.max(self.end.max_deviation_ns),
            interval_mismatch_ns: mismatch,
        })
    }
}

/// Uses ash's VK_KHR_calibrated_timestamps bindings, which are present in
/// ash 0.38 even though the present-timing and present-id2 bindings are not.
pub struct CalibrationApi {
    physical: vk::PhysicalDevice,
    instance_api: ash::khr::calibrated_timestamps::Instance,
    device_api: ash::khr::calibrated_timestamps::Device,
    pub timestamp_period_ns: f32,
}
impl CalibrationApi {
    /// Call only after enabling VK_KHR_calibrated_timestamps on the device.
    ///
    /// # Safety
    /// The extension must be enabled on `device`, which must belong to
    /// `instance` and `physical`. All Vulkan objects must outlive this API.
    pub unsafe fn new(
        entry: &Entry,
        instance: &Instance,
        physical: vk::PhysicalDevice,
        device: &Device,
    ) -> anyhow::Result<Self> {
        let period = instance
            .get_physical_device_properties(physical)
            .limits
            .timestamp_period;
        anyhow::ensure!(
            period.is_finite() && period > 0.0,
            "invalid Vulkan timestampPeriod"
        );
        Ok(Self {
            physical,
            instance_api: ash::khr::calibrated_timestamps::Instance::new(entry, instance),
            device_api: ash::khr::calibrated_timestamps::Device::new(instance, device),
            timestamp_period_ns: period,
        })
    }

    /// # Safety
    /// The physical device and instance used to create this API must remain
    /// alive and must not be destroyed concurrently.
    pub unsafe fn available_domains(&self) -> anyhow::Result<Vec<i32>> {
        let get = self
            .instance_api
            .fp()
            .get_physical_device_calibrateable_time_domains_khr;
        let mut count = 0;
        get(self.physical, &mut count, ptr::null_mut()).result()?;
        anyhow::ensure!(count <= 64, "unreasonable calibrateable time domain count");
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut domains = vec![vk::TimeDomainKHR::default(); count as usize];
        get(self.physical, &mut count, domains.as_mut_ptr()).result()?;
        anyhow::ensure!(
            count as usize <= domains.len(),
            "calibrateable domains changed during query"
        );
        Ok(domains
            .into_iter()
            .take(count as usize)
            .map(|d| d.as_raw())
            .collect())
    }

    /// # Safety
    /// The Vulkan instance, physical device, and logical device used to
    /// create this API must remain alive and must not be destroyed concurrently.
    pub unsafe fn sample_device_monotonic(&self) -> anyhow::Result<CalibrationSample> {
        let domains = self.available_domains()?;
        anyhow::ensure!(
            domains.contains(&DEVICE_DOMAIN),
            "DEVICE calibration domain unavailable"
        );
        anyhow::ensure!(
            domains.contains(&MONOTONIC_DOMAIN),
            "CLOCK_MONOTONIC calibration domain unavailable"
        );
        let infos = [
            vk::CalibratedTimestampInfoKHR::default().time_domain(vk::TimeDomainKHR::DEVICE),
            vk::CalibratedTimestampInfoKHR::default()
                .time_domain(vk::TimeDomainKHR::CLOCK_MONOTONIC),
        ];
        let mut values = [0u64; 2];
        let mut deviation = 0u64;
        (self.device_api.fp().get_calibrated_timestamps_khr)(
            self.device_api.device(),
            2,
            infos.as_ptr(),
            values.as_mut_ptr(),
            &mut deviation,
        )
        .result()?;
        anyhow::ensure!(
            values[0] != 0 && values[1] != 0,
            "calibration returned a zero timestamp"
        );
        Ok(CalibrationSample {
            device_ticks: values[0],
            monotonic_ns: values[1],
            max_deviation_ns: deviation,
        })
    }

    pub fn pair(&self, start: CalibrationSample, end: CalibrationSample) -> CalibrationPair {
        CalibrationPair {
            start,
            end,
            timestamp_period_ns: self.timestamp_period_ns,
        }
    }
}

pub fn stage_name(stage: u32) -> &'static str {
    match stage {
        STAGE_QUEUE_END => "QUEUE_OPERATIONS_END",
        STAGE_DEQUEUED => "REQUEST_DEQUEUED",
        STAGE_FIRST_PIXEL_OUT => "IMAGE_FIRST_PIXEL_OUT",
        STAGE_FIRST_PIXEL_VISIBLE => "IMAGE_FIRST_PIXEL_VISIBLE",
        _ => "UNKNOWN",
    }
}

pub fn domain_name(domain: i32) -> &'static str {
    match domain {
        MONOTONIC_DOMAIN => "CLOCK_MONOTONIC",
        0 => "DEVICE",
        2 => "CLOCK_MONOTONIC_RAW",
        1000208000 => "PRESENT_STAGE_LOCAL",
        1000208001 => "SWAPCHAIN_LOCAL",
        _ => "OTHER",
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod abi_tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn extension_struct_layout_matches_vulkan_header() {
        assert_eq!(size_of::<Features>(), 32);
        assert_eq!(size_of::<Id2Features>(), 24);
        assert_eq!(size_of::<SurfaceCaps>(), 32);
        assert_eq!(size_of::<PresentTimingInfo>(), 48);
        assert_eq!(size_of::<PresentTimingsInfo>(), 32);
        assert_eq!(size_of::<TimeDomainProperties>(), 40);
        assert_eq!(size_of::<PastTiming>(), 72);
        assert_eq!(size_of::<PastProperties>(), 48);
        assert_eq!(offset_of!(PastTiming, stages), 40);
        assert_eq!(offset_of!(PastTiming, time_domain_id), 56);
    }

    #[test]
    fn calibration_scales_device_ticks_and_rejects_inconsistent_rate() {
        let start = CalibrationSample {
            device_ticks: 1_000,
            monotonic_ns: 10_000,
            max_deviation_ns: 10,
        };
        let end = CalibrationSample {
            device_ticks: 2_000,
            monotonic_ns: 12_000,
            max_deviation_ns: 10,
        };
        let pair = CalibrationPair {
            start,
            end,
            timestamp_period_ns: 2.0,
        };
        assert_eq!(
            pair.estimate_monotonic_ns(1_500).unwrap().monotonic_ns,
            11_000
        );
        assert!(pair.estimate_monotonic_ns(2_001).is_none());
        assert!(CalibrationPair {
            timestamp_period_ns: 1.0,
            ..pair
        }
        .estimate_monotonic_ns(1_500)
        .is_none());
    }

    #[test]
    fn present_feedback_conversion_uses_epoch_sized_offsets_and_period_guard() {
        let start = CalibrationSample {
            device_ticks: 1_790_628_109_637_000_000,
            monotonic_ns: 97_654_321_000_000,
            max_deviation_ns: 20,
        };
        let end = CalibrationSample {
            device_ticks: start.device_ticks + 1_000_000,
            monotonic_ns: start.monotonic_ns + 1_000_000,
            max_deviation_ns: 20,
        };
        let pair = CalibrationPair {
            start,
            end,
            timestamp_period_ns: 1.0,
        };
        assert_eq!(
            pair.estimate_present_device_ns(start.device_ticks + 456_789)
                .unwrap()
                .monotonic_ns,
            start.monotonic_ns + 456_789
        );
        assert!(pair
            .estimate_present_device_ns(start.device_ticks - 1)
            .is_none());
        assert!(pair
            .estimate_present_device_ns(end.device_ticks + 1)
            .is_none());
        assert!(CalibrationPair {
            timestamp_period_ns: 2.0,
            ..pair
        }
        .estimate_present_device_ns(start.device_ticks + 456_789)
        .is_none());
        assert!(CalibrationPair {
            end: CalibrationSample {
                monotonic_ns: end.monotonic_ns + 10_000,
                ..end
            },
            ..pair
        }
        .estimate_present_device_ns(start.device_ticks + 456_789)
        .is_none());
    }
}
