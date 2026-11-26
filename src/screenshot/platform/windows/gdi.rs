//! GDI fallback capture. All modes share one checked, RAII-owned DIB copy path.
use super::conversion::convert_bgra_to_rgba_optimized_direct;
use crate::overlay::gdi::layered::LayeredBitmap;
use anyhow::{Context, Result};
use windows::Win32::{
  Foundation::{HWND, RECT},
  Graphics::{
    Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
    Gdi::{BitBlt, GdiFlush, GetDC, HDC, ReleaseDC, SRCCOPY},
  },
  UI::WindowsAndMessaging::{
    GetDesktopWindow, GetForegroundWindow, GetSystemMetrics, GetWindowRect, IsIconic, IsWindow,
    SM_CXSCREEN, SM_CYSCREEN,
  },
};

struct ScreenDc {
  hwnd: Option<HWND>,
  dc: HDC,
}
impl ScreenDc {
  fn acquire(hwnd: Option<HWND>) -> Result<Self> {
    // SAFETY: borrowed DC is released exactly once on this thread by Drop.
    let dc = unsafe { GetDC(hwnd) };
    anyhow::ensure!(!dc.is_invalid(), "Failed to get desktop DC");
    Ok(Self { hwnd, dc })
  }
}
impl Drop for ScreenDc {
  fn drop(&mut self) {
    // SAFETY: pairs this thread's successful GetDC, including all error paths.
    if unsafe { ReleaseDC(self.hwnd, self.dc) } == 0 {
      tracing::warn!("Failed to release capture screen DC");
    }
  }
}

/// Copy a supplied source DC. Tests supply an owned synthetic DIB, never the desktop.
fn copy_dc_region_into(
  output: &mut Vec<u8>,
  source: HDC,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
) -> Result<()> {
  anyhow::ensure!(
    width > 0 && height > 0,
    "GDI capture: zero-sized region ({width}x{height})"
  );
  let w = i32::try_from(width).context("GDI capture width exceeds Windows bitmap dimensions")?;
  let h = i32::try_from(height).context("GDI capture height exceeds Windows bitmap dimensions")?;
  let mut bitmap = LayeredBitmap::new(w, h)
    .map_err(anyhow::Error::msg)
    .context("Failed to create capture DIB")?;
  // SAFETY: source and destination DCs stay live on this thread throughout the copy.
  unsafe { BitBlt(bitmap.dc, 0, 0, w, h, Some(source), x, y, SRCCOPY) }
    .context("GDI capture BitBlt failed")?;
  // SAFETY: flush pending writes before accessing this thread-owned DIB's pixels.
  unsafe { GdiFlush() }
    .ok()
    .context("GDI capture flush failed")?;
  let pixels = bitmap.pixels_mut();
  output.resize(pixels.len(), 0);
  convert_bgra_to_rgba_optimized_direct(pixels, output);
  Ok(())
}

/// Capture primary-desktop pixels into RGBA using the shared DIB copy path.
pub unsafe fn capture_with_dib(init_thread_pool: impl Fn()) -> Result<(u32, u32, Vec<u8>)> {
  init_thread_pool();
  // SAFETY: system metric and desktop handle queries have no pointer arguments.
  let (hwnd, width, height) = unsafe {
    (
      GetDesktopWindow(),
      GetSystemMetrics(SM_CXSCREEN),
      GetSystemMetrics(SM_CYSCREEN),
    )
  };
  let width = u32::try_from(width).context("Invalid primary monitor width")?;
  let height = u32::try_from(height).context("Invalid primary monitor height")?;
  let dc = ScreenDc::acquire(Some(hwnd))?;
  let mut output = Vec::new();
  copy_dc_region_into(&mut output, dc.dc, 0, 0, width, height)?;
  Ok((width, height, output))
}

/// GetDC(None) spans the complete virtual desktop, including negative origins.
pub fn capture_virtual_desktop_into(
  output: &mut Vec<u8>,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
) -> Result<()> {
  anyhow::ensure!(
    width > 0 && height > 0,
    "capture_virtual_desktop_into: zero-sized region ({width}x{height})"
  );
  let dc = ScreenDc::acquire(None)?;
  copy_dc_region_into(output, dc.dc, x, y, width, height)
}

