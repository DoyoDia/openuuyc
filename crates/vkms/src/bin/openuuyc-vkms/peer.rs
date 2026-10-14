//! 谁能用这个辅助程序。
//!
//! 判定只有一条：对端属于一个**正在使用中的本地图形会话**的用户。这正是 polkit
//! `allow_active` 在算的事情，只是我们自己算，好处是包里不用塞一份策略文件，也不
//! 依赖 polkitd 在跑。
//!
//! 为什么不校验对端的可执行文件是哪一个：本机上任何进程都能 exec 我们的程序，认
//! 了 exe 也挡不住「别人用我们的程序来戳套接字」。真正的边界就是「坐在机器前的
//! 那个人」，和 polkit 方案一模一样，多这一层只会让人以为边界更强。
use anyhow::{Context, Result, bail, ensure};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// 内核报给我们的对端身份，客户端伪造不了。
#[derive(Clone, Copy, Debug)]
pub struct Credentials {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

pub fn credentials(stream: &UnixStream) -> Result<Credentials> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: 长度与结构体一致，fd 是本进程持有的已连接套接字。
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut credentials).cast(),
            &mut length,
        )
    };
    ensure!(result == 0, "无法读取对端的身份");
    Ok(Credentials {
        pid: credentials.pid,
        uid: credentials.uid,
        gid: credentials.gid,
    })
}

/// 放行，或者说明为什么不行。原因要写清楚，它会原样转给主控端。
pub fn authorize(credentials: &Credentials) -> Result<()> {
    ensure!(
        credentials.pid > 0,
        "对端进程身份无效（pid {}）",
        credentials.pid
    );
    // root 不走这条路：它本来就能直接写 configfs，让它经过这里只会让审计日志里
    // 多一堆分不清来源的记录。
    ensure!(credentials.uid != 0, "辅助程序不接受 root 自己的请求");
    match graphical_session(credentials.uid)? {
        Some(session) => {
            crate::note(&format!(
                "放行 pid={} uid={} gid={} 会话={session}",
                credentials.pid, credentials.uid, credentials.gid
            ));
            Ok(())
        }
        None => bail!(
            "用户 {} 没有正在使用中的图形会话，不允许添加虚拟显示器",
            credentials.uid
        ),
    }
}

/// 这个用户有没有一个「人在用」的图形会话。有就返回会话号，用于日志。
fn graphical_session(uid: u32) -> Result<Option<String>> {
    use zbus::blocking::{Connection, Proxy};

    let connection = Connection::system().context("连接系统总线失败")?;
    let manager = Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .context("systemd-logind 不可用")?;
    // ListSessions 给的是 (会话号, uid, 用户名, 座位, 对象路径)。空座位的是远程
    // 登录，本机屏幕上没有它的画面，也就没有虚拟屏可挂。
    let sessions: Vec<(String, u32, String, String, zbus::zvariant::OwnedObjectPath)> =
        manager.call("ListSessions", &()).context("列出会话失败")?;
    for (id, session_uid, _, seat, path) in sessions {
        if session_uid != uid || seat.is_empty() {
            continue;
        }
        let session = Proxy::new(
            &connection,
            "org.freedesktop.login1",
            path.as_str(),
            "org.freedesktop.login1.Session",
        )
        .context("读取会话失败")?;
        let active = session
            .get_property::<bool>("Active")
            .context("读取会话的 Active 属性失败")?;
        // 图形会话才有合成器接管新显示器；纯 tty 上建出来没人认领。
        let kind = session
            .get_property::<String>("Type")
            .context("读取会话的 Type 属性失败")?;
        if active && matches!(kind.as_str(), "wayland" | "x11") {
            return Ok(Some(id));
        }
    }
    Ok(None)
}
