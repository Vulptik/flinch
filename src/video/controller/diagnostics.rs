use crate::video::{VideoError, capture::CaptureConfig};

use super::{RecordingStatus, STATUS_ERROR, VideoRecorder};

pub(super) fn check_disk_space(
  recorder: &VideoRecorder,
  output_path: &std::path::Path,
  capture_config: &CaptureConfig,
) -> crate::video::Result<()> {
  let output_dir = output_path
    .parent()
    .ok_or_else(|| VideoError::ConfigError("Invalid output path".into()))?;

  let available_space = free_space(output_dir)
    .map_err(|e| VideoError::ConfigError(format!("Cannot check disk space: {e}")))?;

  const MIN_REQUIRED_SPACE: u64 = 1024 * 1024 * 1024;
  if available_space < MIN_REQUIRED_SPACE {
    return Err(VideoError::ConfigError(format!(
      "Insufficient disk space: {} MB available, need at least 1 GB to start recording",
      available_space / (1024 * 1024)
    )));
  }

  let estimated_bitrate_mbps = estimate_bitrate(recorder, capture_config);
  let estimated_max_duration_secs = (available_space * 8 / 1024 / 1024) / estimated_bitrate_mbps;

  tracing::info!(
    operation = "check_disk_space",
    available_gb = available_space / (1024 * 1024 * 1024),
    estimated_bitrate_mbps,
    estimated_max_duration_min = estimated_max_duration_secs / 60,
    "Starting recording with sufficient disk space"
  );

  Ok(())
}

/// Bytes available to this user on the volume holding `dir`, honoring disk quotas.
fn free_space(dir: &std::path::Path) -> windows::core::Result<u64> {
  use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::HSTRING};
  let mut available = 0u64;
  // SAFETY: the path is a valid null-terminated wide string and the out pointer lives for the call.
  unsafe {
    GetDiskFreeSpaceExW(
      &HSTRING::from(dir.as_os_str()),
      Some(&mut available),
      None,
      None,
    )?
  };
  Ok(available)
}

/// Worst-case video bits per pixel (in thousandths) at each constant QP: a racing game through the
/// software encoder, the largest case of the `flinch-video-lab reencode` sweep.
/// Desktop and text recordings take a small fraction of this.
fn worst_case_millibits_per_pixel(qp: u8) -> u64 {
  match qp {
    0..=18 => 1_127,
    19..=23 => 624,
    24..=26 => 424,
    _ => 251,
  }
}

/// Worst-case recording bitrate in Mbit/s, rounded up, for the disk check's duration estimate.
pub(super) fn estimate_bitrate(recorder: &VideoRecorder, capture_config: &CaptureConfig) -> u64 {
  let pixels = u64::from(capture_config.width) * u64::from(capture_config.height);
  let rate = pixels * u64::from(capture_config.fps);
  let video_mbps =
    (rate * worst_case_millibits_per_pixel(recorder.config.qp())).div_ceil(1_000_000_000);
  // The engine muxes 192 kbit/s AAC; two megabits keep the estimate conservative.
  let audio_bitrate_mbps = if recorder.config.audio_track() { 2 } else { 0 };
  (video_mbps + audio_bitrate_mbps).clamp(2, 1_000)
}

pub(super) fn status(recorder: &mut VideoRecorder) -> RecordingStatus {
  super::native::poll(recorder);
  if recorder
    .status_atomic
    .load(std::sync::atomic::Ordering::Acquire)
    == STATUS_ERROR
  {
    return RecordingStatus::Error;
  }

  recorder.status
}

#[cfg(test)]
mod free_space_tests {
  use super::free_space;

  #[test]
  fn reports_space_on_an_existing_directory() -> windows::core::Result<()> {
    let dir = std::env::temp_dir();
    assert!(free_space(&dir)? > 0);
    Ok(())
  }

  #[test]
  fn fails_for_a_missing_directory() {
    let missing = std::env::temp_dir()
      .join("flinch-no-such-dir")
      .join("nested");
    assert!(free_space(&missing).is_err());
  }
}
