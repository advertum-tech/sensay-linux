// The About window, as on macOS: the logo, the name, the version, and the
// App ID that support asks for, in small grey text the mouse can select and
// copy.

use eframe::egui::{self, FontFamily, RichText};

use super::fonts::BOLD;
use crate::{app_id, tray};

/// The logo's side, in points.
const LOGO_SIDE: f32 = 64.0;

pub struct AboutWindow {
    app_id: String,
    /// The logo drawn for the screen's scale, and its side in pixels.
    logo: Option<(egui::TextureHandle, u32)>,
}

impl AboutWindow {
    pub fn new() -> AboutWindow {
        AboutWindow {
            app_id: app_id::app_id().unwrap_or_else(|| "unavailable".to_string()),
            logo: None,
        }
    }

    /// Draws the content and returns its height, for the window to fit it.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> f32 {
        let weak = ui.visuals().weak_text_color();
        let logo = self.logo(ui.ctx());
        let content = ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            if let Some(logo) = logo {
                ui.add(egui::Image::from_texture((logo, egui::vec2(LOGO_SIDE, LOGO_SIDE))));
                ui.add_space(12.0);
            }
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

    /// The logo as a texture, drawn in pixels of the screen so it stays
    /// sharp, and drawn again when the scale changes.
    fn logo(&mut self, context: &egui::Context) -> Option<egui::TextureId> {
        let side = (LOGO_SIDE * context.pixels_per_point()).round() as u32;
        if self.logo.as_ref().is_none_or(|(_, drawn)| *drawn != side) {
            self.logo = tray::logo_pixmap(side).map(|pixmap| {
                let size = [pixmap.width() as usize, pixmap.height() as usize];
                let image = egui::ColorImage::from_rgba_premultiplied(size, pixmap.data());
                (context.load_texture("sensay-logo", image, egui::TextureOptions::LINEAR), side)
            });
        }
        self.logo.as_ref().map(|(texture, _)| texture.id())
    }
}
