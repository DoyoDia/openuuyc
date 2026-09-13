use super::*;
use crate::power::PowerAction;

fn progress_row(ui: &mut egui::Ui, message: &str, waiting: bool, elapsed: u64) -> bool {
    let (row, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::CONTROL_HEIGHT),
        Sense::hover(),
    );
    let button_rect = egui::Rect::from_min_size(
        egui::pos2(row.right() - 96.0, row.top()),
        vec2(96.0, row.height()),
    );
    let text_right = button_rect.left() - if waiting { 108.0 } else { 12.0 };
    let color = if waiting { BLUE } else { MUTED };
    let icon = egui::Rect::from_center_size(
        egui::pos2(row.left() + 12.0, row.center().y),
        vec2(16.0, 16.0),
    );
    if waiting {
        ui.put(icon, egui::Spinner::new().size(16.0).color(color));
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    } else {
        ui.painter().circle_filled(icon.center(), 3.0, color);
    }
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(row.left() + 32.0, row.top()),
        egui::pos2(text_right, row.bottom()),
    );
    let mut text_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(text_rect)
            .layout(egui::Layout::left_to_right(Align::Center)),
    );
    text_ui.set_clip_rect(text_rect.intersect(ui.clip_rect()));
    text_ui
        .add(egui::Label::new(RichText::new(message).size(theme::BODY)).truncate())
        .on_hover_text(message);
    if waiting {
        ui.painter().text(
            egui::pos2(button_rect.left() - 16.0, row.center().y),
            egui::Align2::RIGHT_CENTER,
            format!("已等待 {elapsed} 秒"),
            FontId::proportional(theme::COMPACT_TEXT),
            MUTED,
        );
    }
    ui.put(
        button_rect,
        egui::Button::new(if waiting {
            "停止等待"
        } else {
            "关闭提示"
        }),
    )
    .on_hover_text(if waiting {
        "仅停止本地等待，不会撤销已发送的电源指令"
    } else {
        message
    })
    .clicked()
}

pub(super) struct PowerConfirmation {
    device: DeviceInfo,
    action: PowerAction,
    generation: u64,
}

impl DeviceCenterApp {
    pub(super) fn begin_power(&mut self, device: DeviceInfo, action: PowerAction) {
        if let Err(error) = self.power_available(&device, action) {
            self.status = StatusMessage::warning(error.to_string());
            return;
        }
        self.center_ui.power = Some(PowerConfirmation {
            device,
            action,
            generation: self.login_generation,
        });
    }

    pub(super) fn power_confirmation(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.center_ui.power.take() else {
            return;
        };
        if confirm.generation != self.login_generation || self.logout_pending {
            return;
        }
        let mut commit = false;
        let mut cancel = false;
        let issue = self.power_available(&confirm.device, confirm.action).err();
        let response = egui::Modal::new(egui::Id::new("device-power-confirmation"))
            .frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(450.0_f32.min(ctx.content_rect().width() - 60.0));
                ui.label(RichText::new(format!("{}这台设备？", confirm.action.label())).size(theme::DIALOG_TITLE).strong());
                ui.add_space(12.0);
                ui.add(egui::Label::new(RichText::new(display_alias(&confirm.device)).strong()).wrap());
                ui.label(RichText::new(&confirm.device.device_id).monospace().color(MUTED));
                ui.label(format!("{} · {}", confirm.device.platform_label(), confirm.device.status_label()));
                ui.add_space(12.0);
                match confirm.action {
                    PowerAction::Wake => { ui.label("发送远程唤醒请求，随后等待设备上线。需要远端已配置网络唤醒。"); }
                    PowerAction::Shutdown | PowerAction::Reboot => {
                        ui.colored_label(AMBER, "请确保已保存远端工作；未保存的应用可能被强制关闭，所有远控连接将断开。");
                        if confirm.device.participant_count() > 0 {
                            ui.label(format!("设备当前有 {} 个远控参与者。", confirm.device.participant_count()));
                        }
                        if self.active_session.as_ref().is_some_and(|s| s.device_id.as_ref().is_none_or(|id| id == &confirm.device.device_id)) {
                            ui.label("本程序会先结束当前观看并释放控制按键，再发送电源请求。");
                        }
                    }
                }
                if let Some(issue) = &issue { ui.colored_label(AMBER, issue.to_string()); }
                ui.add_space(18.0);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    let label = format!("确认{}", confirm.action.label());
                    let button = primary(&label);
                    let button = if confirm.action == PowerAction::Wake { button } else { button.fill(theme::DANGER_FILL) };
                    commit = ui.add_enabled(issue.is_none(), button).clicked();
                    cancel = ui.button("取消").clicked();
                });
            });
        if commit {
            self.queue_mutation(DeviceMutation::Power {
                device: confirm.device,
                action: confirm.action,
            });
        } else if !cancel && !response.should_close() {
            self.center_ui.power = Some(confirm);
        }
    }

    pub(super) fn power_results(&mut self, ui: &mut egui::Ui) {
        let mut dismiss = None;
        for (id, progress) in &self.power_progress {
            if self.center_ui.detail_id.as_deref() == Some(id) {
                continue;
            }
            egui::Frame::new()
                .fill(SURFACE)
                .corner_radius(crate::ui::theme::PANEL_RADIUS)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    let message =
                        format!("{} · {}", display_alias(&progress.device), progress.message);
                    if progress_row(ui, &message, progress.waiting, progress.elapsed()) {
                        dismiss = Some(id.clone());
                    }
                });
            ui.add_space(8.0);
        }
        if let Some(id) = dismiss {
            self.power_progress.remove(&id);
        }
    }

    pub(super) fn detail_power_result(&mut self, ui: &mut egui::Ui, id: &str) -> bool {
        let Some(progress) = self.power_progress.get(id) else {
            return false;
        };
        if progress_row(ui, &progress.message, progress.waiting, progress.elapsed()) {
            self.power_progress.remove(id);
        }
        true
    }
}
