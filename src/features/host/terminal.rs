//! Host-side terminal (`CT_TERMINAL`, connection `kind = 8`).
//!
//! UU's Android client opens a shell on the controlled machine over the
//! low-priority BINARY data channel. The macOS server splits the work in two:
//! `TerminalHandler` parses frames off the channel and `TerminalSession` runs
//! the pty (see [`crate::platform::linux::terminal`]).
//!
//! ## Frame format
//!
//! A frame is `type, flags, sessionId, payload`. The type is the first byte
//! of the message: disassembling `TerminalHandler` shows the dispatcher doing
//! `and w8, w0, #0xff` and then comparing that byte against 1, 2, 3, 6, 8,
//! 10, 13 and 15, with `handleClose`, `handleListSessionsReq` and
//! `handleDetach` reached from specific values. The remaining field layout is
//! still unconfirmed, so this module does not guess at it: until the byte
//! layout is pinned down against a real client, frames are recorded and the
//! session answers nothing.

use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

/// Frame types, in the order the shipped binaries declare them.
///
/// The numeric values are the ones the dispatcher compares against, which the
/// `CT_*`-style declaration order confirms: `close` is 3, `listSessionsReq`
/// is 8 and `detach` is 15 all match their position here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum FrameType {
    Open = 0,
    Stdin = 1,
    Resize = 2,
    Close = 3,
    Stdout = 4,
    Exit = 5,
    Signal = 6,
    Error = 7,
    ListSessionsReq = 8,
    ListSessionsResp = 9,
    KillSessionReq = 10,
    KillSessionResp = 11,
    OpenResp = 12,
    UnlockReq = 13,
    UnlockResp = 14,
    Detach = 15,
}

impl FrameType {
    fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::Open,
            1 => Self::Stdin,
            2 => Self::Resize,
            3 => Self::Close,
            4 => Self::Stdout,
            5 => Self::Exit,
            6 => Self::Signal,
            7 => Self::Error,
            8 => Self::ListSessionsReq,
            9 => Self::ListSessionsResp,
            10 => Self::KillSessionReq,
            11 => Self::KillSessionResp,
            12 => Self::OpenResp,
            13 => Self::UnlockReq,
            14 => Self::UnlockResp,
            15 => Self::Detach,
            _ => return None,
        })
    }
}

/// Binds the terminal frame handler to `channel`.
///
/// Binds the terminal frame handler to `channel`.
///
/// The frame *type* is the one field the shipped binaries prove: the
/// dispatcher masks the first byte and compares it against the type values.
/// The remaining fields are still inference, so this records each frame
/// rather than acting on it — an unverified decoder would corrupt a session,
/// and the recording is what pins the layout down.
pub(crate) fn bind(
    channel: &Arc<RTCDataChannel>,
    sessions: Arc<std::sync::Mutex<Sessions>>,
    cancel: CancellationToken,
) {
    static FRAMES: AtomicU64 = AtomicU64::new(0);
    let stop = cancel.child_token();
    let close = stop.clone();
    channel.on_close(Box::new(move || {
        close.cancel();
        Box::pin(async {})
    }));
    let weak = Arc::downgrade(channel);
    channel.on_message(Box::new(move |message| {
        if stop.is_cancelled() {
            return Box::pin(async {});
        }
        let Some(channel) = weak.upgrade() else {
            return Box::pin(async {});
        };
        let _ = channel;
        let count = FRAMES.fetch_add(1, Ordering::Relaxed);
        // The type byte is the one part of the layout the dispatcher proves.
        // Log it with a bounded hex prefix so the remaining fields can be
        // read off a real capture without flooding the log.
        let kind = message.data.first().copied();
        let parsed = kind.and_then(FrameType::from_byte);
        let prefix: String = message
            .data
            .iter()
            .take(96)
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(
            frame = count,
            bytes = message.data.len(),
            type_byte = ?kind,
            ?parsed,
            open_sessions = sessions.lock().map_or(0, |s| s.list().len()),
            prefix,
            "host terminal frame received"
        );
        Box::pin(async {})
    }));
}

/// Sends one frame. Unused until the encoder is confirmed.
#[allow(dead_code)]
pub(crate) async fn send(
    channel: &RTCDataChannel,
    kind: FrameType,
    frame: &[u8],
) -> anyhow::Result<()> {
    let mut bytes = Vec::with_capacity(frame.len() + 1);
    bytes.push(kind as u8);
    bytes.extend_from_slice(frame);
    channel.send(&Bytes::from(bytes)).await?;
    Ok(())
}

