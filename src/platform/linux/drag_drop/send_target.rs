//! The small window files are dropped on to send them to the controller.
use super::UNSUPPORTED;
use anyhow::{Result, bail};
use std::{path::PathBuf, sync::mpsc};

#[derive(Clone, Default, PartialEq)]
pub(crate) struct Model {
    pub epoch: u64,
    pub revision: u64,
    pub acknowledged: u64,
    pub enabled: bool,
    pub can_send: bool,
    pub status: Option<String>,
    pub items: Vec<Row>,
}
#[derive(Clone, PartialEq)]
pub(crate) struct Row {
    pub id: u64,
    pub name: String,
}
pub(crate) struct Action {
    pub epoch: u64,
    pub revision: u64,
    pub sequence: u64,
    pub kind: ActionKind,
}
#[allow(
    dead_code,
    reason = "Shared code matches on actions Linux never sends."
)]
pub(crate) enum ActionKind {
    Add(Vec<PathBuf>),
    Remove(u64),
    Clear,
    Send,
}
pub(crate) enum SendTarget {}
impl SendTarget {
    pub fn start(_send: mpsc::SyncSender<Action>) -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub fn update(&self, _model: Model) {
        match *self {}
    }
}
