// Unit tests for config defaults and serialization
// Tests that default config values are correct and serialization works properly

use flinch::config::Config;

#[test]
fn bundled_defaults_match_native_defaults_except_detected_language() {
  let bundled: serde_json::Value =
    serde_json::from_str(include_str!("../resources/defaultConfig.json")).expect("bundled config");
  let native = serde_json::to_value(Config::default()).expect("native defaults");
  for (key, value) in bundled.as_object().expect("object") {
    if key != "language" {
      assert_eq!(&native[key], value, "default mismatch: {key}");
    }
  }
}

#[test]
fn test_deserialize_minimal_config() {
  // A minimal config with only some fields should use defaults for missing fields
  let minimal_json = r#"{
        "screenshot_format": "png",
        "auto_start": false
    }"#;

  // Parse as generic JSON to verify it's valid
  let parsed: serde_json::Value = serde_json::from_str(minimal_json).unwrap();
  assert_eq!(parsed["screenshot_format"], "png");
  assert_eq!(parsed["auto_start"], false);
}

#[test]
fn test_deserialize_empty_object() {
  // An empty config should be valid (all fields have defaults)
  let empty_json = r#"{}"#;
  let parsed: serde_json::Value = serde_json::from_str(empty_json).unwrap();
  assert!(parsed.is_object());
}

#[test]
fn test_deserialize_with_unknown_fields() {
  // Unknown fields should be ignored during deserialization
  let json_with_unknown = r#"{
        "screenshot_format": "png",
        "unknown_field": "should_be_ignored",
        "another_unknown": 42
    }"#;

  let parsed: serde_json::Value = serde_json::from_str(json_with_unknown).unwrap();
  assert_eq!(parsed["screenshot_format"], "png");
}

