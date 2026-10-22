// The pill as a layer-shell surface (wlr-layer-shell): on the overlay layer,
// anchored to the bottom edge 24 points up, no keyboard focus ever, and an
// empty input region, so clicks go to the window under it. The compositor
// picks the output, usually the one in use.
//
// A frame is drawn only while something moves; at rest the pill is one
// buffer the compositor keeps showing.

use std::sync::mpsc;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::channel::{Channel, Event};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm, registry_handlers,
};
use tiny_skia::Pixmap;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_region, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};

use super::Message;
use crate::log;
use crate::pill::PillState;
use crate::pill::render::{self, BOTTOM_INSET, Renderer};

pub fn available() -> bool {
    let Ok(connection) = Connection::connect_to_env() else { return false };
    let Ok((globals, _queue)) = registry_queue_init::<Probe>(&connection) else { return false };
    globals
        .contents()
        .with_list(|list| list.iter().any(|global| global.interface == "zwlr_layer_shell_v1"))
}

/// State for the one look at the globals in `available`.
struct Probe;

impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_registry::WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub fn run(messages: Channel<Message>, ready: mpsc::Sender<Result<(), String>>) {
    let mut event_loop = match setup(messages) {
        Ok(parts) => parts,
        Err(reason) => {
            let _ = ready.send(Err(reason));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    let (loop_, mut state) = (&mut event_loop.0, &mut event_loop.1);
    while !state.exit {
        if let Err(error) = loop_.dispatch(None, &mut state) {
            log::error!(Pill, "pill: the Wayland connection failed: {error}");
            return;
        }
    }
}

fn setup(messages: Channel<Message>) -> Result<(EventLoop<'static, Pill>, Pill), String> {
    let connection = Connection::connect_to_env().map_err(|error| format!("no Wayland display: {error}"))?;
    let (globals, event_queue) = registry_queue_init(&connection).map_err(|error| error.to_string())?;
    let queue = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &queue).map_err(|error| format!("no wl_compositor: {error}"))?;
    let layer_shell = LayerShell::bind(&globals, &queue).map_err(|error| format!("no layer-shell: {error}"))?;
    let shm = Shm::bind(&globals, &queue).map_err(|error| format!("no wl_shm: {error}"))?;

    let (width, height) = render::canvas_size();
    let surface = compositor.create_surface(&queue);
    let layer = layer_shell.create_layer_surface(&queue, surface, Layer::Overlay, Some("sensay-pill"), None);
    layer.set_anchor(Anchor::BOTTOM);
    layer.set_margin(0, 0, BOTTOM_INSET as i32, 0);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_size(width, height);
    // An empty input region: clicks pass through to the window under it.
    let region = compositor.wl_compositor().create_region(&queue, ());
    layer.set_input_region(Some(&region));
    region.destroy();
    layer.commit();

    let pool = SlotPool::new((width * height * 4 * 4) as usize, &shm).map_err(|error| error.to_string())?;
    let event_loop: EventLoop<'static, Pill> = EventLoop::try_new().map_err(|error| error.to_string())?;
    WaylandSource::new(connection, event_queue)
        .insert(event_loop.handle())
        .map_err(|error| error.to_string())?;
    let message_queue = queue.clone();
    event_loop
        .handle()
        .insert_source(messages, move |event, _, pill: &mut Pill| match event {
            Event::Msg(message) => pill.receive(message, &message_queue),
            Event::Closed => pill.exit = true,
        })
        .map_err(|error| error.to_string())?;

    log::info!(Pill, "pill: layer-shell surface {width}x{height}");
    let pill = Pill {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &queue),
        shm,
        pool,
        layer,
        renderer: Renderer::new(),
        scale: 1,
        configured: false,
        frame_pending: false,
        exit: false,
    };
    Ok((event_loop, pill))
}

struct Pill {
    registry: RegistryState,
    outputs: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    renderer: Renderer,
    scale: i32,
    configured: bool,
    frame_pending: bool,
    exit: bool,
}

impl Pill {
    fn receive(&mut self, message: Message, queue: &QueueHandle<Self>) {
        match message {
            Message::State(state) => self.renderer.set_state(state),
            Message::Level(level) => {
                self.renderer.set_level(level);
                // The level alone redraws nothing: frames already run while
                // listening.
                return;
            }
        }
        if self.configured && !self.frame_pending {
            self.draw(queue);
        }
    }

    fn draw(&mut self, queue: &QueueHandle<Self>) {
        let moving = self.renderer.tick();
        let (width, height) = render::canvas_size();
        let scale = self.scale.max(1);
        let (pixel_width, pixel_height) = (width * scale as u32, height * scale as u32);
        let Some(mut pixmap) = Pixmap::new(pixel_width, pixel_height) else { return };
        self.renderer.draw(&mut pixmap, scale as f32);

        let stride = pixel_width as i32 * 4;
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(pixel_width as i32, pixel_height as i32, stride, wl_shm::Format::Argb8888)
        else {
            log::warning!(Pill, "pill: no buffer to draw into");
            return;
        };
        // tiny-skia keeps premultiplied RGBA; Argb8888 is BGRA in memory on
        // little-endian machines, also premultiplied.
        for (pixel, out) in pixmap.pixels().iter().zip(canvas.chunks_exact_mut(4)) {
            out[0] = pixel.blue();
            out[1] = pixel.green();
            out[2] = pixel.red();
            out[3] = pixel.alpha();
        }

        let surface = self.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, pixel_width as i32, pixel_height as i32);
        if moving {
            surface.frame(queue, surface.clone());
            self.frame_pending = true;
        }
        if let Err(error) = buffer.attach_to(surface) {
            log::warning!(Pill, "pill: could not attach the buffer: {error}");
            return;
        }
        self.layer.commit();
    }
}

impl CompositorHandler for Pill {
    fn scale_factor_changed(&mut self, _: &Connection, queue: &QueueHandle<Self>, _: &wl_surface::WlSurface, factor: i32) {
        if factor != self.scale {
            self.scale = factor;
            if self.configured && !self.frame_pending {
                self.draw(queue);
            }
        }
    }

    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}

    fn frame(&mut self, _: &Connection, queue: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        self.draw(queue);
    }

    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}

    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Pill {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Pill {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        log::warning!(Pill, "pill: the compositor closed the layer surface");
        self.exit = true;
    }

    fn configure(&mut self, _: &Connection, queue: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        if !self.configured {
            self.configured = true;
            log::info!(Pill, "pill: on screen, {}", PillState::Idle.as_str());
        }
        if !self.frame_pending {
            self.draw(queue);
        }
    }
}

impl ShmHandler for Pill {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Dispatch<wl_region::WlRegion, ()> for Pill {
    fn event(_: &mut Self, _: &wl_region::WlRegion, _: wl_region::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_compositor!(Pill);
delegate_output!(Pill);
delegate_shm!(Pill);
delegate_layer!(Pill);
delegate_registry!(Pill);

impl ProvidesRegistryState for Pill {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
