//! Recording state types
//!
//! ## Purpose
//! Defines the recording status enum and statistics struct used to
//! track the state of video recording.

/// Recording status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingStatus {
  Idle,
  Starting,
  Recording,
  Paused,
  Stopping,
  #[allow(dead_code)]
  Error,
}

/// Recording statistics
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RecordingStats {
  pub status: RecordingStatus,
  pub duration_ms: u64,
  /// Distinct decoded input frame indices retained by CFR scheduling, not native capture attempts.
  pub frames_captured: u64,
  pub frames_encoded: u64,
  /// Input indices skipped by CFR scheduling; this is not a native capture-error counter.
  pub frames_dropped: u64,
  pub file_size_bytes: u64,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_recording_status_equality() {
    assert_eq!(RecordingStatus::Idle, RecordingStatus::Idle);
    assert_eq!(RecordingStatus::Recording, RecordingStatus::Recording);
    assert_ne!(RecordingStatus::Idle, RecordingStatus::Recording);
  }

  #[test]
  fn test_recording_status_state_transitions() {
    // Valid state transition sequence
    let states = [
      RecordingStatus::Idle,
      RecordingStatus::Starting,
      RecordingStatus::Recording,
      RecordingStatus::Paused,
      RecordingStatus::Recording,
      RecordingStatus::Stopping,
      RecordingStatus::Idle,
    ];

    // Verify each state is distinct
    for (i, state) in states.iter().enumerate() {
      for (j, other_state) in states.iter().enumerate() {
        if i == j {
          assert_eq!(state, other_state);
        }
      }
    }
  }

  #[test]
  fn test_recording_stats_initialization() {
    let stats = RecordingStats {
      status: RecordingStatus::Recording,
      duration_ms: 5000,
      frames_captured: 150,
      frames_encoded: 148,
      frames_dropped: 2,
      file_size_bytes: 1024000,
    };

    assert_eq!(stats.status, RecordingStatus::Recording);
    assert_eq!(stats.duration_ms, 5000);
    assert_eq!(stats.frames_captured, 150);
    assert_eq!(stats.frames_encoded, 148);
    assert_eq!(stats.frames_dropped, 2);
    assert_eq!(stats.file_size_bytes, 1024000);
  }

  #[test]
  fn test_recording_stats_clone() {
    let stats = RecordingStats {
      status: RecordingStatus::Paused,
      duration_ms: 10000,
      frames_captured: 300,
      frames_encoded: 300,
      frames_dropped: 0,
      file_size_bytes: 2048000,
    };

    let cloned_stats = stats.clone();
    assert_eq!(stats.status, cloned_stats.status);
    assert_eq!(stats.duration_ms, cloned_stats.duration_ms);
    assert_eq!(stats.frames_captured, cloned_stats.frames_captured);
  }

  #[test]
  fn test_recording_stats_frame_drop_calculation() {
    let stats = RecordingStats {
      status: RecordingStatus::Recording,
      duration_ms: 1000,
      frames_captured: 60,
      frames_encoded: 58,
      frames_dropped: 2,
      file_size_bytes: 512000,
    };

    // Verify drop rate calculation is correct
    let drop_rate = (stats.frames_dropped as f64 / stats.frames_captured as f64) * 100.0;
    assert!((drop_rate - 3.33).abs() < 0.1); // Approximately 3.33%
  }

  #[test]
  fn test_recording_stats_zero_values() {
    let stats = RecordingStats {
      status: RecordingStatus::Idle,
      duration_ms: 0,
      frames_captured: 0,
      frames_encoded: 0,
      frames_dropped: 0,
      file_size_bytes: 0,
    };

    assert_eq!(stats.status, RecordingStatus::Idle);
    assert_eq!(stats.duration_ms, 0);
    assert_eq!(stats.frames_captured, 0);
  }

  #[test]
  fn test_recording_status_debug_format() {
    let status = RecordingStatus::Recording;
    let debug_str = format!("{:?}", status);
    assert_eq!(debug_str, "Recording");
  }

  #[test]
  fn test_recording_status_copy() {
    let status1 = RecordingStatus::Paused;
    let status2 = status1; // Copy (not move)
    assert_eq!(status1, status2);
    // status1 is still valid after copy
    assert_eq!(status1, RecordingStatus::Paused);
  }
}
