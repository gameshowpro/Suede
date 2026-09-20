//! The GPU blend path: keep pixels on the GPU instead of round-tripping them
//! through system memory for `slicer.rs`'s CPU blend.
//!
//! Measured on the four-projector rig (RTX A1000, 3840x2385 canvas, four
//! 1920x1200 outputs) before this module existed: 32 fps, of which 19.6 ms a
//! frame was the compositor reading the canvas back from the GPU into shared
//! memory, 3 ms a memcpy, and 8.6 ms the CPU blend in `Blend::rows` — the GPU
//! itself sat at 38% the whole time. The fix is to never leave the GPU:
//! allocate the capture target and every output buffer as Vulkan images
//! exported as dmabufs, let the compositor blit the canvas straight into the
//! capture image, blend with a fragment shader, and hand the output images
//! back to the compositor as dmabuf `wl_buffer`s.
//!
//! This module is the Vulkan half only — allocating and blending images. It
//! knows nothing about Wayland; the dmabuf protocol, screencopy negotiation
//! with the compositor, and presenting are a separate module that calls the
//! API below and falls back to the CPU path when [`Gpu::new`] fails (no
//! Vulkan, or a Vulkan that is missing something this module needs).
//!
//! ## Rebuilding the shaders
//!
//! The target machines have no shader compiler, so `blend.vert`/`blend.frag`
//! are compiled offline and the `.spv` files checked in alongside the GLSL
//! source, under `shaders/`. Rebuild with naga (`cargo install naga-cli`,
//! or already at `~/.cargo/bin/naga` in this project's WSL dev container):
//!
//! ```text
//! naga --input-kind glsl --shader-stage vert blend.vert blend.vert.spv
//! naga --input-kind glsl --shader-stage frag blend.frag blend.frag.spv
//! ```
//!
//! `blend.frag` declares the canvas texture and its sampler separately
//! (rather than as a combined `sampler2D` uniform) because naga's GLSL
//! frontend does not implement combined image samplers — see the comment at
//! the top of that file. `gpu.rs` matches it with an immutable sampler baked
//! into the descriptor set layout, so nothing here ever writes that binding.
//!
//! ## Keeping shared images in `GENERAL`
//!
//! Every image this module exports or imports as a dmabuf is shared with
//! another Vulkan context (the compositor's) that this process does not
//! control. Ownership of such an image moves between this process's queue
//! family and `VK_QUEUE_FAMILY_FOREIGN_EXT` as each side takes its turn
//! reading or writing it — an "acquire" barrier before use, a "release"
//! barrier after — but the image's *layout* stays `GENERAL` for its entire
//! life on both sides. `GENERAL` is not a performance compromise here: for a
//! `DRM_FORMAT_MODIFIER`-tiled image the driver's tiling comes from the
//! modifier, not from `VkImageLayout`, so there is no `OPTIMAL` layout to
//! transition into, and the two processes have no channel to agree on a
//! layout change even if there were. This whole pattern — foreign queue
//! family, `GENERAL` throughout, one UNDEFINED->GENERAL transition the first
//! time *this* process is the first writer — mirrors wlroots' own Vulkan
//! renderer (`render/vulkan/texture.c` and `render/vulkan/renderer.c` in the
//! wlroots tree; search either for `VK_QUEUE_FAMILY_FOREIGN_EXT`), which is
//! the reference this module was checked against.
//!
//! A `Capture` image never needs that initial transition here: the
//! compositor is the first one to write into it (screencopy only signals
//! readiness after it has), so the compositor's own import path — running
//! the same protocol on its side — is what carries it from `UNDEFINED` to
//! `GENERAL` before we ever touch it. A `Present` image is the opposite: we
//! render into it before the compositor has ever seen it, so `create_image`
//! does that one-off transition itself.
//!
//! ## Queue priority
//!
//! `blend()`'s fence wait is not waiting on its own work — the shader over a
//! slice is trivial — it is waiting for this process's turn on a queue
//! shared with whatever else is running on the GPU. Measured on the
//! four-projector rig: 6.8 ms with the GPU at 45% load from another app, 40
//! ms at 99%, and the wall's presented frame rate followed the wait down in
//! lockstep (see `slicer.rs`'s module doc for what that did to the whole
//! pipeline). `Gpu::new` asks for a high-priority queue to buy that back: if
//! the device advertises `VK_KHR_global_priority` (or the older, split
//! `VK_EXT_global_priority_query`/`VK_EXT_global_priority` pair — see
//! `supported_global_priority_extensions`), it queries which tiers the
//! chosen queue family actually supports and creates the queue at the
//! highest of REALTIME/HIGH that is listed, stepping down through the
//! ladder (see `create_device`) — down to a plain queue with no explicit
//! priority at all — whenever `vkCreateDevice` refuses a tier, most often
//! with `VK_ERROR_NOT_PERMITTED` (REALTIME usually needs `CAP_SYS_NICE`,
//! which this process does not run with by default). Entirely best-effort:
//! every device this module has ever run on still works with no priority
//! extension at all, just with `blend()` back on the GPU's ordinary,
//! contended queue. `describe()` reports whichever tier was actually
//! granted.

use std::ffi::{c_char, CStr};
use std::io::Cursor;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use ash::vk;

const VERT_SPV: &[u8] = include_bytes!("shaders/blend.vert.spv");
const FRAG_SPV: &[u8] = include_bytes!("shaders/blend.frag.spv");
const MEASURE_FRAG_SPV: &[u8] = include_bytes!("shaders/measure.frag.spv");

// DRM fourccs this module understands, and their little-endian byte order —
// see the format mapping table below.
const FOURCC_XR24: u32 = 0x3432_5258; // XRGB8888
const FOURCC_AR24: u32 = 0x3432_5241; // ARGB8888
const FOURCC_XB24: u32 = 0x3432_4258; // XBGR8888
const FOURCC_AB24: u32 = 0x3432_4241; // ABGR8888

/// Descriptor pool / per-output resource ceiling. Generous for any rig this
/// daemon manages (four projectors today), cheap to size for even if unused.
const MAX_OUTPUTS: usize = 8;

/// `uvec4` slots in each output's `sync`-pattern shape buffer.
///
/// Fixed rather than grown on demand because the pattern is redrawn every
/// frame and a reallocation mid-run would mean rewriting a descriptor
/// between a commit and its scanout. 1024 slots is 16 KiB an output — a
/// rounding error against one output-sized image — and comfortably above
/// what `pattern::sync_rects` can emit, which
/// `the_shape_buffer_holds_the_worst_case_sync_frame` asserts.
const SYNC_SHAPE_SLOTS: usize = 1024;

/// A GPU image whose memory is exported as a dmabuf, for a Wayland client to
/// wrap in a `wl_buffer`. Single memory plane.
pub struct DmabufImage {
    pub width: u32,
    pub height: u32,
    /// DRM fourcc the image was created as.
    pub fourcc: u32,
    /// The DRM format modifier the driver actually chose.
    pub modifier: u64,
    /// Row pitch and offset of memory plane 0, for `zwp_linux_buffer_params_v1.add`.
    pub stride: u32,
    pub offset: u32,
    /// The dmabuf. The caller borrows it (`as_fd()`) when building the wl_buffer;
    /// the protocol dups it on send, so one fd per image is enough.
    pub fd: OwnedFd,
    format: vk::Format,
    /// Filtering support of the actual selected format/modifier.
    linear_filter: bool,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    /// Kept alive independently of `Gpu`: the Wayland half can hold a
    /// `DmabufImage` for many frames after a `blend()` call, possibly past
    /// `Gpu` itself being dropped (a config reload rebuilding it, say).
    device: Arc<DeviceState>,
}

impl DmabufImage {
    /// Whether this image's selected DRM modifier supports bilinear sampling.
    ///
    /// Capture negotiation uses this after allocation as a final assertion:
    /// the driver, rather than the candidate list, chooses the modifier.
    pub fn linear_filter_supported(&self) -> bool {
        self.linear_filter
    }
}

impl Drop for DmabufImage {
    fn drop(&mut self) {
        // Safety: `view`, `image`, and `memory` were all created together in
        // `Gpu::create_image` and are exclusively owned by this struct —
        // nothing else ever stores a copy of these handles. `device` (an
        // `Arc`, shared with `Gpu` and every sibling `DmabufImage`) is still
        // valid here regardless of drop order between them, and its own
        // `Drop` only runs once every image made from it, and `Gpu` itself,
        // are gone. Destroying the view before the image before the memory
        // matches the order `create_image` builds them in.
        unsafe {
            self.device.device.destroy_image_view(self.view, None);
            self.device.device.destroy_image(self.image, None);
            self.device.device.free_memory(self.memory, None);
        }
    }
}

/// A private-to-Vulkan, uploaded RGBA canvas used by static projection
/// patterns. Unlike [`DmabufImage`], it is never offered to Wayland, so it
/// remains owned by this queue for its entire life.
pub struct StaticCanvas {
    width: u32,
    height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    device: Arc<DeviceState>,
}

impl Drop for StaticCanvas {
    fn drop(&mut self) {
        // Safety: these handles are exclusively owned by this canvas. Its
        // DeviceState Arc keeps the Vulkan device alive through teardown.
        unsafe {
            self.device.device.destroy_image_view(self.view, None);
            self.device.device.destroy_image(self.image, None);
            self.device.device.free_memory(self.memory, None);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    /// The compositor renders into it; we sample from it.
    Capture,
    /// We render into it; the compositor samples from it (or scans it out).
    Present,
}

/// What `create_device`'s global-priority ladder (see the module doc's
/// "Queue priority" section) settled on for the render queue. `Default`
/// covers both "no global-priority extension at all" and "every tier this
/// process asked for, including the implicit default, was refused anyway" —
/// from `describe()`'s side those are the same queue, so they share a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueuePriority {
    Realtime,
    High,
    Medium,
    Default,
}

impl QueuePriority {
    fn label(self) -> &'static str {
        match self {
            QueuePriority::Realtime => "realtime",
            QueuePriority::High => "high",
            QueuePriority::Medium => "medium",
            QueuePriority::Default => "default",
        }
    }
}

/// Where a draw's colour comes from: the captured canvas, or the `sync`
/// test pattern's own shape list.
///
/// One code path records both because everything around the colour — the
/// per-output transfer, the viewport, the target's queue-family acquire and
/// release, the fence — must be identical, or the pattern would stop
/// measuring the path content actually takes.
enum Source<'a> {
    Canvas {
        image: CanvasImage<'a>,
        /// The canvas is stored bottom-up (screencopy's flag).
        y_invert: bool,
    },
    Sync,
}

#[derive(Clone, Copy)]
enum CanvasImage<'a> {
    Dmabuf(&'a DmabufImage),
    Static(&'a StaticCanvas),
}

impl CanvasImage<'_> {
    fn image(self) -> vk::Image {
        match self {
            Self::Dmabuf(image) => image.image,
            Self::Static(image) => image.image,
        }
    }

    fn view(self) -> vk::ImageView {
        match self {
            Self::Dmabuf(image) => image.view,
            Self::Static(image) => image.view,
        }
    }

    fn height(self) -> u32 {
        match self {
            Self::Dmabuf(image) => image.height,
            Self::Static(image) => image.height,
        }
    }

    fn foreign_owned(self) -> bool {
        matches!(self, Self::Dmabuf(_))
    }

    fn linear_filter_supported(self) -> bool {
        match self {
            Self::Dmabuf(image) => image.linear_filter,
            // Static canvases are locally allocated optimal RGBA images;
            // upload_static_canvas_rgba verifies linear filtering before
            // creating one.
            Self::Static(_) => true,
        }
    }

    fn locally_uploaded(self) -> bool {
        matches!(self, Self::Static(_))
    }
}

/// A 1x1 image that stands in for the canvas while `sync()` draws.
///
/// `blend.frag` names the canvas texture unconditionally, so binding 0 must
/// hold a valid image view even on the branch that never samples it —
/// Vulkan requires every *statically used* descriptor to be valid, and a
/// branch the shader does not take at runtime is still a static use. Created
/// on the first `sync()` call and never on a run that only ever blends.
struct PlaceholderImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

/// One slice to blend this frame.
pub struct BlendJob<'a> {
    pub target: &'a DmabufImage,
    /// Index of the transfer table uploaded with `set_transfer`.
    pub output: usize,
    /// Top-left of this slice in canvas pixels.
    pub source_x: u32,
    pub source_y: u32,
    /// Inverse sampling; None preserves integer texel fetch. The warp and
    /// transfer table must describe this target at unit source pixel density.
    pub warp: Option<&'a super::warp::Warp>,
}

/// One output-local transfer table in an atomic [`Gpu::replace_transfers`]
/// update. The table is borrowed only for the duration of the call; the GPU
/// stages its own inactive buffer before changing any active descriptor.
pub struct TransferUpdate<'a> {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub table: &'a [(u16, u8)],
}

/// A pre-packed transfer table. Entries whose high bit is clear retain the
/// legacy fixed `(a,b)` representation. Entries with high bit set are the
/// adaptive representation: bits 0..15 are ramp UNORM16, 16..23 are border
/// coverage UNORM8, 24..27 are configured-footprint coverage `n` (0..=8),
/// and 28..30 are reserved zero. `n == 0` is outside-picture black.
pub struct PackedTransferUpdate<'a> {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub table: &'a [u32],
}

/// Everything that outlives a single `Gpu::new()` call's setup and is shared
/// with every `DmabufImage` it creates, via `Arc`. Kept separate from `Gpu`
/// itself (which holds the pipeline, descriptor pool, and per-frame command
/// buffer — the things only `Gpu` ever touches) precisely so an image can
/// keep working after `Gpu` is dropped.
struct DeviceState {
    // Kept alive for as long as any instance or device function loaded
    // through it is still in use. With the default `loaded` feature this is
    // a `dlopen`'d `libvulkan.so.1`; ash's documented safety contract for
    // `Entry::load` is that no function loaded directly or indirectly from
    // an `Entry` may be called after that `Entry` is dropped, so it rides
    // along inside the same `Arc` as everything loaded from it.
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family: u32,
    external_memory_fd: ash::khr::external_memory_fd::Device,
    drm_format_modifier: ash::ext::image_drm_format_modifier::Device,
    /// What `create_device`'s global-priority ladder granted `queue` — see
    /// `QueuePriority` and the module doc's "Queue priority" section.
    queue_priority: QueuePriority,
}

