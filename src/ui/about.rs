// The About window, as on macOS: the name, the version, and the App ID that
// support asks for, in small grey text the mouse can select and copy.

use eframe::egui::{self, FontFamily, RichText};

use super::fonts::BOLD;
use crate::app_id;

pub struct AboutWindow {
    app_id: String,
}

impl AboutWindow {
    pub fn new() -> AboutWindow {
        AboutWindow {
            app_id: app_id::app_id().unwrap_or_else(|| "unavailable".to_string()),
        }
    }

    /// Draws the content and returns its height, for the window to fit it.
    pub fn ui(&self, ui: &mut egui::Ui) -> f32 {
        let weak = ui.visuals().weak_text_color();
        let content = ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(RichText::new("Sensay").size(22.0).family(FontFamily::Name(BOLD.into())));
            ui.add_space(4.0);
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.add_space(14.0);
            ui.add(
                egui::Label::new(RichText::new(format!("App ID: {}", self.app_id)).small().color(weak))
                    .selectable(true),
            );
            ui.add_space(24.0);
        });
        content.response.rect.height()
    }
}
