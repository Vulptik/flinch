//! Video recording controller.
//!
//! The recorder is a thin session facade around the native video engine's worker. Flinch owns
//! planning, HUD/orchestration and file lifecycle; the worker owns capture, encoding and the MP4.

mod diagnostics;
mod native;
#[cfg(test)]
mod native_desktop_tests;
#[cfg(test)]
mod native_pipeline_tests;
#[cfg(test)]
mod native_tests;
#[cfg(test)]
mod scripted;
#[cfg(test)]
pub(crate) use scripted::ScriptedSession;
mod state;

pub use native::prewarm;
pub use state::{RecordingStats, RecordingStatus};

use crate::video::{Result, capture::CaptureConfig, config::VideoConfig};
use flinch_video_engine::gif::GifOptions;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// Recording status code used for cross-thread failure signaling.
const STATUS_ERROR: u8 = 5;

const STATUS_IDLE: u8 = 0;
const STATUS_STARTING: u8 = 1;
const STATUS_RECORDING: u8 = 2;
const STATUS_PAUSED: u8 = 3;
const STATUS_STOPPING: u8 = 4;

#[derive(Default)]
pub(super) struct RecordingRuntime {
  pub capture_config: Option<CaptureConfig>,
  pub temp_output_path: Option<PathBuf>,
  pub final_output_path: Option<PathBuf>,
  pub gif_options: Option<GifOptions>,
  pub last_error: Option<String>,
  native: Option<native::NativeRecording>,
}

impl RecordingRuntime {
  fn clear(&mut self) {
    *self = Self::default();
  }
}

/// Worker-owned stages mirrored atomically for nonblocking saving feedback.
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum FinalizationStage {
  Closing,
  Checking,
  Saving,
}

/// Why the last start failed, when the failure notification can say more than "see the logs".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StartFailure {
  #[default]
  Other,
  /// No capture backend can record in this session.
  CaptureUnavailable,
  /// Windows' privacy settings block the microphone.
  MicrophoneBlocked,
  /// The chosen microphone (or any, for the default) is not connected.
  MicrophoneMissing,
}
impl StartFailure {
  pub fn of(error: &flinch_video_engine::EngineError) -> Self {
    use flinch_video_engine::types::{MICROPHONE_BLOCKED_STAGE, MICROPHONE_MISSING_STAGE};
    if error.capture_unavailable() {
      Self::CaptureUnavailable
    } else if error.stage == MICROPHONE_BLOCKED_STAGE {
      Self::MicrophoneBlocked
    } else if error.stage == MICROPHONE_MISSING_STAGE {
      Self::MicrophoneMissing
    } else {
      Self::Other
    }
  }
  /// The notification body that explains it.
  pub fn body(self) -> &'static str {
    match self {
      Self::Other => "notification_error_recording",
      Self::CaptureUnavailable => "notification_error_capture_unavailable",
      Self::MicrophoneBlocked => "notification_error_microphone_blocked",
      Self::MicrophoneMissing => "notification_error_microphone_missing",
    }
  }
}

/// The recording's microphone, as the recording controls show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrophoneState {
  Audible,
  Muted,
  /// It disappeared mid-recording; the rest records no microphone.
  Lost,
}

/// Thin recorder facade around one native engine session.
pub struct VideoRecorder {
  config: VideoConfig,
  status: RecordingStatus,
  status_atomic: Arc<AtomicU8>,
  runtime: RecordingRuntime,
  finalization_stage: Arc<AtomicU8>,
  pending_gif_options: Option<GifOptions>,
  cancellation: Arc<std::sync::atomic::AtomicBool>,
  /// Why the last start failed.
  start_failure: StartFailure,
  /// The last finished recording's microphone disappeared during it.
  microphone_lost: bool,
  /// Replaces the worker, file check and GIF writer in command and overlay tests.
  #[cfg(test)]
  scripted: Option<scripted::Hooks>,
}

impl VideoRecorder {
  /// Whether the last start failed because screen capture is unavailable in this session, so the
  /// failure notification can say so instead of pointing at the logs.
  pub fn capture_unavailable(&self) -> bool {
    self.start_failure == StartFailure::CaptureUnavailable
  }
  pub fn start_failure(&self) -> StartFailure {
    self.start_failure
  }
  pub fn finalization_progress(&self) -> Arc<AtomicU8> {
    self.finalization_stage.clone()
  }
  fn finalization_stage(&self, stage: FinalizationStage) {
    self
      .finalization_stage
      .store(stage as u8, Ordering::Release);
    tracing::info!(?stage, "Recording finalization stage");
  }

