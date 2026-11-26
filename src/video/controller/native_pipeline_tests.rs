//! The native recorder end to end, quietly. The app's recorder drives a real engine worker (the
//! built `flinch.exe --video-engine-worker`, the production entry point) whose only substitution
//! is its source: the engine's synthetic screen and synthetic audio, which number every frame and
//! pulse once a second in both tracks. Real files are published and decoded independently. No
//! capture device, playback, window or global input is touched; displays are only enumerated.
//!
//! The worker binary must exist, so these are ignored by plain `cargo test`.
//! `cargo xtask test:recording` builds it and runs them.

use super::native::{self, Host};
use super::{RecordingStatus, VideoRecorder};
use crate::video::VideoError;
use crate::video::capture::CaptureConfig;
use crate::video::config::{AudioConfig, RecordingProfile, VideoConfig, VideoQuality};
use flinch_video_engine::protocol::{Request, Response};
use flinch_video_engine::types::{CaptureKind, RecordingOutcome, SessionSnapshot, SessionSpec};
use flinch_video_engine::windows::host::{HostSession, Transport};
use flinch_video_engine::windows::supervisor::WorkerProcess;
use flinch_video_engine::windows::verify::{Verification, verify};
use flinch_video_engine::{EngineError, Result as EngineResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The production worker in the test binary's profile directory, `target/<profile>/flinch.exe`.
pub(super) fn worker() -> PathBuf {
  let test = std::env::current_exe().expect("test binary path");
  worker_above(&test).unwrap_or_else(|| {
    panic!(
      "The native pipeline needs the worker binary flinch.exe in a directory above {}; run cargo xtask test:recording, which builds it",
      test.display()
    )
  })
}

/// The nearest `flinch.exe` in a directory above `test`. Test binaries sit in
/// `target/<profile>/deps/`, or since Rust 1.100's build-directory layout in
/// `target/<profile>/build/flinch/<hash>/out/`; the worker is built into `target/<profile>/`.
fn worker_above(test: &Path) -> Option<PathBuf> {
  test
    .ancestors()
    .skip(1)
    .map(|dir| dir.join("flinch.exe"))
    .find(|path| path.is_file())
}

#[test]
fn the_worker_is_found_from_either_test_binary_layout() {
  let target = tempfile::tempdir().expect("target directory");
  let profile = target.path().join("debug");
  let deps = profile.join("deps");
  let out = profile
    .join("build")
    .join("flinch")
    .join("0123abcd")
    .join("out");
  std::fs::create_dir_all(&deps).expect("deps directory");
  std::fs::create_dir_all(&out).expect("build output directory");
  assert_eq!(worker_above(&deps.join("flinch-0123abcd.exe")), None);
  let built = profile.join("flinch.exe");
  std::fs::write(&built, b"worker").expect("worker stand-in");
  assert_eq!(
    worker_above(&deps.join("flinch-0123abcd.exe")),
    Some(built.clone())
  );
  assert_eq!(worker_above(&out.join("flinch-0123abcd.exe")), Some(built));
}

thread_local! {
  /// What the last Stop on this test thread reported; the recorder calls Stop on its caller.
  static LAST_SUMMARY: std::cell::Cell<Option<native::SessionSummary>> =
    const { std::cell::Cell::new(None) };
}

/// The real worker with only its source replaced: the engine's synthetic screen and audio.
struct Synthetic(HostSession);
impl Host for Synthetic {
  fn prepare(&self, spec: SessionSpec) -> Result<PathBuf, EngineError> {
    self.0.prepare(SessionSpec {
      capture: CaptureKind::Synthetic,
      ..spec
    })
  }
  fn start(&self) -> Result<(), EngineError> {
    self.0.start()
  }
  fn pause(&self) -> Result<(), EngineError> {
    self.0.pause()
  }
  fn resume(&self) -> Result<(), EngineError> {
    self.0.resume()
  }
  fn stop(&self) -> Result<RecordingOutcome, EngineError> {
    let outcome = self.0.stop()?;
    LAST_SUMMARY.set(Some(native::SessionSummary::of(&outcome.snapshot)));
    Ok(outcome)
  }
  fn snapshot(&self) -> SessionSnapshot {
    self.0.snapshot()
  }
  fn failure(&self) -> Option<EngineError> {
    self.0.failure()
  }
  fn terminal_outcome(&self) -> Option<RecordingOutcome> {
    self.0.terminal_outcome()
  }
  fn take_diagnostics(&self) -> (Vec<String>, u64) {
    self.0.take_diagnostics()
  }
}

/// The worker's own process transport, which can end that one child abruptly, as a crash would:
/// once `crash` is set, the next command finds the worker gone. No other process is touched.
struct Crashable {
  worker: WorkerProcess,
  crash: Arc<AtomicBool>,
}
impl Transport for Crashable {
  fn send(&mut self, request: &Request) -> EngineResult<()> {
    if self.crash.load(Ordering::Acquire) {
      self.worker.terminate()?;
    }
    self.worker.send(request)
  }
  fn receive(&self, timeout: Duration) -> EngineResult<Option<Response>> {
    self.worker.receive(timeout)
  }
  fn wait(&mut self, timeout: Duration) -> EngineResult<()> {
    self.worker.wait(timeout)
  }
  fn terminate(&mut self) -> EngineResult<()> {
    self.worker.terminate()
  }
  fn diagnostic(&self) -> Option<String> {
    self.worker.diagnostic()
  }
}

fn launch(crash: Option<Arc<AtomicBool>>) -> Result<Box<dyn Host>, EngineError> {
  let worker = worker();
  let session = match crash {
    Some(crash) => HostSession::with_transport(Crashable {
      worker: WorkerProcess::spawn_with(&worker, &[crate::video::native::WORKER_SWITCH])?,
      crash,
    })?,
    None => HostSession::launch(&worker, &[crate::video::native::WORKER_SWITCH])?,
  };
  Ok(Box::new(Synthetic(session)))
}

/// A recording's files: the destination and the session's temporary folder, removed when the
/// test ends. Destinations are free names in the app (`next_available_path`) and publication
/// never replaces a file, so the destination must not exist until a successful publish.
pub(super) struct Session {
  dir: Option<tempfile::TempDir>,
  pub(super) destination: PathBuf,
  pub(super) temp: PathBuf,
}
impl Session {
  pub(super) fn new(name: &str, gif: bool) -> Self {
    let dir = tempfile::tempdir().expect("temporary folder");
    let destination = dir.path().join(name);
    let temp = if gif {
      crate::gif::build_intermediate_output_path(&destination)
    } else {
      crate::commands::video::start::recording_temp_path(&destination)
    };
    Self {
      dir: Some(dir),
      destination,
      temp,
    }
  }
  /// Nothing has been published at the destination.
  pub(super) fn untouched(&self) -> bool {
    !self.destination.exists()
  }
}
impl Drop for Session {
  fn drop(&mut self) {
    // A failed test keeps the engine journal and files to diagnose; the run is expensive to repeat.
    if std::thread::panicking() {
      if let Some(dir) = self.dir.take() {
        eprintln!(
          "Kept {} and {} for diagnosis",
          dir.keep().display(),
          self.temp.display()
        );
      }
      return;
    }
    crate::capture_artifact::remove_recording_session_temp_dir(&self.temp, "native pipeline test");
  }
}

/// The primary display's top-left `width` x `height`: the synthetic frame code sits there, and
/// the verifier needs at least 512 px of it.
fn area(width: u32, height: u32, fps: u32) -> (CaptureConfig, Vec<crate::monitors::MonitorInfo>) {
  let monitors = crate::monitors::list_monitors();
  let primary = monitors
    .iter()
    .find(|monitor| monitor.is_primary)
    .expect("an attached primary display (enumerated only; nothing is captured)");
  assert!(primary.width >= width && primary.height >= height);
  (
    CaptureConfig::area(primary.x, primary.y, width, height, fps),
    monitors,
  )
}

pub(super) fn recorder(fps: u32, audio: bool, hardware: bool) -> VideoRecorder {
  VideoRecorder::new(VideoConfig {
    fps,
    hardware_encoding: hardware,
    quality: VideoQuality::Medium,
    audio: AudioConfig { enabled: audio },
    ..VideoConfig::default()
  })
}

fn start(
  recorder: &mut VideoRecorder,
  session: &Session,
  capture: CaptureConfig,
  monitors: &[crate::monitors::MonitorInfo],
  crash: Option<Arc<AtomicBool>>,
) {
  native::start(
    recorder,
    capture,
    session.temp.clone(),
    session.destination.clone(),
    monitors,
    move || launch(crash),
  )
  .expect("native recording starts");
  assert_eq!(recorder.status(), RecordingStatus::Recording);
}

/// Record `segments`, pausing `pause` between them, checking the recorder stays healthy.
pub(super) fn record(recorder: &mut VideoRecorder, segments: &[Duration], pause: Duration) {
  for (index, segment) in segments.iter().enumerate() {
    if index > 0 {
      recorder.resume_recording().expect("resume");
    }
    let deadline = Instant::now() + *segment;
    while Instant::now() < deadline {
      std::thread::sleep(Duration::from_millis(250).min(*segment));
      let stats = recorder.stats();
      assert_eq!(stats.status, RecordingStatus::Recording, "{stats:?}");
    }
    if index + 1 < segments.len() {
      recorder.pause_recording().expect("pause");
      std::thread::sleep(pause);
      assert_eq!(recorder.status(), RecordingStatus::Paused);
    }
  }
}

/// The independent decode of a published recording of `seconds` of media at `fps`.
fn decoded(path: &Path, fps: u32, seconds: f64, audio: bool, size: (u32, u32)) -> Verification {
  let report = verify(path, true).expect("independent decode");
  // Diagnostics only: markers are dated by container position; CI logs show each machine's drift.
  eprintln!("Decoder clock: {:?}", report.decoder_clock);
  assert_eq!((report.video_width, report.video_height), size);
  // Every frame carries its synthetic number, in order and never repeated.
  assert_eq!(report.numbered_frames_checked, report.video_frames);
  assert_eq!(report.repeated_source_frames, 0, "{report:?}");
  let expected = seconds * f64::from(fps);
  let frames = report.video_frames as f64;
  assert!(
    (frames - expected).abs() <= f64::from(fps) / 2.0 + 2.0,
    "{frames} frames for {seconds} s at {fps} FPS"
  );
  if audio {
    assert!(report.audio_frames > 0, "{report:?}");
    // Markers start after half a second of silence and recur every second of media.
    assert!(
      report.marker_pairs as f64 >= seconds - 2.0,
      "{} marker pairs in {seconds} s",
      report.marker_pairs
    );
    assert!(report.accepted_sync, "{:?}", report.sync_failure);
  } else {
    assert_eq!(report.audio_frames, 0);
  }
  report
}

/// Slots the engine reports losing. Pausing first freezes the count; the worker reports it once a
/// second, so the read waits for one more report.
fn settled_losses(recorder: &mut VideoRecorder) -> u64 {
  recorder
    .pause_recording()
    .expect("pause before reading losses");
  std::thread::sleep(Duration::from_millis(1500));
  let stats = recorder.stats();
  eprintln!(
    "Lost slots: {} of {} encoded frames",
    stats.frames_dropped, stats.frames_encoded
  );
  stats.frames_dropped
}

pub(super) fn media_seconds(segments: &[Duration]) -> f64 {
  segments.iter().map(Duration::as_secs_f64).sum()
}

#[test]
#[ignore = "needs the built worker; run cargo xtask test:recording"]
fn a_started_worker_ends_the_windows_launch_cursor() {
  // Regression: the GUI-subsystem worker never waited for input, so Windows showed
  // the busy launch cursor for seconds after every app start and recording. Windows ends that
  // cursor when the process becomes input idle, which WaitForInputIdle observes.
  use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
  use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE, WaitForInputIdle,
  };
  let mut worker = WorkerProcess::spawn_with(&worker(), &[crate::video::native::WORKER_SWITCH])
    .expect("worker started");
  let hello = worker
    .receive(Duration::from_secs(10))
    .expect("worker output")
    .expect("Hello before the deadline");
  assert!(
    matches!(hello.event, flinch_video_engine::protocol::Event::Hello),
    "{hello:?}"
  );
  // SAFETY: the worker is alive (it just said Hello); the handle is closed below.
  let process = unsafe {
    OpenProcess(
      PROCESS_QUERY_INFORMATION | PROCESS_SYNCHRONIZE,
      false,
      worker.pid(),
    )
  }
  .expect("worker process handle");
  // SAFETY: a valid process handle; the wait is bounded.
  let idle = unsafe { WaitForInputIdle(process, 2_000) };
  // SAFETY: the handle came from OpenProcess and is closed once.
  unsafe { CloseHandle(process) }.expect("close worker handle");
  worker.terminate().expect("worker ended");
  assert_eq!(idle, WAIT_OBJECT_0.0, "worker never became input idle");
}

