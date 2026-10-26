// The Settings window (SettingsWindowController.swift), laid out the way
// GNOME lays out preferences: a group title over a rounded card of rows, the
// title of each row on the left and its control on the right.
//
// Values are written as they are changed, with no OK button, and the app
// picks each change up at once: the service, the Local address and the key
// for the next row sent, the activation key by restarting the key listener,
// the microphone from the next press. The Local address is written when
// typing in it ends, never half-typed.

use eframe::egui;
use egui::epaint::text::VariationCoords;
use egui::{Color32, FontFamily, FontId, Sense, Stroke, StrokeKind, TextStyle, Ui};

use super::Bridge;
use super::fonts::BOLD;
use crate::Command;
use crate::audio::Inputs;
use crate::autostart;
use crate::config::{self, ActivationKey, Microphone, SpeechService};
use crate::log;

/// For what is switched on or chosen when the desktop has no accent colour:
/// Sensay orange.
const BRAND: Color32 = Color32::from_rgb(0xFF, 0x41, 0x22);
const CONTENT_WIDTH: f32 = 530.0;
const MARGIN: f32 = 24.0;
const GROUP_GAP: f32 = 32.0;
const TITLE_GAP: f32 = 10.0;
const ROW_HEIGHT: f32 = 50.0;
const TWO_LINE_HEIGHT: f32 = 58.0;
const ROW_PADDING: f32 = 14.0;
const RADIUS: f32 = 12.0;
const FIELD_WIDTH: f32 = 230.0;

/// Colours of the window, after libadwaita's light and dark styles.
struct Palette {
    accent: Color32,
    card: Color32,
    outline: Color32,
    shadow: Color32,
    line: Color32,
    text: Color32,
    dim: Color32,
    off: Color32,
}

impl Palette {
    fn of(ui: &Ui) -> Palette {
        let accent = ui
            .ctx()
            .data(|data| data.get_temp::<Color32>(super::accent_id()))
            .unwrap_or(BRAND);
        if ui.visuals().dark_mode {
            Palette {
                accent,
                card: Color32::from_rgb(48, 48, 53),
                outline: Color32::from_white_alpha(10),
                shadow: Color32::from_black_alpha(60),
                line: Color32::from_white_alpha(20),
                text: Color32::from_white_alpha(235),
                dim: Color32::from_white_alpha(140),
                off: Color32::from_white_alpha(50),
            }
        } else {
            Palette {
                accent,
                card: Color32::WHITE,
                outline: Color32::from_black_alpha(8),
                shadow: Color32::from_black_alpha(18),
                line: Color32::from_black_alpha(20),
                text: Color32::from_black_alpha(220),
                dim: Color32::from_black_alpha(140),
                off: Color32::from_black_alpha(45),
            }
        }
    }
}

pub struct SettingsWindow {
    /// The field's own text, so a key typed with a space at either end is not
    /// cut while being typed; the stored key is trimmed.
    api_key: Option<String>,
    /// The Local field's own text; the stored address changes only when
    /// typing ends with an http or https address.
    local_address: Option<String>,
    autostart: bool,
}

impl SettingsWindow {
    pub fn new() -> SettingsWindow {
        SettingsWindow {
            api_key: None,
            local_address: None,
            autostart: autostart::is_enabled(),
        }
    }

