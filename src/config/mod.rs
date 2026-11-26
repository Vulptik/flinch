// src/config/mod.rs
// Main configuration module

mod backup;
mod defaults;
mod paths;
mod salvage;
pub mod types;
mod validation;

use anyhow::{Context, Result};
use chrono::Local;
use flinch_settings::edit::FolderTarget;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Which type of capture is being saved — used to resolve the output directory.
#[derive(Debug, Clone, Copy)]
pub enum CaptureType {
  Screenshot,
  Video,
  Gif,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveLocationMode {
  SystemFolders,
  SingleFolder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotFormat {
  Png,
  Jpg,
  Webp,
}

impl ScreenshotFormat {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Png => "png",
      Self::Jpg => "jpg",
      Self::Webp => "webp",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationPosition {
  TopRight,
  TopLeft,
  BottomRight,
  BottomLeft,
}

impl CaptureType {
  #[cfg(test)]
  fn folder_name(self) -> &'static str {
    match self {
      CaptureType::Screenshot => "screenshots",
      CaptureType::Video => "videos",
      CaptureType::Gif => "gifs",
    }
  }
}

// Re-export video config types
pub use crate::video::config::VideoQuality;

// Re-export types
pub use types::ConfigError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
  // Save location settings
  // Mode: "system_folders" (Pictures\Flinch + Videos\Flinch) or "single_folder" (Documents\Flinch\YYYY-MM\)
  #[serde(default = "defaults::default_save_location_mode")]
  pub save_location_mode: SaveLocationMode,
  // Optional single subfolder template for single-folder mode.
  // None = built-in default ("%YEAR%-%MONTH%"), Some("") = no subfolder.
  #[serde(default = "defaults::default_single_folder_subfolder_template")]
  #[serde(skip_serializing_if = "Option::is_none")]
  pub single_folder_subfolder_template: Option<String>,
  // Folders replacing the built-in ones, used exactly as chosen. None = built-in.
  // Each save location mode keeps its own, so switching modes loses nothing.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub custom_single_folder: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub custom_screenshots_folder: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub custom_videos_folder: Option<String>,
  #[serde(default = "defaults::default_screenshot_area_shortcut")]
  pub screenshot_area_shortcut: String,
  #[serde(default = "defaults::default_screenshot_fullscreen_shortcut")]
  pub screenshot_fullscreen_shortcut: String,
  #[serde(default = "defaults::default_screenshot_active_window_shortcut")]
  pub screenshot_active_window_shortcut: String,
  #[serde(default = "defaults::default_screenshot_edit_shortcut")]
  pub screenshot_edit_shortcut: String,
  #[serde(default = "defaults::default_copy_text_shortcut")]
  pub copy_text_shortcut: String,
  #[serde(default = "defaults::default_color_picker_shortcut")]
  pub color_picker_shortcut: String,
  #[serde(default = "defaults::default_last_region_shortcut")]
  pub last_region_shortcut: String,
  /// Seconds every capture waits before it starts: 0 (off), 3, 5 or 10.
  #[serde(default)]
  pub capture_delay: u32,
  #[serde(default = "defaults::default_selection_magnifier")]
  pub selection_magnifier: bool,
  #[serde(default)]
  pub copy_text_direct: bool,
  #[serde(default = "defaults::default_screenshot_format")]
  pub screenshot_format: ScreenshotFormat,
  #[serde(default = "defaults::default_screenshot_quality")]
  pub screenshot_quality: u8,
  /// Save a PNG screenshot over `screenshot::encoding::LARGE_PNG_BYTES` as a JPG instead.
  #[serde(default = "defaults::default_screenshot_large_png_as_jpg")]
  pub screenshot_large_png_as_jpg: bool,
  #[serde(default = "defaults::default_auto_start")]
  pub auto_start: bool,

  // Video recording settings
  #[serde(default = "defaults::default_video_fps")]
  pub video_fps: u32,
  #[serde(default = "defaults::default_video_quality")]
  pub video_quality: VideoQuality,
  #[serde(default = "defaults::default_video_hardware_encoding")]
  pub video_hardware_encoding: bool,
  #[serde(default = "defaults::default_record_area_shortcut")]
  pub record_area_shortcut: String,
  #[serde(default = "defaults::default_record_fullscreen_shortcut")]
  pub record_fullscreen_shortcut: String,
  #[serde(default = "defaults::default_record_active_window_shortcut")]
  pub record_active_window_shortcut: String,

  // Audio recording
  #[serde(default = "defaults::default_record_audio")]
  pub record_audio: bool,
  /// Record a microphone with videos, mixed with system audio.
  #[serde(default)]
  pub record_microphone: bool,
  /// The chosen microphone's endpoint id; None follows the Windows default microphone.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub microphone_device: Option<String>,
  /// Its name when chosen, shown while it is unplugged.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub microphone_name: Option<String>,

  // Cursor visibility in recordings
  #[serde(default = "defaults::default_show_cursor")]
  pub show_cursor: bool,
  #[serde(default = "defaults::default_show_cursor")]
  pub gif_show_cursor: bool,
  /// Click highlights drawn into videos and GIFs; off unless chosen.
  #[serde(default)]
  pub show_clicks: bool,
  #[serde(default)]
  pub gif_show_clicks: bool,

  // GIF recording settings
  #[serde(default = "defaults::default_gif_shortcut")]
  pub gif_shortcut: String,
  #[serde(default = "defaults::default_gif_fps")]
  pub gif_fps: u32,
  #[serde(default = "defaults::default_gif_max_duration")]
  pub gif_max_duration: u32,

  // Notification overlay settings
  #[serde(default = "defaults::default_notifications_enabled")]
  pub notifications_enabled: bool,
  #[serde(default = "defaults::default_notification_sound")]
  pub notification_sound: bool,
  #[serde(default = "defaults::default_notification_position")]
  pub notification_position: NotificationPosition,
  #[serde(default = "defaults::default_notification_duration_ms")]
  pub notification_duration_ms: u64,

  // Interface language (BCP 47 tag: "en", "fr", "de", "es", "it")
  // Empty string means "not yet set" — populated by default_language() on first load
  #[serde(default)]
  pub language: String,

  // Filename template for saved files.
  // Default: "%DATETIME%_%APP%_%TYPE%".
  // Supports tokens: %TYPE%, %APP%, %DATETIME%, %DATE%, %TIME%,
  //                  %YEAR%, %MONTH%, %DAY%, %HOUR%, %MINUTE%, %SECOND%, %TIMESTAMP%
  #[serde(default = "defaults::default_filename_template")]
  #[serde(skip_serializing_if = "Option::is_none")]
  pub filename_template: Option<String>,

  #[serde(default = "defaults::default_recording_visibility_warning_dismissed")]
  pub recording_visibility_warning_dismissed: bool,

  #[serde(default = "defaults::default_check_for_updates")]
  pub check_for_updates: bool,
}

static PENDING_LOAD_WARNING: Mutex<Option<String>> = Mutex::new(None);

impl Default for Config {
  fn default() -> Self {
    Self {
      save_location_mode: defaults::default_save_location_mode(),
      single_folder_subfolder_template: defaults::default_single_folder_subfolder_template(),
      custom_single_folder: None,
      custom_screenshots_folder: None,
      custom_videos_folder: None,
      screenshot_area_shortcut: defaults::default_screenshot_area_shortcut(),
      screenshot_fullscreen_shortcut: defaults::default_screenshot_fullscreen_shortcut(),
      screenshot_active_window_shortcut: defaults::default_screenshot_active_window_shortcut(),
      screenshot_edit_shortcut: defaults::default_screenshot_edit_shortcut(),
      copy_text_shortcut: defaults::default_copy_text_shortcut(),
      color_picker_shortcut: defaults::default_color_picker_shortcut(),
      last_region_shortcut: defaults::default_last_region_shortcut(),
      capture_delay: 0,
      selection_magnifier: defaults::default_selection_magnifier(),
      copy_text_direct: false,
      screenshot_format: defaults::default_screenshot_format(),
      screenshot_quality: defaults::default_screenshot_quality(),
      screenshot_large_png_as_jpg: defaults::default_screenshot_large_png_as_jpg(),
      auto_start: defaults::default_auto_start(),
      // Video recording
      video_fps: defaults::default_video_fps(),
      video_quality: defaults::default_video_quality(),
      video_hardware_encoding: defaults::default_video_hardware_encoding(),
      record_area_shortcut: defaults::default_record_area_shortcut(),
      record_fullscreen_shortcut: defaults::default_record_fullscreen_shortcut(),
      record_active_window_shortcut: defaults::default_record_active_window_shortcut(),
      record_audio: defaults::default_record_audio(),
      record_microphone: false,
      microphone_device: None,
      microphone_name: None,
      show_cursor: defaults::default_show_cursor(),
      gif_show_cursor: defaults::default_show_cursor(),
      show_clicks: false,
      gif_show_clicks: false,
      // GIF recording
      gif_shortcut: defaults::default_gif_shortcut(),
      gif_fps: defaults::default_gif_fps(),
      gif_max_duration: defaults::default_gif_max_duration(),
      // Notification overlay
      notifications_enabled: defaults::default_notifications_enabled(),
      notification_sound: defaults::default_notification_sound(),
      notification_position: defaults::default_notification_position(),
      notification_duration_ms: defaults::default_notification_duration_ms(),
      // Language — empty triggers auto-detection on first validate()
      language: String::new(),
      filename_template: defaults::default_filename_template(),
      recording_visibility_warning_dismissed:
        defaults::default_recording_visibility_warning_dismissed(),
      check_for_updates: defaults::default_check_for_updates(),
    }
  }
}

impl Config {
  // Path-related functions (delegated to paths module)
  pub fn config_file() -> Result<PathBuf> {
    paths::config_file()
  }

