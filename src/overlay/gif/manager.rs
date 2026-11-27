use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
  GWL_EXSTYLE, GetWindowLongPtrW, SW_HIDE, SW_SHOW, SetWindowLongPtrW, WM_APP, WS_EX_LAYERED,
};

use super::GifResult;
use super::commands::GifRecordingConfig;
use crate::capture_plan::{
  CaptureCoordinateSpace, SharedCapturePlanInput, SharedCapturePlanOptions,
  build_shared_capture_plan,
};
use crate::commands::video::startup::{
  PreparedVideoStart, prepare_gif_capture_plan, start_prepared_recording_blocking,
};
use crate::monitors::list_monitors;
use crate::overlay::core::place_toolbar_near_selection;
use crate::overlay::gdi::layered::LayeredBitmap;
use crate::overlay::gdi::window::{
  class_name_for_thread, create_selection_window, disable_window_animations, exclude_from_capture,
};
use crate::overlay::recording_shell::{
  RecordingOverlayMode, RecordingOverlayShell, RecordingOverlayWindow, destroy_overlay_window,
  draw_recording_toolbar, draw_selection_border, fit_recording_bar, overlay_wnd_proc,
  present_overlay_bitmap, request_overlay_close, run_overlay_window_loop, show_overlay_window,
  spawn_overlay_thread, start_overlay_timer, stop_overlay_timer,
};
use crate::overlay::recording_toolbar::{
  HoverTarget, RecordingPhase, TOOLBAR_GAP, TOOLBAR_MARGIN, TimerTextStyle, ToolbarMetrics,
  fitted_metrics, toolbar_hover, toolbar_layout,
};
use crate::overlay::selection::{Rect, Selection};
use crate::overlay::selection_core::SelectionDrawColors;
use crate::overlay::selection_host::{
  SelectionOverlayAdapter, SelectionOverlayWindowState, show_selection_overlay as show_host,
};

const WM_APP_STOP: u32 = WM_APP + 2;
const WM_APP_PAUSE: u32 = WM_APP + 3;
const WM_APP_RESUME: u32 = WM_APP + 4;

const RECORD_TIMER_ID: usize = 1;
const MAX_DURATION_TIMER_ID: usize = 2;
const BORDER_T: i32 = 2;

static GIF_OVERLAY_HWND: AtomicIsize = AtomicIsize::new(0);

#[cfg_attr(test, derive(Debug, PartialEq, Clone))]
enum RecordingCompletion {
  Started(Result<Duration, String>),
  WorkerFailed(String),
  Paused(Result<(bool, Duration), String>),
}

struct GifRecordingMode {
  width: i32,
  height: i32,
  vd_offset_x: i32,
  vd_offset_y: i32,
  selection_rect: Rect,
  metrics: ToolbarMetrics,
  toolbar_left: i32,
  toolbar_top: i32,
  output_width: u32,
  output_height: u32,
  launch_started_at: Instant,
  recording_bitmap: Option<LayeredBitmap>,
  config: GifRecordingConfig,
  prepared_start: Option<PreparedVideoStart>,
  recording_started: bool,
  control_failed: bool,
  startup_dispatched: bool,
  completion_rx: Option<Receiver<RecordingCompletion>>,
  operation_started: Option<Instant>,
  stop_after_operation: bool,
  response_tx: Option<std::sync::mpsc::Sender<Option<GifResult>>>,
}

struct GifSelectionAdapter;

impl SelectionOverlayAdapter for GifSelectionAdapter {
  type Result = Rect;

  const THREAD_NAME: &'static str = "gif-selection-overlay";
  const WINDOW_TITLE: &'static str = "FlinchGifSelectionOverlay";
  const METRICS_PREFIX: &'static str = "overlay.gif.selection";
  // GIFs are encoded from an H.264 recording first.
  const MINIMUM_SELECTION_SIZE: f64 = crate::overlay::selection::RECORDING_MIN_SELECTION_SIZE;

  fn colors() -> SelectionDrawColors {
    SelectionDrawColors {
      border: windows::Win32::Foundation::COLORREF(0x002F2FD9),
      preview: windows::Win32::Foundation::COLORREF(0x006666FF),
    }
  }

  fn map_selection(
    _state: &SelectionOverlayWindowState<Self>,
    selection: Selection,
  ) -> Self::Result {
    selection_to_rect(selection)
  }

  fn finalize_result(_state: &SelectionOverlayWindowState<Self>, _result: &mut Self::Result) {}
}

