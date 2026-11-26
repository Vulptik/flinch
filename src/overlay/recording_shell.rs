use std::time::{Duration, Instant};

use anyhow::Result;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient};
use windows::Win32::UI::Input::KeyboardAndMouse::{
  GetCapture, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
  CREATESTRUCTW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetCursorPos, GetWindowLongPtrW,
  IDC_ARROW, IDC_HAND, IsWindow, KillTimer, PostQuitMessage, SHOW_WINDOW_CMD, SetCursor, SetTimer,
  SetWindowLongPtrW, ShowWindow, WM_CAPTURECHANGED, WM_CREATE, WM_DESTROY, WM_ERASEBKGND,
  WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCHITTEST, WM_PAINT, WM_SETCURSOR,
  WM_SYSKEYDOWN, WM_TIMER, WNDPROC,
};

use crate::overlay::gdi::layered::{LayeredBitmap, premul_argb};
use crate::overlay::gdi::window::{
  register_overlay_class, run_message_loop, show_window_topmost, unregister_overlay_class,
};
use crate::overlay::recording_toolbar::{
  HoverTarget, PauseState, RecordingPhase, TOOLBAR_METRICS, TimerTextStyle, ToolbarAnimation,
  ToolbarLayout, ToolbarMetrics, draw_toolbar_animated, fitted_metrics, recording_pulse_alpha,
  status_dot_color,
};

const MOTION_TIMER_ID: usize = 0x464c;

#[derive(Clone, Copy, PartialEq, Eq)]
struct PaintKey {
  opacity: u8,
  hover_alpha: [u8; 4],
  microphone: Option<crate::overlay::recording_toolbar::MicrophoneState>,
  dot_color: [u8; 3],
  pressed: HoverTarget,
  pulse_alpha: u8,
  phase: RecordingPhase,
  paused: bool,
  seconds: u64,
  hover: HoverTarget,
  position: (i32, i32),
}

pub struct RecordingOverlayShell {
  motion: crate::overlay::motion::Motion,
  precise_clock: bool,
  motion_timer: bool,
  dot_phase: RecordingPhase,
  dot_from: [u8; 3],
  dot_color: [u8; 3],
  pub diagnostics: Option<crate::overlay::performance::Diagnostics>,
  pub start_time: Instant,
  pub pause_state: PauseState,
  pub hover: HoverTarget,
  pub hwnd: HWND,
  pub chrome: crate::overlay::chrome::Renderer,
  pub lang: String,
  pub phase: RecordingPhase,
  pub confirmed_elapsed: Option<Duration>,
  pub metrics: ToolbarMetrics,
  /// The microphone button's state; `None` shows no button.
  pub microphone: Option<crate::overlay::recording_toolbar::MicrophoneState>,
  metric_key: Option<(RecordingPhase, usize, TimerTextStyle, bool)>,
  drag_offset: Option<(i32, i32)>,
  drag_monitors: Vec<crate::monitors::MonitorInfo>,
  painted: Option<PaintKey>,
  pressed: Option<(HoverTarget, Instant)>,
  /// Chromium's hands for the drag grip, created on first use on this overlay thread.
  hands: Option<crate::overlay::screenshot::cursors::ToolCursors>,
  hands_failed: bool,
}

// Constructed inside the recording thread. The renderer deliberately keeps this shell
// non-Send, so initialized windows and Direct2D resources cannot cross threads.

pub struct RecordingOverlayWindow<M> {
  pub shell: RecordingOverlayShell,
  pub mode: M,
}

pub trait RecordingOverlayMode: Sized + 'static {
  fn on_create(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND);
  fn on_destroy(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND);
  fn on_timer(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND, timer_id: usize);
  fn on_click(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND, client_x: i32, client_y: i32);
  fn hover_target(
    window: &RecordingOverlayWindow<Self>,
    client_x: i32,
    client_y: i32,
  ) -> HoverTarget;
  fn hit_test(window: &RecordingOverlayWindow<Self>, screen_x: i32, screen_y: i32) -> LRESULT;
  fn redraw(window: &mut RecordingOverlayWindow<Self>);
  fn toolbar_position(&self) -> (i32, i32);
  fn set_toolbar_position(&mut self, x: i32, y: i32);
  fn viewport(&self) -> (i32, i32, i32, i32);
  fn on_display_change(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND);

  fn on_keydown(_window: &mut RecordingOverlayWindow<Self>, _hwnd: HWND, _vk: u32) {}

  fn on_message(
    _window: &mut RecordingOverlayWindow<Self>,
    _hwnd: HWND,
    _msg: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
  ) -> Option<LRESULT> {
    None
  }
}

impl RecordingOverlayShell {
  pub fn new(start_time: Instant, lang: String) -> Self {
    Self {
      motion: crate::overlay::motion::Motion::new(),
      precise_clock: false,
      motion_timer: false,
      dot_phase: RecordingPhase::Starting,
      dot_from: status_dot_color(RecordingPhase::Starting),
      dot_color: status_dot_color(RecordingPhase::Starting),
      diagnostics: crate::overlay::performance::Diagnostics::from_env("recording_hud"),
      start_time,
      pause_state: PauseState::default(),
      hover: HoverTarget::None,
      hwnd: HWND::default(),
      chrome: crate::overlay::chrome::Renderer::default(),
      lang,
      phase: RecordingPhase::Starting,
      confirmed_elapsed: None,
      metrics: TOOLBAR_METRICS,
      microphone: None,
      metric_key: None,
      drag_offset: None,
      drag_monitors: Vec::new(),
      painted: None,
      pressed: None,
      hands: None,
      hands_failed: false,
    }
  }

