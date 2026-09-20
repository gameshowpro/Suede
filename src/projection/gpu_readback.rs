//! Offscreen validation of the shipped shaders and production pipeline setup.
//! Local optimal images and staging buffers isolate sampling from Wayland and
//! dmabuf ownership. These tests do not claim compositor/presentation coverage.

use super::*;
use crate::projection::warp::Warp;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn embedded_fragment_offsets_match_every_rust_push_constant_member() {
    let words: Vec<u32> = FRAG_SPV
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    assert_eq!(words[0], 0x0723_0203);
    let mut structs = BTreeMap::new();
    let mut offsets: BTreeMap<u32, BTreeMap<u32, u32>> = BTreeMap::new();
    let mut push_types = Vec::new();
    let mut index = 5;
    while index < words.len() {
        let count = (words[index] >> 16) as usize;
        let op = words[index] & 0xffff;
        assert!(count > 0 && index + count <= words.len());
        let args = &words[index + 1..index + count];
        match op {
            30 => {
                structs.insert(args[0], args[1..].to_vec());
            }
            32 if args[1] == 9 => push_types.push(args[2]),
            72 if args[2] == 35 => {
                offsets.entry(args[0]).or_default().insert(args[1], args[3]);
            }
            _ => {}
        }
        index += count;
    }
    // naga emits pointers to both the outer block and the inner user struct.
    let push_types: BTreeSet<_> = push_types
        .into_iter()
        .map(|ty| {
            if structs[&ty].len() == 1 && offsets[&ty].get(&0) == Some(&0) {
                structs[&ty][0]
            } else {
                ty
            }
        })
        .collect();
    assert_eq!(push_types.len(), 1);
    let ty = *push_types.first().unwrap();
    let rows = std::mem::offset_of!(PushConstants, inverse_rows);
    let expected = [
        std::mem::offset_of!(PushConstants, source_x),
        std::mem::offset_of!(PushConstants, source_y),
        std::mem::offset_of!(PushConstants, width),
        std::mem::offset_of!(PushConstants, height),
        std::mem::offset_of!(PushConstants, canvas_height),
        std::mem::offset_of!(PushConstants, y_invert),
        std::mem::offset_of!(PushConstants, mode),
        std::mem::offset_of!(PushConstants, groups),
        rows,
        rows + 16,
        rows + 32,
        std::mem::offset_of!(PushConstants, center),
        std::mem::offset_of!(PushConstants, warp_enabled),
        std::mem::offset_of!(PushConstants, padding),
        std::mem::offset_of!(PushConstants, source_rect),
        std::mem::offset_of!(PushConstants, dynamic_lift),
        std::mem::offset_of!(PushConstants, dynamic_maximum),
        std::mem::offset_of!(PushConstants, dynamic_padding),
        std::mem::offset_of!(PushConstants, dynamic_padding) + 4,
    ];
    assert_eq!(structs[&ty].len(), expected.len());
    let actual: Vec<_> = offsets[&ty].values().map(|v| *v as usize).collect();
    assert_eq!(actual, expected);
    assert_eq!(
        actual.last().unwrap() + std::mem::size_of::<u32>(),
        std::mem::size_of::<PushConstants>()
    );
}

#[test]
fn exact_sampling_does_not_require_linear_filtering_but_warp_does() {
    assert!(validate_filtering(false, false).is_ok());
    assert!(validate_filtering(true, false).is_ok());
    assert!(validate_filtering(true, true).is_ok());
    assert!(validate_filtering(false, true)
        .unwrap_err()
        .to_string()
        .contains("warp unavailable"));
}

struct Image {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    size: [u32; 2],
    initialized: Cell<bool>,
    device: Arc<DeviceState>,
}

impl Image {
    fn new(gpu: &Gpu, size: [u32; 2], format: vk::Format) -> anyhow::Result<Self> {
        let device = &gpu.device.device;
        let properties = unsafe {
            gpu.device
                .instance
                .get_physical_device_format_properties(gpu.device.physical_device, format)
        };
        let needed = vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::TRANSFER_SRC
            | vk::FormatFeatureFlags::TRANSFER_DST;
        anyhow::ensure!(
            properties.optimal_tiling_features.contains(needed),
            "test image format unsupported"
        );
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: size[0],
                height: size[1],
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // Safety: all create-info borrows and the shared device remain alive.
        let image = unsafe { device.create_image(&info, None) }?;
        let mut cleanup = Cleanup {
            device,
            image,
            memory: None,
            view: None,
        };
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let properties = unsafe {
            gpu.device
                .instance
                .get_physical_device_memory_properties(gpu.device.physical_device)
        };
        let index = memory_type_index(
            &properties,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .context("test device-local memory")?;
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index);
        let memory = unsafe { device.allocate_memory(&allocation, None) }?;
        cleanup.memory = Some(memory);
        unsafe { device.bind_image_memory(image, memory, 0) }?;
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(color_subresource_range());
        let view = unsafe { device.create_image_view(&info, None) }?;
        cleanup.view = Some(view);
        cleanup.defuse();
        Ok(Self {
            image,
            memory,
            view,
            size,
            initialized: Cell::new(false),
            device: Arc::clone(&gpu.device),
        })
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // Safety: the harness waits for completion before any resource drops.
        unsafe {
            let _ = self.device.device.device_wait_idle();
            self.device.device.destroy_image_view(self.view, None);
            self.device.device.destroy_image(self.image, None);
            self.device.device.free_memory(self.memory, None);
        }
    }
}

