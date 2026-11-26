//! Window detection for selection snapping
//!
//! Detects the window under the cursor for hover-to-snap selection.
//! Windows-only implementation; other platforms return None.

use crate::overlay::selection::Rect;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
  DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::{
  GWL_EXSTYLE, GetWindowLongW, GetWindowRect, IsIconic, IsWindowVisible,
};

/// Result of window detection at a screen point
#[derive(Debug, Clone)]
pub struct DetectedWindow {
  pub hwnd: isize,
  pub bounds: Rect,
  pub source_app: Option<String>,
}

/// Get the process name (exe stem, no extension) for the given HWND.
/// Returns None if the process cannot be queried (sandboxed, elevated, etc.).
pub fn get_process_name_for_hwnd(hwnd: isize) -> Option<String> {
  use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
  };
  use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

  let hwnd = HWND(hwnd as *mut _);
  let mut pid: u32 = 0;
  // SAFETY: hwnd and &mut pid are valid
  unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
  if pid == 0 {
    return None;
  }

  // SAFETY: pid is a valid process id; PROCESS_QUERY_LIMITED_INFORMATION allows
  // image name queries without requiring full access rights
  let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? };

  let mut buf = vec![0u16; 1024];
  let mut size = buf.len() as u32;
  // SAFETY: process is valid, buf and size are valid
  let ok = unsafe {
    QueryFullProcessImageNameW(
      process,
      PROCESS_NAME_WIN32,
      windows::core::PWSTR(buf.as_mut_ptr()),
      &mut size,
    )
  };
  // SAFETY: process is valid and must be closed
  let _ = unsafe { windows::Win32::Foundation::CloseHandle(process) };

  if ok.is_err() || size == 0 {
    return None;
  }

  let exe_path = String::from_utf16_lossy(&buf[..size as usize]);
  std::path::Path::new(&exe_path)
    .file_stem()
    .and_then(|s| s.to_str())
    .map(|s| s.to_string())
}

/// What Windows reports about one top-level window, queried only as the filters need it.
trait WindowFacts {
  fn visible(&self) -> bool;
  fn minimized(&self) -> bool;
  fn extended_style(&self) -> u32;
  fn cloaked(&self) -> bool;
  /// DWM extended frame bounds (accurate for Windows 10+) as left, top, right, bottom.
  fn frame_bounds(&self) -> Option<(i32, i32, i32, i32)>;
  fn window_rect(&self) -> Option<(i32, i32, i32, i32)>;
}

/// The bounds a selection may snap to, or `None` when the window is not detectable.
fn detectable_bounds(facts: &impl WindowFacts) -> Option<Rect> {
  use windows::Win32::UI::WindowsAndMessaging::{WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT};

  if !facts.visible() || facts.minimized() {
    return None;
  }
  let ex_style = facts.extended_style();
  // Skip transparent/layered windows that are click-through (like our overlay)
  // and tool windows (tooltips, floating toolbars).
  if ex_style & (WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0) != 0 {
    return None;
  }
  // Cloaked windows live on other virtual desktops or are hidden UWP frames.
  if facts.cloaked() {
    return None;
  }
  let (left, top, right, bottom) = facts.frame_bounds().or_else(|| facts.window_rect())?;
  let (width, height) = (right - left, bottom - top);
  if width <= 0 || height <= 0 {
    return None;
  }
  Some(Rect {
    x: left as f64,
    y: top as f64,
    w: width as f64,
    h: height as f64,
  })
}

/// Adds `hwnd` to `windows` unless it is our excluded window or its facts make it undetectable.
fn collect(
  windows: &mut Vec<DetectedWindow>,
  exclude_hwnd: Option<isize>,
  hwnd: isize,
  facts: &impl WindowFacts,
  source_app: impl FnOnce(isize) -> Option<String>,
) {
  if exclude_hwnd != Some(hwnd)
    && let Some(bounds) = detectable_bounds(facts)
  {
    windows.push(DetectedWindow {
      hwnd,
      bounds,
      source_app: source_app(hwnd),
    });
  }
}

