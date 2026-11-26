/// Video capture configuration and Windows helper surface.
pub mod windows;

/// Recording mode - how the recording was initiated
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingMode {
  Fullscreen,
  Area,
  ActiveWindow,
}

impl std::str::FromStr for RecordingMode {
  type Err = String;

  fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
    match s {
      "fullscreen" => Ok(RecordingMode::Fullscreen),
      "area" => Ok(RecordingMode::Area),
      "active_window" => Ok(RecordingMode::ActiveWindow),
      other => Err(format!("unknown recording mode: '{other}'")),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_recording_mode_from_str() {
    assert_eq!(
      "area".parse::<RecordingMode>().unwrap(),
      RecordingMode::Area
    );
    assert_eq!(
      "active_window".parse::<RecordingMode>().unwrap(),
      RecordingMode::ActiveWindow
    );
    assert_eq!(
      "fullscreen".parse::<RecordingMode>().unwrap(),
      RecordingMode::Fullscreen
    );
    assert!(
      "unknown".parse::<RecordingMode>().is_err(),
      "Unknown mode strings should return an error"
    );
  }

  #[test]
  fn test_recording_mode_equality() {
    assert_eq!(RecordingMode::Area, RecordingMode::Area);
    assert_ne!(RecordingMode::Area, RecordingMode::Fullscreen);
  }

  #[test]
  fn test_recording_mode_copy() {
    let mode1 = RecordingMode::ActiveWindow;
    let mode2 = mode1; // Copy
    assert_eq!(mode1, mode2);
  }

  #[test]
  fn test_recording_mode_debug() {
    let mode = RecordingMode::Area;
    let debug_str = format!("{:?}", mode);
    assert_eq!(debug_str, "Area");
  }

  #[test]
  fn test_capture_config_fullscreen() {
    let config = CaptureConfig::fullscreen(1920, 1080, 60);

    assert_eq!(config.width, 1920);
    assert_eq!(config.height, 1080);
    assert_eq!(config.fps, 60);
    assert_eq!(config.x, 0);
    assert_eq!(config.y, 0);
    assert_eq!(config.mode, RecordingMode::Fullscreen);
    assert_eq!(config.monitor_id, None);
    assert!(!config.show_cursor);
  }

  #[test]
  fn test_capture_config_area() {
    let config = CaptureConfig::area(100, 200, 800, 600, 30);

    assert_eq!(config.x, 100);
    assert_eq!(config.y, 200);
    assert_eq!(config.width, 800);
    assert_eq!(config.height, 600);
    assert_eq!(config.fps, 30);
    assert_eq!(config.mode, RecordingMode::Area);
  }

  #[test]
  fn test_capture_config_active_window() {
    let config = CaptureConfig::active_window(50, 100, 640, 480, 60);

    assert_eq!(config.x, 50);
    assert_eq!(config.y, 100);
    assert_eq!(config.width, 640);
    assert_eq!(config.height, 480);
    assert_eq!(config.fps, 60);
    assert_eq!(config.mode, RecordingMode::ActiveWindow);
  }

  #[test]
  fn test_capture_config_align_dimensions_even() {
    let mut config = CaptureConfig::fullscreen(1920, 1080, 60);
    config.align_dimensions();
    assert_eq!(config.width, 1920); // Already even
    assert_eq!(config.height, 1080); // Already even
  }

  #[test]
  fn test_capture_config_align_dimensions_odd() {
    let mut config = CaptureConfig::fullscreen(1921, 1081, 60);
    config.align_dimensions();
    assert_eq!(config.width, 1920); // Rounded down
    assert_eq!(config.height, 1080); // Rounded down
  }

  #[test]
  fn test_capture_config_align_dimensions_zero() {
    let mut config = CaptureConfig::fullscreen(0, 0, 60);
    config.align_dimensions();
    assert_eq!(config.width, 2); // Minimum 2
    assert_eq!(config.height, 2); // Minimum 2
  }

  #[test]
  fn test_capture_config_align_dimensions_one() {
    let mut config = CaptureConfig::fullscreen(1, 1, 60);
    config.align_dimensions();
    assert_eq!(config.width, 2); // Minimum 2
    assert_eq!(config.height, 2); // Minimum 2
  }

  #[test]
  fn test_capture_config_clone() {
    let config = CaptureConfig::area(100, 200, 800, 600, 30);
    let cloned = config.clone();

    assert_eq!(config.x, cloned.x);
    assert_eq!(config.y, cloned.y);
    assert_eq!(config.width, cloned.width);
    assert_eq!(config.height, cloned.height);
    assert_eq!(config.fps, cloned.fps);
    assert_eq!(config.mode, cloned.mode);
  }

  #[test]
  fn test_capture_config_debug() {
    let config = CaptureConfig::fullscreen(1920, 1080, 60);
    let debug_str = format!("{:?}", config);
    assert!(debug_str.contains("1920"));
    assert!(debug_str.contains("1080"));
    assert!(debug_str.contains("60"));
  }

  #[test]
  fn test_capture_config_with_monitor_id() {
    let mut config = CaptureConfig::fullscreen(1920, 1080, 60);
    config.monitor_id = Some("0,0,1920x1080".to_string());

    assert_eq!(config.monitor_id, Some("0,0,1920x1080".to_string()));
  }

  #[test]
  fn test_capture_config_show_cursor() {
    let mut config = CaptureConfig::fullscreen(1920, 1080, 60);
    assert!(!config.show_cursor);

    config.show_cursor = true;
    assert!(config.show_cursor);
  }

  #[test]
  fn test_capture_config_negative_coordinates() {
    let config = CaptureConfig::area(-100, -200, 800, 600, 30);
    assert_eq!(config.x, -100);
    assert_eq!(config.y, -200);
  }

  #[test]
  fn test_capture_config_high_fps() {
    let config = CaptureConfig::fullscreen(1920, 1080, 144);
    assert_eq!(config.fps, 144);
  }
}

/// Capture configuration
#[derive(Debug, Clone)]
pub struct CaptureConfig {
  pub fps: u32,
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
  pub mode: RecordingMode,
  /// Monitor to capture from (locked at recording start).
  /// For video recording, this is set once at start and used for all frames.
  /// Format: "x,y,wxh" (e.g., "0,0,1920x1080")
  #[allow(dead_code)]
  pub monitor_id: Option<String>,
  /// Whether to render software cursor in recording
  #[allow(dead_code)]
  pub show_cursor: bool,
  /// Whether click highlights are drawn into the recording.
  pub show_clicks: bool,
  /// Explicit backend for debug/tests and GIF compatibility recording. DXGI
  /// requires a single output; GDI preserves the full rectangle. WGC is unsupported.
  pub forced_backend: Option<crate::screenshot::CaptureBackendKind>,
  /// When set, the planned rectangle spans multiple monitors (GDI compatibility).
  /// Each entry is a monitor ID string (format: "x,y,wxh").
  /// None means single-monitor capture (existing behavior).
  pub monitor_ids: Option<Vec<String>>,
}

impl CaptureConfig {
  #[allow(dead_code)]
  pub fn fullscreen(width: u32, height: u32, fps: u32) -> Self {
    Self {
      fps,
      x: 0,
      y: 0,
      width,
      height,
      mode: RecordingMode::Fullscreen,
      monitor_id: None,
      show_cursor: false,
      show_clicks: false,
      forced_backend: None,
      monitor_ids: None,
    }
  }

