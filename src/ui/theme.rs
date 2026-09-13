//! Shared visual language for every native UI surface.
//! Layout is page-specific; palette, typography and control density live here.
use egui::{Color32, FontId, Stroke};

pub const BG: Color32 = Color32::from_rgb(22, 26, 33);
pub const SIDEBAR: Color32 = Color32::from_rgb(16, 20, 26);
pub const SURFACE: Color32 = Color32::from_rgb(31, 37, 47);
pub const LINE: Color32 = Color32::from_rgb(45, 53, 66);
pub const TEXT: Color32 = Color32::from_rgb(231, 235, 242);
pub const MUTED: Color32 = Color32::from_rgb(156, 167, 184);
pub const ACCENT: Color32 = Color32::from_rgb(75, 136, 235);
pub const GREEN: Color32 = Color32::from_rgb(102, 207, 156);
pub const AMBER: Color32 = Color32::from_rgb(238, 190, 111);
pub const RED: Color32 = Color32::from_rgb(241, 125, 132);
pub const DANGER_FILL: Color32 = Color32::from_rgb(161, 56, 67);
pub const HOVER: Color32 = Color32::from_rgb(44, 54, 69);
pub const BORDER_FOCUS: Color32 = Color32::from_rgb(81, 107, 142);
pub const SELECTED: Color32 = Color32::from_rgb(33, 51, 77);
pub const SELECTION: Color32 = Color32::from_rgb(40, 70, 112);
pub const ACTIVE: Color32 = Color32::from_rgb(36, 60, 92);
pub const DISABLED: Color32 = Color32::from_gray(90);
pub const WARNING_BG: Color32 = Color32::from_rgb(42, 35, 33);

pub const BODY: f32 = 14.0;
pub const COMPACT_TEXT: f32 = 13.0;
pub const SMALL: f32 = 12.0;
pub const TINY: f32 = 11.0;
pub const MICRO: f32 = 10.0;
pub const SECTION: f32 = 16.0;
pub const DIALOG_TITLE: f32 = 21.0;
pub const TITLE: f32 = 25.0;
pub const BRAND: f32 = 17.0;
pub const CONTROL_HEIGHT: f32 = 34.0;
pub const COMPACT_HEIGHT: f32 = 26.0;
pub const MENU_HEIGHT: f32 = 28.0;
pub const NAV_HEIGHT: f32 = 36.0;
pub const SIDEBAR_WIDTH: f32 = 188.0;
pub const CONTROL_RADIUS: u8 = 5;
pub const PANEL_RADIUS: u8 = 8;

pub fn typography(style: &mut egui::Style, height: f32) {
    let size = if height < CONTROL_HEIGHT {
        COMPACT_TEXT
    } else {
        BODY
    };
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(size));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(size));
    style
        .text_styles
        .insert(egui::TextStyle::Small, FontId::proportional(SMALL));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, FontId::proportional(TITLE));
}

pub fn configure(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = BG;
    style.visuals.extreme_bg_color = SIDEBAR;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.error_fg_color = RED;
    style.visuals.warn_fg_color = AMBER;
    style.visuals.selection.bg_fill = SELECTION;
    style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    style.visuals.window_corner_radius = PANEL_RADIUS.into();
    super::controls::configure(&mut style, CONTROL_HEIGHT);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.interaction.selectable_labels = false;
    ctx.set_style_of(egui::Theme::Dark, style);
}

/// Graph colors communicate different node/port meanings within the same theme.
pub mod graph {
    use egui::Color32;
    pub const SOURCE: Color32 = Color32::from_rgb(77, 190, 178);
    pub const VIDEO: Color32 = Color32::from_rgb(101, 161, 240);
    pub const OVERLAY: Color32 = Color32::from_rgb(183, 136, 235);
    pub const INPUT: Color32 = Color32::from_rgb(232, 128, 137);
    pub const ANALYSIS: Color32 = Color32::from_rgb(225, 180, 91);
    pub const PORT_FRAME: Color32 = Color32::from_rgb(90, 160, 255);
    pub const PORT_DRAW: Color32 = Color32::from_rgb(100, 215, 150);
    pub const PORT_LAYER: Color32 = Color32::from_rgb(205, 145, 255);
    pub const PORT_DETECTIONS: Color32 = Color32::from_rgb(240, 185, 80);
    pub const PORT_INPUT: Color32 = Color32::from_rgb(240, 110, 110);
    pub const PORT_ACTIVATION: Color32 = Color32::from_rgb(240, 210, 110);
}