struct Buffer(Option<HostBuffer>, Arc<DeviceState>);
impl Buffer {
    fn new(gpu: &Gpu, bytes: usize, usage: vk::BufferUsageFlags) -> anyhow::Result<Self> {
        Ok(Self(
            Some(allocate_host_buffer_with_usage(
                &gpu.device,
                bytes as u64,
                usage,
            )?),
            Arc::clone(&gpu.device),
        ))
    }
    fn get(&self) -> &HostBuffer {
        self.0.as_ref().unwrap()
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        // Safety: completion is awaited even on a failed test submission.
        unsafe {
            let _ = self.1.device.device_wait_idle();
        }
        if let Some(buffer) = self.0.take() {
            buffer.destroy(&self.1.device);
        }
    }
}

fn image_barrier(
    image: vk::Image,
    old: vk::ImageLayout,
    src: vk::AccessFlags,
    dst: vk::AccessFlags,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .image(image)
        .old_layout(old)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_access_mask(src)
        .dst_access_mask(dst)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .subresource_range(color_subresource_range())
}

fn copy_region(size: [u32; 2]) -> vk::BufferImageCopy {
    vk::BufferImageCopy::default()
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_extent(vk::Extent3D {
            width: size[0],
            height: size[1],
            depth: 1,
        })
}

// Exercises the production descriptor layout, immutable sampler, pipeline,
// SPIR-V, transfer upload and push-constant bytes, using local image ownership.
#[derive(Clone, Copy)]
enum TestCanvas<'a> {
    Pixels(&'a Image, &'a [u8]),
    Static(&'a StaticCanvas),
}

fn draw(
    gpu: &mut Gpu,
    canvas: &Image,
    target: &Image,
    pixels: &[u8],
    constants: PushConstants,
    table: &[(u16, u8)],
    format: vk::Format,
) -> anyhow::Result<Vec<u8>> {
    gpu.set_transfer(2, target.size[0], target.size[1], table)?;
    draw_installed(
        gpu,
        TestCanvas::Pixels(canvas, pixels),
        target,
        constants,
        format,
    )
}

