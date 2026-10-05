// Copyright (c) 2017-2022, The rav1e contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.

//! Rust and SIMD AV1 encoding for immediate-output desktop streaming.
//! Supports 4:2:0/4:4:4 at 8/10 bits, live rate changes and cancellation.
//! See the crate README for scope and the original rav1e attribution.

#![allow(missing_abi)]
#![allow(unused_unsafe)]


pub use crate::encoder::api::color;
pub use crate::encoder::api::{
  Config, Context, EncoderConfig, EncoderStatus, InvalidConfig, Packet,
};
use crate::encoder::encoder::*;
pub use crate::encoder::frame::Frame;
pub use crate::encoder::util::{CastFromPrimitive, Pixel, PixelType};

#[macro_use]
mod transform;
#[macro_use]
mod cpu_features;

mod activity;
pub(crate) mod asm;
mod dist;
mod ec;
mod partition;
mod predict;
mod quantize;
mod rdo;
mod rdo_tables;
#[macro_use]
mod util;
mod cdef;
#[doc(hidden)]
pub mod context;
mod deblock;
mod encoder;
mod entropymode;
mod levels;
mod lrf;
mod mc;
mod me;
mod rate;
mod recon_intra;
mod scan_order;
mod segmentation;
#[doc(hidden)]
pub mod tiling;
mod token_cdfs;

/// Concrete low-delay interface; keeps codec monomorphization in this crate.
pub mod streaming;

mod api;
mod frame;
mod header;

/// Commonly used types and traits.
pub mod prelude {
  pub use crate::encoder::api::*;
  pub use crate::encoder::encoder::{Sequence, Tune};
  pub use crate::encoder::frame::{
    Frame, FrameParameters, FrameTypeOverride, Plane, PlaneConfig,
  };
  pub use crate::encoder::partition::BlockSize;
  pub use crate::encoder::predict::PredictionMode;
  pub use crate::encoder::transform::TxType;
  pub use crate::encoder::util::{CastFromPrimitive, Pixel, PixelType};
}

/// Basic data structures
pub mod data {
  pub use crate::encoder::api::{
    ChromaticityPoint, EncoderStatus, FrameType, Packet, Rational,
  };
  pub use crate::encoder::frame::{Frame, FrameParameters};
  pub use crate::encoder::util::{CastFromPrimitive, Pixel, PixelType};
}

/// Encoder configuration and settings
pub mod config {

  pub use crate::encoder::api::{
    Config, EncoderConfig, InvalidConfig, PredictionModesSetting,
    SpeedSettings,
  };
  pub use crate::encoder::cpu_features::CpuFeatureLevel;
}
