//! Non-interactive rendering of the shared diagnostic health snapshot.
use super::hud::{compact_hud_line, compact_performance_frame, health_color};
use crate::diagnostics::performance::PerformanceSnapshot;
use std::time::Duration;

pub(super) fn show(ctx: &egui::Context, stats: &PerformanceSnapshot, id: &'static str) {
    ctx.request_repaint_after(Duration::from_millis(250));
    if stats.health.alerts.is_empty() {
        return;
    }
    egui::Window::new("性能异常")
        .id(egui::Id::new((id, "performance-alerts")))
        .interactable(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -44.0])
        .resizable(false)
        .collapsible(false)
        .title_bar(false)
        .frame(compact_performance_frame())
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for alert in &stats.health.alerts {
                let suffix = if alert.historical { "（刚才）" } else { "" };
                let color = if alert.historical {
                    crate::ui::theme::MUTED
                } else {
                    health_color(Some(alert.level))
                };
                compact_hud_line(
                    ui,
                    &format!(
                        "{} {}{}",
                        alert.metric.label(),
                        alert.metric.format(alert.value),
                        suffix
                    ),
                    color,
                );
            }
        });
}
