//! Native configuration transactions behind an injectable environment. One
//! bounded async operation lock protects startup registration, persistence and
//! publication; ordinary state locks are never held across the platform awaits.

use std::{
  future::Future,
  sync::{Arc, LazyLock},
  time::Duration,
};

use anyhow::{Context, Result};

use crate::config::Config;
use flinch_settings::edit::{EditDraft, PreferenceEdit};

pub(crate) enum ConfigChange {
  Replace(Box<Config>),
  Edit(PreferenceEdit),
  Draft(Box<EditDraft>),
}

impl From<Config> for ConfigChange {
  fn from(config: Config) -> Self {
    Self::Replace(Box::new(config))
  }
}

impl From<PreferenceEdit> for ConfigChange {
  fn from(edit: PreferenceEdit) -> Self {
    Self::Edit(edit)
  }
}

impl From<EditDraft> for ConfigChange {
  fn from(draft: EditDraft) -> Self {
    Self::Draft(Box::new(draft))
  }
}

pub(crate) trait ConfigEnvironment: Clone + Send + Sync + 'static {
  fn current(&self) -> Config;
  fn replace(&self, config: Config);
  fn startup_state(&self) -> impl Future<Output = Result<Option<bool>>> + Send;
  fn set_startup(&self, enabled: bool) -> impl Future<Output = Result<()>> + Send;
  fn persist(&self, config: &Config) -> impl Future<Output = Result<()>> + Send;
  fn refresh_shortcuts(&self);
}

pub(crate) struct ConfigService {
  operation: Arc<tokio::sync::Mutex<()>>,
  acquisition_timeout: Duration,
}

pub(crate) static CONFIG_SERVICE: LazyLock<ConfigService> =
  LazyLock::new(|| ConfigService::new(Duration::from_secs(30)));

impl ConfigService {
  pub(crate) fn new(acquisition_timeout: Duration) -> Self {
    Self {
      operation: Arc::new(tokio::sync::Mutex::new(())),
      acquisition_timeout,
    }
  }

  async fn acquire(&self) -> Result<tokio::sync::OwnedMutexGuard<()>> {
    tokio::time::timeout(
      self.acquisition_timeout,
      self.operation.clone().lock_owned(),
    )
    .await
    .context("Configuration operation timed out")
  }

  pub(crate) async fn load(&self, environment: &impl ConfigEnvironment) -> Result<Config> {
    let operation = self.acquire().await?;
    let environment = environment.clone();
    tokio::spawn(async move {
      let _operation = operation;
      let mut config = environment.current();
      // MSIX observes Windows-owned startup state. Other channels return None,
      // preserving the current channel behavior and avoiding an invented state.
      if let Some(enabled) = environment.startup_state().await.map_err(|error| {
        tracing::warn!(%error, "Failed to query Windows startup state");
        error
      })? {
        config.auto_start = enabled;
        environment.replace(config.clone());
      }
      Ok(config)
    })
    .await
    .context("Configuration load worker failed")?
  }

  pub(crate) async fn save(
    &self,
    environment: &impl ConfigEnvironment,
    change: impl Into<ConfigChange>,
  ) -> Result<()> {
    let operation = self.acquire().await?;
    let environment = environment.clone();
    let change = change.into();
    // The worker owns the transaction and guard. Dropping the caller only stops
    // waiting for acknowledgment; it cannot cancel a disk write, skip rollback
    // or allow another save to interleave. The owning runtime must be drained
    // during application shutdown before it is stopped.
    tokio::spawn(persist_transaction(environment, change, operation))
      .await
      .context("Configuration save worker failed")?
  }
}

