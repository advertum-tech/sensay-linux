// What the user sees when something goes wrong, as ErrorPresenter.swift on
// macOS: a desktop notification (org.freedesktop.Notifications) where macOS
// shows a panel by the pill, and the same text as "Last error" in the tray
// menu, where a notification that faded can still be read. Each notification
// replaces the one before, as the panel shows one message at a time.
//
// The wording sits with each error (its message()); this only shows it. The
// first sentence becomes the notification's title and the rest its body,
// which desktops wrap where they cut a long title.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use zbus::Connection;
use zbus::zvariant::Value;

use crate::log;

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
/// The desktop file's name without .desktop: desktops take the app's name
/// and icon for the notification from it.
const DESKTOP_ENTRY: &str = "com.advertumtech.Sensay";

#[derive(Clone)]
pub struct ErrorPresenter {
    last: Arc<watch::Sender<Option<String>>>,
    notifications: mpsc::UnboundedSender<String>,
}

impl ErrorPresenter {
    /// Notifications go out one at a time from a task of their own, so a
    /// notification server that is slow to start delays nobody.
    pub fn start(connection: Option<Connection>) -> ErrorPresenter {
        let (notifications, queue) = mpsc::unbounded_channel();
        if let Some(connection) = connection {
            tokio::spawn(notify_each(connection, queue));
        }
        ErrorPresenter {
            last: Arc::new(watch::Sender::new(None)),
            notifications,
        }
    }

    /// The last message, None before the first: the tray menu shows it.
    pub fn last_error(&self) -> watch::Receiver<Option<String>> {
        self.last.subscribe()
    }

    pub fn show(&self, message: &str) {
        log::info!(App, "error shown: {message}");
        self.last.send_replace(Some(message.to_string()));
        // Without a session bus there is no task to receive it.
        let _ = self.notifications.send(message.to_string());
    }
}

async fn notify_each(connection: Connection, mut queue: mpsc::UnboundedReceiver<String>) {
    // The id of the notification on screen; 0 asks for a new one.
    let mut shown = 0;
    while let Some(message) = queue.recv().await {
        match notify(&connection, shown, &message).await {
            Ok(id) => shown = id,
            Err(error) => log::warning!(App, "notification not shown: {error}"),
        }
    }
}

async fn notify(connection: &Connection, replaces: u32, message: &str) -> zbus::Result<u32> {
    let (title, body) = split(message);
    let hints = HashMap::from([("desktop-entry", Value::from(DESKTOP_ENTRY))]);
    let actions: Vec<&str> = Vec::new();
    // -1: the desktop decides how long it stays.
    let reply = connection
        .call_method(
            Some(NOTIFICATIONS),
            NOTIFICATIONS_PATH,
            Some(NOTIFICATIONS),
            "Notify",
            &("Sensay", replaces, DESKTOP_ENTRY, title, body, actions, hints, -1i32),
        )
        .await?;
    reply.body().deserialize()
}

/// The first sentence and the rest.
fn split(message: &str) -> (&str, &str) {
    match message.find(". ") {
        Some(end) => (&message[..=end], &message[end + 2..]),
        None => (message, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn splits_off_the_first_sentence() {
        assert_eq!(
            split("Keyboard access is off. Run sensay setup in a terminal to turn it on."),
            ("Keyboard access is off.", "Run sensay setup in a terminal to turn it on.")
        );
        assert_eq!(
            split("The speech service is not running at http://192.168.50.239:9000."),
            ("The speech service is not running at http://192.168.50.239:9000.", "")
        );
    }
}
