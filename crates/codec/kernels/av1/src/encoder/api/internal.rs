// Copyright (c) 2018-2022, The rav1e contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.
#![deny(missing_docs)]

use crate::encoder::api::lookahead::*;
use crate::encoder::api::{EncoderConfig, EncoderStatus, FrameType, Opaque, Packet, T35};
use crate::encoder::color::ChromaSampling::Cs400;
use crate::encoder::encoder::*;
use crate::encoder::frame::*;
use crate::encoder::partition::*;
use crate::encoder::rate::{RCState, FRAME_NSUBTYPES, FRAME_SUBTYPE_I, FRAME_SUBTYPE_P, FRAME_SUBTYPE_SEF};
use crate::encoder::util::Pixel;

use std::cmp;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Causal reference selection. Each input produces one visible output.
#[derive(Debug, Clone, Copy)]
pub struct InterConfig {
  pub(crate) multiref: bool,
}
impl InterConfig {
  pub(crate) fn new(config: &EncoderConfig) -> Self {
    Self {
      multiref: config.speed_settings.multiref,
    }
  }
  pub(crate) const fn get_idx_in_group_output(&self, _frame: u64) -> u64 {
    0
  }
  pub(crate) const fn get_order_hint(&self, frame: u64, _index: u64) -> u32 {
    frame as u32
  }
  pub(crate) const fn get_level(&self, _index: u64) -> u64 {
    0
  }
  pub(crate) const fn get_slot_idx(&self, _level: u64, order_hint: u32) -> u32 {
    order_hint & 3
  }
  pub(crate) const fn get_show_frame(&self, _index: u64) -> bool {
    true
  }
  pub(crate) const fn get_show_existing_frame(&self, _index: u64) -> bool {
    false
  }
  pub(crate) const fn get_input_frameno(&self, frame: u64, start: u64) -> u64 {
    start + frame
  }
  pub(crate) fn allowed_ref_frames(&self) -> &[RefType] {
    use crate::encoder::partition::RefType::*;
    if self.multiref {
      &[LAST_FRAME, LAST2_FRAME, LAST3_FRAME, GOLDEN_FRAME]
    } else {
      &[LAST_FRAME]
    }
  }
}

// Thin wrapper for frame-related data
// that gets cached and reused throughout the life of a frame.
#[derive(Clone)]
pub(crate) struct FrameData<T: Pixel> {
  pub(crate) fi: FrameInvariants<T>,
  pub(crate) fs: FrameState<T>,
}

impl<T: Pixel> FrameData<T> {
  pub(crate) fn new(fi: FrameInvariants<T>, frame: Arc<Frame<T>>) -> Self {
    let fs = FrameState::new_with_frame(&fi, frame);
    FrameData { fi, fs }
  }
}

type FrameQueue<T> = BTreeMap<u64, Option<Arc<Frame<T>>>>;
type FrameDataQueue<T> = BTreeMap<u64, Option<FrameData<T>>>;

// the fields pub(super) are accessed only by the tests
pub(crate) struct ContextInner<T: Pixel> {
  pub(crate) frame_count: u64,
  pub(crate) limit: Option<u64>,
  pub(crate) output_frameno: u64,
  pub(super) inter_cfg: InterConfig,
  pub(super) frames_processed: u64,
  /// Maps *`input_frameno`* to frames
  pub(super) frame_q: FrameQueue<T>,
  /// Maps *`output_frameno`* to frame data
  pub(super) frame_data: FrameDataQueue<T>,
  /// A list of the `input_frameno` for keyframes in this encode.
  /// Needed so that we don't need to keep all of the `frame_invariants` in
  ///  memory for the whole life of the encode.
  // TODO: Is this needed at all?
  keyframes: BTreeSet<u64>,
  // TODO: Is this needed at all?
  keyframes_forced: BTreeSet<u64>,
  /// A storage space for reordered frames.
  packet_data: Vec<u8>,
  /// Maps `output_frameno` to `gop_output_frameno_start`.
  gop_output_frameno_start: BTreeMap<u64, u64>,
  /// Maps `output_frameno` to `gop_input_frameno_start`.
  pub(crate) gop_input_frameno_start: BTreeMap<u64, u64>,

