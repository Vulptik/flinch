// src/screenshot/platform/windows/conversion.rs
// SIMD-optimized BGRA to RGBA color space conversion

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// In-place BGRA→RGBA conversion using Rayon. Safe to call with src and dst being the same buffer.
/// Uses raw pointers per chunk to satisfy borrow rules; each chunk is independent.
#[inline(always)]
pub fn convert_bgra_to_rgba_in_place_rayon(buf: &mut [u8]) {
  use rayon::prelude::*;
  const CHUNK: usize = 16384; // 16KB, multiple of 4
  buf.par_chunks_mut(CHUNK).for_each(|chunk| {
    let n = chunk.len() / 4;
    let ptr = chunk.as_mut_ptr();
    for i in 0..n {
      let o = i * 4;
      let r = unsafe { *ptr.add(o + 2) };
      let g = unsafe { *ptr.add(o + 1) };
      let b = unsafe { *ptr.add(o) };
      let a = unsafe { *ptr.add(o + 3) };
      unsafe {
        *ptr.add(o) = r;
        *ptr.add(o + 1) = g;
        *ptr.add(o + 2) = b;
        *ptr.add(o + 3) = a;
      }
    }
  });
}

/// Direct conversion without buffer management
#[inline(always)]
pub fn convert_bgra_to_rgba_optimized_direct(bgra: &[u8], rgba: &mut [u8]) {
  // Use parallel processing with Rayon (pre-warmed thread pool)
  use rayon::prelude::*;

  // Ultra-aggressive chunking: 16KB per chunk for maximum L1 cache locality
  // L1 data cache is typically 32KB, so 16KB chunks fit with room for working data
  const CHUNK_SIZE: usize = 16384; // 16KB chunks = 4096 pixels

  // Process in parallel with SIMD
  rgba
    .par_chunks_mut(CHUNK_SIZE)
    .enumerate()
    .for_each(|(chunk_idx, dst_chunk)| {
      let offset = chunk_idx * CHUNK_SIZE;
      let src_slice = &bgra[offset..offset + dst_chunk.len()];

      #[cfg(target_arch = "x86_64")]
      {
        if is_x86_feature_detected!("avx2") {
          unsafe {
            convert_bgra_to_rgba_avx2(src_slice, dst_chunk);
          }
        } else if is_x86_feature_detected!("sse2") {
          unsafe {
            convert_bgra_to_rgba_sse2(src_slice, dst_chunk);
          }
        } else {
          convert_bgra_to_rgba_scalar(src_slice, dst_chunk);
        }
      }

      #[cfg(not(target_arch = "x86_64"))]
      {
        convert_bgra_to_rgba_scalar(src_slice, dst_chunk);
      }
    });
}

/// SSE2 SIMD conversion - processes 4 pixels (16 bytes) at once
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[inline]
pub unsafe fn convert_bgra_to_rgba_sse2(src: &[u8], dst: &mut [u8]) {
  let pixels = src.len() / 4;
  let simd_pixels = pixels / 4; // Process 4 pixels per iteration
  let remainder = pixels % 4;

  let src_ptr = src.as_ptr();
  let dst_ptr = dst.as_mut_ptr();

  // Shuffle mask for BGRA -> RGBA: [2,1,0,3, 6,5,4,7, 10,9,8,11, 14,13,12,15]
  let shuffle_mask = _mm_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);

  for i in 0..simd_pixels {
    let offset = i * 16;

    unsafe {
      // Load 16 bytes (4 BGRA pixels)
      let pixels = _mm_loadu_si128(src_ptr.add(offset) as *const __m128i);

      // Shuffle to swap B and R channels
      let shuffled = _mm_shuffle_epi8(pixels, shuffle_mask);

      // Store result
      _mm_storeu_si128(dst_ptr.add(offset) as *mut __m128i, shuffled);
    }
  }

  // Handle remaining pixels
  let base = simd_pixels * 16;
  for i in 0..remainder {
    let offset = base + i * 4;
    unsafe {
      *dst_ptr.add(offset) = *src_ptr.add(offset + 2); // R
      *dst_ptr.add(offset + 1) = *src_ptr.add(offset + 1); // G
      *dst_ptr.add(offset + 2) = *src_ptr.add(offset); // B
      *dst_ptr.add(offset + 3) = *src_ptr.add(offset + 3); // A
    }
  }
}