impl Drop for DeviceState {
    fn drop(&mut self) {
        // Safety: every object ever created from `device`/`instance` is
        // owned either by a `DmabufImage` (destroyed in its own `Drop`,
        // which — because it shares this `Arc` — necessarily runs before
        // this one) or by `Gpu` (same rule). `vkDestroyInstance`'s own
        // requirement, that every child object is already destroyed, is
        // therefore already satisfied by the time this runs.
        // `device_wait_idle` first guards against tearing down anything a
        // submission might still be reading, in case a caller dropped
        // everything without waiting on the last `blend()`'s fence.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// The device extensions every candidate physical device must support.
/// `VK_KHR_dynamic_rendering` is deliberately not listed here: this module
/// requires Vulkan 1.3 (see `create_device`), where dynamic rendering is a
/// core *feature* (`dynamicRendering`) rather than an extension to enable —
/// checked separately in `missing_requirement`.
const REQUIRED_DEVICE_EXTENSIONS: [&CStr; 5] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
    ash::ext::physical_device_drm::NAME,
];

/// A device's push constants, `#[repr(C)]` to match `blend.frag`'s
/// `PushConstants` block byte for byte. The legacy eight-u32 prefix is
/// followed by three vec4 rows, a 16-byte control block, and the source rect.
#[repr(C)]
#[derive(Clone, Copy)]
struct PushConstants {
    source_x: u32,
    source_y: u32,
    width: u32,
    height: u32,
    canvas_height: u32,
    y_invert: u32,
    /// 0 blends the canvas; 1 draws the `sync` pattern from the shape
    /// buffer. See [`Source`].
    mode: u32,
    /// How many shape groups the shader should walk in mode 1; ignored in
    /// mode 0.
    groups: u32,
    inverse_rows: [[f32; 4]; 3],
    center: [f32; 2],
    warp_enabled: u32,
    padding: u32,
    source_rect: [f32; 4],
    dynamic_lift: f32,
    dynamic_maximum: u32,
    dynamic_padding: [u32; 2],
}

impl PushConstants {
    fn canvas(
        source: [u32; 2],
        size: [u32; 2],
        canvas_height: u32,
        y_invert: bool,
        warp: Option<&super::warp::Warp>,
    ) -> Self {
        Self {
            source_x: source[0],
            source_y: source[1],
            width: size[0],
            height: size[1],
            canvas_height,
            y_invert: u32::from(y_invert),
            mode: 0,
            groups: 0,
            inverse_rows: warp.map_or([[0.0; 4]; 3], |w| w.inverse_rows()),
            center: warp.map_or([0.5; 2], |w| w.center()),
            warp_enabled: u32::from(warp.is_some()),
            padding: 0,
            source_rect: warp
                .and_then(|w| w.source_rect())
                .unwrap_or([
                    source[0] as f64,
                    source[1] as f64,
                    size[0] as f64,
                    size[1] as f64,
                ])
                .map(|v| v as f32),
            dynamic_lift: 0.0,
            dynamic_maximum: 0,
            dynamic_padding: [0; 2],
        }
    }

    fn as_bytes(&self) -> &[u8] {
        // Safety: repr(C), all fields initialized, and no implicit padding.
        // Layout tests compare every member with the shipped SPIR-V.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// Resources kept per output index from the first `set_transfer` call
/// onward: the transfer table's SSBO and the descriptor set that points at
/// it (and, once a canvas exists, at the canvas view too).
struct OutputResources {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Bytes currently backing `buffer`; retained for diagnostics and future
    /// allocation reuse decisions. Replacement always stages a new buffer so
    /// the active descriptor is never written in place.
    #[allow(dead_code)]
    capacity: vk::DeviceSize,
    size: [u32; 2],
    /// Persistently mapped — the memory is HOST_COHERENT, so no flush and
    /// no repeated map/unmap is needed to update it.
    #[allow(dead_code)]
    mapped: *mut u8,
    /// The `sync` pattern's shape buffer for this output, bound at 3 and
    /// [`SYNC_SHAPE_SLOTS`] long for the life of the output — created
    /// alongside the first transfer table and carried across every later
    /// reallocation of it, so binding 3 is written exactly once.
    shapes: HostBuffer,
    /// How many shape groups `set_sync_shapes` last wrote, for the draw's
    /// push constants.
    sync_groups: u32,
    descriptor_set: vk::DescriptorSet,
}

/// A host-visible, host-coherent, persistently mapped buffer — what both
/// per-output SSBOs are. Pulled out because the two want identical
/// allocation and identical teardown and differ only in size.
struct HostBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    bytes: vk::DeviceSize,
    coherent: bool,
}

struct StagedTransfer {
    index: usize,
    width: u32,
    height: u32,
    table: HostBuffer,
    /// Present only when this is a previously unseen sparse output index.
    shapes: Option<HostBuffer>,
    descriptor_set: vk::DescriptorSet,
}

// Safety: `mapped` is the only field that is not automatically `Send` (a raw
// pointer). It addresses host-visible memory owned exclusively by this
// struct via `memory`, is never aliased, and `Gpu` (the only place this type
// lives) is itself required to be `Send`, not `Sync` — so it is only ever
// touched from whichever single thread currently owns the `Gpu` it belongs
// to, exactly like every other field here. `HostBuffer` is the same
// argument, one level down.
unsafe impl Send for OutputResources {}
unsafe impl Send for HostBuffer {}

struct PipelineState {
    format: vk::Format,
    pipeline: vk::Pipeline,
}

/// Push constants for the source-space measurement shader. The grid is
/// capped at 256 on either axis; source dimensions describe only logical
/// canvas pixels, never dmabuf allocation padding.
#[repr(C)]
#[derive(Clone, Copy)]
struct MeasurePushConstants {
    source_width: u32,
    source_height: u32,
    grid_width: u32,
    grid_height: u32,
    y_invert: u32,
}

impl MeasurePushConstants {
    fn as_bytes(&self) -> &[u8] {
        // Safety: repr(C) primitive fields with no uninitialized bytes.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

/// Short-lived, bounded private resources for one source measurement. The
/// canvas itself remains foreign-owned; only this 256x256 target is local.
struct MeasurementPass {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    readback: HostBuffer,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_set: vk::DescriptorSet,
    initialized: bool,
}

/// Owns the partially-built optional measurement resources until they can be
/// published as a retained [`MeasurementPass`]. This makes an unavailable
/// measurement path bounded even when a driver fails midway through creation.
struct MeasurementCleanup<'a> {
    device: &'a ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    readback: Option<HostBuffer>,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    fragment: vk::ShaderModule,
    pipeline: vk::Pipeline,
}

impl MeasurementCleanup<'_> {
    fn defuse(mut self, descriptor_set: vk::DescriptorSet) -> MeasurementPass {
        let pass = MeasurementPass {
            image: self.image,
            memory: self.memory,
            view: self.view,
            readback: self.readback.take().expect("readback initialized"),
            descriptor_set_layout: self.descriptor_set_layout,
            descriptor_pool: self.descriptor_pool,
            pipeline_layout: self.pipeline_layout,
            pipeline: self.pipeline,
            descriptor_set,
            initialized: false,
        };
        self.image = vk::Image::null();
        self.memory = vk::DeviceMemory::null();
        self.view = vk::ImageView::null();
        self.descriptor_set_layout = vk::DescriptorSetLayout::null();
        self.descriptor_pool = vk::DescriptorPool::null();
        self.pipeline_layout = vk::PipelineLayout::null();
        self.pipeline = vk::Pipeline::null();
        pass
    }
}

impl Drop for MeasurementCleanup<'_> {
    fn drop(&mut self) {
        unsafe {
            if self.pipeline != vk::Pipeline::null() {
                self.device.destroy_pipeline(self.pipeline, None);
            }
            if self.fragment != vk::ShaderModule::null() {
                self.device.destroy_shader_module(self.fragment, None);
            }
            if self.pipeline_layout != vk::PipelineLayout::null() {
                self.device
                    .destroy_pipeline_layout(self.pipeline_layout, None);
            }
            if self.descriptor_pool != vk::DescriptorPool::null() {
                self.device
                    .destroy_descriptor_pool(self.descriptor_pool, None);
            }
            if self.descriptor_set_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            }
            if self.view != vk::ImageView::null() {
                self.device.destroy_image_view(self.view, None);
            }
            if self.image != vk::Image::null() {
                self.device.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                self.device.free_memory(self.memory, None);
            }
        }
        if let Some(readback) = self.readback.take() {
            readback.destroy(self.device);
        }
    }
}

impl MeasurementPass {
    fn destroy(self, device: &ash::Device) {
        // Safety: called only after the measurement fence has completed (or
        // before any command references these resources on construction
        // failure); all handles are owned by this pass.
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
        self.readback.destroy(device);
    }
}

pub struct Gpu {
    device: Arc<DeviceState>,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    sampler: vk::Sampler,
    vert_module: vk::ShaderModule,
    frag_module: vk::ShaderModule,
    /// Built lazily, on the first `blend()` call, rather than in `new()`:
    /// dynamic rendering bakes the color attachment format into the
    /// pipeline at creation time, but that format (BGRA vs RGBA) is a
    /// property of whichever DRM fourcc the *caller* picks for a `Present`
    /// image — not something `new()` can know before any image exists. A
    /// later `blend()` call with a target of a different format is an error
    /// (see `ensure_pipeline`); in practice every output on one rig shares
    /// one format, so this never matters.
    pipeline: Option<PipelineState>,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    outputs: Vec<Option<OutputResources>>,
    /// Identity of the canvas image every currently-allocated descriptor
    /// set's binding 0 is pointed at, so `blend()` can tell whether it needs
    /// rewriting without doing it every frame — see the note on `blend()`.
    /// In `sync()` mode this is the placeholder below, and the same
    /// change-detection keeps the rewrite to once per mode switch.
    last_canvas: Option<vk::Image>,
    /// Built on the first `sync()` call; see [`PlaceholderImage`].
    placeholder: Option<PlaceholderImage>,
    /// True only after this instance has submitted work that has not yet been
    /// observed complete. This keeps a failed command recording from leaving
    /// later resource replacement waiting forever on an unsignalled fence.
    fence_in_flight: bool,
    /// Shared dynamic-transfer globals. They are pushed for every draw, so a
    /// lift tick never reallocates or rewrites an output table.
    dynamic_lift: f32,
    dynamic_maximum: u32,
    /// Lazily allocated bounded source-measurement target, descriptor set,
    /// readback buffer, and pipeline. Optional adaptive measurement never
    /// participates in `Gpu::new`, so its absence cannot disable blending.
    measurement: Option<MeasurementPass>,
    /// Deterministic post-stage failure used only by the hardware readback
    /// transaction test. `Some(n)` fails just before staging item `n`, after
    /// the preceding inactive tables have proved their cleanup path.
    #[cfg(test)]
    transfer_stage_fail_after: Option<usize>,
}

impl Gpu {
    /// Load the Vulkan loader, create an instance and device, and build the
    /// blend pipeline. `render_node` is the compositor's main device as a
    /// `dev_t` (from dmabuf feedback) — pick the physical device whose
    /// `VK_EXT_physical_device_drm` render or primary major/minor match it;
    /// if `None` or no match, pick the first device that has every required
    /// extension, preferring discrete GPUs.
    pub fn new(render_node: Option<u64>) -> anyhow::Result<Gpu> {
        // Safety: dlopen's the system Vulkan loader. `entry` is folded into
        // `DeviceState` below (or, on an early failure, dropped here with
        // nothing yet loaded through it to outlive it).
        let entry = unsafe { ash::Entry::load() }
            .map_err(|error| anyhow!("loading libvulkan.so.1: {error}"))?;

        let app_name = c"suede";
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .engine_name(app_name)
            .api_version(vk::API_VERSION_1_3);
        let instance_create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        // Safety: `instance_create_info`'s only borrow, `app_info`, outlives
        // this call.
        let instance = unsafe { entry.create_instance(&instance_create_info, None) }
            .context("vkCreateInstance")?;

        let device_state = match Self::create_device(&entry, &instance, render_node) {
            Ok(state) => state,
            Err(error) => {
                // Safety: nothing has been created from `instance` on this
                // path (physical device selection and queries create
                // nothing; `create_device` destroys anything it itself
                // created before returning `Err`).
                unsafe { instance.destroy_instance(None) };
                return Err(error);
            }
        };

        Self::from_device(device_state)
    }

    fn create_device(
        entry: &ash::Entry,
        instance: &ash::Instance,
        render_node: Option<u64>,
    ) -> anyhow::Result<Arc<DeviceState>> {
        let physical_device = pick_physical_device(instance, render_node)?;
        let queue_family = pick_queue_family(instance, physical_device)?;

        let mut device_extensions: Vec<*const c_char> = REQUIRED_DEVICE_EXTENSIONS
            .iter()
            .map(|name| name.as_ptr())
            .collect();

        // Global queue priority — see the module doc's "Queue priority"
        // section for why this is worth the trouble. Entirely best-effort:
        // every step below that finds no extension, no queryable tier, or a
        // refused request simply falls through toward the `None` rung of
        // the ladder, which is exactly the device-creation call this
        // function made before any of this existed.
        let global_priority_extensions =
            supported_global_priority_extensions(instance, physical_device);
        device_extensions.extend(global_priority_extensions.iter().map(|name| name.as_ptr()));
        let listed_priorities = if global_priority_extensions.is_empty() {
            Vec::new()
        } else {
            queried_queue_priorities(instance, physical_device, queue_family)
        };

        // Highest tier first. When the driver listed its supported tiers,
        // only the listed ones are tried; when the extension is present but
        // querying was not (the plain `VK_EXT_global_priority`, with no
        // `_query` sibling — see `supported_global_priority_extensions`),
        // REALTIME/HIGH/MEDIUM are tried blind and `vkCreateDevice`'s result
        // rules a tier out. `None` — no explicit request at all — always
        // ends the ladder, as the one rung that must not fail.
        let mut ladder: Vec<Option<vk::QueueGlobalPriorityKHR>> = Vec::new();
        if !global_priority_extensions.is_empty() {
            let tiers = [
                vk::QueueGlobalPriorityKHR::REALTIME,
                vk::QueueGlobalPriorityKHR::HIGH,
                vk::QueueGlobalPriorityKHR::MEDIUM,
            ];
            if listed_priorities.is_empty() {
                ladder.extend(tiers.into_iter().map(Some));
            } else {
                ladder.extend(
                    tiers
                        .into_iter()
                        .filter(|tier| listed_priorities.contains(tier))
                        .map(Some),
                );
            }
        }
        ladder.push(None);

        let mut device = None;
        let mut last_error = None;
        let mut queue_priority = QueuePriority::Default;
        for candidate in ladder {
            match Self::try_create_device(
                instance,
                physical_device,
                queue_family,
                &device_extensions,
                candidate,
            ) {
                Ok(created) => {
                    queue_priority = priority_tier(candidate);
                    device = Some(created);
                    break;
                }
                // VK_ERROR_NOT_PERMITTED_KHR is the documented refusal for a
                // priority this process is not allowed (REALTIME usually
                // needs CAP_SYS_NICE) — expected, and quietly worth trying
                // the next rung for. Any other error is kept too, in case
                // every rung fails and there is something real to report;
                // either way the ladder still ends at `None`, which never
                // chains the priority create-info struct at all.
                Err(error) => last_error = Some(error),
            }
        }
        let device = match device {
            Some(device) => device,
            None => {
                return Err(last_error.unwrap_or(vk::Result::ERROR_INITIALIZATION_FAILED))
                    .context("vkCreateDevice");
            }
        };

        // Safety: `queue_family` was just chosen because it exists and has
        // a queue at index 0 (every queue family reports `queue_count >= 1`).
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let external_memory_fd = ash::khr::external_memory_fd::Device::new(instance, &device);
        let drm_format_modifier =
            ash::ext::image_drm_format_modifier::Device::new(instance, &device);

        Ok(Arc::new(DeviceState {
            _entry: entry.clone(),
            instance: instance.clone(),
            device,
            physical_device,
            queue,
            queue_family,
            external_memory_fd,
            drm_format_modifier,
            queue_priority,
        }))
    }

