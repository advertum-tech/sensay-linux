// The desktop's interface font, so the windows read like the rest of the
// desktop: the family and size GNOME is set to, resolved by fontconfig to a
// file, as GTK resolves it. egui's own fonts stay behind it for whatever the
// system font lacks (emoji, symbols).

use std::process::Command;
use std::sync::{Arc, OnceLock};

use eframe::egui;
use egui::{FontData, FontDefinitions, FontFamily, FontId, TextStyle};

/// The bold face of the interface font, for group titles.
pub const BOLD: &str = "sensay-bold";

struct SystemFont {
    regular: Vec<u8>,
    bold: Option<Vec<u8>>,
    /// Size in points, 11 for "Adwaita Sans 11".
    points: f32,
}

/// Puts the interface font in front of egui's fonts and sizes the text
/// styles after it.
pub fn install(context: &egui::Context) {
    static FONT: OnceLock<Option<SystemFont>> = OnceLock::new();
    let font = FONT.get_or_init(find);

    let mut definitions = FontDefinitions::default();
    let mut proportional = definitions
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    // GNOME's default, 11 points, at 96 dots per inch.
    let mut body = 11.0 * 96.0 / 72.0;
    let mut bold = proportional.clone();
    if let Some(font) = font {
        definitions
            .font_data
            .insert("sensay-system".to_string(), Arc::new(FontData::from_owned(font.regular.clone())));
        proportional.insert(0, "sensay-system".to_string());
        let bold_face = font.bold.clone().unwrap_or_else(|| font.regular.clone());
        definitions
            .font_data
            .insert(BOLD.to_string(), Arc::new(FontData::from_owned(bold_face)));
        bold = std::iter::once(BOLD.to_string()).chain(proportional.iter().cloned()).collect();
        body = font.points * 96.0 / 72.0;
    }
    definitions.families.insert(FontFamily::Proportional, proportional);
    definitions.families.insert(FontFamily::Name(BOLD.into()), bold);
    context.set_fonts(definitions);

    context.all_styles_mut(|style| {
        style.text_styles.insert(TextStyle::Body, FontId::new(body, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Button, FontId::new(body, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Small, FontId::new(body * 0.86, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Heading, FontId::new(body * 1.2, FontFamily::Proportional));
    });
}

fn find() -> Option<SystemFont> {
    let (family, points) = interface_font().unwrap_or_else(|| ("sans-serif".to_string(), 11.0));
    let regular = std::fs::read(fc_match(&family)?).ok()?;
    let bold = fc_match(&format!("{family}:bold")).and_then(|path| std::fs::read(path).ok());
    Some(SystemFont { regular, bold, points })
}

/// GNOME's interface font, as "Family Name 11". None off GNOME.
fn interface_font() -> Option<(String, f32)> {
    let output = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "font-name"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let name = text.trim().trim_matches('\'');
    let (family, size) = name.rsplit_once(' ')?;
    Some((family.to_string(), size.parse().ok()?))
}

/// The file fontconfig picks for this pattern.
fn fc_match(pattern: &str) -> Option<String> {
    let output = Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let path = String::from_utf8(output.stdout).ok()?;
    (!path.is_empty()).then_some(path)
}
