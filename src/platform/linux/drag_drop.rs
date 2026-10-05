//! Native file drag and drop. Windows drives OLE drags for files moving between
//! the two desktops; Linux has no counterpart yet (an XDND or portal-based one
//! would go here), so every session refuses to start with a reason the peer
//! sees, and nothing below can be constructed.
use anyhow::{Result, bail};
use std::sync::Arc;

pub(crate) mod portal;
pub(crate) mod send_target;

const UNSUPPORTED: &str = "Linux 暂不支持拖放文件";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub x: i32,
    pub y: i32,
}
#[allow(dead_code, reason = "Shared code matches on events Linux never emits.")]
pub(crate) enum Event {
    Feedback(u32),
    Released,
    Finished {
        accepted: bool,
        effect: u32,
        error: Option<String>,
    },
}
pub(crate) enum Session {}
impl Session {
    pub fn start<O>(
        _point: Position,
        _allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        _object: impl FnOnce() -> Result<O> + Send + 'static,
        _notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub fn physical<O>(
        _allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        _object: impl FnOnce() -> Result<O> + Send + 'static,
        _notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub fn position(&self, _point: Position) {
        match *self {}
    }
    pub fn commit(&self, _point: Position) -> Result<()> {
        match *self {}
    }
    pub fn cancel(&self) {
        match *self {}
    }
}
