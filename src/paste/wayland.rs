// Paste on the Wayland compositors that let a program do it: Sway, Hyprland,
// river, labwc, niri, COSMIC. linux-spec.md, section 8.
//
// First choice: the whole text through the input method (input-method-v2).
// Sensay becomes the input method for the moment of the paste; if a text
// field is active, the compositor hands it the text: nothing on the
// clipboard, no keys, any layout. The role is given back at once, so IBus or
// Fcitx started later still get it; when one of them holds it already, the
// compositor says "unavailable" and the clipboard way is taken. A terminal
// takes a line break from the input method as Enter, so text with one goes
// the clipboard way there.
//
// Clipboard way, as ClipboardTextSink.swift: the text goes on the clipboard
// through data-control (ext-data-control-v1, else zwlr-data-control-v1),
// Ctrl+V (Ctrl+Shift+V in a terminal) is pressed on a virtual keyboard that
// carries its own us keymap, and 500 ms later the old clipboard comes back if
// it is still ours. The old content is kept in one format, as on GNOME and
// X11. The window in focus comes from wlr-foreign-toplevel-management.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use evdev::KeyCode;
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::{self, ExtDataControlDeviceV1};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{self, ExtDataControlOfferV1};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{self, ExtDataControlSourceV1};
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::{self, ZwpInputMethodV2};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self, ZwlrForeignToplevelHandleV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self, ZwlrForeignToplevelManagerV1,
};

use super::Report;
use super::formats::{TEXT_FORMATS, is_utf8_text, pick_format};
use super::terminals::is_terminal;
use super::wake::{Wake, wait_readable};

// PasteTiming.swift, and the waits for held keys as in paste.js.
const BEFORE_PASTE: Duration = Duration::from_millis(12);
const BETWEEN_KEYS: Duration = Duration::from_millis(8);
const BEFORE_RESTORE: Duration = Duration::from_millis(500);
const MODIFIER_POLL: Duration = Duration::from_millis(15);
const MODIFIER_WAIT: Duration = Duration::from_millis(3000);
const AFTER_MODIFIERS: Duration = Duration::from_millis(25);
/// How long the old clipboard may take to read before the paste goes on
/// without it.
const READ_LIMIT: Duration = Duration::from_secs(2);
/// commit_string takes at most this many bytes (input-method-unstable-v2).
const COMMIT_LIMIT: usize = 4000;

const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_V: u32 = 47;
/// Modifier bits of the keymap below: Shift is the first modifier, Control
/// the third.
const MOD_SHIFT: u32 = 1;
const MOD_CONTROL: u32 = 4;