#[test]
#[ignore = "needs the built worker; run cargo xtask test:recording"]
fn short_recordings_publish_every_frame_and_their_audio() {
  // Software encoding, with and without audio, at both app rates. Automatic selection picks a
  // different encoder per machine (AMF here, software on a GPU-less runner), which would make the
  // exact coverage gate machine-dependent; native_desktop_tests covers it on real hardware.
  for (fps, audio, hardware) in [
    (30, false, false),
    (30, true, false),
    (60, false, false),
    (60, true, false),
  ] {
    let session = Session::new("short.mp4", false);
    let (capture, monitors) = area(640, 360, fps);
    let mut recorder = recorder(fps, audio, hardware);
    let segments = [Duration::from_secs(4)];
    start(&mut recorder, &session, capture, &monitors, None);
    record(&mut recorder, &segments, Duration::ZERO);
    assert!(session.untouched(), "destination replaced before Stop");
    assert_eq!(
      recorder.stop_recording().expect("published"),
      session.destination
    );
    assert_eq!(recorder.status(), RecordingStatus::Idle);
    // The final snapshot names what the session ran on, so its log can explain it.
    let summary = LAST_SUMMARY.take().expect("Stop reported a snapshot");
    assert_ne!(summary.encoder, "unreported", "{summary:?}");
    assert_eq!(summary.capture, "Synthetic", "{summary:?}");
    assert_ne!(summary.conversion, "unreported", "{summary:?}");
    // The engine's picture of the last frame, read back from the real encoder surface, waits for
    // the saved file's notification.
    let thumbnail = crate::thumbnails::take_remembered(&session.destination)
      .expect("the engine pictured the last frame");
    assert_eq!((thumbnail.width, thumbnail.height), (256, 144));
    assert_eq!(thumbnail.rgba.len(), 256 * 144 * 4);
    assert!(
      thumbnail
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[3] == 255)
    );
    decoded(
      &session.destination,
      fps,
      media_seconds(&segments),
      audio,
      (640, 360),
    );
  }
}

