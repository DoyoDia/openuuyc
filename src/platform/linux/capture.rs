//! X11 screen capture for the host role.
//!
//! Frames are read with MIT-SHM `GetImage` from the root window, one monitor's
//! rectangle at a time, and handed on as BGRA in CPU memory: there is no GPU
//! surface shared with the encoder here, so nothing pretends to be one. Without
//! MIT-SHM (a remote X server) the plain request is used, which is slower but
//! returns the same pixels.
//!
//! X11 has no notification that a frame changed, so `Desktop::next` grabs until
//! its timeout and compares against the last frame it returned; an unchanged
//! screen yields `None`, which is what the DXGI path reports when no new frame
//! arrived and what the capture loop treats as "repeat the cached one".
//!
//! Wayland does not allow this: capture there needs the ScreenCast portal and
//! PipeWire, with the user consenting per session.
use anyhow::{Context, Result, bail, ensure};
use std::sync::Arc;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

pub(crate) use crate::media::capture::{Screen, SourceGone};

/// An X11 capture has no GPU device. The one value stands for the CPU path
/// that capture and the software encoder share, so the loop's "same device,
/// no transfer" test always holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Device;

/// Nothing on the CPU path can be removed from under the capture.
pub(crate) fn device_lost(_device: &Device) -> bool {
    false
}

/// Linux captures on the CPU, so there is no adapter for a second device to be
/// opened on. The host only asks for one when a hardware encoder sits on a
/// different adapter than the capture, which cannot happen here.
pub(crate) fn create_device(_adapter: u64) -> Result<(Device, ())> {
    Ok((Device, ()))
}

/// GPU adapters a hardware encoder could run on. None are offered: encoding
/// on Linux is the Rust H.264 core, which needs no adapter.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EncodingAdapter {
    pub luid: u64,
    pub vendor: u32,
    pub name: String,
}

pub(crate) fn encoding_adapters() -> Result<Vec<EncodingAdapter>> {
    Ok(Vec::new())
}

/// Whether the desktop session is locked. X11 has no portable answer: a
/// screen locker is just another client drawing over the root window, and the
/// capture sees exactly what it draws.
pub(crate) fn session_locked() -> Option<bool> {
    None
}

/// One captured picture, already scaled to the size the encoder was asked for.
#[derive(Clone)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed BGRA, `width * height * 4` bytes.
    pub image: Arc<Vec<u8>>,
    pub captured: Instant,
    pub is_new: bool,
    pub hdr_metadata: Option<crate::media::video_color::HdrMetadata>,
}

/// The active RandR outputs, in the protocol's terms. The identity is the
/// one the display topology uses, so screens and targets pair up exactly.
pub(crate) fn screens() -> Result<Vec<Screen>> {
    ensure_x11()?;
    let monitors = super::display::topology::Topology::query(true)?.monitors();
    ensure!(!monitors.is_empty(), "没有检测到显示器");
    monitors
        .into_iter()
        .map(|monitor| {
            Ok(Screen {
                id: super::display::source_id(&monitor.identity)?,
                device_name: monitor.name.clone(),
                display_name: monitor.name,
                width: monitor.width,
                height: monitor.height,
                left: monitor.left,
                top: monitor.top,
                primary: monitor.primary,
                fps: monitor.hz.filter(|hz| *hz > 1).unwrap_or(60),
                // X11 has no per-output scale to report.
                dpi_scale: None,
                hdr: false,
                adapter: 0,
                render_adapter: None,
                identity: Some(monitor.identity),
            })
        })
        .collect()
}

/// Under Wayland the X server is XWayland, whose root window holds only the
/// X clients' pixels: a capture would stream a black desktop. Say so instead.
fn ensure_x11() -> Result<()> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland");
    ensure!(
        !wayland,
        "Wayland 会话暂不支持本机被控，请在登录界面选择 Xorg 会话"
    );
    ensure!(
        std::env::var_os("DISPLAY").is_some(),
        "没有 X11 显示（DISPLAY 未设置）"
    );
    Ok(())
}

