//! OpenUUYC 的虚拟显示器辅助程序。以 root 运行，只做一件事：按主程序要的分辨率
//! 把 VKMS 的一块连接器接上或断开。
//!
//! 为什么要一个 root 的进程：凭空多出一块显示器要写 `/sys/kernel/config/vkms`，
//! 那是 configfs，内核不会把它的权限放给任何非 root 的进程（不像 `/dev/uinput`
//! 能靠 udev 规则授权），所以除了一个只做这一件事的小程序，没有别的选择。
//!
//! 它刻意不依赖任何应用代码，也不做日志框架：特权进程的每一行都该是能一眼看完的。
mod configfs;
mod peer;

use anyhow::{Context, Result, bail, ensure};
use openuuyc_vkms::protocol::{MAX_DIMENSION, MAX_HZ, MIN_DIMENSION, Reply, Request};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// systemd 的 `openuuyc-vkms.socket` 监听这个路径。
const DEFAULT_SOCKET: &str = "/run/openuuyc-vkms/control.sock";
/// 任何人都连得上，因为「谁可以」不是靠文件权限判的，而是连上以后按会话判的
/// （见 [`peer::authorize`]）。这样包里既不用建组，也不用装 polkit 策略。
const SOCKET_MODE: u32 = 0o666;
/// 空转检查退出标志的节拍。短到关机时察觉不到，长到空闲时不烧 CPU。
const STOP_POLL: Duration = Duration::from_millis(200);
/// 同时接受多少条连接。套接字是 0666 的，一个本地用户随手就能开上几千条，而每条连接
/// 一个线程；超过就直接关掉。正常用法（主程序一条、诊断页每 5 秒探一下）离这个数很远。
const MAX_CONNECTIONS: usize = 16;
/// 一行请求的长度上限。请求都是几十字节的 JSON，没人会写满它。
const MAX_LINE: usize = 4096;
/// 还没租到屏的连接多久没动静就断开。主程序真要用时会重新连（见
/// `src/platform/linux/virtual_display.rs`），空闲连接不值得一直占着一个线程。
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// 没有连接也没有租约时，自己退出的时间。套接字单元还在监听，下次连接会把它拉起来。
const IDLE_EXIT: Duration = Duration::from_secs(60);

const EXIT_ERROR: i32 = 1;