  pub(crate) config: Arc<EncoderConfig>,
  pub(crate) cancellation: Option<Arc<std::sync::atomic::AtomicBool>>,
  seq: Arc<Sequence>,
  pub(crate) rc_state: RCState,
  maybe_prev_log_base_q: Option<i64>,
  /// The next `input_frameno` to be processed by lookahead.
  next_lookahead_frame: u64,
  /// The next `output_frameno` to be computed by lookahead.
  next_lookahead_output_frameno: u64,
  /// Optional opaque to be sent back to the user
  opaque_q: BTreeMap<u64, Opaque>,
  /// Optional T35 metadata per frame
  t35_q: BTreeMap<u64, Box<[T35]>>,
}

impl<T: Pixel> ContextInner<T> {
  pub fn new(enc: &EncoderConfig) -> Self {
    // initialize with temporal delimiter
    let packet_data = TEMPORAL_DELIMITER.to_vec();
    let mut keyframes = BTreeSet::new();
    keyframes.insert(0);

    let maybe_ac_qi_max = if enc.quantizer < 255 {
      Some(enc.quantizer as u8)
    } else {
      None
    };

    let seq = Arc::new(Sequence::new(enc));
    let inter_cfg = InterConfig::new(enc);
    ContextInner {
      frame_count: 0,
      limit: None,
      inter_cfg,
      output_frameno: 0,
      frames_processed: 0,
      frame_q: BTreeMap::new(),
      frame_data: BTreeMap::new(),
      keyframes,
      keyframes_forced: BTreeSet::new(),
      packet_data,
      gop_output_frameno_start: BTreeMap::new(),
      gop_input_frameno_start: BTreeMap::new(),

      config: Arc::new(enc.clone()),
      cancellation: None,
      seq,
      rc_state: RCState::new(
        enc.width as i32,
        enc.height as i32,
        enc.time_base.den as i64,
        enc.time_base.num as i64,
        enc.bitrate,
        maybe_ac_qi_max,
        enc.min_quantizer,
        enc.max_key_frame_interval as i32,
        enc.reservoir_frame_delay,
      ),
      maybe_prev_log_base_q: None,
      next_lookahead_frame: 1,
      next_lookahead_output_frameno: 0,
      opaque_q: BTreeMap::new(),
      t35_q: BTreeMap::new(),
    }
  }

  #[profiling::function]
  pub fn send_frame(
    &mut self,
    mut frame: Option<Arc<Frame<T>>>,
    params: Option<FrameParameters>,
  ) -> Result<(), EncoderStatus> {
    if let Some(ref mut frame) = frame {
      use crate::encoder::api::color::ChromaSampling;
      let EncoderConfig {
        width,
        height,
        chroma_sampling,
        ..
      } = *self.config;
      let planes = if chroma_sampling == ChromaSampling::Cs400 {
        1
      } else {
        3
      };
      // Try to add padding
      if let Some(ref mut frame) = Arc::get_mut(frame) {
        for plane in frame.planes[..planes].iter_mut() {
          plane.pad(width, height);
        }
      }
      // Enforce that padding is added
      for (p, plane) in frame.planes[..planes].iter().enumerate() {
        assert!(
          plane.probe_padding(width, height),
          "Plane {p} was not padded before passing Frame to send_frame()."
        );
      }
    }

    let input_frameno = self.frame_count;
    let is_flushing = frame.is_none();
    if !is_flushing {
      self.frame_count += 1;
    }
    self.frame_q.insert(input_frameno, frame);

    if let Some(params) = params {
      if params.frame_type_override == FrameTypeOverride::Key {
        self.keyframes_forced.insert(input_frameno);
      }
      if let Some(op) = params.opaque {
        self.opaque_q.insert(input_frameno, op);
      }
      self.t35_q.insert(input_frameno, params.t35_metadata);
    }

    if !is_flushing {
      let last_key = self
        .keyframes
        .range(..=input_frameno)
        .next_back()
        .copied()
        .unwrap_or(0);
      if input_frameno == 0
        || self.keyframes_forced.contains(&input_frameno)
        || input_frameno.saturating_sub(last_key) >= self.config.max_key_frame_interval
      {
        self.keyframes.insert(input_frameno);
      }
      self.next_lookahead_frame = input_frameno + 1;
    }

    self.compute_frame_invariants();

    Ok(())
  }

  /// Indicates whether more frames need to be read into the frame queue
  /// in order for frame queue lookahead to be full.
  fn needs_more_frame_q_lookahead(&self, input_frameno: u64) -> bool {
    !matches!(self.frame_q.get(&input_frameno), Some(Some(_)))
  }

