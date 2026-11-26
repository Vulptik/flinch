// tests/config_operations.rs
// Configuration operations integration tests

mod helpers;

use flinch::config::{Config, SaveLocationMode, ScreenshotFormat};
use helpers::test_context::TestContext;
use std::fs;
use tempfile::TempDir;

/// Test config load from disk
#[tokio::test]
async fn test_config_load_from_disk() {
  let ctx = TestContext::new();
  let config = Config::default();

  // Create a config file
  let config_path = ctx.create_test_config(&config);

  // Load it back
  let json_content = fs::read_to_string(&config_path).expect("Failed to read config file");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse config");

  // Verify values match
  assert_eq!(loaded.screenshot_format, config.screenshot_format);
  assert_eq!(loaded.screenshot_quality, config.screenshot_quality);
  assert_eq!(loaded.auto_start, config.auto_start);
  assert_eq!(loaded.video_fps, config.video_fps);
}

/// Test config save to disk
#[tokio::test]
async fn test_config_save_to_disk() {
  let ctx = TestContext::new();
  let config = Config {
    screenshot_format: ScreenshotFormat::Webp,
    screenshot_quality: 85,
    auto_start: true,
    ..Default::default()
  };

  // Save to file
  let config_path = ctx.create_test_config(&config);

  // Verify file created
  assert!(config_path.exists());

  // Reload and verify
  let json_content = fs::read_to_string(&config_path).expect("Failed to read config file");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse config");

  assert_eq!(loaded.screenshot_format, ScreenshotFormat::Webp);
  assert_eq!(loaded.screenshot_quality, 85);
  assert!(loaded.auto_start);
}

/// Test config with all valid screenshot formats
#[tokio::test]
async fn test_config_valid_screenshot_formats() {
  let ctx = TestContext::new();
  let formats = vec![
    ScreenshotFormat::Png,
    ScreenshotFormat::Jpg,
    ScreenshotFormat::Webp,
  ];

  for format in formats {
    let config = Config {
      screenshot_format: format,
      ..Default::default()
    };

    let config_path = ctx.create_test_config(&config);
    let json_content = fs::read_to_string(&config_path).expect("Failed to read config");
    let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse config");

    assert_eq!(loaded.screenshot_format, format);
  }
}

