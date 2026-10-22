// The pill as an X11 window: override-redirect, so no window manager frames,
// moves or focuses it; a 32-bit visual for the translucent capsule; an empty
// input shape, so clicks go to the window under it. It sits at the bottom
// centre of the work area of the monitor the pointer is on, above a dock that
// shows there, and moves there again at every new dictation, as on macOS.
//
// The translucency needs a compositing manager (xfwm4, KWin and Muffin have
// one); without one the transparent corners show black.

use std::fmt::Display;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ClipOrdering, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, ImageFormat, MapState, PropMode, StackMode, VisualClass, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::Message;
use crate::log;
use crate::x11_scale;
use crate::pill::PillState;
use crate::pill::render::{self, BOTTOM_INSET, Renderer};

const FRAME: Duration = Duration::from_millis(16);
/// How often X events are looked at while nothing moves.
const IDLE_WAIT: Duration = Duration::from_millis(500);

fn text(error: impl Display) -> String {
    error.to_string()
}

pub fn run(messages: mpsc::Receiver<Message>, ready: mpsc::Sender<Result<(), String>>) {
    let window = match Window::create() {
        Ok(window) => window,
        Err(reason) => {
            let _ = ready.send(Err(reason));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    log::info!(Pill, "pill: on screen, {}", PillState::Idle.as_str());

    let mut renderer = Renderer::new();
    window.draw(&mut renderer);
    let mut moving = false;
    let mut last_frame = Instant::now();
    loop {
        let wait = if moving { FRAME.saturating_sub(last_frame.elapsed()) } else { IDLE_WAIT };
        match messages.recv_timeout(wait) {
            Ok(message) => {
                apply(message, &mut renderer, &window, &mut moving);
                while let Ok(message) = messages.try_recv() {
                    apply(message, &mut renderer, &window, &mut moving);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        let mut exposed = false;
        loop {
            match window.connection.poll_for_event() {
                Ok(Some(Event::Expose(_))) => exposed = true,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    log::error!(Pill, "pill: the X11 connection failed: {error}");
                    return;
                }
            }
        }

        if (moving && last_frame.elapsed() >= FRAME) || exposed {
            moving = renderer.tick();
            window.draw(&mut renderer);
            last_frame = Instant::now();
        }
    }
}

fn apply(message: Message, renderer: &mut Renderer, window: &Window, moving: &mut bool) {
    match message {
        Message::State(state) => {
            // The pill follows the pointer between sessions, not during one.
            if state == PillState::Listening && renderer.state() == PillState::Idle {
                window.place();
            }
            renderer.set_state(state);
            *moving = true;
        }
        Message::Level(level) => renderer.set_level(level),
    }
}

struct Window {
    connection: RustConnection,
    root: u32,
    screen_size: (i32, i32),
    window: u32,
    gc: u32,
    scale: f32,
    width: u16,
    height: u16,
    workarea: u32,
    client_list: u32,
    window_type: u32,
    dock: u32,
}

impl Window {
    fn create() -> Result<Window, String> {
        let (connection, screen_number) = x11rb::connect(None).map_err(|error| format!("no X11 display: {error}"))?;
        let screen = connection.setup().roots[screen_number].clone();
        let visual = screen
            .allowed_depths
            .iter()
            .filter(|depth| depth.depth == 32)
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.class == VisualClass::TRUE_COLOR)
            .map(|visual| visual.visual_id)
            .ok_or("no 32-bit visual, so no translucent window")?;

        let desktop_scale = x11_scale::read(&connection, screen.root, screen_number);
        let scale = desktop_scale.value;
        let (points_wide, points_high) = render::canvas_size();
        let width = (points_wide as f32 * scale).ceil() as u16;
        let height = (points_high as f32 * scale).ceil() as u16;

        let colormap = connection.generate_id().map_err(text)?;
        connection
            .create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)
            .map_err(text)?;
        let window = connection.generate_id().map_err(text)?;
        connection
            .create_window(
                32,
                window,
                screen.root,
                0,
                0,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                visual,
                &CreateWindowAux::new()
                    .background_pixel(0)
                    .border_pixel(0)
                    .colormap(colormap)
                    .override_redirect(1)
                    .event_mask(EventMask::EXPOSURE),
            )
            .map_err(text)?;

        let names = [
            "_NET_WM_WINDOW_TYPE",
            "_NET_WM_WINDOW_TYPE_NOTIFICATION",
            "_NET_WM_STATE",
            "_NET_WM_STATE_ABOVE",
            "_NET_WM_STATE_STICKY",
            "_NET_WM_STATE_SKIP_TASKBAR",
            "_NET_WM_STATE_SKIP_PAGER",
            "_NET_WORKAREA",
            "_NET_CLIENT_LIST",
            "_NET_WM_WINDOW_TYPE_DOCK",
        ];
        let mut atoms = Vec::with_capacity(names.len());
        for name in names {
            let atom = connection
                .intern_atom(false, name.as_bytes())
                .map_err(text)?
                .reply()
                .map_err(text)?
                .atom;
            atoms.push(atom);
        }
        connection
            .change_property32(PropMode::REPLACE, window, atoms[0], AtomEnum::ATOM, &[atoms[1]])
            .map_err(text)?;
        connection
            .change_property32(PropMode::REPLACE, window, atoms[2], AtomEnum::ATOM, &atoms[3..7])
            .map_err(text)?;
        connection
            .change_property8(PropMode::REPLACE, window, AtomEnum::WM_CLASS, AtomEnum::STRING, b"sensay-pill\0Sensay\0")
            .map_err(text)?;
        connection
            .change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, b"Sensay pill")
            .map_err(text)?;

        // An empty input shape: clicks go to the window under the pill.
        if connection
            .extension_information(shape::X11_EXTENSION_NAME)
            .map_err(text)?
            .is_some()
        {
            connection
                .shape_rectangles(shape::SO::SET, shape::SK::INPUT, ClipOrdering::UNSORTED, window, 0, 0, &[])
                .map_err(text)?;
        } else {
            log::warning!(Pill, "pill: the X server has no SHAPE extension, the pill takes clicks");
        }

        let gc = connection.generate_id().map_err(text)?;
        connection.create_gc(gc, window, &CreateGCAux::new()).map_err(text)?;

        warn_without_compositor(&connection, screen_number);
        let me = Window {
            screen_size: (i32::from(screen.width_in_pixels), i32::from(screen.height_in_pixels)),
            connection,
            root: screen.root,
            window,
            gc,
            scale,
            width,
            height,
            workarea: atoms[7],
            client_list: atoms[8],
            window_type: atoms[0],
            dock: atoms[9],
        };
        me.place();
        me.connection.map_window(window).map_err(text)?;
        me.connection.flush().map_err(text)?;
        log::info!(Pill, "pill: X11 window {width}x{height}, {}", desktop_scale.describe());
        Ok(me)
    }

    /// Bottom centre of the work area of the monitor under the pointer, above
    /// a dock showing there, and on top of everything again.
    fn place(&self) {
        let pointer = self
            .connection
            .query_pointer(self.root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map_or((0, 0), |reply| (i32::from(reply.root_x), i32::from(reply.root_y)));
        let monitors: Vec<(i32, i32, i32, i32)> = self
            .connection
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| {
                reply
                    .monitors
                    .iter()
                    .map(|monitor| (i32::from(monitor.x), i32::from(monitor.y), i32::from(monitor.width), i32::from(monitor.height)))
                    .collect()
            })
            .unwrap_or_default();
        let monitor = monitors
            .iter()
            .copied()
            .find(|&(x, y, width, height)| pointer.0 >= x && pointer.0 < x + width && pointer.1 >= y && pointer.1 < y + height)
            .or_else(|| monitors.first().copied())
            .unwrap_or((0, 0, self.screen_size.0, self.screen_size.1));

        // The panels' room is left out of the work area.
        let mut bottom = monitor.1 + monitor.3;
        if let Some((_, area_y, _, area_height)) = self.workarea() {
            bottom = bottom.min(area_y + area_height);
        }
        let x = monitor.0 + (monitor.2 - i32::from(self.width)) / 2;
        let inset = (BOTTOM_INSET * self.scale).round() as i32;
        let height = i32::from(self.height);
        // Where the pill would stand, with the gap under it.
        let room = (x, bottom - inset - height, i32::from(self.width), inset + height);
        if let Some(top) = self.dock_top(monitor, room) {
            log::info!(Pill, "pill: above a dock whose top is at y {top}");
            bottom = bottom.min(top);
        }
        let y = bottom - inset - height;
        let _ = self
            .connection
            .configure_window(self.window, &ConfigureWindowAux::new().x(x).y(y).stack_mode(StackMode::ABOVE));
        let _ = self.connection.flush();
    }

    /// The highest top edge of the docks showing in `room`. A dock that hides
    /// itself reserves no room in the work area (Xfce's bottom dock by
    /// default), so the pill would cover it. Docks whose top is in the upper
    /// half of the monitor, such as a panel along a side, do not count.
    fn dock_top(&self, monitor: (i32, i32, i32, i32), room: (i32, i32, i32, i32)) -> Option<i32> {
        let clients: Vec<u32> = self
            .connection
            .get_property(false, self.root, self.client_list, AtomEnum::WINDOW, 0, 4096)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .collect();
        // All asked at once and read after, so many windows cost one round trip.
        let types: Vec<_> = clients
            .iter()
            .map(|&client| {
                self.connection
                    .get_property(false, client, self.window_type, AtomEnum::ATOM, 0, 16)
                    .ok()
            })
            .collect();
        let docks: Vec<u32> = clients
            .iter()
            .zip(types)
            .filter_map(|(&client, cookie)| {
                let reply = cookie?.reply().ok()?;
                reply.value32()?.any(|kind| kind == self.dock).then_some(client)
            })
            .collect();

        let middle = monitor.1 + monitor.3 / 2;
        let (room_x, room_y, room_width, room_height) = room;
        docks
            .into_iter()
            .filter_map(|dock| {
                let attributes = self.connection.get_window_attributes(dock).ok()?.reply().ok()?;
                if attributes.map_state != MapState::VIEWABLE {
                    return None;
                }
                let geometry = self.connection.get_geometry(dock).ok()?.reply().ok()?;
                let origin = self
                    .connection
                    .translate_coordinates(dock, self.root, 0, 0)
                    .ok()?
                    .reply()
                    .ok()?;
                let (x, y) = (i32::from(origin.dst_x), i32::from(origin.dst_y));
                let (width, height) = (i32::from(geometry.width), i32::from(geometry.height));
                let overlaps =
                    x < room_x + room_width && room_x < x + width && y < room_y + room_height && room_y < y + height;
                (overlaps && y > middle).then_some(y)
            })
            .min()
    }

    /// The first desktop's work area from _NET_WORKAREA.
    fn workarea(&self) -> Option<(i32, i32, i32, i32)> {
        let reply = self
            .connection
            .get_property(false, self.root, self.workarea, AtomEnum::CARDINAL, 0, 4)
            .ok()?
            .reply()
            .ok()?;
        let values: Vec<u32> = reply.value32()?.collect();
        match values.as_slice() {
            [x, y, width, height, ..] => Some((*x as i32, *y as i32, *width as i32, *height as i32)),
            _ => None,
        }
    }

    fn draw(&self, renderer: &mut Renderer) {
        let Some(mut pixmap) = Pixmap::new(u32::from(self.width), u32::from(self.height)) else { return };
        renderer.draw(&mut pixmap, self.scale);
        // A 32-bit visual takes premultiplied ARGB, BGRA in memory on a
        // little-endian server; tiny-skia keeps premultiplied RGBA.
        let mut data = Vec::with_capacity(pixmap.pixels().len() * 4);
        for pixel in pixmap.pixels() {
            data.extend_from_slice(&[pixel.blue(), pixel.green(), pixel.red(), pixel.alpha()]);
        }
        let _ = self.connection.put_image(
            ImageFormat::Z_PIXMAP,
            self.window,
            self.gc,
            self.width,
            self.height,
            0,
            0,
            0,
            32,
            &data,
        );
        let _ = self.connection.flush();
    }
}

fn warn_without_compositor(connection: &RustConnection, screen_number: usize) {
    let name = format!("_NET_WM_CM_S{screen_number}");
    let owner = connection
        .intern_atom(false, name.as_bytes())
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| connection.get_selection_owner(reply.atom).ok())
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.owner);
    if owner == Some(0) {
        log::warning!(Pill, "pill: no compositing manager, the pill's corners will show black");
    }
}
