// Tray icon and its menu (StatusNotifierItem through ksni).
//
// The tray is the main way in: the app usually has no window open. Left click
// opens the history, as clicking the Dock icon does on macOS.
//
// Without a tray host (GNOME without the AppIndicator extension, or a session
// that is still starting) the icon is retried 10 times with the pause doubled
// each time, as whisrs does, and then the app goes on without it.

use std::time::Duration;

use ksni::menu::StandardItem;
use ksni::{Handle, Icon, MenuItem, OfflineReason, TrayMethods};
use tokio::sync::mpsc::UnboundedSender;

use crate::Command;
use crate::log;

const MAX_ATTEMPTS: u32 = 10;
const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(10);

/// Brand orange, #FF4122.
const BRAND: [u8; 3] = [0xFF, 0x41, 0x22];
const ICON_SIZES: [i32; 4] = [16, 22, 32, 48];

pub struct SensayTray {
    commands: UnboundedSender<Command>,
    last_error: String,
}

impl SensayTray {
    fn send(&self, command: Command) {
        log::info!(App, "tray: {command:?} clicked");
        // ksni calls menu callbacks on its own task and asks them not to block;
        // an unbounded send never does.
        let _ = self.commands.send(command);
    }
}

impl ksni::Tray for SensayTray {
    fn id(&self) -> String {
        "sensay".to_string()
    }

    fn title(&self) -> String {
        "Sensay".to_string()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::OpenHistory);
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        ICON_SIZES.iter().map(|&size| circle(size)).collect()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Sensay".to_string(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Last error: {}", self.last_error),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "History".to_string(),
                activate: Box::new(|tray: &mut Self| tray.send(Command::OpenHistory)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Settings…".to_string(),
                activate: Box::new(|tray: &mut Self| tray.send(Command::OpenSettings)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit Sensay".to_string(),
                activate: Box::new(|tray: &mut Self| tray.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_online(&self) {
        log::info!(App, "tray: StatusNotifierWatcher is back");
    }

    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        log::warning!(App, "tray: StatusNotifierWatcher went away ({reason:?}), waiting for it");
        // Keep the service: the icon comes back when the watcher does.
        true
    }
}

/// Puts the icon up, retrying while there is no tray host. None when every
/// attempt failed; the app then runs without an icon.
pub async fn start(commands: UnboundedSender<Command>) -> Option<Handle<SensayTray>> {
    let mut pause = FIRST_PAUSE;
    for attempt in 1..=MAX_ATTEMPTS {
        let tray = SensayTray {
            commands: commands.clone(),
            last_error: "none".to_string(),
        };
        match tray.spawn().await {
            Ok(handle) => {
                log::info!(App, "tray: icon up (attempt {attempt})");
                return Some(handle);
            }
            Err(error) if attempt == MAX_ATTEMPTS => {
                log::warning!(
                    App,
                    "tray: no icon after {MAX_ATTEMPTS} attempts ({error}), running without it"
                );
            }
            Err(error) => {
                log::info!(
                    App,
                    "tray: not available (attempt {attempt}/{MAX_ATTEMPTS}): {error}; retrying in {pause:?}"
                );
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(LONGEST_PAUSE);
            }
        }
    }
    None
}

/// A filled brand-colored circle, `size` pixels square, with a soft edge.
/// Placeholder until the real icons (step 8).
fn circle(size: i32) -> Icon {
    // 4x4 samples per pixel give the edge its antialiasing.
    const SAMPLES: i32 = 4;
    let center = size as f32 / 2.0;
    let radius = size as f32 * 0.4375;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut inside = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32 - center;
                    let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32 - center;
                    if px * px + py * py <= radius * radius {
                        inside += 1;
                    }
                }
            }
            let alpha = (inside * 255 / (SAMPLES * SAMPLES)) as u8;
            // ARGB32, network byte order, straight alpha.
            data.extend_from_slice(&[alpha, BRAND[0], BRAND[1], BRAND[2]]);
        }
    }
    Icon {
        width: size,
        height: size,
        data,
    }
}
