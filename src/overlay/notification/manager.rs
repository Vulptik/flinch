use std::collections::VecDeque;
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, AtomicU32, Ordering},
  mpsc,
};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::renderer;
use super::sound;
use super::types::{ClickAction, NotificationPosition, NotificationRequest};
use crate::overlay::gdi::layered::LayeredBitmap;
use crate::overlay::gdi::window::{
  class_name_for_thread, create_hud_window, disable_window_animations, exclude_from_capture,
  register_overlay_class, run_message_loop, unregister_overlay_class,
};

type SpawnResult = Option<(
  mpsc::SyncSender<NotificationRequest>,
  Arc<Mutex<Option<isize>>>,
)>;

// Animation
const ANIMATION_TIMER_ID: usize = 100;
const ANIMATION_TIMER_INTERVAL_MS: u32 = 16; // ~60fps
const MARGIN: i32 = 16;
const NOTIFICATION_CHANNEL_CAPACITY: usize = 16;
const MAX_QUEUED_NOTIFICATIONS: usize = 16;

// Crash recovery
const MAX_RESPAWN_ATTEMPTS: u32 = 3;

fn fallback_request(request: &NotificationRequest) {
  let config = crate::config::Config::load();
  if request.should_display(config.notifications_enabled) {
    crate::notifications::show_os_toast_text(&request.title, &request.body);
  }
}

fn render_or_fallback<T>(
  request: &NotificationRequest,
  render: impl FnOnce() -> Result<T, String>,
  fallback: impl FnOnce(&NotificationRequest),
) -> Option<T> {
  match render() {
    Ok(rendered) => Some(rendered),
    Err(error) => {
      tracing::error!(operation = "notification_overlay", %error, "Failed to render toast");
      fallback(request);
      None
    },
  }
}

/// Custom window message to wake the WndProc when a new notification arrives.
const WM_NOTIFY_WAKE: u32 = WM_USER + 200;

/// Manages the notification overlay thread and provides a send interface.
pub struct NotificationManager {
  #[cfg(test)]
  test_requests: Option<Mutex<Vec<NotificationRequest>>>,
  inner: Mutex<ManagerInner>,
  stop_flag: Arc<AtomicBool>,
  respawn_count: AtomicU32,
}

struct ManagerInner {
  sender: Option<mpsc::SyncSender<NotificationRequest>>,
  thread_hwnd: Arc<Mutex<Option<isize>>>,
}

impl Default for NotificationManager {
  fn default() -> Self {
    Self::new()
  }
}

impl NotificationManager {
  #[cfg(test)]
  pub(crate) fn isolated() -> Self {
    let mut manager = Self::new();
    manager.test_requests = Some(Mutex::new(Vec::new()));
    manager
  }
  #[cfg(test)]
  pub(crate) fn take_test_requests(&self) -> Vec<NotificationRequest> {
    self
      .test_requests
      .as_ref()
      .expect("isolated notification delivery")
      .lock()
      .expect("notification fixture")
      .drain(..)
      .collect()
  }
  /// Create the manager without eagerly spawning the notification thread.
  pub fn new() -> Self {
    Self {
      #[cfg(test)]
      test_requests: None,
      inner: Mutex::new(ManagerInner {
        sender: None,
        thread_hwnd: Arc::new(Mutex::new(None)),
      }),
      stop_flag: Arc::new(AtomicBool::new(false)),
      respawn_count: AtomicU32::new(0),
    }
  }

  /// Send a notification. Fire-and-forget. Non-blocking.
  /// Falls back to OS toast if the thread is dead and respawn fails.
  pub fn notify(&self, request: NotificationRequest, app: Option<&crate::app::context::AppHandle>) {
    #[cfg(test)]
    if let Some(requests) = &self.test_requests {
      requests
        .lock()
        .expect("isolated notification sink")
        .push(request);
      return;
    }
    let fallback_title = request.title.clone();
    let fallback_body = request.body.clone();

    let should_respawn = {
      let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

      if inner.sender.is_none() {
        match spawn_thread(self.stop_flag.clone(), Some(request)) {
          Some((sender, thread_hwnd)) => {
            inner.sender = Some(sender);
            inner.thread_hwnd = thread_hwnd;
            self.respawn_count.store(0, Ordering::Relaxed);
            return;
          },
          None => None,
        }
      } else {
        let Some(sender) = inner.sender.as_ref() else {
          return;
        };

        match sender.try_send(request) {
          Ok(()) => {
            wake_thread(&inner.thread_hwnd);
            return;
          },
          Err(mpsc::TrySendError::Disconnected(req)) => Some(req),
          Err(mpsc::TrySendError::Full(request)) => {
            tracing::warn!(
              operation = "notification_overlay",
              channel_capacity = NOTIFICATION_CHANNEL_CAPACITY,
              "Notification channel full; critical failures use Windows fallback"
            );
            drop(inner);
            if request.critical {
              fallback_request(&request);
            }
            return;
          },
        }
      }
    };

    if let Some(failed_request) = should_respawn {
      self.try_respawn(failed_request, app);
    } else if let Some(app) = app {
      crate::notifications::show_os_toast(app, &fallback_title, &fallback_body);
    }
  }