  pub fn new(config: VideoConfig) -> Self {
    Self {
      config,
      status: RecordingStatus::Idle,
      status_atomic: Arc::new(AtomicU8::new(STATUS_IDLE)),
      runtime: RecordingRuntime::default(),
      finalization_stage: Arc::new(AtomicU8::new(FinalizationStage::Closing as u8)),
      pending_gif_options: None,
      cancellation: Arc::new(std::sync::atomic::AtomicBool::new(false)),
      start_failure: StartFailure::Other,
      microphone_lost: false,
      #[cfg(test)]
      scripted: None,
    }
  }

  pub fn start_recording(
    &mut self,
    capture_config: CaptureConfig,
    output_path: PathBuf,
    final_output_path: PathBuf,
  ) -> Result<()> {
    if self.status != RecordingStatus::Idle {
      return Err(crate::video::VideoError::AlreadyRecording);
    }
    self.start_failure = StartFailure::Other;
    self.microphone_lost = false;
    self.cancellation.store(false, Ordering::Release);
    let temp_output_dir = output_path.parent().ok_or_else(|| {
      crate::video::VideoError::ConfigError("Invalid recording temp output path".to_string())
    })?;
    std::fs::create_dir_all(temp_output_dir).map_err(crate::video::VideoError::Io)?;
    diagnostics::check_disk_space(self, &output_path, &capture_config)?;
    if output_path.parent() != final_output_path.parent() {
      diagnostics::check_disk_space(self, &final_output_path, &capture_config)?;
    }
    #[cfg(test)]
    if let Some(launch) = self.scripted.as_ref().map(scripted::Hooks::launcher) {
      return native::start(
        self,
        capture_config,
        output_path,
        final_output_path,
        &scripted::displays(),
        launch,
      );
    }
    native::start(
      self,
      capture_config,
      output_path,
      final_output_path,
      &crate::monitors::list_monitors(),
      native::standby_or_launch,
    )
  }

  pub fn stop_recording(&mut self) -> Result<PathBuf> {
    native::stop(self)
  }

  /// The running recording's microphone; `None` when it records none.
  pub fn microphone(&mut self) -> Option<MicrophoneState> {
    native::microphone(self)
  }
  /// Mutes or unmutes the running recording's microphone from now.
  pub fn set_microphone(&mut self, audible: bool) -> Result<()> {
    native::set_microphone(self, audible)
  }
  /// Whether the last finished recording lost its microphone; reported once.
  pub fn take_microphone_lost(&mut self) -> bool {
    std::mem::take(&mut self.microphone_lost)
  }

  pub fn pause_recording(&mut self) -> Result<()> {
    native::pause(self)
  }

  pub fn resume_recording(&mut self) -> Result<()> {
    native::resume(self)
  }

  pub fn status(&mut self) -> RecordingStatus {
    diagnostics::status(self)
  }

  pub fn get_elapsed_time(&mut self) -> Duration {
    native::elapsed(self)
  }

  pub fn stats(&mut self) -> RecordingStats {
    native::stats(self)
  }

  #[cfg(test)]
  pub fn config(&self) -> &VideoConfig {
    &self.config
  }

  pub fn set_config(&mut self, config: VideoConfig) -> Result<()> {
    if self.status != RecordingStatus::Idle {
      return Err(crate::video::VideoError::AlreadyRecording);
    }
    self.config = config;
    Ok(())
  }

  pub fn set_gif_options(&mut self, options: Option<GifOptions>) -> Result<()> {
    if self.status != RecordingStatus::Idle {
      return Err(crate::video::VideoError::AlreadyRecording);
    }
    self.pending_gif_options = options;
    Ok(())
  }

  pub(super) fn set_status(&mut self, status: RecordingStatus) {
    self.status = status;
    self
      .status_atomic
      .store(recording_status_code(status), Ordering::SeqCst);
  }
}

impl Drop for VideoRecorder {
  fn drop(&mut self) {
    // A recording still owned at drop is finished and published rather than abandoned.
    if !matches!(
      self.status,
      RecordingStatus::Recording | RecordingStatus::Paused | RecordingStatus::Stopping
    ) {
      return;
    }
    if let Err(error) = self.stop_recording() {
      tracing::warn!(
        operation = "drop_video_recorder",
        error = %error,
        "Recorder drop cleanup did not complete cleanly"
      );
    }
  }
}

