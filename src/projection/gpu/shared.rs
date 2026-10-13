//! OPAQUE_FD Vulkan-to-Vulkan import and an owned GPU cache for the
//! experimental shared source. Producer images use exactly the usage flags
//! below, and both queues exchange ownership through EXTERNAL in GENERAL.
//! Protocol v1 carries encoded SDR values in UNORM images. sRGB views would
//! decode those values before the existing blend shader's encoded-color
//! arithmetic; floating-point/HDR sources also require a separate contract.
use super::*;
use crate::projection::shared_texture::SlotDescription;
use std::os::fd::{AsRawFd, IntoRawFd};

const SHARED_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::TRANSFER_SRC.as_raw()
        | vk::ImageUsageFlags::TRANSFER_DST.as_raw()
        | vk::ImageUsageFlags::SAMPLED.as_raw(),
);

pub struct SharedImage {
    pub description: SlotDescription,
    pub(super) image: vk::Image,
    memory: vk::DeviceMemory,
    pub(super) view: vk::ImageView,
    ready: vk::Semaphore,
    done: vk::Semaphore,
    queue_owned: std::cell::Cell<bool>,
    device: Arc<DeviceState>,
}

/// Keeps an accepted external wait alive until completion. Drop retires the
/// submission before the caller's imported slots can be destroyed.
pub struct SharedCopy {
    device: Arc<DeviceState>,
    fence: vk::Fence,
    started: Instant,
    complete: bool,
}

impl Drop for SharedCopy {
    fn drop(&mut self) {
        if self.complete || self.device.direct_failed.load(Ordering::Relaxed) {
            return;
        }
        let remaining = Duration::from_secs(2).saturating_sub(self.started.elapsed());
        if unsafe {
            self.device.device.wait_for_fences(
                &[self.fence],
                true,
                remaining.as_nanos().min(u64::MAX as u128) as u64,
            )
        }
        .is_err()
        {
            self.device.direct_failed.store(true, Ordering::Relaxed);
            eprintln!(
                "shared texture: pending copy completion unverified; retaining Vulkan resources"
            );
        }
    }
}

impl Drop for SharedImage {
    fn drop(&mut self) {
        if self.device.direct_failed.load(Ordering::Relaxed) {
            // A timed-out external wait may still reference these handles.
            // Gpu retains the device in that case; process exit reclaims it.
            return;
        }
        unsafe {
            self.device.device.destroy_semaphore(self.ready, None);
            self.device.device.destroy_semaphore(self.done, None);
            self.device.device.destroy_image_view(self.view, None);
            self.device.device.destroy_image(self.image, None);
            self.device.device.free_memory(self.memory, None);
        }
    }
}

fn import_semaphore(device: &DeviceState, fd: OwnedFd) -> anyhow::Result<vk::Semaphore> {
    let extension = device
        .external_semaphore_fd
        .as_ref()
        .context("shared texture: VK_KHR_external_semaphore_fd unavailable")?;
    let semaphore = unsafe {
        device
            .device
            .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
    }
    .context("creating shared binary semaphore")?;
    let info = vk::ImportSemaphoreFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD)
        .fd(fd.as_raw_fd());
    // Permanent import: the producer reuses the same binary payload each
    // slot cycle. Vulkan consumes the FD only on successful import.
    if let Err(error) = unsafe { extension.import_semaphore_fd(&info) } {
        unsafe { device.device.destroy_semaphore(semaphore, None) };
        return Err(error).context("importing shared binary semaphore");
    }
    let _ = fd.into_raw_fd();
    Ok(semaphore)
}

