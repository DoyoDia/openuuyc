//! 让第二个进程把已经跑着的那个叫到前台。
//!
//! 服务或登录自启会把控制中心以不显示主窗口的方式跑起来，用户再点一次图标时
//! 新进程必须先退出（实例锁在运行的进程手里），但退出之前要把窗口叫出来，否则
//! 点图标什么都不会发生。Windows 用注册窗口消息做这件事（见
//! [`crate::application::app::instance`]），Wayland 与 X11 都没有对应的窗口属性，
//! 这里用 `$XDG_RUNTIME_DIR` 下的一个 unix socket 代替。
//!
//! The request is one line, `show`, optionally followed by the launcher's
//! startup-notification tokens (`wayland=<token>`, `x11=<token>`), which the
//! running process uses to raise its window past focus-stealing prevention
//! and to end the launcher's busy feedback. The answer is `ok` or `error`.
//! Both ends give up after [`TIMEOUT`], and every connection is served on its
//! own thread, so a client that never finishes cannot hold up a later start.
use crate::platform::linux::startup_notify::{LaunchToken, valid};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long either end waits for the other.
const TIMEOUT: Duration = Duration::from_secs(2);
/// The longest line either end reads.
const MAX_LINE: u64 = 2048;

/// 控制通道的生命周期：`run` 结束或进程退出时把 socket 文件清掉。
pub(crate) struct Control {
    path: PathBuf,
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 控制通道的位置。只在本地运行时目录里开：那里是 0700 的，别的用户进不来，
/// 而 socket 自己再设成 0600。
fn socket_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("XDG_RUNTIME_DIR 未设置，无法建立控制通道")?;
    Ok(base.join("openuuyc/control.sock"))
}

/// What a second start asks of the running process.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Show(LaunchToken),
}

impl Command {
    fn parse(line: &str) -> Result<Self> {
        let mut words = line.split_ascii_whitespace();
        match words.next() {
            Some("show") => {
                let mut token = LaunchToken::default();
                for word in words {
                    // Unknown or malformed fields are dropped: an activation
                    // without a token still shows the window.
                    match word.split_once('=') {
                        Some(("wayland", value)) if valid(value) => {
                            token.wayland = Some(value.to_owned())
                        }
                        Some(("x11", value)) if valid(value) => token.x11 = Some(value.to_owned()),
                        _ => tracing::debug!(field = word, "控制请求里有无法识别的字段"),
                    }
                }
                Ok(Self::Show(token))
            }
            _ => bail!("未知的控制请求 {}", line.trim()),
        }
    }

    fn line(&self) -> String {
        let Self::Show(token) = self;
        let mut line = "show".to_owned();
        if let Some(value) = token.wayland.as_deref().filter(|value| valid(value)) {
            line.push_str(" wayland=");
            line.push_str(value);
        }
        if let Some(value) = token.x11.as_deref().filter(|value| valid(value)) {
            line.push_str(" x11=");
            line.push_str(value);
        }
        line.push('\n');
        line
    }
}

/// 主窗口起来时占住控制通道，之后由后台线程把收到的请求转给窗口管理器。
pub(crate) fn listen() -> Result<Control> {
    listen_at(socket_path()?, |command| match command {
        Command::Show(token) => {
            super::window_manager::send(super::window_manager::Request::ActivateMain(token))
        }
    })
}

fn listen_at(
    path: PathBuf,
    handle: impl Fn(Command) -> Result<()> + Clone + Send + 'static,
) -> Result<Control> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).context("创建控制通道目录失败")?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .context("设置控制通道目录权限失败")?;
    }
    clear_stale(&path)?;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("建立控制通道失败：{}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .context("设置控制通道权限失败")?;
    std::thread::Builder::new()
        .name("openuuyc-control".into())
        .spawn(move || serve(&listener, handle))
        .context("启动控制通道线程失败")?;
    Ok(Control { path })
}

/// 上一份没来得及清理的 socket 文件会挡住重新绑定。Only a socket nobody
/// listens on (`ECONNREFUSED`) is stale; anything else at the path, or a
/// probe that fails another way, is left alone and reported.
fn clear_stale(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取控制通道 {} 失败", path.display()));
        }
    };
    if !metadata.file_type().is_socket() {
        bail!("控制通道位置 {} 被其他文件占用", path.display());
    }
    match UnixStream::connect(path) {
        Ok(_) => bail!("已经有实例占着控制通道 {}", path.display()),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => std::fs::remove_file(path)
            .with_context(|| format!("清理残留的控制通道 {} 失败", path.display())),
        // Removed between the two looks: nothing left to clear.
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("无法确认控制通道 {} 是否残留", path.display()))
        }
    }
}

fn serve(listener: &UnixListener, handle: impl Fn(Command) -> Result<()> + Clone + Send + 'static) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let handle = handle.clone();
        // Each connection on its own thread: one that stalls until its
        // timeout must not delay the next start's request.
        let spawned = std::thread::Builder::new()
            .name("openuuyc-control-request".into())
            .spawn(move || {
                if let Err(error) = answer(stream, handle) {
                    tracing::debug!(error = %format!("{error:#}"), "控制请求未完成");
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "无法处理控制请求");
        }
    }
}

