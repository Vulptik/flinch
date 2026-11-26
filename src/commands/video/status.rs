//! Video recording status queries and monitors
//!
//! ## Purpose
//! Provides status information and background monitors:
//! - `get_recording_status` - Query current recording state and stats

use crate::app::context::AppHandle;
use crate::commands::VideoRecorderState;
use crate::video::controller::{RecordingStats, RecordingStatus};

fn build_recording_status_payload(
  stats: &RecordingStats,
  status: RecordingStatus,
) -> crate::commands::video::RecordingStatusResponse {
  crate::commands::video::RecordingStatusResponse {
    status: match status {
      RecordingStatus::Idle => crate::commands::video::RecordingLifecycleStatus::Idle,
      RecordingStatus::Starting => crate::commands::video::RecordingLifecycleStatus::Processing,
      RecordingStatus::Recording | RecordingStatus::Paused => {
        crate::commands::video::RecordingLifecycleStatus::Recording
      },
      RecordingStatus::Stopping => crate::commands::video::RecordingLifecycleStatus::Processing,
      RecordingStatus::Error => crate::commands::video::RecordingLifecycleStatus::Idle,
    },
    duration_ms: stats.duration_ms,
    frames_captured: stats.frames_captured,
    frames_encoded: stats.frames_encoded,
    frames_dropped: stats.frames_dropped,
    file_size_bytes: stats.file_size_bytes,
  }
}

pub fn get_recording_status(
  app: AppHandle,
) -> Result<crate::commands::video::RecordingStatusResponse, String> {
  let recorder_state = app.state::<VideoRecorderState>();
  let mut recorder = match recorder_state.inner().0.try_lock() {
    Ok(recorder) => recorder,
    Err(std::sync::TryLockError::WouldBlock) => {
      return Ok(build_recording_status_payload(
        &RecordingStats {
          status: RecordingStatus::Stopping,
          duration_ms: 0,
          frames_captured: 0,
          frames_encoded: 0,
          frames_dropped: 0,
          file_size_bytes: 0,
        },
        RecordingStatus::Stopping,
      ));
    },
    Err(error) => {
      return Err(crate::commands::ipc_display_context(
        "Video recorder state poisoned",
        error,
      ));
    },
  };

  let stats = recorder.stats();
  Ok(build_recording_status_payload(&stats, recorder.status()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_recording_status_payload_contains_expected_fields() {
    let stats = RecordingStats {
      status: RecordingStatus::Recording,
      duration_ms: 1250,
      frames_captured: 72,
      frames_encoded: 70,
      frames_dropped: 2,
      file_size_bytes: 4096,
    };

    let payload = build_recording_status_payload(&stats, RecordingStatus::Paused);

    assert!(matches!(
      payload.status,
      crate::commands::video::RecordingLifecycleStatus::Recording
    ));
    assert_eq!(payload.duration_ms, 1250);
    assert_eq!(payload.frames_captured, 72);
    assert_eq!(payload.frames_encoded, 70);
    assert_eq!(payload.frames_dropped, 2);
    assert_eq!(payload.file_size_bytes, 4096);
  }
}
