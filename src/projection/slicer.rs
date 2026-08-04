//! The slicer: Suede's own compositor for the overlap, run as `suede slice`.
//!
//! Sway cannot blend overlapping outputs — its global coordinate space gives
//! both projectors the same pixels wherever their boxes intersect. So the
//! overlap is managed here instead. The outputs sit edge to edge in sway
//! (nothing overlaps, nothing bleeds); the app renders once into a headless
//! canvas of `Σwidths − (n−1)·overlap`; and this process captures that
//! canvas each frame, cuts it into per-projector slices whose neighbours
//! *repeat* the seam columns, applies the gamma-shaped blend ramps and black
//! lift, and presents each slice fullscreen on its own physical output.
//! Each projector gets its own buffer, so the two sides of a seam can carry
//! opposite fades — the thing the compositor's shared space can never do.
//!
//! One process for the whole installation: capture happens once per frame no matter
//! how many projectors consume it. The frame loop is damage-driven — a
//! static page costs nothing per second.

use std::io::Write;
use std::os::fd::AsFd;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_output::{self, WlOutput},
    wl_region::WlRegion,
    wl_registry::WlRegistry,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use super::blend::{pixel_transfer, OverlaySpec, SlicerSpec};

/// Consecutive capture failures tolerated before giving up. The daemon
/// respawns the slicer on its next pass, which is the retry policy.
const MAX_FAILURES: u32 = 3;

/// How to read one pixel of the captured canvas: bytes per pixel and where
/// red, green, and blue live within them.
///
/// wl_shm names 0 and 1 for ARGB/XRGB; everything else is a DRM fourcc.
/// wlroots offers whatever the renderer holds — the headless output on this
/// hardware arrives as `BG24`, three bytes per pixel, which is exactly why
/// this cannot be assumed to be 32-bit.
#[derive(Clone, Copy)]
struct PixelFormat {
    bytes: usize,
    red: usize,
    green: usize,
    blue: usize,
}

fn pixel_format(raw: u32) -> Option<PixelFormat> {
    let f = |bytes, red, green, blue| {
        Some(PixelFormat {
            bytes,
            red,
            green,
            blue,
        })
    };
    match raw {
        // wl_shm: ARGB8888 / XRGB8888, little-endian bytes B,G,R,A.
        0 | 1 => f(4, 2, 1, 0),
        // DRM 'AB24'/'XB24': ABGR8888/XBGR8888, bytes R,G,B,A.
        0x34324241 | 0x34324258 => f(4, 0, 1, 2),
        // DRM 'BG24': BGR888, bytes R,G,B.
        0x34324742 => f(3, 0, 1, 2),
        // DRM 'RG24': RGB888, bytes B,G,R.
        0x34324752 => f(3, 2, 1, 0),
        _ => None,
    }
}

struct Presenter {
    surface: WlSurface,
    #[allow(dead_code)]
    layer_surface: ZwlrLayerSurfaceV1,
    configured: Option<(u32, u32)>,
    /// Two buffers, alternated so we never write one the compositor reads.
    buffers: Vec<(WlBuffer, memmap2::MmapMut)>,
    next_buffer: usize,
    /// Fixed-point per-pixel transfer `(a, b)`: `out = (a·in)>>8 + b`,
    /// row-major at the configured size. Two-dimensional because seams can
    /// run on any edge — a grid corner is the product of two ramps.
    transfer: Vec<(u16, u8)>,
    /// This presenter's region of the canvas.
    source: crate::model::Rect,
}

#[derive(Default)]
struct Capture {
    /// Offered shm layout: (format, width, height, stride).
    offered: Option<(u32, u32, u32, u32)>,
    format: Option<PixelFormat>,
    buffer: Option<(WlBuffer, memmap2::MmapMut, u32, u32, u32)>,
    buffer_done: bool,
    ready: bool,
    failed: bool,
    y_invert: bool,
    first_copy_done: bool,
}

