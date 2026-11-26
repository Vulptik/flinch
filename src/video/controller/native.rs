//! Native engine backend: one hosted worker session per recording.
//!
//! The worker owns capture, encoding and the MP4 writer; this side only maps the plan, forwards
//! controls, surfaces failures and publishes the finished file. Every control is bounded by the
//! host's deadlines, and every failure is reported with the engine's cause and diagnostics.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use flinch_video_engine::EngineError;
use flinch_video_engine::recovery::Inspection;
use flinch_video_engine::types::{OutcomeKind, RecordingOutcome, SessionSnapshot, SessionSpec};
use flinch_video_engine::windows::host::HostSession;

use super::{RecordingRuntime, RecordingStats, RecordingStatus, VideoRecorder};
use crate::video::config::RecordingProfile;
use crate::video::{Result, VideoError, capture::CaptureConfig};

/// Windows `ERROR_NOT_SAME_DEVICE`: the temporary session and the destination are on different volumes.
const NOT_SAME_DEVICE: i32 = 17;

/// The hosted worker as the recorder sees it; tests substitute a scripted session.
pub(super) trait Host: Send {
  fn prepare(&self, spec: SessionSpec) -> std::result::Result<PathBuf, EngineError>;
  fn start(&self) -> std::result::Result<(), EngineError>;
  fn pause(&self) -> std::result::Result<(), EngineError>;
  fn resume(&self) -> std::result::Result<(), EngineError>;
  /// Mutes or unmutes the recording's microphone; doubles without one refuse.
  fn microphone(&self, _audible: bool) -> std::result::Result<(), EngineError> {
    Err(EngineError::new(
      flinch_video_engine::ErrorKind::InvalidState,
      "microphone",
      "This session has no microphone",
    ))
  }
  fn stop(&self) -> std::result::Result<RecordingOutcome, EngineError>;
  fn snapshot(&self) -> SessionSnapshot;
  fn failure(&self) -> Option<EngineError>;
  fn terminal_outcome(&self) -> Option<RecordingOutcome>;
  fn take_diagnostics(&self) -> (Vec<String>, u64);
}

impl Host for HostSession {
  fn prepare(&self, spec: SessionSpec) -> std::result::Result<PathBuf, EngineError> {
    HostSession::prepare(self, spec)
  }
  fn start(&self) -> std::result::Result<(), EngineError> {
    HostSession::start(self)
  }
  fn pause(&self) -> std::result::Result<(), EngineError> {
    HostSession::pause(self)
  }
  fn resume(&self) -> std::result::Result<(), EngineError> {
    HostSession::resume(self)
  }
  fn microphone(&self, audible: bool) -> std::result::Result<(), EngineError> {
    HostSession::microphone(self, audible)
  }
  fn stop(&self) -> std::result::Result<RecordingOutcome, EngineError> {
    HostSession::stop(self)
  }
  fn snapshot(&self) -> SessionSnapshot {
    HostSession::snapshot(self)
  }
  fn failure(&self) -> Option<EngineError> {
    HostSession::failure(self)
  }
  fn terminal_outcome(&self) -> Option<RecordingOutcome> {
    HostSession::terminal_outcome(self)
  }
  fn take_diagnostics(&self) -> (Vec<String>, u64) {
    HostSession::take_diagnostics(self)
  }
}

/// Launch a worker from this executable with the engine's worker switch.
pub(super) fn launch_worker() -> std::result::Result<Box<dyn Host>, EngineError> {
  let executable =
    std::env::current_exe().map_err(|e| EngineError::io("video worker executable", e))?;
  Ok(Box::new(HostSession::launch(
    &executable,
    &[crate::video::native::WORKER_SWITCH],
  )?))
}

/// Launch a standby worker that performs the one-time process warm-up while it waits.
pub(super) fn launch_standby_worker() -> std::result::Result<Box<dyn Host>, EngineError> {
  let executable =
    std::env::current_exe().map_err(|e| EngineError::io("video worker executable", e))?;
  Ok(Box::new(HostSession::launch(
    &executable,
    &[
      crate::video::native::WORKER_SWITCH,
      flinch_video_engine::windows::runtime::WARM_ARGUMENT,
    ],
  )?))
}

/// At most one worker launched before the user asks to record. It holds no display, GPU device
/// or file: the recording's display and area are only known at Start. Taking it saves the
/// launch, hello and first-use DXGI/D3D initialization (about 50 ms measured).
pub(super) struct Standby {
  slot: Mutex<Slot>,
}
#[derive(Default)]
struct Slot {
  host: Option<Box<dyn Host>>,
  launching: bool,
}

