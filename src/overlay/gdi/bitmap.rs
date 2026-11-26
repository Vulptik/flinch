//! RGBA screenshot buffer → GDI HBITMAP
//!
//! GDI DIBs are BGRA (blue-first). Screenshot capture produces RGBA.
//! `ScreenshotBitmap` owns the HBITMAP and deletes it on drop.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use windows::Win32::Graphics::Gdi::BI_RGB;
use windows::Win32::Graphics::Gdi::{
  BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
  DeleteDC, DeleteObject, GetDIBits, HBITMAP, HDC, HGDIOBJ, SRCCOPY, SelectObject,
};

/// Converts RGBA pixel data to BGRA (Windows DIB format).
/// Each pixel: swap bytes 0 (R) and 2 (B), keep 1 (G) and 3 (A).
pub fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
  let mut bgra = rgba.to_vec();
  for chunk in bgra.as_chunks_mut::<4>().0 {
    chunk.swap(0, 2);
  }
  bgra
}

/// A GDI bitmap created from a screenshot RGBA buffer.
/// Owns the HBITMAP and releases it on drop.
pub struct ScreenshotBitmap {
  pub hbitmap: HBITMAP,
  pub width: i32,
  pub height: i32,
  pub dc: HDC,
  old_bmp: HGDIOBJ,
}

impl ScreenshotBitmap {
  /// Read back the thread-owned DIB after drawing. GDI does not preserve alpha.
  pub fn to_rgba(&mut self) -> Result<Vec<u8>, String> {
    self.to_rgba_into(Vec::new())
  }

  /// Reuse a caller-owned buffer for validated GDI readback and in-place BGRA conversion.
  pub fn to_rgba_into(&mut self, mut bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let size = self.width as usize * self.height as usize * 4;
    bytes.resize(size, 0);
    let mut info = BITMAPINFO {
      bmiHeader: BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: self.width,
        biHeight: -self.height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
      },
      ..Default::default()
    };
    // SAFETY: live DC/bitmap, buffer sized to the DIB. GetDIBits requires an unselected bitmap.
    let rows = unsafe {
      SelectObject(self.dc, self.old_bmp);
      let rows = GetDIBits(
        self.dc,
        self.hbitmap,
        0,
        self.height as u32,
        Some(bytes.as_mut_ptr().cast()),
        &mut info,
        DIB_RGB_COLORS,
      );
      SelectObject(self.dc, self.hbitmap.into());
      rows
    };
    if rows != self.height {
      return Err(format!(
        "Screenshot bitmap readback failed: {rows}/{} rows",
        self.height
      ));
    }
    for pixel in bytes.as_chunks_mut::<4>().0 {
      pixel.swap(0, 2);
      pixel[3] = 255;
    }
    Ok(bytes)
  }

  /// Create from RGBA pixel data. Performs R↔B channel swap.
  pub fn from_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Self, String> {
    Self::from_rgba_with(rgba, width, height, |dc, bmi, bits| {
      // SAFETY: dimensions and output storage are validated by the owning constructor.
      unsafe { CreateDIBSection(Some(dc), bmi, DIB_RGB_COLORS, bits, None, 0) }
    })
  }

  fn from_rgba_with(
    rgba: &[u8],
    width: u32,
    height: u32,
    create: impl FnOnce(HDC, &BITMAPINFO, *mut *mut std::ffi::c_void) -> windows::core::Result<HBITMAP>,
  ) -> Result<Self, String> {
    let w = i32::try_from(width).map_err(|_| "ScreenshotBitmap width exceeds native limits")?;
    let h = i32::try_from(height).map_err(|_| "ScreenshotBitmap height exceeds native limits")?;
    if w == 0 || h == 0 {
      return Err("ScreenshotBitmap dimensions must be positive".into());
    }
    let expected = (width as usize)
      .checked_mul(height as usize)
      .and_then(|n| n.checked_mul(4))
      .ok_or("ScreenshotBitmap dimensions overflow its buffer size")?;
    if rgba.len() != expected {
      return Err(crate::error::rich_display_context(
        "ScreenshotBitmap RGBA buffer size mismatch",
        format!("expected {} bytes, got {}", expected, rgba.len()),
      ));
    }

    // SAFETY: validated positive native dimensions and exact RGBA length bound every copy.
    unsafe {
      let dc = CreateCompatibleDC(None);
      if dc.is_invalid() {
        return Err("CreateCompatibleDC failed for screenshot bitmap".to_string());
      }

      let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
          biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
          biWidth: w,
          biHeight: -h, // negative = top-down
          biPlanes: 1,
          biBitCount: 32,
          biCompression: BI_RGB.0,
          biSizeImage: 0,
          biXPelsPerMeter: 0,
          biYPelsPerMeter: 0,
          biClrUsed: 0,
          biClrImportant: 0,
        },
        ..Default::default()
      };

      let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
      let hbitmap = match create(dc, &bmi, &mut bits_ptr) {
        Ok(bitmap) => bitmap,
        Err(error) => {
          if !DeleteDC(dc).as_bool() {
            tracing::warn!("Failed to release screenshot DC after DIB allocation failure");
          }
          return Err(crate::error::rich_display_context(
            "CreateDIBSection failed",
            error,
          ));
        },
      };

      if bits_ptr.is_null() {
        let _ = DeleteObject(hbitmap.into());
        let _ = DeleteDC(dc);
        return Err("CreateDIBSection returned null bits pointer".to_string());
      }

      copy_rgba_into_bgra_dib(rgba, bits_ptr as *mut u8);

      let old_bmp = SelectObject(dc, hbitmap.into());

      Ok(Self {
        hbitmap,
        width: w,
        height: h,
        dc,
        old_bmp,
      })
    }
  }

  /// Blit this screenshot into `dest_dc` at (0, 0), full size.
  pub fn blit_to(&self, dest_dc: HDC) -> Result<(), String> {
    unsafe {
      BitBlt(
        dest_dc,
        0,
        0,
        self.width,
        self.height,
        Some(self.dc),
        0,
        0,
        SRCCOPY,
      )
      .map_err(|e| crate::error::rich_display_context("BitBlt screenshot failed", e))
    }
  }
}

