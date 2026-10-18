// The window's own frame, drawn the way GNOME draws its windows: rounded
// corners, a header with the title in the middle and a round close button
// on the right, a hairline around the edge. winit's frame is off for these
// windows (with_decorations(false)) and the window is transparent outside
// the rounded shape.

use eframe::egui;
use egui::epaint::text::VariationCoords;
use egui::{Color32, CornerRadius, CursorIcon, FontFamily, FontId, Rect, ResizeDirection, Sense, Stroke, StrokeKind, Ui};

use super::fonts::BOLD;

/// libadwaita's header bar: 46 points and the line under it.
pub const HEADER_HEIGHT: f32 = 47.0;
const RADIUS: u8 = 12;
/// How far from the edge the pointer can grab it to resize.
const GRAB: f32 = 6.0;

struct Colours {
    header: Color32,
    body: Color32,
    line: Color32,
    text: Color32,
    button: Color32,
    button_hover: Color32,
}

fn colours(dark: bool, view: bool) -> Colours {
    if dark {
        Colours {
            header: Color32::from_rgb(46, 46, 50),
            body: if view { Color32::from_rgb(30, 30, 34) } else { Color32::from_rgb(34, 34, 38) },
            line: Color32::from_white_alpha(22),
            text: Color32::from_white_alpha(235),
            button: Color32::from_white_alpha(26),
            button_hover: Color32::from_white_alpha(44),
        }
    } else {
        Colours {
            header: Color32::WHITE,
            body: if view { Color32::WHITE } else { Color32::from_rgb(250, 250, 251) },
            line: Color32::from_black_alpha(22),
            text: Color32::from_black_alpha(220),
            button: Color32::from_black_alpha(18),
            button_hover: Color32::from_black_alpha(34),
        }
    }
}

/// Draws the frame over the whole window and returns the rect left for the
/// content, and whether the close button was clicked. `view` is a window
/// whose body is a list (white), not a form (grey).
pub fn window(ui: &mut Ui, title: &str, resizable: bool, view: bool) -> (Rect, bool) {
    let context = ui.ctx().clone();
    let colours = colours(ui.visuals().dark_mode, view);
    let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
    // A maximized window has square corners, as GNOME's do.
    let radius = if maximized { 0 } else { RADIUS };
    let rect = ui.max_rect();
    let header = Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.min.y + HEADER_HEIGHT));
    let painter = ui.painter().clone();

    painter.rect_filled(rect, CornerRadius::same(radius), colours.body);
    painter.rect_filled(
        header,
        CornerRadius { nw: radius, ne: radius, sw: 0, se: 0 },
        colours.header,
    );
    painter.hline(header.x_range(), header.bottom() - 0.5, Stroke::new(1.0, colours.line));
    painter.rect_stroke(rect, CornerRadius::same(radius), Stroke::new(1.0, colours.line), StrokeKind::Inside);

    let title_format = egui::TextFormat {
        font_id: FontId::new(egui::TextStyle::Body.resolve(ui.style()).size, FontFamily::Name(BOLD.into())),
        color: colours.text,
        coords: VariationCoords::new([(b"wght", 700.0)]),
        ..Default::default()
    };
    let galley = painter.layout_job(egui::text::LayoutJob::single_section(title.to_string(), title_format));
    painter.galley(header.center() - galley.size() / 2.0, galley, colours.text);

    // The header moves the window; a double click maximizes a window that
    // can change size.
    let drag = ui.interact(header, ui.id().with("header"), Sense::click_and_drag());
    if drag.drag_started_by(egui::PointerButton::Primary) {
        context.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if resizable && drag.double_clicked() {
        context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    }

    let close_centre = egui::pos2(header.right() - 23.0, header.center().y);
    let close_rect = Rect::from_center_size(close_centre, egui::vec2(24.0, 24.0));
    let close = ui.interact(close_rect, ui.id().with("close"), Sense::click());
    painter.circle_filled(
        close_centre,
        12.0,
        if close.hovered() { colours.button_hover } else { colours.button },
    );
    let arm = 4.0;
    let cross = Stroke::new(1.6, colours.text);
    painter.line_segment([close_centre + egui::vec2(-arm, -arm), close_centre + egui::vec2(arm, arm)], cross);
    painter.line_segment([close_centre + egui::vec2(-arm, arm), close_centre + egui::vec2(arm, -arm)], cross);

    if resizable && !maximized {
        edges(ui, rect);
    }

    let content = Rect::from_min_max(egui::pos2(rect.min.x, header.bottom()), rect.max);
    (content, close.clicked())
}

/// Grab zones along the edges and corners; dragging one resizes the window.
fn edges(ui: &mut Ui, rect: Rect) {
    let zones = [
        (Rect::from_min_max(rect.left_top(), egui::pos2(rect.right(), rect.top() + GRAB)), ResizeDirection::North, CursorIcon::ResizeNorth),
        (Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - GRAB), rect.right_bottom()), ResizeDirection::South, CursorIcon::ResizeSouth),
        (Rect::from_min_max(rect.left_top(), egui::pos2(rect.left() + GRAB, rect.bottom())), ResizeDirection::West, CursorIcon::ResizeWest),
        (Rect::from_min_max(egui::pos2(rect.right() - GRAB, rect.top()), rect.right_bottom()), ResizeDirection::East, CursorIcon::ResizeEast),
        (Rect::from_min_size(rect.left_top(), egui::vec2(2.0 * GRAB, 2.0 * GRAB)), ResizeDirection::NorthWest, CursorIcon::ResizeNorthWest),
        (Rect::from_min_size(egui::pos2(rect.right() - 2.0 * GRAB, rect.top()), egui::vec2(2.0 * GRAB, 2.0 * GRAB)), ResizeDirection::NorthEast, CursorIcon::ResizeNorthEast),
        (Rect::from_min_size(egui::pos2(rect.left(), rect.bottom() - 2.0 * GRAB), egui::vec2(2.0 * GRAB, 2.0 * GRAB)), ResizeDirection::SouthWest, CursorIcon::ResizeSouthWest),
        (Rect::from_min_size(egui::pos2(rect.right() - 2.0 * GRAB, rect.bottom() - 2.0 * GRAB), egui::vec2(2.0 * GRAB, 2.0 * GRAB)), ResizeDirection::SouthEast, CursorIcon::ResizeSouthEast),
    ];
    for (index, (zone, direction, cursor)) in zones.into_iter().enumerate() {
        let response = ui
            .interact(zone, ui.id().with(("edge", index)), Sense::drag())
            .on_hover_cursor(cursor);
        if response.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}