struct State {
    outputs: Vec<(WlOutput, Option<String>)>,
    presenters: Vec<Presenter>,
    capture: Capture,
    closed: bool,
}

impl State {
    fn all_configured(&self) -> bool {
        self.presenters.iter().all(|p| p.configured.is_some())
    }
}

pub fn run(spec: &SlicerSpec) -> anyhow::Result<()> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let handle = queue.handle();

    let compositor: WlCompositor = globals.bind(&handle, 4..=4, ())?;
    let shm: WlShm = globals.bind(&handle, 1..=1, ())?;
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&handle, 1..=4, ())?;
    let screencopy: ZwlrScreencopyManagerV1 = globals.bind(&handle, 1..=3, ())?;

    let mut state = State {
        outputs: Vec::new(),
        presenters: Vec::new(),
        capture: Capture::default(),
        closed: false,
    };
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" && global.version >= 4 {
            let output: WlOutput = globals.registry().bind(global.name, 4, &handle, ());
            state.outputs.push((output, None));
        }
    }
    queue.roundtrip(&mut state)?;

    let find = |state: &State, name: &str| -> Option<WlOutput> {
        state
            .outputs
            .iter()
            .find(|(_, n)| n.as_deref() == Some(name))
            .map(|(o, _)| o.clone())
    };
    let source = find(&state, &spec.source)
        .ok_or_else(|| anyhow::anyhow!("no output named {} to capture", spec.source))?;

    // One presenter per slice, covering its physical output entirely.
    for (index, slice) in spec.slices.iter().enumerate() {
        let target = find(&state, &slice.output)
            .ok_or_else(|| anyhow::anyhow!("no output named {}", slice.output))?;
        let surface = compositor.create_surface(&handle, ());
        let region: WlRegion = compositor.create_region(&handle, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            Some(&target),
            Layer::Overlay,
            "suede-slice".to_string(),
            &handle,
            index,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_size(0, 0);
        surface.commit();

        state.presenters.push(Presenter {
            surface,
            layer_surface,
            configured: None,
            buffers: Vec::new(),
            next_buffer: 0,
            transfer: Vec::new(),
            source: slice.source,
        });
    }

    while !state.all_configured() {
        queue.blocking_dispatch(&mut state)?;
        if state.closed {
            return Ok(());
        }
    }
    for (presenter, slice) in state.presenters.iter_mut().zip(spec.slices.iter()) {
        let (width, height) = presenter.configured.unwrap();
        for _ in 0..2 {
            presenter
                .buffers
                .push(shm_buffer(&shm, &handle, width, height)?);
        }
        // Built at the *presented* size: ramps are defined against the
        // slice, and any mismatch shows up as identity pixels, not a panic.
        let mut transfer = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                transfer.push(pixel_transfer(
                    &slice.ramps,
                    spec.gamma,
                    spec.black_lift,
                    x,
                    y,
                ));
            }
        }
        presenter.transfer = transfer;
    }

    if let Some(pattern) = spec.pattern {
        present_pattern(&mut state, spec, pattern);
        // Static image: nothing further to do but stay alive.
        loop {
            queue.blocking_dispatch(&mut state)?;
            if state.closed {
                return Ok(());
            }
        }
    }

    // The capture loop. Each iteration asks the compositor for the canvas's
    // next damaged frame, so an idle canvas parks us in blocking_dispatch.
    let mut failures = 0u32;
    loop {
        state.capture.offered = None;
        state.capture.buffer_done = false;
        state.capture.ready = false;
        state.capture.failed = false;
        let frame = screencopy.capture_output(0, &source, &handle, ());

        let mut copied = false;
        while !state.capture.ready && !state.capture.failed {
            queue.blocking_dispatch(&mut state)?;
            if state.closed {
                frame.destroy();
                return Ok(());
            }
            if state.capture.buffer_done && !copied {
                if !state.capture.first_copy_done {
                    if let Some((format, width, height, stride)) = state.capture.offered {
                        eprintln!(
                            "slicer: capture offer {width}x{height} stride {stride} format {format:#x}"
                        );
                    }
                }
                ensure_capture_buffer(&mut state.capture, &shm, &handle)?;
                let buffer = &state.capture.buffer.as_ref().unwrap().0;
                if state.capture.first_copy_done {
                    frame.copy_with_damage(buffer);
                } else {
                    frame.copy(buffer);
                }
                copied = true;
            }
        }

        if state.capture.failed {
            frame.destroy();
            failures += 1;
            if failures >= MAX_FAILURES {
                anyhow::bail!("screencopy failed {failures} times; giving up");
            }
            continue;
        }
        failures = 0;
        state.capture.first_copy_done = true;
        present_frame(&mut state, spec);
        frame.destroy();
    }
}

