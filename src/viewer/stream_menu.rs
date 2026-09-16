//! Viewer-only stream settings UI. Protocol decisions stay in stream_control.
use egui::{Align, FontId, RichText, Sense, Stroke, vec2};

use crate::media::FrameRateChoice;
use crate::stream_control::{MouseMode, StreamControlHandle, StreamControlSettings, StreamQuality};

mod display_menu;
pub(super) mod topology_menu;

pub(super) fn tab_display_menu(
    ui: &mut egui::Ui,
    handle: &StreamControlHandle,
    screen_id: i32,
    local_size: Option<(u32, u32)>,
) -> Option<String> {
    display_menu::context_menu(ui, handle, screen_id, local_size)
}

const WIDTH: f32 = 280.0;
const ROW_HEIGHT: f32 = crate::ui::theme::MENU_HEIGHT;
const ROW_GAP: f32 = 2.0;
const SECTION_GAP: f32 = 4.0;
use crate::ui::theme::{ACCENT, HOVER, LINE, MUTED, TEXT};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Quality,
    Custom,
    Mouse,
    Display,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Quality => "画质",
            Self::Custom => "自定义码率",
            Self::Mouse => "鼠标模式",
            Self::Display => "显示设置",
        }
    }
}

#[derive(Default)]
pub(super) struct StreamControlUi {
    pub(super) open: bool,
    page: Page,
    settings: Option<StreamControlSettings>,
    dirty: bool,
    display: display_menu::DisplayMenu,
    pub(super) local_error: Option<String>,
    format_confirm: Option<(i32, StreamControlSettings)>,
}

pub(super) struct LocalViewSettings {
    pub aspect_locked: bool,
    pub performance_mode: super::PerformancePanelMode,
    pub intercept_shortcuts: bool,
    pub send_ctrl_alt_del: bool,
}

enum Action {
    Apply,
    Color(bool),
    Hdr(bool),
}

#[derive(Clone, Copy)]
enum Icon {
    Back,
    Close,
    Info,
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, hint: &str) -> egui::Response {
    if matches!(icon, Icon::Close) {
        return crate::ui::controls::close_button(ui, hint, crate::ui::controls::COMPACT_HEIGHT);
    }
    let (rect, response) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    if response.hovered() && ui.is_enabled() {
        ui.painter().rect_filled(rect, 4.0, HOVER);
    }
    let center = rect.center();
    let stroke = Stroke::new(1.3, if response.hovered() { TEXT } else { MUTED });
    let point = |x, y| center + vec2(x, y);
    match icon {
        Icon::Back => {
            ui.painter()
                .line_segment([point(2.5, -5.0), point(-2.5, 0.0)], stroke);
            ui.painter()
                .line_segment([point(-2.5, 0.0), point(2.5, 5.0)], stroke);
        }
        Icon::Close => crate::ui::controls::paint_close(ui.painter(), rect, stroke.color),
        Icon::Info => {
            ui.painter().circle_stroke(center, 6.0, stroke);
            ui.painter()
                .circle_filled(point(0.0, -2.5), 0.8, stroke.color);
            ui.painter()
                .line_segment([point(0.0, 0.0), point(0.0, 3.0)], stroke);
        }
    }
    response.on_hover_text(hint)
}

fn separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0, LINE),
    );
}

fn section_separator(ui: &mut egui::Ui) {
    ui.add_space(SECTION_GAP);
    separator(ui);
    ui.add_space(SECTION_GAP);
}

use crate::ui::controls::menu_row;

fn switch_row(ui: &mut egui::Ui, label: &str, value: &mut bool) -> egui::Response {
    let enabled = ui.is_enabled();
    let (row, mut response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if enabled && response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, label)
    });
    if enabled && response.hovered() {
        ui.painter().rect_filled(row, 4.0, HOVER);
    }
    ui.painter().text(
        row.left_center() + vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(crate::ui::theme::COMPACT_TEXT),
        if enabled {
            TEXT
        } else {
            crate::ui::theme::DISABLED
        },
    );
    let rect = egui::Rect::from_center_size(row.right_center() - vec2(25.0, 0.0), vec2(34.0, 20.0));
    ui.painter().rect_filled(
        rect,
        10.0,
        if *value && enabled {
            ACCENT
        } else {
            crate::ui::theme::LINE
        },
    );
    let center = egui::pos2(
        if *value {
            rect.right() - 10.0
        } else {
            rect.left() + 10.0
        },
        rect.center().y,
    );
    ui.painter()
        .circle_filled(center, 7.0, crate::ui::theme::TEXT);
    response
}

