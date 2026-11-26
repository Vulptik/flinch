//! Configuration action workflows shared by the native host and isolated tests.
//! The picker and language publication are injected; the existing bounded service
//! owns persistence, startup rollback and shortcut publication. No UI is captured.
use std::{future::Future, io::Read, path::PathBuf};

use super::config_service::{ConfigEnvironment, ConfigService};
use crate::{config::Config, error::ErrorExt};

const MAX_CONFIG_SIZE: u64 = 10 * 1024 * 1024;

pub(crate) trait ActionEnvironment: ConfigEnvironment {
  fn pick(&self, export: bool) -> impl Future<Output = Result<Option<PathBuf>, String>> + Send;
  fn language(&self, language: String) -> impl Future<Output = Result<(), String>> + Send;
}

pub(crate) async fn reset(
  service: &ConfigService,
  environment: &impl ActionEnvironment,
) -> Result<Config, String> {
  let mut config = Config::default();
  config.validate();
  publish(service, environment, config).await
}

async fn publish(
  service: &ConfigService,
  environment: &impl ActionEnvironment,
  config: Config,
) -> Result<Config, String> {
  service
    .save(environment, config.clone())
    .await
    .map_err(|error| error.to_rich_string())?;
  environment.language(config.language.clone()).await?;
  Ok(config)
}

pub(crate) async fn import(
  service: &ConfigService,
  environment: &impl ActionEnvironment,
) -> Result<Option<Config>, String> {
  let Some(path) = environment.pick(false).await? else {
    return Ok(None);
  };
  let config = tokio::task::spawn_blocking(move || read_import(path))
    .await
    .map_err(|error| crate::commands::ipc_context("Configuration import worker failed", error))??;
  publish(service, environment, config).await.map(Some)
}

fn read_import(path: PathBuf) -> Result<Config, String> {
  let file = std::fs::File::open(&path)
    .map_err(|error| crate::commands::ipc_context("Failed to read config file", error))?;
  let metadata = file
    .metadata()
    .map_err(|error| crate::commands::ipc_context("Failed to read file metadata", error))?;
  crate::commands::ensure_size_limit(metadata.len(), MAX_CONFIG_SIZE, "Config file")?;
  // Bound the read too: the file can grow after its metadata was checked.
  let mut content = Vec::new();
  file
    .take(MAX_CONFIG_SIZE + 1)
    .read_to_end(&mut content)
    .map_err(|error| crate::commands::ipc_context("Failed to read config file", error))?;
  crate::commands::ensure_size_limit(content.len() as u64, MAX_CONFIG_SIZE, "Config file")?;
  let document: serde_json::Value = serde_json::from_slice(&content)
    .map_err(|error| crate::commands::ipc_context("Invalid config file format", error))?;
  if !document.is_object() {
    return Err("Invalid config file format: expected a JSON object".into());
  }
  let mut config: Config = serde_json::from_value(document)
    .map_err(|error| crate::commands::ipc_context("Invalid config file format", error))?;
  config.validate();
  Ok(config)
}