    /// One `vkCreateDevice` attempt for `create_device`'s priority ladder:
    /// the same device (extensions, the one Vulkan 1.3 feature this module
    /// needs, the one queue) every time, differing only in whether a
    /// `VkDeviceQueueGlobalPriorityCreateInfoKHR` asking for `priority` is
    /// chained onto that queue.
    fn try_create_device(
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
        queue_family: u32,
        device_extensions: &[*const c_char],
        priority: Option<vk::QueueGlobalPriorityKHR>,
    ) -> Result<ash::Device, vk::Result> {
        let queue_priorities = [1.0f32];
        let mut global_priority_info = priority.map(|priority| {
            vk::DeviceQueueGlobalPriorityCreateInfoKHR::default().global_priority(priority)
        });
        let mut queue_create_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&queue_priorities);
        if let Some(global_priority_info) = global_priority_info.as_mut() {
            queue_create_info = queue_create_info.push_next(global_priority_info);
        }
        let queue_create_infos = [queue_create_info];
        // `dynamicRendering` is the one 1.3 feature this module needs;
        // `missing_requirement` already confirmed the device reports it.
        let mut vulkan_1_3_features =
            vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
        let device_create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_create_infos)
            .enabled_extension_names(device_extensions)
            .push_next(&mut vulkan_1_3_features);
        // Safety: every pointer this chain holds (`queue_create_infos` and,
        // transitively, `global_priority_info`; `device_extensions`;
        // `vulkan_1_3_features`) outlives this call.
        unsafe { instance.create_device(physical_device, &device_create_info, None) }
    }

    /// Everything after the device exists: the pipeline layout, descriptor
    /// machinery, sampler, shader modules, and the one command buffer and
    /// fence `blend()` reuses every frame.
    ///
    /// A failure partway through leaks whichever of these were already
    /// created (their owning `device`/`instance` are not leaked — those are
    /// destroyed when the `Arc<DeviceState>` this function was given drops
    /// at the end of the failing call). That is a deliberate simplification:
    /// this only runs once, at startup, and the only realistic way to fail
    /// here is host memory exhaustion, at which point a few descriptor pool
    /// or shader module bytes are the least of the process's problems.
    fn from_device(device_state: Arc<DeviceState>) -> anyhow::Result<Gpu> {
        let device = &device_state.device;

        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .min_lod(0.0)
            .max_lod(0.0);
        // Safety: no pNext chain, no borrowed slices.
        let sampler =
            unsafe { device.create_sampler(&sampler_info, None) }.context("vkCreateSampler")?;

        // Binding 0: the canvas, a plain sampled image (not a combined
        // image sampler — see the module doc on naga). Binding 1: the
        // sampler, baked in as immutable since it is always this one
        // linear/clamp sampler, so it never needs a descriptor write.
        // Binding 2: the per-output transfer table. Binding 3: the
        // per-output `sync` pattern shapes (see `set_sync_shapes`), which
        // every set carries whether or not that pattern is ever shown —
        // `blend.frag` names the buffer unconditionally, so the descriptor
        // has to be valid even on the runs whose shader never reads it.
        let immutable_samplers = [sampler];
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .immutable_samplers(&immutable_samplers),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // Safety: `bindings` (and the `immutable_samplers` it borrows)
        // outlive this call.
        let descriptor_set_layout =
            unsafe { device.create_descriptor_set_layout(&layout_info, None) }
                .context("vkCreateDescriptorSetLayout")?;

        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(MAX_OUTPUTS as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(MAX_OUTPUTS as u32),
            // Two per set: the transfer table and the sync shapes.
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(2 * MAX_OUTPUTS as u32),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(MAX_OUTPUTS as u32)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = unsafe { device.create_descriptor_pool(&pool_info, None) }
            .context("vkCreateDescriptorPool")?;

        let set_layouts = [descriptor_set_layout];
        let push_constant_ranges = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::FRAGMENT,
            offset: 0,
            size: std::mem::size_of::<PushConstants>() as u32,
        }];
        let pipeline_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        let pipeline_layout = unsafe { device.create_pipeline_layout(&pipeline_layout_info, None) }
            .context("vkCreatePipelineLayout")?;

        let vert_module = create_shader_module(device, VERT_SPV, "vertex")?;
        let frag_module = create_shader_module(device, FRAG_SPV, "fragment")?;

        // `RESET_COMMAND_BUFFER`: `blend()` re-records the same buffer every
        // frame rather than allocating a fresh one.
        let command_pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(device_state.queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { device.create_command_pool(&command_pool_info, None) }
            .context("vkCreateCommandPool")?;
        let command_buffer_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = unsafe { device.allocate_command_buffers(&command_buffer_info) }
            .context("vkAllocateCommandBuffers")?[0];

        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .context("vkCreateFence")?;

        Ok(Gpu {
            device: device_state,
            descriptor_set_layout,
            descriptor_pool,
            pipeline_layout,
            sampler,
            vert_module,
            frag_module,
            pipeline: None,
            command_pool,
            command_buffer,
            fence,
            outputs: Vec::new(),
            last_canvas: None,
            placeholder: None,
            fence_in_flight: false,
            dynamic_lift: 0.0,
            dynamic_maximum: 0,
            measurement: None,
            #[cfg(test)]
            transfer_stage_fail_after: None,
        })
    }

    /// Human-readable device name and driver version, for a log line.
    pub fn describe(&self) -> String {
        // Safety: read-only query; `properties` is fully written by the
        // driver before use.
        let properties = unsafe {
            self.device
                .instance
                .get_physical_device_properties(self.device.physical_device)
        };
        // Safety: `device_name` is a NUL-terminated byte array the driver
        // fills in as part of `properties` above.
        let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy();
        format!(
            "{name} (driver {:#x}, Vulkan {}.{}.{}, queue priority {})",
            properties.driver_version,
            vk::api_version_major(properties.api_version),
            vk::api_version_minor(properties.api_version),
            vk::api_version_patch(properties.api_version),
            self.device.queue_priority.label(),
        )
    }

    /// The subset of `candidates` this device can use for `fourcc` with the
    /// features `usage` needs, in the caller's order. Empty means "use shm".
    pub fn supported_modifiers(&self, fourcc: u32, candidates: &[u64], usage: Usage) -> Vec<u64> {
        let Ok(format) = format_for_fourcc(fourcc) else {
            return Vec::new();
        };
        if candidates.is_empty() {
            return Vec::new();
        }
        let modifiers = self.modifier_properties(format);
        let required_tiling_feature = match usage {
            Usage::Capture => vk::FormatFeatureFlags::SAMPLED_IMAGE,
            Usage::Present => vk::FormatFeatureFlags::COLOR_ATTACHMENT,
        };
        let usage_flags = image_usage_flags(usage);

        candidates
            .iter()
            .copied()
            .filter(|&modifier| {
                let tiling_ok = modifiers.iter().any(|properties| {
                    properties.drm_format_modifier == modifier
                        && properties.drm_format_modifier_plane_count == 1
                        && properties
                            .drm_format_modifier_tiling_features
                            .contains(required_tiling_feature)
                });
                tiling_ok && self.image_format_creatable(format, modifier, usage_flags)
            })
            .collect()
    }

    /// Capture modifiers suitable for the requested sampling policy.
    ///
    /// When warping is required, only modifiers advertising linear sampled
    /// image support are usable. For an exact rectangular capture, retain an
    /// exact-only fallback but put filtering-capable modifiers first, so a
    /// later live warp can normally start without rebuilding capture buffers.
    /// The order within each group remains the compositor's advertised order.
    pub fn supported_capture_modifiers(
        &self,
        fourcc: u32,
        candidates: &[u64],
        require_linear_filter: bool,
    ) -> Vec<u64> {
        let Ok(format) = format_for_fourcc(fourcc) else {
            return Vec::new();
        };
        let supported = self.supported_modifiers(fourcc, candidates, Usage::Capture);
        let properties = self.modifier_properties(format);
        let linear = |modifier| {
            properties.iter().any(|property| {
                property.drm_format_modifier == modifier
                    && property
                        .drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR)
            })
        };
        if require_linear_filter {
            supported
                .into_iter()
                .filter(|&modifier| linear(modifier))
                .collect()
        } else {
            let (linear, exact_only): (Vec<_>, Vec<_>) = supported
                .into_iter()
                .partition(|&modifier| linear(modifier));
            linear.into_iter().chain(exact_only).collect()
        }
    }

    /// Whether this device can use the local RGBA canvas required for static
    /// GPU patterns. This is intentionally only a format-feature capability
    /// query: an allocation or upload failure remains a real runtime error,
    /// never a reason to silently select a different renderer after buffers
    /// have been created.
    pub fn static_canvas_supported(&self) -> bool {
        // Safety: read-only format-property query.
        let features = unsafe {
            self.device.instance.get_physical_device_format_properties(
                self.device.physical_device,
                vk::Format::R8G8B8A8_UNORM,
            )
        }
        .optimal_tiling_features;
        static_canvas_features_supported(features)
    }

    /// Query the format features of each DRM modifier, rather than assuming
    /// optimal-tiling support applies to a compositor-compatible image.
    fn modifier_properties(&self, format: vk::Format) -> Vec<vk::DrmFormatModifierPropertiesEXT> {
        let instance = &self.device.instance;
        let physical_device = self.device.physical_device;

        // Query-twice: first the count, then the properties themselves —
        // `vkGetPhysicalDeviceFormatProperties2` fills in however many
        // `VkDrmFormatModifierPropertiesEXT` it has via the count/pointer
        // pair in `VkDrmFormatModifierPropertiesListEXT`, the array
        // equivalent of Vulkan's usual `vkEnumerate*` convention.
        let mut count_query = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut probe = vk::FormatProperties2::default().push_next(&mut count_query);
        // Safety: `probe`'s chain outlives the call.
        unsafe {
            instance.get_physical_device_format_properties2(physical_device, format, &mut probe)
        };

        let mut modifiers = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            count_query.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut modifiers);
        let mut properties2 = vk::FormatProperties2::default().push_next(&mut list);
        // Safety: `properties2`'s chain (including `modifiers`, sized above
        // from the driver's own reported count) outlives the call.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical_device,
                format,
                &mut properties2,
            )
        };

        modifiers
    }

    /// Whether the device can actually create+export an image for `format`
    /// at `modifier` with `usage` — the tiling-feature check above says the
    /// *format* supports it in principle; this confirms creation itself
    /// (`vkGetPhysicalDeviceImageFormatProperties2` can still refuse a
    /// specific combination) and that the resulting memory is exportable.
    fn image_format_creatable(
        &self,
        format: vk::Format,
        modifier: u64,
        usage: vk::ImageUsageFlags,
    ) -> bool {
        let instance = &self.device.instance;
        let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .push_next(&mut modifier_info)
            .push_next(&mut external_info);

        let mut external_properties = vk::ExternalImageFormatProperties::default();
        let mut image_properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        // Safety: both chains (`format_info`'s and `image_properties`'s)
        // outlive this call. A `Result::Err` here (most often
        // `ERROR_FORMAT_NOT_SUPPORTED`) is an ordinary "no" for this
        // modifier, not a bug — every caller of this method treats it as
        // such by construction (`bool` return).
        let queried = unsafe {
            self.device
                .instance
                .get_physical_device_image_format_properties2(
                    self.device.physical_device,
                    &format_info,
                    &mut image_properties,
                )
        };
        let _ = instance; // silence the otherwise-unused alias on some paths
        queried.is_ok()
            && external_properties
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
    }

    /// Allocate an exportable image. `modifiers` must be non-empty and come
    /// from `supported_modifiers`; the driver picks one and `DmabufImage::modifier`
    /// says which.
    pub fn create_image(
        &self,
        width: u32,
        height: u32,
        fourcc: u32,
        modifiers: &[u64],
        usage: Usage,
    ) -> anyhow::Result<DmabufImage> {
        if modifiers.is_empty() {
            bail!(
                "create_image: modifiers must be non-empty (empty means the caller should use shm)"
            );
        }
        let format = format_for_fourcc(fourcc)?;
        let device = &self.device.device;
        let usage_flags = image_usage_flags(usage);

        let mut modifier_list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(modifiers);
        let mut external_image_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage_flags)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut modifier_list)
            .push_next(&mut external_image_info);
        // Safety: every pointer `image_info`'s chain holds (`modifiers`,
        // `modifier_list`, `external_image_info`) outlives this call.
        let image = unsafe { device.create_image(&image_info, None) }.context("vkCreateImage")?;
        // From here, `cleanup` destroys `image` (and whatever else it is
        // told about) on any early return — see its doc comment.
        let mut cleanup = Cleanup {
            device,
            image,
            memory: None,
            view: None,
        };

        let mut dedicated_requirements = vk::MemoryDedicatedRequirements::default();
        let mut requirements2 =
            vk::MemoryRequirements2::default().push_next(&mut dedicated_requirements);
        let requirements_info = vk::ImageMemoryRequirementsInfo2::default().image(image);
        // Safety: `requirements2`'s chain outlives the call.
        unsafe { device.get_image_memory_requirements2(&requirements_info, &mut requirements2) };
        let requirements = requirements2.memory_requirements;

        // Safety: read-only query.
        let memory_properties = unsafe {
            self.device
                .instance
                .get_physical_device_memory_properties(self.device.physical_device)
        };
        let memory_type_index = memory_type_index(
            &memory_properties,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| anyhow!("no DEVICE_LOCAL memory type fits a {width}x{height} image"))?;

        let mut export_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated_alloc = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut dedicated_alloc)
            .push_next(&mut export_info);
        // Safety: `allocate_info`'s chain outlives the call.
        let memory =
            unsafe { device.allocate_memory(&allocate_info, None) }.context("vkAllocateMemory")?;
        cleanup.memory = Some(memory);

        // Safety: `image` and `memory` were both just created, `image` is
        // not yet bound to anything, and `memory` was sized and typed from
        // `image`'s own requirements above.
        unsafe { device.bind_image_memory(image, memory, 0) }.context("vkBindImageMemory")?;

        if usage == Usage::Present {
            // See the module doc: we are the first writer of a `Present`
            // image, so we owe it the one-off UNDEFINED -> GENERAL
            // transition before it is ever used.
            self.transition_to_general(image)
                .context("initial UNDEFINED -> GENERAL transition")?;
        }

        let fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // Safety: exports a new reference to memory this call owns; per
        // `VK_KHR_external_memory_fd`, the driver hands back a fresh,
        // uniquely-owned fd each call.
        let raw_fd = unsafe { self.device.external_memory_fd.get_memory_fd(&fd_info) }
            .context("vkGetMemoryFdKHR")?;
        // Safety: `raw_fd` was just returned by `vkGetMemoryFdKHR` above,
        // which transfers ownership of a new fd to the caller.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        let mut modifier_properties = vk::ImageDrmFormatModifierPropertiesEXT::default();
        // Safety: output-only struct, written by the driver, not read
        // beforehand.
        unsafe {
            self.device
                .drm_format_modifier
                .get_image_drm_format_modifier_properties(image, &mut modifier_properties)
        }
        .context("vkGetImageDrmFormatModifierPropertiesEXT")?;

        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
            mip_level: 0,
            array_layer: 0,
        };
        // Safety: `image` was created with `DRM_FORMAT_MODIFIER_EXT` tiling,
        // and every modifier `supported_modifiers` ever offers has exactly
        // one memory plane (checked there via `drm_format_modifier_plane_count
        // == 1`), so plane 0 is the only, and a valid, plane to ask about.
        let layout = unsafe { device.get_image_subresource_layout(image, subresource) };

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        // Safety: `image` is bound to memory; the view is destroyed before
        // the image in both `Cleanup::drop` and `DmabufImage::drop`.
        let view =
            unsafe { device.create_image_view(&view_info, None) }.context("vkCreateImageView")?;
        cleanup.view = Some(view);

        cleanup.defuse();
        Ok(DmabufImage {
            width,
            height,
            fourcc,
            modifier: modifier_properties.drm_format_modifier,
            stride: layout.row_pitch as u32,
            offset: layout.offset as u32,
            fd,
            format,
            linear_filter: self.modifier_properties(format).iter().any(|p| {
                p.drm_format_modifier == modifier_properties.drm_format_modifier
                    && p.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR)
            }),
            image,
            memory,
            view,
            device: Arc::clone(&self.device),
        })
    }

    /// Upload an RGBA8888 static canvas for the normal blend shader.
    ///
    /// This is deliberately a local optimal image rather than a fabricated
    /// dmabuf: static patterns have no compositor writer or reader, so their
    /// canvas never participates in FOREIGN queue-family ownership transfer.
    /// The upload completes before this returns, making the returned canvas
    /// immediately safe to reuse for any number of [`Gpu::blend_static`]
    /// calls.
    pub fn upload_static_canvas_rgba(
        &mut self,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> anyhow::Result<StaticCanvas> {
        if width == 0 || height == 0 {
            bail!("upload_static_canvas_rgba: dimensions must be nonzero");
        }
        let bytes = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| anyhow!("upload_static_canvas_rgba: dimensions overflow"))?;
        let bytes = usize::try_from(bytes)
            .context("upload_static_canvas_rgba: dimensions do not fit in host memory")?;
        if pixels.len() != bytes {
            bail!(
                "upload_static_canvas_rgba: got {} bytes, expected {width}x{height}x4 = {bytes}",
                pixels.len()
            );
        }
        if !self.static_canvas_supported() {
            bail!(
                "upload_static_canvas_rgba: optimal RGBA canvas lacks sampled linear filtering or transfer-destination support"
            );
        }
        self.wait_for_pending_work()
            .context("waiting before static canvas upload")?;

        let device_state = Arc::clone(&self.device);
        let device = &device_state.device;
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // Safety: no pNext chain.
        let image = unsafe { device.create_image(&image_info, None) }
            .context("vkCreateImage (static canvas)")?;
        let mut cleanup = Cleanup {
            device,
            image,
            memory: None,
            view: None,
        };
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        // Safety: read-only query.
        let memory_properties = unsafe {
            self.device
                .instance
                .get_physical_device_memory_properties(self.device.physical_device)
        };
        let memory_type = memory_type_index(
            &memory_properties,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| {
            anyhow!("no DEVICE_LOCAL memory type fits a {width}x{height} static canvas")
        })?;
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // Safety: no pNext chain.
        let memory = unsafe { device.allocate_memory(&allocate_info, None) }
            .context("vkAllocateMemory (static canvas)")?;
        cleanup.memory = Some(memory);
        // Safety: `memory` was selected and sized from `image`'s requirements.
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("vkBindImageMemory (static canvas)")?;
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .subresource_range(color_subresource_range());
        // Safety: the image is bound and the view is cleaned up before it.
        let view = unsafe { device.create_image_view(&view_info, None) }
            .context("vkCreateImageView (static canvas)")?;
        cleanup.view = Some(view);

        let upload = allocate_host_buffer_with_usage(
            &device_state,
            bytes as vk::DeviceSize,
            vk::BufferUsageFlags::TRANSFER_SRC,
        )
        .context("allocating static canvas upload buffer")?;
        // Safety: exact-size, mapped HOST_COHERENT upload allocation.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), upload.mapped, bytes) };

        let upload_result = (|| -> anyhow::Result<()> {
            // Safety: this command buffer is idle after wait_for_pending_work.
            unsafe {
                device.reset_command_buffer(
                    self.command_buffer,
                    vk::CommandBufferResetFlags::empty(),
                )?;
                device.begin_command_buffer(
                    self.command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                let to_transfer = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(color_subresource_range());
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_transfer],
                );
                let region = vk::BufferImageCopy::default()
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    });
                device.cmd_copy_buffer_to_image(
                    self.command_buffer,
                    upload.buffer,
                    image,
                    vk::ImageLayout::GENERAL,
                    &[region],
                );
                let readable = vk::ImageMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(color_subresource_range());
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[readable],
                );
                device.end_command_buffer(self.command_buffer)?;
                device.reset_fences(&[self.fence])?;
                let command_buffers = [self.command_buffer];
                device.queue_submit(
                    self.device.queue,
                    &[vk::SubmitInfo::default().command_buffers(&command_buffers)],
                    self.fence,
                )?;
            }
            self.fence_in_flight = true;
            // Safety: the upload submission above owns this fence.
            unsafe { device.wait_for_fences(&[self.fence], true, u64::MAX) }?;
            self.fence_in_flight = false;
            Ok(())
        })();
        // Safety: the upload is complete on success. On an error after a
        // successful submit, `fence_in_flight` remains true so Gpu's later
        // cleanup waits before the staging buffer can be destroyed.
        if upload_result.is_ok() {
            upload.destroy(device);
        } else if self.fence_in_flight {
            let _ = self.wait_for_pending_work();
            upload.destroy(device);
        } else {
            upload.destroy(device);
        }
        upload_result.context("uploading static canvas")?;

        cleanup.defuse();
        Ok(StaticCanvas {
            width,
            height,
            image,
            memory,
            view,
            device: device_state,
        })
    }

    /// A one-off submit that carries a freshly created, not-yet-shared
    /// image from `UNDEFINED` straight to `GENERAL` — see the module doc.
    /// No queue family transfer here: the image has not been handed to the
    /// compositor yet, so there is no `FOREIGN` ownership to acquire from,
    /// only an ordinary local layout transition.
    fn transition_to_general(&self, image: vk::Image) -> anyhow::Result<()> {
        let device = &self.device.device;
        let pool_info =
            vk::CommandPoolCreateInfo::default().queue_family_index(self.device.queue_family);
        // Safety: no pNext chain.
        let pool = unsafe { device.create_command_pool(&pool_info, None) }
            .context("vkCreateCommandPool (initial layout transition)")?;
        let result = self.transition_to_general_with_pool(device, pool, image);
        // Safety: `pool` was created just above in this function; destroying
        // it also frees the one command buffer allocated from it below.
        unsafe { device.destroy_command_pool(pool, None) };
        result
    }

    fn transition_to_general_with_pool(
        &self,
        device: &ash::Device,
        pool: vk::CommandPool,
        image: vk::Image,
    ) -> anyhow::Result<()> {
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // Safety: `pool` was just created and is not in use elsewhere.
        let command_buffer = unsafe { device.allocate_command_buffers(&allocate_info) }
            .context("vkAllocateCommandBuffers")?[0];
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { device.begin_command_buffer(command_buffer, &begin_info) }
            .context("vkBeginCommandBuffer")?;

        let barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(color_subresource_range());
        // Safety: `command_buffer` is recording (just began above); `image`
        // was created by our caller moments ago and is not in use anywhere
        // else yet.
        unsafe {
            device.cmd_pipeline_barrier(
                command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
        unsafe { device.end_command_buffer(command_buffer) }.context("vkEndCommandBuffer")?;

        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .context("vkCreateFence (initial layout transition)")?;
        let result = (|| -> anyhow::Result<()> {
            let command_buffers = [command_buffer];
            let submit = vk::SubmitInfo::default().command_buffers(&command_buffers);
            // Safety: `command_buffer` finished recording above; `fence` was
            // just created, unsignalled, and used by nothing else.
            unsafe { device.queue_submit(self.device.queue, &[submit], fence) }
                .context("vkQueueSubmit")?;
            unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }
                .context("vkWaitForFences")?;
            Ok(())
        })();
        // Safety: `fence` is done being useful either way — waited on above
        // when submission succeeded, never signalled (so nothing could be
        // waiting on it) when it did not.
        unsafe { device.destroy_fence(fence, None) };
        result
    }

    /// Upload output `index`'s per-pixel transfer table, row-major at the
    /// output's size, `(a, b)` meaning `out = ((a * in) >> 8) + b` per channel —
    /// exactly the CPU slicer's `Presenter.transfer`. Replaces any previous table
    /// for that index. Called once per output at startup.
    pub fn set_transfer(
        &mut self,
        index: usize,
        width: u32,
        height: u32,
        table: &[(u16, u8)],
    ) -> anyhow::Result<()> {
        self.replace_transfers(&[TransferUpdate {
            index,
            width,
            height,
            table,
        }])
    }

    /// Replace several output-local transfer tables as one bounded resource
    /// transaction. Every update is validated and uploaded into an inactive
    /// host-visible buffer before any active descriptor is touched. The last
    /// completed GPU submission is then awaited before descriptors are
    /// rebound or old buffers are retired. A failed validation/allocation or
    /// fence wait leaves every active output unchanged; sparse indices retain
    /// their existing slots and outputs absent from `updates` are untouched.
    pub fn replace_transfers(&mut self, updates: &[TransferUpdate<'_>]) -> anyhow::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let mut seen = [false; MAX_OUTPUTS];
        let mut prepared = Vec::with_capacity(updates.len());
        for update in updates {
            if update.index >= MAX_OUTPUTS {
                bail!(
                    "replace_transfers: output index {} exceeds the {MAX_OUTPUTS}-output descriptor pool",
                    update.index
                );
            }
            if seen[update.index] {
                bail!(
                    "replace_transfers: output index {} appears more than once",
                    update.index
                );
            }
            seen[update.index] = true;
            if update.width == 0 || update.height == 0 {
                bail!("replace_transfers: dimensions must be nonzero");
            }
            let entries = update
                .width
                .checked_mul(update.height)
                .ok_or_else(|| anyhow!("replace_transfers: dimensions overflow"))?;
            let entries = usize::try_from(entries)
                .context("replace_transfers: dimensions do not fit in host memory")?;
            if update.table.len() != entries {
                bail!(
                    "replace_transfers: output {} has {} entries, expected {}x{} = {entries}",
                    update.index,
                    update.table.len(),
                    update.width,
                    update.height
                );
            }
            if update.table.iter().any(|&(a, _)| a > 256) {
                bail!(
                    "replace_transfers: output {} has a gain outside 0..=256",
                    update.index
                );
            }
            let packed: Vec<u32> = update
                .table
                .iter()
                .map(|&(a, b)| pack_transfer(a, b))
                .collect();
            let unchanged = self
                .outputs
                .get(update.index)
                .and_then(Option::as_ref)
                .is_some_and(|output| {
                    output.size == [update.width, update.height]
                        && output.capacity
                            == (packed.len() * std::mem::size_of::<u32>()) as vk::DeviceSize
                        // Safety: `output.mapped` covers the active table's
                        // complete capacity and remains mapped for its life.
                        && unsafe {
                            std::slice::from_raw_parts(
                                output.mapped.cast::<u32>(),
                                packed.len(),
                            ) == packed.as_slice()
                        }
                });
            if unchanged {
                continue;
            }
            prepared.push((update.index, update.width, update.height, packed));
        }

        self.replace_prepared_transfers(prepared)
    }

    /// Install an already packed table. This is the dynamic-transfer entry
    /// point; callers that need legacy byte-exact fixed transfer should keep
    /// using [`Gpu::set_transfer`] or [`Gpu::replace_transfers`].
    pub fn set_packed_transfer(
        &mut self,
        index: usize,
        width: u32,
        height: u32,
        table: &[u32],
    ) -> anyhow::Result<()> {
        self.replace_packed_transfers(&[PackedTransferUpdate {
            index,
            width,
            height,
            table,
        }])
    }

    /// Atomically replace one or more tables in the documented packed
    /// representation. Fixed and dynamic entries may coexist across outputs,
    /// but every dynamic entry is range-checked before any active descriptor
    /// can change.
    pub fn replace_packed_transfers(
        &mut self,
        updates: &[PackedTransferUpdate<'_>],
    ) -> anyhow::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut seen = [false; MAX_OUTPUTS];
        let mut prepared = Vec::with_capacity(updates.len());
        for update in updates {
            if update.index >= MAX_OUTPUTS {
                bail!("replace_packed_transfers: output index {} exceeds the {MAX_OUTPUTS}-output descriptor pool", update.index);
            }
            if seen[update.index] {
                bail!(
                    "replace_packed_transfers: output index {} appears more than once",
                    update.index
                );
            }
            seen[update.index] = true;
            if update.width == 0 || update.height == 0 {
                bail!("replace_packed_transfers: dimensions must be nonzero");
            }
            let entries = update
                .width
                .checked_mul(update.height)
                .ok_or_else(|| anyhow!("replace_packed_transfers: dimensions overflow"))?;
            let entries = usize::try_from(entries)
                .context("replace_packed_transfers: dimensions do not fit in host memory")?;
            if update.table.len() != entries {
                bail!("replace_packed_transfers: output {} has {} entries, expected {}x{} = {entries}", update.index, update.table.len(), update.width, update.height);
            }
            for &entry in update.table {
                validate_packed_transfer(entry)?;
            }
            let packed = update.table.to_vec();
            let unchanged = self.outputs.get(update.index).and_then(Option::as_ref).is_some_and(|output| {
                output.size == [update.width, update.height]
                    && output.capacity == (packed.len() * std::mem::size_of::<u32>()) as vk::DeviceSize
                    // Safety: the active persistently-mapped table covers its capacity.
                    && unsafe { std::slice::from_raw_parts(output.mapped.cast::<u32>(), packed.len()) == packed.as_slice() }
            });
            if !unchanged {
                prepared.push((update.index, update.width, update.height, packed));
            }
        }
        self.replace_prepared_transfers(prepared)
    }

    fn replace_prepared_transfers(
        &mut self,
        prepared: Vec<(usize, u32, u32, Vec<u32>)>,
    ) -> anyhow::Result<()> {
        if prepared.is_empty() {
            return Ok(());
        }

        let device = Arc::clone(&self.device);
        let dev = &device.device;
        let mut staged: Vec<StagedTransfer> = Vec::with_capacity(prepared.len());
        for (index, width, height, packed) in prepared {
            #[cfg(test)]
            if self.transfer_stage_fail_after == Some(staged.len()) {
                self.transfer_stage_fail_after = None;
                for item in &staged {
                    item.table.destroy(dev);
                    if let Some(shapes) = &item.shapes {
                        shapes.destroy(dev);
                    }
                }
                bail!("replace_transfers: injected staging failure");
            }
            let table_bytes = (packed.len() * std::mem::size_of::<u32>()) as vk::DeviceSize;
            let table = match allocate_host_buffer(&device, table_bytes) {
                Ok(table) => table,
                Err(error) => {
                    for item in &staged {
                        item.table.destroy(dev);
                        if let Some(shapes) = &item.shapes {
                            shapes.destroy(dev);
                        }
                    }
                    return Err(error).context("the transfer table");
                }
            };
            // Safety: `table.mapped` addresses this newly allocated coherent
            // buffer, which is not referenced by any descriptor yet.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    packed.as_ptr().cast::<u8>(),
                    table.mapped,
                    table_bytes as usize,
                );
            }
            let fresh_shapes = if self.outputs.get(index).and_then(Option::as_ref).is_none() {
                let slot_bytes =
                    (SYNC_SHAPE_SLOTS * std::mem::size_of::<[u32; 4]>()) as vk::DeviceSize;
                match allocate_host_buffer(&device, slot_bytes) {
                    Ok(shapes) => Some(shapes),
                    Err(error) => {
                        table.destroy(dev);
                        for item in &staged {
                            item.table.destroy(dev);
                            if let Some(shapes) = &item.shapes {
                                shapes.destroy(dev);
                            }
                        }
                        return Err(error).context("the sync pattern's shape buffer");
                    }
                }
            } else {
                None
            };
            staged.push(StagedTransfer {
                index,
                width,
                height,
                table,
                shapes: fresh_shapes,
                descriptor_set: vk::DescriptorSet::null(),
            });
        }

        if let Err(error) = self.wait_for_pending_work() {
            for item in &staged {
                item.table.destroy(dev);
                if let Some(shapes) = &item.shapes {
                    shapes.destroy(dev);
                }
            }
            return Err(error).context("waiting before transfer replacement");
        }

        let missing = staged.iter().filter(|item| item.shapes.is_some()).count();
        if missing != 0 {
            let layouts = vec![self.descriptor_set_layout; missing];
            let allocate_info = vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(self.descriptor_pool)
                .set_layouts(&layouts);
            let sets = match unsafe { dev.allocate_descriptor_sets(&allocate_info) } {
                Ok(sets) => sets,
                Err(error) => {
                    for item in &staged {
                        item.table.destroy(dev);
                        if let Some(shapes) = &item.shapes {
                            shapes.destroy(dev);
                        }
                    }
                    return Err(error).context("vkAllocateDescriptorSets");
                }
            };
            let mut sets = sets.into_iter();
            for item in staged.iter_mut().filter(|item| item.shapes.is_some()) {
                item.descriptor_set = sets.next().expect("descriptor count matches");
            }
        }
        for item in &mut staged {
            if item.descriptor_set == vk::DescriptorSet::null() {
                item.descriptor_set = self.outputs[item.index]
                    .as_ref()
                    .expect("existing output has a descriptor set")
                    .descriptor_set;
            }
        }

        // Build every write before publishing any of them. Vulkan descriptor
        // updates have no fallible return path, so this is the commit point.
        let table_infos: Vec<[vk::DescriptorBufferInfo; 1]> = staged
            .iter()
            .map(|item| {
                [vk::DescriptorBufferInfo::default()
                    .buffer(item.table.buffer)
                    .offset(0)
                    .range(item.table.bytes)]
            })
            .collect();
        let shape_infos: Vec<Option<[vk::DescriptorBufferInfo; 1]>> = staged
            .iter()
            .map(|item| {
                item.shapes.as_ref().map(|shapes| {
                    [vk::DescriptorBufferInfo::default()
                        .buffer(shapes.buffer)
                        .offset(0)
                        .range(shapes.bytes)]
                })
            })
            .collect();
        let mut writes = Vec::with_capacity(staged.len() * 2);
        for (position, item) in staged.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(item.descriptor_set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&table_infos[position]),
            );
            if let Some(shape_info) = &shape_infos[position] {
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(item.descriptor_set)
                        .dst_binding(3)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(shape_info),
                );
            }
        }
        // Safety: all descriptor and buffer handles belong to `dev`; the
        // backing info arrays outlive this call. The fence wait above makes
        // rebinding safe for the previous submission.
        unsafe { dev.update_descriptor_sets(&writes, &[]) };

        if let Some(max_index) = staged.iter().map(|item| item.index).max() {
            if self.outputs.len() <= max_index {
                self.outputs.resize_with(max_index + 1, || None);
            }
        }
        for mut item in staged {
            let (shapes, sync_groups) = match self.outputs[item.index].take() {
                Some(old) => {
                    // The descriptor now points at `item.table`, and the
                    // fence wait above proves the old table is no longer in
                    // use by this queue. Preserve the output's pattern state.
                    unsafe {
                        dev.unmap_memory(old.memory);
                        dev.destroy_buffer(old.buffer, None);
                        dev.free_memory(old.memory, None);
                    }
                    (old.shapes, old.sync_groups)
                }
                None => (item.shapes.take().expect("new output has shapes"), 0),
            };
            self.outputs[item.index] = Some(OutputResources {
                buffer: item.table.buffer,
                memory: item.table.memory,
                capacity: item.table.bytes,
                size: [item.width, item.height],
                mapped: item.table.mapped,
                shapes,
                sync_groups,
                descriptor_set: item.descriptor_set,
            });
        }
        Ok(())
    }

    /// Upload output `index`'s `sync`-pattern geometry for this frame, as
    /// the fragment shader reads it: `groups` bounding-box/header pairs and
    /// the rectangles between them, packed by the caller (see
    /// `slicer.rs`'s `sync_shape_items`, and the layout comment on
    /// `blend.frag`'s `Shapes` block). Cheap enough to call every frame —
    /// it is a memcpy of a few kilobytes into a persistently mapped,
    /// host-coherent buffer, with no descriptor write and no reallocation.
    pub fn set_sync_shapes(
        &mut self,
        index: usize,
        groups: u32,
        items: &[[u32; 4]],
    ) -> anyhow::Result<()> {
        if items.len() > SYNC_SHAPE_SLOTS {
            bail!(
                "set_sync_shapes: {} items exceeds the {SYNC_SHAPE_SLOTS}-slot shape buffer",
                items.len()
            );
        }
        let output = self
            .outputs
            .get_mut(index)
            .and_then(|entry| entry.as_mut())
            .ok_or_else(|| {
                anyhow!(
                    "set_sync_shapes: no resources for output {index} (call set_transfer first)"
                )
            })?;
        // Safety: `output.shapes.mapped` addresses `SYNC_SHAPE_SLOTS`
        // items' worth of HOST_COHERENT memory, mapped for the whole life
        // of `output.shapes.memory`, and the length was just bounded
        // against that. No flush is needed and nothing else writes here.
        unsafe {
            std::ptr::copy_nonoverlapping(
                items.as_ptr().cast::<u8>(),
                output.shapes.mapped,
                std::mem::size_of_val(items),
            );
        }
        output.sync_groups = groups;
        Ok(())
    }

    /// The 1x1 stand-in for binding 0 while `sync()` draws, created on
    /// first use — see [`PlaceholderImage`] for why it has to exist at all.
    ///
    /// Plain `OPTIMAL` tiling and device-local memory: it is never exported,
    /// never imported, and never sampled, so none of the dmabuf machinery
    /// `create_image` carries applies. It is created with
    /// `COLOR_ATTACHMENT` beside `SAMPLED` purely so `transition_to_general`
    /// — shared with the Present images — describes it accurately.
    fn ensure_placeholder(&mut self) -> anyhow::Result<(vk::Image, vk::ImageView)> {
        if let Some(existing) = &self.placeholder {
            return Ok((existing.image, existing.view));
        }
        let device = &self.device.device;
        let format = vk::Format::R8G8B8A8_UNORM;
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: 1,
                height: 1,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // Safety: no pNext chain.
        let image = unsafe { device.create_image(&image_info, None) }
            .context("vkCreateImage (sync placeholder)")?;
        let mut cleanup = Cleanup {
            device,
            image,
            memory: None,
            view: None,
        };

        let requirements = unsafe { device.get_image_memory_requirements(image) };
        // Safety: read-only query.
        let memory_properties = unsafe {
            self.device
                .instance
                .get_physical_device_memory_properties(self.device.physical_device)
        };
        let memory_type_index = memory_type_index(
            &memory_properties,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| anyhow!("no DEVICE_LOCAL memory type fits a 1x1 image"))?;
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        // Safety: no pNext chain.
        let memory = unsafe { device.allocate_memory(&allocate_info, None) }
            .context("vkAllocateMemory (sync placeholder)")?;
        cleanup.memory = Some(memory);
        // Safety: `image` was just created and is not yet bound; `memory`
        // was sized and typed from its own requirements.
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("vkBindImageMemory (sync placeholder)")?;

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(color_subresource_range());
        // Safety: `image` is bound to memory; the view is destroyed before
        // the image in both `Cleanup::drop` and `Gpu::drop`.
        let view = unsafe { device.create_image_view(&view_info, None) }
            .context("vkCreateImageView (sync placeholder)")?;
        cleanup.view = Some(view);

        // A descriptor's image layout must match the image's actual layout,
        // and `render` writes binding 0 as GENERAL like every other image
        // here.
        self.transition_to_general(image)
            .context("sync placeholder UNDEFINED -> GENERAL transition")?;

        cleanup.defuse();
        self.placeholder = Some(PlaceholderImage {
            image,
            memory,
            view,
        });
        Ok((image, view))
    }

    /// Build the graphics pipeline for `format`, the first time any target
    /// of that format is blended into. See the `pipeline` field's doc for
    /// why this is lazy instead of living in `new()`.
    fn ensure_pipeline(&mut self, format: vk::Format) -> anyhow::Result<()> {
        if let Some(existing) = &self.pipeline {
            if existing.format == format {
                return Ok(());
            }
            bail!(
                "blend: target pixel format changed after the pipeline was already built for a \
                 different one; every output in one rig must share a DRM fourcc"
            );
        }
        let device = &self.device.device;
        let entry_point = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vert_module)
                .name(entry_point),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.frag_module)
                .name(entry_point),
        ];
        let vertex_input_state = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly_state = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization_state = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample_state = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let color_blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend_state =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_blend_attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [format];
        let mut rendering_create_info =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);

        let create_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input_state)
            .input_assembly_state(&input_assembly_state)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization_state)
            .multisample_state(&multisample_state)
            .color_blend_state(&color_blend_state)
            .dynamic_state(&dynamic_state)
            .layout(self.pipeline_layout)
            .push_next(&mut rendering_create_info);

        // Safety: every pointer `create_info`'s chain holds (`stages` and
        // every *_state struct above, `rendering_create_info`) outlives
        // this call; no pipeline cache, no base pipeline to inherit from.
        let pipelines = unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[create_info], None)
        }
        .map_err(|(_, error)| error)
        .context("vkCreateGraphicsPipelines")?;
        self.pipeline = Some(PipelineState {
            format,
            pipeline: pipelines[0],
        });
        Ok(())
    }

    fn create_measurement_pass(&self) -> anyhow::Result<MeasurementPass> {
        let device = &self.device.device;
        let format = vk::Format::R8G8B8A8_UNORM;
        let features = unsafe {
            self.device
                .instance
                .get_physical_device_format_properties(self.device.physical_device, format)
        }
        .optimal_tiling_features;
        anyhow::ensure!(
            features.contains(
                vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::TRANSFER_SRC
            ),
            "source measurement unavailable: local RGBA measurement target is unsupported"
        );
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: 256,
                height: 256,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_info, None) }
            .context("vkCreateImage source measurement")?;
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let properties = unsafe {
            self.device
                .instance
                .get_physical_device_memory_properties(self.device.physical_device)
        };
        let memory_type = memory_type_index(
            &properties,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| anyhow!("source measurement unavailable: no DEVICE_LOCAL target memory"))?;
        let memory = match unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(memory_type),
                None,
            )
        } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { device.destroy_image(image, None) };
                return Err(error).context("vkAllocateMemory source measurement");
            }
        };
        if let Err(error) = unsafe { device.bind_image_memory(image, memory, 0) } {
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(memory, None)
            };
            return Err(error).context("vkBindImageMemory source measurement");
        }
        let view = match unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(color_subresource_range()),
                None,
            )
        } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    device.destroy_image(image, None);
                    device.free_memory(memory, None)
                };
                return Err(error).context("vkCreateImageView source measurement");
            }
        };
        let readback = match allocate_readback_buffer(&self.device, 256 * 256 * 4) {
            Ok(buffer) => buffer,
            Err(error) => {
                unsafe {
                    device.destroy_image_view(view, None);
                    device.destroy_image(image, None);
                    device.free_memory(memory, None)
                };
                return Err(error).context("source measurement readback buffer");
            }
        };
        let mut cleanup = MeasurementCleanup {
            device,
            image,
            memory,
            view,
            readback: Some(readback),
            descriptor_set_layout: vk::DescriptorSetLayout::null(),
            descriptor_pool: vk::DescriptorPool::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            fragment: vk::ShaderModule::null(),
            pipeline: vk::Pipeline::null(),
        };
        let immutable = [self.sampler];
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .immutable_samplers(&immutable),
        ];
        let descriptor_set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .context("vkCreateDescriptorSetLayout source measurement")?;
        cleanup.descriptor_set_layout = descriptor_set_layout;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(1),
        ];
        let descriptor_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .context("vkCreateDescriptorPool source measurement")?;
        cleanup.descriptor_pool = descriptor_pool;
        let layouts = [descriptor_set_layout];
        let descriptor_set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&layouts),
            )
        }
        .context("vkAllocateDescriptorSets source measurement")?[0];
        let range = [vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::FRAGMENT,
            offset: 0,
            size: std::mem::size_of::<MeasurePushConstants>() as u32,
        }];
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&range),
                None,
            )
        }
        .context("vkCreatePipelineLayout source measurement")?;
        cleanup.pipeline_layout = pipeline_layout;
        let fragment = create_shader_module(device, MEASURE_FRAG_SPV, "measurement fragment")?;
        cleanup.fragment = fragment;
        let entry = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vert_module)
                .name(entry),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment)
                .name(entry),
        ];
        let vertex = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let attachment = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachment);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let formats = [format];
        let mut rendering =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic_state)
            .layout(pipeline_layout)
            .push_next(&mut rendering);
        let pipeline =
            unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }
                .map_err(|(_, e)| e)
                .context("vkCreateGraphicsPipelines source measurement")?[0];
        cleanup.pipeline = pipeline;
        unsafe { device.destroy_shader_module(fragment, None) };
        cleanup.fragment = vk::ShaderModule::null();
        Ok(cleanup.defuse(descriptor_set))
    }

    /// Blend every job from `canvas` into its target, submit, and wait for the
    /// GPU to finish. Returns how long the wait took. `y_invert` means the
    /// canvas is stored bottom-up (screencopy's flag).
    pub fn blend(
        &mut self,
        canvas: &DmabufImage,
        y_invert: bool,
        jobs: &[BlendJob<'_>],
    ) -> anyhow::Result<Duration> {
        self.render(
            Source::Canvas {
                image: CanvasImage::Dmabuf(canvas),
                y_invert,
            },
            jobs,
        )
    }

    /// Blend a canvas previously uploaded with [`Gpu::upload_static_canvas_rgba`]
    /// into the same Present targets and through the same fence/presentation
    /// path as a captured canvas.
    pub fn blend_static(
        &mut self,
        canvas: &StaticCanvas,
        jobs: &[BlendJob<'_>],
    ) -> anyhow::Result<Duration> {
        for job in jobs {
            // Explicit source rectangles are validated by Warp and may
            // scale or extend beyond the canvas. The general shader checks
            // each sample and writes opaque black outside the canvas.
            if job.warp.is_some_and(|warp| warp.source_rect().is_some()) {
                continue;
            }
            let right = job
                .source_x
                .checked_add(job.target.width)
                .ok_or_else(|| anyhow!("blend_static: source x extent overflow"))?;
            let bottom = job
                .source_y
                .checked_add(job.target.height)
                .ok_or_else(|| anyhow!("blend_static: source y extent overflow"))?;
            if right > canvas.width || bottom > canvas.height {
                bail!(
                    "blend_static: output {} source {}x{} at {},{} exceeds {}x{} canvas",
                    job.output,
                    job.target.width,
                    job.target.height,
                    job.source_x,
                    job.source_y,
                    canvas.width,
                    canvas.height,
                );
            }
        }
        self.render(
            Source::Canvas {
                image: CanvasImage::Static(canvas),
                y_invert: false,
            },
            jobs,
        )
    }

    /// Draw the `sync` test pattern into every job's target from the shapes
    /// last given to [`Gpu::set_sync_shapes`], and wait the same way
    /// [`Gpu::blend`] does.
    ///
    /// Deliberately the identical submit, fence and barrier sequence: the
    /// pattern exists to measure how long a frame takes to reach the glass,
    /// so anything it did differently from content would be measuring
    /// something else.
    pub fn sync(&mut self, jobs: &[BlendJob<'_>]) -> anyhow::Result<Duration> {
        self.render(Source::Sync, jobs)
    }

    /// Set the shared adaptive lift state used by tagged transfer entries.
    /// This only changes push constants for later draws; it never rebuilds or
    /// uploads geometry-derived tables. `maximum` is the configured physical
    /// footprint coverage maximum and is deliberately bounded by the packed
    /// entry's four-bit count domain.
    pub fn set_dynamic_lift(&mut self, level: f64, maximum: u32) -> anyhow::Result<()> {
        if !level.is_finite() || !(0.0..=1.0).contains(&level) {
            bail!("dynamic lift must be finite and within 0..=1");
        }
        if maximum > 8 {
            bail!("dynamic lift maximum must be within 0..=8");
        }
        self.dynamic_lift = level as f32;
        self.dynamic_maximum = maximum;
        Ok(())
    }

    /// Measure mean pre-transfer linear Rec.709 luminance from a nearest-texel
    /// source grid. The grid is at most 256×256 and covers the supplied
    /// logical domain once; `width` and `height` deliberately exclude any
    /// allocation padding in `canvas`. Failure is non-destructive: callers
    /// can retain the fixed-lift renderer and surface this error as the
    /// adaptive-mode unavailable reason.
    pub fn measure_luminance(
        &mut self,
        canvas: &DmabufImage,
        y_invert: bool,
        width: u32,
        height: u32,
    ) -> anyhow::Result<(f64, u32)> {
        if width == 0 || height == 0 || width > canvas.width || height > canvas.height {
            bail!("source measurement domain must be nonzero and fit the capture canvas");
        }
        self.measure_luminance_inner(canvas.image, canvas.view, true, y_invert, width, height)
    }

    #[cfg(test)]
    fn measure_static_luminance(
        &mut self,
        canvas: &StaticCanvas,
        width: u32,
        height: u32,
    ) -> anyhow::Result<(f64, u32)> {
        if width == 0 || height == 0 || width > canvas.width || height > canvas.height {
            bail!("source measurement domain must be nonzero and fit the static canvas");
        }
        self.measure_luminance_inner(canvas.image, canvas.view, false, false, width, height)
    }

    fn measure_luminance_inner(
        &mut self,
        source_image: vk::Image,
        source_view: vk::ImageView,
        foreign_source: bool,
        y_invert: bool,
        width: u32,
        height: u32,
    ) -> anyhow::Result<(f64, u32)> {
        self.wait_for_pending_work()?;
        let grid = [width.min(256), height.min(256)];
        if self.measurement.is_none() {
            self.measurement = Some(self.create_measurement_pass()?);
        }
        let (
            measurement_image,
            measurement_view,
            measurement_readback,
            measurement_set,
            measurement_layout,
            measurement_pipeline,
            measurement_memory,
            measurement_coherent,
            measurement_initialized,
        ) = {
            let pass = self
                .measurement
                .as_ref()
                .expect("measurement just initialized");
            (
                pass.image,
                pass.view,
                pass.readback.buffer,
                pass.descriptor_set,
                pass.pipeline_layout,
                pass.pipeline,
                pass.readback.memory,
                pass.readback.coherent,
                pass.initialized,
            )
        };
        let measurement_mapped = self
            .measurement
            .as_ref()
            .expect("measurement just initialized")
            .readback
            .mapped;
        let result = (|| -> anyhow::Result<(f64, u32)> {
            let device = &self.device.device;
            let queue_family = self.device.queue_family;
            let color_range = color_subresource_range();
            let canvas_info = [vk::DescriptorImageInfo::default()
                .image_view(source_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let write = [vk::WriteDescriptorSet::default()
                .dst_set(measurement_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&canvas_info)];
            // Safety: the descriptor and source view belong to this device;
            // source ownership is transferred in the command below.
            unsafe { device.update_descriptor_sets(&write, &[]) };
            unsafe {
                device
                    .reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
            }
            .context("vkResetCommandBuffer for source measurement")?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            unsafe { device.begin_command_buffer(self.command_buffer, &begin) }
                .context("vkBeginCommandBuffer for source measurement")?;

            let acquire_canvas = vk::ImageMemoryBarrier::default()
                .src_access_mask(if foreign_source {
                    vk::AccessFlags::empty()
                } else {
                    vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_READ
                })
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(if foreign_source {
                    vk::QUEUE_FAMILY_FOREIGN_EXT
                } else {
                    vk::QUEUE_FAMILY_IGNORED
                })
                .dst_queue_family_index(if foreign_source {
                    queue_family
                } else {
                    vk::QUEUE_FAMILY_IGNORED
                })
                .image(source_image)
                .subresource_range(color_range);
            let initialize_target = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .old_layout(if measurement_initialized {
                    vk::ImageLayout::GENERAL
                } else {
                    vk::ImageLayout::UNDEFINED
                })
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(measurement_image)
                .subresource_range(color_range);
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    if foreign_source {
                        vk::PipelineStageFlags::TOP_OF_PIPE
                    } else {
                        vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::FRAGMENT_SHADER
                    },
                    vk::PipelineStageFlags::FRAGMENT_SHADER
                        | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[acquire_canvas, initialize_target],
                )
            };

            let attachment = [vk::RenderingAttachmentInfo::default()
                .image_view(measurement_view)
                .image_layout(vk::ImageLayout::GENERAL)
                .load_op(vk::AttachmentLoadOp::DONT_CARE)
                .store_op(vk::AttachmentStoreOp::STORE)];
            let rendering = vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: grid[0],
                        height: grid[1],
                    },
                })
                .layer_count(1)
                .color_attachments(&attachment);
            unsafe { device.cmd_begin_rendering(self.command_buffer, &rendering) };
            unsafe {
                device.cmd_bind_pipeline(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    measurement_pipeline,
                );
                device.cmd_set_viewport(
                    self.command_buffer,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: grid[0] as f32,
                        height: grid[1] as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                device.cmd_set_scissor(
                    self.command_buffer,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D {
                            width: grid[0],
                            height: grid[1],
                        },
                    }],
                );
                device.cmd_bind_descriptor_sets(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    measurement_layout,
                    0,
                    &[measurement_set],
                    &[],
                );
                let constants = MeasurePushConstants {
                    source_width: width,
                    source_height: height,
                    grid_width: grid[0],
                    grid_height: grid[1],
                    y_invert: u32::from(y_invert),
                };
                device.cmd_push_constants(
                    self.command_buffer,
                    measurement_layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    constants.as_bytes(),
                );
                device.cmd_draw(self.command_buffer, 3, 1, 0, 0);
                device.cmd_end_rendering(self.command_buffer);
            }
            let readable_target = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(measurement_image)
                .subresource_range(color_range);
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[readable_target],
                )
            };
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width: grid[0],
                    height: grid[1],
                    depth: 1,
                });
            unsafe {
                device.cmd_copy_image_to_buffer(
                    self.command_buffer,
                    measurement_image,
                    vk::ImageLayout::GENERAL,
                    measurement_readback,
                    &[region],
                )
            };
            let host_read = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(measurement_readback)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            let release_canvas = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(if foreign_source {
                    queue_family
                } else {
                    vk::QUEUE_FAMILY_IGNORED
                })
                .dst_queue_family_index(if foreign_source {
                    vk::QUEUE_FAMILY_FOREIGN_EXT
                } else {
                    vk::QUEUE_FAMILY_IGNORED
                })
                .image(source_image)
                .subresource_range(color_range);
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[host_read],
                    &[release_canvas],
                )
            };
            unsafe { device.end_command_buffer(self.command_buffer) }
                .context("vkEndCommandBuffer for source measurement")?;
            unsafe { device.reset_fences(&[self.fence]) }
                .context("vkResetFences for source measurement")?;
            let cbs = [self.command_buffer];
            let submit = vk::SubmitInfo::default().command_buffers(&cbs);
            unsafe { device.queue_submit(self.device.queue, &[submit], self.fence) }
                .context("vkQueueSubmit source measurement")?;
            self.fence_in_flight = true;
            unsafe { device.wait_for_fences(&[self.fence], true, u64::MAX) }
                .context("vkWaitForFences source measurement")?;
            self.fence_in_flight = false;
            // Readback allocation may fall back to noncoherent host-visible
            // memory, which must be invalidated after the GPU write.
            if !measurement_coherent {
                let range = [vk::MappedMemoryRange::default()
                    .memory(measurement_memory)
                    .offset(0)
                    .size(vk::WHOLE_SIZE)];
                unsafe { device.invalidate_mapped_memory_ranges(&range) }
                    .context("vkInvalidateMappedMemoryRanges source measurement")?;
            }
            let count = grid[0] * grid[1];
            let bytes =
                unsafe { std::slice::from_raw_parts(measurement_mapped, count as usize * 4) };
            Ok((mean_linear_luminance_rgba(bytes), count))
        })();
        if result.is_ok() {
            self.measurement
                .as_mut()
                .expect("measurement remains retained")
                .initialized = true;
        }
        result
    }

    fn wait_for_pending_work(&mut self) -> anyhow::Result<()> {
        if !self.fence_in_flight {
            return Ok(());
        }
        // Safety: `fence` belongs to this device and is the fence attached to
        // the only submission that can reference the active output buffers.
        unsafe {
            self.device
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)
        }
        .context("vkWaitForFences before resource replacement")?;
        self.fence_in_flight = false;
        Ok(())
    }

    fn render(&mut self, source: Source<'_>, jobs: &[BlendJob<'_>]) -> anyhow::Result<Duration> {
        if jobs.is_empty() {
            return Ok(Duration::ZERO);
        }
        self.wait_for_pending_work()?;
        for job in jobs {
            if job.target.format != jobs[0].target.format {
                bail!("blend: all targets must use the same pixel format");
            }
            let output = self
                .outputs
                .get(job.output)
                .and_then(Option::as_ref)
                .ok_or_else(|| anyhow!("blend: no transfer table for output {}", job.output))?;
            let size = [job.target.width, job.target.height];
            if output.size != size || job.warp.is_some_and(|w| w.output_size() != size) {
                bail!("blend: target, warp, and transfer dimensions must agree");
            }
            if let Source::Canvas { image, .. } = source {
                validate_filtering(image.linear_filter_supported(), job.warp.is_some())?;
            }
        }
        // Binding 0 wants a valid view either way — see `PlaceholderImage`.
        let (
            canvas_image,
            canvas_view,
            canvas_height,
            y_invert,
            mode,
            foreign_canvas,
            locally_uploaded_canvas,
        ) = match source {
            Source::Canvas { image, y_invert } => (
                image.image(),
                image.view(),
                image.height(),
                u32::from(y_invert),
                0u32,
                image.foreign_owned(),
                image.locally_uploaded(),
            ),
            Source::Sync => {
                let (image, view) = self.ensure_placeholder()?;
                (image, view, 0, 0, 1, false, false)
            }
        };
        let format = jobs[0].target.format;
        self.ensure_pipeline(format)?;
        let pipeline_handle = self
            .pipeline
            .as_ref()
            .expect("ensure_pipeline just set it or returned Err")
            .pipeline;

        let device = &self.device.device;
        let queue_family = self.device.queue_family;
        let color_subresource = color_subresource_range();

        // Rewrite every allocated descriptor set's canvas binding only when
        // the canvas image changed since the last call — the common case
        // (a steady capture image, different slices/targets each frame)
        // then costs this function nothing but the barriers and draws.
        if self.last_canvas != Some(canvas_image) {
            let canvas_info = [vk::DescriptorImageInfo::default()
                .image_view(canvas_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let writes: Vec<vk::WriteDescriptorSet> = self
                .outputs
                .iter()
                .filter_map(|entry| entry.as_ref())
                .map(|output| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(output.descriptor_set)
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(&canvas_info)
                })
                .collect();
            if !writes.is_empty() {
                // Safety: `canvas_info` outlives the call; every descriptor
                // set in `writes` was allocated from `self.descriptor_pool`
                // in `allocate_output` and still exists.
                unsafe { device.update_descriptor_sets(&writes, &[]) };
            }
            self.last_canvas = Some(canvas_image);
        }

        // Safety: `self.command_buffer`'s pool was created with
        // `RESET_COMMAND_BUFFER`, and the buffer is not in use — the
        // previous `blend()` call, if any, already waited on `self.fence`
        // before returning.
        unsafe {
            device.reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
        }
        .context("vkResetCommandBuffer")?;
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { device.begin_command_buffer(self.command_buffer, &begin_info) }
            .context("vkBeginCommandBuffer")?;

        // Acquire the canvas from the compositor's queue family — see the
        // module doc: layout stays GENERAL on both sides, only ownership
        // moves. Skipped in `sync` mode, whose stand-in image the compositor
        // has never seen and so cannot own.
        if foreign_canvas {
            let acquire_canvas = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(queue_family)
                .image(canvas_image)
                .subresource_range(color_subresource);
            // Safety: recording into `self.command_buffer`, which just began.
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[acquire_canvas],
                );
            }
        }
        if locally_uploaded_canvas {
            // The upload and this draw are separate queue submissions. Keep
            // the image in GENERAL, but make the preceding transfer write
            // explicitly visible to this shader read. Repeated static draws
            // also remain valid because SHADER_READ is included as a source.
            let readable_canvas = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(canvas_image)
                .subresource_range(color_subresource);
            // Safety: recording into the current command buffer; this local
            // image is permanently owned by our queue family.
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[readable_canvas],
                );
            }
        }

        for job in jobs {
            let output = self.outputs[job.output]
                .as_ref()
                .expect("validated before recording");

            let acquire_target = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(queue_family)
                .image(job.target.image)
                .subresource_range(color_subresource);
            // Safety: same command buffer, still recording.
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[acquire_target],
                );
            }

            let color_attachments = [vk::RenderingAttachmentInfo::default()
                .image_view(job.target.view)
                .image_layout(vk::ImageLayout::GENERAL)
                .load_op(vk::AttachmentLoadOp::DONT_CARE)
                .store_op(vk::AttachmentStoreOp::STORE)];
            let rendering_info = vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: job.target.width,
                        height: job.target.height,
                    },
                })
                .layer_count(1)
                .color_attachments(&color_attachments);
            // Safety: `job.target.view` has `COLOR_ATTACHMENT` usage and was
            // just acquired into our queue family above.
            unsafe { device.cmd_begin_rendering(self.command_buffer, &rendering_info) };
            unsafe {
                device.cmd_bind_pipeline(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipeline_handle,
                )
            };
            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: job.target.width as f32,
                height: job.target.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            unsafe { device.cmd_set_viewport(self.command_buffer, 0, &[viewport]) };
            let scissor = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: job.target.width,
                    height: job.target.height,
                },
            };
            unsafe { device.cmd_set_scissor(self.command_buffer, 0, &[scissor]) };
            unsafe {
                device.cmd_bind_descriptor_sets(
                    self.command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[output.descriptor_set],
                    &[],
                );
            }
            let mut push_constants = PushConstants::canvas(
                [job.source_x, job.source_y],
                [job.target.width, job.target.height],
                canvas_height,
                y_invert != 0,
                job.warp,
            );
            push_constants.mode = mode;
            push_constants.groups = output.sync_groups;
            push_constants.dynamic_lift = self.dynamic_lift;
            push_constants.dynamic_maximum = self.dynamic_maximum;
            unsafe {
                device.cmd_push_constants(
                    self.command_buffer,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push_constants.as_bytes(),
                );
            }
            unsafe { device.cmd_draw(self.command_buffer, 3, 1, 0, 0) };
            unsafe { device.cmd_end_rendering(self.command_buffer) };

            let release_target = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .image(job.target.image)
                .subresource_range(color_subresource);
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[release_target],
                );
            }
        }

        // The other half of the acquire above, and skipped for the same
        // reason: the placeholder is never handed back to anyone.
        if foreign_canvas {
            let release_canvas = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .image(canvas_image)
                .subresource_range(color_subresource);
            unsafe {
                device.cmd_pipeline_barrier(
                    self.command_buffer,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[release_canvas],
                );
            }
        }

        unsafe { device.end_command_buffer(self.command_buffer) }.context("vkEndCommandBuffer")?;

        // Safety: `self.fence` was last waited on by the previous `blend()`
        // call (or has never been signalled, on the first call), so
        // resetting it here cannot race an in-flight wait.
        unsafe { device.reset_fences(&[self.fence]) }.context("vkResetFences")?;
        let command_buffers = [self.command_buffer];
        let submit_info = vk::SubmitInfo::default().command_buffers(&command_buffers);
        // Safety: `self.command_buffer` finished recording above; `self.fence`
        // was just reset and is used by nothing else.
        unsafe { device.queue_submit(self.device.queue, &[submit_info], self.fence) }
            .context("vkQueueSubmit")?;
        self.fence_in_flight = true;

        let wait_from = Instant::now();
        // The compositor reads these images the moment we commit the
        // `wl_buffer`s built from them, and the NVIDIA driver gives dmabufs
        // no implicit fencing — see the module doc's measurements. Waiting
        // here is what makes it safe for the caller to commit right after
        // `blend()` returns.
        unsafe { device.wait_for_fences(&[self.fence], true, u64::MAX) }
            .context("vkWaitForFences")?;
        self.fence_in_flight = false;
        Ok(wait_from.elapsed())
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let device = &self.device.device;
        // Safety: waits out anything still in flight from our own
        // submissions before destroying the objects they used. `blend()`
        // always waits on `self.fence` before returning, so in practice
        // this only matters for a `Gpu` dropped without ever calling
        // `blend()`, or one dropped right after a `blend()` call that
        // itself failed partway through recording (never submitted).
        let _ = unsafe { device.device_wait_idle() };
        // Safety: every handle destroyed below was created by this same
        // `Gpu` (in `from_device`, `ensure_pipeline`, or `allocate_output`)
        // and is owned exclusively by it — nothing else holds a copy.
        // Destroying the command pool frees `self.command_buffer` with it.
        unsafe {
            if let Some(pipeline) = self.pipeline.take() {
                device.destroy_pipeline(pipeline.pipeline, None);
            }
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_shader_module(self.vert_module, None);
            device.destroy_shader_module(self.frag_module, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            device.destroy_sampler(self.sampler, None);
            for output in self.outputs.drain(..).flatten() {
                device.unmap_memory(output.memory);
                device.destroy_buffer(output.buffer, None);
                device.free_memory(output.memory, None);
                output.shapes.destroy(device);
            }
            if let Some(placeholder) = self.placeholder.take() {
                device.destroy_image_view(placeholder.view, None);
                device.destroy_image(placeholder.image, None);
                device.free_memory(placeholder.memory, None);
            }
            if let Some(measurement) = self.measurement.take() {
                measurement.destroy(device);
            }
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.command_pool, None);
        }
    }
}