pub fn show_gif_selection(
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  config: GifRecordingConfig,
) -> anyhow::Result<Option<GifResult>> {
  let options = crate::overlay::selection_host::SelectionOptions::for_app(&config.app_handle);
  let selection =
    show_host::<GifSelectionAdapter>(width, height, rgba, vd_offset_x, vd_offset_y, options)?;
  let Some(selection_rect) = selection else {
    return Ok(None);
  };
  crate::last_region::remember(
    &config.app_handle,
    crate::last_region::Region {
      x: vd_offset_x.saturating_add(selection_rect.x.round() as i32),
      y: vd_offset_y.saturating_add(selection_rect.y.round() as i32),
      width: selection_rect.w.round() as u32,
      height: selection_rect.h.round() as u32,
    },
  );
  // Capture Delay counts down once the area is picked, so recording starts when the user is ready.
  if !crate::capture_delay::wait_blocking(&config.app_handle, crate::capture_delay::Trigger::Gif)
    .proceeds()
  {
    return Ok(None);
  }

  let (response_tx, response_rx) = std::sync::mpsc::channel();
  let launch_started_at = Instant::now();
  let state = Box::new(build_recording_mode(
    width as i32,
    height as i32,
    vd_offset_x,
    vd_offset_y,
    selection_rect,
    config,
    response_tx,
    launch_started_at,
  )?);

  spawn_gif_recording_overlay(state)?;

  crate::overlay::channel::recv_overlay_result("GIF overlay", response_rx)
}

pub fn stop_gif_recording_external() {
  post_gif_message(WM_APP_STOP);
}

pub fn pause_gif_recording_external() {
  post_gif_message(WM_APP_PAUSE);
}

pub fn resume_gif_recording_external() {
  post_gif_message(WM_APP_RESUME);
}

fn post_gif_message(message: u32) {
  let hwnd_val = GIF_OVERLAY_HWND.load(Ordering::SeqCst);
  if hwnd_val == 0 {
    return;
  }

  let hwnd = HWND(hwnd_val as *mut std::ffi::c_void);
  // SAFETY: the published overlay HWND is withdrawn by its owner before teardown.
  if let Err(error) = unsafe {
    windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0))
  } {
    tracing::error!(%error, message, "Cannot submit GIF recording control");
  }
}

fn spawn_gif_recording_overlay(mode: Box<GifRecordingMode>) -> anyhow::Result<()> {
  let start_time = Instant::now();
  let lang = crate::i18n::get_lang(&mode.config.app_handle);
  spawn_overlay_thread("gif-recording-overlay", move || {
    // Construct window/render state here so Direct2D resources never need Send.
    let mut shell = RecordingOverlayShell::new(start_time, lang);
    shell.metrics = mode.metrics;
    let state = Box::new(RecordingOverlayWindow { shell, mode: *mode });
    let class_name = class_name_for_thread();
    let width = state.mode.width;
    let height = state.mode.height;
    let vd_offset_x = state.mode.vd_offset_x;
    let vd_offset_y = state.mode.vd_offset_y;
    run_overlay_window_loop(
      &class_name,
      Some(overlay_wnd_proc::<GifRecordingMode>),
      state,
      move |class_name, state_ptr| {
        create_selection_window(
          class_name,
          "FlinchGifOverlay",
          vd_offset_x,
          vd_offset_y,
          width,
          height,
          state_ptr,
        )
        .map_err(|error| anyhow::anyhow!("Failed to create GIF overlay window: {}", error))
      },
      gif_overlay_error,
    )
  })
}

fn gif_overlay_error(state: Box<RecordingOverlayWindow<GifRecordingMode>>) {
  crate::overlay::channel::send_overlay_result("GIF overlay", state.mode.response_tx, None);
}

#[allow(clippy::too_many_arguments)]
fn build_recording_mode(
  width: i32,
  height: i32,
  vd_offset_x: i32,
  vd_offset_y: i32,
  selection_rect: Rect,
  config: GifRecordingConfig,
  response_tx: std::sync::mpsc::Sender<Option<GifResult>>,
  launch_started_at: Instant,
) -> anyhow::Result<GifRecordingMode> {
  build_recording_mode_with(
    width,
    height,
    vd_offset_x,
    vd_offset_y,
    selection_rect,
    config,
    response_tx,
    launch_started_at,
    |plan, config, source_app| {
      prepare_gif_capture_plan(
        &config.app_handle,
        plan.capture_x,
        plan.capture_y,
        plan.capture_width,
        plan.capture_height,
        config.fps,
        source_app,
      )
    },
  )
}

