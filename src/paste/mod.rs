// Puts the recognised text where the cursor is. The whole text goes in
// through the input method when the focused field takes it; anywhere else it
// goes on the clipboard, then the paste keys, then the old clipboard back, as
// ClipboardTextSink.swift. The order is in linux-spec.md, section 8.
//
// On GNOME Wayland no application may commit text, set the clipboard or press
// keys for another window. GNOME Shell can, so its extension
// sensay-pill@sensay.app does it (gnome/sensay-pill@sensay.app/paste.js) and
// this asks it to over the session bus. In an X11 session there is no input
// method to hand text to, so x11.rs pastes through the clipboard. On the other
// Wayland compositors wayland.rs hands the text to the input method or pastes
// through data-control.

mod formats;
mod terminals;
mod wake;
mod wayland;
mod x11;

use std::fmt;
use std::time::{Duration, Instant};

use serde::Deserialize;
use zbus::Connection;

use crate::desktop;
use crate::log;

const SHELL_NAME: &str = "org.gnome.Shell";
const SHELL_PATH: &str = "/com/advertumtech/Sensay/Shell";
const SHELL_INTERFACE: &str = "com.advertumtech.Sensay.Shell";
/// The extension answers after about half a second, or after up to three
/// seconds of waiting for held keys; past this it counts as stuck.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

// Report::keys when no keys were pressed. KEYS_HELD is also in paste.js.
const KEYS_HELD: &str = "none, modifiers still held";
const NO_KEYBOARD: &str = "none, no virtual keyboard and no uinput";
const NO_XTEST: &str = "none, the X server has no XTest";
const NOT_TAKEN: &str = "none, the clipboard could not be taken";

#[derive(Debug)]
pub enum PasteError {
    /// No way to paste on this desktop yet.
    NoPath,
    /// GNOME on Wayland, and the extension does not answer.
    NoExtension,
    /// The text is on the clipboard and no keys were pressed. `setup`: there
    /// was no keyboard to press them with.
    LeftOnClipboard { setup: bool },
    Failed(String),
}

impl PasteError {
    /// What the user is told, in the words of ErrorPresenter.swift.
    pub fn message(&self) -> String {
        match self {
            PasteError::NoPath => "Sensay cannot paste on this desktop. The text is in the history.".to_string(),
            PasteError::NoExtension => "The text was not pasted: the GNOME extension is not running yet. \
                                        It starts after you log out and back in."
                .to_string(),
            PasteError::LeftOnClipboard { setup: false } => {
                "The text is on the clipboard: press Ctrl+V to paste it.".to_string()
            }
            PasteError::LeftOnClipboard { setup: true } => "The text is on the clipboard: press Ctrl+V to paste it. \
                                                             Run sensay setup in a terminal to have Sensay do it."
                .to_string(),
            PasteError::Failed(reason) => format!("The text was not pasted: {reason}. It is in the history."),
        }
    }
}

impl fmt::Display for PasteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PasteError::NoPath => f.write_str("no way to paste on this desktop yet, the text is in the history"),
            PasteError::NoExtension => f.write_str("the GNOME extension sensay-pill@sensay.app is not loaded"),
            PasteError::LeftOnClipboard { .. } => f.write_str("no keys pressed, the text is left on the clipboard"),
            PasteError::Failed(reason) => write!(f, "paste failed: {reason}"),
        }
    }
}

/// What one paste did, for the log: from the GNOME extension as JSON, or
/// from the X11 path.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Report {
    /// "input-method" or "clipboard".
    path: Option<String>,
    before: Option<String>,
    waited_ms: Option<u64>,
    target: Option<String>,
    keys: Option<String>,
    /// Who read the clipboard after the keys (X11 and Wayland paths).
    reads: Option<String>,
    /// Why the input method did not take the text (Wayland path).
    input_method: Option<String>,
    restore: Option<String>,
    total_ms: Option<u64>,
}

#[derive(Clone)]
enum Path {
    /// The session bus, to ask the GNOME extension.
    Gnome(Connection),
    X11(x11::X11Paste),
    Wayland(wayland::WaylandPaste),
    Nothing,
}

#[derive(Clone)]
pub struct TextSink {
    path: Path,
}

impl TextSink {
    /// The clipboard and keys parts of the `backends:` line.
    pub fn backend_names() -> (&'static str, &'static str) {
        if desktop::is_gnome_wayland() {
            ("gnome-extension", "gnome-extension")
        } else if desktop::windows_use_x11() {
            ("x11-selection", "xtest")
        } else if desktop::is_wayland() {
            ("data-control", "input-method/virtual-keyboard")
        } else {
            ("none", "none")
        }
    }

