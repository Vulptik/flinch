use super::*;
use crate::overlay::gdi::window::{
  create_hud_window, register_overlay_class, unregister_overlay_class,
};
use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, DestroyWindow, IsWindowVisible};

#[test]
fn successful_notification_respawn_publishes_the_replacement_sender_without_holding_its_lock() {
  let manager = NotificationManager::new();
  let (tx, rx) = mpsc::sync_channel(1);
  let publication = Arc::new(Mutex::new(None));
  let replacement = publication.clone();
  manager.try_respawn_with(
    request("Original failure", "Detailed reason", true),
    |req| {
      assert!(
        manager.inner.try_lock().is_ok(),
        "native spawn must run outside the manager lock"
      );
      tx.try_send(req).unwrap();
      Some((tx, replacement))
    },
    |_, _| panic!("successful recovery cannot also deliver fallback"),
  );
  assert_eq!(
    rx.recv_timeout(Duration::from_secs(1)).unwrap().body,
    "Detailed reason"
  );
  assert!(Arc::ptr_eq(
    &manager.inner.lock().unwrap().thread_hwnd,
    &publication
  ));
  manager.notify(request("next", "body", false), None);
  assert_eq!(
    rx.recv_timeout(Duration::from_secs(1)).unwrap().title,
    "next"
  );
}

#[test]
fn native_notification_messages_bound_queue_draining_and_hover_without_showing() {
  let mut f = Fixture::new();
  f.state
    .queue
    .push_back(request("in progress", "body", false));
  f.prepare(true);
  f.state.phase = ToastPhase::Hold;
  let hwnd = f.state.hwnd;
  // SAFETY: stack state stays at a fixed address throughout synchronous dispatch;
  // the fixture class uses DefWindowProc, so destruction cannot reclaim this pointer.
  unsafe {
    SetWindowLongPtrW(
      hwnd,
      GWLP_USERDATA,
      (&mut f.state as *mut NotifWindowState) as isize,
    );
  }
  for msg in [
    WM_MOUSEMOVE,
    WM_MOUSEMOVE,
    WM_MOUSELEAVE,
    WM_TIMER,
    WM_DISPLAYCHANGE,
  ] {
    // SAFETY: messages target only the fixture's hidden owned HWND.
    unsafe {
      notif_wnd_proc(hwnd, msg, WPARAM(ANIMATION_TIMER_ID), LPARAM(0));
    }
    f.assert_hidden();
  }
  for index in 0..MAX_QUEUED_NOTIFICATIONS + 4 {
    f._sender
      .try_send(request(&index.to_string(), "queued", false))
      .unwrap();
    // SAFETY: phase remains Hold, so no queued request can be shown by this wake.
    unsafe {
      notif_wnd_proc(hwnd, WM_NOTIFY_WAKE, WPARAM(0), LPARAM(0));
    }
  }
  assert_eq!(f.state.queue.len(), MAX_QUEUED_NOTIFICATIONS);
  assert_eq!(f.state.queue.front().unwrap().title, "4");
  f.state.queue.clear();
  f.state.click_action = None;
  // SAFETY: dismissal has no navigation action or next queued notification.
  unsafe {
    notif_wnd_proc(hwnd, WM_LBUTTONDOWN, WPARAM(0), LPARAM(0));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
  }
  assert!(f.state.phase == ToastPhase::Idle && f.state.bitmap.is_none());
  f.assert_hidden();
}

#[test]
fn empty_native_notification_owner_publishes_a_hidden_window_and_reclaims_it_on_shutdown() {
  let manager = NotificationManager::new();
  let publication = manager.inner.lock().unwrap().thread_hwnd.clone();
  let stop = manager.stop_flag.clone();
  let (sender, receiver) = mpsc::sync_channel(1);
  let (done_tx, done) = mpsc::sync_channel(1);
  let thread_publication = publication.clone();
  let worker = std::thread::spawn(move || {
    let result = run_notification_thread(receiver, thread_publication, stop, &mut None);
    done_tx.send(result).unwrap();
  });
  let deadline = Instant::now() + Duration::from_secs(5);
  let address = loop {
    if let Some(address) = *publication.lock().unwrap() {
      break address;
    }
    assert!(
      Instant::now() < deadline,
      "notification HWND publication timed out"
    );
    std::thread::sleep(Duration::from_millis(2));
  };
  // SAFETY: query this fixture's owned HWND; no request capable of showing a toast is sent.
  assert!(!unsafe { IsWindowVisible(HWND(address as *mut _)) }.as_bool());
  // Stop first, then wake: the woken thread always finds the stop request and closes itself.
  // Waking first raced the stop, so a run took one path or the other.
  manager.stop_flag.store(true, Ordering::Release);
  wake_thread(&publication);
  done.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
  worker.join().unwrap();
  assert!(publication.lock().unwrap().is_none());
  // With the window gone, shutting down has nothing left to post.
  manager.shutdown();
  assert!(manager.stop_flag.load(Ordering::Acquire));
  drop(sender);
}