  pub fn elapsed(&self, now: Instant) -> Duration {
    self
      .confirmed_elapsed
      .unwrap_or_else(|| self.pause_state.elapsed_since(self.start_time, now))
  }

  pub fn set_paused(&mut self, paused: bool, now: Instant) -> bool {
    self.pause_state.set_paused(paused, now)
  }

  pub fn acknowledge_pause(&mut self, paused: bool, elapsed: Duration, now: Instant) {
    self.start_time = now.checked_sub(elapsed).unwrap_or_else(|| {
      tracing::error!("Recorder elapsed duration exceeds the monotonic clock range");
      now
    });
    self.pause_state = PauseState::default();
    self.set_paused(paused, now);
    self.phase = if paused {
      RecordingPhase::Paused
    } else {
      RecordingPhase::Recording
    };
  }

  pub fn is_paused(&self) -> bool {
    self.pause_state.is_paused
  }

  fn pressed_target(&self, now: Instant) -> HoverTarget {
    match self.pressed {
      Some((target, started))
        if now.saturating_duration_since(started) < Duration::from_millis(160) =>
      {
        target
      },
      _ => HoverTarget::None,
    }
  }

  pub fn needs_paint(&mut self, position: (i32, i32)) -> bool {
    let now = Instant::now();
    let was_animating = self.motion.active();
    if self.dot_phase != self.phase {
      self.dot_from = self.dot_color;
      self.dot_phase = self.phase;
    }
    self.motion.update(
      true,
      self.phase as u8 + 1,
      match self.hover {
        HoverTarget::Drag => Some(60),
        HoverTarget::Microphone => Some(3),
        HoverTarget::Pause => Some(1),
        HoverTarget::Stop => Some(2),
        HoverTarget::None => None,
      },
      now,
    );
    self.dot_color = crate::overlay::motion::blend_rgb(
      self.dot_from,
      status_dot_color(self.phase),
      self.motion.popup_alpha(now),
    );
    let key = PaintKey {
      opacity: self.motion.toolbar_alpha(now),
      hover_alpha: [
        self.motion.hover_alpha(60, now),
        self.motion.hover_alpha(1, now),
        self.motion.hover_alpha(2, now),
        self.motion.hover_alpha(3, now),
      ],
      microphone: self.microphone,
      dot_color: self.dot_color,
      pressed: self.pressed_target(Instant::now()),
      pulse_alpha: recording_pulse_alpha(self.phase, self.start_time.elapsed()),
      phase: self.phase,
      paused: self.is_paused(),
      seconds: self.elapsed(Instant::now()).as_secs(),
      hover: self.hover,
      position,
    };
    if self.painted == Some(key) && (!was_animating || self.motion.active()) {
      return false;
    }
    self.painted = Some(key);
    true
  }
}

/// The recording toolbar's pointer: the arrow, or the hand over the drag grip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GripCursor {
  Arrow,
  Open,
  Closed,
}

impl GripCursor {
  fn for_pointer(dragging: bool, hover: HoverTarget) -> Self {
    if dragging {
      Self::Closed
    } else if hover == HoverTarget::Drag {
      Self::Open
    } else {
      Self::Arrow
    }
  }
}

fn set_recording_cursor(shell: &mut RecordingOverlayShell, cursor: GripCursor) {
  let hand = if cursor == GripCursor::Arrow {
    None
  } else {
    if shell.hands.is_none() && !shell.hands_failed {
      match crate::overlay::screenshot::cursors::ToolCursors::new() {
        Ok(hands) => shell.hands = Some(hands),
        Err(error) => {
          // Logged once; the Windows hand stands in for the rest of this recording.
          shell.hands_failed = true;
          tracing::warn!(%error, "Cannot create the recording toolbar's grip hands; using the Windows hand");
        },
      }
    }
    shell.hands.as_ref().map(|hands| {
      if cursor == GripCursor::Closed {
        hands.closed_hand()
      } else {
        hands.open_hand()
      }
    })
  };
  let cursor = match hand {
    Some(hand) => Ok(hand),
    None => crate::overlay::screenshot::cursors::system(if cursor == GripCursor::Arrow {
      IDC_ARROW
    } else {
      IDC_HAND
    }),
  };
  match cursor {
    // SAFETY: the shell owns its hands, and Windows the shared cursors, on this overlay thread.
    Ok(cursor) => unsafe {
      SetCursor(Some(cursor));
    },
    Err(error) => tracing::warn!(%error, "Cannot load recording toolbar cursor"),
  }
}

pub fn client_cursor_from_lparam(lparam: LPARAM) -> (i32, i32) {
  (
    (lparam.0 & 0xFFFF) as i16 as i32,
    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
  )
}