  /// Indicates whether more frames need to be processed into `FrameInvariants`
  /// in order for FI lookahead to be full.
  pub fn needs_more_fi_lookahead(&self) -> bool {
    let ready_frames = self.get_rdo_lookahead_frames().count();
    ready_frames < self.config.speed_settings.rdo_lookahead_frames + 1
      && self.needs_more_frames(self.next_lookahead_frame)
  }

  pub fn needs_more_frames(&self, frame_count: u64) -> bool {
    self.limit.map(|limit| frame_count < limit).unwrap_or(true)
  }

  fn get_rdo_lookahead_frames(&self) -> impl Iterator<Item = (&u64, &FrameData<T>)> {
    self
      .frame_data
      .iter()
      .skip_while(move |(&output_frameno, _)| output_frameno < self.output_frameno)
      .filter_map(|(fno, data)| data.as_ref().map(|data| (fno, data)))
      .filter(|(_, data)| !data.fi.is_show_existing_frame())
      .take(self.config.speed_settings.rdo_lookahead_frames + 1)
  }

  fn next_keyframe_input_frameno(&self, gop_input_frameno_start: u64, ignore_limit: bool) -> u64 {
    let next_detected = self
      .keyframes
      .iter()
      .find(|&&input_frameno| input_frameno > gop_input_frameno_start)
      .cloned();
    let mut next_limit = gop_input_frameno_start + self.config.max_key_frame_interval;
    if !ignore_limit {
      if let Some(limit) = self.limit {
        next_limit = next_limit.min(limit);
      }
    }
    if next_detected.is_none() {
      return next_limit;
    }
    cmp::min(next_detected.unwrap(), next_limit)
  }

  fn set_frame_properties(&mut self, output_frameno: u64) -> Result<(), EncoderStatus> {
    let fi = self.build_frame_properties(output_frameno)?;

    self.frame_data.insert(
      output_frameno,
      fi.map(|mut fi| {
        fi.cancellation = self.cancellation.clone();
        let frame = self
          .frame_q
          .get(&fi.input_frameno)
          .as_ref()
          .unwrap()
          .as_ref()
          .unwrap();
        FrameData::new(fi, frame.clone())
      }),
    );

    Ok(())
  }

  fn build_frame_properties(
    &mut self,
    output_frameno: u64,
  ) -> Result<Option<FrameInvariants<T>>, EncoderStatus> {
    let (prev_gop_output_frameno_start, prev_gop_input_frameno_start) = if output_frameno == 0 {
      (0, 0)
    } else {
      (
        self.gop_output_frameno_start[&(output_frameno - 1)],
        self.gop_input_frameno_start[&(output_frameno - 1)],
      )
    };

    self
      .gop_output_frameno_start
      .insert(output_frameno, prev_gop_output_frameno_start);
    self
      .gop_input_frameno_start
      .insert(output_frameno, prev_gop_input_frameno_start);

    let output_frameno_in_gop = output_frameno - self.gop_output_frameno_start[&output_frameno];
    let mut input_frameno = self.inter_cfg.get_input_frameno(
      output_frameno_in_gop,
      self.gop_input_frameno_start[&output_frameno],
    );

    if self.needs_more_frame_q_lookahead(input_frameno) {
      return Err(EncoderStatus::NeedMoreData);
    }

    let t35_metadata = if let Some(t35) = self.t35_q.remove(&input_frameno) {
      t35
    } else {
      Box::new([])
    };

    if output_frameno_in_gop > 0 {
      let next_keyframe_input_frameno =
        self.next_keyframe_input_frameno(self.gop_input_frameno_start[&output_frameno], false);
      if input_frameno >= next_keyframe_input_frameno {
        input_frameno = next_keyframe_input_frameno;

        // If we'll return early, do it before modifying the state.
        match self.frame_q.get(&input_frameno) {
          Some(Some(_)) => {}
          _ => {
            return Err(EncoderStatus::NeedMoreData);
          }
        }

        *self
          .gop_output_frameno_start
          .get_mut(&output_frameno)
          .unwrap() = output_frameno;
        *self
          .gop_input_frameno_start
          .get_mut(&output_frameno)
          .unwrap() = next_keyframe_input_frameno;
      }
    }

    match self.frame_q.get(&input_frameno) {
      Some(Some(_)) => {}
      _ => {
        return Err(EncoderStatus::NeedMoreData);
      }
    }

    // Now that we know the input_frameno, look up the correct frame type
    let frame_type = if self.keyframes.contains(&input_frameno) {
      FrameType::KEY
    } else {
      FrameType::INTER
    };
    if frame_type == FrameType::KEY {
      *self
        .gop_output_frameno_start
        .get_mut(&output_frameno)
        .unwrap() = output_frameno;
      *self
        .gop_input_frameno_start
        .get_mut(&output_frameno)
        .unwrap() = input_frameno;
    }

    let output_frameno_in_gop = output_frameno - self.gop_output_frameno_start[&output_frameno];
    if output_frameno_in_gop == 0 {
      let fi = FrameInvariants::new_key_frame(
        self.config.clone(),
        self.seq.clone(),
        self.gop_input_frameno_start[&output_frameno],
        t35_metadata,
      );
      Ok(Some(fi))
    } else {
      let next_keyframe_input_frameno =
        self.next_keyframe_input_frameno(self.gop_input_frameno_start[&output_frameno], false);
      let fi = FrameInvariants::new_inter_frame(
        self.get_previous_coded_fi(output_frameno),
        &self.inter_cfg,
        self.gop_input_frameno_start[&output_frameno],
        output_frameno_in_gop,
        next_keyframe_input_frameno,
        self.config.error_resilient,
        t35_metadata,
      );
      assert!(fi.is_some());
      Ok(fi)
    }
  }

