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
/// Until the wire layout is confirmed this records each frame instead of
/// acting on it, so a real client's traffic can be read back and the format
/// pinned down. It never writes to the channel, which keeps an unverified
/// encoder from corrupting the session.
pub(crate) fn bind(channel: &Arc<RTCDataChannel>, cancel: CancellationToken) {
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
            .take(64)
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(
            frame = count,
            bytes = message.data.len(),
            type_byte = ?kind,
            ?parsed,
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
