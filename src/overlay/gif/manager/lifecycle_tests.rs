use super::*;

#[test]
fn silent_native_gif_thread_starts_and_stops_through_window_messages_using_only_synthetic_frames() {
  use crate::video::capture::CaptureConfig;
  let f = Fixture::new();
  let mut config = f.window.mode.config.clone();
  config.fps = 30;
  let app = config.app_handle.clone();
  drop(f);
  // Held for the whole real overlay run, after the fixture released it.
  let _handle = hold_overlay_handle();
  let folder = tempfile::tempdir().unwrap();
  let output = folder.path().join("native-thread.gif");
  let (recorder, source) = crate::video::controller::ScriptedSession::new();
  *app
    .state::<crate::commands::VideoRecorderState>()
    .inner()
    .0
    .lock()
    .unwrap() = recorder;
  let (tx, rx) = std::sync::mpsc::channel();
  let mode = build_recording_mode_with(
    640,
    480,
    -100000,
    -100000,
    Rect {
      x: 10.,
      y: 20.,
      w: 64.,
      h: 64.,
    },
    config,
    tx,
    Instant::now(),
    |_, _, _| {
      Ok(PreparedVideoStart {
        output_path: output.clone(),
        temp_output_path: crate::gif::build_intermediate_output_path(&output),
        capture_config: CaptureConfig::area(0, 0, 64, 64, 30),
        video_config: crate::gif::build_gif_video_config(folder.path(), 30),
        gif_options: Some(crate::video::native::gif_options()),
        active_kind: crate::state::ActiveRecordingKind::Gif,
      })
    },
  )
  .unwrap();
  struct Stop;
  impl Drop for Stop {
    fn drop(&mut self) {
      stop_gif_recording_external();
    }
  }
  let _stop = Stop;
  spawn_gif_recording_overlay(Box::new(mode)).unwrap();
  let deadline = Instant::now() + Duration::from_secs(15);
  loop {
    let ready = app
      .state::<crate::commands::VideoRecorderState>()
      .inner()
      .0
      .try_lock()
      .is_ok_and(|mut r| r.status() == crate::video::controller::RecordingStatus::Recording);
    if ready {
      break;
    }
    assert!(
      Instant::now() < deadline,
      "silent native GIF startup must remain bounded"
    );
    std::thread::sleep(Duration::from_millis(5));
  }
  let address = GIF_OVERLAY_HWND.load(Ordering::SeqCst);
  assert_ne!(address, 0);
  assert!(!unsafe { IsWindowVisible(HWND(address as *mut _)) }.as_bool());
  source.finish_segment(
    &mut app
      .state::<crate::commands::VideoRecorderState>()
      .inner()
      .0
      .lock()
      .unwrap(),
  );
  stop_gif_recording_external();
  let result = rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
  assert_eq!(result.path, output);
  assert_eq!((result.width, result.height), (64, 64));
  assert!(output.is_file());
  let deadline = Instant::now() + Duration::from_secs(2);
  while GIF_OVERLAY_HWND.load(Ordering::SeqCst) != 0 {
    assert!(
      Instant::now() < deadline,
      "silent GIF window was not released"
    );
    std::thread::sleep(Duration::from_millis(5));
  }
  assert_eq!(
    app
      .state::<crate::commands::VideoRecorderState>()
      .inner()
      .0
      .lock()
      .unwrap()
      .status(),
    crate::video::controller::RecordingStatus::Idle
  );
  // All external control entry points are harmless after the owner has withdrawn its HWND.
  pause_gif_recording_external();
  resume_gif_recording_external();
  stop_gif_recording_external();
}