#[test]
#[ignore = "needs the built worker; run cargo xtask test:recording"]
fn pauses_leave_one_continuous_numbered_timeline_with_audio_in_sync() {
  let fps = 60;
  let session = Session::new("paused.mp4", false);
  let (capture, monitors) = area(640, 360, fps);
  let mut recorder = recorder(fps, true, false);
  let segments = [
    Duration::from_secs(4),
    Duration::from_secs(3),
    Duration::from_millis(2500),
    Duration::from_secs(4),
  ];
  start(&mut recorder, &session, capture, &monitors, None);
  record(&mut recorder, &segments, Duration::from_millis(1500));
  let lost = settled_losses(&mut recorder);
  let stopped = recorder.stop_recording().expect("published");
  assert_eq!(stopped, session.destination);
  let report = decoded(
    &session.destination,
    fps,
    media_seconds(&segments),
    true,
    (640, 360),
  );
  // Paused time is not in the file: frames stay one slot apart, except across slots the engine
  // reported losing. A real-time worker on a busy machine can miss one; it never hides one.
  assert!(
    report.maximum_video_timestamp_gap_ms < 1000.0 / f64::from(fps) * (1.5 + lost as f64),
    "{} ms gap with {lost} reported lost slots",
    report.maximum_video_timestamp_gap_ms
  );
}

