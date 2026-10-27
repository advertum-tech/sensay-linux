// Settings file: $XDG_CONFIG_HOME/sensay/config.toml, mode 0600.
//
// Written with the defaults when missing, read at launch, and written again
// whenever the Settings window changes a value; the app picks the change up
// at once. Edits by hand count from the next launch. A value that
// is missing falls back to its default quietly; a value that is there but not
// one of the known ones falls back too and is written to the log, as
// Settings.activationKey().unknownValue is on macOS.

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::log;

/// Which of the three services dictation is sent to. Sensay Whisper and
/// Sensay Parakeet go through the Sensay API, which picks the model by the
/// request's `model` field; Local is a server the user runs, at the address
/// in the settings.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpeechService {
    Whisper,
    Parakeet,
    Local,
}

impl SpeechService {
    const FALLBACK: SpeechService = SpeechService::Parakeet;

    /// In the order the Settings window lists them.
    pub const ALL: [SpeechService; 3] = [SpeechService::Whisper, SpeechService::Parakeet, SpeechService::Local];

    pub fn title(self) -> &'static str {
        match self {
            SpeechService::Whisper => "Sensay Whisper",
            SpeechService::Parakeet => "Sensay Parakeet",
            SpeechService::Local => "Local",
        }
    }

    fn parse(raw: &str) -> Option<SpeechService> {
        match raw {
            "whisper" => Some(SpeechService::Whisper),
            "parakeet" => Some(SpeechService::Parakeet),
            "local" => Some(SpeechService::Local),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SpeechService::Whisper => "whisper",
            SpeechService::Parakeet => "parakeet",
            SpeechService::Local => "local",
        }
    }

    /// The value of the request's `model` field. A local server gets the
    /// request as before, without it.
    pub fn model(self) -> Option<&'static str> {
        match self {
            SpeechService::Whisper => Some("whisper"),
            SpeechService::Parakeet => Some("parakeet"),
            SpeechService::Local => None,
        }
    }

    /// One word for the log. Which of the three was asked is the whole
    /// question, as logName on macOS.
    pub fn log_name(self) -> &'static str {
        match self {
            SpeechService::Whisper => "sensay-whisper",
            SpeechService::Parakeet => "sensay-parakeet",
            SpeechService::Local => "local",
        }
    }
}

/// Where dictation is recorded from.
#[derive(Clone, PartialEq, Eq)]
pub enum Microphone {
    Auto,
    BuiltIn,
    /// A PulseAudio source name.
    Named(String),
}

impl Microphone {
    const FALLBACK: Microphone = Microphone::BuiltIn;
    const NAME_PREFIX: &'static str = "name:";

    fn parse(raw: &str) -> Option<Microphone> {
        match raw {
            "auto" => Some(Microphone::Auto),
            "built-in" => Some(Microphone::BuiltIn),
            _ => match raw.strip_prefix(Self::NAME_PREFIX) {
                Some(name) if !name.is_empty() => Some(Microphone::Named(name.to_string())),
                _ => None,
            },
        }
    }
}

impl fmt::Display for Microphone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Microphone::Auto => f.write_str("auto"),
            Microphone::BuiltIn => f.write_str("built-in"),
            Microphone::Named(name) => write!(f, "{}{name}", Self::NAME_PREFIX),
        }
    }
}

/// evdev names of the single keys the activation key can be. Caps Lock is
/// left out: it toggles case and nothing grabs it. F13-F24 are left out as
/// in the Windows version: users' keyboards do not have them.
pub const ACTIVATION_KEYS: [&str; 5] = [
    "KEY_RIGHTCTRL",
    "KEY_RIGHTALT",
    "KEY_RIGHTSHIFT",
    "KEY_SCROLLLOCK",
    "KEY_PAUSE",
];

/// What starts and stops dictation: the Ctrl+Super chord (either Ctrl with
/// either Super, the same as Ctrl+Win in the Windows version) or one key from
/// ACTIVATION_KEYS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActivationKey {
    CtrlSuper,
    Single(&'static str),
}