  fn get_previous_coded_fi(&self, output_frameno: u64) -> &FrameInvariants<T> {
    let res = self
      .frame_data
      .iter()
      .filter(|(fno, _)| **fno < output_frameno)
      .rfind(|(_, fd)| {
        fd.as_ref()
          .map(|fd| !fd.fi.is_show_existing_frame())
          .unwrap_or(false)
      })
      .unwrap();
    &res.1.as_ref().unwrap().fi
  }

  pub(crate) fn done_processing(&self) -> bool {
    self
      .limit
      .map(|limit| self.frames_processed == limit)
      .unwrap_or(false)
  }

  /// Computes lookahead motion vectors and fills in `lookahead_mvs`,
  /// `rec_buffer` and `lookahead_rec_buffer` on the `FrameInvariants`. This
  /// function must be called after every new `FrameInvariants` is initially
  /// computed.
  #[profiling::function]
  fn compute_lookahead_motion_vectors(&mut self, output_frameno: u64) {
    let frame_data = self.frame_data.get(&output_frameno).unwrap();

    // We're only interested in valid frames which are not show-existing-frame.
    // Those two don't modify the rec_buffer so there's no need to do anything
    // special about it either, it'll propagate on its own.
    if frame_data
      .as_ref()
      .map(|fd| fd.fi.is_show_existing_frame())
      .unwrap_or(true)
    {
      return;
    }

    let qps = {
      let fti = frame_data.as_ref().unwrap().fi.get_frame_subtype();
      self
        .rc_state
        .select_qi(self, output_frameno, fti, self.maybe_prev_log_base_q, 0)
    };

    let frame_data = self
      .frame_data
      .get_mut(&output_frameno)
      .unwrap()
      .as_mut()
      .unwrap();
    let fs = &mut frame_data.fs;
    let fi = &mut frame_data.fi;
    let coded_data = fi.coded_frame_data.as_mut().unwrap();

    // Do not modify the next output frame's FrameInvariants.
    if self.output_frameno == output_frameno
      && (self.config.speed_settings.rdo_lookahead_frames != 0 || output_frameno == 0)
    {
      // We do want to propagate the lookahead_rec_buffer though.
      let rfs = Arc::new(ReferenceFrame {
        order_hint: fi.order_hint,
        width: fi.width as u32,
        height: fi.height as u32,
        render_width: fi.render_width,
        render_height: fi.render_height,
        // Use the original frame contents.
        frame: fs.input.clone(),
        input_hres: fs.input_hres.clone(),
        input_qres: fs.input_qres.clone(),
        cdfs: fs.cdfs,
        frame_me_stats: fs.frame_me_stats.clone(),
        output_frameno,
        segmentation: fs.segmentation,
      });
      for i in 0..REF_FRAMES {
        if (fi.refresh_frame_flags & (1 << i)) != 0 {
          coded_data.lookahead_rec_buffer.frames[i] = Some(Arc::clone(&rfs));
          coded_data.lookahead_rec_buffer.deblock[i] = fs.deblock;
        }
      }

      return;
    }

    // Our lookahead_rec_buffer should be filled with correct original frame
    // data from the previous frames. Copy it into rec_buffer because that's
    // what the MV search uses. During the actual encoding rec_buffer is
    // overwritten with its correct values anyway.
    // With no queued future FI, the previous coded reference is already final.
    // Preserve it while deriving causal search seeds from the source pictures.
    let saved_rec = if self.config.speed_settings.rdo_lookahead_frames == 0 {
      Some(fi.rec_buffer.clone())
    } else {
      None
    };
    fi.rec_buffer = coded_data.lookahead_rec_buffer.clone();

    // Estimate lambda with rate-control dry-run
    fi.set_quantizers(&qps);

    // TODO: as in the encoding code, key frames will have no references.
    // However, for block importance purposes we want key frames to act as
    // P-frames in this instance.
    //
    // Compute the motion vectors.
    compute_motion_vectors(fi, fs, &self.inter_cfg);

    let coded_data = fi.coded_frame_data.as_mut().unwrap();

    // Set lookahead_rec_buffer on this FrameInvariants for future
    // FrameInvariants to pick it up.
    let rfs = Arc::new(ReferenceFrame {
      order_hint: fi.order_hint,
      width: fi.width as u32,
      height: fi.height as u32,
      render_width: fi.render_width,
      render_height: fi.render_height,
      // Use the original frame contents.
      frame: fs.input.clone(),
      input_hres: fs.input_hres.clone(),
      input_qres: fs.input_qres.clone(),
      cdfs: fs.cdfs,
      frame_me_stats: fs.frame_me_stats.clone(),
      output_frameno,
      segmentation: fs.segmentation,
    });
    for i in 0..REF_FRAMES {
      if (fi.refresh_frame_flags & (1 << i)) != 0 {
        coded_data.lookahead_rec_buffer.frames[i] = Some(Arc::clone(&rfs));
        coded_data.lookahead_rec_buffer.deblock[i] = fs.deblock;
      }
    }
    if let Some(saved_rec) = saved_rec {
      fi.rec_buffer = saved_rec;
    }
  }

