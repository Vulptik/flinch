//! Shared native overlay palette. RGB here; convert only at the GDI/BGRA boundary.
use windows::Win32::Foundation::COLORREF;

// Panels sit a step above the dimmed capture so they read as raised chrome.
pub const SURFACE: [u8; 3] = [30, 30, 34];
pub const OUTLINE: [u8; 3] = [64, 64, 72];
/// Resting button tile, one step above the surface.
pub const TILE: [u8; 3] = [42, 42, 48];
pub const HOVER: [u8; 3] = [58, 58, 66];
pub const ACTIVE: [u8; 3] = [72, 36, 41];
pub const ACCENT: [u8; 3] = [239, 68, 68];
/// Selected tool or option: a solid accent tile with a white icon or label.
pub const SELECTED: [u8; 3] = ACCENT;
pub const SELECTED_ICON: [u8; 3] = [255, 255, 255];
pub const TEXT: [u8; 3] = [240, 240, 245];
pub const MUTED: [u8; 3] = [150, 150, 161];
pub const SECONDARY_TEXT: [u8; 3] = [174, 174, 185];
pub const DISABLED: [u8; 3] = [100, 100, 110];
pub const ACTIVE_ICON: [u8; 3] = [255, 115, 115];
pub const SEPARATOR: [u8; 3] = [64, 64, 72];

pub const fn colorref(rgb: [u8; 3]) -> COLORREF {
  COLORREF(rgb[0] as u32 | ((rgb[1] as u32) << 8) | ((rgb[2] as u32) << 16))
}

/// Read actual native panel pixels once per overlay thread, without capturing desktop content.
#[cfg(debug_assertions)]
pub fn debug_sample_surface(dc: windows::Win32::Graphics::Gdi::HDC, toolbar: bool, x: i32, y: i32) {
  thread_local! { static SAMPLED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }
  let bit = if toolbar { 2 } else { 1 };
  if SAMPLED.with(|value| value.get() & bit != 0) {
    return;
  }
  // SAFETY: caller supplies the live rendering DC owned by this overlay thread.
  let pixel = unsafe { windows::Win32::Graphics::Gdi::GetPixel(dc, x, y) };
  if pixel.0 == u32::MAX {
    tracing::warn!(toolbar, "Could not sample native overlay panel background");
  } else {
    tracing::info!(
      panel = if toolbar {
        "drawing toolbar"
      } else {
        "size indicator"
      },
      red = pixel.0 & 255,
      green = (pixel.0 >> 8) & 255,
      blue = (pixel.0 >> 16) & 255,
      "Native overlay background pixel"
    );
  }
  SAMPLED.with(|value| value.set(value.get() | bit));
}
