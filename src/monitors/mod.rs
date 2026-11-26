//! Monitor detection and cursor position utilities for Windows.
//!
//! This module provides a unified API for:
//! - Enumerating connected monitors with their bounds
//! - Getting the current cursor position
//! - Determining which monitor contains a given point
//!
//! The primary use case is "cursor-follows" capture behavior where
//! the active monitor is determined by cursor position at capture time.

mod windows;

pub use windows::*;

/// Monitor information with bounds in physical pixels.
///
/// Coordinates use virtual screen space where:
/// - Primary monitor typically starts at (0, 0)
/// - Monitors to the left have negative x
/// - Monitors above have negative y
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
  /// Unique identifier for this monitor (platform-specific format)
  pub id: String,
  /// Human-readable display name
  pub name: String,
  /// Left edge x-coordinate in virtual screen space
  pub x: i32,
  /// Top edge y-coordinate in virtual screen space
  pub y: i32,
  /// Width in physical pixels
  pub width: u32,
  /// Height in physical pixels
  pub height: u32,
  /// Whether this is the primary/main display
  pub is_primary: bool,
  /// DPI scale factor (e.g., 1.0, 1.25, 1.5, 2.0)
  pub dpi_scale: f64,
  /// Virtual desktop x in logical pixels
  pub logical_x: i32,
  /// Virtual desktop y in logical pixels
  pub logical_y: i32,
  /// Width in logical pixels (physical / dpi_scale)
  pub logical_width: u32,
  /// Height in logical pixels (physical / dpi_scale)
  pub logical_height: u32,
}

impl MonitorInfo {
  /// Check if a point (in virtual screen coordinates) is within this monitor's bounds.
  pub fn contains_point(&self, x: i32, y: i32) -> bool {
    x >= self.x && x < self.x + self.width as i32 && y >= self.y && y < self.y + self.height as i32
  }

  /// Get the monitor bounds as (x, y, width, height).
  pub fn bounds(&self) -> (i32, i32, u32, u32) {
    (self.x, self.y, self.width, self.height)
  }
}

/// Find the monitor containing the given point in virtual screen coordinates.
///
/// Returns `None` if no monitor contains the point (shouldn't happen with
/// `MONITOR_DEFAULTTONEAREST` on Windows).
pub fn monitor_at_point(x: i32, y: i32) -> Option<MonitorInfo> {
  windows::monitor_at_point_impl(x, y)
}

/// Get the monitor currently under the cursor.
///
/// This is the primary function for "cursor-follows" behavior.
/// Falls back to primary monitor if cursor position cannot be determined.
pub fn monitor_under_cursor() -> Option<MonitorInfo> {
  let (x, y) = get_cursor_position()?;
  monitor_at_point(x, y)
}

/// Get the primary monitor.
///
/// Used as fallback when cursor detection fails.
pub fn get_primary_monitor() -> Option<MonitorInfo> {
  list_monitors().into_iter().find(|m| m.is_primary)
}

/// Returns all monitors whose logical bounds have non-zero area intersection
/// with the given rect (in logical/virtual desktop coordinates).
/// Results are sorted left-to-right, then top-to-bottom.
pub fn monitors_in_rect(
  monitors: &[MonitorInfo],
  rect_x: i32,
  rect_y: i32,
  rect_width: u32,
  rect_height: u32,
) -> Vec<MonitorInfo> {
  let rect_right = rect_x.saturating_add(rect_width as i32);
  let rect_bottom = rect_y.saturating_add(rect_height as i32);

  let mut result: Vec<MonitorInfo> = monitors
    .iter()
    .filter(|m| {
      let m_right = m.logical_x.saturating_add(m.logical_width as i32);
      let m_bottom = m.logical_y.saturating_add(m.logical_height as i32);
      let overlap_x = rect_x.max(m.logical_x) < rect_right.min(m_right);
      let overlap_y = rect_y.max(m.logical_y) < rect_bottom.min(m_bottom);
      overlap_x && overlap_y
    })
    .cloned()
    .collect();

  result.sort_by(|a, b| {
    a.logical_x
      .cmp(&b.logical_x)
      .then(a.logical_y.cmp(&b.logical_y))
  });

  result
}

