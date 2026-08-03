//! The blend overlay: a tiny layer-shell client run as `suede blend`.
//!
//! One process per projector. It puts a black, input-transparent surface on
//! the overlay layer of its output and fills the alpha channel with the ramps
//! from its [`OverlaySpec`] — the content underneath keeps rendering and
//! receiving input untouched; only the seam regions are attenuated.
//!
//! The image is static: it is computed once per configure and never redrawn,
//! so at steady state this process costs nothing per frame. The daemon spawns
//! it, kills it when the seams change, and respawns it if it dies.

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
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

use super::blend::{alpha_map, OverlaySpec};

#[derive(Default)]
struct State {
    /// Outputs seen so far, with names once the compositor reports them.
    outputs: Vec<(WlOutput, Option<String>)>,
    /// Latest size from the layer-surface configure, in logical pixels.
    size: Option<(u32, u32)>,
    needs_paint: bool,
    closed: bool,
}

/// Run the overlay until the compositor closes it or the process is killed.
pub fn run(spec: &OverlaySpec) -> anyhow::Result<()> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let handle = queue.handle();
    let mut state = State::default();

    let compositor: WlCompositor = globals.bind(&handle, 4..=4, ())?;
    let shm: WlShm = globals.bind(&handle, 1..=1, ())?;
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&handle, 1..=4, ())?;

    // wl_output only reports its connector name from version 4.
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" && global.version >= 4 {
            let output: WlOutput = globals.registry().bind(global.name, 4, &handle, ());
            state.outputs.push((output, None));
        }
    }
    queue.roundtrip(&mut state)?;

    let target = state
        .outputs
        .iter()
        .find(|(_, name)| name.as_deref() == Some(spec.output.as_str()))
        .map(|(output, _)| output.clone())
        .ok_or_else(|| anyhow::anyhow!("no output named {} on this compositor", spec.output))?;

    let surface = compositor.create_surface(&handle, ());
    // An empty input region: clicks and touches fall through to the content.
    let region: WlRegion = compositor.create_region(&handle, ());
    surface.set_input_region(Some(&region));
    region.destroy();

    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        Some(&target),
        Layer::Overlay,
        "suede-blend".to_string(),
        &handle,
        (),
    );
    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    // Cover the whole output, ignoring panels and exclusive zones.
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_size(0, 0);
    surface.commit();

    let mut buffer: Option<WlBuffer> = None;
    loop {
        queue.blocking_dispatch(&mut state)?;
        if state.closed {
            return Ok(());
        }
        if state.needs_paint {
            state.needs_paint = false;
            let (width, height) = state.size.unwrap_or((0, 0));
            if width == 0 || height == 0 {
                continue;
            }
            if let Some(old) = buffer.take() {
                old.destroy();
            }
            buffer = Some(paint(&shm, &surface, spec, width, height, &handle)?);
        }
    }
}

/// Render the alpha map into a shared-memory buffer and attach it.
fn paint(
    shm: &WlShm,
    surface: &WlSurface,
    spec: &OverlaySpec,
    width: u32,
    height: u32,
    handle: &QueueHandle<State>,
) -> anyhow::Result<WlBuffer> {
    let alpha = alpha_map(width, height, spec);

    // ARGB8888, premultiplied: black at alpha a is simply (a, 0, 0, 0),
    // stored little-endian as B G R A.
    let mut pixels = vec![0u8; alpha.len() * 4];
    for (index, a) in alpha.iter().enumerate() {
        pixels[index * 4 + 3] = *a;
    }

    // The content is static, so plain file IO suffices: no mmap on our side.
    let mut file = tempfile::tempfile()?;
    file.write_all(&pixels)?;
    file.flush()?;

    let stride = width as i32 * 4;
    let pool: WlShmPool = shm.create_pool(file.as_fd(), stride * height as i32, handle, ());
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

    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, width as i32, height as i32);
    surface.commit();
    Ok(buffer)
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
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

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
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
                state.size = Some((width, height));
                state.needs_paint = true;
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: WlRegion);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);