/// A window handle Windows provided (EnumWindows), valid while it is being examined.
struct Enumerated(HWND);
impl WindowFacts for Enumerated {
  fn visible(&self) -> bool {
    // SAFETY: hwnd is valid, provided by EnumWindows
    unsafe { IsWindowVisible(self.0) }.as_bool()
  }
  fn minimized(&self) -> bool {
    // SAFETY: hwnd is valid, provided by EnumWindows
    unsafe { IsIconic(self.0) }.as_bool()
  }
  fn extended_style(&self) -> u32 {
    // SAFETY: hwnd is valid, provided by EnumWindows
    unsafe { GetWindowLongW(self.0, GWL_EXSTYLE) as u32 }
  }
  fn cloaked(&self) -> bool {
    let mut cloaked: u32 = 0;
    // SAFETY: hwnd is valid, cloaked is a valid u32 pointer
    let result = unsafe {
      DwmGetWindowAttribute(
        self.0,
        DWMWA_CLOAKED,
        &mut cloaked as *mut _ as *mut _,
        std::mem::size_of::<u32>() as u32,
      )
    };
    result.is_ok() && cloaked != 0
  }
  fn frame_bounds(&self) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    // SAFETY: hwnd is valid, rect is a valid RECT pointer
    unsafe {
      DwmGetWindowAttribute(
        self.0,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut rect as *mut _ as *mut _,
        std::mem::size_of::<RECT>() as u32,
      )
    }
    .ok()
    .map(|()| (rect.left, rect.top, rect.right, rect.bottom))
  }
  fn window_rect(&self) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    // SAFETY: hwnd is valid, rect is a valid RECT pointer
    unsafe { GetWindowRect(self.0, &mut rect) }
      .ok()
      .map(|()| (rect.left, rect.top, rect.right, rect.bottom))
  }
}

fn enumerate_detectable_windows(exclude_hwnd: Option<isize>) -> Vec<DetectedWindow> {
  use windows::Win32::Foundation::LPARAM;
  use windows::Win32::UI::WindowsAndMessaging::EnumWindows;

  // Structure to pass data to the enum callback
  struct EnumData {
    exclude_hwnd: Option<isize>,
    windows: Vec<DetectedWindow>,
  }

  unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    // SAFETY: lparam is a valid pointer to EnumData passed from EnumWindows caller
    let data = unsafe { &mut *(lparam.0 as *mut EnumData) };
    collect(
      &mut data.windows,
      data.exclude_hwnd,
      hwnd.0 as isize,
      &Enumerated(hwnd),
      get_process_name_for_hwnd,
    );
    windows::core::BOOL(1) // Continue enumeration
  }

  unsafe {
    let mut data = EnumData {
      exclude_hwnd,
      windows: Vec::new(),
    };

    // EnumWindows enumerates top-level windows in Z-order (topmost first)
    let _ = EnumWindows(Some(enum_callback), LPARAM(&mut data as *mut _ as isize));
    data.windows
  }
}

/// Capture a frozen top-level window snapshot in Z-order for selection overlays.
pub fn snapshot_detectable_windows(exclude_hwnd: Option<isize>) -> Vec<DetectedWindow> {
  enumerate_detectable_windows(exclude_hwnd)
}

/// Detect the topmost cached window under the given screen coordinates.
pub fn detect_window_in_snapshot_at_point(
  windows: &[DetectedWindow],
  screen_x: i32,
  screen_y: i32,
) -> Option<DetectedWindow> {
  windows.iter().find_map(|window| {
    let bounds = window.bounds;
    let screen_x = screen_x as f64;
    let screen_y = screen_y as f64;
    let contains = screen_x >= bounds.x
      && screen_x < bounds.x + bounds.w
      && screen_y >= bounds.y
      && screen_y < bounds.y + bounds.h;
    contains.then(|| window.clone())
  })
}

/// Detect the window under the given screen coordinates.
/// Returns None if no suitable window is found.
pub fn detect_window_at_point(
  screen_x: i32,
  screen_y: i32,
  exclude_hwnd: Option<isize>,
) -> Option<DetectedWindow> {
  detect_window_in_snapshot_at_point(
    &snapshot_detectable_windows(exclude_hwnd),
    screen_x,
    screen_y,
  )
}

