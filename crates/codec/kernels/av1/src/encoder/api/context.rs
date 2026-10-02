// Copyright (c) 2018-2022, The rav1e contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.
#![deny(missing_docs)]

use std::fmt;
use std::io;
use std::sync::Arc;

use bitstream_io::{BigEndian, BitWrite, BitWriter};

use crate::encoder::api::color::*;
use crate::encoder::api::config::*;
use crate::encoder::api::internal::*;
use crate::encoder::api::util::*;
use crate::encoder::encoder::*;
use crate::encoder::frame::*;
use crate::encoder::util::Pixel;

/// The encoder context.
///
/// Contains the encoding state.
pub struct Context<T: Pixel> {
  pub(crate) inner: ContextInner<T>,
  pub(crate) config: EncoderConfig,
  pub(crate) pool: Option<Arc<rayon::ThreadPool>>,
  pub(crate) is_flushing: bool,
}

impl<T: Pixel> Context<T> {
  /// Update a zero-lookahead stream between inputs, retaining references and
  /// the learned rate model. Size, profile and color configuration are fixed.
  pub fn reconfigure_stream(
    &mut self,
    bitrate: i32,
    time_base: Rational,
  ) -> Result<(), EncoderStatus> {
    if !self.config.low_latency
      || self.config.speed_settings.rdo_lookahead_frames != 0
      || self.inner.frame_count != self.inner.frames_processed
      || self.is_flushing
      || bitrate <= 0
      || time_base.num == 0
      || time_base.den == 0
      || time_base.num > u32::MAX as u64
      || time_base.den > u32::MAX as u64
    {
      return Err(EncoderStatus::Failure);
    }
    let mut updated = self.config.clone();
    updated.bitrate = bitrate;
    updated.time_base = time_base;
    Config::new()
      .with_encoder_config(updated)
      .validate()
      .map_err(|_| EncoderStatus::Failure)?;
    self
      .inner
      .rc_state
      .reconfigure_stream(bitrate, time_base.den as i64, time_base.num as i64);
    self.config.bitrate = bitrate;
    self.config.time_base = time_base;
    let config = Arc::new(self.config.clone());
    self.inner.config = config.clone();
    for data in self.inner.frame_data.values_mut().flatten() {
      data.fi.config = config.clone();
    }
    Ok(())
  }

  /// Install an owning cancellation signal. A cancelled encode invalidates
  /// this context; the caller recreates it and resumes with a keyframe.
  pub fn set_cancellation(&mut self, signal: Option<Arc<std::sync::atomic::AtomicBool>>) {
    if self
      .inner
      .cancellation
      .as_ref()
      .zip(signal.as_ref())
      .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
    {
      return;
    }
    self.inner.cancellation = signal.clone();
    for data in self.inner.frame_data.values_mut().flatten() {
      data.fi.cancellation = signal.clone();
    }
  }

  /// Allocates and returns a new frame.
  ///
  /// # Examples
  ///
  /// ```
  /// use openuuyc_av1::encoder::prelude::*;
  ///
  /// # fn main() -> Result<(), InvalidConfig> {
  /// let cfg = Config::default();
  /// let ctx: Context<u8> = cfg.new_context()?;
  /// let frame = ctx.new_frame();
  /// # Ok(())
  /// # }
  /// ```
  #[inline]
  pub fn new_frame(&self) -> Frame<T> {
    Frame::new(
      self.config.width,
      self.config.height,
      self.config.chroma_sampling,
    )
  }