#[allow(clippy::too_many_arguments)]
fn build_recording_mode_with(
  width: i32,
  height: i32,
  vd_offset_x: i32,
  vd_offset_y: i32,
  selection_rect: Rect,
  config: GifRecordingConfig,
  response_tx: std::sync::mpsc::Sender<Option<GifResult>>,
  launch_started_at: Instant,
  prepare: impl FnOnce(
    &crate::capture_plan::SharedCapturePlan,
    &GifRecordingConfig,
    Option<String>,
  ) -> Result<PreparedVideoStart, String>,
) -> anyhow::Result<GifRecordingMode> {
  let screen_x = selection_rect.x as i32 + vd_offset_x;
  let screen_y = selection_rect.y as i32 + vd_offset_y;
  let crop_w = selection_rect.w as u32;
  let crop_h = selection_rect.h as u32;
  let center_x = screen_x + (crop_w as i32 / 2);
  let center_y = screen_y + (crop_h as i32 / 2);

  let all_monitors = crate::monitors::list_monitors();
  let capture_plan = build_shared_capture_plan(
    SharedCapturePlanInput {
      x: screen_x,
      y: screen_y,
      width: crop_w,
      height: crop_h,
      show_cursor: config.show_cursor,
      coordinate_space: CaptureCoordinateSpace::Physical,
    },
    SharedCapturePlanOptions::GIF,
    &all_monitors,
  );

  let source_app =
    crate::overlay::core::window_detection::detect_window_at_point(center_x, center_y, None)
      .and_then(|window| {
        crate::overlay::core::window_detection::get_process_name_for_hwnd(window.hwnd)
      });

  let prepared_start = prepare(&capture_plan, &config, source_app).map_err(anyhow::Error::msg)?;

  let metrics = fitted_metrics(
    &mut crate::overlay::chrome::Renderer::default(),
    &crate::i18n::get_lang(&config.app_handle),
    RecordingPhase::Starting,
    Duration::ZERO,
    TimerTextStyle::Gif {
      limit_secs: config.max_duration_secs,
    },
  )?;
  let monitors = list_monitors();
  let selection_bounds = (
    selection_rect.x as i32,
    selection_rect.y as i32,
    (selection_rect.x + selection_rect.w) as i32,
    (selection_rect.y + selection_rect.h) as i32,
  );
  let (toolbar_left, toolbar_top) = place_toolbar_near_selection(
    &monitors,
    vd_offset_x,
    vd_offset_y,
    selection_bounds,
    (metrics.width, metrics.height),
    TOOLBAR_GAP,
    TOOLBAR_MARGIN,
    (width, height),
  );

  let recording_bitmap = if config.presentation_mode.is_silent() {
    None
  } else {
    Some(LayeredBitmap::new(width, height).map_err(|e| anyhow::anyhow!("{e}"))?)
  };

  Ok(GifRecordingMode {
    width,
    height,
    vd_offset_x,
    vd_offset_y,
    selection_rect,
    metrics,
    toolbar_left,
    toolbar_top,
    output_width: capture_plan.output_width,
    output_height: capture_plan.output_height,
    launch_started_at,
    recording_bitmap,
    config,
    prepared_start: Some(prepared_start),
    recording_started: false,
    control_failed: false,
    startup_dispatched: false,
    completion_rx: None,
    operation_started: None,
    stop_after_operation: false,
    response_tx: Some(response_tx),
  })
}

fn update_gif_session_state(
  app: &crate::app::context::AppHandle,
  paused: Option<bool>,
  clear: bool,
  presentation_mode: crate::recording_visibility::RecordingPresentationMode,
) {
  if clear {
    crate::commands::clear_recording_session(app);
    return;
  }

  if crate::commands::current_recording_session(app).is_none() {
    crate::commands::set_recording_session(
      app,
      crate::commands::ActiveRecordingKind::Gif,
      presentation_mode,
    );
  }

  if let Some(paused) = paused {
    crate::commands::set_recording_session_paused(app, paused);
  }
}

impl RecordingOverlayMode for GifRecordingMode {
  fn on_create(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND) {
    GIF_OVERLAY_HWND.store(hwnd.0 as isize, Ordering::SeqCst);

    disable_window_animations(hwnd);
    exclude_from_capture(hwnd);

    unsafe {
      let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
      SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
    }

    if !start_overlay_timer(hwnd, RECORD_TIMER_ID, 200) {
      show_recording_failure(&window.mode.config.app_handle);
      request_overlay_close(hwnd);
      return;
    }

    if window.mode.config.presentation_mode.is_silent() {
      show_overlay_window(hwnd, SW_HIDE);
    } else {
      Self::redraw(window);
      show_overlay_window(hwnd, SW_SHOW);
      crate::overlay::gdi::window::take_foreground(hwnd);
    }

    crate::commands::log_speed_snapshot(
      "overlay.gif.recording_ui_ready",
      window.mode.launch_started_at.elapsed(),
    );

    let Some(prepared_start) = window.mode.prepared_start.take() else {
      tracing::error!(
        operation = "gif_overlay",
        "Prepared GIF recording start was missing during startup"
      );
      request_overlay_close(hwnd);
      return;
    };

    let app = window.mode.config.app_handle.clone();
    window.mode.startup_dispatched = true;
    window.mode.operation_started = Some(Instant::now());
    window.mode.completion_rx = Some(dispatch_recording_operation(move || {
      RecordingCompletion::Started(
        start_prepared_recording_blocking(&app, &prepared_start)
          .and_then(|_| shared_recording_elapsed(&app)),
      )
    }));
  }

