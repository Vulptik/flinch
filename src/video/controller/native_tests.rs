//! Recorder lifecycle on the native backend against a scripted host: no worker, capture or encoder.

use super::native::{self, Host};
use super::{RecordingStatus, VideoRecorder};
use crate::monitors::MonitorInfo;
use crate::video::VideoError;
use crate::video::capture::CaptureConfig;
use crate::video::config::{RecordingProfile, VideoConfig};
use flinch_video_engine::recovery::{Inspection, TrackMedia};
use flinch_video_engine::types::{
  OutcomeKind, RecordingOutcome, SessionSnapshot, SessionSpec, SessionState,
};
use flinch_video_engine::{EngineError, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Script {
  prepare_error: Mutex<Option<EngineError>>,
  start_error: Mutex<Option<EngineError>>,
  pause_error: Mutex<Option<EngineError>>,
  failure: Mutex<Option<EngineError>>,
  terminal: Mutex<Option<RecordingOutcome>>,
  stop: Mutex<Option<RecordingOutcome>>,
  spec: Mutex<Option<SessionSpec>>,
  /// The session directory Prepare reports; `session` when unset.
  directory: Mutex<Option<PathBuf>>,
  calls: Mutex<Vec<&'static str>>,
  dropped: AtomicBool,
  /// The worker refuses to switch the microphone.
  microphone_refused: AtomicBool,
  /// The worker reports its microphone gone.
  microphone_lost: AtomicBool,
}
struct Scripted(Arc<Script>);
impl Drop for Scripted {
  fn drop(&mut self) {
    self.0.dropped.store(true, Ordering::Release);
  }
}
fn take(slot: &Mutex<Option<EngineError>>) -> Result<(), EngineError> {
  match slot.lock().expect("script").take() {
    Some(error) => Err(error),
    None => Ok(()),
  }
}
impl Scripted {
  fn call(&self, name: &'static str) {
    self.0.calls.lock().expect("calls").push(name);
  }
}
impl Host for Scripted {
  fn prepare(&self, spec: SessionSpec) -> Result<PathBuf, EngineError> {
    self.call("prepare");
    *self.0.spec.lock().expect("spec") = Some(spec);
    let directory = self.0.directory.lock().expect("directory").clone();
    take(&self.0.prepare_error).map(|()| directory.unwrap_or_else(|| PathBuf::from("session")))
  }
  fn start(&self) -> Result<(), EngineError> {
    self.call("start");
    take(&self.0.start_error)
  }
  fn pause(&self) -> Result<(), EngineError> {
    self.call("pause");
    take(&self.0.pause_error)
  }
  fn resume(&self) -> Result<(), EngineError> {
    self.call("resume");
    Ok(())
  }
  fn microphone(&self, _audible: bool) -> Result<(), EngineError> {
    self.call("microphone");
    if self.0.microphone_refused.load(Ordering::Acquire) {
      return Err(EngineError::new(
        ErrorKind::InvalidState,
        "microphone",
        "scripted refusal",
      ));
    }
    Ok(())
  }
  fn stop(&self) -> Result<RecordingOutcome, EngineError> {
    self.call("stop");
    self
      .0
      .stop
      .lock()
      .expect("stop")
      .take()
      .ok_or_else(|| EngineError::new(ErrorKind::Timeout, "stop", "scripted stop timeout"))
  }
  fn snapshot(&self) -> SessionSnapshot {
    SessionSnapshot {
      state: SessionState::Recording,
      frames_new: 40,
      frames_encoded: 42,
      slots_dropped: 2,
      bytes_written: 4096,
      microphone_lost: self.0.microphone_lost.load(Ordering::Acquire),
      ..Default::default()
    }
  }
  fn failure(&self) -> Option<EngineError> {
    self.0.failure.lock().expect("failure").clone()
  }
  fn terminal_outcome(&self) -> Option<RecordingOutcome> {
    self.0.terminal.lock().expect("terminal").clone()
  }
  fn take_diagnostics(&self) -> (Vec<String>, u64) {
    (vec!["worker said hello".into()], 3)
  }
}

fn display() -> Vec<MonitorInfo> {
  vec![MonitorInfo {
    id: "0,0,640x360".into(),
    name: "fixture".into(),
    x: 0,
    y: 0,
    width: 640,
    height: 360,
    is_primary: true,
    dpi_scale: 1.0,
    logical_x: 0,
    logical_y: 0,
    logical_width: 640,
    logical_height: 360,
  }]
}
fn outcome(
  kind: OutcomeKind,
  output: Option<PathBuf>,
  error: Option<EngineError>,
) -> RecordingOutcome {
  RecordingOutcome {
    thumbnail: None,
    kind,
    output,
    partial: None,
    error,
    // What the worker claims the file holds: three 30 FPS frames.
    snapshot: SessionSnapshot {
      frames_encoded: 3,
      media_100ns: 1_000_000,
      ..Default::default()
    },
  }
}

struct Fixture {
  _dir: tempfile::TempDir,
  temp: PathBuf,
  engine_output: PathBuf,
  destination: PathBuf,
  script: Arc<Script>,
}
fn fixture() -> Fixture {
  let dir = tempfile::tempdir().expect("tempdir");
  let session = dir.path().join("session");
  std::fs::create_dir_all(&session).expect("session dir");
  Fixture {
    temp: session.join("recording.mp4"),
    engine_output: session.join("recording.mp4"),
    destination: dir.path().join("final.mp4"),
    _dir: dir,
    script: Arc::new(Script::default()),
  }
}
fn start(
  recorder: &mut VideoRecorder,
  fixture: &Fixture,
  capture: CaptureConfig,
) -> crate::video::Result<()> {
  let script = fixture.script.clone();
  native::start(
    recorder,
    capture,
    fixture.temp.clone(),
    fixture.destination.clone(),
    &display(),
    move || Ok(Box::new(Scripted(script)) as Box<dyn Host>),
  )
}
/// The structure the claimed outcome describes, standing in for a real MP4 parse.
fn matching_file(_: &Path) -> Result<Inspection, EngineError> {
  Ok(Inspection {
    tracks: 1,
    track_media: vec![TrackMedia {
      id: 1,
      handler: Some(*b"vide"),
      codec: Some(*b"avc1"),
      samples: 3,
      start_100ns: Some(0),
      end_100ns: 1_000_000,
      ..Default::default()
    }],
    ..Default::default()
  })
}
fn stub_inspection(recorder: &mut VideoRecorder) {
  recorder
    .runtime
    .native
    .as_mut()
    .expect("native session")
    .inspect = matching_file;
}
fn calls(script: &Script) -> Vec<&'static str> {
  script.calls.lock().expect("calls").clone()
}

