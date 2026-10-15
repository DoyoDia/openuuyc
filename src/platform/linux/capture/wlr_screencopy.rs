//! Capture through the compositor's own `zwlr_screencopy` protocol: wlroots,
//! niri, Sway, Hyprland and their relatives all serve it. Unlike the portal it
//! asks the local user for nothing, and it needs no permission the session does
//! not already have.
//!
//! Frames come back in shared memory. The protocol's other half — a dma-buf the
//! compositor draws into — would keep the picture on the GPU, but the pixels
//! still have to be read here for the encoder's own conversion, and a device
//! buffer mapped into a process on the discrete GPU of this machine reads at
//! about 85 MB/s: a 16 MB frame took 195 ms, against 1.4 ms for the same frame
//! through shared memory. dma-buf pays off only once the conversion moves onto
//! the GPU too, which the encoder does not do yet.
//!
//! The compositor copies its own framebuffer, not the picture the monitor
//! shows: a rotated or flipped output arrives sideways, and the transform
//! `wl_output.geometry` advertises has to be undone here, exactly as `grim`
//! and every other screencopy client does. Left undone, a portrait monitor
//! streams a landscape picture of a sideways desktop twice over — the shape
//! is wrong and so is everything in it.
//!
//! `copy_with_damage` is used rather than `copy`: the compositor then holds the
//! frame back until something has actually changed, which is what keeps an idle
//! desktop from being encoded and sent over and over.
//!
//! The copy carries no scaling: the buffer has to be exactly the output's size,
//! and the caller scales afterwards like it does for the portal.
use super::Picture;
use super::Screen;
use anyhow::{Context, Result, bail, ensure};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_output::Transform;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

/// `wl_output.name` arrived at 4, and it is how a captured monitor is paired
/// with the one the caller asked for.
const OUTPUT_VERSION: u32 = 4;

/// The one thing a compositor says when it will not copy: a `failed` event,
/// which covers an output that has been turned off as well as any other
/// refusal.
const COPY_REFUSED: &str = "合成器拒绝了屏幕复制（输出可能已关闭）";

/// One output the compositor offers to copy.
struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    /// The mode the output is driving, before `transform` is applied.
    mode: (u32, u32),
    transform: Transform,
}

impl Output {
    /// The size the desktop sees: the mode as the transform turns it, which is
    /// what the caller's `Screen` describes and what a frame will come back as.
    fn size(&self) -> (u32, u32) {
        match self.transform {
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
                (self.mode.1, self.mode.0)
            }
            _ => self.mode,
        }
    }
}

/// The shared memory every frame is copied into, and the buffer that names it.
/// The pool and the descriptor stay alive as long as the buffer does: the
/// compositor reads the memory through them.
struct Shm {
    _fd: OwnedFd,
    _pool: wl_shm_pool::WlShmPool,
    map: *mut u8,
    length: usize,
    buffer: wl_buffer::WlBuffer,
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
    /// The compositor's pixels have red and blue the other way round from the
    /// BGRA the encoder wants; see `swapped_channels`.
    swap: bool,
}

impl Shm {
    fn matches(&self, format: u32, width: u32, height: u32, stride: u32) -> bool {
        self.format == format
            && self.width == width
            && self.height == height
            && self.stride == stride
    }

