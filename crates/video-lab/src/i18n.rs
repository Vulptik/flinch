use flinch_video_engine::{EngineError, Result};
static COPY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
pub fn initialize(locale: &str) -> Result<()> {
  COPY
    .set(catalog(locale)?)
    .map_err(|_| EngineError::protocol("Lab catalog already initialized"))
}
fn catalog(locale: &str) -> Result<serde_json::Value> {
  let source = match locale {
    "en" => include_str!("../../../messages/en.json"),
    "fr" => include_str!("../../../messages/fr.json"),
    "de" => include_str!("../../../messages/de.json"),
    "es" => include_str!("../../../messages/es.json"),
    "it" => include_str!("../../../messages/it.json"),
    _ => {
      return Err(EngineError::protocol(
        "--lang requires en, fr, de, es or it",
      ));
    },
  };
  serde_json::from_str(source).map_err(|e| EngineError::protocol(format!("Lab catalog: {e}")))
}

pub fn text(key: &str) -> &str {
  COPY
    .get()
    .and_then(|v| v.get(key))
    .and_then(|v| v.as_str())
    .unwrap_or(key)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn all_lab_catalogs_have_copy_and_initialization_does_not_change_the_active_locale() {
    for locale in ["en", "fr", "de", "es", "it"] {
      assert!(
        !catalog(locale).expect("catalog")["video_lab_help"]
          .as_str()
          .expect("help")
          .is_empty()
      );
    }
    assert!(initialize("unknown").is_err());
    initialize("fr").expect("initialize once");
    let copy = text("video_lab_error").to_owned();
    assert!(initialize("en").is_err());
    assert_eq!(text("video_lab_error"), copy);
    assert_eq!(text("unknown-key"), "unknown-key");
  }
}
