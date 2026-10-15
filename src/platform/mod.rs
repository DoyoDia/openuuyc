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

impl Rotation {
    /// A position on the screen as a fraction of it, to the same position as a
    /// fraction of the touchscreen's own, untransformed panel.
    ///
    /// The compositor does the opposite: it reads a touch as a fraction of the
    /// panel and applies the output's transform to it, in the output's
    /// rectangle (smithay's `transform_point_in`, which is what a compositor
    /// scales a touch device's axes with). Undoing that is this inverse, in
    /// normalized coordinates: both spaces are the unit square, so the axis
    /// lengths, the output's scale and its physical size never enter.
    pub(crate) fn panel(self, x: f32, y: f32) -> (f32, f32) {
        match self {
            Self::Normal => (x, y),
            Self::Rotate90 => (y, 1.0 - x),
            Self::Rotate180 => (1.0 - x, 1.0 - y),
            Self::Rotate270 => (1.0 - y, x),
            Self::Flipped => (1.0 - x, y),
            Self::Flipped90 => (y, x),
            Self::Flipped180 => (x, 1.0 - y),
            Self::Flipped270 => (1.0 - y, 1.0 - x),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::Rotation;

    /// Where a screen position lands on an untransformed panel: on an output
    /// the compositor has turned 90°, the screen's top-left is the panel's
    /// bottom-left, and so on.
    #[test]
    fn a_rotated_output_maps_screen_corners_to_panel_corners() {
        let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
        assert_eq!(Rotation::Normal.panel(0.25, 0.75), (0.25, 0.75));
        assert_eq!(
            corners.map(|(x, y)| Rotation::Rotate90.panel(x, y)),
            [(0.0, 1.0), (0.0, 0.0), (1.0, 1.0), (1.0, 0.0)]
        );
        assert_eq!(
            corners.map(|(x, y)| Rotation::Rotate180.panel(x, y)),
            [(1.0, 1.0), (0.0, 1.0), (1.0, 0.0), (0.0, 0.0)]
        );
        assert_eq!(
            corners.map(|(x, y)| Rotation::Rotate270.panel(x, y)),
            [(1.0, 0.0), (1.0, 1.0), (0.0, 0.0), (0.0, 1.0)]
        );
    }

    /// The compositor's side of the same mapping: a point as a fraction of the
    /// panel, to a point as a fraction of the screen, which is smithay's
    /// `transform_point_in` with the panel's own size as the area. Written out
    /// here so [`Rotation::panel`] is checked against it rather than against
    /// itself.
    fn compositor(rotation: Rotation, panel: (f32, f32)) -> (f32, f32) {
        let (x, y) = panel;
        match rotation {
            Rotation::Normal => (x, y),
            Rotation::Rotate90 => (1.0 - y, x),
            Rotation::Rotate180 => (1.0 - x, 1.0 - y),
            Rotation::Rotate270 => (y, 1.0 - x),
            Rotation::Flipped => (1.0 - x, y),
            Rotation::Flipped90 => (y, x),
            Rotation::Flipped180 => (x, 1.0 - y),
            Rotation::Flipped270 => (1.0 - y, 1.0 - x),
        }
    }

    /// Every turn, including the flipped ones: where the compositor puts the
    /// point this side names for it has to be the point the controller aimed
    /// at in the first place.
    #[test]
    fn a_panel_position_is_the_inverse_of_what_the_compositor_does() {
        let rotations = [
            Rotation::Normal,
            Rotation::Rotate90,
            Rotation::Rotate180,
            Rotation::Rotate270,
            Rotation::Flipped,
            Rotation::Flipped90,
            Rotation::Flipped180,
            Rotation::Flipped270,
        ];
        for rotation in rotations {
            for (x, y) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.25, 0.6)] {
                let panel = rotation.panel(x, y);
                let (sx, sy) = compositor(rotation, panel);
                assert!(
                    (sx - x).abs() < 1e-6 && (sy - y).abs() < 1e-6,
                    "{rotation:?}: 屏幕 ({x}, {y}) 报成面板 {panel:?}，合成器又算回 ({sx}, {sy})"
                );
            }
        }
    }
}

pub(crate) mod paths;
pub(crate) mod wallpaper;