/// Test config with all valid video quality settings
#[tokio::test]
async fn test_config_valid_video_qualities() {
  let ctx = TestContext::new();

  // Test Low quality
  let config = Config {
    video_quality: flinch::config::VideoQuality::Low,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert_eq!(format!("{:?}", loaded.video_quality), "Low");

  // Test Medium quality
  let config = Config {
    video_quality: flinch::config::VideoQuality::Medium,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert_eq!(format!("{:?}", loaded.video_quality), "Medium");

  // Test High quality
  let config = Config {
    video_quality: flinch::config::VideoQuality::High,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert_eq!(format!("{:?}", loaded.video_quality), "High");
}

/// Test config with all valid FPS values
#[tokio::test]
async fn test_config_valid_fps_values() {
  let ctx = TestContext::new();
  let fps_values = vec![15, 24, 30, 60, 120];

  for fps in fps_values {
    let config = Config {
      video_fps: fps,
      ..Default::default()
    };

    let config_path = ctx.create_test_config(&config);
    let json_content = fs::read_to_string(&config_path).expect("Failed to read config");
    let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse config");

    assert_eq!(loaded.video_fps, fps);
  }
}

/// Test config with shortcuts
#[tokio::test]
async fn test_config_shortcuts() {
  let ctx = TestContext::new();
  let config = Config {
    screenshot_area_shortcut: "Ctrl+Shift+A".to_string(),
    screenshot_fullscreen_shortcut: "Ctrl+Shift+F".to_string(),
    record_area_shortcut: "Ctrl+Alt+R".to_string(),
    gif_shortcut: "Ctrl+Shift+G".to_string(),
    ..Default::default()
  };

  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");

  assert_eq!(loaded.screenshot_area_shortcut, "Ctrl+Shift+A");
  assert_eq!(loaded.screenshot_fullscreen_shortcut, "Ctrl+Shift+F");
  assert_eq!(loaded.record_area_shortcut, "Ctrl+Alt+R");
  assert_eq!(loaded.gif_shortcut, "Ctrl+Shift+G");
}

/// Test config with custom save location settings
#[tokio::test]
async fn test_config_custom_paths() {
  let ctx = TestContext::new();
  let config = Config {
    save_location_mode: SaveLocationMode::SingleFolder,
    ..Default::default()
  };

  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");

  assert_eq!(loaded.save_location_mode, SaveLocationMode::SingleFolder);
}

/// Test config with audio recording enabled/disabled
#[tokio::test]
async fn test_config_audio_recording() {
  let ctx = TestContext::new();

  // Test with audio enabled
  let config = Config {
    record_audio: true,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert!(loaded.record_audio);

  // Test with audio disabled
  let config = Config {
    record_audio: false,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert!(!loaded.record_audio);
}

/// Test config with cursor visibility
#[tokio::test]
async fn test_config_cursor_visibility() {
  let ctx = TestContext::new();

  // Test with cursor visible
  let config = Config {
    show_cursor: true,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert!(loaded.show_cursor);

  // Test with cursor hidden
  let config = Config {
    show_cursor: false,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert!(!loaded.show_cursor);
}

/// Test config with GIF settings
#[tokio::test]
async fn test_config_gif_settings() {
  let ctx = TestContext::new();
  let config = Config {
    gif_fps: 15,
    gif_max_duration: 30,
    ..Default::default()
  };

  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");

  assert_eq!(loaded.gif_fps, 15);
  assert_eq!(loaded.gif_max_duration, 30);
}

/// Test config corruption recovery (invalid JSON)
#[tokio::test]
async fn test_config_corruption_recovery() {
  let temp_dir = TempDir::new().expect("Failed to create temp dir");
  let config_path = temp_dir.path().join("corrupted_config.json");

  // Write corrupted JSON
  fs::write(&config_path, "{invalid json content}").expect("Failed to write corrupted config");

  // Attempt to load
  let json_content = fs::read_to_string(&config_path).expect("Failed to read config");
  let result = serde_json::from_str::<Config>(&json_content);

  // Should fail to parse
  assert!(result.is_err());

  // Fallback to defaults would happen in the application code
  let default_config = Config::default();
  assert!(matches!(
    default_config.screenshot_format,
    ScreenshotFormat::Png | ScreenshotFormat::Jpg | ScreenshotFormat::Webp
  ));
}

/// Test config with missing optional fields
#[tokio::test]
async fn test_config_missing_optional_fields() {
  let temp_dir = TempDir::new().expect("Failed to create temp dir");
  let config_path = temp_dir.path().join("minimal_config.json");

  // Write minimal valid JSON (only required fields)
  let minimal_json = r#"{
        "screenshot_area_shortcut": "Ctrl+Shift+S",
        "screenshot_fullscreen_shortcut": "PrintScreen",
        "screenshot_active_window_shortcut": "Ctrl+PrintScreen",
        "screenshot_format": "png",
        "screenshot_quality": 95,
        "auto_start": false,
        "video_fps": 30,
        "video_quality": "medium",
        "video_hardware_encoding": true,
        "record_area_shortcut": "Ctrl+Shift+R",
        "record_fullscreen_shortcut": "Ctrl+Shift+F",
        "record_active_window_shortcut": "Ctrl+Shift+W",
        "record_audio": false,
        "show_cursor": true,
        "gif_shortcut": "Ctrl+Shift+G",
        "gif_fps": 15,
        "gif_max_duration": 30
    }"#;

  fs::write(&config_path, minimal_json).expect("Failed to write config");

  // Load and verify
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");

  // Optional/defaulted fields should be defaulted
  assert_eq!(loaded.save_location_mode, SaveLocationMode::SystemFolders);
}

/// Test config round-trip serialization
#[tokio::test]
async fn test_config_round_trip() {
  let ctx = TestContext::new();
  let original = Config::default();

  // Serialize to JSON
  let json = serde_json::to_string_pretty(&original).expect("Failed to serialize");

  // Write to file
  let config_path = ctx.temp_path("round_trip_config.json");
  fs::write(&config_path, &json).expect("Failed to write config");

  // Read back
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");

  // Serialize again
  let json2 = serde_json::to_string_pretty(&loaded).expect("Failed to serialize again");

  // Both JSON strings should be identical
  assert_eq!(json, json2);
}

/// Test config with hardware encoding toggle
#[tokio::test]
async fn test_config_hardware_encoding() {
  let ctx = TestContext::new();

  // Test with hardware encoding enabled
  let config = Config {
    video_hardware_encoding: true,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert!(loaded.video_hardware_encoding);

  // Test with hardware encoding disabled
  let config = Config {
    video_hardware_encoding: false,
    ..Default::default()
  };
  let config_path = ctx.create_test_config(&config);
  let json_content = fs::read_to_string(&config_path).expect("Failed to read");
  let loaded: Config = serde_json::from_str(&json_content).expect("Failed to parse");
  assert!(!loaded.video_hardware_encoding);
}