  #[profiling::function]
  pub fn compute_frame_invariants(&mut self) {
    while self
      .set_frame_properties(self.next_lookahead_output_frameno)
      .is_ok()
    {
      self.compute_lookahead_motion_vectors(self.next_lookahead_output_frameno);

      self.next_lookahead_output_frameno += 1;
    }
  }

  pub(crate) fn encode_packet(
    &mut self,
    cur_output_frameno: u64,
  ) -> Result<Packet<T>, EncoderStatus> {
    if self
      .frame_data
      .get(&cur_output_frameno)
      .unwrap()
      .as_ref()
      .unwrap()
      .fi
      .is_show_existing_frame()
    {
      if !self.rc_state.ready() {
        return Err(EncoderStatus::NotReady);
      }

      self.encode_show_existing_packet(cur_output_frameno)
    } else if let Some(Some(_)) = self.frame_q.get(
      &self
        .frame_data
        .get(&cur_output_frameno)
        .unwrap()
        .as_ref()
        .unwrap()
        .fi
        .input_frameno,
    ) {
      if !self.rc_state.ready() {
        return Err(EncoderStatus::NotReady);
      }

      self.encode_normal_packet(cur_output_frameno)
    } else {
      Err(EncoderStatus::NeedMoreData)
    }
  }

  #[profiling::function]
  pub fn encode_show_existing_packet(
    &mut self,
    cur_output_frameno: u64,
  ) -> Result<Packet<T>, EncoderStatus> {
    let frame_data = self
      .frame_data
      .get_mut(&cur_output_frameno)
      .unwrap()
      .as_mut()
      .unwrap();
    let sef_data = encode_show_existing_frame(&frame_data.fi, &mut frame_data.fs, &self.inter_cfg);
    let bits = (sef_data.len() * 8) as i64;
    self.packet_data.extend(sef_data);
    self.rc_state.update_state(
      bits,
      FRAME_SUBTYPE_SEF,
      frame_data.fi.show_frame,
      0,
      false,
      false,
    );
    let (rec, source) = if frame_data.fi.show_frame {
      (
        Some(frame_data.fs.rec.clone()),
        Some(frame_data.fs.input.clone()),
      )
    } else {
      (None, None)
    };

    self.output_frameno += 1;

    let input_frameno = frame_data.fi.input_frameno;
    let frame_type = frame_data.fi.frame_type;
    let qp = frame_data.fi.base_q_idx;
    self.finalize_packet(rec, source, input_frameno, frame_type, qp)
  }