impl Standby {
  pub(super) const fn new() -> Self {
    Self {
      slot: Mutex::new(Slot {
        host: None,
        launching: false,
      }),
    }
  }

  /// The waiting worker when it is still healthy. One whose heartbeat stopped (for example across
  /// system sleep) or that exited is dropped with its cause logged; the caller launches cold.
  pub(super) fn take(&self) -> Option<Box<dyn Host>> {
    let host = match self.slot.lock() {
      Ok(mut slot) => slot.host.take(),
      Err(_) => {
        tracing::error!("Native standby worker slot poisoned; launching a worker on demand");
        return None;
      },
    }?;
    match host.failure() {
      None => Some(host),
      Some(failure) => {
        tracing::warn!(error = %describe(host.as_ref(), &failure), "Native standby worker was lost; launching a worker on demand");
        None
      },
    }
  }

  #[cfg(test)]
  #[allow(clippy::panic)]
  pub(super) fn poison_for_test(&self) {
    let _held = self.slot.lock();
    panic!("poisoning the standby slot");
  }

  /// Launch one standby worker on this thread unless one is waiting or already launching.
  pub(super) fn fill_now(
    &self,
    launch: impl FnOnce() -> std::result::Result<Box<dyn Host>, EngineError>,
  ) {
    match self.slot.lock() {
      Ok(mut slot) if slot.host.is_none() && !slot.launching => slot.launching = true,
      Ok(_) => return,
      Err(_) => {
        tracing::error!("Native standby worker slot poisoned; no standby worker launched");
        return;
      },
    }
    let launched = launch();
    // The flag is cleared on every path, so a failed launch never blocks a later refill.
    let Ok(mut slot) = self.slot.lock() else {
      tracing::error!("Native standby worker slot poisoned; standby worker discarded");
      return;
    };
    slot.launching = false;
    match launched {
      Ok(host) => slot.host = Some(host),
      Err(error) => {
        tracing::warn!(%error, "Native standby worker launch failed; recordings launch on demand")
      },
    }
  }
}

static STANDBY: Standby = Standby::new();

/// Prepare a standby worker in the background. Called at application start and after every
/// recording.
pub fn prewarm() {
  // The launcher thread is detached: it is bounded by the hello deadline and fills only the slot.
  drop(prewarm_with(&STANDBY, launch_standby_worker));
}

/// Start the background fill; returns the launcher thread when one was started.
pub(super) fn prewarm_with(
  standby: &'static Standby,
  launch: fn() -> std::result::Result<Box<dyn Host>, EngineError>,
) -> Option<std::thread::JoinHandle<()>> {
  match std::thread::Builder::new()
    .name("video standby launcher".into())
    .spawn(move || standby.fill_now(launch))
  {
    Ok(thread) => Some(thread),
    Err(error) => {
      tracing::warn!(%error, "Native standby worker thread could not start");
      None
    },
  }
}

/// The standby worker when one is healthy, otherwise a worker launched now.
pub(super) fn standby_or_launch() -> std::result::Result<Box<dyn Host>, EngineError> {
  take_or_launch(&STANDBY, launch_worker)
}

pub(super) fn take_or_launch(
  standby: &Standby,
  launch: impl FnOnce() -> std::result::Result<Box<dyn Host>, EngineError>,
) -> std::result::Result<Box<dyn Host>, EngineError> {
  match standby.take() {
    Some(host) => Ok(host),
    None => launch(),
  }
}

pub(super) struct NativeRecording {
  host: Box<dyn Host>,
  session_directory: PathBuf,
  accumulated: Duration,
  active_since: Option<Instant>,
  /// Monotonic paused time and pause count, logged so a session's media length can be checked
  /// against the app's clock without trusting wall-clock log timestamps (they follow clock steps).
  paused: Duration,
  paused_since: Option<Instant>,
  pauses: u32,
  /// The recording's microphone: `None` without one, else whether it is audible.
  microphone: Option<bool>,
  /// Reads the published file's structure for the Stop check (the engine's parser; tests stub it).
  pub(super) inspect: fn(&Path) -> std::result::Result<Inspection, EngineError>,
}

