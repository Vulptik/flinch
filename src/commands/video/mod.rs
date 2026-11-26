//! Video recording commands
//!
//! ## Purpose
//! Provides typed operations for video recording including:
//! - Start/stop recording
//! - Pause/resume recording
//! - Status queries
//!
//! ## Module Structure
//! - `start` - Start recording commands
//! - `stop` - Stop recording commands
//! - `control` - Pause/resume commands
//! - `status` - Status queries and flag monitors

pub mod control;
pub mod start;
pub(crate) mod startup;
pub mod status;
pub mod stop;

#[cfg(test)]
mod service_tests;

// Re-export RecordingMode for convenience
pub use crate::video::capture::RecordingMode;

pub use crate::commands::capture_finalize::RecordingLifecycleStatus;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordingStartResponse {
  pub status: RecordingLifecycleStatus,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub path: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub source_app: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordingStopResponse {
  pub status: RecordingLifecycleStatus,
  pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordingStatusResponse {
  pub status: RecordingLifecycleStatus,
  pub duration_ms: u64,
  pub frames_captured: u64,
  pub frames_encoded: u64,
  pub frames_dropped: u64,
  pub file_size_bytes: u64,
}