/// The virtual keyboard's keymap: the plain us layout, so the evdev codes
/// above mean Ctrl, Shift and V whatever the user's own layout is.
const KEYMAP: &str = "xkb_keymap {
    xkb_keycodes { include \"evdev+aliases(qwerty)\" };
    xkb_types { include \"complete\" };
    xkb_compat { include \"complete\" };
    xkb_symbols { include \"pc+us+inet(evdev)\" };
};
";

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn ms(length: Duration) -> u64 {
    u64::try_from(length.as_millis()).unwrap_or(u64::MAX)
}

enum Command {
    Paste {
        text: String,
        reply: tokio::sync::oneshot::Sender<Result<Report, String>>,
    },
}

/// The app's side of the paste thread.
#[derive(Clone)]
pub struct WaylandPaste {
    commands: mpsc::Sender<Command>,
    wake: Arc<Wake>,
}

impl WaylandPaste {
    pub fn start() -> Result<WaylandPaste, String> {
        let wake = Arc::new(Wake::new().map_err(|error| format!("no eventfd: {error}"))?);
        let (commands_tx, commands_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_wake = wake.clone();
        std::thread::Builder::new()
            .name("paste-wayland".to_string())
            .spawn(move || match Paster::connect() {
                Ok(mut paster) => {
                    let _ = ready_tx.send(Ok(()));
                    paster.run(&commands_rx, &thread_wake);
                }
                Err(reason) => {
                    let _ = ready_tx.send(Err(reason));
                }
            })
            .map_err(|error| format!("could not start the paste thread: {error}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(WaylandPaste {
                commands: commands_tx,
                wake,
            }),
            Ok(Err(reason)) => Err(reason),
            Err(_) => Err("the paste thread did not start".to_string()),
        }
    }

    pub async fn paste(&self, text: &str) -> Result<Report, String> {
        let gone = || "the Wayland paste thread is gone".to_string();
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Paste {
                text: text.to_string(),
                reply,
            })
            .map_err(|_| gone())?;
        self.wake.ring();
        answer.await.map_err(|_| gone())?
    }
}

// ext-data-control-v1 and zwlr-data-control-v1 are the same protocol under
// two names; these wrappers let the rest of the file use either.

enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone)]
enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl Manager {
    fn name(&self) -> &'static str {
        match self {
            Manager::Ext(_) => "ext-data-control",
            Manager::Wlr(_) => "wlr-data-control",
        }
    }

    fn device(&self, seat: &WlSeat, qh: &QueueHandle<State>) -> Device {
        match self {
            Manager::Ext(manager) => Device::Ext(manager.get_data_device(seat, qh, ())),
            Manager::Wlr(manager) => Device::Wlr(manager.get_data_device(seat, qh, ())),
        }
    }

    fn source(&self, qh: &QueueHandle<State>) -> Source {
        match self {
            Manager::Ext(manager) => Source::Ext(manager.create_data_source(qh, ())),
            Manager::Wlr(manager) => Source::Wlr(manager.create_data_source(qh, ())),
        }
    }
}

impl Device {
    fn set_selection(&self, source: Option<&Source>) {
        match (self, source) {
            (Device::Ext(device), Some(Source::Ext(source))) => device.set_selection(Some(source)),
            (Device::Wlr(device), Some(Source::Wlr(source))) => device.set_selection(Some(source)),
            (Device::Ext(device), None) => device.set_selection(None),
            (Device::Wlr(device), None) => device.set_selection(None),
            // A manager makes sources of its own kind only.
            _ => {}
        }
    }
}

impl Offer {
    fn id(&self) -> ObjectId {
        match self {
            Offer::Ext(offer) => offer.id(),
            Offer::Wlr(offer) => offer.id(),
        }
    }

    fn receive(&self, mime: &str, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            Offer::Ext(offer) => offer.receive(mime.to_string(), fd),
            Offer::Wlr(offer) => offer.receive(mime.to_string(), fd),
        }
    }

    fn destroy(&self) {
        match self {
            Offer::Ext(offer) => offer.destroy(),
            Offer::Wlr(offer) => offer.destroy(),
        }
    }
}

impl Source {
    fn id(&self) -> ObjectId {
        match self {
            Source::Ext(source) => source.id(),
            Source::Wlr(source) => source.id(),
        }
    }

    fn offer(&self, mime: &str) {
        match self {
            Source::Ext(source) => source.offer(mime.to_string()),
            Source::Wlr(source) => source.offer(mime.to_string()),
        }
    }

    fn destroy(&self) {
        match self {
            Source::Ext(source) => source.destroy(),
            Source::Wlr(source) => source.destroy(),
        }
    }
}

/// What our clipboard source hands out.
enum Held {
    Nothing,
    Text(Arc<Vec<u8>>),
    Saved { mime: String, bytes: Arc<Vec<u8>> },
}

impl Held {
    /// The types offered, each with its bytes.
    fn offered(&self) -> Vec<(String, Arc<Vec<u8>>)> {
        let as_text = |bytes: &Arc<Vec<u8>>| {
            TEXT_FORMATS
                .iter()
                .map(|name| (name.to_string(), bytes.clone()))
                .collect::<Vec<_>>()
        };
        match self {
            Held::Nothing => Vec::new(),
            Held::Text(bytes) => as_text(bytes),
            Held::Saved { mime, bytes } if is_utf8_text(mime) => as_text(bytes),
            Held::Saved { mime, bytes } => vec![(mime.clone(), bytes.clone())],
        }
    }
}