impl NativeRecording {
  fn elapsed(&self) -> Duration {
    self.accumulated
      + self
        .active_since
        .map_or(Duration::ZERO, |since| since.elapsed())
  }
  fn paused(&self) -> Duration {
    self.paused
      + self
        .paused_since
        .map_or(Duration::ZERO, |since| since.elapsed())
  }
  /// Engine error text plus any worker diagnostics, so the cause is never only "it failed".
  fn describe(&self, error: &EngineError) -> String {
    describe(self.host.as_ref(), error)
  }
}

fn describe(host: &dyn Host, error: &EngineError) -> String {
  let mut message = error.to_string();
  if let Some(path) = &error.recoverable_path {
    message.push_str(&format!(". Recording data retained at {}", path.display()));
  }
  let (diagnostics, lost) = host.take_diagnostics();
  if !diagnostics.is_empty() {
    message.push_str(&format!(". Worker diagnostics: {}", diagnostics.join("; ")));
  }
  if lost > 0 {
    message.push_str(&format!(" ({lost} more dropped)"));
  }
  message
}

pub(super) fn start(
  recorder: &mut VideoRecorder,
  capture_config: CaptureConfig,
  output_path: PathBuf,
  final_output_path: PathBuf,
  monitors: &[crate::monitors::MonitorInfo],
  launch: impl FnOnce() -> std::result::Result<Box<dyn Host>, EngineError>,
) -> Result<()> {
  recorder.start_failure = super::StartFailure::Other;
  let spec =
    crate::video::native::session_spec(&recorder.config, &capture_config, monitors, &output_path)
      .map_err(VideoError::ConfigError)?;
  if recorder.config.recording_profile == RecordingProfile::GifIntermediate
    && recorder.pending_gif_options.is_none()
  {
    return Err(VideoError::ConfigError(
      "Missing GIF encode settings for a GIF recording".into(),
    ));
  }
  // Written before the worker exists, so any session that never publishes is recoverable.
  crate::video::recovery::write_marker(
    &output_path,
    &final_output_path,
    recorder.config.recording_profile == RecordingProfile::GifIntermediate,
  )
  .map_err(VideoError::Io)?;
  let started = Instant::now();
  tracing::info!(
    monitor = %spec.monitor,
    region = ?spec.region,
    capture = ?spec.capture,
    encoder = ?spec.encoder,
    fps = spec.fps,
    "Recording uses the native video engine"
  );
  recorder.runtime = RecordingRuntime {
    capture_config: Some(capture_config),
    temp_output_path: Some(output_path),
    final_output_path: Some(final_output_path),
    gif_options: recorder.pending_gif_options.take(),
    ..RecordingRuntime::default()
  };
  recorder.set_status(RecordingStatus::Starting);
  let host = match launch() {
    Ok(host) => host,
    Err(error) => {
      return Err(fail_start(
        recorder,
        format!("Native video worker launch failed: {error}"),
        false,
      ));
    },
  };
  // Dropping an unfinished host aborts its worker, so every failed step below cleans up.
  let session_directory = match host.prepare(spec) {
    Ok(directory) => directory,
    Err(error) => {
      let message = format!(
        "Native recording prepare failed: {}",
        describe(host.as_ref(), &error)
      );
      drop(host);
      recorder.start_failure = super::StartFailure::of(&error);
      return Err(fail_start(
        recorder,
        message,
        error.recoverable_path.is_some(),
      ));
    },
  };
  if let Err(error) = host.start() {
    let message = format!(
      "Native recording start failed: {}",
      describe(host.as_ref(), &error)
    );
    drop(host);
    return Err(fail_start(
      recorder,
      message,
      error.recoverable_path.is_some(),
    ));
  }
  recorder.runtime.native = Some(NativeRecording {
    host,
    session_directory,
    accumulated: Duration::ZERO,
    active_since: Some(Instant::now()),
    paused: Duration::ZERO,
    paused_since: None,
    pauses: 0,
    microphone: recorder.config.microphone.is_some().then_some(true),
    inspect: inspector(recorder),
  });
  recorder.set_status(RecordingStatus::Recording);
  tracing::info!(
    elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
    "Native recording started"
  );
  Ok(())
}

fn inspector(
  _recorder: &VideoRecorder,
) -> fn(&Path) -> std::result::Result<Inspection, EngineError> {
  #[cfg(test)]
  if let Some(hooks) = _recorder.scripted.as_ref() {
    return hooks.inspect;
  }
  flinch_video_engine::recovery::inspect
}

