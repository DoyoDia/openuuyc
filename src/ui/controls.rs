//! Shared control appearance for the center, viewer menus and node editor.
use egui::{Color32, RichText, Stroke, vec2};

pub const HEIGHT: f32 = 34.0;
pub const COMPACT_HEIGHT: f32 = 26.0;
pub const ACCENT: Color32 = Color32::from_rgb(75, 136, 235);
const SURFACE: Color32 = Color32::from_rgb(31, 37, 47);
const LINE: Color32 = Color32::from_rgb(45, 53, 66);
const TEXT: Color32 = Color32::from_rgb(231, 235, 242);
const MUTED: Color32 = Color32::from_rgb(156, 167, 184);

pub fn configure(style: &mut egui::Style, height: f32) {
    style.spacing.interact_size.y = height;
    style.spacing.button_padding = if height >= HEIGHT {
        vec2(12.0, 6.0)
    } else {
        vec2(8.0, 4.0)
    };
    style.visuals.extreme_bg_color = Color32::from_rgb(16, 20, 26);
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.selection.bg_fill = Color32::from_rgb(40, 70, 112);
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    for widgets in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.open,
    ] {
        widgets.bg_fill = SURFACE;
        widgets.weak_bg_fill = SURFACE;
        widgets.bg_stroke = Stroke::new(1.0, LINE);
        widgets.fg_stroke.color = TEXT;
        widgets.corner_radius = 5.0.into();
    }
    let hovered = &mut style.visuals.widgets.hovered;
    hovered.bg_fill = Color32::from_rgb(44, 54, 69);
    hovered.weak_bg_fill = hovered.bg_fill;
    hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(81, 107, 142));
    hovered.fg_stroke.color = TEXT;
    hovered.corner_radius = 5.0.into();
    let active = &mut style.visuals.widgets.active;
    active.bg_fill = ACCENT;
    active.weak_bg_fill = Color32::from_rgb(36, 60, 92);
    active.bg_stroke = Stroke::new(1.0, ACCENT);
    active.fg_stroke.color = TEXT;
    active.corner_radius = 5.0.into();
}

pub fn singleline(value: &mut String, height: f32) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(value)
        .font(egui::TextStyle::Body)
        .vertical_align(egui::Align::Center)
        .margin(if height >= HEIGHT {
            egui::Margin::symmetric(12, 6)
        } else {
            egui::Margin::symmetric(8, 4)
        })
}

pub fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(Color32::WHITE))
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .min_size(vec2(64.0, HEIGHT))
}

pub fn paint_close(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    let stroke = Stroke::new(1.4, color);
    for sign in [-1.0, 1.0] {
        painter.line_segment(
            [
                center + vec2(-4.5, -4.5 * sign),
                center + vec2(4.5, 4.5 * sign),
            ],
            stroke,
        );
    }
}

pub fn close_button(ui: &mut egui::Ui, hint: &str, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    if response.hovered() || response.is_pointer_button_down_on() || response.has_focus() {
        ui.painter().rect(
            rect,
            5.0,
            visuals.weak_bg_fill,
            if response.has_focus() {
                visuals.bg_stroke
            } else {
                Stroke::NONE
            },
            egui::StrokeKind::Inside,
        );
    }
    paint_close(
        ui.painter(),
        rect,
        if !ui.is_enabled() {
            Color32::from_gray(90)
        } else if response.hovered() {
            TEXT
        } else {
            MUTED
        },
    );
    response.on_hover_text(hint)
}
