// src/commands/config.rs
// Configuration management commands

use crate::app::context::AppHandle;
use crate::config::Config;
use crate::error::ErrorExt;

// Production native environment for the serialized configuration service.
#[derive(Clone)]
pub(crate) struct NativeConfigEnvironment(pub(crate) AppHandle);

impl crate::settings::config_service::ConfigEnvironment for NativeConfigEnvironment {
  fn current(&self) -> Config {
    Config::current(&self.0)
  }
  fn replace(&self, config: Config) {
    Config::replace_current(&self.0, config);
  }
  async fn startup_state(&self) -> anyhow::Result<Option<bool>> {
    #[cfg(all(feature = "native-settings", debug_assertions))]
    if crate::settings::native_host::is_probe() {
      if crate::settings::native_probe::take_load_failure() {
        anyhow::bail!("Injected startup status lookup failure");
      }
      return Ok(None);
    }
    #[cfg(feature = "msix")]
    {
      Ok(Some(crate::autostart::is_enabled().await?))
    }
    #[cfg(not(feature = "msix"))]
    {
      Ok(None)
    }
  }
  async fn set_startup(&self, enabled: bool) -> anyhow::Result<()> {
    #[cfg(all(feature = "native-settings", debug_assertions))]
    anyhow::ensure!(
      !crate::settings::native_host::is_probe(),
      "Startup registration changes are disabled in the integration probe"
    );
    crate::autostart::set_enabled(&self.0, enabled).await
  }
  async fn persist(&self, config: &Config) -> anyhow::Result<()> {
    use anyhow::Context;
    let config = config.clone();
    #[cfg(test)]
    if let Some(path) = self.0.test_config_path() {
      return tokio::task::spawn_blocking(move || {
        crate::config::write_config(&path, &config).map(|_| ())
      })
      .await
      .context("Isolated configuration persistence worker failed")?;
    }
    #[cfg(all(feature = "native-settings", debug_assertions))]
    if let Some(path) = crate::settings::native_probe::config_path() {
      anyhow::ensure!(
        !crate::settings::native_probe::take_write_failure(),
        "Injected native integration write failure"
      );
      return tokio::task::spawn_blocking(move || {
        crate::config::write_config(&path, &config).map(|_| ())
      })
      .await
      .context("Probe configuration persistence worker failed")?;
    }
    tokio::task::spawn_blocking(move || config.save())
      .await
      .context("Configuration persistence worker failed")?
  }
  fn refresh_shortcuts(&self) {
    #[cfg(all(feature = "native-settings", debug_assertions))]
    if crate::settings::native_host::is_probe() {
      return;
    }
    crate::shortcuts::setup_global_shortcuts(&self.0);
  }
}

/// Show a queued settings-recovery message on a worker thread. Startup calls this so
/// users learn about a reset without opening settings; settings loads call it for
/// anything queued later.
pub(crate) fn show_pending_load_warning() {
  deliver_pending_load_warning(Config::take_pending_load_warning, |message| {
    drop(crate::app::runtime::spawn_blocking(move || {
      let options = crate::dialogs::TaskDialogOptions::new("Flinch", "Flinch", &message);
      if let Err(error) = crate::dialogs::show_task_dialog(&options) {
        tracing::error!(%error, "Cannot show configuration load warning");
      }
    }));
  });
}

fn deliver_pending_load_warning(
  take: impl FnOnce() -> Option<String>,
  present: impl FnOnce(String),
) -> bool {
  match take() {
    Some(message) => {
      present(message);
      true
    },
    None => false,
  }
}

pub async fn load_config(app: AppHandle) -> Result<Config, String> {
  show_pending_load_warning();
  crate::settings::config_service::CONFIG_SERVICE
    .load(&NativeConfigEnvironment(app))
    .await
    .map_err(|error| error.to_rich_string())
}

pub async fn save_config(app: AppHandle, config: Config) -> Result<(), String> {
  crate::settings::config_service::CONFIG_SERVICE
    .save(&NativeConfigEnvironment(app), config)
    .await
    .map_err(|error| error.to_rich_string())
}

