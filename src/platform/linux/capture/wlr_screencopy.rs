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
//! `copy_with_damage` is used rather than `copy`: the compositor then holds the
//! frame back until something has actually changed, which is what keeps an idle
//! desktop from being encoded and sent over and over.
//!
//! The copy carries no scaling: the buffer has to be exactly the output's size,
//! and the caller scales afterwards like it does for the portal.
use super::Screen;
use super::Picture;
use anyhow::{Context, Result, bail, ensure};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

/// `wl_output.name` arrived at 4, and it is how a captured monitor is paired
/// with the one the caller asked for.
const OUTPUT_VERSION: u32 = 4;

/// One output the compositor offers to copy.
struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    size: (u32, u32),
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
}

impl Shm {
    fn matches(&self, format: u32, width: u32, height: u32, stride: u32) -> bool {
        self.format == format
            && self.width == width
            && self.height == height
            && self.stride == stride
    }

    /// The picture as tightly packed BGRA rows, without the stride padding.
    fn picture(&self) -> Picture {
        let row = self.width as usize * 4;
        // Safe because the mapping is live for as long as `self` is, and the
        // compositor has finished with it: a `ready` event preceded this.
        let all = unsafe { std::slice::from_raw_parts(self.map, self.length) };
        let pixels = if self.stride as usize == row {
            all[..row * self.height as usize].to_vec()
        } else {
            let mut out = Vec::with_capacity(row * self.height as usize);
            for line in all
                .chunks_exact(self.stride as usize)
                .take(self.height as usize)
            {
                out.extend_from_slice(&line[..row]);
            }
            out
        };
        Picture {
            width: self.width,
            height: self.height,
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
        ensure!(
            state.manager.is_some(),
            "合成器没有提供 zwlr_screencopy_manager_v1"
        );
        ensure!(state.shm.is_some(), "合成器没有提供 wl_shm");
        ensure!(!state.outputs.is_empty(), "合成器没有提供任何输出");
        let output = pick(&state.outputs, screen)?;
        tracing::info!(
            output = screen.device_name,
            "host wlr-screencopy output selected"
        );
        Ok(Self {
            queue,
            state,
            cursor,
            output,
        })
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
            if let Some(error) = self.state.error.take() {
                bail!("{error}");
            }
            if std::mem::take(&mut self.state.ready) {
                let picture = self.state.shm_buffer.as_ref().map(Shm::picture);
                // Ask for the next frame before returning, so the compositor
                // draws into the buffer while the caller encodes this one.
                self.request()?;
                if let Some(picture) = picture {
                    return Ok(Some(picture));
                }
            }
            if std::mem::take(&mut self.state.failed) {
                self.request()?;
                bail!("合成器拒绝了屏幕复制");
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
        let output = self
            .state
            .outputs
            .get(self.output)
            .context("采集的输出已消失")?;
        self.state.wanted = None;
        self.state.frame =
            Some(manager.capture_output(i32::from(self.cursor), &output.proxy, &handle, ()));
        Ok(())
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
        .position(|output| output.name.is_none() && output.size == (screen.width, screen.height))
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
                    size: (0, 0),
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
            wl_output::Event::Mode {
                width,
                height,
                flags,
                ..
            } if flags
                .into_result()
                .is_ok_and(|flags| flags.contains(wl_output::Mode::Current)) =>
            {
                entry.size = (width.max(0) as u32, height.max(0) as u32)
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
        .map_err(|()| anyhow::anyhow!("不支持的共享内存像素格式 {format:#x}"))?;
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
    })
}