#[test]
fn gif_start_plan_preserves_negative_offsets_and_allocates_chrome_only_for_full_ui() {
  use crate::recording_visibility::RecordingPresentationMode;
  let f = Fixture::new();
  for presentation_mode in [
    RecordingPresentationMode::Silent,
    RecordingPresentationMode::FullUi,
  ] {
    let mut config = f.window.mode.config.clone();
    config.presentation_mode = presentation_mode;
    let (tx, _rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let mode = build_recording_mode_with(
      640,
      480,
      -100000,
      -100000,
      Rect {
        x: 10.,
        y: 20.,
        w: 320.,
        h: 200.,
      },
      config,
      tx,
      started,
      |plan, config, source| {
        assert_eq!(
          (
            plan.capture_x,
            plan.capture_y,
            plan.capture_width,
            plan.capture_height
          ),
          (-99990, -99980, 320, 200)
        );
        assert!(source.is_none());
        Ok(PreparedVideoStart {
          output_path: "fixture.gif".into(),
          temp_output_path: "fixture.mkv".into(),
          capture_config: crate::video::capture::CaptureConfig::area(
            plan.capture_x,
            plan.capture_y,
            320,
            200,
            config.fps,
          ),
          video_config: crate::gif::build_gif_video_config(
            std::path::Path::new("isolated"),
            config.fps,
          ),
          gif_options: Some(crate::video::native::gif_options()),
          active_kind: crate::state::ActiveRecordingKind::Gif,
        })
      },
    )
    .unwrap();
    assert_eq!((mode.output_width, mode.output_height), (320, 200));
    assert_eq!(mode.launch_started_at, started);
    assert_eq!(
      mode.recording_bitmap.is_some(),
      presentation_mode == RecordingPresentationMode::FullUi
    );
    assert!(mode.prepared_start.is_some() && mode.response_tx.is_some());
    assert!(!mode.recording_started && !mode.startup_dispatched && !mode.control_failed);
    assert!(mode.completion_rx.is_none() && mode.operation_started.is_none());
    assert!(mode.metrics.width > 0 && mode.metrics.height > 0);
  }
  let (tx, _) = std::sync::mpsc::channel();
  let error = build_recording_mode_with(
    640,
    480,
    -100000,
    -100000,
    Rect {
      x: 10.,
      y: 20.,
      w: 320.,
      h: 200.,
    },
    f.window.mode.config.clone(),
    tx,
    Instant::now(),
    |_, _, _| Err("output directory is read-only".into()),
  )
  .err()
  .unwrap();
  assert_eq!(error.to_string(), "output directory is read-only");
}

#[test]
fn gif_hud_renders_starting_paused_and_resuming_controls_offscreen_and_keeps_the_capture_interior_clear()
 {
  let mut f = Fixture::new();
  f.window.mode.recording_bitmap = Some(LayeredBitmap::new(640, 480).unwrap());
  for phase in [
    RecordingPhase::Starting,
    RecordingPhase::Recording,
    RecordingPhase::Pausing,
    RecordingPhase::Paused,
    RecordingPhase::Resuming,
  ] {
    f.window.shell.phase = phase;
    f.window.shell.confirmed_elapsed = Some(Duration::from_secs(7));
    let mut presented = false;
    render_gif_with(&mut f.window, |bitmap, hwnd, x, y| {
      assert_eq!(hwnd, f.hwnd);
      assert_eq!((x, y), (-640, -200));
      assert_eq!((bitmap.width, bitmap.height), (640, 480));
      presented = true;
      Ok(())
    });
    assert!(presented);
    let pixels = f
      .window
      .mode
      .recording_bitmap
      .as_mut()
      .unwrap()
      .pixels_mut();
    assert_eq!(&pixels[(100 * 640 + 100) * 4..][..4], &[0; 4]);
    assert!(
      pixels[(20 * 640 + 10) * 4 + 3] > 0,
      "selection border must be visible"
    );
    assert!(!unsafe { IsWindowVisible(f.hwnd) }.as_bool());
  }
  f.window.shell.phase = RecordingPhase::Recording;
  render_gif_with(&mut f.window, |_, _, _, _| {
    Err(anyhow::anyhow!("scripted bitmap presentation failure"))
  });
  assert!(
    !f.window.mode.control_failed,
    "painting failure must not invent a recorder failure"
  );
}
use crate::overlay::gdi::window::{
  create_hud_window, register_overlay_class, unregister_overlay_class,
};
use windows::Win32::UI::WindowsAndMessaging::{
  DefWindowProcW, DestroyWindow, IsWindow, IsWindowVisible,
};

unsafe extern "system" fn hidden_proc(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  // SAFETY: a conventional hidden fixture window with no borrowed application state.
  unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Every test that sets or clears the process-wide GIF overlay handle holds this, so a fixture
/// tearing down in a parallel test never erases another test's live handle (with extra
/// parallel test threads that race reproduced on every run).
static OVERLAY_HANDLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn hold_overlay_handle() -> std::sync::MutexGuard<'static, ()> {
  OVERLAY_HANDLE
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Fixture {
  window: RecordingOverlayWindow<GifRecordingMode>,
  hwnd: HWND,
  class: String,
  result: std::sync::mpsc::Receiver<Option<GifResult>>,
  /// Released after `Drop` clears the handle: fields drop after the destructor body.
  _handle: std::sync::MutexGuard<'static, ()>,
}
impl Fixture {
  fn new() -> Self {
    let handle = hold_overlay_handle();
    crate::app::runtime::test_runtime();
    let class = format!("{}-gif-fixture", class_name_for_thread());
    register_overlay_class(&class, Some(hidden_proc)).expect("hidden class");
    let hwnd = create_hud_window(
      &class,
      "Hidden GIF lifecycle fixture",
      0,
      0,
      640,
      480,
      std::ptr::null_mut(),
    )
    .expect("hidden fixture HWND");
    let app = crate::app::context::AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        notifications_enabled: false,
        notification_sound: false,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let (tx, result) = std::sync::mpsc::channel();
    let mut shell = RecordingOverlayShell::new(Instant::now(), "en".into());
    shell.hwnd = hwnd;
    let mode = GifRecordingMode {
      width: 640,
      height: 480,
      vd_offset_x: -640,
      vd_offset_y: -200,
      selection_rect: Rect {
        x: 10.0,
        y: 20.0,
        w: 320.0,
        h: 200.0,
      },
      metrics: shell.metrics,
      toolbar_left: 10,
      toolbar_top: 300,
      output_width: 320,
      output_height: 200,
      launch_started_at: Instant::now(),
      recording_bitmap: None,
      config: GifRecordingConfig {
        fps: 15,
        max_duration_secs: 30,
        output_dir: "isolated".into(),
        show_cursor: true,
        filename_template: "fixture".into(),
        app_handle: app,
        presentation_mode: crate::recording_visibility::RecordingPresentationMode::Silent,
      },
      prepared_start: None,
      recording_started: false,
      control_failed: false,
      startup_dispatched: false,
      completion_rx: None,
      operation_started: None,
      stop_after_operation: false,
      response_tx: Some(tx),
    };
    Self {
      window: RecordingOverlayWindow { shell, mode },
      hwnd,
      class,
      result,
      _handle: handle,
    }
  }
  fn queue(&mut self, completion: RecordingCompletion) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    tx.send(completion).unwrap();
    self.window.mode.completion_rx = Some(rx);
    self.window.mode.operation_started = Some(Instant::now());
  }
  fn poll(&mut self) {
    poll_recording_completion(&mut self.window, self.hwnd);
  }
  fn notices(&self) -> Vec<crate::overlay::notification::NotificationRequest> {
    self
      .window
      .mode
      .config
      .app_handle
      .try_state::<crate::overlay::notification::NotificationManager>()
      .unwrap()
      .take_test_requests()
  }
  fn destroy(&mut self) {
    GifRecordingMode::on_destroy(&mut self.window, self.hwnd);
  }
}
impl Drop for Fixture {
  fn drop(&mut self) {
    // SAFETY: only this fixture's hidden HWND/class are touched.
    unsafe {
      if IsWindow(Some(self.hwnd)).as_bool() {
        DestroyWindow(self.hwnd).expect("hidden fixture teardown");
      }
    }
    unregister_overlay_class(&self.class);
    GIF_OVERLAY_HWND.store(0, Ordering::SeqCst);
  }
}

#[test]
fn hidden_gif_host_runs_real_start_pause_resume_and_gif_publication_with_synthetic_frames() {
  use crate::commands::video::startup::PreparedVideoStart;
  use crate::video::capture::CaptureConfig;
  let mut f = Fixture::new();
  let folder = tempfile::tempdir().unwrap();
  let output = folder.path().join("complete.gif");
  let temp = crate::gif::build_intermediate_output_path(&output);
  let app = f.window.mode.config.app_handle.clone();
  let (recorder, source) = crate::video::controller::ScriptedSession::new();
  *app
    .state::<crate::commands::VideoRecorderState>()
    .inner()
    .0
    .lock()
    .unwrap() = recorder;
  f.window.mode.config.fps = 30;
  f.window.mode.config.output_dir = folder.path().to_path_buf();
  f.window.mode.output_width = 64;
  f.window.mode.output_height = 64;
  f.window.mode.selection_rect.w = 64.;
  f.window.mode.selection_rect.h = 64.;
  f.window.mode.prepared_start = Some(PreparedVideoStart {
    output_path: output.clone(),
    temp_output_path: temp.clone(),
    capture_config: CaptureConfig::area(0, 0, 64, 64, 30),
    video_config: crate::gif::build_gif_video_config(folder.path(), 30),
    gif_options: Some(crate::video::native::gif_options()),
    active_kind: crate::state::ActiveRecordingKind::Gif,
  });
  GifRecordingMode::on_create(&mut f.window, f.hwnd);
  fn drain(f: &mut Fixture) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while f.window.mode.completion_rx.is_some() {
      assert!(Instant::now() < deadline, "GIF worker did not acknowledge");
      f.poll();
      std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!unsafe { IsWindowVisible(f.hwnd) }.as_bool());
  }
  drain(&mut f);
  assert!(f.window.mode.recording_started);
  source.finish_segment(
    &mut app
      .state::<crate::commands::VideoRecorderState>()
      .inner()
      .0
      .lock()
      .unwrap(),
  );
  set_gif_paused_state(&mut f.window, f.hwnd, true);
  drain(&mut f);
  assert!(f.window.shell.is_paused());
  assert!(!output.exists());
  set_gif_paused_state(&mut f.window, f.hwnd, false);
  drain(&mut f);
  assert!(!f.window.shell.is_paused());
  source.finish_segment(
    &mut app
      .state::<crate::commands::VideoRecorderState>()
      .inner()
      .0
      .lock()
      .unwrap(),
  );
  f.poll();
  stop_recording_and_close(&mut f.window, f.hwnd);
  let result = f
    .result
    .recv_timeout(Duration::from_secs(30))
    .unwrap()
    .unwrap();
  assert_eq!(result.path, output);
  assert_eq!((result.width, result.height), (64, 64));
  assert!(!temp.parent().unwrap().exists());
  let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(
    std::fs::File::open(&output).unwrap(),
  ))
  .unwrap();
  use image::AnimationDecoder;
  let frames = decoder.into_frames().collect_frames().unwrap();
  assert!(frames.len() >= 115);
  assert!(
    frames
      .iter()
      .all(|frame| frame.buffer().dimensions() == (64, 64))
  );
  assert!(f.notices().iter().all(|notice| !notice.critical));
  f.destroy();
  assert!(crate::commands::current_recording_session(&app).is_none());
}