fn fail_start(recorder: &mut VideoRecorder, message: String, retained: bool) -> VideoError {
  tracing::error!(error = %message, "Native recording did not start");
  // A directory the engine reported as recoverable is named in the message and kept.
  if !retained && let Some(path) = recorder.runtime.temp_output_path.as_deref() {
    crate::capture_artifact::remove_recording_session_temp_dir(path, "native_start");
  }
  recorder.runtime.clear();
  recorder.set_status(RecordingStatus::Idle);
  prewarm();
  VideoError::CaptureFailed(message)
}

/// Surface a worker that failed or finished on its own while the app believes it is recording.
pub(super) fn poll(recorder: &mut VideoRecorder) {
  if !matches!(
    recorder.status,
    RecordingStatus::Recording | RecordingStatus::Paused
  ) || recorder.runtime.last_error.is_some()
  {
    return;
  }
  let Some(native) = recorder.runtime.native.as_ref() else {
    return;
  };
  let message = if let Some(failure) = native.host.failure() {
    native.describe(&failure)
  } else if let Some(outcome) = native.host.terminal_outcome() {
    match outcome.error {
      Some(error) => native.describe(&error),
      None => format!(
        "Native recording ended unexpectedly with {:?}",
        outcome.kind
      ),
    }
  } else {
    return;
  };
  tracing::error!(error = %message, "Native recording failed during capture");
  if let Some(native) = recorder.runtime.native.as_mut()
    && let Some(since) = native.active_since.take()
  {
    native.accumulated += since.elapsed();
  }
  recorder.runtime.last_error = Some(message);
  recorder.set_status(RecordingStatus::Error);
}

/// The microphone as the recording controls show it; `None` when the recording has none.
pub(super) fn microphone(recorder: &mut VideoRecorder) -> Option<super::MicrophoneState> {
  let native = recorder.runtime.native.as_ref()?;
  let audible = native.microphone?;
  Some(if native.host.snapshot().microphone_lost {
    super::MicrophoneState::Lost
  } else if audible {
    super::MicrophoneState::Audible
  } else {
    super::MicrophoneState::Muted
  })
}

/// Mutes or unmutes the microphone from now. A refusal leaves the recording running as it was.
pub(super) fn set_microphone(recorder: &mut VideoRecorder, audible: bool) -> Result<()> {
  if !matches!(
    recorder.status,
    RecordingStatus::Recording | RecordingStatus::Paused
  ) {
    return Err(VideoError::NotRecording);
  }
  let native = active(recorder)?;
  if native.microphone.is_none() {
    return Err(VideoError::ConfigError(
      "This recording has no microphone".into(),
    ));
  }
  native
    .host
    .microphone(audible)
    .map_err(|error| VideoError::CaptureFailed(native.describe(&error)))?;
  native.microphone = Some(audible);
  tracing::info!(audible, "Native recording microphone switched");
  Ok(())
}

pub(super) fn pause(recorder: &mut VideoRecorder) -> Result<()> {
  if recorder.status != RecordingStatus::Recording {
    return Err(VideoError::NotRecording);
  }
  let native = active(recorder)?;
  native
    .host
    .pause()
    .map_err(|error| VideoError::CaptureFailed(native.describe(&error)))?;
  if let Some(since) = native.active_since.take() {
    native.accumulated += since.elapsed();
  }
  native.paused_since = Some(Instant::now());
  native.pauses = native.pauses.saturating_add(1);
  tracing::info!(
    pause = native.pauses,
    active_ms = native.accumulated.as_millis() as u64,
    "Native recording paused"
  );
  recorder.set_status(RecordingStatus::Paused);
  Ok(())
}

pub(super) fn resume(recorder: &mut VideoRecorder) -> Result<()> {
  if recorder.status != RecordingStatus::Paused {
    return Err(VideoError::ConfigError("Not paused".to_string()));
  }
  let native = active(recorder)?;
  native
    .host
    .resume()
    .map_err(|error| VideoError::CaptureFailed(native.describe(&error)))?;
  let paused_ms = native
    .paused_since
    .take()
    .map_or(Duration::ZERO, |since| {
      let paused = since.elapsed();
      native.paused += paused;
      paused
    })
    .as_millis() as u64;
  native.active_since = Some(Instant::now());
  tracing::info!(
    pause = native.pauses,
    paused_ms,
    active_ms = native.accumulated.as_millis() as u64,
    "Native recording resumed"
  );
  recorder.set_status(RecordingStatus::Recording);
  Ok(())
}

fn active(recorder: &mut VideoRecorder) -> Result<&mut NativeRecording> {
  recorder
    .runtime
    .native
    .as_mut()
    .ok_or_else(|| VideoError::ConfigError("No native recording session".into()))
}

