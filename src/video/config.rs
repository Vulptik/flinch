/// Video recording configuration types
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Video recording configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
  /// Output directory for video files
  pub videos_path: Option<PathBuf>,

  /// Target frames per second
  pub fps: u32,

  /// Video quality preset
  pub quality: VideoQuality,

  /// Prefer a hardware encoder; the engine falls back to software when none initializes
  pub hardware_encoding: bool,

  /// System audio recording
  pub audio: AudioConfig,

  /// A microphone mixed into the audio track; `None` records none
  #[serde(default)]
  pub microphone: Option<MicrophoneConfig>,

  /// Whether this recording is a GIF's intermediate video
  #[serde(default)]
  pub recording_profile: RecordingProfile,
}

impl VideoConfig {
  /// Whether the recording has an audio track: system audio, a microphone or both share one,
  /// as the engine's `SessionSpec::audio_track` decides.
  pub fn audio_track(&self) -> bool {
    self.audio.enabled || self.microphone.is_some()
  }
}

impl Default for VideoConfig {
  fn default() -> Self {
    Self {
      videos_path: None, // Will use platform default (~/Videos)
      fps: 30,
      quality: VideoQuality::Medium,
      hardware_encoding: true,
      audio: AudioConfig::default(),
      microphone: None,
      recording_profile: RecordingProfile::Standard,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingProfile {
  #[default]
  Standard,
  GifIntermediate,
}

/// Video quality preset
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoQuality {
  Low,
  Medium,
  High,
}

impl VideoQuality {
  /// Constant encoder QP (quantization parameter) for every encoder; lower is better quality.
  /// Chosen from a `flinch-video-lab reencode` sweep of desktop, text and game footage:
  /// High roughly halves game footage
  /// against the former 18; desktop and text stay small at every level.
  pub fn hw_qp(&self) -> u8 {
    match self {
      VideoQuality::Low => 30,
      VideoQuality::Medium => 26,
      VideoQuality::High => 23,
    }
  }
}

/// A GIF's intermediate video is temporary and only feeds the palette and dithering passes, so it
/// keeps the cleanest picture regardless of the video quality setting: compression noise would
/// otherwise show up as changed pixels in every GIF frame.
pub const GIF_INTERMEDIATE_QP: u8 = 18;

impl VideoConfig {
  /// The constant QP this recording is encoded with.
  pub fn qp(&self) -> u8 {
    match self.recording_profile {
      RecordingProfile::Standard => self.quality.hw_qp(),
      RecordingProfile::GifIntermediate => GIF_INTERMEDIATE_QP,
    }
  }
}

/// The microphone a recording opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MicrophoneConfig {
  /// Endpoint id; `None` is the Windows default microphone
  pub device: Option<String>,
}

/// Audio recording configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AudioConfig {
  pub enabled: bool,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_video_config_default() {
    let config = VideoConfig::default();
    assert_eq!(config.videos_path, None);
    assert_eq!(config.fps, 30);
    assert_eq!(config.quality, VideoQuality::Medium);
    assert!(config.hardware_encoding);
    assert!(!config.audio.enabled);
    assert_eq!(config.recording_profile, RecordingProfile::Standard);
  }

  #[test]
  fn test_video_quality_hw_qp() {
    assert_eq!(VideoQuality::Low.hw_qp(), 30);
    assert_eq!(VideoQuality::Medium.hw_qp(), 26);
    assert_eq!(VideoQuality::High.hw_qp(), 23);
  }

  #[test]
  fn gif_intermediates_keep_their_own_qp_whatever_the_quality_setting() {
    for quality in [VideoQuality::Low, VideoQuality::Medium, VideoQuality::High] {
      let mut config = VideoConfig {
        quality,
        ..VideoConfig::default()
      };
      assert_eq!(config.qp(), quality.hw_qp());
      config.recording_profile = RecordingProfile::GifIntermediate;
      assert_eq!(config.qp(), GIF_INTERMEDIATE_QP);
    }
  }

  #[test]
  fn test_video_quality_serialization() {
    let quality = VideoQuality::High;
    let json = serde_json::to_string(&quality).unwrap();
    assert_eq!(json, r#""high""#);

    let deserialized: VideoQuality = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, VideoQuality::High);
  }

  #[test]
  fn a_configuration_written_with_the_retired_codec_field_still_loads() {
    let config: VideoConfig = serde_json::from_str(
      r#"{"videos_path":null,"codec":"h264_nvenc","fps":60,"quality":"medium",
          "hardware_encoding":false,"audio":{"enabled":true}}"#,
    )
    .unwrap();
    assert_eq!(config.fps, 60);
    assert_eq!(config.quality, VideoQuality::Medium);
    assert!(config.audio.enabled);
    assert_eq!(config.recording_profile, RecordingProfile::Standard);
  }

  #[test]
  fn test_audio_config_equality() {
    let audio1 = AudioConfig { enabled: true };
    let audio2 = AudioConfig { enabled: true };
    let audio3 = AudioConfig { enabled: false };

    assert_eq!(audio1, audio2);
    assert_ne!(audio1, audio3);
  }

  #[test]
  fn test_video_config_custom() {
    let config = VideoConfig {
      videos_path: Some(PathBuf::from("/tmp/videos")),
      fps: 60,
      quality: VideoQuality::Medium,
      hardware_encoding: false,
      audio: AudioConfig { enabled: true },
      microphone: None,
      recording_profile: RecordingProfile::GifIntermediate,
    };

    assert_eq!(config.fps, 60);
    assert_eq!(config.quality, VideoQuality::Medium);
    assert!(!config.hardware_encoding);
    assert!(config.audio.enabled);
    assert_eq!(config.recording_profile, RecordingProfile::GifIntermediate);
  }
}
