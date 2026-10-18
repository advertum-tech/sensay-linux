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
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::time::{Interval, MissedTickBehavior};
use zbus::Connection;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use super::PillState;
use super::meter::LevelScale;
use crate::log;

const PATH: &str = "/com/advertumtech/Sensay/Pill";
const EXTENSION_UUID: &str = "sensay-pill@sensay.app";
/// How often the level goes out while listening: the screen's frame rate.
const LEVEL_PERIOD: Duration = Duration::from_micros(16_667);

/// XDG_CURRENT_DESKTOP names GNOME (Ubuntu says "ubuntu:GNOME") and the
/// session is Wayland. On GNOME under X11 the X11 pill of step 7 applies.
pub fn is_gnome_wayland() -> bool {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    let gnome = std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|desktops| desktops.split(':').any(|desktop| desktop.eq_ignore_ascii_case("GNOME")));
    wayland && gnome
}

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

pub struct GnomePill {
    states: mpsc::UnboundedSender<PillState>,
}

impl GnomePill {
    pub async fn start(connection: Connection, level: watch::Receiver<f32>) -> zbus::Result<GnomePill> {
        let (current_tx, current) = watch::channel(PillState::Idle);
        connection
            .object_server()
            .at(PATH, PillInterface { current })
            .await?;
        report_extension(&connection).await;

        let emitter = SignalEmitter::new(&connection, PATH)?.into_owned();
        let (states, states_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(emitter, states_rx, current_tx, level));
        Ok(GnomePill { states })
    }

    pub fn set(&self, state: PillState) {
        // The task ends only with the app.
        let _ = self.states.send(state);
    }
}

/// One task sends everything, so the extension sees the states in order.
async fn run(
    emitter: SignalEmitter<'static>,
    mut states: mpsc::UnboundedReceiver<PillState>,
    current: watch::Sender<PillState>,
    level: watch::Receiver<f32>,
) {
    let mut state = PillState::Idle;
    let mut scale = LevelScale::new();
    let mut ticker: Option<Interval> = None;
    let mut session_start = Instant::now();
    let mut last_tick = Instant::now();
    let mut send_failed = false;

    loop {
        tokio::select! {
            next = states.recv() => {
                let Some(next) = next else { return };
                if next == state {
                    continue;
                }
                state = next;
                current.send_replace(next);
                log::info!(Pill, "pill {}", next.as_str());
                let sent = PillInterface::state_changed(&emitter, next.as_str()).await;
                note_send(sent, &mut send_failed);

                if next == PillState::Listening {
                    scale.start_session();
                    session_start = Instant::now();
                    last_tick = session_start;
                    let mut interval = tokio::time::interval(LEVEL_PERIOD);
                    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    ticker = Some(interval);
                } else {
                    ticker = None;
                }
            }
            _ = tick(&mut ticker) => {
                let now = Instant::now();
                let dt = (now - last_tick).as_secs_f64().clamp(0.001, 0.1);
                last_tick = now;
                let rms = f64::from(*level.borrow());
                let value = scale.scale(rms, dt, (now - session_start).as_secs_f64());
                let sent = PillInterface::level(&emitter, value).await;
                note_send(sent, &mut send_failed);
            }
        }
    }
}

/// Resolves on the next tick; never, while there is no ticker.
async fn tick(ticker: &mut Option<Interval>) {
    match ticker {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// One line for the first failure in a row, one when sending works again.
fn note_send(result: zbus::Result<()>, failed: &mut bool) {
    match result {
        Ok(()) if *failed => {
            *failed = false;
            log::info!(Pill, "pill: sending to the extension works again");
        }
        Ok(()) => {}
        Err(error) if !*failed => {
            *failed = true;
            log::warning!(Pill, "pill: could not send to the extension: {error}");
        }
        Err(_) => {}
    }
}

/// One line on whether GNOME Shell has the extension and what state it is
/// in, as GNOME's ExtensionState numbers it.
async fn report_extension(connection: &Connection) {
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
            return;
        }
    };
    let Some(state) = info.get("state").and_then(|value| f64::try_from(value).ok()) else {
        log::warning!(
            Pill,
            "pill: GNOME extension {EXTENSION_UUID} is not installed, the pill stays hidden"
        );
        return;
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
    match error {
        Some(error) => log::warning!(Pill, "pill: GNOME extension {EXTENSION_UUID}: {name}, {error}"),
        None if state as i64 == 1 => log::info!(Pill, "pill: GNOME extension {EXTENSION_UUID}: {name}"),
        None => log::warning!(Pill, "pill: GNOME extension {EXTENSION_UUID}: {name}"),
    }
}