pub(super) fn elapsed(recorder: &mut VideoRecorder) -> Duration {
  poll(recorder);
  recorder
    .runtime
    .native
    .as_ref()
    .map_or(Duration::ZERO, NativeRecording::elapsed)
}

pub(super) fn stats(recorder: &mut VideoRecorder) -> RecordingStats {
  let duration = elapsed(recorder);
  let snapshot = recorder
    .runtime
    .native
    .as_ref()
    .map(|native| native.host.snapshot())
    .unwrap_or_default();
  RecordingStats {
    status: recorder.status,
    duration_ms: duration.as_millis() as u64,
    frames_captured: snapshot.frames_new,
    frames_encoded: snapshot.frames_encoded,
    frames_dropped: snapshot.slots_dropped,
    file_size_bytes: snapshot.bytes_written,
  }
}

pub(super) fn stop(recorder: &mut VideoRecorder) -> Result<PathBuf> {
  if !matches!(
    recorder.status,
    RecordingStatus::Recording | RecordingStatus::Paused | RecordingStatus::Error
  ) {
    return Err(VideoError::NotRecording);
  }
  let started = Instant::now();
  recorder.finalization_stage(super::FinalizationStage::Closing);
  recorder.set_status(RecordingStatus::Stopping);
  let result = finish(recorder);
  let temp_output_path = recorder.runtime.temp_output_path.clone();
  if result.is_ok()
    && let Some(path) = temp_output_path.as_deref()
  {
    crate::capture_artifact::remove_recording_session_temp_dir(path, "native_stop");
  }
  recorder.runtime.clear();
  recorder.set_status(RecordingStatus::Idle);
  prewarm();
  match &result {
    Ok(path) => tracing::info!(
      elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
      path = %path.display(),
      "Native recording finalization completed"
    ),
    Err(error) => tracing::error!(%error, "Native recording finalization failed"),
  }
  result
}

fn finish(recorder: &mut VideoRecorder) -> Result<PathBuf> {
  let native = recorder
    .runtime
    .native
    .as_ref()
    .ok_or_else(|| VideoError::ConfigError("No native recording session".into()))?;
  let final_output_path = recorder
    .runtime
    .final_output_path
    .clone()
    .ok_or_else(|| VideoError::ConfigError("No final output path set".to_string()))?;
  // The app's own clock, read before Stop, is one of the independent checks on the file.
  let elapsed = native.elapsed();
  let clock = SessionClock {
    active: elapsed,
    paused: native.paused(),
    pauses: native.pauses,
  };
  let inspect = native.inspect;
  let outcome = match native.host.terminal_outcome() {
    Some(outcome) => outcome,
    None => native
      .host
      .stop()
      .map_err(|error| VideoError::CaptureFailed(native.describe(&error)))?,
  };
  // Read now: the session directory holding it is removed once the recording is published.
  let thumbnail = outcome
    .thumbnail
    .as_ref()
    .and_then(|thumbnail| session_thumbnail(thumbnail, &native.session_directory));
  let claimed = outcome.snapshot;
  log_session(&claimed, &clock);
  recorder.microphone_lost = claimed.microphone_lost;
  let output = match (outcome.kind, outcome.output, outcome.error) {
    (OutcomeKind::Completed, Some(output), None) => output,
    // Lost slots or a recreated source still leave a complete file; it passes the same check.
    (OutcomeKind::Degraded, Some(output), None) => {
      tracing::warn!(
        slots_dropped = claimed.slots_dropped,
        frames_encoded = claimed.frames_encoded,
        "Native recording completed degraded"
      );
      output
    },
    (kind, _, Some(error)) => {
      return Err(VideoError::CaptureFailed(format!(
        "Native recording ended with {kind:?}: {}",
        native.describe(&error)
      )));
    },
    (kind, _, None) => {
      let retained = outcome
        .partial
        .unwrap_or_else(|| native.session_directory.clone());
      return Err(VideoError::CaptureFailed(format!(
        "Native recording ended with {kind:?} and no published file. Recording data retained at {}",
        retained.display()
      )));
    },
  };
  recorder.finalization_stage(super::FinalizationStage::Checking);
  let fps = recorder
    .runtime
    .capture_config
    .as_ref()
    .map_or(recorder.config.fps, |capture| capture.fps);
  inspect(&output)
    .map_err(|error| error.to_string())
    .and_then(|inspection| {
      check_output(
        &inspection,
        &claimed,
        recorder.config.audio_track(),
        fps,
        elapsed,
      )
    })
    .map_err(|reason| {
      VideoError::EncodingFailed(format!(
        "Native recording failed its output check ({reason}). Recording retained at {}",
        output.display()
      ))
    })?;
  if recorder
    .cancellation
    .load(std::sync::atomic::Ordering::Acquire)
  {
    return Err(VideoError::CaptureFailed(format!(
      "Recording finalization cancelled. Recording retained at {}",
      output.display()
    )));
  }
  recorder.finalization_stage(super::FinalizationStage::Saving);
  let saved = if recorder.config.recording_profile == RecordingProfile::GifIntermediate {
    finish_gif(recorder, &output, &final_output_path)?
  } else {
    publish(&output, &final_output_path)?
  };
  // A GIF's last frame is its intermediate recording's, so the same picture serves it.
  if let Some(pixels) = thumbnail {
    crate::thumbnails::remember(&saved, pixels);
  }
  Ok(saved)
}

