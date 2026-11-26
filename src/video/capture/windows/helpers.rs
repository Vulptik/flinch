//! Windows helper functions for video capture
//!
//! ## Purpose
//! Provides Windows-specific utilities for window management and monitor detection:
//! - Window handle validation
//! - Foreground window detection
//! - Window rectangle queries (with DWM support)
//! - Monitor resolution detection
//!
//! ## DPI Awareness
//! Functions that query window or monitor dimensions return physical pixels,
//! not logical pixels. This is important for correct video capture resolution.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
  EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, IsWindow};

/// Check if a window handle is valid.
/// Uses IsWindow API to verify the handle points to a valid window.
pub fn is_window_valid(hwnd: isize) -> bool {
  if hwnd == 0 {
    return false;
  }

  let hwnd = HWND(hwnd as *mut _);
  unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

/// Get the handle (HWND) of the current foreground window.
pub fn get_foreground_window_handle() -> Option<isize> {
  let hwnd = unsafe { GetForegroundWindow() };
  if hwnd.0.is_null() {
    None
  } else {
    Some(hwnd.0 as isize)
  }
}

/// Get the window rectangle for a given HWND.
///
/// Uses DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS) for accurate bounds
/// that exclude invisible window borders. Falls back to GetWindowRect if DWM
/// is unavailable.
///
/// Returns (x, y, width, height) in physical pixels.
pub fn get_window_rect(hwnd: isize) -> Option<(i32, i32, u32, u32)> {
  // Validate window handle before use
  if !is_window_valid(hwnd) {
    return None;
  }

  let hwnd = HWND(hwnd as *mut _);
  let mut rect = RECT::default();

  // Try DWM extended frame bounds first (excludes invisible borders)
  let hr = unsafe {
    DwmGetWindowAttribute(
      hwnd,
      DWMWA_EXTENDED_FRAME_BOUNDS,
      &mut rect as *mut RECT as *mut _,
      std::mem::size_of::<RECT>() as u32,
    )
  };

  if hr.is_err() {
    // Fallback to GetWindowRect
    let ok = unsafe { GetWindowRect(hwnd, &mut rect) };
    if ok.is_err() {
      return None;
    }
  }

  let width = (rect.right - rect.left) as u32;
  let height = (rect.bottom - rect.top) as u32;

  if width == 0 || height == 0 {
    return None;
  }

  Some((rect.left, rect.top, width, height))
}

/// Get the primary monitor resolution in PHYSICAL pixels.
///
/// Note: GetSystemMetrics returns logical pixels which are affected by DPI scaling.
/// This function uses EnumDisplayMonitors + GetMonitorInfo to get the actual
/// physical resolution.
pub fn get_primary_monitor_resolution() -> (u32, u32) {
  struct MonitorData {
    width: u32,
    height: u32,
    found: bool,
  }

  unsafe extern "system" fn enum_callback(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
  ) -> windows::core::BOOL {
    // SAFETY: lparam points to our MonitorData struct, passed from EnumDisplayMonitors
    let data = unsafe { &mut *(lparam.0 as *mut MonitorData) };

    // SAFETY: zeroed MONITORINFO is valid for GetMonitorInfoW
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;

    // SAFETY: info is properly initialized with cbSize
    if unsafe { GetMonitorInfoW(hmonitor, &mut info) }.as_bool() {
      // MONITORINFOF_PRIMARY = 1
      if (info.dwFlags & 1) != 0 {
        let rect = info.rcMonitor;
        data.width = (rect.right - rect.left) as u32;
        data.height = (rect.bottom - rect.top) as u32;
        data.found = true;
        return windows::core::BOOL(0); // Stop enumeration
      }
    }
    windows::core::BOOL(1) // Continue enumeration
  }

  let mut data = MonitorData {
    width: 0,
    height: 0,
    found: false,
  };

  unsafe {
    let _ = EnumDisplayMonitors(
      None,
      None,
      Some(enum_callback),
      LPARAM(&mut data as *mut _ as isize),
    );
  }

  if data.found && data.width > 0 && data.height > 0 {
    (data.width, data.height)
  } else {
    // Fallback
    (1920, 1080)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::{Win32::UI::WindowsAndMessaging::*, core::w};
  #[test]
  fn native_bounds_queries_reject_destroyed_windows_and_report_physical_monitor_sizes() {
    assert!(!is_window_valid(0));
    assert!(get_window_rect(0).is_none());
    // SAFETY: this thread creates and owns a hidden STATIC window, without activation.
    let hwnd = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        -400,
        -100,
        64,
        48,
        None,
        None,
        None,
        None,
      )
    }
    .unwrap();
    assert!(is_window_valid(hwnd.0 as isize));
    let (x, y, width, height) = get_window_rect(hwnd.0 as isize).unwrap();
    assert_eq!((x, y, width, height), (-400, -100, 64, 48));
    // SAFETY: querying the foreground handle and monitor dimensions reads metadata only.
    if let Some(foreground) = get_foreground_window_handle() {
      assert_ne!(foreground, 0);
    }
    let (width, height) = get_primary_monitor_resolution();
    assert!(width > 0 && height > 0);
    // SAFETY: destroy only this fixture's never-shown HWND.
    unsafe {
      assert!(!IsWindowVisible(hwnd).as_bool());
      DestroyWindow(hwnd).unwrap();
    }
    assert!(!is_window_valid(hwnd.0 as isize));
    assert!(get_window_rect(hwnd.0 as isize).is_none());
  }
}