/// Detect the window under the given overlay-local coordinates, accounting for
/// the virtual desktop offset. Returns bounds in overlay-local coordinates.
///
/// This is the multi-monitor-aware version of `detect_window_at_point`.
/// Overlay pos (0,0) corresponds to screen (vd_offset_x, vd_offset_y).
pub fn detect_window_at_overlay_point(
  overlay_x: i32,
  overlay_y: i32,
  exclude_hwnd: Option<isize>,
  vd_offset_x: i32,
  vd_offset_y: i32,
) -> Option<DetectedWindow> {
  // Convert overlay-local → screen coordinates
  let screen_x = overlay_x + vd_offset_x;
  let screen_y = overlay_y + vd_offset_y;

  detect_window_in_snapshot_at_point(
    &snapshot_detectable_windows(exclude_hwnd),
    screen_x,
    screen_y,
  )
  .map(|mut w| {
    // Convert screen bounds → overlay-local coordinates
    w.bounds.x -= vd_offset_x as f64;
    w.bounds.y -= vd_offset_y as f64;
    w
  })
}

/// Detect the cached window under the given overlay-local coordinates, accounting for
/// the virtual desktop offset. Returns bounds in overlay-local coordinates.
pub fn detect_window_in_snapshot_at_overlay_point(
  windows: &[DetectedWindow],
  overlay_x: i32,
  overlay_y: i32,
  vd_offset_x: i32,
  vd_offset_y: i32,
) -> Option<DetectedWindow> {
  let screen_x = overlay_x + vd_offset_x;
  let screen_y = overlay_y + vd_offset_y;

  detect_window_in_snapshot_at_point(windows, screen_x, screen_y).map(|mut w| {
    w.bounds.x -= vd_offset_x as f64;
    w.bounds.y -= vd_offset_y as f64;
    w
  })
}

