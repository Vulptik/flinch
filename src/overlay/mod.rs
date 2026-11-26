//! Overlay module - Win32 overlay system
//!
//! Each overlay type runs on a dedicated thread with its own Win32 window
//! and thread-owned rendering. Selection/annotation presentation uses GDI buffers;
//! drawing-toolbar chrome uses Direct2D/DirectWrite.
//!
//! ## Architecture
//! - `core/` - Shared infrastructure (window detection)
//! - `gdi/` - GDI drawing primitives, bitmap conversion, layered windows
//! - `screenshot/` - Screenshot selection overlay
//! - `recording/` - Video recording overlays (HUD and area selection)
//! - `gif/` - GIF recording overlay

pub(crate) mod animation_clock;
pub(crate) mod background;
pub(crate) mod channel;
pub(crate) mod chrome;
pub(crate) mod constants;
pub mod core;
pub(crate) mod countdown;
pub(crate) mod edit_hint;
pub mod gdi;
pub mod gif;
pub(crate) mod icons;
pub(crate) mod magnifier;
pub(crate) mod motion;
pub mod notification;
pub(crate) mod performance;
pub(crate) mod pin;
mod recording;
pub(crate) mod recording_shell;
pub(crate) mod recording_toolbar;
pub(crate) mod render_state;
pub(crate) mod screenshot;
pub mod selection;
pub(crate) mod selection_border;
pub mod selection_core;
pub(crate) mod selection_hint;
pub(crate) mod selection_host;
pub(crate) mod theme;
pub mod types;

use anyhow::Result;
use std::future::Future;

// Re-export main types
pub use gif::GifResult;
pub use recording::AreaRecordingResult;
pub use recording::events::RecordingStatus;
pub use selection::{RenderParams, Selection};

/// Main overlay window interface
pub struct OverlayWindow;

/// Result of area selection containing just the coordinates (for video recording).
///
/// `x` and `y` are in Windows virtual screen coordinates and can be negative
/// (e.g., a monitor to the left of the primary monitor has negative x values).
#[derive(Debug, Clone)]
pub struct AreaSelectionResult {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
  /// Screen dimensions from the fullscreen capture
  pub screen_width: u32,
  pub screen_height: u32,
}

fn finalize_overlay_capture_buffer(rgba: std::sync::Arc<Vec<u8>>) {
  if let Ok(buf) = std::sync::Arc::try_unwrap(rgba) {
    crate::screenshot::platform::return_screenshot_buffer(buf);
  }
  crate::screenshot::platform::clear_cached_screenshot_buffer();
}

async fn with_virtual_desktop_capture<T, F, Fut>(
  error_context: &'static str,
  run_overlay: F,
) -> Result<T>
where
  F: FnOnce(u32, u32, std::sync::Arc<Vec<u8>>, i32, i32) -> Fut,
  Fut: Future<Output = Result<T>>,
{
  use crate::screenshot::platform::capture_virtual_desktop_rgba;

  with_virtual_desktop_capture_with(error_context, capture_virtual_desktop_rgba, run_overlay).await
}

async fn with_virtual_desktop_capture_with<T, F, Fut>(
  error_context: &'static str,
  capture: impl FnOnce() -> Result<(i32, i32, u32, u32, Vec<u8>)>,
  run_overlay: F,
) -> Result<T>
where
  F: FnOnce(u32, u32, std::sync::Arc<Vec<u8>>, i32, i32) -> Fut,
  Fut: Future<Output = Result<T>>,
{
  let (vd_offset_x, vd_offset_y, width, height, rgba) =
    capture().map_err(|e| anyhow::anyhow!("{error_context}: {}", e))?;

  let rgba = std::sync::Arc::new(rgba);
  let result = run_overlay(width, height, rgba.clone(), vd_offset_x, vd_offset_y).await;
  finalize_overlay_capture_buffer(rgba);
  result
}

impl OverlayWindow {
  /// Show the overlay for area selection and return only the coordinates.
  ///
  /// Used for video recording area selection - captures a fullscreen screenshot,
  /// shows the overlay, and returns the selected region coordinates.
  pub async fn show_area_selection(
    app: &crate::app::context::AppHandle,
  ) -> Result<Option<AreaSelectionResult>> {
    with_virtual_desktop_capture(
      "Failed to capture virtual desktop for area selection",
      |width, height, rgba, vd_offset_x, vd_offset_y| async move {
        let screenshot = crate::screenshot::ScreenshotData {
          width,
          height,
          rgba,
          format: "png".to_string(),
          quality: 100,
          vd_offset_x,
          vd_offset_y,
        };
        let selection = Self::show_selection_overlay(app, screenshot).await?;

        Ok(selection.map(|sel| AreaSelectionResult {
          x: sel.x as i32 + vd_offset_x,
          y: sel.y as i32 + vd_offset_y,
          width: sel.width,
          height: sel.height,
          screen_width: width,
          screen_height: height,
        }))
      },
    )
    .await
  }