#[test]
fn native_session_records_pauses_and_publishes_through_the_recorder_api() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  assert_eq!(recorder.status(), RecordingStatus::Recording);
  let spec = fixture
    .script
    .spec
    .lock()
    .expect("spec")
    .clone()
    .expect("prepared");
  assert_eq!(
    (spec.monitor.as_str(), spec.region, spec.fps),
    ("0,0,640x360", None, 30)
  );
  assert_eq!(spec.output, fixture.temp);

  let stats = recorder.stats();
  assert_eq!(
    (
      stats.frames_captured,
      stats.frames_encoded,
      stats.frames_dropped,
      stats.file_size_bytes
    ),
    (40, 42, 2, 4096)
  );
  recorder.pause_recording().expect("pause");
  assert_eq!(recorder.status(), RecordingStatus::Paused);
  let paused = recorder.get_elapsed_time();
  std::thread::sleep(Duration::from_millis(30));
  assert_eq!(
    recorder.get_elapsed_time(),
    paused,
    "paused time does not advance"
  );
  assert!(matches!(
    recorder.pause_recording(),
    Err(VideoError::NotRecording)
  ));
  recorder.resume_recording().expect("resume");
  std::thread::sleep(Duration::from_millis(30));
  assert!(recorder.get_elapsed_time() > paused);

  std::fs::write(&fixture.engine_output, b"native media").expect("engine output");
  *fixture.script.stop.lock().expect("stop") = Some(outcome(
    OutcomeKind::Completed,
    Some(fixture.engine_output.clone()),
    None,
  ));
  assert_eq!(
    recorder.stop_recording().expect("stop"),
    fixture.destination
  );
  assert_eq!(
    std::fs::read(&fixture.destination).expect("published"),
    b"native media"
  );
  assert!(!fixture.engine_output.exists());
  assert_eq!(recorder.status(), RecordingStatus::Idle);
  assert!(fixture.script.dropped.load(Ordering::Acquire));
  assert_eq!(
    calls(&fixture.script),
    ["prepare", "start", "pause", "resume", "stop"]
  );
  assert!(matches!(
    recorder.stop_recording(),
    Err(VideoError::NotRecording)
  ));
}

#[test]
fn start_failures_return_to_idle_with_the_engine_cause_and_worker_diagnostics() {
  for failing in ["launch", "prepare", "start"] {
    let fixture = fixture();
    let mut recorder = VideoRecorder::new(VideoConfig::default());
    let cause = EngineError::new(ErrorKind::Capability, "region", "scripted refusal");
    let result = if failing == "launch" {
      let cause = cause.clone();
      native::start(
        &mut recorder,
        CaptureConfig::fullscreen(640, 360, 30),
        fixture.temp.clone(),
        fixture.destination.clone(),
        &display(),
        move || Err(cause),
      )
    } else {
      let slot = if failing == "prepare" {
        &fixture.script.prepare_error
      } else {
        &fixture.script.start_error
      };
      *slot.lock().expect("slot") = Some(cause);
      start(
        &mut recorder,
        &fixture,
        CaptureConfig::fullscreen(640, 360, 30),
      )
    };
    let message = result.expect_err(failing).to_string();
    assert!(message.contains("scripted refusal"), "{failing}: {message}");
    if failing != "launch" {
      assert!(
        message.contains("worker said hello") && message.contains("3 more dropped"),
        "{message}"
      );
      // The unfinished host is dropped, which aborts its worker.
      assert!(fixture.script.dropped.load(Ordering::Acquire));
    }
    assert_eq!(recorder.status(), RecordingStatus::Idle, "{failing}");
    assert!(recorder.runtime.temp_output_path.is_none());
  }
}

#[test]
fn only_an_engine_capture_unavailability_marks_the_start_failure_as_unavailable() {
  let unavailable = EngineError::new(
    ErrorKind::Capability,
    flinch_video_engine::types::CAPTURE_UNAVAILABLE_STAGE,
    "quiet capture: Dxgi refused; Wgc also rejected",
  );
  // Another capability refusal (a region, an encoder) keeps the generic failure message.
  let other = EngineError::new(ErrorKind::Capability, "region", "scripted refusal");
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  for (cause, expected) in [(unavailable, true), (other, false)] {
    *fixture.script.prepare_error.lock().expect("slot") = Some(cause);
    start(
      &mut recorder,
      &fixture,
      CaptureConfig::fullscreen(640, 360, 30),
    )
    .expect_err("prepare refused");
    assert_eq!(recorder.capture_unavailable(), expected);
    assert_eq!(recorder.status(), RecordingStatus::Idle);
  }
  // A later successful start clears the previous unavailability.
  *fixture.script.prepare_error.lock().expect("slot") = Some(EngineError::new(
    ErrorKind::Capability,
    flinch_video_engine::types::CAPTURE_UNAVAILABLE_STAGE,
    "unavailable",
  ));
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect_err("unavailable");
  assert!(recorder.capture_unavailable());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("capture available again");
  assert!(!recorder.capture_unavailable());
}

