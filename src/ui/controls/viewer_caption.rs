//! Shared icons and interaction states for the player caption.
use crate::ui::theme;

#[derive(Clone, Copy)]
pub(crate) enum ViewerCaptionIcon {
    Plugins,
    Annotation,
    OneToOne,
    Mouse,
    Quality,
    Minimize,
    Maximize,
    Restore,
    Close,
}

pub(crate) fn viewer_caption_button(
    ui: &mut egui::Ui,
    icon: ViewerCaptionIcon,
    selected: bool,
    tooltip: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(theme::VIEWER_CAPTION_BUTTON, theme::VIEWER_CAPTION_BUTTON),
        egui::Sense::click(),
    );
    let fill = if !ui.is_enabled() {
        egui::Color32::TRANSPARENT
    } else if matches!(icon, ViewerCaptionIcon::Close) && response.hovered() {
        crate::ui::theme::DANGER_FILL
    } else if selected {
        crate::ui::theme::SELECTED
    } else if response.hovered() {
        theme::HOVER
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, theme::CONTROL_RADIUS, fill);
    let color = if !ui.is_enabled() {
        theme::DISABLED
    } else if selected {
        crate::ui::theme::ACCENT
    } else if response.hovered() {
        theme::TEXT
    } else {
        crate::ui::theme::MUTED
    };
    paint_caption_icon(ui.painter(), rect, icon, color);
    response.on_hover_text(tooltip)
}

fn paint_caption_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: ViewerCaptionIcon,
    color: egui::Color32,
) {
    let center = rect.center();
    let stroke = egui::Stroke::new(theme::ICON_STROKE, color);
    match icon {
        ViewerCaptionIcon::Annotation => super::annotation::paint_annotation_icon(
            painter,
            egui::Rect::from_center_size(center, egui::vec2(16., 16.)),
            super::AnnotationIcon::Pen,
            color,
        ),
        ViewerCaptionIcon::Plugins => crate::plugins::paint_plugin_icon(painter, rect, color),
        ViewerCaptionIcon::OneToOne => {
            for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = center + egui::vec2(x * 7.0, y * 6.0);
                painter.line(
                    vec![
                        corner - egui::vec2(x * 4.0, 0.0),
                        corner,
                        corner - egui::vec2(0.0, y * 4.0),
                    ],
                    stroke,
                );
            }
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(3.0, 3.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        ViewerCaptionIcon::Mouse => {
            let body = egui::Rect::from_center_size(center, egui::vec2(11.0, 16.0));
            painter.rect_stroke(body, 5.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    center + egui::vec2(0.0, -6.0),
                    center + egui::vec2(0.0, -2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-4.5, -1.0),
                    center + egui::vec2(4.5, -1.0),
                ],
                stroke,
            );
        }
        ViewerCaptionIcon::Quality => {
            for (offset, knob) in [(-5.0, -3.0), (0.0, 4.0), (5.0, -1.0)] {
                painter.line_segment(
                    [
                        egui::pos2(center.x - 7.0, center.y + offset),
                        egui::pos2(center.x + 7.0, center.y + offset),
                    ],
                    stroke,
                );
                painter.circle_filled(egui::pos2(center.x + knob, center.y + offset), 2.0, color);
            }
        }
        ViewerCaptionIcon::Minimize => {
            painter.line_segment(
                [
                    egui::pos2(center.x - 6.0, center.y + 4.0),
                    egui::pos2(center.x + 6.0, center.y + 4.0),
                ],
                stroke,
            );
        }
        ViewerCaptionIcon::Maximize => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(11.0, 9.0)),
                0.5,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        ViewerCaptionIcon::Restore => {
            let back = egui::Rect::from_min_size(
                egui::pos2(center.x - 4.0, center.y - 6.0),
                egui::vec2(9.0, 8.0),
            );
            let front = back.translate(egui::vec2(-2.5, 2.5));
            painter.rect_stroke(back, 0.5, stroke, egui::StrokeKind::Inside);
            painter.rect_filled(front, 0.0, crate::ui::theme::SIDEBAR);
            painter.rect_stroke(front, 0.5, stroke, egui::StrokeKind::Inside);
        }
        ViewerCaptionIcon::Close => {
            painter.line_segment(
                [center - egui::vec2(5.0, 5.0), center + egui::vec2(5.0, 5.0)],
                stroke,
            );
            painter.line_segment(
                [
                    center + egui::vec2(-5.0, 5.0),
                    center + egui::vec2(5.0, -5.0),
                ],
                stroke,
            );
        }
    }
}