  fn try_respawn(
    &self,
    request: NotificationRequest,
    app: Option<&crate::app::context::AppHandle>,
  ) {
    self.try_respawn_with(
      request,
      |request| spawn_thread(self.stop_flag.clone(), Some(request)),
      |title, body| {
        if let Some(app) = app {
          crate::notifications::show_os_toast(app, title, body);
        }
      },
    );
  }

  fn try_respawn_with(
    &self,
    request: NotificationRequest,
    spawn: impl FnOnce(NotificationRequest) -> SpawnResult,
    fallback: impl FnOnce(&str, &str),
  ) {
    let fallback_title = request.title.clone();
    let fallback_body = request.body.clone();
    let count = self.respawn_count.fetch_add(1, Ordering::Relaxed);
    if count >= MAX_RESPAWN_ATTEMPTS {
      tracing::error!(
        operation = "notification_overlay",
        "Max respawn attempts ({}) reached — permanent OS toast fallback",
        MAX_RESPAWN_ATTEMPTS
      );
      fallback(&fallback_title, &fallback_body);
      return;
    }

    tracing::warn!(
      operation = "notification_overlay",
      attempt = count + 1,
      "Notification thread died, respawning"
    );

    let (sender, thread_hwnd) = match spawn(request) {
      Some(result) => result,
      None => {
        tracing::error!(
          operation = "notification_overlay",
          "Respawn failed — thread could not be created"
        );
        fallback(&fallback_title, &fallback_body);
        return;
      },
    };

    let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.sender = Some(sender);
    inner.thread_hwnd = thread_hwnd;
    drop(inner);
  }

  /// Signal the thread to stop and clean up.
  pub fn shutdown(&self) {
    self.stop_flag.store(true, Ordering::Release);
    let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    post_to_published_window(&inner.thread_hwnd, WM_CLOSE, "notification shutdown");
  }
}

impl Drop for NotificationManager {
  fn drop(&mut self) {
    self.shutdown();
  }
}

fn post_to_published_window(shared: &Mutex<Option<isize>>, message: u32, operation: &str) {
  let publication = shared.lock().unwrap_or_else(|error| {
    tracing::warn!(
      operation,
      "Recovering poisoned notification HWND publication lock"
    );
    error.into_inner()
  });
  if let Some(address) = *publication {
    // SAFETY: publication lock prevents destruction from withdrawing/recycling this HWND
    // until the non-blocking post returns. No synchronous dispatch or state borrow occurs.
    if let Err(error) =
      unsafe { PostMessageW(Some(HWND(address as *mut _)), message, WPARAM(0), LPARAM(0)) }
    {
      tracing::error!(operation, %error, "Notification window message could not be posted");
    }
  }
}

fn wake_thread(thread_hwnd: &Arc<Mutex<Option<isize>>>) {
  post_to_published_window(thread_hwnd, WM_NOTIFY_WAKE, "notification wake");
}

fn clear_publication(shared: &Mutex<Option<isize>>, address: isize) {
  let mut publication = shared.lock().unwrap_or_else(|error| {
    tracing::warn!("Recovering poisoned notification HWND publication during cleanup");
    error.into_inner()
  });
  if *publication == Some(address) {
    *publication = None;
  }
}

fn spawn_thread(
  stop_flag: Arc<AtomicBool>,
  mut initial_request: Option<NotificationRequest>,
) -> SpawnResult {
  let (tx, rx) = mpsc::sync_channel::<NotificationRequest>(NOTIFICATION_CHANNEL_CAPACITY);
  let thread_hwnd: Arc<Mutex<Option<isize>>> = Arc::new(Mutex::new(None));
  let hwnd_clone = thread_hwnd.clone();

  let builder = std::thread::Builder::new().name("notification-overlay".into());
  match builder.spawn(move || {
    if let Err(e) = run_notification_thread(rx, hwnd_clone, stop_flag, &mut initial_request) {
      tracing::error!(
        operation = "notification_overlay",
        error = %e,
        "Notification thread exited with error"
      );
      if let Some(request) = initial_request {
        fallback_request(&request);
      }
    }
  }) {
    // Publication self-wakes the receiver, so requests can be queued immediately.
    // Never wait on the caller/UI thread for HWND creation or rendering.
    Ok(_handle) => Some((tx, thread_hwnd)),
    Err(e) => {
      tracing::error!(
        operation = "notification_overlay",
        error = %e,
        "Failed to spawn notification thread"
      );
      None
    },
  }
}

