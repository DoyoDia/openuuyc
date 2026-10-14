//! Wayland 那边的 drop target：接住远端用户从这块屏上拖走的那些文件，问出路径。
//!
//! X11 那条路是 `portal.rs`（一个临时 XDND 交接窗口）。Wayland 上不需要「合成
//! 一个 drop」——那也做不到，drop 只有合成器能投递。要的东西其实更简单：**我们
//! 自己当那个合法的 drop target**。拖放本来就是合成器按指针位置投递给指针底下
//! 那个客户端的；我们只要在那时候是那个客户端就行。所以这里一个事件都不合成，
//! 只是在指针底下摆一块我们自己的 surface 等着被投递。
//!
//! 和 X11 那条并存：两个世界的拖放互相看不见（XWayland 不会把 XDND 拖拽桥接给
//! Wayland 客户端，反过来也一样），谁先接住算谁的，所以两边一起摆。
use super::{portal::Event, xdnd};
use anyhow::{Context as _, Result, bail};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_output,
    wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// 接取用的方形 surface 边长，以指针为中心。用户拖着东西在动，指针每动一次合成器
/// 就重新判定一次指针底下是谁，所以只要他还在这块里就一定会交给我们；开这么大是
/// 给快速甩动留余量（每一次 motion 事件之间指针可能跳过几十像素）。
const SIDE: i32 = 512;
/// 一直没等到拖放就安静收工。整体超时由 `tick` 管（30 秒），这里不必等那么久。
const LIMIT: Duration = Duration::from_secs(20);
/// 轮询间隔。合成器有事会立刻唤醒，这个只是兜底。
const POLL: Duration = Duration::from_millis(2);
/// 等合成器给尺寸、等对端写文件列表，各自的上限。
const SETUP: Duration = Duration::from_secs(2);
const READ: Duration = Duration::from_millis(800);
/// 文件列表不可能有多大，防止对端一直写。
const MAX_LIST: usize = 1 << 20;
/// 认得出的文件列表类型。GTK 的程序在 Wayland 上只用得上第一个，但两边的来源都
/// 认一遍不花什么代价。
const MIMES: [&str; 2] = ["text/uri-list", "x-special/gnome-copied-files"];

/// 摆在那儿的接取者。丢掉或者 `cancel` 之后就收工。
pub(crate) struct Catcher {
    stop: Arc<AtomicBool>,
}
impl Catcher {
    /// `fraction` 是光标在这块屏上的归一化位置（0..1）。
    ///
    /// 不能收采集那边给的像素：那是**物理**像素，而 layer surface 的边距是**逻辑**
    /// 坐标——本机 DP-1 是 1.35 倍、DP-3 是 1.6 倍，照物理值摆出去方块会跑到
    /// 1.35 倍远的地方（实测 2304 摆到了 3110）。归一化之后由这里乘逻辑尺寸，
    /// 跟缩放是多少无关。
    pub fn start(
        device: &str,
        fraction: (f64, f64),
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let device = device.to_owned();
        std::thread::Builder::new()
            .name("Wayland drag catch".into())
            .spawn(move || {
                // 失败不报给状态机：X11 那条还在跑，这里一次摆设不成功不该把整个
                // 拖放判死。真的一直没结果，`tick` 的超时会收场。
                if let Err(error) = run(&device, fraction, &ending, &allowed, &notify) {
                    tracing::debug!(error = format!("{error:#}"), "Wayland 拖放接取失败");
                }
            })?;
        Ok(Self { stop })
    }
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    layer: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    /// 拖放事件的来源。要一直拿着，丢了就没有事件了。
    device: Option<wl_data_device::WlDataDevice>,
    /// 逻辑尺寸（`zwlr_layer_surface_v1` 的边距按它算），只问目标那块屏。
    xdg: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    /// 目标那块屏的 xdg-output。要一直拿着，丢了就收不到逻辑尺寸了。
    described: Option<zxdg_output_v1::ZxdgOutputV1>,
    logical: Option<(i32, i32)>,
    /// 合成器的输出，以及它们报上来的名字（名字要等绑定之后再走一轮才到）。
    outputs: Vec<(wl_output::WlOutput, String)>,
    /// 这次拖放带的 offer，以及它报出来的类型。
    offer: Option<wl_data_offer::WlDataOffer>,
    mimes: Vec<String>,
    /// 指针已经进到我们这块 surface 上。
    entered: bool,
    /// 已经发出 `receive`，就等读。
    requested: Option<(String, OwnedFd, OwnedFd)>,
    paths: Option<Vec<PathBuf>>,
    configure: Option<(u32, u32, u32)>,
    closed: bool,
}

