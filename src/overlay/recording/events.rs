// Event type definitions for overlay system

/// Recording status enum (mirrored from video::controller for overlay thread)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingStatus {
  Idle,
  Starting,
  Recording,
  Paused,
  Stopping,
  Error,
}
