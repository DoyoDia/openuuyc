//! Windows toast delivery, activation registration and notification placement.

use anyhow::{Context, Result};
use windows::Win32::{Graphics::Gdi::*, UI::WindowsAndMessaging::GetForegroundWindow};
use winit::platform::windows::WindowExtWindows;

mod registration;
pub(crate) use registration::{owns_shortcut, unregister};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, mpsc};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::{DateTime, IReference, PropertyValue};
use windows::UI::Notifications::{
    NotificationSetting, ToastNotification, ToastNotificationManager, ToastNotifier,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{HSTRING, Interface};
const AUMID: &str = "OpenUUYC.RemoteAccess";
const GROUP: &str = "remote-access";

type Action = crate::application::app::notifications::Action;
fn activation_sink() -> &'static Mutex<Option<mpsc::Sender<Action>>> {
    static SINK: OnceLock<Mutex<Option<mpsc::Sender<Action>>>> = OnceLock::new();
    SINK.get_or_init(Mutex::default)
}
pub(crate) fn set_activation_sink(sender: Option<mpsc::Sender<Action>>) {
    *activation_sink().lock().unwrap_or_else(|p| p.into_inner()) = sender;
}
pub(crate) fn receive_activation(uri: &str) -> bool {
    let Ok(action) = Action::parse(uri) else {
        return false;
    };
    activation_sink()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|s| s.send(action).is_ok())
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ToastAction {
    pub label: String,
    pub uri: String,
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Toast {
    pub key: String,
    pub title: String,
    pub body: String,
    pub expires_at: i64,
    pub buttons: Vec<ToastAction>,
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn xml(toast: &Toast) -> String {
    let uri = crate::application::app::notifications::Action::uri(
        &toast.key,
        crate::application::app::notifications::Verb::Open,
    );
    let actions = toast
        .buttons
        .iter()
        .map(|a| {
            format!(
                "<action content=\"{}\" arguments=\"{}\" activationType=\"protocol\"/>",
                escape(&a.label),
                escape(&a.uri)
            )
        })
        .collect::<String>();
    format!(
        "<toast activationType=\"protocol\" launch=\"{}\" duration=\"long\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><actions>{actions}</actions><audio silent=\"true\"/></toast>",
        escape(&uri),
        escape(&toast.title),
        escape(&toast.body)
    )
}
struct Apartment(std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() }
    }
}
pub(crate) struct Native {
    notifier: ToastNotifier,
    shown: HashMap<String, (Toast, ToastNotification)>,
    _apartment: Apartment,
}
impl Native {
    pub fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        let apartment = Apartment(Default::default());
        registration::register(AUMID).context("登记 Windows 通知身份失败")?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))
            .context("创建 Windows 通知发送器失败")?;

        Ok(Self {
            notifier,
            shown: HashMap::new(),
            _apartment: apartment,
        })
    }
    pub fn synchronize(&mut self, items: &[Toast]) -> Result<()> {
        let remove = self
            .shown
            .keys()
            .filter(|key| !items.iter().any(|i| &i.key == *key))
            .cloned()
            .collect::<Vec<_>>();
        for key in remove {
            self.remove(&key)?;
        }
        for item in items {
            if self
                .shown
                .get(&item.key)
                .is_some_and(|(old, _)| old == item)
            {
                continue;
            }
            anyhow::ensure!(
                item.key.len() == 32 && item.key.bytes().all(|b| b.is_ascii_hexdigit()),
                "无效的通知标识"
            );
            let document = XmlDocument::new()?;
            document.LoadXml(&HSTRING::from(xml(item)))?;
            let notification = ToastNotification::CreateToastNotification(&document)?;
            notification.SetTag(&HSTRING::from(&item.key[..16]))?;
            notification.SetGroup(&HSTRING::from(GROUP))?;
            notification.SetSuppressPopup(self.shown.contains_key(&item.key))?;
            if item.expires_at > 0 {
                let date = DateTime {
                    UniversalTime: (item.expires_at + 11_644_473_600) * 10_000_000,
                };
                let value: IReference<DateTime> = PropertyValue::CreateDateTime(date)?.cast()?;
                notification.SetExpirationTime(&value)?;
            }
            // A newly registered unpackaged identity has no notification settings
            // entry until its first Show. Windows still applies global policies.
            match self.notifier.Setting() {
                Ok(value) => anyhow::ensure!(
                    value == NotificationSetting::Enabled,
                    "Windows 已禁用此程序的通知，请检查系统通知设置"
                ),
                Err(error) if error.code() == windows::core::HRESULT::from_win32(1168) => (),
                Err(error) => return Err(error).context("查询 Windows 通知设置失败"),
            }
            self.notifier
                .Show(&notification)
                .context("显示 Windows 通知失败")?;
            self.shown
                .insert(item.key.clone(), (item.clone(), notification));
        }
        Ok(())
    }
    fn remove(&mut self, key: &str) -> Result<()> {
        if let Some((_, notification)) = self.shown.get(key) {
            self.notifier.Hide(notification)?;
        }
        ToastNotificationManager::History()?.RemoveGroupedTagWithId(
            &HSTRING::from(&key[..16]),
            &HSTRING::from(GROUP),
            &HSTRING::from(AUMID),
        )?;
        self.shown.remove(key);
        Ok(())
    }
    pub fn clear(&mut self) -> Result<()> {
        let keys = self.shown.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.remove(&key)?;
        }
        Ok(())
    }
}

pub(crate) fn place(window: &winit::window::Window, initial: bool) -> Result<()> {
    let anchor = if initial {
        unsafe { GetForegroundWindow() }
    } else {
        crate::platform::graphics::window_hwnd(window)?
    };
    let monitor = unsafe { MonitorFromWindow(anchor, MONITOR_DEFAULTTOPRIMARY) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(monitor, &mut info).ok()?;
    }
    window.set_skip_taskbar(true);
    window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
    let size = window.outer_size();
    let margin =
        (crate::ui::theme::NOTIFICATION_MARGIN * window.scale_factor() as f32).round() as i32;
    window.set_outer_position(winit::dpi::PhysicalPosition::new(
        (info.rcWork.right - size.width as i32 - margin).max(info.rcWork.left),
        (info.rcWork.bottom - size.height as i32 - margin).max(info.rcWork.top),
    ));
    Ok(())
}