#[test]
fn unsupported_plans_are_refused_before_any_worker_launches() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  // An area running off the only display cannot be recorded and is never clipped.
  let mut off_desktop = CaptureConfig::fullscreen(640, 360, 30);
  off_desktop.x = 600;
  let launched = Arc::new(AtomicBool::new(false));
  let flag = launched.clone();
  let error = native::start(
    &mut recorder,
    off_desktop,
    fixture.temp.clone(),
    fixture.destination.clone(),
    &display(),
    move || {
      flag.store(true, Ordering::Release);
      Err(EngineError::protocol("must not launch"))
    },
  )
  .expect_err("off desktop");
  assert!(matches!(error, VideoError::ConfigError(_)), "{error}");
  assert!(!launched.load(Ordering::Acquire));

  let mut gif = VideoRecorder::new(VideoConfig {
    recording_profile: RecordingProfile::GifIntermediate,
    ..VideoConfig::default()
  });
  let error = start(&mut gif, &fixture, CaptureConfig::fullscreen(640, 360, 30))
    .expect_err("missing GIF settings");
  assert!(error.to_string().contains("GIF encode settings"), "{error}");
  assert!(calls(&fixture.script).is_empty());
  assert_eq!(gif.status(), RecordingStatus::Idle);
}

#[test]
fn a_refused_pause_keeps_recording_and_a_worker_failure_surfaces_as_an_error_state() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  *fixture.script.pause_error.lock().expect("pause") = Some(EngineError::new(
    ErrorKind::InvalidState,
    "worker",
    "not now",
  ));
  assert!(
    recorder
      .pause_recording()
      .expect_err("refused")
      .to_string()
      .contains("not now")
  );
  assert_eq!(recorder.status(), RecordingStatus::Recording);

  let mut failure = EngineError::new(ErrorKind::Timeout, "heartbeat", "worker went silent");
  failure.recoverable_path = Some(PathBuf::from("session").join("capture.partial.mp4"));
  *fixture.script.failure.lock().expect("failure") = Some(failure.clone());
  assert_eq!(recorder.status(), RecordingStatus::Error);
  let frozen = recorder.get_elapsed_time();
  std::thread::sleep(Duration::from_millis(20));
  assert_eq!(recorder.get_elapsed_time(), frozen);
  // Stop asks the worker anyway; its answer names the cause and the retained partial file.
  *fixture.script.stop.lock().expect("stop") =
    Some(outcome(OutcomeKind::Failed, None, Some(failure)));
  let message = recorder
    .stop_recording()
    .expect_err("failed session")
    .to_string();
  assert!(
    message.contains("worker went silent") && message.contains("capture.partial.mp4"),
    "{message}"
  );
  assert_eq!(recorder.status(), RecordingStatus::Idle);
  assert!(!fixture.destination.exists());
}

#[test]
fn a_session_the_worker_already_ended_is_finalized_from_its_terminal_outcome() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  *fixture.script.terminal.lock().expect("terminal") =
    Some(outcome(OutcomeKind::Aborted, None, None));
  assert_eq!(recorder.status(), RecordingStatus::Error);
  let message = recorder.stop_recording().expect_err("aborted").to_string();
  assert!(
    message.contains("Aborted") && message.contains("retained at session"),
    "{message}"
  );
  assert!(
    !calls(&fixture.script).contains(&"stop"),
    "no Stop after a terminal outcome"
  );

  // A stop that times out is reported with the host's cause, never treated as success.
  let fixture = self::fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  let message = recorder.stop_recording().expect_err("timeout").to_string();
  assert!(message.contains("scripted stop timeout"), "{message}");
  assert_eq!(recorder.status(), RecordingStatus::Idle);
}

#[test]
fn dropping_a_recorder_mid_session_stops_and_publishes_the_native_recording() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  std::fs::write(&fixture.engine_output, b"kept").expect("engine output");
  *fixture.script.stop.lock().expect("stop") = Some(outcome(
    OutcomeKind::Completed,
    Some(fixture.engine_output.clone()),
    None,
  ));
  drop(recorder);
  assert_eq!(
    std::fs::read(&fixture.destination).expect("published"),
    b"kept"
  );
  assert!(Path::new(&fixture.destination).exists());
}

#[test]
fn a_degraded_recording_is_published_once_its_file_matches_the_claim() {
  // One lost slot in a ninety-minute session used to leave the whole recording unpublished.
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  std::fs::write(&fixture.engine_output, b"one slot lost").expect("engine output");
  let mut degraded = outcome(
    OutcomeKind::Degraded,
    Some(fixture.engine_output.clone()),
    None,
  );
  degraded.snapshot.slots_dropped = 1;
  degraded.snapshot.degraded = true;
  *fixture.script.stop.lock().expect("stop") = Some(degraded.clone());
  assert_eq!(
    recorder.stop_recording().expect("published"),
    fixture.destination
  );
  assert_eq!(
    std::fs::read(&fixture.destination).expect("published"),
    b"one slot lost"
  );

  // Degraded is not a pass on its own: a file that disagrees with the claim stays retained.
  let fixture = self::fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  std::fs::write(&fixture.engine_output, b"short").expect("engine output");
  degraded.output = Some(fixture.engine_output.clone());
  degraded.snapshot.frames_encoded = 4;
  *fixture.script.stop.lock().expect("stop") = Some(degraded);
  let message = recorder.stop_recording().expect_err("mismatch").to_string();
  assert!(message.contains("output check"), "{message}");
  assert!(!fixture.destination.exists());
  assert!(fixture.engine_output.exists());
}