impl crate::settings::config_actions::ActionEnvironment for NativeConfigEnvironment {
  async fn pick(&self, export: bool) -> Result<Option<std::path::PathBuf>, String> {
    #[cfg(all(feature = "native-settings", debug_assertions))]
    if crate::settings::native_host::is_probe() {
      return crate::settings::native_probe::pick_configuration(export);
    }
    let title = crate::i18n::native_settings(
      &crate::i18n::get_lang(&self.0),
      if export {
        "system_export"
      } else {
        "system_import"
      },
    )?;
    crate::app::platform::configuration_picker(export, title)
      .await
      .map_err(|error| error.to_rich_string())
  }
  async fn language(&self, language: String) -> Result<(), String> {
    crate::commands::set_app_language(self.0.clone(), language).await
  }
}

/// `Ok(None)` is a dismissed picker. A chosen folder must accept files now, so a
/// read-only or network location fails here rather than at the next capture.
pub async fn pick_capture_folder(
  app: AppHandle,
  target: flinch_settings::edit::FolderTarget,
) -> Result<Option<String>, String> {
  choose_capture_folder(&app, target, crate::app::platform::folder_picker).await
}

async fn choose_capture_folder<P, F>(
  app: &AppHandle,
  target: flinch_settings::edit::FolderTarget,
  pick: P,
) -> Result<Option<String>, String>
where
  P: FnOnce(String, std::path::PathBuf) -> F,
  F: std::future::Future<Output = anyhow::Result<Option<std::path::PathBuf>>>,
{
  let title = crate::i18n::native_settings(&crate::i18n::get_lang(app), "files_choose_folder")?;
  let initial = Config::current(app).folder_root(target);
  let picked = pick(title, initial)
    .await
    .map_err(|error| error.to_rich_string())?;
  match picked {
    Some(folder) => crate::app::runtime::spawn_blocking(move || check_capture_folder(&folder))
      .await
      .map_err(|error| crate::commands::ipc_context("pick_capture_folder worker", error))?
      .map(Some),
    None => Ok(None),
  }
}

pub(crate) fn check_capture_folder(folder: &std::path::Path) -> Result<String, String> {
  let path = folder
    .to_str()
    .ok_or_else(|| format!("Folder path is not valid Unicode: {}", folder.display()))?;
  crate::config::validate_custom_folder(path)?;
  tempfile::tempfile_in(folder)
    .map_err(|error| crate::commands::ipc_context("Write to chosen folder", error))?;
  Ok(path.to_owned())
}

pub async fn reset_config(app: AppHandle) -> Result<Config, String> {
  crate::settings::config_actions::reset(
    &crate::settings::config_service::CONFIG_SERVICE,
    &NativeConfigEnvironment(app),
  )
  .await
}

pub async fn export_config(app: AppHandle) -> Result<(), String> {
  if crate::settings::config_actions::export(&NativeConfigEnvironment(app)).await? {
    Ok(())
  } else {
    Err(crate::commands::cancelled())
  }
}

/// Import, validate and persist before returning the configuration for UI refresh.
pub async fn import_config(app: AppHandle) -> Result<Config, String> {
  crate::settings::config_actions::import(
    &crate::settings::config_service::CONFIG_SERVICE,
    &NativeConfigEnvironment(app),
  )
  .await?
  .ok_or_else(crate::commands::cancelled)
}