#[test]
fn hidden_gif_host_acknowledges_start_pause_resume_and_defers_stop_until_worker_finishes() {
  let mut f = Fixture::new();
  f.queue(RecordingCompletion::Started(Ok(Duration::from_secs(2))));
  f.poll();
  assert!(f.window.mode.recording_started);
  assert_eq!(f.window.shell.phase, RecordingPhase::Recording);
  assert!(
    (Duration::from_secs(2)..Duration::from_secs(3))
      .contains(&f.window.shell.elapsed(Instant::now()))
  );
  assert!(
    !unsafe { IsWindowVisible(f.hwnd) }.as_bool(),
    "quiet host must stay hidden"
  );
  f.queue(RecordingCompletion::Paused(Ok((
    true,
    Duration::from_secs(3),
  ))));
  f.poll();
  assert!(f.window.shell.is_paused());
  assert_eq!(f.window.shell.phase, RecordingPhase::Paused);
  f.queue(RecordingCompletion::Paused(Ok((
    false,
    Duration::from_secs(3),
  ))));
  f.poll();
  assert!(!f.window.shell.is_paused());
  let (tx, rx) = std::sync::mpsc::sync_channel(1);
  f.window.mode.completion_rx = Some(rx);
  stop_recording_and_close(&mut f.window, f.hwnd);
  assert!(f.window.mode.stop_after_operation && f.window.mode.recording_started);
  assert!(
    unsafe { IsWindow(Some(f.hwnd)) }.as_bool(),
    "pending operation retains its window"
  );
  tx.send(RecordingCompletion::Paused(Ok((
    true,
    Duration::from_secs(4),
  ))))
  .unwrap();
  f.poll();
  assert!(!f.window.mode.recording_started);
  assert!(!unsafe { IsWindow(Some(f.hwnd)) }.as_bool());
  assert!(
    f.result
      .recv_timeout(Duration::from_secs(3))
      .expect("bounded finalize result")
      .is_none()
  );
  let notices = f.notices();
  assert_eq!(notices.len(), 1);
  assert!(
    notices[0].critical,
    "failed idle finalization must never claim a saved GIF"
  );
}

