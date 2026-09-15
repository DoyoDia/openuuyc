use super::*;
use egui::{Rect, Response, Sense, Ui, pos2};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnotationIcon {
    Pen,
    Laser,
    Pointer,
    Line,
    Arrow,
    Rectangle,
    Ellipse,
    Eraser,
    Undo,
    Redo,
    Clear,
}

pub(crate) fn annotation_button(
    ui: &mut Ui,
    icon: AnnotationIcon,
    selected: bool,
    hint: &str,
) -> Response {
    ink_button(ui, icon, selected, hint, theme::ANNOTATION_BUTTON)
}
pub(crate) fn annotation_tool_button(
    ui: &mut Ui,
    icon: AnnotationIcon,
    selected: bool,
    hint: &str,
) -> Response {
    ink_button(ui, icon, selected, hint, theme::ANNOTATION_TOOL_SIZE)
}
fn ink_button(
    ui: &mut Ui,
    icon: AnnotationIcon,
    selected: bool,
    hint: &str,
    size: f32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let painter = ui.painter();
    if selected || response.hovered() {
        painter.rect_filled(
            rect,
            theme::CONTROL_RADIUS,
            if selected {
                theme::SELECTED
            } else {
                theme::HOVER
            },
        );
    }
    let color = if !ui.is_enabled() {
        theme::MUTED.gamma_multiply(0.45)
    } else if selected {
        theme::ACCENT
    } else {
        theme::TEXT
    };
    let icon_rect = Rect::from_center_size(rect.center(), vec2(16., 16.));
    paint_annotation_icon(painter, icon_rect, icon, color);
    response.on_hover_text(hint)
}

