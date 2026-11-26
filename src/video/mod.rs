//! Video recording: capture plans, recorder configuration and the bridge to the native video
//! engine, which captures, encodes and writes every recording.
pub mod capture;
pub mod config;
pub mod controller;
pub mod native;
pub mod recovery;

/// Re-export common types for convenience
pub type Result<T> = std::result::Result<T, VideoError>;

/// Video recording errors
#[derive(Debug, thiserror::Error)]
pub enum VideoError {
  #[error("Capture failed: {0}")]
  CaptureFailed(String),

  #[error("Encoding failed: {0}")]
  EncodingFailed(String),

  #[error("Configuration error: {0}")]
  ConfigError(String),

  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),

  #[error("Recording already in progress")]
  AlreadyRecording,

  #[error("No recording in progress")]
  NotRecording,
}
