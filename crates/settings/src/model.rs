use serde::Deserialize;

pub const LOCALES: [&str; 5] = ["en", "fr", "de", "es", "it"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
  General,
  Notifications,
  Hotkeys,
  Screenshots,
  Videos,
  Files,
  About,
  Tools,
}

impl Section {
  pub const ALL: [Self; 8] = [
    Self::General,
    Self::Notifications,
    Self::Hotkeys,
    Self::Screenshots,
    Self::Videos,
    Self::Files,
    Self::About,
    Self::Tools,
  ];

  pub fn id(self) -> &'static str {
    match self {
      Self::General => "general",
      Self::Notifications => "notifications",
      Self::Hotkeys => "hotkeys",
      Self::Screenshots => "screenshot",
      Self::Videos => "video",
      Self::Files => "files",
      Self::About => "about",
      Self::Tools => "tools",
    }
  }

  pub fn index(self) -> i32 {
    Self::ALL
      .iter()
      .position(|section| *section == self)
      .map_or(0, |index| index as i32)
  }

  pub fn from_id(id: &str) -> Option<Self> {
    Self::ALL.into_iter().find(|section| section.id() == id)
  }
}

/// A read-only projection of the persisted configuration, not a second config
/// schema or writer. The application supplies validated values in production.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingsSnapshot {
  pub language: String,
  pub auto_start: bool,
  pub check_for_updates: bool,
  pub notifications_enabled: bool,
  pub notification_sound: bool,
  pub notification_duration_ms: u64,
  pub notification_position: String,
  pub screenshot_format: String,
  pub screenshot_quality: u8,
  pub gif_fps: u32,
  pub gif_max_duration: u32,
  pub video_fps: u32,
  pub video_quality: String,
  pub video_hardware_encoding: bool,
  pub record_audio: bool,
  pub show_cursor: bool,
  pub gif_show_cursor: bool,
  pub show_clicks: bool,
  pub gif_show_clicks: bool,
  #[serde(default)]
  pub record_microphone: bool,
  #[serde(default)]
  pub microphone_device: Option<String>,
  #[serde(default)]
  pub microphone_name: Option<String>,
  #[serde(default)]
  pub selection_magnifier: bool,
  pub copy_text_direct: bool,
  #[serde(default)]
  pub capture_delay: u32,
  #[serde(default)]
  pub screenshot_large_png_as_jpg: bool,
  pub save_location_mode: String,
  pub custom_single_folder: Option<String>,
  pub custom_screenshots_folder: Option<String>,
  pub custom_videos_folder: Option<String>,
  pub filename_template: Option<String>,
  pub single_folder_subfolder_template: Option<String>,
  pub screenshot_area_shortcut: String,
  pub screenshot_fullscreen_shortcut: String,
  pub screenshot_active_window_shortcut: String,
  pub screenshot_edit_shortcut: String,
  pub copy_text_shortcut: String,
  pub color_picker_shortcut: String,
  pub last_region_shortcut: String,
  pub gif_shortcut: String,
  pub record_area_shortcut: String,
  pub record_fullscreen_shortcut: String,
  pub record_active_window_shortcut: String,
}

impl SettingsSnapshot {
  /// Refuse values the current widgets cannot represent rather than silently
  /// displaying another choice. The read-only runner never repairs user files.
  pub fn validate_projection(&self) -> Result<(), String> {
    let checks = [
      (
        self.language.is_empty() || LOCALES.contains(&self.language.as_str()),
        "language",
      ),
      (
        ["png", "jpg", "webp"].contains(&self.screenshot_format.as_str()),
        "screenshot_format",
      ),
      (
        [50, 75, 90].contains(&self.screenshot_quality),
        "screenshot_quality",
      ),
      ([15, 30].contains(&self.gif_fps), "gif_fps"),
      (
        [10, 20, 30].contains(&self.gif_max_duration),
        "gif_max_duration",
      ),
      ([0, 3, 5, 10].contains(&self.capture_delay), "capture_delay"),
      ([30, 60].contains(&self.video_fps), "video_fps"),
      (
        ["low", "medium", "high"].contains(&self.video_quality.as_str()),
        "video_quality",
      ),
      (
        (1000..=10000).contains(&self.notification_duration_ms),
        "notification_duration_ms",
      ),
      (
        ["top-right", "top-left", "bottom-right", "bottom-left"]
          .contains(&self.notification_position.as_str()),
        "notification_position",
      ),
      (
        ["system_folders", "single_folder"].contains(&self.save_location_mode.as_str()),
        "save_location_mode",
      ),
    ];
    for (valid, field) in checks {
      if !valid {
        return Err(format!("Settings snapshot contains an unsupported {field}"));
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn section_ids_preserve_tray_and_backend_navigation() {
    for section in Section::ALL {
      assert_eq!(Section::from_id(section.id()), Some(section));
      assert_eq!(Section::ALL[section.index() as usize], section);
    }
    assert_eq!(Section::from_id("debug"), None);
    assert_eq!(Section::from_id("screenshots"), None);
  }

  #[test]
  fn bundled_config_projects_without_inventing_defaults() {
    let snapshot: SettingsSnapshot =
      serde_json::from_str(include_str!("../../../resources/defaultConfig.json"))
        .expect("bundled config");
    snapshot.validate_projection().expect("supported values");
    assert!(!snapshot.auto_start);
    assert!(!snapshot.record_audio);
    assert_eq!(snapshot.notification_duration_ms, 3000);
    assert_eq!(snapshot.video_fps, 30);
  }

  #[test]
  fn projection_accepts_custom_notification_duration_but_rejects_unknown_choices() {
    let mut snapshot: SettingsSnapshot =
      serde_json::from_str(include_str!("../../../resources/defaultConfig.json"))
        .expect("bundled config");
    snapshot.notification_duration_ms = 2500;
    assert!(snapshot.validate_projection().is_ok());
    snapshot.video_fps = 144;
    assert!(snapshot.validate_projection().is_err());
  }
}