pub fn screen_cursor_from_lparam(lparam: LPARAM) -> (i32, i32) {
  client_cursor_from_lparam(lparam)
}

pub fn clamp_toolbar_position(
  monitors: &[crate::monitors::MonitorInfo],
  viewport: (i32, i32, i32, i32),
  pointer: (i32, i32),
  proposed: (i32, i32),
  size: (i32, i32),
) -> (i32, i32) {
  let (offset_x, offset_y, width, height) = viewport;
  let screen_x = pointer.0 + offset_x;
  let screen_y = pointer.1 + offset_y;
  let monitor = monitors
    .iter()
    .filter(|monitor| monitor.width > 0 && monitor.height > 0)
    .min_by_key(|monitor| {
      let x = screen_x.clamp(monitor.x, monitor.x + monitor.width as i32 - 1);
      let y = screen_y.clamp(monitor.y, monitor.y + monitor.height as i32 - 1);
      let dx = i64::from(screen_x) - i64::from(x);
      let dy = i64::from(screen_y) - i64::from(y);
      dx * dx + dy * dy
    });
  let (left, top, right, bottom) = monitor
    .map(|monitor| {
      (
        monitor.x - offset_x,
        monitor.y - offset_y,
        monitor.x - offset_x + monitor.width as i32,
        monitor.y - offset_y + monitor.height as i32,
      )
    })
    .unwrap_or((0, 0, width, height));
  (
    proposed.0.clamp(left, (right - size.0).max(left)),
    proposed.1.clamp(top, (bottom - size.1).max(top)),
  )
}

pub fn fit_recording_bar<M: RecordingOverlayMode>(
  window: &mut RecordingOverlayWindow<M>,
  style: TimerTextStyle,
) -> Result<()> {
  if window.shell.drag_offset.is_some() {
    return Ok(());
  }
  let elapsed = window.shell.elapsed(Instant::now());
  let microphone = window.shell.microphone.is_some();
  let key = (
    window.shell.phase,
    crate::overlay::recording_toolbar::format_elapsed_hms(elapsed).len(),
    style,
    microphone,
  );
  if window.shell.metric_key == Some(key) {
    return Ok(());
  }
  let metrics = crate::overlay::recording_toolbar::with_microphone(
    fitted_metrics(
      &mut window.shell.chrome,
      &window.shell.lang,
      window.shell.phase,
      elapsed,
      style,
    )?,
    microphone,
  );
  let (x, y) = window.mode.toolbar_position();
  let proposed = (x + window.shell.metrics.width - metrics.width, y);
  let monitors = crate::monitors::list_monitors();
  if monitors.is_empty() {
    tracing::warn!("Monitor enumeration failed; resized recording controls use the overlay bounds");
  }
  let position = clamp_toolbar_position(
    &monitors,
    window.mode.viewport(),
    (x, y),
    proposed,
    (metrics.width, metrics.height),
  );
  window.mode.set_toolbar_position(position.0, position.1);
  window.shell.metrics = metrics;
  window.shell.metric_key = Some(key);
  Ok(())
}

pub fn track_mouse_leave(hwnd: HWND) {
  let mut tracking = TRACKMOUSEEVENT {
    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
    dwFlags: TME_LEAVE,
    hwndTrack: hwnd,
    dwHoverTime: 0,
  };
  // SAFETY: the descriptor refers to this owner thread's live overlay window.
  if let Err(error) = unsafe { TrackMouseEvent(&mut tracking) } {
    tracing::warn!(%error, "Cannot track recording toolbar pointer exit");
  }
}

pub fn update_hover(shell: &mut RecordingOverlayShell, hover: HoverTarget) -> bool {
  if shell.hover == hover {
    return false;
  }
  shell.hover = hover;
  true
}

pub fn reset_hover(shell: &mut RecordingOverlayShell) -> bool {
  update_hover(shell, HoverTarget::None)
}

