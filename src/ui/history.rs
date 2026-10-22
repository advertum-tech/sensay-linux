// The History window (HistoryWindowController.swift, HistoryRowView.swift).
//
// A day heading, then one entry per dictation: the time on the left in grey,
// the text taking the rest, a hairline under each row. The buttons show only
// while the pointer is over a row. A row is here from the moment its audio
// was written, so a dictation the service could not handle can be sent again.
//
// The list keeps itself up to date from the history's changes; nothing polls.

use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use eframe::egui;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::TryRecvError;

use egui::epaint::text::VariationCoords;
use egui::{Color32, Painter, Pos2, Rect, Stroke, StrokeKind};

use super::fonts::BOLD;
use super::Bridge;
use crate::history::{self, Change, History, Transcript, TranscriptState};
use crate::log;
use crate::paths;

/// Where the text starts; the time is centred in the strip before it.
const TEXT_LEFT: f32 = 104.0;
const INSET: f32 = 24.0;
/// Kept clear on the right so a button never lands on the text.
const ACTIONS_WIDTH: f32 = 130.0;
const PADDING: f32 = 22.0;
const TEXT_SIZE: f32 = 15.0;
const TIME_SIZE: f32 = 12.0;
const BUTTON: f32 = 32.0;
const BUTTON_GAP: f32 = 6.0;
/// Sensay orange. It appears only under the pointer, on one icon at a time,
/// as on macOS (HistoryActionButton.swift).
const BRAND: Color32 = Color32::from_rgb(0xFF, 0x41, 0x22);
const LIMIT: usize = 500;

enum RowAction {
    Copy(i64, String),
    Again(Transcript),
    Delete(Transcript),
}

pub struct HistoryWindow {
    rows: Vec<Transcript>,
    changes: Option<broadcast::Receiver<Change>>,
    /// The copy button of this row shows a tick for a moment.
    copied: Option<(i64, Instant)>,
}

impl HistoryWindow {
    pub fn new() -> HistoryWindow {
        HistoryWindow {
            rows: Vec::new(),
            changes: None,
            copied: None,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, bridge: &Bridge) {
        let Some(history) = &bridge.history else {
            centred(ui, "The history could not be opened.");
            return;
        };
        if self.changes.is_none() {
            // Watched before reading, so nothing that happens in between is
            // missed; a change for a row already read just repeats it.
            self.changes = Some(history.watch());
            self.rows = bridge.runtime.block_on(history.all(LIMIT));
            // One of the two moments the audio is swept.
            bridge.runtime.spawn(history::sweep(history.clone()));
        }
        self.apply_changes(bridge, history);

        if self.rows.is_empty() {
            centred(ui, "Nothing dictated yet.");
            return;
        }

        let mut pending = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut last_day: Option<String> = None;
            for row in &self.rows {
                let day = day_title(row.created_at);
                if last_day.as_ref() != Some(&day) {
                    heading(ui, &day, last_day.is_none());
                    last_day = Some(day);
                }
                if let Some(action) = entry(ui, row, self.copied) {
                    pending = Some(action);
                }
            }
            ui.add_space(12.0);
        });

        if let Some((_, at)) = self.copied {
            if at.elapsed() < Duration::from_secs(1) {
                ui.ctx().request_repaint_after(Duration::from_secs(1) - at.elapsed());
            } else {
                self.copied = None;
            }
        }

