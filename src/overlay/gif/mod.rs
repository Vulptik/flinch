// GIF overlay module — independent overlay for GIF recording with area selection

mod commands;
pub mod manager;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

pub use commands::GifRecordingConfig;

/// Result of GIF recording
#[derive(Debug, Clone)]
pub struct GifResult {
  /// Path to the saved GIF file
  pub path: PathBuf,
  /// Width of the GIF
  pub width: u32,
  /// Height of the GIF
  pub height: u32,
  /// Duration in seconds
  pub duration_seconds: u64,
}

/// Show the GIF selection overlay, then record. Blocks until complete or cancelled.
pub async fn show_gif_selection(
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  config: GifRecordingConfig,
) -> Result<Option<GifResult>> {
  tokio::task::spawn_blocking(move || {
    manager::show_gif_selection(width, height, rgba, vd_offset_x, vd_offset_y, config)
  })
  .await
  .map_err(|e| anyhow::anyhow!("Task join error: {}", e))?
}

/// Stop the current GIF recording (if any is active).
/// Fire-and-forget — the result comes via the original response channel.
pub fn stop_gif_recording() {
  manager::stop_gif_recording_external();
}

pub fn pause_gif_recording() {
  manager::pause_gif_recording_external();
}

pub fn resume_gif_recording() {
  manager::resume_gif_recording_external();
}