async fn persist_transaction(
  environment: impl ConfigEnvironment,
  change: ConfigChange,
  operation: tokio::sync::OwnedMutexGuard<()>,
) -> Result<()> {
  let _operation = operation;
  let previous = environment.current();
  let mut config = resolve_change(&previous, change).map_err(|error| {
    tracing::warn!(%error, "Rejected native settings changes before persistence");
    error
  })?;
  config.validate();
  let shortcut_changed = crate::shortcuts::registry::shortcuts_changed(&previous, &config);
  let autostart_changed = previous.auto_start != config.auto_start;
  if autostart_changed {
    environment.set_startup(config.auto_start).await.map_err(|error| {
      tracing::error!(%error, enabled = config.auto_start, "Failed to change startup registration");
      error
    })?;
  }
  if let Err(error) = environment.persist(&config).await {
    tracing::error!(%error, "Configuration persistence failed");
    if autostart_changed
      && let Err(rollback_error) = environment.set_startup(previous.auto_start).await
    {
      tracing::error!(%rollback_error, "Failed to restore startup registration after config save failed");
      return Err(error.context(format!("Startup rollback also failed: {rollback_error:#}")));
    }
    return Err(error);
  }
  environment.replace(config);
  if shortcut_changed {
    tracing::debug!(
      operation = "save_config",
      "Shortcuts changed, re-registering"
    );
    environment.refresh_shortcuts();
  }
  tracing::info!(operation = "save_config", "Config saved successfully");
  Ok(())
}