#[test]
fn gif_sessions_encode_natively_and_keep_an_unreadable_intermediate() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig {
    recording_profile: RecordingProfile::GifIntermediate,
    ..VideoConfig::default()
  });
  recorder
    .set_gif_options(Some(crate::video::native::gif_options()))
    .expect("idle");
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 15),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  assert!(matches!(
    recorder.resume_recording(),
    Err(VideoError::ConfigError(_))
  ));
  std::fs::write(&fixture.engine_output, b"intermediate").expect("engine output");
  *fixture.script.stop.lock().expect("stop") = Some(outcome(
    OutcomeKind::Completed,
    Some(fixture.engine_output.clone()),
    None,
  ));
  // The failure is the native decoder's, naming its cause.
  let error = recorder.stop_recording().expect_err("not an MP4");
  assert!(matches!(error, VideoError::EncodingFailed(_)), "{error}");
  assert!(
    error.to_string().contains("Failed to encode GIF"),
    "{error}"
  );
  assert!(fixture.engine_output.exists(), "intermediate retained");
  assert!(!fixture.destination.exists());
  assert_eq!(recorder.status(), RecordingStatus::Idle);
}

#[test]
fn a_published_file_that_vanished_is_a_publish_failure_not_a_saved_recording() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  *fixture.script.stop.lock().expect("stop") = Some(outcome(
    OutcomeKind::Completed,
    Some(fixture.engine_output.clone()),
    None,
  ));
  let message = recorder.stop_recording().expect_err("missing").to_string();
  assert!(message.contains("Recording publish failed"), "{message}");
  assert!(!fixture.destination.exists());
}

#[test]
fn a_file_that_fails_the_independent_check_is_retained_and_never_published() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  // The real parser sees that the engine's "published" file is not an MP4.
  std::fs::write(&fixture.engine_output, b"not an mp4").expect("engine output");
  *fixture.script.stop.lock().expect("stop") = Some(outcome(
    OutcomeKind::Completed,
    Some(fixture.engine_output.clone()),
    None,
  ));
  let error = recorder.stop_recording().expect_err("check");
  assert!(matches!(error, VideoError::EncodingFailed(_)), "{error}");
  assert!(error.to_string().contains("output check"), "{error}");
  assert!(
    error.to_string().contains("Recording retained at"),
    "{error}"
  );
  assert!(fixture.engine_output.exists());
  assert!(!fixture.destination.exists());
  assert_eq!(recorder.status(), RecordingStatus::Idle);
}

fn inspection(tracks: &[(&[u8; 4], &[u8; 4], u64, u64)]) -> Inspection {
  Inspection {
    tracks: tracks.len(),
    track_media: tracks
      .iter()
      .enumerate()
      .map(|(index, (handler, codec, samples, end))| TrackMedia {
        id: index as u32 + 1,
        handler: Some(**handler),
        codec: Some(**codec),
        samples: *samples,
        start_100ns: Some(0),
        end_100ns: *end,
        ..Default::default()
      })
      .collect(),
    ..Default::default()
  }
}

#[test]
fn the_stop_check_holds_the_file_to_the_engine_claims_and_the_app_clock() {
  let claimed = SessionSnapshot {
    frames_encoded: 300,
    media_100ns: 100_000_000,
    ..Default::default()
  };
  let ten = Duration::from_secs(10);
  let video = (b"vide", b"avc1", 300, 100_000_000);
  let sound = (b"soun", b"mp4a", 469, 100_053_333);
  let check = |inspection: &Inspection, audio: bool, elapsed: Duration| {
    native::check_output(inspection, &claimed, audio, 30, elapsed)
  };
  assert_eq!(check(&inspection(&[video]), false, ten), Ok(()));
  assert_eq!(check(&inspection(&[video, sound]), true, ten), Ok(()));
  // Within one frame of the claimed media time, and within 1 s + 2 % of the app clock.
  assert_eq!(
    check(
      &inspection(&[(b"vide", b"avc1", 300, 100_333_333)]),
      false,
      Duration::from_millis(10_900)
    ),
    Ok(())
  );

  let failures: Vec<(Inspection, bool, Duration, &str)> = vec![
    (
      inspection(&[(b"vide", b"avc1", 299, 100_000_000)]),
      false,
      ten,
      "299 video samples",
    ),
    (
      inspection(&[(b"vide", b"avc1", 300, 99_000_000)]),
      false,
      ten,
      "engine reported",
    ),
    (
      inspection(&[(b"vide", b"hev1", 300, 100_000_000)]),
      false,
      ten,
      "no H.264",
    ),
    (inspection(&[video]), true, ten, "1 tracks instead of 2"),
    (
      inspection(&[video, sound]),
      false,
      ten,
      "2 tracks instead of 1",
    ),
    (
      inspection(&[video, (b"soun", b"mp4a", 400, 90_000_000)]),
      true,
      ten,
      "audio ends",
    ),
    (
      inspection(&[video, (b"soun", b"Opus", 469, 100_000_000)]),
      true,
      ten,
      "no AAC",
    ),
    (
      inspection(&[video]),
      false,
      Duration::from_secs(12),
      "10.000 s of media for 12.000 s",
    ),
  ];
  for (inspection, audio, elapsed, expected) in failures {
    let reason = check(&inspection, audio, elapsed).expect_err(expected);
    assert!(reason.contains(expected), "{expected}: {reason}");
  }
  let mut late = inspection(&[video]);
  late.track_media[0].start_100ns = Some(333_333);
  assert!(check(&late, false, ten).is_err());
  let mut damaged = inspection(&[video]);
  damaged.damage_reason = Some("bad moof".into());
  assert!(
    check(&damaged, false, ten)
      .expect_err("damaged")
      .contains("bad moof")
  );
  let mut straddling = inspection(&[video]);
  straddling.chunk_straddles = 2;
  assert!(
    check(&straddling, false, ten)
      .expect_err("straddle")
      .contains("Windows players would stop")
  );
  let mut truncated = inspection(&[video]);
  truncated.truncated = true;
  assert!(
    check(&truncated, false, ten)
      .expect_err("truncated")
      .contains("truncated")
  );
}

