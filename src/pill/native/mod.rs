// The pill where Sensay draws it itself: a layer-shell surface on the Wayland
// desktops that have the protocol (KDE, Sway, Hyprland, labwc, COSMIC), an
// override-redirect window on X11. Both run on a thread of their own with
// their own connection to the display, as whisrs does, and draw with
// render.rs.

mod wayland;
mod x11;

use std::sync::mpsc;
use std::time::Duration;

use smithay_client_toolkit::reexports::calloop::channel as calloop_channel;

use super::PillState;
use crate::log;

/// How long the drawing thread may take to show the pill.
const START_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum Message {
    State(PillState),
    /// 0...1 on the meter's floating scale.
    Level(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    LayerShell,
    X11,
}

/// Whether the Wayland compositor offers layer-shell. Asks it over a
/// connection of its own, closed right after.
pub fn layer_shell_available() -> bool {
    wayland::available()
}

enum Sender {
    Wayland(calloop_channel::Sender<Message>),
    X11(mpsc::Sender<Message>),
}

pub struct NativePill {
    sender: Sender,
}

impl NativePill {
    /// Starts the drawing thread and waits until the pill is up. Blocking:
    /// call from a blocking context.
    pub fn start(kind: Kind) -> Result<NativePill, String> {
        let (ready_tx, ready) = mpsc::channel();
        let sender = match kind {
            Kind::LayerShell => {
                let (sender, messages) = calloop_channel::channel();
                spawn(move || wayland::run(messages, ready_tx))?;
                Sender::Wayland(sender)
            }
            Kind::X11 => {
                let (sender, messages) = mpsc::channel();
                spawn(move || x11::run(messages, ready_tx))?;
                Sender::X11(sender)
            }
        };
        match ready.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok(NativePill { sender }),
            Ok(Err(reason)) => Err(reason),
            Err(_) => Err(format!("the pill did not come up in {} s", START_TIMEOUT.as_secs())),
        }
    }

    pub fn send(&self, message: Message) -> Result<(), String> {
        let sent = match &self.sender {
            Sender::Wayland(sender) => sender.send(message).is_ok(),
            Sender::X11(sender) => sender.send(message).is_ok(),
        };
        if sent { Ok(()) } else { Err("the pill thread has stopped".to_string()) }
    }
}

fn spawn(body: impl FnOnce() + Send + 'static) -> Result<(), String> {
    std::thread::Builder::new()
        .name("pill".to_string())
        .spawn(body)
        .map(|_| ())
        .map_err(|error| {
            log::error!(Pill, "pill: could not start the drawing thread: {error}");
            error.to_string()
        })
}