/// Frames and size of a GIF: the logical screen and every image descriptor, walking its blocks.
fn gif_frames(bytes: &[u8]) -> (u32, u32, usize) {
  assert!(bytes.starts_with(b"GIF89a"), "not a GIF");
  let width = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
  let height = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
  let mut at = 13;
  if bytes[10] & 0x80 != 0 {
    at += 3 << ((bytes[10] & 7) + 1);
  }
  let skip_blocks = |mut at: usize| {
    while bytes[at] != 0 {
      at += usize::from(bytes[at]) + 1;
    }
    at + 1
  };
  let mut frames = 0;
  loop {
    match bytes[at] {
      0x21 => at = skip_blocks(at + 2),
      0x2c => {
        frames += 1;
        let packed = bytes[at + 9];
        at += 10;
        if packed & 0x80 != 0 {
          at += 3 << ((packed & 7) + 1);
        }
        at = skip_blocks(at + 1);
      },
      0x3b => return (width, height, frames),
      other => panic!("unexpected GIF block {other:#04x} at {at}"),
    }
  }
}

#[test]
#[ignore = "needs the built worker; run cargo xtask test:recording"]
fn a_gif_records_encodes_and_publishes() {
  let fps = 15;
  let session = Session::new("animation.gif", true);
  let (capture, monitors) = area(640, 360, fps);
  let mut recorder = VideoRecorder::new(VideoConfig {
    recording_profile: RecordingProfile::GifIntermediate,
    ..crate::gif::build_gif_video_config(session.destination.parent().expect("folder"), fps)
  });
  recorder
    .set_gif_options(Some(crate::video::native::gif_options()))
    .expect("idle");
  start(&mut recorder, &session, capture, &monitors, None);
  record(&mut recorder, &[Duration::from_secs(3)], Duration::ZERO);
  assert_eq!(
    recorder.stop_recording().expect("published GIF"),
    session.destination
  );
  let (width, height, frames) = gif_frames(&std::fs::read(&session.destination).expect("GIF"));
  assert_eq!((width, height), (640, 360));
  // Every source frame differs, so none are merged: about three seconds at 15 FPS.
  assert!((40..=50).contains(&frames), "{frames} frames");
  // A GIF never replaces an existing file: a second one aimed at it fails and keeps the first.
  let first = std::fs::read(&session.destination).expect("first GIF");
  let (capture, monitors) = area(640, 360, fps);
  recorder
    .set_gif_options(Some(crate::video::native::gif_options()))
    .expect("idle");
  start(&mut recorder, &session, capture, &monitors, None);
  record(&mut recorder, &[Duration::from_secs(1)], Duration::ZERO);
  let error = recorder.stop_recording().expect_err("existing GIF");
  assert!(matches!(error, VideoError::EncodingFailed(_)), "{error}");
  assert_eq!(std::fs::read(&session.destination).expect("kept"), first);
}

