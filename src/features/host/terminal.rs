//! Host-side terminal (`CT_TERMINAL`, connection `kind = 8`).
//!
//! UU's Android client opens a shell on the controlled machine. The macOS
//! server splits the work in two: `TerminalHandler` parses requests off the
//! wire and `TerminalSession` runs the pty (see
//! [`crate::platform::terminal`]).
//!
//! ## Two protocols on one channel
//!
//! The client mixes two message shapes on `BINARY_DATA_CHANNEL`:
//!
//! * `terminal_check_env_req` and the rename RPC are ordinary protobuf
//!   `PbControlMessage` envelopes, handled by the shared session handler.
//! * Everything else is a private `TERM` frame.
//!
//! The client itself makes the same split: its coordinator logs
//! `TERM 帧解析失败` and `非 TERM 数据解析 GvPb_Message 失败` on the two paths.
//!
//! ## `TERM` frame layout
//!
//! Reconstructed from the shipped encoder (`UURemoteServer.arm64`
//! `0x1001a5888`) and confirmed against a capture:
//!
//! ```text
//! offset  size  field
//!      0     4  "TERM"
//!      4     1  protocol version (1)
//!      5     1  frame type, declaration index + 1
//!      6     2  zero
//!      8     4  session handle, little endian (0 for list requests)
//!     12     4  payload length, little endian
//!     16     n  payload
//! ```
//!
//! The captured `terminal_check_env_req` frame from the Android client was
//! `54 45 52 4d 01 09 ...`: version 1, type 9 = `listSessionsReq`, empty
//! handle and payload, which matches the session list the phone then shows.
//!
//! The handle is chosen by the controller and echoed by the host, so it is the
//! only thing that identifies a session on the wire.

use bytes::Bytes;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

/// The four bytes every terminal frame starts with.
const MAGIC: &[u8; 4] = b"TERM";
/// Fixed header size, magic included.
const HEADER: usize = 16;
/// Only version 1 has ever been seen; refuse anything else rather than guess.
const VERSION: u8 = 1;
/// Stop a shell from running forever unnoticed.
const MAX_SESSIONS: usize = 16;
/// The client rejects a name longer than this and shows its own length hint;
/// matching the limit keeps the two in agreement instead of failing at the
/// client after the server already accepted it.
const MAX_NAME: usize = 64;
/// Output kept per shell so a reattach can replay the screen the controller
/// last saw. The client keeps its own scrollback, so a screenful and a little
/// history is enough.
const SCROLLBACK_LIMIT: usize = 128 * 1024;

/// Frame kinds, in the declaration order the shipped enum uses.
///
/// The wire value is `index + 1`, so `open` is 1 and `detach` is 16. The
/// encoder adds one; both binaries agree on the resulting numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameType {
    Open = 1,
    Stdin = 2,
    Resize = 3,
    Close = 4,
    Stdout = 5,
    Exit = 6,
    Signal = 7,
    Error = 8,
    ListSessionsReq = 9,
    ListSessionsResp = 10,
    KillSessionReq = 11,
    KillSessionResp = 12,
    OpenResp = 13,
    UnlockReq = 14,
    UnlockResp = 15,
    Detach = 16,
}

impl FrameType {
    fn from_wire(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::Open,
            2 => Self::Stdin,
            3 => Self::Resize,
            4 => Self::Close,
            5 => Self::Stdout,
            6 => Self::Exit,
            7 => Self::Signal,
            8 => Self::Error,
            9 => Self::ListSessionsReq,
            10 => Self::ListSessionsResp,
            11 => Self::KillSessionReq,
            12 => Self::KillSessionResp,
            13 => Self::OpenResp,
            14 => Self::UnlockReq,
            15 => Self::UnlockResp,
            16 => Self::Detach,
            _ => return None,
        })
    }

    fn wire(self) -> u8 {
        self as u8
    }
}

/// One decoded `TERM` frame.
pub(crate) struct Frame {
    pub(crate) kind: FrameType,
    pub(crate) session: u32,
    pub(crate) payload: Vec<u8>,
}

