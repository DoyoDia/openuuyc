//! Linux OS and GPU backends: VA-API decoding, wgpu presentation, X11 capture
//! and display topology, software encoding for the host role. No
//! account/session ownership in this layer.
pub(crate) mod capture;
pub(crate) mod cuda;
pub(crate) mod cursor_shape;
pub(crate) mod decoder;
pub(crate) mod display;
pub(crate) mod display_hdr;
pub(crate) mod encoder;
pub(crate) mod graphics;
pub(crate) mod host_service;
pub(crate) mod input;
pub(crate) mod nvenc;
pub(crate) mod surface;
pub(crate) mod transfer;
mod video_layer;