#[test]
fn a_cancelled_finalization_keeps_the_recording_and_publishes_nothing() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  stub_inspection(&mut recorder);
  std::fs::write(&fixture.engine_output, b"native media").expect("engine output");
  *fixture.script.stop.lock().expect("stop") = Some(outcome(
    OutcomeKind::Completed,
    Some(fixture.engine_output.clone()),
    None,
  ));
  recorder
    .cancellation
    .store(true, std::sync::atomic::Ordering::Release);
  let message = recorder
    .stop_recording()
    .expect_err("cancelled")
    .to_string();
  assert!(message.contains("finalization cancelled"), "{message}");
  assert!(message.contains("retained at"), "{message}");
  assert!(fixture.engine_output.exists());
  assert!(!fixture.destination.exists());
  assert_eq!(recorder.status(), RecordingStatus::Idle);
}

#[test]
fn publication_never_replaces_a_file_created_during_the_recording() {
  // The name was free at start; another application created it before Stop.
  let directory = tempfile::tempdir().expect("directory");
  let destination = directory.path().join("capture.mp4");
  std::fs::write(&destination, b"someone else's file").expect("intervening file");
  let source = directory.path().join("engine.mp4");
  std::fs::write(&source, b"native media").expect("source");
  let saved =
    native::publish_with(&source, &destination, native::move_no_replace).expect("published");
  assert_eq!(saved, directory.path().join("capture (1).mp4"));
  assert_eq!(std::fs::read(&saved).expect("saved"), b"native media");
  assert_eq!(
    std::fs::read(&destination).expect("kept"),
    b"someone else's file"
  );
  assert!(!source.exists());

  // Across volumes the staged copy also lands beside it, and no staging file is left.
  let folder = directory.path().join("other-volume");
  std::fs::create_dir_all(&folder).expect("folder");
  let destination = folder.join("capture.mp4");
  std::fs::write(&destination, b"someone else's file").expect("intervening file");
  std::fs::write(&source, b"native media").expect("source");
  let saved = native::publish_with(&source, &destination, |_, _| {
    Err(std::io::Error::from_raw_os_error(17))
  })
  .expect("copied across volumes");
  assert_eq!(saved, folder.join("capture (1).mp4"));
  assert_eq!(std::fs::read(&saved).expect("saved"), b"native media");
  assert_eq!(
    std::fs::read(&destination).expect("kept"),
    b"someone else's file"
  );
  assert_eq!(std::fs::read_dir(&folder).expect("folder").count(), 2);

  // A move that keeps refusing gives up within its bound and names where the recording is.
  let mut attempts = 0;
  let error = native::publish_with(&source, &destination, |_, _| {
    attempts += 1;
    Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
  })
  .expect_err("every name taken");
  assert_eq!(attempts, 16);
  assert!(error.to_string().contains("retained at"), "{error}");
}

#[test]
fn cross_volume_publication_copies_beside_the_destination_and_other_failures_are_reported() {
  let directory = tempfile::tempdir().expect("directory");
  let source = directory.path().join("engine.mp4");
  let destination = directory.path().join("final").join("capture.mp4");
  std::fs::create_dir_all(destination.parent().expect("parent")).expect("folder");
  std::fs::write(&source, b"native media").expect("source");
  native::publish_with(&source, &destination, |_, _| {
    Err(std::io::Error::from_raw_os_error(17))
  })
  .expect("copied across volumes");
  assert_eq!(
    std::fs::read(&destination).expect("published"),
    b"native media"
  );
  let staged: Vec<_> = std::fs::read_dir(destination.parent().expect("parent"))
    .expect("folder")
    .map(|entry| entry.expect("entry").file_name())
    .collect();
  assert_eq!(staged.len(), 1, "no staging file left: {staged:?}");

  let error = native::publish_with(&source, &directory.path().join("other.mp4"), |_, _| {
    Err(std::io::Error::from_raw_os_error(5))
  })
  .expect_err("access denied");
  assert!(
    error.to_string().contains("Recording publish failed"),
    "{error}"
  );
  // A cross-volume copy whose source vanished fails without leaving a staged file.
  let missing = directory.path().join("missing.mp4");
  let target = directory.path().join("final").join("second.mp4");
  assert!(
    native::publish_with(&missing, &target, |_, _| Err(
      std::io::Error::from_raw_os_error(17)
    ))
    .is_err()
  );
  assert!(!target.exists());
  assert_eq!(
    std::fs::read_dir(destination.parent().expect("parent"))
      .expect("folder")
      .count(),
    1
  );
}

