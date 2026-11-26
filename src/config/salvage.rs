// src/config/salvage.rs
// Keeps the fields of a saved config that still parse when the file as a whole does not

use super::Config;
use serde_json::Value;

/// Merge saved fields over `base` one at a time, dropping each one `Config` rejects.
/// Returns `None` when the content is not a JSON object or no known field survives.
pub(super) fn keep_valid_fields(content: &str, base: &Config) -> Option<Config> {
  let Ok(Value::Object(saved)) = serde_json::from_str::<Value>(content) else {
    return None;
  };
  let mut merged = match serde_json::to_value(base) {
    Ok(Value::Object(merged)) => merged,
    Ok(_) => return None,
    Err(error) => {
      tracing::warn!(
        operation = "salvage_config",
        error = %error,
        "Cannot prepare defaults for field recovery"
      );
      return None;
    },
  };
  let mut kept = 0_usize;
  let mut rejected = Vec::new();
  for (field, value) in saved {
    let previous = merged.insert(field.clone(), value);
    if serde_json::from_value::<Config>(Value::Object(merged.clone())).is_ok() {
      kept += usize::from(previous.is_some());
      continue;
    }
    match previous {
      Some(previous) => merged.insert(field.clone(), previous),
      None => merged.remove(&field),
    };
    rejected.push(field);
  }
  if kept == 0 {
    return None;
  }
  tracing::warn!(
    operation = "salvage_config",
    fields = ?rejected,
    "Reset saved settings that no longer parse"
  );
  match serde_json::from_value(Value::Object(merged)) {
    Ok(config) => Some(config),
    Err(error) => {
      tracing::warn!(
        operation = "salvage_config",
        error = %error,
        "Recovered settings did not parse"
      );
      None
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_field_of_the_wrong_type_resets_alone() {
    let content = r#"{"language":"fr","screenshot_quality":50,"video_fps":"sixty"}"#;
    let base = Config {
      video_fps: 60,
      ..Config::default()
    };

    let kept = keep_valid_fields(content, &base).unwrap();

    assert_eq!(kept.language, "fr");
    assert_eq!(kept.screenshot_quality, 50);
    assert_eq!(kept.video_fps, 60);
  }

  #[test]
  fn optional_fields_missing_from_the_base_are_kept_or_dropped_cleanly() {
    let content = r#"{"language":"fr","custom_single_folder":"D:\\Captures","custom_videos_folder":7,"video_fps":"sixty"}"#;

    let kept = keep_valid_fields(content, &Config::default()).unwrap();

    assert_eq!(kept.custom_single_folder.as_deref(), Some("D:\\Captures"));
    assert!(kept.custom_videos_folder.is_none());
  }

  #[test]
  fn nothing_usable_returns_none() {
    let base = Config::default();
    assert!(keep_valid_fields("{broken", &base).is_none());
    assert!(keep_valid_fields("[1, 2]", &base).is_none());
    assert!(keep_valid_fields("null", &base).is_none());
    assert!(keep_valid_fields(r#"{"video_fps":"sixty"}"#, &base).is_none());
    assert!(keep_valid_fields(r#"{"retired":1,"video_fps":"sixty"}"#, &base).is_none());
  }
}
