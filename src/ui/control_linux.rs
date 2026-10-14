//! 让第二个进程把已经跑着的那个叫到前台。
//!
//! 服务或登录自启会把控制中心以不显示主窗口的方式跑起来，用户再点一次图标时
//! 新进程必须先退出（实例锁在运行的进程手里），但退出之前要把窗口叫出来，否则
//! 点图标什么都不会发生。Windows 用注册窗口消息做这件事（见
//! [`crate::application::app::instance`]），Wayland 与 X11 都没有对应的窗口属性，
//! 这里用 `$XDG_RUNTIME_DIR` 下的一个 unix socket 代替。
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

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

/// 主窗口起来时占住控制通道，之后由后台线程把收到的请求转给窗口管理器。
pub(crate) fn listen() -> Result<Control> {
    let path = socket_path()?;
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).context("创建控制通道目录失败")?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .context("设置控制通道目录权限失败")?;
    }
    // 上一份没来得及清理的 socket 文件会挡住重新绑定；先确认它后面没有人。
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!("已经有实例占着控制通道 {}", path.display());
        }
        std::fs::remove_file(&path).context("清理残留的控制通道失败")?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("建立控制通道失败：{}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .context("设置控制通道权限失败")?;
    std::thread::Builder::new()
        .name("openuuyc-control".into())
        .spawn(move || serve(&listener))
        .context("启动控制通道线程失败")?;
    Ok(Control { path })
}

fn serve(listener: &UnixListener) {
    // 一个一个来：这是本机单用户的通道，不需要并发，也就不会被连接数拖住。
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let mut request = String::new();
        if BufReader::new(&stream).read_line(&mut request).is_err() {
            continue;
        }
        let result = match request.trim() {
            "show" => super::window_manager::send(super::window_manager::Request::ShowMain),
            other => Err(anyhow::anyhow!("未知的控制请求 {other}")),
        };
        let reply = if result.is_ok() { "ok\n" } else { "error\n" };
        let _ = stream.write_all(reply.as_bytes());
    }
}

/// 第二个进程：把已经在跑的那个叫到前台，然后自己就该退出了。
pub(crate) fn activate() -> Result<()> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("连接控制通道失败：{}", path.display()))?;
    stream.write_all(b"show\n").context("发送显示请求失败")?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .context("读取显示请求答复失败")?;
    anyhow::ensure!(reply.trim() == "ok", "运行中的实例没有接受显示请求");
    Ok(())
}