  fn on_destroy(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND) {
    stop_overlay_timer(hwnd, RECORD_TIMER_ID);
    stop_overlay_timer(hwnd, MAX_DURATION_TIMER_ID);
    GIF_OVERLAY_HWND.store(0, Ordering::SeqCst);

    if window.mode.startup_dispatched && window.mode.response_tx.is_some() {
      let app_handle = window.mode.config.app_handle.clone();
      let presentation_mode = window.mode.config.presentation_mode;
      let response_tx = window.mode.response_tx.take();

      let completion_rx = window.mode.completion_rx.take();
      let fallback_tx = response_tx.clone();
      let cleanup_app = app_handle.clone();
      let control_failed = window.mode.control_failed;
      crate::app::runtime::spawn(async move {
        if let Err(error) = tokio::task::spawn_blocking(move || {
          if let Some(rx) = completion_rx
            && let Err(error) = rx.recv_timeout(Duration::from_secs(45))
          {
            tracing::error!(%error, "GIF cleanup worker acknowledgment failed or timed out");
          }
          match crate::commands::video::stop::stop_video_recorder_blocking(&app_handle) {
            Ok(Some(output_path)) => {
              if let Err(error) = std::fs::remove_file(&output_path) {
                tracing::warn!(
                  operation = "gif_overlay_destroy",
                  path = %output_path.display(),
                  error = %error,
                  "Failed to remove cancelled GIF artifact during overlay teardown"
                );
              }
            },
            Ok(None) => {},
            Err(error) => {
              tracing::warn!(
                operation = "gif_overlay_destroy",
                error = %error,
                "Best-effort GIF recorder cleanup failed during overlay teardown"
              );
            },
          }
          update_gif_session_state(&app_handle, None, true, presentation_mode);
          if control_failed {
            show_recording_failure(&app_handle);
          }
          crate::screenshot::release_capture_idle_memory();
          crate::overlay::channel::send_overlay_result("GIF overlay cleanup", response_tx, None);
        })
        .await
        {
          tracing::error!(%error, "GIF cancellation cleanup worker failed");
          update_gif_session_state(&cleanup_app, None, true, presentation_mode);
          if control_failed {
            show_recording_failure(&cleanup_app);
          }
          crate::overlay::channel::send_overlay_result("GIF overlay cleanup", fallback_tx, None);
        }
      });
      return;
    }

    update_gif_session_state(
      &window.mode.config.app_handle,
      None,
      true,
      window.mode.config.presentation_mode,
    );

    crate::overlay::channel::send_overlay_result(
      "GIF overlay",
      window.mode.response_tx.take(),
      None,
    );

    crate::screenshot::release_capture_idle_memory();
  }

  fn on_timer(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND, timer_id: usize) {
    match timer_id {
      RECORD_TIMER_ID => {
        poll_recording_completion(window, hwnd);
        // Polling may destroy the window; do not use the state afterwards.
      },
      MAX_DURATION_TIMER_ID => {
        stop_overlay_timer(hwnd, MAX_DURATION_TIMER_ID);
        stop_recording_and_close(window, hwnd);
      },
      _ => {},
    }
  }

  fn on_click(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND, client_x: i32, client_y: i32) {
    match Self::hover_target(window, client_x, client_y) {
      HoverTarget::Stop => stop_recording_and_close(window, hwnd),
      HoverTarget::Pause => set_gif_paused_state(window, hwnd, !window.shell.is_paused()),
      // GIFs record no audio, so their controls never show a microphone.
      HoverTarget::None | HoverTarget::Drag | HoverTarget::Microphone => {},
    }
  }

  fn hover_target(
    window: &RecordingOverlayWindow<Self>,
    client_x: i32,
    client_y: i32,
  ) -> HoverTarget {
    let layout = toolbar_layout(
      window.shell.metrics,
      window.mode.toolbar_left,
      window.mode.toolbar_top,
    );
    toolbar_hover(client_x, client_y, layout)
  }

  fn hit_test(window: &RecordingOverlayWindow<Self>, screen_x: i32, screen_y: i32) -> LRESULT {
    let client_x = screen_x - window.mode.vd_offset_x;
    let client_y = screen_y - window.mode.vd_offset_y;
    if Self::hover_target(window, client_x, client_y) != HoverTarget::None {
      LRESULT(1)
    } else {
      LRESULT(-1)
    }
  }

  fn redraw(window: &mut RecordingOverlayWindow<Self>) {
    render_gif_with(window, present_overlay_bitmap);
  }