fn main() {
    let socket = match socket_path() {
        Ok(socket) => socket,
        Err(error) => fail(error),
    };
    let listener = match listen(&socket) {
        Ok(listener) => listener,
        Err(error) => fail(error),
    };
    // 设备建不出来也要继续监听。启动失败就退出的话，`Restart=on-failure` 会在一秒内
    // 重试 5 次，撞上 start limit 之后连套接字单元一起变成 failed
    // （`service-start-limit-hit`），诊断页只会看到「连不上」，看不到真正的原因；
    // 而内核太老、模块没编进去这类问题重试多少次都好不了。所以照常起来，把原因原样
    // 报给每一个请求。
    let device = match configfs::Instance::open() {
        Ok(instance) => {
            note(&format!(
                "已就绪，监听 {socket}，可用 {} 块虚拟显示器",
                instance.capacity().unwrap_or(0)
            ));
            Device::Ready(instance)
        }
        Err(error) => {
            let reason = format!("{error:#}");
            note(&format!(
                "已就绪，监听 {socket}，但用不了虚拟显示器：{reason}"
            ));
            Device::Unavailable(reason)
        }
    };

    let shared = Arc::new(Shared::new(device));
    watch_signals();
    let live = Arc::new(AtomicUsize::new(0));
    let mut connections = 0u64;
    let mut idle_since = Instant::now();
    loop {
        if stopping() {
            break;
        }
        // 退出的节拍握在这里，而不是指望 SIGTERM 把 accept 打断：信号是发给整个进程的，
        // 内核可能把它投递给某条连接线程（每条连接一个线程），主线程的 accept 于是照旧
        // 阻塞，收尾逻辑永远轮不到——systemd 只能等满 TimeoutStopSec 再 SIGKILL，
        // 关机就白卡 90 秒。
        if !readable(&listener, STOP_POLL) {
            // 「不常驻」得名副其实：拉起一次就一直跑到关机的话，单元里写的就是假话。
            if live.load(Ordering::Acquire) == 0 && idle_since.elapsed() >= IDLE_EXIT {
                note("空闲超时，退出");
                break;
            }
            continue;
        }
        idle_since = Instant::now();
        match listener.accept() {
            Ok((stream, _)) => {
                if live.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                    note(&format!(
                        "同时连接数已达上限（{MAX_CONNECTIONS}），拒绝新连接"
                    ));
                    continue;
                }
                connections += 1;
                let id = connections;
                let shared = Arc::clone(&shared);
                let counter = Arc::clone(&live);
                live.fetch_add(1, Ordering::AcqRel);
                // 每条连接一个线程：主程序会一直连着不放，串行处理会让诊断页的
                // 探测排在它后面永远不返回。
                let spawned = std::thread::Builder::new()
                    .name(format!("client-{id}"))
                    .spawn(move || {
                        serve(&shared, id, stream);
                        counter.fetch_sub(1, Ordering::AcqRel);
                    });
                if let Err(error) = spawned {
                    live.fetch_sub(1, Ordering::AcqRel);
                    note(&format!("无法处理新连接：{error}"));
                }
            }
            Err(error) => {
                if stopping() {
                    break;
                }
                note(&format!("接受连接失败：{error}"));
                std::thread::sleep(STOP_POLL);
            }
        }
    }
    // 退出前把所有路断开：我们这个进程没了以后，屏幕上不该还留着一块没人管的屏。
    shared.release_all();
    note("已退出");
}

/// 设备这一侧的状态。用不了的时候进程照样活着，只是把原因如实报给请求方。
enum Device {
    Ready(configfs::Instance),
    /// 原因会原样转给主控和诊断页，例如「本机内核 6.8 的 VKMS 不支持 configfs」。
    Unavailable(String),
}

impl Device {
    /// 用不了的原因，能用就是 `None`。
    fn unavailable(&self) -> Option<&str> {
        match self {
            Self::Ready(_) => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }

    fn instance(&self) -> Result<&configfs::Instance> {
        match self {
            Self::Ready(instance) => Ok(instance),
            Self::Unavailable(reason) => bail!("{reason}"),
        }
    }
}

struct Shared {
    device: Device,
    /// 全局占用表：哪一路被哪条连接占着。跨连接共享，所以不同客户端不会互相抢。
    ///
    /// 租约跟着连接走，不跟着「现在是谁在用这台机器」走：A 登录时建了一块屏、然后
    /// 快速切到用户 B，那块屏归 B 的合成器管，租约却还在 A 的 OpenUUYC 手里，直到
    /// 它断开。可以接受——切换用户之后 A 的会话本来也还在屏幕上，而这里是同一台机器、
    /// 同一个 uid 边界内的事。
    taken: Mutex<BTreeMap<usize, (configfs::Lease, u64)>>,
}

impl Shared {
    fn new(device: Device) -> Self {
        Self {
            device,
            taken: Mutex::new(BTreeMap::new()),
        }
    }

    /// 设备本身用不了的原因，没有就是 `None`。每个请求都要先看这个。
    fn unavailable(&self) -> Option<&str> {
        self.device.unavailable()
    }

    /// 这条连接现在租着几块屏。连接算不算「空闲」由它决定。
    fn held(&self, id: u64) -> usize {
        lock(&self.taken)
            .values()
            .filter(|(_, owner)| *owner == id)
            .count()
    }

    fn release_all(&self) {
        if let Device::Ready(instance) = &self.device
            && let Err(error) = instance.release_all()
        {
            note(&format!("退出时断开连接器失败：{error:#}"));
        }
    }