/// The clipboard as it was before a paste.
enum Before {
    Empty,
    Unreadable(String),
    Saved { mime: String, bytes: Arc<Vec<u8>> },
}

#[derive(Default)]
struct Toplevel {
    app_id: String,
    activated: bool,
    pending_app_id: Option<String>,
    pending_activated: Option<bool>,
}

/// Everything the Wayland events change.
#[derive(Default)]
struct State {
    /// Types offered by each live clipboard offer.
    offers: HashMap<ObjectId, Vec<String>>,
    selection: Option<Offer>,
    source: Option<Source>,
    held: HeldCell,
    /// Somebody else took the clipboard while a paste waited.
    lost: bool,
    keys_at: Option<Instant>,
    reads: Vec<(String, Duration)>,
    im_pending_active: bool,
    im_active: bool,
    im_done: u32,
    im_unavailable: bool,
    /// What the input method heard during one paste, with times, for the
    /// log when it does not take the text.
    im_events: Vec<String>,
    im_asked: Option<Instant>,
    synced: bool,
    toplevels: HashMap<ObjectId, Toplevel>,
}

/// Held, with a default for State.
struct HeldCell(Held);

impl Default for HeldCell {
    fn default() -> HeldCell {
        HeldCell(Held::Nothing)
    }
}

impl State {
    fn set_selection(&mut self, offer: Option<Offer>) {
        if let Some(old) = self.selection.take() {
            if offer.as_ref().map(Offer::id) != Some(old.id()) {
                self.offers.remove(&old.id());
                old.destroy();
            }
        }
        self.selection = offer;
    }

    /// Somebody reads the clipboard from us: the bytes go out on a thread of
    /// their own, so a slow reader never holds up the paste.
    fn send(&mut self, mime: String, fd: OwnedFd) {
        let bytes = self
            .held
            .0
            .offered()
            .into_iter()
            .find(|(name, _)| *name == mime)
            .map(|(_, bytes)| bytes);
        let Some(bytes) = bytes else { return };
        if let Some(keys_at) = self.keys_at {
            self.reads.push((mime, keys_at.elapsed()));
        }
        let _ = std::thread::Builder::new()
            .name("paste-send".to_string())
            .spawn(move || {
                let mut file = File::from(fd);
                let _ = file.write_all(&bytes);
            });
    }

    fn cancelled(&mut self, id: &ObjectId) {
        if self.source.as_ref().map(Source::id).as_ref() == Some(id) {
            if let Some(source) = self.source.take() {
                source.destroy();
            }
            self.held = HeldCell(Held::Nothing);
            self.lost = true;
        }
    }

    fn focused_app(&self) -> String {
        self.toplevels
            .values()
            .find(|toplevel| toplevel.activated)
            .map(|toplevel| toplevel.app_id.clone())
            .unwrap_or_default()
    }
}

struct Paster {
    connection: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    seat: WlSeat,
    manager: Option<Manager>,
    device: Option<Device>,
    input_methods: Option<ZwpInputMethodManagerV2>,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    _toplevels: Option<ZwlrForeignToplevelManagerV1>,
    /// For the time stamps of the virtual keys.
    born: Instant,
}