/// Render using the table already bound to the sparse test output. This keeps
/// the transaction test below honest: it exercises the descriptor installed by
/// `replace_transfers`, rather than replacing it again in a helper.
fn draw_installed(
    gpu: &mut Gpu,
    canvas: TestCanvas<'_>,
    target: &Image,
    constants: PushConstants,
    format: vk::Format,
) -> anyhow::Result<Vec<u8>> {
    let output = 2; // sparse index is intentional
    gpu.ensure_pipeline(format)?;
    let upload = match canvas {
        TestCanvas::Pixels(_, pixels) => Some(Buffer::new(
            gpu,
            pixels.len(),
            vk::BufferUsageFlags::TRANSFER_SRC,
        )?),
        TestCanvas::Static(_) => None,
    };
    let readback_bytes = target.size[0] as usize * target.size[1] as usize * 4;
    let readback = Buffer::new(gpu, readback_bytes, vk::BufferUsageFlags::TRANSFER_DST)?;
    // Safety: exact-sized host-coherent allocations, with no in-flight use.
    if let (TestCanvas::Pixels(_, pixels), Some(upload)) = (canvas, upload.as_ref()) {
        // Safety: exact-sized host-coherent allocations, with no in-flight use.
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), upload.get().mapped, pixels.len());
        }
    }
    let device = &gpu.device.device;
    let cb = gpu.command_buffer;
    let set = gpu.outputs[output].as_ref().unwrap().descriptor_set;
    let canvas_view = match canvas {
        TestCanvas::Pixels(image, _) => image.view,
        TestCanvas::Static(image) => image.view,
    };
    let canvas_info = [vk::DescriptorImageInfo::default()
        .image_view(canvas_view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let write = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
        .image_info(&canvas_info)];
    // Safety: all handles belong to this device. Each draw waits before the
    // next descriptor update or command-buffer reset. Images start undefined.
    unsafe {
        device.update_descriptor_sets(&write, &[]);
        device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
        device.begin_command_buffer(
            cb,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        match canvas {
            TestCanvas::Pixels(image, _) => {
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[image_barrier(
                        image.image,
                        vk::ImageLayout::UNDEFINED,
                        vk::AccessFlags::empty(),
                        vk::AccessFlags::TRANSFER_WRITE,
                    )],
                );
                device.cmd_copy_buffer_to_image(
                    cb,
                    upload.as_ref().unwrap().get().buffer,
                    image.image,
                    vk::ImageLayout::GENERAL,
                    &[copy_region(image.size)],
                );
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[image_barrier(
                        image.image,
                        vk::ImageLayout::GENERAL,
                        vk::AccessFlags::TRANSFER_WRITE,
                        vk::AccessFlags::SHADER_READ,
                    )],
                );
            }
            TestCanvas::Static(image) => {
                device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[image_barrier(
                        image.image,
                        vk::ImageLayout::GENERAL,
                        vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_READ,
                        vk::AccessFlags::SHADER_READ,
                    )],
                );
            }
        }
        device.cmd_pipeline_barrier(
            cb,
            if target.initialized.get() {
                vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
            } else {
                vk::PipelineStageFlags::TOP_OF_PIPE
            },
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[image_barrier(
                target.image,
                if target.initialized.get() {
                    vk::ImageLayout::GENERAL
                } else {
                    vk::ImageLayout::UNDEFINED
                },
                if target.initialized.get() {
                    vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                } else {
                    vk::AccessFlags::empty()
                },
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            )],
        );
        let attachments = [vk::RenderingAttachmentInfo::default()
            .image_view(target.view)
            .image_layout(vk::ImageLayout::GENERAL)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)];
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: target.size[0],
                height: target.size[1],
            },
        };
        device.cmd_begin_rendering(
            cb,
            &vk::RenderingInfo::default()
                .render_area(area)
                .layer_count(1)
                .color_attachments(&attachments),
        );
        device.cmd_bind_pipeline(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            gpu.pipeline.as_ref().unwrap().pipeline,
        );
        device.cmd_set_viewport(
            cb,
            0,
            &[vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: target.size[0] as f32,
                height: target.size[1] as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            }],
        );
        device.cmd_set_scissor(cb, 0, &[area]);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            gpu.pipeline_layout,
            0,
            &[set],
            &[],
        );
        device.cmd_push_constants(
            cb,
            gpu.pipeline_layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            constants.as_bytes(),
        );
        device.cmd_draw(cb, 3, 1, 0, 0);
        device.cmd_end_rendering(cb);
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[image_barrier(
                target.image,
                vk::ImageLayout::GENERAL,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::AccessFlags::TRANSFER_READ,
            )],
        );
        device.cmd_copy_image_to_buffer(
            cb,
            target.image,
            vk::ImageLayout::GENERAL,
            readback.get().buffer,
            &[copy_region(target.size)],
        );
        let host = vk::BufferMemoryBarrier::default()
            .buffer(readback.get().buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ);
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &[host],
            &[],
        );
        device.end_command_buffer(cb)?;
        device.reset_fences(&[gpu.fence])?;
        let buffers = [cb];
        device.queue_submit(
            gpu.device.queue,
            &[vk::SubmitInfo::default().command_buffers(&buffers)],
            gpu.fence,
        )?;
        device.wait_for_fences(&[gpu.fence], true, 10_000_000_000)?;
        target.initialized.set(true);
        Ok(std::slice::from_raw_parts(readback.get().mapped, readback_bytes).to_vec())
    }
}

fn encode(mut rgba: [u8; 4], format: vk::Format) -> [u8; 4] {
    if format == vk::Format::B8G8R8A8_UNORM {
        rgba.swap(0, 2);
    }
    rgba
}

fn fixture(size: [u32; 2], spectrum: bool) -> Vec<[u8; 4]> {
    (0..size[1])
        .flat_map(|y| {
            (0..size[0]).map(move |x| {
                if spectrum {
                    [
                        x as u8,
                        (x * 37 + y * 17) as u8,
                        (255 - x) as u8,
                        (x + y) as u8,
                    ]
                } else {
                    [
                        (x * 7 + y * 3) as u8,
                        (x * 2 + y * 11) as u8,
                        (190 - x * 3 + y * 2) as u8,
                        (x + y) as u8,
                    ]
                }
            })
        })
        .collect()
}

fn reference(
    canvas: &[[u8; 4]],
    canvas_size: [u32; 2],
    size: [u32; 2],
    origin: [u32; 2],
    invert: bool,
    warp: Option<&Warp>,
    table: &[(u16, u8)],
) -> Vec<[u8; 4]> {
    (0..size[1])
        .flat_map(|y| {
            (0..size[0]).map(move |x| {
                let index = (y * size[0] + x) as usize;
                let mut source = warp.map_or(
                    [
                        x as f64 + 0.5 + origin[0] as f64,
                        y as f64 + 0.5 + origin[1] as f64,
                    ],
                    |w| {
                        let p = w.source_at(x as f64 + 0.5, y as f64 + 0.5).unwrap();
                        let rect = w.source_rect().unwrap_or([
                            origin[0] as f64,
                            origin[1] as f64,
                            size[0] as f64,
                            size[1] as f64,
                        ]);
                        std::array::from_fn(|axis| {
                            let inset = (rect[axis + 2] * 0.5).min(0.5);
                            (rect[axis] + p[axis] / size[axis] as f64 * rect[axis + 2]).clamp(
                                rect[axis] + inset,
                                rect[axis] + (rect[axis + 2] - inset).max(inset),
                            )
                        })
                    },
                );
                if invert {
                    source[1] = canvas_size[1] as f64 - source[1];
                }
                if source[0] < 0.5
                    || source[1] < 0.5
                    || source[0] > canvas_size[0] as f64 - 0.5
                    || source[1] > canvas_size[1] as f64 - 0.5
                {
                    return [0, 0, 0, 255];
                }
                let sx = source[0] - 0.5;
                let sy = source[1] - 0.5;
                let x0 = sx.floor() as u32;
                let y0 = sy.floor() as u32;
                let tx = sx - x0 as f64;
                let ty = sy - y0 as f64;
                let mut rgba = [0, 0, 0, 255];
                let (a, b) = table[index];
                for c in 0..3 {
                    let at = |dx: u32, dy: u32| {
                        f64::from(
                            canvas[((y0 + dy).min(canvas_size[1] - 1) * canvas_size[0]
                                + (x0 + dx).min(canvas_size[0] - 1))
                                as usize][c],
                        )
                    };
                    let value = ((1.0 - ty) * ((1.0 - tx) * at(0, 0) + tx * at(1, 0))
                        + ty * ((1.0 - tx) * at(0, 1) + tx * at(1, 1)))
                    .round() as u32;
                    rgba[c] = (((u32::from(a) * value) >> 8) + u32::from(b)).min(255) as u8;
                }
                rgba
            })
        })
        .collect()
}

