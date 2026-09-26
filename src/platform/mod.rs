//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    capture, decoder, display, display_hdr, encoder, graphics, surface, swapchain, transfer,
    video_shader,
};
#[cfg(target_os = "linux")]
pub(crate) mod linux;
// The client path only; the host role's capture, encoding and display drivers
// have no Linux implementation and are not built there.
#[cfg(target_os = "linux")]
pub(crate) use linux::{decoder, display, display_hdr, graphics, surface};
pub(crate) mod paths;