/// The same physical output as `selected`, as it is now.
pub(crate) fn refresh(selected: &Screen) -> Result<Screen> {
    let mut found = screens()?
        .into_iter()
        .find(|screen| screen.identity == selected.identity)
        .ok_or(SourceGone)?;
    found.id = selected.id;
    Ok(found)
}

/// A shared-memory segment the X server writes captured pixels into.
struct Segment {
    id: u32,
    shmid: i32,
    address: *mut u8,
    size: usize,
}

// The segment is only touched from the capture thread that owns the Desktop.
unsafe impl Send for Segment {}

impl Segment {
    fn new(connection: &RustConnection, size: usize) -> Result<Self> {
        // SAFETY: plain SysV shared memory calls; every failure is checked and
        // the segment is marked for removal as soon as the server holds it, so
        // it cannot outlive this process.
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        ensure!(shmid >= 0, "分配共享内存失败");
        let address = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if address as isize == -1 {
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            bail!("映射共享内存失败");
        }
        let id = connection.generate_id()?;
        let attached = connection
            .shm_attach(id, shmid as u32, false)
            .map_err(anyhow::Error::from)
            .and_then(|cookie| cookie.check().map_err(anyhow::Error::from));
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        if let Err(error) = attached {
            unsafe { libc::shmdt(address) };
            return Err(error.context("X 服务器无法附加共享内存"));
        }
        Ok(Self {
            id,
            shmid,
            address: address.cast(),
            size,
        })
    }

    fn pixels(&self) -> &[u8] {
        // SAFETY: the mapping stays valid until drop, and it is only read after
        // the server has answered the request that filled it.
        unsafe { std::slice::from_raw_parts(self.address, self.size) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        let _ = self.shmid;
        unsafe { libc::shmdt(self.address.cast()) };
    }
}

enum Grabber {
    Shm(Segment),
    Plain,
}

/// A capture of one monitor, kept open between frames.
pub(crate) struct Desktop {
    pub device: Device,
    pub screen: Screen,
    pub generation: u64,
    pub available: bool,
    /// The pointer as of the latest `next`, for the cursor channel.
    pub cursor: Option<super::cursor_shape::Snapshot>,
    sampler: super::cursor_shape::Sampler,
    connection: RustConnection,
    root: Window,
    grabber: Grabber,
    cursor_available: bool,
    last: Option<Arc<Vec<u8>>>,
    last_size: (u32, u32),
    source: Vec<u8>,
    refreshed: Instant,
}

impl Desktop {
    pub fn open_selected(selected: &Screen) -> Result<Self> {
        let (connection, screen_index) = x11rb::connect(None).context("连接 X11 显示失败")?;
        let setup = connection.setup();
        let root_screen = &setup.roots[screen_index];
        let root = root_screen.root;
        // Only the common 24/32-bit TrueColor layout is read as BGRA; anything
        // else would need a conversion this capture does not have.
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == root_screen.root_depth)
            .context("X 服务器没有根窗口深度对应的像素格式")?;
        ensure!(
            format.bits_per_pixel == 32
                && matches!(root_screen.root_depth, 24 | 32)
                && setup.image_byte_order == x11rb::protocol::xproto::ImageOrder::LSB_FIRST,
            "只支持 32 位小端 TrueColor 桌面"
        );
        let screen = refresh(selected)?;
        let size = screen.width as usize * screen.height as usize * 4;
        let grabber = match connection
            .extension_information(x11rb::protocol::shm::X11_EXTENSION_NAME)
            .ok()
            .flatten()
        {
            Some(_) => match Segment::new(&connection, size) {
                Ok(segment) => Grabber::Shm(segment),
                Err(error) => {
                    tracing::debug!(error = %format!("{error:#}"), "MIT-SHM 不可用，改用普通 GetImage");
                    Grabber::Plain
                }
            },
            None => Grabber::Plain,
        };
        let cursor_available = connection
            .xfixes_query_version(4, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some();
        Ok(Self {
            device: Device,
            screen,
            generation: 0,
            available: true,
            cursor: None,
            sampler: Default::default(),
            connection,
            root,
            grabber,
            cursor_available,
            last: None,
            last_size: (0, 0),
            source: Vec::with_capacity(size),
            refreshed: Instant::now(),
        })
    }