unsafe extern "system" fn hidden(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
  // SAFETY: this fixture has no application window-state pointer or visible surface.
  unsafe { DefWindowProcW(hwnd, message, w, l) }
}
struct Fixture {
  state: NotifWindowState,
  class: String,
  _sender: mpsc::SyncSender<NotificationRequest>,
}
impl Fixture {
  fn new() -> Self {
    let class = format!("{}-notification-quiet", class_name_for_thread());
    register_overlay_class(&class, Some(hidden)).unwrap();
    let hwnd = create_hud_window(
      &class,
      "Hidden notification fixture",
      0,
      0,
      340,
      76,
      std::ptr::null_mut(),
    )
    .unwrap();
    let (sender, receiver) = mpsc::sync_channel(4);
    Self {
      class,
      _sender: sender,
      state: NotifWindowState {
        motion: crate::overlay::motion::Motion::new(),
        hovered: false,
        hover_bitmap: None,
        renderer: Default::default(),
        publication: Arc::new(Mutex::new(None)),
        precise_clock: true,
        frame_interval: Duration::from_millis(16),
        diagnostics: None,
        receiver,
        queue: VecDeque::new(),
        stop_flag: Arc::new(AtomicBool::new(false)),
        hwnd,
        bitmap: None,
        toast_height: 76,
        click_action: None,
        completed: None,
        processing: None,
        processing_tick: None,
        phase: ToastPhase::Idle,
        anim_start: Instant::now(),
        position: NotificationPosition::BottomRight,
        duration_ms: 1000,
        offscreen_x: 400,
        target_x: 10,
        target_y: 10,
      },
    }
  }
  fn assert_hidden(&self) {
    assert!(!unsafe { IsWindowVisible(self.state.hwnd) }.as_bool());
  }
  fn prepare(&mut self, enabled: bool) {
    prepare_next_toast(
      &mut self.state,
      &|| crate::config::Config {
        notifications_enabled: enabled,
        notification_sound: false,
        notification_duration_ms: 1234,
        ..Default::default()
      },
      &|state| {
        assert!(state.bitmap.is_some());
        assert!(state.phase == ToastPhase::SlideIn);
      },
      &|_| panic!("offscreen rendering must succeed"),
    );
    self.assert_hidden();
  }
}
impl Drop for Fixture {
  fn drop(&mut self) {
    // SAFETY: destroy only this hidden fixture's live, owner-thread window.
    unsafe {
      DestroyWindow(self.state.hwnd).unwrap();
    }
    unregister_overlay_class(&self.class);
  }
}
fn request(title: &str, body: &str, critical: bool) -> NotificationRequest {
  NotificationRequest {
    title: title.into(),
    body: body.into(),
    thumbnail: None,
    click_action: None,
    play_sound: false,
    critical,
    completed: None,
    processing: None,
  }
}

#[test]
fn saturated_notification_channel_keeps_the_queued_message_and_remains_available_after_drain() {
  let manager = NotificationManager::new();
  let (sender, receiver) = mpsc::sync_channel(1);
  manager.inner.lock().unwrap().sender = Some(sender);
  manager.notify(request("first", "original body", false), None);
  manager.notify(request("overflow", "discard routine overflow", false), None);
  let queued = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
  assert_eq!(queued.title, "first");
  assert_eq!(queued.body, "original body");
  assert!(receiver.try_recv().is_err());
  manager.notify(request("after drain", "new body", false), None);
  let next = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
  assert_eq!(next.title, "after drain");
  assert_eq!(next.body, "new body");
  assert!(
    manager
      .inner
      .lock()
      .unwrap()
      .thread_hwnd
      .lock()
      .unwrap()
      .is_none()
  );
  manager.shutdown();
  assert!(manager.stop_flag.load(Ordering::Acquire));
}

#[test]
fn disabled_routine_toasts_and_obsolete_statuses_cannot_hide_a_queued_critical_failure() {
  let mut f = Fixture::new();
  let mut obsolete = request("obsolete", "finished", true);
  obsolete.completed = Some(Arc::new(AtomicBool::new(true)));
  f.state.queue.extend([
    obsolete,
    request("routine", "saved", false),
    request("capture failed", "original failure", true),
    request("routine 2", "saved", false),
  ]);
  f.prepare(false);
  assert!(f.state.queue.is_empty());
  assert_eq!(f.state.duration_ms, 1234);
  assert_eq!(f.state.toast_height, 76);
  assert!(f.state.bitmap.is_some());
  f.state.completed = Some(Arc::new(AtomicBool::new(true)));
  tick_animation(&mut f.state);
  assert!(f.state.phase == ToastPhase::Idle);
  assert!(f.state.bitmap.is_none() && f.state.completed.is_none());
  f.assert_hidden();
}