#[test]
#[ignore = "requires a Vulkan 1.3 GPU; run explicitly on brain"]
fn shipped_shader_matches_reference_pixels() -> anyhow::Result<()> {
    let formats = [vk::Format::R8G8B8A8_UNORM, vk::Format::B8G8R8A8_UNORM];
    let mut cases = 0;
    let mut worst_error = 0;
    for target_format in formats {
        let mut gpu = Gpu::new(None)?;
        println!("GPU readback: {}", gpu.describe());
        for source_format in formats {
            for invert in [false, true] {
                for case in 0..18 {
                    let spectrum = case < 2;
                    let size = if spectrum { [256, 7] } else { [17, 9] };
                    let canvas_size = if spectrum { [256, 9] } else { [23, 15] };
                    let mut origin = if spectrum { [0, 1] } else { [3, 2] };
                    let w = size[0] as f64;
                    let h = size[1] as f64;
                    let identity = [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]];
                    let (pins, center) = match case {
                        3 => (
                            [[2.0, 1.5], [w - 1.0, 0.25], [w, h - 0.5], [0.5, h]],
                            [0.5, 0.5],
                        ),
                        4 => (identity, [0.3, 0.7]),
                        5 => ([[0.5, 0.5], [w, 0.5], [w, h], [0.5, h]], [0.5, 0.5]),
                        6 | 16 => (
                            [[-2.0, -1.0], [w + 2.0, 0.0], [w + 1.0, h + 1.0], [-1.0, h]],
                            [0.5, 0.5],
                        ),
                        _ => (identity, [0.5, 0.5]),
                    };
                    if (7..11).contains(&case) {
                        origin = [canvas_size[0] - 3, canvas_size[1] - 2];
                    }
                    let mut warp = Warp::new(pins, center, size[0], size[1]).unwrap();
                    if (9..11).contains(&case) {
                        origin = [u32::MAX, u32::MAX];
                    }
                    let rect = match case {
                        11 => Some([3.25, 2.375, w, h]),           // fractional placement
                        12 => Some([2.0, 1.0, w * 0.75, h * 1.4]), // unequal density
                        // Actual rounded canvas height determines vertical density.
                        13 => Some([3.0, 2.0, w, h * 15.0 / 14.75]),
                        14 => Some([-3.25, -2.125, w * 1.5, h * 1.75]), // clipping
                        15 => Some([5.25, 3.125, 0.25, 0.375]),         // subpixel extent
                        16 => Some([1.125, 0.875, w * 1.125, h * 1.375]), // overscan
                        17 => Some([0.0, 0.0, 23.0, 15.0]),             // first/last rows, scale
                        _ => None,
                    };
                    if let Some(rect) = rect {
                        warp = warp.with_source_rect(rect).unwrap();
                    }
                    let enabled = !matches!(case, 0 | 7 | 9);
                    let mapping = enabled.then_some(&warp);
                    let rgba = fixture(canvas_size, spectrum);
                    let pixels: Vec<u8> = rgba
                        .iter()
                        .flat_map(|p| encode(*p, source_format))
                        .collect();
                    // Independent pre-warp arithmetic: gamma-shaped right ramp
                    // and lift, with exact output-pixel border coverage applied.
                    let table: Vec<_> = (0..size[1])
                        .flat_map(|y| (0..size[0]).map(move |x| (x, y)))
                        .map(|(x, y)| {
                            let e = mapping.map_or(1.0, |m| m.coverage(x, y));
                            let lift = [0.0, 0.1, 0.5, 1.0][(y % 4) as usize];
                            let r = (1.0 - (x as f64 + 0.5) / w).powf(1.0 / 2.2);
                            (
                                (e * (1.0 - lift) * r * 256.0).round() as u16,
                                (e * lift * 255.0).round() as u8,
                            )
                        })
                        .collect();
                    let expected =
                        reference(&rgba, canvas_size, size, origin, invert, mapping, &table);
                    let constants =
                        PushConstants::canvas(origin, size, canvas_size[1], invert, mapping);
                    let canvas = Image::new(&gpu, canvas_size, source_format)?;
                    let target = Image::new(&gpu, size, target_format)?;
                    let got = draw(
                        &mut gpu,
                        &canvas,
                        &target,
                        &pixels,
                        constants,
                        &table,
                        target_format,
                    )?;
                    let tolerance = if matches!(case, 3..=6 | 11..=17) {
                        1
                    } else {
                        0
                    };
                    for (i, (got, want)) in got.chunks_exact(4).zip(expected).enumerate() {
                        let want = encode(want, target_format);
                        assert_eq!(got[3], 255, "alpha case {case} pixel {i}");
                        for channel in 0..3 {
                            let error = got[channel].abs_diff(want[channel]);
                            worst_error = worst_error.max(error);
                            assert!(
                                error <= tolerance,
                                "case={case} source={source_format:?} target={target_format:?} invert={invert} pixel={i} channel={channel}: {} != {}",
                                got[channel],
                                want[channel]
                            );
                        }
                    }
                    cases += 1;
                }
            }
        }
    }
    println!(
        "GPU readback passed: {cases} cases, worst channel error {worst_error} byte(s); identity/exact tolerance 0, warped tolerance 1"
    );
    Ok(())
}