/// State for the notification window, stored in GWLP_USERDATA.
struct NotifWindowState {
  motion: crate::overlay::motion::Motion,
  hovered: bool,
  hover_bitmap: Option<LayeredBitmap>,
  renderer: crate::overlay::chrome::Renderer,
  publication: Arc<Mutex<Option<isize>>>,
  precise_clock: bool,
  frame_interval: Duration,
  diagnostics: Option<crate::overlay::performance::Diagnostics>,
  receiver: mpsc::Receiver<NotificationRequest>,
  queue: VecDeque<NotificationRequest>,
  stop_flag: Arc<AtomicBool>,
  hwnd: HWND,

  // Current toast state
  bitmap: Option<LayeredBitmap>,
  toast_height: i32,
  click_action: Option<ClickAction>,
  completed: Option<Arc<AtomicBool>>,
  processing: Option<NotificationRequest>,
  processing_tick: Option<u64>,

  // Animation
  phase: ToastPhase,
  anim_start: Instant,
  position: NotificationPosition,
  duration_ms: u64,

  // Screen position (computed per-show)
  offscreen_x: i32,
  target_x: i32,
  target_y: i32,
}

#[derive(PartialEq, Clone, Copy)]
enum ToastPhase {
  Idle,
  SlideIn,
  Hold,
  SlideOut,
}

fn run_notification_thread(
  receiver: mpsc::Receiver<NotificationRequest>,
  hwnd_out: Arc<Mutex<Option<isize>>>,
  stop_flag: Arc<AtomicBool>,
  initial_request: &mut Option<NotificationRequest>,
) -> anyhow::Result<()> {
  let class_name = class_name_for_thread();
  register_overlay_class(&class_name, Some(notif_wnd_proc)).map_err(|e| anyhow::anyhow!("{e}"))?;

  let hwnd = create_hud_window(
    &class_name,
    "FlinchNotification",
    0,
    0,
    1,
    1,
    std::ptr::null_mut(),
  )
  .map_err(|e| {
    unregister_overlay_class(&class_name);
    anyhow::anyhow!("{e}")
  })?;

  disable_window_animations(hwnd);
  exclude_from_capture(hwnd);

  let clock = match crate::overlay::animation_clock::AnimationClock::new() {
    Ok(clock) => Some(clock),
    Err(error) => {
      tracing::warn!(%error, "High-resolution notification clock unavailable; using Win32 animation timer");
      None
    },
  };
  let mut state = NotifWindowState {
    motion: crate::overlay::motion::Motion::new(),
    hovered: false,
    hover_bitmap: None,
    renderer: crate::overlay::chrome::Renderer::default(),
    publication: hwnd_out.clone(),
    precise_clock: clock.is_some(),
    frame_interval: Duration::from_millis(16),
    diagnostics: crate::overlay::performance::Diagnostics::from_env("notification_animation"),
    receiver,
    queue: VecDeque::new(),
    stop_flag,
    hwnd,
    bitmap: None,
    toast_height: 0,
    click_action: None,
    completed: None,
    processing: None,
    processing_tick: None,
    phase: ToastPhase::Idle,
    anim_start: Instant::now(),
    position: NotificationPosition::BottomRight,
    duration_ms: 3000,
    offscreen_x: 0,
    target_x: 0,
    target_y: 0,
  };

  if let Some(request) = initial_request.take() {
    state.queue.push_back(request);
  }

  let state = Box::new(state);

  unsafe {
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
  }

  // Publish only after HWND state is installed. Self-wake drains requests queued during startup,
  // including callers that returned before publication. No producer waits for rendering.
  {
    let mut publication = hwnd_out.lock().unwrap_or_else(|error| {
      tracing::warn!("Recovering poisoned notification HWND publication during startup");
      error.into_inner()
    });
    *publication = Some(hwnd.0 as isize);
  }
  // SAFETY: state is installed in this thread's live HWND before posting its initial wake.
  if let Err(error) = unsafe { PostMessageW(Some(hwnd), WM_NOTIFY_WAKE, WPARAM(0), LPARAM(0)) } {
    // SAFETY: teardown on the owning thread releases state and withdraws its publication.
    if let Err(cleanup_error) = unsafe { DestroyWindow(hwnd) } {
      tracing::error!(%cleanup_error, "Notification startup cleanup failed");
    }
    unregister_overlay_class(&class_name);
    return Err(anyhow::anyhow!("Notification startup wake failed: {error}"));
  }

  if let Some(clock) = clock {
    let result = clock.run(hwnd, ANIMATION_TIMER_ID, |last_tick| {
      // SAFETY: called only between dispatches on the owning thread while HWND is live.
      let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const NotifWindowState;
      if pointer.is_null() {
        return None;
      }
      // SAFETY: state is owned by the live HWND; borrow ends before the next dispatch.
      let state = unsafe { &*pointer };
      match state.phase {
        ToastPhase::SlideIn | ToastPhase::SlideOut => Some(last_tick + state.frame_interval),
        ToastPhase::Hold if state.motion.active() => Some(last_tick + state.frame_interval),
        ToastPhase::Hold if state.processing.is_some() => {
          Some(last_tick + Duration::from_millis(250))
        },
        ToastPhase::Hold => Some(state.anim_start + Duration::from_millis(state.duration_ms)),
        ToastPhase::Idle => None,
      }
    });
    if let Err(error) = result {
      tracing::error!(%error, "Notification animation clock failed; closing overlay thread");
      // SAFETY: live HWND belongs to this thread; destroy releases its state and bitmap.
      if unsafe { IsWindow(Some(hwnd)) }.as_bool()
        && let Err(cleanup_error) = unsafe { DestroyWindow(hwnd) }
      {
        tracing::error!(%cleanup_error, "Notification window cleanup failed");
      }
    }
  } else {
    run_message_loop();
  }

  // WM_QUIT can also arrive without WM_DESTROY; reclaim state before exiting the thread.
  // SAFETY: this thread exclusively owns the HWND and its attached state.
  if unsafe { IsWindow(Some(hwnd)) }.as_bool()
    && let Err(error) = unsafe { DestroyWindow(hwnd) }
  {
    tracing::error!(%error, "Notification thread exit cleanup failed");
  }
  clear_publication(&hwnd_out, hwnd.0 as isize);
  unregister_overlay_class(&class_name);

  Ok(())
}