  #[allow(dead_code)]
  pub fn area(x: i32, y: i32, width: u32, height: u32, fps: u32) -> Self {
    Self {
      fps,
      x,
      y,
      width,
      height,
      mode: RecordingMode::Area,
      monitor_id: None,
      show_cursor: false,
      show_clicks: false,
      forced_backend: None,
      monitor_ids: None,
    }
  }

  #[allow(dead_code)]
  pub fn active_window(x: i32, y: i32, width: u32, height: u32, fps: u32) -> Self {
    Self {
      fps,
      x,
      y,
      width,
      height,
      mode: RecordingMode::ActiveWindow,
      monitor_id: None,
      show_cursor: false,
      show_clicks: false,
      forced_backend: None,
      monitor_ids: None,
    }
  }

  /// Align width and height to even numbers (required by H.264/YUV420P).
  /// Rounds down to avoid capturing outside screen bounds.
  pub fn align_dimensions(&mut self) {
    self.width &= !1;
    self.height &= !1;
    if self.width == 0 {
      self.width = 2;
    }
    if self.height == 0 {
      self.height = 2;
    }
  }

  /// Align x and y coordinates to even numbers (required by H.264/YUV420P).
  /// Rounds down to avoid capturing outside screen bounds.
  pub fn align_coordinates(&mut self) {
    self.x &= !1; // Round down to even
    self.y &= !1; // Round down to even
  }
}
