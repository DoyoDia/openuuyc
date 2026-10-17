//! Host terminal on Windows: not implemented yet.
//!
//! UU's controllers can open a shell on the controlled machine (connection
//! `kind = 8`); Linux runs it on a pty (`platform/linux/terminal.rs`). Windows
//! has no backend yet, so this module states that honestly instead of
//! pretending: [`unavailable`] makes the host refuse terminal connections and
//! the environment check and hides the permission switch, and [`Session`] is
//! uninhabited, so no session can ever exist.
//!
//! This is a stopgap. The real backend is ConPTY (`CreatePseudoConsole`) with
//! the same `Session` shape, at which point [`unavailable`] returns `None`.

use anyhow::{Result, anyhow};

const MESSAGE: &str = "Windows 版暂不支持终端";

/// Why this machine cannot serve a terminal; Windows always has a reason.
///
/// Linux adds a runtime check (running as root) on top of this.
pub(crate) fn unavailable() -> Option<&'static str> {
    Some(MESSAGE)
}

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

    pub(crate) fn terminate(self) {
        match self {}
    }
}
