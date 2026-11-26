// src/screenshot/types.rs
// Shared types used by both screenshot and overlay modules

/// Screenshot data captured from the screen
#[derive(Clone)]
pub struct ScreenshotData {
  pub width: u32,
  pub height: u32,
  pub rgba: std::sync::Arc<Vec<u8>>,
  pub format: String,
  pub quality: u8,
  /// Virtual desktop X offset (0 for single-monitor capture)
  pub vd_offset_x: i32,
  /// Virtual desktop Y offset (0 for single-monitor capture)
  pub vd_offset_y: i32,
}

/// Result of user selection in the overlay
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionResult {
  pub x: u32,
  pub y: u32,
  pub width: u32,
  pub height: u32,
  /// Exe stem of the process under the selection center (e.g. "Code"). None if unknown.
  pub source_app: Option<String>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  #[test]
  fn test_screenshot_data_creation() {
    let rgba = Arc::new(vec![255u8; 1920 * 1080 * 4]);
    let data = ScreenshotData {
      width: 1920,
      height: 1080,
      rgba: rgba.clone(),
      format: "png".to_string(),
      quality: 90,
      vd_offset_x: 0,
      vd_offset_y: 0,
    };

    assert_eq!(data.width, 1920);
    assert_eq!(data.height, 1080);
    assert_eq!(data.format, "png");
    assert_eq!(data.quality, 90);
    assert_eq!(data.vd_offset_x, 0);
    assert_eq!(data.vd_offset_y, 0);
    assert_eq!(data.rgba.len(), 1920 * 1080 * 4);
  }

  #[test]
  fn test_screenshot_data_clone() {
    let rgba = Arc::new(vec![128u8; 100 * 100 * 4]);
    let data = ScreenshotData {
      width: 100,
      height: 100,
      rgba: rgba.clone(),
      format: "jpg".to_string(),
      quality: 75,
      vd_offset_x: 100,
      vd_offset_y: 200,
    };

    let cloned = data.clone();
    assert_eq!(data.width, cloned.width);
    assert_eq!(data.height, cloned.height);
    assert_eq!(data.format, cloned.format);
    assert_eq!(data.quality, cloned.quality);
    assert_eq!(data.vd_offset_x, cloned.vd_offset_x);
    assert_eq!(data.vd_offset_y, cloned.vd_offset_y);

    // Arc clone shares data
    assert_eq!(Arc::strong_count(&data.rgba), 3); // original + cloned + rgba var
  }

  #[test]
  fn test_screenshot_data_multi_monitor_offsets() {
    let rgba = Arc::new(vec![0u8; 10 * 10 * 4]);
    let data = ScreenshotData {
      width: 10,
      height: 10,
      rgba,
      format: "png".to_string(),
      quality: 100,
      vd_offset_x: -1920, // Monitor to the left
      vd_offset_y: 0,
    };

    assert_eq!(data.vd_offset_x, -1920);
    assert_eq!(data.vd_offset_y, 0);
  }

  #[test]
  fn test_selection_result_creation() {
    let selection = SelectionResult {
      x: 100,
      y: 200,
      width: 300,
      height: 400,
      source_app: None,
    };

    assert_eq!(selection.x, 100);
    assert_eq!(selection.y, 200);
    assert_eq!(selection.width, 300);
    assert_eq!(selection.height, 400);
  }

  #[test]
  fn test_selection_result_copy() {
    let selection1 = SelectionResult {
      x: 50,
      y: 50,
      width: 100,
      height: 100,
      source_app: None,
    };

    let selection2 = selection1.clone(); // Clone
    assert_eq!(selection1, selection2);

    // selection1 still valid after copy
    assert_eq!(selection1.x, 50);
  }

  #[test]
  fn test_selection_result_equality() {
    let sel1 = SelectionResult {
      x: 0,
      y: 0,
      width: 100,
      height: 100,
      source_app: None,
    };

    let sel2 = SelectionResult {
      x: 0,
      y: 0,
      width: 100,
      height: 100,
      source_app: None,
    };

    let sel3 = SelectionResult {
      x: 0,
      y: 0,
      width: 200,
      height: 100,
      source_app: None,
    };

    assert_eq!(sel1, sel2);
    assert_ne!(sel1, sel3);
  }

  #[test]
  fn test_selection_result_zero_size() {
    let selection = SelectionResult {
      x: 100,
      y: 100,
      width: 0,
      height: 0,
      source_app: None,
    };

    assert_eq!(selection.width, 0);
    assert_eq!(selection.height, 0);
  }

  #[test]
  fn test_screenshot_data_different_formats() {
    let rgba = Arc::new(vec![0u8; 10 * 10 * 4]);

    let png_data = ScreenshotData {
      width: 10,
      height: 10,
      rgba: rgba.clone(),
      format: "png".to_string(),
      quality: 100,
      vd_offset_x: 0,
      vd_offset_y: 0,
    };

    let jpg_data = ScreenshotData {
      width: 10,
      height: 10,
      rgba: rgba.clone(),
      format: "jpg".to_string(),
      quality: 85,
      vd_offset_x: 0,
      vd_offset_y: 0,
    };

    let webp_data = ScreenshotData {
      width: 10,
      height: 10,
      rgba,
      format: "webp".to_string(),
      quality: 90,
      vd_offset_x: 0,
      vd_offset_y: 0,
    };

    assert_eq!(png_data.format, "png");
    assert_eq!(jpg_data.format, "jpg");
    assert_eq!(webp_data.format, "webp");
  }

  #[test]
  fn test_selection_result_debug_format() {
    let selection = SelectionResult {
      x: 10,
      y: 20,
      width: 30,
      height: 40,
      source_app: None,
    };

    let debug_str = format!("{:?}", selection);
    assert!(debug_str.contains("10"));
    assert!(debug_str.contains("20"));
    assert!(debug_str.contains("30"));
    assert!(debug_str.contains("40"));
  }

  #[test]
  fn test_screenshot_data_arc_sharing() {
    let rgba = Arc::new(vec![128u8; 100]);
    let initial_count = Arc::strong_count(&rgba);

    let data1 = ScreenshotData {
      width: 5,
      height: 5,
      rgba: rgba.clone(),
      format: "png".to_string(),
      quality: 90,
      vd_offset_x: 0,
      vd_offset_y: 0,
    };

    let _data2 = data1.clone();

    // Arc is shared between rgba, data1, and data2
    assert!(Arc::strong_count(&rgba) > initial_count);
  }
}
