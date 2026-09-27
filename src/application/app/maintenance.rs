//! Ordinary-user installation entry. Only component mutations request elevation.
use crate::platform::windows::{
    components::{self, Kind, Operation, application as deployment},
    host_service,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

pub(crate) fn route_gui() -> Result<bool> {
    cleanup_helpers();
    if !deployment::needs_handoff()? {
        return Ok(false);
    }
    let installed = deployment::image()?;
    if host_service::process::image_hash(&installed)?
        == host_service::process::image_hash(&std::env::current_exe()?)?
    {
        deployment::start_installed(std::env::args_os().skip(1))?;
    } else {
        let choice = run(false)?;
        if choice {
            deployment::start_installed(std::env::args_os().skip(1))?;
        }
    }
    Ok(true)
}
pub(crate) fn uninstall(parent: Option<u32>) -> Result<()> {
    if let Some(parent) = parent {
        use windows::Win32::{Foundation::*, System::Threading::*};
        ensure!(parent != std::process::id(), "无效卸载交接");
        match unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                parent,
            )
        } {
            Ok(handle) => {
                let handle = host_service::pipe::Handle(handle);
                let identity = (|| -> Result<()> {
                    host_service::process::verify_image(parent)?;
                    ensure!(
                        host_service::vault::sid(parent)?
                            == host_service::vault::sid(std::process::id())?,
                        "卸载交接用户不匹配"
                    );
                    Ok(())
                })();
                if unsafe { WaitForSingleObject(handle.0, 0) } != WAIT_OBJECT_0 {
                    identity?;
                }
                ensure!(
                    unsafe { WaitForSingleObject(handle.0, 30_000) } == WAIT_OBJECT_0,
                    "等待原程序退出超时"
                );
            }
            Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => (),
            Err(error) => return Err(error.into()),
        }
    } else if std::env::current_exe()?.parent() == Some(deployment::active_directory()?.as_path()) {
        use std::os::windows::process::CommandExt;
        let directory = helpers()?;
        components::files::reject_reparse(directory.parent().context("卸载工作目录无效")?)?;
        components::files::reject_reparse(&directory)?;
        std::fs::create_dir_all(&directory)?;
        let helper = directory.join(format!("uninstall-{}.exe", uuid::Uuid::new_v4().simple()));
        std::fs::copy(std::env::current_exe()?, &helper)?;
        std::process::Command::new(&helper)
            .args(["uninstall", "--parent", &std::process::id().to_string()])
            .creation_flags(0x08000000)
            .spawn()
            .context("启动卸载程序失败")?;
        return Ok(());
    }
    run(true)?;
    Ok(())
}
pub(crate) fn report_error(error: &anyhow::Error) {
    use windows::{
        Win32::UI::WindowsAndMessaging::*,
        core::{PCWSTR, w},
    };
    let text: Vec<u16> = format!("{error:#}").encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("OpenUUYC"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}
fn helpers() -> Result<PathBuf> {
    Ok(std::env::temp_dir().join("OpenUUYC-maintenance"))
}
fn cleanup_helpers() {
    let Ok(directory) = helpers() else { return };
    if components::files::reject_reparse(&directory).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix("uninstall-")
            .and_then(|s| s.strip_suffix(".exe"))
            .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            && components::files::reject_reparse(&entry.path()).is_ok()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
fn run(uninstall: bool) -> Result<bool> {
    let _installer = super::instance::reserve_installer()?;
    let _instance = if uninstall {
        Some(super::instance::reserve_maintenance()?)
    } else {
        None
    };
    if uninstall && deployment::active_directory()?.exists() {
        deployment::verify_directory(&deployment::active_directory()?)?;
    }
    let launch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output = launch.clone();
    let installed_version = deployment::installed_version().ok();
    crate::ui::run(
        crate::ui::WindowConfig {
            viewport: egui::ViewportBuilder::default()
                .with_title(if uninstall {
                    "卸载 OpenUUYC"
                } else {
                    "更新 OpenUUYC"
                })
                .with_icon(crate::ui::branding::icon())
                .with_inner_size(crate::ui::theme::MAINTENANCE_WINDOW_SIZE)
                .with_resizable(false),
            centered: true,
        },
        Box::new(move |ctx, _| {
            super::view::configure_visuals(ctx);
            Box::new(Maintenance {
                uninstall,
                removal: Default::default(),
                pending: None,
                error: None,
                finished: false,
                launch: output,
                installed_version,
            })
        }),
    )?;
    Ok(launch.load(std::sync::atomic::Ordering::Acquire))
}
struct Maintenance {
    uninstall: bool,
    removal: components::RemovalOptions,
    pending: Option<Receiver<Result<bool, String>>>,
    error: Option<String>,
    finished: bool,
    installed_version: Option<String>,
    launch: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl crate::ui::App for Maintenance {
    fn uses_tray(&self) -> bool {
        false
    }
    fn on_close_requested(&mut self) -> bool {
        self.pending.is_none()
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    match result {
                        Ok(false) => {
                            self.finished = true;
                            self.launch
                                .store(!self.uninstall, std::sync::atomic::Ordering::Release);
                            if !self.uninstall {
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                        Ok(true) => self.error = Some("需要重启Windows后完成操作".into()),
                        Err(error) => self.error = Some(error),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.error = Some("操作任务中断".into());
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        use crate::ui::{controls, theme};
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(theme::DIALOG_MARGIN),
            )
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(if self.uninstall {
                        "卸载 OpenUUYC"
                    } else {
                        "更新 OpenUUYC"
                    })
                    .size(theme::DIALOG_TITLE),
                );
                ui.add_space(theme::DIALOG_HEADER_GAP);
                if !self.uninstall {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("当前版本").color(theme::MUTED));
                        ui.label(
                            self.installed_version
                                .as_ref()
                                .map(|v| format!("v{v}"))
                                .unwrap_or_else(|| "未知".into()),
                        );
                        ui.label(egui::RichText::new("→").color(theme::MUTED));
                        ui.label(egui::RichText::new("本次版本").color(theme::MUTED));
                        ui.label(
                            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .color(theme::ACCENT),
                        );
                    });
                    ui.add_space(12.);
                }
                ui.label(if self.finished {
                    if self.removal.remove_data { "卸载完成，本机数据已清除。" } else { "卸载完成，账号和用户设置已保留。" }
                } else if self.uninstall {
                    "将移除程序、后台服务、输入驱动、快捷方式及自启动。"
                } else {
                    "更新将结束当前连接并关闭运行中的 OpenUUYC，完成后自动打开新版本。"
                });
                ui.add_space(12.);
                if !self.finished {
                    ui.add_enabled_ui(self.pending.is_none(), |ui| {
                        if self.uninstall {
                            ui.checkbox(&mut self.removal.remove_display_driver, "同时卸载 OpenUUYC 虚拟显示驱动");
                            ui.checkbox(&mut self.removal.remove_audio_driver, "同时卸载 OpenUUYC 虚拟声卡" );
                            ui.checkbox(&mut self.removal.remove_data, "同时删除本机 OpenUUYC 数据");
                            if self.removal.remove_data {
                                ui.label(egui::RichText::new("删除登录信息、设置、插件、缓存和日志，下次使用需重新登录。").color(theme::AMBER));
                            }
                        } else if ui.link("打开已安装版本").clicked() {
                            self.launch
                                .store(true, std::sync::atomic::Ordering::Release);
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                ui.add_space(12.);
                let available = ui.available_height();
                let body_height = (available
                    - theme::CONTROL_HEIGHT
                    - theme::DIALOG_ACTION_GAP
                    - ui.spacing().item_spacing.y)
                    .max(0.);
                ui.allocate_ui(egui::vec2(ui.available_width(), body_height), |ui| {
                    ui.set_min_height(body_height);
                    if let Some(error) = &self.error {
                        egui::ScrollArea::vertical()
                            .max_height(body_height)
                            .show(ui, |ui| {
                                ui.colored_label(theme::RED, error);
                            });
                    } else if self.pending.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("正在处理…");
                        });
                    }
                });
                if self.finished {
                    if controls::dialog_actions(ui, Some(controls::DialogAction::new("关闭")), None)
                        .0
                    {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                } else {
                    let (yes, cancel) = ui
                        .add_enabled_ui(self.pending.is_none(), |ui| {
                            controls::dialog_actions(
                                ui,
                                Some(controls::DialogAction::new(if self.uninstall {
                                    "卸载程序"
                                } else {
                                    "更新并打开"
                                })),
                                Some("取消"),
                            )
                        })
                        .inner;
                    if yes {
                        self.start(ui.ctx().clone());
                    }
                    if cancel {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            });
        if self.pending.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl Maintenance {
    fn start(&mut self, ctx: egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        let uninstall = self.uninstall;
        let removal = self.removal;
        std::thread::spawn(move || {
            let result = if !uninstall {
                update_running()
            } else {
                components::request(Kind::Application, Operation::Uninstall, false, removal)
            }
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
}

fn update_running() -> Result<bool> {
    let gate = super::instance::reserve_update()?;
    let running = super::instance::running_installed()?;
    let mut had_window = running.is_some();
    let background = host_service::resident::managed() && host_service::install::running()?;
    let resume = background && !host_service::resident::paused()?;
    let result = (|| -> Result<bool> {
        if let Some(running) = running {
            running.close()?;
        }
        // Headless and pre-handoff builds can leave their resident online after
        // the UI exits. Pause it and wait for its agents before deployment.
        if background {
            host_service::resident::call(host_service::resident::Request::Pause)?;
        }
        let started = std::time::Instant::now();
        let _reservation = loop {
            match super::instance::reserve_after_exit()? {
                Some(guard) => break guard,
                None if started.elapsed() < Duration::from_secs(5) => {
                    if let Some(running) = super::instance::running_installed()? {
                        had_window = true;
                        running.close()?;
                    }
                    std::thread::sleep(Duration::from_millis(50))
                }
                None => anyhow::bail!("运行版本仍在退出，尚未开始替换文件"),
            }
        };
        components::request(Kind::Suite, Operation::Install, false, Default::default())
    })();
    drop(gate);
    if let Err(error) = result {
        let restored = if had_window {
            deployment::start_installed(std::iter::empty::<std::ffi::OsString>())
        } else if resume {
            host_service::resident::call(host_service::resident::Request::Resume).map(|_| ())
        } else {
            Ok(())
        };
        return match restored {
            Ok(()) => Err(error),
            Err(recovery) => Err(anyhow::anyhow!(
                "{error:#}；恢复原运行状态失败：{recovery:#}"
            )),
        };
    }
    result
}
