// src/error.rs
// Error conversion utilities for preserving context across native service boundaries

use std::fmt::Display;

/// Extension trait for converting anyhow::Error to rich string representations
///
/// This preserves the entire error chain when crossing a native service boundary,
/// where we're required to use `Result<T, String>` for user-facing errors.
pub trait ErrorExt {
  /// Convert error to string with full context chain
  ///
  /// Returns a single-line string with all error causes separated by ": ".
  /// Suitable for simple error messages and logs.
  ///
  /// # Example
  /// ```no_run
  /// use anyhow::{Context, Result};
  /// use flinch::error::ErrorExt;
  ///
  /// fn inner() -> Result<()> {
  ///   Err(anyhow::anyhow!("disk full"))
  /// }
  ///
  /// fn outer() -> Result<()> {
  ///   inner().context("failed to write config")?;
  ///   Ok(())
  /// }
  ///
  /// let err = outer().unwrap_err();
  /// assert_eq!(
  ///   err.to_rich_string(),
  ///   "failed to write config: disk full"
  /// );
  /// ```
  fn to_rich_string(&self) -> String;
}

impl ErrorExt for anyhow::Error {
  fn to_rich_string(&self) -> String {
    // Collect all errors in the chain
    let chain: Vec<String> = self.chain().map(|e| e.to_string()).collect();

    // Join with ": " separator
    chain.join(": ")
  }
}

pub fn rich_context<E>(context: &str, error: E) -> String
where
  E: Into<anyhow::Error>,
{
  error.into().context(context.to_string()).to_rich_string()
}

pub fn rich_display_context(context: &str, error: impl Display) -> String {
  anyhow::anyhow!("{error}")
    .context(context.to_string())
    .to_rich_string()
}

/// Helper macro for converting anyhow::Result to Result<T, String> with rich context
///
/// Use this in native services to preserve error chains at the service boundary.
///
/// # Example
///
/// ```no_run
/// use flinch::rich_error;
///
/// pub async fn save_config(_config: serde_json::Value) -> Result<(), String> {
///   let operation: anyhow::Result<()> = Ok(());
///   operation.map_err(rich_error!())?;
///   Ok(())
/// }
/// ```
#[macro_export]
macro_rules! rich_error {
  () => {
    |e: anyhow::Error| $crate::error::ErrorExt::to_rich_string(&e)
  };
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_rich_string_single_error() {
    let err = anyhow::anyhow!("disk full");
    assert_eq!(err.to_rich_string(), "disk full");
  }

  #[test]
  fn test_rich_string_with_context() {
    let err = anyhow::anyhow!("disk full")
      .context("failed to write file")
      .context("save config failed");

    assert_eq!(
      err.to_rich_string(),
      "save config failed: failed to write file: disk full"
    );
  }

  #[test]
  fn test_rich_string_multiple_contexts() {
    let err = anyhow::anyhow!("permission denied")
      .context("write failed")
      .context("save failed")
      .context("config update failed");

    let rich = err.to_rich_string();
    assert!(rich.contains("permission denied"));
    assert!(rich.contains("write failed"));
    assert!(rich.contains("save failed"));
    assert!(rich.contains("config update failed"));
  }

  #[test]
  fn test_rich_string_preserves_order() {
    let err = anyhow::anyhow!("root cause")
      .context("middle layer")
      .context("top level");

    // Chain should be: top -> middle -> root
    assert_eq!(err.to_rich_string(), "top level: middle layer: root cause");
  }

  #[test]
  fn test_rich_string_empty_message() {
    let err = anyhow::anyhow!("");
    assert_eq!(err.to_rich_string(), "");
  }

  #[test]
  fn test_rich_string_special_characters() {
    let err = anyhow::anyhow!("error: with: colons").context("context with: colons");

    let rich = err.to_rich_string();
    // Should contain colons from both the message and separator
    assert!(rich.contains("error: with: colons"));
    assert!(rich.contains("context with: colons"));
  }

  #[test]
  fn test_rich_string_unicode() {
    let err = anyhow::anyhow!("错误消息").context("日本語のコンテキスト");

    let rich = err.to_rich_string();
    assert!(rich.contains("错误消息"));
    assert!(rich.contains("日本語のコンテキスト"));
  }

  #[test]
  fn test_rich_string_long_message() {
    let long_msg = "a".repeat(1000);
    let err = anyhow::anyhow!("{}", long_msg);

    let rich = err.to_rich_string();
    assert_eq!(rich.len(), 1000);
  }

  #[test]
  fn test_rich_string_newlines() {
    let err = anyhow::anyhow!("line1\nline2").context("context");

    let rich = err.to_rich_string();
    assert!(rich.contains("line1\nline2"));
  }

  #[test]
  fn test_rich_error_macro() {
    fn inner() -> anyhow::Result<()> {
      Err(anyhow::anyhow!("inner error"))
    }

    fn outer() -> Result<(), String> {
      inner().map_err(rich_error!())?;
      Ok(())
    }

    let result = outer();
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "inner error");
  }

  #[test]
  fn test_rich_error_macro_with_context() {
    use anyhow::Context;

    fn inner() -> anyhow::Result<()> {
      Err(anyhow::anyhow!("io error"))
    }

    fn outer() -> Result<(), String> {
      inner().context("failed to read").map_err(rich_error!())?;
      Ok(())
    }

    let result = outer();
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "failed to read: io error");
  }
}