  #[profiling::function]
  pub fn encode_normal_packet(
    &mut self,
    cur_output_frameno: u64,
  ) -> Result<Packet<T>, EncoderStatus> {
    let mut frame_data = self
      .frame_data
      .remove(&cur_output_frameno)
      .unwrap()
      .unwrap();

    let log_isqrt_mean_scale = 0i64;

    let fti = frame_data.fi.get_frame_subtype();
    let mut qps = self.rc_state.select_qi(
      self,
      cur_output_frameno,
      fti,
      self.maybe_prev_log_base_q,
      log_isqrt_mean_scale,
    );
    frame_data.fi.set_quantizers(&qps);

    let began = (fti == FRAME_SUBTYPE_I && self.rc_state.needs_trial_encode(fti))
      .then(std::time::Instant::now);
    let mut data = encode_frame(&frame_data.fi, &mut frame_data.fs, &self.inter_cfg)
      .ok_or(EncoderStatus::Failure)?;
    if began.is_some_and(|start| {
      self
        .rc_state
        .retry_startup(fti, data.len() * 8, start.elapsed())
    }) {
      if frame_data
        .fi
        .cancellation
        .as_ref()
        .is_some_and(|v| v.load(std::sync::atomic::Ordering::Acquire))
      {
        return Err(EncoderStatus::Failure);
      }
      self.rc_state.update_state(
        (data.len() * 8) as i64,
        fti,
        frame_data.fi.show_frame,
        qps.log_target_q,
        true,
        false,
      );
      let next_qps = self.rc_state.select_qi(
        self,
        cur_output_frameno,
        fti,
        self.maybe_prev_log_base_q,
        log_isqrt_mean_scale,
      );
      // A finer retry cannot save transmission time.
      if next_qps.ac_qi[0] > qps.ac_qi[0] {
        frame_data.fi.set_quantizers(&next_qps);
        let next_fs = FrameState::new_with_frame(&frame_data.fi, frame_data.fs.input.clone());
        let previous_fs = std::mem::replace(&mut frame_data.fs, next_fs);
        let next_data = encode_frame(&frame_data.fi, &mut frame_data.fs, &self.inter_cfg)
          .ok_or(EncoderStatus::Failure)?;
        if next_data.len() < data.len() {
          data = next_data;
          qps = next_qps;
        } else {
          frame_data.fs = previous_fs;
          frame_data.fi.set_quantizers(&qps);
        }
      }
    }
    
    self.maybe_prev_log_base_q = Some(qps.log_base_q);
    // TODO: Add support for dropping frames.
    self.rc_state.update_state(
      (data.len() * 8) as i64,
      fti,
      frame_data.fi.show_frame,
      qps.log_target_q,
      false,
      false,
    );
    self.packet_data.extend(data);

    let planes = if frame_data.fi.sequence.chroma_sampling == Cs400 {
      1
    } else {
      3
    };

    Arc::get_mut(&mut frame_data.fs.rec).unwrap().pad(
      frame_data.fi.width,
      frame_data.fi.height,
      planes,
    );

    let (rec, source) = if frame_data.fi.show_frame {
      (
        Some(frame_data.fs.rec.clone()),
        Some(frame_data.fs.input.clone()),
      )
    } else {
      (None, None)
    };

    update_rec_buffer(cur_output_frameno, &mut frame_data.fi, &frame_data.fs);

    // Copy persistent fields into subsequent FrameInvariants.
    let rec_buffer = frame_data.fi.rec_buffer.clone();
    for subsequent_fi in self
      .frame_data
      .iter_mut()
      .skip_while(|(&output_frameno, _)| output_frameno <= cur_output_frameno)
      // Here we want the next valid non-show-existing-frame inter frame.
      //
      // Copying to show-existing-frame frames isn't actually required
      // for correct encoding, but it's needed for the reconstruction to
      // work correctly.
      .filter_map(|(_, frame_data)| frame_data.as_mut().map(|fd| &mut fd.fi))
      .take_while(|fi| fi.frame_type != FrameType::KEY)
    {
      subsequent_fi.rec_buffer = rec_buffer.clone();
      subsequent_fi.set_ref_frame_sign_bias();

      // Stop after the first non-show-existing-frame.
      if !subsequent_fi.is_show_existing_frame() {
        break;
      }
    }

    self.frame_data.insert(cur_output_frameno, Some(frame_data));
    let frame_data = self
      .frame_data
      .get(&cur_output_frameno)
      .unwrap()
      .as_ref()
      .unwrap();
    let fi = &frame_data.fi;

    self.output_frameno += 1;

    if fi.show_frame {
      let input_frameno = fi.input_frameno;
      let frame_type = fi.frame_type;
      let qp = fi.base_q_idx;
      self.finalize_packet(rec, source, input_frameno, frame_type, qp)
    } else {
      Err(EncoderStatus::Encoded)
    }
  }