#[test]
#[ignore = "requires a Vulkan 1.3 GPU; run explicitly on brain"]
fn tagged_dynamic_transfer_matches_the_documented_rounding_and_outside_black() -> anyhow::Result<()>
{
    use crate::projection::blend::{dynamic_shade, pack_dynamic_shape};
    let mut gpu = Gpu::new(None)?;
    let format = vk::Format::R8G8B8A8_UNORM;
    let size = [256, 9 * 5 * 5];
    let canvas = Image::new(&gpu, size, format)?;
    let target = Image::new(&gpu, size, format)?;
    let mut pixels = Vec::new();
    let mut table = Vec::new();
    for n in 0..=8 {
        for ramp in [0.0, 0.13, 0.5, 0.87, 1.0] {
            for edge in [0.0, 1.0 / 255.0, 0.5, 254.0 / 255.0, 1.0] {
                for value in 0..=255u8 {
                    pixels.extend_from_slice(&[value, 255 - value, value.wrapping_mul(37), 11]);
                    table.push(pack_dynamic_shape(ramp, edge, n));
                }
            }
        }
    }
    gpu.set_packed_transfer(2, size[0], size[1], &table)?;
    let mut worst = 0;
    for maximum in [1, 4, 8] {
        for level in [0.0, 0.01, 0.12, 0.3333333, 0.5] {
            gpu.set_dynamic_lift(level, maximum)?;
            let mut constants = PushConstants::canvas([0, 0], size, size[1], false, None);
            constants.dynamic_lift = level as f32;
            constants.dynamic_maximum = maximum;
            let got = draw_installed(
                &mut gpu,
                TestCanvas::Pixels(&canvas, &pixels),
                &target,
                constants,
                format,
            )?;
            for (index, pixel) in got.chunks_exact(4).enumerate() {
                assert_eq!(pixel[3], 255);
                for channel in 0..3 {
                    let expected =
                        dynamic_shade(table[index], pixels[index * 4 + channel], level, maximum);
                    let error = pixel[channel].abs_diff(expected);
                    worst = worst.max(error);
                    assert!(error <= 1, "dynamic parity pixel {index} channel {channel}, L={level} N={maximum}: {} vs {expected}", pixel[channel]);
                    if (table[index] >> 24) & 15 == 0
                        || (table[index] >> 16) & 255 == 0
                        || (table[index] & 0xffff == 65535 && pixels[index * 4 + channel] == 255)
                    {
                        assert_eq!(
                            pixel[channel], expected,
                            "outside black and covered white must be exact"
                        );
                    }
                }
            }
        }
    }
    println!(
        "Dynamic GPU parity: 864000 pixels, worst channel error {worst}; fixed tables untouched"
    );
    Ok(())
}

#[test]
#[ignore = "requires a Vulkan 1.3 GPU; run explicitly on brain"]
fn source_measurement_reuses_its_target_without_stale_samples_or_padding() -> anyhow::Result<()> {
    let mut gpu = Gpu::new(None)?;
    let white = [255u8, 255, 255, 255].repeat(4);
    let black = [0u8, 0, 0, 255].repeat(4);
    let first = gpu.upload_static_canvas_rgba(2, 2, &white)?;
    let (luminance, count) = gpu.measure_static_luminance(&first, 2, 2)?;
    assert_eq!(count, 4);
    assert!((luminance - 1.0).abs() < 1e-12);
    let dark = gpu.upload_static_canvas_rgba(2, 2, &black)?;
    let (luminance, count) = gpu.measure_static_luminance(&dark, 2, 2)?;
    assert_eq!(count, 4);
    assert_eq!(
        luminance, 0.0,
        "previous white measurement leaked into reset readback"
    );
    let third = gpu.upload_static_canvas_rgba(2, 2, &white)?;
    let (luminance, count) = gpu.measure_static_luminance(&third, 2, 2)?;
    assert_eq!(count, 4);
    assert!((luminance - 1.0).abs() < 1e-12);

    // The logical domain can be smaller than the backing canvas: exclude a
    // bright second column as allocation padding would be excluded in capture.
    let padded = gpu.upload_static_canvas_rgba(
        2,
        2,
        &[
            0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
        ],
    )?;
    let (luminance, count) =
        gpu.measure_luminance_inner(padded.image, padded.view, false, false, 1, 2)?;
    assert_eq!(count, 2);
    assert_eq!(luminance, 0.0);
    Ok(())
}