fn speaker_button(ui: &mut egui::Ui, volume: u8, muted: &mut bool) {
    let (rect, mut response) = ui.allocate_exact_size(vec2(ROW_HEIGHT, ROW_HEIGHT), Sense::click());
    if response.clicked() {
        *muted = !*muted;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *muted, "静音")
    });
    if response.hovered() || response.has_focus() {
        ui.painter().rect_filled(rect, 4.0, HOVER);
    }
    let stage = if *muted || volume == 0 {
        0
    } else if volume <= 33 {
        1
    } else if volume <= 66 {
        2
    } else {
        3
    };
    let color = if stage == 0 { MUTED } else { TEXT };
    let stroke = Stroke::new(1.3, color);
    let center = rect.center();
    ui.painter().add(egui::Shape::closed_line(
        [
            (-9.0, -3.0),
            (-6.0, -3.0),
            (-2.0, -7.0),
            (-2.0, 7.0),
            (-6.0, 3.0),
            (-9.0, 3.0),
        ]
        .into_iter()
        .map(|(x, y)| center + vec2(x, y))
        .collect(),
        stroke,
    ));
    if stage == 0 {
        ui.painter()
            .line_segment([center + vec2(3.0, -3.0), center + vec2(9.0, 3.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(3.0, 3.0), center + vec2(9.0, -3.0)], stroke);
    } else {
        for arc in 0..stage {
            let radius = 6.0 + arc as f32 * 3.0;
            let points = (0..=10)
                .map(|step| {
                    let angle = -0.8 + step as f32 * 0.16;
                    center + vec2(-4.0 + radius * angle.cos(), radius * angle.sin())
                })
                .collect();
            ui.painter().add(egui::Shape::line(points, stroke));
        }
    }
    response.on_hover_text(if *muted { "取消静音" } else { "静音" });
}

#[derive(Clone, Copy)]
struct VolumeMeterMotion {
    levels: [f32; 2],
    time: f64,
}

