// src/config/defaults.rs
// Default values for configuration fields

use super::{NotificationPosition, SaveLocationMode, ScreenshotFormat};
use crate::video::config::VideoQuality;

pub fn default_auto_start() -> bool {
  false
}

pub fn default_screenshot_area_shortcut() -> String {
  "Ctrl+PrintScreen".to_string()
}

pub fn default_screenshot_fullscreen_shortcut() -> String {
  "Shift+PrintScreen".to_string()
}

pub fn default_screenshot_active_window_shortcut() -> String {
  "Alt+PrintScreen".to_string()
}

/// Unassigned by default so an update never takes a key from another app.
pub fn default_screenshot_edit_shortcut() -> String {
  String::new()
}

pub fn default_selection_magnifier() -> bool {
  true
}

/// Unassigned by default so an update never takes a key from another app.
pub fn default_copy_text_shortcut() -> String {
  String::new()
}

/// Unassigned by default so an update never takes a key from another app.
pub fn default_color_picker_shortcut() -> String {
  String::new()
}

/// Unassigned by default so an update never takes a key from another app.
pub fn default_last_region_shortcut() -> String {
  String::new()
}

pub fn default_screenshot_format() -> ScreenshotFormat {
  ScreenshotFormat::Png
}

pub fn default_screenshot_quality() -> u8 {
  90
}

/// On by default: large PNGs, typically games and photos, become much smaller JPGs.
pub fn default_screenshot_large_png_as_jpg() -> bool {
  true
}

// Save location defaults
pub fn default_save_location_mode() -> SaveLocationMode {
  SaveLocationMode::SystemFolders
}

pub fn default_single_folder_subfolder_template() -> Option<String> {
  Some(crate::capture_subfolder::DEFAULT_TEMPLATE.to_string())
}

pub fn default_video_fps() -> u32 {
  30
}

pub fn default_video_quality() -> VideoQuality {
  VideoQuality::Medium
}

pub fn default_video_hardware_encoding() -> bool {
  true
}

pub fn default_record_area_shortcut() -> String {
  String::new()
}

pub fn default_record_fullscreen_shortcut() -> String {
  String::new()
}

pub fn default_record_active_window_shortcut() -> String {
  String::new()
}

pub fn default_record_audio() -> bool {
  false
}

pub fn default_show_cursor() -> bool {
  true
}

// Language default — reads OS locale and maps to a supported tag
pub fn default_language() -> String {
  let locale = sys_locale::get_locale().unwrap_or_default();
  let lang = locale.split(['-', '_']).next().unwrap_or("en");
  match lang {
    "fr" => "fr",
    "de" => "de",
    "es" => "es",
    "it" => "it",
    _ => "en",
  }
  .to_string()
}

// Notification overlay defaults
pub fn default_notifications_enabled() -> bool {
  true
}

pub fn default_notification_sound() -> bool {
  true
}

pub fn default_notification_position() -> NotificationPosition {
  NotificationPosition::BottomRight
}

pub fn default_notification_duration_ms() -> u64 {
  3000
}

// GIF recording defaults
pub fn default_gif_shortcut() -> String {
  String::new()
}

pub fn default_gif_fps() -> u32 {
  15
}

pub fn default_gif_max_duration() -> u32 {
  10
}

pub fn default_filename_template() -> Option<String> {
  Some(crate::filename::DEFAULT_TEMPLATE.to_string())
}

pub fn default_recording_visibility_warning_dismissed() -> bool {
  false
}

pub fn default_check_for_updates() -> bool {
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_default_auto_start() {
    assert!(!default_auto_start());
  }

  #[test]
  fn test_default_screenshot_format() {
    assert_eq!(default_screenshot_format(), ScreenshotFormat::Png);
  }

  #[test]
  fn test_default_screenshot_quality() {
    assert_eq!(default_screenshot_quality(), 90);
  }

  #[test]
  fn large_pngs_are_saved_as_jpg_by_default() {
    assert!(default_screenshot_large_png_as_jpg());
  }

  #[test]
  fn test_default_video_fps() {
    assert_eq!(default_video_fps(), 30);
  }

  #[test]
  fn test_default_video_quality() {
    assert_eq!(default_video_quality(), VideoQuality::Medium);
  }

  #[test]
  fn test_default_show_cursor() {
    assert!(default_show_cursor());
  }

  #[test]
  fn test_default_gif_fps() {
    assert_eq!(default_gif_fps(), 15);
  }

  #[test]
  fn test_default_gif_max_duration() {
    assert_eq!(default_gif_max_duration(), 10);
  }

  #[test]
  fn test_default_gif_shortcut() {
    assert_eq!(default_gif_shortcut(), "");
  }

  #[test]
  fn test_default_screenshot_shortcuts() {
    let area = default_screenshot_area_shortcut();
    assert!(!area.is_empty());
    assert!(area.contains("PrintScreen"));

    let fullscreen = default_screenshot_fullscreen_shortcut();
    assert_eq!(fullscreen, "Shift+PrintScreen");

    let window = default_screenshot_active_window_shortcut();
    assert!(!window.is_empty());
    assert!(window.contains("PrintScreen"));
  }

  #[test]
  fn test_default_save_location_mode() {
    assert_eq!(
      default_save_location_mode(),
      SaveLocationMode::SystemFolders
    );
  }

  #[test]
  fn test_default_single_folder_subfolder_template() {
    assert_eq!(
      default_single_folder_subfolder_template(),
      Some(crate::capture_subfolder::DEFAULT_TEMPLATE.to_string())
    );
  }
}
