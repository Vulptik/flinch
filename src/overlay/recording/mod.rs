// Recording overlay module - independent overlay for recording indicator
//
// ## Module Structure
// - `manager/` - HUD and area selection (Win32 windows)
// - `events` - Recording status enum
// - `commands` - Recording command types

mod commands;
pub mod events;
pub mod manager;

use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;

pub use commands::AreaRecordingResult;

/// Show the recording HUD overlay (fire-and-forget Win32 layered window).
/// Spawns its own thread — returns immediately.
///
/// All video recording modes use the same fullscreen border+toolbar overlay.
/// When `selection_rect` is `None` (fullscreen recording), the manager uses the
/// full virtual desktop bounds so multi-monitor fullscreen capture stays aligned
/// with the area-recording path.
pub async fn show_recording_hud(
  start_time: Instant,
  selection_rect: Option<(f64, f64, f64, f64)>,
  _screen_width: Option<u32>,
  _screen_height: Option<u32>,
  controls: Option<Arc<crate::state::RecordingControls>>,
  lang: String,
) -> Result<()> {
  let controls = controls.ok_or_else(|| anyhow::anyhow!("controls required for recording HUD"))?;

  if let Some((x1, y1, x2, y2)) = selection_rect {
    manager::start_area_recording_hud(x1, y1, x2, y2, start_time, controls, lang)
  } else {
    manager::start_recording_hud(start_time, controls, lang)
  }
}

/// Hide the recording HUD overlay.
/// The Win32 HUD is self-closing (monitors stop_flag), so this is a no-op.
pub async fn hide_recording_hud() -> Result<()> {
  // The Win32 HUD thread polls stop_flag and closes itself.
  // The caller already sets stop_flag before calling this.
  Ok(())
}

/// Update the recording HUD with new timer state.
/// The Win32 HUD manages its own timer internally, so this is a no-op.
pub async fn update_recording_hud(_elapsed_ms: u64, _is_paused: bool) -> Result<()> {
  // The Win32 HUD thread reads start_time and pause_flag directly.
  Ok(())
}

/// Show the self-contained area recording selection overlay (Win32 WndProc).
/// Returns coordinates when selection is made.
pub async fn show_area_recording_selection(
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  options: crate::overlay::selection_host::SelectionOptions,
) -> Result<Option<AreaRecordingResult>> {
  tokio::task::spawn_blocking(move || {
    manager::area_selection::show_area_recording_selection(
      width,
      height,
      rgba,
      vd_offset_x,
      vd_offset_y,
      options,
    )
  })
  .await
  .map_err(|e| anyhow::anyhow!("Task join error: {}", e))?
}