  pub fn default_config_file(app: &crate::app::context::AppHandle) -> Option<PathBuf> {
    paths::default_config_file(app)
  }

  // Config loading and saving
  pub fn load() -> Self {
    // Load without AppHandle - will use hardcoded defaults
    Self::load_internal(None)
  }

  pub fn load_with_app_handle(app: &crate::app::context::AppHandle) -> Self {
    // Load with AppHandle - can access bundled defaultConfig.json
    Self::load_internal(Some(app))
  }

  pub fn current(app: &crate::app::context::AppHandle) -> Self {
    app
      .try_state::<crate::state::ConfigState>()
      .map(|state| state.snapshot())
      .unwrap_or_else(|| Self::load_with_app_handle(app))
  }

  pub fn replace_current(app: &crate::app::context::AppHandle, config: Self) {
    if let Some(state) = app.try_state::<crate::state::ConfigState>() {
      state.replace(config);
    }
  }

  pub fn take_pending_load_warning() -> Option<String> {
    match PENDING_LOAD_WARNING.lock() {
      Ok(mut pending) => pending.take(),
      Err(error) => {
        tracing::warn!(
          operation = "config_warning",
          error = %error,
          "Failed to read pending config recovery warning"
        );
        None
      },
    }
  }

  fn load_internal(app: Option<&crate::app::context::AppHandle>) -> Config {
    Self::load_from_paths(app.and_then(Self::default_config_file), Self::config_file())
  }

