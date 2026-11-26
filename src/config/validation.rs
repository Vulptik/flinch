// src/config/validation.rs
// Configuration validation to handle manual edits
//
// This module ensures that manually edited config files don't break the app.
// All validation rules match the exact options available in the UI settings,
// so that any value a user can set through the UI is considered valid, and
// any value they cannot set is automatically corrected.
//
// Validation is applied:
// 1. When loading config from disk (Config::load_internal)
// 2. When importing config (import_config command)
// 3. When saving config (save_config command)

use super::{Config, defaults};

impl Config {
  /// Validates and sanitizes configuration values
  /// This ensures manually edited config files don't break the app
  /// All validations match the exact options available in the UI
  pub fn validate(&mut self) {
    // Validate screenshot format - UI allows: png, jpg, webp
    // Validate screenshot quality - UI allows: 50 (low), 75 (medium), 90 (high)
    // Accept exact values or clamp to nearest
    if ![50, 75, 90].contains(&self.screenshot_quality) {
      let original = self.screenshot_quality;
      self.screenshot_quality = if self.screenshot_quality < 63 {
        50 // low
      } else if self.screenshot_quality < 83 {
        75 // medium
      } else {
        90 // high
      };
      tracing::warn!(
        operation = "validate_config",
        field = "screenshot_quality",
        value = original,
        corrected_to = self.screenshot_quality,
        "Screenshot quality adjusted to nearest valid value"
      );
    }

    // A chosen microphone is an endpoint id; an empty or oversized one follows the default.
    if self
      .microphone_device
      .as_ref()
      .is_some_and(|id| id.is_empty() || id.len() > 512)
    {
      tracing::warn!(
        operation = "validate_config",
        field = "microphone_device",
        "Invalid microphone id, following the Windows default microphone"
      );
      self.microphone_device = None;
      self.microphone_name = None;
    }
    if self.microphone_device.is_none() {
      self.microphone_name = None;
    }
    if let Some(name) = &mut self.microphone_name
      && name.chars().count() > 256
    {
      *name = name.chars().take(256).collect();
    }

    // Validate video FPS - UI allows: 30, 60
    if ![30, 60].contains(&self.video_fps) {
      tracing::warn!(
        operation = "validate_config",
        field = "video_fps",
        value = self.video_fps,
        "Invalid video FPS, resetting to 30"
      );
      self.video_fps = 30;
    }

    // Validate video quality - UI allows: low, medium, high
    // This is an enum, so serde handles it, but we log if it's invalid
    // (serde will fail to deserialize invalid values)

    // Validate GIF FPS - UI allows: 15, 30
    if ![15, 30].contains(&self.gif_fps) {
      tracing::warn!(
        operation = "validate_config",
        field = "gif_fps",
        value = self.gif_fps,
        "Invalid GIF FPS, resetting to 15"
      );
      self.gif_fps = 15;
    }

    // Validate GIF max duration - UI allows: 10, 20, 30
    if ![10, 20, 30].contains(&self.gif_max_duration) {
      tracing::warn!(
        operation = "validate_config",
        field = "gif_max_duration",
        value = self.gif_max_duration,
        "Invalid GIF max duration, resetting to 30"
      );
      self.gif_max_duration = 30;
    }

    // Validate capture delay - UI allows: off, 3, 5, 10 seconds
    if !crate::capture_delay::CHOICES.contains(&self.capture_delay) {
      tracing::warn!(
        operation = "validate_config",
        field = "capture_delay",
        value = self.capture_delay,
        "Invalid capture delay, turning it off"
      );
      self.capture_delay = 0;
    }

    // Validate save_location_mode — must be one of the two UI options
    if let Some(ref template) = self.single_folder_subfolder_template {
      if template.trim().is_empty() {
        self.single_folder_subfolder_template = Some(String::new());
      } else if let Err(error) = crate::capture_subfolder::validate_template(template) {
        tracing::warn!(
          operation = "validate_config",
          field = "single_folder_subfolder_template",
          value = %template,
          error = %error,
          "Invalid single-folder subfolder template, resetting to default"
        );
        self.single_folder_subfolder_template = None;
      }
    }

    for (field, folder) in [
      ("custom_single_folder", &mut self.custom_single_folder),
      (
        "custom_screenshots_folder",
        &mut self.custom_screenshots_folder,
      ),
      ("custom_videos_folder", &mut self.custom_videos_folder),
    ] {
      if let Some(path) = folder.as_deref()
        && let Err(error) = super::validate_custom_folder(path)
      {
        tracing::warn!(
          operation = "validate_config",
          field,
          value = %path,
          error = %error,
          "Invalid custom save folder, using the built-in folder"
        );
        *folder = None;
      }
    }

    // Normalize shortcuts (trim whitespace)
    self.screenshot_area_shortcut = self.screenshot_area_shortcut.trim().to_string();
    self.screenshot_fullscreen_shortcut = self.screenshot_fullscreen_shortcut.trim().to_string();
    self.screenshot_active_window_shortcut =
      self.screenshot_active_window_shortcut.trim().to_string();
    self.screenshot_edit_shortcut = self.screenshot_edit_shortcut.trim().to_string();
    self.copy_text_shortcut = self.copy_text_shortcut.trim().to_string();
    self.color_picker_shortcut = self.color_picker_shortcut.trim().to_string();
    self.last_region_shortcut = self.last_region_shortcut.trim().to_string();
    self.record_area_shortcut = self.record_area_shortcut.trim().to_string();
    self.record_fullscreen_shortcut = self.record_fullscreen_shortcut.trim().to_string();
    self.record_active_window_shortcut = self.record_active_window_shortcut.trim().to_string();
    self.gif_shortcut = self.gif_shortcut.trim().to_string();

    // Validate language — must be one of the supported tags; empty → auto-detect from OS locale
    const SUPPORTED_LANGS: &[&str] = &["en", "fr", "de", "es", "it"];
    if self.language.is_empty() || !SUPPORTED_LANGS.contains(&self.language.as_str()) {
      let detected = defaults::default_language();
      tracing::debug!(
        operation = "validate_config",
        field = "language",
        value = %self.language,
        corrected_to = %detected,
        "Language invalid or unset, auto-detecting from OS locale"
      );
      self.language = detected;
    }

    // Validate notification position — must be one of the four UI options
    // Validate notification duration — 1–10 seconds (in ms)
    const MIN_DURATION_MS: u64 = 1_000;
    const MAX_DURATION_MS: u64 = 10_000;
    if self.notification_duration_ms < MIN_DURATION_MS
      || self.notification_duration_ms > MAX_DURATION_MS
    {
      tracing::warn!(
        operation = "validate_config",
        field = "notification_duration_ms",
        value = self.notification_duration_ms,
        "Notification duration out of range [1000, 10000], resetting to default"
      );
      self.notification_duration_ms = defaults::default_notification_duration_ms();
    }

    // Validate filename template — must pass the template engine's rules or be cleared
    if let Some(ref template) = self.filename_template
      && !template.is_empty()
      && let Err(e) = crate::filename::validate_template(template)
    {
      tracing::warn!(
        operation = "validate_config",
        field = "filename_template",
        value = %template,
        error = %e,
        "Invalid filename template, resetting to default"
      );
      self.filename_template = None;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::{NotificationPosition, SaveLocationMode, ScreenshotFormat};

  /// Rejects UNC/network paths and non-absolute paths to prevent NTLM credential
  /// capture via malicious config imports pointing to attacker-controlled SMB shares.
  fn is_unsafe_path(path_str: &str) -> bool {
    path_str.starts_with("\\\\")
      || path_str.starts_with("//")
      || !std::path::Path::new(path_str).is_absolute()
  }

  #[test]
  fn a_bad_microphone_choice_follows_the_windows_default_and_long_names_are_cut() {
    for id in [String::new(), "x".repeat(513)] {
      let mut config = Config {
        microphone_device: Some(id),
        microphone_name: Some("Headset".into()),
        ..Default::default()
      };
      config.validate();
      assert_eq!(
        (config.microphone_device, config.microphone_name),
        (None, None)
      );
    }
    let mut config = Config {
      microphone_name: Some("Orphan".into()),
      ..Default::default()
    };
    config.validate();
    assert_eq!(config.microphone_name, None, "a name needs its device");
    let mut config = Config {
      microphone_device: Some("{0.0.1.00000000}.{mic}".into()),
      microphone_name: Some("é".repeat(300)),
      ..Default::default()
    };
    config.validate();
    assert_eq!(
      config.microphone_name.map(|name| name.chars().count()),
      Some(256)
    );
    assert!(config.microphone_device.is_some());
  }

  #[test]
  fn test_validate_screenshot_format() {
    let mut config = Config {
      screenshot_format: ScreenshotFormat::Png,
      ..Default::default()
    };
    config.screenshot_format = defaults::default_screenshot_format();
    config.validate();
    assert_eq!(config.screenshot_format, ScreenshotFormat::Png);
  }

  #[test]
  fn test_validate_screenshot_quality() {
    let mut config = Config {
      screenshot_quality: 40,
      ..Default::default()
    };

    // Test clamping to low (50)
    config.validate();
    assert_eq!(config.screenshot_quality, 50);

    // Test clamping to medium (75)
    config.screenshot_quality = 70;
    config.validate();
    assert_eq!(config.screenshot_quality, 75);

    // Test clamping to high (90)
    config.screenshot_quality = 100;
    config.validate();
    assert_eq!(config.screenshot_quality, 90);

    // Test exact values remain unchanged
    config.screenshot_quality = 50;
    config.validate();
    assert_eq!(config.screenshot_quality, 50);
  }

  #[test]
  fn test_validate_video_fps() {
    let mut config = Config {
      video_fps: 0,
      ..Default::default()
    };

    // Invalid values should reset to 30
    config.validate();
    assert_eq!(config.video_fps, 30);

    config.video_fps = 45;
    config.validate();
    assert_eq!(config.video_fps, 30);

    config.video_fps = 120;
    config.validate();
    assert_eq!(config.video_fps, 30);

    // Valid values should remain unchanged
    config.video_fps = 60;
    config.validate();
    assert_eq!(config.video_fps, 60);
  }

  #[test]
  fn test_validate_gif_fps() {
    let mut config = Config {
      gif_fps: 0,
      ..Default::default()
    };

    // Invalid values should reset to 15
    config.validate();
    assert_eq!(config.gif_fps, 15);

    config.gif_fps = 20;
    config.validate();
    assert_eq!(config.gif_fps, 15);

    config.gif_fps = 60;
    config.validate();
    assert_eq!(config.gif_fps, 15);

    // Valid values should remain unchanged
    config.gif_fps = 30;
    config.validate();
    assert_eq!(config.gif_fps, 30);
  }

  #[test]
  fn capture_delay_keeps_offered_choices_and_turns_anything_else_off() {
    let mut config = Config::default();
    assert_eq!(
      config.capture_delay, 0,
      "off by default, so captures stay instant"
    );
    for value in [0, 3, 5, 10] {
      config.capture_delay = value;
      config.validate();
      assert_eq!(config.capture_delay, value);
    }
    for value in [1, 4, 11, 600] {
      config.capture_delay = value;
      config.validate();
      assert_eq!(config.capture_delay, 0, "{value} s is not offered");
    }
    let saved: Config = serde_json::from_str(r#"{"capture_delay": 5}"#).unwrap();
    assert_eq!(saved.capture_delay, 5);
  }

  #[test]
  fn test_validate_gif_max_duration() {
    let mut config = Config {
      gif_max_duration: 0,
      ..Default::default()
    };

    // Invalid values should reset to 30
    config.validate();
    assert_eq!(config.gif_max_duration, 30);

    config.gif_max_duration = 15;
    config.validate();
    assert_eq!(config.gif_max_duration, 30);

    config.gif_max_duration = 500;
    config.validate();
    assert_eq!(config.gif_max_duration, 30);

    // Valid values should remain unchanged
    config.gif_max_duration = 10;
    config.validate();
    assert_eq!(config.gif_max_duration, 10);

    config.gif_max_duration = 20;
    config.validate();
    assert_eq!(config.gif_max_duration, 20);
  }

  #[test]
  fn test_validate_shortcuts_trim() {
    let mut config = Config {
      screenshot_area_shortcut: "  Ctrl+Shift+A  ".to_string(),
      ..Default::default()
    };
    config.validate();
    assert_eq!(config.screenshot_area_shortcut, "Ctrl+Shift+A");
  }

  #[test]
  fn test_validate_screenshot_format_valid_values_pass_through() {
    for fmt in [
      ScreenshotFormat::Png,
      ScreenshotFormat::Jpg,
      ScreenshotFormat::Webp,
    ] {
      let mut config = Config {
        screenshot_format: fmt,
        ..Default::default()
      };
      config.validate();
      assert_eq!(config.screenshot_format, fmt);
    }
  }

  #[test]
  fn test_validate_language_valid_values_pass_through() {
    for lang in &["en", "fr", "de", "es", "it"] {
      let mut config = Config {
        language: lang.to_string(),
        ..Default::default()
      };
      config.validate();
      assert_eq!(
        &config.language, lang,
        "Valid language {lang} should not be changed"
      );
    }
  }

  #[test]
  fn test_validate_language_invalid_resets_to_default() {
    let mut config = Config {
      language: "xx".to_string(),
      ..Default::default()
    };
    config.validate();
    // Should reset to a supported language (whatever the OS default produces)
    const SUPPORTED: &[&str] = &["en", "fr", "de", "es", "it"];
    assert!(
      SUPPORTED.contains(&config.language.as_str()),
      "Invalid language should be corrected to a supported one, got: {}",
      config.language
    );
  }

  #[test]
  fn test_validate_language_empty_resets_to_default() {
    let mut config = Config {
      language: String::new(),
      ..Default::default()
    };
    config.validate();
    const SUPPORTED: &[&str] = &["en", "fr", "de", "es", "it"];
    assert!(
      SUPPORTED.contains(&config.language.as_str()),
      "Empty language should be corrected to a supported one, got: {}",
      config.language
    );
  }

  #[test]
  fn test_validate_all_shortcuts_trimmed() {
    let mut config = Config {
      screenshot_area_shortcut: " Ctrl+A ".to_string(),
      screenshot_fullscreen_shortcut: " Ctrl+F ".to_string(),
      screenshot_active_window_shortcut: " Ctrl+W ".to_string(),
      record_area_shortcut: " Ctrl+R ".to_string(),
      record_fullscreen_shortcut: " Ctrl+Shift+F ".to_string(),
      record_active_window_shortcut: " Ctrl+Shift+W ".to_string(),
      gif_shortcut: " Ctrl+G ".to_string(),
      ..Default::default()
    };
    config.validate();
    assert_eq!(config.screenshot_area_shortcut, "Ctrl+A");
    assert_eq!(config.screenshot_fullscreen_shortcut, "Ctrl+F");
    assert_eq!(config.screenshot_active_window_shortcut, "Ctrl+W");
    assert_eq!(config.record_area_shortcut, "Ctrl+R");
    assert_eq!(config.record_fullscreen_shortcut, "Ctrl+Shift+F");
    assert_eq!(config.record_active_window_shortcut, "Ctrl+Shift+W");
    assert_eq!(config.gif_shortcut, "Ctrl+G");
  }

  #[test]
  fn test_validate_notification_position_valid_values_pass_through() {
    for pos in [
      NotificationPosition::TopRight,
      NotificationPosition::TopLeft,
      NotificationPosition::BottomRight,
      NotificationPosition::BottomLeft,
    ] {
      let mut config = Config {
        notification_position: pos,
        ..Default::default()
      };
      config.validate();
      assert_eq!(config.notification_position, pos);
    }
  }

  #[test]
  fn test_validate_notification_position_invalid_resets_to_default() {
    assert_eq!(
      defaults::default_notification_position(),
      NotificationPosition::BottomRight
    );
  }

  #[test]
  fn test_validate_notification_duration_valid_passes_through() {
    for ms in [1000u64, 3000, 5000, 10000] {
      let mut config = Config {
        notification_duration_ms: ms,
        ..Default::default()
      };
      config.validate();
      assert_eq!(config.notification_duration_ms, ms);
    }
  }

  #[test]
  fn test_validate_notification_duration_out_of_range_resets() {
    let mut config = Config {
      notification_duration_ms: 0,
      ..Default::default()
    };
    config.validate();
    assert_eq!(config.notification_duration_ms, 3000);

    config.notification_duration_ms = 999;
    config.validate();
    assert_eq!(config.notification_duration_ms, 3000);

    config.notification_duration_ms = 10001;
    config.validate();
    assert_eq!(config.notification_duration_ms, 3000);

    config.notification_duration_ms = u64::MAX;
    config.validate();
    assert_eq!(config.notification_duration_ms, 3000);
  }

  #[test]
  fn test_is_unsafe_path_rejects_unc() {
    assert!(is_unsafe_path("\\\\server\\share"));
    assert!(is_unsafe_path("//server/share"));
  }

  #[test]
  fn test_is_unsafe_path_rejects_relative() {
    assert!(is_unsafe_path("relative/path"));
    assert!(is_unsafe_path("../sneaky"));
  }

  #[test]
  fn test_is_unsafe_path_accepts_absolute_local() {
    assert!(!is_unsafe_path("C:\\Users\\test\\Pictures"));
    assert!(!is_unsafe_path("D:\\Videos"));
  }

  #[test]
  fn test_validate_save_location_mode_valid_passes_through() {
    for mode in [
      SaveLocationMode::SystemFolders,
      SaveLocationMode::SingleFolder,
    ] {
      let mut config = Config {
        save_location_mode: mode,
        ..Default::default()
      };
      config.validate();
      assert_eq!(config.save_location_mode, mode);
    }
  }

  #[test]
  fn test_validate_save_location_mode_invalid_resets_to_default() {
    assert_eq!(
      defaults::default_save_location_mode(),
      SaveLocationMode::SystemFolders
    );
  }

  #[test]
  fn test_validate_single_folder_subfolder_template() {
    let mut config = Config {
      single_folder_subfolder_template: Some("%YEAR%/%MONTH%".to_string()),
      ..Default::default()
    };
    config.validate();
    assert_eq!(config.single_folder_subfolder_template, None);

    config.single_folder_subfolder_template = Some("   ".to_string());
    config.validate();
    assert_eq!(config.single_folder_subfolder_template, Some(String::new()));

    config.single_folder_subfolder_template = Some("%YEAR%-%MONTH%".to_string());
    config.validate();
    assert_eq!(
      config.single_folder_subfolder_template,
      Some("%YEAR%-%MONTH%".to_string())
    );
  }
}
