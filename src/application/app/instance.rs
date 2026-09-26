//! One control center per Windows login session, independent of the EXE name.
use anyhow::{Context, Result};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, EnumWindows, GetPropW, GetWindowThreadProcessId, MB_ICONERROR, MB_OK,
    MB_SETFOREGROUND, MessageBoxW, PostMessageW, RegisterWindowMessageW, RemovePropW, SetPropW,
    WM_NCDESTROY,
};
use windows::core::{BOOL, PCWSTR, w};

pub struct Instance(HANDLE);
const SHOW_SUBCLASS: usize = 0x4f554943;

fn show_message() -> u32 {
    static MESSAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe { RegisterWindowMessageW(w!("OpenUUYC.ControlCenter.Show.v1")) })
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn acquire_named(name: PCWSTR) -> Result<Option<Instance>> {
    // Object lifetime is the reservation; no thread ownership or abandoned lock remains.
    let handle = unsafe { CreateMutexW(None, false, name) }.context("创建程序实例保护失败")?;
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let instance = Instance(handle);
    if exists {
        drop(instance);
        Ok(None)
    } else {
        Ok(Some(instance))
    }
}

pub fn acquire() -> Result<Option<Instance>> {
    let result = acquire_named(w!("Local\\OpenUUYC.ControlCenter.v1"));
    let message = match &result {
        Ok(Some(_)) => return result,
        Ok(None) => {
            activate_existing()?;
            return result;
        }
        Err(error) => format!("无法启动 OpenUUYC：{error:#}"),
    };
    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("OpenUUYC"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
    result
}

pub(crate) fn register_window(hwnd: HWND) -> Result<()> {
    anyhow::ensure!(show_message() != 0, "注册控制中心恢复消息失败");
    unsafe {
        SetWindowSubclass(hwnd, Some(window_message), SHOW_SUBCLASS, 0).ok()?;
    }
    let result = unsafe {
        SetPropW(
            hwnd,
            w!("OpenUUYC.ControlCenter.Window.v1"),
            Some(HANDLE(hwnd.0)),
        )
    }
    .context("标记控制中心窗口失败");
    if result.is_err() {
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(window_message), SHOW_SUBCLASS);
        }
    }
    result
}

unsafe extern "system" fn window_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == show_message() {
        // Updating visibility through the UI owner keeps winit's WindowFlags in
        // sync. Showing the HWND directly leaves VISIBLE false after tray hide.
        let _ = crate::ui::window_manager::send(crate::ui::window_manager::Request::ShowMain);
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        unsafe {
            let _ = RemovePropW(hwnd, w!("OpenUUYC.ControlCenter.Window.v1"));
            let _ = RemoveWindowSubclass(hwnd, Some(window_message), SHOW_SUBCLASS);
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
pub(crate) fn reserve_maintenance() -> Result<Instance> {
    acquire_named(w!("Local\\OpenUUYC.ControlCenter.v1"))?
        .context("请先从托盘退出 OpenUUYC，再安装更新或卸载程序")
}

fn activate_existing() -> Result<()> {
    unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> BOOL {
        if !unsafe { GetPropW(hwnd, w!("OpenUUYC.ControlCenter.Window.v1")) }.is_invalid() {
            unsafe {
                *(data.0 as *mut Option<HWND>) = Some(hwnd);
            }
            return false.into();
        }
        true.into()
    }
    let mut window: Option<HWND> = None;
    unsafe {
        let _ = EnumWindows(Some(find), LPARAM(&mut window as *mut _ as isize));
        if let Some(hwnd) = window {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
            let message = show_message();
            anyhow::ensure!(message != 0, "注册控制中心恢复消息失败");
            PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0))?;
        }
    }
    Ok(())
}
