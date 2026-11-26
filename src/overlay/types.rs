// Shared types for the overlay module

/// Monitor rectangle in overlay-local coordinates (physical pixels).
///
/// Used to confine rendering (dim overlay, screenshot texture) to actual
/// monitor areas, leaving dead zones transparent in multi-monitor setups
/// where monitors have different resolutions.
#[derive(Clone, Debug)]
pub struct MonitorRect {
  pub x: u32,
  pub y: u32,
  pub width: u32,
  pub height: u32,
}

/// Compute monitor rects in overlay-local coordinates.
///
/// Converts physical monitor bounds from virtual screen space into
/// overlay-local space by subtracting the virtual desktop offset.
/// The overlay window starts at `(vd_offset_x, vd_offset_y)` in screen space.
pub fn compute_monitor_rects(vd_offset_x: i32, vd_offset_y: i32) -> Vec<MonitorRect> {
  crate::monitors::list_monitors()
    .into_iter()
    .map(|m| MonitorRect {
      x: (m.x - vd_offset_x).max(0) as u32,
      y: (m.y - vd_offset_y).max(0) as u32,
      width: m.width,
      height: m.height,
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_monitor_rect_clone() {
    let rect = MonitorRect {
      x: 10,
      y: 20,
      width: 1920,
      height: 1080,
    };
    let cloned = rect.clone();
    assert_eq!(cloned.x, 10);
    assert_eq!(cloned.width, 1920);
  }
}