#[test]
#[ignore = "needs the built worker; run cargo xtask test:recording"]
fn a_crashed_worker_fails_the_recording_keeps_a_recoverable_file_and_the_destination() {
  let fps = 30;
  let session = Session::new("crash.mp4", false);
  let (capture, monitors) = area(640, 360, fps);
  let mut recorder = recorder(fps, true, false);
  let crash = Arc::new(AtomicBool::new(false));
  start(
    &mut recorder,
    &session,
    capture,
    &monitors,
    Some(crash.clone()),
  );
  record(&mut recorder, &[Duration::from_secs(3)], Duration::ZERO);
  // The worker dies as the pause is sent.
  crash.store(true, Ordering::Release);
  assert!(recorder.pause_recording().is_err());
  let deadline = Instant::now() + Duration::from_secs(10);
  while recorder.stats().status != RecordingStatus::Error {
    assert!(Instant::now() < deadline, "the crash was never surfaced");
    std::thread::sleep(Duration::from_millis(50));
  }
  let error = recorder.stop_recording().expect_err("nothing to publish");
  assert!(matches!(error, VideoError::CaptureFailed(_)), "{error}");
  assert_eq!(recorder.status(), RecordingStatus::Idle);
  assert!(
    session.untouched(),
    "a failed recording replaced the destination"
  );
  // The next launch saves what the engine was writing at the still-free destination. The
  // folder moves to a private root, so the launch scan cannot meet anything else.
  let folder = session.temp.parent().expect("session folder");
  let root = session.destination.with_file_name("recordings");
  let moved = root.join(folder.file_name().expect("folder name"));
  std::fs::create_dir_all(&root).expect("private root");
  // The killed worker's handles close shortly after it dies.
  let deadline = Instant::now() + Duration::from_secs(5);
  while let Err(error) = std::fs::rename(folder, &moved) {
    assert!(
      Instant::now() < deadline,
      "session folder still held: {error}"
    );
    std::thread::sleep(Duration::from_millis(100));
  }
  let recovered = session.destination.clone();
  let no_gif = |_: &Path, _: &Path| Err("not a GIF".to_owned());
  // Process 0 stands for a later launch; the marker names this test's process.
  assert_eq!(
    crate::video::recovery::recover_all(&root, 0, &no_gif),
    [(
      moved.clone(),
      Ok(crate::video::recovery::Recovered::Saved(recovered.clone()))
    )]
  );
  assert!(!moved.exists());
  let report = verify(&recovered, true).expect("recovered decode");
  assert!(report.video_frames >= u64::from(fps), "{report:?}");
  assert_eq!(report.numbered_frames_checked, report.video_frames);
}

#[test]
#[ignore = "needs the built worker; run cargo xtask test:recording"]
fn dropping_the_recorder_mid_recording_still_publishes_a_complete_file() {
  let fps = 30;
  let session = Session::new("dropped.mp4", false);
  let (capture, monitors) = area(640, 360, fps);
  let mut recorder = recorder(fps, false, false);
  start(&mut recorder, &session, capture, &monitors, None);
  record(&mut recorder, &[Duration::from_secs(3)], Duration::ZERO);
  drop(recorder);
  decoded(&session.destination, fps, 3.0, false, (640, 360));
}