pub(crate) async fn export(environment: &impl ActionEnvironment) -> Result<bool, String> {
  let content = serde_json::to_vec_pretty(&environment.current())
    .map_err(|error| crate::commands::ipc_context("Failed to serialize config", error))?;
  crate::commands::ensure_size_limit(content.len() as u64, MAX_CONFIG_SIZE, "Config file")?;
  let Some(path) = environment.pick(true).await? else {
    return Ok(false);
  };
  tokio::fs::write(path, content)
    .await
    .map_err(|error| crate::commands::ipc_context("Failed to write config file", error))?;
  Ok(true)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{
    sync::{Arc, Mutex},
    time::Duration,
  };

  #[derive(Clone)]
  struct Environment {
    config: Arc<Mutex<Config>>,
    events: Arc<Mutex<Vec<String>>>,
    selected: Option<PathBuf>,
    disk: PathBuf,
    picker_failure: bool,
    write_failure: bool,
    startup_failure: bool,
    language_failure: bool,
  }

  impl Environment {
    fn new(directory: &tempfile::TempDir, selected: Option<PathBuf>) -> Self {
      let config = Config {
        language: "en".into(),
        record_audio: true,
        video_fps: 60,
        ..Config::default()
      };
      let disk = directory.path().join("saved.json");
      crate::config::write_config(&disk, &config).expect("initial isolated config");
      Self {
        config: Arc::new(Mutex::new(config)),
        events: Arc::default(),
        selected,
        disk,
        picker_failure: false,
        write_failure: false,
        startup_failure: false,
        language_failure: false,
      }
    }
    fn event(&self, event: &str) {
      self.events.lock().expect("events").push(event.into());
    }
    fn events(&self) -> Vec<String> {
      self.events.lock().expect("events").clone()
    }
    fn bytes(&self) -> Vec<u8> {
      std::fs::read(&self.disk).expect("isolated saved config")
    }
  }

  impl ConfigEnvironment for Environment {
    fn current(&self) -> Config {
      self.config.lock().expect("config").clone()
    }
    fn replace(&self, config: Config) {
      self.event("publish");
      *self.config.lock().expect("config") = config;
    }
    async fn startup_state(&self) -> anyhow::Result<Option<bool>> {
      Ok(None)
    }
    async fn set_startup(&self, enabled: bool) -> anyhow::Result<()> {
      self.event(if enabled { "startup-on" } else { "startup-off" });
      anyhow::ensure!(!self.startup_failure, "Injected startup failure");
      Ok(())
    }
    async fn persist(&self, config: &Config) -> anyhow::Result<()> {
      self.event("persist");
      anyhow::ensure!(!self.write_failure, "Injected persistence failure");
      crate::config::write_config(&self.disk, config)?;
      Ok(())
    }
    fn refresh_shortcuts(&self) {
      self.event("shortcuts");
    }
  }
  impl ActionEnvironment for Environment {
    async fn pick(&self, export: bool) -> Result<Option<PathBuf>, String> {
      self.event(if export {
        "export-picker"
      } else {
        "import-picker"
      });
      if self.picker_failure {
        return Err("Injected picker failure".into());
      }
      Ok(self.selected.clone())
    }
    async fn language(&self, language: String) -> Result<(), String> {
      self.event(&format!("language:{language}"));
      if self.language_failure {
        return Err("Injected language failure".into());
      }
      Ok(())
    }
  }

  fn service() -> ConfigService {
    ConfigService::new(Duration::from_secs(1))
  }
  fn json(config: &Config) -> serde_json::Value {
    serde_json::to_value(config).expect("config JSON")
  }

  #[tokio::test]
  async fn export_import_round_trip_persists_and_returns_refresh_configuration() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("export.json");
    let source = Environment::new(&directory, Some(path.clone()));
    let expected = source.current();
    assert!(export(&source).await.expect("export"));
    assert_eq!(source.events(), ["export-picker"]);
    let mut target = Environment::new(&directory, Some(path));
    target.config = Arc::new(Mutex::new(Config {
      language: "fr".into(),
      ..Config::default()
    }));
    let imported = import(&service(), &target)
      .await
      .expect("import")
      .expect("refresh config");
    assert_eq!(json(&imported), json(&expected));
    assert_eq!(json(&target.current()), json(&expected));
    let persisted: Config = serde_json::from_slice(&target.bytes()).expect("persisted config");
    assert_eq!(json(&persisted), json(&expected));
    assert_eq!(
      target.events(),
      ["import-picker", "persist", "publish", "language:en"]
    );
  }

  #[tokio::test]
  async fn exported_reset_templates_retain_their_effective_defaults_on_import() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("export.json");
    let environment = Environment::new(&directory, Some(path.clone()));
    {
      let mut config = environment.config.lock().expect("config");
      config.filename_template = None;
      config.single_folder_subfolder_template = None;
    }
    let previous = environment.current();
    export(&environment).await.expect("export reset templates");
    let document: serde_json::Value =
      serde_json::from_slice(&std::fs::read(path).expect("exported JSON")).expect("JSON object");
    assert_eq!(document, json(&previous));
    let imported = import(&service(), &environment)
      .await
      .expect("import reset templates")
      .expect("refresh config");
    assert_eq!(
      imported.effective_filename_template(),
      previous.effective_filename_template()
    );
    assert_eq!(
      imported.effective_single_folder_subfolder_template(),
      previous.effective_single_folder_subfolder_template()
    );
    assert_eq!(imported.video_fps, previous.video_fps);
    assert_eq!(imported.record_audio, previous.record_audio);
  }

  #[tokio::test]
  async fn cancelled_pickers_do_not_write_or_publish() {
    let directory = tempfile::tempdir().expect("temp directory");
    let environment = Environment::new(&directory, None);
    let before = environment.bytes();
    assert!(!export(&environment).await.expect("cancel export"));
    assert!(
      import(&service(), &environment)
        .await
        .expect("cancel import")
        .is_none()
    );
    assert_eq!(environment.bytes(), before);
    assert_eq!(environment.events(), ["export-picker", "import-picker"]);
  }

  #[tokio::test]
  async fn unreadable_malformed_and_oversized_imports_preserve_configuration() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("import.json");
    for input in [
      None,
      Some(b"not JSON".as_slice()),
      Some(b"[]".as_slice()),
      Some(b"{\"video_fps\":\"bad\"}".as_slice()),
      Some(b"\xff".as_slice()),
    ] {
      let environment = Environment::new(&directory, Some(path.clone()));
      if let Some(bytes) = input {
        std::fs::write(&path, bytes).expect("input");
      }
      let before = environment.bytes();
      let current = json(&environment.current());
      assert!(import(&service(), &environment).await.is_err());
      assert_eq!(environment.bytes(), before);
      assert_eq!(json(&environment.current()), current);
      assert_eq!(environment.events(), ["import-picker"]);
    }
    std::fs::File::create(&path)
      .expect("large file")
      .set_len(MAX_CONFIG_SIZE + 1)
      .expect("oversized length");
    let environment = Environment::new(&directory, Some(path));
    let before = environment.bytes();
    assert!(import(&service(), &environment).await.is_err());
    assert_eq!(environment.bytes(), before);
    assert_eq!(environment.events(), ["import-picker"]);
  }

  #[tokio::test]
  async fn import_validates_partial_config_before_persistence() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("import.json");
    std::fs::write(
      &path,
      r#"{"video_fps":144,"language":"fr","screenshot_quality":101}"#,
    )
    .expect("input");
    let environment = Environment::new(&directory, Some(path));
    let result = import(&service(), &environment)
      .await
      .expect("import")
      .expect("config");
    assert_eq!(result.video_fps, 30);
    assert_eq!(result.screenshot_quality, 90);
    assert_eq!(result.language, "fr");
    assert_eq!(
      result.screenshot_format,
      Config::default().screenshot_format
    );
    assert_eq!(json(&result), json(&environment.current()));
    assert_eq!(
      environment.events().last().map(String::as_str),
      Some("language:fr")
    );
  }

  #[tokio::test]
  async fn picker_and_export_write_failures_are_reported_without_mutation() {
    let directory = tempfile::tempdir().expect("temp directory");
    let mut environment = Environment::new(&directory, Some(directory.path().to_owned()));
    let before = environment.bytes();
    assert!(
      export(&environment)
        .await
        .expect_err("directory cannot be exported to")
        .contains("Failed to write")
    );
    environment.picker_failure = true;
    assert!(
      export(&environment)
        .await
        .expect_err("picker failed")
        .contains("picker failure")
    );
    assert!(
      import(&service(), &environment)
        .await
        .expect_err("picker failed")
        .contains("picker failure")
    );
    assert_eq!(environment.bytes(), before);
    assert_eq!(
      environment.events(),
      ["export-picker", "export-picker", "import-picker"]
    );
  }

  #[tokio::test]
  async fn import_and_reset_write_failures_preserve_disk_runtime_and_language() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("import.json");
    std::fs::write(&path, r#"{"language":"fr","auto_start":true}"#).expect("input");
    let mut environment = Environment::new(&directory, Some(path));
    environment.write_failure = true;
    let before = environment.bytes();
    let current = json(&environment.current());
    assert!(
      import(&service(), &environment)
        .await
        .expect_err("failed write")
        .contains("persistence failure")
    );
    assert_eq!(
      environment.events(),
      ["import-picker", "startup-on", "persist", "startup-off"]
    );
    assert!(reset(&service(), &environment).await.is_err());
    assert_eq!(environment.bytes(), before);
    assert_eq!(json(&environment.current()), current);
    assert!(
      !environment
        .events()
        .iter()
        .any(|event| event == "publish" || event.starts_with("language:"))
    );
  }

  #[tokio::test]
  async fn denied_startup_import_does_not_write_or_publish() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("import.json");
    std::fs::write(&path, r#"{"auto_start":true}"#).expect("input");
    let mut environment = Environment::new(&directory, Some(path));
    environment.startup_failure = true;
    let before = environment.bytes();
    assert!(import(&service(), &environment).await.is_err());
    assert_eq!(environment.bytes(), before);
    assert_eq!(environment.events(), ["import-picker", "startup-on"]);
  }

  #[tokio::test]
  async fn reset_persists_defaults_and_returns_refresh_configuration() {
    let directory = tempfile::tempdir().expect("temp directory");
    let environment = Environment::new(&directory, None);
    let mut expected = Config::default();
    expected.validate();
    let result = reset(&service(), &environment).await.expect("reset");
    assert_eq!(json(&result), json(&expected));
    assert_eq!(json(&environment.current()), json(&expected));
    let persisted: Config = serde_json::from_slice(&environment.bytes()).expect("disk config");
    assert_eq!(json(&persisted), json(&expected));
    assert_eq!(
      environment.events().first().map(String::as_str),
      Some("persist")
    );
    assert_eq!(
      environment.events().last(),
      Some(&format!("language:{}", expected.language))
    );
  }

  #[tokio::test]
  async fn language_publication_failure_is_explicit_after_successful_save() {
    let directory = tempfile::tempdir().expect("temp directory");
    let mut environment = Environment::new(&directory, None);
    environment.language_failure = true;
    assert!(
      reset(&service(), &environment)
        .await
        .expect_err("language failure")
        .contains("language failure")
    );
    assert!(environment.events().iter().any(|event| event == "publish"));
    let persisted: Config = serde_json::from_slice(&environment.bytes()).expect("disk config");
    assert_eq!(json(&persisted), json(&environment.current()));
  }
}
