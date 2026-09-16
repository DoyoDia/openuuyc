//! The model/view contract is independent of platform and renderer.
pub(crate) trait App {
    fn ui(&mut self, ui: &mut egui::Ui);
    fn on_focus_changed(&mut self, _focused: bool) {}
    /// Return false to keep the window alive (for example, while confirming exit).
    fn on_close_requested(&mut self) -> bool {
        true
    }
    fn on_exit(&mut self) {}
}

pub(crate) struct WindowConfig {
    pub viewport: egui::ViewportBuilder,
    pub centered: bool,
}

pub(crate) type AppFactory = Box<dyn FnOnce(&egui::Context, Option<String>) -> Box<dyn App> + Send>;

pub(super) struct AppSession(pub(super) Box<dyn App>);

impl Drop for AppSession {
    fn drop(&mut self) {
        self.0.on_exit();
    }
}
