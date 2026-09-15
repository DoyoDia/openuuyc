use super::*;
use egui::{Align, Align2, FontId, Rect, Sense, pos2};

#[derive(Clone, Copy)]
pub(crate) enum UpdateIcon {
    Required,
    Waiting,
    Error,
    Ready,
}

pub(crate) fn update_dialog_frame() -> egui::Frame {
    dialog_frame().inner_margin(theme::UPDATE_DIALOG_MARGIN)
}

fn update_icon(ui: &mut egui::Ui, icon: UpdateIcon, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let color = match icon {
        UpdateIcon::Required => theme::AMBER,
        UpdateIcon::Error => theme::RED,
        _ => theme::ACCENT,
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, theme::CONTROL_RADIUS, theme::SURFACE);
    let c = rect.center();
    let stroke = Stroke::new(1.6, color);
    match icon {
        UpdateIcon::Waiting => {
            let mut spinner_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(8.0)));
            spinner_ui.add(egui::Spinner::new().size(size - 16.0).color(color));
        }
        UpdateIcon::Error => {
            painter.circle_stroke(c, 8.0, stroke);
            painter.line_segment([c + vec2(0.0, -4.0), c + vec2(0.0, 1.0)], stroke);
            painter.circle_filled(c + vec2(0.0, 4.5), 1.0, color);
        }
        UpdateIcon::Ready => {
            painter.line_segment([c + vec2(-7.0, 0.0), c + vec2(-2.0, 5.0)], stroke);
            painter.line_segment([c + vec2(-2.0, 5.0), c + vec2(7.0, -5.0)], stroke);
        }
        UpdateIcon::Required => {
            painter.line_segment([c + vec2(0.0, 6.0), c + vec2(0.0, -7.0)], stroke);
            painter.line_segment([c + vec2(-5.0, -2.0), c + vec2(0.0, -7.0)], stroke);
            painter.line_segment([c + vec2(0.0, -7.0), c + vec2(5.0, -2.0)], stroke);
            painter.line_segment([c + vec2(-8.0, 8.0), c + vec2(8.0, 8.0)], stroke);
        }
    }
}

pub(crate) fn update_dialog_header(
    ui: &mut egui::Ui,
    title: &str,
    icon: UpdateIcon,
    closable: bool,
) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        update_icon(ui, icon, theme::UPDATE_HEADER_HEIGHT);
        ui.add_space(4.0);
        ui.label(
            RichText::new(title)
                .size(theme::UPDATE_DIALOG_TITLE)
                .strong(),
        );
        if closable {
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                close = close_button(ui, "关闭提示", theme::COMPACT_HEIGHT).clicked();
            });
        }
    });
    ui.add_space(20.0);
    close
}

pub(crate) fn update_device_row(ui: &mut egui::Ui, alias: &str, version: &str) {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .corner_radius(theme::CONTROL_RADIUS)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
            let painter = ui.painter_at(rect);
            let screen = Rect::from_center_size(
                pos2(rect.left() + 9.0, rect.center().y - 2.0),
                vec2(16.0, 11.0),
            );
            painter.rect_stroke(
                screen,
                2,
                Stroke::new(1.2, theme::MUTED),
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    screen.center_bottom(),
                    screen.center_bottom() + vec2(0.0, 3.0),
                ],
                Stroke::new(1.2, theme::MUTED),
            );
            painter.line_segment(
                [
                    screen.center_bottom() + vec2(-4.0, 3.0),
                    screen.center_bottom() + vec2(4.0, 3.0),
                ],
                Stroke::new(1.2, theme::MUTED),
            );
            let version_width = painter
                .layout_no_wrap(
                    version.to_owned(),
                    FontId::proportional(theme::SMALL),
                    theme::MUTED,
                )
                .size()
                .x
                .min(120.0);
            let name_rect = Rect::from_min_max(
                rect.min + vec2(30.0, 0.0),
                pos2(
                    (rect.right() - version_width - 16.0).max(rect.left() + 30.0),
                    rect.bottom(),
                ),
            );
            let mut name_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(name_rect)
                    .layout(egui::Layout::left_to_right(Align::Center)),
            );
            let name_response = name_ui
                .add(egui::Label::new(RichText::new(alias).size(theme::COMPACT_TEXT)).truncate());
            if painter
                .layout_no_wrap(
                    alias.to_owned(),
                    FontId::proportional(theme::COMPACT_TEXT),
                    theme::TEXT,
                )
                .size()
                .x
                > name_rect.width()
            {
                name_response.on_hover_text(alias);
            }
            let version_rect =
                Rect::from_min_max(pos2(rect.right() - version_width, rect.top()), rect.max);
            let mut version_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(version_rect)
                    .layout(egui::Layout::right_to_left(Align::Center)),
            );
            version_ui.add(
                egui::Label::new(
                    RichText::new(version)
                        .size(theme::SMALL)
                        .color(theme::MUTED),
                )
                .truncate(),
            );
        });
}

/// Primary action is consistently on the right; the two choices have equal targets.
pub(crate) fn update_actions(
    ui: &mut egui::Ui,
    primary_label: Option<&str>,
    secondary_label: Option<(&str, &str)>,
) -> (bool, bool) {
    ui.add_space(24.0);
    let mut actions = (false, false);
    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
        if let Some(label) = primary_label {
            actions.0 = ui
                .add_sized(
                    [theme::UPDATE_ACTION_WIDTH, theme::CONTROL_HEIGHT],
                    primary(label),
                )
                .clicked();
        }
        if let Some((label, hint)) = secondary_label {
            actions.1 = ui
                .add_sized(
                    [theme::UPDATE_ACTION_WIDTH, theme::CONTROL_HEIGHT],
                    secondary(label),
                )
                .on_hover_text(hint)
                .clicked();
        }
    });
    actions
}

pub(crate) fn update_countdown(ui: &mut egui::Ui, seconds: u64) {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .corner_radius(theme::CONTROL_RADIUS)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("自动重新连接")
                        .color(theme::MUTED)
                        .size(theme::COMPACT_TEXT),
                );
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{seconds} 秒"))
                            .size(theme::SECTION)
                            .strong(),
                    );
                });
            });
        });
}

pub(crate) fn update_prepared_notice(ctx: &egui::Context) -> bool {
    let mut postpone = false;
    egui::Area::new(egui::Id::new("remote-update-prepared"))
        .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -24.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            dialog_frame()
                .inner_margin(egui::Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.set_width(theme::REMOTE_UPGRADE_WIDTH);
                    ui.horizontal(|ui| {
                        update_icon(ui, UpdateIcon::Ready, 32.0);
                        ui.add_space(2.0);
                        ui.label(RichText::new("被控端更新已准备就绪").size(theme::COMPACT_TEXT));
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            postpone = ui.add(secondary("暂不安装")).clicked();
                        });
                    });
                });
        });
    postpone
}