pub(super) fn recording_status_code(status: RecordingStatus) -> u8 {
  match status {
    RecordingStatus::Idle => STATUS_IDLE,
    RecordingStatus::Starting => STATUS_STARTING,
    RecordingStatus::Recording => STATUS_RECORDING,
    RecordingStatus::Paused => STATUS_PAUSED,
    RecordingStatus::Stopping => STATUS_STOPPING,
    RecordingStatus::Error => STATUS_ERROR,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::video::{
    VideoError,
    config::{AudioConfig, VideoQuality},
  };

  fn make_recorder() -> VideoRecorder {
    VideoRecorder::new(VideoConfig::default())
  }

  #[test]
  fn new_recorder_starts_idle() {
    let mut recorder = make_recorder();
    assert_eq!(recorder.status(), RecordingStatus::Idle);
    assert_eq!(recorder.get_elapsed_time(), Duration::ZERO);
  }

  #[test]
  fn idle_stats_are_zeroed() {
    let mut recorder = make_recorder();
    let stats = recorder.stats();
    assert_eq!(stats.status, RecordingStatus::Idle);
    assert_eq!(stats.duration_ms, 0);
    assert_eq!(stats.frames_captured, 0);
    assert_eq!(stats.frames_encoded, 0);
    assert_eq!(stats.frames_dropped, 0);
  }

  #[test]
  fn idle_state_rejects_stop_pause_and_resume() {
    let mut recorder = make_recorder();
    assert!(matches!(
      recorder.stop_recording(),
      Err(VideoError::NotRecording)
    ));
    assert!(matches!(
      recorder.pause_recording(),
      Err(VideoError::NotRecording)
    ));
    assert!(matches!(
      recorder.resume_recording(),
      Err(VideoError::ConfigError(_))
    ));
  }

  #[test]
  fn config_and_gif_options_change_only_while_idle() {
    let mut recorder = make_recorder();
    let new_config = VideoConfig {
      fps: 60,
      quality: VideoQuality::Low,
      ..VideoConfig::default()
    };
    recorder.set_config(new_config.clone()).unwrap();
    assert_eq!(recorder.config().fps, 60);
    assert_eq!(recorder.config().quality, VideoQuality::Low);

    recorder.set_status(RecordingStatus::Recording);
    let error = recorder.set_config(new_config).unwrap_err();
    assert_eq!(error.to_string(), VideoError::AlreadyRecording.to_string());
    let error = recorder
      .set_gif_options(Some(crate::video::native::gif_options()))
      .unwrap_err();
    assert_eq!(error.to_string(), VideoError::AlreadyRecording.to_string());
    let error = recorder
      .start_recording(
        CaptureConfig::fullscreen(64, 64, 30),
        PathBuf::from("unused.mp4"),
        PathBuf::from("unused-final.mp4"),
      )
      .unwrap_err();
    assert_eq!(error.to_string(), VideoError::AlreadyRecording.to_string());
    recorder.set_status(RecordingStatus::Idle);
  }

  #[test]
  fn bitrate_estimation_follows_quality_and_adds_audio_headroom() {
    let estimate = |quality, audio, profile| {
      let recorder = VideoRecorder::new(VideoConfig {
        quality,
        audio: AudioConfig { enabled: audio },
        recording_profile: profile,
        ..VideoConfig::default()
      });
      diagnostics::estimate_bitrate(&recorder, &CaptureConfig::fullscreen(2560, 1440, 30))
    };
    use crate::video::config::{RecordingProfile::*, VideoQuality::*};
    // The sweep's worst case at 1440p30: 69.0, 46.9 and 27.8 Mbit/s, rounded up.
    assert_eq!(estimate(High, false, Standard), 70);
    assert_eq!(estimate(Medium, false, Standard), 47);
    assert_eq!(estimate(Low, false, Standard), 28);
    assert_eq!(estimate(High, true, Standard), 72);
    // A GIF intermediate keeps QP 18 whatever the quality setting.
    assert_eq!(estimate(Low, false, GifIntermediate), 125);
    let recorder = VideoRecorder::new(VideoConfig::default());
    let tiny = CaptureConfig::fullscreen(64, 64, 15);
    assert_eq!(diagnostics::estimate_bitrate(&recorder, &tiny), 2, "floor");
  }
}