pub(crate) fn annotation_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1., theme::LINE))
        .corner_radius(theme::PANEL_RADIUS)
        .inner_margin(12)
}
pub(crate) fn annotation_tool_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::SIDEBAR)
        .corner_radius(theme::CONTROL_RADIUS)
        .inner_margin(5)
}
pub(crate) fn annotation_header(
    ui: &mut Ui,
    can_undo: bool,
    can_redo: bool,
) -> (Response, bool, bool, bool) {
    let mut undo = false;
    let mut redo = false;
    let mut close = false;
    let row = ui.horizontal(|ui| {
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width() - 102., 30.), Sense::drag());
        let p = ui.painter();
        for x in [0., 5.] {
            for y in [-5., 0., 5.] {
                p.circle_filled(
                    pos2(rect.left() + 3. + x, rect.center().y + y),
                    1.,
                    theme::MUTED,
                );
            }
        }
        p.text(
            pos2(rect.left() + 22., rect.center().y),
            egui::Align2::LEFT_CENTER,
            "批注",
            egui::FontId::proportional(theme::COMPACT_TEXT),
            theme::TEXT,
        );
        let response = response
            .on_hover_cursor(egui::CursorIcon::Grab)
            .on_hover_text("拖动调整面板位置");
        if response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        ui.spacing_mut().item_spacing.x = 2.;
        undo = ui
            .add_enabled_ui(can_undo, |ui| {
                annotation_button(ui, AnnotationIcon::Undo, false, "撤销 · Ctrl+Z")
            })
            .inner
            .clicked();
        redo = ui
            .add_enabled_ui(can_redo, |ui| {
                annotation_button(ui, AnnotationIcon::Redo, false, "重做 · Ctrl+Y")
            })
            .inner
            .clicked();
        close = close_button(ui, "关闭批注并清除笔迹", 30.).clicked();
        response
    });
    (row.inner, close, undo, redo)
}
fn ink_property(ui: &mut Ui, width: f32) -> (Rect, Response) {
    let (r, response) = ui.allocate_exact_size(vec2(width, 30.), Sense::click());
    ui.painter().rect_filled(
        r,
        theme::CONTROL_RADIUS,
        if response.hovered() {
            theme::HOVER
        } else {
            theme::SURFACE
        },
    );
    let c = pos2(r.right() - 12., r.center().y);
    ui.painter().add(egui::Shape::line(
        vec![c + vec2(-3., -1.5), c + vec2(0., 1.5), c + vec2(3., -1.5)],
        Stroke::new(1., theme::MUTED),
    ));
    (r, response)
}
pub(crate) fn annotation_color(ui: &mut Ui, rgb: &mut [u8; 3], opacity: &mut u8) {
    let (r, response) = ink_property(ui, 132.);
    let alpha = (u32::from(*opacity) * 255 / 100) as u8;
    let swatch = Rect::from_center_size(pos2(r.left() + 16., r.center().y), vec2(16., 16.));
    egui::color_picker::show_color_at(
        ui.painter(),
        Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha),
        swatch,
    );
    ui.painter().text(
        pos2(r.left() + 32., r.center().y),
        egui::Align2::LEFT_CENTER,
        format!("颜色  {}%", opacity),
        egui::FontId::proportional(theme::SMALL),
        theme::TEXT,
    );
    egui::Popup::from_toggle_button_response(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| super::annotation_color::show(ui, rgb, opacity));
}
pub(crate) fn annotation_width(ui: &mut Ui, width: &mut f32, tool: AnnotationIcon) {
    let laser = tool == AnnotationIcon::Laser;
    let pointer = tool == AnnotationIcon::Pointer;
    let (r, response) = ink_property(ui, 104.);
    let y = r.center().y;
    if pointer {
        paint_annotation_icon(
            ui.painter(),
            Rect::from_center_size(pos2(r.left() + 18., y), vec2(15., 15.)),
            AnnotationIcon::Pointer,
            theme::TEXT,
        );
    } else if laser {
        ui.painter().circle_filled(
            pos2(r.left() + 18., y),
            (*width * 0.5).clamp(2., 6.),
            theme::TEXT,
        );
    } else {
        ui.painter().line_segment(
            [pos2(r.left() + 9., y), pos2(r.left() + 27., y)],
            Stroke::new((*width).clamp(1., 5.), theme::TEXT),
        );
    }
    ui.painter().text(
        pos2(r.left() + 34., y),
        egui::Align2::LEFT_CENTER,
        format!("{width:.1} px"),
        egui::FontId::proportional(theme::SMALL),
        theme::TEXT,
    );
    egui::Popup::from_toggle_button_response(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(theme::ANNOTATION_PICKER_WIDTH);
            ui.spacing_mut().interact_size.y = theme::COMPACT_HEIGHT;
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(
                RichText::new(if pointer {
                    "光标大小"
                } else if laser {
                    "光点大小"
                } else {
                    "线条粗细"
                })
                .strong(),
            );
            ui.horizontal(|ui| {
                for value in if pointer {
                    [16., 24., 32., 48.]
                } else if laser {
                    [4., 8., 12., 20.]
                } else {
                    [1., 3., 6., 12.]
                } {
                    if ui
                        .selectable_label((*width - value).abs() < 0.01, format!("{value:.0} px"))
                        .clicked()
                    {
                        *width = value;
                    }
                }
            });
            ui.spacing_mut().slider_width = theme::ANNOTATION_PICKER_WIDTH - 64.;
            ui.add(
                egui::Slider::new(width, if pointer { 12.0..=64.0 } else { 1.0..=64.0 })
                    .logarithmic(true)
                    .suffix(" px"),
            );
        });
}
pub(crate) fn paint_annotation_icon(
    p: &egui::Painter,
    r: Rect,
    icon: AnnotationIcon,
    color: Color32,
) {
    let stroke = Stroke::new(1.4, color);
    let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
    let line = |a, b| {
        p.line_segment([a, b], stroke);
    };
    match icon {
        AnnotationIcon::Pen => {
            p.add(egui::Shape::closed_line(
                vec![
                    at(0.1, 0.9),
                    at(0.15, 0.62),
                    at(0.72, 0.05),
                    at(0.95, 0.28),
                    at(0.38, 0.85),
                ],
                stroke,
            ));
            line(at(0.62, 0.15), at(0.85, 0.38));
        }
        AnnotationIcon::Pointer => {
            p.add(egui::Shape::closed_line(
                vec![
                    at(0.15, 0.0),
                    at(0.15, 0.85),
                    at(0.38, 0.64),
                    at(0.57, 1.0),
                    at(0.73, 0.92),
                    at(0.52, 0.57),
                    at(0.85, 0.57),
                ],
                stroke,
            ));
        }
        AnnotationIcon::Laser => {
            p.circle_filled(r.center(), 2.4, color);
            p.circle_stroke(r.center(), 4.7, Stroke::new(1.0, color));
            for (a, b) in [
                (at(0.5, 0.0), at(0.5, 0.12)),
                (at(0.5, 0.88), at(0.5, 1.0)),
                (at(0.0, 0.5), at(0.12, 0.5)),
                (at(0.88, 0.5), at(1.0, 0.5)),
            ] {
                line(a, b);
            }
        }
        AnnotationIcon::Line => line(at(0.1, 0.9), at(0.9, 0.1)),
        AnnotationIcon::Arrow => {
            line(at(0.1, 0.9), at(0.9, 0.1));
            line(at(0.4, 0.1), at(0.9, 0.1));
            line(at(0.9, 0.1), at(0.9, 0.6));
        }
        AnnotationIcon::Rectangle => {
            p.rect_stroke(r.shrink(1.0), 0, stroke, egui::StrokeKind::Inside);
        }
        AnnotationIcon::Ellipse => {
            p.circle_stroke(r.center(), r.width() * 0.45, stroke);
        }
        AnnotationIcon::Eraser => {
            p.add(egui::Shape::closed_line(
                vec![at(0.0, 0.6), at(0.6, 0.0), at(1.0, 0.4), at(0.4, 1.0)],
                stroke,
            ));
            line(at(0.3, 0.3), at(0.7, 0.7));
        }
        AnnotationIcon::Undo | AnnotationIcon::Redo => {
            let f = |x, y| {
                at(
                    if icon == AnnotationIcon::Undo {
                        x
                    } else {
                        1.0 - x
                    },
                    y,
                )
            };
            p.add(egui::Shape::line(
                vec![
                    f(0.15, 0.32),
                    f(0.65, 0.32),
                    f(0.85, 0.48),
                    f(0.85, 0.75),
                    f(0.7, 0.9),
                    f(0.5, 0.9),
                ],
                stroke,
            ));
            line(f(0.4, 0.05), f(0.1, 0.32));
            line(f(0.1, 0.32), f(0.4, 0.6));
        }
        AnnotationIcon::Clear => {
            line(at(0.1, 0.2), at(0.9, 0.2));
            line(at(0.4, 0.0), at(0.6, 0.0));
            p.add(egui::Shape::line(
                vec![
                    at(0.23, 0.35),
                    at(0.28, 0.95),
                    at(0.72, 0.95),
                    at(0.77, 0.35),
                ],
                stroke,
            ));
        }
    }
}