    /// Draws the window and returns the height of what it drew.
    pub fn ui(&mut self, ui: &mut Ui, bridge: &Bridge) -> f32 {
        let palette = Palette::of(ui);

        let stored = bridge.config.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let mut config = stored.clone();
        let inputs = bridge.inputs.borrow().clone();

        let top = ui.cursor().top();
        let full = ui.available_width();
        let width = (full - 2.0 * MARGIN).min(CONTENT_WIDTH);
        ui.add_space(MARGIN);
        ui.horizontal(|ui| {
            ui.add_space((full - width) / 2.0);
            ui.vertical(|ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing.y = 0.0;

                group_title(ui, &palette, "Speech service");
                card(ui, &palette, |ui| {
                    for (index, service) in SpeechService::ALL.into_iter().enumerate() {
                        let chosen = config.speech_service == service;
                        // The Sensay rows show no address: it is the API's,
                        // not a choice.
                        let row = choice_row(ui, &palette, index == 0, chosen, service.title(), None);
                        if service == SpeechService::Local {
                            // Live only while Local is chosen. An address that
                            // is not http or https is not kept, and the field
                            // goes back to the one in use.
                            let text = self.local_address.get_or_insert_with(|| stored.local_address.clone());
                            let field = egui::TextEdit::singleline(text)
                                .hint_text(config::DEFAULT_LOCAL_ADDRESS)
                                .desired_width(FIELD_WIDTH);
                            if trailing(ui, row.rect, |ui| ui.add_enabled(chosen, field)).lost_focus() {
                                match config::web_address(text) {
                                    Some(address) => {
                                        *text = address.clone();
                                        config.local_address = address;
                                    }
                                    None => *text = stored.local_address.clone(),
                                }
                            }
                        }
                        if row.clicked() {
                            config.speech_service = service;
                        }
                    }
                    let api_key = self.api_key.get_or_insert_with(|| stored.api_key.clone());
                    control_row(ui, &palette, false, "API key", |ui| {
                        let field = egui::TextEdit::singleline(api_key)
                            .password(true)
                            .hint_text("empty means no key is sent")
                            .desired_width(FIELD_WIDTH);
                        if ui.add(field).changed() {
                            config.api_key = api_key.trim().to_string();
                        }
                    });
                });

                ui.add_space(GROUP_GAP);
                group_title(ui, &palette, "Activation");
                card(ui, &palette, |ui| {
                    control_row(ui, &palette, true, "Activation key", |ui| {
                        activation_choice(ui, &palette, &mut config.activation_key);
                    });
                    let mut wanted = self.autostart;
                    if switch_row(ui, &palette, false, "Start Sensay at login", &mut wanted) {
                        self.set_autostart(wanted);
                    }
                });

                ui.add_space(GROUP_GAP);
                group_title(ui, &palette, "Microphone");
                microphones(ui, &palette, &inputs, &mut config.microphone);
            });
        });
        ui.add_space(MARGIN);
        let height = ui.cursor().top() - top;

        if config != stored {
            *bridge.config.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = config.clone();
            if let Err(error) = config::save(&config) {
                log::error!(App, "settings: could not save: {error}");
            }
            let _ = bridge.commands.send(Command::SettingsChanged);
        }
        height
    }

    fn set_autostart(&mut self, wanted: bool) {
        match autostart::set(wanted) {
            Ok(()) => {
                self.autostart = wanted;
                log::info!(App, "settings: start at login {}", if wanted { "on" } else { "off" });
            }
            Err(error) => {
                log::error!(App, "settings: could not change start at login: {error}");
                // Shown as it is, not as it was clicked.
                self.autostart = autostart::is_enabled();
            }
        }
    }
}

fn body(ui: &Ui) -> FontId {
    TextStyle::Body.resolve(ui.style())
}

fn small(ui: &Ui) -> FontId {
    TextStyle::Small.resolve(ui.style())
}

fn group_title(ui: &mut Ui, palette: &Palette, text: &str) {
    let format = egui::TextFormat {
        font_id: FontId::new(body(ui).size, FontFamily::Name(BOLD.into())),
        color: palette.text,
        // A variable font is drawn bold by its weight axis; a static bold
        // face ignores this.
        coords: VariationCoords::new([(b"wght", 700.0)]),
        ..Default::default()
    };
    ui.label(egui::text::LayoutJob::single_section(text.to_string(), format));
    ui.add_space(TITLE_GAP);
}

