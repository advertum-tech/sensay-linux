// Activation key through evdev, as whisrs reads its hotkeys (src/hotkey/mod.rs).
//
// Every /dev/input/event* that reports both KEY_A and KEY_LEFTMETA counts as
// a keyboard. Devices whose name starts with "Sensay" are our own virtual
// keyboards (step 5) and are skipped. Nothing is grabbed: the key still
// reaches the desktop and the focused window.
//
// One std thread waits in poll(2) on the keyboards, an inotify watch on
// /dev/input and a stop pipe. A keyboard plugged in later is opened when it
// appears; one unplugged is closed when its node reports an error or goes
// away.
//
// The activation key is either one key or the Ctrl+Super chord (either Ctrl
// with either Super). It counts as down while the keyboards together hold
// it, so two keyboards reporting the same press still give one Down and one
// Up, and a chord split across two keyboards works.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread::JoinHandle;

use ::evdev::{Device, EventSummary, KeyCode};
use inotify::{EventMask, Inotify, WatchMask};
use tokio::sync::mpsc::UnboundedSender;

use super::HotkeyEvent;
use crate::config::ActivationKey;
use crate::log;

const INPUT_DIR: &str = "/dev/input";
const OWN_DEVICE_PREFIX: &str = "Sensay";

// Values of an EV_KEY event.
const RELEASED: i32 = 0;
const PRESSED: i32 = 1;

/// The running thread and the pipe that stops it. Both are None when the
/// thread could not be started.
pub struct EvdevListener {
    stop: Option<PipeWriter>,
    thread: Option<JoinHandle<()>>,
    /// The first scan opened no keyboard and was refused permission.
    lacks_access: bool,
}

impl EvdevListener {
    pub fn start(key: ActivationKey, events: UnboundedSender<HotkeyEvent>) -> EvdevListener {
        let key = match key {
            ActivationKey::CtrlSuper => KeySpec::CtrlSuper,
            ActivationKey::Single(name) => KeySpec::Single(KeyCode::from_str(name).unwrap_or_else(|_| {
                log::error!(Hotkey, "evdev has no key named {name}, using KEY_RIGHTCTRL");
                KeyCode::KEY_RIGHTCTRL
            })),
        };

        // The first scan runs here, on the caller's thread, so the keyboards
        // are in the log right after the startup lines.
        let scan = scan();
        report_scan(&scan, key);
        let lacks_access = access_denied(&scan);
        let idle = EvdevListener {
            stop: None,
            thread: None,
            lacks_access,
        };

        let inotify = watch_input_dir();

        let (stop_reader, stop_writer) = match io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                log::error!(Hotkey, "could not create the stop pipe ({error}), activation key is off");
                return idle;
            }
        };

        let listener = Listener {
            key,
            keyboards: scan.keyboards,
            not_keyboards: scan.not_keyboards,
            inotify,
            stop: stop_reader,
            events,
        };

        let thread = std::thread::Builder::new()
            .name("hotkey".to_string())
            .spawn(move || listener.run());
        match thread {
            Ok(thread) => EvdevListener {
                stop: Some(stop_writer),
                thread: Some(thread),
                lacks_access,
            },
            Err(error) => {
                log::error!(Hotkey, "could not start the hotkey thread ({error}), activation key is off");
                idle
            }
        }
    }

    pub fn lacks_access(&self) -> bool {
        self.lacks_access
    }

    pub fn stop(mut self) {
        // One byte wakes the thread; if the write fails the thread still sees
        // the pipe close when the writer is dropped.
        if let Some(mut stop) = self.stop.take() {
            let _ = stop.write_all(&[1]);
        }
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::error!(Hotkey, "hotkey thread panicked");
            }
        }
    }
}

/// The keys the listener watches, and when they make the activation key down.
#[derive(Clone, Copy)]
enum KeySpec {
    /// One key: down while any keyboard holds it.
    Single(KeyCode),
    /// Either Ctrl with either Super: down while both are held, on any
    /// keyboards.
    CtrlSuper,
}