pub fn draw_selection_border(
  bitmap: &mut LayeredBitmap,
  left: i32,
  top: i32,
  right: i32,
  bottom: i32,
  _border_thickness: i32,
) {
  use windows::Win32::Foundation::RECT;
  let bounds = RECT {
    left,
    top,
    right,
    bottom,
  };
  if right <= left || bottom <= top {
    return;
  }
  let accent = crate::overlay::theme::ACCENT;
  let edge = premul_argb(accent[2], accent[1], accent[0], 64);
  for line in [
    RECT {
      bottom: top + 1,
      ..bounds
    },
    RECT {
      top: bottom - 1,
      ..bounds
    },
    RECT {
      right: left + 1,
      ..bounds
    },
    RECT {
      left: right - 1,
      ..bounds
    },
  ] {
    bitmap.fill_rect(
      line.left,
      line.top,
      line.right,
      line.bottom,
      edge.0,
      edge.1,
      edge.2,
      edge.3,
    );
  }
  let mark = premul_argb(accent[2], accent[1], accent[0], 255);
  // Recording bounds are fixed: corner brackets only, without resize grips.
  for line in crate::overlay::gdi::drawing::selection_mark_rects(bounds, false) {
    bitmap.fill_rect(
      line.left,
      line.top,
      line.right,
      line.bottom,
      mark.0,
      mark.1,
      mark.2,
      mark.3,
    );
  }
}
pub fn draw_recording_toolbar(
  bitmap: &mut LayeredBitmap,
  shell: &mut RecordingOverlayShell,
  layout: ToolbarLayout,
  metrics: ToolbarMetrics,
  timer_style: TimerTextStyle,
  now: Instant,
) -> Result<()> {
  let is_paused = shell.is_paused();
  let elapsed = shell.elapsed(now);
  let pressed = shell.pressed_target(now);
  if !shell.precise_clock && shell.motion.active() && !shell.motion_timer {
    if start_overlay_timer(shell.hwnd, MOTION_TIMER_ID, 16) {
      shell.motion_timer = true;
    } else {
      tracing::warn!("Recording motion timer unavailable; completing transitions immediately");
      shell.motion.finish();
    }
  }
  if shell.motion_timer && !shell.motion.active() {
    // SAFETY: this shell owns this timer on its live owner-thread window.
    if let Err(error) = unsafe { KillTimer(Some(shell.hwnd), MOTION_TIMER_ID) } {
      tracing::warn!(%error, "Cannot stop recording motion timer");
    }
    shell.motion_timer = false;
  }
  draw_toolbar_animated(
    bitmap,
    &mut shell.chrome,
    layout,
    metrics,
    shell.hover,
    is_paused,
    elapsed,
    timer_style,
    &shell.lang,
    shell.phase,
    recording_pulse_alpha(shell.phase, now.saturating_duration_since(shell.start_time)),
    pressed,
    ToolbarAnimation {
      opacity: shell.motion.toolbar_alpha(now),
      hover: [
        shell.motion.hover_alpha(60, now),
        shell.motion.hover_alpha(1, now),
        shell.motion.hover_alpha(2, now),
        shell.motion.hover_alpha(3, now),
      ],
      dot_color: shell.dot_color,
      microphone: shell.microphone,
    },
  )
}

pub unsafe fn get_overlay_window<M: 'static>(
  hwnd: HWND,
) -> Option<&'static mut RecordingOverlayWindow<M>> {
  let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut RecordingOverlayWindow<M>;
  if ptr.is_null() {
    None
  } else {
    Some(unsafe { &mut *ptr })
  }
}

pub unsafe extern "system" fn overlay_wnd_proc<M: RecordingOverlayMode>(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  match msg {
    WM_CREATE => handle_overlay_create::<M>(hwnd, lparam),
    WM_DESTROY => handle_overlay_destroy::<M>(hwnd),
    WM_ERASEBKGND => LRESULT(1),
    WM_PAINT => {
      let mut ps = PAINTSTRUCT::default();
      let _ = unsafe { BeginPaint(hwnd, &mut ps) };
      let _ = unsafe { EndPaint(hwnd, &ps) };
      LRESULT(0)
    },
    WM_MOUSEMOVE => {
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
        let (client_x, client_y) = client_cursor_from_lparam(lparam);
        if let Some((dx, dy)) = window.shell.drag_offset {
          set_recording_cursor(&mut window.shell, GripCursor::Closed);
          let viewport = window.mode.viewport();
          let position = clamp_toolbar_position(
            &window.shell.drag_monitors,
            viewport,
            (client_x, client_y),
            (client_x - dx, client_y - dy),
            (window.shell.metrics.width, window.shell.metrics.height),
          );
          window.mode.set_toolbar_position(position.0, position.1);
          M::redraw(window);
          return LRESULT(0);
        }
        let new_hover = M::hover_target(window, client_x, client_y);
        set_recording_cursor(&mut window.shell, GripCursor::for_pointer(false, new_hover));
        if update_hover(&mut window.shell, new_hover) {
          M::redraw(window);
        }
        track_mouse_leave(hwnd);
      }
      LRESULT(0)
    },
    WM_LBUTTONDOWN => {
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
        let (client_x, client_y) = client_cursor_from_lparam(lparam);
        if M::hover_target(window, client_x, client_y) == HoverTarget::Drag {
          let (x, y) = window.mode.toolbar_position();
          window.shell.drag_offset = Some((client_x - x, client_y - y));
          set_recording_cursor(&mut window.shell, GripCursor::Closed);
          window.shell.drag_monitors = crate::monitors::list_monitors();
          if window.shell.drag_monitors.is_empty() {
            tracing::warn!(
              "Monitor enumeration failed; toolbar drag is constrained to the overlay bounds"
            );
          }
          // SAFETY: the current overlay owner thread captures its own live HWND.
          unsafe {
            SetCapture(hwnd);
            if GetCapture() != hwnd {
              window.shell.drag_offset = None;
              tracing::warn!("Recording toolbar could not acquire pointer capture");
            }
          }
        } else {
          let target = M::hover_target(window, client_x, client_y);
          if matches!(
            target,
            HoverTarget::Pause | HoverTarget::Stop | HoverTarget::Microphone
          ) {
            window.shell.pressed = Some((target, Instant::now()));
            M::redraw(window);
          }
          M::on_click(window, hwnd, client_x, client_y);
        }
      }
      LRESULT(0)
    },
    WM_TIMER => {
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
        if wparam.0 == MOTION_TIMER_ID {
          M::redraw(window);
        } else {
          M::on_timer(window, hwnd, wparam.0);
        }
      }
      LRESULT(0)
    },
    windows::Win32::UI::WindowsAndMessaging::WM_DISPLAYCHANGE => {
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
        M::on_display_change(window, hwnd);
      }
      LRESULT(0)
    },
    WM_LBUTTONUP | WM_CAPTURECHANGED => {
      let was_dragging = unsafe { get_overlay_window::<M>(hwnd) }
        .is_some_and(|window| window.shell.drag_offset.take().is_some());
      if msg == WM_LBUTTONUP && was_dragging {
        // SAFETY: release this overlay's capture after releasing its state borrow;
        // ReleaseCapture may synchronously send WM_CAPTURECHANGED to this window.
        if let Err(error) = unsafe { ReleaseCapture() } {
          tracing::debug!(%error, "Recording toolbar capture already released");
        }
        // The hand opens again on release, without waiting for the next move.
        if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
          let (client_x, client_y) = client_cursor_from_lparam(lparam);
          let hover = M::hover_target(window, client_x, client_y);
          set_recording_cursor(&mut window.shell, GripCursor::for_pointer(false, hover));
        }
      }
      LRESULT(0)
    },
    WM_KEYDOWN | WM_SYSKEYDOWN => {
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
        M::on_keydown(window, hwnd, wparam.0 as u32);
      }
      LRESULT(0)
    },
    WM_SETCURSOR => {
      let mut point = POINT::default();
      // SAFETY: cursor position and conversion use this live owner-thread HWND.
      let position_valid =
        unsafe { GetCursorPos(&mut point).is_ok() && ScreenToClient(hwnd, &mut point).as_bool() };
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
        let hover = if position_valid {
          M::hover_target(window, point.x, point.y)
        } else {
          HoverTarget::None
        };
        let dragging = window.shell.drag_offset.is_some();
        set_recording_cursor(&mut window.shell, GripCursor::for_pointer(dragging, hover));
      }
      LRESULT(1)
    },
    WM_NCHITTEST => {
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
        let (screen_x, screen_y) = screen_cursor_from_lparam(lparam);
        return M::hit_test(window, screen_x, screen_y);
      }
      unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    },
    0x02A3 => {
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) }
        && reset_hover(&mut window.shell)
      {
        M::redraw(window);
      }
      LRESULT(0)
    },
    _ => {
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) }
        && let Some(result) = M::on_message(window, hwnd, msg, wparam, lparam)
      {
        return result;
      }
      unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    },
  }
}