#[test]
fn a_process_that_is_not_an_engine_worker_fails_the_launch_within_its_deadline() {
  // In tests the current executable is the test harness: it rejects the worker switch as an
  // unknown option and exits, so the host never receives a hello and must not wait past it.
  let started = std::time::Instant::now();
  let error = native::launch_worker()
    .err()
    .expect("the test harness is not an engine worker");
  assert!(
    started.elapsed() < flinch_video_engine::limits::HELLO_TIMEOUT + Duration::from_secs(2),
    "{:?}",
    started.elapsed()
  );
  assert!(
    matches!(
      error.kind,
      ErrorKind::Timeout | ErrorKind::Protocol | ErrorKind::Io
    ),
    "{error}"
  );
}

#[test]
fn a_standby_worker_is_launched_once_taken_once_and_a_lost_one_is_never_used() {
  let standby = native::Standby::new();
  assert!(standby.take().is_none(), "nothing launched yet");
  let script = Arc::new(Script::default());
  let launches = std::sync::atomic::AtomicUsize::new(0);
  let launch = |script: Arc<Script>| {
    launches.fetch_add(1, Ordering::AcqRel);
    Ok(Box::new(Scripted(script)) as Box<dyn Host>)
  };
  standby.fill_now(|| launch(script.clone()));
  // A waiting worker is never replaced or duplicated.
  standby.fill_now(|| launch(Arc::new(Script::default())));
  assert_eq!(launches.load(Ordering::Acquire), 1);
  let host = standby.take().expect("healthy standby");
  assert!(standby.take().is_none(), "taken once");
  assert!(!script.dropped.load(Ordering::Acquire));
  drop(host);

  // A failed launch leaves the slot refillable.
  standby.fill_now(|| Err(EngineError::new(ErrorKind::Timeout, "hello", "scripted")));
  assert!(standby.take().is_none());
  let lost = Arc::new(Script::default());
  standby.fill_now(|| launch(lost.clone()));
  assert_eq!(launches.load(Ordering::Acquire), 2);

  // A worker whose heartbeat stopped while waiting (for example across sleep) is dropped.
  *lost.failure.lock().expect("failure") = Some(EngineError::new(
    ErrorKind::Timeout,
    "supervisor heartbeat",
    "No worker event for three seconds",
  ));
  assert!(standby.take().is_none());
  assert!(lost.dropped.load(Ordering::Acquire));
  assert!(
    lost.calls.lock().expect("calls").is_empty(),
    "never prepared"
  );
}

#[test]
fn a_launch_in_progress_is_not_duplicated_by_a_concurrent_refill() {
  let standby = Arc::new(native::Standby::new());
  let (entered, proceed) = (
    Arc::new(std::sync::Barrier::new(2)),
    Arc::new(std::sync::Barrier::new(2)),
  );
  let worker = {
    let (standby, entered, proceed) = (standby.clone(), entered.clone(), proceed.clone());
    std::thread::spawn(move || {
      standby.fill_now(|| {
        entered.wait();
        proceed.wait();
        Ok(Box::new(Scripted(Arc::new(Script::default()))) as Box<dyn Host>)
      })
    })
  };
  entered.wait();
  let mut second = false;
  standby.fill_now(|| {
    second = true;
    Err(EngineError::protocol("must not launch"))
  });
  assert!(!second, "launching flag blocks a second launch");
  assert!(standby.take().is_none(), "not ready while launching");
  proceed.wait();
  worker.join().expect("launcher");
  assert!(standby.take().is_some());
}

fn scripted_host() -> Result<Box<dyn Host>, EngineError> {
  Ok(Box::new(Scripted(Arc::new(Script::default()))))
}

#[test]
fn prewarm_fills_the_standby_slot_once() {
  static STANDBY: native::Standby = native::Standby::new();
  native::prewarm_with(&STANDBY, scripted_host)
    .expect("launcher thread")
    .join()
    .expect("launcher");
  // A waiting worker is not replaced by a second fill.
  native::prewarm_with(&STANDBY, || -> Result<Box<dyn Host>, EngineError> {
    Err(EngineError::protocol("a full slot never launches again"))
  })
  .expect("launcher thread")
  .join()
  .expect("launcher");
  assert!(STANDBY.take().is_some());
  assert!(STANDBY.take().is_none());
}

#[test]
fn a_recording_takes_the_standby_worker_and_otherwise_launches_one() {
  let standby = native::Standby::new();
  let launched = AtomicBool::new(false);
  let launch = || {
    launched.store(true, Ordering::Release);
    scripted_host()
  };
  assert!(native::take_or_launch(&standby, launch).is_ok());
  assert!(
    launched.swap(false, Ordering::AcqRel),
    "no standby: launched on demand"
  );
  standby.fill_now(scripted_host);
  assert!(native::take_or_launch(&standby, launch).is_ok());
  assert!(
    !launched.load(Ordering::Acquire),
    "the standby worker was used"
  );
  let error = native::take_or_launch(&standby, || {
    Err(EngineError::new(ErrorKind::Timeout, "hello", "scripted"))
  })
  .err()
  .expect("launch failure is reported");
  assert_eq!(error.kind, ErrorKind::Timeout);
}

#[test]
fn a_poisoned_standby_slot_launches_on_demand_instead_of_failing() {
  let standby = Arc::new(native::Standby::new());
  standby.fill_now(scripted_host);
  // A thread that panics while holding the slot lock poisons it.
  let poisoned = standby.clone();
  let _ = std::thread::spawn(move || poisoned.poison_for_test()).join();
  assert!(standby.take().is_none(), "a poisoned slot yields no worker");
  standby.fill_now(|| panic!("a poisoned slot must not launch"));
}