impl Frame {
    /// Decodes a frame, or returns `None` for protobuf or malformed data.
    ///
    /// A frame is never abandoned halfway: the declared payload length has to
    /// fit, otherwise a truncated read would be mistaken for a short payload.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER || &bytes[..4] != MAGIC || bytes[4] != VERSION {
            return None;
        }
        let kind = FrameType::from_wire(bytes[5])?;
        let session = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let length = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
        let payload = bytes.get(HEADER..HEADER.checked_add(length)?)?.to_vec();
        Some(Self {
            kind,
            session,
            payload,
        })
    }

    /// Builds a frame the controller will accept.
    fn encode(kind: FrameType, session: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + payload.len());
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(kind.wire());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&session.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }
}

/// Lifecycle state of one shell, mirroring the client's `TerminalSessionState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionState {
    Running,
    Detached,
    Exited,
}

impl SessionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Detached => "detached",
            Self::Exited => "exited",
        }
    }
}

/// One shell and the metadata the session list shows.
struct Session {
    pty: crate::platform::terminal::Session,
    name: String,
    state: SessionState,
    created_ms: i64,
    last_active_ms: i64,
    /// Output the shell has produced, so a reattach can replay the screen the
    /// controller last saw instead of showing it blank.
    scrollback: Vec<u8>,
}

/// Sessions taken out of the table.
///
/// Dropping a session ends its shell, which can block for the pty's hangup
/// grace period while a shell that ignores `SIGHUP` is killed. The table
/// therefore hands ended sessions back instead of dropping them under its
/// lock, and [`Ended::finish`] drops them once the caller has let go.
#[must_use = "dropping ended sessions can block; call `finish` after releasing the table"]
pub(crate) struct Ended(Vec<Session>);

impl Ended {
    /// Ends the shells on the blocking pool when called from the runtime, so
    /// no async worker waits for a shell to die.
    pub(crate) fn finish(self) {
        if self.0.is_empty() {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || drop(self));
            }
            Err(_) => drop(self),
        }
    }
}

/// Owns every shell opened by the controller, keyed by the controller's handle.
///
/// The controller picks the handle, so the host only has to remember which
/// shell it points at. Names are unique because the client lists sessions by
/// name and offers a rename action; a duplicate would make two rows
/// indistinguishable.
#[derive(Default)]
pub(crate) struct Sessions {
    sessions: HashMap<u32, Session>,
    next_name: u64,
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

impl Session {
    /// Records output for a later reattach, dropping the oldest bytes once the
    /// limit is reached so a chatty shell cannot grow without bound.
    fn push_scrollback(&mut self, data: &[u8]) {
        self.scrollback.extend_from_slice(data);
        if self.scrollback.len() > SCROLLBACK_LIMIT {
            let excess = self.scrollback.len() - SCROLLBACK_LIMIT;
            self.scrollback.drain(..excess);
        }
    }
}

impl Sessions {
    /// Opens a shell and returns its handle, name, and any output to replay.
    ///
    /// The controller normally picks the handle, but its `open` frame may
    /// carry zero when it wants the host to allocate one; the returned handle
    /// is what later `stdin`/`resize` frames must use. Re-opening an existing
    /// handle re-uses the shell and returns its scrollback, which is what the
    /// controller redraws on reattach.
    pub(crate) fn open(
        &mut self,
        handle: u32,
        shell: &str,
        name: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<(u32, String, Vec<u8>), SessionError> {
        let handle = if handle == 0 {
            self.free_handle()
        } else {
            handle
        };
        // Re-opening an existing handle happens on reconnect and when the
        // size changes; replacing the pty would hang up a shell in use.
        if let Some(existing) = self.sessions.get_mut(&handle) {
            existing
                .pty
                .resize(cols, rows)
                .map_err(|_| SessionError::Gone)?;
            existing.state = SessionState::Running;
            existing.last_active_ms = now_ms();
            return Ok((handle, existing.name.clone(), existing.scrollback.clone()));
        }
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(SessionError::TooMany);
        }
        let name = match name {
            Some(name) => validate_name(&name)?,
            None => self.default_name(),
        };
        if self
            .sessions
            .iter()
            .any(|(other, session)| *other != handle && session.name == name)
        {
            return Err(SessionError::DuplicateName);
        }
        let pty = crate::platform::terminal::Session::open(shell, cols, rows)
            .map_err(|_| SessionError::Gone)?;
        let stamp = now_ms();
        self.sessions.insert(
            handle,
            Session {
                pty,
                name: name.clone(),
                state: SessionState::Running,
                created_ms: stamp,
                last_active_ms: stamp,
                scrollback: Vec::new(),
            },
        );
        Ok((handle, name, Vec::new()))
    }

