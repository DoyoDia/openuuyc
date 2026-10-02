// Copyright (c) 2018-2022, The rav1e contributors. BSD-2-Clause; see LICENSE/PATENTS.
use crate::encoder::api::internal::InterConfig;
use crate::encoder::context::FrameBlocks;
use crate::encoder::encoder::{FrameInvariants, FrameState};
use crate::encoder::me::estimate_tile_motion;
use crate::encoder::Pixel;
use rayon::iter::*;

#[profiling::function]
pub(crate) fn compute_motion_vectors<T: Pixel>(
  fi: &mut FrameInvariants<T>, fs: &mut FrameState<T>, inter_cfg: &InterConfig,
) {
  let mut blocks = FrameBlocks::new(fi.w_in_b, fi.h_in_b);
  fi.sequence
    .tiling
    .tile_iter_mut(fs, &mut blocks)
    .collect::<Vec<_>>()
    .into_par_iter()
    .for_each(|mut ctx| {
      let ts = &mut ctx.ts;
      estimate_tile_motion(fi, ts, inter_cfg);
    });
}