  fn on_keydown(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND, vk: u32) {
    match vk {
      crate::overlay::selection_core::VK_ESCAPE => destroy_overlay_window(hwnd),
      crate::overlay::selection_core::VK_RETURN => stop_recording_and_close(window, hwnd),
      crate::overlay::selection_core::VK_SPACE => {
        set_gif_paused_state(window, hwnd, !window.shell.is_paused())
      },
      _ => {},
    }
  }

  fn on_message(
    window: &mut RecordingOverlayWindow<Self>,
    hwnd: HWND,
    msg: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
  ) -> Option<LRESULT> {
    match msg {
      WM_APP_STOP => {
        stop_recording_and_close(window, hwnd);
        Some(LRESULT(0))
      },
      WM_APP_PAUSE => {
        set_gif_paused_state(window, hwnd, true);
        Some(LRESULT(0))
      },
      WM_APP_RESUME => {
        set_gif_paused_state(window, hwnd, false);
        Some(LRESULT(0))
      },
      _ => None,
    }
  }
  fn toolbar_position(&self) -> (i32, i32) {
    (self.toolbar_left, self.toolbar_top)
  }
  fn set_toolbar_position(&mut self, x: i32, y: i32) {
    self.toolbar_left = x;
    self.toolbar_top = y;
  }
  fn viewport(&self) -> (i32, i32, i32, i32) {
    (self.vd_offset_x, self.vd_offset_y, self.width, self.height)
  }
  fn on_display_change(window: &mut RecordingOverlayWindow<Self>, hwnd: HWND) {
    tracing::warn!("Display topology changed; stopping GIF with obsolete capture bounds");
    stop_recording_and_close(window, hwnd);
  }
}

fn render_gif_with(
  window: &mut RecordingOverlayWindow<GifRecordingMode>,
  present: impl FnOnce(&LayeredBitmap, HWND, i32, i32) -> anyhow::Result<()>,
) {
  let style = TimerTextStyle::Gif {
    limit_secs: window.mode.config.max_duration_secs,
  };
  if let Err(error) = fit_recording_bar(window, style) {
    tracing::error!(%error, "Cannot measure GIF recording controls");
    return;
  }
  if !window.shell.needs_paint(window.mode.toolbar_position()) {
    return;
  }
  let measured_start = window.shell.diagnostics.as_ref().map(|_| Instant::now());
  let Some(bitmap) = window.mode.recording_bitmap.as_mut() else {
    return;
  };

  bitmap.clear();

  let selection_left = window.mode.selection_rect.x as i32;
  let selection_top = window.mode.selection_rect.y as i32;
  let selection_right = (window.mode.selection_rect.x + window.mode.selection_rect.w) as i32;
  let selection_bottom = (window.mode.selection_rect.y + window.mode.selection_rect.h) as i32;
  draw_selection_border(
    bitmap,
    selection_left,
    selection_top,
    selection_right,
    selection_bottom,
    BORDER_T,
  );

  let layout = toolbar_layout(
    window.shell.metrics,
    window.mode.toolbar_left,
    window.mode.toolbar_top,
  );

  let metrics = window.shell.metrics;
  if let Err(error) = draw_recording_toolbar(
    bitmap,
    &mut window.shell,
    layout,
    metrics,
    TimerTextStyle::Gif {
      limit_secs: window.mode.config.max_duration_secs,
    },
    Instant::now(),
  ) {
    tracing::error!(%error, "GIF toolbar rendering failed");
    return;
  }

  if let Err(error) = present(
    bitmap,
    window.shell.hwnd,
    window.mode.vd_offset_x,
    window.mode.vd_offset_y,
  ) {
    tracing::warn!(operation = "gif_recording_hud", error = %error, "apply_to failed");
  }
  if let Some(started) = measured_start
    && let Some(diagnostics) = &mut window.shell.diagnostics
  {
    diagnostics.record(started);
  }
}
fn set_gif_paused_state(
  window: &mut RecordingOverlayWindow<GifRecordingMode>,
  _hwnd: HWND,
  pause: bool,
) {
  if window.shell.is_paused() == pause
    || !window.mode.recording_started
    || window.mode.completion_rx.is_some()
  {
    return;
  }
  window.shell.phase = if pause {
    RecordingPhase::Pausing
  } else {
    RecordingPhase::Resuming
  };
  GifRecordingMode::redraw(window);
  let app_handle = window.mode.config.app_handle.clone();
  window.mode.operation_started = Some(Instant::now());
  window.mode.completion_rx = Some(dispatch_recording_operation(move || {
    RecordingCompletion::Paused(
      set_shared_gif_paused_state(&app_handle, pause)
        .and_then(|_| shared_recording_elapsed(&app_handle).map(|elapsed| (pause, elapsed))),
    )
  }));
}

fn shared_recording_elapsed(app: &crate::app::context::AppHandle) -> Result<Duration, String> {
  let state = app.state::<crate::commands::VideoRecorderState>();
  let mut recorder = state
    .0
    .lock()
    .map_err(|error| crate::commands::ipc_display_context("GIF recorder lock poisoned", error))?;
  Ok(recorder.get_elapsed_time())
}