    /// The lowest handle no live session uses; the controller's handle space
    /// starts at 1, leaving 0 free to mean "assign one".
    fn free_handle(&self) -> u32 {
        (1..u32::MAX)
            .find(|handle| !self.sessions.contains_key(handle))
            .unwrap_or(1)
    }

    /// `shell-N`, skipping names already taken so a closed session's number
    /// is not immediately reused by a different shell.
    fn default_name(&mut self) -> String {
        for _ in 0..=MAX_SESSIONS {
            self.next_name = self.next_name.wrapping_add(1);
            let name = format!("shell-{}", self.next_name);
            if !self.sessions.values().any(|s| s.name == name) {
                return name;
            }
        }
        format!("shell-{}", self.next_name)
    }

    /// The default shell, matching what a terminal emulator would start.
    pub(crate) fn default_shell() -> String {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty() && std::path::Path::new(shell).exists())
            .unwrap_or_else(|| "/bin/bash".to_owned())
    }

    /// The sessions the client lists, as the objects it parses.
    pub(crate) fn list(&self) -> Vec<Value> {
        let mut listed: Vec<_> = self
            .sessions
            .iter()
            .map(|(handle, session)| {
                json!({
                    "session_id": handle,
                    "state": session.state.as_str(),
                    "name": session.name,
                    "created_at_ms": session.created_ms,
                    "last_active_ms": session.last_active_ms,
                    "shell": session.pty.shell(),
                })
            })
            .collect();
        listed.sort_by_key(Value::to_string);
        listed
    }

    /// Writes keyboard input to one session.
    pub(crate) fn write(&mut self, handle: u32, data: &[u8]) -> Result<usize, SessionError> {
        let session = self.sessions.get_mut(&handle).ok_or(SessionError::Gone)?;
        session.last_active_ms = now_ms();
        session.pty.write(data).map_err(|_| SessionError::Gone)
    }

    /// Applies a new size, which the shell sees as `SIGWINCH`.
    pub(crate) fn resize(&mut self, handle: u32, cols: u16, rows: u16) -> Result<(), SessionError> {
        let session = self.sessions.get_mut(&handle).ok_or(SessionError::Gone)?;
        session
            .pty
            .resize(cols, rows)
            .map_err(|_| SessionError::Gone)?;
        session.last_active_ms = now_ms();
        Ok(())
    }

    /// Renames a session by its handle.
    pub(crate) fn rename(&mut self, handle: u32, name: &str) -> Result<(), SessionError> {
        let name = validate_name(name)?;
        if self
            .sessions
            .iter()
            .any(|(other, session)| *other != handle && session.name == name)
        {
            return Err(SessionError::DuplicateName);
        }
        let session = self.sessions.get_mut(&handle).ok_or(SessionError::Gone)?;
        session.name = name;
        Ok(())
    }

    /// Removes a session from the list; finishing the result ends the shell.
    pub(crate) fn kill(&mut self, handle: u32) -> Result<Ended, SessionError> {
        let session = self.sessions.remove(&handle).ok_or(SessionError::Gone)?;
        Ok(Ended(vec![session]))
    }

    /// Marks a session detached without ending the shell.
    pub(crate) fn detach(&mut self, handle: u32) -> Result<(), SessionError> {
        let session = self.sessions.get_mut(&handle).ok_or(SessionError::Gone)?;
        session.state = SessionState::Detached;
        session.last_active_ms = now_ms();
        Ok(())
    }

    /// Takes whatever the shells have written since the last poll.
    ///
    /// Each shell's exit is reported once, so the controller learns a session
    /// ended without polling the list.
    pub(crate) fn pump(&mut self) -> Vec<(u32, FrameType, Vec<u8>)> {
        let mut out = Vec::new();
        let mut exited = Vec::new();
        let mut buffer = [0u8; 8192];
        for (handle, session) in self.sessions.iter_mut() {
            if session.state == SessionState::Exited {
                continue;
            }
            loop {
                match session.pty.read(&mut buffer) {
                    Ok(Some(0)) | Err(_) => break,
                    Ok(Some(read)) => {
                        session.push_scrollback(&buffer[..read]);
                        out.push((*handle, FrameType::Stdout, buffer[..read].to_vec()));
                    }
                    Ok(None) => break,
                }
            }
            session.last_active_ms = now_ms();
            if session.pty.exited() {
                session.state = SessionState::Exited;
                exited.push((*handle, FrameType::Exit, b"{}".to_vec()));
            }
        }
        out.extend(exited);
        out
    }