    pub fn backend_name(&self) -> &'static str {
        match self.grabber {
            Grabber::Shm(_) => "X11 MIT-SHM",
            Grabber::Plain => "X11 GetImage",
        }
    }

    /// X11 has no HDR output to capture from.
    pub fn hdr_available(&self) -> bool {
        false
    }

    /// The next picture, or `None` when nothing changed before `timeout` ms.
    pub fn next(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        _hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        if self.refreshed.elapsed() >= Duration::from_secs(1) {
            let current = refresh(&self.screen)?;
            self.refreshed = Instant::now();
            self.screen.display_name.clone_from(&current.display_name);
            if current != self.screen {
                let mut replacement = Self::open_selected(&current)?;
                replacement.generation = self.generation.wrapping_add(1);
                *self = replacement;
            }
        }
        self.cursor = match self.sampler.sample() {
            Ok(pointer) => Some(pointer),
            Err(error) => {
                tracing::debug!(%error, "host cursor sampling failed");
                None
            }
        };
        let size =
            crate::media::geometry::output_size(self.screen.width, self.screen.height, quality);
        let size = crate::media::geometry::fit_size(size.0, size.1, maximum);
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout));
        loop {
            match self.grab(cursor) {
                Ok(()) => self.available = true,
                Err(error) => {
                    self.available = false;
                    return Err(error);
                }
            }
            let image = scale(&self.source, (self.screen.width, self.screen.height), size);
            let changed = self.last_size != size
                || self
                    .last
                    .as_deref()
                    .is_none_or(|last| last.as_slice() != image.as_slice());
            if changed {
                let image = Arc::new(image);
                self.last = Some(image.clone());
                self.last_size = size;
                return Ok(Some(Frame {
                    width: size.0,
                    height: size.1,
                    image,
                    captured: Instant::now(),
                    is_new: true,
                    hdr_metadata: None,
                }));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(8)));
        }
    }

    /// Read the monitor's rectangle into `self.source`, cursor included when
    /// asked for.
    fn grab(&mut self, cursor: bool) -> Result<()> {
        let (x, y) = (
            i16::try_from(self.screen.left).context("显示器坐标超出 X11 范围")?,
            i16::try_from(self.screen.top).context("显示器坐标超出 X11 范围")?,
        );
        let (width, height) = (
            u16::try_from(self.screen.width).context("显示器宽度超出 X11 范围")?,
            u16::try_from(self.screen.height).context("显示器高度超出 X11 范围")?,
        );
        let length = usize::from(width) * usize::from(height) * 4;
        self.source.clear();
        match &self.grabber {
            Grabber::Shm(segment) => {
                ensure!(segment.size >= length, "共享内存小于所选显示器");
                self.connection
                    .shm_get_image(
                        self.root,
                        x,
                        y,
                        width,
                        height,
                        !0,
                        ImageFormat::Z_PIXMAP.into(),
                        segment.id,
                        0,
                    )?
                    .reply()
                    .context("读取屏幕画面失败")?;
                self.source.extend_from_slice(&segment.pixels()[..length]);
            }
            Grabber::Plain => {
                let reply = self
                    .connection
                    .get_image(ImageFormat::Z_PIXMAP, self.root, x, y, width, height, !0)?
                    .reply()
                    .context("读取屏幕画面失败")?;
                ensure!(reply.data.len() >= length, "屏幕画面数据不完整");
                self.source.extend_from_slice(&reply.data[..length]);
            }
        }
        // X leaves the padding byte undefined; an opaque alpha keeps every
        // consumer of BGRA honest about what it is looking at.
        for pixel in self.source.chunks_exact_mut(4) {
            pixel[3] = 0xff;
        }
        if cursor && self.cursor_available {
            // A cursor that cannot be read is simply left out of this frame.
            let _ = self.draw_cursor();
        }
        Ok(())
    }

    fn draw_cursor(&mut self) -> Result<()> {
        let image = self.connection.xfixes_get_cursor_image()?.reply()?;
        let (width, height) = (i32::from(image.width), i32::from(image.height));
        ensure!(
            image.cursor_image.len() >= (width * height) as usize,
            "光标图像不完整"
        );
        let origin_x = i32::from(image.x) - i32::from(image.xhot) - self.screen.left;
        let origin_y = i32::from(image.y) - i32::from(image.yhot) - self.screen.top;
        let (screen_width, screen_height) = (self.screen.width as i32, self.screen.height as i32);
        for row in 0..height {
            let y = origin_y + row;
            if !(0..screen_height).contains(&y) {
                continue;
            }
            for column in 0..width {
                let x = origin_x + column;
                if !(0..screen_width).contains(&x) {
                    continue;
                }
                // Premultiplied ARGB, one u32 per pixel.
                let argb = image.cursor_image[(row * width + column) as usize];
                let alpha = argb >> 24;
                if alpha == 0 {
                    continue;
                }
                let offset = ((y * screen_width + x) * 4) as usize;
                let pixel = &mut self.source[offset..offset + 4];
                let inverse = 255 - alpha;
                for (channel, shift) in [(0usize, 0u32), (1, 8), (2, 16)] {
                    let over = (argb >> shift) & 0xff;
                    let under = u32::from(pixel[channel]);
                    pixel[channel] = (over + (under * inverse + 127) / 255).min(255) as u8;
                }
            }
        }
        Ok(())
    }
}