/// Tears down an in-progress image (and whatever of its memory/view already
/// exist) unless [`Cleanup::defuse`] is called, so a failure partway through
/// `Gpu::create_image` cannot leak the image, its memory, or its view — nor
/// destroy them in the wrong order (a view before its image would be, or
/// memory before the image it is bound to).
struct Cleanup<'a> {
    device: &'a ash::Device,
    image: vk::Image,
    memory: Option<vk::DeviceMemory>,
    view: Option<vk::ImageView>,
}

impl Cleanup<'_> {
    /// The image (and its memory/view) are now owned by the `DmabufImage`
    /// being returned; nothing here should be destroyed after all.
    fn defuse(self) {
        std::mem::forget(self);
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        // Safety: exactly the handles this struct was told about, each
        // created earlier in the same `create_image` call, not yet handed
        // to a `DmabufImage`, and not destroyed anywhere else. View, then
        // image, then memory — the order `DmabufImage::drop` also uses.
        unsafe {
            if let Some(view) = self.view {
                self.device.destroy_image_view(view, None);
            }
            self.device.destroy_image(self.image, None);
            if let Some(memory) = self.memory {
                self.device.free_memory(memory, None);
            }
        }
    }
}

fn create_shader_module(
    device: &ash::Device,
    spv: &[u8],
    name: &str,
) -> anyhow::Result<vk::ShaderModule> {
    let words = ash::util::read_spv(&mut Cursor::new(spv))
        .with_context(|| format!("decoding embedded {name} SPIR-V"))?;
    let info = vk::ShaderModuleCreateInfo::default().code(&words);
    // Safety: `info`'s only borrow, `words`, outlives this call.
    unsafe { device.create_shader_module(&info, None) }
        .with_context(|| format!("vkCreateShaderModule ({name})"))
}

