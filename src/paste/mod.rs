// Puts the recognised text where the cursor is: on the clipboard, then the
// paste keys, then the old clipboard back, as ClipboardTextSink.swift.
//
// On GNOME Wayland no application may set the clipboard or press keys for
// another window. GNOME Shell can, so its extension sensay-pill@sensay.app
// does the paste (gnome/sensay-pill@sensay.app/paste.js) and this asks it to
// over the session bus. Other desktops get their own paths in step 7.

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

#[derive(Debug)]
pub enum PasteError {
    /// No way to paste on this desktop yet.
    NoPath,
    Failed(String),
}

impl fmt::Display for PasteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PasteError::NoPath => f.write_str("no way to paste on this desktop yet, the text is in the history"),
            PasteError::Failed(reason) => write!(f, "paste failed: {reason}"),
        }
    }
}

/// What the extension tells about one paste.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Report {
    before: Option<String>,
    waited_ms: Option<u64>,
    target: Option<String>,
    keys: Option<String>,
    restore: Option<String>,
    total_ms: Option<u64>,
}

#[derive(Clone)]
pub struct TextSink {
    /// The session bus, on GNOME Wayland; None elsewhere for now.
    connection: Option<Connection>,
}

impl TextSink {
    /// The clipboard and keys parts of the `backends:` line.
    pub fn backend_names() -> (&'static str, &'static str) {
        if desktop::is_gnome_wayland() {
            ("gnome-extension", "gnome-extension")
        } else {
            ("none", "none")
        }
    }

    pub fn new(connection: Option<&Connection>) -> TextSink {
        TextSink {
            connection: connection.filter(|_| desktop::is_gnome_wayland()).cloned(),
        }
    }

    pub async fn deliver(&self, text: &str, tag: &str) -> Result<(), PasteError> {
        let Some(connection) = &self.connection else {
            return Err(PasteError::NoPath);
        };
        let started = Instant::now();
        let body = (text,);
        let call = connection.call_method(Some(SHELL_NAME), SHELL_PATH, Some(SHELL_INTERFACE), "Paste", &body);
        let reply = match tokio::time::timeout(CALL_TIMEOUT, call).await {
            Err(_) => {
                return Err(PasteError::Failed(format!(
                    "the GNOME extension did not answer in {} s",
                    CALL_TIMEOUT.as_secs()
                )));
            }
            Ok(Err(error)) => return Err(PasteError::Failed(describe(&error))),
            Ok(Ok(reply)) => reply,
        };
        let report: String = reply
            .body()
            .deserialize()
            .map_err(|error| PasteError::Failed(format!("unexpected answer: {error}")))?;
        let report: Report = serde_json::from_str(&report).unwrap_or_default();

        let keys = report.keys.as_deref().unwrap_or("?");
        let target = report.target.as_deref().unwrap_or("?");
        log::info!(Paste, "[{tag}] {keys} posted, {} characters, target {target}", text.chars().count());
        log::info!(
            Paste,
            "[{tag}] was on the clipboard before: {}",
            report.before.as_deref().unwrap_or("?")
        );
        if let Some(waited) = report.waited_ms.filter(|waited| *waited > 0) {
            log::info!(Paste, "[{tag}] waited {waited} ms for held keys to be released");
        }
        log::info!(Paste, "[{tag}] {}", report.restore.as_deref().unwrap_or("?"));
        log::info!(
            Paste,
            "[{tag}] paste done in {} ms, {} ms with the call",
            report.total_ms.unwrap_or(0),
            started.elapsed().as_millis()
        );
        Ok(())
    }
}

/// Says in plain words when the extension is not there to answer.
fn describe(error: &zbus::Error) -> String {
    if let zbus::Error::MethodError(name, message, _) = error {
        let name = name.as_str();
        if name.ends_with("UnknownObject") || name.ends_with("UnknownMethod") || name.ends_with("UnknownInterface") {
            return "the GNOME extension sensay-pill@sensay.app is not loaded".to_string();
        }
        return format!("{name}: {}", message.as_deref().unwrap_or(""));
    }
    error.to_string()
}
