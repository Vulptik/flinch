//! Area recording overlay — fullscreen layered window showing the selection
//! border and a timer / Pause / Stop toolbar during video recording.
//!
//! The window covers the full virtual desktop but is nearly transparent.
//! Only the selection border lines and toolbar have non-zero alpha, so the
//! user still sees live screen content everywhere else.
//!
//! `WM_NCHITTEST` returns `HTTRANSPARENT` (-1) outside the toolbar buttons,
//! making non-interactive areas click-through to windows below.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LRESULT};
use windows::Win32::UI::WindowsAndMessaging::{
  GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
  SW_SHOWNOACTIVATE,
};

use crate::monitors::list_monitors;
use crate::overlay::core::place_toolbar_near_selection;
use crate::overlay::gdi::layered::LayeredBitmap;
use crate::overlay::gdi::window::{
  class_name_for_thread, create_hud_window, disable_window_animations, exclude_from_capture,
};
use crate::overlay::recording_shell::{
  RecordingOverlayMode, RecordingOverlayShell, RecordingOverlayWindow, destroy_overlay_window,
  draw_recording_toolbar, draw_selection_border, fit_recording_bar, overlay_wnd_proc,
  present_overlay_bitmap, request_overlay_close, run_overlay_window_loop, show_overlay_window,
  spawn_overlay_thread, start_overlay_timer,
};
use crate::overlay::recording_toolbar::{
  HoverTarget, MicrophoneState, RecordingPhase, TOOLBAR_GAP, TOOLBAR_MARGIN, TimerTextStyle,
  fitted_metrics, toolbar_hover, toolbar_layout,
};

const HUD_TIMER_ID: usize = 1;
const BORDER_T: i32 = 2;

struct AreaHudMode {
  bitmap: LayeredBitmap,
  win_x: i32,
  win_y: i32,
  #[allow(dead_code)]
  win_w: i32,
  #[allow(dead_code)]
  win_h: i32,
  sel_l: i32,
  sel_t: i32,
  sel_r: i32,
  sel_b: i32,
  tbar_l: i32,
  tbar_t: i32,
  controls: Arc<crate::state::RecordingControls>,
}

/// Spawn the area recording border+toolbar overlay on its own thread.
/// `sel_x1/y1/x2/y2` are absolute screen coordinates of the recording region.
/// Returns immediately — the thread closes itself when `stop_flag` is set.
pub fn start_area_hud(
  sel_x1: f64,
  sel_y1: f64,
  sel_x2: f64,
  sel_y2: f64,
  start_time: Instant,
  controls: Arc<crate::state::RecordingControls>,
  lang: String,
) -> anyhow::Result<()> {
  spawn_overlay_thread("area-recording-hud", move || {
    run_thread(sel_x1, sel_y1, sel_x2, sel_y2, start_time, controls, lang)
  })
}

fn run_thread(
  sel_x1: f64,
  sel_y1: f64,
  sel_x2: f64,
  sel_y2: f64,
  start_time: Instant,
  controls: Arc<crate::state::RecordingControls>,
  lang: String,
) -> anyhow::Result<()> {
  let win_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
  let win_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
  let win_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
  let win_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };

  let state = build_area_hud(
    (win_x, win_y, win_w, win_h),
    (sel_x1, sel_y1, sel_x2, sel_y2),
    start_time,
    controls,
    lang,
  )?;

  let class_name = class_name_for_thread();
  run_overlay_window_loop(
    &class_name,
    Some(overlay_wnd_proc::<AreaHudMode>),
    state,
    |class_name, state_ptr| {
      let hwnd = create_hud_window(
        class_name,
        "Flinch Area Recording Overlay",
        win_x,
        win_y,
        win_w,
        win_h,
        state_ptr,
      )
      .map_err(|error| anyhow::anyhow!("create_hud_window: {error}"))?;
      disable_window_animations(hwnd);
      exclude_from_capture(hwnd);
      Ok(hwnd)
    },
    |_| {},
  )?;

  Ok(())
}

