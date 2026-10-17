//! 终端会话页：列出被控端与本机正在跑的 shell，并接到本机终端里。
//!
//! 手机端 UU 打开「终端」时会话会出现在这里；本机也可以自己新建，不必等
//! 远程连接。页面不随本地终端关闭而结束会话。
use super::*;

impl DeviceCenterApp {
    pub(super) fn terminal_sessions_page(&mut self, ui: &mut egui::Ui) {
        use crate::features::host::terminal;
        let sessions = terminal::local_sessions();
        // 会话可能在另一头被改名、附着、结束，前台命令也在变，页面每秒刷一次。
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        ui.horizontal(|ui| {
            ui.label(RichText::new("终端会话").size(theme::TITLE).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.add(primary("新建会话")).clicked() {
                    match terminal::create_local(None) {
                        Ok(handle) => {
                            if let Err(error) = terminal::open_local_terminal(handle) {
                                self.status = StatusMessage::error(format!("{error:#}"));
                            }
                        }
                        Err(error) => self.status = StatusMessage::error(error.to_string()),
                    }
                }
                if ui
                    .add_enabled(
                        !sessions.is_empty(),
                        crate::ui::controls::secondary("结束全部"),
                    )
                    .clicked()
                {
                    terminal::end_all();
                }
            });
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "手机端 UU 打开「终端」后，shell 会出现在这里；也可以自己新建一个。\
                 点「在本地打开」用系统终端接上，本机键盘与手机端操作的是同一个 shell；\
                 关掉本地终端只会断开这一端，会话继续运行。",
            )
            .small()
            .color(MUTED),
        );
        ui.add_space(14.0);
        if sessions.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("现在没有终端会话").strong());
                ui.label(
                    RichText::new("点右上角「新建会话」，或在手机端连上本机后打开终端。")
                        .small()
                        .color(MUTED),
                );
            });
            return;
        }
        for session in sessions {
            let (state, state_color) = match session.state {
                "running" => ("运行中", GREEN),
                "detached" => ("已分离", MUTED),
                _ => ("已退出", RED),
            };
            crate::ui::controls::section_frame()
                .inner_margin(egui::Margin::symmetric(16, 12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&session.name).strong());
                        ui.label(RichText::new(state).small().color(state_color));
                        if session.remote {
                            ui.label(RichText::new("主控已连接").small().color(BLUE));
                        }
                        if session.local {
                            ui.label(RichText::new("本机已接入").small().color(GREEN));
                        }
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    session.state != "exited",
                                    crate::ui::controls::secondary("在本地打开"),
                                )
                                .clicked()
                                && let Err(error) = terminal::open_local_terminal(session.handle)
                            {
                                self.status = StatusMessage::error(format!("{error:#}"));
                            }
                            if ui.add(crate::ui::controls::secondary("结束")).clicked()
                                && !terminal::kill_local(session.handle)
                            {
                                self.status = StatusMessage::error("会话已经不存在");
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} · 编号 {} · 创建于 {}",
                                session.shell,
                                session.handle,
                                stamp(session.created_ms)
                            ))
                            .small()
                            .color(MUTED),
                        );
                        if let Some(foreground) = &session.foreground {
                            ui.label(
                                RichText::new(format!("正在运行 {}", foreground.name))
                                    .small()
                                    .color(AMBER),
                            )
                            .on_hover_text(&foreground.command);
                        } else if session.state != "exited" && terminal::foreground_supported() {
                            ui.label(RichText::new("空闲").small().color(MUTED));
                        }
                    });
                });
            ui.add_space(8.0);
        }
    }
}

/// 毫秒时间戳按本地时区显示，与诊断页的写法一致。
fn stamp(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "未知".into())
}
