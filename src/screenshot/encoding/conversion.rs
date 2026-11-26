// src/screenshot/encoding/conversion.rs
// SIMD-optimized RGBA to RGB conversion for JPEG encoding

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// Convert RGBA buffer to RGB with optimal SIMD path selection.
/// Allocates a new RGB buffer of size (width * height * 3).
///
/// Uses parallel processing with Rayon for large buffers.
#[inline]
pub fn convert_rgba_to_rgb(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
  let pixel_count = (width as usize) * (height as usize);
  let expected_len = pixel_count * 4;

  debug_assert_eq!(
    rgba.len(),
    expected_len,
    "RGBA buffer size mismatch: got {}, expected {}",
    rgba.len(),
    expected_len
  );

  let mut rgb = vec![0u8; pixel_count * 3];
  convert_rgba_to_rgb_direct(rgba, &mut rgb);
  rgb
}

/// Convert RGBA to RGB directly into a pre-allocated buffer.
/// Uses parallel processing for large buffers (> 64KB).
#[inline]
pub fn convert_rgba_to_rgb_direct(rgba: &[u8], rgb: &mut [u8]) {
  let pixel_count = rgba.len() / 4;
  debug_assert_eq!(
    rgb.len(),
    pixel_count * 3,
    "RGB buffer size mismatch: got {}, expected {}",
    rgb.len(),
    pixel_count * 3
  );

  // For small buffers, use single-threaded conversion
  if rgba.len() < 65536 {
    convert_rgba_to_rgb_chunk(rgba, rgb);
    return;
  }

  // For large buffers, use parallel processing
  use rayon::prelude::*;

  // 12KB chunks (4096 pixels * 4 bytes RGBA = 16KB input, 12KB RGB output)
  // Fits well in L1 cache (typically 32KB)
  const CHUNK_PIXELS: usize = 4096;
  const RGBA_CHUNK_SIZE: usize = CHUNK_PIXELS * 4;
  const RGB_CHUNK_SIZE: usize = CHUNK_PIXELS * 3;

  let chunks = pixel_count.div_ceil(CHUNK_PIXELS);

  rgb
    .par_chunks_mut(RGB_CHUNK_SIZE)
    .enumerate()
    .take(chunks)
    .for_each(|(chunk_idx, dst_chunk)| {
      let rgba_offset = chunk_idx * RGBA_CHUNK_SIZE;
      let rgba_end = (rgba_offset + RGBA_CHUNK_SIZE).min(rgba.len());
      let src_chunk = &rgba[rgba_offset..rgba_end];
      convert_rgba_to_rgb_chunk(src_chunk, dst_chunk);
    });
}

/// Convert a single chunk of RGBA to RGB using the best available SIMD.
#[inline(always)]
fn convert_rgba_to_rgb_chunk(rgba: &[u8], rgb: &mut [u8]) {
  #[cfg(target_arch = "x86_64")]
  {
    let backend = rgb_backend(
      is_x86_feature_detected!("avx2"),
      is_x86_feature_detected!("sse2"),
      is_x86_feature_detected!("ssse3"),
    );
    if backend == RgbBackend::Avx2 {
      unsafe {
        convert_rgba_to_rgb_avx2(rgba, rgb);
      }
      return;
    }
    if backend == RgbBackend::SseShuffle {
      unsafe {
        convert_rgba_to_rgb_sse2(rgba, rgb);
      }
      return;
    }
  }

  convert_rgba_to_rgb_scalar(rgba, rgb);
}

#[cfg(target_arch = "x86_64")]
#[derive(Debug, PartialEq, Eq)]
enum RgbBackend {
  Avx2,
  SseShuffle,
  Scalar,
}

#[cfg(target_arch = "x86_64")]
fn rgb_backend(avx2: bool, sse2: bool, ssse3: bool) -> RgbBackend {
  if avx2 {
    RgbBackend::Avx2
  } else if sse2 && ssse3 {
    RgbBackend::SseShuffle
  } else {
    RgbBackend::Scalar
  }
}