/// Lifecycle state of one shell, mirroring the client's `TerminalSessionState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionState {
    Running,
    Detached,
    Exited,
}

/// One shell and the metadata the session list shows.
struct Session {
    pty: crate::platform::linux::terminal::Session,
    name: String,
    state: SessionState,
    cols: u16,
    rows: u16,
}

/// Owns every shell opened by the controller.
///
/// Names are unique, because the client lists sessions by name and offers a
/// rename action; a duplicate would make two rows indistinguishable.
#[derive(Default)]
pub(crate) struct Sessions {
    sessions: HashMap<String, Session>,
    next_id: u64,
}

/// Why a session operation was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum SessionError {
    #[error("终端会话不存在")]
    Gone,
    #[error("终端会话名无效")]
    InvalidName,
    #[error("终端会话名已存在")]
    DuplicateName,
    #[error("终端会话名过长")]
    NameTooLong,
    #[error("终端会话数已达上限")]
    TooMany,
}

/// The client rejects a name longer than this and shows its own length hint;
/// matching the limit keeps the two in agreement instead of failing at the
/// client after the server already accepted it.
const MAX_NAME: usize = 64;
/// A shell is a large grant; keep the count small enough to audit by hand.
const MAX_SESSIONS: usize = 16;

impl Sessions {
    /// Opens a shell and returns its session id.
    pub(crate) fn open(
        &mut self,
        shell: &str,
        name: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<String, SessionError> {
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(SessionError::TooMany);
        }
        let name = match name {
            Some(name) => validate_name(&name)?,
            None => self.default_name(),
        };
        if self.sessions.values().any(|session| session.name == name) {
            return Err(SessionError::DuplicateName);
        }
        let pty = crate::platform::linux::terminal::Session::open(shell, cols, rows)
            .map_err(|_| SessionError::Gone)?;
        self.next_id = self.next_id.wrapping_add(1);
        let id = format!("{}-{}", self.next_id, name);
        self.sessions.insert(
            id.clone(),
            Session {
                pty,
                name,
                state: SessionState::Running,
                cols,
                rows,
            },
        );
        Ok(id)
    }

    /// `shell-N`, skipping names already taken so a closed session's number
    /// is not immediately reused by a different shell.
    fn default_name(&self) -> String {
        for index in 1..=MAX_SESSIONS + 1 {
            let name = format!("shell-{index}");
            if !self.sessions.values().any(|s| s.name == name) {
                return name;
            }
        }
        format!("shell-{}", self.next_id.wrapping_add(1))
    }