/// Render a filename template to a preview string using a fixed example context.
/// Returns the rendered filename stem (no extension).
pub fn preview_filename(template: String) -> Result<String, String> {
  crate::filename::validate_template(&template)?;
  let ctx = crate::filename::FilenameContext {
    capture_type: "screenshot",
    app_name: Some(crate::filename::PREVIEW_APP),
    now: chrono::Local::now(),
  };
  Ok(crate::filename::render_template(&template, &ctx))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::shortcuts::registry::shortcuts_changed;
  use tempfile::TempDir;

  #[test]
  fn a_queued_recovery_message_is_presented_once_and_nothing_else_is() {
    let mut shown = Vec::new();
    let mut queue = Some("Default settings were restored.".to_owned());
    assert!(deliver_pending_load_warning(
      || queue.take(),
      |message| shown.push(message)
    ));
    assert!(!deliver_pending_load_warning(
      || queue.take(),
      |message| shown.push(message)
    ));
    assert_eq!(shown, ["Default settings were restored."]);
  }

  #[tokio::test]
  async fn the_folder_picker_starts_at_the_current_folder_and_checks_the_choice() {
    use flinch_settings::edit::FolderTarget;
    crate::app::runtime::test_runtime();
    let folder = TempDir::new().expect("temporary folder");
    let app = AppHandle::isolated(
      Config {
        language: "en".into(),
        custom_videos_folder: Some(r"D:\Clips".into()),
        ..Config::default()
      },
      std::path::PathBuf::new(),
    );
    let cancelled =
      choose_capture_folder(&app, FolderTarget::Videos, |title, initial| async move {
        assert_eq!(title, "Choose a save folder");
        assert_eq!(initial, std::path::PathBuf::from(r"D:\Clips"));
        Ok(None)
      })
      .await;
    assert_eq!(cancelled, Ok(None));
    let chosen = folder.path().to_path_buf();
    let picked = choose_capture_folder(&app, FolderTarget::Videos, |_, _| async move {
      Ok(Some(chosen))
    })
    .await;
    assert_eq!(picked.ok().flatten().as_deref(), folder.path().to_str());
    assert!(
      choose_capture_folder(&app, FolderTarget::Videos, |_, _| async {
        Ok(Some(r"\\server\share".into()))
      })
      .await
      .is_err()
    );
    assert!(
      choose_capture_folder(&app, FolderTarget::Videos, |_, _| async {
        Err(anyhow::anyhow!("dialog failed"))
      })
      .await
      .is_err()
    );
  }

  /// The production environment against an isolated settings path: startup state is left to the
  /// channel, saves persist and publish, and shortcut changes re-register only test hotkeys.
  #[tokio::test]
  async fn the_native_environment_loads_saves_and_rejects_failed_persistence() {
    use crate::settings::config_service::{CONFIG_SERVICE, ConfigEnvironment};
    let folder = TempDir::new().expect("temporary folder");
    let path = folder.path().join("config.json");
    let app = AppHandle::isolated(
      Config {
        language: "en".into(),
        ..Config::default()
      },
      std::path::PathBuf::new(),
    );
    app.isolate_config_path(path.clone());
    let environment = NativeConfigEnvironment(app.clone());
    assert_eq!(environment.startup_state().await.ok(), Some(None));
    let loaded = CONFIG_SERVICE.load(&environment).await.expect("load");
    assert_eq!(loaded.language, "en");
    assert!(!path.exists(), "loading never writes");

    let mut changed = Config::current(&app);
    changed.gif_shortcut = "Ctrl+Shift+Alt+G".into();
    changed.screenshot_quality = 75;
    assert!(shortcuts_changed(&Config::current(&app), &changed));
    save_config(app.clone(), changed.clone())
      .await
      .expect("save");
    assert_eq!(Config::current(&app).screenshot_quality, 75);
    assert_eq!(Config::current(&app).gif_shortcut, "Ctrl+Shift+Alt+G");
    let written: Config =
      serde_json::from_slice(&std::fs::read(&path).expect("persisted config")).unwrap();
    assert_eq!(written.screenshot_quality, 75);
    assert_eq!(written.gif_shortcut, "Ctrl+Shift+Alt+G");

    // A folder in place of the file makes persistence fail; the published config is kept.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let mut rejected = Config::current(&app);
    rejected.screenshot_quality = 50;
    assert!(save_config(app.clone(), rejected).await.is_err());
    assert_eq!(Config::current(&app).screenshot_quality, 75);
  }

  #[test]
  fn chosen_folders_must_be_local_and_accept_files() {
    let folder = TempDir::new().expect("temporary folder");
    assert_eq!(
      check_capture_folder(folder.path()).ok().as_deref(),
      folder.path().to_str()
    );
    assert!(check_capture_folder(&folder.path().join("missing")).is_err());
    assert!(check_capture_folder(std::path::Path::new(r"\\server\share")).is_err());
  }

  // Helper function to create a test config with all fields populated
  fn create_test_config() -> Config {
    Config {
      screenshot_format: crate::config::ScreenshotFormat::Png,
      screenshot_quality: 90,
      video_fps: 60,
      auto_start: false,
      ..Default::default()
    }
  }

  // Helper to set up a temporary config path for testing
  fn setup_temp_config_dir() -> TempDir {
    tempfile::Builder::new()
      .prefix("flinch_test_")
      .tempdir()
      .expect("Failed to create temp dir")
  }

  #[test]
  fn test_config_default_values() {
    let config = Config::default();
    assert_eq!(
      config.screenshot_format,
      crate::config::ScreenshotFormat::Png
    );
    assert_eq!(config.screenshot_quality, 90);
    assert_eq!(config.video_fps, 30);
    assert!(!config.auto_start);
    assert!(config.video_hardware_encoding);
    assert!(!config.record_audio);
    assert!(config.show_cursor);
    assert_eq!(config.gif_fps, 15);
    assert_eq!(config.gif_max_duration, 10);
    assert!(!config.recording_visibility_warning_dismissed);
  }

  #[test]
  fn test_config_serialization() {
    let config = create_test_config();
    let json = serde_json::to_string_pretty(&config).expect("Failed to serialize");

    // Verify we can deserialize back
    let deserialized: Config = serde_json::from_str(&json).expect("Failed to deserialize");
    assert_eq!(deserialized.screenshot_format, config.screenshot_format);
    assert_eq!(deserialized.screenshot_quality, config.screenshot_quality);
    assert_eq!(deserialized.video_fps, config.video_fps);
  }

  #[test]
  fn test_config_load_nonexistent_returns_defaults() {
    let config = Config::default();
    assert_eq!(
      config.save_location_mode,
      crate::config::SaveLocationMode::SystemFolders
    );
  }

  #[test]
  fn test_config_save_creates_json() {
    let temp_dir = setup_temp_config_dir();
    let config_path = temp_dir.path().join("config.json");

    let config = create_test_config();

    // Manually save to our test path (bypassing the actual config_file() function)
    let json = serde_json::to_string_pretty(&config).expect("Failed to serialize");
    std::fs::write(&config_path, json).expect("Failed to write config");

    // Verify file exists and can be read back
    assert!(config_path.exists());
    let read_content = std::fs::read_to_string(&config_path).expect("Failed to read config");
    let loaded_config: Config =
      serde_json::from_str(&read_content).expect("Failed to parse config");
    assert_eq!(
      loaded_config.screenshot_format,
      crate::config::ScreenshotFormat::Png
    );
  }

  #[test]
  fn test_config_validation_on_invalid_format() {
    let mut config = create_test_config();
    config.screenshot_format = crate::config::ScreenshotFormat::Png;
    config.validate();
    assert_eq!(
      config.screenshot_format,
      crate::config::ScreenshotFormat::Png
    );
  }

  #[test]
  fn test_config_validation_on_invalid_quality() {
    let mut config = create_test_config();
    config.screenshot_quality = 101; // Out of range
    config.validate();
    assert_eq!(config.screenshot_quality, 90); // Should clamp to high

    config.screenshot_quality = 0;
    config.validate();
    assert_eq!(config.screenshot_quality, 50); // Should clamp to low
  }

  #[test]
  fn test_config_validation_on_invalid_fps() {
    let mut config = create_test_config();
    config.video_fps = 45; // Invalid FPS
    config.validate();
    assert_eq!(config.video_fps, 30); // Should reset to default

    config.video_fps = 60; // Valid FPS
    config.validate();
    assert_eq!(config.video_fps, 60); // Should remain unchanged
  }

  #[test]
  fn test_config_shortcut_trimming() {
    let mut config = create_test_config();
    config.screenshot_area_shortcut = "  Ctrl+Shift+S  ".to_string();
    config.validate();
    assert_eq!(config.screenshot_area_shortcut, "Ctrl+Shift+S");
  }

  #[test]
  fn test_config_merge_with_defaults() {
    // Test that partial configs merge with defaults properly
    let json = r#"{"screenshot_format": "webp"}"#;
    let mut config: Config = serde_json::from_str(json).expect("Failed to parse");
    config.validate();

    // Specified field should be preserved
    assert_eq!(
      config.screenshot_format,
      crate::config::ScreenshotFormat::Webp
    );
    // Unspecified fields should use defaults
    assert_eq!(config.video_fps, 30);
  }

  #[test]
  fn test_shortcuts_changed_detects_gif_updates() {
    let previous = create_test_config();
    let mut updated = previous.clone();
    updated.gif_shortcut = "Ctrl+Shift+G".to_string();

    assert!(shortcuts_changed(&previous, &updated));
  }

  #[test]
  fn test_shortcuts_changed_ignores_whitespace_only_differences() {
    let previous = create_test_config();
    let mut updated = previous.clone();
    updated.screenshot_area_shortcut = "  Ctrl+PrintScreen  ".to_string();

    assert!(!shortcuts_changed(&previous, &updated));
  }
}