fn image_usage_flags(usage: Usage) -> vk::ImageUsageFlags {
    match usage {
        Usage::Capture => vk::ImageUsageFlags::SAMPLED,
        Usage::Present => vk::ImageUsageFlags::COLOR_ATTACHMENT,
    }
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// fourcc -> `vk::Format`. Alpha is ignored on read and written as 1.0 (the
/// shader's `outColor.a` is a literal `1.0`, and `texelFetch` never reads
/// `.a` at all) — matching an `X`/`A` fourcc pair identically is deliberate,
/// not a shortcut: this module never composites, so there is no alpha
/// channel for either variant to usefully carry.
fn format_for_fourcc(fourcc: u32) -> anyhow::Result<vk::Format> {
    match fourcc {
        FOURCC_XR24 | FOURCC_AR24 => Ok(vk::Format::B8G8R8A8_UNORM),
        FOURCC_XB24 | FOURCC_AB24 => Ok(vk::Format::R8G8B8A8_UNORM),
        _ => bail!("unsupported DRM fourcc {fourcc:#010x}"),
    }
}

/// `(a, b)` packed as `a << 8 | b`, matching `blend.frag`'s unpacking
/// (`ab >> 8`, `ab & 0xff`). `a` only ever needs 9 bits (`pixel_transfer` in
/// blend.rs produces up to 256), so it never collides with `b`'s low byte.
fn pack_transfer(a: u16, b: u8) -> u32 {
    (u32::from(a) << 8) | u32::from(b)
}

fn validate_packed_transfer(entry: u32) -> anyhow::Result<()> {
    if entry & 0x8000_0000 == 0 {
        if entry >> 17 != 0 || ((entry >> 8) & 0x1ff) > 256 {
            bail!("packed fixed transfer has reserved bits or a gain outside 0..=256");
        }
    } else if entry & 0x7000_0000 != 0 {
        bail!("packed dynamic transfer has nonzero reserved bits 28..30");
    } else if ((entry >> 24) & 0x0f) > 8 {
        bail!("packed dynamic transfer has coverage count outside 0..=8");
    }
    Ok(())
}

fn validate_filtering(linear_filter: bool, warped: bool) -> anyhow::Result<()> {
    if warped && !linear_filter {
        bail!("warp unavailable: capture format/modifier does not support linear filtering");
    }
    Ok(())
}

fn static_canvas_features_supported(features: vk::FormatFeatureFlags) -> bool {
    features.contains(
        vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::TRANSFER_DST,
    )
}

impl HostBuffer {
    /// Unmap and destroy, in the order Vulkan requires (a mapping must not
    /// outlive the memory it maps, and a buffer must go before the memory
    /// it is bound to). Used both on the failure paths of
    /// `allocate_output` and from `Gpu::drop`.
    ///
    /// # Safety
    /// Nothing may still reference this buffer: no descriptor pointing at
    /// it, and no submission in flight that reads it.
    fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.unmap_memory(self.memory);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// A HOST_VISIBLE|HOST_COHERENT storage buffer of `bytes`, bound and mapped
/// for its whole life. Both per-output SSBOs — the transfer table and the
/// `sync` shape list — are written by the CPU every time they change and
/// read by the fragment shader, which is exactly this shape; keeping one
/// allocator for both is what keeps their teardown identical too.
///
/// Cleans up after itself on every failure path, so a caller that gets an
/// `Err` has leaked nothing.
fn allocate_host_buffer(
    device: &Arc<DeviceState>,
    bytes: vk::DeviceSize,
) -> anyhow::Result<HostBuffer> {
    allocate_host_buffer_with_usage(device, bytes, vk::BufferUsageFlags::STORAGE_BUFFER)
}

fn allocate_host_buffer_with_usage(
    device: &Arc<DeviceState>,
    bytes: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
) -> anyhow::Result<HostBuffer> {
    let dev = &device.device;
    let buffer_info = vk::BufferCreateInfo::default()
        .size(bytes)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // Safety: no pNext chain.
    let buffer = unsafe { dev.create_buffer(&buffer_info, None) }.context("vkCreateBuffer")?;

    let requirements = unsafe { dev.get_buffer_memory_requirements(buffer) };
    let memory_properties = unsafe {
        device
            .instance
            .get_physical_device_memory_properties(device.physical_device)
    };
    let memory_type_index = match memory_type_index(
        &memory_properties,
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    ) {
        Some(index) => index,
        None => {
            // Safety: `buffer` was just created and is not bound to anything.
            unsafe { dev.destroy_buffer(buffer, None) };
            bail!("no HOST_VISIBLE|HOST_COHERENT memory type for a {bytes}-byte storage buffer");
        }
    };
    let allocate_info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_type_index);
    let memory = match unsafe { dev.allocate_memory(&allocate_info, None) } {
        Ok(memory) => memory,
        Err(error) => {
            // Safety: `buffer` was just created and is not bound to anything.
            unsafe { dev.destroy_buffer(buffer, None) };
            return Err(error).context("vkAllocateMemory");
        }
    };
    if let Err(error) = unsafe { dev.bind_buffer_memory(buffer, memory, 0) } {
        // Safety: both were just created and neither is referenced anywhere
        // else yet.
        unsafe {
            dev.destroy_buffer(buffer, None);
            dev.free_memory(memory, None);
        }
        return Err(error).context("vkBindBufferMemory");
    }
    let mapped = match unsafe { dev.map_memory(memory, 0, bytes, vk::MemoryMapFlags::empty()) } {
        Ok(ptr) => ptr.cast::<u8>(),
        Err(error) => {
            // Safety: both were just created and neither is referenced
            // anywhere else yet.
            unsafe {
                dev.destroy_buffer(buffer, None);
                dev.free_memory(memory, None);
            }
            return Err(error).context("vkMapMemory");
        }
    };
    Ok(HostBuffer {
        buffer,
        memory,
        mapped,
        bytes,
        coherent: true,
    })
}

/// Readback may use noncoherent host-visible memory on small Vulkan devices.
/// Unlike transfer/shape buffers it is written only by the GPU, so callers
/// invalidate it after the fence before reading the persistent mapping.
fn allocate_readback_buffer(
    device: &Arc<DeviceState>,
    bytes: vk::DeviceSize,
) -> anyhow::Result<HostBuffer> {
    let dev = &device.device;
    let info = vk::BufferCreateInfo::default()
        .size(bytes)
        .usage(vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { dev.create_buffer(&info, None) }.context("vkCreateBuffer readback")?;
    let requirements = unsafe { dev.get_buffer_memory_requirements(buffer) };
    let properties = unsafe {
        device
            .instance
            .get_physical_device_memory_properties(device.physical_device)
    };
    let memory_type_index =
        match readback_memory_type_index(&properties, requirements.memory_type_bits) {
            Some(index) => index,
            None => {
                unsafe { dev.destroy_buffer(buffer, None) };
                bail!("no HOST_VISIBLE memory type for a {bytes}-byte readback buffer");
            }
        };
    let coherent = properties.memory_types[memory_type_index as usize]
        .property_flags
        .contains(vk::MemoryPropertyFlags::HOST_COHERENT);
    let memory = match unsafe {
        dev.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type_index),
            None,
        )
    } {
        Ok(memory) => memory,
        Err(error) => {
            unsafe { dev.destroy_buffer(buffer, None) };
            return Err(error).context("vkAllocateMemory readback");
        }
    };
    if let Err(error) = unsafe { dev.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            dev.destroy_buffer(buffer, None);
            dev.free_memory(memory, None)
        };
        return Err(error).context("vkBindBufferMemory readback");
    }
    let mapped =
        match unsafe { dev.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) } {
            Ok(mapped) => mapped.cast::<u8>(),
            Err(error) => {
                unsafe {
                    dev.destroy_buffer(buffer, None);
                    dev.free_memory(memory, None)
                };
                return Err(error).context("vkMapMemory readback");
            }
        };
    Ok(HostBuffer {
        buffer,
        memory,
        mapped,
        bytes,
        coherent,
    })
}

