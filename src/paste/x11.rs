// Paste on X11: the text goes on the CLIPBOARD selection, Ctrl+V (Ctrl+Shift+V
// in a terminal) is pressed through XTest, and 500 ms later the old clipboard
// comes back if the selection is still ours, as ClipboardTextSink.swift does
// on macOS. linux-spec.md, section 8.
//
// X11 keeps no clipboard of its own: the owner of the CLIPBOARD selection
// hands its data to whoever asks, at the moment they ask. So a thread of its
// own holds an invisible window that owns the selection and answers every
// request: for the text during a paste, and for the old content after it,
// until another program takes the clipboard.
//
// The old content is kept in one format, the one that matters most (files,
// then an image, then text), as on GNOME: reading every format makes some
// programs produce each of them, which can take seconds (linux-text-input.md).
// Keys go as X keycodes of the evdev positions (Ctrl 37, Shift 50, V 55), so
// with Russian on top Ctrl plus the V key still pastes.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, KeyButMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::Report;
use super::formats::pick_format;
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

const KEYCODE_CTRL: u8 = 37;
const KEYCODE_SHIFT: u8 = 50;
const KEYCODE_V: u8 = 55;

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn ms(length: Duration) -> u64 {
    u64::try_from(length.as_millis()).unwrap_or(u64::MAX)
}

/// WM_CLASS holds the instance and the class, each ending in a zero byte.
fn parse_wm_class(value: &[u8]) -> (String, String) {
    let mut parts = value.split(|byte| *byte == 0).map(|part| String::from_utf8_lossy(part).into_owned());
    let instance = parts.next().unwrap_or_default();
    let class = parts.next().unwrap_or_default();
    (instance, class)
}

/// UTF-8 text under each name programs ask for it by.
fn text_forms<'a>(atoms: &Atoms, bytes: &'a [u8]) -> Vec<(Atom, &'a [u8])> {
    vec![
        (atoms.utf8_string, bytes),
        (atoms.text_plain_utf8, bytes),
        (atoms.text_plain, bytes),
        (atoms.text, bytes),
    ]
}

enum Command {
    Paste {
        text: String,
        reply: tokio::sync::oneshot::Sender<Result<Report, String>>,
    },
}

/// The app's side of the paste thread.
#[derive(Clone)]
pub struct X11Paste {
    commands: mpsc::Sender<Command>,
    wake: Arc<Wake>,
}

impl X11Paste {
    pub fn start() -> Result<X11Paste, String> {
        let wake = Arc::new(Wake::new().map_err(|error| format!("no eventfd: {error}"))?);
        let (commands_tx, commands_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_wake = wake.clone();
        std::thread::Builder::new()
            .name("paste-x11".to_string())
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
            Ok(Ok(())) => Ok(X11Paste {
                commands: commands_tx,
                wake,
            }),
            Ok(Err(reason)) => Err(reason),
            Err(_) => Err("the paste thread did not start".to_string()),
        }
    }