  /// Sends the frame for encoding.
  ///
  /// This method adds the frame into the frame queue and runs the first passes
  /// of the look-ahead computation.
  ///
  /// Passing `None` is equivalent to calling [`flush`].
  ///
  /// The caller is responsible for padding the invisible portion of the frame,
  /// if multiple references to the frame are held.
  /// Calling [`Plane::pad()`] after filling each plane or equivalent is required.
  ///
  /// # Errors
  ///
  /// If this method is called with a frame after the encoder has been flushed
  /// or the encoder internal limit is hit (`std::i32::MAX` frames) the
  /// [`EncoderStatus::EnoughData`] error is returned.
  ///
  /// # Examples
  ///
  /// ```
  /// use openuuyc_av1::encoder::prelude::*;
  ///
  /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
  /// # if false {
  /// let cfg = Config::default();
  /// let mut ctx: Context<u8> = cfg.new_context().unwrap();
  /// let f1 = ctx.new_frame();
  /// let f2 = f1.clone();
  /// let info = FrameParameters {
  ///   frame_type_override: FrameTypeOverride::Key,
  ///   opaque: None,
  ///   ..Default::default()
  /// };
  ///
  /// // Send the plain frame data
  /// ctx.send_frame(f1)?;
  /// // Send the data and the per-frame parameters
  /// // In this case the frame is forced to be a keyframe.
  /// ctx.send_frame((f2, info))?;
  /// // Flush the encoder, it is equivalent to a call to `flush()`
  /// ctx.send_frame(None)?;
  /// # }
  /// # Ok(())
  /// # }
  /// ```
  ///
  /// [`flush`]: #method.flush
  /// [`EncoderStatus::EnoughData`]: enum.EncoderStatus.html#variant.EnoughData
  #[inline]
  pub fn send_frame<F>(&mut self, frame: F) -> Result<(), EncoderStatus>
  where
    F: IntoFrame<T>,
  {
    let (frame, params) = frame.into();

    if frame.is_none() {
      if self.is_flushing {
        return Ok(());
      }
      self.inner.limit = Some(self.inner.frame_count);
      self.is_flushing = true;
    } else if self.is_flushing || (self.inner.config.still_picture && self.inner.frame_count > 0) {
      return Err(EncoderStatus::EnoughData);
    // The rate control can process at most i32::MAX frames
    } else if self.inner.frame_count == i32::MAX as u64 - 1 {
      self.inner.limit = Some(self.inner.frame_count);
      self.is_flushing = true;
    }

    let inner = &mut self.inner;
    let run = move || inner.send_frame(frame, params);

    match &self.pool {
      Some(pool) => pool.install(run),
      None => run(),
    }
  }

  /// Encodes the next frame and returns the encoded data.
  ///
  /// This method is where the main encoding work is done.
  ///
  /// # Errors
  ///
  /// May return `Err(EncoderStatus)`, which should be handled by the caller.
  ///
  /// # Examples
  ///
  /// Encoding a single frame:
  ///
  /// ```
  /// use openuuyc_av1::encoder::prelude::*;
  ///
  /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
  /// # if false {
  /// let cfg = Config::default();
  /// let mut ctx: Context<u8> = cfg.new_context()?;
  /// let frame = ctx.new_frame();
  ///
  /// ctx.send_frame(frame)?;
  /// ctx.flush();
  ///
  /// loop {
  ///     match ctx.receive_packet() {
  ///         Ok(packet) => { /* Mux the packet. */ },
  ///         Err(EncoderStatus::Encoded) => (),
  ///         Err(EncoderStatus::LimitReached) => break,
  ///         Err(err) => Err(err)?,
  ///     }
  /// }
  /// # }
  /// # Ok(())
  /// # }
  /// ```
  ///
  /// Encoding a sequence of frames:
  ///
  /// ```
  /// use std::sync::Arc;
  /// use openuuyc_av1::encoder::prelude::*;
  ///
  /// fn encode_frames(
  ///     ctx: &mut Context<u8>,
  ///     mut frames: impl Iterator<Item=Frame<u8>>
  /// ) -> Result<(), EncoderStatus> {
  ///     // This is a slightly contrived example, intended to showcase the
  ///     // various statuses that can be returned from receive_packet().
  ///     // Assume that, for example, there are a lot of frames in the
  ///     // iterator, which are produced lazily, so you don't want to send
  ///     // them all in at once as to not exhaust the memory.
  ///     loop {
  ///         match ctx.receive_packet() {
  ///             Ok(packet) => { /* Mux the packet. */ },
  ///             Err(EncoderStatus::Encoded) => {
  ///                 // A frame was encoded without emitting a packet. This is
  ///                 // normal, just proceed as usual.
  ///             },
  ///             Err(EncoderStatus::LimitReached) => {
  ///                 // All frames have been encoded. Time to break out of the
  ///                 // loop.
  ///                 break;
  ///             },
  ///             Err(EncoderStatus::NeedMoreData) => {
  ///                 // The encoder has requested additional frames. Push the
  ///                 // next frame in, or flush the encoder if there are no
  ///                 // frames left (on None).
  ///                 ctx.send_frame(frames.next().map(Arc::new))?;
  ///             },
  ///             Err(EncoderStatus::EnoughData) => {
  ///                 // Since we aren't trying to push frames after flushing,
  ///                 // this should never happen in this example.
  ///                 unreachable!();
  ///             },
  ///             Err(EncoderStatus::NotReady) => {
  ///                 // We're not doing two-pass encoding, so this can never
  ///                 // occur.
  ///                 unreachable!();
  ///             },
  ///             Err(EncoderStatus::Failure) => {
  ///                 return Err(EncoderStatus::Failure);
  ///             },
  ///         }
  ///     }
  ///
  ///     Ok(())
  /// }
  /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
  /// #   if false {
  /// #     let mut enc = EncoderConfig::default();
  /// #     // So it runs faster.
  /// #     enc.width = 16;
  /// #     enc.height = 16;
  /// #     let cfg = Config::new().with_encoder_config(enc);
  /// #     let mut ctx: Context<u8> = cfg.new_context()?;
  /// #
  /// #     let frames = vec![ctx.new_frame(); 4].into_iter();
  /// #     encode_frames(&mut ctx, frames);
  /// #   }
  /// #   Ok(())
  /// # }
  /// ```
  #[inline]
  pub fn receive_packet(&mut self) -> Result<Packet<T>, EncoderStatus> {
    let inner = &mut self.inner;
    let mut run = move || inner.receive_packet();

    match &self.pool {
      Some(pool) => pool.install(run),
      None => run(),
    }
  }

