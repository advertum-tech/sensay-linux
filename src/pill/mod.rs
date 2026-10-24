// The pill: a thin capsule at the bottom of the screen while the app runs, a
// wide one with the level meter while the key is held, and the same with a
// spinner while the text is on its way. The counterpart of PillPanel.swift
// and PillView.swift.
//
// What the pill shows is decided here; where it is drawn depends on the
// desktop, chosen once at launch:
//
//   GNOME on Wayland      the GNOME Shell extension, from D-Bus signals
//                         (gnome.rs): GNOME has no protocol for such a window
//   Wayland with
//   layer-shell           a layer-shell surface Sensay draws (native/)
//   X11                   an override-redirect window Sensay draws (native/)
//   anything else         no pill; dictation works without it

mod gnome;
mod meter;
mod native;
mod render;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::time::{Interval, MissedTickBehavior};
use zbus::Connection;
use zbus::object_server::SignalEmitter;

use crate::desktop::is_gnome_wayland;
use crate::error_presenter::ErrorPresenter;
use crate::log;
use meter::LevelScale;

/// How often the level goes out while listening: the screen's frame rate.
const LEVEL_PERIOD: Duration = Duration::from_micros(16_667);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillState {
    /// The thin resting capsule.
    Idle,
    /// The key is down: the level meter.
    Listening,
    /// The key is up and the text is on its way: the meter settles and a
    /// spinner turns.
    Waiting,
}

impl PillState {
    pub fn as_str(self) -> &'static str {
        match self {
            PillState::Idle => "idle",
            PillState::Listening => "listening",
            PillState::Waiting => "waiting",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    GnomeExtension,
    Native(native::Kind),
    None,
}

/// Chosen once: the desktop does not change while the app runs.
fn backend() -> Backend {
    static BACKEND: OnceLock<Backend> = OnceLock::new();
    *BACKEND.get_or_init(|| {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
        let x11 = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
        if is_gnome_wayland() {
            Backend::GnomeExtension
        } else if wayland {
            if native::layer_shell_available() {
                Backend::Native(native::Kind::LayerShell)
            } else {
                Backend::None
            }
        } else if x11 {
            Backend::Native(native::Kind::X11)
        } else {
            Backend::None
        }
    })
}

/// Where the states and levels go.
enum Output {
    Gnome {
        emitter: SignalEmitter<'static>,
        current: watch::Sender<PillState>,
    },
    Native(native::NativePill),
}

impl Output {
    async fn state(&self, state: PillState) -> Result<(), String> {
        match self {
            Output::Gnome { emitter, current } => {
                current.send_replace(state);
                gnome::emit_state(emitter, state).await.map_err(|error| error.to_string())
            }
            Output::Native(pill) => pill.send(native::Message::State(state)),
        }
    }

    async fn level(&self, level: f64) -> Result<(), String> {
        match self {
            Output::Gnome { emitter, .. } => gnome::emit_level(emitter, level).await.map_err(|error| error.to_string()),
            Output::Native(pill) => pill.send(native::Message::Level(level)),
        }
    }
}

pub struct Pill {
    states: Option<mpsc::UnboundedSender<PillState>>,
}

impl Pill {
    /// Name of the path the pill is drawn through, for the `backends:` line.
    pub fn backend_name() -> &'static str {
        match backend() {
            Backend::GnomeExtension => "gnome-extension",
            Backend::Native(native::Kind::LayerShell) => "layer-shell",
            Backend::Native(native::Kind::X11) => "x11",
            Backend::None => "none",
        }
    }

    /// Starts the pill for this desktop. Always returns: without a way to
    /// draw it the reason goes to the log and dictation works without it.
    /// On GNOME the user is told when the extension is not running.
    pub async fn start(connection: Option<&Connection>, level: watch::Receiver<f32>, errors: &ErrorPresenter) -> Pill {
        let output = match backend() {
            Backend::GnomeExtension => {
                let Some(connection) = connection else {
                    log::warning!(Pill, "pill: no session bus, no pill");
                    return Pill { states: None };
                };
                match gnome::serve(connection, errors).await {
                    Ok((emitter, current)) => Output::Gnome { emitter, current },
                    Err(error) => {
                        log::error!(Pill, "pill: could not serve the pill on the session bus: {error}");
                        return Pill { states: None };
                    }
                }
            }
            Backend::Native(kind) => match tokio::task::spawn_blocking(move || native::NativePill::start(kind)).await {
                Ok(Ok(pill)) => Output::Native(pill),
                Ok(Err(reason)) => {
                    log::error!(Pill, "pill: {reason}; no pill");
                    return Pill { states: None };
                }
                Err(error) => {
                    log::error!(Pill, "pill: {error}; no pill");
                    return Pill { states: None };
                }
            },
            Backend::None => {
                log::info!(Pill, "pill: no way to draw it on this desktop, dictation works without it");
                return Pill { states: None };
            }
        };
        let (states, states_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(output, states_rx, level));
        Pill { states: Some(states) }
    }

    pub fn set(&self, state: PillState) {
        if let Some(states) = &self.states {
            // The task ends only with the app.
            let _ = states.send(state);
        }
    }
}

/// One task sends everything, so the pill sees the states in order. While
/// listening it also sends the level on the meter's scale, once a frame.
async fn run(output: Output, mut states: mpsc::UnboundedReceiver<PillState>, level: watch::Receiver<f32>) {
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
                log::info!(Pill, "pill {}", next.as_str());
                let sent = output.state(next).await;
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
                let sent = output.level(value).await;
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
fn note_send(result: Result<(), String>, failed: &mut bool) {
    match result {
        Ok(()) if *failed => {
            *failed = false;
            log::info!(Pill, "pill: sending works again");
        }
        Ok(()) => {}
        Err(error) if !*failed => {
            *failed = true;
            log::warning!(Pill, "pill: could not send: {error}");
        }
        Err(_) => {}
    }
}