    pub fn new(connection: Option<&Connection>) -> TextSink {
        let path = if desktop::is_gnome_wayland() {
            connection.cloned().map_or(Path::Nothing, Path::Gnome)
        } else if desktop::windows_use_x11() {
            match x11::X11Paste::start() {
                Ok(paste) => Path::X11(paste),
                Err(reason) => {
                    log::error!(Paste, "paste: the X11 path could not start: {reason}");
                    Path::Nothing
                }
            }
        } else if desktop::is_wayland() {
            match wayland::WaylandPaste::start() {
                Ok(paste) => Path::Wayland(paste),
                Err(reason) => {
                    log::error!(Paste, "paste: the Wayland path could not start: {reason}");
                    Path::Nothing
                }
            }
        } else {
            Path::Nothing
        };
        TextSink { path }
    }

    pub async fn deliver(&self, text: &str, tag: &str) -> Result<(), PasteError> {
        let started = Instant::now();
        let report = match &self.path {
            Path::Nothing => return Err(PasteError::NoPath),
            Path::Gnome(connection) => ask_extension(connection, text).await?,
            Path::X11(paste) => finish(paste.paste(text)).await?,
            Path::Wayland(paste) => finish(paste.paste(text)).await?,
        };
        log_report(tag, text, &report, started);
        match report.keys.as_deref() {
            Some(NOT_TAKEN) => Err(PasteError::Failed("the clipboard could not be taken".to_string())),
            Some(NO_KEYBOARD) => Err(PasteError::LeftOnClipboard { setup: true }),
            Some(KEYS_HELD | NO_XTEST) => Err(PasteError::LeftOnClipboard { setup: false }),
            _ => Ok(()),
        }
    }
}

/// Waits for a paste thread's report, no longer than the call limit.
async fn finish(paste: impl std::future::Future<Output = Result<Report, String>>) -> Result<Report, PasteError> {
    match tokio::time::timeout(CALL_TIMEOUT, paste).await {
        Err(_) => Err(PasteError::Failed(format!(
            "the paste did not finish in {} s",
            CALL_TIMEOUT.as_secs()
        ))),
        Ok(Err(reason)) => Err(PasteError::Failed(reason)),
        Ok(Ok(report)) => Ok(report),
    }
}

/// Hands the text to the GNOME extension and reads back its report.
async fn ask_extension(connection: &Connection, text: &str) -> Result<Report, PasteError> {
    let body = (text,);
    let call = connection.call_method(Some(SHELL_NAME), SHELL_PATH, Some(SHELL_INTERFACE), "Paste", &body);
    let reply = match tokio::time::timeout(CALL_TIMEOUT, call).await {
        Err(_) => {
            return Err(PasteError::Failed(format!(
                "the GNOME extension did not answer in {} s",
                CALL_TIMEOUT.as_secs()
            )));
        }
        Ok(Err(error)) => return Err(extension_error(&error)),
        Ok(Ok(reply)) => reply,
    };
    let report: String = reply
        .body()
        .deserialize()
        .map_err(|error| PasteError::Failed(format!("unexpected answer: {error}")))?;
    Ok(serde_json::from_str(&report).unwrap_or_default())
}

/// The lines one paste leaves in the log, whichever path it took.
fn log_report(tag: &str, text: &str, report: &Report, started: Instant) {
    let target = report.target.as_deref().unwrap_or("?");
    if report.path.as_deref() == Some("input-method") {
        log::info!(
            Paste,
            "[{tag}] committed through the input method, {} characters, target {target}, clipboard not touched, done in {} ms with the call",
            text.chars().count(),
            started.elapsed().as_millis()
        );
        return;
    }
    if let Some(why) = &report.input_method {
        log::info!(Paste, "[{tag}] input method: {why}");
    }
    let keys = report.keys.as_deref().unwrap_or("?");
    log::info!(Paste, "[{tag}] {keys} posted, {} characters, target {target}", text.chars().count());
    log::info!(
        Paste,
        "[{tag}] was on the clipboard before: {}",
        report.before.as_deref().unwrap_or("?")
    );
    if let Some(waited) = report.waited_ms.filter(|waited| *waited > 0) {
        log::info!(Paste, "[{tag}] waited {waited} ms for held keys to be released");
    }
    if let Some(reads) = &report.reads {
        log::info!(Paste, "[{tag}] {reads}");
    }
    log::info!(Paste, "[{tag}] {}", report.restore.as_deref().unwrap_or("?"));
    log::info!(
        Paste,
        "[{tag}] paste done in {} ms, {} ms with the call",
        report.total_ms.unwrap_or(0),
        started.elapsed().as_millis()
    );
}

/// NoExtension when the extension is not there to answer.
fn extension_error(error: &zbus::Error) -> PasteError {
    if let zbus::Error::MethodError(name, message, _) = error {
        let name = name.as_str();
        if name.ends_with("UnknownObject") || name.ends_with("UnknownMethod") || name.ends_with("UnknownInterface") {
            return PasteError::NoExtension;
        }
        return PasteError::Failed(format!("{name}: {}", message.as_deref().unwrap_or("")));
    }
    PasteError::Failed(error.to_string())
}