  #[profiling::function]
  pub fn receive_packet(&mut self) -> Result<Packet<T>, EncoderStatus> {
    if self.done_processing() {
      return Err(EncoderStatus::LimitReached);
    }

    if self.needs_more_fi_lookahead() {
      return Err(EncoderStatus::NeedMoreData);
    }

    // Find the next output_frameno corresponding to a non-skipped frame.
    self.output_frameno = self
      .frame_data
      .iter()
      .skip_while(|(&output_frameno, _)| output_frameno < self.output_frameno)
      .find(|(_, data)| data.is_some())
      .map(|(&output_frameno, _)| output_frameno)
      .ok_or(EncoderStatus::NeedMoreData)?; // TODO: doesn't play well with the below check?

    let input_frameno = self.frame_data[&self.output_frameno]
      .as_ref()
      .unwrap()
      .fi
      .input_frameno;
    if !self.needs_more_frames(input_frameno) {
      return Err(EncoderStatus::LimitReached);
    }

    let cur_output_frameno = self.output_frameno;

    let mut ret = self.encode_packet(cur_output_frameno);

    if let Ok(ref mut pkt) = ret {
      self.garbage_collect(pkt.input_frameno);
      pkt.opaque = self.opaque_q.remove(&pkt.input_frameno);
    }

    ret
  }

  fn finalize_packet(
    &mut self,
    rec: Option<Arc<Frame<T>>>,
    source: Option<Arc<Frame<T>>>,
    input_frameno: u64,
    frame_type: FrameType,
    qp: u8,
  ) -> Result<Packet<T>, EncoderStatus> {
    let data = self.packet_data.clone();
    self.packet_data.clear();
    if write_temporal_delimiter(&mut self.packet_data).is_err() {
      return Err(EncoderStatus::Failure);
    }

    self.frames_processed += 1;
    Ok(Packet {
      data,
      rec,
      source,
      input_frameno,
      frame_type,
      qp,
      opaque: None,
    })
  }

  #[profiling::function]
  fn garbage_collect(&mut self, cur_input_frameno: u64) {
    if cur_input_frameno == 0 {
      return;
    }
    let frame_q_start = self.frame_q.keys().next().cloned().unwrap_or(0);
    for i in frame_q_start..cur_input_frameno {
      self.frame_q.remove(&i);
    }

    if self.output_frameno < 2 {
      return;
    }
    let fi_start = self.frame_data.keys().next().cloned().unwrap_or(0);
    for i in fi_start..(self.output_frameno - 1) {
      self.frame_data.remove(&i);
      self.gop_output_frameno_start.remove(&i);
      self.gop_input_frameno_start.remove(&i);
    }
  }