fn shm_buffer(
    shm: &WlShm,
    handle: &QueueHandle<State>,
    width: u32,
    height: u32,
) -> anyhow::Result<(WlBuffer, memmap2::MmapMut)> {
    let stride = width as i32 * 4;
    let size = stride as usize * height as usize;
    let mut file = tempfile::tempfile()?;
    file.write_all(&vec![0u8; size])?;
    let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
    let pool: WlShmPool = shm.create_pool(file.as_fd(), size as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride,
        wl_shm::Format::Argb8888,
        handle,
        (),
    );
    pool.destroy();
    Ok((buffer, map))
}

fn ensure_capture_buffer(
    capture: &mut Capture,
    shm: &WlShm,
    handle: &QueueHandle<State>,
) -> anyhow::Result<()> {
    let (format, width, height, stride) = capture
        .offered
        .ok_or_else(|| anyhow::anyhow!("screencopy offered no shm buffer"))?;
    capture.format = Some(
        pixel_format(format)
            .ok_or_else(|| anyhow::anyhow!("unsupported capture format {format:#x}"))?,
    );
    if let Some((_, _, w, h, s)) = &capture.buffer {
        if (*w, *h, *s) == (width, height, stride) {
            return Ok(());
        }
    }
    let size = stride as usize * height as usize;
    let mut file = tempfile::tempfile()?;
    file.write_all(&vec![0u8; size])?;
    let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
    let pool: WlShmPool = shm.create_pool(file.as_fd(), size as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        // Echo whatever the compositor offered; 4-byte formats only.
        WEnum::<wl_shm::Format>::from(format)
            .into_result()
            .map_err(|_| anyhow::anyhow!("unknown shm format {format}"))?,
        handle,
        (),
    );
    pool.destroy();
    if let Some((old, ..)) = capture.buffer.take() {
        old.destroy();
    }
    capture.buffer = Some((buffer, map, width, height, stride));
    Ok(())
}

