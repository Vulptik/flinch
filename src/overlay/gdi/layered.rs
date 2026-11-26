//! UpdateLayeredWindow wrapper for the recording HUD.
//!
//! The HUD uses a 32-bit pre-multiplied ARGB HBITMAP. GDI ignores the alpha
//! channel when drawing, so semi-transparent shapes require direct pixel writes.
//! All pixel data must be pre-multiplied before passing to UpdateLayeredWindow.

use crate::error::rich_display_context;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
  AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
  CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
  HGDIOBJ, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};

/// Compute pre-multiplied (B, G, R, A) from raw values.
/// Windows ARGB bitmaps store pixels as BGRA in memory.
pub fn premul_argb(b: u8, g: u8, r: u8, a: u8) -> (u8, u8, u8, u8) {
  let scale = |c: u8| ((c as u16 * a as u16) / 255) as u8;
  (scale(b), scale(g), scale(r), a)
}

/// Zero all bytes in a pixel buffer (set all pixels to fully transparent).
pub fn clear_pixel_buffer(buf: &mut [u8]) {
  buf.fill(0);
}

/// A 32-bit ARGB GDI bitmap for use with UpdateLayeredWindow.
/// Exposes raw pixel memory for direct BGRA writes (pre-multiplied alpha).
pub struct LayeredBitmap {
  pub hbitmap: HBITMAP,
  pub dc: HDC,
  old_bmp: HGDIOBJ,
  bits: *mut u8,
  pub width: i32,
  pub height: i32,
}

impl LayeredBitmap {
  /// Allocate a blank (transparent) layered bitmap of the given size.
  pub fn new(width: i32, height: i32) -> Result<Self, String> {
    if width <= 0 || height <= 0 || width as u64 * height as u64 * 4 > i32::MAX as u64 {
      return Err("LayeredBitmap: dimensions exceed pixel buffer range".into());
    }
    // SAFETY: validated dimensions fit buffer indexing; selected objects and DC are owned by Self.
    unsafe {
      let dc = CreateCompatibleDC(None);
      if dc.is_invalid() {
        return Err("LayeredBitmap: CreateCompatibleDC failed".to_string());
      }

      let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
          biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
          biWidth: width,
          biHeight: -height, // top-down
          biPlanes: 1,
          biBitCount: 32,
          biCompression: BI_RGB.0,
          ..Default::default()
        },
        ..Default::default()
      };