#[test]
#[ignore = "benchmark only; run explicitly on the target GPU"]
fn benchmark_source_measurement_grid_readback_25_frames() -> anyhow::Result<()> {
    // This is an offscreen draw+measurement microbenchmark, not a playback
    // claim. It retains source, target, transfer, and measurement resources.
    for size in [[1920, 1080], [3680, 2000]] {
        let mut gpu = Gpu::new(None)?;
        let pixels = vec![32u8, 32, 32, 255]
            .into_iter()
            .cycle()
            .take((size[0] * size[1] * 4) as usize)
            .collect::<Vec<_>>();
        let canvas = gpu.upload_static_canvas_rgba(size[0], size[1], &pixels)?;
        let target = Image::new(&gpu, [16, 16], vk::Format::R8G8B8A8_UNORM)?;
        let table = vec![(256, 0); 16 * 16];
        gpu.set_transfer(2, 16, 16, &table)?;
        let constants = PushConstants::canvas([0, 0], [16, 16], size[1], false, None);
        // Build retained resources outside timed samples.
        let _ = draw_installed(
            &mut gpu,
            TestCanvas::Static(&canvas),
            &target,
            constants,
            vk::Format::R8G8B8A8_UNORM,
        )?;
        let _ = gpu.measure_static_luminance(&canvas, size[0], size[1])?;
        let mut off_samples = Vec::with_capacity(25);
        for _ in 0..25 {
            let started = std::time::Instant::now();
            let _ = draw_installed(
                &mut gpu,
                TestCanvas::Static(&canvas),
                &target,
                constants,
                vk::Format::R8G8B8A8_UNORM,
            )?;
            off_samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        let mut on_samples = Vec::with_capacity(25);
        for _ in 0..25 {
            let started = std::time::Instant::now();
            let _ = draw_installed(
                &mut gpu,
                TestCanvas::Static(&canvas),
                &target,
                constants,
                vk::Format::R8G8B8A8_UNORM,
            )?;
            let _ = gpu.measure_static_luminance(&canvas, size[0], size[1])?;
            on_samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        let off_mean = off_samples.iter().sum::<f64>() / off_samples.len() as f64;
        let on_mean = on_samples.iter().sum::<f64>() / on_samples.len() as f64;
        println!(
            "source measurement {}x{} (one 16x16 offscreen target; not compositor playback): off mean {:.3} ms/frame samples {:?}; on mean {:.3} ms/frame samples {:?}; delta {:.3} ms/frame ({})",
            size[0],
            size[1],
            off_mean,
            off_samples,
            on_mean,
            on_samples,
            on_mean - off_mean,
            gpu.describe()
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires a Vulkan 1.3 GPU; run explicitly on brain"]
fn sync_shapes_keep_output_local_coordinates_with_scaled_source_rectangles() -> anyhow::Result<()> {
    let mut gpu = Gpu::new(None)?;
    let format = vk::Format::R8G8B8A8_UNORM;
    let size = [17, 9];
    let canvas = Image::new(&gpu, [1, 1], format)?;
    let target = Image::new(&gpu, size, format)?;
    let shape_rects = [[2, 1, 7, 4], [9, 5, 15, 8]];
    for center in [[0.5, 0.5], [0.3, 0.7]] {
        let local = Warp::new(
            [[0.0, 0.0], [17.0, 0.0], [17.0, 9.0], [0.0, 9.0]],
            center,
            size[0],
            size[1],
        )
        .map_err(anyhow::Error::msg)?;
        let table = vec![(256, 0); (size[0] * size[1]) as usize];
        gpu.set_transfer(2, size[0], size[1], &table)?;
        gpu.set_sync_shapes(
            2,
            1,
            &[[2, 1, 15, 8], [2, 4, 0, 0], shape_rects[0], shape_rects[1]],
        )?;
        let expected: Vec<_> = (0..size[1])
            .flat_map(|y| (0..size[0]).map(move |x| (x, y)))
            .flat_map(|(x, y)| {
                let p = local.source_at(x as f64 + 0.5, y as f64 + 0.5).unwrap();
                let q = [p[0].clamp(0.5, 16.5), p[1].clamp(0.5, 8.5)];
                let lit = shape_rects.iter().any(|r| {
                    q[0] >= r[0] as f64
                        && q[0] < r[2] as f64
                        && q[1] >= r[1] as f64
                        && q[1] < r[3] as f64
                });
                let byte = if lit { 255 } else { 0 };
                [byte, byte, byte, 255]
            })
            .collect();
        for rect in [[3.25, 2.375, 25.5, 4.5], [-30.25, 100.875, 0.25, 25.5]] {
            let mapping = local
                .clone()
                .with_source_rect(rect)
                .map_err(anyhow::Error::msg)?;
            for invert in [false, true] {
                let mut constants = PushConstants::canvas([0, 0], size, 1, invert, Some(&mapping));
                constants.mode = 1;
                constants.groups = 1;
                let got = draw_installed(
                    &mut gpu,
                    TestCanvas::Pixels(&canvas, &[0, 0, 0, 255]),
                    &target,
                    constants,
                    format,
                )?;
                assert_eq!(
                    got, expected,
                    "center={center:?}, rect={rect:?}, invert={invert}"
                );
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a Vulkan 1.3 GPU; run explicitly on brain"]
fn transfer_replacement_is_atomic_and_reaches_the_shader() -> anyhow::Result<()> {
    let mut gpu = Gpu::new(None)?;
    let format = vk::Format::R8G8B8A8_UNORM;
    let size = [4, 4];
    let canvas = Image::new(&gpu, size, format)?;
    let target = Image::new(&gpu, size, format)?;
    let original = vec![(256, 0); 16];
    let untouched = vec![(128, 17); 16];
    gpu.replace_transfers(&[
        TransferUpdate {
            index: 2,
            width: size[0],
            height: size[1],
            table: &original,
        },
        TransferUpdate {
            index: 5,
            width: size[0],
            height: size[1],
            table: &untouched,
        },
    ])?;
    let old_output = gpu.outputs[2].as_ref().unwrap().buffer;
    let old_untouched = gpu.outputs[5].as_ref().unwrap().buffer;
    let old_first = unsafe { *(gpu.outputs[2].as_ref().unwrap().mapped as *const u32) };
    gpu.replace_transfers(&[
        TransferUpdate {
            index: 2,
            width: size[0],
            height: size[1],
            table: &original,
        },
        TransferUpdate {
            index: 5,
            width: size[0],
            height: size[1],
            table: &untouched,
        },
    ])?;
    assert_eq!(gpu.outputs[2].as_ref().unwrap().buffer, old_output);
    assert_eq!(gpu.outputs[5].as_ref().unwrap().buffer, old_untouched);

    // The invalid member is discovered during the all-input validation pass,
    // before output 2 is allocated, uploaded, rebound, or retired.
    let replacement = vec![(0, 0); 16];
    let invalid = vec![(257, 0); 16];
    assert!(gpu
        .replace_transfers(&[
            TransferUpdate {
                index: 2,
                width: size[0],
                height: size[1],
                table: &replacement,
            },
            TransferUpdate {
                index: 6,
                width: size[0],
                height: size[1],
                table: &invalid,
            },
        ])
        .is_err());
    assert_eq!(gpu.outputs[2].as_ref().unwrap().buffer, old_output);
    assert_eq!(
        unsafe { *(gpu.outputs[2].as_ref().unwrap().mapped as *const u32) },
        old_first
    );
    assert!(gpu.outputs.get(6).and_then(Option::as_ref).is_none());

    // This is deliberately later than validation: allocation of output 2's
    // inactive replacement has already succeeded, then a deterministic
    // staging fault fires before descriptors are committed. A real Vulkan
    // device therefore exercises the same cleanup/rollback boundary as an
    // allocation failure without depending on memory-pressure timing.
    gpu.transfer_stage_fail_after = Some(1);
    let extra = vec![(1, 2); 16];
    assert!(gpu
        .replace_transfers(&[
            TransferUpdate {
                index: 2,
                width: size[0],
                height: size[1],
                table: &replacement,
            },
            TransferUpdate {
                index: 6,
                width: size[0],
                height: size[1],
                table: &extra,
            },
        ])
        .unwrap_err()
        .to_string()
        .contains("injected staging failure"));
    assert_eq!(gpu.outputs[2].as_ref().unwrap().buffer, old_output);
    assert_eq!(
        unsafe { *(gpu.outputs[2].as_ref().unwrap().mapped as *const u32) },
        old_first
    );
    assert!(gpu.outputs.get(6).and_then(Option::as_ref).is_none());

    gpu.replace_transfers(&[
        TransferUpdate {
            index: 2,
            width: size[0],
            height: size[1],
            table: &replacement,
        },
        TransferUpdate {
            index: 6,
            width: size[0],
            height: size[1],
            table: &untouched,
        },
    ])?;
    assert_ne!(gpu.outputs[2].as_ref().unwrap().buffer, old_output);
    assert_eq!(gpu.outputs[5].as_ref().unwrap().buffer, old_untouched);
    let high_output = gpu.outputs[6].as_ref().unwrap().buffer;
    let low_index_followup = vec![(0, 1); 16];
    gpu.replace_transfers(&[TransferUpdate {
        index: 2,
        width: size[0],
        height: size[1],
        table: &low_index_followup,
    }])?;
    assert_eq!(gpu.outputs[5].as_ref().unwrap().buffer, old_untouched);
    assert_eq!(gpu.outputs[6].as_ref().unwrap().buffer, high_output);

    let pixels: Vec<u8> = fixture(size, false)
        .into_iter()
        .flat_map(|pixel| pixel.into_iter())
        .collect();
    let constants = PushConstants::canvas([0, 0], size, size[1], false, None);
    let got = draw_installed(
        &mut gpu,
        TestCanvas::Pixels(&canvas, &pixels),
        &target,
        constants,
        format,
    )?;
    for pixel in got.chunks_exact(4) {
        assert_eq!(pixel, [1, 1, 1, 255]);
    }
    Ok(())
}

#[test]
#[ignore = "requires a Vulkan 1.3 GPU; run explicitly on brain"]
fn uploaded_static_canvas_is_sampled_by_the_production_shader() -> anyhow::Result<()> {
    let mut gpu = Gpu::new(None)?;
    let format = vk::Format::R8G8B8A8_UNORM;
    let size = [4, 4];
    let target = Image::new(&gpu, size, format)?;
    let pixels: Vec<u8> = fixture(size, true)
        .into_iter()
        .flat_map(|pixel| pixel.into_iter())
        .collect();
    let canvas = gpu.upload_static_canvas_rgba(size[0], size[1], &pixels)?;
    gpu.set_transfer(2, size[0], size[1], &[(256, 0); 16])?;
    let constants = PushConstants::canvas([0, 0], size, size[1], false, None);
    let got = draw_installed(
        &mut gpu,
        TestCanvas::Static(&canvas),
        &target,
        constants,
        format,
    )?;
    // Uploaded alpha is deliberately varied by the fixture. Projection
    // output is opaque, matching every retained content shader fixture.
    let mut expected = pixels;
    for pixel in expected.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    assert_eq!(got, expected);
    Ok(())
}

#[test]
#[ignore = "requires exportable DRM images; run explicitly on brain"]
fn dmabuf_sampling_contract_rejects_incompatible_jobs_before_recording() -> anyhow::Result<()> {
    let mut gpu = Gpu::new(None)?;
    let format = format_for_fourcc(FOURCC_AB24)?;
    let properties = gpu.modifier_properties(format);
    let candidates: Vec<_> = properties.iter().map(|p| p.drm_format_modifier).collect();
    let capture_modifiers = gpu.supported_capture_modifiers(FOURCC_AB24, &candidates, false);
    let warp_capture_modifiers = gpu.supported_capture_modifiers(FOURCC_AB24, &candidates, true);
    let present_modifiers = gpu.supported_modifiers(FOURCC_AB24, &candidates, Usage::Present);
    anyhow::ensure!(
        !capture_modifiers.is_empty() && !present_modifiers.is_empty(),
        "no exportable test modifiers"
    );
    let mut capture = gpu.create_image(8, 8, FOURCC_AB24, &capture_modifiers, Usage::Capture)?;
    let target = gpu.create_image(8, 8, FOURCC_AB24, &present_modifiers, Usage::Present)?;
    let expected = properties
        .iter()
        .find(|p| p.drm_format_modifier == capture.modifier)
        .unwrap()
        .drm_format_modifier_tiling_features
        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR);
    assert_eq!(capture.linear_filter_supported(), expected);
    assert!(warp_capture_modifiers.iter().all(|modifier| {
        properties.iter().any(|property| {
            property.drm_format_modifier == *modifier
                && property
                    .drm_format_modifier_tiling_features
                    .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR)
        })
    }));
    println!(
        "dmabuf capture modifier {:#x}, linear filtering {}; present modifier {:#x}",
        capture.modifier,
        capture.linear_filter_supported(),
        target.modifier
    );
    gpu.set_transfer(2, 8, 8, &[(256, 0); 64])?;
    assert!(gpu.set_transfer(2, 0, 8, &[]).is_err());
    assert!(gpu.set_transfer(2, 8, 8, &[(257, 0); 64]).is_err());
    assert!(gpu.set_transfer(2, 8, 8, &[(256, 0); 63]).is_err());
    assert_eq!(gpu.outputs[2].as_ref().unwrap().size, [8, 8]);
    let warp = Warp::identity(8, 8);
    capture.linear_filter = false; // simulate an exact-only capture modifier
    let job = BlendJob {
        target: &target,
        output: 2,
        source_x: 0,
        source_y: 0,
        warp: Some(&warp),
    };
    assert!(gpu
        .blend(&capture, false, &[job])
        .unwrap_err()
        .to_string()
        .contains("linear filtering"));
    let wrong_size = Warp::identity(7, 8);
    let job = BlendJob {
        target: &target,
        output: 2,
        source_x: 0,
        source_y: 0,
        warp: Some(&wrong_size),
    };
    assert!(gpu
        .blend(&capture, false, &[job])
        .unwrap_err()
        .to_string()
        .contains("dimensions"));
    gpu.set_transfer(2, 4, 16, &[(256, 0); 64])?; // same capacity, wrong shape
    let job = BlendJob {
        target: &target,
        output: 2,
        source_x: 0,
        source_y: 0,
        warp: None,
    };
    assert!(gpu
        .blend(&capture, false, &[job])
        .unwrap_err()
        .to_string()
        .contains("dimensions"));
    // Each failure above occurs before recording/submission: the capture is
    // deliberately never initialized by a compositor or sampled in this test.
    Ok(())
}
