//! Host terminal on Windows: not implemented yet.
//!
//! UU's controllers can open a shell on the controlled machine (connection
//! `kind = 8`); Linux runs it on a pty (`platform/linux/terminal.rs`). Windows
//! has no backend yet: [`Session`] is uninhabited, so no session can ever
//! exist, and opening one fails with a message the controller can show.
//!
//! This is a stopgap. The real backend is ConPTY (`CreatePseudoConsole`) with
//! the same `Session` shape.

use anyhow::{Result, anyhow};

const MESSAGE: &str = "Windows 版暂不支持终端";

/// A shell session. Windows cannot create one yet.
pub(crate) enum Session {}

impl Session {
    pub(crate) fn open(_shell: &str, _cols: u16, _rows: u16) -> Result<Self> {
        Err(anyhow!(MESSAGE))
    }

    pub(crate) fn shell(&self) -> &str {
        match *self {}
    }

    pub(crate) fn read(&self, _buffer: &mut [u8]) -> Result<Option<usize>> {
        match *self {}
    }

    pub(crate) fn write(&self, _data: &[u8]) -> Result<usize> {
        match *self {}
    }

    pub(crate) fn resize(&self, _cols: u16, _rows: u16) -> Result<()> {
        match *self {}
    }

    pub(crate) fn exited(&mut self) -> bool {
        match *self {}
    }
}