fn build_area_hud(
  screen: (i32, i32, i32, i32),
  selection: (f64, f64, f64, f64),
  start_time: Instant,
  controls: Arc<crate::state::RecordingControls>,
  lang: String,
) -> anyhow::Result<Box<RecordingOverlayWindow<AreaHudMode>>> {
  let (win_x, win_y, win_w, win_h) = screen;
  let (sel_x1, sel_y1, sel_x2, sel_y2) = selection;
  let sel_l = sel_x1 as i32 - win_x;
  let sel_t = sel_y1 as i32 - win_y;
  let sel_r = sel_x2 as i32 - win_x;
  let sel_b = sel_y2 as i32 - win_y;

  let mut shell = RecordingOverlayShell::new(start_time, lang);
  shell.metrics = fitted_metrics(
    &mut shell.chrome,
    &shell.lang,
    RecordingPhase::Starting,
    std::time::Duration::ZERO,
    TimerTextStyle::Hms,
  )?;
  let monitors = list_monitors();
  let (tbar_l, tbar_t) = place_toolbar_near_selection(
    &monitors,
    win_x,
    win_y,
    (sel_l, sel_t, sel_r, sel_b),
    (shell.metrics.width, shell.metrics.height),
    TOOLBAR_GAP,
    TOOLBAR_MARGIN,
    (win_w, win_h),
  );

  Ok(Box::new(RecordingOverlayWindow {
    shell,
    mode: AreaHudMode {
      bitmap: LayeredBitmap::new(win_w, win_h).map_err(|e| anyhow::anyhow!("{e}"))?,
      win_x,
      win_y,
      win_w,
      win_h,
      sel_l,
      sel_t,
      sel_r,
      sel_b,
      tbar_l,
      tbar_t,
      controls,
    },
  }))
}

impl RecordingOverlayMode for AreaHudMode {
  fn on_create(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND) {
    if !start_overlay_timer(hwnd, HUD_TIMER_ID, 200) {
      window.mode.controls.request_stop();
      request_overlay_close(hwnd);
      return;
    }
    Self::redraw(window);
    show_overlay_window(hwnd, SW_SHOWNOACTIVATE);
  }

  fn on_destroy(_window: &mut RecordingOverlayWindow<Self>, _hwnd: HWND) {}

  fn on_timer(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND, _timer_id: usize) {
    if window.mode.controls.pause_failed.load(Ordering::SeqCst) {
      window.mode.controls.request_stop();
      destroy_overlay_window(hwnd);
      return;
    }
    if window.mode.controls.stop_requested() {
      destroy_overlay_window(hwnd);
      return;
    }

    Self::sync_controls(window);
    Self::redraw(window);
  }

  fn on_click(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND, client_x: i32, client_y: i32) {
    Self::click_with(window, hwnd, client_x, client_y, Self::redraw);
  }

  fn hover_target(
    window: &RecordingOverlayWindow<Self>,
    client_x: i32,
    client_y: i32,
  ) -> HoverTarget {
    let layout = toolbar_layout(window.shell.metrics, window.mode.tbar_l, window.mode.tbar_t);
    toolbar_hover(client_x, client_y, layout)
  }

  fn hit_test(window: &RecordingOverlayWindow<Self>, screen_x: i32, screen_y: i32) -> LRESULT {
    let client_x = screen_x - window.mode.win_x;
    let client_y = screen_y - window.mode.win_y;
    if Self::hover_target(window, client_x, client_y) != HoverTarget::None {
      LRESULT(1)
    } else {
      LRESULT(-1)
    }
  }

  fn redraw(window: &mut RecordingOverlayWindow<Self>) {
    Self::render_with(window, present_overlay_bitmap);
  }

  fn toolbar_position(&self) -> (i32, i32) {
    (self.tbar_l, self.tbar_t)
  }
  fn set_toolbar_position(&mut self, x: i32, y: i32) {
    self.tbar_l = x;
    self.tbar_t = y;
  }
  fn viewport(&self) -> (i32, i32, i32, i32) {
    (self.win_x, self.win_y, self.win_w, self.win_h)
  }
  fn on_display_change(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND) {
    tracing::warn!("Display topology changed; stopping recording with obsolete capture bounds");
    window.mode.controls.request_stop();
    destroy_overlay_window(hwnd);
  }
}