    /// Takes out the sessions that have already reported their exit.
    pub(crate) fn reap(&mut self) -> Ended {
        let exited: Vec<u32> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.state == SessionState::Exited)
            .map(|(handle, _)| *handle)
            .collect();
        Ended(
            exited
                .iter()
                .filter_map(|handle| self.sessions.remove(handle))
                .collect(),
        )
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// Maps the controller's shell choice to a real program.
///
/// The `open` payload names a shell *kind* (`powershell`, `cmd`, `bash`), not
/// a path. The Windows names have no Linux equivalent, so they fall back to
/// the user's login shell rather than failing the request; the client shows
/// its generic creation error when `open` is refused.
fn resolve_shell(requested: &str) -> String {
    let candidate = match requested.trim().to_ascii_lowercase().as_str() {
        "bash" => Some("/bin/bash"),
        "zsh" => Some("/bin/zsh"),
        "sh" => Some("/bin/sh"),
        "fish" => Some("/usr/bin/fish"),
        "pwsh" | "powershell" => Some("/usr/bin/pwsh"),
        // `cmd` and friends are Windows-only; a POSIX shell is the closest
        // thing that can actually run.
        "cmd" | "cmd.exe" | "powershell.exe" => Some("/bin/sh"),
        _ => None,
    };
    match candidate {
        Some(path) if std::path::Path::new(path).exists() => path.to_owned(),
        _ => Sessions::default_shell(),
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
    // and what survives a round trip through the request payload.
    if trimmed
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(SessionError::InvalidName);
    }
    Ok(trimmed.to_owned())
}

/// The process-wide shell table.
///
/// Terminals outlive a single connection: the controller reconnects
/// routinely (screen rotation, app switching) and expects the same sessions
/// to still be there. Keeping the table per `Screens` would drop every shell
/// when a connection closed, so one table serves the whole process, the way
/// the reference server keeps its sessions in tmux.
pub(crate) fn shared() -> Arc<std::sync::Mutex<Sessions>> {
    static SESSIONS: std::sync::OnceLock<Arc<std::sync::Mutex<Sessions>>> =
        std::sync::OnceLock::new();
    Arc::clone(SESSIONS.get_or_init(|| Arc::new(std::sync::Mutex::new(Sessions::default()))))
}

/// Binds the terminal handler to `channel`.
///
/// Protobuf requests (the environment check, rename) go through the shared
/// session handler. `TERM` frames are handled here. Responses return on the
/// channel the request arrived on.
pub(crate) fn bind(
    channel: &Arc<RTCDataChannel>,
    screens: Arc<tokio::sync::Mutex<crate::features::host::peer::screens::Screens>>,
    cancel: CancellationToken,
) {
    let stop = cancel.child_token();
    let close = stop.clone();
    channel.on_close(Box::new(move || {
        close.cancel();
        Box::pin(async {})
    }));
    // The message closure consumes the originals, so keep a handle for the
    // output pump first.
    let pump_screens = screens.clone();
    let pump_stop = stop.clone();
    let weak = Arc::downgrade(channel);
    channel.on_message(Box::new(move |message| {
        let weak = weak.clone();
        let screens = screens.clone();
        let stop = stop.clone();
        Box::pin(async move {
            if stop.is_cancelled() {
                return;
            }
            let Some(channel) = weak.upgrade() else {
                return;
            };
            let data = message.data.clone();
            if data.starts_with(MAGIC) {
                handle_frame(&channel, &screens, &data, &stop).await;
                return;
            }
            let responses = {
                let mut state = screens.lock().await;
                crate::features::stream_control::publisher::receive_session(
                    &mut state, &data, false,
                )
                .await
            };
            match responses {
                Ok(responses) => {
                    for response in responses.messages {
                        if stop.is_cancelled() {
                            break;
                        }
                        if let Err(error) = channel.send(&Bytes::from(response)).await {
                            tracing::warn!(%error, "host terminal response failed");
                            break;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        bytes = data.len(),
                        "rejected host terminal message"
                    );
                }
            }
        })
    }));

    // Pump shell output. The read side is non-blocking, so poll on a short
    // interval; a shell that prints nothing costs one lock per tick.
    let pump_channel = Arc::downgrade(channel);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
        loop {
            ticker.tick().await;
            if pump_stop.is_cancelled() {
                return;
            }
            let Some(channel) = pump_channel.upgrade() else {
                return;
            };
            let frames = {
                let state = pump_screens.lock().await;
                let sessions = state.terminal();
                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                sessions.pump()
            };
            let mut failed = false;
            for (handle, kind, payload) in frames {
                let frame = Frame::encode(kind, handle, &payload);
                if let Err(error) = channel.send(&Bytes::from(frame)).await {
                    tracing::warn!(%error, "host terminal output failed");
                    failed = true;
                    break;
                }
            }
            if failed {
                return;
            }
            let ended = {
                let state = pump_screens.lock().await;
                let sessions = state.terminal();
                sessions.lock().unwrap_or_else(|e| e.into_inner()).reap()
            };
            ended.finish();
        }
    });
}

