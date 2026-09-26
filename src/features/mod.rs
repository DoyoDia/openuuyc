//! Features ownership and module boundaries.

pub(crate) mod clipboard;
pub(crate) mod file_transfer;
// The host role: capture, GPU encoding and virtual displays have no Linux
// backend, so the role is not built there.
#[cfg(windows)]
pub(crate) mod host;
pub(crate) mod network_control;
pub(crate) mod port_mapping;
pub(crate) mod remote_cursor;
pub(crate) mod remote_input;
pub(crate) mod remote_upgrade;
pub mod stream_control;
pub(crate) mod viewing_settings;