/// AVX2 SIMD conversion - processes 8 pixels per iteration.
/// Input: 32 bytes (8 RGBA pixels)
/// Output: 24 bytes (8 RGB pixels)
///
/// # Safety
/// - Caller must ensure `rgba.len()` is divisible by 4 (valid RGBA pixel data)
/// - Caller must ensure `rgb.len()` is at least `(rgba.len() / 4) * 3`
/// - Requires AVX2 CPU support (checked at runtime before calling)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
pub unsafe fn convert_rgba_to_rgb_avx2(rgba: &[u8], rgb: &mut [u8]) {
  unsafe {
    let pixels = rgba.len() / 4;
    let simd_pixels = pixels / 8;
    let simd_bulk = simd_pixels.saturating_sub(1);
    let remainder_pixels = pixels - (simd_bulk * 8);

    let src_ptr = rgba.as_ptr();
    let dst_ptr = rgb.as_mut_ptr();

    // Process 8 pixels at a time
    // Each iteration: read 32 bytes RGBA, write 24 bytes RGB
    for i in 0..simd_bulk {
      let src_offset = i * 32;
      let dst_offset = i * 24;

      // Load 32 bytes (8 RGBA pixels)
      let pixels_lo = _mm_loadu_si128(src_ptr.add(src_offset) as *const __m128i);
      let pixels_hi = _mm_loadu_si128(src_ptr.add(src_offset + 16) as *const __m128i);

      // Shuffle mask to extract RGB from RGBA (drop every 4th byte)
      // For first 4 pixels: [R0,G0,B0,R1,G1,B1,R2,G2,B2,R3,G3,B3, X,X,X,X]
      let shuffle_lo = _mm_setr_epi8(0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -1, -1, -1, -1);
      let shuffle_hi = _mm_setr_epi8(0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -1, -1, -1, -1);

      let rgb_lo = _mm_shuffle_epi8(pixels_lo, shuffle_lo); // First 12 RGB bytes
      let rgb_hi = _mm_shuffle_epi8(pixels_hi, shuffle_hi); // Second 12 RGB bytes

      // Write 12 bytes from low part
      _mm_storeu_si128(dst_ptr.add(dst_offset) as *mut __m128i, rgb_lo);

      // Write 12 bytes from high part (offset by 12)
      _mm_storeu_si128(dst_ptr.add(dst_offset + 12) as *mut __m128i, rgb_hi);
    }

    // Handle remaining pixels with scalar code (includes last SIMD chunk to avoid OOB stores)
    let base_src = simd_bulk * 32;
    let base_dst = simd_bulk * 24;
    for i in 0..remainder_pixels {
      let src_off = base_src + i * 4;
      let dst_off = base_dst + i * 3;
      *dst_ptr.add(dst_off) = *src_ptr.add(src_off); // R
      *dst_ptr.add(dst_off + 1) = *src_ptr.add(src_off + 1); // G
      *dst_ptr.add(dst_off + 2) = *src_ptr.add(src_off + 2); // B
    }
  }
}

/// SSE2 SIMD conversion - processes 4 pixels per iteration.
/// Input: 16 bytes (4 RGBA pixels)
/// Output: 12 bytes (4 RGB pixels)
///
/// # Safety
/// - Caller must ensure `rgba.len()` is divisible by 4 (valid RGBA pixel data)
/// - Caller must ensure `rgb.len()` is at least `(rgba.len() / 4) * 3`
/// - Requires SSE2 and SSSE3 CPU support (checked at runtime before calling)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2", enable = "ssse3")]
#[inline]
pub unsafe fn convert_rgba_to_rgb_sse2(rgba: &[u8], rgb: &mut [u8]) {
  unsafe {
    let pixels = rgba.len() / 4;
    let simd_pixels = pixels / 4;
    let simd_bulk = simd_pixels.saturating_sub(1);
    let remainder_pixels = pixels - (simd_bulk * 4);

    let src_ptr = rgba.as_ptr();
    let dst_ptr = rgb.as_mut_ptr();

    // Shuffle mask to extract RGB from RGBA
    // [R0,G0,B0,R1,G1,B1,R2,G2,B2,R3,G3,B3, X,X,X,X]
    let shuffle_mask = _mm_setr_epi8(0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -1, -1, -1, -1);

    for i in 0..simd_bulk {
      let src_offset = i * 16;
      let dst_offset = i * 12;

      // Load 16 bytes (4 RGBA pixels)
      let pixels = _mm_loadu_si128(src_ptr.add(src_offset) as *const __m128i);

      // Shuffle to remove alpha bytes
      let rgb_pixels = _mm_shuffle_epi8(pixels, shuffle_mask);

      // Store only 12 bytes (we write 16 but only first 12 are valid)
      // Use unaligned store and let the next iteration overwrite the padding
      _mm_storeu_si128(dst_ptr.add(dst_offset) as *mut __m128i, rgb_pixels);
    }

    // Fix potential overwrite from last SIMD iteration and handle remainder
    let base_src = simd_bulk * 16;
    let base_dst = simd_bulk * 12;
    for i in 0..remainder_pixels {
      let src_off = base_src + i * 4;
      let dst_off = base_dst + i * 3;
      *dst_ptr.add(dst_off) = *src_ptr.add(src_off); // R
      *dst_ptr.add(dst_off + 1) = *src_ptr.add(src_off + 1); // G
      *dst_ptr.add(dst_off + 2) = *src_ptr.add(src_off + 2); // B
    }
  }
}

