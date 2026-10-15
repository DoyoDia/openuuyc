//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    audio_thread, capture, cursor_shape, decoder, device_profile, display, display_hdr, drag_drop,
    encoder, file_locations, graphics, host_service, input::deadline, loopback, notifications,
    power_request, surface, swapchain, system_power, transfer, video_shader, virtual_audio, wol,
};
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{
    audio_thread, capture, cursor_shape, deadline, decoder, device_profile, display, display_hdr,
    drag_drop, encoder, file_locations, graphics, host_service, loopback, notifications,
    power_request, surface, system_power, transfer, virtual_audio, wol,
};
/// One output as the compositor lays it out, in the global space it resolves
/// an absolute pointer position in. X11 and Windows have no such thing: their
/// display rectangles are already the space a pointer is named in.
#[derive(Clone, Debug)]
pub(crate) struct CompositorOutput {
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

pub(crate) mod paths;
pub(crate) mod wallpaper;