fn handle_overlay_create<M: RecordingOverlayMode>(hwnd: HWND, lparam: LPARAM) -> LRESULT {
  let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
  let state_ptr = cs.lpCreateParams as *mut RecordingOverlayWindow<M>;
  if state_ptr.is_null() {
    tracing::error!("Overlay WM_CREATE: null state pointer");
    return LRESULT(-1);
  }

  unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };

  let window = unsafe { &mut *state_ptr };
  window.shell.hwnd = hwnd;
  M::on_create(window, hwnd);
  LRESULT(0)
}

fn handle_overlay_destroy<M: RecordingOverlayMode>(hwnd: HWND) -> LRESULT {
  let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut RecordingOverlayWindow<M>;
  if !ptr.is_null() {
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
    let mut window = unsafe { Box::from_raw(ptr) };
    M::on_destroy(&mut window, hwnd);
  }

  unsafe { PostQuitMessage(0) };
  LRESULT(0)
}

#[must_use]
pub fn start_overlay_timer(hwnd: HWND, timer_id: usize, interval_ms: u32) -> bool {
  let timer_result = unsafe { SetTimer(Some(hwnd), timer_id, interval_ms, None) };
  if timer_result == 0 {
    tracing::warn!(
      timer_id,
      interval_ms,
      "SetTimer failed for recording overlay"
    );
  }
  timer_result != 0
}

pub fn request_overlay_close(hwnd: HWND) {
  // SAFETY: defer destruction until WM_CREATE or the current handler has released its state borrow.
  if let Err(error) = unsafe {
    windows::Win32::UI::WindowsAndMessaging::PostMessageW(
      Some(hwnd),
      windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
      WPARAM(0),
      LPARAM(0),
    )
  } {
    tracing::error!(%error, "Cannot post recording overlay close request");
  }
}

pub fn stop_overlay_timer(hwnd: HWND, timer_id: usize) {
  let _ = unsafe { KillTimer(Some(hwnd), timer_id) };
}

pub fn show_overlay_window(hwnd: HWND, show_cmd: SHOW_WINDOW_CMD) {
  let _ = unsafe { ShowWindow(hwnd, show_cmd) };
}

pub fn destroy_overlay_window(hwnd: HWND) {
  let _ = unsafe { DestroyWindow(hwnd) };
}

pub fn present_overlay_bitmap(bitmap: &LayeredBitmap, hwnd: HWND, x: i32, y: i32) -> Result<()> {
  show_window_topmost(hwnd, x, y, bitmap.width, bitmap.height);
  bitmap
    .apply_to(hwnd, x, y)
    .map_err(|error| anyhow::anyhow!("{error}"))
}

