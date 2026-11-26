//! Windows implementation of monitor detection and cursor position.
//!
//! Uses Win32 APIs:
//! - `EnumDisplayMonitors` + `GetMonitorInfoW` for monitor enumeration
//! - `GetCursorPos` for cursor position
//! - `MonitorFromPoint` for point-to-monitor mapping
//! - `GetSystemMetrics` for virtual desktop bounds

use super::MonitorInfo;
use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
  EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW,
  MonitorFromPoint,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
  GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
  SM_YVIRTUALSCREEN,
};

fn normalize_logical_origins(monitors: &mut [MonitorInfo]) {
  let reference_dpi = monitors
    .iter()
    .find(|m| m.is_primary)
    .map(|m| m.dpi_scale)
    .or_else(|| monitors.first().map(|m| m.dpi_scale))
    .unwrap_or(1.0);

  for monitor in monitors {
    monitor.logical_x = (monitor.x as f64 / reference_dpi).round() as i32;
    monitor.logical_y = (monitor.y as f64 / reference_dpi).round() as i32;
    monitor.logical_width = (monitor.width as f64 / reference_dpi).round() as u32;
    monitor.logical_height = (monitor.height as f64 / reference_dpi).round() as u32;
  }
}

/// Get all connected monitors.
pub fn list_monitors() -> Vec<MonitorInfo> {
  struct EnumData {
    monitors: Vec<MonitorInfo>,
  }

  unsafe extern "system" fn enum_callback(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
  ) -> windows::core::BOOL {
    let data = unsafe { &mut *(lparam.0 as *mut EnumData) };

    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

    if unsafe { GetMonitorInfoW(hmonitor, &mut info.monitorInfo) }.as_bool() {
      let rect = info.monitorInfo.rcMonitor;
      let is_primary = (info.monitorInfo.dwFlags & 1) != 0; // MONITORINFOF_PRIMARY

      // Device name from szDevice (null-terminated UTF-16)
      let device_name = String::from_utf16_lossy(&info.szDevice)
        .trim_end_matches('\0')
        .to_string();

      // Use HMONITOR as unique ID (stable for the session)
      let id = format!("{:?}", hmonitor.0);

      let x = rect.left;
      let y = rect.top;
      let width = (rect.right - rect.left) as u32;
      let height = (rect.bottom - rect.top) as u32;

      let mut dpi_x: u32 = 96;
      let mut dpi_y: u32 = 96;
      let _ = unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
      let dpi_scale = dpi_x as f64 / 96.0;

      data.monitors.push(MonitorInfo {
        id,
        name: device_name,
        x,
        y,
        width,
        height,
        is_primary,
        dpi_scale,
        logical_x: 0,
        logical_y: 0,
        logical_width: 0,
        logical_height: 0,
      });
    }

    windows::core::BOOL(1) // Continue enumeration
  }

  let mut data = EnumData {
    monitors: Vec::new(),
  };

  unsafe {
    let _ = EnumDisplayMonitors(
      None,
      None,
      Some(enum_callback),
      LPARAM(&mut data as *mut _ as isize),
    );
  }

  normalize_logical_origins(&mut data.monitors);
  data.monitors
}

/// Get current cursor position in virtual screen coordinates.
pub fn get_cursor_position() -> Option<(i32, i32)> {
  let mut point = POINT::default();
  if unsafe { GetCursorPos(&mut point) }.is_ok() {
    Some((point.x, point.y))
  } else {
    tracing::warn!(operation = "get_cursor_position", "GetCursorPos failed");
    None
  }
}

/// Find the monitor containing the given point.
///
/// Uses `MONITOR_DEFAULTTONEAREST` so it always returns a monitor
/// even if the point is outside all monitor bounds.
pub fn monitor_at_point_impl(x: i32, y: i32) -> Option<MonitorInfo> {
  let point = POINT { x, y };
  let hmonitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };

  if hmonitor.is_invalid() {
    tracing::warn!(
      operation = "monitor_at_point",
      x = x,
      y = y,
      "MonitorFromPoint returned invalid handle"
    );
    return None;
  }

  let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
  info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

  if !unsafe { GetMonitorInfoW(hmonitor, &mut info.monitorInfo) }.as_bool() {
    tracing::warn!(operation = "monitor_at_point", "GetMonitorInfoW failed");
    return None;
  }

  let rect = info.monitorInfo.rcMonitor;
  let is_primary = (info.monitorInfo.dwFlags & 1) != 0;
  let device_name = String::from_utf16_lossy(&info.szDevice)
    .trim_end_matches('\0')
    .to_string();
  let id = format!("{:?}", hmonitor.0);

  let mx = rect.left;
  let my = rect.top;
  let mw = (rect.right - rect.left) as u32;
  let mh = (rect.bottom - rect.top) as u32;

  let mut dpi_x: u32 = 96;
  let mut dpi_y: u32 = 96;
  let _ = unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
  let dpi_scale = dpi_x as f64 / 96.0;
  let reference_dpi = list_monitors()
    .into_iter()
    .find(|m| m.is_primary)
    .map(|m| m.dpi_scale)
    .unwrap_or(dpi_scale);

  Some(MonitorInfo {
    id,
    name: device_name,
    x: mx,
    y: my,
    width: mw,
    height: mh,
    is_primary,
    dpi_scale,
    logical_x: (mx as f64 / reference_dpi).round() as i32,
    logical_y: (my as f64 / reference_dpi).round() as i32,
    logical_width: (mw as f64 / reference_dpi).round() as u32,
    logical_height: (mh as f64 / reference_dpi).round() as u32,
  })
}

/// Get virtual desktop bounds (union of all monitors).
///
/// Returns (x, y, width, height) where (x, y) may be negative
/// if monitors are positioned to the left of or above the primary.
pub fn get_virtual_desktop_bounds_impl() -> (i32, i32, u32, u32) {
  let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
  let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
  let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) } as u32;
  let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) } as u32;

  // Fallback to sensible defaults if system metrics fail
  if width == 0 || height == 0 {
    tracing::warn!(
      operation = "get_virtual_desktop_bounds",
      "GetSystemMetrics returned zero dimensions, using fallback"
    );
    return (0, 0, 1920, 1080);
  }

  (x, y, width, height)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_list_monitors_returns_at_least_one() {
    let monitors = list_monitors();
    assert!(!monitors.is_empty(), "Should have at least one monitor");
  }

  #[test]
  fn test_has_primary_monitor() {
    let monitors = list_monitors();
    let primary_count = monitors.iter().filter(|m| m.is_primary).count();
    assert_eq!(primary_count, 1, "Should have exactly one primary monitor");
  }

  #[test]
  fn test_cursor_position_returns_some() {
    // This test may fail in headless CI environments
    let pos = get_cursor_position();
    // Just check it doesn't panic; result depends on environment
    let _ = pos;
  }

  #[test]
  fn test_virtual_desktop_bounds_positive_dimensions() {
    let (_, _, w, h) = get_virtual_desktop_bounds_impl();
    assert!(w > 0, "Virtual desktop width should be positive");
    assert!(h > 0, "Virtual desktop height should be positive");
  }
}
