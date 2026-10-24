// The pill on GNOME: drawn by the GNOME Shell extension sensay-pill@sensay.app
// (gnome/ in this repository) from signals on the session bus.
//
//   object    /com/advertumtech/Sensay/Pill
//   interface com.advertumtech.Sensay.Pill
//   signal    StateChanged(s)   "idle", "listening" or "waiting"
//   signal    Level(d)          0...1 on the meter's floating scale, 60 times
//                               a second while listening
//   method    GetState() -> s   for an extension that starts after the app
//
// The extension shows the pill while this app owns com.advertumtech.Sensay
// and hides it when the name goes away, so quitting needs no signal.

use std::collections::HashMap;

use tokio::sync::watch;
use zbus::Connection;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use super::PillState;
use crate::error_presenter::ErrorPresenter;
use crate::log;

const PATH: &str = "/com/advertumtech/Sensay/Pill";
const EXTENSION_UUID: &str = "sensay-pill@sensay.app";

// What the user is told when the extension does not run: without it there
// is no pill and no paste.
const NOT_INSTALLED: &str =
    "The GNOME extension is not installed. Run sensay setup in a terminal, then log out and back in.";
const SWITCHED_OFF: &str = "The GNOME extension is switched off. Run sensay setup in a terminal to switch it on.";
const OUT_OF_DATE: &str = "The GNOME extension does not support this version of GNOME.";
const AFTER_LOGIN: &str = "The GNOME extension starts after you log out and back in.";

struct PillInterface {
    current: watch::Receiver<PillState>,
}

#[zbus::interface(name = "com.advertumtech.Sensay.Pill")]
impl PillInterface {
    fn get_state(&self) -> String {
        self.current.borrow().as_str().to_string()
    }

    #[zbus(signal)]
    async fn state_changed(emitter: &SignalEmitter<'_>, state: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn level(emitter: &SignalEmitter<'_>, level: f64) -> zbus::Result<()>;
}

/// Serves the pill object on the session bus. Returns what the signals are
/// sent with, and where GetState reads the current state.
pub async fn serve(
    connection: &Connection,
    errors: &ErrorPresenter,
) -> zbus::Result<(SignalEmitter<'static>, watch::Sender<PillState>)> {
    let (current_tx, current) = watch::channel(PillState::Idle);
    connection
        .object_server()
        .at(PATH, PillInterface { current })
        .await?;
    if let Some(message) = report_extension(connection).await {
        errors.show(&message);
    }
    let emitter = SignalEmitter::new(connection, PATH)?.into_owned();
    Ok((emitter, current_tx))
}

pub async fn emit_state(emitter: &SignalEmitter<'_>, state: PillState) -> zbus::Result<()> {
    PillInterface::state_changed(emitter, state.as_str()).await
}

pub async fn emit_level(emitter: &SignalEmitter<'_>, level: f64) -> zbus::Result<()> {
    PillInterface::level(emitter, level).await
}

/// One line on whether GNOME Shell has the extension and what state it is
/// in, as GNOME's ExtensionState numbers it; the message for the user when
/// it is not running.
async fn report_extension(connection: &Connection) -> Option<String> {
    let reply = connection
        .call_method(
            Some("org.gnome.Shell.Extensions"),
            "/org/gnome/Shell/Extensions",
            Some("org.gnome.Shell.Extensions"),
            "GetExtensionInfo",
            &(EXTENSION_UUID,),
        )
        .await;
    let info: HashMap<String, OwnedValue> = match reply.and_then(|reply| Ok(reply.body().deserialize()?)) {
        Ok(info) => info,
        Err(error) => {
            log::warning!(Pill, "pill: cannot ask GNOME Shell about {EXTENSION_UUID}: {error}");
            return None;
        }
    };
    let Some(state) = info.get("state").and_then(|value| f64::try_from(value).ok()) else {
        log::warning!(
            Pill,
            "pill: GNOME extension {EXTENSION_UUID} is not installed, the pill stays hidden"
        );
        return Some(NOT_INSTALLED.to_string());
    };
    let name = match state as i64 {
        1 => "active",
        2 => "inactive",
        3 => "error",
        4 => "out of date",
        6 => "installed, loads after the next login",
        7 => "deactivating",
        8 => "activating",
        _ => "unknown",
    };
    let error = info
        .get("error")
        .and_then(|value| String::try_from(value.clone()).ok())
        .filter(|text| !text.is_empty());
    match &error {
        Some(error) => log::warning!(Pill, "pill: GNOME extension {EXTENSION_UUID}: {name}, {error}"),
        None if state as i64 == 1 => log::info!(Pill, "pill: GNOME extension {EXTENSION_UUID}: {name}"),
        None => log::warning!(Pill, "pill: GNOME extension {EXTENSION_UUID}: {name}"),
    }
    match state as i64 {
        2 => Some(SWITCHED_OFF.to_string()),
        3 => Some(format!(
            "The GNOME extension failed to start: {}",
            error.as_deref().unwrap_or("no reason given")
        )),
        4 => Some(OUT_OF_DATE.to_string()),
        6 => Some(AFTER_LOGIN.to_string()),
        _ => None,
    }
}