pub fn spawn_overlay_thread<F>(thread_name: &str, run: F) -> Result<()>
where
  F: FnOnce() -> Result<()> + Send + 'static,
{
  let thread_name = thread_name.to_string();
  let log_name = thread_name.clone();
  let err_name = thread_name.clone();
  std::thread::Builder::new()
    .name(thread_name)
    .spawn(move || {
      if let Err(error) = run() {
        tracing::error!(overlay = %log_name, error = %error, "Overlay thread failed");
      }
    })
    .map(|_| ())
    .map_err(|error| anyhow::anyhow!("Failed to spawn {err_name} thread: {error}"))
}

pub fn run_overlay_window_loop<M: RecordingOverlayMode, FCreate, FRecover>(
  class_name: &str,
  wnd_proc: WNDPROC,
  mut state: Box<RecordingOverlayWindow<M>>,
  create_window: FCreate,
  recover_state: FRecover,
) -> Result<()>
where
  FCreate: FnOnce(&str, *mut std::ffi::c_void) -> Result<HWND>,
  FRecover: FnOnce(Box<RecordingOverlayWindow<M>>),
{
  register_overlay_class(class_name, wnd_proc).map_err(|error| anyhow::anyhow!("{error}"))?;

  let clock = match crate::overlay::animation_clock::AnimationClock::new() {
    Ok(clock) => Some(clock),
    Err(error) => {
      tracing::warn!(%error, "Recording animation clock unavailable; using active-only window timer");
      None
    },
  };
  state.shell.precise_clock = clock.is_some();
  let state_ptr = Box::into_raw(state) as *mut std::ffi::c_void;
  let hwnd = match create_window(class_name, state_ptr) {
    Ok(hwnd) => hwnd,
    Err(error) => {
      // SAFETY: failed creation leaves this allocation with the creator, matching existing recovery.
      let state = unsafe { Box::from_raw(state_ptr as *mut RecordingOverlayWindow<M>) };
      recover_state(state);
      unregister_overlay_class(class_name);
      return Err(error);
    },
  };

  if let Some(clock) = clock {
    let mut cadence = crate::overlay::animation_clock::MonitorCadence::default();
    let result = clock.run(hwnd, MOTION_TIMER_ID, |last_tick| {
      // SAFETY: the callback borrows the live recording state only between owner-thread dispatches.
      let window = unsafe { get_overlay_window::<M>(hwnd) }?;
      window
        .shell
        .motion
        .active()
        .then(|| last_tick + cadence.at_cursor())
    });
    if let Err(error) = result {
      tracing::error!(%error, "Recording animation clock failed; disabling transitions and retaining recorder controls");
      // SAFETY: the clock returned on the owner thread; redraw retains the recorder's acknowledged state.
      if let Some(window) = unsafe { get_overlay_window::<M>(hwnd) } {
        window.shell.motion.finish();
        M::redraw(window);
        run_message_loop();
      }
    }
  } else {
    run_message_loop();
  }
  // SAFETY: this thread owns this HWND; handle WM_QUIT paths that precede WM_DESTROY.
  let cleanup = if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
    // SAFETY: destroy only this recording host's window after its message loop exits.
    unsafe { DestroyWindow(hwnd) }.map_err(anyhow::Error::from)
  } else {
    Ok(())
  };
  unregister_overlay_class(class_name);
  cleanup
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_grip_shows_an_open_hand_that_closes_while_dragging() {
    assert_eq!(
      GripCursor::for_pointer(false, HoverTarget::Drag),
      GripCursor::Open
    );
    assert_eq!(
      GripCursor::for_pointer(true, HoverTarget::Drag),
      GripCursor::Closed
    );
    assert_eq!(
      GripCursor::for_pointer(true, HoverTarget::None),
      GripCursor::Closed,
      "the hand stays closed when the pointer outruns a clamped toolbar"
    );
    for hover in [HoverTarget::None, HoverTarget::Pause, HoverTarget::Stop] {
      assert_eq!(GripCursor::for_pointer(false, hover), GripCursor::Arrow);
    }
    let mut shell = RecordingOverlayShell::new(Instant::now(), "en".into());
    set_recording_cursor(&mut shell, GripCursor::Arrow);
    assert!(shell.hands.is_none(), "the arrow needs no hands");
    set_recording_cursor(&mut shell, GripCursor::Open);
    assert!(shell.hands.is_some() && !shell.hands_failed);
    set_recording_cursor(&mut shell, GripCursor::Arrow);
  }

  #[test]
  fn fallback_motion_timer_starts_and_stops_on_its_hidden_owner_without_presenting_chrome() {
    use windows::{Win32::UI::WindowsAndMessaging::*, core::w};
    let hwnd = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        0,
        0,
        640,
        80,
        None,
        None,
        None,
        None,
      )
    }
    .unwrap();
    let mut shell = RecordingOverlayShell::new(Instant::now(), "en".into());
    shell.hwnd = hwnd;
    shell.motion = crate::overlay::motion::Motion::preview(
      Duration::ZERO,
      Duration::ZERO,
      Duration::ZERO,
      1,
      None,
    );
    assert!(shell.motion.active());
    let mut bitmap = LayeredBitmap::new(TOOLBAR_METRICS.width, TOOLBAR_METRICS.height).unwrap();
    let layout = crate::overlay::recording_toolbar::toolbar_layout(TOOLBAR_METRICS, 0, 0);
    draw_recording_toolbar(
      &mut bitmap,
      &mut shell,
      layout,
      TOOLBAR_METRICS,
      TimerTextStyle::Hms,
      Instant::now(),
    )
    .unwrap();
    assert!(shell.motion_timer);
    shell.motion.finish();
    draw_recording_toolbar(
      &mut bitmap,
      &mut shell,
      layout,
      TOOLBAR_METRICS,
      TimerTextStyle::Hms,
      Instant::now(),
    )
    .unwrap();
    assert!(!shell.motion_timer);
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
    unsafe { DestroyWindow(hwnd) }.unwrap();
  }

  #[test]
  fn hidden_recording_host_animates_and_retains_backend_polling() {
    use std::sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
    };
    struct Probe {
      paints: Arc<AtomicUsize>,
      polls: Arc<AtomicUsize>,
      routes: Arc<AtomicUsize>,
      bitmap: LayeredBitmap,
    }
    impl RecordingOverlayMode for Probe {
      fn on_create(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND) {
        if !start_overlay_timer(hwnd, 77, 250) {
          request_overlay_close(hwnd);
          return;
        }
        Self::redraw(window);
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP, WM_DISPLAYCHANGE};
        // Queue owner-thread messages after WM_CREATE releases its state borrow.
        // No mouse motion/capture, cursor changes, focus or visible presentation.
        for (message, wp, lp) in [
          (WM_LBUTTONDOWN, 0, (17 << 16) | 12),
          (WM_LBUTTONUP, 0, 0),
          (WM_KEYDOWN, 65, 0),
          (WM_DISPLAYCHANGE, 0, 0),
          (WM_APP + 99, 0, 0),
          (WM_PAINT, 0, 0),
          (0x02A3, 0, 0),
        ] {
          unsafe { PostMessageW(Some(hwnd), message, WPARAM(wp), LPARAM(lp)) }.unwrap();
        }
      }
      fn on_destroy(_: &mut RecordingOverlayWindow<Self>, _: HWND) {}
      fn on_timer(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND, _: usize) {
        assert_eq!(window.mode.routes.load(Ordering::SeqCst), 29);
        if window.mode.polls.fetch_add(1, Ordering::SeqCst) == 0 {
          window
            .shell
            .acknowledge_pause(true, Duration::from_secs(12), Instant::now());
          Self::redraw(window);
        } else {
          destroy_overlay_window(hwnd);
        }
      }
      fn on_click(window: &mut RecordingOverlayWindow<Self>, _: HWND, x: i32, y: i32) {
        assert_eq!((x, y), (12, 17));
        window.mode.routes.fetch_or(1, Ordering::SeqCst);
      }
      fn hover_target(_: &RecordingOverlayWindow<Self>, x: i32, y: i32) -> HoverTarget {
        if (x, y) == (12, 17) {
          HoverTarget::Pause
        } else {
          HoverTarget::None
        }
      }
      fn hit_test(window: &RecordingOverlayWindow<Self>, x: i32, y: i32) -> LRESULT {
        let _ = (window, x, y);
        LRESULT(0)
      }
      fn on_keydown(window: &mut RecordingOverlayWindow<Self>, _: HWND, key: u32) {
        assert_eq!(key, 65);
        window.mode.routes.fetch_or(4, Ordering::SeqCst);
      }
      fn on_message(
        window: &mut RecordingOverlayWindow<Self>,
        _: HWND,
        message: u32,
        _: WPARAM,
        _: LPARAM,
      ) -> Option<LRESULT> {
        if message == windows::Win32::UI::WindowsAndMessaging::WM_APP + 99 {
          window.mode.routes.fetch_or(8, Ordering::SeqCst);
          Some(LRESULT(42))
        } else {
          None
        }
      }
      fn redraw(window: &mut RecordingOverlayWindow<Self>) {
        if !window.shell.needs_paint((0, 0)) {
          return;
        }
        window.mode.bitmap.clear();
        draw_recording_toolbar(
          &mut window.mode.bitmap,
          &mut window.shell,
          crate::overlay::recording_toolbar::toolbar_layout(TOOLBAR_METRICS, 0, 0),
          TOOLBAR_METRICS,
          TimerTextStyle::Hms,
          Instant::now(),
        )
        .expect("hidden HUD paint");
        window.mode.paints.fetch_add(1, Ordering::SeqCst);
      }
      fn toolbar_position(&self) -> (i32, i32) {
        (0, 0)
      }
      fn set_toolbar_position(&mut self, _: i32, _: i32) {}
      fn viewport(&self) -> (i32, i32, i32, i32) {
        (0, 0, 640, 480)
      }
      fn on_display_change(window: &mut RecordingOverlayWindow<Self>, _: HWND) {
        window.mode.routes.fetch_or(16, Ordering::SeqCst);
      }
    }
    let paints = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let mut shell = RecordingOverlayShell::new(Instant::now(), "en".into());
    shell.motion = crate::overlay::motion::Motion::preview(
      Duration::ZERO,
      Duration::ZERO,
      Duration::ZERO,
      shell.phase as u8 + 1,
      None,
    );
    let state = Box::new(RecordingOverlayWindow {
      shell,
      mode: Probe {
        paints: paints.clone(),
        polls: polls.clone(),
        routes: Arc::new(AtomicUsize::new(0)),
        bitmap: LayeredBitmap::new(TOOLBAR_METRICS.width, TOOLBAR_METRICS.height).expect("fixture"),
      },
    });
    let class = crate::overlay::gdi::window::class_name_for_thread();
    run_overlay_window_loop(
      &class,
      Some(overlay_wnd_proc::<Probe>),
      state,
      |name, pointer| {
        crate::overlay::gdi::window::create_hud_window(
          name,
          "Flinch hidden motion fixture",
          0,
          0,
          TOOLBAR_METRICS.width,
          TOOLBAR_METRICS.height,
          pointer,
        )
        .map_err(anyhow::Error::msg)
      },
      |_| {},
    )
    .expect("hidden host");
    assert_eq!(
      polls.load(Ordering::SeqCst),
      2,
      "recorder polling must continue independently"
    );
    assert!(
      paints.load(Ordering::SeqCst) > 4,
      "entrance and acknowledgment must have intermediate frames"
    );
  }

  fn monitor(x: i32, y: i32, width: u32, height: u32) -> crate::monitors::MonitorInfo {
    crate::monitors::MonitorInfo {
      id: String::new(),
      name: String::new(),
      x,
      y,
      width,
      height,
      is_primary: x == 0,
      dpi_scale: 1.0,
      logical_x: x,
      logical_y: y,
      logical_width: width,
      logical_height: height,
    }
  }

  #[test]
  fn drag_clamps_to_real_monitors_in_negative_and_mixed_height_layouts() {
    let monitors = [monitor(-1920, 0, 1920, 1080), monitor(0, 0, 2560, 1440)];
    let viewport = (-1920, 0, 4480, 1440);
    assert_eq!(
      clamp_toolbar_position(&monitors, viewport, (100, 100), (-20, -30), (428, 46)),
      (0, 0)
    );
    assert_eq!(
      clamp_toolbar_position(&monitors, viewport, (1800, 1070), (1800, 1200), (428, 46)),
      (1492, 1034)
    );
    assert_eq!(
      clamp_toolbar_position(&monitors, viewport, (4300, 1400), (4300, 1450), (428, 46)),
      (4052, 1394)
    );
    // A pointer in the gap below the shorter monitor uses its nearest real monitor.
    assert_eq!(
      clamp_toolbar_position(&monitors, viewport, (1000, 1300), (1000, 1300), (428, 46)),
      (1000, 1034)
    );
  }

  #[test]
  fn hover_motion_preserves_recorder_state_and_final_paint_settles() {
    let now = Instant::now();
    let mut shell = RecordingOverlayShell::new(now, "en".into());
    shell.acknowledge_pause(false, Duration::from_secs(12), now);
    shell.motion = crate::overlay::motion::Motion::preview(
      Duration::from_secs(1),
      Duration::from_secs(1),
      Duration::ZERO,
      shell.phase as u8 + 1,
      Some(1),
    );
    update_hover(&mut shell, HoverTarget::Pause);
    assert!(shell.needs_paint((10, 10)));
    assert!(
      !shell.is_paused(),
      "hover must never request or acknowledge pause"
    );
    assert_eq!(shell.phase, RecordingPhase::Recording);
    shell.motion = crate::overlay::motion::Motion::preview(
      Duration::from_secs(1),
      Duration::from_secs(1),
      Duration::from_secs(1),
      shell.phase as u8 + 1,
      Some(1),
    );
    assert!(
      shell.needs_paint((10, 10)),
      "settled frame is still painted"
    );
    assert!(!shell.motion.active());
    shell.acknowledge_pause(true, Duration::from_secs(12), now);
    assert!(shell.is_paused());
    assert_eq!(shell.phase, RecordingPhase::Paused);
  }

  #[test]
  fn recorder_acknowledgments_anchor_the_timer_and_unchanged_states_skip_paint() {
    let now = Instant::now();
    let mut shell = RecordingOverlayShell::new(now, "en".into());
    shell.acknowledge_pause(true, Duration::from_secs(12), now);
    assert_eq!(
      shell.elapsed(now + Duration::from_secs(10)),
      Duration::from_secs(12)
    );
    assert_eq!(shell.phase, RecordingPhase::Paused);
    assert!(shell.needs_paint((100, 200)));
    assert!(!shell.needs_paint((100, 200)));
    assert!(shell.needs_paint((110, 200)));
    shell.phase = RecordingPhase::Resuming;
    assert!(shell.needs_paint((110, 200)));
    shell.acknowledge_pause(
      false,
      Duration::from_secs(12),
      now + Duration::from_secs(20),
    );
    assert_eq!(
      shell.elapsed(now + Duration::from_secs(23)),
      Duration::from_secs(15)
    );
    assert_eq!(shell.phase, RecordingPhase::Recording);
  }
}