/// AVX2 SIMD conversion - processes 8 pixels (32 bytes) at once
/// Optimized for maximum throughput
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
pub unsafe fn convert_bgra_to_rgba_avx2(src: &[u8], dst: &mut [u8]) {
  let pixels = src.len() / 4;
  let simd_pixels = pixels / 8; // Process 8 pixels per iteration
  let remainder = pixels % 8;

  let src_ptr = src.as_ptr();
  let dst_ptr = dst.as_mut_ptr();

  // Shuffle mask for BGRA -> RGBA (repeated for 2 sets of 4 pixels)
  let shuffle_mask = _mm256_setr_epi8(
    2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, 2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14,
    13, 12, 15,
  );

  for i in 0..simd_pixels {
    let offset = i * 32;

    unsafe {
      // Load 32 bytes (8 BGRA pixels)
      let pixels = _mm256_loadu_si256(src_ptr.add(offset) as *const __m256i);

      // Shuffle to swap B and R channels
      let shuffled = _mm256_shuffle_epi8(pixels, shuffle_mask);

      // Store result
      _mm256_storeu_si256(dst_ptr.add(offset) as *mut __m256i, shuffled);
    }
  }

  // Handle remaining pixels with SSE2
  let base = simd_pixels * 32;
  if remainder >= 4 {
    let shuffle_mask_sse = _mm_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);

    unsafe {
      let pixels = _mm_loadu_si128(src_ptr.add(base) as *const __m128i);
      let shuffled = _mm_shuffle_epi8(pixels, shuffle_mask_sse);
      _mm_storeu_si128(dst_ptr.add(base) as *mut __m128i, shuffled);
    }
  }

  // Handle final 0-3 pixels
  let final_base = base + ((remainder / 4) * 16);
  let final_remainder = remainder % 4;
  for i in 0..final_remainder {
    let offset = final_base + i * 4;
    unsafe {
      *dst_ptr.add(offset) = *src_ptr.add(offset + 2); // R
      *dst_ptr.add(offset + 1) = *src_ptr.add(offset + 1); // G
      *dst_ptr.add(offset + 2) = *src_ptr.add(offset); // B
      *dst_ptr.add(offset + 3) = *src_ptr.add(offset + 3); // A
    }
  }
}