fn run(
    device: &str,
    fraction: (f64, f64),
    stop: &AtomicBool,
    allowed: &Arc<dyn Fn() -> bool + Send + Sync>,
    notify: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<()> {
    let connection = Connection::connect_to_env().context("没有可用的 Wayland 会话")?;
    let mut queue = connection.new_event_queue::<State>();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());
    let mut state = State::default();
    queue
        .roundtrip(&mut state)
        .context("读取 Wayland 全局对象失败")?;
    // 输出绑定之后才会报上自己的名字，所以要再来一轮。
    queue
        .roundtrip(&mut state)
        .context("读取 Wayland 输出描述失败")?;

    let (target, compositor, layer, shm, seat, manager) = (
        state
            .outputs
            .iter()
            .find(|(_, name)| name == device)
            .map(|(output, _)| output.clone())
            .with_context(|| format!("合成器没有名为 {device} 的输出"))?,
        state.compositor.clone().context("合成器没有 wl_compositor")?,
        state.layer.clone().context("合成器没有 zwlr_layer_shell_v1")?,
        state.shm.clone().context("合成器没有 wl_shm")?,
        state.seat.clone().context("合成器没有 wl_seat")?,
        state
            .manager
            .clone()
            .context("合成器没有 wl_data_device_manager")?,
    );

    // 逻辑尺寸只能问 xdg-output：`wl_output` 给的永远是物理尺寸，而两者之间差着
    // 一个缩放（本机 DP-1 是 1.35 倍），边距却是按逻辑算的。
    let xdg = state
        .xdg
        .clone()
        .context("合成器没有 zxdg_output_manager_v1")?;
    state.described = Some(xdg.get_xdg_output(&target, &handle, ()));
    let deadline = Instant::now() + SETUP;
    while state.logical.is_none() && Instant::now() < deadline {
        dispatch(&mut queue, &mut state, &connection)?;
        wait(&mut queue, &mut state, &connection, POLL)?;
    }
    let (logical_width, logical_height) = state.logical.context("输出没有给出逻辑尺寸")?;
    let local = (
        (fraction.0 * f64::from(logical_width)).round() as i32,
        (fraction.1 * f64::from(logical_height)).round() as i32,
    );

    let surface = compositor.create_surface(&handle, ());
    let shell = layer.get_layer_surface(
        &surface,
        Some(&target),
        zwlr_layer_shell_v1::Layer::Overlay,
        "openuuyc-drag".to_owned(),
        &handle,
        (),
    );
    // 锚左上角加边距就是绝对定位：把这块方形摆在指针中心。
    shell.set_size(SIDE as u32, SIDE as u32);
    shell.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
    shell.set_margin(
        (local.1 - SIDE / 2).max(0),
        0,
        0,
        (local.0 - SIDE / 2).max(0),
    );
    // 不占键盘焦点，也绝不参与布局：`-1` 就是「别把这块算进排布里」，否则
    // niri 会为了它把底下的窗口挤走。
    shell.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    shell.set_exclusive_zone(-1);
    surface.commit();

    let deadline = Instant::now() + SETUP;
    while state.configure.is_none() && !state.closed && Instant::now() < deadline {
        dispatch(&mut queue, &mut state, &connection)?;
        wait(&mut queue, &mut state, &connection, POLL)?;
    }
    let (serial, width, height) = state.configure.context("合成器没有给出这块 surface 的尺寸")?;

    // 全透明：memfd 的页是内核给的零页，一个字节都不用写就是透明的。透明不代表
    // 收不到输入——surface 有缓冲才算上屏，上了屏才会被判定成指针底下的那个。
    let (buffer, _pool, _fd) = transparent(&shm, &handle, width, height)?;
    surface.attach(Some(&buffer), 0, 0);
    shell.ack_configure(serial);
    surface.commit();

    let data = manager.get_data_device(&seat, &handle, ());
    state.device = Some(data);

    let deadline = Instant::now() + LIMIT;
    while state.paths.is_none()
        && !state.closed
        && !stop.load(Ordering::Acquire)
        && allowed()
        && Instant::now() < deadline
    {
        dispatch(&mut queue, &mut state, &connection)?;
        connection.flush().context("发送 Wayland 请求失败")?;
        // `receive` 是个请求，得先发出去对端才会往管道里写。写完我们这一头的
        // 写端必须关掉，否则读端等不到 EOF。
        if let Some((mime, write, read)) = state.requested.take() {
            drop(write);
            let list = read_list(&read, READ)?;
            let paths = xdnd::parse_uri_list(&list);
            if paths.is_empty() {
                bail!("{mime} 里没有本地路径");
            }
            state.paths = Some(paths);
            break;
        }
        wait(&mut queue, &mut state, &connection, POLL)?;
    }

    shell.destroy();
    surface.destroy();
    if let Some(paths) = state.paths.take() {
        tracing::info!(files = paths.len(), "Wayland 拖放接住了");
        notify(Event::Captured(paths, false));
    }
    Ok(())
}

