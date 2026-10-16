// The activation key: one event when it goes down, one when it comes up, in
// any window, without Sensay having the focus. Auto-repeat while it is held
// gives nothing.
//
// The only path so far is evdev. The GlobalShortcuts portal joins as a second
// one in step 9, behind the same type.

mod evdev;

use tokio::sync::mpsc::UnboundedSender;

use crate::config::ActivationKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Down,
    Up,
}

pub struct Hotkey {
    listener: evdev::EvdevListener,
}

impl Hotkey {
    /// Name of the path the key is read through, for the `backends:` line.
    pub fn backend_name() -> &'static str {
        "evdev"
    }

    /// Starts listening for `key`. Always returns: without access to the
    /// keyboards the reason goes to the log and the app runs on without the
    /// key.
    pub fn start(key: ActivationKey, events: UnboundedSender<HotkeyEvent>) -> Hotkey {
        Hotkey {
            listener: evdev::EvdevListener::start(key, events),
        }
    }

    /// Stops the listener and waits for its thread to end.
    pub fn stop(self) {
        self.listener.stop();
    }
}