/// CPU reduction reads every sample. Prefer cached host memory over an
/// earlier write-combined type; explicit invalidation handles noncoherent
/// cached memory. Devices without cached memory retain the visible fallback.
fn readback_memory_type_index(
    properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
) -> Option<u32> {
    memory_type_index(
        properties,
        type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED,
    )
    .or_else(|| memory_type_index(properties, type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE))
}

/// Mean linear-light Rec.709 luminance of RGBA8 source samples. The source
/// compositor supplies sRGB signal values; conversion happens before the
/// 0.2126/0.7152/0.0722 weighting so dark content is not biased upward.
fn mean_linear_luminance_rgba(bytes: &[u8]) -> f64 {
    debug_assert_eq!(bytes.len() % 4, 0);
    if bytes.is_empty() {
        return 0.0;
    }
    bytes
        .chunks_exact(4)
        .map(|p| crate::projection::adaptive::rgb8_luminance([p[0], p[1], p[2]]))
        .sum::<f64>()
        / (bytes.len() / 4) as f64
}

/// The first memory type (by index — the order `VkPhysicalDeviceMemoryProperties`
/// reports them, which drivers document as their preference order) that both
/// `type_bits` allows and has every flag in `required`.
fn memory_type_index(
    properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..properties.memory_type_count).find(|&index| {
        let allowed = type_bits & (1 << index) != 0;
        allowed
            && properties.memory_types[index as usize]
                .property_flags
                .contains(required)
    })
}