impl Gpu {
    /// Sample an already acquired slot while keeping graphics ownership for
    /// future free-run redraws. Replacement/shutdown explicitly releases it.
    pub fn blend_shared(
        &mut self,
        canvas: &SharedImage,
        jobs: &[BlendJob<'_>],
    ) -> anyhow::Result<Duration> {
        self.validate_shared_sampling(canvas)?;
        self.render(
            Source::Canvas {
                image: CanvasImage::Shared(canvas),
                y_invert: false,
            },
            jobs,
        )
    }

    pub fn blend_shared_direct(
        &mut self,
        canvas: &SharedImage,
        jobs: &[display::DirectBlendJob<'_>],
    ) -> anyhow::Result<display::DirectFrame> {
        self.validate_shared_sampling(canvas)?;
        self.render_direct(
            Source::Canvas {
                image: CanvasImage::Shared(canvas),
                y_invert: false,
            },
            jobs,
        )
    }

    fn validate_shared_sampling(&self, canvas: &SharedImage) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&canvas.device, &self.device) && canvas.queue_owned.get(),
            "shared texture: sampling requires an acquired, retained slot on this Vulkan device"
        );
        Ok(())
    }

    /// Replace a retained source, or release it at shutdown. Old reads have
    /// completed before recording; the new ready semaphore gates its acquire.
    /// Neither source may be sampled while this submission is outstanding.
    pub fn submit_shared_switch(
        &mut self,
        previous: Option<&SharedImage>,
        next: Option<&SharedImage>,
    ) -> anyhow::Result<SharedCopy> {
        self.wait_for_pending_work()?;
        anyhow::ensure!(
            previous.is_some() || next.is_some(),
            "shared texture: empty ownership switch"
        );
        if let Some(previous) = previous {
            self.validate_shared_sampling(previous)?;
        }
        if let Some(next) = next {
            anyhow::ensure!(
                Arc::ptr_eq(&next.device, &self.device) && !next.queue_owned.get(),
                "shared texture: next slot is not externally owned on this Vulkan device"
            );
        }
        let started = Instant::now();
        let device = &self.device.device;
        let cb = self.command_buffer;
        let mut barriers = Vec::with_capacity(2);
        if let Some(previous) = previous {
            barriers.push(
                vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::SHADER_READ)
                    .dst_access_mask(vk::AccessFlags::empty())
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(self.device.queue_family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                    .image(previous.image)
                    .subresource_range(color_subresource_range()),
            );
        }
        if let Some(next) = next {
            barriers.push(
                vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                    .dst_queue_family_index(self.device.queue_family)
                    .image(next.image)
                    .subresource_range(color_subresource_range()),
            );
        }
        unsafe {
            device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE | vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            device.end_command_buffer(cb)?;
            device.reset_fences(&[self.fence])?;
            let commands = [cb];
            let waits: Vec<_> = next.iter().map(|image| image.ready).collect();
            // Ownership transfers have no defined pipeline stage without the
            // ownership-transfer-use-all-stages flag. The external acquire
            // must wait for ready at ALL_COMMANDS, not only the sampled read.
            // https://docs.vulkan.org/spec/latest/chapters/synchronization.html#synchronization-queue-transfers
            let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
            let signals: Vec<_> = previous.iter().map(|image| image.done).collect();
            device
                .queue_submit(
                    self.device.queue,
                    &[vk::SubmitInfo::default()
                        .command_buffers(&commands)
                        .wait_semaphores(&waits)
                        .wait_dst_stage_mask(&stages)
                        .signal_semaphores(&signals)],
                    self.fence,
                )
                .context("submitting retained shared ownership switch")?;
        }
        self.fence_in_flight = true;
        if let Some(previous) = previous {
            previous.queue_owned.set(false);
        }
        if let Some(next) = next {
            next.queue_owned.set(true);
        }
        Ok(SharedCopy {
            device: Arc::clone(&self.device),
            fence: self.fence,
            started,
            complete: false,
        })
    }

    pub fn import_shared_image(
        &self,
        description: SlotDescription,
        fds: [OwnedFd; 3],
    ) -> anyhow::Result<SharedImage> {
        let state = &self.device;
        anyhow::ensure!(
            state.external_semaphore_fd.is_some(),
            "shared texture: external semaphore FD extension unavailable"
        );
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        unsafe {
            state
                .instance
                .get_physical_device_properties2(state.physical_device, &mut properties)
        };
        anyhow::ensure!(
            description.device_uuid == id.device_uuid,
            "shared texture: producer Vulkan device UUID does not match compositor GPU"
        );
        let format = vk::Format::from_raw(description.format);
        anyhow::ensure!(
            matches!(
                format,
                vk::Format::R8G8B8A8_UNORM
                    | vk::Format::B8G8R8A8_UNORM
                    | vk::Format::A2B10G10R10_UNORM_PACK32
                    | vk::Format::A2R10G10B10_UNORM_PACK32
            ),
            "shared texture: protocol v1 requires encoded SDR in RGBA/BGRA8 or RGB10A2 UNORM; sRGB/HDR and other formats are unsupported (VkFormat {})",
            description.format
        );
        let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let query = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(SHARED_USAGE)
            .push_next(&mut external);
        let mut external_properties = vk::ExternalImageFormatProperties::default();
        let mut image_properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        unsafe {
            state.instance.get_physical_device_image_format_properties2(
                state.physical_device,
                &query,
                &mut image_properties,
            )
        }
        .context("querying external image support")?;
        let limits = image_properties.image_format_properties;
        let support = external_properties.external_memory_properties;
        anyhow::ensure!(
            support
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
                && support
                    .compatible_handle_types
                    .contains(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
            "shared texture: format/usage is not OPAQUE_FD importable"
        );
        anyhow::ensure!(
            description.width <= limits.max_extent.width
                && description.height <= limits.max_extent.height,
            "shared texture: image exceeds Vulkan format limits"
        );
        let mut semaphore_properties = vk::ExternalSemaphoreProperties::default();
        let semaphore_query = vk::PhysicalDeviceExternalSemaphoreInfo::default()
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
        unsafe {
            state
                .instance
                .get_physical_device_external_semaphore_properties(
                    state.physical_device,
                    &semaphore_query,
                    &mut semaphore_properties,
                )
        };
        anyhow::ensure!(
            semaphore_properties
                .external_semaphore_features
                .contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE),
            "shared texture: OPAQUE_FD binary semaphore import unavailable"
        );
        let features = unsafe {
            state
                .instance
                .get_physical_device_format_properties(state.physical_device, format)
        }
        .optimal_tiling_features;
        anyhow::ensure!(
            features.contains(
                vk::FormatFeatureFlags::SAMPLED_IMAGE
                    | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                    | vk::FormatFeatureFlags::TRANSFER_SRC
                    | vk::FormatFeatureFlags::TRANSFER_DST
            ),
            "shared texture: format lacks sampled/filter/transfer support"
        );

        let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: description.width,
                height: description.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(SHARED_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info);
        let image = unsafe { state.device.create_image(&image_info, None) }
            .context("creating shared import image")?;
        let mut cleanup = Cleanup {
            device: &state.device,
            image,
            memory: None,
            view: None,
        };
        let requirements = unsafe { state.device.get_image_memory_requirements(image) };
        let memory_properties = unsafe {
            state
                .instance
                .get_physical_device_memory_properties(state.physical_device)
        };
        anyhow::ensure!(
            description.memory_type < memory_properties.memory_type_count
                && requirements.memory_type_bits & (1u32 << description.memory_type) != 0,
            "shared texture: producer memory type incompatible with imported image"
        );
        let heap =
            memory_properties.memory_types[description.memory_type as usize].heap_index as usize;
        anyhow::ensure!(
            description.allocation_size >= requirements.size
                && description.allocation_size <= memory_properties.memory_heaps[heap].size,
            "shared texture: allocation incompatible with image memory requirements"
        );
        // OPAQUE_FD allocations must use the exporter's exact memory type and
        // size. vkGetMemoryFdPropertiesKHR does not accept OPAQUE_FD.
        let [memory_fd, ready_fd, done_fd] = fds;
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD)
            .fd(memory_fd.as_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(description.allocation_size)
            .memory_type_index(description.memory_type)
            .push_next(&mut import)
            .push_next(&mut dedicated);
        let memory = unsafe { state.device.allocate_memory(&allocate, None) }
            .context("importing shared image memory")?;
        let _ = memory_fd.into_raw_fd();
        cleanup.memory = Some(memory);
        unsafe { state.device.bind_image_memory(image, memory, 0) }
            .context("binding shared image memory")?;
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(color_subresource_range());
        let view = unsafe { state.device.create_image_view(&view_info, None) }
            .context("creating shared sampled image view")?;
        cleanup.view = Some(view);
        let ready = import_semaphore(state, ready_fd)?;
        let done = match import_semaphore(state, done_fd) {
            Ok(done) => done,
            Err(error) => {
                unsafe { state.device.destroy_semaphore(ready, None) };
                return Err(error);
            }
        };
        cleanup.defuse();
        Ok(SharedImage {
            description,
            image,
            memory,
            view,
            ready,
            done,
            queue_owned: std::cell::Cell::new(false),
            device: Arc::clone(state),
        })
    }

    /// Allocate a native-format cache; no conversion, upload, or CPU pixels.
    pub fn create_shared_cache(&mut self, source: &SharedImage) -> anyhow::Result<StaticCanvas> {
        anyhow::ensure!(
            Arc::ptr_eq(&source.device, &self.device),
            "shared texture: source belongs to a different Vulkan device"
        );
        self.wait_for_pending_work()?;
        let state = Arc::clone(&self.device);
        let description = &source.description;
        let format = vk::Format::from_raw(description.format);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: description.width,
                height: description.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { state.device.create_image(&info, None) }
            .context("creating owned shared canvas")?;
        let mut cleanup = Cleanup {
            device: &state.device,
            image,
            memory: None,
            view: None,
        };
        let requirements = unsafe { state.device.get_image_memory_requirements(image) };
        let properties = unsafe {
            state
                .instance
                .get_physical_device_memory_properties(state.physical_device)
        };
        let memory_type = memory_type_index(
            &properties,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .context("shared texture: no device-local cache memory")?;
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        let memory = unsafe { state.device.allocate_memory(&allocation, None) }
            .context("allocating owned shared canvas")?;
        cleanup.memory = Some(memory);
        unsafe { state.device.bind_image_memory(image, memory, 0) }?;
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(color_subresource_range());
        let view = unsafe { state.device.create_image_view(&view_info, None) }?;
        cleanup.view = Some(view);
        cleanup.defuse();
        Ok(StaticCanvas {
            width: description.width,
            height: description.height,
            format,
            image,
            memory,
            view,
            device: state,
        })
    }

    /// Queue ready wait, EXTERNAL acquire, copy, EXTERNAL release, and done
    /// signal. The caller sends its release packet after this returns, then
    /// dispatches presentation events while polling completion. No shader
    /// may sample the cache, or reuse this command buffer, before completion.
    pub fn submit_shared_frame(
        &mut self,
        source: &SharedImage,
        cache: &StaticCanvas,
        first: bool,
    ) -> anyhow::Result<SharedCopy> {
        self.wait_for_pending_work()?;
        anyhow::ensure!(
            !source.queue_owned.get()
                && cache.width == source.description.width
                && cache.height == source.description.height
                && cache.format == vk::Format::from_raw(source.description.format)
                && Arc::ptr_eq(&cache.device, &self.device)
                && Arc::ptr_eq(&source.device, &self.device),
            "shared texture: cache/source dimensions, format, or device mismatch"
        );
        let started = Instant::now();
        let device = &self.device.device;
        let cb = self.command_buffer;
        let range = color_subresource_range();
        unsafe {
            device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            let acquire = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .dst_queue_family_index(self.device.queue_family)
                .image(source.image)
                .subresource_range(range);
            let writable = vk::ImageMemoryBarrier::default()
                .src_access_mask(if first {
                    vk::AccessFlags::empty()
                } else {
                    vk::AccessFlags::SHADER_READ | vk::AccessFlags::TRANSFER_WRITE
                })
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(if first {
                    vk::ImageLayout::UNDEFINED
                } else {
                    vk::ImageLayout::GENERAL
                })
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(cache.image)
                .subresource_range(range);
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TOP_OF_PIPE
                    | vk::PipelineStageFlags::FRAGMENT_SHADER
                    | vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire, writable],
            );
            let layers = vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            };
            let region = vk::ImageCopy::default()
                .src_subresource(layers)
                .dst_subresource(layers)
                .extent(vk::Extent3D {
                    width: cache.width,
                    height: cache.height,
                    depth: 1,
                });
            device.cmd_copy_image(
                cb,
                source.image,
                vk::ImageLayout::GENERAL,
                cache.image,
                vk::ImageLayout::GENERAL,
                &[region],
            );
            let release = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(self.device.queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .image(source.image)
                .subresource_range(range);
            let readable = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(cache.image)
                .subresource_range(range);
            device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE | vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release, readable],
            );
            device.end_command_buffer(cb)?;
            device.reset_fences(&[self.fence])?;
            let commands = [cb];
            let waits = [source.ready];
            let signals = [source.done];
            // Match the external ownership acquire's semaphore dependency;
            // see the queue-transfer rule documented in submit_shared_switch.
            let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            let submit = vk::SubmitInfo::default()
                .command_buffers(&commands)
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages)
                .signal_semaphores(&signals);
            device
                .queue_submit(self.device.queue, &[submit], self.fence)
                .context("submitting shared copy")?;
        }
        self.fence_in_flight = true;
        Ok(SharedCopy {
            device: Arc::clone(&self.device),
            fence: self.fence,
            started,
            complete: false,
        })
    }

    /// The elapsed time includes producer readiness and completion-poll
    /// latency; it is asynchronous wall latency, not GPU timestamp timing.
    pub fn poll_shared_copy(&mut self, copy: &mut SharedCopy) -> anyhow::Result<Option<Duration>> {
        anyhow::ensure!(
            Arc::ptr_eq(&copy.device, &self.device) && copy.fence == self.fence,
            "shared texture: copy belongs to a different Vulkan queue"
        );
        match unsafe { self.device.device.get_fence_status(copy.fence) } {
            Ok(true) => {
                copy.complete = true;
                self.fence_in_flight = false;
                Ok(Some(copy.started.elapsed()))
            }
            Ok(false) if copy.started.elapsed() < Duration::from_secs(2) => Ok(None),
            result => {
                self.device.direct_failed.store(true, Ordering::Relaxed);
                bail!("shared texture: copy completion failed or exceeded two seconds: {result:?}");
            }
        }
    }

    /// Bounded retirement for orderly shutdown. Polling remains the normal
    /// frame path; this wait runs only when the renderer is stopping.
    pub fn finish_shared_copy(&mut self, copy: &mut SharedCopy) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&copy.device, &self.device) && copy.fence == self.fence,
            "shared texture: submission belongs to a different Vulkan queue"
        );
        let remaining = Duration::from_secs(2).saturating_sub(copy.started.elapsed());
        if let Err(error) = unsafe {
            self.device.device.wait_for_fences(
                &[copy.fence],
                true,
                remaining.as_nanos().min(u64::MAX as u128) as u64,
            )
        } {
            self.device.direct_failed.store(true, Ordering::Relaxed);
            return Err(error)
                .context("shared texture: shutdown could not retire source submission");
        }
        copy.complete = true;
        self.fence_in_flight = false;
        Ok(())
    }
}
