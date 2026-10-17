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
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

/// The four bytes every terminal frame starts with.
const MAGIC: &[u8; 4] = b"TERM";
/// Fixed header size, magic included.
const HEADER: usize = 16;
/// Only version 1 has ever been seen; refuse anything else rather than guess.
const VERSION: u8 = 1;
/// Shells one login may hold.
const MAX_SESSIONS: usize = 16;
/// The client rejects a name longer than this and shows its own length hint;
/// matching the limit keeps the two in agreement instead of failing at the
/// client after the server already accepted it.
const MAX_NAME: usize = 64;
/// Shells all logins together may hold, on top of [`MAX_SESSIONS`] each.
const MAX_TOTAL: usize = 64;
/// Input a session may hold when the pty will not take it yet. A frame that
/// would go past it is refused whole (see [`Sessions::write`]).
const PENDING_LIMIT: usize = 1024 * 1024;
/// Most pty output one connection reads in a single pump tick.
const PUMP_BUDGET: usize = 256 * 1024;
/// Output the data channel may hold before the pump stops reading the ptys,
/// the same mark the file transfer keeps to.
const OUTPUT_HIGH_WATER: usize = 2 * 1024 * 1024;
/// A shell nobody has attached to for this long is ended.
const IDLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// How often detached shells are checked against [`IDLE_LIMIT`].
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// The largest terminal a controller may ask for. The screen model allocates
/// every cell up front (32 bytes each, twice while a full-screen program uses
/// the alternate screen), so an unchecked 65535 x 65535 would ask for 128 GiB
/// and take the whole host down. 512 columns is wider than a 4K screen at a
/// 7-pixel font; 512 x 256 is 4 MiB a screen. The bound also keeps the size
/// inside the signed 16-bit `COORD` ConPTY takes.
const MAX_COLS: u16 = 512;
const MAX_ROWS: u16 = 256;

/// The size the pty and the model actually get for a requested one.
fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

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

/// 一个附着的输出消费者：远程主控连接，或本机 attach 的终端。
///
/// 两者读的是同一个 pty，只是出口不同：远程走 DataChannel，本机走 Unix
/// socket。用同一个枚举做队列的键，pump 读一次 pty 就能把数据分给所有
/// 消费者。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Consumer {
    Remote(u64),
    Local(u64),
}

/// One shell and the metadata the session list shows.
struct Session {
    pty: crate::platform::terminal::Session,
    name: String,
    state: SessionState,
    created_ms: i64,
    last_active_ms: i64,
    /// The shell's screen, kept as a terminal model rather than a byte log.
    ///
    /// A reattach replays this, so what the controller draws is what was on
    /// screen. Re-feeding a byte log instead runs the output through the
    /// client's parser a second time, which is wrong the moment the log is
    /// cut: byte N of a terminal stream has no meaning on its own, and a
    /// full-screen program that never emits a newline has no safe cut at all —
    /// the byte cap left a `top` replay truncated in the middle of one.
    screen: vt100::Parser,
    /// Input the pty would not take yet. A paste or a slow shell fills the
    /// pty buffer; dropping the rest would lose keystrokes silently.
    pending: Vec<u8>,
    /// 每个消费者待发的输出。只要有消费者，pump 就会读 pty；没人附着时
    /// shell 由 pty 自己的流控挡住，输出等下一次 attach。
    output: BTreeMap<Consumer, Vec<u8>>,
    /// 远程主控最近一次给出的尺寸。
    remote_size: Option<(u16, u16)>,
    /// 本机 attach 最近一次给出的尺寸。
    local_size: Option<(u16, u16)>,
    /// When the last connection let go, for the idle limit.
    detached_since: Option<std::time::Instant>,
}

impl Session {
    /// Pushes as much queued input as the pty accepts right now.
    fn flush_pending(&mut self) {
        while !self.pending.is_empty() {
            match self.pty.write(&self.pending) {
                Ok(0) | Err(_) => break,
                Ok(written) => {
                    self.pending.drain(..written);
                }
            }
        }
    }

    /// Feeds output to the session's screen model.
    ///
    /// The model applies the terminal's own rules — cursor motion, scrolling,
    /// the alternate screen — so what a reattach replays is a screen rather
    /// than a byte log. No scrollback is kept: a reattach shows the visible
    /// screen, which is what `capture-pane` gives the reference server.
    fn record(&mut self, data: &[u8]) {
        self.screen.process(data);
    }

    /// 让一个消费者离开。只有最后一个消费者走掉时才算「没人附着」。
    fn detach(&mut self, consumer: Consumer) {
        if self.output.remove(&consumer).is_none() {
            return;
        }
        match consumer {
            Consumer::Remote(_) => self.remote_size = None,
            Consumer::Local(_) => self.local_size = None,
        }
        self.apply_size();
        if self.output.is_empty() {
            self.detached_since = Some(std::time::Instant::now());
            if self.state == SessionState::Running {
                self.state = SessionState::Detached;
            }
        }
        self.last_active_ms = now_ms();
    }