    /// The picture as tightly packed BGRA rows, without the stride padding and
    /// with the output's transform undone.
    fn picture(&self, transform: Transform) -> Picture {
        let row = self.width as usize * 4;
        // Safe because the mapping is live for as long as `self` is, and the
        // compositor has finished with it: a `ready` event preceded this.
        let all = unsafe { std::slice::from_raw_parts(self.map, self.length) };
        let mut pixels = Vec::with_capacity(row * self.height as usize);
        if self.stride as usize == row && all.len() >= row * self.height as usize {
            pixels.extend_from_slice(&all[..row * self.height as usize]);
        } else {
            for line in all
                .chunks_exact(self.stride as usize)
                .take(self.height as usize)
            {
                // A stride narrower than a row is refused in `allocate`, so
                // this takes whole rows; the clamp is only there to keep a
                // short mapping from panicking.
                pixels.extend_from_slice(&line[..row.min(line.len())]);
            }
        }
        if self.swap {
            // The alpha byte of an `abgr8888` frame sits in byte 3 like the
            // encoder's, so only the two colour bytes move.
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        let (pixels, width, height) = orient(pixels, self.width, self.height, transform);
        Picture {
            width,
            height,
            pixels,
            nv12: None,
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // Safe because the mapping was made for this length in `Shm::new`.
        unsafe { libc::munmap(self.map.cast(), self.length) };
    }
}

pub(super) struct Grab {
    queue: EventQueue<State>,
    state: State,
    cursor: bool,
    output: usize,
}

#[derive(Default)]
struct State {
    connection: Option<Connection>,
    manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    shm: Option<wl_shm::WlShm>,
    outputs: Vec<Output>,
    frame: Option<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1>,
    /// What the frame in flight says its buffer must look like.
    wanted: Option<(u32, u32, u32, u32)>,
    shm_buffer: Option<Shm>,
    /// The compositor has finished drawing the frame already requested.
    ready: bool,
    failed: bool,
    error: Option<String>,
}

impl Grab {
    pub fn open(screen: &Screen, cursor: bool) -> Result<Self> {
        let connection =
            Connection::connect_to_env().context("没有可用的 Wayland 会话（WAYLAND_DISPLAY）")?;
        let mut queue = connection.new_event_queue::<State>();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let mut state = State {
            connection: Some(connection),
            ..Default::default()
        };
        queue
            .roundtrip(&mut state)
            .context("读取 Wayland 全局对象失败")?;
        // The compositor answers a bind from its own event loop, so what the
        // outputs say about themselves arrives one round trip later.
        queue
            .roundtrip(&mut state)
            .context("读取 Wayland 输出描述失败")?;
        // The registry reports what it could not bind; a compositor too old
        // for `buffer_done` says so there. Refusing here leaves the next
        // candidate — the portal — to try.
        if let Some(error) = &state.error {
            bail!("{error}");
        }
        ensure!(
            state.manager.is_some(),
            "合成器没有提供 zwlr_screencopy_manager_v1"
        );
        ensure!(state.shm.is_some(), "合成器没有提供 wl_shm");
        ensure!(!state.outputs.is_empty(), "合成器没有提供任何输出");
        let output = pick(&state.outputs, screen)?;
        tracing::info!(
            output = screen.device_name,
            mode = ?state.outputs[output].mode,
            transform = ?state.outputs[output].transform,
            "host wlr-screencopy output selected"
        );
        let mut grab = Self {
            queue,
            state,
            cursor,
            output,
        };
        grab.probe()?;
        Ok(grab)
    }

    /// Takes one frame before the grab is handed over. Serving the protocol is
    /// not the same as being able to copy: the Vulkan renderer of wlroots has
    /// no read-back at all, an output that is off answers `failed`, and a
    /// format this backend refuses is only known once a frame announces it.
    /// `Backend::select` moves on to the next candidate only when `open`
    /// returns an error, so all three have to be found out here or the portal
    /// below is never reached.
    fn probe(&mut self) -> Result<()> {
        self.request()?;
        // `buffer` and `buffer_done` leave the compositor as it handles the
        // request itself, so this answers at once even on a still desktop
        // where no frame would be drawn for a while.
        self.queue
            .roundtrip(&mut self.state)
            .context("读取 Wayland 事件失败")?;
        if let Some(error) = self.state.error.take() {
            bail!("{error}");
        }
        ensure!(!self.state.failed, "{COPY_REFUSED}");
        ensure!(self.state.wanted.is_some(), "合成器没有说明帧的缓冲格式");
        // The probe's frame is single-use and is not wanted: drop it, and let
        // the first `next` open a frame of its own. The buffer is kept, and is
        // taken up again when the next frame asks for the same shape.
        self.drop_frame();
        self.state.wanted = None;
        self.state.ready = false;
        Ok(())
    }

    pub fn name(&self) -> &'static str {
        "wlroots screencopy"
    }

    /// Whether the compositor draws the cursor into the pictures.
    pub fn cursor(&self) -> bool {
        self.cursor
    }

    pub fn next(&mut self, timeout: Duration) -> Result<Option<Picture>> {
        let deadline = Instant::now() + timeout;
        loop {
            // Kept rather than taken: the compositor is not going to change
            // its mind on its own, and `Ok(None)` every frame would leave the
            // caller with a black picture and nothing to report.
            if let Some(error) = &self.state.error {
                bail!("{error}");
            }
            if std::mem::take(&mut self.state.ready) {
                let transform = self
                    .state
                    .outputs
                    .get(self.output)
                    .map_or(Transform::Normal, |output| output.transform);
                let picture = self
                    .state
                    .shm_buffer
                    .as_ref()
                    .map(|shm| shm.picture(transform));
                // Ask for the next frame before returning, so the compositor
                // draws into the buffer while the caller encodes this one.
                self.request()?;
                if let Some(picture) = picture {
                    return Ok(Some(picture));
                }
            }
            if std::mem::take(&mut self.state.failed) {
                self.request()?;
                bail!("{COPY_REFUSED}");
            }
            if self.state.frame.is_none() {
                self.request()?;
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            self.wait(deadline - now)?;
        }
    }

    /// Starts one frame; the frame object is single-use, so every copy needs a
    /// new one.
    fn request(&mut self) -> Result<()> {
        let handle = self.queue.handle();
        let manager = self.state.manager.clone().context("采集已关闭")?;
        self.state.wanted = None;
        self.drop_frame();
        let output = self
            .state
            .outputs
            .get(self.output)
            .context("采集的输出已消失")?;
        self.state.frame =
            Some(manager.capture_output(i32::from(self.cursor), &output.proxy, &handle, ()));
        Ok(())
    }

    /// Drops the frame of the previous request. wayland-client sends no
    /// destructor when a proxy is dropped, and a frame object serves one copy,
    /// so it has to be destroyed here or every copy leaves an object behind on
    /// the compositor's side.
    fn drop_frame(&mut self) {
        if let Some(frame) = self.state.frame.take() {
            frame.destroy();
        }
    }

    /// Dispatches until an event arrives or `timeout` runs out. Events already
    /// queued are handled first, so a compositor that answers at once never
    /// waits on the socket.
    fn wait(&mut self, timeout: Duration) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        let connection = self.state.connection.as_ref().context("采集已关闭")?;
        // `dispatch_pending` only drains what has already arrived; a request
        // made since the last wait is still in the outgoing buffer.
        connection.flush().context("发送 Wayland 请求失败")?;
        let Some(guard) = connection.prepare_read() else {
            return Ok(());
        };
        let mut poll = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Safe because `poll` only writes into the one descriptor it was given.
        let ready = unsafe { libc::poll(&mut poll, 1, millis(timeout)) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            drop(guard);
            bail!("等待 Wayland 事件失败：{error}");
        }
        if ready == 0 {
            // Dropping the guard releases the prepared read.
            drop(guard);
            return Ok(());
        }
        match guard.read() {
            Ok(_) => {}
            // The socket can report readable with nothing left to read.
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(());
            }
            Err(error) => return Err(error).context("读取 Wayland 事件失败"),
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
}

/// Undoes the output's transform, which the compositor leaves to the client:
/// what arrives is the framebuffer, and a monitor standing on its side shows up
/// in it lying down. The mapping is `wl_output`'s own, read as "the framebuffer
/// pixel this picture pixel is", and `Normal` — the one case that needs no work
/// — passes the rows straight through.
fn orient(source: Vec<u8>, width: u32, height: u32, transform: Transform) -> (Vec<u8>, u32, u32) {
    if transform == Transform::Normal {
        return (source, width, height);
    }
    let turned = matches!(
        transform,
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
    );
    let (out_width, out_height) = if turned {
        (height, width)
    } else {
        (width, height)
    };
    let (last_x, last_y) = (width as i32 - 1, height as i32 - 1);
    let source_at = |x: i32, y: i32| match transform {
        Transform::_90 => (y, last_y - x),
        Transform::_180 => (last_x - x, last_y - y),
        Transform::_270 => (last_x - y, x),
        Transform::Flipped => (last_x - x, y),
        Transform::Flipped90 => (y, x),
        Transform::Flipped180 => (x, last_y - y),
        Transform::Flipped270 => (last_x - y, last_y - x),
        // `Normal`, and any transform a later revision adds.
        _ => (x, y),
    };
    let mut out = vec![0u8; out_width as usize * out_height as usize * 4];
    for y in 0..out_height as i32 {
        for x in 0..out_width as i32 {
            let (from_x, from_y) = source_at(x, y);
            let from = (from_y as usize * width as usize + from_x as usize) * 4;
            let to = (y as usize * out_width as usize + x as usize) * 4;
            out[to..to + 4].copy_from_slice(&source[from..from + 4]);
        }
    }
    (out, out_width, out_height)
}

fn millis(timeout: Duration) -> i32 {
    timeout.as_millis().min(i32::MAX as u128) as i32
}

/// The output the caller asked for, by the name the compositor gives it (the
/// same `DP-1` the display topology uses). A compositor too old to name its
/// outputs is matched by size, and a single-output desktop needs no match.
fn pick(outputs: &[Output], screen: &Screen) -> Result<usize> {
    if let Some(index) = outputs
        .iter()
        .position(|output| output.name.as_deref() == Some(screen.device_name.as_str()))
    {
        return Ok(index);
    }
    if outputs.len() == 1 {
        return Ok(0);
    }
    if let Some(index) = outputs
        .iter()
        .position(|output| output.name.is_none() && output.size() == (screen.width, screen.height))
    {
        return Ok(index);
    }
    bail!(
        "合成器没有提供名为 {} 的输出（可用：{}）",
        screen.device_name,
        outputs
            .iter()
            .map(|output| output.name.as_deref().unwrap_or("未命名"))
            .collect::<Vec<_>>()
            .join("、")
    )
}

/// Whether this compositor serves the protocol at all. One connection and one
/// registry round trip: no frame is asked for, so nobody is asked for
/// anything. The startup probe uses this to tell a compositor that will copy
/// from one that will fall through to the portal.
pub(crate) fn available() -> bool {
    let Ok(connection) = Connection::connect_to_env() else {
        return false;
    };
    let mut queue = connection.new_event_queue::<State>();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());
    let mut state = State {
        connection: Some(connection),
        ..Default::default()
    };
    queue.roundtrip(&mut state).is_ok() && state.manager.is_some()
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_screencopy_manager_v1" => {
                    if version < 3 {
                        // The copy is asked for from `buffer_done`, which
                        // arrived at 3: an older compositor would never
                        // deliver a frame, and the caller would sit on a black
                        // picture with nothing to report. Every compositor
                        // that serves the protocol today is at 3 or above, so
                        // this is a guard, not a fix.
                        state.error = Some(format!(
                            "合成器的 zwlr_screencopy_manager_v1 只有版本 {version}，缺少 buffer_done，无法采集"
                        ));
                        return;
                    }
                    state.manager = Some(
                        registry.bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(
                            name,
                            version.min(3),
                            handle,
                            (),
                        ),
                    );
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                        name,
                        version.min(1),
                        handle,
                        (),
                    ));
                }
                "wl_output" => state.outputs.push(Output {
                    proxy: registry.bind::<wl_output::WlOutput, _, _>(
                        name,
                        version.min(OUTPUT_VERSION),
                        handle,
                        (),
                    ),
                    name: None,
                    mode: (0, 0),
                    transform: Transform::Normal,
                }),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state
            .outputs
            .iter_mut()
            .find(|entry| entry.proxy == *output)
        else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.name = Some(name),
            // The transform lives here and nowhere else: no later protocol
            // revision reports it another way.
            wl_output::Event::Geometry { transform, .. } => {
                if let Ok(transform) = transform.into_result() {
                    entry.transform = transform;
                }
            }
            wl_output::Event::Mode {
                width,
                height,
                flags,
                ..
            } if flags
                .into_result()
                .is_ok_and(|flags| flags.contains(wl_output::Mode::Current)) =>
            {
                entry.mode = (width.max(0) as u32, height.max(0) as u32)
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let Ok(format) = format.into_result() else {
                    state.error = Some("合成器给出了未知的共享内存像素格式".into());
                    return;
                };
                state.wanted = Some((format as u32, width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                if let Some(error) = state.copy(handle).err() {
                    state.error = Some(format!("{error:#}"));
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            // A dma-buf offer is not taken up: see the module comment.
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

impl State {
    /// Points the compositor at the shared buffer for this frame, allocating it
    /// on the first frame and whenever the output's shape changes.
    fn copy(&mut self, handle: &QueueHandle<Self>) -> Result<()> {
        let (format, width, height, stride) = self.wanted.context("帧没有说明缓冲格式")?;
        ensure!(width > 0 && height > 0 && stride > 0, "帧的缓冲尺寸无效");
        let stale = self
            .shm_buffer
            .as_ref()
            .is_none_or(|shm| !shm.matches(format, width, height, stride));
        if stale {
            let shm = self.shm.as_ref().context("采集已关闭")?;
            self.shm_buffer = Some(allocate(shm, format, width, height, stride, handle)?);
        }
        let shm = self.shm_buffer.as_ref().context("没有共享缓冲")?;
        if let Some(frame) = &self.frame {
            frame.copy_with_damage(&shm.buffer);
        }
        Ok(())
    }
}

/// Whether the compositor's pixels arrive with red and blue the other way
/// round, for the formats this backend takes. `argb8888` and `xrgb8888` store
/// B,G,R,X — what the encoder wants. `abgr8888` and `xbgr8888` store R,G,B,X
/// and have to be swapped. Everything else is refused: a 10-bit format read as
/// 8-bit is noise, and a YUV one is not even the right shape.
fn swapped_channels(format: u32) -> Result<bool> {
    use wayland_client::protocol::wl_shm::Format;
    Ok(match Format::try_from(format) {
        Ok(Format::Argb8888) | Ok(Format::Xrgb8888) => false,
        Ok(Format::Abgr8888) | Ok(Format::Xbgr8888) => true,
        _ => bail!("不支持的共享内存像素格式 {format:#x}（只支持 32 位 ARGB/XRGB/ABGR/XBGR）"),
    })
}

/// The shared memory the compositor writes into: a memfd, mapped here and
/// exported as a pool with a single buffer over the whole of it.
fn allocate(
    shm: &wl_shm::WlShm,
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
    handle: &QueueHandle<State>,
) -> Result<Shm> {
    use wayland_client::protocol::wl_shm::Format;
    let format = Format::try_from(format)
        .map_err(|()| anyhow::anyhow!("未知的共享内存像素格式 {format:#x}"))?;
    let swap = swapped_channels(format as u32)?;
    // A row of this buffer is four bytes per pixel wide, and a compositor that
    // says otherwise is describing something this backend cannot read: every
    // compositor in use writes `4 * width`, but `picture` indexes rows by that
    // width, so it is checked rather than trusted.
    ensure!(
        u64::from(stride) >= u64::from(width) * 4,
        "合成器给出的行距 {stride} 小于一行的宽度 {}",
        u64::from(width) * 4
    );
    let length = stride as usize * height as usize;
    let name = c"openuuyc-screencopy";
    // Safe because the name is a valid C string and the flags are plain bits.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("无法创建共享内存：{}", std::io::Error::last_os_error());
    }
    // Safe because `memfd_create` returned a fresh descriptor on success.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Safe because `fd` is a live descriptor and the length was just computed.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), length as libc::off_t) } < 0 {
        bail!("无法调整共享内存大小：{}", std::io::Error::last_os_error());
    }
    let pool = shm.create_pool(fd.as_fd(), length as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        handle,
        (),
    );
    // Safe because the mapping covers the whole of a descriptor this process
    // owns, and it is unmapped again in `Shm::drop`.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        bail!("无法映射共享内存：{}", std::io::Error::last_os_error());
    }
    Ok(Shm {
        _fd: fd,
        _pool: pool,
        map: map.cast(),
        length,
        buffer,
        format: format as u32,
        width,
        height,
        stride,
        swap,
    })
}

#[cfg(test)]
mod tests {
    use super::orient;
    use wayland_client::protocol::wl_output::Transform;

    /// A `width`x`height` BGRA picture whose pixels count up from 1, one value
    /// per pixel repeated across its four bytes.
    fn numbered(width: u32, height: u32) -> Vec<u8> {
        (1..=(width * height))
            .flat_map(|value| [value as u8; 4])
            .collect()
    }

    /// The value of each pixel, in order.
    fn values(pixels: &[u8]) -> Vec<u8> {
        pixels.chunks_exact(4).map(|pixel| pixel[0]).collect()
    }

    #[test]
    fn normal_leaves_the_rows_alone() {
        let (pixels, width, height) = orient(numbered(2, 3), 2, 3, Transform::Normal);
        assert_eq!((width, height), (2, 3));
        assert_eq!(values(&pixels), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn turning_swaps_the_shape_and_keeps_every_pixel() {
        let source = numbered(4, 3);
        let mut seen = Vec::new();
        for transform in [
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let (pixels, width, height) = orient(source.clone(), 4, 3, transform);
            let turned = matches!(
                transform,
                Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
            );
            assert_eq!(
                (width, height),
                if turned { (3, 4) } else { (4, 3) },
                "{transform:?} size"
            );
            let mut sorted = values(&pixels);
            sorted.sort_unstable();
            assert_eq!(
                sorted,
                (1..=12).collect::<Vec<u8>>(),
                "{transform:?} lost or duplicated a pixel"
            );
            seen.push(values(&pixels));
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 7, "a transform repeats another one");
    }

    #[test]
    fn a_quarter_turn_is_clockwise_and_270_undoes_it() {
        let source = numbered(2, 3);
        let (turned, width, height) = orient(source.clone(), 2, 3, Transform::_90);
        assert_eq!((width, height), (3, 2));
        assert_eq!(values(&turned), vec![5, 3, 1, 6, 4, 2]);
        let (back, width, height) = orient(turned, 3, 2, Transform::_270);
        assert_eq!((width, height), (2, 3));
        assert_eq!(back, source);
    }

    #[test]
    fn a_flipped_quarter_turn_is_the_plain_one_mirrored() {
        let source = numbered(2, 3);
        let (plain, _, _) = orient(source.clone(), 2, 3, Transform::_90);
        let (flipped, _, _) = orient(source.clone(), 2, 3, Transform::Flipped90);
        assert_eq!(values(&plain), vec![5, 3, 1, 6, 4, 2]);
        assert_eq!(values(&flipped), vec![1, 3, 5, 2, 4, 6]);
        let (plain, _, _) = orient(source.clone(), 2, 3, Transform::_270);
        let (flipped, _, _) = orient(source, 2, 3, Transform::Flipped270);
        assert_eq!(values(&plain), vec![2, 4, 6, 1, 3, 5]);
        assert_eq!(values(&flipped), vec![6, 4, 2, 5, 3, 1]);
    }

    #[test]
    fn only_the_two_byte_orders_of_32_bit_are_taken() {
        use super::swapped_channels;
        use wayland_client::protocol::wl_shm::Format;
        // What the encoder reads as BGRA already.
        assert!(!swapped_channels(Format::Argb8888 as u32).unwrap());
        assert!(!swapped_channels(Format::Xrgb8888 as u32).unwrap());
        // Red and blue the other way round, which `picture` swaps.
        assert!(swapped_channels(Format::Abgr8888 as u32).unwrap());
        assert!(swapped_channels(Format::Xbgr8888 as u32).unwrap());
        // 10-bit, YUV and anything unknown is refused rather than misread.
        assert!(swapped_channels(Format::Xbgr2101010 as u32).is_err());
        assert!(swapped_channels(Format::Rgb565 as u32).is_err());
        assert!(swapped_channels(Format::Nv12 as u32).is_err());
        assert!(swapped_channels(0xdead_beef).is_err());
    }

    #[test]
    fn flipping_mirrors_without_turning() {
        let (pixels, _, _) = orient(numbered(3, 2), 3, 2, Transform::Flipped);
        assert_eq!(values(&pixels), vec![3, 2, 1, 6, 5, 4]);
        let (pixels, _, _) = orient(numbered(3, 2), 3, 2, Transform::Flipped180);
        assert_eq!(values(&pixels), vec![4, 5, 6, 1, 2, 3]);
    }
}
