use std::path::{Path, PathBuf};

use crate::video::config::{AudioConfig, RecordingProfile, VideoConfig, VideoQuality};

pub const GIF_TEMP_MARKER: &str = ".flinch-gif-part";

pub fn build_gif_output_path(
  output_dir: &Path,
  filename_template: &str,
  app_name: Option<&str>,
) -> PathBuf {
  let filename_ctx = crate::filename::FilenameContext {
    capture_type: "gif",
    app_name,
    now: chrono::Local::now(),
  };
  let stem = crate::filename::render_template(filename_template, &filename_ctx);
  crate::capture_artifact::next_available_path(&output_dir.join(format!("{stem}.gif")), "gif")
}

pub fn build_intermediate_output_path(final_output_path: &Path) -> PathBuf {
  let parent =
    crate::capture_artifact::recording_session_temp_dir(final_output_path, "gif-recording", "gif");
  let stem = final_output_path
    .file_stem()
    .map(|stem| stem.to_string_lossy().to_string())
    .or_else(|| {
      final_output_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
    })
    .unwrap_or_else(|| "gif-recording".to_string());
  parent.join(format!("{stem}{GIF_TEMP_MARKER}.mp4"))
}

/// The intermediate video a GIF is encoded from: software-encoded at high quality, without audio,
/// so the palette pass sees the cleanest picture.
pub fn build_gif_video_config(output_dir: &Path, fps: u32) -> VideoConfig {
  VideoConfig {
    videos_path: Some(output_dir.to_path_buf()),
    fps,
    quality: VideoQuality::High,
    hardware_encoding: false,
    audio: AudioConfig { enabled: false },
    microphone: None,
    recording_profile: RecordingProfile::GifIntermediate,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn build_gif_output_path_uses_gif_extension() {
    let path = build_gif_output_path(Path::new(r"C:\captures"), "%TYPE%_%APP%", Some("Code"));
    assert_eq!(path, PathBuf::from(r"C:\captures\gif_Code.gif"));
  }

  #[test]
  fn build_intermediate_output_path_uses_mp4_sidecar() {
    let path = build_intermediate_output_path(Path::new(r"C:\captures\clip.gif"));

    assert!(path.starts_with(crate::capture_artifact::recording_temp_root()));
    assert_eq!(
      path.file_name().and_then(|name| name.to_str()),
      Some("clip.flinch-gif-part.mp4")
    );
  }

  #[test]
  fn gif_session_video_config_uses_gif_fps_and_disables_audio() {
    let config = build_gif_video_config(Path::new(r"C:\captures"), 15);

    assert_eq!(config.fps, 15);
    assert!(!config.hardware_encoding);
    assert!(!config.audio.enabled);
    assert_eq!(config.recording_profile, RecordingProfile::GifIntermediate);
  }
}
