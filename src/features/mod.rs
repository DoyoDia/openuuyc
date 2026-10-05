//! Features ownership and module boundaries.

pub(crate) mod clipboard;
// Native drag-and-drop runs only on Windows: the Linux platform drag
// session and file offers are uninhabited, so the shared engine is unreachable.
#[cfg_attr(
    not(windows),
    allow(dead_code, unused_variables, unused_assignments, unreachable_code)
)]
pub(crate) mod drag_drop;
pub(crate) mod file_transfer;
pub(crate) mod host;
pub(crate) mod network_control;
pub(crate) mod port_mapping;
pub(crate) mod remote_cursor;
pub(crate) mod remote_input;
pub(crate) mod remote_upgrade;
pub mod stream_control;
pub(crate) mod viewing_settings;