impl Paster {
    fn connect() -> Result<Paster, String> {
        let connection = Connection::connect_to_env().map_err(|error| format!("no Wayland display: {error}"))?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection).map_err(text)?;
        let qh = queue.handle();
        let seat: WlSeat = globals.bind(&qh, 1..=7, ()).map_err(|error| format!("no seat: {error}"))?;
        let manager = globals
            .bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ())
            .map(Manager::Ext)
            .ok()
            .or_else(|| {
                globals
                    .bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ())
                    .map(Manager::Wlr)
                    .ok()
            });
        let device = manager.as_ref().map(|manager| manager.device(&seat, &qh));
        let input_methods = globals.bind::<ZwpInputMethodManagerV2, _, _>(&qh, 1..=1, ()).ok();
        let keyboard = globals
            .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
            .ok()
            .map(|keyboards| keyboards.create_virtual_keyboard(&seat, &qh, ()));
        let toplevels = globals.bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ()).ok();

        if let Some(keyboard) = &keyboard {
            upload_keymap(keyboard, &connection)?;
        }
        let mut state = State::default();
        queue.roundtrip(&mut state).map_err(text)?;

        crate::log::info!(
            Paste,
            "paste: Wayland: input-method-v2 {}, {}, virtual-keyboard {}, foreign-toplevel {}",
            if input_methods.is_some() { "yes" } else { "no" },
            manager.as_ref().map_or("no data-control", Manager::name),
            if keyboard.is_some() { "yes" } else { "no" },
            if toplevels.is_some() { "yes" } else { "no" },
        );
        if manager.is_none() && input_methods.is_none() {
            return Err("the compositor offers neither input-method-v2 nor data-control".to_string());
        }
        Ok(Paster {
            connection,
            queue,
            qh,
            state,
            seat,
            manager,
            device,
            input_methods,
            keyboard,
            _toplevels: toplevels,
            born: Instant::now(),
        })
    }

    /// Serves the clipboard and takes pastes until the app quits.
    fn run(&mut self, commands: &mpsc::Receiver<Command>, wake: &Wake) {
        loop {
            loop {
                match commands.try_recv() {
                    Ok(Command::Paste { text, reply }) => {
                        let _ = reply.send(self.paste(text));
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if let Err(reason) = self.block(None, Some(wake)) {
                crate::log::error!(Paste, "paste: the Wayland connection failed: {reason}");
                return;
            }
        }
    }

    /// Handles the events in, then sleeps until more come, the time is up or
    /// the app sends a command.
    fn block(&mut self, timeout: Option<Duration>, wake: Option<&Wake>) -> Result<(), String> {
        self.queue.dispatch_pending(&mut self.state).map_err(text)?;
        self.connection.flush().map_err(text)?;
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        let mut fds = vec![guard.connection_fd().as_raw_fd()];
        if let Some(wake) = wake {
            fds.push(wake.fd);
        }
        let ready = wait_readable(&fds, timeout);
        if ready[0] {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.to_string()),
            }
        } else {
            drop(guard);
        }
        if let Some(wake) = wake {
            if ready[1] {
                wake.clear();
            }
        }
        self.queue.dispatch_pending(&mut self.state).map_err(text)?;
        Ok(())
    }

    /// Waits until `done` says so or `deadline` passes, serving the
    /// clipboard meanwhile.
    fn wait_until(&mut self, deadline: Instant, done: &dyn Fn(&State) -> bool) -> Result<bool, String> {
        loop {
            self.queue.dispatch_pending(&mut self.state).map_err(text)?;
            if done(&self.state) {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            self.block(Some(deadline - now), None)?;
        }
    }

    fn pause(&mut self, length: Duration) -> Result<(), String> {
        self.wait_until(Instant::now() + length, &|_| false).map(|_| ())
    }

    /// One round trip: everything the compositor sent in answer to our
    /// requests so far is in.
    fn sync(&mut self) -> Result<(), String> {
        self.state.synced = false;
        self.connection.display().sync(&self.qh, ());
        self.wait_until(Instant::now() + Duration::from_secs(1), &|state| state.synced)?;
        Ok(())
    }

    fn paste(&mut self, dictated: String) -> Result<Report, String> {
        let started = Instant::now();
        self.queue.dispatch_pending(&mut self.state).map_err(text)?;
        let app = self.state.focused_app();
        let target = if app.is_empty() { "(unknown window)".to_string() } else { app.clone() };
        let terminal = is_terminal(&app);

        let input_method = if terminal && dictated.contains('\n') {
            "skipped, a line break in a terminal would run the command".to_string()
        } else if dictated.len() > COMMIT_LIMIT {
            format!("skipped, longer than {COMMIT_LIMIT} bytes")
        } else {
            let (committed, how) = self.commit(&dictated)?;
            if committed {
                return Ok(Report {
                    path: Some("input-method".to_string()),
                    target: Some(target),
                    total_ms: Some(ms(started.elapsed())),
                    ..Report::default()
                });
            }
            how
        };

        let mut report = Report {
            path: Some("clipboard".to_string()),
            target: Some(target),
            input_method: Some(input_method),
            ..Report::default()
        };
        if self.manager.is_none() {
            return Err("no input method field and no data-control for the clipboard".to_string());
        }

        let before = self.snapshot();
        report.before = Some(match &before {
            Before::Empty => "(empty)".to_string(),
            Before::Unreadable(reason) => format!("could not be read: {reason}"),
            Before::Saved { mime, bytes } => format!("{mime}, {} bytes", bytes.len()),
        });

        // Ours from here until somebody else takes the clipboard.
        self.state.reads.clear();
        self.state.keys_at = None;
        self.take_clipboard(Held::Text(Arc::new(dictated.into_bytes())))?;

        self.pause(BEFORE_PASTE)?;
        let keyboards = keyboards_with_modifiers();
        let mut waited = Duration::ZERO;
        while modifiers_held(&keyboards) {
            if waited >= MODIFIER_WAIT {
                // Ctrl+V on top of held keys would be some other shortcut. The
                // text stays on the clipboard for a paste by hand.
                report.waited_ms = Some(ms(waited));
                report.keys = Some("none, modifiers still held".to_string());
                report.restore = Some("text left on the clipboard".to_string());
                report.total_ms = Some(ms(started.elapsed()));
                return Ok(report);
            }
            self.pause(MODIFIER_POLL)?;
            waited += MODIFIER_POLL;
        }
        if !waited.is_zero() {
            self.pause(AFTER_MODIFIERS)?;
        }
        report.waited_ms = Some(ms(waited));

        if self.keyboard.is_none() {
            report.keys = Some("none, the compositor has no virtual keyboard".to_string());
            report.restore = Some("text left on the clipboard".to_string());
            report.total_ms = Some(ms(started.elapsed()));
            return Ok(report);
        }
        report.keys = Some(if terminal { "Ctrl+Shift+V" } else { "Ctrl+V" }.to_string());
        self.state.keys_at = Some(Instant::now());
        self.press_paste(terminal)?;

        self.pause(BEFORE_RESTORE)?;
        report.reads = Some(self.describe_reads());
        self.state.keys_at = None;
        report.restore = Some(if self.state.lost {
            "clipboard changed meanwhile, left as it is".to_string()
        } else {
            match before {
                Before::Saved { mime, bytes } => {
                    let line = format!("restored {mime}, {} bytes", bytes.len());
                    self.take_clipboard(Held::Saved { mime, bytes })?;
                    line
                }
                Before::Empty => {
                    if let Some(device) = &self.device {
                        device.set_selection(None);
                    }
                    if let Some(source) = self.state.source.take() {
                        source.destroy();
                    }
                    self.state.held = HeldCell(Held::Nothing);
                    self.connection.flush().map_err(text)?;
                    "cleared, it was empty".to_string()
                }
                Before::Unreadable(_) => "text left on the clipboard, the old content could not be read".to_string(),
            }
        });
        report.total_ms = Some(ms(started.elapsed()));
        Ok(report)
    }

    /// Hands the whole text to the focused field through the input method;
    /// false when no field takes it or another input method has the role,
    /// with what the compositor said, for the log.
    ///
    /// Two tries. On Sway 1.11 the first input method after a fresh start
    /// of Sensay sometimes hears only a "done" and is never turned on, while
    /// the next one is turned on in about 10 ms (24 October 2026); giving
    /// the role back and taking it again makes the field turn it on.
    fn commit(&mut self, dictated: &str) -> Result<(bool, String), String> {
        if self.input_methods.is_none() {
            return Ok((false, "the compositor has no input-method-v2".to_string()));
        }
        let asked = Instant::now();
        self.state.im_events.clear();
        self.state.im_asked = Some(asked);
        for attempt in 1..=2 {
            if attempt > 1 {
                self.state.im_events.push("again".to_string());
            }
            match self.try_input_method(dictated)? {
                Some(true) => return Ok((true, String::new())),
                Some(false) => return Ok((false, "another input method holds the role".to_string())),
                None => {}
            }
        }
        Ok((
            false,
            format!(
                "no active field after {} ms (heard: {})",
                asked.elapsed().as_millis(),
                self.state.im_events.join(", ")
            ),
        ))
    }

    /// One try: Some(true) when the text went in, Some(false) when another
    /// input method holds the role, None when no field turned it on.
    fn try_input_method(&mut self, dictated: &str) -> Result<Option<bool>, String> {
        let Some(input_methods) = &self.input_methods else {
            return Ok(None);
        };
        self.state.im_pending_active = false;
        self.state.im_active = false;
        self.state.im_done = 0;
        self.state.im_unavailable = false;
        let input_method = input_methods.get_input_method(&self.seat, &self.qh, ());
        self.sync()?;
        if !self.state.im_active && !self.state.im_unavailable {
            // The field learns about the new input method and turns it on
            // only after a trip through its own program.
            self.wait_until(Instant::now() + Duration::from_millis(150), &|state| {
                state.im_active || state.im_unavailable
            })?;
        }
        let outcome = if self.state.im_unavailable {
            Some(false)
        } else if self.state.im_active {
            input_method.commit_string(dictated.to_string());
            input_method.commit(self.state.im_done);
            Some(true)
        } else {
            None
        };
        input_method.destroy();
        self.connection.flush().map_err(text)?;
        if outcome.is_none() {
            // Let the compositor and the field see the role given back.
            self.sync()?;
        }
        Ok(outcome)
    }

    /// The clipboard before the paste, in the one format it is kept in.
    fn snapshot(&mut self) -> Before {
        if self.state.source.is_some() {
            return match &self.state.held.0 {
                Held::Nothing => Before::Empty,
                Held::Text(bytes) => Before::Saved {
                    mime: TEXT_FORMATS[0].to_string(),
                    bytes: bytes.clone(),
                },
                Held::Saved { mime, bytes } => Before::Saved {
                    mime: mime.clone(),
                    bytes: bytes.clone(),
                },
            };
        }
        let Some(offer) = self.state.selection.clone() else {
            return Before::Empty;
        };
        let mimes = self.state.offers.get(&offer.id()).cloned().unwrap_or_default();
        let Some(index) = pick_format(&mimes) else {
            return Before::Unreadable("no format that can be kept".to_string());
        };
        match self.receive(&offer, &mimes[index]) {
            Ok(bytes) => Before::Saved {
                mime: mimes[index].clone(),
                bytes: Arc::new(bytes),
            },
            Err(reason) => Before::Unreadable(format!("{}: {reason}", mimes[index])),
        }
    }

    /// Reads one type of the current clipboard through a pipe.
    fn receive(&mut self, offer: &Offer, mime: &str) -> Result<Vec<u8>, String> {
        let (reader, writer) = pipe().map_err(text)?;
        offer.receive(mime, writer.as_fd());
        self.connection.flush().map_err(text)?;
        drop(writer);
        let deadline = Instant::now() + READ_LIMIT;
        let mut reader = File::from(reader);
        let mut data = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err("the owner did not finish sending".to_string());
            }
            if !wait_readable(&[reader.as_raw_fd()], Some(deadline - now))[0] {
                continue;
            }
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(data),
                Ok(count) => data.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    fn take_clipboard(&mut self, held: Held) -> Result<(), String> {
        let (Some(manager), Some(device)) = (&self.manager, &self.device) else {
            return Err("no data-control".to_string());
        };
        let source = manager.source(&self.qh);
        for (mime, _) in held.offered() {
            source.offer(&mime);
        }
        device.set_selection(Some(&source));
        if let Some(old) = self.state.source.replace(source) {
            old.destroy();
        }
        self.state.held = HeldCell(held);
        self.state.lost = false;
        self.connection.flush().map_err(text)?;
        Ok(())
    }

    fn press_paste(&mut self, terminal: bool) -> Result<(), String> {
        let Some(keyboard) = self.keyboard.clone() else {
            return Err("no virtual keyboard".to_string());
        };
        let held = if terminal { MOD_CONTROL | MOD_SHIFT } else { MOD_CONTROL };
        let mut steps = vec![(KEY_LEFTCTRL, 1, MOD_CONTROL)];
        if terminal {
            steps.push((KEY_LEFTSHIFT, 1, held));
        }
        steps.push((KEY_V, 1, held));
        steps.push((KEY_V, 0, held));
        if terminal {
            steps.push((KEY_LEFTSHIFT, 0, MOD_CONTROL));
        }
        steps.push((KEY_LEFTCTRL, 0, 0));
        for (index, (key, pressed, modifiers)) in steps.into_iter().enumerate() {
            if index > 0 {
                self.pause(BETWEEN_KEYS)?;
            }
            let time = u32::try_from(self.born.elapsed().as_millis()).unwrap_or(u32::MAX);
            keyboard.key(time, key, pressed);
            keyboard.modifiers(modifiers, 0, 0, 0);
            self.connection.flush().map_err(text)?;
        }
        Ok(())
    }

    /// For the log: who read the clipboard after the keys.
    fn describe_reads(&self) -> String {
        let Some((_, first)) = self.state.reads.first() else {
            return "nobody read the clipboard after the keys".to_string();
        };
        let mut names: Vec<&str> = Vec::new();
        for (mime, _) in &self.state.reads {
            if !names.contains(&mime.as_str()) {
                names.push(mime);
            }
        }
        format!(
            "read {} time(s) after the keys, first after {} ms ({})",
            self.state.reads.len(),
            first.as_millis(),
            names.join(", ")
        )
    }
}

fn upload_keymap(keyboard: &ZwpVirtualKeyboardV1, connection: &Connection) -> Result<(), String> {
    let mut data = KEYMAP.as_bytes().to_vec();
    data.push(0);
    // SAFETY: a fresh memfd with a C string name; the descriptor goes into a
    // File that owns it from here.
    let fd = unsafe { libc::memfd_create(c"sensay-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("memfd_create: {}", io::Error::last_os_error()));
    }
    // SAFETY: the descriptor was just created and nothing else owns it.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(&data).map_err(text)?;
    let size = u32::try_from(data.len()).map_err(text)?;
    // Format 1 is XKB_V1 (wl_keyboard.keymap_format).
    keyboard.keymap(1, file.as_fd(), size);
    connection.flush().map_err(text)?;
    Ok(())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills the two-element array it is given.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned from here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

const MODIFIER_KEYS: [KeyCode; 8] = [
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_RIGHTCTRL,
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_RIGHTSHIFT,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_RIGHTALT,
    KeyCode::KEY_LEFTMETA,
    KeyCode::KEY_RIGHTMETA,
];

/// The keyboards that have modifier keys, to ask which of them are held.
/// Wayland tells no program about keys pressed in other windows; the evdev
/// devices the activation key is read from do.
fn keyboards_with_modifiers() -> Vec<evdev::Device> {
    evdev::enumerate()
        .map(|(_, device)| device)
        .filter(|device| {
            device
                .supported_keys()
                .is_some_and(|keys| keys.contains(KeyCode::KEY_LEFTCTRL))
        })
        .collect()
}

fn modifiers_held(keyboards: &[evdev::Device]) -> bool {
    keyboards.iter().any(|keyboard| {
        keyboard
            .get_key_state()
            .is_ok_and(|keys| MODIFIER_KEYS.iter().any(|key| keys.contains(*key)))
    })
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut State, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut State, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<WlCallback, ()> for State {
    fn event(this: &mut State, _: &WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let wl_callback::Event::Done { .. } = event {
            this.synced = true;
        }
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(_: &mut State, _: &ExtDataControlManagerV1, _: <ExtDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(_: &mut State, _: &ZwlrDataControlManagerV1, _: <ZwlrDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(this: &mut State, _: &ExtDataControlDeviceV1, event: ext_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                this.offers.insert(id.id(), Vec::new());
            }
            ext_data_control_device_v1::Event::Selection { id } => this.set_selection(id.map(Offer::Ext)),
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                this.offers.remove(&offer.id());
                offer.destroy();
            }
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(this: &mut State, _: &ZwlrDataControlDeviceV1, event: zwlr_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                this.offers.insert(id.id(), Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => this.set_selection(id.map(Offer::Wlr)),
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                this.offers.remove(&offer.id());
                offer.destroy();
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(this: &mut State, offer: &ExtDataControlOfferV1, event: ext_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            this.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(this: &mut State, offer: &ZwlrDataControlOfferV1, event: zwlr_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            this.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(this: &mut State, source: &ExtDataControlSourceV1, event: ext_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => this.send(mime_type, fd),
            ext_data_control_source_v1::Event::Cancelled => this.cancelled(&source.id()),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(this: &mut State, source: &ZwlrDataControlSourceV1, event: zwlr_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => this.send(mime_type, fd),
            zwlr_data_control_source_v1::Event::Cancelled => this.cancelled(&source.id()),
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for State {
    fn event(_: &mut State, _: &ZwpInputMethodManagerV2, _: <ZwpInputMethodManagerV2 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(this: &mut State, _: &ZwpInputMethodV2, event: zwp_input_method_v2::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        let name = match &event {
            zwp_input_method_v2::Event::Activate => Some("activate"),
            zwp_input_method_v2::Event::Deactivate => Some("deactivate"),
            zwp_input_method_v2::Event::Done => Some("done"),
            zwp_input_method_v2::Event::Unavailable => Some("unavailable"),
            _ => None,
        };
        if let (Some(name), Some(asked)) = (name, this.im_asked) {
            this.im_events.push(format!("{name} {} ms", asked.elapsed().as_millis()));
        }
        match event {
            zwp_input_method_v2::Event::Activate => this.im_pending_active = true,
            zwp_input_method_v2::Event::Deactivate => this.im_pending_active = false,
            zwp_input_method_v2::Event::Done => {
                this.im_done += 1;
                this.im_active = this.im_pending_active;
            }
            zwp_input_method_v2::Event::Unavailable => this.im_unavailable = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(_: &mut State, _: &ZwpVirtualKeyboardManagerV1, _: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(_: &mut State, _: &ZwpVirtualKeyboardV1, _: <ZwpVirtualKeyboardV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(this: &mut State, _: &ZwlrForeignToplevelManagerV1, event: zwlr_foreign_toplevel_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            this.toplevels.insert(toplevel.id(), Toplevel::default());
        }
    }

    event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(this: &mut State, handle: &ZwlrForeignToplevelHandleV1, event: zwlr_foreign_toplevel_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                this.toplevels.entry(handle.id()).or_default().pending_app_id = Some(app_id);
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state } => {
                // 2 is "activated" in the state array.
                let activated = state
                    .chunks_exact(4)
                    .any(|value| u32::from_ne_bytes([value[0], value[1], value[2], value[3]]) == 2);
                this.toplevels.entry(handle.id()).or_default().pending_activated = Some(activated);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                let toplevel = this.toplevels.entry(handle.id()).or_default();
                if let Some(app_id) = toplevel.pending_app_id.take() {
                    toplevel.app_id = app_id;
                }
                if let Some(activated) = toplevel.pending_activated.take() {
                    toplevel.activated = activated;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                this.toplevels.remove(&handle.id());
                handle.destroy();
            }
            _ => {}
        }
    }
}