impl AreaHudMode {
  fn click_with(
    window: &mut RecordingOverlayWindow<Self>,
    hwnd: HWND,
    client_x: i32,
    client_y: i32,
    redraw: impl FnOnce(&mut RecordingOverlayWindow<Self>),
  ) {
    match Self::hover_target(window, client_x, client_y) {
      HoverTarget::Stop => {
        window.mode.controls.request_stop();
        destroy_overlay_window(hwnd);
      },
      HoverTarget::Pause => {
        if !window.shell.phase.can_pause() {
          return;
        }
        let paused = !window.shell.is_paused();
        window.mode.controls.request_pause(paused);
        window.shell.phase = if paused {
          RecordingPhase::Pausing
        } else {
          RecordingPhase::Resuming
        };
        redraw(window);
      },
      HoverTarget::Microphone => {
        let controls = &window.mode.controls;
        let audible = match window.shell.microphone {
          Some(MicrophoneState::Audible) => false,
          Some(MicrophoneState::Muted) => true,
          Some(MicrophoneState::Lost) | None => return,
        };
        if !window.shell.phase.can_pause() || controls.microphone_pending() {
          return;
        }
        controls.request_microphone(audible);
        if controls.microphone_pending() {
          // Shown at once; the recorder's answer corrects it if the switch is refused.
          window.shell.microphone = Some(if audible {
            MicrophoneState::Audible
          } else {
            MicrophoneState::Muted
          });
          redraw(window);
        }
      },
      HoverTarget::None | HoverTarget::Drag => {},
    }
  }

  fn sync_controls(window: &mut RecordingOverlayWindow<Self>) {
    let controls = &window.mode.controls;
    let now_paused = controls.paused();
    window.shell.phase = if !controls.ready.load(Ordering::SeqCst) {
      RecordingPhase::Starting
    } else if controls.pause_pending.load(Ordering::SeqCst) {
      if now_paused {
        RecordingPhase::Resuming
      } else {
        RecordingPhase::Pausing
      }
    } else if now_paused {
      RecordingPhase::Paused
    } else {
      RecordingPhase::Recording
    };
    window.shell.confirmed_elapsed = Some(std::time::Duration::from_millis(
      controls.elapsed_ms.load(Ordering::SeqCst),
    ));
    window
      .shell
      .pause_state
      .sync_from_flag(now_paused, Instant::now());
    if !controls.microphone_pending() {
      window.shell.microphone = controls.microphone();
    }
  }

  fn render_with(
    window: &mut RecordingOverlayWindow<Self>,
    present: impl FnOnce(&LayeredBitmap, HWND, i32, i32) -> anyhow::Result<()>,
  ) {
    if let Err(error) = fit_recording_bar(window, TimerTextStyle::Hms) {
      tracing::error!(%error, "Cannot measure recording controls");
      return;
    }
    if !window.shell.needs_paint(window.mode.toolbar_position()) {
      return;
    }
    let measured_start = window.shell.diagnostics.as_ref().map(|_| Instant::now());
    window.mode.bitmap.clear();
    draw_selection_border(
      &mut window.mode.bitmap,
      window.mode.sel_l,
      window.mode.sel_t,
      window.mode.sel_r,
      window.mode.sel_b,
      BORDER_T,
    );
    let layout = toolbar_layout(window.shell.metrics, window.mode.tbar_l, window.mode.tbar_t);
    let metrics = window.shell.metrics;
    if let Err(error) = draw_recording_toolbar(
      &mut window.mode.bitmap,
      &mut window.shell,
      layout,
      metrics,
      TimerTextStyle::Hms,
      Instant::now(),
    ) {
      tracing::error!(%error, "Recording toolbar rendering failed");
      return;
    }

    if let Err(error) = present(
      &window.mode.bitmap,
      window.shell.hwnd,
      window.mode.win_x,
      window.mode.win_y,
    ) {
      tracing::warn!(operation = "area_recording_hud", error = %error, "apply_to failed");
    }
    if let Some(started) = measured_start
      && let Some(diagnostics) = &mut window.shell.diagnostics
    {
      diagnostics.record(started);
    }
  }
}

#[cfg(test)]
mod tests;
