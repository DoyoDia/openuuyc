// Copyright (c) The rav1e contributors. BSD-2-Clause and AOM patent grant.
// Local concrete streaming boundary; see LICENSE and PATENTS.
use crate::encoder::prelude::{
  ChromaSampling, FrameParameters, FrameTypeOverride, Pixel, Rational,
};
use crate::encoder::{Config, Context, EncoderStatus, InvalidConfig};
use std::sync::{
  atomic::{AtomicBool, Ordering},
  Arc,
};

/// Borrowed, tightly packed planar input in the configured sample depth.
pub enum Samples<'a> {
  Eight([&'a [u8]; 3]),
  Ten([&'a [u16]; 3]),
}
/// One complete access unit for the current input; never a queued future frame.
pub struct Packet {
  pub data: Vec<u8>,
  pub keyframe: bool,
}

enum Core {
  Eight(Context<u8>),
  Ten(Context<u16>),
}
/// The application uses concrete methods so its size-optimized glue does not
/// instantiate the encoder's generic RDO and pixel kernels at a different level.
pub struct Encoder {
  core: Core,
  next_frame: u64,
}
impl Encoder {
  #[inline(never)]
  pub fn new(config: Config) -> Result<Self, InvalidConfig> {
    config.validate()?;
    let core = match config.enc.bit_depth {
      8 => Core::Eight(config.new_context()?),
      10 => Core::Ten(config.new_context()?),
      _ => return Err(InvalidConfig::UnsupportedStreamingConfiguration),
    };
    Ok(Self {
      core,
      next_frame: 0,
    })
  }
  #[inline(never)]
  pub fn reconfigure(&mut self, bitrate: i32, time_base: Rational) -> Result<(), EncoderStatus> {
    match &mut self.core {
      Core::Eight(c) => c.reconfigure_stream(bitrate, time_base),
      Core::Ten(c) => c.reconfigure_stream(bitrate, time_base),
    }
  }
  #[inline(never)]
  pub fn set_cancellation(&mut self, signal: Arc<AtomicBool>) {
    match &mut self.core {
      Core::Eight(c) => c.set_cancellation(Some(signal)),
      Core::Ten(c) => c.set_cancellation(Some(signal)),
    }
  }
  /// On failure discard this encoder before supplying another image.
  #[inline(never)]
  pub fn encode(&mut self, samples: Samples<'_>, key: bool) -> Result<Packet, EncoderStatus> {
    let packet = match (&mut self.core, samples) {
      (Core::Eight(c), Samples::Eight(p)) => submit(c, p, key, self.next_frame)?,
      (Core::Ten(c), Samples::Ten(p)) => submit(c, p, key, self.next_frame)?,
      _ => return Err(EncoderStatus::Failure),
    };
    self.next_frame += 1;
    Ok(packet)
  }
}
fn submit<T: Pixel + bytemuck::Pod>(
  c: &mut Context<T>,
  planes: [&[T]; 3],
  key: bool,
  index: u64,
) -> Result<Packet, EncoderStatus> {
  let mut frame = c.new_frame();
  for (p, samples) in planes.into_iter().enumerate() {
    let sub = usize::from(p != 0 && c.config.chroma_sampling == ChromaSampling::Cs420);
    let width = c.config.width.div_ceil(1 << sub);
    let height = c.config.height.div_ceil(1 << sub);
    if samples.len() != width * height {
      return Err(EncoderStatus::Failure);
    }
    let bytes = std::mem::size_of::<T>();
    frame.planes[p].copy_from_raw_u8(bytemuck::cast_slice(samples), width * bytes, bytes);
  }
  c.send_frame((
    frame,
    FrameParameters {
      frame_type_override: if key {
        FrameTypeOverride::Key
      } else {
        FrameTypeOverride::No
      },
      ..Default::default()
    },
  ))?;
  let packet = c.receive_packet()?;
  if packet.input_frameno != index
    || !matches!(c.receive_packet(), Err(EncoderStatus::NeedMoreData))
  {
    return Err(EncoderStatus::Failure);
  }
  Ok(Packet {
    data: packet.data,
    keyframe: packet.frame_type == crate::encoder::data::FrameType::KEY,
  })
}

/// Copy mapped NV12/AYUV
/// into reusable planar buffers, without instantiating hot loops in the app.
#[inline(never)]
pub fn unpack_8(
  data: &[u8],
  pitch: usize,
  size: (usize, usize),
  half: bool,
  p: &mut [Vec<u8>; 3],
  cancel: &AtomicBool,
) -> Result<(), EncoderStatus> {
  #[cfg(target_arch = "x86_64")]
  if std::is_x86_feature_detected!("avx2") {
    // SAFETY: detected target feature; bounds stay checked by the common body.
    return unsafe { unpack_8_avx2(data, pitch, size, half, p, cancel) };
  }
  unpack_8_inner(data, pitch, size, half, p, cancel)
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn unpack_8_avx2(
  data: &[u8],
  pitch: usize,
  size: (usize, usize),
  half: bool,
  p: &mut [Vec<u8>; 3],
  cancel: &AtomicBool,
) -> Result<(), EncoderStatus> {
  unpack_8_inner(data, pitch, size, half, p, cancel)
}
#[inline(always)]
fn unpack_8_inner(
  data: &[u8],
  pitch: usize,
  size: (usize, usize),
  half: bool,
  p: &mut [Vec<u8>; 3],
  cancel: &AtomicBool,
) -> Result<(), EncoderStatus> {
  let (w, h) = size;
  let row = w
    .checked_mul(if half { 1 } else { 4 })
    .ok_or(EncoderStatus::Failure)?;
  let rows = if half {
    h.checked_add(h / 2).ok_or(EncoderStatus::Failure)?
  } else {
    h
  };
  let y = w.checked_mul(h).ok_or(EncoderStatus::Failure)?;
  let uv = if half { y / 4 } else { y };
  if w == 0
    || h == 0
    || (half && (w % 2 != 0 || h % 2 != 0))
    || pitch < row
    || data.len() < pitch.checked_mul(rows).ok_or(EncoderStatus::Failure)?
    || p[0].len() != y
    || p[1].len() != uv
    || p[2].len() != uv
  {
    return Err(EncoderStatus::Failure);
  }

  for y in 0..h {
    if cancel.load(Ordering::Acquire) {
      return Err(EncoderStatus::Failure);
    }
    let source = &data[y * pitch..y * pitch + row];
    if half {
      p[0][y * w..(y + 1) * w].copy_from_slice(source);
    } else {
      for (x, pixel) in source.chunks_exact(4).enumerate() {
        let at = y * w + x;
        p[0][at] = pixel[2];
        p[1][at] = pixel[1];
        p[2][at] = pixel[0];
      }
    }
  }
  if half {
    for y in 0..h / 2 {
      if cancel.load(Ordering::Acquire) {
        return Err(EncoderStatus::Failure);
      }
      let source = &data[(h + y) * pitch..(h + y) * pitch + w];
      for (x, uv) in source.chunks_exact(2).enumerate() {
        let at = y * (w / 2) + x;
        p[1][at] = uv[0];
        p[2][at] = uv[1];
      }
    }
  }

  Ok(())
}

/// Copy mapped P010/Y410
/// into reusable planar buffers, without instantiating hot loops in the app.
#[inline(never)]
pub fn unpack_10(
  data: &[u8],
  pitch: usize,
  size: (usize, usize),
  half: bool,
  p: &mut [Vec<u16>; 3],
  cancel: &AtomicBool,
) -> Result<(), EncoderStatus> {
  #[cfg(target_arch = "x86_64")]
  if std::is_x86_feature_detected!("avx2") {
    // SAFETY: detected target feature; bounds stay checked by the common body.
    return unsafe { unpack_10_avx2(data, pitch, size, half, p, cancel) };
  }
  unpack_10_inner(data, pitch, size, half, p, cancel)
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn unpack_10_avx2(
  data: &[u8],
  pitch: usize,
  size: (usize, usize),
  half: bool,
  p: &mut [Vec<u16>; 3],
  cancel: &AtomicBool,
) -> Result<(), EncoderStatus> {
  unpack_10_inner(data, pitch, size, half, p, cancel)
}
#[inline(always)]
fn unpack_10_inner(
  data: &[u8],
  pitch: usize,
  size: (usize, usize),
  half: bool,
  p: &mut [Vec<u16>; 3],
  cancel: &AtomicBool,
) -> Result<(), EncoderStatus> {
  let (w, h) = size;
  let row = w
    .checked_mul(if half { 2 } else { 4 })
    .ok_or(EncoderStatus::Failure)?;
  let rows = if half {
    h.checked_add(h / 2).ok_or(EncoderStatus::Failure)?
  } else {
    h
  };
  let y = w.checked_mul(h).ok_or(EncoderStatus::Failure)?;
  let uv = if half { y / 4 } else { y };
  if w == 0
    || h == 0
    || (half && (w % 2 != 0 || h % 2 != 0))
    || pitch < row
    || data.len() < pitch.checked_mul(rows).ok_or(EncoderStatus::Failure)?
    || p[0].len() != y
    || p[1].len() != uv
    || p[2].len() != uv
  {
    return Err(EncoderStatus::Failure);
  }

  for y in 0..h {
    if cancel.load(Ordering::Acquire) {
      return Err(EncoderStatus::Failure);
    }
    let source = &data[y * pitch..y * pitch + row];
    if half {
      for (dst, src) in p[0][y * w..(y + 1) * w]
        .iter_mut()
        .zip(source.chunks_exact(2))
      {
        *dst = u16::from_le_bytes([src[0], src[1]]) >> 6;
      }
    } else {
      for (x, src) in source.chunks_exact(4).enumerate() {
        let value = u32::from_le_bytes(src.try_into().unwrap());
        let at = y * w + x;
        p[0][at] = (value >> 10 & 1023) as u16;
        p[1][at] = (value & 1023) as u16;
        p[2][at] = (value >> 20 & 1023) as u16;
      }
    }
  }
  if half {
    for y in 0..h / 2 {
      if cancel.load(Ordering::Acquire) {
        return Err(EncoderStatus::Failure);
      }
      let source = &data[(h + y) * pitch..(h + y) * pitch + w * 2];
      for (x, uv) in source.chunks_exact(4).enumerate() {
        let at = y * (w / 2) + x;
        p[1][at] = u16::from_le_bytes([uv[0], uv[1]]) >> 6;
        p[2][at] = u16::from_le_bytes([uv[2], uv[3]]) >> 6;
      }
    }
  }

  Ok(())
}