  /// Counts the number of output frames of each subtype in the next
  ///  `reservoir_frame_delay` temporal units (needed for rate control).
  /// Returns the number of output frames (excluding SEF frames) and output TUs
  ///  until the last keyframe in the next `reservoir_frame_delay` temporal units,
  ///  or the end of the interval, whichever comes first.
  /// The former is needed because it indicates the number of rate estimates we
  ///  will make.
  /// The latter is needed because it indicates the number of times new bitrate
  ///  is added to the buffer.
  pub(crate) fn guess_frame_subtypes(
    &self,
    nframes: &mut [i32; FRAME_NSUBTYPES + 1],
    reservoir_frame_delay: i32,
  ) -> (i32, i32) {
    for fti in 0..=FRAME_NSUBTYPES {
      nframes[fti] = 0;
    }

    // Two-pass calls this function before receive_packet(), and in particular
    // before the very first send_frame(), when the following maps are empty.
    // In this case, return 0 as the default value.
    let mut prev_keyframe_input_frameno = *self
      .gop_input_frameno_start
      .get(&self.output_frameno)
      .unwrap_or_else(|| {
        assert!(self.output_frameno == 0);
        &0
      });
    let mut prev_keyframe_output_frameno = *self
      .gop_output_frameno_start
      .get(&self.output_frameno)
      .unwrap_or_else(|| {
        assert!(self.output_frameno == 0);
        &0
      });

    let mut prev_keyframe_ntus = 0;
    // Does not include SEF frames.
    let mut prev_keyframe_nframes = 0;
    let mut acc: [i32; FRAME_NSUBTYPES + 1] = [0; FRAME_NSUBTYPES + 1];
    // Updates the frame counts with the accumulated values when we hit a
    //  keyframe.
    fn collect_counts(
      nframes: &mut [i32; FRAME_NSUBTYPES + 1],
      acc: &mut [i32; FRAME_NSUBTYPES + 1],
    ) {
      for fti in 0..=FRAME_NSUBTYPES {
        nframes[fti] += acc[fti];
        acc[fti] = 0;
      }
      acc[FRAME_SUBTYPE_I] += 1;
    }
    let mut output_frameno = self.output_frameno;
    let mut ntus = 0;
    // Does not include SEF frames.
    let mut nframes_total = 0;
    while ntus < reservoir_frame_delay {
      let output_frameno_in_gop = output_frameno - prev_keyframe_output_frameno;
      let is_kf = if let Some(Some(frame_data)) = self.frame_data.get(&output_frameno) {
        if frame_data.fi.frame_type == FrameType::KEY {
          prev_keyframe_input_frameno = frame_data.fi.input_frameno;
          // We do not currently use forward keyframes, so they should always
          //  end the current TU (thus we always increment ntus below).
          debug_assert!(frame_data.fi.show_frame);
          true
        } else {
          false
        }
      } else {
        // It is possible to be invoked for the first time from twopass_out()
        //  before receive_packet() is called, in which case frame_invariants
        //  will not be populated.
        // Force the first frame in each GOP to be a keyframe in that case.
        output_frameno_in_gop == 0
      };
      if is_kf {
        collect_counts(nframes, &mut acc);
        prev_keyframe_output_frameno = output_frameno;
        prev_keyframe_ntus = ntus;
        prev_keyframe_nframes = nframes_total;
        output_frameno += 1;
        ntus += 1;
        nframes_total += 1;
        continue;
      }
      let idx_in_group_output = self
        .inter_cfg
        .get_idx_in_group_output(output_frameno_in_gop);
      let input_frameno = prev_keyframe_input_frameno
        + self
          .inter_cfg
          .get_order_hint(output_frameno_in_gop, idx_in_group_output) as u64;
      // For rate control purposes, ignore any limit on frame count that has
      //  been set.
      // We pretend that we will keep encoding frames forever to prevent the
      //  control loop from driving us into the rails as we come up against a
      //  hard stop (with no more chance to correct outstanding errors).
      let next_keyframe_input_frameno =
        self.next_keyframe_input_frameno(prev_keyframe_input_frameno, true);
      // If we are re-ordering, we may skip some output frames in the final
      //  re-order group of the GOP.
      if input_frameno >= next_keyframe_input_frameno {
        // If we have encoded enough whole groups to reach the next keyframe,
        //  then start the next keyframe gop.
        if 1 + (output_frameno - prev_keyframe_output_frameno) / 1 * 1
          >= next_keyframe_input_frameno - prev_keyframe_input_frameno
        {
          collect_counts(nframes, &mut acc);
          prev_keyframe_input_frameno = input_frameno;
          prev_keyframe_output_frameno = output_frameno;
          prev_keyframe_ntus = ntus;
          prev_keyframe_nframes = nframes_total;
          // We do not currently use forward keyframes, so they should always
          //  end the current TU.
          output_frameno += 1;
          ntus += 1;
        }
        output_frameno += 1;
        continue;
      }
      if self.inter_cfg.get_show_existing_frame(idx_in_group_output) {
        acc[FRAME_SUBTYPE_SEF] += 1;
      } else {
        // TODO: Implement golden P-frames.
        let fti = FRAME_SUBTYPE_P + (self.inter_cfg.get_level(idx_in_group_output) as usize);
        acc[fti] += 1;
        nframes_total += 1;
      }
      if self.inter_cfg.get_show_frame(idx_in_group_output) {
        ntus += 1;
      }
      output_frameno += 1;
    }
    if prev_keyframe_output_frameno <= self.output_frameno {
      // If there were no keyframes at all, or only the first frame was a
      //  keyframe, the accumulators never flushed and still contain counts for
      //  the entire buffer.
      // In both cases, we return these counts.
      collect_counts(nframes, &mut acc);
      (nframes_total, ntus)
    } else {
      // Otherwise, we discard what remains in the accumulators as they contain
      //  the counts from and past the last keyframe.
      (prev_keyframe_nframes, prev_keyframe_ntus)
    }
  }
}