/// Decode a Linux packed `dev_t` into `(major, minor)`, matching glibc's
/// `major()`/`minor()` macros (`bits/sysmacros.h`) — the encoding both the
/// compositor's dmabuf feedback and this module's `render_node` parameter
/// use. `VkPhysicalDeviceDrmPropertiesEXT` reports major/minor already split
/// out, so this is only needed for the caller's packed input.
fn dev_major_minor(dev: u64) -> (i64, i64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as i64, minor as i64)
}

/// Why a physical device cannot be used, or `None` when it has everything
/// this module needs. Named per-candidate (rather than a bare bool) so that,
/// when *no* device qualifies, `pick_physical_device` can report something
/// more useful than "no device found".
fn missing_requirement(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Option<String> {
    // Safety: read-only query.
    let properties = unsafe { instance.get_physical_device_properties(physical_device) };
    if properties.api_version < vk::API_VERSION_1_3 {
        return Some(format!(
            "Vulkan 1.3 (has {}.{}.{})",
            vk::api_version_major(properties.api_version),
            vk::api_version_minor(properties.api_version),
            vk::api_version_patch(properties.api_version),
        ));
    }

    // Safety: read-only query.
    let extensions =
        match unsafe { instance.enumerate_device_extension_properties(physical_device) } {
            Ok(extensions) => extensions,
            Err(error) => {
                return Some(format!(
                    "vkEnumerateDeviceExtensionProperties failed: {error}"
                ));
            }
        };
    for &required in &REQUIRED_DEVICE_EXTENSIONS {
        let present = extensions.iter().any(|extension| {
            // Safety: `extension_name` is a NUL-terminated byte array filled
            // in by the driver.
            let name = unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) };
            name == required
        });
        if !present {
            return Some(required.to_string_lossy().into_owned());
        }
    }

    let mut vulkan_1_3_features = vk::PhysicalDeviceVulkan13Features::default();
    let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut vulkan_1_3_features);
    // Safety: `features2`'s chain outlives the call.
    unsafe { instance.get_physical_device_features2(physical_device, &mut features2) };
    if vulkan_1_3_features.dynamic_rendering == vk::FALSE {
        return Some("dynamicRendering (Vulkan 1.3 core feature)".to_string());
    }

    None
}