  fn load_from_paths(default_path: Option<PathBuf>, config_path: Result<PathBuf>) -> Config {
    // Try to load default config from bundled resource
    let mut default_config = Self::default();
    if let Some(default_path) = default_path
      && default_path.exists()
    {
      if let Ok(content) = fs::read_to_string(&default_path) {
        if let Ok(loaded_default) = serde_json::from_str::<Config>(&content) {
          // Apply platform-specific overrides for shortcuts
          default_config = loaded_default;
          default_config.screenshot_area_shortcut = defaults::default_screenshot_area_shortcut();
          default_config.screenshot_fullscreen_shortcut =
            defaults::default_screenshot_fullscreen_shortcut();
          default_config.screenshot_active_window_shortcut =
            defaults::default_screenshot_active_window_shortcut();
        } else {
          tracing::warn!(
              operation = "load_default_config",
              path = %default_path.display(),
              "Failed to parse defaultConfig.json, using hardcoded defaults"
          );
        }
      } else {
        tracing::warn!(
            operation = "load_default_config",
            path = %default_path.display(),
            "Failed to read defaultConfig.json, using hardcoded defaults"
        );
      }
    }

    let json_path = match config_path {
      Ok(path) => path,
      Err(_) => {
        tracing::debug!("Could not get config file path, using defaults");
        return default_config;
      },
    };

    if json_path.exists() {
      match fs::read_to_string(&json_path) {
        Ok(content) => match serde_json::from_str::<Config>(&content) {
          Ok(mut cfg) => {
            // Validate and sanitize config after loading
            cfg.validate();
            cfg
          },
          Err(error) => {
            tracing::warn!(
                operation = "load_config",
                path = %json_path.display(),
                error = %error,
                "Failed to parse JSON config, recovering what it can"
            );
            if let Some(restored) = restore_saved_settings(&json_path, &content, &default_config) {
              return restored;
            }
            default_config.validate();
            recover_config(&json_path, &default_config);
            default_config
          },
        },
        Err(error) => {
          tracing::warn!(
              operation = "load_config",
              path = %json_path.display(),
              error = %error,
              "Failed to read JSON config; leaving the saved file untouched"
          );
          default_config.validate();
          queue_load_warning(&format!(
            "{}\n{}\n{}",
            crate::i18n::config_recovery(&default_config.language).failed,
            json_path.display(),
            error
          ));
          default_config
        },
      }
    } else {
      tracing::debug!(
          operation = "load_config",
          path = %json_path.display(),
          "Config file does not exist, using defaults"
      );
      // Validate defaults so language gets auto-detected from OS locale on first launch
      default_config.validate();
      default_config
    }
  }

  pub fn save(&self) -> Result<()> {
    // Save main config.
    let path = Self::config_file().context("Failed to get config file path")?;

    write_config(&path, self).map(|_| ())
  }

  /// Keep a copy of the saved settings the first time this app version runs.
  pub fn back_up_for_current_version() -> Result<()> {
    let path = Self::config_file().context("Failed to get config file path")?;

    backup::back_up_for_version(&path, env!("CARGO_PKG_VERSION")).map(|_| ())
  }

  // Helper methods