#[test]
fn gif_worker_failures_disconnects_and_timeouts_close_owned_window_and_deliver_one_failure() {
  for case in 0..5 {
    let mut f = Fixture::new();
    f.window.mode.startup_dispatched = true;
    match case {
      0 => f.queue(RecordingCompletion::Started(Err("encoder refused".into()))),
      1 => f.queue(RecordingCompletion::WorkerFailed("worker panicked".into())),
      2 => f.queue(RecordingCompletion::Paused(Err("pause failed".into()))),
      3 => {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        f.window.mode.completion_rx = Some(rx);
      },
      _ => {
        let (tx, rx) = std::sync::mpsc::channel();
        f.window.mode.completion_rx = Some(rx);
        f.window.mode.operation_started = Some(Instant::now() - Duration::from_secs(46));
        f.poll();
        drop(tx);
      },
    }
    if case != 4 {
      f.poll();
    }
    assert!(!unsafe { IsWindow(Some(f.hwnd)) }.as_bool(), "case {case}");
    f.destroy();
    assert!(
      f.result
        .recv_timeout(Duration::from_secs(3))
        .expect("bounded cleanup acknowledgment")
        .is_none()
    );
    let notices = f.notices();
    assert_eq!(
      notices.len(),
      1,
      "case {case}: failure must be delivered once"
    );
    assert!(notices[0].critical);
  }
}

