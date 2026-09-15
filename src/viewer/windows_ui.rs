//! UI composition stays on the window thread and never submits a video frame.
//! UU hands a separate native video window to streamer. DirectComposition gives
//! our egui UI the equivalent independent layer above that child window.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{COLORREF, GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, DC_BRUSH, FillRect, GetDC, GetStockObject, HBRUSH, HDC, ReleaseDC, SetDCBrushColor,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;
use winit::dpi::PhysicalSize;
use winit::window::Window;

use super::windows_presenter::title_bar_height_pixels;
use crate::ui::d3d11::window_hwnd;
pub(super) use crate::ui::d3d11::{UiPresenter, UiTimingAudit};

const BACKDROP_SUBCLASS: usize = 0x4f555542;

fn video_window_class() -> Result<()> {
    static REGISTERED: std::sync::OnceLock<std::result::Result<(), u32>> =
        std::sync::OnceLock::new();
    let module = unsafe { GetModuleHandleW(None) }?;
    let result = REGISTERED.get_or_init(|| unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(video_window_proc),
            hInstance: module.into(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: w!("OpenUUYC.VideoSurface"),
            ..Default::default()
        };
        if RegisterClassW(&class) != 0 {
            Ok(())
        } else {
            Err(GetLastError().0)
        }
    });
    result.map_err(|code| anyhow::anyhow!("register native video surface: Windows error {code}"))
}

unsafe extern "system" fn video_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if matches!(message, WM_ERASEBKGND | WM_PRINTCLIENT) {
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_ok() {
                FillRect(
                    HDC(wparam.0 as _),
                    &rect,
                    HBRUSH(GetStockObject(BLACK_BRUSH).0),
                );
            }
            return LRESULT(1);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub(super) fn prepare_window_background(window: &Window) -> Result<()> {
    anyhow::ensure!(
        unsafe {
            SetWindowSubclass(
                window_hwnd(window)?,
                Some(backdrop_proc),
                BACKDROP_SUBCLASS,
                0,
            )
            .as_bool()
        },
        "install player window background"
    );
    unsafe {
        let hwnd = window_hwnd(window)?;
        let dc = GetDC(Some(hwnd));
        paint_backdrop(hwnd, dc);
        ReleaseDC(Some(hwnd), dc);
    }
    Ok(())
}

unsafe extern "system" fn backdrop_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        match message {
            WM_ERASEBKGND => {
                paint_backdrop(hwnd, HDC(wparam.0 as _));
                return LRESULT(1);
            }
            WM_NCDESTROY => {
                let _ = RemoveWindowSubclass(hwnd, Some(backdrop_proc), id);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}

unsafe fn paint_backdrop(hwnd: HWND, dc: HDC) {
    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_ok() {
            let color = crate::ui::theme::BG;
            let old = SetDCBrushColor(
                dc,
                COLORREF(
                    u32::from(color.r())
                        | (u32::from(color.g()) << 8)
                        | (u32::from(color.b()) << 16),
                ),
            );
            FillRect(dc, &rect, HBRUSH(GetStockObject(DC_BRUSH).0));
            SetDCBrushColor(dc, old);
        }
    }
}

pub(super) struct VideoWindow {
    hwnd: HWND,
}

impl VideoWindow {
    pub(super) fn new(parent: &Window) -> Result<Self> {
        video_window_class()?;
        // A disabled child is skipped by hit testing, so the parent continues
        // to receive all local UI input, including menus drawn above the video.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("OpenUUYC.VideoSurface"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_DISABLED | WS_CLIPSIBLINGS,
                0,
                0,
                1,
                1,
                Some(window_hwnd(parent)?),
                None,
                Some(GetModuleHandleW(None)?.into()),
                None,
            )
        }
        .context("create native video child window")?;
        let child = Self { hwnd };
        child.resize(parent, parent.inner_size())?;
        tracing::info!(
            parent = window_hwnd(parent)?.0 as usize,
            video = hwnd.0 as usize,
            "created independent UI and video presentation targets"
        );
        Ok(child)
    }

    pub(super) fn handle(&self) -> isize {
        self.hwnd.0 as isize
    }

    pub(super) fn resize(
        &self,
        parent: &Window,
        size: PhysicalSize<u32>,
    ) -> Result<PhysicalSize<u32>> {
        let top = title_bar_height_pixels(parent);
        let video_size =
            PhysicalSize::new(size.width.max(1), size.height.saturating_sub(top).max(1));
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                top as i32,
                video_size.width as i32,
                video_size.height as i32,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .context("resize native video child window")?;
        Ok(video_size)
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        // The owner drops/join()s Video Render before destroying this HWND.
        if let Err(error) = unsafe { DestroyWindow(self.hwnd) } {
            tracing::debug!(%error, "destroy video child window");
        }
    }
}
