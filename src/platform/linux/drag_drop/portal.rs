//! The handoff target that captures a drag leaving the remote desktop.
use super::{Position, UNSUPPORTED};
use anyhow::{Result, bail};
use std::{path::PathBuf, sync::Arc};

#[allow(dead_code, reason = "Shared code matches on events Linux never emits.")]
pub(crate) enum Event {
    Unavailable,
    Captured(Vec<PathBuf>),
    Released,
    Failed(String),
}
pub(crate) enum Portal {}
impl Portal {
    pub fn start(
        _point: Position,
        _allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        _notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        bail!(UNSUPPORTED)
    }
}