unsafe extern "system" fn notif_wnd_proc(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut NotifWindowState;
  if state_ptr.is_null() {
    return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
  }
  let state = unsafe { &mut *state_ptr };

  match msg {
    WM_MOUSEMOVE | WM_MOUSELEAVE => {
      let hovered = msg == WM_MOUSEMOVE;
      if hovered != state.hovered && state.phase != ToastPhase::Idle {
        state.hovered = hovered;
        if hovered {
          let mut tracking = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
          };
          // SAFETY: tracking refers to this notification thread's live HWND.
          if let Err(error) = unsafe { TrackMouseEvent(&mut tracking) } {
            tracing::warn!(%error, "Cannot track notification pointer exit");
          }
        }
        tick_animation(state);
      }
      LRESULT(0)
    },
    WM_TIMER if wparam.0 == ANIMATION_TIMER_ID => {
      let started = state.diagnostics.as_ref().map(|_| Instant::now());
      let animated = matches!(state.phase, ToastPhase::SlideIn | ToastPhase::SlideOut);
      tick_animation(state);
      if let Some(diagnostics) = &mut state.diagnostics {
        if animated {
          if let Some(started) = started {
            diagnostics.record(started);
          }
        } else {
          diagnostics.reset_interval();
        }
      }
      LRESULT(0)
    },
    m if m == WM_NOTIFY_WAKE => {
      // Bound work even if producers keep refilling the channel. Each producer posts a wake;
      // startup self-wake covers requests that arrived before publication.
      for _ in 0..NOTIFICATION_CHANNEL_CAPACITY {
        let req = match state.receiver.try_recv() {
          Ok(request) => request,
          Err(mpsc::TryRecvError::Empty) => break,
          Err(mpsc::TryRecvError::Disconnected) => {
            tracing::warn!("Notification sender disconnected; closing overlay thread");
            // SAFETY: defer destruction until this message handler releases its state borrow.
            if let Err(error) = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
            {
              tracing::error!(%error, "Cannot close disconnected notification thread");
            }
            return LRESULT(0);
          },
        };
        if state.queue.len() >= MAX_QUEUED_NOTIFICATIONS {
          if let Some(request) = state.queue.pop_front()
            && request.critical
          {
            fallback_request(&request);
          }
          tracing::warn!(
            operation = "notification_overlay",
            queue_capacity = MAX_QUEUED_NOTIFICATIONS,
            "Notification queue full, dropping oldest queued notification"
          );
        }
        state.queue.push_back(req);
      }
      // A persistent status yields to actionable notifications, then resumes with
      // its original elapsed clock. Keep the existing bounded queue capacity.
      if suspend_processing_for_queue(state) {
        dismiss_toast(state);
      }
      // If idle, start showing next toast
      if state
        .completed
        .as_ref()
        .is_some_and(|done| done.load(Ordering::Acquire))
      {
        dismiss_toast(state);
      } else if state.phase == ToastPhase::Idle {
        show_next_toast(state);
      }
      LRESULT(0)
    },
    WM_DISPLAYCHANGE => {
      if state.phase != ToastPhase::Idle {
        let (x, y, offscreen_x) =
          compute_target_position(state.position, renderer::toast_width(), state.toast_height);
        state.target_x = x;
        state.target_y = y;
        state.offscreen_x = offscreen_x;
        state.frame_interval = crate::overlay::animation_clock::frame_interval_at_point(x, y);
        if state.phase == ToastPhase::Hold
          && let Some(bitmap) = &state.bitmap
          && let Err(error) = bitmap.apply_to(hwnd, x, y)
        {
          tracing::warn!(%error, "Cannot reposition notification after display change");
        }
      }
      LRESULT(0)
    },
    WM_SETCURSOR => {
      // Override the crosshair cursor from the shared overlay window class
      unsafe {
        let cursor = LoadCursorW(None, IDC_HAND).unwrap_or_default();
        SetCursor(Some(cursor));
      }
      LRESULT(1)
    },
    WM_LBUTTONDOWN => {
      // Saving feedback stays visible until its producer completes.
      if state.processing.is_some() {
        return LRESULT(0);
      }
      // Execute click action
      if let Some(ref action) = state.click_action {
        execute_click_action(action);
      }
      state.click_action = None;
      // Dismiss immediately
      dismiss_toast(state);
      LRESULT(0)
    },
    WM_DESTROY => {
      // Withdraw publication before Windows can recycle the HWND. Clear userdata before Drop.
      clear_publication(&state.publication, hwnd.0 as isize);
      // SAFETY: thread owns the state; callbacks must not see a pointer to freed memory.
      unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
      // SAFETY: state was attached exactly once and detached above; reclaim its unique Box.
      drop(unsafe { Box::from_raw(state_ptr) });
      unsafe { PostQuitMessage(0) };
      LRESULT(0)
    },
    WM_CLOSE => {
      // SAFETY: this is the owning thread's live notification HWND.
      if let Err(error) = unsafe { DestroyWindow(hwnd) } {
        tracing::error!(%error, "Notification window destruction failed");
      }
      LRESULT(0)
    },
    _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
  }
}

