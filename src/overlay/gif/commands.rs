// Commands for GIF overlay

use std::path::PathBuf;

/// GIF recording configuration passed from the native capture workflow
#[derive(Debug, Clone)]
pub struct GifRecordingConfig {
  /// Target FPS for capture (default: 15)
  pub fps: u32,
  /// Maximum recording duration in seconds (default: 30)
  pub max_duration_secs: u32,
  /// Directory to save GIF files
  pub output_dir: PathBuf,
  /// Whether to render the cursor into GIF frames
  pub show_cursor: bool,
  /// Filename template string (e.g. "%TYPE%_%DATETIME%")
  pub filename_template: String,
  /// App handle used for tray/session/notification coordination.
  pub app_handle: crate::app::context::AppHandle,
  /// Recording UI policy for this capture.
  pub presentation_mode: crate::recording_visibility::RecordingPresentationMode,
}
