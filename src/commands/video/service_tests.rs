use super::*;
use crate::{app::context::AppHandle, config::Config, state::VideoRecorderState};
use std::time::Duration;

fn app() -> AppHandle {
  AppHandle::isolated(
    Config {
      language: "en".into(),
      notifications_enabled: false,
      notification_sound: false,
      ..Default::default()
    },
    std::path::PathBuf::new(),
  )
}

#[tokio::test]
async fn synthetic_recording_commands_pause_resume_and_publish_only_a_valid_final_capture() {
  use crate::recording_visibility::RecordingPresentationMode;
  use crate::video::{
    capture::CaptureConfig,
    config::{AudioConfig, VideoConfig, VideoQuality},
  };
  let app = app();
  let folder = tempfile::tempdir().unwrap();
  let output = folder.path().join("saved.mp4");
  let (recorder, source) = crate::video::controller::ScriptedSession::new();
  *app.state::<VideoRecorderState>().inner().0.lock().unwrap() = recorder;
  let controls = std::sync::Arc::new(crate::state::RecordingControls::new());
  app
    .try_state::<crate::state::RecordingHudState>()
    .unwrap()
    .set(controls.clone());
  let prepared = startup::PreparedVideoStart {
    output_path: output.clone(),
    temp_output_path: start::recording_temp_path(&output),
    capture_config: CaptureConfig::area(0, 0, 64, 64, 30),
    video_config: VideoConfig {
      fps: 30,
      hardware_encoding: false,
      quality: VideoQuality::Low,
      audio: AudioConfig { enabled: false },
      ..Default::default()
    },
    gif_options: None,
    active_kind: crate::state::ActiveRecordingKind::Video,
  };
  let temp = prepared.temp_output_path.clone();
  assert_eq!(
    startup::launch_video_recorder(&app, prepared.clone(), RecordingPresentationMode::Silent)
      .await
      .unwrap(),
    output
  );
  assert!(!output.exists());
  let updates = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let observed = updates.clone();
  let supervisor = tokio::spawn(startup::supervise_recording_hud(
    app.clone(),
    controls.clone(),
    move |_, _| {
      observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      std::future::ready(Ok(()))
    },
  ));
  async fn wait_for(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
      while !ready() {
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
    })
    .await
    .unwrap();
  }
  wait_for(|| controls.ready.load(std::sync::atomic::Ordering::SeqCst)).await;
  // A command holding the recorder makes the supervisor skip its ticks instead of waiting.
  let (held, holding) = std::sync::mpsc::channel();
  let (release, released) = std::sync::mpsc::channel::<()>();
  let holder = std::thread::spawn({
    let app = app.clone();
    move || {
      let state = app.state::<VideoRecorderState>();
      let _recorder = state.inner().0.lock().unwrap();
      held.send(()).unwrap();
      released.recv_timeout(Duration::from_secs(10)).unwrap();
    }
  });
  holding.recv_timeout(Duration::from_secs(10)).unwrap();
  let before = updates.load(std::sync::atomic::Ordering::Relaxed);
  // Three supervisor intervals.
  tokio::time::sleep(Duration::from_millis(350)).await;
  assert_eq!(updates.load(std::sync::atomic::Ordering::Relaxed), before);
  release.send(()).unwrap();
  holder.join().unwrap();
  wait_for(|| updates.load(std::sync::atomic::Ordering::Relaxed) > before).await;
  source.finish_segment(&mut app.state::<VideoRecorderState>().inner().0.lock().unwrap());
  assert!(
    status::get_recording_status(app.clone())
      .unwrap()
      .duration_ms
      >= 2000
  );
  assert!(
    startup::launch_video_recorder(&app, prepared, RecordingPresentationMode::Silent)
      .await
      .unwrap_err()
      .contains("config")
  );
  controls.request_pause(true);
  wait_for(|| controls.paused()).await;
  assert!(controls.paused());
  assert!(
    crate::commands::current_recording_session(&app)
      .unwrap()
      .paused
  );
  assert!(!output.exists());
  controls.request_pause(false);
  wait_for(|| !controls.paused()).await;
  assert!(!controls.paused());
  source.finish_segment(&mut app.state::<VideoRecorderState>().inner().0.lock().unwrap());
  controls.request_stop();
  tokio::time::timeout(Duration::from_secs(10), supervisor)
    .await
    .unwrap()
    .unwrap();
  assert!(updates.load(std::sync::atomic::Ordering::Relaxed) >= 2);
  assert!(controls.stop_requested());
  assert!(crate::commands::current_recording_session(&app).is_none());
  let notices = tokio::time::timeout(Duration::from_secs(30), async {
    let mut notices = Vec::new();
    loop {
      notices.extend(app.try_state::<crate::overlay::notification::NotificationManager>().unwrap().take_test_requests());
      if notices.iter().any(|notice| matches!(&notice.click_action, Some(crate::overlay::notification::ClickAction::OpenFolder(path)) if path == &output)) { break notices; }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  }).await.unwrap();
  assert!(notices.iter().all(|notice| !notice.critical));
  assert!(output.is_file());
  assert!(!temp.parent().unwrap().exists());
  assert!(matches!(
    status::get_recording_status(app.clone()).unwrap().status,
    RecordingLifecycleStatus::Idle
  ));
  // Two recorded segments around a pause: the paused time is not recorded.
  let seconds = crate::video::controller::ScriptedSession::recorded_seconds(&output).unwrap();
  assert!((4.0..5.0).contains(&seconds), "{seconds}");
}

