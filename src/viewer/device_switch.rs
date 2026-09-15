//! Local player navigation. Device selection never changes the UU control contract.
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use winit::window::WindowId;

use crate::{api::DeviceInfo, client::AuthenticatedClient};

pub(crate) struct SwitchRequest {
    pub from: String,
    pub device: DeviceInfo,
    pub window: WindowId,
}

#[derive(Clone)]
pub(crate) struct DeviceSwitcher {
    client: Arc<AuthenticatedClient>,
    current: String,
    sender: mpsc::Sender<SwitchRequest>,
    runtime: tokio::runtime::Handle,
    cancel: CancellationToken,
    state: Arc<Mutex<PickerState>>,
}

#[derive(Clone, Default)]
struct PickerState {
    devices: Vec<DeviceInfo>,
    loading: bool,
    switching: bool,
    error: Option<String>,
}

impl DeviceSwitcher {
    pub(crate) fn new(
        client: Arc<AuthenticatedClient>,
        current: String,
        sender: mpsc::Sender<SwitchRequest>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            client,
            current,
            sender,
            runtime: tokio::runtime::Handle::current(),
            cancel,
            state: Arc::new(Mutex::new(PickerState::default())),
        }
    }

    pub(crate) fn failed(&self, message: String) {
        let mut state = super::mutex_lock(&self.state);
        state.switching = false;
        state.error = Some(message);
    }

    pub(super) fn is_switching(&self) -> bool {
        super::mutex_lock(&self.state).switching
    }

    fn refresh(&self, ctx: &egui::Context) {
        {
            let mut state = super::mutex_lock(&self.state);
            if state.loading || state.switching {
                return;
            }
            state.loading = true;
            state.error = None;
        }
        let this = self.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let result = tokio::select! {
                _ = this.cancel.cancelled() => return,
                result = tokio::time::timeout(std::time::Duration::from_secs(15), this.client.list_devices()) => result,
            };
            let mut state = super::mutex_lock(&this.state);
            state.loading = false;
            match result {
                Ok(Ok(list)) => {
                    state.devices = list.my_binded_devices.into_iter().filter(|device| {
                        device.is_connected()
                            && matches!(device.platform, 1 | 4)
                            && device.device_id != list.current_device.device_id
                            && device.validated_device_id().is_ok()
                    }).collect();
                    state.devices.sort_by(|a, b| {
                        (a.device_id != this.current, &a.alias, &a.device_id)
                            .cmp(&(b.device_id != this.current, &b.alias, &b.device_id))
                    });
                    state.devices.dedup_by(|a, b| a.device_id == b.device_id);
                }
                Ok(Err(error)) => state.error = Some(format!("获取设备失败：{error}")),
                Err(_) => state.error = Some("获取设备超时，请重试".into()),
            }
            ctx.request_repaint();
        });
    }

    pub(super) fn menu(&self, response: &egui::Response, window: WindowId) {
        if response.clicked() {
            self.refresh(&response.ctx);
        }
        egui::Popup::menu(response)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .gap(6.0)
            .width(crate::ui::theme::DEVICE_MENU_WIDTH)
            .show(|ui| {
                super::stream_menu::menu_style(ui);
                ui.set_width(crate::ui::theme::DEVICE_MENU_WIDTH);
                let state = super::mutex_lock(&self.state).clone();
                if crate::ui::controls::device_menu_header(ui, state.loading || state.switching) {
                    self.refresh(ui.ctx());
                }
                ui.add_space(4.0);
                if let Some(error) = &state.error {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(error).color(ui.visuals().error_fg_color),
                        )
                        .wrap(),
                    );
                }
                if state.switching {
                    ui.weak("正在检查目标设备…");
                }
                if state.devices.is_empty() && !state.loading {
                    ui.weak("没有其他在线电脑");
                }
                // Reserve the actual row height after an asynchronous refresh.
                // Otherwise the popup's previous loading size constrains the scroll
                // viewport to its 64px minimum and clips even a three-device list.
                let row_height = crate::ui::theme::DEVICE_MENU_ROW_HEIGHT;
                let row_gap = crate::ui::theme::DEVICE_MENU_ROW_GAP;
                let list_height = (state.devices.len() as f32 * (row_height + row_gap) - row_gap)
                    .max(0.0)
                    .min(7.0 * (row_height + row_gap) - row_gap)
                    .min((ui.ctx().content_rect().height() - 120.0).max(row_height));
                if !state.devices.is_empty() {
                    egui::ScrollArea::vertical()
                        .min_scrolled_height(list_height)
                        .max_height(list_height)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = row_gap;
                            for device in &state.devices {
                                let selected = device.device_id == self.current;
                                let unavailable =
                                    if !device.controlled_support || !device.controllable {
                                        Some("暂不可连接")
                                    } else if device.participant_count() != 0 && !selected {
                                        Some("使用中")
                                    } else {
                                        None
                                    };
                                let alias = if device.alias.is_empty() {
                                    &device.device_id
                                } else {
                                    &device.alias
                                };
                                let duplicate = state
                                    .devices
                                    .iter()
                                    .filter(|d| d.alias == device.alias)
                                    .count()
                                    > 1;
                                let label = if duplicate {
                                    format!(
                                        "{} · {}",
                                        alias,
                                        &device.device_id[device.device_id.len() - 4..]
                                    )
                                } else {
                                    alias.to_owned()
                                };
                                let detail = unavailable.unwrap_or("");
                                let response = ui
                                    .push_id(&device.device_id, |ui| {
                                        crate::ui::controls::device_menu_row(
                                            ui,
                                            &label,
                                            detail,
                                            selected,
                                            !selected
                                                && unavailable.is_none()
                                                && !state.loading
                                                && !state.switching,
                                        )
                                    })
                                    .inner
                                    .on_hover_text(format!(
                                        "{} · {}",
                                        device.platform_label(),
                                        device.device_id
                                    ));
                                if response.clicked() {
                                    let mut state = super::mutex_lock(&self.state);
                                    if !state.switching {
                                        state.switching = true;
                                        state.error = None;
                                        if self
                                            .sender
                                            .try_send(SwitchRequest {
                                                from: self.current.clone(),
                                                device: device.clone(),
                                                window,
                                            })
                                            .is_err()
                                        {
                                            state.switching = false;
                                            state.error =
                                                Some("连接已结束，请重新打开观看窗口".into());
                                        }
                                    }
                                }
                            }
                        });
                }
            });
    }
}