/// Reads the request payload as JSON, falling back to `u16` columns and rows.
///
/// The client sends `open` and `resize` as JSON objects; keeping the binary
/// fallback means a version that packs the pair differently still resizes.
fn geometry(payload: &[u8]) -> (Option<u16>, Option<u16>) {
    if let Ok(value) = serde_json::from_slice::<Value>(payload) {
        let number = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|v| u16::try_from(v).ok())
        };
        return (number("cols"), number("rows"));
    }
    if payload.len() >= 4 {
        return (
            Some(u16::from_le_bytes([payload[0], payload[1]])),
            Some(u16::from_le_bytes([payload[2], payload[3]])),
        );
    }
    (None, None)
}

async fn handle_frame(
    channel: &Arc<RTCDataChannel>,
    screens: &Arc<tokio::sync::Mutex<crate::features::host::peer::screens::Screens>>,
    data: &[u8],
    stop: &CancellationToken,
) {
    let Some(frame) = Frame::parse(data) else {
        tracing::warn!(bytes = data.len(), "host terminal frame unparsed");
        return;
    };
    // Metadata only: payloads carry keystrokes, screen contents and replayed
    // history, none of which may reach the log files or a diagnostics bundle.
    tracing::debug!(
        kind = ?frame.kind,
        session = frame.session,
        bytes = frame.payload.len(),
        "host terminal frame"
    );
    let mut outgoing = Vec::new();
    match frame.kind {
        FrameType::ListSessionsReq => {
            let listed = {
                let state = screens.lock().await;
                let sessions = state.terminal();
                let sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                sessions.list()
            };
            let payload =
                serde_json::to_vec(&Value::Array(listed)).unwrap_or_else(|_| b"[]".into());
            outgoing.push((FrameType::ListSessionsResp, frame.session, payload));
        }
        FrameType::Open => {
            let text = serde_json::from_slice::<Value>(&frame.payload).ok();
            let (cols, rows) = geometry(&frame.payload);
            let shell = match text
                .as_ref()
                .and_then(|v| v.get("shell").and_then(Value::as_str))
            {
                Some(requested) if !requested.is_empty() => resolve_shell(requested),
                _ => Sessions::default_shell(),
            };
            tracing::info!(requested = shell, "host terminal shell selected");
            let name = text
                .as_ref()
                .and_then(|v| v.get("name").and_then(Value::as_str))
                .map(str::to_owned);
            let result = {
                let state = screens.lock().await;
                let sessions = state.terminal();
                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                sessions.open(
                    frame.session,
                    &shell,
                    name,
                    cols.unwrap_or(80),
                    rows.unwrap_or(24),
                )
            };
            let (payload, session, replay) = match result {
                Ok((handle, name, replay)) => (
                    json!({
                        // The client reads `code`/`message` and a session id;
                        // the exact spelling of the id key is not certain, so
                        // every plausible one carries the same value and the
                        // client's decoder picks up whichever it knows.
                        "code": 0,
                        "message": "",
                        "session_id": handle,
                        "resp_session_id": handle,
                        "respSessionId": handle,
                        "handle": handle,
                        "name": name,
                        "shell": shell,
                    }),
                    handle,
                    replay,
                ),
                Err(error) => (
                    json!({
                        "code": 1,
                        "message": error.to_string(),
                        "session_id": frame.session,
                    }),
                    frame.session,
                    Vec::new(),
                ),
            };
            outgoing.push((
                FrameType::OpenResp,
                session,
                serde_json::to_vec(&payload).unwrap_or_default(),
            ));
            if !replay.is_empty() {
                // Redraw what the screen last held. Clearing first stops a
                // reattach from appending the history to what is already
                // there.
                let mut redraw = Vec::with_capacity(replay.len() + 12);
                redraw.extend_from_slice(b"\x1b[2J\x1b[3J\x1b[H");
                redraw.extend_from_slice(&replay);
                outgoing.push((FrameType::Stdout, session, redraw));
            }
        }
        FrameType::Stdin => {
            let state = screens.lock().await;
            let sessions = state.terminal();
            let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(error) = sessions.write(frame.session, &frame.payload) {
                tracing::warn!(%error, "host terminal write failed");
            }
        }
        FrameType::Resize => {
            let (cols, rows) = geometry(&frame.payload);
            if let (Some(cols), Some(rows)) = (cols, rows) {
                let state = screens.lock().await;
                let sessions = state.terminal();
                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(error) = sessions.resize(frame.session, cols, rows) {
                    tracing::warn!(%error, "host terminal resize failed");
                }
            }
        }
        FrameType::Close | FrameType::KillSessionReq => {
            let result = {
                let state = screens.lock().await;
                let sessions = state.terminal();
                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                sessions.kill(frame.session)
            };
            // End the shell only once both locks are released.
            let result = result.map(Ended::finish);
            if frame.kind == FrameType::KillSessionReq {
                let payload = match result {
                    Ok(()) => json!({"code": 0, "message": "", "session_id": frame.session}),
                    Err(error) => json!({
                        "code": 1,
                        "message": error.to_string(),
                        "session_id": frame.session,
                    }),
                };
                outgoing.push((
                    FrameType::KillSessionResp,
                    frame.session,
                    serde_json::to_vec(&payload).unwrap_or_default(),
                ));
            }
        }
        FrameType::Detach => {
            let state = screens.lock().await;
            let sessions = state.terminal();
            let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(error) = sessions.detach(frame.session) {
                tracing::warn!(%error, "host terminal detach failed");
            }
        }
        FrameType::UnlockReq => {
            // The lock screen is a mobile concept; a Linux desktop is never
            // locked from the terminal's point of view.
            outgoing.push((FrameType::UnlockResp, frame.session, b"{}".to_vec()));
        }
        FrameType::Signal => {
            let ended = {
                let state = screens.lock().await;
                let sessions = state.terminal();
                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                sessions.kill(frame.session)
            };
            if let Ok(ended) = ended {
                ended.finish();
            }
        }
        other => {
            tracing::debug!(?other, "host terminal frame ignored");
        }
    }
    for (kind, handle, payload) in outgoing {
        if stop.is_cancelled() {
            return;
        }
        let frame = Frame::encode(kind, handle, &payload);
        tracing::debug!(
            kind = ?kind,
            session = handle,
            bytes = payload.len(),
            "host terminal reply"
        );
        if let Err(error) = channel.send(&Bytes::from(frame)).await {
            tracing::warn!(%error, "host terminal reply failed");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let encoded = Frame::encode(FrameType::ListSessionsResp, 7, b"hello");
        assert_eq!(&encoded[..4], MAGIC);
        assert_eq!(encoded[4], VERSION);
        assert_eq!(encoded[5], 10);
        let frame = Frame::parse(&encoded).expect("parse");
        assert_eq!(frame.kind, FrameType::ListSessionsResp);
        assert_eq!(frame.session, 7);
        assert_eq!(frame.payload, b"hello");
    }

    #[test]
    fn the_captured_list_request_decodes() {
        // The phone's first terminal frame, byte for byte.
        let captured = [
            0x54, 0x45, 0x52, 0x4d, 0x01, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let frame = Frame::parse(&captured).expect("parse");
        assert_eq!(frame.kind, FrameType::ListSessionsReq);
        assert_eq!(frame.session, 0);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn protobuf_is_not_mistaken_for_a_frame() {
        // The check_env envelope the phone sends first.
        let protobuf = [
            0x08, 0x05, 0x10, 0x8a, 0x86, 0xb6, 0xed, 0x93, 0x34, 0xaa, 0x01, 0x07, 0x0a, 0x02,
            0x08, 0x01, 0xe2, 0x01, 0x00,
        ];
        assert!(Frame::parse(&protobuf).is_none());
    }

    #[test]
    fn a_truncated_frame_is_refused() {
        let mut encoded = Frame::encode(FrameType::Stdout, 1, b"payload");
        encoded.truncate(HEADER + 2);
        assert!(Frame::parse(&encoded).is_none());
    }

    fn shell() -> &'static str {
        "/bin/sh"
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn sessions_are_keyed_by_the_controller_handle() {
        let mut sessions = Sessions::default();
        sessions.open(11, shell(), None, 80, 24).expect("open");
        sessions.open(22, shell(), None, 80, 24).expect("open");
        assert_eq!(sessions.list().len(), 2);
        assert!(sessions.write(11, b"echo A\n").is_ok());
        assert!(sessions.write(33, b"echo B\n").is_err());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn a_duplicate_name_is_refused() {
        let mut sessions = Sessions::default();
        sessions
            .open(1, shell(), Some("build".into()), 80, 24)
            .expect("open");
        assert_eq!(
            sessions.open(2, shell(), Some("build".into()), 80, 24),
            Err(SessionError::DuplicateName)
        );
    }

    #[test]
    fn invalid_names_are_refused() {
        let mut sessions = Sessions::default();
        assert_eq!(
            sessions.open(1, shell(), Some("   ".into()), 80, 24),
            Err(SessionError::InvalidName)
        );
        assert_eq!(
            sessions.open(1, shell(), Some("a/b".into()), 80, 24),
            Err(SessionError::InvalidName)
        );
        assert_eq!(
            sessions.open(1, shell(), Some("x".repeat(MAX_NAME + 1)), 80, 24),
            Err(SessionError::NameTooLong)
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn write_and_resize_reach_the_shell() {
        let mut sessions = Sessions::default();
        sessions.open(1, shell(), None, 80, 24).expect("open");
        sessions.resize(1, 120, 40).expect("resize");
        sessions.write(1, b"echo READY\n").expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump() {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if String::from_utf8_lossy(&seen).contains("READY") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("shell produced: {:?}", String::from_utf8_lossy(&seen));
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn a_reattach_replays_the_screen() {
        let mut sessions = Sessions::default();
        let (handle, _, replay) = sessions.open(0, shell(), None, 80, 24).expect("open");
        assert!(replay.is_empty(), "a fresh session has nothing to replay");
        sessions.write(handle, b"echo HISTORY\n").expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let output: Vec<u8> = sessions
                .pump()
                .into_iter()
                .filter(|(_, kind, _)| *kind == FrameType::Stdout)
                .flat_map(|(_, _, payload)| payload)
                .collect();
            if String::from_utf8_lossy(&output).contains("HISTORY") {
                break;
            }
        }
        let (again, _, replay) = sessions.open(handle, shell(), None, 100, 40).expect("open");
        assert_eq!(again, handle, "reattach keeps the same shell");
        assert!(
            String::from_utf8_lossy(&replay).contains("HISTORY"),
            "reattach replays the screen: {:?}",
            String::from_utf8_lossy(&replay)
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn an_exited_shell_reports_once() {
        let mut sessions = Sessions::default();
        sessions.open(1, shell(), None, 80, 24).expect("open");
        sessions.write(1, b"exit\n").expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if sessions
                .pump()
                .iter()
                .any(|(_, kind, _)| *kind == FrameType::Exit)
            {
                sessions.reap().finish();
                assert!(sessions.is_empty());
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("shell did not exit");
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn killing_removes_a_session_and_a_second_kill_is_gone() {
        let mut sessions = Sessions::default();
        sessions.open(1, shell(), None, 80, 24).expect("open");
        sessions.kill(1).expect("kill").finish();
        assert_eq!(sessions.kill(1).err(), Some(SessionError::Gone));
        assert!(sessions.is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn windows_shell_kinds_fall_back_to_a_posix_shell() {
        // The Android client always asks for `powershell`; refusing it is what
        // made the phone report a failed session creation.
        assert!(std::path::Path::new(&resolve_shell("powershell")).exists());
        assert!(std::path::Path::new(&resolve_shell("cmd")).exists());
        assert!(std::path::Path::new(&resolve_shell("bash")).exists());
    }

    #[test]
    fn geometry_accepts_json_and_binary() {
        assert_eq!(
            geometry(br#"{"cols":100,"rows":40}"#),
            (Some(100), Some(40))
        );
        assert_eq!(geometry(&[50, 0, 20, 0]), (Some(50), Some(20)));
        assert_eq!(geometry(b""), (None, None));
    }
}