fn show_next_toast(state: &mut NotifWindowState) {
  prepare_next_toast(
    state,
    &crate::config::Config::load,
    &present_toast_window,
    &fallback_request,
  );
}

fn prepare_next_toast<C, P, F>(state: &mut NotifWindowState, load: &C, present: &P, fallback: &F)
where
  C: Fn() -> crate::config::Config,
  P: Fn(&mut NotifWindowState),
  F: Fn(&NotificationRequest),
{
  // Check stop flag
  if state.stop_flag.load(Ordering::Acquire) {
    // SAFETY: defer destruction until the current window-state borrow ends.
    if let Err(error) = unsafe { PostMessageW(Some(state.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
      tracing::error!(%error, "Notification shutdown request failed");
    }
    return;
  }

  state.queue.retain(|request| !request.is_obsolete());
  let Some(mut request) = state.queue.pop_front() else {
    state.completed = None;
    state.processing = None;
    state.processing_tick = None;
    // Nothing in queue — hide window and wait
    unsafe {
      let _ = ShowWindow(state.hwnd, SW_HIDE);
      let _ = KillTimer(Some(state.hwnd), ANIMATION_TIMER_ID);
    }
    state.phase = ToastPhase::Idle;
    return;
  };

  // Re-read config each toast so settings changes take effect immediately.
  // The overlay thread does not have direct AppHandle access, so this uses disk-backed config.
  let cfg = load();

  if !request.should_display(cfg.notifications_enabled) {
    // Keep critical failures queued even when routine notifications are disabled.
    state
      .queue
      .retain(|request| request.should_display(cfg.notifications_enabled));
    unsafe {
      let _ = ShowWindow(state.hwnd, SW_HIDE);
      let _ = KillTimer(Some(state.hwnd), ANIMATION_TIMER_ID);
    }
    state.phase = ToastPhase::Idle;
    if !state.queue.is_empty() {
      prepare_next_toast(state, load, present, fallback);
    }
    return;
  }

  state.position = NotificationPosition::from_config(&cfg.notification_position);
  state.duration_ms = cfg.notification_duration_ms;

  if cfg.notification_sound && request.play_sound {
    sound::play();
  }

  state.click_action = request.click_action.take();
  state.completed = request.completed.clone();

  // Render the toast
  state.hovered = false;
  state.motion = crate::overlay::motion::Motion::new();
  state.hover_bitmap = None;
  match render_or_fallback(
    &request,
    || renderer::render_toast(&mut state.renderer, &request),
    fallback,
  ) {
    Some((bitmap, height)) => {
      state.toast_height = height;
      state.bitmap = Some(bitmap);
    },
    None => {
      // Try next in queue
      prepare_next_toast(state, load, present, fallback);
      return;
    },
  }

  let (tx, ty, offscreen_x) =
    compute_target_position(state.position, renderer::toast_width(), state.toast_height);
  state.target_x = tx;
  state.target_y = ty;
  state.offscreen_x = offscreen_x;
  state.frame_interval = crate::overlay::animation_clock::frame_interval_at_point(tx, ty);

  if let Some(diagnostics) = &mut state.diagnostics {
    diagnostics.reset_interval();
  }

  // Start slide-in animation
  state.phase = ToastPhase::SlideIn;
  state.anim_start = Instant::now();

  state.processing_tick = None;
  state.processing = request.processing.is_some().then_some(request);
  present(state);
}

fn present_toast_window(state: &mut NotifWindowState) {
  // Position window at the off-screen start position
  let start_x = if state.motion.enabled() {
    state.offscreen_x
  } else {
    state.target_x
  };
  unsafe {
    if let Err(error) = SetWindowPos(
      state.hwnd,
      Some(HWND_TOPMOST),
      start_x,
      state.target_y,
      renderer::toast_width(),
      state.toast_height,
      SWP_NOACTIVATE | SWP_SHOWWINDOW,
    ) {
      tracing::warn!(%error, "Initial notification placement failed");
    }

    if let Some(ref bmp) = state.bitmap
      && let Err(error) = bmp.apply_with_opacity(
        state.hwnd,
        start_x,
        state.target_y,
        if state.motion.enabled() { 0 } else { 255 },
      )
    {
      tracing::warn!(%error, "Initial notification presentation failed");
    }

    if !state.precise_clock
      && SetTimer(
        Some(state.hwnd),
        ANIMATION_TIMER_ID,
        ANIMATION_TIMER_INTERVAL_MS,
        None,
      ) == 0
    {
      tracing::error!("Notification animation timer creation failed; closing overlay");
      if let Err(error) = PostMessageW(Some(state.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) {
        tracing::error!(%error, "Notification close request failed");
      }
    }
  }
}

// Queue state only; presentation remains owned by the caller.
fn suspend_processing_for_queue(state: &mut NotifWindowState) -> bool {
  if state.processing.is_none() || state.queue.is_empty() {
    return false;
  }
  let suspended = state.processing.take();
  if let Some(request) = suspended
    && !request.is_obsolete()
  {
    if state.queue.len() >= MAX_QUEUED_NOTIFICATIONS {
      if let Some(displaced) = state.queue.pop_back()
        && displaced.critical
      {
        fallback_request(&displaced);
      }
      tracing::warn!("Notification queue full while preserving active saving feedback");
    }
    state.queue.push_back(request);
  }
  true
}

fn tick_animation(state: &mut NotifWindowState) {
  if state
    .completed
    .as_ref()
    .is_some_and(|done| done.load(Ordering::Acquire))
  {
    dismiss_toast(state);
    return;
  }
  if let Some(request) = &state.processing
    && let Some(feedback) = &request.processing
  {
    let seconds = feedback.started.elapsed().as_secs();
    if state.processing_tick != Some(seconds) {
      state.processing_tick = Some(seconds);
      match renderer::render_toast(&mut state.renderer, request) {
        Ok((bitmap, _)) => {
          state.bitmap = Some(bitmap);
          state.hover_bitmap = None;
        },
        Err(error) => tracing::warn!(%error, "Processing feedback refresh failed"),
      }
    }
  }
  let elapsed_ms = state.anim_start.elapsed().as_secs_f64() * 1000.0;
  state
    .motion
    .update(true, 0, state.hovered.then_some(1), Instant::now());
  if let Err(error) = render_hover(state) {
    tracing::warn!(%error, "Notification hover presentation failed");
    state.hover_bitmap = None;
  }

  match state.phase {
    ToastPhase::SlideIn => {
      let t = if state.motion.enabled() {
        (elapsed_ms / 180.0).min(1.0)
      } else {
        1.0
      };
      let eased = ease_out_cubic(t);
      let x = interpolate_slide(state.offscreen_x, state.target_x, eased);

      // The window is already topmost; UpdateLayeredWindow moves the cached bitmap.
      if let Some(bmp) = state.hover_bitmap.as_ref().or(state.bitmap.as_ref())
        && let Err(error) =
          bmp.apply_with_opacity(state.hwnd, x, state.target_y, (eased * 255.0).round() as u8)
      {
        tracing::warn!(%error, "Notification animation presentation failed");
      }

      if t >= 1.0 {
        state.phase = ToastPhase::Hold;
        state.anim_start = Instant::now();
      }
    },
    ToastPhase::Hold => {
      if let Some(bmp) = state.hover_bitmap.as_ref().or(state.bitmap.as_ref())
        && let Err(error) = bmp.apply_to(state.hwnd, state.target_x, state.target_y)
      {
        tracing::warn!(%error, "Notification hover presentation failed");
      }
      if state.processing.is_none() && elapsed_ms >= state.duration_ms as f64 {
        state.phase = ToastPhase::SlideOut;
        state.anim_start = Instant::now();
      }
    },
    ToastPhase::SlideOut => {
      let t = if state.motion.enabled() {
        (elapsed_ms / 140.0).min(1.0)
      } else {
        1.0
      };
      let eased = ease_out_cubic(t);
      // Reverse: from target to off-screen
      let x = interpolate_slide(state.offscreen_x, state.target_x, 1.0 - eased);

      if let Some(bmp) = state.hover_bitmap.as_ref().or(state.bitmap.as_ref())
        && let Err(error) = bmp.apply_with_opacity(
          state.hwnd,
          x,
          state.target_y,
          ((1.0 - eased) * 255.0).round() as u8,
        )
      {
        tracing::warn!(%error, "Notification animation presentation failed");
      }

      if t >= 1.0 {
        dismiss_toast(state);
      }
    },
    ToastPhase::Idle => {},
  }
}

fn render_hover(state: &mut NotifWindowState) -> anyhow::Result<()> {
  use windows::Win32::Graphics::Gdi::{BitBlt, SRCCOPY};
  let Some(base) = &state.bitmap else {
    return Ok(());
  };
  if state
    .hover_bitmap
    .as_ref()
    .is_none_or(|bitmap| bitmap.height != base.height)
  {
    state.hover_bitmap =
      Some(LayeredBitmap::new(base.width, base.height).map_err(anyhow::Error::msg)?);
  }
  let Some(target) = &state.hover_bitmap else {
    return Ok(());
  };
  // SAFETY: source/destination DIBs have matching dimensions and remain on this owner thread.
  unsafe {
    BitBlt(
      target.dc,
      0,
      0,
      base.width,
      base.height,
      Some(base.dc),
      0,
      0,
      SRCCOPY,
    )
  }?;
  let alpha = state.motion.hover_alpha(1, Instant::now());
  renderer::render_hover(&mut state.renderer, target, alpha)
}

fn dismiss_toast(state: &mut NotifWindowState) {
  state.bitmap = None;
  state.hover_bitmap = None;
  state.hovered = false;
  state.click_action = None;
  state.completed = None;
  state.processing = None;
  state.processing_tick = None;
  // Show next or go idle
  show_next_toast(state);
}

/// Compute where the fully-visible toast should sit.
fn compute_target_position(
  pos: NotificationPosition,
  toast_w: i32,
  toast_h: i32,
) -> (i32, i32, i32) {
  let (mx, my, mw, mh) = match crate::monitors::get_primary_monitor() {
    Some(m) => (m.x, m.y, m.width as i32, m.height as i32),
    None => {
      tracing::warn!("Primary monitor unavailable; notification placement uses 1920x1080 bounds");
      (0, 0, 1920, 1080)
    },
  };

  let x = if pos.slides_from_right() {
    mx + mw - toast_w - MARGIN
  } else {
    mx + MARGIN
  };

  // BASE_OFFSET_Y adds a fixed clearance from the screen edge (e.g. above the taskbar)
  // on top of MARGIN.
  const BASE_OFFSET_Y: i32 = 50;
  let y = if pos.is_bottom() {
    my + mh - toast_h - MARGIN - BASE_OFFSET_Y
  } else {
    my + MARGIN + BASE_OFFSET_Y
  };

  let offscreen_x = if pos.slides_from_right() {
    mx + mw
  } else {
    mx - toast_w
  };
  (x, y, offscreen_x)
}

fn interpolate_slide(offscreen_x: i32, target_x: i32, progress: f64) -> i32 {
  (f64::from(offscreen_x)
    + (f64::from(target_x) - f64::from(offscreen_x)) * progress.clamp(0.0, 1.0)) as i32
}

fn ease_out_cubic(t: f64) -> f64 {
  1.0 - (1.0 - t).powi(3)
}

fn execute_click_action(action: &ClickAction) {
  // SAFETY: ShellExecuteW is called with HSTRING references that outlive the call.
  // All string arguments are constructed from valid Rust strings (UTF-16 via HSTRING).
  // ShellExecuteW is thread-safe (it does not modify shared state).
  // The return value (HINSTANCE) is not needed — fire-and-forget semantics.
  unsafe {
    match action {
      ClickAction::OpenFolder(path) => {
        let arg = format!("/select,\"{}\"", path.to_string_lossy());
        ShellExecuteW(
          None,
          windows::core::w!("open"),
          windows::core::w!("explorer.exe"),
          &windows::core::HSTRING::from(arg.as_str()),
          None,
          SW_SHOWNORMAL,
        );
      },
    }
  }
}

#[cfg(test)]
mod quiet_tests;

#[cfg(test)]
mod lifecycle_tests {
  use super::*;

  #[test]
  fn render_failure_delivers_original_message_once_without_retry() {
    let request = NotificationRequest {
      title: "Capture failed".into(),
      body: "No image captured".into(),
      thumbnail: None,
      click_action: None,
      play_sound: false,
      critical: true,
      completed: None,
      processing: None,
    };
    let attempts = std::cell::Cell::new(0);
    let delivered = std::cell::RefCell::new(Vec::new());
    let result: Option<()> = render_or_fallback(
      &request,
      || {
        attempts.set(attempts.get() + 1);
        Err("Injected renderer failure".into())
      },
      |request| {
        delivered
          .borrow_mut()
          .push((request.title.clone(), request.body.clone()))
      },
    );
    assert!(result.is_none());
    assert_eq!(attempts.get(), 1);
    assert_eq!(
      *delivered.borrow(),
      vec![(request.title.clone(), request.body.clone())]
    );
    assert_eq!(
      render_or_fallback(&request, || Ok(42), |_| panic!("Unexpected fallback")),
      Some(42)
    );
  }

  #[test]
  fn recovery_failure_preserves_original_error_without_retry_loop() {
    for exhausted in [false, true] {
      let manager = NotificationManager::new();
      if exhausted {
        manager
          .respawn_count
          .store(MAX_RESPAWN_ATTEMPTS, Ordering::Relaxed);
      }
      let spawned = std::cell::Cell::new(0);
      let delivered = std::cell::RefCell::new(Vec::new());
      manager.try_respawn_with(
        NotificationRequest {
          title: "Save failed".into(),
          body: "Not saved".into(),
          thumbnail: None,
          click_action: None,
          play_sound: false,
          critical: true,
          completed: None,
          processing: None,
        },
        |_| {
          spawned.set(spawned.get() + 1);
          None
        },
        |title, body| {
          delivered
            .borrow_mut()
            .push((title.to_owned(), body.to_owned()))
        },
      );
      assert_eq!(spawned.get(), usize::from(!exhausted));
      assert_eq!(
        *delivered.borrow(),
        vec![("Save failed".into(), "Not saved".into())]
      );
    }
  }

  #[test]
  fn stale_thread_cleanup_does_not_clear_replacement_publication() {
    let publication = Mutex::new(Some(2));
    clear_publication(&publication, 1);
    assert_eq!(*publication.lock().expect("publication"), Some(2));
    clear_publication(&publication, 2);
    assert_eq!(*publication.lock().expect("publication"), None);
  }

  #[test]
  fn shutdown_before_publication_reclaims_hidden_thread() {
    let stop = Arc::new(AtomicBool::new(true));
    let (sender, publication) =
      spawn_thread(stop.clone(), None).expect("spawn hidden notification thread");
    // A caller can enqueue before HWND publication; startup self-wake must cover that race.
    sender
      .try_send(NotificationRequest {
        title: "test".into(),
        body: String::new(),
        thumbnail: None,
        click_action: None,
        play_sound: false,
        critical: false,
        completed: None,
        processing: None,
      })
      .expect("early buffered request");
    drop(sender);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Arc::strong_count(&stop) != 1 && Instant::now() < deadline {
      std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
      Arc::strong_count(&stop),
      1,
      "notification thread retained state after shutdown"
    );
    assert_eq!(*publication.lock().expect("publication"), None);
  }
}