#[test]
fn toast_animation_hover_and_dismissal_release_rendered_buffers_without_showing_a_window() {
  let mut f = Fixture::new();
  let mut input = request("Capture saved", "C:\\fixture\\capture.png", false);
  input.click_action = Some(ClickAction::OpenFolder("fixture.png".into()));
  input.thumbnail = Some(super::super::types::ThumbnailImage {
    rgba: [30, 90, 120, 255].repeat(48 * 48),
    width: 48,
    height: 48,
  });
  f.state.queue.push_back(input);
  f.prepare(true);
  for phase in [ToastPhase::SlideIn, ToastPhase::Hold, ToastPhase::SlideOut] {
    f.state.phase = phase;
    f.state.anim_start = Instant::now() - Duration::from_secs(3);
    f.state.hovered = true;
    tick_animation(&mut f.state);
    f.assert_hidden();
    if phase == ToastPhase::SlideIn {
      assert!(f.state.phase == ToastPhase::Hold);
      assert!(f.state.hover_bitmap.is_some());
    }
    if phase == ToastPhase::Hold {
      assert!(f.state.phase == ToastPhase::SlideOut);
    }
  }
  assert!(f.state.phase == ToastPhase::Idle);
  assert!(
    f.state.bitmap.is_none() && f.state.hover_bitmap.is_none() && f.state.click_action.is_none()
  );
}

#[test]
fn title_only_toast_is_compact_and_empty_queues_clear_transient_completion() {
  let mut f = Fixture::new();
  f.state
    .queue
    .push_back(request("Title only", " \n\t ", false));
  f.prepare(true);
  assert_eq!(f.state.toast_height, 46);
  f.state.queue.clear();
  dismiss_toast(&mut f.state);
  assert!(f.state.phase == ToastPhase::Idle);
  render_hover(&mut f.state).unwrap();
  tick_animation(&mut f.state);
  f.assert_hidden();
}

#[test]
fn processing_feedback_survives_minutes_and_is_removed_on_completion() {
  let mut f = Fixture::new();
  let done = Arc::new(AtomicBool::new(false));
  let stage = Arc::new(std::sync::atomic::AtomicU8::new(0));
  let mut input = request("Saving video", "", false);
  input.completed = Some(done.clone());
  input.processing = Some(super::super::types::ProcessingFeedback {
    started: Instant::now() - Duration::from_secs(243),
    stage: stage.clone(),
    labels: vec!["Preparing tracks".into(), "Checking recording".into()],
  });
  f.state.queue.push_back(input);
  f.prepare(true);
  f.state.phase = ToastPhase::Hold;
  f.state.anim_start = Instant::now() - Duration::from_secs(243);
  tick_animation(&mut f.state);
  assert!(
    f.state.phase == ToastPhase::Hold,
    "saving must outlive the toast timeout"
  );
  assert_eq!(f.state.toast_height, 76);
  assert!(f.state.processing_tick.is_some());
  stage.store(1, Ordering::Release);
  f.state.processing_tick = None;
  tick_animation(&mut f.state);
  assert!(
    f.state
      .processing
      .as_ref()
      .unwrap()
      .processing
      .as_ref()
      .unwrap()
      .body()
      .starts_with("Checking recording")
  );
  done.store(true, Ordering::Release);
  tick_animation(&mut f.state);
  assert!(f.state.phase == ToastPhase::Idle);
  assert!(f.state.processing.is_none() && f.state.completed.is_none());
  f.assert_hidden();
}

#[test]
fn saving_feedback_yields_to_results_resumes_and_never_outlives_completion() {
  let mut f = Fixture::new();
  let done = Arc::new(AtomicBool::new(false));
  let mut input = request("Saving", "", false);
  let started = Instant::now() - Duration::from_secs(243);
  input.completed = Some(done.clone());
  input.processing = Some(super::super::types::ProcessingFeedback {
    started,
    stage: Arc::new(std::sync::atomic::AtomicU8::new(0)),
    labels: vec!["Preparing".into()],
  });
  f.state.queue.push_back(input);
  f.prepare(true);
  f.state
    .queue
    .push_back(request("Screenshot saved", "capture.png", false));
  assert!(suspend_processing_for_queue(&mut f.state));
  f.prepare(true);
  assert!(f.state.processing.is_none());
  assert_eq!(
    f.state
      .queue
      .front()
      .unwrap()
      .processing
      .as_ref()
      .unwrap()
      .started,
    started
  );
  f.prepare(true);
  assert!(
    f.state.processing.is_some(),
    "saving resumes after the actionable result"
  );
  f.state
    .queue
    .push_back(request("Video saved", "recording.mp4", false));
  done.store(true, Ordering::Release);
  assert!(suspend_processing_for_queue(&mut f.state));
  f.prepare(true);
  assert!(
    f.state.queue.is_empty(),
    "completed saving status must not resume"
  );
  assert!(f.state.processing.is_none());
  f.assert_hidden();
}