impl ActivationKey {
    const FALLBACK: ActivationKey = ActivationKey::CtrlSuper;
    const CTRL_SUPER: &'static str = "ctrl+super";

    /// Every choice, in the order the Settings window lists them.
    pub fn choices() -> Vec<ActivationKey> {
        std::iter::once(ActivationKey::CtrlSuper)
            .chain(ACTIVATION_KEYS.iter().copied().map(ActivationKey::Single))
            .collect()
    }

    pub fn title(self) -> String {
        match self {
            ActivationKey::CtrlSuper => "Ctrl+Super".to_string(),
            ActivationKey::Single("KEY_RIGHTCTRL") => "Right Ctrl".to_string(),
            ActivationKey::Single("KEY_RIGHTALT") => "Right Alt".to_string(),
            ActivationKey::Single("KEY_RIGHTSHIFT") => "Right Shift".to_string(),
            ActivationKey::Single("KEY_SCROLLLOCK") => "Scroll Lock".to_string(),
            ActivationKey::Single("KEY_PAUSE") => "Pause".to_string(),
            ActivationKey::Single(name) => name.trim_start_matches("KEY_").to_string(),
        }
    }

    fn parse(raw: &str) -> Option<ActivationKey> {
        if raw == Self::CTRL_SUPER {
            return Some(ActivationKey::CtrlSuper);
        }
        ACTIVATION_KEYS
            .iter()
            .copied()
            .find(|key| *key == raw)
            .map(ActivationKey::Single)
    }
}

impl fmt::Display for ActivationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActivationKey::CtrlSuper => f.write_str(Self::CTRL_SUPER),
            ActivationKey::Single(name) => f.write_str(name),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct Config {
    pub speech_service: SpeechService,
    /// Where Local is sent: an http or https address.
    pub local_address: String,
    pub activation_key: ActivationKey,
    pub microphone: Microphone,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            speech_service: SpeechService::FALLBACK,
            local_address: DEFAULT_LOCAL_ADDRESS.to_string(),
            activation_key: ActivationKey::FALLBACK,
            microphone: Microphone::FALLBACK,
        }
    }
}

/// Where Local is sent until another address is saved.
pub const DEFAULT_LOCAL_ADDRESS: &str = "http://127.0.0.1:9410";

/// The address trimmed, when it is an http or https URL with a host; None
/// for anything else, which is never stored.
pub fn web_address(raw: &str) -> Option<String> {
    let address = raw.trim();
    let url = reqwest::Url::parse(address).ok()?;
    let web = matches!(url.scheme(), "http" | "https") && url.host_str().is_some_and(|host| !host.is_empty());
    web.then(|| address.to_string())
}

/// The file as it is written, with these values.
fn render(config: &Config) -> String {
    let quote = |value: &str| toml::Value::String(value.to_string()).to_string();
    format!(
        r#"# Sensay settings. The Settings window writes here; edits by hand count
# from the next launch.

# Speech service: "whisper" (Sensay Whisper), "parakeet" (Sensay Parakeet)
# or "local".
speech_service = {}

# Where "local" is sent: an http or https address.
local_address = {}

# "ctrl+super" (hold either Ctrl with either Super), or one evdev key name:
# KEY_RIGHTCTRL, KEY_RIGHTALT, KEY_RIGHTSHIFT, KEY_SCROLLLOCK, KEY_PAUSE.
activation_key = {}

# "built-in", "auto" or "name:<PulseAudio source name>".
microphone = {}
"#,
        quote(config.speech_service.as_str()),
        quote(&config.local_address),
        quote(&config.activation_key.to_string()),
        quote(&config.microphone.to_string()),
    )
}

/// Writes the settings. Written beside the file and renamed over it, so a
/// crash never leaves half a file; mode 0600.
pub fn save(config: &Config) -> io::Result<()> {
    let path = path().ok_or_else(|| io::Error::other("neither XDG_CONFIG_HOME nor HOME is set"))?;
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let temporary = path.with_extension("toml.new");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(render(config).as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&temporary, &path)
}