fn dispatch_recording_operation<F>(operation: F) -> Receiver<RecordingCompletion>
where
  F: FnOnce() -> RecordingCompletion + Send + 'static,
{
  let (tx, rx) = std::sync::mpsc::sync_channel(1);
  crate::app::runtime::spawn(async move {
    let completion = match tokio::task::spawn_blocking(operation).await {
      Ok(completion) => completion,
      Err(error) => RecordingCompletion::WorkerFailed(crate::commands::ipc_context(
        "GIF recording worker failed",
        error,
      )),
    };
    if let Err(error) = tx.try_send(completion) {
      tracing::error!(%error, "GIF worker acknowledgment disconnected");
    }
  });
  rx
}

fn poll_recording_completion(window: &mut RecordingOverlayWindow<GifRecordingMode>, hwnd: HWND) {
  let result = receive_recording_completion(
    &mut window.mode.completion_rx,
    &mut window.mode.operation_started,
    Instant::now(),
  );
  match result {
    Ok(Some(completion)) => {
      match completion {
        RecordingCompletion::Started(Ok(elapsed)) => {
          window.mode.recording_started = true;
          window
            .shell
            .acknowledge_pause(false, elapsed, Instant::now());
          update_gif_session_state(
            &window.mode.config.app_handle,
            Some(false),
            false,
            window.mode.config.presentation_mode,
          );
          if !start_overlay_timer(
            hwnd,
            MAX_DURATION_TIMER_ID,
            remaining_gif_ms(window.mode.config.max_duration_secs, elapsed),
          ) {
            stop_recording_and_close(window, hwnd);
            return;
          }
        },
        RecordingCompletion::Started(Err(error)) | RecordingCompletion::WorkerFailed(error) => {
          tracing::error!(%error, "GIF recording startup failed");
          show_recording_failure(&window.mode.config.app_handle);
          destroy_overlay_window(hwnd);
          return;
        },
        RecordingCompletion::Paused(Ok((paused, elapsed))) => {
          window
            .shell
            .acknowledge_pause(paused, elapsed, Instant::now());
          update_gif_session_state(
            &window.mode.config.app_handle,
            Some(paused),
            false,
            window.mode.config.presentation_mode,
          );
          if paused {
            stop_overlay_timer(hwnd, MAX_DURATION_TIMER_ID);
          } else {
            let remaining = remaining_gif_ms(
              window.mode.config.max_duration_secs,
              window.shell.elapsed(Instant::now()),
            );
            if !start_overlay_timer(hwnd, MAX_DURATION_TIMER_ID, remaining) {
              stop_recording_and_close(window, hwnd);
              return;
            }
          }
        },
        RecordingCompletion::Paused(Err(error)) => {
          tracing::error!(%error, "GIF pause/resume failed");
          window.mode.control_failed = true;
          destroy_overlay_window(hwnd);
          return;
        },
      }
      if window.mode.stop_after_operation {
        stop_recording_and_close(window, hwnd);
        return;
      }
    },
    Err(error) => {
      tracing::error!(%error, "GIF recording acknowledgment failed");
      show_recording_failure(&window.mode.config.app_handle);
      destroy_overlay_window(hwnd);
      return;
    },
    _ => {},
  }
  if !window.mode.stop_after_operation {
    GifRecordingMode::redraw(window);
  }
}

/// Poll once on the overlay owner thread. A queued acknowledgment takes priority
/// over its deadline; only a received result releases the single operation slot.
fn receive_recording_completion(
  receiver: &mut Option<Receiver<RecordingCompletion>>,
  started: &mut Option<Instant>,
  now: Instant,
) -> Result<Option<RecordingCompletion>, &'static str> {
  match receiver.as_ref().map(Receiver::try_recv) {
    Some(Ok(completion)) => {
      *receiver = None;
      *started = None;
      Ok(Some(completion))
    },
    Some(Err(TryRecvError::Disconnected)) => {
      Err("GIF recording worker disconnected before acknowledgment")
    },
    Some(Err(TryRecvError::Empty))
      if started.is_some_and(|started| {
        now.saturating_duration_since(started) > Duration::from_secs(45)
      }) =>
    {
      Err("GIF recording operation exceeded its acknowledgment deadline")
    },
    _ => Ok(None),
  }
}

fn remaining_gif_ms(limit_secs: u32, elapsed: Duration) -> u32 {
  (u128::from(limit_secs) * 1000)
    .saturating_sub(elapsed.as_millis())
    .clamp(1, u128::from(u32::MAX)) as u32
}

