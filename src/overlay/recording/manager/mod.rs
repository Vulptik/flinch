//! Recording overlay manager
//!
//! ## Module Structure
//! - `area_hud` - Unified fullscreen border+toolbar overlay for video recording
//! - `area_selection` - Area selection overlay (Win32 WndProc, self-contained)

mod area_hud;
pub mod area_selection;

use std::time::Instant;

fn virtual_desktop_selection_rect() -> (f64, f64, f64, f64) {
  let (x, y, width, height) = crate::monitors::get_virtual_desktop_bounds();
  (
    x as f64,
    y as f64,
    x.saturating_add(width as i32) as f64,
    y.saturating_add(height as i32) as f64,
  )
}

/// Start the unified recording HUD for fullscreen recording.
/// Uses the full virtual desktop as the recording selection so multi-monitor
/// fullscreen capture follows the same overlay path as area recording.
pub fn start_recording_hud(
  start_time: Instant,
  controls: std::sync::Arc<crate::state::RecordingControls>,
  lang: String,
) -> anyhow::Result<()> {
  let (x1, y1, x2, y2) = virtual_desktop_selection_rect();
  area_hud::start_area_hud(x1, y1, x2, y2, start_time, controls, lang)
}

/// Start the unified recording HUD for an explicit screen region.
/// Shows the selection border and timer/pause/stop controls.
/// Spawns its own thread — returns immediately.
pub fn start_area_recording_hud(
  sel_x1: f64,
  sel_y1: f64,
  sel_x2: f64,
  sel_y2: f64,
  start_time: Instant,
  controls: std::sync::Arc<crate::state::RecordingControls>,
  lang: String,
) -> anyhow::Result<()> {
  area_hud::start_area_hud(sel_x1, sel_y1, sel_x2, sel_y2, start_time, controls, lang)
}

#[cfg(test)]
mod tests {
  use super::virtual_desktop_selection_rect;

  #[test]
  fn virtual_desktop_selection_rect_has_non_negative_extent() {
    let (x1, y1, x2, y2) = virtual_desktop_selection_rect();
    assert!(x2 >= x1);
    assert!(y2 >= y1);
  }
}