#[test]
fn the_standby_launcher_fails_within_its_hello_deadline_outside_a_worker() {
  let started = std::time::Instant::now();
  let error = native::launch_standby_worker()
    .err()
    .expect("the test harness is not an engine worker");
  assert!(
    started.elapsed() < flinch_video_engine::limits::HELLO_TIMEOUT + Duration::from_secs(2),
    "{:?}",
    started.elapsed()
  );
  assert!(
    matches!(
      error.kind,
      ErrorKind::Timeout | ErrorKind::Protocol | ErrorKind::Io
    ),
    "{error}"
  );
}

#[test]
fn the_session_summary_names_what_auto_resolved_to_and_why_tiers_were_skipped() {
  use flinch_video_engine::types::{
    CaptureKind, CaptureRejection, CaptureTierReport, ConversionKind, EncoderKind, TierRejection,
    TierReport,
  };
  let snapshot = SessionSnapshot {
    tier: Some(TierReport {
      requested: EncoderKind::Auto,
      selected: Some(EncoderKind::MediaFoundation),
      rejected: vec![TierRejection {
        encoder: EncoderKind::Amf,
        error: EngineError::new(ErrorKind::Capability, "AMF runtime", "not installed"),
      }],
    }),
    capture_tier: Some(CaptureTierReport {
      requested: CaptureKind::Auto,
      selected: Some(CaptureKind::Dxgi),
      rejected: vec![CaptureRejection {
        capture: CaptureKind::Wgc,
        error: EngineError::new(ErrorKind::Capability, "WGC support", "unsupported"),
      }],
    }),
    conversion: Some(ConversionKind::Gpu),
    ..SessionSnapshot::default()
  };

  assert_eq!(
    native::SessionSummary::of(&snapshot),
    native::SessionSummary {
      encoder: "MediaFoundation".into(),
      encoder_rejected: "Amf: AMF runtime: not installed (Capability)".into(),
      capture: "Dxgi".into(),
      capture_rejected: "Wgc: WGC support: unsupported (Capability)".into(),
      conversion: "Gpu".into(),
    }
  );
  let unreported = native::SessionSummary::of(&SessionSnapshot::default());
  assert_eq!(unreported.encoder, "unreported");
  assert_eq!(unreported.capture, "unreported");
  assert_eq!(unreported.conversion, "unreported");
  assert_eq!(unreported.encoder_rejected, "none");
  assert_eq!(unreported.capture_rejected, "none");
}

/// Log output of `run`, captured on this thread only.
fn captured_logs(run: impl FnOnce()) -> String {
  let output = tempfile::NamedTempFile::new().expect("log file");
  let writer = output.reopen().expect("log writer");
  let subscriber = tracing_subscriber::fmt::Subscriber::builder()
    .with_max_level(tracing::Level::INFO)
    .with_ansi(false)
    .with_writer(move || writer.try_clone().expect("clone log writer"))
    .finish();
  tracing::subscriber::with_default(subscriber, run);
  std::fs::read_to_string(output.path()).expect("read log")
}

fn field(line: &str, name: &str) -> u64 {
  let start = line
    .find(&format!(" {name}="))
    .unwrap_or_else(|| panic!("{name} missing from {line}"))
    + name.len()
    + 2;
  line[start..]
    .split(|c: char| !c.is_ascii_digit())
    .next()
    .and_then(|value| value.parse().ok())
    .unwrap_or_else(|| panic!("{name} is not a number in {line}"))
}

#[test]
fn pauses_and_the_session_clock_are_logged_so_media_length_is_provable() {
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig::default());
  let logs = captured_logs(|| {
    start(
      &mut recorder,
      &fixture,
      CaptureConfig::fullscreen(640, 360, 30),
    )
    .expect("start");
    stub_inspection(&mut recorder);
    recorder.pause_recording().expect("pause");
    std::thread::sleep(Duration::from_millis(40));
    recorder.resume_recording().expect("resume");
    recorder.pause_recording().expect("second pause");
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&fixture.engine_output, b"native media").expect("engine output");
    *fixture.script.stop.lock().expect("stop") = Some(outcome(
      OutcomeKind::Completed,
      Some(fixture.engine_output.clone()),
      None,
    ));
    // Stopping while paused still counts the open pause.
    recorder.stop_recording().expect("stop");
  });

  let line = |message: &str, nth: usize| {
    logs
      .lines()
      .filter(|line| line.contains(message))
      .nth(nth)
      .unwrap_or_else(|| panic!("no {message:?} #{nth} in:\n{logs}"))
      .to_owned()
  };
  assert_eq!(field(&line("Native recording paused", 0), "pause"), 1);
  assert_eq!(field(&line("Native recording paused", 1), "pause"), 2);
  let resumed = line("Native recording resumed", 0);
  assert_eq!(field(&resumed, "pause"), 1);
  assert!(field(&resumed, "paused_ms") >= 40, "{resumed}");
  assert_eq!(logs.matches("Native recording resumed").count(), 1);

  let session = line("Native recording session", 0);
  assert_eq!(
    field(&session, "media_ms"),
    100,
    "engine claim of 1_000_000 x 100 ns"
  );
  assert_eq!(field(&session, "pauses"), 2);
  assert!(field(&session, "paused_ms") >= 60, "{session}");
  // Present and a plain count of milliseconds; its value is real elapsed test time.
  field(&session, "active_ms");
}

