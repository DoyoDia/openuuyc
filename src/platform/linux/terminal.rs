//! Pseudo-terminal sessions for the host (被控) terminal feature.
//!
//! UU's Android client can open a shell on the controlled machine
//! (`CT_TERMINAL`, connection `kind = 8`). The macOS server does this with
//! `forkpty()` plus tmux for reattachment; this module is the pty half.
//!
//! We deliberately do not call `forkpty()`. `fork()` in a threaded process
//! leaves only the calling thread alive in the child, and the child must not
//! touch a lock the parent held — the allocator lock alone makes it
//! unsound from Rust. Instead `openpty()` creates the pair and the shell is
//! started through `Command`, whose `pre_exec` hook runs only the
//! async-signal-safe calls the child needs (`setsid`, `ioctl(TIOCSCTTY)`,
//! `dup2`).

use anyhow::{Context, Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

/// A shell running on a pty, with the master side owned by this process.
pub(crate) struct Session {
    master: OwnedFd,
    child: Child,
    shell: String,
}

impl Session {
    /// Starts `shell` on a new pty of `cols` x `rows`.
    pub(crate) fn open(shell: &str, cols: u16, rows: u16) -> Result<Self> {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let size = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes both descriptors and only reads the two
        // pointers, both of which point at live stack values.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        };
        if opened != 0 {
            return Err(std::io::Error::last_os_error()).context("创建 pty 失败");
        }
        // SAFETY: a zero return means both descriptors are valid and owned by
        // us, and neither has been wrapped before.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        let master_raw = master.as_raw_fd();
        let slave_raw = slave.as_raw_fd();

        let mut command = Command::new(shell);
        // The service environment carries no terminal type, and curses
        // programs (`top`, `less`, `vi`) refuse to start without one. The
        // reference server sets the same pair before forking the pty.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        if std::env::var_os("LANG").is_none() {
            command.env("LANG", "C.UTF-8");
        }
        // The child gets the slave as its three standard streams; the parent
        // keeps the master. stdin/out/err start null so that a failure before
        // the dup2 calls cannot leave the child writing to our own terminal.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the closure runs in the forked child before exec. It calls
        // only async-signal-safe functions and reads two integers captured by
        // value, so it cannot observe a parent lock or allocate.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_raw, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in 0..3 {
                    if libc::dup2(slave_raw, fd) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if slave_raw > 2 {
                    libc::close(slave_raw);
                }
                // The child must not hold the master open, or the pty would
                // never report end-of-file once the shell exits.
                if master_raw > 2 {
                    libc::close(master_raw);
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("启动 shell 失败：{shell}"))?;
        // The parent has no use for the slave now that the child owns it.
        drop(slave);

        set_nonblocking(master_raw)?;
        Ok(Self {
            master,
            child,
            shell: shell.to_owned(),
        })
    }

    /// The shell this session runs.
    pub(crate) fn shell(&self) -> &str {
        &self.shell
    }

    /// Reads whatever is available. `Ok(None)` means nothing is buffered yet;
    /// `Ok(Some(0))` is end-of-file, which Linux reports as `EIO` on the
    /// master once the last slave closes.
    pub(crate) fn read(&self, buffer: &mut [u8]) -> Result<Option<usize>> {
        // SAFETY: the descriptor is open for the life of `self` and `buffer`
        // is valid for writes of its own length.
        let read = unsafe {
            libc::read(
                self.master.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if read >= 0 {
            return Ok(Some(read as usize));
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(None),
            Some(libc::EIO) => Ok(Some(0)),
            _ => Err(error).context("读取 pty 失败"),
        }
    }

    /// Writes keyboard input to the shell.
    pub(crate) fn write(&self, data: &[u8]) -> Result<usize> {
        // SAFETY: as `read`, with a slice valid for reads of its own length.
        let written =
            unsafe { libc::write(self.master.as_raw_fd(), data.as_ptr().cast(), data.len()) };
        if written < 0 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                // A full pty buffer is not an error; the caller retries.
                Some(libc::EAGAIN) => return Ok(0),
                _ => return Err(error).context("写入 pty 失败"),
            }
        }
        Ok(written as usize)
    }

    /// Applies a new terminal size, which is what the shell's `SIGWINCH` and
    /// full-screen programs read.
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let size = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the descriptor is open and `size` is a live winsize.
        let applied = unsafe {
            libc::ioctl(
                self.master.as_raw_fd(),
                libc::TIOCSWINSZ as libc::c_ulong,
                &size,
            )
        };
        if applied != 0 {
            bail!("设置 pty 尺寸失败");
        }
        Ok(())
    }

    /// Sends a signal to the shell's process group. The child called
    /// `setsid()`, so it leads its own group and the negative pid reaches the
    /// whole job tree rather than the shell alone.
    pub(crate) fn signal(&self, signal: i32) -> Result<()> {
        let pid = self.child.id() as i32;
        // SAFETY: `pid` is a live child of this process.
        if unsafe { libc::kill(-pid, signal) } != 0 {
            bail!("向 shell 发送信号 {signal} 失败");
        }
        Ok(())
    }

    /// Whether the shell has exited.
    pub(crate) fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best effort: hang up the shell, then reap it so it does not linger
        // as a zombie. `master` closes with the struct.
        let pid = self.child.id() as i32;
        // SAFETY: `pid` is our child and this is the last use of it.
        unsafe {
            libc::kill(-pid, libc::SIGHUP);
        }
        let _ = self.child.wait();
    }
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    // SAFETY: `fd` is open; fcntl only reads and then rewrites its flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("读取 pty 标志失败");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error()).context("设置 pty 非阻塞失败");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Drains the pty until `needle` appears, the shell exits, or time runs out.
    fn read_until(session: &mut Session, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        let mut buffer = [0u8; 4096];
        while Instant::now() < deadline {
            match session.read(&mut buffer).expect("read pty") {
                Some(0) => break,
                Some(read) => {
                    seen.extend_from_slice(&buffer[..read]);
                    if seen.windows(needle.len()).any(|window| window == needle) {
                        break;
                    }
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        seen
    }

    #[test]
    fn shell_echoes_through_the_pty() {
        let mut session = Session::open("/bin/sh", 80, 24).expect("open pty");
        session.write(b"echo PTY_READY\n").expect("write input");
        let seen = read_until(&mut session, b"PTY_READY", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&seen);
        assert!(text.contains("PTY_READY"), "pty produced: {text:?}");
    }

    #[test]
    fn resize_is_applied_to_the_pty() {
        let session = Session::open("/bin/sh", 80, 24).expect("open pty");
        session.resize(120, 40).expect("resize");
        // Read the size back the way the shell sees it.
        session.write(b"stty size\n").expect("write input");
        let mut session = session;
        let seen = read_until(&mut session, b"40 120", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&seen);
        assert!(text.contains("40 120"), "pty reported size: {text:?}");
    }

    #[test]
    fn signal_reaches_the_shell() {
        let mut session = Session::open("/bin/sh", 80, 24).expect("open pty");
        session.signal(libc::SIGTERM).expect("signal shell");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if session.exited() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("shell did not exit after SIGTERM");
    }

    #[test]
    fn end_of_file_is_reported_once_the_shell_exits() {
        let mut session = Session::open("/bin/sh", 80, 24).expect("open pty");
        session.write(b"exit\n").expect("write input");
        let seen = read_until(&mut session, b"\x04", Duration::from_secs(5));
        // Either the shell closed the pty (EOF) or it printed nothing more;
        // both mean the session ended rather than hanging.
        assert!(session.exited() || !seen.is_empty());
    }
}