fn volume_bar(
    ui: &mut egui::Ui,
    volume: &mut u8,
    muted: &mut bool,
    audio: &crate::audio::AudioPlayback,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
        speaker_button(ui, *volume, muted);
        ui.spacing_mut().slider_width = ui.available_width();
        ui.spacing_mut().interact_size.y = ROW_HEIGHT;
        let response = ui.add(
            egui::Slider::new(volume, 0..=100)
                .show_value(false)
                .smart_aim(false)
                .handle_shape(egui::style::HandleShape::Rect { aspect_ratio: 0.0 }),
        );
        let input = audio.input_levels().into_iter().fold(0.0_f32, f32::max);
        let output = if *muted || *volume == 0 {
            0.0
        } else {
            audio.output_levels().into_iter().fold(0.0_f32, f32::max)
        };
        let target = [input, output].map(|amplitude| {
            if amplitude > 0.0 {
                ((20.0 * amplitude.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
            } else {
                0.0
            }
        });
        let now = ui.input(|input| input.time);
        let levels = ui.ctx().data_mut(|data| {
            let id = response.id.with("volume-meter-motion");
            let mut motion = data
                .get_temp::<VolumeMeterMotion>(id)
                .unwrap_or(VolumeMeterMotion {
                    levels: target,
                    time: now,
                });
            let elapsed = now - motion.time;
            if !(0.0..=0.5).contains(&elapsed) {
                motion.levels = target;
            } else {
                for (shown, target) in motion.levels.iter_mut().zip(target) {
                    // Display-only ballistics. Gain and the audio callback do
                    // not wait for this visual attack/release.
                    let tau = if target > *shown { 0.12 } else { 0.45 };
                    *shown += (target - *shown) * (1.0 - (-elapsed / tau).exp()) as f32;
                    if target == 0.0 && *shown < 0.002 {
                        *shown = 0.0;
                    }
                }
            }
            if *muted || *volume == 0 {
                motion.levels[1] = 0.0;
            }
            motion.time = now;
            data.insert_temp(id, motion);
            motion.levels
        });
        let rect = response.rect;
        // Keep the full row as the hit target, but draw only a slim rail.
        ui.painter().rect_filled(rect, 0.0, crate::ui::theme::BG);
        let track = egui::Rect::from_center_size(rect.center(), vec2(rect.width(), 6.0));
        ui.painter()
            .rect_filled(track, 3.0, crate::ui::theme::SIDEBAR);
        for (level, color) in [
            (levels[0], crate::ui::theme::MUTED),
            (levels[1], crate::ui::theme::GREEN),
        ] {
            if level > 0.0 {
                let fill = egui::Rect::from_min_max(
                    track.min,
                    egui::pos2(track.left() + track.width() * level, track.bottom()),
                );
                ui.painter()
                    .with_clip_rect(fill.intersect(ui.clip_rect()))
                    .rect_filled(track, 3.0, color);
            }
        }
        let handle_x = (rect.left() + rect.width() * f32::from(*volume) / 100.0)
            .clamp(rect.left() + 2.0, rect.right() - 2.0);
        ui.painter().rect_filled(
            egui::Rect::from_center_size(egui::pos2(handle_x, rect.center().y), vec2(2.5, 14.0)),
            1.0,
            TEXT,
        );
        if response.hovered() || response.dragged() || response.has_focus() {
            ui.painter().rect_stroke(
                track,
                3.0,
                Stroke::new(1.0, ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        response
    })
    .inner
}

fn bitrate_editor(ui: &mut egui::Ui, value: &mut u32, multi_screen: bool, limit: u32) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(if multi_screen {
            "每屏码率"
        } else {
            "视频码率"
        });
        icon_button(
            ui,
            Icon::Info,
            if multi_screen {
                "同一上限分别应用到各屏幕；实际码率随内容和带宽变化，音频与重传另计。"
            } else {
                "使用UU的自定义码率设置；实际流量随画面内容和网络变化。"
            },
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(format!("{value}/{limit} Mbps"));
        });
    });
    ui.add_space(6.0);
    changed |= ui
        .scope(|ui| {
            ui.spacing_mut().slider_width = ui.available_width();
            ui.spacing_mut().slider_rail_height = 3.0;
            ui.spacing_mut().interact_size.y = 18.0;
            let choices = crate::stream_control::custom_bitrate_choices(limit);
            let mut index = choices.iter().rposition(|n| *n <= *value).unwrap_or(0);
            let response = ui.add(
                egui::Slider::new(&mut index, 0..=choices.len() - 1)
                    .show_value(false)
                    .trailing_fill(true)
                    .handle_shape(egui::style::HandleShape::Circle),
            );
            if response.changed() {
                *value = choices[index];
            }
            response.changed()
        })
        .inner;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("1")
                .size(crate::ui::theme::MICRO)
                .color(MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(limit.to_string())
                    .size(crate::ui::theme::MICRO)
                    .color(MUTED),
            );
        });
    });
    changed
}

pub(super) fn menu_style(ui: &mut egui::Ui) {
    let style = ui.style_mut();
    style.override_text_style = Some(egui::TextStyle::Body);
    style.spacing.item_spacing = vec2(6.0, ROW_GAP);
    crate::ui::controls::configure(style, ROW_HEIGHT);
    style.interaction.selectable_labels = false;
}