#[tokio::test]
async fn rejected_start_requests_and_idle_stop_never_initialize_capture_or_publish_files() {
  let app = app();
  for (w, h, fps) in [
    (0, 10, 30),
    (10, 0, 30),
    (16385, 10, 30),
    (10, 16385, 30),
    (10, 10, 61),
  ] {
    let error = start::start_video_capture_session(
      &app,
      0,
      0,
      w,
      h,
      fps,
      None,
      RecordingMode::Area,
      crate::recording_visibility::RecordingPresentationMode::Silent,
    )
    .await
    .unwrap_err();
    assert!(error.contains("Invalid"));
  }
  assert!(
    start::start_video_recording(app.clone(), 0, 0, 10, 10, 30, "not-a-mode".into())
      .await
      .unwrap_err()
      .contains("Invalid recording mode")
  );
  for _ in 0..3 {
    let response = stop::stop_video_recording(app.clone()).await.unwrap();
    assert!(matches!(response.status, RecordingLifecycleStatus::Idle));
    assert!(stop::stop_video_recorder_blocking(&app).unwrap().is_none());
    assert!(
      tokio::time::timeout(Duration::from_secs(1), stop::stop_video_recorder_raw(&app))
        .await
        .unwrap()
        .unwrap()
        .is_none()
    );
    let status = status::get_recording_status(app.clone()).unwrap();
    assert!(matches!(status.status, RecordingLifecycleStatus::Idle));
    assert_eq!(
      (
        status.duration_ms,
        status.frames_captured,
        status.file_size_bytes
      ),
      (0, 0, 0)
    );
  }
  assert!(
    app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .unwrap()
      .take_test_requests()
      .is_empty()
  );
}

#[tokio::test]
async fn control_failure_cleans_hud_session_and_delivers_critical_notice_even_when_notifications_disabled()
 {
  for pause in [false, true] {
    let app = app();
    let controls = std::sync::Arc::new(crate::state::RecordingControls::new());
    app
      .try_state::<crate::state::RecordingHudState>()
      .unwrap()
      .set(controls.clone());
    app
      .try_state::<crate::state::RecordingSessionState>()
      .unwrap()
      .set(
        crate::state::ActiveRecordingKind::Video,
        crate::recording_visibility::RecordingPresentationMode::Silent,
      );
    let result = if pause {
      control::pause_video_recording(app.clone()).await
    } else {
      control::resume_video_recording(app.clone()).await
    };
    assert!(result.unwrap_err().contains(if pause {
      "Failed to pause recording"
    } else {
      "Failed to resume recording"
    }));
    assert!(
      app
        .try_state::<crate::state::RecordingSessionState>()
        .unwrap()
        .snapshot()
        .is_none()
    );
    assert!(
      app
        .try_state::<crate::state::RecordingHudState>()
        .unwrap()
        .controls
        .lock()
        .unwrap()
        .is_none()
    );
    let notices = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .unwrap()
      .take_test_requests();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].critical);
    assert!(!notices[0].body.is_empty());
  }
}