        let Some(action) = pending else { return };
        match action {
            RowAction::Copy(id, text) => {
                ui.ctx().copy_text(text);
                self.copied = Some((id, Instant::now()));
            }
            RowAction::Again(row) => transcribe_again(bridge, history, row),
            RowAction::Delete(row) => delete(bridge, history, row),
        }
    }

    fn apply_changes(&mut self, bridge: &Bridge, history: &History) {
        let Some(changes) = self.changes.as_mut() else { return };
        loop {
            match changes.try_recv() {
                Ok(Change::Added(row)) => {
                    // The newest dictation goes on top.
                    if !self.rows.iter().any(|known| known.id == row.id) {
                        self.rows.insert(0, row);
                    }
                }
                Ok(Change::Changed(row)) => {
                    if let Some(known) = self.rows.iter_mut().find(|known| known.id == row.id) {
                        *known = row;
                    }
                }
                Ok(Change::Removed(id)) => self.rows.retain(|known| known.id != id),
                Err(TryRecvError::Lagged(_)) => {
                    self.rows = bridge.runtime.block_on(history.all(LIMIT));
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
    }
}

fn centred(ui: &mut egui::Ui, text: &str) {
    ui.centered_and_justified(|ui| {
        ui.label(egui::RichText::new(text).size(14.0).weak());
    });
}

/// TODAY, YESTERDAY or the date.
fn day_title(created_at: f64) -> String {
    let Some(moment) = Local.timestamp_opt(created_at as i64, 0).single() else {
        return String::new();
    };
    let day = moment.date_naive();
    let today = Local::now().date_naive();
    if day == today {
        "TODAY".to_string()
    } else if today.pred_opt() == Some(day) {
        "YESTERDAY".to_string()
    } else {
        day.format("%b %-d, %Y").to_string().to_uppercase()
    }
}

fn time(created_at: f64) -> String {
    Local
        .timestamp_opt(created_at as i64, 0)
        .single()
        .map(|moment| moment.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// TODAY, YESTERDAY or the date, bold, in the colour of the text: a heading
/// in grey read as one more row.
fn heading(ui: &mut egui::Ui, title: &str, first: bool) {
    let height = if first { 44.0 } else { 68.0 };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let format = egui::TextFormat {
        font_id: egui::FontId::new(13.0, egui::FontFamily::Name(BOLD.into())),
        color: ui.visuals().text_color(),
        coords: VariationCoords::new([(b"wght", 700.0)]),
        extra_letter_spacing: 0.6,
        ..Default::default()
    };
    let galley = ui
        .painter()
        .layout_job(egui::text::LayoutJob::single_section(title.to_string(), format));
    let position = egui::pos2(rect.left() + TEXT_LEFT, rect.bottom() - 12.0 - galley.size().y);
    ui.painter().galley(position, galley, ui.visuals().text_color());
}

/// What the row shows in place of its text, and whether that is a status.
fn shown_text(row: &Transcript) -> (String, bool) {
    let status = match row.state {
        TranscriptState::Recorded => Some("Queued".to_string()),
        TranscriptState::Sending => Some("Sending".to_string()),
        TranscriptState::Recognising => Some("Transcribing".to_string()),
        TranscriptState::Empty => Some("No text returned".to_string()),
        TranscriptState::Failed => Some(row.failure_reason.clone().unwrap_or_else(|| "Failed".to_string())),
        TranscriptState::Done => None,
    };
    if let Some(status) = status {
        return (status, true);
    }
    let flat = row.text.replace("\n\n", " ").replace('\n', " ").trim().to_string();
    if flat.is_empty() {
        ("—".to_string(), true)
    } else {
        (flat, false)
    }
}

fn entry(ui: &mut egui::Ui, row: &Transcript, copied: Option<(i64, Instant)>) -> Option<RowAction> {
    let width = ui.available_width();
    let text_width = (width - TEXT_LEFT - INSET - ACTIONS_WIDTH).max(120.0);
    let (shown, status) = shown_text(row);
    let visuals = ui.visuals().clone();
    let color = if status { visuals.weak_text_color() } else { visuals.text_color() };

    let mut job = egui::text::LayoutJob::single_section(
        shown,
        egui::TextFormat {
            font_id: egui::FontId::proportional(TEXT_SIZE),
            color,
            italics: status,
            ..Default::default()
        },
    );
    job.wrap.max_width = text_width;
    let galley = ui.painter().layout_job(job);

    let height = galley.size().y + 2.0 * PADDING;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let hovered = ui.rect_contains_pointer(rect);
    let painter = ui.painter().clone();

    if hovered {
        let tint = visuals.text_color();
        painter.rect_filled(
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.max.y - 1.0)),
            0.0,
            egui::Color32::from_rgba_unmultiplied(tint.r(), tint.g(), tint.b(), 12),
        );
    }
    painter.text(
        egui::pos2(rect.left() + TEXT_LEFT / 2.0, rect.top() + PADDING + 2.0),
        egui::Align2::CENTER_TOP,
        time(row.created_at),
        egui::FontId::proportional(TIME_SIZE),
        visuals.weak_text_color(),
    );
    painter.galley(egui::pos2(rect.left() + TEXT_LEFT, rect.top() + PADDING), galley, color);
    // A hairline, quieter than egui's own separator.
    let line = if visuals.dark_mode {
        Color32::from_white_alpha(14)
    } else {
        Color32::from_black_alpha(14)
    };
    painter.hline(rect.x_range(), rect.bottom() - 0.5, egui::Stroke::new(1.0, line));

    if !hovered {
        return None;
    }

    let mut buttons: Vec<(Icon, &str, RowAction)> = Vec::new();
    if row.state == TranscriptState::Done && !row.text.is_empty() {
        let copy_icon = if copied.is_some_and(|(id, at)| id == row.id && at.elapsed() < Duration::from_secs(1)) {
            Icon::Copied
        } else {
            Icon::Copy
        };
        buttons.push((copy_icon, "Copy the text", RowAction::Copy(row.id, row.text.clone())));
    }
    // A row still waiting its turn is about to be sent anyway.
    if row.audio_file_name.is_some() && row.state != TranscriptState::Recorded {
        buttons.push((Icon::Again, "Transcribe again", RowAction::Again(row.clone())));
    }
    buttons.push((Icon::Delete, "Delete, with the recording", RowAction::Delete(row.clone())));

    let count = buttons.len() as f32;
    let mut x = rect.right() - INSET - (count * BUTTON + (count - 1.0) * BUTTON_GAP);
    let mut chosen = None;
    for (index, (icon, help, action)) in buttons.into_iter().enumerate() {
        let place = Rect::from_min_size(egui::pos2(x, rect.center().y - BUTTON / 2.0), egui::vec2(BUTTON, BUTTON));
        // Not a widget: an interact zone and a drawing, so nothing is
        // allocated in the list and the rows under it stay where they are.
        let response = ui
            .interact(place, ui.id().with(("row-button", row.id, index)), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(help);
        let painter = ui.painter();
        // At rest the icon is grey like the secondary text; under the pointer
        // it turns orange on a faint orange circle, stronger while held.
        let pressed = response.is_pointer_button_down_on();
        let hovered = response.hovered();
        if pressed {
            painter.circle_filled(place.center(), BUTTON / 2.0 - 1.0, BRAND.gamma_multiply(0.30));
        } else if hovered {
            painter.circle_filled(place.center(), BUTTON / 2.0 - 1.0, BRAND.gamma_multiply(0.16));
        }
        let rest = if visuals.dark_mode {
            Color32::from_white_alpha(140)
        } else {
            Color32::from_black_alpha(128)
        };
        // The tick after a copy is orange too, for the moment it is shown.
        let colour = if pressed || hovered || matches!(icon, Icon::Copied) { BRAND } else { rest };
        draw_icon(painter, place.center(), icon, colour);
        if response.clicked() {
            chosen = Some(action);
        }
        x += BUTTON + BUTTON_GAP;
    }
    chosen
}

#[derive(Clone, Copy)]
enum Icon {
    Copy,
    Copied,
    Again,
    Delete,
}

/// The row's icons, drawn as lines so they sit centred and sharp at any
/// scale, after the SF Symbols of the macOS version: square.on.square, a
/// tick, arrow.clockwise, trash.
fn draw_icon(painter: &Painter, c: Pos2, icon: Icon, colour: Color32) {
    let stroke = Stroke::new(1.6, colour);
    let at = |x: f32, y: f32| egui::pos2(c.x + x, c.y + y);
    match icon {
        Icon::Copy => {
            let front = Rect::from_min_max(at(-3.0, -3.0), at(7.0, 7.0));
            painter.rect_stroke(front, 2.5, stroke, StrokeKind::Middle);
            // The square behind, up and to the left, shows only where the
            // front one leaves it.
            painter.add(egui::Shape::line(
                vec![at(3.0, -3.5), at(3.0, -5.0), at(1.0, -7.0), at(-5.0, -7.0), at(-7.0, -5.0), at(-7.0, 1.0), at(-5.0, 3.0), at(-3.5, 3.0)],
                stroke,
            ));
        }
        Icon::Copied => {
            painter.add(egui::Shape::line(vec![at(-5.5, 0.0), at(-1.5, 4.0), at(6.0, -4.5)], Stroke::new(2.0, colour)));
        }
        Icon::Again => {
            // An arc turning clockwise from two o'clock round to twelve, the
            // head at twelve pointing right.
            let radius = 6.0;
            let start = -30f32.to_radians();
            let end = 270f32.to_radians();
            let steps = 24;
            let points: Vec<Pos2> = (0..=steps)
                .map(|step| {
                    let angle = start + (end - start) * step as f32 / steps as f32;
                    at(radius * angle.cos(), radius * angle.sin())
                })
                .collect();
            let tip_base = *points.last().unwrap_or(&c);
            painter.add(egui::Shape::line(points, stroke));
            let direction = egui::vec2(-end.sin(), end.cos());
            let across = egui::vec2(-direction.y, direction.x);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    tip_base + direction * 3.0,
                    tip_base - direction * 1.5 + across * 3.0,
                    tip_base - direction * 1.5 - across * 3.0,
                ],
                colour,
                Stroke::NONE,
            ));
        }
        Icon::Delete => {
            painter.line_segment([at(-7.0, -4.5), at(7.0, -4.5)], stroke);
            painter.add(egui::Shape::line(vec![at(-2.5, -4.5), at(-2.5, -7.0), at(2.5, -7.0), at(2.5, -4.5)], stroke));
            painter.add(egui::Shape::line(
                vec![at(-5.0, -4.5), at(-4.2, 6.0), at(-3.0, 7.0), at(3.0, 7.0), at(4.2, 6.0), at(5.0, -4.5)],
                stroke,
            ));
            for x in [-2.2, 0.0, 2.2] {
                painter.line_segment([at(x, -2.0), at(x, 4.5)], stroke);
            }
        }
    }
}

