// Logging. Every line goes to journald and to stderr, but for stderr that is
// the journal already: then once.
//
// Read with:
//   journalctl --user -t sensay --since -10m -o cat
//   journalctl --user -t sensay SENSAY_CATEGORY=hotkey
//
// The category is a field of its own, as the category of os.Logger is on
// macOS. Call sites use the macros below, which add it:
//   log::info!(Hotkey, "KEY_DOWN");
//
// Lines from this crate are kept from INFO up (SENSAY_DEBUG=1 adds DEBUG);
// lines from libraries only from WARN up.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Journal identifier, the value `journalctl -t` filters on.
const IDENTIFIER: &str = "sensay";

/// Values of the SENSAY_CATEGORY field.
#[derive(Clone, Copy)]
pub enum Category {
    App,
    Hotkey,
    Audio,
    Pill,
    Asr,
    Paste,
    Storage,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::App => "app",
            Category::Hotkey => "hotkey",
            Category::Audio => "audio",
            Category::Pill => "pill",
            Category::Asr => "asr",
            Category::Paste => "paste",
            Category::Storage => "storage",
        }
    }
}

// The field is named sensay_category and the journald layer runs without a
// field prefix, so it arrives in the journal as SENSAY_CATEGORY.
macro_rules! info {
    ($cat:ident, $($arg:tt)+) => {
        ::tracing::info!(sensay_category = $crate::log::Category::$cat.as_str(), $($arg)+)
    };
}

macro_rules! warning {
    ($cat:ident, $($arg:tt)+) => {
        ::tracing::warn!(sensay_category = $crate::log::Category::$cat.as_str(), $($arg)+)
    };
}

macro_rules! error {
    ($cat:ident, $($arg:tt)+) => {
        ::tracing::error!(sensay_category = $crate::log::Category::$cat.as_str(), $($arg)+)
    };
}

macro_rules! debug {
    ($cat:ident, $($arg:tt)+) => {
        ::tracing::debug!(sensay_category = $crate::log::Category::$cat.as_str(), $($arg)+)
    };
}

pub(crate) use {debug, error, info, warning};

/// Installs the journald and stderr outputs. When journald cannot be reached
/// the app logs to stderr alone and says so on the first line.
pub fn init() {
    let own_level = if std::env::var_os("SENSAY_DEBUG").is_some() {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    let filter = Targets::new()
        .with_target(env!("CARGO_CRATE_NAME"), own_level)
        .with_default(LevelFilter::WARN);

    let (journald, journald_error) = match tracing_journald::layer() {
        Ok(layer) => (
            Some(
                layer
                    .with_syslog_identifier(IDENTIFIER.to_string())
                    .with_field_prefix(None),
            ),
            None,
        ),
        Err(error) => (None, Some(error)),
    };

    let stderr = (journald.is_none() || !stderr_is_journal())
        .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));

    tracing_subscriber::registry()
        .with(journald)
        .with(stderr)
        .with(filter)
        .init();

    if let Some(error) = journald_error {
        warning!(App, "journald is not reachable ({error}), logging to stderr only");
    }

    // A panic goes to the journal too, not only to the terminal.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!(App, "panic: {info}");
        default_hook(info);
    }));
}

/// Whether stderr is the journal, as for an app the desktop started: systemd
/// then sets JOURNAL_STREAM to that stream's device and inode
/// (systemd.exec(5)). A terminal started that way passes the variable on to
/// what runs in it, hence the comparison.
fn stderr_is_journal() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(stream) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Some((device, inode)) = stream.to_str().and_then(|stream| stream.split_once(':')) else {
        return false;
    };
    let Ok(stderr) = std::fs::metadata("/proc/self/fd/2") else {
        return false;
    };
    device.parse() == Ok(stderr.dev()) && inode.parse() == Ok(stderr.ino())
}