  /// Flushes the encoder.
  ///
  /// Flushing signals the end of the video. After the encoder has been
  /// flushed, no additional frames are accepted.
  ///
  /// # Panics
  ///
  /// Panics if `send_frame` returns an `Err`.
  /// This should never happen when calling it with `None`
  /// and indicates a development error.
  #[inline]
  pub fn flush(&mut self) {
    self.send_frame(None).unwrap();
  }

  /// Produces a sequence header matching the current encoding context.
  ///
  /// Its format is compatible with the AV1 Matroska and ISOBMFF specification.
  /// Note that the returned header does not include any config OBUs which are
  /// required for some uses. See [the specification].
  ///
  /// [the specification]:
  /// https://aomediacodec.github.io/av1-isobmff/#av1codecconfigurationbox-section
  ///
  /// # Panics
  ///
  /// Panics if the header cannot be written in memory. This is unrecoverable,
  /// and usually indicates the system is out of memory.
  #[inline]
  pub fn container_sequence_header(&self) -> Vec<u8> {
    fn sequence_header_inner(seq: &Sequence) -> io::Result<Vec<u8>> {
      let mut buf = Vec::new();

      {
        let mut bw = BitWriter::endian(&mut buf, BigEndian);
        bw.write_bit(true)?; // marker
        bw.write::<7, u8>(1)?; // version
        bw.write::<3, u8>(seq.profile)?;
        bw.write::<5, u8>(31)?; // level
        bw.write_bit(false)?; // tier
        bw.write_bit(seq.bit_depth > 8)?; // high_bitdepth
        bw.write_bit(seq.bit_depth == 12)?; // twelve_bit
        bw.write_bit(seq.chroma_sampling == ChromaSampling::Cs400)?; // monochrome
        bw.write_bit(seq.chroma_sampling != ChromaSampling::Cs444)?; // chroma_subsampling_x
        bw.write_bit(seq.chroma_sampling == ChromaSampling::Cs420)?; // chroma_subsampling_y
        bw.write::<2, u8>(0)?; // chroma_sample_position
        bw.write::<3, u8>(0)?; // reserved
        bw.write_bit(false)?; // initial_presentation_delay_present

        bw.write::<4, u8>(0)?; // reserved
      }

      Ok(buf)
    }

    let seq = Sequence::new(&self.config);

    sequence_header_inner(&seq).unwrap()
  }
}

impl<T: Pixel> fmt::Debug for Context<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::result::Result<(), fmt::Error> {
    write!(
      f,
      "{{ \
        config: {:?}, \
        is_flushing: {}, \
      }}",
      self.config, self.is_flushing,
    )
  }
}