fn copy_rgba_into_bgra_dib(rgba: &[u8], dst: *mut u8) {
  #[cfg(target_arch = "x86_64")]
  {
    if is_x86_feature_detected!("avx2") {
      unsafe {
        copy_rgba_into_bgra_dib_avx2(rgba, dst);
      }
      return;
    }

    if is_x86_feature_detected!("ssse3") {
      unsafe {
        copy_rgba_into_bgra_dib_ssse3(rgba, dst);
      }
      return;
    }
  }

  copy_rgba_into_bgra_dib_scalar(rgba, dst);
}

fn copy_rgba_into_bgra_dib_scalar(rgba: &[u8], dst: *mut u8) {
  let pixels = rgba.len() / 4;

  unsafe {
    let src = rgba.as_ptr();

    for idx in 0..pixels {
      let offset = idx * 4;
      let pixel = std::ptr::read_unaligned(src.add(offset) as *const u32);
      let swapped =
        (pixel & 0xFF00_FF00) | ((pixel & 0x00FF_0000) >> 16) | ((pixel & 0x0000_00FF) << 16);
      std::ptr::write_unaligned(dst.add(offset) as *mut u32, swapped);
    }
  }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn copy_rgba_into_bgra_dib_ssse3(rgba: &[u8], dst: *mut u8) {
  let simd_len = rgba.len() / 16 * 16;
  let shuffle_mask = _mm_setr_epi8(2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15);

  for offset in (0..simd_len).step_by(16) {
    unsafe {
      let pixels = _mm_loadu_si128(rgba.as_ptr().add(offset) as *const __m128i);
      let shuffled = _mm_shuffle_epi8(pixels, shuffle_mask);
      _mm_storeu_si128(dst.add(offset) as *mut __m128i, shuffled);
    }
  }

  copy_rgba_into_bgra_dib_scalar(&rgba[simd_len..], unsafe { dst.add(simd_len) });
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_rgba_into_bgra_dib_avx2(rgba: &[u8], dst: *mut u8) {
  let simd_len = rgba.len() / 32 * 32;
  let shuffle_mask = _mm256_setr_epi8(
    2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, 18, 17, 16, 19, 22, 21, 20, 23, 26, 25,
    24, 27, 30, 29, 28, 31,
  );

  for offset in (0..simd_len).step_by(32) {
    unsafe {
      let pixels = _mm256_loadu_si256(rgba.as_ptr().add(offset) as *const __m256i);
      let shuffled = _mm256_shuffle_epi8(pixels, shuffle_mask);
      _mm256_storeu_si256(dst.add(offset) as *mut __m256i, shuffled);
    }
  }

  copy_rgba_into_bgra_dib_scalar(&rgba[simd_len..], unsafe { dst.add(simd_len) });
}

impl Drop for ScreenshotBitmap {
  fn drop(&mut self) {
    unsafe {
      let _ = SelectObject(self.dc, self.old_bmp); // deselect hbitmap first
      let _ = DeleteDC(self.dc);
      let _ = DeleteObject(self.hbitmap.into());
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bitmap_allocation_failure_releases_its_owned_dc() {
    let mut allocated = HDC::default();
    let result = ScreenshotBitmap::from_rgba_with(&[1, 2, 3, 255], 1, 1, |dc, _, _| {
      allocated = dc;
      Err(windows::core::Error::from_hresult(
        windows::Win32::Foundation::E_OUTOFMEMORY,
      ))
    });
    assert!(result.is_err());
    // SAFETY: no intervening GDI allocation can reuse this fixture's handle.
    // GetObjectType may return a cached DC type after deletion; DeleteDC must
    // reject an already released DC and cleans a leak before reporting failure.
    let live = unsafe { DeleteDC(allocated) }.as_bool();
    assert!(!live, "failed DIB allocation leaked its compatible DC");
  }

  #[test]
  fn overflowing_bitmap_dimensions_return_an_error_before_native_allocation() {
    let worker =
      std::thread::spawn(|| ScreenshotBitmap::from_rgba(&[], u32::MAX, u32::MAX).is_err());
    assert!(worker.join().expect("invalid dimensions must not panic"));
  }

  fn copy_into_vec_with_scalar(rgba: &[u8]) -> Vec<u8> {
    let mut dst = vec![0_u8; rgba.len()];
    copy_rgba_into_bgra_dib_scalar(rgba, dst.as_mut_ptr());
    dst
  }

  fn copy_into_vec_with_auto(rgba: &[u8]) -> Vec<u8> {
    let mut dst = vec![0_u8; rgba.len()];
    copy_rgba_into_bgra_dib(rgba, dst.as_mut_ptr());
    dst
  }

  #[test]
  fn test_rgba_to_bgra_swap() {
    let rgba = vec![0xFF_u8, 0x00, 0x80, 0xFF];
    let bgra = rgba_to_bgra(&rgba);
    assert_eq!(bgra, vec![0x80_u8, 0x00, 0xFF, 0xFF]);
  }

  #[test]
  fn test_rgba_to_bgra_multiple_pixels() {
    let rgba = vec![0x01, 0x02, 0x03, 0xFF, 0x10, 0x20, 0x30, 0xFF];
    let bgra = rgba_to_bgra(&rgba);
    assert_eq!(bgra[0], 0x03);
    assert_eq!(bgra[1], 0x02);
    assert_eq!(bgra[2], 0x01);
    assert_eq!(bgra[4], 0x30);
  }

  #[test]
  fn test_copy_rgba_into_bgra_dib_scalar_matches_vec_conversion() {
    let rgba = vec![
      0x01, 0x02, 0x03, 0xFE, 0x10, 0x20, 0x30, 0x80, 0xAA, 0xBB, 0xCC, 0xDD,
    ];
    let copied = copy_into_vec_with_scalar(&rgba);
    assert_eq!(copied, rgba_to_bgra(&rgba));
  }

  #[test]
  fn test_copy_rgba_into_bgra_dib_auto_matches_scalar_for_unaligned_length() {
    let rgba: Vec<u8> = (0..(19 * 4)).map(|i| ((i * 37) % 256) as u8).collect();
    let auto = copy_into_vec_with_auto(&rgba);
    let scalar = copy_into_vec_with_scalar(&rgba);
    assert_eq!(auto, scalar);
  }

  #[test]
  fn test_copy_rgba_into_bgra_dib_auto_matches_scalar_for_large_buffer() {
    let rgba: Vec<u8> = (0..(4097 * 4))
      .map(|i| ((i * 13 + 17) % 256) as u8)
      .collect();
    let auto = copy_into_vec_with_auto(&rgba);
    let scalar = copy_into_vec_with_scalar(&rgba);
    assert_eq!(auto, scalar);
  }
}