/// A rounded card behind the rows `content` draws, with libadwaita's faint
/// outline and shadow. The rows lay themselves out first; the card is put
/// under them afterwards.
fn card(ui: &mut Ui, palette: &Palette, content: impl FnOnce(&mut Ui)) {
    let background = ui.painter().add(egui::Shape::Noop);
    let rect = ui.vertical(content).response.rect;
    let shadow = egui::epaint::Shadow {
        offset: [0, 1],
        blur: 5,
        spread: 0,
        color: palette.shadow,
    };
    ui.painter().set(
        background,
        egui::Shape::Vec(vec![
            shadow.as_shape(rect, RADIUS).into(),
            egui::epaint::RectShape::new(rect, RADIUS, palette.card, Stroke::new(1.0, palette.outline), StrokeKind::Inside)
                .into(),
        ]),
    );
}

/// The space of one row, with the line above it unless it is the first.
fn row_rect(ui: &mut Ui, palette: &Palette, first: bool, height: f32, sense: Sense) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), sense);
    if !first {
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, palette.line));
    }
    response
}

/// A row that is one of several choices: a round mark on the left, the
/// title, and under it in grey what it stands for. The whole row is clicked,
/// except a control put on it afterwards.
fn choice_row(
    ui: &mut Ui,
    palette: &Palette,
    first: bool,
    chosen: bool,
    title: &str,
    subtitle: Option<&str>,
) -> egui::Response {
    let height = if subtitle.is_some() { TWO_LINE_HEIGHT } else { ROW_HEIGHT };
    let response = row_rect(ui, palette, first, height, Sense::click());
    let rect = response.rect;
    let painter = ui.painter();

    let mark = egui::pos2(rect.left() + ROW_PADDING + 9.0, rect.center().y);
    if chosen {
        painter.circle_filled(mark, 9.0, palette.accent);
        painter.circle_filled(mark, 3.5, Color32::WHITE);
    } else {
        painter.circle_stroke(mark, 8.0, Stroke::new(2.0, palette.off));
    }

    let left = mark.x + 9.0 + ROW_PADDING;
    match subtitle {
        Some(subtitle) => {
            painter.text(egui::pos2(left, rect.center().y - 1.0), egui::Align2::LEFT_BOTTOM, title, body(ui), palette.text);
            painter.text(egui::pos2(left, rect.center().y + 2.0), egui::Align2::LEFT_TOP, subtitle, small(ui), palette.dim);
        }
        None => {
            painter.text(egui::pos2(left, rect.center().y), egui::Align2::LEFT_CENTER, title, body(ui), palette.text);
        }
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The title on the left, `control` against the right edge.
fn control_row(ui: &mut Ui, palette: &Palette, first: bool, title: &str, control: impl FnOnce(&mut Ui)) {
    let rect = row_rect(ui, palette, first, ROW_HEIGHT, Sense::hover()).rect;
    ui.painter().text(
        egui::pos2(rect.left() + ROW_PADDING, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        body(ui),
        palette.text,
    );
    trailing(ui, rect, control);
}

/// Runs `control` against the right edge of the row at `rect`. A child that
/// is not allocated in the row's parent: the row already has its height, and
/// the control must not move the next row up.
fn trailing<R>(ui: &mut Ui, rect: egui::Rect, control: impl FnOnce(&mut Ui) -> R) -> R {
    let place = rect.shrink2(egui::vec2(ROW_PADDING, 6.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(place)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    control(&mut child)
}

/// The title on the left, a switch on the right; the whole row flips it.
/// Returns whether it was flipped.
fn switch_row(ui: &mut Ui, palette: &Palette, first: bool, title: &str, on: &mut bool) -> bool {
    let response = row_rect(ui, palette, first, ROW_HEIGHT, Sense::click());
    let rect = response.rect;
    let flipped = response.clicked();
    if flipped {
        *on = !*on;
    }
    let painter = ui.painter();
    painter.text(
        egui::pos2(rect.left() + ROW_PADDING, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        body(ui),
        palette.text,
    );

    let track = egui::Rect::from_center_size(
        egui::pos2(rect.right() - ROW_PADDING - 23.0, rect.center().y),
        egui::vec2(46.0, 26.0),
    );
    let position = ui.ctx().animate_bool_responsive(response.id, *on);
    let fill = lerp_colour(palette.off, palette.accent, position);
    painter.rect_filled(track, 13.0, fill);
    let knob_x = egui::lerp(track.left() + 13.0..=track.right() - 13.0, position);
    painter.circle_filled(egui::pos2(knob_x, track.center().y), 10.0, Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand);
    flipped
}

fn lerp_colour(from: Color32, to: Color32, t: f32) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

/// The chosen key and a chevron; the list opens under it. No frame of its
/// own: the row is the frame.
fn activation_choice(ui: &mut Ui, palette: &Palette, key: &mut ActivationKey) {
    let visuals = &mut ui.style_mut().visuals;
    visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.inactive.bg_stroke = Stroke::NONE;
    visuals.widgets.inactive.fg_stroke.color = palette.text;
    visuals.widgets.hovered.weak_bg_fill = palette.line;
    visuals.widgets.hovered.bg_stroke = Stroke::NONE;
    egui::ComboBox::from_id_salt("activation-key")
        .selected_text(key.title())
        // A thin chevron, as GNOME's lists have.
        .icon(|ui, rect, visuals, _open| {
            let centre = rect.center();
            let stroke = Stroke::new(1.5, visuals.fg_stroke.color);
            let (half_width, half_height) = (4.0, 2.2);
            ui.painter().line_segment(
                [centre + egui::vec2(-half_width, -half_height), centre + egui::vec2(0.0, half_height)],
                stroke,
            );
            ui.painter().line_segment(
                [centre + egui::vec2(0.0, half_height), centre + egui::vec2(half_width, -half_height)],
                stroke,
            );
        })
        .show_ui(ui, |ui| {
            for choice in ActivationKey::choices() {
                ui.selectable_value(key, choice, choice.title());
            }
        });
}

/// One list, every row live: the built-in microphone first, then Auto with
/// the device it resolves to now, then every other device.
fn microphones(ui: &mut Ui, palette: &Palette, inputs: &Inputs, chosen: &mut Microphone) {
    let default_name = inputs
        .default
        .as_ref()
        .map(|name| {
            inputs
                .list
                .iter()
                .find(|input| &input.name == name)
                .map_or(name.clone(), |input| input.description.clone())
        })
        .unwrap_or_else(|| "none".to_string());

    let mut rows: Vec<(String, Option<String>, Microphone)> = Vec::new();
    if let Some(built_in) = inputs.list.iter().find(|input| input.built_in) {
        rows.push((built_in.description.clone(), Some("Recommended".to_string()), Microphone::BuiltIn));
    }
    rows.push(("Auto".to_string(), Some(default_name.clone()), Microphone::Auto));
    for input in inputs.list.iter().filter(|input| !input.built_in) {
        rows.push((input.description.clone(), None, Microphone::Named(input.name.clone())));
    }

    let current = rows.iter().position(|(_, _, selection)| selection == chosen);
    card(ui, palette, |ui| {
        for (index, (title, subtitle, selection)) in rows.iter().enumerate() {
            // With the chosen device unplugged nothing would be marked; the
            // first row says what is used now.
            let marked = index == current.unwrap_or(0);
            if choice_row(ui, palette, index == 0, marked, title, subtitle.as_deref()).clicked() {
                *chosen = selection.clone();
            }
        }
    });

    let note = if inputs.list.is_empty() {
        Some("No microphones are connected.".to_string())
    } else if current.is_none() {
        Some(format!("The chosen microphone is not connected. Recording from {default_name}."))
    } else {
        None
    };
    if let Some(note) = note {
        ui.add_space(8.0);
        ui.label(egui::RichText::new(note).font(small(ui)).color(palette.dim));
    }
}