    /// 把 pty 与屏幕模型的尺寸同步到当前所有附着者。
    ///
    /// 有多个附着者时取各维的最小值，与 tmux 一致：多出来的那端留白，
    /// 谁都不会看到被折行的内容。最后一个附着者离开后恢复成仍附着那端
    /// 的尺寸。
    fn apply_size(&mut self) {
        let size = match (self.remote_size, self.local_size) {
            (Some((remote_cols, remote_rows)), Some((local_cols, local_rows))) => {
                (remote_cols.min(local_cols), remote_rows.min(local_rows))
            }
            (Some(size), None) | (None, Some(size)) => size,
            (None, None) => return,
        };
        let (cols, rows) = clamp_size(size.0, size.1);
        if self.pty.resize(cols, rows).is_err() {
            return;
        }
        // The model has to follow the pty, or the replay would describe a
        // screen of the old size.
        self.screen.screen_mut().set_size(rows, cols);
    }
}

/// Sessions taken out of the table.
///
/// Dropping a session ends its shell, which can block for the pty's hangup
/// grace period while a shell that ignores `SIGHUP` is killed. The table
/// therefore hands ended sessions back instead of dropping them under its
/// lock, and [`Ended::finish`] drops them once the caller has let go.
#[must_use = "dropping ended sessions can block; call `finish` after releasing the table"]
pub(crate) struct Ended {
    sessions: Vec<Session>,
    /// Ended by someone other than the shell itself: every process the
    /// shell's session still holds goes too, background jobs included.
    forced: bool,
}

impl Ended {
    fn exited(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            forced: false,
        }
    }

    fn forced(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            forced: true,
        }
    }

    /// Ends the shells off the caller's thread: on the blocking pool from the
    /// runtime, on a short-lived thread otherwise (the settings page calls
    /// this from the UI thread). No async worker or frame waits for a shell
    /// to die.
    pub(crate) fn finish(self) {
        if self.sessions.is_empty() {
            return;
        }
        let end = move || {
            for session in self.sessions {
                if self.forced {
                    session.pty.terminate();
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(end);
            }
            Err(_) => {
                if let Err(error) = std::thread::Builder::new()
                    .name("terminal end".into())
                    .spawn(end)
                {
                    tracing::warn!(%error, "terminal shells could not be ended off thread");
                }
            }
        }
    }
}

/// Owns every shell opened by a controller.
///
/// A shell is keyed by the login generation that opened it and the handle
/// its controller picked: the controller chooses handles, so two logins may
/// well pick the same number, and nothing one login does (open, rename,
/// count against the limit, read output) may touch or reveal another's.
/// Names are unique per login because the client lists sessions by name and
/// offers a rename action; a duplicate would make two rows
/// indistinguishable.
#[derive(Default)]
pub(crate) struct Sessions {
    sessions: HashMap<(String, u32), Session>,
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
    #[error("输入过快，这段输入未送达终端")]
    InputOverflow,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

fn key(owner: &str, handle: u32) -> (String, u32) {
    (owner.to_owned(), handle)
}

impl Sessions {
    /// Opens a shell, or reattaches to one, and returns its handle, name and
    /// any output to replay. `connection` becomes the one its output goes to.
    ///
    /// The controller normally picks the handle, but its `open` frame may
    /// carry zero when it wants the host to allocate one; the returned handle
    /// is what later `stdin`/`resize` frames must use. Re-opening an existing
    /// handle re-uses the shell and returns its screen, which is what the
    /// controller redraws on reattach.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        &mut self,
        owner: &str,
        handle: u32,
        connection: u64,
        shell: &str,
        name: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<(u32, String, Vec<u8>), SessionError> {
        let handle = if handle == 0 {
            self.free_handle(owner)
        } else {
            handle
        };
        // Re-opening an existing handle happens on reconnect and when the
        // size changes; replacing the pty would hang up a shell in use.
        if let Some(existing) = self.sessions.get_mut(&key(owner, handle)) {
            if existing.state != SessionState::Exited {
                existing.state = SessionState::Running;
            }
            existing.remote_size = Some((cols, rows));
            existing
                .output
                .entry(Consumer::Remote(connection))
                .or_default();
            existing.detached_since = None;
            existing.apply_size();
            existing.last_active_ms = now_ms();
            return Ok((
                handle,
                existing.name.clone(),
                existing.screen.screen().state_formatted(),
            ));
        }
        if self.owned(owner).count() >= MAX_SESSIONS || self.sessions.len() >= MAX_TOTAL {
            return Err(SessionError::TooMany);
        }
        let name = match name {
            Some(name) => validate_name(&name)?,
            None => self.default_name(owner),
        };
        if self.owned(owner).any(|(_, session)| session.name == name) {
            return Err(SessionError::DuplicateName);
        }
        let (cols, rows) = clamp_size(cols, rows);
        let pty = crate::platform::terminal::Session::open(shell, cols, rows)
            .map_err(|_| SessionError::Gone)?;
        let stamp = now_ms();
        self.sessions.insert(
            key(owner, handle),
            Session {
                pty,
                name: name.clone(),
                state: SessionState::Running,
                created_ms: stamp,
                last_active_ms: stamp,
                screen: vt100::Parser::new(rows, cols, 0),
                pending: Vec::new(),
                output: BTreeMap::from([(Consumer::Remote(connection), Vec::new())]),
                remote_size: Some((cols, rows)),
                local_size: None,
                detached_since: None,
            },
        );
        Ok((handle, name, Vec::new()))
    }