pub(super) fn show_stream_control_window(
    ctx: &egui::Context,
    handle: &StreamControlHandle,
    state: &mut StreamControlUi,
    view: &mut LocalViewSettings,
    screen_id: i32,
    local_size: Option<(u32, u32)>,
) {
    topology_menu::show(ctx, handle, local_size);
    topology_menu::show_error(ctx, handle);
    let snapshot = handle.snapshot();
    state.display.select_screen(screen_id);
    if let Some(notice) = snapshot.remote_notice {
        egui::Area::new(egui::Id::new("remote-session-notice"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -48.0])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(notice);
                });
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    let multi_screen = snapshot.screens.len() > 1;
    if !state.open {
        state.page = Page::Quality;
        state.settings = None;
        state.dirty = false;
        state.local_error = None;
        state.display.reset();
        state.format_confirm = None;
        return;
    }
    if !state.dirty && state.local_error.is_none() {
        state.settings = Some(snapshot.settings);
    }
    let mut settings = state.settings.unwrap_or(snapshot.settings);
    let mut open = state.open;
    let mut action = None;
    let mut back = false;
    let mut close = false;
    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        open = false;
    }
    egui::Window::new("串流画质")
        .id(egui::Id::new("runtime-stream-settings-window"))
        .open(&mut open)
        .collapsible(false)
        .auto_sized()
        .title_bar(false)
        .anchor(egui::Align2::RIGHT_TOP, [-114.0, 52.0])
        .default_width(WIDTH + 24.0)
        .frame(
            egui::Frame::new()
                .fill(crate::ui::theme::BG)
                .stroke(Stroke::new(1.0, LINE))
                .corner_radius(crate::ui::theme::PANEL_RADIUS)
                .inner_margin(egui::Margin::symmetric(12, 6)),
        )
        .show(ctx, |ui| {
            menu_style(ui);
            ui.set_width(WIDTH);
            ui.horizontal(|ui| {
                if state.page != Page::Quality {
                    back =
                        icon_button(ui, Icon::Back, "返回画质菜单；未应用的修改会取消").clicked();
                }
                let title = if state.page == Page::Display {
                    "显示设置".to_owned()
                } else if multi_screen {
                    format!("{} · 全部屏幕", state.page.title())
                } else {
                    state.page.title().to_owned()
                };
                ui.label(
                    RichText::new(title)
                        .size(crate::ui::theme::COMPACT_TEXT)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    close = icon_button(ui, Icon::Close, "关闭").clicked();
                });
            });
            ui.add_space(SECTION_GAP);
            ui.scope(|ui| {
                ui.set_width(WIDTH);
                match state.page {
                    Page::Display => {
                        egui::ScrollArea::vertical()
                            .max_height((ctx.content_rect().height() - 150.0).max(140.0))
                            .show(ui, |ui| {
                                state
                                    .display
                                    .draw(ui, handle, &snapshot, screen_id, local_size);
                            });
                    }
                    Page::Quality => {
                        egui::ScrollArea::vertical()
                            .id_salt("stream-settings-root")
                            .max_height((ctx.content_rect().height() - 150.0).max(140.0))
                            .show(ui, |ui| {
                                for (quality, label, detail) in [
                                    (
                                        StreamQuality::Auto,
                                        snapshot.auto_quality_label.as_str(),
                                        String::new(),
                                    ),
                                    (StreamQuality::Original, "原画", "30M".into()),
                                    (StreamQuality::High, "超清", "14M".into()),
                                    (StreamQuality::Clear, "高清", "8M".into()),
                                    (
                                        StreamQuality::Custom,
                                        "自定义码率",
                                        format!(
                                            "{} Mbps",
                                            settings
                                                .custom_bitrate_mbps
                                                .min(snapshot.custom_bitrate_limit)
                                        ),
                                    ),
                                ] {
                                    let more = matches!(quality, StreamQuality::Custom);
                                    let enabled = snapshot.ready
                                        && (!more || snapshot.custom_bitrate_supported);
                                    let response = menu_row(
                                        ui,
                                        label,
                                        &detail,
                                        Some(settings.quality == quality),
                                        enabled,
                                        more,
                                    );
                                    if response.clicked() {
                                        if more {
                                            state.page = Page::Custom;
                                            settings.quality = StreamQuality::Custom;
                                            settings.custom_bitrate_mbps = settings
                                                .custom_bitrate_mbps
                                                .min(snapshot.custom_bitrate_limit);
                                            state.dirty = settings != snapshot.settings;
                                        }
                                        if !more
                                            && (settings.quality != quality
                                                || state.local_error.is_some()
                                                || snapshot.last_error.is_some())
                                        {
                                            settings.quality = quality;
                                            action = Some(Action::Apply);
                                        }
                                    }
                                    if more && !snapshot.custom_bitrate_supported {
                                        response.on_hover_text("此被控端不支持自定义码率");
                                    }
                                }
                                ui.add_space(crate::ui::theme::MENU_GROUP_GAP);
                                if menu_row(
                                    ui,
                                    "显示器分辨率与 DPI",
                                    "当前屏幕",
                                    None,
                                    snapshot.display_settings_supported,
                                    true,
                                )
                                .clicked()
                                {
                                    state.page = Page::Display;
                                }
                                ui.add_space(crate::ui::theme::MENU_GROUP_GAP);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("帧率")
                                            .size(crate::ui::theme::TINY)
                                            .color(MUTED),
                                    )
                                    .on_hover_text(
                                        snapshot
                                            .last_notice
                                            .as_deref()
                                            .unwrap_or("实际帧率受被控端刷新率和画面内容影响"),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new("FPS")
                                                    .size(crate::ui::theme::MICRO)
                                                    .color(MUTED),
                                            );
                                        },
                                    );
                                });
                                ui.add_space(ROW_GAP);
                                let choices = FrameRateChoice::available(snapshot.local_display)
                                    .into_iter()
                                    .filter(|choice| *choice != FrameRateChoice::Auto)
                                    .collect::<Vec<_>>();
                                let width = (WIDTH
                                    - 6.0 * (choices.len().saturating_sub(1)) as f32)
                                    / choices.len().max(1) as f32;
                                ui.add_enabled_ui(snapshot.ready, |ui| {
                                    ui.horizontal(|ui| {
                                        for choice in choices {
                                            if ui
                                                .add_sized(
                                                    [width, ROW_HEIGHT],
                                                    egui::Button::new(
                                                        choice
                                                            .value(snapshot.local_display)
                                                            .to_string(),
                                                    )
                                                    .selected(
                                                        settings
                                                            .frame_rate
                                                            .value(snapshot.local_display)
                                                            == choice.value(snapshot.local_display),
                                                    ),
                                                )
                                                .clicked()
                                                && settings.frame_rate != choice
                                            {
                                                settings.frame_rate = choice;
                                                action = Some(Action::Apply);
                                            }
                                        }
                                    });
                                });
                                if snapshot.true_color_supported {
                                    ui.add_space(crate::ui::theme::MENU_GROUP_GAP);
                                    ui.label(
                                        RichText::new("色度采样")
                                            .size(crate::ui::theme::TINY)
                                            .color(MUTED),
                                    );
                                    ui.add_space(ROW_GAP);
                                    ui.add_enabled_ui(
                                        snapshot.ready && snapshot.pending_sequence.is_none(),
                                        |ui| {
                                            ui.horizontal(|ui| {
                                                for (enabled, label, hint) in [
                                                    (
                                                        true,
                                                        "YUV 4:4:4",
                                                        "保留完整色度细节，适合文字与图形",
                                                    ),
                                                    (
                                                        false,
                                                        "YUV 4:2:0",
                                                        "对色度降采样，减少传输与解码开销",
                                                    ),
                                                ] {
                                                    if ui
                                                        .add_sized(
                                                            [(WIDTH - 6.0) / 2.0, ROW_HEIGHT],
                                                            egui::Button::new(label).selected(
                                                                settings.true_color == enabled,
                                                            ),
                                                        )
                                                        .on_hover_text(hint)
                                                        .clicked()
                                                        && settings.true_color != enabled
                                                    {
                                                        action = Some(Action::Color(enabled));
                                                    }
                                                }
                                            });
                                        },
                                    );
                                }
                                if snapshot.hdr_supported {
                                    ui.add_space(SECTION_GAP);
                                    let mut hdr = settings.hdr;
                                    let enabled = snapshot.ready
                                        && snapshot.pending_sequence.is_none()
                                        && (hdr || snapshot.hdr_unavailable.is_none());
                                    let response = ui
                                        .add_enabled_ui(enabled, |ui| {
                                            switch_row(ui, "HDR", &mut hdr)
                                        })
                                        .inner;
                                    if response.changed() {
                                        action = Some(Action::Hdr(hdr));
                                    }
                                    response.on_hover_text(
                                        snapshot
                                            .hdr_unavailable
                                            .as_deref()
                                            .unwrap_or("高动态范围；需要双方屏幕已开启 HDR"),
                                    );
                                }
                                section_separator(ui);
                                let mut relay = snapshot.network.relay_enabled;
                                let response = ui
                                    .add_enabled_ui(snapshot.network.available, |ui| {
                                        switch_row(ui, "高速中转连接 Beta", &mut relay)
                                    })
                                    .inner;
                                if response.changed() {
                                    state.local_error = handle
                                        .set_relay_enabled(relay)
                                        .err()
                                        .map(|e| e.to_string());
                                }
                                response.on_hover_text(
                                    snapshot.network.unavailable_reason.unwrap_or(
                                        "仅本次连接生效；关闭后恢复自动选路，不保证一定直连",
                                    ),
                                );
                                switch_row(ui, "按比例缩放", &mut view.aspect_locked)
                                    .on_hover_text("仅当前播放窗口");
                                let mut monitoring =
                                    view.performance_mode != super::PerformancePanelMode::Hidden;
                                if switch_row(ui, "性能监控", &mut monitoring)
                                    .on_hover_text(format!(
                                        "仅当前播放窗口；切换快捷键：{}",
                                        crate::viewer_shortcuts::label(
                                            crate::viewer_shortcuts::Action::Performance
                                        )
                                    ))
                                    .changed()
                                {
                                    view.performance_mode = if monitoring {
                                        super::PerformancePanelMode::Compact
                                    } else {
                                        super::PerformancePanelMode::Hidden
                                    };
                                }
                                if monitoring {
                                    ui.horizontal(|ui| {
                                        for (mode, label) in [
                                            (super::PerformancePanelMode::Compact, "简洁"),
                                            (super::PerformancePanelMode::Detailed, "详细"),
                                        ] {
                                            if ui
                                                .add_sized(
                                                    [(WIDTH - 6.0) / 2.0, ROW_HEIGHT],
                                                    egui::Button::new(label)
                                                        .selected(view.performance_mode == mode),
                                                )
                                                .clicked()
                                            {
                                                view.performance_mode = mode;
                                            }
                                        }
                                    });
                                }
                                let mode_label = match snapshot.mouse_preference {
                                    MouseMode::Smart | MouseMode::View => "智能鼠标",
                                    MouseMode::Remote => "被控端鼠标",
                                    MouseMode::Local => "主控端鼠标",
                                };
                                if menu_row(ui, "鼠标模式", mode_label, None, true, true).clicked()
                                {
                                    state.page = Page::Mouse;
                                }
                                switch_row(ui, "拦截本机快捷键", &mut view.intercept_shortcuts)
                                    .on_hover_text("仅当前播放窗口。开启后，控制时优先将按键交给远端；关闭后允许本机快捷键响应。播放器自身快捷键始终保留。");
                                let can_send = snapshot.ready
                                    && !snapshot.mouse_pending
                                    && snapshot.mouse_mode != MouseMode::View
                                    && handle.mouse().keyboard_supported()
                                    && !handle.mouse().waiting_for_neutral();
                                if menu_row(ui, "发送 Ctrl+Alt+Del", "", None, can_send, false)
                                    .on_hover_text(if can_send {
                                        "打开远端 Windows 安全选项"
                                    } else {
                                        "请先开启 Windows 设备的键鼠控制并松开按键"
                                    })
                                    .clicked()
                                {
                                    view.send_ctrl_alt_del = true;
                                }
                                let audio = handle.audio();
                                let mut audio_settings = audio.settings();
                                let audio_status = audio.snapshot();
                                volume_bar(
                                    ui,
                                    &mut audio_settings.volume,
                                    &mut audio_settings.muted,
                                    &audio,
                                )
                                .on_hover_text(
                                    if audio_status.device.is_empty() && !audio_status.receiving {
                                        "等待音频"
                                    } else {
                                        "音量"
                                    },
                                );
                                audio.set_settings(audio_settings);
                                if let Some(error) = audio_status.error {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.colored_label(crate::ui::theme::AMBER, error);
                                        if ui.small_button("重试").clicked() {
                                            audio.retry();
                                        }
                                    });
                                }
                                if snapshot.network.pending {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label(
                                            RichText::new("正在切换线路…")
                                                .size(crate::ui::theme::TINY)
                                                .color(MUTED),
                                        );
                                    });
                                } else if let Some(notice) = snapshot.network.notice {
                                    ui.label(
                                        RichText::new(notice)
                                            .size(crate::ui::theme::TINY)
                                            .color(MUTED),
                                    );
                                }
                            });
                    }
                    Page::Mouse => {
                        for (mode, label, description) in [
                            (
                                MouseMode::Smart,
                                "智能鼠标",
                                "根据远端光标和软件状态切换输入方式。",
                            ),
                            (
                                MouseMode::Remote,
                                "使用被控端鼠标",
                                "更好的兼容性，光标随视频显示，受网络延迟影响。",
                            ),
                            (
                                MouseMode::Local,
                                "使用主控端鼠标",
                                "本地光标即时响应，部分游戏或软件可能不兼容。",
                            ),
                        ] {
                            if menu_row(
                                ui,
                                label,
                                "",
                                Some(snapshot.mouse_preference == mode),
                                snapshot.ready
                                    && snapshot.mouse_modes_supported
                                    && !snapshot.mouse_pending,
                                false,
                            )
                            .clicked()
                            {
                                state.local_error = handle
                                    .set_mouse_mode(mode)
                                    .err()
                                    .map(|error| error.to_string());
                            }
                            ui.add(
                                egui::Label::new(
                                    RichText::new(description)
                                        .size(crate::ui::theme::TINY)
                                        .color(MUTED),
                                )
                                .wrap(),
                            );
                            ui.add_space(7.0);
                        }
                        separator(ui);
                        ui.add_space(7.0);
                        ui.label(
                            RichText::new(format!(
                                "退出控制快捷键：{}",
                                crate::viewer_shortcuts::label(
                                    crate::viewer_shortcuts::Action::ReleaseMouse
                                )
                            ))
                            .size(crate::ui::theme::TINY)
                            .color(MUTED),
                        );
                    }
                    Page::Custom => {
                        ui.add_enabled_ui(snapshot.ready, |ui| {
                            state.dirty |= bitrate_editor(
                                ui,
                                &mut settings.custom_bitrate_mbps,
                                multi_screen,
                                snapshot.custom_bitrate_limit,
                            );
                            ui.add_space(16.0);
                            let can_apply = state.dirty
                                || state.local_error.is_some()
                                || snapshot.last_error.is_some();
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(if state.dirty { "尚未应用" } else { "" })
                                        .size(crate::ui::theme::TINY)
                                        .color(MUTED),
                                );
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    if ui
                                        .add_enabled(
                                            can_apply,
                                            egui::Button::new("应用")
                                                .fill(if can_apply { ACCENT } else { HOVER })
                                                .stroke(Stroke::NONE)
                                                .min_size(vec2(76.0, 30.0)),
                                        )
                                        .clicked()
                                    {
                                        action = Some(Action::Apply);
                                    }
                                });
                            });
                        });
                    }
                }
                if state.page != Page::Display {
                    if let Some(error) = state
                        .local_error
                        .as_ref()
                        .or(snapshot.mouse_error.as_ref())
                        .or(snapshot.cursor_error.as_ref())
                        .or(snapshot.last_error.as_ref())
                        .or(snapshot.network.error.as_ref())
                    {
                        ui.add_space(9.0);
                        ui.add(egui::Label::new(
                            RichText::new("设置未生效")
                                .size(crate::ui::theme::TINY)
                                .color(super::bad_color()),
                        ))
                        .on_hover_text(error);
                    } else if let Some(error) = snapshot.persistence_error.as_ref() {
                        ui.add_space(9.0);
                        ui.label(
                            RichText::new("设置未保存")
                                .size(crate::ui::theme::TINY)
                                .color(super::bad_color()),
                        )
                        .on_hover_text(error);
                    } else if let Some(waiting) = snapshot.waiting_for {
                        ui.add_space(9.0);
                        ui.label(
                            RichText::new("等待串流就绪…")
                                .size(crate::ui::theme::TINY)
                                .color(MUTED),
                        )
                        .on_hover_text(waiting);
                    } else if snapshot.pending_sequence.is_some() {
                        ui.add_space(9.0);
                        ui.label(
                            RichText::new("正在应用…")
                                .size(crate::ui::theme::TINY)
                                .color(MUTED),
                        );
                    }
                }
            });
        });
    if let Some((target, proposed)) = state.format_confirm {
        let current = handle.snapshot().settings;
        let response = egui::Modal::new(egui::Id::new("stream-format-confirmation"))
            .frame(crate::ui::controls::dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(340.0);
                ui.label(RichText::new("调整显示效果？").size(crate::ui::theme::SECTION));
                ui.add_space(12.0);
                ui.label("双方设备需要使用以下组合：");
                ui.label(format!(
                    "{} · {} · {}",
                    proposed.quality.label(),
                    if proposed.true_color {
                        "YUV 4:4:4"
                    } else {
                        "YUV 4:2:0"
                    },
                    if proposed.hdr { "HDR" } else { "SDR" }
                ));
                if proposed.quality != current.quality {
                    ui.label("画质会一并调整。");
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui.add(crate::ui::controls::secondary("取消")).clicked() {
                        state.format_confirm = None;
                    }
                    if ui.add(crate::ui::controls::primary("应用")).clicked() {
                        state.local_error = handle
                            .apply_format(target, proposed)
                            .err()
                            .map(|e| e.to_string());
                        state.format_confirm = None;
                        settings = handle.snapshot().settings;
                        state.dirty = false;
                    }
                });
            });
        if response.should_close() {
            state.format_confirm = None;
        }
    }
    if back {
        state.page = Page::Quality;
        state.display.reset();
        state.dirty = false;
        state.local_error = None;
        settings = snapshot.settings;
    } else if let Some(action) = action {
        let result = match action {
            Action::Apply if handle.frame_rate_needs_super_screen(settings) => {
                let (width, height, dpi) = topology_menu::local_parameters(ctx, handle, local_size);
                topology_menu::request(
                    ctx,
                    handle,
                    screen_id,
                    crate::stream_control::DisplayTopologyAction::FrameRate {
                        settings,
                        width,
                        height,
                        dpi,
                    },
                );
                settings = snapshot.settings;
                Ok(0)
            }
            Action::Apply => handle.apply(settings),
            Action::Color(enabled) | Action::Hdr(enabled) => {
                let hdr_change = matches!(action, Action::Hdr(_));
                match handle.propose_format(
                    (!hdr_change).then_some(enabled),
                    hdr_change.then_some(enabled),
                ) {
                    Ok(proposed) => {
                        let current = handle.snapshot().settings;
                        let extra_change = proposed.quality != current.quality
                            || (hdr_change && proposed.true_color != current.true_color)
                            || (!hdr_change && proposed.hdr != current.hdr);
                        if extra_change {
                            state.format_confirm = Some((screen_id, proposed));
                            Ok(0)
                        } else {
                            let result = handle.apply_format(screen_id, proposed);
                            settings = handle.snapshot().settings;
                            result
                        }
                    }
                    Err(error) => Err(error),
                }
            }
        };
        match result {
            Ok(sequence) => {
                state.local_error = None;
                state.dirty = false;
                tracing::info!(
                    sequence,
                    ?settings,
                    "runtime stream settings requested from GUI"
                );
            }
            Err(error) => state.local_error = Some(error.to_string()),
        }
    }
    state.settings = Some(settings);
    state.open = open && !close;
}