      let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
      let hbitmap = match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0) {
        Ok(bitmap) => bitmap,
        Err(error) => {
          if !DeleteDC(dc).as_bool() {
            tracing::warn!("Failed to release DC after DIB allocation failure");
          }
          return Err(rich_display_context(
            "LayeredBitmap: CreateDIBSection failed",
            error,
          ));
        },
      };

      if bits_ptr.is_null() {
        let _ = DeleteObject(hbitmap.into());
        let _ = DeleteDC(dc);
        return Err("LayeredBitmap: bits pointer is null".to_string());
      }

      let old_bmp = SelectObject(dc, hbitmap.into());
      let bits = bits_ptr as *mut u8;

      Ok(Self {
        hbitmap,
        dc,
        old_bmp,
        bits,
        width,
        height,
      })
    }
  }

  /// Returns a mutable byte slice over all pixel data (BGRA, row-major).
  pub fn pixels_mut(&mut self) -> &mut [u8] {
    let len = (self.width * self.height * 4) as usize;
    unsafe { std::slice::from_raw_parts_mut(self.bits, len) }
  }

  /// Clear all pixels to fully transparent (ARGB 0,0,0,0).
  pub fn clear(&mut self) {
    clear_pixel_buffer(self.pixels_mut());
  }

  /// Write a pre-multiplied ARGB pixel at (x, y).
  pub fn set_pixel(&mut self, x: i32, y: i32, b: u8, g: u8, r: u8, a: u8) {
    if x < 0 || y < 0 || x >= self.width || y >= self.height {
      return;
    }
    let offset = ((y * self.width + x) * 4) as usize;
    // SAFETY: offset is bounds-checked above
    unsafe {
      *self.bits.add(offset) = b;
      *self.bits.add(offset + 1) = g;
      *self.bits.add(offset + 2) = r;
      *self.bits.add(offset + 3) = a;
    }
  }

  /// Fill a rectangle with a pre-multiplied ARGB color.
  #[allow(clippy::too_many_arguments)]
  pub fn fill_rect(
    &mut self,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    b: u8,
    g: u8,
    r: u8,
    a: u8,
  ) {
    for y in top..bottom {
      for x in left..right {
        self.set_pixel(x, y, b, g, r, a);
      }
    }
  }

  /// Fill a rounded rectangle with a pre-multiplied ARGB color.
  #[allow(clippy::too_many_arguments)]
  pub fn fill_roundrect(
    &mut self,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: i32,
    b: u8,
    g: u8,
    r: u8,
    a: u8,
  ) {
    for py in top..bottom {
      for px in left..right {
        let in_corner_x = (px - left < radius) || (right - px - 1 < radius);
        let in_corner_y = (py - top < radius) || (bottom - py - 1 < radius);
        if in_corner_x && in_corner_y {
          let cx = if px - left < radius {
            left + radius
          } else {
            right - radius
          };
          let cy = if py - top < radius {
            top + radius
          } else {
            bottom - radius
          };
          let dx = (px - cx) as f32;
          let dy = (py - cy) as f32;
          if (dx * dx + dy * dy).sqrt() > radius as f32 {
            continue;
          }
        }
        self.set_pixel(px, py, b, g, r, a);
      }
    }
  }

  /// Apply this bitmap to `hwnd` via UpdateLayeredWindow.
  /// `x`, `y` are the window's screen position.
  pub fn apply_to(&self, hwnd: HWND, x: i32, y: i32) -> Result<(), String> {
    self.apply_with_opacity(hwnd, x, y, 255)
  }

  pub fn apply_with_opacity(&self, hwnd: HWND, x: i32, y: i32, opacity: u8) -> Result<(), String> {
    // SAFETY: this bitmap and live HWND are used by their owning overlay thread.
    unsafe {
      let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: opacity,
        AlphaFormat: AC_SRC_ALPHA as u8,
      };

      let pos = POINT { x, y };
      let size = SIZE {
        cx: self.width,
        cy: self.height,
      };
      let src_pos = POINT { x: 0, y: 0 };

      UpdateLayeredWindow(
        hwnd,
        None,
        Some(&pos),
        Some(&size),
        Some(self.dc),
        Some(&src_pos),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
      )
      .map_err(|e| rich_display_context("UpdateLayeredWindow failed", e))
    }
  }
}

impl Drop for LayeredBitmap {
  fn drop(&mut self) {
    unsafe {
      let _ = SelectObject(self.dc, self.old_bmp); // deselect hbitmap first
      let _ = DeleteDC(self.dc);
      let _ = DeleteObject(self.hbitmap.into());
    }
  }
}

// SAFETY: `bits` is exclusively owned by this struct with no aliasing — safe
// to send between threads from a Rust ownership perspective. However, Win32
// DCs have thread affinity (GDI operations must occur on the creating thread),
// so callers must ensure `self.dc` is only used from the thread that created it.
// `Send` enables moving the struct to the overlay thread before use begins.
unsafe impl Send for LayeredBitmap {}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_premul_color_opaque() {
    let c = premul_argb(255, 255, 255, 255);
    assert_eq!(c, (255u8, 255u8, 255u8, 255u8));
  }

  #[test]
  fn test_premul_color_half_alpha() {
    // 50% alpha red in BGRA: B=0, G=0, R=255, A=128
    let c = premul_argb(0, 0, 255, 128);
    assert_eq!(c.2, 128); // premul R = 255*128/255 = 128
    assert_eq!(c.3, 128); // alpha unchanged
  }

  #[test]
  fn test_clear_sets_all_zero() {
    let mut buf = vec![0xFFu8; 4 * 4];
    clear_pixel_buffer(&mut buf);
    assert!(buf.iter().all(|&b| b == 0));
  }
}