    fn owned<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = (u32, &'a Session)> + 'a {
        self.sessions
            .iter()
            .filter(move |((who, _), _)| who == owner)
            .map(|((_, handle), session)| (*handle, session))
    }

    fn get_mut(&mut self, owner: &str, handle: u32) -> Result<&mut Session, SessionError> {
        self.sessions
            .get_mut(&key(owner, handle))
            .ok_or(SessionError::Gone)
    }

    /// The lowest handle this login does not use; the controller's handle
    /// space starts at 1, leaving 0 free to mean "assign one".
    fn free_handle(&self, owner: &str) -> u32 {
        (1..u32::MAX)
            .find(|handle| !self.sessions.contains_key(&key(owner, *handle)))
            .unwrap_or(1)
    }

    /// `shell-N`, skipping names this login already uses so a closed
    /// session's number is not immediately reused by a different shell.
    fn default_name(&mut self, owner: &str) -> String {
        for _ in 0..=MAX_SESSIONS {
            self.next_name = self.next_name.wrapping_add(1);
            let name = format!("shell-{}", self.next_name);
            if !self.owned(owner).any(|(_, session)| session.name == name) {
                return name;
            }
        }
        format!("shell-{}", self.next_name)
    }

    /// The default shell, matching what a terminal emulator would start.
    ///
    /// The platform decides: `$SHELL` (falling back to the system's login
    /// shell) on POSIX, `ComSpec` on Windows.
    pub(crate) fn default_shell() -> String {
        crate::platform::terminal::default_shell()
    }

    /// The sessions `owner` may see, as the objects the client parses.
    pub(crate) fn list(&self, owner: &str) -> Vec<Value> {
        let mut listed: Vec<_> = self
            .owned(owner)
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

    /// Queues keyboard input and pushes what the pty accepts right now.
    ///
    /// Input that does not fit is refused as a whole frame. Dropping part of
    /// the queue instead would hand the shell the start of one command and
    /// the end of another, which it would then run.
    pub(crate) fn write(
        &mut self,
        owner: &str,
        handle: u32,
        data: &[u8],
    ) -> Result<(), SessionError> {
        let session = self.get_mut(owner, handle)?;
        session.last_active_ms = now_ms();
        session.flush_pending();
        if session.pending.len() + data.len() > PENDING_LIMIT {
            return Err(SessionError::InputOverflow);
        }
        session.pending.extend_from_slice(data);
        session.flush_pending();
        Ok(())
    }

    /// Applies a new size, which the shell sees as `SIGWINCH`.
    pub(crate) fn resize(
        &mut self,
        owner: &str,
        handle: u32,
        cols: u16,
        rows: u16,
    ) -> Result<(), SessionError> {
        let session = self.get_mut(owner, handle)?;
        session.remote_size = Some((cols, rows));
        session.apply_size();
        session.last_active_ms = now_ms();
        Ok(())
    }

    /// Renames one of `owner`'s sessions.
    pub(crate) fn rename(
        &mut self,
        owner: &str,
        handle: u32,
        name: &str,
    ) -> Result<(), SessionError> {
        let name = validate_name(name)?;
        if !self.sessions.contains_key(&key(owner, handle)) {
            return Err(SessionError::Gone);
        }
        if self
            .owned(owner)
            .any(|(other, session)| other != handle && session.name == name)
        {
            return Err(SessionError::DuplicateName);
        }
        self.get_mut(owner, handle)?.name = name;
        Ok(())
    }

    /// Ends every session `owner` owns.
    pub(crate) fn kill_owned(&mut self, owner: &str) -> Ended {
        let keys: Vec<_> = self
            .sessions
            .keys()
            .filter(|(who, _)| who == owner)
            .cloned()
            .collect();
        Ended::forced(
            keys.iter()
                .filter_map(|key| self.sessions.remove(key))
                .collect(),
        )
    }

    /// Ends every session, whoever opened it.
    pub(crate) fn kill_all(&mut self) -> Ended {
        Ended::forced(self.sessions.drain().map(|(_, session)| session).collect())
    }

    /// Removes one of `owner`'s sessions; finishing the result ends the shell.
    pub(crate) fn kill(&mut self, owner: &str, handle: u32) -> Result<Ended, SessionError> {
        let session = self
            .sessions
            .remove(&key(owner, handle))
            .ok_or(SessionError::Gone)?;
        Ok(Ended::forced(vec![session]))
    }

    /// Lets go of one of `owner`'s sessions without ending the shell.
    pub(crate) fn detach(&mut self, owner: &str, handle: u32) -> Result<(), SessionError> {
        let session = self.get_mut(owner, handle)?;
        let remotes: Vec<_> = session
            .output
            .keys()
            .filter(|consumer| matches!(consumer, Consumer::Remote(_)))
            .copied()
            .collect();
        for consumer in remotes {
            session.detach(consumer);
        }
        Ok(())
    }

    /// Lets go of every session attached to `connection`, which has closed.
    pub(crate) fn release(&mut self, connection: u64) {
        for session in self.sessions.values_mut() {
            session.detach(Consumer::Remote(connection));
        }
    }

    /// How many shells are running, for the host's settings page.
    pub(crate) fn count(&self) -> usize {
        self.sessions.len()
    }

    /// 把 pty 里现成的输出读出来，分发给每个消费者的队列。
    ///
    /// 一次读取同时喂屏幕模型和所有队列，所以远程与本机看到的是同一份
    /// 数据。任一个消费者的积压到了上限就不再读这个会话，让 pty 的缓冲
    /// 反压 shell，而不是把内存堆爆。
    fn drain(&mut self, budget: usize) {
        let mut buffer = [0u8; 8192];
        let mut left = budget;
        for session in self.sessions.values_mut() {
            if left == 0 {
                break;
            }
            if session.output.is_empty() || session.state == SessionState::Exited {
                continue;
            }
            if session
                .output
                .values()
                .any(|queue| queue.len() >= OUTPUT_HIGH_WATER)
            {
                continue;
            }
            session.flush_pending();
            while left > 0 {
                let want = left.min(buffer.len());
                match session.pty.read(&mut buffer[..want]) {
                    Ok(Some(0)) | Err(_) => break,
                    Ok(Some(read)) => {
                        session.record(&buffer[..read]);
                        for queue in session.output.values_mut() {
                            queue.extend_from_slice(&buffer[..read]);
                        }
                        left -= read;
                        session.last_active_ms = now_ms();
                    }
                    Ok(None) => break,
                }
            }
            if session.pty.exited() {
                session.state = SessionState::Exited;
            }
        }
    }

    /// Takes up to `budget` bytes the shells attached to `connection` have
    /// written since the last poll.
    ///
    /// The budget is what the data channel can still take, so a shell that
    /// floods output is held back by the pty (the shell blocks writing)
    /// instead of piling up in the send queue. Each shell's exit is reported
    /// once, so the controller learns a session ended without polling the
    /// list.
    pub(crate) fn pump(
        &mut self,
        connection: u64,
        budget: usize,
    ) -> Vec<(u32, FrameType, Vec<u8>)> {
        self.drain(budget);
        let consumer = Consumer::Remote(connection);
        let mut out = Vec::new();
        let mut attached: Vec<_> = self
            .sessions
            .iter_mut()
            .filter(|(_, session)| session.output.contains_key(&consumer))
            .map(|((_, handle), session)| (*handle, session))
            .collect();
        attached.sort_by_key(|(handle, _)| *handle);
        // Split the budget so one busy shell cannot starve the others.
        let share = (budget / attached.len().max(1)).max(1);
        for (handle, session) in attached {
            if let Some(queue) = session.output.get_mut(&consumer) {
                if !queue.is_empty() {
                    let take = queue.len().min(share);
                    out.push((handle, FrameType::Stdout, queue.drain(..take).collect()));
                }
            }
            if session.state == SessionState::Exited {
                // Report the exit once, then drop this consumer so `reap`
                // can recover the session after every other one saw it too.
                session.output.remove(&consumer);
                out.push((handle, FrameType::Exit, b"{}".to_vec()));
            }
        }
        out
    }

    /// Takes out the sessions whose exit every attached consumer has seen.
    ///
    /// A session is not removed on the first pump that notices the shell
    /// ended: the other connection still has
    /// to be told, and its queue only empties once it was.
    pub(crate) fn reap(&mut self) -> Ended {
        let exited: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.state == SessionState::Exited && session.output.is_empty()
            })
            .map(|(key, _)| key.clone())
            .collect();
        Ended::exited(
            exited
                .iter()
                .filter_map(|key| self.sessions.remove(key))
                .collect(),
        )
    }