/// Whether `physical_device`'s `VK_EXT_physical_device_drm` primary or
/// render major/minor equal `want`.
fn drm_node_matches(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    want: (i64, i64),
) -> bool {
    let mut drm_properties = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm_properties);
    // Safety: `properties2`'s chain outlives the call; `drm_properties` is
    // written by the driver, not read beforehand
    // (`PhysicalDeviceDrmPropertiesEXT` implements `Default`).
    unsafe { instance.get_physical_device_properties2(physical_device, &mut properties2) };
    (drm_properties.has_primary != 0
        && (drm_properties.primary_major, drm_properties.primary_minor) == want)
        || (drm_properties.has_render != 0
            && (drm_properties.render_major, drm_properties.render_minor) == want)
}

fn pick_physical_device(
    instance: &ash::Instance,
    render_node: Option<u64>,
) -> anyhow::Result<vk::PhysicalDevice> {
    // Safety: read-only query.
    let all =
        unsafe { instance.enumerate_physical_devices() }.context("vkEnumeratePhysicalDevices")?;
    if all.is_empty() {
        bail!("no Vulkan physical devices reported by the loader");
    }

    let mut usable = Vec::new();
    let mut last_reason = None;
    for &physical_device in &all {
        match missing_requirement(instance, physical_device) {
            None => usable.push(physical_device),
            Some(reason) => last_reason = Some(reason),
        }
    }
    if usable.is_empty() {
        let reason = last_reason.unwrap_or_else(|| "an unknown requirement".to_string());
        bail!(
            "no usable Vulkan device ({} candidate(s) checked; last one was missing {reason})",
            all.len()
        );
    }

    if let Some(render_node) = render_node {
        let want = dev_major_minor(render_node);
        if let Some(&matched) = usable
            .iter()
            .find(|&&pd| drm_node_matches(instance, pd, want))
        {
            return Ok(matched);
        }
        // No match: fall through to the ordinary preference order below.
    }

    // Safety: read-only query, called once per candidate below.
    let discrete = usable
        .iter()
        .find(|&&pd| unsafe { instance.get_physical_device_properties(pd) }.device_type == vk::PhysicalDeviceType::DISCRETE_GPU);
    Ok(*discrete.unwrap_or(&usable[0]))
}

fn pick_queue_family(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> anyhow::Result<u32> {
    // Safety: read-only query.
    let families = unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
    families
        .iter()
        .position(|family| family.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .map(|index| index as u32)
        .ok_or_else(|| anyhow!("device has no graphics-capable queue family"))
}

/// Which of the three global-priority extension names — see the module
/// doc's "Queue priority" section — `physical_device` advertises, in the
/// order `create_device` should enable them. Empty means none: the queue
/// this module creates gets whatever priority the driver defaults to,
/// exactly as before this feature existed.
///
/// `VK_KHR_global_priority` alone is enough for both querying supported
/// tiers and requesting one, so it wins outright when present. The older,
/// split pair otherwise needs both names to do the same job:
/// `VK_EXT_global_priority` alone still lets `create_device` request a tier
/// (just blind, with no query to consult first — see its ladder),
/// `VK_EXT_global_priority_query` on its own would add nothing (querying
/// without the ability to request is useless here), so it is only ever
/// enabled alongside the base extension.
fn supported_global_priority_extensions(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Vec<&'static CStr> {
    // Safety: read-only query.
    let Ok(extensions) =
        (unsafe { instance.enumerate_device_extension_properties(physical_device) })
    else {
        return Vec::new();
    };
    let has = |name: &CStr| {
        extensions.iter().any(|extension| {
            // Safety: `extension_name` is a NUL-terminated byte array filled
            // in by the driver.
            unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) == name }
        })
    };
    if has(ash::khr::global_priority::NAME) {
        return vec![ash::khr::global_priority::NAME];
    }
    let mut found = Vec::new();
    if has(ash::ext::global_priority::NAME) {
        found.push(ash::ext::global_priority::NAME);
        if has(ash::ext::global_priority_query::NAME) {
            found.push(ash::ext::global_priority_query::NAME);
        }
    }
    found
}

/// The global priorities `queue_family` supports, via
/// `VkQueueFamilyGlobalPriorityPropertiesKHR` — empty when the driver
/// reports the `globalPriorityQuery` feature unsupported (checked first,
/// since the property is documented to carry nothing meaningful otherwise)
/// or either query call fails outright.
fn queried_queue_priorities(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
) -> Vec<vk::QueueGlobalPriorityKHR> {
    let mut query_feature = vk::PhysicalDeviceGlobalPriorityQueryFeaturesKHR::default();
    let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut query_feature);
    // Safety: `features2`'s chain outlives the call.
    unsafe { instance.get_physical_device_features2(physical_device, &mut features2) };
    if query_feature.global_priority_query == vk::FALSE {
        return Vec::new();
    }

    // Safety: read-only query; length-then-fill, the same two-call shape as
    // `supported_modifiers` above.
    let family_count =
        unsafe { instance.get_physical_device_queue_family_properties2_len(physical_device) };
    if queue_family as usize >= family_count {
        return Vec::new();
    }
    let mut priority_props =
        vec![vk::QueueFamilyGlobalPriorityPropertiesKHR::default(); family_count];
    let mut properties2: Vec<vk::QueueFamilyProperties2> = priority_props
        .iter_mut()
        .map(|props| vk::QueueFamilyProperties2::default().push_next(props))
        .collect();
    // Safety: every element of `properties2` outlives the call, each
    // chained to the `priority_props` entry at the same index — a `Vec`
    // already sized to its final length above, so nothing here reallocates
    // or moves out from under those pointers.
    unsafe {
        instance.get_physical_device_queue_family_properties2(physical_device, &mut properties2)
    };
    drop(properties2);
    priority_props[queue_family as usize]
        .priorities_as_slice()
        .to_vec()
}

/// What `create_device`'s ladder ends up meaning for `describe()`.
fn priority_tier(candidate: Option<vk::QueueGlobalPriorityKHR>) -> QueuePriority {
    match candidate {
        Some(vk::QueueGlobalPriorityKHR::REALTIME) => QueuePriority::Realtime,
        Some(vk::QueueGlobalPriorityKHR::HIGH) => QueuePriority::High,
        Some(vk::QueueGlobalPriorityKHR::MEDIUM) => QueuePriority::Medium,
        _ => QueuePriority::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_maps_to_the_right_vulkan_format() {
        assert_eq!(
            format_for_fourcc(FOURCC_XR24).unwrap(),
            vk::Format::B8G8R8A8_UNORM
        );
        assert_eq!(
            format_for_fourcc(FOURCC_AR24).unwrap(),
            vk::Format::B8G8R8A8_UNORM
        );
        assert_eq!(
            format_for_fourcc(FOURCC_XB24).unwrap(),
            vk::Format::R8G8B8A8_UNORM
        );
        assert_eq!(
            format_for_fourcc(FOURCC_AB24).unwrap(),
            vk::Format::R8G8B8A8_UNORM
        );
        assert!(format_for_fourcc(0x1234_5678).is_err());
    }

    #[test]
    fn transfer_packing_matches_the_shaders_unpacking() {
        // `blend.frag`: `a = ab >> 8u; b = ab & 0xffu;`
        let packed = pack_transfer(300, 7);
        assert_eq!(packed >> 8, 300);
        assert_eq!(packed & 0xff, 7);

        // The identity transfer (`a = 256, b = 0`, from `pixel_transfer` in
        // blend.rs when nothing is ramped or lifted) must round-trip too.
        let identity = pack_transfer(256, 0);
        assert_eq!(identity >> 8, 256);
        assert_eq!(identity & 0xff, 0);
    }

    #[test]
    fn dynamic_transfer_tag_and_reserved_bits_are_unambiguous() {
        let dynamic = 0x8000_0000 | 0x0000_beef | (127 << 16) | (8 << 24);
        assert!(validate_packed_transfer(dynamic).is_ok());
        assert!(validate_packed_transfer(dynamic | (1 << 28)).is_err());
        assert!(validate_packed_transfer(0x8000_0000 | (9 << 24)).is_err());
        assert!(validate_packed_transfer(pack_transfer(256, 255)).is_ok());
        assert!(validate_packed_transfer(257 << 8).is_err());
    }

    #[test]
    fn source_luminance_uses_linear_rec709_not_signal_average() {
        let black = [0, 0, 0, 255];
        let white = [255, 255, 255, 255];
        assert_eq!(mean_linear_luminance_rgba(&black), 0.0);
        assert!((mean_linear_luminance_rgba(&white) - 1.0).abs() < 1e-12);
        // Mid-signal gray is about 0.214 in linear light, not 0.5.
        let middle = [128, 128, 128, 0];
        assert!((mean_linear_luminance_rgba(&middle) - 0.215_86).abs() < 0.001);
    }

    #[test]
    fn static_canvas_requires_sampling_linear_filtering_and_upload() {
        let required = vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::TRANSFER_DST;
        assert!(static_canvas_features_supported(required));
        assert!(!static_canvas_features_supported(
            required & !vk::FormatFeatureFlags::TRANSFER_DST
        ));
        assert!(!static_canvas_features_supported(
            required & !vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
        ));
    }

    #[test]
    fn push_constants_match_warp_shader_layout() {
        assert_eq!(std::mem::size_of::<PushConstants>(), 128);
        assert_eq!(std::mem::offset_of!(PushConstants, inverse_rows), 32);
        assert_eq!(std::mem::offset_of!(PushConstants, center), 80);
        assert_eq!(std::mem::offset_of!(PushConstants, warp_enabled), 88);
        assert_eq!(std::mem::offset_of!(PushConstants, padding), 92);
        assert_eq!(std::mem::offset_of!(PushConstants, source_rect), 96);
        assert_eq!(std::mem::offset_of!(PushConstants, dynamic_lift), 112);
        assert_eq!(std::mem::offset_of!(PushConstants, dynamic_maximum), 116);
        assert_eq!(std::mem::align_of::<PushConstants>(), 4);
        // Vulkan guarantees only 128 bytes of push constants, and a device
        // that offers exactly that must still take this block.
        assert!(std::mem::size_of::<PushConstants>() <= 128);
    }

    #[test]
    fn the_shape_buffer_holds_the_worst_case_sync_frame() {
        // `set_sync_shapes` refuses anything longer, and the slicer has no
        // fallback for that refusal — so the two constants have to be
        // checked against each other rather than assumed compatible.
        let groups = 8usize;
        let worst = groups * 2 + crate::projection::pattern::MAX_SYNC_RECTS;
        assert!(
            worst <= SYNC_SHAPE_SLOTS,
            "a worst-case sync frame is {worst} items but the buffer holds {SYNC_SHAPE_SLOTS}"
        );
    }

    #[test]
    fn embedded_spirv_blobs_start_with_the_spirv_magic_number() {
        for spv in [VERT_SPV, FRAG_SPV] {
            assert!(
                spv.len() >= 4,
                "shader blob too short to hold a SPIR-V header"
            );
            let magic = u32::from_le_bytes(spv[0..4].try_into().unwrap());
            assert_eq!(magic, 0x0723_0203, "missing the SPIR-V magic number");
        }
    }

    #[test]
    fn dev_major_minor_matches_glibc_makedev() {
        // /dev/dri/renderD128 is conventionally major 226, minor 128.
        let dev = (226u64 << 8) | 128;
        assert_eq!(dev_major_minor(dev), (226, 128));
        // A minor number past the low 8 bits spills into the high field —
        // exactly the case `major()`/`minor()` exist to get right.
        let dev = ((226u64 & 0xfff) << 8) | (300 & 0xff) | ((300u64 & !0xff) << 12);
        assert_eq!(dev_major_minor(dev), (226, 300));
    }

    #[test]
    fn readback_prefers_cached_memory_and_retains_visible_fallback() {
        let mut properties = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        properties.memory_types[0].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        properties.memory_types[1].property_flags = vk::MemoryPropertyFlags::HOST_CACHED;
        properties.memory_types[2].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED;
        assert_eq!(readback_memory_type_index(&properties, 0b111), Some(2));
        assert_eq!(readback_memory_type_index(&properties, 0b011), Some(0));
        assert_eq!(readback_memory_type_index(&properties, 0b010), None);
    }

    #[test]
    fn memory_type_index_only_considers_allowed_bits() {
        let mut memory_types = [vk::MemoryType::default(); vk::MAX_MEMORY_TYPES];
        memory_types[0] = vk::MemoryType {
            property_flags: vk::MemoryPropertyFlags::DEVICE_LOCAL,
            heap_index: 0,
        };
        memory_types[1] = vk::MemoryType {
            property_flags: vk::MemoryPropertyFlags::DEVICE_LOCAL,
            heap_index: 0,
        };
        memory_types[2] = vk::MemoryType {
            property_flags: vk::MemoryPropertyFlags::DEVICE_LOCAL
                | vk::MemoryPropertyFlags::HOST_VISIBLE,
            heap_index: 0,
        };
        let properties = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            memory_types,
            ..Default::default()
        };
        // Bit 1 (type index 1) is excluded even though it matches, and type
        // 0 does not carry HOST_VISIBLE — only type 2 qualifies.
        let index = memory_type_index(&properties, 0b101, vk::MemoryPropertyFlags::HOST_VISIBLE);
        assert_eq!(index, Some(2));
        assert_eq!(
            memory_type_index(&properties, 0b101, vk::MemoryPropertyFlags::DEVICE_LOCAL),
            Some(0),
            "the lowest allowed, matching index should win"
        );
        // Only index 2 carries HOST_VISIBLE; excluding it (bit 2 unset)
        // must fail the search even though 0 and 1 are DEVICE_LOCAL.
        assert_eq!(
            memory_type_index(&properties, 0b011, vk::MemoryPropertyFlags::HOST_VISIBLE),
            None
        );
    }

    /// WSL has a Vulkan loader (`/dev/dxg` exists), but which ICD it finds
    /// is a property of the distro image, not something this module
    /// controls: at the time of writing the dev container registers only
    /// Mesa's `lvp` software rasterizer, capped at Vulkan 1.1, so
    /// `Gpu::new` is expected to fail here with a clear "missing Vulkan
    /// 1.3" error rather than get far enough to touch dmabufs or the
    /// embedded SPIR-V — still a useful run, since it is this error path
    /// that matters most on a real target with no usable GPU. Run with
    /// `SUEDE_GPU_TEST=1` to try it against whatever Vulkan the current
    /// machine actually has.
    #[test]
    #[ignore]
    fn a_real_device_can_be_opened() {
        if std::env::var_os("SUEDE_GPU_TEST").is_none() {
            return;
        }
        match Gpu::new(None) {
            Ok(gpu) => println!("Gpu::new succeeded: {}", gpu.describe()),
            Err(error) => panic!("Gpu::new failed: {error:#}"),
        }
    }
}

#[cfg(test)]
#[path = "gpu_readback.rs"]
mod readback;