/// The engine's picture of the session's last frame, for the saved recording's notification. It
/// must be the engine's file in this session's directory; anything else, or a file that cannot be
/// read, leaves the notification to the Shell's thumbnail.
fn session_thumbnail(
  thumbnail: &flinch_video_engine::thumbnail::Thumbnail,
  session_directory: &Path,
) -> Option<crate::thumbnails::Pixels> {
  let expected = session_directory.join(flinch_video_engine::thumbnail::FILE);
  if thumbnail.path != expected {
    tracing::warn!(
      path = %thumbnail.path.display(),
      "Recording thumbnail is not in its session directory; ignoring it"
    );
    return None;
  }
  match flinch_video_engine::thumbnail::read(thumbnail) {
    Ok(pixels) => Some(crate::thumbnails::Pixels {
      width: pixels.width,
      height: pixels.height,
      rgba: pixels.rgba,
    }),
    Err(error) => {
      tracing::warn!(%error, "Recording thumbnail unreadable; the notification asks the Shell");
      None
    },
  }
}

/// What the session resolved to and how it went, from the worker's final snapshot. The request
/// alone (`encoder=Auto`) cannot explain a recording after the fact. `media_ms` is the engine's
/// timeline; `active_ms` and `paused_ms` are the app's monotonic clock, so a gap against the
/// wall-clock log timestamps is attributable to a system clock step rather than lost media.
fn log_session(snapshot: &SessionSnapshot, clock: &SessionClock) {
  let summary = SessionSummary::of(snapshot);
  tracing::info!(
    media_ms = snapshot.media_100ns / 10_000,
    active_ms = clock.active.as_millis() as u64,
    paused_ms = clock.paused.as_millis() as u64,
    pauses = clock.pauses,
    encoder = %summary.encoder,
    encoder_rejected = %summary.encoder_rejected,
    capture = %summary.capture,
    capture_rejected = %summary.capture_rejected,
    conversion = %summary.conversion,
    surfaces = snapshot.surfaces,
    frames_new = snapshot.frames_new,
    frames_reused = snapshot.frames_reused,
    frames_encoded = snapshot.frames_encoded,
    slots_dropped = snapshot.slots_dropped,
    audio_silence = snapshot.audio_silence,
    degraded = snapshot.degraded,
    "Native recording session"
  );
}