/// Scalar fallback for non-x86_64 or when SIMD not available.
#[inline(always)]
pub fn convert_rgba_to_rgb_scalar(rgba: &[u8], rgb: &mut [u8]) {
  let pixels = rgba.len() / 4;

  for i in 0..pixels {
    let src_off = i * 4;
    let dst_off = i * 3;
    rgb[dst_off] = rgba[src_off]; // R
    rgb[dst_off + 1] = rgba[src_off + 1]; // G
    rgb[dst_off + 2] = rgba[src_off + 2]; // B
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[cfg(target_arch = "x86_64")]
  fn sse2_only_processors_must_never_execute_ssse3_byte_shuffle_instructions() {
    assert_eq!(rgb_backend(false, true, false), RgbBackend::Scalar);
    assert_eq!(rgb_backend(false, true, true), RgbBackend::SseShuffle);
    assert_eq!(rgb_backend(true, true, true), RgbBackend::Avx2);
    assert_eq!(rgb_backend(false, false, false), RgbBackend::Scalar);
  }

  #[test]
  #[cfg(target_arch = "x86_64")]
  fn shuffle_and_avx_converters_preserve_exact_pixels_without_overwriting_tail_guards() {
    for pixels in [
      0, 1, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 4095, 4096, 4097,
    ] {
      let rgba: Vec<_> = (0..pixels * 4).map(|i| (i % 251) as u8).collect();
      let mut expected = vec![0; pixels * 3];
      convert_rgba_to_rgb_scalar(&rgba, &mut expected);
      for avx in [false, true] {
        if (avx && !is_x86_feature_detected!("avx2"))
          || (!avx && !is_x86_feature_detected!("ssse3"))
        {
          continue;
        }
        let mut guarded = vec![0xab; pixels * 3 + 16];
        // SAFETY: feature support is checked, pixels are complete, and the exact output slice is large enough.
        unsafe {
          if avx {
            convert_rgba_to_rgb_avx2(&rgba, &mut guarded[8..8 + pixels * 3]);
          } else {
            convert_rgba_to_rgb_sse2(&rgba, &mut guarded[8..8 + pixels * 3]);
          }
        }
        assert_eq!(&guarded[8..8 + pixels * 3], expected);
        assert_eq!(&guarded[..8], &[0xab; 8]);
        assert_eq!(&guarded[8 + pixels * 3..], &[0xab; 8]);
      }
    }
  }

  #[test]
  fn test_rgba_to_rgb_basic() {
    // 4 pixels: red, green, blue, white (with varying alpha)
    let rgba = vec![
      255, 0, 0, 255, // Red, full alpha
      0, 255, 0, 128, // Green, half alpha
      0, 0, 255, 64, // Blue, quarter alpha
      255, 255, 255, 0, // White, zero alpha (alpha ignored)
    ];

    let rgb = convert_rgba_to_rgb(&rgba, 2, 2);

    assert_eq!(rgb.len(), 12);
    assert_eq!(rgb, vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
  }

  #[test]
  fn test_rgba_to_rgb_scalar_basic() {
    let rgba = vec![255, 128, 64, 200, 32, 64, 96, 128];
    let mut rgb = vec![0u8; 6];

    convert_rgba_to_rgb_scalar(&rgba, &mut rgb);

    assert_eq!(rgb, vec![255, 128, 64, 32, 64, 96]);
  }

  #[test]
  fn test_rgba_to_rgb_single_pixel() {
    let rgba = vec![100, 150, 200, 255];
    let rgb = convert_rgba_to_rgb(&rgba, 1, 1);

    assert_eq!(rgb.len(), 3);
    assert_eq!(rgb, vec![100, 150, 200]);
  }

  #[test]
  fn test_rgba_to_rgb_ignores_alpha() {
    let rgba = vec![
      255, 0, 0, 0, // Red with zero alpha
      0, 255, 0, 255, // Green with full alpha
    ];

    let rgb = convert_rgba_to_rgb(&rgba, 2, 1);

    // Alpha should be ignored, only RGB preserved
    assert_eq!(rgb, vec![255, 0, 0, 0, 255, 0]);
  }

  #[test]
  fn test_convert_rgba_to_rgb_direct() {
    let rgba = vec![10, 20, 30, 40, 50, 60, 70, 80];
    let mut rgb = vec![0u8; 6];

    convert_rgba_to_rgb_direct(&rgba, &mut rgb);

    assert_eq!(rgb, vec![10, 20, 30, 50, 60, 70]);
  }

  #[test]
  fn test_rgba_to_rgb_large_buffer() {
    // Test with buffer > 64KB to trigger parallel path
    let pixel_count = 20000; // 80KB RGBA, 60KB RGB
    let rgba = vec![128u8; pixel_count * 4];

    let rgb = convert_rgba_to_rgb(&rgba, pixel_count as u32, 1);

    assert_eq!(rgb.len(), pixel_count * 3);
    // All RGB values should be 128
    assert!(rgb.iter().all(|&v| v == 128));
  }

  #[test]
  fn test_rgba_to_rgb_small_buffer() {
    // Test with buffer < 64KB to trigger single-threaded path
    let pixel_count = 100;
    let rgba = vec![50u8; pixel_count * 4];

    let rgb = convert_rgba_to_rgb(&rgba, pixel_count as u32, 1);

    assert_eq!(rgb.len(), pixel_count * 3);
    assert!(rgb.iter().all(|&v| v == 50));
  }

  #[test]
  fn test_rgba_to_rgb_scalar_empty() {
    let rgba = vec![];
    let mut rgb = vec![];

    convert_rgba_to_rgb_scalar(&rgba, &mut rgb);

    assert_eq!(rgb.len(), 0);
  }

  #[test]
  fn test_rgba_to_rgb_chunk_dispatcher() {
    // Test that chunk dispatcher works correctly
    let rgba = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let mut rgb = vec![0u8; 6];

    convert_rgba_to_rgb_chunk(&rgba, &mut rgb);

    assert_eq!(rgb, vec![1, 2, 3, 5, 6, 7]);
  }

  #[test]
  fn test_rgba_to_rgb_all_zeros() {
    let rgba = vec![0u8; 16]; // 4 pixels of black
    let rgb = convert_rgba_to_rgb(&rgba, 2, 2);

    assert_eq!(rgb.len(), 12);
    assert!(rgb.iter().all(|&v| v == 0));
  }

  #[test]
  fn test_rgba_to_rgb_all_ones() {
    let rgba = vec![255u8; 16]; // 4 pixels of white with full alpha
    let rgb = convert_rgba_to_rgb(&rgba, 2, 2);

    assert_eq!(rgb.len(), 12);
    assert!(rgb.iter().all(|&v| v == 255));
  }

  #[test]
  fn test_rgba_to_rgb_pattern() {
    // Test with repeating pattern
    let rgba = vec![1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4];
    let rgb = convert_rgba_to_rgb(&rgba, 4, 1);

    // Should have 4 pixels worth of RGB (12 bytes)
    assert_eq!(rgb.len(), 12);
    assert_eq!(rgb, vec![1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 3]);
  }

  #[test]
  fn test_rgba_to_rgb_varying_values() {
    let rgba = vec![
      10, 20, 30, 255, 40, 50, 60, 200, 70, 80, 90, 150, 100, 110, 120, 100,
    ];

    let rgb = convert_rgba_to_rgb(&rgba, 4, 1);

    assert_eq!(rgb, vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120]);
  }

  #[test]
  fn test_rgba_to_rgb_1920x1080() {
    // Test with actual screenshot resolution
    let width = 1920;
    let height = 1080;
    let pixel_count = (width * height) as usize;
    let rgba = vec![128u8; pixel_count * 4];

    let rgb = convert_rgba_to_rgb(&rgba, width, height);

    assert_eq!(rgb.len(), pixel_count * 3);
    assert_eq!(rgb[0], 128);
    assert_eq!(rgb[rgb.len() - 1], 128);
  }

  #[test]
  fn test_rgba_to_rgb_direct_exact_size() {
    let rgba = vec![100, 101, 102, 103, 104, 105, 106, 107];
    let mut rgb = vec![0u8; 6];

    convert_rgba_to_rgb_direct(&rgba, &mut rgb);

    assert_eq!(rgb.len(), 6);
    assert_eq!(rgb[0], 100);
    assert_eq!(rgb[5], 106);
  }

  #[test]
  fn test_convert_rgba_to_rgb_preserves_order() {
    let rgba = vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255];

    let rgb = convert_rgba_to_rgb(&rgba, 3, 1);

    assert_eq!(rgb[0], 1); // First R
    assert_eq!(rgb[1], 2); // First G
    assert_eq!(rgb[2], 3); // First B
    assert_eq!(rgb[3], 4); // Second R
    assert_eq!(rgb[8], 9); // Third B
  }
}