/// 读一遍待处理事件。
fn dispatch(
    queue: &mut EventQueue<State>,
    state: &mut State,
    connection: &Connection,
) -> Result<()> {
    queue.dispatch_pending(state)?;
    connection.flush().context("发送 Wayland 请求失败")?;
    Ok(())
}

/// 等 socket 上有事件，最多 `timeout`。`dispatch_pending` 只处理已经读进来的，
/// 所以这里要先把连接上的数据读出来——采集那边是同一个套路。
fn wait(
    queue: &mut EventQueue<State>,
    state: &mut State,
    connection: &Connection,
    timeout: Duration,
) -> Result<()> {
    queue.dispatch_pending(state)?;
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
    let ready = unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        drop(guard);
        bail!("等待 Wayland 事件失败：{error}");
    }
    if ready == 0 {
        drop(guard);
        return Ok(());
    }
    match guard.read() {
        Ok(_) => {}
        // socket 报告可读但什么都没剩下，是正常情况。
        Err(wayland_client::backend::WaylandError::Io(error))
            if error.kind() == std::io::ErrorKind::WouldBlock =>
        {
            return Ok(())
        }
        Err(error) => return Err(error).context("读取 Wayland 事件失败"),
    }
    Ok(())
}

/// 一块全透明的共享内存缓冲，连同撑住它的东西（池子和 fd 要活到 surface 销毁）。
fn transparent(
    shm: &wl_shm::WlShm,
    handle: &QueueHandle<State>,
    width: u32,
    height: u32,
) -> Result<(wl_buffer::WlBuffer, wl_shm_pool::WlShmPool, OwnedFd)> {
    let stride = width * 4;
    let length = (stride as usize * height as usize).max(4);
    let name = std::ffi::CString::new("openuuyc-drag")?;
    // Safe because the name is a valid C string and the flags are a plain bit set.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("创建共享内存失败：{}", std::io::Error::last_os_error());
    }
    // Safe because the descriptor was just created by us and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Safe because the descriptor is ours and open for writing.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), length as libc::off_t) } < 0 {
        bail!("调整共享内存大小失败：{}", std::io::Error::last_os_error());
    }
    let pool = shm.create_pool(fd.as_fd(), length as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        handle,
        (),
    );
    Ok((buffer, pool, fd))
}

/// 把对端写进管道的内容读完。它写完就关，所以读到 EOF 为止。
fn read_list(fd: &OwnedFd, timeout: Duration) -> Result<Vec<u8>> {
    // 非阻塞读取：`poll` 说可读之后仍然可能被别的东西抢先读空。
    // Safe because the descriptor is ours and `F_GETFL`/`F_SETFL` take no pointers.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("对端没有在时限内给出文件列表");
        }
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Safe because `poll` only writes into the one descriptor it was given.
        let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis() as i32) };
        if ready < 0 {
            bail!("等待文件列表失败：{}", std::io::Error::last_os_error());
        }
        if ready == 0 {
            continue;
        }
        // Safe because `buffer` is a live slice and the descriptor is open.
        let read = unsafe {
            libc::read(
                fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        match read {
            0 => break,
            size if size > 0 => {
                data.extend_from_slice(&buffer[..size as usize]);
                if data.len() > MAX_LIST {
                    bail!("文件列表过大");
                }
            }
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    continue;
                }
                bail!("读取文件列表失败：{error}");
            }
        }
    }
    Ok(data)
}

