//! Compact remote-access notices; state and consent remain with the host owner.
use super::theme;
use crate::application::app::notifications::{Card, Verb};

pub(crate) fn host_notice(ui: &mut egui::Ui, card: &Card) -> Option<Verb> {
    let mut action = None;
    egui::Frame::new()
        .fill(theme::BG)
        .inner_margin(theme::NOTIFICATION_PADDING)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = theme::NOTIFICATION_GAP;
            ui.horizontal(|ui| {
                crate::ui::chrome::paint_brand_logo(ui);
                ui.label(
                    egui::RichText::new(&card.title)
                        .size(theme::COMPACT_TEXT)
                        .color(theme::MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if super::close_button(ui, "收起通知", theme::COMPACT_HEIGHT).clicked() {
                        action = Some(Verb::Dismiss);
                    }
                    if card.confirmation {
                        let seconds = (card.expires_at - chrono::Utc::now().timestamp()).max(0);
                        ui.label(
                            egui::RichText::new(format!("{seconds}s"))
                                .monospace()
                                .size(theme::SMALL)
                                .color(theme::MUTED),
                        );
                    }
                });
            });
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&card.body)
                        .size(theme::SECTION)
                        .strong()
                        .color(theme::TEXT),
                )
                .wrap(),
            );
            if card.confirmation {
                ui.label(
                    egui::RichText::new(&card.detail)
                        .size(theme::SMALL)
                        .color(theme::MUTED),
                );
                ui.add_space(theme::NOTIFICATION_GAP);
                let enabled = !card.busy && card.expires_at > chrono::Utc::now().timestamp();
                ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal(|ui| {
                        let width = (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
                        if ui
                            .add_sized([width, theme::CONTROL_HEIGHT], super::secondary("拒绝"))
                            .clicked()
                        {
                            action = Some(Verb::Reject);
                        }
                        if ui
                            .add_sized([width, theme::CONTROL_HEIGHT], super::primary("允许本次"))
                            .clicked()
                        {
                            action = Some(Verb::Allow);
                        }
                    });
                });
            } else if card.expires_at == 0 {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(6., 6.), egui::Sense::hover());
                    ui.painter().circle_filled(
                        dot.center(),
                        2.5,
                        if card.connected {
                            theme::RED
                        } else {
                            theme::AMBER
                        },
                    );
                    ui.label(
                        egui::RichText::new(&card.detail)
                            .monospace()
                            .size(theme::SMALL)
                            .color(theme::MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if super::value_button(
                            ui,
                            egui::vec2(theme::NOTIFICATION_LINK_WIDTH, theme::COMPACT_HEIGHT),
                            egui::RichText::new("控制中心 ›")
                                .size(theme::SMALL)
                                .color(theme::ACCENT),
                        )
                        .clicked()
                        {
                            action = Some(Verb::Open);
                        }
                    });
                });
            } else {
                ui.label(
                    egui::RichText::new(&card.detail)
                        .size(theme::SMALL)
                        .color(theme::MUTED),
                );
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if super::value_button(
                            ui,
                            egui::vec2(theme::NOTIFICATION_LINK_WIDTH, theme::COMPACT_HEIGHT),
                            egui::RichText::new("查看 ›")
                                .size(theme::SMALL)
                                .color(theme::ACCENT),
                        )
                        .clicked()
                        {
                            action = Some(Verb::Open);
                        }
                    });
                });
            }
        });
    action
}
