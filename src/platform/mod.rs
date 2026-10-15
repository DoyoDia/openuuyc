//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
use serde::{Deserialize, Serialize};
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompositorOutput {
    /// The connector name (`DP-1`), which is also the name XWayland gives the
    /// output. Missing where the compositor states none.
    pub name: Option<String>,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
}

/// How far the compositor has turned the panel, as `wl_output.transform`
/// states it. Only the touch path needs it: a touchscreen reports its own
/// panel's coordinates and the compositor rotates them into the screen, while
/// every other device names a position that is already a screen position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Rotation {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

/// Every output the compositor lays out, named or not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompositorLayout {
    pub outputs: Vec<CompositorOutput>,
}
impl CompositorLayout {
    /// The output a screen of this name is, in the compositor's space.
    pub fn output(&self, name: &str) -> Option<&CompositorOutput> {
        self.outputs
            .iter()
            .find(|output| output.name.as_deref() == Some(name))
    }
}

pub(crate) mod paths;
pub(crate) mod wallpaper;