/// 指针进了我们这块 surface：把文件列表要过来。
fn request(state: &mut State) {
    if state.requested.is_some() || !state.entered {
        return;
    }
    let Some(offer) = state.offer.as_ref() else {
        return;
    };
    let Some(mime) = MIMES
        .iter()
        .find(|mime| state.mimes.iter().any(|offered| offered == *mime))
    else {
        return;
    };
    let (read, write) = match pipe() {
        Ok(ends) => ends,
        Err(error) => {
            tracing::debug!(%error, "建立管道失败");
            return;
        }
    };
    offer.receive(mime.to_string(), write.as_fd());
    state.requested = Some(((*mime).to_owned(), write, read));
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0i32; 2];
    // Safe because `ends` is a live array of exactly the two descriptors `pipe2` writes.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        bail!("创建管道失败：{}", std::io::Error::last_os_error());
    }
    // Safe because both descriptors were just created by us and nothing else owns them.
    unsafe { Ok((OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1]))) }
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
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                    name,
                    version.min(1),
                    handle,
                    (),
                ))
            }
            "wl_shm" => {
                state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                    name,
                    version.min(1),
                    handle,
                    (),
                ))
            }
            "wl_seat" => {
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(
                    name,
                    version.min(1),
                    handle,
                    (),
                ))
            }
            "wl_data_device_manager" => {
                state.manager = Some(registry.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(
                    name,
                    version.min(1),
                    handle,
                    (),
                ))
            }
            "zxdg_output_manager_v1" => {
                state.xdg = Some(registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(
                    name,
                    version.min(3),
                    handle,
                    (),
                ))
            }
            "zwlr_layer_shell_v1" => {
                state.layer = Some(registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                    name,
                    version.min(1),
                    handle,
                    (),
                ))
            }
            // `wl_output.name` 是 v4 才有的，也就是认这块屏靠的那个字段。
            "wl_output" => state.outputs.push((
                registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), handle, ()),
                String::new(),
            )),
            _ => {}
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
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state.outputs.iter_mut().find(|(it, _)| it == output)
        {
            entry.1 = name;
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => state.configure = Some((serial, width, height)),
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.mimes.clear();
                state.offer = Some(id);
                request(state);
            }
            // 指针落到我们这块 surface 上了。类型可能先到、也可能后到，两边都
            // 试一次，凑齐了才发 `receive`。
            wl_data_device::Event::Enter { .. } => {
                state.entered = true;
                request(state);
            }
            wl_data_device::Event::Leave { .. } => state.entered = false,
            _ => {}
        }
    }

    // `data_offer` 是「创建子对象」的事件，wayland-client 要在这里说明新对象用
    // 什么数据去建。少了这条，合成器真送来这个事件时会 panic，而那个 panic 要跨
    // 过 C 回调、没法 unwinding，**整个进程直接 abort**。本地探针碰不到这条——
    // 没有真的拖放就不会有 `data_offer`，只有真拖一次才踩得上。
    wayland_client::event_created_child!(Self, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.mimes.push(mime_type);
            request(state);
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_output_v1::Event::LogicalSize { width, height } = event {
            state.logical = Some((width, height));
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 「创建子对象」的事件必须在 `Dispatch` 里给出特化，缺了会在合成器真送来
    /// 事件时 panic，而那个 panic 直接 abort 整个进程。这条就是把这个坑钉住：
    /// 不连真拖放也能查出来。
    #[test]
    fn data_offer_has_a_child_specialization() {
        let connection = Connection::connect_to_env().expect("需要 Wayland 会话");
        let queue = connection.new_event_queue::<State>();
        let _ = <State as Dispatch<wl_data_device::WlDataDevice, ()>>::event_created_child(
            0,
            &queue.handle(),
        );
    }
}

wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);
wayland_client::delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