impl KeySpec {
    fn watches(self, code: KeyCode) -> bool {
        match self {
            KeySpec::Single(key) => code == key,
            KeySpec::CtrlSuper => matches!(
                code,
                KeyCode::KEY_LEFTCTRL
                    | KeyCode::KEY_RIGHTCTRL
                    | KeyCode::KEY_LEFTMETA
                    | KeyCode::KEY_RIGHTMETA
            ),
        }
    }

    /// Whether the keys held across all keyboards make the activation key down.
    fn is_down(self, keyboards: &[Keyboard]) -> bool {
        let held = |code: KeyCode| keyboards.iter().any(|keyboard| keyboard.held.contains(&code));
        match self {
            KeySpec::Single(key) => held(key),
            KeySpec::CtrlSuper => {
                (held(KeyCode::KEY_LEFTCTRL) || held(KeyCode::KEY_RIGHTCTRL))
                    && (held(KeyCode::KEY_LEFTMETA) || held(KeyCode::KEY_RIGHTMETA))
            }
        }
    }
}

impl fmt::Display for KeySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeySpec::Single(key) => write!(f, "{key:?}"),
            KeySpec::CtrlSuper => f.write_str("ctrl+super"),
        }
    }
}

struct Keyboard {
    path: PathBuf,
    name: String,
    device: Device,
    /// Watched keys this keyboard reported down and not yet up.
    held: Vec<KeyCode>,
}

struct Scan {
    keyboards: Vec<Keyboard>,
    /// Event nodes that opened but are not keyboards, or are our own.
    not_keyboards: HashSet<PathBuf>,
    /// Event nodes that could not be opened, with the reason.
    failed: Vec<(PathBuf, io::Error)>,
    /// Set when /dev/input itself could not be listed.
    dir_error: Option<io::Error>,
}

enum Opened {
    Keyboard(Keyboard),
    NotKeyboard,
}

fn scan() -> Scan {
    let mut result = Scan {
        keyboards: Vec::new(),
        not_keyboards: HashSet::new(),
        failed: Vec::new(),
        dir_error: None,
    };

    let entries = match std::fs::read_dir(INPUT_DIR) {
        Ok(entries) => entries,
        Err(error) => {
            result.dir_error = Some(error);
            return result;
        }
    };

    let mut nodes: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| is_event_node(path))
        .collect();
    nodes.sort_by_key(|path| event_number(path));

    for path in nodes {
        match open(&path) {
            Ok(Opened::Keyboard(keyboard)) => result.keyboards.push(keyboard),
            Ok(Opened::NotKeyboard) => {
                result.not_keyboards.insert(path);
            }
            Err(error) => result.failed.push((path, error)),
        }
    }
    result
}

fn report_scan(scan: &Scan, key: KeySpec) {
    if let Some(error) = &scan.dir_error {
        log::warning!(Hotkey, "cannot list {INPUT_DIR}: {error}; activation key is off");
        return;
    }

    for keyboard in &scan.keyboards {
        log::info!(Hotkey, "keyboard: {} ({})", keyboard.name, keyboard.path.display());
    }

    let denied: Vec<&PathBuf> = scan
        .failed
        .iter()
        .filter(|(_, error)| error.kind() == io::ErrorKind::PermissionDenied)
        .map(|(path, _)| path)
        .collect();
    if !denied.is_empty() {
        log::warning!(
            Hotkey,
            "no permission to read {} of {} input nodes ({}); sensay setup gives access",
            denied.len(),
            scan.failed.len() + scan.keyboards.len() + scan.not_keyboards.len(),
            names(&denied)
        );
    }
    for (path, error) in &scan.failed {
        if error.kind() != io::ErrorKind::PermissionDenied {
            log::debug!(Hotkey, "cannot open {}: {error}", path.display());
        }
    }

    if scan.keyboards.is_empty() {
        log::warning!(Hotkey, "no keyboard to read: activation key is off until one can be opened");
    } else {
        log::info!(
            Hotkey,
            "activation key {key} on {} keyboard(s)",
            scan.keyboards.len()
        );
    }
}

/// No keyboard opened, and nodes or the folder refused for want of
/// permission.
fn access_denied(scan: &Scan) -> bool {
    let denied = |error: &io::Error| error.kind() == io::ErrorKind::PermissionDenied;
    scan.dir_error.as_ref().is_some_and(denied)
        || (scan.keyboards.is_empty() && scan.failed.iter().any(|(_, error)| denied(error)))
}

