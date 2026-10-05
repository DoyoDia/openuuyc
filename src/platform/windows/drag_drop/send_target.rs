//! Session-owned, ordinary-user OLE send target. WinEvent only observes drag
//! image lifetime; files are obtained from the actual drop IDataObject.
use super::*;
use std::{cell::Cell, path::PathBuf, rc::Rc, sync::mpsc, time::Duration};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::{Graphics::Gdi::*, UI::Accessibility::*};
const WM_MOUSELEAVE: u32 = 0x02a3; // WinUser.h; no common-controls runtime needed.

#[derive(Clone, Default, PartialEq)]
pub(crate) struct Model {
    pub epoch: u64,
    pub revision: u64,
    pub acknowledged: u64,
    pub enabled: bool,
    pub can_send: bool,
    pub status: Option<String>,
    pub items: Vec<Row>,
}
#[derive(Clone, PartialEq)]
pub(crate) struct Row {
    pub id: u64,
    pub name: String,
}
pub(crate) struct Action {
    pub epoch: u64,
    pub revision: u64,
    pub sequence: u64,
    pub kind: ActionKind,
}
pub(crate) enum ActionKind {
    Add(Vec<PathBuf>),
    Remove(u64),
    Clear,
    Send,
}

#[derive(Clone, Default, PartialEq)]
struct View {
    model: Model,
    issued: u64,
}
impl View {
    fn pending(&self) -> bool {
        self.issued > self.model.acknowledged
    }
    fn submit(&mut self, send: &mpsc::SyncSender<Action>, kind: ActionKind) -> bool {
        if !self.model.enabled || self.pending() {
            return false;
        }
        let sequence = self.issued.wrapping_add(1);
        if send
            .try_send(Action {
                epoch: self.model.epoch,
                revision: self.model.revision,
                sequence,
                kind,
            })
            .is_err()
        {
            return false;
        }
        self.issued = sequence;
        true
    }
}
pub(crate) struct SendTarget {
    view: Arc<Mutex<View>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl SendTarget {
    pub fn start(send: mpsc::SyncSender<Action>) -> Result<Self> {
        let view = Arc::new(Mutex::new(View::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let state = view.clone();
        let ending = stop.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("file send target".into())
            .spawn(move || {
                if let Err(error) = run(state, ending, send, &ready) {
                    let message = error.to_string();
                    let _ = ready.try_send(Err(message.clone()));
                    tracing::warn!(%message, "file send target stopped");
                }
            })?;
        let mut result = Self {
            view,
            stop,
            worker: Some(worker),
        };
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(result),
            error => {
                result.stop.store(true, Ordering::Release);
                result.worker.take();
                anyhow::bail!("文件发送浮窗初始化失败: {error:?}")
            }
        }
    }
    pub fn update(&self, model: Model) {
        let mut view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        if view.model.epoch != model.epoch || !model.enabled {
            view.issued = model.acknowledged;
        } else {
            view.issued = view.issued.max(model.acknowledged);
        }
        view.model = model;
    }
}
impl Drop for SendTarget {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
thread_local! {
    static DRAGS: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
}
unsafe extern "system" fn event(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if hwnd.is_invalid() || object != 0 {
        return;
    }
    if event == EVENT_OBJECT_CREATE {
        let mut name = [0u16; 64];
        let n = unsafe { GetClassNameW(hwnd, &mut name) };
        if n > 0 && String::from_utf16_lossy(&name[..n as usize]) == "SysDragImage" {
            DRAGS.with(|v| {
                let mut v = v.borrow_mut();
                if v.len() < 16 && !v.contains(&hwnd) {
                    v.push(hwnd);
                }
            });
        }
    } else if event == EVENT_OBJECT_DESTROY {
        DRAGS.with(|v| v.borrow_mut().retain(|h| *h != hwnd));
    }
}
struct DropHandler {
    hwnd: HWND,
    view: Arc<Mutex<View>>,
    stop: Arc<AtomicBool>,
    paths: Vec<PathBuf>,
    hover: Rc<Cell<bool>>,
    entered: Rc<Cell<bool>>,
    send: mpsc::SyncSender<Action>,
}
impl DropHandler {
    fn available(&self) -> bool {
        let view = self.view.lock().unwrap_or_else(|e| e.into_inner());
        view.model.enabled && !view.pending() && !self.stop.load(Ordering::Acquire)
    }
    fn hovered(&self, value: bool) {
        if self.hover.replace(value) != value {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }
}
impl target::Handler for DropHandler {
    fn enter(&mut self, paths: Vec<PathBuf>, point: POINTL) -> u32 {
        self.paths = paths;
        self.entered.set(true);
        self.over(point)
    }
    fn over(&mut self, _: POINTL) -> u32 {
        let ready = self.available() && !self.paths.is_empty();
        self.hovered(ready);
        if ready { DROPEFFECT_COPY.0 } else { 0 }
    }
    fn leave(&mut self) {
        self.paths.clear();
        self.entered.set(false);
        self.hovered(false);
    }
    fn drop_at(&mut self, _: POINTL) -> u32 {
        if !self.available() || self.paths.is_empty() {
            self.leave();
            return 0;
        }
        // COPY alone means an Explorer move gesture can never remove its source.
        let accepted = {
            let mut view = self.view.lock().unwrap_or_else(|e| e.into_inner());
            view.submit(&self.send, ActionKind::Add(std::mem::take(&mut self.paths)))
        };
        self.entered.set(false);
        self.hovered(false);
        if accepted { DROPEFFECT_COPY.0 } else { 0 }
    }
}
struct Paint {
    view: Arc<Mutex<View>>,
    hover: Rc<Cell<bool>>,
    entered: Rc<Cell<bool>>,
    cursor: HCURSOR,
    send: mpsc::SyncSender<Action>,
    offset: Cell<usize>,
    hot: Cell<Option<Hit>>,
    pressed: Cell<Option<(u64, u64, Hit)>>,
    position: Cell<Option<(i32, i32)>>,
    moving: Cell<bool>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Remove(u64),
    Clear,
    Send,
}
impl Hit {
    fn action(self) -> ActionKind {
        match self {
            Self::Remove(id) => ActionKind::Remove(id),
            Self::Clear => ActionKind::Clear,
            Self::Send => ActionKind::Send,
        }
    }
}
fn rows(view: &View) -> usize {
    view.model
        .items
        .len()
        .min(crate::ui::theme::FILE_SEND_VISIBLE_ROWS)
}
fn height(view: &View) -> f32 {
    crate::ui::theme::FILE_SEND_HEIGHT
        + if view.model.items.is_empty() {
            0.
        } else {
            rows(view) as f32 * crate::ui::theme::FILE_SEND_ROW_HEIGHT + 40.
        }
}
fn hit(view: &View, offset: usize, x: f32, y: f32) -> Option<Hit> {
    use crate::ui::theme;
    if !view.model.enabled || view.pending() || view.model.items.is_empty() {
        return None;
    }
    let row = (y - 70.) / theme::FILE_SEND_ROW_HEIGHT;
    if x >= theme::FILE_SEND_WIDTH - 36.
        && x < theme::FILE_SEND_WIDTH - 12.
        && row >= 0.
        && row < (rows(view) as f32)
    {
        return view
            .model
            .items
            .get(offset + row.floor() as usize)
            .map(|r| Hit::Remove(r.id));
    }
    let top = height(view) - 40.;
    if y < top || y >= top + 28. || x < 12. || x >= theme::FILE_SEND_WIDTH - 12. {
        return None;
    }
    let half = (theme::FILE_SEND_WIDTH - 32.) / 2.;
    if x < 12. + half {
        Some(Hit::Clear)
    } else if x >= 20. + half && view.model.can_send {
        Some(Hit::Send)
    } else {
        None
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(l.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let paint = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Paint;
        if !paint.is_null() {
            let p = &*paint;
            match message {
                WM_NCHITTEST if !p.entered.get() => {
                    let mut point = POINT {
                        x: l.0 as i16 as i32,
                        y: (l.0 >> 16) as i16 as i32,
                    };
                    let _ = ScreenToClient(hwnd, &mut point);
                    let dpi = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
                    // Only the header moves the window; rows, buttons and OLE
                    // file drops retain their own input handling.
                    if point.y >= 0 && (point.y as f32) < 64. * dpi {
                        return LRESULT(HTCAPTION as isize);
                    }
                }
                WM_ENTERSIZEMOVE => p.moving.set(true),
                WM_MOVING => {
                    let rect = &mut *(l.0 as *mut RECT);
                    let mut point = POINT::default();
                    if GetCursorPos(&mut point).is_ok() {
                        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
                        if let Some(work) = work_area(monitor) {
                            *rect = within_work_area(*rect, work);
                        }
                    }
                    return LRESULT(1);
                }
                WM_EXITSIZEMOVE => {
                    p.moving.set(false);
                    let mut rect = RECT::default();
                    if GetWindowRect(hwnd, &mut rect).is_ok() {
                        p.position.set(Some((rect.left, rect.top)));
                    }
                }
                _ => {}
            }
        }
        if !paint.is_null()
            && matches!(
                message,
                WM_MOUSEMOVE
                    | WM_MOUSELEAVE
                    | WM_LBUTTONDOWN
                    | WM_LBUTTONUP
                    | WM_CAPTURECHANGED
                    | WM_MOUSEWHEEL
            )
        {
            let p = &*paint;
            if message == WM_CAPTURECHANGED {
                p.pressed.set(None);
                return LRESULT(0);
            }
            if message == WM_MOUSELEAVE {
                p.hot.set(None);
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            if !p.entered.get() {
                let view = p.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let dpi = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
                let selected = hit(
                    &view,
                    p.offset.get(),
                    l.0 as i16 as f32 / dpi,
                    (l.0 >> 16) as i16 as f32 / dpi,
                );
                match message {
                    WM_MOUSEWHEEL => {
                        let delta = (w.0 >> 16) as i16;
                        let max = view.model.items.len().saturating_sub(rows(&view));
                        let offset = if delta > 0 {
                            p.offset.get().saturating_sub(1)
                        } else {
                            p.offset.get().saturating_add(1).min(max)
                        };
                        p.offset.set(offset);
                        p.hot.set(None);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    WM_MOUSEMOVE => {
                        let mut track = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            ..Default::default()
                        };
                        let _ = TrackMouseEvent(&mut track);
                        if p.hot.replace(selected) != selected {
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                    WM_LBUTTONDOWN => {
                        if let Some(h) = selected {
                            p.pressed
                                .set(Some((view.model.epoch, view.model.revision, h)));
                            SetCapture(hwnd);
                        }
                    }
                    WM_LBUTTONUP => {
                        let pressed = p.pressed.take();
                        let _ = ReleaseCapture();
                        if let Some((epoch, revision, h)) = pressed {
                            let mut latest = p.view.lock().unwrap_or_else(|e| e.into_inner());
                            if selected == Some(h)
                                && epoch == latest.model.epoch
                                && revision == latest.model.revision
                            {
                                latest.submit(&p.send, h.action());
                            }
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
        }
        match message {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_ERASEBKGND => return LRESULT(1),
            WM_SETCURSOR if (l.0 as u32 & 0xffff) == HTCLIENT => {
                let paint = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Paint;
                if !paint.is_null() {
                    // OLE owns copy/forbidden feedback while inside a drag.
                    // After Drop/DragLeave this passive notice owns an arrow.
                    if !(*paint).entered.get() {
                        SetCursor(Some((*paint).cursor));
                    }
                    return LRESULT(1);
                }
            }
            WM_PAINT => {
                let paint = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Paint;
                if !paint.is_null() {
                    draw(hwnd, &*paint);
                    return LRESULT(0);
                }
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}
fn work_area(monitor: HMONITOR) -> Option<RECT> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some(info.rcWork)
    }
}
fn within_work_area(rect: RECT, work: RECT) -> RECT {
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    let left = rect
        .left
        .clamp(work.left, (work.right - width).max(work.left));
    let top = rect
        .top
        .clamp(work.top, (work.bottom - height).max(work.top));
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}
fn color(c: egui::Color32) -> COLORREF {
    COLORREF(u32::from(c.r()) | (u32::from(c.g()) << 8) | (u32::from(c.b()) << 16))
}
struct BackBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
impl BackBuffer {
    unsafe fn new(target: HDC, width: i32, height: i32) -> Option<Self> {
        unsafe {
            if target.is_invalid() || width <= 0 || height <= 0 {
                return None;
            }
            let dc = CreateCompatibleDC(Some(target));
            if dc.is_invalid() {
                return None;
            }
            let bitmap = CreateCompatibleBitmap(target, width, height);
            if bitmap.is_invalid() {
                let _ = DeleteDC(dc);
                return None;
            }
            let previous = SelectObject(dc, bitmap.into());
            if previous.is_invalid() {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                return None;
            }
            Some(Self {
                dc,
                bitmap,
                previous,
            })
        }
    }
}
impl Drop for BackBuffer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
unsafe fn draw(hwnd: HWND, state: &Paint) {
    use crate::ui::theme;
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let target = BeginPaint(hwnd, &mut ps);
        let mut bounds = RECT::default();
        let _ = GetClientRect(hwnd, &mut bounds);
        let Some(buffer) = BackBuffer::new(target, bounds.right, bounds.bottom) else {
            let _ = EndPaint(hwnd, &ps);
            return;
        };
        let dc = buffer.dc;
        let view = state.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let background = CreateSolidBrush(color(if state.hover.get() {
            theme::SELECTED
        } else {
            theme::SURFACE
        }));
        FillRect(dc, &bounds, background);
        let _ = DeleteObject(background.into());
        let border = CreateSolidBrush(color(if state.hover.get() {
            theme::ACCENT
        } else {
            theme::LINE
        }));
        FrameRect(dc, &bounds, border);
        let _ = DeleteObject(border.into());
        let dpi = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
        let font = CreateFontW(
            -(theme::BODY * dpi).round() as i32,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            windows::core::w!("Segoe UI"),
        );
        let old = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        let title = if state.hover.get() {
            "松开加入待发送队列".to_owned()
        } else if view.pending() {
            "正在处理…".to_owned()
        } else if let Some(status) = &view.model.status {
            status.clone()
        } else if !view.model.items.is_empty() {
            format!("待发送 · {} 项", view.model.items.len())
        } else {
            "拖入文件或文件夹".to_owned()
        };
        let mut text: Vec<u16> = title.encode_utf16().collect();
        let mut line = RECT {
            left: (12. * dpi) as i32,
            top: (12. * dpi) as i32,
            right: bounds.right - (12. * dpi) as i32,
            bottom: (36. * dpi) as i32,
        };
        SetTextColor(dc, color(theme::TEXT));
        DrawTextW(
            dc,
            &mut text,
            &mut line,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        line.top = (40. * dpi) as i32;
        line.bottom = (64. * dpi) as i32;
        let caption = if view.model.items.len() > rows(&view) {
            "可滚动查看 · 点击发送后开始传输"
        } else {
            "可继续拖入 · 点击发送后开始传输"
        };
        let mut text: Vec<u16> = caption.encode_utf16().collect();
        SetTextColor(dc, color(theme::MUTED));
        DrawTextW(
            dc,
            &mut text,
            &mut line,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        let offset = state
            .offset
            .get()
            .min(view.model.items.len().saturating_sub(rows(&view)));
        state.offset.set(offset);
        for (index, item) in view
            .model
            .items
            .iter()
            .skip(offset)
            .take(rows(&view))
            .enumerate()
        {
            let y = 70. + index as f32 * theme::FILE_SEND_ROW_HEIGHT;
            let mut rect = RECT {
                left: (12. * dpi) as i32,
                top: (y * dpi) as i32,
                right: bounds.right - (42. * dpi) as i32,
                bottom: ((y + theme::FILE_SEND_ROW_HEIGHT) * dpi) as i32,
            };
            let mut text: Vec<u16> = item.name.encode_utf16().collect();
            SetTextColor(dc, color(theme::TEXT));
            DrawTextW(
                dc,
                &mut text,
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            rect.left = bounds.right - (36. * dpi) as i32;
            rect.right = bounds.right - (12. * dpi) as i32;
            if state.hot.get() == Some(Hit::Remove(item.id)) {
                let brush = CreateSolidBrush(color(theme::HOVER));
                FillRect(dc, &rect, brush);
                let _ = DeleteObject(brush.into());
            }
            SetTextColor(
                dc,
                color(if view.pending() {
                    theme::DISABLED
                } else {
                    theme::MUTED
                }),
            );
            DrawTextW(
                dc,
                &mut "×".encode_utf16().collect::<Vec<_>>(),
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        if !view.model.items.is_empty() {
            let top = height(&view) - 40.;
            let half = (theme::FILE_SEND_WIDTH - 32.) / 2.;
            for (action, x, label, enabled) in [
                (Hit::Clear, 12., "清空队列".to_owned(), !view.pending()),
                (
                    Hit::Send,
                    20. + half,
                    format!("发送 ({})", view.model.items.len()),
                    view.model.can_send && !view.pending(),
                ),
            ] {
                let rect = RECT {
                    left: (x * dpi) as i32,
                    top: (top * dpi) as i32,
                    right: ((x + half) * dpi) as i32,
                    bottom: ((top + 28.) * dpi) as i32,
                };
                let fill = if !enabled {
                    theme::BG
                } else if state.hot.get() == Some(action) {
                    theme::HOVER
                } else if action == Hit::Send {
                    theme::ACCENT
                } else {
                    theme::SURFACE
                };
                let brush = CreateSolidBrush(color(fill));
                FillRect(dc, &rect, brush);
                let _ = DeleteObject(brush.into());
                let brush = CreateSolidBrush(color(theme::LINE));
                FrameRect(dc, &rect, brush);
                let _ = DeleteObject(brush.into());
                SetTextColor(
                    dc,
                    color(if enabled {
                        theme::TEXT
                    } else {
                        theme::DISABLED
                    }),
                );
                DrawTextW(
                    dc,
                    &mut label.encode_utf16().collect::<Vec<_>>(),
                    &mut { rect },
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
            }
        }
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
        // Present only the completed style, never an erased background or a
        // partially drawn caption to desktop capture or the compositor.
        let _ = BitBlt(
            target,
            0,
            0,
            bounds.right,
            bounds.bottom,
            Some(dc),
            0,
            0,
            SRCCOPY,
        );
        let _ = EndPaint(hwnd, &ps);
    }
}
fn run(
    view: Arc<Mutex<View>>,
    stop: Arc<AtomicBool>,
    send: mpsc::SyncSender<Action>,
    ready: &mpsc::SyncSender<Result<(), String>>,
) -> Result<()> {
    let old = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    struct Dpi(DPI_AWARENESS_CONTEXT);
    impl Drop for Dpi {
        fn drop(&mut self) {
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
    let _dpi = Dpi(old);
    let module = unsafe { GetModuleHandleW(None)? };
    let class = windows::core::w!("OpenUUYC.FileSendTarget");
    let hover = Rc::new(Cell::new(false));
    let entered = Rc::new(Cell::new(false));
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW)? };
    let paint = Box::new(Paint {
        view: view.clone(),
        hover: hover.clone(),
        entered: entered.clone(),
        cursor,
        send: send.clone(),
        offset: Cell::new(0),
        hot: Cell::new(None),
        pressed: Cell::new(None),
        position: Cell::new(None),
        moving: Cell::new(false),
    });
    unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: module.into(),
            lpszClassName: class,
            hCursor: cursor,
            ..Default::default()
        });
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class,
            windows::core::w!("发送文件到主控"),
            WS_POPUP,
            0,
            0,
            crate::ui::theme::FILE_SEND_WIDTH as i32,
            crate::ui::theme::FILE_SEND_HEIGHT as i32,
            None,
            None,
            Some(module.into()),
            Some((&*paint as *const Paint).cast()),
        )?
    };
    struct Window(HWND);
    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
    let _window = Window(hwnd);
    let handler = Rc::new(RefCell::new(DropHandler {
        hwnd,
        view: view.clone(),
        stop: stop.clone(),
        paths: Vec::new(),
        hover,
        entered: entered.clone(),
        send,
    }));
    let _registration = target::Registration::new(hwnd, handler.clone())?;
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_DESTROY,
            None,
            Some(event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    ensure!(!hook.is_invalid(), "无法监听系统文件拖动");
    struct Hook(HWINEVENTHOOK);
    impl Drop for Hook {
        fn drop(&mut self) {
            unsafe {
                let _ = UnhookWinEvent(self.0);
            }
        }
    }
    let _hook = Hook(hook);
    tracing::info!("official file send drag listener started");
    let timer = unsafe { SetTimer(Some(hwnd), 1, 80, None) };
    ensure!(timer != 0, "无法创建拖放窗口计时器");
    let _ = ready.send(Ok(()));
    let mut visible = false;
    let mut last = View::default();
    let mut placement = (0, 0, 0, 0);
    let mut released_at = None;
    // Drag-image windows are presentation resources, not the drag lifetime.
    // Keep the discovered gesture across the gap before OLE DragEnter arrives.
    let mut discovered_drag = false;
    let mut cancelled_drag = false;
    while !stop.load(Ordering::Acquire) {
        let mut msg = MSG::default();
        unsafe {
            if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let current = view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if current.model.epoch != last.model.epoch || !current.model.enabled {
            paint.position.set(None);
        }
        let held = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
        let source_seen = DRAGS.with(|v| !v.borrow().is_empty());
        if !held {
            discovered_drag = false;
            cancelled_drag = false;
        } else {
            if unsafe {
                GetAsyncKeyState(windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE.0 as i32)
            } < 0
                || unsafe { GetAsyncKeyState(VK_RBUTTON.0 as i32) } < 0
            {
                cancelled_drag = true;
                discovered_drag = false;
            }
            if !cancelled_drag && source_seen {
                discovered_drag = true;
            }
        }
        // Shell can retire its drag-image window on entering a non-Shell target.
        // Mouse-up also precedes the cross-thread Drop RPC. Neither is DragLeave.
        if entered.get() && !held {
            let at = released_at.get_or_insert_with(std::time::Instant::now);
            if at.elapsed() > Duration::from_millis(500) {
                // A dead source must not leave an interactive target behind.
                target::Handler::leave(&mut *handler.borrow_mut());
            }
        } else {
            released_at = None;
        }
        let show = current.model.enabled
            && ((entered.get() && !cancelled_drag)
                || discovered_drag
                || current.pending()
                || !current.model.items.is_empty()
                || current.model.status.is_some());
        if show != visible {
            tracing::debug!(show, "official file send target visibility");
        }
        if show {
            let mut point = POINT::default();
            unsafe {
                GetCursorPos(&mut point)?;
            }
            let monitor = unsafe {
                if let Some((x, y)) = paint.position.get() {
                    MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST)
                } else if visible
                    && (entered.get()
                        || current.pending()
                        || !current.model.items.is_empty()
                        || current.model.status.is_some())
                {
                    MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
                } else {
                    MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST)
                }
            };
            let work = work_area(monitor).ok_or_else(|| anyhow::anyhow!("无法读取显示器工作区"))?;
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.;
            let width = (crate::ui::theme::FILE_SEND_WIDTH * dpi) as i32;
            let height = (height(&current) * dpi) as i32;
            let (x, y) = paint.position.get().unwrap_or((
                work.left + (work.right - work.left - width) / 2,
                work.top + (24. * dpi) as i32,
            ));
            let rect = within_work_area(
                RECT {
                    left: x,
                    top: y,
                    right: x + width,
                    bottom: y + height,
                },
                work,
            );
            let next = (rect.left, rect.top, width, height);
            if !paint.moving.get() && (!visible || placement != next) {
                unsafe {
                    SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        next.0,
                        next.1,
                        next.2,
                        next.3,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    )?;
                }
                placement = next;
            }
        } else if visible {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
        if current != last {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
                // A save receipt can arrive without another mouse move. Do
                // not leave OLE's old busy cursor above the completed notice.
                if show && !entered.get() {
                    let mut point = POINT::default();
                    if GetCursorPos(&mut point).is_ok() && WindowFromPoint(point) == hwnd {
                        SetCursor(Some(cursor));
                    }
                }
            }
            last = current;
        }
        visible = show;
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), timer);
    }
    Ok(())
}