#[test]
fn the_engines_last_frame_picture_is_kept_for_the_saved_file_only_from_its_session() {
  use flinch_video_engine::thumbnail::{self, Pixels};
  let pixels = Pixels {
    width: 2,
    height: 1,
    rgba: vec![1, 2, 3, 255, 4, 5, 6, 255],
  };
  for inside in [true, false] {
    let fixture = fixture();
    let session = fixture.temp.parent().expect("session").to_path_buf();
    *fixture.script.directory.lock().expect("directory") = Some(session.clone());
    let mut recorder = VideoRecorder::new(VideoConfig::default());
    start(
      &mut recorder,
      &fixture,
      CaptureConfig::fullscreen(640, 360, 30),
    )
    .expect("start");
    stub_inspection(&mut recorder);
    std::fs::write(&fixture.engine_output, b"native media").expect("engine output");
    // A picture outside the session directory is never read, whatever it holds.
    let folder = if inside {
      session.clone()
    } else {
      fixture
        .destination
        .parent()
        .expect("elsewhere")
        .to_path_buf()
    };
    let written = thumbnail::write(&folder, &pixels).expect("thumbnail");
    let mut stopped = outcome(
      OutcomeKind::Completed,
      Some(fixture.engine_output.clone()),
      None,
    );
    stopped.thumbnail = Some(written);
    *fixture.script.stop.lock().expect("stop") = Some(stopped);
    assert_eq!(
      recorder.stop_recording().expect("stop"),
      fixture.destination
    );
    let remembered = crate::thumbnails::take_remembered(&fixture.destination);
    if inside {
      let remembered = remembered.expect("remembered for the saved file");
      assert_eq!(
        (remembered.width, remembered.height, remembered.rgba),
        (2, 1, pixels.rgba.clone())
      );
    } else {
      assert!(remembered.is_none(), "foreign thumbnail must be ignored");
    }
  }
}

/// A file with the claimed video and an AAC track ending with it.
fn matching_file_with_audio(_: &Path) -> Result<Inspection, EngineError> {
  Ok(inspection(&[
    (b"vide", b"avc1", 3, 1_000_000),
    (b"soun", b"mp4a", 5, 1_000_000),
  ]))
}

#[test]
fn a_microphone_without_system_audio_publishes_its_audio_track() {
  // Regression: the Stop check expected an audio track only with system audio, so a recording
  // with just the microphone failed with "2 tracks instead of 1" and was not saved.
  use crate::video::config::{AudioConfig, MicrophoneConfig};
  let config = VideoConfig {
    audio: AudioConfig { enabled: false },
    microphone: Some(MicrophoneConfig { device: None }),
    ..VideoConfig::default()
  };
  assert!(config.audio_track());
  assert!(!VideoConfig::default().audio_track());
  let fixture = fixture();
  let mut recorder = VideoRecorder::new(config);
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  recorder
    .runtime
    .native
    .as_mut()
    .expect("native session")
    .inspect = matching_file_with_audio;
  let spec = fixture
    .script
    .spec
    .lock()
    .expect("spec")
    .clone()
    .expect("prepared");
  assert!(spec.microphone && !spec.audio && spec.audio_track());
  std::fs::write(&fixture.engine_output, b"native media").expect("engine output");
  *fixture.script.stop.lock().expect("stop") = Some(outcome(
    OutcomeKind::Completed,
    Some(fixture.engine_output.clone()),
    None,
  ));
  assert_eq!(
    recorder
      .stop_recording()
      .expect("microphone recording saved"),
    fixture.destination
  );
}

#[test]
fn the_microphone_mutes_and_unmutes_only_while_a_recording_with_one_runs() {
  use super::MicrophoneState;
  use crate::video::config::{AudioConfig, MicrophoneConfig};
  let idle = &mut VideoRecorder::new(VideoConfig::default());
  assert!(matches!(
    idle.set_microphone(false),
    Err(VideoError::NotRecording)
  ));
  assert_eq!(idle.microphone(), None);

  // A recording without a microphone has no button and refuses the switch.
  let first = fixture();
  let mut plain = VideoRecorder::new(VideoConfig::default());
  start(&mut plain, &first, CaptureConfig::fullscreen(640, 360, 30)).expect("start");
  assert_eq!(plain.microphone(), None);
  assert!(matches!(
    plain.set_microphone(false),
    Err(VideoError::ConfigError(_))
  ));

  let fixture = fixture();
  let mut recorder = VideoRecorder::new(VideoConfig {
    audio: AudioConfig { enabled: false },
    microphone: Some(MicrophoneConfig { device: None }),
    ..VideoConfig::default()
  });
  start(
    &mut recorder,
    &fixture,
    CaptureConfig::fullscreen(640, 360, 30),
  )
  .expect("start");
  assert_eq!(recorder.microphone(), Some(MicrophoneState::Audible));
  recorder.set_microphone(false).expect("muted");
  assert_eq!(recorder.microphone(), Some(MicrophoneState::Muted));
  // A refusal keeps the recording and the microphone as they were.
  fixture
    .script
    .microphone_refused
    .store(true, Ordering::Release);
  assert!(matches!(
    recorder.set_microphone(true),
    Err(VideoError::CaptureFailed(_))
  ));
  assert_eq!(recorder.microphone(), Some(MicrophoneState::Muted));
  assert_eq!(recorder.status, RecordingStatus::Recording);
  fixture
    .script
    .microphone_lost
    .store(true, Ordering::Release);
  assert_eq!(recorder.microphone(), Some(MicrophoneState::Lost));
}