    /// The id and display name of every session, for `listSessionsResp`.
    pub(crate) fn list(&self) -> Vec<(String, String, SessionState)> {
        let mut listed: Vec<_> = self
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.name.clone(), session.state))
            .collect();
        listed.sort_by(|a, b| a.1.cmp(&b.1));
        listed
    }

    /// Writes keyboard input to one session.
    pub(crate) fn write(&self, id: &str, data: &[u8]) -> Result<usize, SessionError> {
        let session = self.sessions.get(id).ok_or(SessionError::Gone)?;
        session.pty.write(data).map_err(|_| SessionError::Gone)
    }

    /// Applies a new size, which the shell sees as `SIGWINCH`.
    pub(crate) fn resize(&mut self, id: &str, cols: u16, rows: u16) -> Result<(), SessionError> {
        let session = self.sessions.get_mut(id).ok_or(SessionError::Gone)?;
        session.pty.resize(cols, rows).map_err(|_| SessionError::Gone)?;
        session.cols = cols;
        session.rows = rows;
        Ok(())
    }

    /// Renames a session, which is what the client's rename action does.
    pub(crate) fn rename(&mut self, id: &str, name: &str) -> Result<(), SessionError> {
        let name = validate_name(name)?;
        if self
            .sessions
            .iter()
            .any(|(other, session)| other != id && session.name == name)
        {
            return Err(SessionError::DuplicateName);
        }
        let session = self.sessions.get_mut(id).ok_or(SessionError::Gone)?;
        session.name = name;
        Ok(())
    }

    /// Ends a session and removes it from the list.
    pub(crate) fn kill(&mut self, id: &str) -> Result<(), SessionError> {
        self.sessions.remove(id).ok_or(SessionError::Gone)?;
        Ok(())
    }

    /// Drops sessions whose shell has already exited.
    pub(crate) fn reap(&mut self) -> Vec<String> {
        let exited: Vec<String> = self
            .sessions
            .iter_mut()
            .filter_map(|(id, session)| session.pty.exited().then(|| id.clone()))
            .collect();
        for id in &exited {
            self.sessions.remove(id);
        }
        exited
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// Rejects the names the client's own validation rejects, so a rename that
/// the server accepts is never refused at the client afterwards.
fn validate_name(name: &str) -> Result<String, SessionError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(SessionError::InvalidName);
    }
    if trimmed.chars().count() > MAX_NAME {
        return Err(SessionError::NameTooLong);
    }
    // The client reports `errorInvalidChar`; keep names to what it can render
    // and what survives a round trip through the frame payload.
    if trimmed
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(SessionError::InvalidName);
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> &'static str {
        "/bin/sh"
    }

    #[test]
    fn sessions_get_distinct_default_names() {
        let mut sessions = Sessions::default();
        let first = sessions.open(shell(), None, 80, 24).expect("open");
        let second = sessions.open(shell(), None, 80, 24).expect("open");
        let names: Vec<String> = sessions.list().into_iter().map(|(_, name, _)| name).collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert_ne!(first, second);
    }

    #[test]
    fn a_duplicate_name_is_refused() {
        let mut sessions = Sessions::default();
        sessions
            .open(shell(), Some("build".into()), 80, 24)
            .expect("open");
        assert_eq!(
            sessions.open(shell(), Some("build".into()), 80, 24),
            Err(SessionError::DuplicateName)
        );
    }

    #[test]
    fn invalid_names_are_refused() {
        let mut sessions = Sessions::default();
        assert_eq!(
            sessions.open(shell(), Some("   ".into()), 80, 24),
            Err(SessionError::InvalidName)
        );
        assert_eq!(
            sessions.open(shell(), Some("a/b".into()), 80, 24),
            Err(SessionError::InvalidName)
        );
        assert_eq!(
            sessions.open(shell(), Some("x".repeat(MAX_NAME + 1)), 80, 24),
            Err(SessionError::NameTooLong)
        );
    }

    #[test]
    fn renaming_onto_another_session_is_refused() {
        let mut sessions = Sessions::default();
        let first = sessions
            .open(shell(), Some("one".into()), 80, 24)
            .expect("open");
        sessions
            .open(shell(), Some("two".into()), 80, 24)
            .expect("open");
        assert_eq!(
            sessions.rename(&first, "two"),
            Err(SessionError::DuplicateName)
        );
        // Renaming a session to its own name is not a conflict.
        assert_eq!(sessions.rename(&first, "one"), Ok(()));
    }

    #[test]
    fn write_and_resize_reach_the_shell() {
        let mut sessions = Sessions::default();
        let id = sessions.open(shell(), None, 80, 24).expect("open");
        sessions.resize(&id, 120, 40).expect("resize");
        assert!(sessions.write(&id, b"echo READY\n").expect("write") > 0);
        // Let the shell run, then read back through the pty to prove the
        // bytes arrived rather than just being accepted by the buffer.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let session = sessions.sessions.get(&id).expect("session");
        let mut buffer = [0u8; 4096];
        let mut seen = Vec::new();
        for _ in 0..50 {
            match session.pty.read(&mut buffer) {
                Ok(Some(0)) => break,
                Ok(Some(read)) => {
                    seen.extend_from_slice(&buffer[..read]);
                    if String::from_utf8_lossy(&seen).contains("READY") {
                        break;
                    }
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(error) => panic!("read: {error}"),
            }
        }
        let text = String::from_utf8_lossy(&seen);
        assert!(text.contains("READY"), "shell produced: {text:?}");
    }

    #[test]
    fn killing_removes_a_session_and_a_second_kill_is_gone() {
        let mut sessions = Sessions::default();
        let id = sessions.open(shell(), None, 80, 24).expect("open");
        assert_eq!(sessions.kill(&id), Ok(()));
        assert_eq!(sessions.kill(&id), Err(SessionError::Gone));
        assert!(sessions.is_empty());
    }

    #[test]
    fn an_exited_shell_is_reaped() {
        let mut sessions = Sessions::default();
        let id = sessions.open(shell(), None, 80, 24).expect("open");
        sessions.write(&id, b"exit\n").expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !sessions.reap().is_empty() {
                assert!(sessions.is_empty());
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("shell did not exit");
    }
}