  /// Resolves the output directory for the given capture type, applying the configured
  /// save location mode and optional date/type subfolder structure.
  ///
  /// - `system_folders`: Screenshots/GIFs → Pictures\Flinch, Videos → Videos\Flinch
  /// - `single_folder`:  All → Documents\Flinch\<subfolder?>\
  ///
  /// A custom folder replaces the built-in one. An unavailable custom folder is
  /// never swapped for another: the capture fails and reports it.
  pub fn resolve_capture_path(&self, capture_type: CaptureType) -> PathBuf {
    match self.save_location_mode {
      SaveLocationMode::SingleFolder => {
        let base = self.folder_root(FolderTarget::Together);
        match self.effective_single_folder_subfolder_template() {
          Some(template) => {
            let subfolder = crate::capture_subfolder::render_template(
              template,
              &crate::capture_subfolder::SubfolderContext { now: Local::now() },
            );
            if subfolder.is_empty() {
              base
            } else {
              base.join(subfolder)
            }
          },
          None => base,
        }
      },
      SaveLocationMode::SystemFolders => match capture_type {
        CaptureType::Video => self.folder_root(FolderTarget::Videos),
        CaptureType::Screenshot | CaptureType::Gif => self.folder_root(FolderTarget::Screenshots),
      },
    }
  }

  /// The chosen or built-in folder for a target, before any subfolder.
  pub fn folder_root(&self, target: FolderTarget) -> PathBuf {
    let (custom, built_in) = match target {
      FolderTarget::Together => (&self.custom_single_folder, dirs::document_dir()),
      FolderTarget::Screenshots => (&self.custom_screenshots_folder, dirs::picture_dir()),
      FolderTarget::Videos => (&self.custom_videos_folder, dirs::video_dir()),
    };
    custom.as_ref().map_or_else(
      || {
        built_in
          .unwrap_or_else(|| PathBuf::from("."))
          .join("Flinch")
      },
      PathBuf::from,
    )
  }

  pub fn custom_folder_mut(&mut self, target: FolderTarget) -> &mut Option<String> {
    match target {
      FolderTarget::Together => &mut self.custom_single_folder,
      FolderTarget::Screenshots => &mut self.custom_screenshots_folder,
      FolderTarget::Videos => &mut self.custom_videos_folder,
    }
  }

  /// Returns the effective filename template: the user's configured value,
  /// or the built-in default if none is set.
  pub fn effective_filename_template(&self) -> &str {
    self
      .filename_template
      .as_deref()
      .filter(|t| !t.is_empty())
      .unwrap_or(crate::filename::DEFAULT_TEMPLATE)
  }

