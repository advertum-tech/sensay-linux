// The pill: a thin capsule at the bottom of the screen under the pointer
// while the app runs, a wide one with the level meter while the key is held,
// and the same with a spinner while the text is on its way. The counterpart
// of PillPanel.swift and PillView.swift.
//
// What the pill shows is decided here. Where it is drawn depends on the
// desktop. GNOME on Wayland has no protocol for such a window, so the GNOME
// Shell extension sensay-pill@sensay.app draws it from signals this app
// sends on the session bus (gnome.rs). Other desktops get their own drawing
// in step 7; until then they run without a pill.

mod gnome;
mod meter;

use tokio::sync::watch;
use zbus::Connection;

use crate::desktop::is_gnome_wayland;
use crate::log;

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

pub struct Pill {
    gnome: Option<gnome::GnomePill>,
}

impl Pill {
    /// Name of the path the pill is drawn through, for the `backends:` line.
    pub fn backend_name() -> &'static str {
        if is_gnome_wayland() {
            "gnome-extension"
        } else {
            "none"
        }
    }

    /// Starts the pill for this desktop. Always returns: without a way to
    /// draw it the reason goes to the log and dictation works without it.
    pub async fn start(connection: Option<&Connection>, level: watch::Receiver<f32>) -> Pill {
        if !is_gnome_wayland() {
            log::info!(Pill, "pill: not a GNOME Wayland session, no pill on this desktop yet");
            return Pill { gnome: None };
        }
        let Some(connection) = connection else {
            log::warning!(Pill, "pill: no session bus, no pill");
            return Pill { gnome: None };
        };
        match gnome::GnomePill::start(connection.clone(), level).await {
            Ok(pill) => Pill { gnome: Some(pill) },
            Err(error) => {
                log::error!(Pill, "pill: could not serve the pill on the session bus: {error}");
                Pill { gnome: None }
            }
        }
    }

    pub fn set(&self, state: PillState) {
        if let Some(gnome) = &self.gnome {
            gnome.set(state);
        }
    }
}