  /// Show the selection overlay and return the selected area
  ///
  /// This uses a persistent thread with a winit event loop to avoid
  /// the "EventLoop can't be recreated" error.
  pub async fn show_selection_overlay(
    app: &crate::app::context::AppHandle,
    screenshot: crate::screenshot::ScreenshotData,
  ) -> Result<Option<crate::screenshot::SelectionResult>> {
    // Use screenshot overlay module with virtual desktop offset
    let selection = screenshot::show_selection_overlay(
      screenshot.width,
      screenshot.height,
      Some(screenshot.rgba),
      screenshot.vd_offset_x,
      screenshot.vd_offset_y,
      crate::overlay::selection_host::SelectionOptions::for_app(app),
    )
    .await?;

    Ok(selection.map(|sel| crate::screenshot::SelectionResult {
      x: sel.x,
      y: sel.y,
      width: sel.width,
      height: sel.height,
      source_app: sel.source_app,
    }))
  }

  /// Show the recording HUD overlay
  ///
  /// Displays a recording indicator (timer + Pause/Stop buttons).
  /// Both area and fullscreen recording now show interactive toolbars.
  /// If selection_rect is provided, shows a fullscreen overlay with borders at that position.
  pub async fn show_recording_hud(
    start_time: std::time::Instant,
    _recording_status: RecordingStatus,
    selection_rect: Option<(f64, f64, f64, f64)>,
    screen_width: Option<u32>,
    screen_height: Option<u32>,
    controls: Option<std::sync::Arc<crate::state::RecordingControls>>,
    lang: String,
  ) -> Result<()> {
    recording::show_recording_hud(
      start_time,
      selection_rect,
      screen_width,
      screen_height,
      controls,
      lang,
    )
    .await
  }

  /// Hide the recording HUD overlay
  pub async fn hide_recording_hud() -> Result<()> {
    // Use recording overlay module
    recording::hide_recording_hud().await
  }

  /// Update the recording HUD with new timer state
  pub async fn update_recording_hud(elapsed_ms: u64, is_paused: bool) -> Result<()> {
    recording::update_recording_hud(elapsed_ms, is_paused).await
  }

  /// Show the self-contained area recording selection overlay (GIF-style).
  ///
  /// Displays a fullscreen overlay with dim background for area selection.
  /// On mouse release (selection finalized), the overlay transitions to recording mode
  /// (border + timer + Stop/Pause buttons) and returns the selected coordinates.
  /// The stop_flag is shared so the overlay's Stop button can signal the caller to stop.
  /// The pause_flag is shared so the overlay's Pause button can signal the caller to pause/resume.
  pub async fn show_area_recording_selection(
    options: selection_host::SelectionOptions,
  ) -> Result<Option<AreaRecordingResult>> {
    with_virtual_desktop_capture(
      "Failed to capture screen for area recording selection",
      |width, height, rgba, vd_offset_x, vd_offset_y| async move {
        recording::show_area_recording_selection(
          width,
          height,
          rgba,
          vd_offset_x,
          vd_offset_y,
          options,
        )
        .await
      },
    )
    .await
  }

  /// Show the GIF recording overlay
  ///
  /// Displays a fullscreen overlay with area selection UI and Start/Stop button.
  /// Returns the path to the recorded GIF file when recording is complete.
  pub async fn show_gif_selection(config: gif::GifRecordingConfig) -> Result<Option<GifResult>> {
    with_virtual_desktop_capture(
      "Failed to get screen dimensions for GIF selection",
      |width, height, rgba, vd_offset_x, vd_offset_y| async move {
        gif::show_gif_selection(width, height, rgba, vd_offset_x, vd_offset_y, config).await
      },
    )
    .await
  }
}

#[cfg(test)]
mod frozen_capture_tests {
  use super::*;

  #[tokio::test]
  async fn frozen_capture_preserves_offsets_errors_and_releases_overlay_ownership() {
    use std::sync::{Arc, Mutex};
    for outcome in 0..3 {
      let weak = Arc::new(Mutex::new(None));
      let observed = weak.clone();
      let result = with_virtual_desktop_capture_with(
        "selection capture",
        || Ok((-40, -20, 3, 2, vec![17; 24])),
        move |width, height, pixels, x, y| async move {
          assert_eq!((width, height, x, y), (3, 2, -40, -20));
          assert_eq!(*pixels, vec![17; 24]);
          *observed.lock().unwrap() = Some(Arc::downgrade(&pixels));
          match outcome {
            0 => Ok(Some(42)),
            1 => Ok(None),
            _ => anyhow::bail!("overlay worker disconnected"),
          }
        },
      )
      .await;
      match outcome {
        0 => assert_eq!(result.unwrap(), Some(42)),
        1 => assert_eq!(result.unwrap(), None),
        _ => assert_eq!(
          result.unwrap_err().to_string(),
          "overlay worker disconnected"
        ),
      }
      assert!(weak.lock().unwrap().as_ref().unwrap().upgrade().is_none());
    }
    let result: Result<()> = with_virtual_desktop_capture_with(
      "selection capture",
      || anyhow::bail!("monitor removed"),
      |_, _, _, _, _| async { panic!("failed capture must not open an overlay") },
    )
    .await;
    assert_eq!(
      result.unwrap_err().to_string(),
      "selection capture: monitor removed"
    );
  }
}