    fn create(&self, id: u64, width: u32, height: u32, hz: u32) -> Result<String> {
        validate(width, height, hz)?;
        let instance = self.device.instance()?;
        let mut taken = lock(&self.taken);
        let slots = taken.keys().copied().collect::<BTreeSet<_>>();
        let lease = instance.attach(&slots)?;
        note(&format!(
            "连接 {id} 拿到 {}（{width}x{height}@{hz}）",
            lease.connector()
        ));
        let connector = lease.connector().to_owned();
        taken.insert(lease.slot(), (lease, id));
        Ok(connector)
    }

    /// 只能还自己拿到的：名字由辅助程序发出，也在辅助程序这里核对属于谁。
    fn destroy(&self, id: u64, connector: &str) -> Result<()> {
        let instance = self.device.instance()?;
        let mut taken = lock(&self.taken);
        let Some((slot, _)) = taken
            .iter()
            .find(|(_, (lease, owner))| lease.connector() == connector && *owner == id)
        else {
            bail!("{connector} 不是这条连接创建的虚拟显示器");
        };
        let slot = *slot;
        instance.detach(&taken[&slot].0)?;
        taken.remove(&slot);
        note(&format!("连接 {id} 归还 {connector}"));
        Ok(())
    }

    fn release(&self, id: u64) {
        let mut taken = lock(&self.taken);
        let mine = taken
            .iter()
            .filter(|(_, (_, owner))| *owner == id)
            .map(|(slot, (lease, _))| (*slot, lease.clone()))
            .collect::<Vec<_>>();
        for (slot, lease) in mine {
            if let Err(error) = self.device.instance().and_then(|i| i.detach(&lease)) {
                note(&format!(
                    "连接 {id} 断开 {} 失败：{error:#}",
                    lease.connector()
                ));
                // 断开失败也要把它从占用表里拿掉：留着的话这一路在重启之前都用不了，
                // 而且记在一条已经断开的连接名下，谁都还不回来。
                taken.remove(&slot);
                continue;
            }
            taken.remove(&slot);
        }
    }

    fn status(&self, id: u64) -> Result<openuuyc_vkms::protocol::Status> {
        let instance = self.device.instance()?;
        let taken = lock(&self.taken);
        Ok(openuuyc_vkms::protocol::Status {
            capacity: instance.capacity()? as u32,
            // 本连接租的，和整机现在占着的（不带属主）：诊断页每次都是一条新连接，
            // 只看前者永远显示不出「已在使用」。
            leased: taken
                .values()
                .filter(|(_, owner)| *owner == id)
                .map(|(lease, _)| lease.connector().to_owned())
                .collect(),
            in_use: taken
                .values()
                .map(|(lease, _)| lease.connector().to_owned())
                .collect(),
        })
    }
}

/// 一条连接：先认人，再按行收请求。
fn serve(shared: &Shared, id: u64, stream: UnixStream) {
    let credentials = match peer::credentials(&stream) {
        Ok(credentials) => credentials,
        Err(error) => {
            note(&format!("连接 {id} 无法识别：{error:#}"));
            return;
        }
    };
    // 拒绝要在读到请求之前就能给出：不然对方只会看到一条莫名其妙的断开。
    let (session, refusal) = match peer::authorize(&credentials) {
        Ok(session) => (Some(session), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            note(&format!("连接 {id} 无法复制套接字：{error}"));
            return;
        }
    };
    let mut writer = writer;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // 还没租到屏的连接设短的读超时：套接字是 0666 的，不设的话一条不发数据的连接
    // 就能一直占着一个线程。租到屏的连接把超时撤掉——主程序会把它开一整个会话。
    let mut idle = false;
    let mut logged = false;
    set_idle(&writer, true, &mut idle);
    loop {
        line.clear();
        match read_request(&mut reader, &mut line) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                note(&format!("连接 {id} 读取失败：{error}"));
                break;
            }
        }
        let reply = match &refusal {
            // 拒绝原因要读到请求之后再回：先回的话，对端已经写进来的那些字节还在接收
            // 缓冲里，close(2) 会送 RST，刚写出去的应答可能就丢了。
            Some(error) => Reply::failed(error.clone()),
            None => match serde_json::from_str::<Request>(line.trim()) {
                Ok(request) => match shared.unavailable() {
                    Some(reason) => Reply::unsupported(reason),
                    None => {
                        // 只在真的动内核的时候记一行：诊断页每 5 秒探一次的纯粹查询
                        // 不值得写进 journal，写下来就是刷屏。
                        if let Some(session) = session.as_deref()
                            && !logged
                            && matches!(request, Request::Create { .. } | Request::Destroy { .. })
                        {
                            logged = true;
                            note(&format!(
                                "放行 pid={} uid={} gid={} 会话={session}",
                                credentials.pid, credentials.uid, credentials.gid
                            ));
                        }
                        match request {
                            Request::Create { width, height, hz } => {
                                match shared.create(id, width, height, hz) {
                                    Ok(connector) => Reply::connector(connector),
                                    Err(error) => Reply::failed(error),
                                }
                            }
                            Request::Destroy { connector } => {
                                match shared.destroy(id, &connector) {
                                    Ok(()) => Reply::done(),
                                    Err(error) => Reply::failed(error),
                                }
                            }
                            Request::Status => match shared.status(id) {
                                Ok(status) => Reply::status(status),
                                Err(error) => Reply::failed(error),
                            },
                        }
                    }
                },
                Err(error) => Reply::failed(format!("请求无法解析：{error}")),
            },
        };
        let mut text = match serde_json::to_string(&reply) {
            Ok(text) => text,
            Err(error) => {
                note(&format!("连接 {id} 应答无法序列化：{error}"));
                break;
            }
        };
        text.push('\n');
        if let Err(error) = writer.write_all(text.as_bytes()) {
            note(&format!("连接 {id} 应答失败：{error}"));
            break;
        }
        // 被拒绝的对端回一句就走：继续读下去只是给它一条长连接占着这里的机会。
        if refusal.is_some() {
            break;
        }
        set_idle(&writer, shared.held(id) == 0, &mut idle);
    }
    // 连接断了就是租约到期。主程序崩溃、被杀、正常退出，走的都是这里。
    shared.release(id);
}