    /// Takes out the shells nobody is attached to that have exited, and ends
    /// those left alone longer than `idle`.
    pub(crate) fn sweep(&mut self, idle: std::time::Duration) -> (Ended, Ended) {
        let mut exited = Vec::new();
        let mut abandoned = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            let Some(since) = session.detached_since.filter(|_| session.output.is_empty()) else {
                continue;
            };
            if session.pty.exited() {
                exited.push(key.clone());
            } else if since.elapsed() >= idle {
                abandoned.push(key.clone());
            }
        }
        let mut take = |keys: Vec<(String, u32)>| -> Vec<Session> {
            keys.iter()
                .filter_map(|key| self.sessions.remove(key))
                .collect()
        };
        (Ended::exited(take(exited)), Ended::forced(take(abandoned)))
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// Maps the controller's shell choice to a real program.
///
/// The `open` payload names a shell *kind* (`powershell`, `cmd`, `bash`), not
/// a path, and each platform knows which kinds it can offer. The platform
/// layer decides; this stays as a named seam so the call sites and tests here
/// read the same on every OS.
fn resolve_shell(requested: &str) -> String {
    crate::platform::terminal::resolve_shell(requested)
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
static SESSIONS: std::sync::OnceLock<Arc<std::sync::Mutex<Sessions>>> = std::sync::OnceLock::new();

pub(crate) fn shared() -> Arc<std::sync::Mutex<Sessions>> {
    Arc::clone(SESSIONS.get_or_init(|| {
        let table = Arc::new(std::sync::Mutex::new(Sessions::default()));
        let janitor = Arc::downgrade(&table);
        if let Err(error) = std::thread::Builder::new()
            .name("terminal sweep".into())
            .spawn(move || sweep_forever(janitor))
        {
            tracing::warn!(%error, "terminal idle sweep could not start");
        }
        table
    }))
}

/// Every [`SWEEP_INTERVAL`], takes out shells that exited while nobody was
/// attached and ends those left detached longer than [`IDLE_LIMIT`].
fn sweep_forever(table: std::sync::Weak<std::sync::Mutex<Sessions>>) {
    loop {
        std::thread::sleep(SWEEP_INTERVAL);
        let Some(table) = table.upgrade() else {
            return;
        };
        let (exited, abandoned) = lock_table(&table).sweep(IDLE_LIMIT);
        exited.finish();
        abandoned.finish();
    }
}

fn lock_table(table: &std::sync::Mutex<Sessions>) -> std::sync::MutexGuard<'_, Sessions> {
    table.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ends every shell a login generation opened, whether or not a controller is
/// connected right now. Called wherever that login's grant ends: sign-out or
/// switching accounts ([`crate::features::host::Handle::retire`]), turning the
/// terminal or remote control off, and the local user's "disconnect".
pub(crate) fn end_login(account: &str) {
    // Nothing to end in a process that never served a terminal (the Windows
    // service, any platform without a backend); do not create the table.
    let Some(table) = SESSIONS.get() else {
        return;
    };
    let ended = lock_table(table).kill_owned(account);
    if !ended.sessions.is_empty() {
        tracing::info!(
            sessions = ended.sessions.len(),
            "host terminal shells ended with their grant"
        );
    }
    ended.finish();
}

/// Ends every shell, for the host's "end all" button.
pub(crate) fn end_all() {
    if let Some(table) = SESSIONS.get() {
        lock_table(table).kill_all().finish();
    }
}

/// Why this host cannot serve a terminal; `None` when it can.
///
/// The platform answers for this process (root on POSIX, LocalSystem on
/// Windows). In unattended mode the shell would come from the SYSTEM service
/// rather than from this process, so the settings page, which runs as the
/// user, is told the same thing the service would answer and does not offer
/// a switch every connection would refuse.
pub(crate) fn unavailable() -> Option<&'static str> {
    crate::platform::terminal::unavailable().or_else(|| {
        crate::platform::host_service::resident::managed()
            .then_some("无人值守模式以 SYSTEM 身份运行，不提供终端")
    })
}

/// How many shells this process is running, for the host's settings page.
pub(crate) fn running() -> usize {
    SESSIONS.get().map_or(0, |table| lock_table(table).count())
}

/// Numbers each terminal connection, so a shell knows whose pump reads it.
fn next_connection() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    let connection = next_connection();
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
                handle_frame(&channel, &screens, &data, connection, &stop).await;
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
    let table = shared();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
        loop {
            ticker.tick().await;
            let Some(channel) = pump_channel.upgrade().filter(|_| !pump_stop.is_cancelled()) else {
                break;
            };
            // The grant is re-read every tick, not only when a frame comes
            // in: a controller that only watches output must lose it as soon
            // as the terminal is turned off, remote control is disabled or
            // the login ends. Ending the shells is the job of those switches
            // (`end_login`); a lease that merely went stale (signalling
            // reconnected) only closes this connection, so the shells wait
            // for the reattach.
            let allowed = pump_screens.lock().await.authorization().terminal_allowed();
            if !allowed {
                tracing::info!("host terminal grant ended; closing the terminal connection");
                pump_stop.cancel();
                cancel.cancel();
                let _ = channel.close().await;
                break;
            }
            let room = OUTPUT_HIGH_WATER.saturating_sub(channel.buffered_amount().await);
            if room == 0 {
                continue;
            }
            let frames = lock_table(&table).pump(connection, room.min(PUMP_BUDGET));
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
                // Output can no longer reach the controller; stop taking its
                // input too, and let the shells wait for a reattach.
                pump_stop.cancel();
                break;
            }
            lock_table(&table).reap().finish();
        }
        lock_table(&table).release(connection);
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
    connection: u64,
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
    // Re-check the grant on every frame, not only when the connection was
    // accepted. A refused frame is answered and dropped; ending the shells is
    // up to whatever ended the grant (`end_login`), because a lease that only
    // went stale in a signalling reconnect must not take the shells with it.
    // The output pump closes the connection within a tick.
    let (allowed, account, table) = {
        let state = screens.lock().await;
        let lease = state.authorization();
        (
            lease.terminal_allowed(),
            lease.account().to_owned(),
            state.terminal(),
        )
    };
    let reply = |code: i32, message: &str, session: u32| {
        serde_json::to_vec(&json!({
            "code": code,
            "message": message,
            "session_id": session,
        }))
        .unwrap_or_default()
    };
    let mut outgoing = Vec::new();
    if !allowed {
        tracing::warn!(kind = ?frame.kind, "host terminal frame refused");
        match frame.kind {
            FrameType::Open => {
                outgoing.push((
                    FrameType::OpenResp,
                    frame.session,
                    reply(1, "本机未允许终端", frame.session),
                ));
            }
            FrameType::KillSessionReq => {
                outgoing.push((
                    FrameType::KillSessionResp,
                    frame.session,
                    reply(1, "本机未允许终端", frame.session),
                ));
            }
            FrameType::ListSessionsReq => {
                outgoing.push((FrameType::ListSessionsResp, frame.session, b"[]".to_vec()));
            }
            _ => {}
        }
    } else {
        match frame.kind {
            FrameType::ListSessionsReq => {
                let listed = lock_table(&table).list(&account);
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
                let result = lock_table(&table).open(
                    &account,
                    frame.session,
                    connection,
                    &shell,
                    name,
                    cols.unwrap_or(80),
                    rows.unwrap_or(24),
                );
                let (payload, session, replay) = match result {
                    Ok((handle, name, replay)) => (
                        json!({
                            // The client reads `code`/`message` and a session
                            // id; the exact spelling of the id key is not
                            // certain, so every plausible one carries the same
                            // value and the client's decoder picks up
                            // whichever it knows.
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
                    // Redraw what the screen last held. The reset and the
                    // clear come first so the replay lands on a blank screen:
                    // the formatted state positions the cursor and sets
                    // attributes itself, but assumes it draws onto one.
                    let mut redraw = Vec::with_capacity(replay.len() + 16);
                    redraw.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[3J\x1b[H");
                    redraw.extend_from_slice(&replay);
                    outgoing.push((FrameType::Stdout, session, redraw));
                }
            }
            FrameType::Stdin => {
                let result = lock_table(&table).write(&account, frame.session, &frame.payload);
                match result {
                    Ok(()) => {}
                    Err(SessionError::InputOverflow) => {
                        tracing::warn!(
                            session = frame.session,
                            bytes = frame.payload.len(),
                            "host terminal input refused: the shell is not reading"
                        );
                        outgoing.push((
                            FrameType::Error,
                            frame.session,
                            reply(1, &SessionError::InputOverflow.to_string(), frame.session),
                        ));
                    }
                    Err(error) => {
                        tracing::debug!(%error, session = frame.session, "host terminal input dropped");
                    }
                }
            }
            FrameType::Resize => {
                let (cols, rows) = geometry(&frame.payload);
                if let (Some(cols), Some(rows)) = (cols, rows)
                    && let Err(error) =
                        lock_table(&table).resize(&account, frame.session, cols, rows)
                {
                    tracing::debug!(%error, session = frame.session, "host terminal resize dropped");
                }
            }
            FrameType::Close | FrameType::KillSessionReq => {
                let result = lock_table(&table).kill(&account, frame.session);
                // End the shell only once the table is released.
                let result = result.map(Ended::finish);
                if frame.kind == FrameType::KillSessionReq {
                    let payload = match result {
                        Ok(()) => reply(0, "", frame.session),
                        Err(error) => reply(1, &error.to_string(), frame.session),
                    };
                    outgoing.push((FrameType::KillSessionResp, frame.session, payload));
                }
            }
            FrameType::Detach => {
                if let Err(error) = lock_table(&table).detach(&account, frame.session) {
                    tracing::debug!(%error, session = frame.session, "host terminal detach dropped");
                }
            }
            FrameType::UnlockReq => {
                // The controller drives a locked machine's unlock sheet with
                // this: the macOS server checks `com.apple.screenIsUnlocked`
                // and replies with `code`/`message`/`success`. We never drive
                // an unlock (that needs the user's password); we only report
                // the state we can actually observe, and an unknown state is
                // not reported as unlocked.
                let payload = match crate::platform::capture::session_locked() {
                    Some(false) => json!({"success": true, "code": 0, "message": ""}),
                    Some(true) => json!({
                        "success": false,
                        "code": 1,
                        "message": "本机已锁屏，不支持远程解锁",
                    }),
                    None => json!({
                        "success": false,
                        "code": 1,
                        "message": "无法确认本机是否锁屏，不支持远程解锁",
                    }),
                };
                outgoing.push((
                    FrameType::UnlockResp,
                    frame.session,
                    serde_json::to_vec(&payload).unwrap_or_default(),
                ));
            }
            FrameType::Signal => {
                // The payload's signal semantics are unverified, and killing
                // the shell on an unknown signal would lose the session.
                // Ignore it until the frame is understood; Ctrl-C reaches the
                // shell as a byte on stdin anyway.
                tracing::debug!(session = frame.session, "host terminal signal ignored");
            }
            other => {
                tracing::debug!(?other, "host terminal frame ignored");
            }
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

    /// The shell the pty tests drive. `cmd.exe` is present on every Windows
    /// install; `/bin/sh` is on every POSIX one.
    fn shell() -> &'static str {
        if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }
    }

    /// One command line for that shell. `cmd.exe` reads a bare `\n` as part of
    /// the command in some cases, so Windows gets the CRLF it expects.
    fn line(command: &str) -> Vec<u8> {
        if cfg!(windows) {
            format!("{command}\r\n").into_bytes()
        } else {
            format!("{command}\n").into_bytes()
        }
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn sessions_are_keyed_by_the_controller_handle() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 11, 1, shell(), None, 80, 24)
            .expect("open");
        sessions
            .open("acct", 22, 1, shell(), None, 80, 24)
            .expect("open");
        assert_eq!(sessions.list("acct").len(), 2);
        assert!(sessions.write("acct", 11, &line("echo A")).is_ok());
        assert!(sessions.write("acct", 33, &line("echo B")).is_err());
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn a_duplicate_name_is_refused() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), Some("build".into()), 80, 24)
            .expect("open");
        assert_eq!(
            sessions.open("acct", 2, 1, shell(), Some("build".into()), 80, 24),
            Err(SessionError::DuplicateName)
        );
    }

    #[test]
    fn invalid_names_are_refused() {
        let mut sessions = Sessions::default();
        assert_eq!(
            sessions.open("acct", 1, 1, shell(), Some("   ".into()), 80, 24),
            Err(SessionError::InvalidName)
        );
        assert_eq!(
            sessions.open("acct", 1, 1, shell(), Some("a/b".into()), 80, 24),
            Err(SessionError::InvalidName)
        );
        assert_eq!(
            sessions.open(
                "acct",
                1,
                1,
                shell(),
                Some("x".repeat(MAX_NAME + 1)),
                80,
                24
            ),
            Err(SessionError::NameTooLong)
        );
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn write_and_resize_reach_the_shell() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.resize("acct", 1, 120, 40).expect("resize");
        sessions
            .write("acct", 1, &line("echo READY"))
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(1, PUMP_BUDGET) {
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
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn a_reattach_replays_the_screen() {
        let mut sessions = Sessions::default();
        let (handle, _, replay) = sessions
            .open("acct", 0, 1, shell(), None, 80, 24)
            .expect("open");
        assert!(replay.is_empty(), "a fresh session has nothing to replay");
        sessions
            .write("acct", handle, &line("echo HISTORY"))
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let output: Vec<u8> = sessions
                .pump(1, PUMP_BUDGET)
                .into_iter()
                .filter(|(_, kind, _)| *kind == FrameType::Stdout)
                .flat_map(|(_, _, payload)| payload)
                .collect();
            if String::from_utf8_lossy(&output).contains("HISTORY") {
                break;
            }
        }
        let (again, _, replay) = sessions
            .open("acct", handle, 1, shell(), None, 100, 40)
            .expect("open");
        assert_eq!(again, handle, "reattach keeps the same shell");
        assert!(
            String::from_utf8_lossy(&replay).contains("HISTORY"),
            "reattach replays the screen: {:?}",
            String::from_utf8_lossy(&replay)
        );
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn an_exited_shell_reports_once() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.write("acct", 1, &line("exit")).expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if sessions
                .pump(1, PUMP_BUDGET)
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
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn killing_removes_a_session_and_a_second_kill_is_gone() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.kill("acct", 1).expect("kill").finish();
        assert_eq!(sessions.kill("acct", 1).err(), Some(SessionError::Gone));
        assert!(sessions.is_empty());
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn every_requested_shell_kind_resolves_to_a_real_program() {
        // The Android client always asks for `powershell`. On Windows that is
        // the program itself; elsewhere it falls back to the login shell.
        // Either way the answer has to be a path that exists, because refusing
        // it is what made the phone report a failed session creation.
        assert!(std::path::Path::new(&resolve_shell("powershell")).exists());
        assert!(std::path::Path::new(&resolve_shell("cmd")).exists());
        assert!(std::path::Path::new(&resolve_shell("bash")).exists());
    }

    /// Drains what `connection` would send for a few seconds, until `needle`.
    fn output_until(sessions: &mut Sessions, connection: u64, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(connection, PUMP_BUDGET) {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if String::from_utf8_lossy(&seen).contains(needle) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn output_goes_only_to_the_attached_connection() {
        let mut sessions = Sessions::default();
        sessions
            .open("old-login", 1, 7, shell(), None, 80, 24)
            .expect("open");
        sessions
            .write("old-login", 1, b"echo SECRET_$((40+2))\n")
            .expect("write");
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Another login's connection, and a second connection of the same
        // login that never attached, read nothing.
        assert!(sessions.list("new-login").is_empty());
        for _ in 0..10 {
            assert!(sessions.pump(8, PUMP_BUDGET).is_empty());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(output_until(&mut sessions, 7, "SECRET_42").contains("SECRET_42"));
        // Another login cannot reattach to it either: the handle is its own.
        let (handle, _, replay) = sessions
            .open("new-login", 1, 8, shell(), None, 80, 24)
            .expect("open");
        assert_eq!(handle, 1);
        assert!(
            replay.is_empty(),
            "a new login gets a new shell, not the old screen"
        );
        assert_eq!(sessions.list("old-login").len(), 1);
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn a_released_shell_waits_for_the_reattach() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 7, shell(), None, 80, 24)
            .expect("open");
        sessions.release(7);
        assert_eq!(sessions.list("acct")[0]["state"], "detached");
        sessions.write("acct", 1, b"echo LATER\n").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            sessions.pump(7, PUMP_BUDGET).is_empty(),
            "nobody is attached"
        );
        sessions
            .open("acct", 1, 9, shell(), None, 80, 24)
            .expect("reattach");
        assert_eq!(sessions.list("acct")[0]["state"], "running");
        assert!(output_until(&mut sessions, 9, "LATER").contains("LATER"));
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn limits_and_names_are_per_login() {
        let mut sessions = Sessions::default();
        for handle in 1..=MAX_SESSIONS as u32 {
            sessions
                .open("a", handle, 1, shell(), None, 80, 24)
                .expect("open");
        }
        assert_eq!(
            sessions.open("a", 99, 1, shell(), None, 80, 24),
            Err(SessionError::TooMany)
        );
        sessions.rename("a", 1, "prod").expect("rename");
        // Another login is not held back by the first one's shells, and its
        // names do not reveal the first one's.
        sessions
            .open("b", 1, 2, shell(), Some("prod".into()), 80, 24)
            .expect("a second login opens its own shell");
        sessions.kill_owned("a").finish();
        assert_eq!(sessions.count(), 1);
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn input_that_does_not_fit_is_refused_whole() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        // The shell is busy and reads nothing; the pty takes a little, the
        // queue the rest, then a frame that would overflow is refused.
        sessions.write("acct", 1, b"sleep 3\n").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let line = [b'#'; 1023];
        let mut chunk = Vec::new();
        for _ in 0..64 {
            chunk.extend_from_slice(&line);
            chunk.push(b'\n');
        }
        let mut refused = false;
        for _ in 0..64 {
            match sessions.write("acct", 1, &chunk) {
                Ok(()) => {}
                Err(SessionError::InputOverflow) => {
                    refused = true;
                    break;
                }
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        assert!(refused, "a 4 MiB paste into a busy shell is refused");
        // Nothing was cut out of the queue to make room: what is queued is
        // whole frames on top of what the pty took, and stays within limit.
        let queued = sessions.sessions[&key("acct", 1)].pending.len();
        assert!(queued <= PENDING_LIMIT, "queue stays bounded: {queued}");
        assert!(
            queued + chunk.len() > PENDING_LIMIT,
            "refused only when it would not fit"
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn the_sweep_takes_exited_and_abandoned_shells() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions
            .open("acct", 2, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.write("acct", 1, &line("exit")).expect("write");
        std::thread::sleep(std::time::Duration::from_millis(300));
        sessions.release(1);
        let (exited, abandoned) = sessions.sweep(std::time::Duration::from_secs(3600));
        assert_eq!((exited.sessions.len(), abandoned.sessions.len()), (1, 0));
        exited.finish();
        abandoned.finish();
        let (exited, abandoned) = sessions.sweep(std::time::Duration::ZERO);
        assert_eq!((exited.sessions.len(), abandoned.sessions.len()), (0, 1));
        exited.finish();
        abandoned.finish();
        assert!(sessions.is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn ending_a_login_ends_only_its_shells() {
        let table = shared();
        lock_table(&table)
            .open("ending-login", 1, 1, shell(), None, 80, 24)
            .expect("open");
        lock_table(&table)
            .open("staying-login", 1, 2, shell(), None, 80, 24)
            .expect("open");
        end_login("ending-login");
        assert!(lock_table(&table).list("ending-login").is_empty());
        assert_eq!(lock_table(&table).list("staying-login").len(), 1);
        lock_table(&table).kill_owned("staying-login").finish();
    }

    #[test]
    // The bulk producer below is a POSIX shell loop. The model itself is
    // platform-independent, so Linux coverage of this property is enough.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_reattach_replays_the_screen_not_the_output_log() {
        let mut sessions = Sessions::default();
        let (handle, _, _) = sessions
            .open("acct", 0, 1, "/bin/sh", None, 80, 24)
            .expect("open");
        // Far more output than the byte log we used to keep, then a marker. A
        // log cut at 128 KiB can land inside an escape sequence and leave the
        // client printing the tail of it; a screen cannot be cut at all.
        sessions
            .write(
                "acct",
                handle,
                &line("for i in $(seq 1 4000); do echo FILLER-FILLER-FILLER-$i; done"),
            )
            .expect("write");
        sessions
            .write("acct", handle, &line("echo THE-LAST-LINE"))
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(1, usize::MAX) {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if String::from_utf8_lossy(&seen).contains("THE-LAST-LINE") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            String::from_utf8_lossy(&seen).contains("THE-LAST-LINE"),
            "the shell never finished"
        );
        let (again, _, replay) = sessions
            .open("acct", handle, 1, "/bin/sh", None, 80, 24)
            .expect("reattach");
        assert_eq!(again, handle, "reattach keeps the same shell");
        let text = String::from_utf8_lossy(&replay);
        assert!(
            text.contains("THE-LAST-LINE"),
            "the replay is not the current screen"
        );
        // The property the model gives us: the replay describes one screen, so
        // it does not grow with how much the shell has printed.
        assert!(
            replay.len() < 16 * 1024,
            "the replay grew with the output: {} bytes",
            replay.len()
        );
    }

    #[test]
    // A POSIX `printf`; the model is platform-independent.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_resize_reaches_the_replayed_screen() {
        let mut sessions = Sessions::default();
        let (handle, _, _) = sessions
            .open("acct", 0, 1, "/bin/sh", None, 80, 24)
            .expect("open");
        // A phone rotating or raising its keyboard narrows the terminal.
        sessions.resize("acct", handle, 40, 24).expect("resize");
        let wide = format!("{}{}", "A".repeat(30), "B".repeat(31));
        // The command line splits the two halves, so only the output holds
        // the joined line.
        sessions
            .write(
                "acct",
                handle,
                &line(&format!(
                    "printf '%s%s\\n' {} {}",
                    "A".repeat(30),
                    "B".repeat(31)
                )),
            )
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(1, PUMP_BUDGET) {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if String::from_utf8_lossy(&seen).contains(&wide) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            String::from_utf8_lossy(&seen).contains(&wide),
            "the shell never printed the line"
        );
        let (_, _, replay) = sessions
            .open("acct", handle, 1, "/bin/sh", None, 40, 24)
            .expect("reattach");
        // Draw the replay the way the controller would, at its width; the line
        // wraps onto a second row there, and none of it may be missing.
        let mut client = vt100::Parser::new(24, 40, 0);
        client.process(&replay);
        let drawn: String = client
            .screen()
            .contents()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(drawn.contains(&wide), "the replay lost the line: {drawn:?}");
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn an_oversized_terminal_is_clamped() {
        let mut sessions = Sessions::default();
        // Unclamped, this model alone would ask for 128 GiB.
        let (handle, _, _) = sessions
            .open("acct", 0, 1, shell(), None, u16::MAX, u16::MAX)
            .expect("open");
        let size = |sessions: &Sessions| {
            sessions.sessions[&key("acct", handle)]
                .screen
                .screen()
                .size()
        };
        assert_eq!(size(&sessions), (MAX_ROWS, MAX_COLS));
        sessions.resize("acct", handle, 100, 40).expect("resize");
        assert_eq!(size(&sessions), (40, 100));
        sessions
            .resize("acct", handle, u16::MAX, 0)
            .expect("resize");
        assert_eq!(size(&sessions), (1, MAX_COLS));
        sessions
            .open("acct", handle, 1, shell(), None, 0, u16::MAX)
            .expect("reattach");
        assert_eq!(size(&sessions), (MAX_ROWS, 1));
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