    pub async fn paste(&self, text: &str) -> Result<Report, String> {
        let gone = || "the X11 paste thread is gone".to_string();
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

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    utf8_string: Atom,
    text: Atom,
    text_plain_utf8: Atom,
    text_plain: Atom,
    /// Our window's property that conversions of the old clipboard land in.
    transfer: Atom,
    /// Our window's property touched to learn the server time.
    clock: Atom,
    active_window: Atom,
}

impl Atoms {
    fn intern(connection: &RustConnection) -> Result<Atoms, String> {
        let names = [
            "CLIPBOARD",
            "TARGETS",
            "TIMESTAMP",
            "INCR",
            "UTF8_STRING",
            "TEXT",
            "text/plain;charset=utf-8",
            "text/plain",
            "SENSAY_TRANSFER",
            "SENSAY_CLOCK",
            "_NET_ACTIVE_WINDOW",
        ];
        let cookies = names
            .iter()
            .map(|name| connection.intern_atom(false, name.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(text)?;
        let atoms = cookies
            .into_iter()
            .map(|cookie| cookie.reply().map(|reply| reply.atom))
            .collect::<Result<Vec<_>, _>>()
            .map_err(text)?;
        Ok(Atoms {
            clipboard: atoms[0],
            targets: atoms[1],
            timestamp: atoms[2],
            incr: atoms[3],
            utf8_string: atoms[4],
            text: atoms[5],
            text_plain_utf8: atoms[6],
            text_plain: atoms[7],
            transfer: atoms[8],
            clock: atoms[9],
            active_window: atoms[10],
        })
    }
}

/// What the window hands out while it owns the clipboard.
enum Held {
    Nothing,
    Text(Vec<u8>),
    Saved { format: Atom, name: String, bytes: Vec<u8> },
}

/// The clipboard as it was before a paste.
enum Before {
    Empty,
    Unreadable(String),
    Saved { format: Atom, name: String, bytes: Vec<u8> },
}

struct Paster {
    connection: RustConnection,
    root: Window,
    window: Window,
    atoms: Atoms,
    xtest: bool,
    held: Held,
    owned_since: u32,
    /// Somebody else took the clipboard while a paste waited.
    lost: bool,
    /// When the keys went, for counting the reads that follow.
    keys_at: Option<Instant>,
    reads: Vec<(Atom, Duration)>,
}

impl Paster {
    fn connect() -> Result<Paster, String> {
        let (connection, screen_number) = x11rb::connect(None).map_err(|error| format!("no X11 display: {error}"))?;
        let root = connection.setup().roots[screen_number].root;
        let xtest = connection
            .extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(text)?
            .is_some();
        let window = connection.generate_id().map_err(text)?;
        connection
            .create_window(
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(text)?;
        let atoms = Atoms::intern(&connection)?;
        connection.flush().map_err(text)?;
        Ok(Paster {
            connection,
            root,
            window,
            atoms,
            xtest,
            held: Held::Nothing,
            owned_since: 0,
            lost: false,
            keys_at: None,
            reads: Vec::new(),
        })
    }

    fn fd(&self) -> RawFd {
        self.connection.stream().as_raw_fd()
    }

    /// Answers clipboard requests and takes pastes until the app quits.
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
            if let Err(reason) = self.drain(&|_| false) {
                crate::log::error!(Paste, "paste: the X11 connection failed: {reason}");
                return;
            }
            let ready = wait_readable(&[self.fd(), wake.fd], None);
            if ready[1] {
                wake.clear();
            }
        }
    }

    /// Handles every event already in, answering clipboard requests on the
    /// way; returns the first one `want` takes.
    fn drain(&mut self, want: &dyn Fn(&Event) -> bool) -> Result<Option<Event>, String> {
        while let Some(event) = self.connection.poll_for_event().map_err(text)? {
            if let Event::SelectionRequest(request) = &event {
                let request = request.clone();
                self.answer(&request)?;
                continue;
            }
            if let Event::SelectionClear(clear) = &event {
                if clear.selection == self.atoms.clipboard {
                    self.held = Held::Nothing;
                    self.lost = true;
                }
                continue;
            }
            if want(&event) {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    /// Waits until `deadline`, answering requests meanwhile; returns early
    /// with the first event `want` takes.
    fn wait_until(&mut self, deadline: Instant, want: &dyn Fn(&Event) -> bool) -> Result<Option<Event>, String> {
        loop {
            self.connection.flush().map_err(text)?;
            if let Some(event) = self.drain(want)? {
                return Ok(Some(event));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            wait_readable(&[self.fd()], Some(deadline - now));
        }
    }

    fn pause(&mut self, length: Duration) -> Result<(), String> {
        self.wait_until(Instant::now() + length, &|_| false).map(|_| ())
    }

    fn paste(&mut self, dictated: String) -> Result<Report, String> {
        let started = Instant::now();
        let mut report = Report {
            path: Some("clipboard".to_string()),
            ..Report::default()
        };

        let before = self.snapshot();
        report.before = Some(match &before {
            Before::Empty => "(empty)".to_string(),
            Before::Unreadable(reason) => format!("could not be read: {reason}"),
            Before::Saved { name, bytes, .. } => format!("{name}, {} bytes", bytes.len()),
        });

        // Ours from here until somebody else takes the clipboard.
        self.lost = false;
        self.reads.clear();
        self.keys_at = None;
        self.held = Held::Text(dictated.into_bytes());
        let time = self.server_time()?;
        if !self.own(time)? {
            report.keys = Some("none, the clipboard could not be taken".to_string());
            report.restore = Some("nothing changed".to_string());
            report.total_ms = Some(ms(started.elapsed()));
            return Ok(report);
        }

        self.pause(BEFORE_PASTE)?;
        let mut waited = Duration::ZERO;
        while self.modifiers_held()? {
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

        let (instance, class) = self.focused_class();
        report.target = Some(
            [&class, &instance]
                .into_iter()
                .find(|name| !name.is_empty())
                .cloned()
                .unwrap_or_else(|| "(no window)".to_string()),
        );
        if !self.xtest {
            report.keys = Some("none, the X server has no XTest".to_string());
            report.restore = Some("text left on the clipboard".to_string());
            report.total_ms = Some(ms(started.elapsed()));
            return Ok(report);
        }
        let terminal = is_terminal(&instance) || is_terminal(&class);
        report.keys = Some(if terminal { "Ctrl+Shift+V" } else { "Ctrl+V" }.to_string());
        self.keys_at = Some(Instant::now());
        self.press_paste(terminal)?;

        self.pause(BEFORE_RESTORE)?;
        report.reads = Some(self.describe_reads());
        self.keys_at = None;
        report.restore = Some(if self.lost {
            "clipboard changed meanwhile, left as it is".to_string()
        } else {
            match before {
                Before::Saved { format, name, bytes } => {
                    let line = format!("restored {name}, {} bytes", bytes.len());
                    self.held = Held::Saved { format, name, bytes };
                    let time = self.server_time()?;
                    self.own(time)?;
                    line
                }
                Before::Empty => {
                    let time = self.server_time()?;
                    self.connection
                        .set_selection_owner(NONE, self.atoms.clipboard, time)
                        .map_err(text)?;
                    self.held = Held::Nothing;
                    "cleared, it was empty".to_string()
                }
                Before::Unreadable(_) => "text left on the clipboard, the old content could not be read".to_string(),
            }
        });
        report.total_ms = Some(ms(started.elapsed()));
        Ok(report)
    }

    /// The clipboard before the paste, in the one format it is kept in.
    fn snapshot(&mut self) -> Before {
        let owner = match self
            .connection
            .get_selection_owner(self.atoms.clipboard)
            .map_err(text)
            .and_then(|cookie| cookie.reply().map_err(text))
        {
            Ok(reply) => reply.owner,
            Err(reason) => return Before::Unreadable(reason),
        };
        if owner == NONE {
            return Before::Empty;
        }
        if owner == self.window {
            return match &self.held {
                Held::Nothing => Before::Empty,
                Held::Text(bytes) => Before::Saved {
                    format: self.atoms.utf8_string,
                    name: "UTF8_STRING".to_string(),
                    bytes: bytes.clone(),
                },
                Held::Saved { format, name, bytes } => Before::Saved {
                    format: *format,
                    name: name.clone(),
                    bytes: bytes.clone(),
                },
            };
        }

        let deadline = Instant::now() + READ_LIMIT;
        let targets: Vec<Atom> = match self.read(self.atoms.targets, deadline) {
            Ok(bytes) => bytes
                .chunks_exact(4)
                .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect(),
            Err(reason) => return Before::Unreadable(reason),
        };
        let names = match self.atom_names(&targets) {
            Ok(names) => names,
            Err(reason) => return Before::Unreadable(reason),
        };
        let Some(index) = pick_format(&names) else {
            return Before::Unreadable("no format that can be kept".to_string());
        };
        match self.read(targets[index], deadline) {
            Ok(bytes) => Before::Saved {
                format: targets[index],
                name: names[index].clone(),
                bytes,
            },
            Err(reason) => Before::Unreadable(format!("{}: {reason}", names[index])),
        }
    }

    /// Converts the clipboard into `target` on our window and reads it, in
    /// parts when the owner sends it so (INCR).
    fn read(&mut self, target: Atom, deadline: Instant) -> Result<Vec<u8>, String> {
        let (window, clipboard, transfer, incr) = (self.window, self.atoms.clipboard, self.atoms.transfer, self.atoms.incr);
        self.connection.delete_property(window, transfer).map_err(text)?;
        self.connection
            .convert_selection(window, clipboard, target, transfer, CURRENT_TIME)
            .map_err(text)?;
        let answered = self.wait_until(deadline, &|event| {
            matches!(event, Event::SelectionNotify(notify) if notify.requestor == window && notify.selection == clipboard)
        })?;
        let Some(Event::SelectionNotify(notify)) = answered else {
            return Err("the owner did not answer".to_string());
        };
        if notify.property == NONE {
            return Err("the owner refused".to_string());
        }
        let first = self.take_property()?;
        if first.0 != incr {
            return Ok(first.1);
        }
        // Deleting the INCR property asked for the first part; each part comes
        // as a new value, and an empty one ends the transfer.
        let mut data = Vec::new();
        loop {
            let arrived = self.wait_until(deadline, &|event| {
                matches!(event, Event::PropertyNotify(change)
                    if change.window == window && change.atom == transfer && change.state == Property::NEW_VALUE)
            })?;
            if arrived.is_none() {
                return Err("the owner stopped sending".to_string());
            }
            let (_, part) = self.take_property()?;
            if part.is_empty() {
                return Ok(data);
            }
            data.extend_from_slice(&part);
        }
    }

    /// Reads and deletes our transfer property: its type and bytes.
    fn take_property(&self) -> Result<(Atom, Vec<u8>), String> {
        let reply = self
            .connection
            .get_property(true, self.window, self.atoms.transfer, AtomEnum::ANY, 0, u32::MAX / 4)
            .map_err(text)?
            .reply()
            .map_err(text)?;
        Ok((reply.type_, reply.value))
    }

    fn atom_names(&self, atoms: &[Atom]) -> Result<Vec<String>, String> {
        let cookies = atoms
            .iter()
            .map(|atom| self.connection.get_atom_name(*atom))
            .collect::<Result<Vec<_>, _>>()
            .map_err(text)?;
        cookies
            .into_iter()
            .map(|cookie| {
                cookie
                    .reply()
                    .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
                    .map_err(text)
            })
            .collect()
    }

    /// The X server's time now, from a property change on our window: the
    /// clipboard rules want a real time, not CurrentTime.
    fn server_time(&mut self) -> Result<u32, String> {
        let (window, clock) = (self.window, self.atoms.clock);
        self.connection
            .change_property8(PropMode::APPEND, window, clock, AtomEnum::STRING, &[])
            .map_err(text)?;
        let changed = self.wait_until(Instant::now() + Duration::from_secs(1), &|event| {
            matches!(event, Event::PropertyNotify(change) if change.window == window && change.atom == clock)
        })?;
        match changed {
            Some(Event::PropertyNotify(change)) => Ok(change.time),
            _ => Err("the X server did not tell the time".to_string()),
        }
    }

    /// Takes the clipboard; false when the server gave it to someone else.
    fn own(&mut self, time: u32) -> Result<bool, String> {
        self.connection
            .set_selection_owner(self.window, self.atoms.clipboard, time)
            .map_err(text)?;
        let owner = self
            .connection
            .get_selection_owner(self.atoms.clipboard)
            .map_err(text)?
            .reply()
            .map_err(text)?
            .owner;
        self.owned_since = time;
        Ok(owner == self.window)
    }

    fn modifiers_held(&self) -> Result<bool, String> {
        let mask = self
            .connection
            .query_pointer(self.root)
            .map_err(text)?
            .reply()
            .map_err(text)?
            .mask;
        // Caps Lock (Lock) and Num Lock (Mod2) stay on and do not count.
        let held = KeyButMask::SHIFT | KeyButMask::CONTROL | KeyButMask::MOD1 | KeyButMask::MOD3 | KeyButMask::MOD4 | KeyButMask::MOD5;
        Ok(u16::from(mask) & u16::from(held) != 0)
    }

    /// Instance and class of the active window, from _NET_ACTIVE_WINDOW and
    /// WM_CLASS; empty when the window manager does not say.
    fn focused_class(&self) -> (String, String) {
        let active = self
            .connection
            .get_property(false, self.root, self.atoms.active_window, AtomEnum::WINDOW, 0, 1)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32().and_then(|mut values| values.next()))
            .filter(|window| *window != NONE);
        let Some(window) = active else { return (String::new(), String::new()) };
        self.connection
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| parse_wm_class(&reply.value))
            .unwrap_or_default()
    }

    fn press_paste(&mut self, terminal: bool) -> Result<(), String> {
        let mut keys = vec![(KEY_PRESS_EVENT, KEYCODE_CTRL)];
        if terminal {
            keys.push((KEY_PRESS_EVENT, KEYCODE_SHIFT));
        }
        keys.push((KEY_PRESS_EVENT, KEYCODE_V));
        keys.push((KEY_RELEASE_EVENT, KEYCODE_V));
        if terminal {
            keys.push((KEY_RELEASE_EVENT, KEYCODE_SHIFT));
        }
        keys.push((KEY_RELEASE_EVENT, KEYCODE_CTRL));
        for (index, (kind, keycode)) in keys.into_iter().enumerate() {
            if index > 0 {
                self.pause(BETWEEN_KEYS)?;
            }
            self.connection
                .xtest_fake_input(kind, keycode, CURRENT_TIME, NONE, 0, 0, 0)
                .map_err(text)?;
            self.connection.flush().map_err(text)?;
        }
        Ok(())
    }

    /// The formats held now, with their bytes.
    fn offered(&self) -> Vec<(Atom, &[u8])> {
        let atoms = &self.atoms;
        match &self.held {
            Held::Nothing => Vec::new(),
            Held::Text(bytes) => text_forms(atoms, bytes),
            Held::Saved { format, bytes, .. } => {
                if [atoms.utf8_string, atoms.text_plain_utf8, atoms.text_plain].contains(format) {
                    text_forms(atoms, bytes)
                } else {
                    vec![(*format, bytes.as_slice())]
                }
            }
        }
    }

    /// Writes what `target` asks for into the requestor's property; false
    /// when there is nothing to give in that form.
    fn provide(&self, requestor: Window, property: Atom, target: Atom) -> Result<bool, String> {
        let offered = self.offered();
        if offered.is_empty() {
            return Ok(false);
        }
        if target == self.atoms.targets {
            let mut list = vec![self.atoms.targets, self.atoms.timestamp];
            list.extend(offered.iter().map(|(atom, _)| *atom));
            self.connection
                .change_property32(PropMode::REPLACE, requestor, property, AtomEnum::ATOM, &list)
                .map_err(text)?;
            return Ok(true);
        }
        if target == self.atoms.timestamp {
            self.connection
                .change_property32(PropMode::REPLACE, requestor, property, AtomEnum::INTEGER, &[self.owned_since])
                .map_err(text)?;
            return Ok(true);
        }
        let Some((_, bytes)) = offered.iter().find(|(atom, _)| *atom == target) else {
            return Ok(false);
        };
        // Larger data would need INCR; nothing this app holds gets there in
        // practice, so it is refused rather than sent in parts.
        if bytes.len() + 1024 > self.connection.maximum_request_bytes() {
            return Ok(false);
        }
        let kind = if target == self.atoms.text { self.atoms.utf8_string } else { target };
        self.connection
            .change_property8(PropMode::REPLACE, requestor, property, kind, bytes)
            .map_err(text)?;
        Ok(true)
    }

    fn answer(&mut self, request: &SelectionRequestEvent) -> Result<(), String> {
        // Old clients leave the property empty and mean the target's name.
        let property = if request.property == NONE { request.target } else { request.property };
        let fresh = request.time == CURRENT_TIME || request.time >= self.owned_since;
        let served = request.selection == self.atoms.clipboard
            && fresh
            && self.provide(request.requestor, property, request.target)?;
        if served {
            if let Some(keys_at) = self.keys_at {
                self.reads.push((request.target, keys_at.elapsed()));
            }
        }
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: if served { property } else { NONE },
        };
        self.connection
            .send_event(false, request.requestor, EventMask::NO_EVENT, notify)
            .map_err(text)?;
        self.connection.flush().map_err(text)?;
        Ok(())
    }

    /// For the log: who read the clipboard after the keys, the receipt that
    /// the paste happened.
    fn describe_reads(&self) -> String {
        let Some((_, first)) = self.reads.first() else {
            return "nobody read the clipboard after the keys".to_string();
        };
        let mut targets: Vec<Atom> = Vec::new();
        for (target, _) in &self.reads {
            if !targets.contains(target) {
                targets.push(*target);
            }
        }
        let names = self.atom_names(&targets).unwrap_or_default().join(", ");
        format!(
            "read {} time(s) after the keys, first after {} ms ({names})",
            self.reads.len(),
            first.as_millis()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_wm_class() {
        assert_eq!(
            parse_wm_class(b"xfce4-terminal\0Xfce4-terminal\0"),
            ("xfce4-terminal".to_string(), "Xfce4-terminal".to_string())
        );
        assert_eq!(parse_wm_class(b""), (String::new(), String::new()));
    }
}