#[tokio::test]
async fn poisoned_recorder_errors_retain_service_context_and_never_report_idle_success() {
  let app = app();
  let recorder = app.state::<crate::state::VideoRecorderState>().0.clone();
  // Idle, the microphone switch is refused and reports no microphone.
  assert_eq!(
    control::set_microphone(app.clone(), false).await,
    Some(None)
  );
  assert!(
    std::thread::spawn(move || {
      let _guard = recorder.lock().unwrap();
      panic!("injected recorder poison");
    })
    .join()
    .is_err()
  );
  assert!(
    status::get_recording_status(app.clone())
      .unwrap_err()
      .contains("Video recorder state poisoned")
  );
  assert!(
    stop::stop_video_recorder_blocking(&app)
      .unwrap_err()
      .contains("Recorder lock poisoned")
  );
  assert!(
    stop::stop_video_recording(app.clone())
      .await
      .unwrap_err()
      .contains("Recorder lock poisoned")
  );
  assert!(
    control::pause_video_recording(app.clone())
      .await
      .unwrap_err()
      .contains("Video recorder state poisoned")
  );
  assert_eq!(control::set_microphone(app.clone(), true).await, None);
  let notices = app
    .try_state::<crate::overlay::notification::NotificationManager>()
    .unwrap()
    .take_test_requests();
  // The failed stop and the failed pause each tell the user; neither fails silently.
  assert_eq!(notices.len(), 2);
  assert!(notices.iter().all(|notice| notice.critical));
}

#[tokio::test]
async fn saving_does_not_block_queries_or_queue_duplicate_controls() {
  let app = app();
  crate::commands::set_recording_session(
    &app,
    crate::state::ActiveRecordingKind::Video,
    crate::recording_visibility::RecordingPresentationMode::Silent,
  );
  let state = app.state::<VideoRecorderState>();
  let operation = state.claim_operation().unwrap();
  assert!(state.claim_operation().is_err());
  let recorder = state.inner().0.clone();
  let (ready_tx, ready_rx) = std::sync::mpsc::channel();
  let (release_tx, release_rx) = std::sync::mpsc::channel();
  let owner = std::thread::spawn(move || {
    let _guard = recorder.lock().unwrap();
    ready_tx.send(()).unwrap();
    release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
  });
  ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  let started = std::time::Instant::now();
  assert!(matches!(
    status::get_recording_status(app.clone()).unwrap().status,
    RecordingLifecycleStatus::Processing
  ));
  assert!(crate::commands::current_recording_session(&app).is_some());
  assert!(matches!(
    stop::stop_video_recording(app.clone())
      .await
      .unwrap()
      .status,
    RecordingLifecycleStatus::Processing
  ));
  assert!(
    control::pause_video_recording(app.clone())
      .await
      .unwrap_err()
      .contains("in progress")
  );
  assert!(
    control::resume_video_recording(app.clone())
      .await
      .unwrap_err()
      .contains("in progress")
  );
  assert!(started.elapsed() < Duration::from_millis(250));
  release_tx.send(()).unwrap();
  owner.join().unwrap();
  drop(operation);
  assert!(state.claim_operation().is_ok());
}

#[tokio::test]
async fn stop_survives_a_transient_monitor_read_without_blocking_the_async_executor() {
  let app = app();
  let state = app.state::<VideoRecorderState>();
  let recorder = state.inner().0.clone();
  let (send, receive) = std::sync::mpsc::channel();
  let reader = std::thread::spawn(move || {
    let _guard = recorder.lock().unwrap();
    send.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(75));
  });
  receive.recv_timeout(Duration::from_secs(1)).unwrap();
  let stop = stop::stop_video_recording(app);
  let heartbeat = async {
    tokio::time::sleep(Duration::from_millis(10)).await;
  };
  let (response, ()) = tokio::join!(stop, heartbeat);
  assert!(matches!(
    response.unwrap().status,
    RecordingLifecycleStatus::Idle
  ));
  reader.join().unwrap();
}