/// Parse a monitor ID string and return a `MonitorInfo`.
///
/// Format: `"x,y,wxh"` (e.g. `"0,0,1920x1080"`).
///
/// First tries to match against currently connected monitors returned by
/// [`list_monitors`] so that full DPI/logical info is available.  If no
/// connected monitor matches, falls back to constructing a minimal
/// `MonitorInfo` with `dpi_scale: 1.0` and logical coordinates equal to the
/// physical ones parsed from the ID string.
///
/// Returns `None` only when the ID string is malformed (cannot be parsed).
pub fn parse_monitor_id(id: &str) -> Option<MonitorInfo> {
  // Prefer matching live monitor IDs first so callers can pass through the
  // platform-native identifiers returned by `list_monitors()`.
  if let Some(matched) = list_monitors().into_iter().find(|m| m.id == id) {
    return Some(matched);
  }

  let parts: Vec<&str> = id.split(',').collect();
  if parts.len() != 3 {
    return None;
  }

  let x: i32 = parts[0].parse().ok()?;
  let y: i32 = parts[1].parse().ok()?;

  let size_parts: Vec<&str> = parts[2].split('x').collect();
  if size_parts.len() != 2 {
    return None;
  }

  let width: u32 = size_parts[0].parse().ok()?;
  let height: u32 = size_parts[1].parse().ok()?;

  // Fallback: construct a minimal MonitorInfo from the parsed coordinates.
  Some(MonitorInfo {
    id: id.to_string(),
    name: format!("Monitor at ({}, {})", x, y),
    x,
    y,
    width,
    height,
    is_primary: false,
    dpi_scale: 1.0,
    logical_x: x,
    logical_y: y,
    logical_width: width,
    logical_height: height,
  })
}

