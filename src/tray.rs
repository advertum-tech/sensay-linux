// Tray icon and its menu (StatusNotifierItem through ksni).
//
// The tray is the main way in: the app usually has no window open. Left click
// opens the history, as clicking the Dock icon does on macOS.
//
// Without a tray host (GNOME without the AppIndicator extension, or a session
// that is still starting) the icon is retried 10 times with the pause doubled
// each time, as whisrs does, and then the app goes on without it.

use std::path::PathBuf;
use std::time::Duration;

use ksni::menu::StandardItem;
use ksni::{Handle, Icon, MenuItem, OfflineReason, TrayMethods};
use tiny_skia::{FillRule, LineCap, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::Command;
use crate::asr::Refusal;
use crate::error_presenter::LastError;
use crate::log;

/// How often the menu is built anew while it shows an error: the age in it
/// counts in minutes, and the error goes after an hour.
const AGE_STEP: Duration = Duration::from_secs(60);

const MAX_ATTEMPTS: u32 = 10;
const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(10);

/// Brand orange, #FF4122.
const BRAND: [u8; 3] = [0xFF, 0x41, 0x22];
/// Pixel sizes handed to the tray host; it picks the one that fits its panel.
const ICON_SIZES: [i32; 6] = [16, 22, 24, 32, 48, 64];

/// The one-colour icons of a ban, a used-up limit and every other error:
/// nosign, hourglass and exclamationmark.triangle on macOS. Taken by name
/// from the icon theme, which draws a symbolic icon in the colour of the
/// text.
const BAN_ICON: &str = "action-unavailable-symbolic";
const LIMIT_ICON: &str = "hourglass-symbolic";
const ERROR_ICON: &str = "dialog-warning-symbolic";

/// Copies of the three for a theme that lacks one (Adwaita has no
/// hourglass), drawn on a 16-unit square. The theme paints them in the
/// colour of the text whatever their fill.
const ICONS: [(&str, &str); 3] = [
    (
        BAN_ICON,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><path fill="#2e3436" fill-rule="evenodd" d="M15 8A7 7 0 1 1 1 8a7 7 0 0 1 14 0zM4.68 3.62a5.5 5.5 0 0 1 7.7 7.7zm-1.06 1.06a5.5 5.5 0 0 0 7.7 7.7z"/></svg>
"##,
    ),
    (
        LIMIT_ICON,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><path fill="#2e3436" fill-rule="evenodd" d="M3 1h10v1.5h-1v1.8c0 1.7-1.3 2.9-2.8 3.7 1.5.8 2.8 2 2.8 3.7v1.8h1V15H3v-1.5h1v-1.8c0-1.7 1.3-2.9 2.8-3.7C5.3 7.2 4 6 4 4.3V2.5H3zm2.5 1.5v1.8c0 1 .9 1.9 2.5 2.9 1.6-1 2.5-1.9 2.5-2.9V2.5z"/></svg>
"##,
    ),
    (
        ERROR_ICON,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><path fill="#2e3436" fill-rule="evenodd" d="M8 1.2c.4 0 .8.2 1 .6l6.3 11.4c.4.8-.1 1.7-1 1.7H1.7c-.9 0-1.4-.9-1-1.7L7 1.8c.2-.4.6-.6 1-.6zM7.25 5.5v4.5h1.5V5.5zM8 11.2a.9.9 0 1 0 0 1.8.9.9 0 0 0 0-1.8z"/></svg>
"##,
    ),
];

pub struct SensayTray {
    commands: UnboundedSender<Command>,
    /// From the ErrorPresenter: the error the menu shows, None when there is
    /// none now.
    last_error: Option<LastError>,
    /// Where the copies of the icons are (install_icons); empty without them.
    icon_theme_path: String,
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

    fn icon_theme_path(&self) -> String {
        self.icon_theme_path.clone()
    }

    /// Called as the menu opens; the menu is then built anew, so the age of
    /// the error in it is worked out at that moment.
    fn menu_about_to_show(&mut self) {}

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: "Sensay".to_string(),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ];
        // Only an error that is still current; with none, no item at all. The
        // whole text on one line, however wide: the Xfce panel showed a
        // submenu with the text in it empty (26 October 2026). The item does
        // nothing; enabled, so the text is not greyed out.
        if let Some(error) = self.last_error.as_ref().filter(|error| error.is_current()) {
            items.push(
                StandardItem {
                    label: last_error_label(error),
                    icon_name: error_icon(error.refusal).to_string(),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.extend([
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
        ]);
        items
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
    mut last_error: watch::Receiver<Option<LastError>>,
) -> Option<Handle<SensayTray>> {
    let icon_theme_path = install_icons();
    let mut pause = FIRST_PAUSE;
    for attempt in 1..=MAX_ATTEMPTS {
        let tray = SensayTray {
            commands: commands.clone(),
            last_error: last_error.borrow_and_update().clone(),
            icon_theme_path: icon_theme_path.clone(),
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

/// Puts each new error into the menu and takes it out once it is over; while
/// one is shown, builds the menu anew every minute for its age. Until the
/// icon goes away.
async fn follow_last_error(handle: Handle<SensayTray>, mut last_error: watch::Receiver<Option<LastError>>) {
    let mut shown = last_error.borrow().clone();
    loop {
        tokio::select! {
            changed = last_error.changed() => {
                if changed.is_err() {
                    return;
                }
                shown = last_error.borrow_and_update().clone();
            }
            _ = tokio::time::sleep(AGE_STEP), if shown.is_some() => {
                shown = shown.filter(LastError::is_current);
            }
        }
        let error = shown.clone();
        if handle.update(|tray| tray.last_error = error).await.is_none() {
            return;
        }
    }
}

/// "Last error, 3 min ago: …", with the age as it is when the menu is built.
/// A single underscore in the text would be taken for an access key.
fn last_error_label(error: &LastError) -> String {
    let minutes = error.age().as_secs() / 60;
    let age = if minutes == 0 {
        "just now".to_string()
    } else {
        format!("{minutes} min ago")
    };
    format!("Last error, {age}: {}", error.message.replace('_', "__"))
}

fn error_icon(refusal: Option<Refusal>) -> &'static str {
    match refusal {
        Some(Refusal::Ban) => BAN_ICON,
        Some(Refusal::Limit) => LIMIT_ICON,
        None => ERROR_ICON,
    }
}

/// Writes the copies of the icons as a hicolor theme folder, which the menu
/// names to the panel as its icon theme path, and returns that folder; empty
/// when it cannot be written. Under XDG_RUNTIME_DIR, which the session
/// clears, or else in ~/.cache.
fn install_icons() -> String {
    let base = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".cache"),
            None => return String::new(),
        },
    };
    let folder = base.join("sensay").join("icons");
    let status = folder.join("hicolor").join("scalable").join("status");
    let written = std::fs::create_dir_all(&status).and_then(|()| {
        ICONS
            .iter()
            .try_for_each(|(name, svg)| std::fs::write(status.join(format!("{name}.svg")), svg))
    });
    match written {
        Ok(()) => folder.to_string_lossy().into_owned(),
        Err(error) => {
            log::warning!(App, "tray: the error icons could not be written to {}: {error}", status.display());
            String::new()
        }
    }
}

/// The tray icon, `size` pixels square. In colour rather than one-colour as
/// on macOS: a tray icon cannot follow the panel being light or dark.
fn logo(size: i32) -> Icon {
    let Some(pixmap) = u32::try_from(size).ok().and_then(logo_pixmap) else {
        return Icon {
            width: 0,
            height: 0,
            data: Vec::new(),
        };
    };
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

/// The Sensay logo, `side` pixels square: the brand-orange rounded square
/// with seven white bars of docs/windows/icon-source/logo.svg (viewBox 120),
/// drawn for each size so it stays sharp. The tray icon and the About
/// window's logo.
pub fn logo_pixmap(side: u32) -> Option<Pixmap> {
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
    let mut pixmap = Pixmap::new(side, side)?;
    let scale = side as f32 / 120.0;
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
    Some(pixmap)
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
