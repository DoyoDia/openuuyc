//! Login startup switch. The system's own startup entry is the setting, so
//! the switch shows what the system will do, read back after every change.
use super::*;
use crate::platform::host_service;
use std::time::{Duration, Instant};

/// Reading the entry and the install mode touches the registry/filesystem,
/// so the page rereads them at most this often.
const REFRESH: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct ViewState {
    checked: Option<Instant>,
    enabled: Option<Result<bool, String>>,
    /// The resident service keeps the device online before login on its own.
    managed: bool,
}
impl ViewState {
    fn refresh(&mut self) {
        self.enabled = Some(host_service::startup::enabled().map_err(|error| format!("{error:#}")));
        self.managed = host_service::resident::managed();
        self.checked = Some(Instant::now());
    }
}

impl DeviceCenterApp {
    pub(super) fn startup_settings(&mut self, ui: &mut egui::Ui) {
        let state = &mut self.center_ui.startup;
        if state.checked.is_none_or(|at| at.elapsed() >= REFRESH) {
            state.refresh();
        }
        ui.ctx().request_repaint_after(REFRESH);
        let readable = matches!(state.enabled, Some(Ok(_)));
        let mut enabled = matches!(state.enabled, Some(Ok(true)));
        let managed = state.managed;
        // Installing or removing components rewrites this entry.
        let busy = self.center_ui.components.busy();
        section(ui, "启动");
        let mut changed = false;
        form_row(
            ui,
            "开机自启",
            if managed {
                "登录后在托盘启动；常驻服务不受此项影响，开机即可被控"
            } else {
                "登录系统后在托盘启动"
            },
            |ui| {
                changed = ui
                    .add_enabled_ui(readable && !busy, |ui| {
                        crate::ui::controls::switch(ui, &mut enabled)
                    })
                    .inner
                    .changed();
            },
        );
        if changed {
            if let Err(error) = host_service::startup::configure(enabled) {
                self.status = StatusMessage::error(format!("开机自启设置失败：{error:#}"));
            }
            self.center_ui.startup.refresh();
        }
        let wol_enabled = self
            .host
            .as_ref()
            .filter(|host| !host.is_guest())
            .is_some_and(|host| host.status().wol_setup.enabled == Some(true));
        match &self.center_ui.startup.enabled {
            Some(Err(error)) => {
                ui.colored_label(AMBER, format!("无法读取开机自启设置：{error}"));
            }
            Some(Ok(false)) if wol_enabled && !managed => {
                ui.colored_label(
                    AMBER,
                    "已开启远程开机：关闭自启后，被唤醒的电脑需登录并打开 OpenUUYC 才会上线。",
                );
            }
            _ => {}
        }
    }
}