  /// Returns the effective single-folder subfolder template:
  /// None when the user explicitly disabled subfolders,
  /// otherwise the configured value or the built-in default.
  pub fn effective_single_folder_subfolder_template(&self) -> Option<&str> {
    match self.single_folder_subfolder_template.as_deref() {
      Some(template) if template.trim().is_empty() => None,
      Some(template) => Some(template),
      None => Some(crate::capture_subfolder::DEFAULT_TEMPLATE),
    }
  }
}

/// Custom folders are absolute and not UNC, the same paths Flinch can open. Whether
/// a folder exists is left to capture time: a removable drive may come back.
pub fn validate_custom_folder(path: &str) -> Result<(), String> {
  crate::commands::ensure_absolute_non_unc_path(path, "custom save folder")
}

/// A letter with no drive behind it stands in for an unplugged drive.
#[cfg(test)]
pub(crate) fn unused_drive_letter() -> char {
  ('D'..='Z')
    .rev()
    .find(|letter| !std::path::Path::new(&format!("{letter}:\\")).exists())
    .expect("a free drive letter")
}

/// Preserve invalid user data before replacing it. All I/O is finite and local;
/// no state mutex is held here. The same-directory temporary file keeps a failed
/// write from truncating the previous configuration.
pub(crate) fn write_config(path: &std::path::Path, config: &Config) -> Result<Option<PathBuf>> {
  let json = serde_json::to_vec_pretty(config)
    .map_err(|error| ConfigError::ParseError(error.to_string()))?;
  let parent = path
    .parent()
    .context("Configuration path has no parent directory")?;
  let backup = match fs::read(path) {
    Ok(bytes) if serde_json::from_slice::<Config>(&bytes).is_err() => {
      let mut file = tempfile::Builder::new()
        .prefix("config-recovery-")
        .suffix(".json")
        .tempfile_in(parent)
        .context("Failed to create configuration recovery backup")?;
      file
        .write_all(&bytes)
        .context("Failed to write configuration recovery backup")?;
      file
        .as_file()
        .sync_all()
        .context("Failed to flush configuration recovery backup")?;
      let (_, backup_path) = file
        .keep()
        .context("Failed to retain configuration recovery backup")?;
      Some(backup_path)
    },
    Ok(_) => None,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
    Err(error) => return Err(error).context("Cannot safely replace unreadable configuration"),
  };
  let mut file =
    tempfile::NamedTempFile::new_in(parent).context("Failed to create configuration save file")?;
  file
    .write_all(&json)
    .context("Failed to write configuration save file")?;
  file
    .as_file()
    .sync_all()
    .context("Failed to flush configuration save file")?;
  file
    .persist(path)
    .map_err(|error| ConfigError::WriteError(error.to_string()))
    .context("Failed to replace saved configuration")?;
  Ok(backup)
}

#[derive(Debug, Clone, Copy)]
enum RecoverySource {
  Fields,
  Backup,
}

/// Replace unparsable settings with the fields that still parse, or else the
/// newest usable backup. A failed write returns `None`, so the caller's defaults
/// path reports the failure accurately.
fn restore_saved_settings(
  path: &std::path::Path,
  content: &str,
  defaults: &Config,
) -> Option<Config> {
  let (mut restored, source) = match salvage::keep_valid_fields(content, defaults) {
    Some(kept) => (kept, RecoverySource::Fields),
    None => {
      let (restored, backup) = backup::newest_valid(path)?;
      tracing::warn!(
        operation = "restore_config_backup",
        backup = %backup.display(),
        "Restoring settings from a backup"
      );
      (restored, RecoverySource::Backup)
    },
  };
  restored.validate();
  match write_config(path, &restored) {
    Ok(recovery) => {
      queue_load_warning(&recovery_message(
        source,
        &restored.language,
        recovery.as_deref().unwrap_or(path),
      ));
      Some(restored)
    },
    Err(error) => {
      tracing::warn!(
        operation = "restore_config_backup",
        path = %path.display(),
        error = %error,
        "Could not write restored settings"
      );
      None
    },
  }
}

fn recovery_message(source: RecoverySource, language: &str, recovery: &std::path::Path) -> String {
  let labels = crate::i18n::config_recovery(language);
  let label = match source {
    RecoverySource::Fields => labels.kept_fields,
    RecoverySource::Backup => labels.restored_backup,
  };
  format!("{label}\n{}", recovery.display())
}

fn recover_config(path: &std::path::Path, config: &Config) {
  let labels = crate::i18n::config_recovery(&config.language);
  let message = match write_config(path, config) {
    Ok(backup) => {
      let backup_path = backup.as_deref().unwrap_or(path);
      format!("{}\n{}", labels.restored, backup_path.display())
    },
    Err(error) => {
      tracing::warn!(operation = "restore_default_config", path = %path.display(), error = %error,
        "Config recovery failed; original configuration was not replaced");
      format!("{}\n{}\n{error:#}", labels.failed, path.display())
    },
  };
  queue_load_warning(&message);
}

fn queue_load_warning(message: &str) {
  match PENDING_LOAD_WARNING.lock() {
    Ok(mut pending) => {
      *pending = Some(message.to_string());
    },
    Err(error) => {
      tracing::warn!(
        operation = "config_warning",
        error = %error,
        "Failed to queue config recovery warning"
      );
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn real_loader_handles_bundled_defaults_user_overrides_damage_and_unreadable_paths() {
    let folder = tempfile::tempdir().unwrap();
    let bundled = folder.path().join("default.json");
    let user = folder.path().join("config.json");
    let config = Config {
      language: "fr".into(),
      screenshot_quality: 75,
      screenshot_area_shortcut: "Ctrl+Alt+F12".into(),
      ..Default::default()
    };
    fs::write(&bundled, serde_json::to_vec(&config).unwrap()).unwrap();
    let loaded = Config::load_from_paths(Some(bundled.clone()), Ok(user.clone()));
    assert_eq!(loaded.screenshot_quality, 75);
    assert_eq!(
      loaded.screenshot_area_shortcut,
      defaults::default_screenshot_area_shortcut()
    );
    assert!(
      !user.exists(),
      "reading missing preferences must not write them"
    );
    let no_path = Config::load_from_paths(
      Some(bundled.clone()),
      Err(anyhow::anyhow!("unavailable configuration directory")),
    );
    assert_eq!(no_path.screenshot_quality, 75);
    let user_config = Config {
      language: "de".into(),
      screenshot_quality: 90,
      video_fps: 999,
      ..Default::default()
    };
    fs::write(&user, serde_json::to_vec(&user_config).unwrap()).unwrap();
    let loaded = Config::load_from_paths(Some(bundled.clone()), Ok(user.clone()));
    assert_eq!(loaded.language, "de");
    assert_eq!(loaded.screenshot_quality, 90);
    assert!(loaded.video_fps <= 60);
    fs::write(&user, b"{broken preferences").unwrap();
    let loaded = Config::load_from_paths(Some(bundled.clone()), Ok(user.clone()));
    assert_eq!(loaded.screenshot_quality, 75);
    assert!(serde_json::from_slice::<Config>(&fs::read(&user).unwrap()).is_ok());
    let backups = fs::read_dir(folder.path())
      .unwrap()
      .filter_map(|entry| entry.ok())
      .filter(|entry| {
        entry
          .file_name()
          .to_string_lossy()
          .starts_with("config-recovery-")
      })
      .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(backups[0].path()).unwrap(), b"{broken preferences");
    assert!(Config::take_pending_load_warning().is_some());
    let directory = folder.path().join("unreadable-as-file");
    fs::create_dir(&directory).unwrap();
    let loaded = Config::load_from_paths(None, Ok(directory.clone()));
    assert_eq!(
      loaded.screenshot_quality,
      Config::default().screenshot_quality
    );
    assert!(directory.is_dir());
    assert!(Config::take_pending_load_warning().is_some());
    fs::remove_file(&user).unwrap();
    fs::write(&bundled, b"{bad bundled defaults").unwrap();
    assert_eq!(
      Config::load_from_paths(Some(bundled), Ok(user.clone())).screenshot_quality,
      Config::default().screenshot_quality
    );
    assert_eq!(
      Config::load_from_paths(Some(directory), Ok(user)).screenshot_quality,
      Config::default().screenshot_quality
    );
  }

  #[test]
  fn upgrading_from_1_1_0_keeps_every_preference() {
    // Every 1.1.0 field, set away from its default. Never edit this fixture to pass.
    let fixture = include_str!("fixtures/config-1.1.0.json");
    let folder = tempfile::tempdir().unwrap();
    let user = folder.path().join("config.json");
    fs::write(&user, fixture).unwrap();

    let loaded = Config::load_from_paths(None, Ok(user.clone()));

    assert_eq!(fs::read_to_string(&user).unwrap(), fixture);
    let expected: serde_json::Value = serde_json::from_str(fixture).unwrap();
    let expected = expected.as_object().unwrap();
    let actual = serde_json::to_value(&loaded).unwrap();
    let defaults = serde_json::to_value(Config::default()).unwrap();
    assert_eq!(expected.len(), 31);
    // Defaults that later moved onto this fixture's value, each with its 1.1.0 default. Keeping
    // the fixture's value proves nothing for them, so a file holding the old default must keep
    // that instead: 1.1.0 wrote every field, so an untouched High is a saved choice now.
    let moved = [("video_quality", "high")];
    for (field, value) in expected {
      assert_eq!(actual.get(field), Some(value), "{field} changed on upgrade");
      if moved.iter().any(|(name, _)| name == field) {
        assert_eq!(
          defaults.get(field),
          Some(value),
          "{field} is no longer moved"
        );
      } else {
        assert_ne!(defaults.get(field), Some(value), "{field} proves nothing");
      }
    }
    for (field, old) in moved {
      let old = serde_json::Value::from(old);
      assert_ne!(defaults.get(field), Some(&old), "{field} proves nothing");
      let mut file = expected.clone();
      file.insert(field.to_owned(), old.clone());
      fs::write(&user, serde_json::to_string(&file).unwrap()).unwrap();
      let loaded = serde_json::to_value(Config::load_from_paths(None, Ok(user.clone()))).unwrap();
      assert_eq!(
        loaded.get(field),
        Some(&old),
        "{field}'s old default changed on upgrade"
      );
    }
  }

  #[test]
  fn the_magnifier_is_on_unless_a_saved_file_turned_it_off() {
    assert!(Config::default().selection_magnifier);
    let missing: Config = serde_json::from_str("{}").unwrap();
    assert!(missing.selection_magnifier);
    let off: Config = serde_json::from_str(r#"{"selection_magnifier": false}"#).unwrap();
    assert!(!off.selection_magnifier);
  }

  #[test]
  fn damaged_settings_come_back_from_the_newest_backup() {
    let folder = tempfile::tempdir().unwrap();
    let user = folder.path().join("config.json");
    let bundled = folder.path().join("default.json");
    fs::write(&bundled, serde_json::to_vec(&Config::default()).unwrap()).unwrap();
    let saved = Config {
      language: "fr".into(),
      screenshot_quality: 50,
      ..Default::default()
    };
    fs::write(&user, serde_json::to_vec(&saved).unwrap()).unwrap();
    backup::back_up_for_version(&user, "1.2.0")
      .unwrap()
      .unwrap();
    fs::write(&user, b"{broken preferences").unwrap();

    let loaded = Config::load_from_paths(Some(bundled), Ok(user.clone()));

    assert_eq!(loaded.language, "fr");
    assert_eq!(loaded.screenshot_quality, 50);
    let on_disk: Config = serde_json::from_slice(&fs::read(&user).unwrap()).unwrap();
    assert_eq!(on_disk.screenshot_quality, 50);
    let preserved = fs::read_dir(folder.path())
      .unwrap()
      .filter_map(|entry| entry.ok())
      .find(|entry| {
        entry
          .file_name()
          .to_string_lossy()
          .starts_with("config-recovery-")
      })
      .unwrap();
    assert_eq!(fs::read(preserved.path()).unwrap(), b"{broken preferences");
  }

  #[test]
  fn a_failed_restore_uses_defaults_and_leaves_the_damaged_file() {
    let folder = tempfile::tempdir().unwrap();
    let user = folder.path().join("config.json");
    let saved = Config {
      screenshot_quality: 50,
      ..Default::default()
    };
    fs::write(&user, serde_json::to_vec(&saved).unwrap()).unwrap();
    backup::back_up_for_version(&user, "1.2.0")
      .unwrap()
      .unwrap();
    fs::write(&user, b"{broken preferences").unwrap();
    let mut permissions = fs::metadata(&user).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&user, permissions.clone()).unwrap();

    let loaded = Config::load_from_paths(None, Ok(user.clone()));

    assert_eq!(
      loaded.screenshot_quality,
      Config::default().screenshot_quality
    );
    assert_eq!(fs::read(&user).unwrap(), b"{broken preferences");
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(&user, permissions).unwrap();
  }

  #[test]
  fn recovery_messages_are_localized_and_name_the_preserved_file() {
    let path = std::path::Path::new("config-recovery-1.json");
    for source in [RecoverySource::Fields, RecoverySource::Backup] {
      let english = recovery_message(source, "en", path);
      assert!(english.ends_with("config-recovery-1.json"));
      for language in ["fr", "de", "es", "it"] {
        let message = recovery_message(source, language, path);
        assert_ne!(message, english);
        assert!(message.ends_with("config-recovery-1.json"));
      }
    }
    assert_ne!(
      recovery_message(RecoverySource::Fields, "en", path),
      recovery_message(RecoverySource::Backup, "en", path)
    );
  }

  #[test]
  fn a_future_type_change_resets_one_field_even_when_backups_share_it() {
    let folder = tempfile::tempdir().unwrap();
    let user = folder.path().join("config.json");
    let mut saved: serde_json::Value =
      serde_json::from_str(include_str!("fixtures/config-1.1.0.json")).unwrap();
    saved["video_fps"] = serde_json::json!("sixty");
    let bytes = serde_json::to_vec(&saved).unwrap();
    let backups = folder.path().join("backups");
    fs::create_dir(&backups).unwrap();
    fs::write(backups.join("config-1.1.0.json"), &bytes).unwrap();
    fs::write(&user, &bytes).unwrap();

    let loaded = Config::load_from_paths(None, Ok(user.clone()));

    assert_eq!(loaded.video_fps, Config::default().video_fps);
    let actual = serde_json::to_value(&loaded).unwrap();
    for (field, value) in saved.as_object().unwrap() {
      if field != "video_fps" {
        assert_eq!(actual.get(field), Some(value), "{field} was lost");
      }
    }
    let on_disk: Config = serde_json::from_slice(&fs::read(&user).unwrap()).unwrap();
    assert_eq!(on_disk.language, "fr");
    let preserved = fs::read_dir(folder.path())
      .unwrap()
      .filter_map(|entry| entry.ok())
      .find(|entry| {
        entry
          .file_name()
          .to_string_lossy()
          .starts_with("config-recovery-")
      })
      .unwrap();
    assert_eq!(fs::read(preserved.path()).unwrap(), bytes);
  }

  #[test]
  fn a_file_with_no_usable_fields_still_comes_back_from_a_backup() {
    let folder = tempfile::tempdir().unwrap();
    let user = folder.path().join("config.json");
    let saved = Config {
      screenshot_quality: 50,
      ..Default::default()
    };
    fs::write(&user, serde_json::to_vec(&saved).unwrap()).unwrap();
    backup::back_up_for_version(&user, "1.2.0")
      .unwrap()
      .unwrap();
    fs::write(&user, br#"{"video_fps":"sixty"}"#).unwrap();

    let loaded = Config::load_from_paths(None, Ok(user));

    assert_eq!(loaded.screenshot_quality, 50);
  }

  #[test]
  fn config_save_preserves_corrupt_bytes_before_replacing_them() {
    let dir = tempfile::tempdir().expect("temporary config directory");
    let path = dir.path().join("config.json");
    let original = b"{broken config\xff";
    fs::write(&path, original).expect("write damaged config");

    let backup = write_config(&path, &Config::default())
      .expect("save with recovery")
      .expect("recovery backup");
    assert_eq!(fs::read(&backup).expect("read backup"), original);
    let saved = fs::read(&path).expect("read saved defaults");
    assert!(serde_json::from_slice::<Config>(&saved).is_ok());
    assert!(
      write_config(&path, &Config::default())
        .expect("second save")
        .is_none()
    );
    assert_eq!(
      fs::read_dir(dir.path()).expect("list config files").count(),
      2
    );
  }

  #[test]
  fn config_save_replaces_valid_settings_without_leaving_temporary_files() {
    let dir = tempfile::tempdir().expect("temporary config directory");
    let path = dir.path().join("config.json");
    assert!(
      write_config(&path, &Config::default())
        .expect("initial save")
        .is_none()
    );
    let changed = Config {
      language: "fr".to_string(),
      ..Config::default()
    };
    assert!(
      write_config(&path, &changed)
        .expect("replace save")
        .is_none()
    );
    let saved: Config = serde_json::from_slice(&fs::read(&path).expect("read saved config"))
      .expect("parse saved config");
    assert_eq!(saved.language, "fr");
    assert_eq!(
      fs::read_dir(dir.path()).expect("list config files").count(),
      1
    );
  }

  #[test]
  fn config_save_does_not_replace_an_unreadable_target() {
    let dir = tempfile::tempdir().expect("temporary config directory");
    let path = dir.path().join("config.json");
    fs::create_dir(&path).expect("create unreadable config target");
    let marker = path.join("preserve.txt");
    fs::write(&marker, b"preserve").expect("write marker");
    assert!(write_config(&path, &Config::default()).is_err());
    assert_eq!(
      fs::read(&marker).expect("original marker survives"),
      b"preserve"
    );
    assert_eq!(
      fs::read_dir(dir.path()).expect("list config files").count(),
      1
    );
  }

  #[test]
  fn capture_type_folder_name_matches_expected_output_folders() {
    assert_eq!(CaptureType::Screenshot.folder_name(), "screenshots");
    assert_eq!(CaptureType::Video.folder_name(), "videos");
    assert_eq!(CaptureType::Gif.folder_name(), "gifs");
  }

  #[test]
  fn effective_filename_template_falls_back_for_missing_or_empty_values() {
    let default_config = Config::default();
    let empty_template = Config {
      filename_template: Some(String::new()),
      ..Config::default()
    };
    let custom_template = Config {
      filename_template: Some("%TYPE%_%DATE%".to_string()),
      ..Config::default()
    };

    assert_eq!(
      default_config.effective_filename_template(),
      crate::filename::DEFAULT_TEMPLATE
    );
    assert_eq!(
      empty_template.effective_filename_template(),
      crate::filename::DEFAULT_TEMPLATE
    );
    assert_eq!(
      custom_template.effective_filename_template(),
      "%TYPE%_%DATE%"
    );
  }

  #[test]
  fn effective_single_folder_subfolder_template_supports_default_and_none() {
    let default_config = Config::default();
    let no_subfolder = Config {
      single_folder_subfolder_template: Some(String::new()),
      ..Config::default()
    };
    let custom_template = Config {
      single_folder_subfolder_template: Some("%YEAR%-%MONTH%-%DAY%".to_string()),
      ..Config::default()
    };

    assert_eq!(
      default_config.effective_single_folder_subfolder_template(),
      Some(crate::capture_subfolder::DEFAULT_TEMPLATE)
    );
    assert_eq!(
      no_subfolder.effective_single_folder_subfolder_template(),
      None
    );
    assert_eq!(
      custom_template.effective_single_folder_subfolder_template(),
      Some("%YEAR%-%MONTH%-%DAY%")
    );
  }

  #[test]
  fn custom_folders_replace_only_their_own_built_in_folder() {
    let together = Config {
      save_location_mode: SaveLocationMode::SingleFolder,
      single_folder_subfolder_template: Some("sub".into()),
      custom_single_folder: Some(r"D:\Captures".into()),
      custom_videos_folder: Some(r"E:\Unused".into()),
      ..Config::default()
    };
    for capture in [
      CaptureType::Screenshot,
      CaptureType::Gif,
      CaptureType::Video,
    ] {
      assert_eq!(
        together.resolve_capture_path(capture),
        PathBuf::from(r"D:\Captures\sub")
      );
    }
    let separate = Config {
      save_location_mode: SaveLocationMode::SystemFolders,
      custom_single_folder: Some(r"D:\Unused".into()),
      custom_videos_folder: Some(r"E:\Clips".into()),
      ..Config::default()
    };
    assert_eq!(
      separate.resolve_capture_path(CaptureType::Video),
      PathBuf::from(r"E:\Clips")
    );
    assert_eq!(
      separate.resolve_capture_path(CaptureType::Gif),
      Config::default().resolve_capture_path(CaptureType::Gif)
    );
  }

  #[test]
  fn loading_keeps_an_offline_custom_folder_but_drops_an_invalid_one() {
    let mut config = Config {
      custom_single_folder: Some(r"Z:\Unplugged\Captures".into()),
      custom_screenshots_folder: Some("relative".into()),
      custom_videos_folder: Some(r"\\server\share".into()),
      ..Config::default()
    };
    config.validate();
    assert_eq!(
      config.custom_single_folder.as_deref(),
      Some(r"Z:\Unplugged\Captures")
    );
    assert!(config.custom_screenshots_folder.is_none());
    assert!(config.custom_videos_folder.is_none());
    let saved = serde_json::to_value(Config::default()).expect("config JSON");
    assert!(saved.get("custom_single_folder").is_none());
  }

  #[test]
  fn resolve_capture_path_omits_date_subfolder_when_template_is_empty() {
    let config = Config {
      save_location_mode: SaveLocationMode::SingleFolder,
      single_folder_subfolder_template: Some(String::new()),
      ..Config::default()
    };

    let screenshot_path = config.resolve_capture_path(CaptureType::Screenshot);

    assert!(screenshot_path.ends_with(std::path::Path::new("Flinch")));
  }
}