/// Get virtual desktop bounds (union of all monitors).
///
/// Returns (x, y, width, height) where (x, y) is the top-left corner
/// of the virtual desktop (may be negative).
pub fn get_virtual_desktop_bounds() -> (i32, i32, u32, u32) {
  windows::get_virtual_desktop_bounds_impl()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_monitor(
    id: &str,
    name: &str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    is_primary: bool,
  ) -> MonitorInfo {
    MonitorInfo {
      id: id.to_string(),
      name: name.to_string(),
      x,
      y,
      width,
      height,
      is_primary,
      dpi_scale: 1.0,
      logical_x: x,
      logical_y: y,
      logical_width: width,
      logical_height: height,
    }
  }

  #[test]
  fn test_monitor_info_creation() {
    let monitor = make_monitor("monitor_1", "Primary Display", 0, 0, 1920, 1080, true);

    assert_eq!(monitor.id, "monitor_1");
    assert_eq!(monitor.name, "Primary Display");
    assert_eq!(monitor.x, 0);
    assert_eq!(monitor.y, 0);
    assert_eq!(monitor.width, 1920);
    assert_eq!(monitor.height, 1080);
    assert!(monitor.is_primary);
  }

  #[test]
  fn test_monitor_contains_point_inside() {
    let monitor = make_monitor("1", "Test", 0, 0, 1920, 1080, true);

    assert!(monitor.contains_point(960, 540)); // Center
    assert!(monitor.contains_point(0, 0)); // Top-left corner
    assert!(monitor.contains_point(1919, 1079)); // Bottom-right (inside)
  }

  #[test]
  fn test_monitor_contains_point_outside() {
    let monitor = make_monitor("1", "Test", 0, 0, 1920, 1080, true);

    assert!(!monitor.contains_point(-1, 0)); // Left edge outside
    assert!(!monitor.contains_point(0, -1)); // Top edge outside
    assert!(!monitor.contains_point(1920, 540)); // Right edge outside
    assert!(!monitor.contains_point(960, 1080)); // Bottom edge outside
  }

  #[test]
  fn test_monitor_contains_point_negative_coords() {
    let monitor = make_monitor("2", "Secondary", -1920, 0, 1920, 1080, false);

    assert!(monitor.contains_point(-960, 540)); // Center of left monitor
    assert!(monitor.contains_point(-1920, 0)); // Top-left
    assert!(!monitor.contains_point(0, 540)); // Outside (on right monitor)
  }

  #[test]
  fn test_monitor_bounds() {
    let monitor = make_monitor("1", "Test", 100, 200, 1920, 1080, true);

    let (x, y, w, h) = monitor.bounds();
    assert_eq!(x, 100);
    assert_eq!(y, 200);
    assert_eq!(w, 1920);
    assert_eq!(h, 1080);
  }

  #[test]
  fn test_monitor_info_clone() {
    let monitor = make_monitor("1", "Test", 0, 0, 1920, 1080, true);

    let cloned = monitor.clone();
    assert_eq!(monitor.id, cloned.id);
    assert_eq!(monitor.name, cloned.name);
    assert_eq!(monitor.x, cloned.x);
    assert_eq!(monitor.width, cloned.width);
  }

  #[test]
  fn test_monitor_info_equality() {
    let monitor1 = make_monitor("1", "Test", 0, 0, 1920, 1080, true);
    let monitor2 = make_monitor("1", "Test", 0, 0, 1920, 1080, true);
    let monitor3 = make_monitor("2", "Different", 0, 0, 1920, 1080, false);

    assert_eq!(monitor1, monitor2);
    assert_ne!(monitor1, monitor3);
  }

  #[test]
  fn test_monitor_info_debug() {
    let monitor = make_monitor("debug_test", "Debug Monitor", 0, 0, 1920, 1080, true);

    let debug_str = format!("{:?}", monitor);
    assert!(debug_str.contains("debug_test"));
    assert!(debug_str.contains("Debug Monitor"));
    assert!(debug_str.contains("1920"));
    assert!(debug_str.contains("1080"));
  }

  #[test]
  fn test_parse_monitor_id_accepts_live_monitor_ids() {
    let monitors = list_monitors();
    let first = monitors
      .first()
      .expect("Should have at least one connected monitor");

    let parsed = parse_monitor_id(&first.id).expect("Live monitor ID should resolve");

    assert_eq!(parsed.id, first.id);
    assert_eq!(parsed.x, first.x);
    assert_eq!(parsed.y, first.y);
    assert_eq!(parsed.width, first.width);
    assert_eq!(parsed.height, first.height);
  }

  #[test]
  fn disconnected_monitor_ids_fall_back_to_their_physical_rectangle() {
    // No connected display sits at this origin with this size.
    let id = "-40001,30007,1234x567";
    assert!(list_monitors().iter().all(|monitor| monitor.id != id));
    assert_eq!(
      parse_monitor_id(id),
      Some(MonitorInfo {
        name: "Monitor at (-40001, 30007)".into(),
        ..make_monitor(id, "", -40001, 30007, 1234, 567, false)
      })
    );
    for malformed in [
      "",
      "1,2",
      "1,2,3x4,5",
      "x,2,3x4",
      "1,y,3x4",
      "1,2,3",
      "1,2,3x4x5",
      "1,2,wx4",
      "1,2,3xh",
      "1,2,-3x4",
    ] {
      assert_eq!(parse_monitor_id(malformed), None, "{malformed:?}");
    }
  }

  #[test]
  fn test_monitor_zero_size() {
    let monitor = make_monitor("zero", "Zero Size", 0, 0, 0, 0, true);

    assert!(!monitor.contains_point(0, 0)); // Even (0,0) not contained in zero-size
    let (_, _, w, h) = monitor.bounds();
    assert_eq!(w, 0);
    assert_eq!(h, 0);
  }

  #[test]
  fn test_monitor_large_dimensions() {
    let monitor = make_monitor("large", "8K Monitor", 0, 0, 7680, 4320, true);

    assert_eq!(monitor.width, 7680);
    assert_eq!(monitor.height, 4320);
    assert!(monitor.contains_point(3840, 2160)); // Center of 8K
  }

  #[test]
  fn test_monitor_vertical_layout() {
    let top_monitor = make_monitor("top", "Top", 0, -1080, 1920, 1080, false);
    let bottom_monitor = make_monitor("bottom", "Bottom", 0, 0, 1920, 1080, true);

    assert!(top_monitor.contains_point(960, -540));
    assert!(!top_monitor.contains_point(960, 540));
    assert!(bottom_monitor.contains_point(960, 540));
    assert!(!bottom_monitor.contains_point(960, -540));
  }
}