#[test]
fn gif_native_message_routing_hit_tests_and_prestartup_cancellation_do_not_launch_capture() {
  let mut f = Fixture::new();
  GifRecordingMode::on_create(&mut f.window, f.hwnd);
  assert!(f.window.mode.prepared_start.is_none() && !f.window.mode.startup_dispatched);
  GifRecordingMode::set_toolbar_position(&mut f.window.mode, 20, 250);
  assert_eq!(f.window.mode.viewport(), (-640, -200, 640, 480));
  let layout = toolbar_layout(f.window.shell.metrics, 20, 250);
  // The toolbar's background is a hit region and points outside pass through.
  assert_eq!(
    GifRecordingMode::hit_test(&f.window, -10000, -10000),
    LRESULT(-1)
  );
  assert_eq!(
    GifRecordingMode::hover_target(
      &f.window,
      layout.stop_rect.left + 1,
      layout.stop_rect.top + 1
    ),
    HoverTarget::Stop
  );
  assert_eq!(
    GifRecordingMode::hit_test(
      &f.window,
      layout.stop_rect.left + 1 - 640,
      layout.stop_rect.top + 1 - 200
    ),
    LRESULT(1)
  );
  assert_eq!(
    GifRecordingMode::on_message(&mut f.window, f.hwnd, WM_APP_PAUSE, WPARAM(0), LPARAM(0)),
    Some(LRESULT(0))
  );
  assert!(
    f.window.mode.completion_rx.is_none(),
    "pause before startup cannot dispatch a worker"
  );
  assert_eq!(
    GifRecordingMode::on_message(&mut f.window, f.hwnd, WM_APP_RESUME, WPARAM(0), LPARAM(0)),
    Some(LRESULT(0))
  );
  assert!(
    GifRecordingMode::on_message(&mut f.window, f.hwnd, WM_APP + 99, WPARAM(0), LPARAM(0))
      .is_none()
  );
  GifRecordingMode::on_keydown(
    &mut f.window,
    f.hwnd,
    crate::overlay::selection_core::VK_RETURN,
  );
  f.destroy();
  assert!(
    f.result
      .recv_timeout(Duration::from_secs(1))
      .unwrap()
      .is_none()
  );
  assert!(f.notices().is_empty());
}

#[test]
fn gif_worker_dispatch_preserves_acknowledgments_and_reports_panicked_worker() {
  crate::app::runtime::test_runtime();
  let rx =
    dispatch_recording_operation(|| RecordingCompletion::Started(Ok(Duration::from_secs(7))));
  assert_eq!(
    rx.recv_timeout(Duration::from_secs(3)).unwrap(),
    RecordingCompletion::Started(Ok(Duration::from_secs(7)))
  );
  let rx = dispatch_recording_operation(|| panic!("injected worker failure"));
  let RecordingCompletion::WorkerFailed(error) = rx.recv_timeout(Duration::from_secs(3)).unwrap()
  else {
    panic!("worker failure must be explicit")
  };
  assert!(error.contains("GIF recording worker failed"));
}
