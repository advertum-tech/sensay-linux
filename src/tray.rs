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
use tiny_skia::{FillRule, LineCap, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::Command;
use crate::log;

const MAX_ATTEMPTS: u32 = 10;
const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(10);

/// Brand orange, #FF4122.
const BRAND: [u8; 3] = [0xFF, 0x41, 0x22];
/// Pixel sizes handed to the tray host; it picks the one that fits its panel.
const ICON_SIZES: [i32; 6] = [16, 22, 24, 32, 48, 64];

pub struct SensayTray {
    commands: UnboundedSender<Command>,
    /// From the ErrorPresenter; None until the first error.
    last_error: Option<String>,
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
        ICON_SIZES.iter().map(|&size| logo(size)).collect()
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
                label: format!("Last error: {}", self.last_error.as_deref().unwrap_or("none")),
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
                label: "About Sensay".to_string(),
                activate: Box::new(|tray: &mut Self| tray.send(Command::OpenAbout)),
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
pub async fn start(
    commands: UnboundedSender<Command>,
    mut last_error: watch::Receiver<Option<String>>,
) -> Option<Handle<SensayTray>> {
    let mut pause = FIRST_PAUSE;
    for attempt in 1..=MAX_ATTEMPTS {
        let tray = SensayTray {
            commands: commands.clone(),
            last_error: last_error.borrow_and_update().clone(),
        };
        match tray.spawn().await {
            Ok(handle) => {
                log::info!(App, "tray: icon up (attempt {attempt})");
                tokio::spawn(follow_last_error(handle.clone(), last_error));
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

/// Puts each new error message into the menu, until the icon goes away.
async fn follow_last_error(handle: Handle<SensayTray>, mut last_error: watch::Receiver<Option<String>>) {
    while last_error.changed().await.is_ok() {
        let message = last_error.borrow_and_update().clone();
        if handle.update(|tray| tray.last_error = message).await.is_none() {
            return;
        }
    }
}

/// The Sensay logo, `size` pixels square: the brand-orange rounded square
/// with seven white bars of docs/windows/icon-source/logo.svg (viewBox 120),
/// drawn for each size so it stays sharp. In colour rather than one-colour as
/// on macOS: a tray icon cannot follow the panel being light or dark.
fn logo(size: i32) -> Icon {
    // x, top and bottom of each bar, from logo.svg.
    const BARS: [(f32, f32, f32); 7] = [
        (21.0, 51.2759, 68.4124),
        (33.7953, 34.1394, 85.7774),
        (46.819, 43.0504, 76.6379),
        (59.6143, 25.0, 94.9168),
        (72.4096, 43.2789, 76.8664),
        (85.2049, 34.368, 86.0059),
        (98.0, 51.5044, 68.6409),
    ];
    let side = u32::try_from(size).unwrap_or(16);
    let Some(mut pixmap) = Pixmap::new(side, side) else {
        return Icon {
            width: 0,
            height: 0,
            data: Vec::new(),
        };
    };
    let scale = size as f32 / 120.0;
    let transform = Transform::from_scale(scale, scale);

    let mut orange = Paint::default();
    orange.set_color_rgba8(BRAND[0], BRAND[1], BRAND[2], 255);
    orange.anti_alias = true;
    if let Some(square) = rounded_square(120.0, 20.0) {
        pixmap.fill_path(&square, &orange, FillRule::Winding, transform, None);
    }

    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 255);
    white.anti_alias = true;
    let stroke = Stroke {
        width: 11.0,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    let mut bars = PathBuilder::new();
    for (x, top, bottom) in BARS {
        bars.move_to(x, top);
        bars.line_to(x, bottom);
    }
    if let Some(bars) = bars.finish() {
        pixmap.stroke_path(&bars, &white, &stroke, transform, None);
    }

    // ARGB32, network byte order, straight alpha.
    let data = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.alpha(), color.red(), color.green(), color.blue()]
        })
        .collect();
    Icon {
        width: size,
        height: size,
        data,
    }
}

/// A square of `side` with corners of `radius`, as cubic curves.
fn rounded_square(side: f32, radius: f32) -> Option<Path> {
    // How far the control points sit along the tangents for a quarter circle.
    let k = radius * 0.552_284_8;
    let (r, far) = (radius, side);
    let mut builder = PathBuilder::new();
    builder.move_to(r, 0.0);
    builder.line_to(far - r, 0.0);
    builder.cubic_to(far - r + k, 0.0, far, r - k, far, r);
    builder.line_to(far, far - r);
    builder.cubic_to(far, far - r + k, far - r + k, far, far - r, far);
    builder.line_to(r, far);
    builder.cubic_to(r - k, far, 0.0, far - r + k, 0.0, far - r);
    builder.line_to(0.0, r);
    builder.cubic_to(0.0, r - k, r - k, 0.0, r, 0.0);
    builder.close();
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(icon: &Icon, x: i32, y: i32) -> [u8; 4] {
        let at = ((y * icon.width + x) * 4) as usize;
        [icon.data[at], icon.data[at + 1], icon.data[at + 2], icon.data[at + 3]]
    }

    #[test]
    fn logo_is_an_orange_square_with_white_bars() {
        let icon = logo(64);
        assert_eq!(icon.data.len(), 64 * 64 * 4);
        // The rounded corner is see-through.
        assert!(pixel(&icon, 0, 0)[0] < 64);
        // Left of the first bar: orange.
        assert_eq!(pixel(&icon, 3, 32), [255, BRAND[0], BRAND[1], BRAND[2]]);
        // The middle bar runs through the centre: white.
        assert_eq!(pixel(&icon, 32, 32), [255, 255, 255, 255]);
    }
}
