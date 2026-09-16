//! Explicit, single-use authorization for taking over one owned computer.
use super::*;
use crate::api::DeviceInfo;

pub(crate) struct Approval {
    device_id: String,
}

impl Approval {
    pub(crate) fn confirmed(device: &DeviceInfo) -> Self {
        Self {
            device_id: device.device_id.clone(),
        }
    }

    pub(super) fn permits(&self, device_id: &str) -> bool {
        self.device_id == device_id
    }

    pub(super) async fn verify(self, client: &AuthenticatedClient, target: &str) -> Result<bool> {
        anyhow::ensure!(self.permits(target), "接管确认对应的设备已变化，请重新选择");
        let list = client.list_devices().await?;
        self.verify_list(&list, target)
    }

    fn verify_list(self, list: &crate::api::DeviceList, target: &str) -> Result<bool> {
        anyhow::ensure!(self.permits(target), "接管确认对应的设备已变化，请重新选择");
        anyhow::ensure!(
            target != list.current_device.device_id,
            "不能接管本机观看身份"
        );
        let device = list
            .my_binded_devices
            .iter()
            .find(|d| d.device_id == target)
            .context("该设备已不在当前账号中，请刷新后重试")?;
        device.validated_device_id()?;
        anyhow::ensure!(matches!(device.platform, 1 | 4), "此设备类型不支持接管");
        anyhow::ensure!(device.is_connected(), "设备已离线");
        anyhow::ensure!(
            device.controlled_support && device.controllable,
            "设备当前未开放远程控制"
        );
        Ok(device.participant_count() > 0)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Required(pub DeviceInfo);

impl std::fmt::Display for Required {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("设备正在被其他设备使用，需要确认后才能接管")
    }
}

impl std::error::Error for Required {}

pub(crate) fn confirmation(ctx: &egui::Context, device: &DeviceInfo) -> Option<bool> {
    let mut choice = None;
    let response = egui::Modal::new(egui::Id::new(("takeover-device", &device.device_id)))
        .frame(crate::ui::controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(crate::ui::theme::TAKEOVER_DIALOG_WIDTH);
            ui.label(
                egui::RichText::new("接管设备？")
                    .size(crate::ui::theme::DIALOG_TITLE)
                    .strong(),
            );
            ui.add_space(14.0);
            let alias = if device.alias.is_empty() {
                "未命名设备"
            } else {
                &device.alias
            };
            ui.label(format!(
                "{alias} 正在被其他设备使用。强制接管将断开当前连接，并由本客户端连接。"
            ));
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(format!("设备 ID：{}", device.device_id))
                    .size(crate::ui::theme::SMALL)
                    .color(crate::ui::theme::MUTED),
            );
            ui.add_space(24.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(crate::ui::controls::primary("强制接管")).clicked() {
                    choice = Some(true);
                }
                if ui.add(crate::ui::controls::secondary("取消")).clicked() {
                    choice = Some(false);
                }
            });
        });
    choice.or_else(|| response.should_close().then_some(false))
}