/// Scalar fallback for non-x86_64 or when SIMD not available
#[inline(always)]
pub fn convert_bgra_to_rgba_scalar(src: &[u8], dst: &mut [u8]) {
  let pixels = src.len() / 4;

  unsafe {
    let src_ptr = src.as_ptr();
    let dst_ptr = dst.as_mut_ptr();

    for i in 0..pixels {
      let offset = i * 4;
      *dst_ptr.add(offset) = *src_ptr.add(offset + 2); // R
      *dst_ptr.add(offset + 1) = *src_ptr.add(offset + 1); // G
      *dst_ptr.add(offset + 2) = *src_ptr.add(offset); // B
      *dst_ptr.add(offset + 3) = *src_ptr.add(offset + 3); // A
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Scalar conversion tests (safe, no SIMD, runs on all platforms)

  #[test]
  fn test_scalar_single_pixel() {
    // BGRA: B=10, G=20, R=30, A=255
    let src = [10u8, 20, 30, 255];
    let mut dst = [0u8; 4];
    convert_bgra_to_rgba_scalar(&src, &mut dst);
    // RGBA: R=30, G=20, B=10, A=255
    assert_eq!(dst[0], 30); // R
    assert_eq!(dst[1], 20); // G
    assert_eq!(dst[2], 10); // B
    assert_eq!(dst[3], 255); // A preserved
  }

  #[test]
  fn test_scalar_preserves_alpha() {
    let src = [100u8, 150, 200, 128]; // semi-transparent
    let mut dst = [0u8; 4];
    convert_bgra_to_rgba_scalar(&src, &mut dst);
    assert_eq!(dst[3], 128);
  }

  #[test]
  fn test_scalar_fully_transparent() {
    let src = [255u8, 0, 0, 0]; // blue, transparent
    let mut dst = [0u8; 4];
    convert_bgra_to_rgba_scalar(&src, &mut dst);
    assert_eq!(dst[0], 0); // R (was B in source)
    assert_eq!(dst[2], 255); // B (was B=255 in source)
    assert_eq!(dst[3], 0); // A = 0
  }

  #[test]
  fn test_scalar_multiple_pixels() {
    // Two pixels: BGRA format
    let src = [
      10u8, 20, 30, 255, // pixel 1: B=10, G=20, R=30, A=255
      40, 50, 60, 200, // pixel 2: B=40, G=50, R=60, A=200
    ];
    let mut dst = [0u8; 8];
    convert_bgra_to_rgba_scalar(&src, &mut dst);
    // Pixel 1: R=30, G=20, B=10, A=255
    assert_eq!(dst[0], 30);
    assert_eq!(dst[1], 20);
    assert_eq!(dst[2], 10);
    assert_eq!(dst[3], 255);
    // Pixel 2: R=60, G=50, B=40, A=200
    assert_eq!(dst[4], 60);
    assert_eq!(dst[5], 50);
    assert_eq!(dst[6], 40);
    assert_eq!(dst[7], 200);
  }

  #[test]
  fn test_scalar_green_channel_unchanged() {
    // G channel is at offset+1 in both BGRA and RGBA
    let src = [0u8, 255, 0, 255]; // pure green in BGRA
    let mut dst = [0u8; 4];
    convert_bgra_to_rgba_scalar(&src, &mut dst);
    assert_eq!(dst[1], 255); // G stays at index 1
  }

  #[test]
  fn test_scalar_black_pixel() {
    let src = [0u8, 0, 0, 255];
    let mut dst = [0u8; 4];
    convert_bgra_to_rgba_scalar(&src, &mut dst);
    assert_eq!(dst, [0, 0, 0, 255]);
  }

  #[test]
  fn test_scalar_white_pixel() {
    let src = [255u8, 255, 255, 255];
    let mut dst = [0u8; 4];
    convert_bgra_to_rgba_scalar(&src, &mut dst);
    assert_eq!(dst, [255, 255, 255, 255]);
  }

  #[test]
  fn test_scalar_empty_buffer() {
    let src: [u8; 0] = [];
    let mut dst: [u8; 0] = [];
    convert_bgra_to_rgba_scalar(&src, &mut dst);
    // No panic, no crash
  }

  #[test]
  fn test_in_place_rayon_single_pixel() {
    let mut buf = [10u8, 20, 30, 255]; // BGRA
    convert_bgra_to_rgba_in_place_rayon(&mut buf);
    assert_eq!(buf[0], 30); // R
    assert_eq!(buf[1], 20); // G
    assert_eq!(buf[2], 10); // B
    assert_eq!(buf[3], 255); // A
  }

  #[test]
  fn test_in_place_rayon_idempotent_gray() {
    // Gray pixel: R=G=B, so swap is idempotent
    let original = [128u8, 128, 128, 255];
    let mut buf = original;
    convert_bgra_to_rgba_in_place_rayon(&mut buf);
    assert_eq!(buf, original);
  }

  #[test]
  fn test_in_place_rayon_double_swap_restores() {
    // Two conversions should restore original BGRA
    let original = [10u8, 20, 30, 255];
    let mut buf = original;
    convert_bgra_to_rgba_in_place_rayon(&mut buf);
    convert_bgra_to_rgba_in_place_rayon(&mut buf);
    assert_eq!(buf, original);
  }

  #[test]
  fn test_scalar_and_in_place_agree() {
    // Scalar and in-place should produce the same result
    let bgra = [42u8, 84, 128, 200, 10, 20, 30, 100];
    let mut dst = bgra;
    let mut dst_inplace = bgra;

    convert_bgra_to_rgba_scalar(&bgra, &mut dst);
    convert_bgra_to_rgba_in_place_rayon(&mut dst_inplace);

    assert_eq!(dst, dst_inplace);
  }

  #[test]
  fn test_optimized_direct_single_pixel() {
    let src = [10u8, 20, 30, 255];
    let mut dst = vec![0u8; 4];
    convert_bgra_to_rgba_optimized_direct(&src, &mut dst);
    assert_eq!(dst[0], 30); // R
    assert_eq!(dst[1], 20); // G
    assert_eq!(dst[2], 10); // B
    assert_eq!(dst[3], 255); // A
  }

  #[test]
  fn test_optimized_matches_scalar_for_large_buffer() {
    // Generate a pseudo-random buffer (deterministic)
    let size = 1024 * 4; // 1024 pixels
    let src: Vec<u8> = (0..size).map(|i| (i * 37 % 256) as u8).collect();

    let mut dst_scalar = src.clone();
    let mut dst_optimized = src.clone();

    convert_bgra_to_rgba_scalar(&src, &mut dst_scalar);
    convert_bgra_to_rgba_optimized_direct(&src, &mut dst_optimized);

    assert_eq!(dst_scalar, dst_optimized);
  }
}