/// 按需切换空闲读超时。`idle` 是目标状态，`current` 记着现在是什么状态，省一次系统调用。
fn set_idle(writer: &UnixStream, idle: bool, current: &mut bool) {
    if *current == idle {
        return;
    }
    if let Err(error) = writer.set_read_timeout(idle.then_some(IDLE_TIMEOUT)) {
        note(&format!("无法调整读超时：{error}"));
    }
    *current = idle;
}

/// 读一行请求。`Ok(true)` 是拿到一行，`Ok(false)` 是对端关掉了。
///
/// 长度要封顶：没有上限的 `read_line` 意味着一个本地用户发一条不带换行的 1 GiB 数据
/// 就能把这个 root 进程的内存吃光。
fn read_request(reader: &mut impl BufRead, line: &mut String) -> Result<bool> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let (consumed, complete) = {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                // 对端关掉了。写了一半的行不算一条请求。
                return Ok(false);
            }
            match chunk.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    bytes.extend_from_slice(&chunk[..=index]);
                    (index + 1, true)
                }
                None => {
                    bytes.extend_from_slice(chunk);
                    (chunk.len(), false)
                }
            }
        };
        reader.consume(consumed);
        if complete {
            line.push_str(&String::from_utf8_lossy(&bytes));
            return Ok(true);
        }
        if bytes.len() >= MAX_LINE {
            bail!("请求超过 {MAX_LINE} 字节");
        }
    }
}

fn validate(width: u32, height: u32, hz: u32) -> Result<()> {
    let size = MIN_DIMENSION..=MAX_DIMENSION;
    ensure!(
        size.contains(&width) && size.contains(&height),
        "无效的虚拟显示器尺寸"
    );
    ensure!((1..=MAX_HZ).contains(&hz), "无效的虚拟显示器刷新率");
    Ok(())
}

