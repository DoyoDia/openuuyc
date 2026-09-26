//! Linux OS and GPU backends: VA-API decoding, wgpu presentation and display
//! capability queries. No account/session ownership in this layer.
pub(crate) mod decoder;
pub(crate) mod display;
pub(crate) mod display_hdr;
pub(crate) mod graphics;
pub(crate) mod surface;
mod video_layer;