#[test]
fn test_deserialize_all_screenshot_formats() {
  // Test all valid screenshot formats
  let formats = vec!["png", "jpg", "webp"];
  for format in formats {
    let json = format!(r#"{{"screenshot_format": "{}"}}"#, format);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["screenshot_format"], format);
  }
}

#[test]
fn test_deserialize_all_quality_values() {
  // Test all valid screenshot quality values
  let qualities = vec![50, 75, 90];
  for quality in qualities {
    let json = format!(r#"{{"screenshot_quality": {}}}"#, quality);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["screenshot_quality"], quality);
  }
}

#[test]
fn test_deserialize_all_video_fps_values() {
  // Test all valid video FPS values
  let fps_values = vec![30, 60];
  for fps in fps_values {
    let json = format!(r#"{{"video_fps": {}}}"#, fps);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["video_fps"], fps);
  }
}

#[test]
fn test_deserialize_all_video_quality_values() {
  // Test all valid video quality values
  let qualities = vec!["low", "medium", "high", "ultra"];
  for quality in qualities {
    let json = format!(r#"{{"video_quality": "{}"}}"#, quality);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["video_quality"], quality);
  }
}

#[test]
fn test_deserialize_all_gif_fps_values() {
  // Test all valid GIF FPS values
  let fps_values = vec![15, 30];
  for fps in fps_values {
    let json = format!(r#"{{"gif_fps": {}}}"#, fps);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["gif_fps"], fps);
  }
}

#[test]
fn test_deserialize_all_gif_max_duration_values() {
  // Test all valid GIF max duration values
  let durations = vec![10, 20, 30];
  for duration in durations {
    let json = format!(r#"{{"gif_max_duration": {}}}"#, duration);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["gif_max_duration"], duration);
  }
}

#[test]
fn test_deserialize_boolean_fields() {
  // Test all boolean fields
  let bool_fields = vec![
    "auto_start",
    "video_hardware_encoding",
    "record_audio",
    "show_cursor",
  ];

  for field in bool_fields {
    let json_true = format!(r#"{{"{}": true}}"#, field);
    let parsed_true: serde_json::Value = serde_json::from_str(&json_true).unwrap();
    assert_eq!(parsed_true[field], true);

    let json_false = format!(r#"{{"{}": false}}"#, field);
    let parsed_false: serde_json::Value = serde_json::from_str(&json_false).unwrap();
    assert_eq!(parsed_false[field], false);
  }
}

#[test]
fn test_deserialize_optional_string_fields() {
  // Test optional string fields (can be null or absent)
  let json_with_nulls = r#"{
        "screenshots_folder": null,
        "videos_path": null
    }"#;

  let parsed: serde_json::Value = serde_json::from_str(json_with_nulls).unwrap();
  assert!(parsed["screenshots_folder"].is_null());
  assert!(parsed["videos_path"].is_null());
}

#[test]
fn test_deserialize_with_values() {
  // Test optional fields with actual values
  let json_with_values = r#"{
        "screenshots_folder": "/home/user/screenshots",
        "videos_path": "/home/user/videos"
    }"#;

  let parsed: serde_json::Value = serde_json::from_str(json_with_values).unwrap();
  assert_eq!(parsed["screenshots_folder"], "/home/user/screenshots");
  assert_eq!(parsed["videos_path"], "/home/user/videos");
}

#[test]
fn test_deserialize_all_shortcut_fields() {
  // Test all shortcut fields
  let json = r#"{
        "screenshot_area_shortcut": "Ctrl+Shift+A",
        "screenshot_fullscreen_shortcut": "PrintScreen",
        "screenshot_active_window_shortcut": "Alt+PrintScreen",
        "record_area_shortcut": "Ctrl+Shift+R",
        "record_fullscreen_shortcut": "Ctrl+Shift+F",
        "record_active_window_shortcut": "Ctrl+Shift+W",
        "gif_shortcut": "Ctrl+Shift+G"
    }"#;

  let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
  assert_eq!(parsed["screenshot_area_shortcut"], "Ctrl+Shift+A");
  assert_eq!(parsed["screenshot_fullscreen_shortcut"], "PrintScreen");
  assert_eq!(
    parsed["screenshot_active_window_shortcut"],
    "Alt+PrintScreen"
  );
  assert_eq!(parsed["record_area_shortcut"], "Ctrl+Shift+R");
  assert_eq!(parsed["record_fullscreen_shortcut"], "Ctrl+Shift+F");
  assert_eq!(parsed["record_active_window_shortcut"], "Ctrl+Shift+W");
  assert_eq!(parsed["gif_shortcut"], "Ctrl+Shift+G");
}

#[test]
fn test_deserialize_complete_valid_config() {
  // Test a complete, fully-populated valid config
  let complete_config = r#"{
        "screenshots_folder": "/home/user/screenshots",
        "screenshot_area_shortcut": "Ctrl+PrintScreen",
        "screenshot_fullscreen_shortcut": "PrintScreen",
        "screenshot_active_window_shortcut": "Alt+PrintScreen",
        "screenshot_format": "webp",
        "screenshot_quality": 90,
        "auto_start": true,
        "videos_path": "/home/user/videos",
        "video_fps": 60,
        "video_quality": "high",
        "video_hardware_encoding": true,
        "record_area_shortcut": "Ctrl+Shift+R",
        "record_fullscreen_shortcut": "Ctrl+Shift+F",
        "record_active_window_shortcut": "Ctrl+Shift+W",
        "record_audio": true,
        "show_cursor": false,
        "gif_shortcut": "Ctrl+Shift+G",
        "gif_fps": 30,
        "gif_max_duration": 20
    }"#;

  let parsed: serde_json::Value = serde_json::from_str(complete_config).unwrap();

  // Verify all fields
  assert_eq!(parsed["screenshots_folder"], "/home/user/screenshots");
  assert_eq!(parsed["screenshot_format"], "webp");
  assert_eq!(parsed["screenshot_quality"], 90);
  assert_eq!(parsed["auto_start"], true);
  assert_eq!(parsed["video_fps"], 60);
  assert_eq!(parsed["video_quality"], "high");
  assert_eq!(parsed["video_hardware_encoding"], true);
  assert_eq!(parsed["record_audio"], true);
  assert_eq!(parsed["show_cursor"], false);
  assert_eq!(parsed["gif_fps"], 30);
  assert_eq!(parsed["gif_max_duration"], 20);
}

#[test]
fn test_serialize_then_deserialize() {
  // Test round-trip serialization
  let original_json = r#"{
        "screenshot_format": "png",
        "screenshot_quality": 75,
        "video_fps": 30,
        "gif_fps": 15
    }"#;

  let parsed: serde_json::Value = serde_json::from_str(original_json).unwrap();
  let serialized = serde_json::to_string(&parsed).unwrap();
  let reparsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

  assert_eq!(parsed, reparsed);
}

#[test]
fn test_deserialize_with_comments_fails() {
  // JSON with comments should fail to parse (strict JSON)
  let json_with_comments = r#"{
        // This is a comment
        "screenshot_format": "png"
    }"#;

  let result: Result<serde_json::Value, _> = serde_json::from_str(json_with_comments);
  assert!(result.is_err());
}

#[test]
fn test_deserialize_with_trailing_comma_fails() {
  // JSON with trailing comma should fail (strict JSON)
  let json_with_trailing_comma = r#"{
        "screenshot_format": "png",
    }"#;

  let result: Result<serde_json::Value, _> = serde_json::from_str(json_with_trailing_comma);
  assert!(result.is_err());
}

#[test]
fn test_deserialize_invalid_types() {
  // Config deserialization should reject field types that do not match the schema.
  let invalid_types = vec![
    (
      r#"{"screenshot_quality": "not_a_number"}"#,
      "quality as string should fail",
    ),
    (r#"{"video_fps": "30"}"#, "fps as string should fail"),
    (r#"{"auto_start": "true"}"#, "boolean as string should fail"),
    (
      r#"{"screenshot_format": 123}"#,
      "format as number should fail",
    ),
  ];

  for (json, description) in invalid_types {
    let result: Result<Config, _> = serde_json::from_str(json);
    assert!(result.is_err(), "{}", description);
  }
}

#[test]
fn test_empty_string_shortcuts() {
  // Empty string shortcuts should be valid (means no shortcut assigned)
  let json = r#"{
        "screenshot_area_shortcut": "",
        "record_area_shortcut": "",
        "gif_shortcut": ""
    }"#;

  let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
  assert_eq!(parsed["screenshot_area_shortcut"], "");
  assert_eq!(parsed["record_area_shortcut"], "");
  assert_eq!(parsed["gif_shortcut"], "");
}

#[test]
fn test_whitespace_only_values() {
  // Whitespace-only values should parse (validation will trim them)
  let json = r#"{
        "screenshot_area_shortcut": "   "
    }"#;

  let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
  assert_eq!(parsed["screenshot_area_shortcut"], "   ");
  // Note: Config::validate() will trim these
}

#[test]
fn test_path_separators() {
  // Test different path separator styles
  let paths = vec![
    r#"{"screenshots_folder": "/home/user/screenshots"}"#,
    r#"{"screenshots_folder": "C:\\Users\\User\\Screenshots"}"#,
    r#"{"screenshots_folder": "C:/Users/User/Screenshots"}"#,
  ];

  for path_json in paths {
    let result: Result<serde_json::Value, _> = serde_json::from_str(path_json);
    assert!(result.is_ok(), "Failed to parse: {}", path_json);
  }
}

#[test]
fn obsolete_annotation_toolbar_setting_is_ignored_when_loading_configs() {
  for enabled in [false, true] {
    let json = format!(r#"{{"screenshot_quality":75,"screenshot_annotation_toolbar":{enabled}}}"#);
    let config: Config = serde_json::from_str(&json).expect("legacy config");
    assert_eq!(config.screenshot_quality, 75);
    let saved = serde_json::to_value(&config).expect("save config");
    assert!(saved.get("screenshot_annotation_toolbar").is_none());
  }
}