/// Clamp an overlay-local point to the nearest valid monitor area.
///
/// On multi-monitor setups with different resolutions, the virtual desktop bounding box
/// contains "dead zones" not covered by any monitor. This function snaps points in dead
/// zones to the nearest monitor edge, preventing selection rectangles from extending
/// into invisible areas.
///
/// Points already on a monitor are returned unchanged.
pub fn clamp_to_monitor_area(
  overlay_x: f64,
  overlay_y: f64,
  vd_offset_x: i32,
  vd_offset_y: i32,
) -> (f64, f64) {
  let monitors = crate::monitors::list_monitors();
  if monitors.is_empty() {
    return (overlay_x, overlay_y);
  }

  // Convert to screen coords
  let screen_x = overlay_x + vd_offset_x as f64;
  let screen_y = overlay_y + vd_offset_y as f64;

  // Check if already on a monitor
  for m in &monitors {
    let mx = m.x as f64;
    let my = m.y as f64;
    let mw = m.width as f64;
    let mh = m.height as f64;
    if screen_x >= mx && screen_x < mx + mw && screen_y >= my && screen_y < my + mh {
      return (overlay_x, overlay_y);
    }
  }

  // Find nearest point on any monitor
  let mut best_x = screen_x;
  let mut best_y = screen_y;
  let mut best_dist = f64::MAX;
  for m in &monitors {
    let mx = m.x as f64;
    let my = m.y as f64;
    let mw = m.width as f64;
    let mh = m.height as f64;
    // Clamp to this monitor's bounds
    let cx = screen_x.max(mx).min(mx + mw - 1.0);
    let cy = screen_y.max(my).min(my + mh - 1.0);
    let dx = screen_x - cx;
    let dy = screen_y - cy;
    let dist = dx * dx + dy * dy;
    if dist < best_dist {
      best_dist = dist;
      best_x = cx;
      best_y = cy;
    }
  }

  // Convert back to overlay-local
  (best_x - vd_offset_x as f64, best_y - vd_offset_y as f64)
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::UI::WindowsAndMessaging::{
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
  };

  /// Scripted window facts; `None` bounds model a failed query.
  #[derive(Clone, Copy)]
  struct Facts {
    visible: bool,
    minimized: bool,
    ex_style: u32,
    cloaked: bool,
    frame: Option<(i32, i32, i32, i32)>,
    rect: Option<(i32, i32, i32, i32)>,
  }
  impl WindowFacts for Facts {
    fn visible(&self) -> bool {
      self.visible
    }
    fn minimized(&self) -> bool {
      self.minimized
    }
    fn extended_style(&self) -> u32 {
      self.ex_style
    }
    fn cloaked(&self) -> bool {
      self.cloaked
    }
    fn frame_bounds(&self) -> Option<(i32, i32, i32, i32)> {
      self.frame
    }
    fn window_rect(&self) -> Option<(i32, i32, i32, i32)> {
      self.rect
    }
  }
  const NORMAL: Facts = Facts {
    visible: true,
    minimized: false,
    ex_style: 0,
    cloaked: false,
    frame: Some((10, 20, 110, 70)),
    rect: Some((0, 0, 1, 1)),
  };

  fn bounds(facts: Facts) -> Option<(f64, f64, f64, f64)> {
    detectable_bounds(&facts).map(|rect| (rect.x, rect.y, rect.w, rect.h))
  }

  #[test]
  fn only_shown_selectable_windows_with_an_area_are_detectable() {
    assert_eq!(bounds(NORMAL), Some((10.0, 20.0, 100.0, 50.0)));
    // Layered alone is an ordinary window; click-through and tool windows are not.
    let layered = Facts {
      ex_style: WS_EX_LAYERED.0,
      ..NORMAL
    };
    assert!(bounds(layered).is_some());
    for rejected in [
      Facts {
        visible: false,
        ..NORMAL
      },
      Facts {
        minimized: true,
        ..NORMAL
      },
      Facts {
        ex_style: WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0,
        ..NORMAL
      },
      Facts {
        ex_style: WS_EX_TOOLWINDOW.0,
        ..NORMAL
      },
      Facts {
        cloaked: true,
        ..NORMAL
      },
      Facts {
        frame: Some((10, 20, 10, 70)),
        ..NORMAL
      },
      Facts {
        frame: Some((10, 20, 110, 20)),
        ..NORMAL
      },
      Facts {
        frame: None,
        rect: None,
        ..NORMAL
      },
    ] {
      assert_eq!(bounds(rejected), None);
    }
  }

  #[test]
  fn snapshots_keep_detectable_windows_in_order_with_their_app_and_skip_the_excluded_one() {
    let mut windows = Vec::new();
    let named = |hwnd: isize| Some(format!("app-{hwnd}"));
    collect(&mut windows, Some(2), 1, &NORMAL, named);
    collect(&mut windows, Some(2), 2, &NORMAL, named);
    let hidden = Facts {
      visible: false,
      ..NORMAL
    };
    collect(&mut windows, Some(2), 3, &hidden, named);
    collect(&mut windows, None, 4, &NORMAL, |_| None);
    let kept: Vec<_> = windows
      .iter()
      .map(|window| (window.hwnd, window.source_app.clone()))
      .collect();
    assert_eq!(kept, [(1, Some("app-1".into())), (4, None)]);
    assert_eq!(windows[0].bounds.w, 100.0);
  }

  #[test]
  fn process_names_come_from_the_window_owner_and_unknown_windows_have_none() {
    use windows::Win32::UI::WindowsAndMessaging::{
      CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    // A message-only window is never shown and belongs to this test process.
    // SAFETY: the predefined STATIC class needs no registration; the window is destroyed below.
    let window = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE(0),
        windows::core::w!("STATIC"),
        None,
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        None,
        None,
      )
    }
    .expect("message-only window");
    let expected = std::env::current_exe()
      .ok()
      .and_then(|exe| exe.file_stem()?.to_str().map(str::to_owned));
    assert!(expected.is_some());
    assert_eq!(get_process_name_for_hwnd(window.0 as isize), expected);
    // The live facts read the same window whatever else is on the desktop.
    let facts = Enumerated(window);
    assert!(!facts.visible() && !facts.minimized() && !facts.cloaked());
    assert_eq!(facts.extended_style() & WS_EX_TOOLWINDOW.0, 0);
    // Windows applies its minimum window size, so only the origin is known in advance.
    assert!(matches!(facts.window_rect(), Some((0, 0, _, _))));
    let _ = facts.frame_bounds();
    assert_eq!(detectable_bounds(&facts).map(|rect| rect.w), None);
    // SAFETY: created above on this thread and no longer used.
    unsafe { DestroyWindow(window) }.expect("destroy window");
    assert_eq!(get_process_name_for_hwnd(0), None);
  }

  #[test]
  fn window_rect_stands_in_when_dwm_has_no_frame_bounds() {
    let fallback = Facts {
      frame: None,
      rect: Some((-5, -6, 15, 14)),
      ..NORMAL
    };
    assert_eq!(bounds(fallback), Some((-5.0, -6.0, 20.0, 20.0)));
  }
}
