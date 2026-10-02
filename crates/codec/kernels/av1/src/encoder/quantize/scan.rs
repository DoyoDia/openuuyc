//! Exact SIMD reduction of the last surviving scan position.
use crate::encoder::util::*;
use std::arch::x86_64::*;

#[target_feature(enable = "avx2")]
pub(super) unsafe fn last<T: Coefficient>(positions: &[u16], coeffs: &[T], threshold: T) -> u16 {
  let n = positions.len().min(coeffs.len());
  let mut at = 0;
  let mut best = _mm256_setzero_si256();
  let mut result;
  if std::mem::size_of::<T>() == 2 {
    let threshold = _mm256_set1_epi16(i32::cast_from(threshold) as i16);
    while at + 16 <= n {
      let values = _mm256_abs_epi16(_mm256_loadu_si256(coeffs.as_ptr().add(at).cast()));
      let below = _mm256_cmpgt_epi16(threshold, values);
      let index = _mm256_loadu_si256(positions.as_ptr().add(at).cast());
      best = _mm256_max_epu16(best, _mm256_andnot_si256(below, index));
      at += 16;
    }
    let mut lanes = [0u16; 16];
    _mm256_storeu_si256(lanes.as_mut_ptr().cast(), best);
    result = lanes.into_iter().max().unwrap();
  } else {
    let threshold = _mm256_set1_epi32(i32::cast_from(threshold));
    while at + 8 <= n {
      let values = _mm256_abs_epi32(_mm256_loadu_si256(coeffs.as_ptr().add(at).cast()));
      let below = _mm256_cmpgt_epi32(threshold, values);
      let index = _mm256_cvtepu16_epi32(_mm_loadu_si128(positions.as_ptr().add(at).cast()));
      best = _mm256_max_epu32(best, _mm256_andnot_si256(below, index));
      at += 8;
    }
    let mut lanes = [0u32; 8];
    _mm256_storeu_si256(lanes.as_mut_ptr().cast(), best);
    result = lanes.into_iter().max().unwrap() as u16;
  }
  for (&pos, &value) in positions[at..n].iter().zip(&coeffs[at..n]) {
    if value.abs() >= threshold {
      result = result.max(pos);
    }
  }
  result
}