/// A second try for a finished row, the repair for a failed one, a fresh
/// start for one stuck on its way. The row goes back to Queued at once. Text
/// from here is never pasted, only written to the row.
fn transcribe_again(bridge: &Bridge, history: &History, mut row: Transcript) {
    let history = history.clone();
    let recogniser = bridge.recogniser.clone();
    let Some(name) = row.audio_file_name.clone() else { return };
    bridge.runtime.spawn(async move {
        let Some(audio) = paths::audio_folder().map(|folder| folder.join(&name)) else { return };
        let tag = format!("row{}", row.id);
        if !audio.exists() {
            log::warning!(Storage, "[{tag}] audio {name} is gone, cannot send again");
            row.audio_file_name = None;
            history.update(&row).await;
            return;
        }
        row.state = TranscriptState::Recorded;
        row.failure_reason = None;
        history.update(&row).await;
        log::info!(App, "[{tag}] transcribing again");
        if let Some(done) = recogniser.recognise(row, audio, history.clone(), tag.clone()).await {
            history.update(&done).await;
            log::info!(Asr, "[{tag}] transcribed again: {}", done.state.as_str());
        }
    });
}

/// The row and its recording go together.
fn delete(bridge: &Bridge, history: &History, row: Transcript) {
    let history = history.clone();
    bridge.runtime.spawn(async move {
        if let (Some(name), Some(folder)) = (&row.audio_file_name, paths::audio_folder()) {
            if let Err(error) = std::fs::remove_file(folder.join(name)) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warning!(Storage, "could not delete {name}: {error}");
                }
            }
        }
        history.remove(row.id).await;
        log::info!(Storage, "row {} deleted", row.id);
    });
}