/// Capture visible foreground bounds, falling back to primary-screen capture
/// when Windows cannot provide a usable foreground rectangle.
pub unsafe fn capture_active_window(init_thread_pool: impl Fn()) -> Result<(u32, u32, Vec<u8>)> {
  init_thread_pool();
  // SAFETY: query current foreground handle without modifying activation.
  let hwnd = unsafe { GetForegroundWindow() };
  // SAFETY: validity and minimization queries accept a borrowed HWND.
  if hwnd.is_invalid()
    || !unsafe { IsWindow(Some(hwnd)) }.as_bool()
    || unsafe { IsIconic(hwnd) }.as_bool()
  {
    tracing::warn!("No usable foreground window; falling back to primary screen");
    // SAFETY: this function has the same capture preconditions as its fallback.
    return unsafe { capture_with_dib(init_thread_pool) };
  }
  let mut bounds = RECT::default();
  // SAFETY: DWM fills the live RECT for the supplied borrowed window.
  let dwm = unsafe {
    DwmGetWindowAttribute(
      hwnd,
      DWMWA_EXTENDED_FRAME_BOUNDS,
      (&mut bounds as *mut RECT).cast(),
      std::mem::size_of::<RECT>() as u32,
    )
  };
  if let Err(error) = dwm {
    tracing::debug!(%error,"DWM frame bounds unavailable; using window rectangle");
    // SAFETY: bounds is a live writable RECT.
    if let Err(error) = unsafe { GetWindowRect(hwnd, &mut bounds) } {
      tracing::warn!(%error,"Foreground bounds unavailable; falling back to primary screen");
      // SAFETY: same capture preconditions as this entry point.
      return unsafe { capture_with_dib(init_thread_pool) };
    }
  }
  let Some((width, height)) = positive_bounds(bounds) else {
    tracing::warn!("Foreground rectangle is empty or inverted; falling back to primary screen");
    // SAFETY: same capture preconditions as this entry point.
    return unsafe { capture_with_dib(init_thread_pool) };
  };
  // SAFETY: GetDesktopWindow is a borrowed process-independent handle.
  let dc = ScreenDc::acquire(Some(unsafe { GetDesktopWindow() }))?;
  let mut output = Vec::new();
  copy_dc_region_into(&mut output, dc.dc, bounds.left, bounds.top, width, height)?;
  Ok((width, height, output))
}

fn positive_bounds(bounds: RECT) -> Option<(u32, u32)> {
  let width = bounds.right.checked_sub(bounds.left)?;
  let height = bounds.bottom.checked_sub(bounds.top)?;
  (width > 0 && height > 0).then_some((width as u32, height as u32))
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn synthetic_gdi_copy_preserves_crop_rows_colors_and_reuses_output() {
    let mut source = LayeredBitmap::new(16, 12).expect("synthetic source");
    for y in 0..12 {
      for x in 0..16 {
        source.set_pixel(x, y, x as u8, y as u8, 120, 255);
      }
    }
    let mut output = Vec::with_capacity(16 * 12 * 4);
    let pointer = output.as_ptr();
    for (x, y, w, h) in [(0, 0, 16, 12), (3, 2, 7, 5), (15, 11, 1, 1)] {
      copy_dc_region_into(&mut output, source.dc, x, y, w, h).expect("offscreen copy");
      assert_eq!(output.as_ptr(), pointer, "output allocation must be reused");
      assert_eq!(output.len(), (w * h * 4) as usize);
      for row in 0..h {
        for col in 0..w {
          let i = ((row * w + col) * 4) as usize;
          assert_eq!(
            &output[i..i + 4],
            &[120, (y + row as i32) as u8, (x + col as i32) as u8, 255]
          );
        }
      }
    }
  }
  #[test]
  fn invalid_gdi_inputs_cannot_mutate_output_or_overflow_bitmap_sizes() {
    let mut output = vec![1, 2, 3, 4];
    for (w, h) in [
      (0, 10),
      (10, 0),
      (u32::MAX, 2),
      (2, u32::MAX),
      (65536, 65536),
    ] {
      let result = copy_dc_region_into(&mut output, HDC::default(), 0, 0, w, h);
      assert!(result.is_err());
      assert_eq!(output, [1, 2, 3, 4]);
    }
    assert!(
      copy_dc_region_into(&mut output, HDC::default(), 0, 0, 2, 2)
        .unwrap_err()
        .to_string()
        .contains("BitBlt")
    );
  }
  #[test]
  fn foreground_bounds_reject_empty_inverted_and_overflowed_rectangles() {
    assert_eq!(
      positive_bounds(RECT {
        left: -1920,
        top: -100,
        right: -1800,
        bottom: 100
      }),
      Some((120, 200))
    );
    for r in [
      RECT::default(),
      RECT {
        left: 20,
        top: 0,
        right: 10,
        bottom: 20,
      },
      RECT {
        left: i32::MIN,
        top: 0,
        right: i32::MAX,
        bottom: 20,
      },
    ] {
      assert!(positive_bounds(r).is_none());
    }
  }
}
