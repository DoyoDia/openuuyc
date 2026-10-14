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

use anyhow::{Context, Result, ensure};
use openuuyc_vkms::protocol::{Reply, Request, MAX_DIMENSION, MAX_HZ, MIN_DIMENSION};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// systemd 的 `openuuyc-vkms.socket` 监听这个路径。
const DEFAULT_SOCKET: &str = "/run/openuuyc-vkms/control.sock";
/// 任何人都连得上，因为「谁可以」不是靠文件权限判的，而是连上以后按会话判的
/// （见 [`peer::authorize`]）。这样包里既不用建组，也不用装 polkit 策略。
const SOCKET_MODE: u32 = 0o666;

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
    let instance = match configfs::Instance::open() {
        Ok(instance) => instance,
        Err(error) => fail(error.context("初始化虚拟显示器设备失败")),
    };
    note(&format!(
        "已就绪，监听 {socket}，可用 {} 块虚拟显示器",
        instance.capacity().unwrap_or(0)
    ));

    let shared = Arc::new(Shared::new(instance));
    watch_signals();
    let mut connections = 0u64;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                connections += 1;
                let id = connections;
                let shared = Arc::clone(&shared);
                // 每条连接一个线程：主程序会一直连着不放，串行处理会让诊断页的
                // 探测排在它后面永远不返回。
                if let Err(error) = std::thread::Builder::new()
                    .name(format!("client-{id}"))
                    .spawn(move || serve(&shared, id, stream))
                {
                    note(&format!("无法处理新连接：{error}"));
                }
            }
            Err(error) => {
                if stopping() {
                    break;
                }
                note(&format!("接受连接失败：{error}"));
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        if stopping() {
            break;
        }
    }
    // 退出前把所有路断开：我们这个进程没了以后，屏幕上不该还留着一块没人管的屏。
    if let Err(error) = shared.instance.release_all() {
        note(&format!("退出时断开连接器失败：{error:#}"));
    }
    note("已退出");
}

struct Shared {
    instance: configfs::Instance,
    /// 全局占用表：哪一路被哪条连接占着。跨连接共享，所以不同客户端不会互相抢。
    taken: Mutex<BTreeMap<usize, (configfs::Lease, u64)>>,
}

impl Shared {
    fn new(instance: configfs::Instance) -> Self {
        Self {
            instance,
            taken: Mutex::new(BTreeMap::new()),
        }
    }

    fn create(&self, id: u64, width: u32, height: u32, hz: u32) -> Result<String> {
        validate(width, height, hz)?;
        let mut taken = lock(&self.taken);
        let slots = taken.keys().copied().collect::<BTreeSet<_>>();
        let lease = self.instance.attach(&slots)?;
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
        let mut taken = lock(&self.taken);
        let Some((slot, _)) = taken
            .iter()
            .find(|(_, (lease, owner))| lease.connector() == connector && *owner == id)
        else {
            anyhow::bail!("{connector} 不是这条连接创建的虚拟显示器");
        };
        let slot = *slot;
        self.instance.detach(&taken[&slot].0)?;
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
            if let Err(error) = self.instance.detach(&lease) {
                note(&format!(
                    "连接 {id} 断开 {} 失败：{error:#}",
                    lease.connector()
                ));
                continue;
            }
            taken.remove(&slot);
        }
    }

    fn status(&self, id: u64) -> Result<openuuyc_vkms::protocol::Status> {
        let taken = lock(&self.taken);
        Ok(openuuyc_vkms::protocol::Status {
            capacity: self.instance.capacity()? as u32,
            leased: taken
                .values()
                .filter(|(_, owner)| *owner == id)
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
    // 拒绝要在读到请求之前给出：不然对方只会看到一条莫名其妙的断开。
    let refusal = peer::authorize(&credentials).err();
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
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                note(&format!("连接 {id} 读取失败：{error}"));
                break;
            }
        }
        let reply = match serde_json::from_str::<Request>(line.trim()) {
            Ok(request) => match &refusal {
                Some(error) => Reply::failed(format!("{error:#}")),
                None => match request {
                    Request::Create {
                        width,
                        height,
                        hz,
                    } => match shared.create(id, width, height, hz) {
                        Ok(connector) => Reply::connector(connector),
                        Err(error) => Reply::failed(error),
                    },
                    Request::Destroy { connector } => match shared.destroy(id, &connector) {
                        Ok(()) => Reply::done(),
                        Err(error) => Reply::failed(error),
                    },
                    Request::Status => match shared.status(id) {
                        Ok(status) => Reply::status(status),
                        Err(error) => Reply::failed(error),
                    },
                },
            },
            Err(error) => Reply::failed(format!("请求无法解析：{error}")),
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
    }
    // 连接断了就是租约到期。主程序崩溃、被杀、正常退出，走的都是这里。
    shared.release(id);
}

fn validate(width: u32, height: u32, hz: u32) -> Result<()> {
    let size = MIN_DIMENSION..=MAX_DIMENSION;
    ensure!(size.contains(&width) && size.contains(&height), "无效的虚拟显示器尺寸");
    ensure!((1..=MAX_HZ).contains(&hz), "无效的虚拟显示器刷新率");
    Ok(())
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
    let listener = UnixListener::bind(path)
        .with_context(|| format!("无法监听 {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .context("无法设置套接字权限")?;
    Ok(listener)
}

fn activated() -> bool {
    let pid = std::env::var("LISTEN_PID").ok().and_then(|v| v.parse::<u32>().ok());
    let fds = std::env::var("LISTEN_FDS").ok().and_then(|v| v.parse::<u32>().ok());
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
}