fn resolve_change(previous: &Config, change: ConfigChange) -> Result<Config> {
  let config = match change {
    ConfigChange::Replace(config) => *config,
    ConfigChange::Edit(edit) => Config::try_from((previous.clone(), edit))?,
    ConfigChange::Draft(draft) => {
      let mut config = previous.clone();
      for edit in draft.edits() {
        config = Config::try_from((config, edit.clone()))?;
      }
      config
    },
  };
  Ok(config)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
  };

  #[derive(Clone)]
  struct Environment {
    config: Arc<Mutex<Config>>,
    events: Arc<Mutex<Vec<String>>>,
    startup_state: Option<bool>,
    deny_startup: bool,
    fail_write: bool,
    fail_rollback: bool,
    startup_changed: Arc<AtomicBool>,
    pause_write: bool,
    write_started: Arc<tokio::sync::Notify>,
    finish_write: Arc<tokio::sync::Notify>,
    disk_path: Option<std::path::PathBuf>,
  }

  #[tokio::test]
  async fn queued_preference_edits_merge_with_the_latest_configuration() {
    let environment = Environment::new();
    let service = ConfigService::new(Duration::from_secs(1));
    let (audio, fps) = tokio::join!(
      service.save(&environment, PreferenceEdit::RecordAudio(true)),
      service.save(&environment, PreferenceEdit::VideoFps(60)),
    );
    audio.expect("audio saved");
    fps.expect("fps saved");
    let config = environment.current();
    assert!(config.record_audio);
    assert_eq!(config.video_fps, 60);
    assert_eq!(
      environment.events(),
      ["persist", "publish", "persist", "publish"]
    );
  }

  #[tokio::test]
  async fn rejected_native_edit_has_no_platform_or_disk_side_effects() {
    let environment = Environment::new();
    let service = ConfigService::new(Duration::from_secs(1));
    assert!(
      service
        .save(&environment, PreferenceEdit::VideoFps(144))
        .await
        .is_err()
    );
    assert!(environment.events().is_empty());
  }

  #[tokio::test]
  async fn native_edit_batch_is_validated_before_any_side_effects() {
    let environment = Environment::new();
    let service = ConfigService::new(Duration::from_secs(1));
    let mut draft = EditDraft::default();
    draft.insert(PreferenceEdit::AutoStart(true));
    draft.insert(PreferenceEdit::VideoFps(144));
    assert!(service.save(&environment, draft).await.is_err());
    assert!(environment.events().is_empty());
    assert!(!environment.current().auto_start);
  }

  #[tokio::test]
  async fn native_edit_batch_persists_once_and_keeps_unrelated_preferences() {
    let environment = Environment::new();
    let service = ConfigService::new(Duration::from_secs(1));
    let previous = environment.current();
    let mut draft = EditDraft::default();
    draft.insert(PreferenceEdit::RecordAudio(true));
    draft.insert(PreferenceEdit::VideoFps(60));
    service
      .save(&environment, draft)
      .await
      .expect("batch saved");
    let saved = environment.current();
    assert!(saved.record_audio);
    assert_eq!(saved.video_fps, 60);
    assert_eq!(
      saved.screenshot_area_shortcut,
      previous.screenshot_area_shortcut
    );
    assert_eq!(environment.events(), ["persist", "publish"]);
  }

  impl Environment {
    fn new() -> Self {
      Self {
        config: Arc::new(Mutex::new(Config {
          language: "en".into(),
          ..Config::default()
        })),
        events: Arc::new(Mutex::new(Vec::new())),
        startup_state: None,
        deny_startup: false,
        fail_write: false,
        fail_rollback: false,
        startup_changed: Arc::new(AtomicBool::new(false)),
        pause_write: false,
        write_started: Arc::new(tokio::sync::Notify::new()),
        finish_write: Arc::new(tokio::sync::Notify::new()),
        disk_path: None,
      }
    }
    fn record(&self, event: impl Into<String>) {
      self.events.lock().expect("event lock").push(event.into());
    }
    fn events(&self) -> Vec<String> {
      self.events.lock().expect("event lock").clone()
    }
  }

  #[tokio::test]
  async fn every_preference_round_trips_through_disk_without_changing_other_fields() {
    use PreferenceEdit::*;
    use flinch_settings::edit::ShortcutAction;
    let mut cases = Vec::new();
    for value in [false, true] {
      for (field, edit) in [
        ("auto_start", AutoStart(value)),
        ("check_for_updates", CheckForUpdates(value)),
        ("notifications_enabled", NotificationsEnabled(value)),
        ("notification_sound", NotificationSound(value)),
        ("video_hardware_encoding", HardwareEncoding(value)),
        ("record_audio", RecordAudio(value)),
        ("show_cursor", ShowCursor(value)),
        ("gif_show_cursor", GifShowCursor(value)),
        ("show_clicks", ShowClicks(value)),
        ("gif_show_clicks", GifShowClicks(value)),
        ("record_microphone", RecordMicrophone(value)),
        ("selection_magnifier", SelectionMagnifier(value)),
        ("copy_text_direct", CopyTextDirect(value)),
        ("screenshot_large_png_as_jpg", LargePngAsJpg(value)),
      ] {
        cases.push((field, edit, serde_json::json!(value)));
      }
    }
    for value in ["en", "fr", "de", "es", "it"] {
      cases.push(("language", Language(value.into()), serde_json::json!(value)));
    }
    for value in ["png", "jpg", "webp"] {
      cases.push((
        "screenshot_format",
        ScreenshotFormat(value.into()),
        serde_json::json!(value),
      ));
    }
    for value in [50, 75, 90] {
      cases.push((
        "screenshot_quality",
        ScreenshotQuality(value),
        serde_json::json!(value),
      ));
    }
    for value in [15, 30] {
      cases.push(("gif_fps", GifFps(value), serde_json::json!(value)));
    }
    for value in [10, 20, 30] {
      cases.push((
        "gif_max_duration",
        GifDuration(value),
        serde_json::json!(value),
      ));
    }
    for value in [0, 3, 5, 10] {
      cases.push((
        "capture_delay",
        CaptureDelay(value),
        serde_json::json!(value),
      ));
    }
    for value in [30, 60] {
      cases.push(("video_fps", VideoFps(value), serde_json::json!(value)));
    }
    for value in ["low", "medium", "high"] {
      cases.push((
        "video_quality",
        VideoQuality(value.into()),
        serde_json::json!(value),
      ));
    }
    for value in [1000, 2500, 3000, 5000, 10000] {
      cases.push((
        "notification_duration_ms",
        NotificationDuration(value),
        serde_json::json!(value),
      ));
    }
    for value in ["top-right", "top-left", "bottom-right", "bottom-left"] {
      cases.push((
        "notification_position",
        NotificationPosition(value.into()),
        serde_json::json!(value),
      ));
    }
    for value in ["system_folders", "single_folder"] {
      cases.push((
        "save_location_mode",
        SaveLocation(value.into()),
        serde_json::json!(value),
      ));
    }
    for value in ["Custom_%TYPE%", "é_%DATETIME%"] {
      cases.push((
        "filename_template",
        Filename(value.into()),
        serde_json::json!(value),
      ));
    }
    for value in ["", "%YEAR%-%MONTH%", "é"] {
      cases.push((
        "single_folder_subfolder_template",
        Subfolder(value.into()),
        serde_json::json!(value),
      ));
    }
    cases.push(("filename_template", ResetFilename, serde_json::Value::Null));
    cases.push((
      "single_folder_subfolder_template",
      ResetSubfolder,
      serde_json::Value::Null,
    ));
    for (action, field) in [
      (ShortcutAction::ScreenshotArea, "screenshot_area_shortcut"),
      (
        ShortcutAction::ScreenshotFullscreen,
        "screenshot_fullscreen_shortcut",
      ),
      (
        ShortcutAction::ScreenshotWindow,
        "screenshot_active_window_shortcut",
      ),
      (ShortcutAction::ScreenshotEdit, "screenshot_edit_shortcut"),
      (ShortcutAction::CopyText, "copy_text_shortcut"),
      (ShortcutAction::PickColor, "color_picker_shortcut"),
      (ShortcutAction::LastRegion, "last_region_shortcut"),
      (ShortcutAction::Gif, "gif_shortcut"),
      (ShortcutAction::VideoArea, "record_area_shortcut"),
      (
        ShortcutAction::VideoFullscreen,
        "record_fullscreen_shortcut",
      ),
      (ShortcutAction::VideoWindow, "record_active_window_shortcut"),
    ] {
      for value in ["", "Ctrl+Alt+F9"] {
        cases.push((
          field,
          Shortcut(action, value.into()),
          serde_json::json!(value),
        ));
      }
    }
    let directory = tempfile::tempdir().expect("isolated configuration directory");
    for (field, edit, value) in cases {
      let mut environment = Environment::new();
      environment.disk_path = Some(directory.path().join("config.json"));
      let mut original = environment.current();
      original.filename_template = Some("before".into());
      original.single_folder_subfolder_template = Some("before".into());
      original.recording_visibility_warning_dismissed = true;
      environment.replace(original.clone());
      let mut expected = serde_json::to_value(original).expect("expected JSON");
      if value.is_null() {
        expected.as_object_mut().expect("object").remove(field);
      } else {
        expected[field] = value;
      }
      let service = ConfigService::new(Duration::from_secs(1));
      service
        .save(&environment, edit.clone())
        .await
        .expect("preference saved");
      let bytes = std::fs::read(environment.disk_path.as_ref().expect("path")).expect("read disk");
      let disk: Config = serde_json::from_slice(&bytes).expect("reload config");
      assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).expect("disk JSON"),
        expected,
        "disk round trip for {edit:?}"
      );
      assert_eq!(
        serde_json::to_value(environment.current()).expect("runtime JSON"),
        expected,
        "runtime for {edit:?}"
      );
      // Omitted reset fields deserialize to their documented built-in defaults.
      // Compare raw disk data separately from these effective restored values.
      match edit {
        ResetFilename => expected[field] = serde_json::json!(crate::filename::DEFAULT_TEMPLATE),
        ResetSubfolder => {
          expected[field] = serde_json::json!(crate::capture_subfolder::DEFAULT_TEMPLATE)
        },
        _ => {},
      }
      let reloaded = Environment::new();
      reloaded.replace(disk);
      assert_eq!(
        serde_json::to_value(service.load(&reloaded).await.expect("load")).expect("reload JSON"),
        expected,
        "restoration for {edit:?}"
      );
    }
  }

  impl ConfigEnvironment for Environment {
    fn current(&self) -> Config {
      self.config.lock().expect("config lock").clone()
    }
    fn replace(&self, config: Config) {
      self.record("publish");
      *self.config.lock().expect("config lock") = config;
    }
    async fn startup_state(&self) -> Result<Option<bool>> {
      Ok(self.startup_state)
    }
    async fn set_startup(&self, enabled: bool) -> Result<()> {
      self.record(format!("startup:{enabled}"));
      if self.deny_startup {
        anyhow::bail!("Windows denied startup registration");
      }
      if self.startup_changed.swap(true, Ordering::SeqCst) && self.fail_rollback {
        anyhow::bail!("Windows denied startup rollback");
      }
      Ok(())
    }
    async fn persist(&self, config: &Config) -> Result<()> {
      self.record("persist");
      if self.pause_write {
        self.write_started.notify_one();
        tokio::time::timeout(Duration::from_secs(1), self.finish_write.notified())
          .await
          .context("Test persistence release timed out")?;
      }
      if self.fail_write {
        anyhow::bail!("Configuration write failed");
      }
      if let Some(path) = &self.disk_path {
        crate::config::write_config(path, config)?;
      }
      Ok(())
    }
    fn refresh_shortcuts(&self) {
      self.record("shortcuts");
    }
  }

  #[tokio::test]
  async fn startup_and_write_succeed_before_state_and_shortcut_publication() {
    let environment = Environment::new();
    let mut config = environment.current();
    config.auto_start = true;
    config.screenshot_area_shortcut = "Ctrl+Shift+F9".into();
    CONFIG_SERVICE
      .save(&environment, config)
      .await
      .expect("save");
    assert_eq!(
      environment.events(),
      ["startup:true", "persist", "publish", "shortcuts"]
    );
    assert!(environment.current().auto_start);
  }

  #[tokio::test]
  async fn denied_startup_never_persists_or_publishes() {
    let mut environment = Environment::new();
    environment.deny_startup = true;
    let mut config = environment.current();
    config.auto_start = true;
    assert!(CONFIG_SERVICE.save(&environment, config).await.is_err());
    assert_eq!(environment.events(), ["startup:true"]);
    assert!(!environment.current().auto_start);
  }

  #[tokio::test]
  async fn failed_write_rolls_back_startup_and_leaves_previous_state_retryable() {
    let mut environment = Environment::new();
    environment.fail_write = true;
    let mut config = environment.current();
    config.auto_start = true;
    assert!(CONFIG_SERVICE.save(&environment, config).await.is_err());
    assert_eq!(
      environment.events(),
      ["startup:true", "persist", "startup:false"]
    );
    assert!(!environment.current().auto_start);
  }

  #[tokio::test]
  async fn failed_rollback_reports_both_failures() {
    let mut environment = Environment::new();
    environment.fail_write = true;
    environment.fail_rollback = true;
    let mut config = environment.current();
    config.auto_start = true;
    let error = CONFIG_SERVICE
      .save(&environment, config)
      .await
      .expect_err("write and rollback failure");
    let message = format!("{error:#}");
    assert!(message.contains("write failed"));
    assert!(message.contains("rollback"));
    assert!(!environment.events().iter().any(|event| event == "publish"));
  }

  #[tokio::test]
  async fn unrelated_save_does_not_touch_startup_or_shortcut_registration() {
    let environment = Environment::new();
    let mut config = environment.current();
    config.video_fps = 60;
    CONFIG_SERVICE
      .save(&environment, config)
      .await
      .expect("save");
    assert_eq!(environment.events(), ["persist", "publish"]);
  }

  #[tokio::test]
  async fn load_observes_windows_startup_without_writing_preferences() {
    let mut environment = Environment::new();
    environment.startup_state = Some(true);
    assert!(
      CONFIG_SERVICE
        .load(&environment)
        .await
        .expect("load")
        .auto_start
    );
    assert_eq!(environment.events(), ["publish"]);
  }

  #[tokio::test]
  async fn operation_lock_times_out_before_any_side_effects() {
    let service = ConfigService::new(Duration::from_millis(5));
    let environment = Environment::new();
    let _guard = service.operation.lock().await;
    let error = service
      .save(&environment, environment.current())
      .await
      .expect_err("bounded acquisition");
    assert!(format!("{error:#}").contains("timed out"));
    assert!(environment.events().is_empty());
  }

  #[tokio::test]
  async fn interrupted_caller_does_not_release_an_in_progress_transaction() {
    let service = Arc::new(ConfigService::new(Duration::from_millis(10)));
    let mut environment = Environment::new();
    environment.pause_write = true;
    let mut config = environment.current();
    config.auto_start = true;
    let caller_service = service.clone();
    let caller_environment = environment.clone();
    let caller =
      tokio::spawn(async move { caller_service.save(&caller_environment, config).await });
    tokio::time::timeout(Duration::from_secs(1), environment.write_started.notified())
      .await
      .expect("write started");
    caller.abort();
    assert!(caller.await.expect_err("caller aborted").is_cancelled());
    assert!(service.load(&environment).await.is_err());
    assert!(!environment.current().auto_start);
    environment.finish_write.notify_one();
    tokio::time::timeout(Duration::from_secs(1), async {
      let _completion = service.operation.lock().await;
    })
    .await
    .expect("transaction completed");
    assert!(environment.current().auto_start);
    assert_eq!(environment.events(), ["startup:true", "persist", "publish"]);
  }
}