fn show_recording_failure(app: &crate::app::context::AppHandle) {
  let lang = crate::i18n::get_lang(app);
  match crate::i18n::native_settings(&lang, "recording_gif_failed") {
    Ok(title) => crate::notifications::show_error(
      app,
      &title,
      crate::commands::video::stop::recording_failure_body(app),
    ),
    Err(error) => tracing::error!(%error, "GIF failure notification localization failed"),
  }
}

fn stop_recording_and_close(window: &mut RecordingOverlayWindow<GifRecordingMode>, hwnd: HWND) {
  if window.mode.completion_rx.is_some() {
    window.mode.stop_after_operation = true;
    stop_overlay_timer(hwnd, MAX_DURATION_TIMER_ID);
    show_overlay_window(hwnd, SW_HIDE);
    return;
  }
  stop_overlay_timer(hwnd, RECORD_TIMER_ID);
  stop_overlay_timer(hwnd, MAX_DURATION_TIMER_ID);

  if !window.mode.recording_started {
    destroy_overlay_window(hwnd);
    return;
  }
  window.mode.recording_started = false;

  let response_tx = window.mode.response_tx.take();
  let selection_rect = window.mode.selection_rect;
  let output_width = window.mode.output_width;
  let output_height = window.mode.output_height;
  let duration_seconds = window.shell.elapsed(Instant::now()).as_secs();
  let app_handle = window.mode.config.app_handle.clone();

  if let Some(tx) = response_tx {
    let fallback_tx = tx.clone();
    let processing_notice = crate::notifications::recording_processing(&app_handle, true);
    let error_app = app_handle.clone();
    crate::app::runtime::spawn(async move {
      if let Err(error) = tokio::task::spawn_blocking(move || {
        encode_gif_background(
          app_handle,
          selection_rect,
          output_width,
          output_height,
          duration_seconds,
          tx,
          processing_notice,
        );
      })
      .await
      {
        tracing::error!(%error, "GIF finalization worker failed");
        show_recording_failure(&error_app);
        crate::overlay::channel::send_overlay_result("GIF encode", Some(fallback_tx), None);
      }
    });
  }

  destroy_overlay_window(hwnd);
}

fn selection_to_rect(selection: Selection) -> Rect {
  Rect {
    x: selection.x as f64,
    y: selection.y as f64,
    w: selection.width as f64,
    h: selection.height as f64,
  }
}

fn encode_gif_background(
  app: crate::app::context::AppHandle,
  selection_rect: Rect,
  output_width: u32,
  output_height: u32,
  duration_seconds: u64,
  tx: std::sync::mpsc::Sender<Option<GifResult>>,
  processing_notice: Option<crate::notifications::ProcessingNotice>,
) {
  let stop_started = Instant::now();
  let stopped = stop_shared_gif_recording_blocking(&app);
  drop(processing_notice);
  let result = match stopped {
    Ok(output_path) => {
      crate::commands::log_speed_snapshot("gif.capture.stop_controller", stop_started.elapsed());
      crate::screenshot::release_capture_idle_memory();
      crate::commands::log_speed_snapshot("gif.encode.finalize", stop_started.elapsed());
      tracing::info!(
        operation = "gif_encode_bg",
        path = %output_path.display(),
        "GIF encoding complete"
      );
      Some(GifResult {
        path: output_path,
        width: output_width.max(selection_rect.w as u32),
        height: output_height.max(selection_rect.h as u32),
        duration_seconds,
      })
    },
    Err(error) => {
      crate::commands::log_speed_snapshot("gif.capture.stop_controller", stop_started.elapsed());
      tracing::error!(
        operation = "gif_encode_bg",
        error = %error,
        error_chain = %format!("{error:#}"),
        "Failed to stop GIF capture"
      );
      show_recording_failure(&app);
      None
    },
  };

  crate::overlay::channel::send_overlay_result("GIF encode", Some(tx), result);
}

fn set_shared_gif_paused_state(
  app: &crate::app::context::AppHandle,
  pause: bool,
) -> Result<(), String> {
  let recorder_state = app.state::<crate::commands::VideoRecorderState>();
  let mut recorder = recorder_state
    .inner()
    .0
    .lock()
    .map_err(|e| crate::commands::ipc_display_context("Video recorder state poisoned", e))?;

  let result = if pause {
    recorder.pause_recording()
  } else {
    recorder.resume_recording()
  };
  result.map_err(|e| crate::commands::ipc_context("Failed to update GIF recording pause state", e))
}

