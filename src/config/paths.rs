// src/config/paths.rs
// Path resolution and filesystem utilities for configuration

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

use super::types::ConfigError;

pub fn config_dir() -> Result<PathBuf> {
  let dir = match crate::probe_profile::folder() {
    Some(profile) => profile,
    None => dirs::config_dir()
      .ok_or_else(|| ConfigError::ReadError("Could not find config directory".to_string()))?
      .join("flinch"),
  };

  fs::create_dir_all(&dir).context("Failed to create config directory")?;

  Ok(dir)
}

pub fn config_file() -> Result<PathBuf> {
  Ok(config_dir()?.join("config.json"))
}

/// Resolve installed resources; source lookup is explicit and debug-only.
pub fn default_config_file(app: &crate::app::context::AppHandle) -> Option<PathBuf> {
  let bundled = app.resources().join("resources/defaultConfig.json");
  if bundled.is_file() {
    return Some(bundled);
  }
  #[cfg(debug_assertions)]
  {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/defaultConfig.json");
    if source.is_file() {
      return Some(source);
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bundled_configuration_takes_precedence_over_debug_sources() {
    let folder = tempfile::tempdir().unwrap();
    let app = crate::app::context::AppHandle::isolated(
      super::super::Config::default(),
      folder.path().to_path_buf(),
    );
    fs::create_dir(folder.path().join("resources")).unwrap();
    let defaults = folder.path().join("resources/defaultConfig.json");
    fs::write(&defaults, b"{}").unwrap();
    assert_eq!(default_config_file(&app), Some(defaults));
    fs::remove_file(folder.path().join("resources/defaultConfig.json")).unwrap();
    assert!(
      default_config_file(&app)
        .unwrap()
        .ends_with("resources/defaultConfig.json")
    );
  }

  #[test]
  fn test_config_dir_returns_path() {
    let result = config_dir();
    assert!(result.is_ok());

    let path = result.unwrap();
    assert!(path.ends_with("flinch"));
  }

  #[test]
  fn test_config_dir_creates_directory() {
    let result = config_dir();
    assert!(result.is_ok());

    let path = result.unwrap();
    assert!(path.exists());
    assert!(path.is_dir());
  }

  #[test]
  fn test_config_file_has_json_extension() {
    let result = config_file();
    assert!(result.is_ok());

    let path = result.unwrap();
    assert!(path.ends_with("config.json"));
  }

  #[test]
  fn test_config_file_parent_is_config_dir() {
    let config_dir_path = config_dir().unwrap();
    let config_file_path = config_file().unwrap();

    assert_eq!(
      config_file_path.parent().unwrap(),
      config_dir_path.as_path()
    );
  }

  #[test]
  fn test_config_dir_path_format() {
    let path = config_dir().unwrap();
    let path_str = path.to_string_lossy();

    // Should contain "flinch" directory
    assert!(path_str.contains("flinch"));

    // Windows-specific path check
    assert!(path_str.contains("AppData") || path_str.contains("APPDATA"));
  }

  #[test]
  fn test_config_paths_are_absolute() {
    let config_dir_path = config_dir().unwrap();
    let config_file_path = config_file().unwrap();

    assert!(config_dir_path.is_absolute());
    assert!(config_file_path.is_absolute());
  }
}
