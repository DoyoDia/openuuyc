//! Compiles and runs the host terminal's POSIX pty on macOS.
//!
//! The crate does not build for macOS end to end yet — there is no capture,
//! encoder or input backend — so `cargo test` cannot reach
//! `src/platform/posix/terminal.rs` there at all. This scratch crate pulls the
//! module in on its own, which is enough to compile the macOS arm and exercise
//! a real pty:
//!
//! ```sh
//! cargo test --manifest-path devtools/posix-pty-check/Cargo.toml
//! ```
//!
//! It is the only coverage macOS has, so keep it working when the shared pty
//! changes. The module is deliberately dependency-light (`anyhow` and `libc`)
//! precisely so this stays possible.

#[path = "../../../src/platform/posix/terminal.rs"]
pub mod terminal;