/// The app's monotonic view of a session at Stop.
struct SessionClock {
  active: Duration,
  paused: Duration,
  pauses: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct SessionSummary {
  pub(super) encoder: String,
  pub(super) encoder_rejected: String,
  pub(super) capture: String,
  pub(super) capture_rejected: String,
  pub(super) conversion: String,
}

impl SessionSummary {
  pub(super) fn of(snapshot: &SessionSnapshot) -> Self {
    let selected = |selected: Option<String>| selected.unwrap_or_else(|| "unreported".into());
    let rejected = |rejections: Vec<String>| {
      if rejections.is_empty() {
        "none".into()
      } else {
        rejections.join("; ")
      }
    };
    let encoder = snapshot.tier.as_ref();
    let capture = snapshot.capture_tier.as_ref();
    Self {
      encoder: selected(
        encoder
          .and_then(|tier| tier.selected)
          .map(|kind| format!("{kind:?}")),
      ),
      encoder_rejected: rejected(encoder.map_or_else(Vec::new, |tier| {
        tier
          .rejected
          .iter()
          .map(|r| format!("{:?}: {}", r.encoder, r.error))
          .collect()
      })),
      capture: selected(
        capture
          .and_then(|tier| tier.selected)
          .map(|kind| format!("{kind:?}")),
      ),
      capture_rejected: rejected(capture.map_or_else(Vec::new, |tier| {
        tier
          .rejected
          .iter()
          .map(|r| format!("{:?}: {}", r.capture, r.error))
          .collect()
      })),
      conversion: selected(snapshot.conversion.map(|kind| format!("{kind:?}"))),
    }
  }
}

/// Independent check of the engine's file before it is published: structure, tracks, the
/// worker's own encoded-frame and media-time claims, and the app's elapsed recording time.
/// It reads box headers only, so it stays within the Stop budget for long recordings.
pub(super) fn check_output(
  inspection: &Inspection,
  claimed: &SessionSnapshot,
  audio: bool,
  fps: u32,
  elapsed: Duration,
) -> std::result::Result<(), String> {
  if inspection.truncated || inspection.damage_reason.is_some() {
    return Err(format!(
      "incomplete or damaged file: {}",
      inspection
        .damage_reason
        .as_deref()
        .unwrap_or("truncated fragment")
    ));
  }
  // Windows players would end the video at such a fragment; the engine realigns them.
  if inspection.chunk_straddles != 0 {
    return Err(format!(
      "{} fragments that Windows players would stop at",
      inspection.chunk_straddles
    ));
  }
  let expected_tracks = 1 + usize::from(audio);
  if inspection.tracks != expected_tracks || inspection.track_media.len() != expected_tracks {
    return Err(format!(
      "{} tracks instead of {expected_tracks}",
      inspection.tracks
    ));
  }
  let track = |handler: &[u8; 4], codec: &[u8; 4]| {
    inspection
      .track_media
      .iter()
      .find(|t| t.handler.as_ref() == Some(handler) && t.codec.as_ref() == Some(codec))
  };
  let video = track(b"vide", b"avc1").ok_or("no H.264 video track")?;
  if video.samples == 0 || video.samples != claimed.frames_encoded {
    return Err(format!(
      "{} video samples, but the engine encoded {}",
      video.samples, claimed.frames_encoded
    ));
  }
  let frame = 10_000_000 / u64::from(fps.max(1));
  if video.start_100ns != Some(0) || video.end_100ns.abs_diff(claimed.media_100ns) > frame {
    return Err(format!(
      "video spans {:?}..{} (100 ns), but the engine reported {}",
      video.start_100ns, video.end_100ns, claimed.media_100ns
    ));
  }
  if audio {
    let sound = track(b"soun", b"mp4a").ok_or("no AAC audio track")?;
    // One AAC packet (1024 frames at 48 kHz) plus one video frame.
    if sound.samples == 0 || sound.end_100ns.abs_diff(video.end_100ns) > frame + 213_334 {
      return Err(format!(
        "audio ends at {} but video at {} (100 ns)",
        sound.end_100ns, video.end_100ns
      ));
    }
  }
  // The app's clock includes control round trips, so it bounds gross loss rather than frames.
  let recorded = Duration::from_nanos(video.end_100ns.saturating_mul(100));
  let slack = Duration::from_secs(1) + elapsed / 50;
  if recorded.abs_diff(elapsed) > slack {
    return Err(format!(
      "{:.3} s of media for {:.3} s of recording",
      recorded.as_secs_f64(),
      elapsed.as_secs_f64()
    ));
  }
  Ok(())
}

/// Palette training, dithering and GIF writing run natively from the intermediate MP4.
fn finish_gif(
  recorder: &mut VideoRecorder,
  intermediate: &Path,
  destination: &Path,
) -> Result<PathBuf> {
  let options = recorder.runtime.gif_options.ok_or_else(|| {
    VideoError::ConfigError("Missing GIF encode settings for recording finalization".to_string())
  })?;
  #[cfg(test)]
  if let Some(hooks) = recorder.scripted.as_ref() {
    (hooks.write_gif)(intermediate, destination, options).map_err(VideoError::EncodingFailed)?;
    return Ok(destination.to_path_buf());
  }
  let started = Instant::now();
  let cancellation = recorder.cancellation.clone();
  let report =
    flinch_video_engine::windows::gif::encode_with(intermediate, destination, options, || {
      if cancellation.load(std::sync::atomic::Ordering::Acquire) {
        return Err(EngineError::new(
          flinch_video_engine::ErrorKind::Cancelled,
          "gif",
          "Recording finalization cancelled",
        ));
      }
      Ok(())
    })
    .map_err(|error| {
      VideoError::EncodingFailed(format!(
        "Failed to encode GIF to {}: {error}",
        destination.display()
      ))
    })?;
  tracing::info!(
    elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
    ?report,
    "Native GIF encoded"
  );
  Ok(destination.to_path_buf())
}

/// Free names tried when the chosen one was taken during the recording, before Stop gives up and
/// keeps the recording where the engine wrote it.
const PUBLISH_ATTEMPTS: usize = 16;

/// Move the engine's published file into place, never replacing a file. The name was free when
/// the recording started; if something created it since, the next free name is used, as at
/// start. Across volumes the file is copied beside the destination first, so the destination
/// only ever appears complete. Returns where the recording was saved.
fn publish(source: &Path, destination: &Path) -> Result<PathBuf> {
  publish_with(source, destination, move_no_replace)
}
/// [`publish`] with the same-volume move injected, so the cross-volume branch is testable. The
/// move must refuse an existing destination with `ErrorKind::AlreadyExists`.
pub(super) fn publish_with(
  source: &Path,
  destination: &Path,
  mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf> {
  let mut target = destination.to_path_buf();
  for _ in 0..PUBLISH_ATTEMPTS {
    match rename(source, &target) {
      Ok(()) => return Ok(target),
      Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
        target = next_free(destination);
      },
      Err(error) if error.raw_os_error() == Some(NOT_SAME_DEVICE) => {
        return publish_across_volumes(source, destination, target);
      },
      Err(error) => {
        return Err(VideoError::CaptureFailed(format!(
          "Recording publish failed ({} -> {}): {error}",
          source.display(),
          target.display()
        )));
      },
    }
  }
  Err(names_taken(source, destination))
}
/// Copy into a staging file beside the destination, then move it to a free name without replacing.
fn publish_across_volumes(
  source: &Path,
  destination: &Path,
  mut target: PathBuf,
) -> Result<PathBuf> {
  let parent = destination
    .parent()
    .ok_or_else(|| VideoError::ConfigError("Missing output folder".into()))?;
  let mut staged = tempfile::Builder::new()
    .prefix(".flinch-publishing-")
    .suffix(".mp4")
    .tempfile_in(parent)
    .map_err(VideoError::Io)?;
  std::fs::copy(source, staged.path()).map_err(|e| {
    VideoError::CaptureFailed(format!(
      "Recording copy to {} failed: {e}",
      parent.display()
    ))
  })?;
  staged.as_file().sync_all().map_err(VideoError::Io)?;
  for _ in 0..PUBLISH_ATTEMPTS {
    match staged.persist_noclobber(&target) {
      Ok(_) => return Ok(target),
      // The staging file comes back and is removed when dropped.
      Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
        staged = error.file;
        target = next_free(destination);
      },
      Err(error) => {
        return Err(VideoError::CaptureFailed(format!(
          "Recording publish to {} failed: {}",
          target.display(),
          error.error
        )));
      },
    }
  }
  Err(names_taken(source, destination))
}
/// The first free name after `destination`, with the same numbering as at recording start.
fn next_free(destination: &Path) -> PathBuf {
  crate::capture_artifact::next_available_path_with_limit(destination, "recording", Some(9999))
}
fn names_taken(source: &Path, destination: &Path) -> VideoError {
  VideoError::CaptureFailed(format!(
    "Recording publish found {} and its next names taken. Recording retained at {}",
    destination.display(),
    source.display()
  ))
}
/// Same-volume move that refuses to replace an existing file (`ErrorKind::AlreadyExists`) and
/// reports a different volume as Windows `ERROR_NOT_SAME_DEVICE`.
pub(super) fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
  use std::os::windows::ffi::OsStrExt;
  use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
  let wide = |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain([0]).collect() };
  let (source, destination) = (wide(from), wide(to));
  // SAFETY: both paths are NUL-terminated and outlive the call; no replacement flag is passed.
  unsafe {
    MoveFileExW(
      windows::core::PCWSTR(source.as_ptr()),
      windows::core::PCWSTR(destination.as_ptr()),
      MOVEFILE_WRITE_THROUGH,
    )
  }
  .map_err(|error| {
    // Win32 failures arrive as HRESULT_FROM_WIN32; keep the raw code so callers can match it.
    let code = error.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
      std::io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
      std::io::Error::other(error)
    }
  })
}
