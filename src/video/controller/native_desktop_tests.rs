//! The native recorder against the real desktop and system audio: the production worker with no
//! substitution: publish and decode, pause/resume and status transitions, back-to-back sessions,
//! a recorder dropped while recording or paused, and a GIF.
//!
//! Captures the screen and loopback audio, so every test is ignored. Run them with
//! `cargo xtask test:recording:desktop`, which builds the worker first.

use super::native::{self, Host};
use super::native_pipeline_tests::{Session, media_seconds, record, recorder, worker};
use super::{RecordingStatus, VideoRecorder};
use crate::video::capture::CaptureConfig;
use flinch_video_engine::windows::host::HostSession;
use flinch_video_engine::windows::verify::{Verification, verify};
use std::path::Path;
use std::time::Duration;

const SIZE: (u32, u32) = (1280, 720);

fn start(recorder: &mut VideoRecorder, session: &Session, fps: u32) {
  let monitors = crate::monitors::list_monitors();
  let primary = monitors
    .iter()
    .find(|monitor| monitor.is_primary)
    .expect("an attached primary display");
  assert!(primary.width >= SIZE.0 && primary.height >= SIZE.1);
  let capture = CaptureConfig {
    show_cursor: true,
    ..CaptureConfig::area(primary.x, primary.y, SIZE.0, SIZE.1, fps)
  };
  native::start(
    recorder,
    capture,
    session.temp.clone(),
    session.destination.clone(),
    &monitors,
    || {
      Ok(Box::new(HostSession::launch(
        &worker(),
        &[crate::video::native::WORKER_SWITCH],
      )?) as Box<dyn Host>)
    },
  )
  .expect("native desktop recording starts");
  assert_eq!(recorder.status(), RecordingStatus::Recording);
}

/// The independent decode of a published desktop recording. The desktop carries no frame numbers
/// or sync markers, so this checks size, frame count and the audio track's presence.
fn decoded(path: &Path, fps: u32, seconds: f64, audio: bool) -> Verification {
  let report = verify(path, false).expect("independent decode");
  assert_eq!((report.video_width, report.video_height), SIZE);
  let expected = seconds * f64::from(fps);
  let frames = report.video_frames as f64;
  assert!(
    (frames - expected).abs() <= f64::from(fps) / 2.0 + 2.0,
    "{frames} frames for {seconds} s at {fps} FPS: {report:?}"
  );
  // Loopback records silence when nothing plays, so the track exists either way.
  assert_eq!(report.audio_frames > 0, audio, "{report:?}");
  report
}

#[test]
#[ignore = "captures the real desktop and loopback audio; run cargo xtask test:recording:desktop"]
fn native_desktop_recordings_publish_and_decode() {
  // Automatic encoder selection with audio at both app rates, then the software encoder.
  for (fps, audio, hardware) in [(30, true, true), (60, true, true), (30, false, false)] {
    let session = Session::new("desktop.mp4", false);
    let mut recorder = recorder(fps, audio, hardware);
    let segments = [Duration::from_secs(4)];
    start(&mut recorder, &session, fps);
    record(&mut recorder, &segments, Duration::ZERO);
    assert!(session.untouched(), "destination replaced before Stop");
    assert_eq!(
      recorder.stop_recording().expect("published"),
      session.destination
    );
    assert_eq!(recorder.status(), RecordingStatus::Idle);
    decoded(&session.destination, fps, media_seconds(&segments), audio);
  }
}

#[test]
#[ignore = "captures the real desktop and loopback audio; run cargo xtask test:recording:desktop"]
fn native_desktop_pauses_keep_status_and_only_recorded_time() {
  let fps = 60;
  let session = Session::new("paused.mp4", false);
  let mut recorder = recorder(fps, true, true);
  let segments = [Duration::from_secs(2); 3];
  start(&mut recorder, &session, fps);
  // `record` asserts Recording during each segment and Paused during each one-second pause.
  record(&mut recorder, &segments, Duration::from_secs(1));
  recorder.pause_recording().expect("pause before Stop");
  assert_eq!(recorder.status(), RecordingStatus::Paused);
  assert_eq!(
    recorder.stop_recording().expect("published from pause"),
    session.destination
  );
  assert_eq!(recorder.status(), RecordingStatus::Idle);
  // Paused time is not media: six seconds recorded across eight of wall time.
  decoded(&session.destination, fps, media_seconds(&segments), true);
}

#[test]
#[ignore = "captures the real desktop and loopback audio; run cargo xtask test:recording:desktop"]
fn native_desktop_back_to_back_sessions_each_publish() {
  let fps = 30;
  for index in 0..3 {
    let session = Session::new(&format!("session-{index}.mp4"), false);
    let mut recorder = recorder(fps, true, true);
    start(&mut recorder, &session, fps);
    record(&mut recorder, &[Duration::from_secs(2)], Duration::ZERO);
    assert_eq!(
      recorder.stop_recording().expect("published"),
      session.destination
    );
    decoded(&session.destination, fps, 2.0, true);
  }
}

#[test]
#[ignore = "captures the real desktop and loopback audio; run cargo xtask test:recording:desktop"]
fn native_desktop_dropped_recorders_still_publish() {
  let fps = 30;
  for paused in [false, true] {
    let session = Session::new("dropped.mp4", false);
    let mut recorder = recorder(fps, true, true);
    start(&mut recorder, &session, fps);
    record(&mut recorder, &[Duration::from_secs(3)], Duration::ZERO);
    if paused {
      recorder.pause_recording().expect("pause");
      assert_eq!(recorder.status(), RecordingStatus::Paused);
    }
    drop(recorder);
    decoded(&session.destination, fps, 3.0, true);
  }
}

#[test]
#[ignore = "captures the real desktop; run cargo xtask test:recording:desktop"]
fn native_desktop_gifs_publish_at_their_size_and_recorded_duration() {
  use image::AnimationDecoder;
  let fps = 15;
  let session = Session::new("desktop.gif", true);
  let folder = session.destination.parent().expect("folder");
  let mut recorder = VideoRecorder::new(crate::gif::build_gif_video_config(folder, fps));
  recorder
    .set_gif_options(Some(crate::video::native::gif_options()))
    .expect("GIF settings");
  start(&mut recorder, &session, fps);
  record(&mut recorder, &[Duration::from_secs(3)], Duration::ZERO);
  assert_eq!(
    recorder.stop_recording().expect("published GIF"),
    session.destination
  );
  let file = std::fs::File::open(&session.destination).expect("GIF");
  let frames = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
    .expect("GIF header")
    .into_frames()
    .collect_frames()
    .expect("every GIF frame decodes");
  assert!(!frames.is_empty());
  assert!(
    frames
      .iter()
      .all(|frame| frame.buffer().dimensions() == SIZE)
  );
  // A still desktop collapses into few frames; their delays still span the recording.
  let seconds: f64 = frames
    .iter()
    .map(|frame| {
      let (numerator, denominator) = frame.delay().numer_denom_ms();
      f64::from(numerator) / f64::from(denominator) / 1000.0
    })
    .sum();
  assert!((seconds - 3.0).abs() <= 0.5, "{seconds} s of GIF for 3 s");
}