fn answer(stream: UnixStream, handle: impl Fn(Command) -> Result<()>) -> Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut request = String::new();
    BufReader::new((&stream).take(MAX_LINE)).read_line(&mut request)?;
    let result = Command::parse(&request).and_then(handle);
    let reply = if result.is_ok() { "ok\n" } else { "error\n" };
    (&stream).write_all(reply.as_bytes())?;
    result
}

/// 第二个进程：把已经在跑的那个叫到前台，然后自己就该退出了。The launcher's
/// tokens go along so the running process can raise its window with them.
pub(crate) fn activate() -> Result<()> {
    activate_at(&socket_path()?, &Command::Show(LaunchToken::from_env()))
}

fn activate_at(path: &Path, command: &Command) -> Result<()> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("连接控制通道失败：{}", path.display()))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream
        .write_all(command.line().as_bytes())
        .context("发送显示请求失败")?;
    let mut reply = String::new();
    BufReader::new(stream.take(MAX_LINE))
        .read_line(&mut reply)
        .context("读取显示请求答复失败")?;
    anyhow::ensure!(reply.trim() == "ok", "运行中的实例没有接受显示请求");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    /// A socket path short enough for `sockaddr_un`, private to one test.
    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("openuuyc-control-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        directory.join("control.sock")
    }

    fn stale_socket(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: a socket bound to a path that fits sockaddr_un, then closed.
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0);
            let mut address: libc::sockaddr_un = std::mem::zeroed();
            address.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let bytes = path.as_os_str().as_bytes();
            assert!(bytes.len() < address.sun_path.len());
            for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
                *slot = *byte as libc::c_char;
            }
            let length = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            assert_eq!(
                libc::bind(fd, std::ptr::from_ref(&address).cast(), length),
                0
            );
            libc::close(fd);
        }
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn requests_round_trip_with_their_tokens() {
        let token = LaunchToken {
            wayland: Some("abc-123".into()),
            x11: Some("host-1-0_TIME42".into()),
        };
        let command = Command::Show(token.clone());
        assert_eq!(Command::parse(&command.line()).unwrap(), command);
        assert_eq!(
            Command::parse("show\n").unwrap(),
            Command::Show(LaunchToken::default())
        );
        // A malformed token is dropped, the request still shows the window.
        assert_eq!(
            Command::parse("show wayland=a\"b x11=ok future=1\n").unwrap(),
            Command::Show(LaunchToken {
                wayland: None,
                x11: Some("ok".into()),
            })
        );
        assert!(Command::parse("hide\n").is_err());
        assert!(Command::parse("").is_err());
    }

    #[test]
    fn a_stuck_client_holds_up_neither_the_listener_nor_a_later_start() {
        let path = scratch("stuck");
        let (sender, received) = mpsc::channel();
        let control = listen_at(path.clone(), move |command| {
            sender.send(command).unwrap();
            Ok(())
        })
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let directory = std::fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(directory.permissions().mode() & 0o777, 0o700);
        // Connects and never says anything.
        let _stuck = UnixStream::connect(&path).unwrap();
        let started = Instant::now();
        let token = LaunchToken {
            wayland: Some("w".into()),
            x11: None,
        };
        activate_at(&path, &Command::Show(token.clone())).unwrap();
        assert!(
            started.elapsed() < TIMEOUT,
            "the stuck client delayed the request"
        );
        assert_eq!(received.recv().unwrap(), Command::Show(token));
        drop(control);
        assert!(!path.exists());
        cleanup(&path);
    }

    #[test]
    fn a_running_instance_that_never_answers_does_not_block_a_start() {
        let path = scratch("silent");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Bound and listening, but nobody accepts or answers.
        let _silent = UnixListener::bind(&path).unwrap();
        let started = Instant::now();
        assert!(activate_at(&path, &Command::Show(LaunchToken::default())).is_err());
        let waited = started.elapsed();
        assert!(
            waited >= TIMEOUT && waited < TIMEOUT * 3,
            "waited {waited:?}"
        );
        cleanup(&path);
    }

    #[test]
    fn only_a_refused_socket_counts_as_stale() {
        let path = scratch("stale");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A previous run killed with its socket file left behind: bound, not
        // listening. (Binding a UnixListener and dropping it is not enough in
        // a test binary: a test forking a child meanwhile keeps the listening
        // socket alive until the child's exec.)
        stale_socket(&path);
        assert!(path.exists());
        clear_stale(&path).unwrap();
        assert!(!path.exists());
        // Someone is listening: not stale, kept.
        let live = UnixListener::bind(&path).unwrap();
        assert!(clear_stale(&path).is_err());
        assert!(path.exists());
        drop(live);
        std::fs::remove_file(&path).unwrap();
        // Not a socket at all: kept and reported.
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(clear_stale(&path).is_err());
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
        // Nothing there.
        clear_stale(&path).unwrap();
        cleanup(&path);
    }

    #[test]
    fn a_refused_request_is_reported_to_the_second_start() {
        let path = scratch("refused");
        let control = listen_at(path.clone(), |_| bail!("窗口管理器已关闭")).unwrap();
        assert!(activate_at(&path, &Command::Show(LaunchToken::default())).is_err());
        drop(control);
        cleanup(&path);
    }
}