fn names(paths: &[&PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Opens `path` without grabbing it and tells whether it is a keyboard we
/// read. Keyboards are switched to non-blocking reads.
fn open(path: &Path) -> io::Result<Opened> {
    let device = Device::open(path)?;
    let name = device.name().unwrap_or("unknown").to_string();

    let is_keyboard = device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_LEFTMETA));
    if !is_keyboard {
        return Ok(Opened::NotKeyboard);
    }
    if name.starts_with(OWN_DEVICE_PREFIX) {
        log::debug!(Hotkey, "skipping own device {name} ({})", path.display());
        return Ok(Opened::NotKeyboard);
    }

    device.set_nonblocking(true)?;
    Ok(Opened::Keyboard(Keyboard {
        path: path.to_path_buf(),
        name,
        device,
        held: Vec::new(),
    }))
}

fn is_event_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("event"))
}

fn event_number(path: &Path) -> u32 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("event"))
        .and_then(|number| number.parse().ok())
        .unwrap_or(u32::MAX)
}

fn watch_input_dir() -> Option<Inotify> {
    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(error) => {
            log::warning!(Hotkey, "inotify unavailable ({error}): keyboards plugged in later need a restart");
            return None;
        }
    };
    // ATTRIB matters: udev gives a new node its group a few milliseconds after
    // creating it, so the first open can fail and the second succeed.
    let mask = WatchMask::CREATE | WatchMask::ATTRIB | WatchMask::DELETE;
    if let Err(error) = inotify.watches().add(INPUT_DIR, mask) {
        log::warning!(
            Hotkey,
            "cannot watch {INPUT_DIR} ({error}): keyboards plugged in later need a restart"
        );
        return None;
    }
    Some(inotify)
}

struct Listener {
    key: KeySpec,
    keyboards: Vec<Keyboard>,
    not_keyboards: HashSet<PathBuf>,
    inotify: Option<Inotify>,
    stop: PipeReader,
    events: UnboundedSender<HotkeyEvent>,
}

