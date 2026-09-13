//! Shared control appearance for the center, viewer menus and node editor.
use super::theme::{self, LINE, MUTED, SURFACE, TEXT};
use egui::{Color32, RichText, Stroke, vec2};

pub const HEIGHT: f32 = theme::CONTROL_HEIGHT;
pub const COMPACT_HEIGHT: f32 = theme::COMPACT_HEIGHT;
pub const ACCENT: Color32 = theme::ACCENT;

pub fn configure(style: &mut egui::Style, height: f32) {
    theme::typography(style, height);
    style.spacing.interact_size.y = height;
    style.spacing.button_padding = if height >= HEIGHT {
        vec2(12.0, 6.0)
    } else {
        vec2(8.0, 4.0)
    };
    style.visuals.extreme_bg_color = theme::SIDEBAR;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.selection.bg_fill = theme::SELECTION;
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    for widgets in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.open,
    ] {
        widgets.bg_fill = SURFACE;
        widgets.weak_bg_fill = SURFACE;
        widgets.bg_stroke = Stroke::new(1.0, LINE);
        widgets.fg_stroke.color = TEXT;
        widgets.corner_radius = theme::CONTROL_RADIUS.into();
    }
    let hovered = &mut style.visuals.widgets.hovered;
    hovered.bg_fill = theme::HOVER;
    hovered.weak_bg_fill = hovered.bg_fill;
    hovered.bg_stroke = Stroke::new(1.0, theme::BORDER_FOCUS);
    hovered.fg_stroke.color = TEXT;
    hovered.corner_radius = theme::CONTROL_RADIUS.into();
    let active = &mut style.visuals.widgets.active;
    active.bg_fill = ACCENT;
    active.weak_bg_fill = theme::ACTIVE;
    active.bg_stroke = Stroke::new(1.0, ACCENT);
    active.fg_stroke.color = TEXT;
    active.corner_radius = theme::CONTROL_RADIUS.into();
}

pub fn singleline(value: &mut String, height: f32) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(value)
        .font(egui::TextStyle::Body)
        // TextEdit's current atom layout does not use min_size.y. An empty
        // prefix reserves the inner height for both values and placeholders.
        .prefix(egui::Atom {
            size: Some(vec2(
                0.0,
                height - if height >= HEIGHT { 12.0 } else { 8.0 },
            )),
            ..Default::default()
        })
        .vertical_align(egui::Align::Center)
        .margin(if height >= HEIGHT {
            egui::Margin::symmetric(12, 6)
        } else {
            egui::Margin::symmetric(8, 4)
        })
}

pub fn dialog_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(theme::PANEL_RADIUS)
        .inner_margin(egui::Margin::same(20))
}

pub fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(Color32::WHITE))
        .fill(ACCENT)
        .stroke(Stroke::NONE)
        .min_size(vec2(64.0, HEIGHT))
}

/// A full button target with a vector chevron, independent of font glyphs.
pub fn back_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let text =
        ui.painter()
            .layout_no_wrap(label.into(), egui::FontId::proportional(theme::BODY), TEXT);
    let response = ui.add_sized(
        vec2(text.size().x + 48.0, HEIGHT),
        egui::Button::new("").frame(false),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let center = response.rect.left_center() + vec2(18.0, 0.0);
    let color = ui.style().interact(&response).fg_stroke.color;
    ui.painter().add(egui::Shape::line(
        vec![
            center + vec2(3.0, -5.0),
            center + vec2(-2.0, 0.0),
            center + vec2(3.0, 5.0),
        ],
        Stroke::new(1.6, color),
    ));
    ui.painter().galley(
        response.rect.left_center() + vec2(34.0, -text.size().y / 2.0),
        text,
        color,
    );
    response.on_hover_text("返回上一页（Esc）")
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
            theme::DISABLED
        } else if response.hovered() {
            TEXT
        } else {
            MUTED
        },
    );
    response.on_hover_text(hint)
}
