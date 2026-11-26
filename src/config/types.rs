// src/config/types.rs
// Type definitions for configuration

use thiserror::Error;

#[derive(Error, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum ConfigError {
  #[error("Failed to read config file: {0}")]
  ReadError(String),
  #[error("Failed to write config file: {0}")]
  WriteError(String),
  #[error("Failed to parse config: {0}")]
  ParseError(String),
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_config_error_display() {
    let err = ConfigError::ReadError("file not found".to_string());
    assert_eq!(
      err.to_string(),
      "Failed to read config file: file not found"
    );

    let err = ConfigError::WriteError("permission denied".to_string());
    assert_eq!(
      err.to_string(),
      "Failed to write config file: permission denied"
    );

    let err = ConfigError::ParseError("invalid JSON".to_string());
    assert_eq!(err.to_string(), "Failed to parse config: invalid JSON");
  }
}