/// Bilinear BGRA scaling, or a crop to even dimensions when the size already
/// fits. The encoder takes exactly the size it was opened with.
fn scale(source: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let (source_width, source_height) = (from.0 as usize, from.1 as usize);
    let (width, height) = (to.0 as usize, to.1 as usize);
    let mut out = vec![0u8; width * height * 4];
    if width <= source_width
        && height <= source_height
        && (source_width - width) <= 1
        && (source_height - height) <= 1
    {
        for y in 0..height {
            let start = y * source_width * 4;
            out[y * width * 4..(y + 1) * width * 4]
                .copy_from_slice(&source[start..start + width * 4]);
        }
        return out;
    }
    // 16.16 fixed point, sampling pixel centres.
    let step_x = ((source_width as u64) << 16) / width as u64;
    let step_y = ((source_height as u64) << 16) / height as u64;
    let max_x = source_width - 1;
    let max_y = source_height - 1;
    for y in 0..height {
        let sy = ((y as u64 * step_y) + (step_y >> 1)).saturating_sub(1 << 15);
        let y0 = ((sy >> 16) as usize).min(max_y);
        let y1 = (y0 + 1).min(max_y);
        let fy = (sy & 0xffff) as u32;
        for x in 0..width {
            let sx = ((x as u64 * step_x) + (step_x >> 1)).saturating_sub(1 << 15);
            let x0 = ((sx >> 16) as usize).min(max_x);
            let x1 = (x0 + 1).min(max_x);
            let fx = (sx & 0xffff) as u32;
            let at = |px: usize, py: usize| (py * source_width + px) * 4;
            let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
            let o = (y * width + x) * 4;
            for channel in 0..4 {
                let top = u32::from(source[a + channel]) * (0x10000 - fx)
                    + u32::from(source[b + channel]) * fx;
                let bottom = u32::from(source[c + channel]) * (0x10000 - fx)
                    + u32::from(source[d + channel]) * fx;
                let value = ((u64::from(top >> 8) * u64::from(0x10000 - fy)
                    + u64::from(bottom >> 8) * u64::from(fy))
                    >> 24) as u32;
                out[o + channel] = value.min(255) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::scale;

    #[test]
    fn crop_to_even_keeps_pixels() {
        let source: Vec<u8> = (0..3 * 3 * 4).map(|v| v as u8).collect();
        let out = scale(&source, (3, 3), (2, 2));
        assert_eq!(&out[..8], &source[..8]);
        assert_eq!(&out[8..16], &source[12..20]);
    }

    #[test]
    fn downscale_of_flat_colour_is_flat() {
        let source = [10u8, 20, 30, 255].repeat(64 * 48);
        let out = scale(&source, (64, 48), (32, 24));
        assert!(out.chunks_exact(4).all(|p| p == [10, 20, 30, 255]));
    }
}
