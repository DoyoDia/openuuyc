//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    capture, cursor_shape, decoder, display, display_hdr, encoder, graphics, host_service,
    loopback, surface, swapchain, transfer, video_shader, virtual_audio,
};
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{
    capture, cursor_shape, decoder, display, display_hdr, encoder, graphics, host_service,
    loopback, surface, transfer, virtual_audio,
};
pub(crate) mod paths;
