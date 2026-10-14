//! SIGTERM/SIGINT 走和托盘「退出」同一条收尾路径。
//!
//! `systemctl --user stop openuuyc.service` 发的是 SIGTERM，直接死掉的话本机在线
//! 状态要等服务器超时才掉，正在进行的被控会话也不会走正常收尾；`Restart=on-failure`
//! 的单元更该看到一次干净的退出。winit 不装信号处理，事件循环也没有注册文件描述符
//! 的接口，所以按老办法：处理函数往自建管道里写一个字节（异步信号安全），一个后台
//! 线程读到就通过窗口管理器请求退出。
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicI32, Ordering};

/// 自建管道的写端。信号处理函数只能读这个原子量。
static WAKE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_signal: libc::c_int) {
    let fd = WAKE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: 只对常量地址做一次 write，异步信号安全。
        unsafe {
            libc::write(fd, std::ptr::from_ref(&byte).cast(), 1);
        }
    }
}

/// 让 SIGTERM、SIGINT 变成一次正常退出。进程里只会装一次。
pub(crate) fn install() -> Result<()> {
    anyhow::ensure!(
        WAKE_WRITE.load(Ordering::Relaxed) < 0,
        "退出信号处理已经装过了"
    );
    let mut fds = [0i32; 2];
    // SAFETY: 传的是两个 i32 的有效地址，失败会返回负值。
    anyhow::ensure!(
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0,
        "创建退出信号管道失败：{}",
        std::io::Error::last_os_error()
    );
    let [read, write] = fds;
    WAKE_WRITE.store(write, Ordering::Relaxed);
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: 处理函数不做任何不安全的事，安装失败时返回值是 SIG_ERR。
        let previous =
            unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
        anyhow::ensure!(previous != libc::SIG_ERR, "安装信号处理失败");
    }
    std::thread::Builder::new()
        .name("openuuyc-signal".into())
        .spawn(move || {
            let mut byte = 0u8;
            // SAFETY: 阻塞读一个字节，直到管道被关闭。
            while unsafe { libc::read(read, std::ptr::from_mut(&mut byte).cast(), 1) } == 1 {
                if super::window_manager::send(super::window_manager::Request::Exit).is_err() {
                    break;
                }
            }
        })
        .context("启动退出信号线程失败")?;
    Ok(())
}