/// 等监听套接字上来连接，最多等 `timeout`。返回它是不是可读了。
fn readable(listener: &UnixListener, timeout: Duration) -> bool {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: 描述符在这次调用期间一直有效，pollfd 数组只有一个元素。
    unsafe { libc::poll(&mut descriptor, 1, timeout.as_millis() as libc::c_int) > 0 }
}

/// systemd 套接字激活时监听套接字是 fd 3，不必自己建、也不该自己删。
fn socket_path() -> Result<String> {
    let mut args = std::env::args().skip(1);
    match args.next() {
        None => Ok(DEFAULT_SOCKET.to_owned()),
        Some(arg) if arg == "--socket" => args.next().context("--socket 后面要跟路径"),
        Some(other) => anyhow::bail!("无法识别的参数：{other}"),
    }
}

fn listen(path: &str) -> Result<UnixListener> {
    if activated() {
        note("由 systemd 套接字激活启动");
        // SAFETY: LISTEN_FDS/LISTEN_PID 表明父进程把监听套接字放在 fd 3 上了。
        return Ok(unsafe { UnixListener::from_raw_fd(3) });
    }
    let path = std::path::Path::new(path);
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("无法创建 {}", directory.display()))?;
    }
    // 上一次没清干净的套接字文件会让 bind 失败，而它一定是没人监听的。
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).context("无法清理旧的套接字");
        }
        _ => {}
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("无法监听 {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .context("无法设置套接字权限")?;
    Ok(listener)
}

fn activated() -> bool {
    let pid = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    let fds = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    pid == Some(std::process::id()) && fds.unwrap_or(0) >= 1
}

static STOPPING: AtomicBool = AtomicBool::new(false);

fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire)
}

/// systemd 停服务时发 SIGTERM。handler 只置一个标志，真正的收尾（断开所有连接器）
/// 回到主线程做——信号处理函数里不能做那些事。
fn watch_signals() {
    extern "C" fn on_signal(_: libc::c_int) {
        STOPPING.store(true, Ordering::Release);
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: handler 只写一个原子量，是异步信号安全的。
        unsafe {
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 日志就写到标准错误。这个程序由 systemd 拉起，stderr 直接进 journal，
/// 不值得为它拉一个日志框架进来。
pub fn note(message: &str) {
    eprintln!("openuuyc-vkms: {message}");
}

fn fail(error: anyhow::Error) -> ! {
    note(&format!("{error:#}"));
    std::process::exit(EXIT_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_dimensions_the_kernel_or_the_protocol_cannot_carry() {
        assert!(validate(1920, 1080, 60).is_ok());
        assert!(validate(1, 1080, 60).is_err());
        assert!(validate(1920, 0, 60).is_err());
        assert!(validate(1920, 1080, 0).is_err());
        assert!(validate(MAX_DIMENSION + 1, 1080, 60).is_err());
    }

    #[test]
    fn reads_one_request_line_and_stops_at_end_of_stream() {
        let mut reader = std::io::Cursor::new(b"{\"op\":\"status\"}\n".to_vec());
        let mut line = String::new();
        assert!(read_request(&mut reader, &mut line).unwrap());
        assert_eq!(line.trim(), "{\"op\":\"status\"}");
        line.clear();
        assert!(!read_request(&mut reader, &mut line).unwrap());
    }

    /// 不带换行的长数据不能把这个进程的内存吃光。
    #[test]
    fn a_request_line_is_bounded() {
        let mut reader = std::io::Cursor::new(vec![b'x'; MAX_LINE * 8]);
        let mut line = String::new();
        assert!(read_request(&mut reader, &mut line).is_err());
        assert!(line.is_empty());
    }

    /// 设备用不了的时候进程照样活着，只是每次请求都回同一句原因。
    #[test]
    fn an_unusable_device_reports_its_reason() {
        let device = Device::Unavailable("本机内核 6.8 的 VKMS 不支持 configfs".into());
        assert_eq!(
            device.unavailable(),
            Some("本机内核 6.8 的 VKMS 不支持 configfs")
        );
        assert!(device.instance().is_err());
    }
}