impl Listener {
    fn run(mut self) {
        loop {
            // Order: stop pipe, inotify (when there is one), then keyboards.
            let mut fds = Vec::with_capacity(2 + self.keyboards.len());
            fds.push(poll_fd(self.stop.as_raw_fd()));
            if let Some(inotify) = &self.inotify {
                fds.push(poll_fd(inotify.as_raw_fd()));
            }
            let first_keyboard = fds.len();
            for keyboard in &self.keyboards {
                fds.push(poll_fd(keyboard.device.as_raw_fd()));
            }

            // SAFETY: `fds` is a valid, initialized array of `fds.len()` pollfd.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                log::error!(Hotkey, "poll failed ({error}), activation key is off");
                return;
            }

            if fds[0].revents != 0 {
                let mut byte = [0u8; 1];
                let _ = self.stop.read(&mut byte);
                log::debug!(Hotkey, "hotkey thread stopping");
                return;
            }

            // Keyboards first: their indexes in `fds` match `self.keyboards`
            // only until the list changes.
            let mut gone = Vec::new();
            for (index, pollfd) in fds[first_keyboard..].iter().enumerate() {
                if pollfd.revents == 0 {
                    continue;
                }
                if let Err(reason) = self.read_keyboard(index, pollfd.revents) {
                    gone.push((index, reason));
                }
            }
            for (index, reason) in gone.into_iter().rev() {
                self.remove_keyboard(index, &reason);
            }

            if first_keyboard == 2 && fds[1].revents != 0 {
                self.read_inotify();
            }

            if self.events.is_closed() {
                return;
            }
        }
    }

    /// Reads what one keyboard has. Err carries why it should be closed.
    fn read_keyboard(&mut self, index: usize, revents: libc::c_short) -> Result<(), String> {
        let key = self.key;
        let keyboard = &mut self.keyboards[index];
        let mut values = Vec::new();
        match keyboard.device.fetch_events() {
            Ok(events) => {
                for event in events {
                    if let EventSummary::Key(_, code, value) = event.destructure() {
                        if key.watches(code) {
                            values.push((code, value));
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(format!("poll reported {revents:#x}"));
                }
            }
            Err(error) => return Err(error.to_string()),
        }

        for (code, value) in values {
            match value {
                PRESSED => self.set_held(index, code, true),
                RELEASED => self.set_held(index, code, false),
                // 2 is auto-repeat while the key is held.
                _ => {}
            }
        }
        Ok(())
    }

    /// Records that keyboard `index` holds or released a watched key and
    /// sends Down or Up when the activation key as a whole changes.
    fn set_held(&mut self, index: usize, code: KeyCode, held: bool) {
        let was_down = self.is_down();
        let keys = &mut self.keyboards[index].held;
        if held {
            if !keys.contains(&code) {
                keys.push(code);
            }
        } else {
            keys.retain(|key| *key != code);
        }
        self.report_change(was_down);
    }

    fn report_change(&self, was_down: bool) {
        let is_down = self.is_down();
        if is_down != was_down {
            let event = if is_down { HotkeyEvent::Down } else { HotkeyEvent::Up };
            let _ = self.events.send(event);
        }
    }

    fn is_down(&self) -> bool {
        self.key.is_down(&self.keyboards)
    }

    fn remove_keyboard(&mut self, index: usize, reason: &str) {
        // A keyboard pulled out with keys held counts as releasing them.
        let was_down = self.is_down();
        let keyboard = self.keyboards.remove(index);
        self.report_change(was_down);
        log::info!(
            Hotkey,
            "keyboard removed: {} ({}): {reason}",
            keyboard.name,
            keyboard.path.display()
        );
    }

    fn read_inotify(&mut self) {
        let Some(inotify) = self.inotify.as_mut() else { return };

        let mut buffer = [0u8; 4096];
        let mut changes: Vec<(EventMask, Option<OsString>)> = Vec::new();
        loop {
            match inotify.read_events(&mut buffer) {
                Ok(events) => {
                    let before = changes.len();
                    changes.extend(events.map(|event| (event.mask, event.name.map(OsString::from))));
                    if changes.len() == before {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    log::warning!(
                        Hotkey,
                        "reading inotify failed ({error}): keyboards plugged in later need a restart"
                    );
                    self.inotify = None;
                    break;
                }
            }
        }

        for (mask, name) in changes {
            if mask.contains(EventMask::Q_OVERFLOW) {
                self.rescan();
                continue;
            }
            let Some(name) = name else { continue };
            let path = Path::new(INPUT_DIR).join(name);
            if !is_event_node(&path) {
                continue;
            }
            if mask.contains(EventMask::DELETE) {
                self.not_keyboards.remove(&path);
                if let Some(index) = self.keyboards.iter().position(|k| k.path == path) {
                    self.remove_keyboard(index, "node deleted");
                }
            } else {
                self.try_add(&path);
            }
        }
    }

    /// Opens a node that appeared or changed, unless it is already known.
    fn try_add(&mut self, path: &Path) {
        if self.not_keyboards.contains(path) || self.keyboards.iter().any(|k| k.path == path) {
            return;
        }
        match open(path) {
            Ok(Opened::Keyboard(keyboard)) => {
                log::info!(
                    Hotkey,
                    "keyboard added: {} ({})",
                    keyboard.name,
                    keyboard.path.display()
                );
                self.keyboards.push(keyboard);
            }
            Ok(Opened::NotKeyboard) => {
                self.not_keyboards.insert(path.to_path_buf());
            }
            // Usually permission denied in the moment before udev sets the
            // group; the ATTRIB event that follows brings another try.
            Err(error) => log::debug!(Hotkey, "cannot open {} yet: {error}", path.display()),
        }
    }

    /// Events were lost: look at every node again.
    fn rescan(&mut self) {
        log::debug!(Hotkey, "inotify queue overflowed, rescanning {INPUT_DIR}");
        let Ok(entries) = std::fs::read_dir(INPUT_DIR) else { return };
        let nodes: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| is_event_node(path))
            .collect();
        for path in nodes {
            self.try_add(&path);
        }
    }
}

fn poll_fd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}