/// Cut the captured canvas into slices, apply each column's transfer, and
/// commit every presenter.
fn present_frame(state: &mut State, _spec: &SlicerSpec) {
    // Split-borrow: the canvas is read while presenter buffers are written.
    let State {
        capture,
        presenters,
        ..
    } = state;
    let Some((_, canvas, canvas_width, canvas_height, stride)) = capture
        .buffer
        .as_ref()
        .map(|(b, m, w, h, s)| (b, m, *w, *h, *s))
    else {
        return;
    };
    let y_invert = capture.y_invert;
    let format = capture.format.unwrap_or(PixelFormat {
        bytes: 4,
        red: 2,
        green: 1,
        blue: 0,
    });
    // The offer is not gospel: a scale or a mid-resize race can make width,
    // stride, and buffer length disagree. The buffer length is the only hard
    // truth, so derive the usable geometry from it.
    let usable_width = canvas_width.min(stride / format.bytes as u32);
    let usable_height = canvas_height.min((canvas.len() as u32) / stride.max(1));

    for presenter in presenters.iter_mut() {
        let Some((width, height)) = presenter.configured else {
            continue;
        };
        let source_x = presenter.source.x.max(0) as u32;
        let source_y = presenter.source.y.max(0) as u32;
        let rows = height.min(usable_height.saturating_sub(source_y));
        let index = presenter.next_buffer % presenter.buffers.len();
        presenter.next_buffer = presenter.next_buffer.wrapping_add(1);
        let copy_width = width.min(usable_width.saturating_sub(source_x)) as usize;
        if copy_width == 0 || rows == 0 {
            continue;
        }

        {
            let (_, map) = &mut presenter.buffers[index];
            for y in 0..rows {
                let canvas_row = source_y + y;
                let canvas_row = if y_invert {
                    usable_height - 1 - canvas_row
                } else {
                    canvas_row
                };
                let src_row = canvas_row as usize * stride as usize;
                let dst_row = y as usize * width as usize * 4;
                let transfer_row = y as usize * width as usize;
                for x in 0..copy_width {
                    let src = src_row + (source_x as usize + x) * format.bytes;
                    let (a, b) = presenter
                        .transfer
                        .get(transfer_row + x)
                        .copied()
                        .unwrap_or((256, 0));
                    let shade = |v: u8| (((a as u32 * v as u32) >> 8) + b as u32).min(255) as u8;
                    let dst = &mut map[dst_row + x * 4..dst_row + x * 4 + 4];
                    // Presenters are always BGRA, opaque.
                    dst[0] = shade(canvas[src + format.blue]);
                    dst[1] = shade(canvas[src + format.green]);
                    dst[2] = shade(canvas[src + format.red]);
                    dst[3] = 255;
                }
            }
        }

        let (buffer, _) = &presenter.buffers[index];
        presenter.surface.attach(Some(buffer), 0, 0);
        presenter
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        presenter.surface.commit();
    }
}

/// Test patterns, drawn once in canvas coordinates so they continue exactly
/// across the seams — with the same ramps and lift content would get.
fn present_pattern(state: &mut State, spec: &SlicerSpec, pattern: crate::model::TestPattern) {
    for (slice, presenter) in spec.slices.iter().zip(state.presenters.iter_mut()) {
        let Some((width, height)) = presenter.configured else {
            continue;
        };
        let fake = OverlaySpec {
            output: slice.output.clone(),
            gamma: spec.gamma,
            black_lift: 0.0,
            rect: slice.source,
            pattern: Some(pattern),
            ramps: Vec::new(),
        };
        let rgb = super::pattern::render(width, height, &fake);
        let (_, map) = &mut presenter.buffers[0];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let (a, b) = presenter
                    .transfer
                    .get(y * width as usize + x)
                    .copied()
                    .unwrap_or((256, 0));
                let src = &rgb[(y * width as usize + x) * 3..(y * width as usize + x) * 3 + 3];
                let out = |v: u8| (((a as u32 * v as u32) >> 8) + b as u32).min(255) as u8;
                let dst = &mut map[(y * width as usize + x) * 4..(y * width as usize + x) * 4 + 4];
                dst[0] = out(src[2]);
                dst[1] = out(src[1]);
                dst[2] = out(src[0]);
                dst[3] = 255;
            }
        }
        let (buffer, _) = &presenter.buffers[0];
        presenter.surface.attach(Some(buffer), 0, 0);
        presenter
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);
        presenter.surface.commit();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            for (candidate, stored) in &mut state.outputs {
                if candidate == output {
                    *stored = Some(name);
                    break;
                }
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                if let Some(presenter) = state.presenters.get_mut(*index) {
                    presenter.configured = Some((width, height));
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                // Only 4-byte formats are handled; anything else is refused
                // at copy time by never storing an offer.
                let raw = match format {
                    WEnum::Value(value) => value as u32,
                    WEnum::Unknown(value) => value,
                };
                state.capture.offered = Some((raw, width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.capture.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.capture.y_invert = flags
                    .into_result()
                    .map(|f| f.contains(zwlr_screencopy_frame_v1::Flags::YInvert))
                    .unwrap_or(false);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.capture.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.capture.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: WlRegion);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);