// Every field is a raw value, so a wrong type is reported like an unknown
// value instead of failing the whole file.
#[derive(Deserialize)]
struct RawConfig {
    speech_service: Option<toml::Value>,
    local_address: Option<toml::Value>,
    activation_key: Option<toml::Value>,
    microphone: Option<toml::Value>,
}

/// `$XDG_CONFIG_HOME/sensay/config.toml`, or `~/.config/sensay/config.toml`
/// when the variable is unset, empty or relative. None without HOME.
pub fn path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("sensay").join("config.toml"))
}

/// Reads the settings, creating the file first when it is missing. Never
/// fails: whatever cannot be read falls back to the defaults, with a line in
/// the log.
pub fn load() -> Config {
    let Some(path) = path() else {
        log::error!(App, "settings: neither XDG_CONFIG_HOME nor HOME is set, using defaults");
        return Config::default();
    };

    match fs::read_to_string(&path) {
        Ok(text) => {
            log::info!(App, "settings: {}", path.display());
            tighten_mode(&path);
            parse(&text, &path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match create(&path) {
                Ok(()) => log::info!(App, "settings: {} (created with defaults)", path.display()),
                Err(error) => log::error!(
                    App,
                    "settings: could not create {}: {error}; using defaults",
                    path.display()
                ),
            }
            Config::default()
        }
        Err(error) => {
            log::error!(App, "settings: could not read {}: {error}; using defaults", path.display());
            Config::default()
        }
    }
}

fn create(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(render(&Config::default()).as_bytes())
}

/// Keeps the file readable by its owner only.
fn tighten_mode(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else { return };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        return;
    }
    match fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        Ok(()) => log::warning!(App, "settings: mode was {mode:o}, set to 600"),
        Err(error) => log::warning!(App, "settings: mode is {mode:o}, could not set 600: {error}"),
    }
}

fn parse(text: &str, path: &Path) -> Config {
    let raw: RawConfig = match toml::from_str(text) {
        Ok(raw) => raw,
        Err(error) => {
            log::error!(
                App,
                "settings: {} is not valid TOML, using defaults: {}",
                path.display(),
                error.message()
            );
            return Config::default();
        }
    };

    let defaults = Config::default();
    Config {
        speech_service: field(
            "speech_service",
            raw.speech_service,
            SpeechService::parse,
            defaults.speech_service,
            SpeechService::as_str(defaults.speech_service),
        ),
        local_address: field(
            "local_address",
            raw.local_address,
            web_address,
            defaults.local_address.clone(),
            DEFAULT_LOCAL_ADDRESS,
        ),
        activation_key: field(
            "activation_key",
            raw.activation_key,
            ActivationKey::parse,
            defaults.activation_key,
            &defaults.activation_key.to_string(),
        ),
        microphone: field(
            "microphone",
            raw.microphone,
            Microphone::parse,
            defaults.microphone.clone(),
            &defaults.microphone.to_string(),
        ),
    }
}

/// Reads one field. A value that is not a string, or a string `parse`
/// rejects, gives `fallback` and a log line naming what was stored.
fn field<T>(
    name: &str,
    value: Option<toml::Value>,
    parse: impl Fn(&str) -> Option<T>,
    fallback: T,
    fallback_name: &str,
) -> T {
    let Some(value) = value else {
        return fallback;
    };
    if let Some(parsed) = value.as_str().and_then(&parse) {
        return parsed;
    }
    log::error!(App, "settings: unknown {name} {value}, using {fallback_name}");
    fallback
}

#[cfg(test)]
mod tests {
    use super::web_address;

    #[test]
    fn only_http_and_https_addresses_are_kept() {
        assert_eq!(web_address(" http://127.0.0.1:9410 ").as_deref(), Some("http://127.0.0.1:9410"));
        assert_eq!(web_address("https://asr.example.com/base").as_deref(), Some("https://asr.example.com/base"));
        assert_eq!(web_address("127.0.0.1:9410"), None);
        assert_eq!(web_address("localhost:9410"), None);
        assert_eq!(web_address("ftp://example.com"), None);
        assert_eq!(web_address("http://"), None);
        assert_eq!(web_address(""), None);
    }
}
