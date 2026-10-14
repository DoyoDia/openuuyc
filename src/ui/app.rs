//! The model/view contract is independent of platform and renderer.
pub(crate) trait App {
    fn title_bar_alert(&self) -> Option<super::chrome::TitleBarAlert> {
        None
    }
    fn uses_tray(&self) -> bool {
        true
    }
    fn ui(&mut self, ui: &mut egui::Ui);
    /// 主窗口收进托盘期间的轮询：不渲染，只推进后台事件、通知卡片与通知按钮的
    /// 动作。默认什么都不做。
    fn tick(&mut self, _context: &egui::Context) {}
    fn on_focus_changed(&mut self, _focused: bool) {}
    /// Return false to keep the window alive (for example, while confirming exit).
    fn on_close_requested(&mut self) -> bool {
        true
    }
    fn exit_ready(&self) -> bool {
        false
    }
    fn on_exit(&mut self) {}
}

pub(crate) struct WindowConfig {
    pub viewport: egui::ViewportBuilder,
    pub centered: bool,
    pub notification: bool,
    /// No caption and no placement of its own: the app positions and shows
    /// the window itself (a drop zone that appears during a drag).
    pub floating: bool,
}

pub(crate) type AppFactory = Box<dyn FnOnce(&egui::Context, Option<String>) -> Box<dyn App> + Send>;

pub(super) struct AppSession(pub(super) Box<dyn App>);

impl Drop for AppSession {
    fn drop(&mut self) {
        self.0.on_exit();
    }
}