fn stop_shared_gif_recording_blocking(
  app: &crate::app::context::AppHandle,
) -> Result<std::path::PathBuf, String> {
  crate::commands::video::stop::stop_video_recorder_blocking(app)?
    .ok_or_else(|| "GIF recorder was already idle during stop".to_string())
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod recording_tests {
  use super::*;

  /// The production start plan saves in the configured GIF folder, sized to the selection.
  #[test]
  fn the_production_gif_plan_saves_in_the_configured_folder() {
    let folder = tempfile::tempdir().expect("folder");
    let app = crate::app::context::AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        custom_screenshots_folder: Some(folder.path().to_string_lossy().into_owned()),
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let config = GifRecordingConfig {
      fps: 15,
      max_duration_secs: 10,
      output_dir: folder.path().into(),
      show_cursor: false,
      filename_template: "fixture".into(),
      app_handle: app,
      presentation_mode: crate::recording_visibility::RecordingPresentationMode::Silent,
    };
    let (tx, _rx) = std::sync::mpsc::channel();
    let mode = build_recording_mode(
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
      Instant::now(),
    )
    .expect("GIF mode");
    let prepared = mode.prepared_start.as_ref().expect("prepared start");
    assert!(prepared.output_path.starts_with(folder.path()));
    assert_eq!(
      prepared.output_path.extension().and_then(|e| e.to_str()),
      Some("gif")
    );
    assert_eq!((mode.output_width, mode.output_height), (320, 200));
    let colors = <GifSelectionAdapter as SelectionOverlayAdapter>::colors();
    assert_ne!(colors.border, colors.preview);
  }

  #[test]
  fn gif_areas_require_the_recording_minimum() {
    assert_eq!(
      <GifSelectionAdapter as SelectionOverlayAdapter>::MINIMUM_SELECTION_SIZE,
      crate::overlay::selection::RECORDING_MIN_SELECTION_SIZE
    );
  }
  #[test]
  fn gif_deadline_uses_recorded_time_and_saturates_expired_limits() {
    assert_eq!(remaining_gif_ms(15, Duration::from_millis(1200)), 13800);
    assert_eq!(remaining_gif_ms(15, Duration::from_secs(20)), 1);
    assert_eq!(remaining_gif_ms(0, Duration::ZERO), 1);
    assert_eq!(remaining_gif_ms(u32::MAX, Duration::ZERO), u32::MAX);
  }

  #[test]
  fn pending_worker_poll_is_nonblocking_and_deadline_is_bounded() {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let start = Instant::now();
    let mut started = Some(start);
    let mut receiver = Some(rx);
    for elapsed in [Duration::ZERO, Duration::from_secs(45)] {
      assert!(matches!(
        receive_recording_completion(&mut receiver, &mut started, start + elapsed),
        Ok(None)
      ));
      assert!(
        receiver.is_some() && started.is_some(),
        "pending operation retains its slot"
      );
    }
    let error = receive_recording_completion(
      &mut receiver,
      &mut started,
      start + Duration::from_millis(45001),
    )
    .err()
    .unwrap();
    assert!(error.contains("deadline"));
    assert!(
      receiver.is_some(),
      "teardown still owns the worker acknowledgment"
    );
    drop(tx);
    assert!(
      receive_recording_completion(&mut receiver, &mut started, start)
        .err()
        .unwrap()
        .contains("disconnected")
    );
  }

  #[test]
  fn queued_worker_result_wins_over_deadline_and_releases_operation_slot_once() {
    let start = Instant::now();
    for completion in [
      RecordingCompletion::Started(Ok(Duration::from_secs(2))),
      RecordingCompletion::Paused(Ok((true, Duration::from_secs(3)))),
      RecordingCompletion::Paused(Err("pause rejected".into())),
      RecordingCompletion::WorkerFailed("worker failed".into()),
    ] {
      let (tx, rx) = std::sync::mpsc::sync_channel(1);
      let expected = completion.clone();
      assert!(tx.send(completion).is_ok());
      drop(tx);
      let mut receiver = Some(rx);
      let mut started = Some(start);
      let completed =
        receive_recording_completion(&mut receiver, &mut started, start + Duration::from_secs(46));
      assert_eq!(
        completed,
        Ok(Some(expected)),
        "acknowledgments must retain the original result instead of timeout/disconnect errors"
      );
      assert!(receiver.is_none() && started.is_none());
      assert!(matches!(
        receive_recording_completion(&mut receiver, &mut started, start),
        Ok(None)
      ));
    }
  }

  #[test]
  fn idle_gif_controls_report_failure_without_starting_capture_or_returning_output() {
    let directory = tempfile::tempdir().expect("isolated services");
    let app = crate::app::context::AppHandle::isolated(
      crate::config::Config::default(),
      directory.path().into(),
    );
    assert_eq!(
      shared_recording_elapsed(&app).expect("idle elapsed"),
      Duration::ZERO
    );
    for pause in [true, false] {
      let error =
        set_shared_gif_paused_state(&app, pause).expect_err("idle pause/resume must fail");
      assert!(error.contains("Failed to update GIF recording pause state"));
    }
    assert_eq!(
      stop_shared_gif_recording_blocking(&app).unwrap_err(),
      "GIF recorder was already idle during stop"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
  }
}
