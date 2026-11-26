// tests/video_config_tests.rs
// Video configuration tests

mod helpers;

use flinch::video::config::{AudioConfig, VideoConfig, VideoQuality};
use helpers::test_context::TestContext;

/// Test VideoConfig default values
#[tokio::test]
async fn test_video_config_default() {
  let config = VideoConfig::default();

  assert_eq!(config.fps, 30);
  assert_eq!(config.quality, VideoQuality::Medium);
  assert!(config.hardware_encoding);
  assert!(config.videos_path.is_none());
}

/// Test VideoConfig JSON round trip
#[tokio::test]
async fn test_video_config_json_round_trip() {
  let original = VideoConfig {
    fps: 60,
    quality: VideoQuality::Medium,
    hardware_encoding: false,
    audio: AudioConfig { enabled: true },
    ..Default::default()
  };

  let json = serde_json::to_string(&original).expect("Failed to serialize");
  let deserialized: VideoConfig = serde_json::from_str(&json).expect("Failed to deserialize");

  assert_eq!(deserialized.fps, original.fps);
  assert_eq!(deserialized.quality, original.quality);
  assert_eq!(deserialized.hardware_encoding, original.hardware_encoding);
  assert_eq!(deserialized.audio, original.audio);
}

/// Test VideoConfig cloning
#[tokio::test]
async fn test_video_config_clone() {
  let config = VideoConfig::default();
  let cloned = config.clone();

  assert_eq!(config.fps, cloned.fps);
  assert_eq!(config.quality, cloned.quality);
}

/// Test VideoConfig with custom FPS values
#[tokio::test]
async fn test_video_config_custom_fps() {
  let fps_values = vec![15, 24, 30, 60, 120];

  for fps in fps_values {
    let config = VideoConfig {
      fps,
      ..Default::default()
    };

    assert_eq!(config.fps, fps);
  }
}

/// Test VideoConfig with all quality presets
#[tokio::test]
async fn test_video_config_all_qualities() {
  let qualities = vec![VideoQuality::Low, VideoQuality::Medium, VideoQuality::High];

  for quality in qualities {
    let config = VideoConfig {
      quality,
      ..Default::default()
    };

    assert_eq!(config.quality, quality);
  }
}

/// Test VideoQuality serialization
#[tokio::test]
async fn test_video_quality_serialization() {
  let quality = VideoQuality::High;

  let json = serde_json::to_string(&quality).expect("Failed to serialize");
  assert!(json.contains("high"));

  let deserialized: VideoQuality = serde_json::from_str(&json).expect("Failed to deserialize");
  assert_eq!(deserialized, quality);
}

/// Test VideoQuality all variants
#[tokio::test]
async fn test_video_quality_all_variants() {
  let low = VideoQuality::Low;
  let medium = VideoQuality::Medium;
  let high = VideoQuality::High;

  // All variants should be distinct
  assert_ne!(low, medium);
  assert_ne!(medium, high);
  assert_ne!(low, high);
}

/// Test VideoConfig serialization
#[tokio::test]
async fn test_video_config_serialization() {
  let config = VideoConfig::default();

  let json = serde_json::to_string_pretty(&config).expect("Failed to serialize");

  // Should contain key fields
  assert!(json.contains("fps"));
  assert!(json.contains("quality"));
  assert!(!json.contains("codec"));

  // Deserialize back
  let deserialized: VideoConfig = serde_json::from_str(&json).expect("Failed to deserialize");
  assert_eq!(deserialized.fps, config.fps);
}

/// Test VideoConfig with custom path
#[tokio::test]
async fn test_video_config_custom_path() {
  let ctx = TestContext::new();
  let custom_path = ctx.temp_dir_path().to_path_buf();
  let config = VideoConfig {
    videos_path: Some(custom_path.clone()),
    ..Default::default()
  };

  assert_eq!(config.videos_path, Some(custom_path));
}

/// Test VideoConfig hardware encoding toggle
#[tokio::test]
async fn test_video_config_hardware_encoding_toggle() {
  let mut config = VideoConfig::default();

  // Default is true
  assert!(config.hardware_encoding);

  // Toggle to false
  config.hardware_encoding = false;
  assert!(!config.hardware_encoding);

  // Toggle back to true
  config.hardware_encoding = true;
  assert!(config.hardware_encoding);
}

/// Test AudioConfig default
#[tokio::test]
async fn test_audio_config_default() {
  let audio_config = AudioConfig::default();

  // Default audio config should be valid
  let _ = format!("{:?}", audio_config);
}

/// Test VideoQuality Debug formatting
#[tokio::test]
async fn test_video_quality_debug() {
  let quality = VideoQuality::High;
  let debug_str = format!("{:?}", quality);

  assert!(debug_str.contains("High"));
}

/// Test VideoConfig Debug formatting
#[tokio::test]
async fn test_video_config_debug() {
  let config = VideoConfig::default();
  let debug_str = format!("{:?}", config);

  assert!(debug_str.contains("VideoConfig"));
  assert!(debug_str.contains("fps"));
}

/// Test VideoQuality copy trait
#[tokio::test]
async fn test_video_quality_copy() {
  let quality1 = VideoQuality::Medium;
  let quality2 = quality1; // Copy, not move

  // Both should still be accessible
  assert_eq!(quality1, VideoQuality::Medium);
  assert_eq!(quality2, VideoQuality::Medium);
}

/// Test VideoConfig with minimum FPS
#[tokio::test]
async fn test_video_config_minimum_fps() {
  let config = VideoConfig {
    fps: 1,
    ..Default::default()
  };

  assert_eq!(config.fps, 1);
}

/// Test VideoConfig with high FPS
#[tokio::test]
async fn test_video_config_high_fps() {
  let config = VideoConfig {
    fps: 240,
    ..Default::default()
  };

  assert_eq!(config.fps, 240);
}

/// Test VideoQuality serde lowercase
#[tokio::test]
async fn test_video_quality_serde_lowercase() {
  // Test that quality serializes to lowercase
  let quality = VideoQuality::High;
  let json = serde_json::to_string(&quality).expect("Failed to serialize");

  // Should be lowercase
  assert!(json.contains("high"));
}

/// Test VideoConfig with extreme values
#[tokio::test]
async fn test_video_config_extreme_values() {
  let mut config = VideoConfig {
    fps: u32::MAX,
    ..Default::default()
  };

  // Very high FPS
  assert_eq!(config.fps, u32::MAX);

  // Zero FPS (edge case)
  config.fps = 0;
  assert_eq!(config.fps, 0);
}

/// Test VideoQuality enum size
#[tokio::test]
async fn test_video_quality_enum_size() {
  use std::mem::size_of;

  // VideoQuality should be small (it's Copy)
  let size = size_of::<VideoQuality>();

  // Should fit in a few bytes
  assert!(size <= 8, "VideoQuality size is {} bytes", size);
}
