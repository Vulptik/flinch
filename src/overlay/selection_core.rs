use std::sync::Arc;
use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::HGDIOBJ;
use windows::Win32::Graphics::Gdi::{
  BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleBitmap, CreateCompatibleDC,
  CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DeleteDC, DeleteObject, FW_SEMIBOLD, GetDC, HBITMAP,
  HDC, HFONT, InvalidateRect, OUT_DEFAULT_PRECIS, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
  GetSystemMetrics, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNOACTIVATE, SetCursor,
  SetForegroundWindow, ShowWindow,
};

use crate::overlay::core::window_detection::{
  DetectedWindow, clamp_to_monitor_area, detect_window_in_snapshot_at_overlay_point,
  snapshot_detectable_windows,
};
use crate::overlay::gdi::bitmap::ScreenshotBitmap;
use crate::overlay::gdi::drawing::{
  draw_cutout, draw_dim_layer, draw_selection_center_guides, draw_selection_label,
};
use crate::overlay::gdi::window::{
  class_name_for_thread, create_selection_window, disable_window_animations, exclude_from_capture,
  register_overlay_class, unregister_overlay_class,
};
use crate::overlay::render_state::SnapAnimationState;
use crate::overlay::selection::cursor_for_hit;
use crate::overlay::selection::{
  DRAG_THRESHOLD_PX, HitResult, Modifiers, NudgeDirection, OverlayState, Rect, Selection,
  SelectionInteraction, distance, finalize_rect_at_least, hit_test, nudge, start_move,
  start_resize, update_move, update_resize,
};

pub const SNAP_ANIM_TIMER_ID: usize = 1;

pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_RETURN: u32 = 0x0D;
pub const VK_BACK: u32 = 0x08;
pub const VK_SHIFT: u32 = 0x10;
pub const VK_MENU: u32 = 0x12;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_SPACE: u32 = 0x20;
pub const VK_ALT: u32 = VK_MENU;

unsafe extern "system" {
  fn SetCapture(hwnd: HWND) -> HWND;
  fn ReleaseCapture() -> i32;
}

pub struct SelectionWindowResources {
  pub(crate) border_feedback: crate::overlay::selection_border::Feedback,
  border_chrome: crate::overlay::chrome::Renderer,
  pub(crate) handle_motion: crate::overlay::motion::Motion,
  pub(crate) indicator_motion: crate::overlay::motion::Motion,
  pub(crate) background: crate::overlay::background::Background,
  pub offscreen_dc: HDC,
  pub offscreen_bmp: HBITMAP,
  pub old_offscreen_bmp: HGDIOBJ,
  pub screenshot: Option<ScreenshotBitmap>,
  pub hfont: HFONT,
  chrome: crate::overlay::chrome::Renderer,
  pub rgba_data: Option<Arc<Vec<u8>>>,
  pub rgba_width: u32,
  pub rgba_height: u32,
  pub launch_started_at: Instant,
  pub first_paint_logged: bool,
  pub cursor_clipped: bool,
}

impl SelectionWindowResources {
  pub fn new(width: u32, height: u32, rgba: Arc<Vec<u8>>, launch_started_at: Instant) -> Self {
    Self {
      border_feedback: crate::overlay::selection_border::Feedback::new(),
      border_chrome: crate::overlay::chrome::Renderer::default(),
      handle_motion: crate::overlay::motion::Motion::new(),
      indicator_motion: crate::overlay::motion::Motion::new(),
      background: crate::overlay::background::Background::default(),
      offscreen_dc: HDC::default(),
      offscreen_bmp: HBITMAP::default(),
      old_offscreen_bmp: HGDIOBJ::default(),
      screenshot: None,
      hfont: HFONT::default(),
      chrome: crate::overlay::chrome::Renderer::default(),
      rgba_data: Some(rgba),
      rgba_width: width,
      rgba_height: height,
      launch_started_at,
      first_paint_logged: false,
      cursor_clipped: false,
    }
  }
}

pub struct SelectionControllerState {
  pub overlay_state: OverlayState,
  pub interaction: SelectionInteraction,
  pub cursor_pos: (f64, f64),
  pub modifiers: Modifiers,
  pub snap_anim: SnapAnimationState,
  pub width: i32,
  pub height: i32,
  pub vd_offset_x: i32,
  pub vd_offset_y: i32,
  pub overlay_hwnd: Option<isize>,
  pub window_snapshot: Vec<DetectedWindow>,
  /// Smallest width and height a selection may confirm with; recordings need more than screenshots.
  pub minimum_size: f64,
  /// Keep the selection on release so the editor opens, as holding Alt does.
  pub hold_on_release: bool,
  /// Set when the last gesture ended below the minimum size; the host takes and clears it.
  pub rejected_too_small: bool,
  /// Muted hint under the dimensions while dragging, teaching Alt-to-edit. Only the screenshot
  /// editor sets it, and never for Capture and Edit, which already opens the editor.
  pub edit_hint: Option<&'static str>,
}

impl SelectionControllerState {
  pub fn new(width: u32, height: u32, vd_offset_x: i32, vd_offset_y: i32) -> Self {
    let fullscreen_rect = Rect {
      x: 0.0,
      y: 0.0,
      w: width as f64,
      h: height as f64,
    };

    Self {
      overlay_state: OverlayState::new_idle(),
      interaction: SelectionInteraction::Idle {
        snapped_window: Some(fullscreen_rect),
      },
      cursor_pos: (width as f64 / 2.0, height as f64 / 2.0),
      modifiers: Modifiers::default(),
      snap_anim: SnapAnimationState::new(),
      width: width as i32,
      height: height as i32,
      vd_offset_x,
      vd_offset_y,
      overlay_hwnd: None,
      window_snapshot: Vec::new(),
      minimum_size: crate::overlay::selection::MIN_SELECTION_SIZE,
      hold_on_release: false,
      rejected_too_small: false,
      edit_hint: None,
    }
  }

  /// The edit hint, from opening until a click or release would finish the selection: snapping
  /// a window or the screen and dragging an area all accept Alt.
  pub fn visible_edit_hint(&self) -> Option<&'static str> {
    self
      .edit_hint
      .filter(|_| !self.hold_on_release)
      .filter(|_| !matches!(self.interaction, SelectionInteraction::Confirmed { .. }))
  }
}

pub struct SharedSelectionOverlayState {
  pub resources: SelectionWindowResources,
  pub controller: SelectionControllerState,
}

impl SharedSelectionOverlayState {
  pub fn new(
    width: u32,
    height: u32,
    rgba: Arc<Vec<u8>>,
    vd_offset_x: i32,
    vd_offset_y: i32,
    launch_started_at: Instant,
  ) -> Self {
    Self {
      resources: SelectionWindowResources::new(width, height, rgba, launch_started_at),
      controller: SelectionControllerState::new(width, height, vd_offset_x, vd_offset_y),
    }
  }
}

pub enum SelectionMouseUpOutcome {
  None,
  Confirm(Selection),
}

pub enum SelectionKeyOutcome {
  None,
  Confirm(Selection),
  CancelOverlay,
}

pub struct SelectionDrawColors {
  pub border: COLORREF,
  pub preview: COLORREF,
}

impl SelectionDrawColors {
  pub const DEFAULT: Self = Self {
    border: COLORREF(0x002F2FD9),
    preview: COLORREF(0x005555FF),
  };
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_selection_overlay_thread<T>(
  thread_name: &str,
  window_title: &'static str,
  width: u32,
  height: u32,
  state: Box<T>,
  wnd_proc: unsafe extern "system" fn(
    windows::Win32::Foundation::HWND,
    u32,
    windows::Win32::Foundation::WPARAM,
    windows::Win32::Foundation::LPARAM,
  ) -> windows::Win32::Foundation::LRESULT,
  run_loop: fn(HWND),
  on_registration_error: fn(Box<T>),
  on_creation_error: fn(Box<T>),
) -> anyhow::Result<()>
where
  T: 'static,
{
  let state_addr = Box::into_raw(state) as usize;

  std::thread::Builder::new()
    .name(thread_name.to_string())
    .spawn(move || {
      let state_ptr = state_addr as *mut T;
      let class_name = class_name_for_thread();

      if let Err(e) = register_overlay_class(&class_name, Some(wnd_proc)) {
        tracing::error!(error = %e, "Failed to register overlay class");
        on_registration_error(unsafe { Box::from_raw(state_ptr) });
        return;
      }

      let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
      let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };

      match create_selection_window(
        &class_name,
        window_title,
        x,
        y,
        width as i32,
        height as i32,
        state_ptr as *mut std::ffi::c_void,
      ) {
        Ok(hwnd) => run_loop(hwnd),
        Err(e) => {
          tracing::error!(error = %e, "Failed to create selection window");
          on_creation_error(unsafe { Box::from_raw(state_ptr) });
        },
      }

      unregister_overlay_class(&class_name);
    })
    .map(|_| ())
    .map_err(|e| {
      // SAFETY: spawn failure means the closure never ran; creator still owns this allocation.
      on_creation_error(unsafe { Box::from_raw(state_addr as *mut T) });
      anyhow::anyhow!("Failed to spawn selection overlay thread: {}", e)
    })
}

pub fn initialize_selection_window(
  shared: &mut SharedSelectionOverlayState,
  hwnd: HWND,
  metrics_prefix: &str,
) -> anyhow::Result<()> {
  initialize_selection_window_with(shared, hwnd, metrics_prefix, |hwnd| {
    disable_window_animations(hwnd);
    exclude_from_capture(hwnd);
    // SAFETY: presentation runs on this live selection window's owner thread.
    unsafe {
      let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
      let _ = SetForegroundWindow(hwnd);
    }
    crate::overlay::gdi::window::clip_cursor_to(hwnd)
  })
}

fn initialize_selection_window_with(
  shared: &mut SharedSelectionOverlayState,
  hwnd: HWND,
  metrics_prefix: &str,
  present: impl FnOnce(HWND) -> bool,
) -> anyhow::Result<()> {
  shared.controller.overlay_hwnd = Some(hwnd.0 as isize);
  shared.controller.window_snapshot = snapshot_detectable_windows(shared.controller.overlay_hwnd);

  let create_started = Instant::now();
  crate::commands::log_speed_snapshot(
    &format!("{metrics_prefix}.window_created"),
    shared.resources.launch_started_at.elapsed(),
  );

  let rgba_bytes = shared.resources.rgba_width as usize * shared.resources.rgba_height as usize * 4;
  let offscreen_bytes =
    shared.controller.width.max(0) as usize * shared.controller.height.max(0) as usize * 4;
  tracing::info!(
    operation = "selection_overlay_memory",
    metrics_prefix,
    stage = "create_begin",
    rgba_bytes,
    screenshot_width = shared.resources.rgba_width,
    screenshot_height = shared.resources.rgba_height,
    offscreen_bytes,
    offscreen_width = shared.controller.width,
    offscreen_height = shared.controller.height,
    "Selection overlay create begin"
  );

  if let Some(rgba) = shared.resources.rgba_data.take() {
    let bitmap_started = Instant::now();
    let bitmap = ScreenshotBitmap::from_rgba(
      &rgba,
      shared.resources.rgba_width,
      shared.resources.rgba_height,
    )
    .map_err(|e| anyhow::anyhow!("Failed to create ScreenshotBitmap: {e}"))?;
    shared.resources.screenshot = Some(bitmap);
    crate::commands::log_speed_snapshot(
      &format!("{metrics_prefix}.create_bitmap"),
      bitmap_started.elapsed(),
    );
  } else {
    anyhow::bail!("No RGBA data available");
  }

  let offscreen_started = Instant::now();
  let screen_dc = unsafe { GetDC(Some(hwnd)) };
  let offscreen_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
  let offscreen_bmp =
    unsafe { CreateCompatibleBitmap(screen_dc, shared.controller.width, shared.controller.height) };
  unsafe { ReleaseDC(Some(hwnd), screen_dc) };

  if offscreen_dc.is_invalid() || offscreen_bmp.is_invalid() {
    if !offscreen_dc.is_invalid() {
      let _ = unsafe { DeleteDC(offscreen_dc) };
    }
    if !offscreen_bmp.is_invalid() {
      let _ = unsafe { DeleteObject(offscreen_bmp.into()) };
    }
    anyhow::bail!("Failed to create offscreen DC/bitmap");
  }

  let old_offscreen_bmp = unsafe { SelectObject(offscreen_dc, offscreen_bmp.into()) };
  shared.resources.offscreen_dc = offscreen_dc;
  shared.resources.offscreen_bmp = offscreen_bmp;
  shared.resources.old_offscreen_bmp = old_offscreen_bmp;
  crate::commands::log_speed_snapshot(
    &format!("{metrics_prefix}.create_offscreen_surface"),
    offscreen_started.elapsed(),
  );

  let font_started = Instant::now();
  let font_name: Vec<u16> = "Segoe UI"
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
  shared.resources.hfont = unsafe {
    CreateFontW(
      15,
      0,
      0,
      0,
      FW_SEMIBOLD.0 as i32,
      0,
      0,
      0,
      DEFAULT_CHARSET,
      OUT_DEFAULT_PRECIS,
      CLIP_DEFAULT_PRECIS,
      CLEARTYPE_QUALITY,
      DEFAULT_PITCH.0 as u32,
      windows::core::PCWSTR(font_name.as_ptr()),
    )
  };
  crate::commands::log_speed_snapshot(
    &format!("{metrics_prefix}.create_font"),
    font_started.elapsed(),
  );

  let show_started = Instant::now();
  shared.resources.cursor_clipped = present(hwnd);
  crate::commands::log_speed_snapshot(
    &format!("{metrics_prefix}.show_window"),
    show_started.elapsed(),
  );
  crate::commands::log_speed_snapshot(
    &format!("{metrics_prefix}.create_total"),
    create_started.elapsed(),
  );

  Ok(())
}

pub fn destroy_selection_window(shared: &mut SharedSelectionOverlayState) {
  // Window destruction removes any legacy HWND timer; the precise clock owns its handle.

  unsafe {
    if !shared.resources.offscreen_dc.is_invalid() {
      SelectObject(
        shared.resources.offscreen_dc,
        shared.resources.old_offscreen_bmp,
      );
      if !shared.resources.offscreen_bmp.is_invalid() {
        let _ = DeleteObject(shared.resources.offscreen_bmp.into());
      }
      let _ = DeleteDC(shared.resources.offscreen_dc);
    }
    if !shared.resources.hfont.is_invalid() {
      let _ = DeleteObject(shared.resources.hfont.into());
    }
  }

  if shared.resources.cursor_clipped {
    crate::overlay::gdi::window::release_cursor_clip();
    shared.resources.cursor_clipped = false;
  }
}

pub fn selection_hit_for_cursor(controller: &SelectionControllerState) -> HitResult {
  if let OverlayState::OnHold { ref rect, .. } = controller.overlay_state {
    hit_test(rect, controller.cursor_pos)
  } else {
    HitResult::Outside
  }
}

pub fn should_invalidate_selection_timer(controller: &SelectionControllerState) -> bool {
  controller.snap_anim.is_animating()
    || matches!(
      controller.interaction,
      SelectionInteraction::Idle { .. } | SelectionInteraction::PendingDecision { .. }
    ) && controller.snap_anim.get_current_rect().is_some()
}

pub fn handle_selection_mouse_move(
  controller: &mut SelectionControllerState,
  hwnd: HWND,
  lparam: LPARAM,
) {
  let x = (lparam.0 & 0xFFFF) as i16 as f64;
  let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f64;

  let clamped = clamp_to_monitor_area(x, y, controller.vd_offset_x, controller.vd_offset_y);
  update_selection_pointer(controller, clamped);
  let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
}

fn update_selection_pointer(controller: &mut SelectionControllerState, position: (f64, f64)) {
  controller.cursor_pos = position;
  let pos = controller.cursor_pos;
  let mods = controller.modifiers;
  let ww = controller.width as f64;
  let wh = controller.height as f64;

  match controller.interaction.clone() {
    SelectionInteraction::Idle { .. } => {
      let snapped = resolve_idle_snap_rect(controller, pos).unwrap_or(Rect {
        x: 0.0,
        y: 0.0,
        w: ww,
        h: wh,
      });
      controller.snap_anim.update_target(Some(snapped));
      controller.interaction = SelectionInteraction::Idle {
        snapped_window: Some(snapped),
      };
    },
    SelectionInteraction::PendingDecision {
      press_pos,
      snapped_window,
    } => {
      if distance(pos, press_pos) > DRAG_THRESHOLD_PX {
        controller.interaction = SelectionInteraction::FreeDraw {
          start: press_pos,
          current: pos,
        };
        controller.overlay_state = OverlayState::Drawing {
          start: press_pos,
          current: pos,
        };
      } else {
        controller.interaction = SelectionInteraction::PendingDecision {
          press_pos,
          snapped_window,
        };
      }
    },
    SelectionInteraction::FreeDraw { start, .. } => {
      controller.interaction = SelectionInteraction::FreeDraw {
        start,
        current: pos,
      };
      controller.overlay_state = OverlayState::Drawing {
        start,
        current: pos,
      };
    },
    SelectionInteraction::Confirmed {
      rect,
      from_snap,
      moving: Some(offset),
      resizing: None,
    } => {
      let new_rect = update_move(rect, offset, pos, ww, wh);
      controller.interaction = SelectionInteraction::Confirmed {
        rect: new_rect,
        from_snap,
        moving: Some(offset),
        resizing: None,
      };
      controller.overlay_state = OverlayState::OnHold { rect: new_rect };
    },
    SelectionInteraction::Confirmed {
      rect,
      from_snap,
      moving: None,
      resizing: Some((handle, start_rect)),
    } => {
      let new_rect = update_resize(rect, handle, start_rect, pos, mods, ww, wh);
      controller.interaction = SelectionInteraction::Confirmed {
        rect: new_rect,
        from_snap,
        moving: None,
        resizing: Some((handle, start_rect)),
      };
      controller.overlay_state = OverlayState::OnHold { rect: new_rect };
    },
    SelectionInteraction::Confirmed {
      moving: None,
      resizing: None,
      ..
    } => {},
    _ => {},
  }
}

pub fn handle_selection_lbutton_down(controller: &mut SelectionControllerState, hwnd: HWND) {
  begin_selection_gesture(controller);
  unsafe { SetCapture(hwnd) };
  let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
}

fn begin_selection_gesture(controller: &mut SelectionControllerState) {
  let pos = controller.cursor_pos;

  match controller.interaction.clone() {
    SelectionInteraction::Idle { snapped_window } => {
      controller.interaction = SelectionInteraction::PendingDecision {
        press_pos: pos,
        snapped_window,
      };
    },
    SelectionInteraction::PendingDecision { .. } | SelectionInteraction::FreeDraw { .. } => {},
    SelectionInteraction::Confirmed {
      rect,
      moving: None,
      resizing: None,
      from_snap,
    } => match hit_test(&rect, pos) {
      HitResult::Outside => {
        let snapped = resolve_idle_snap_rect(controller, pos);
        controller.interaction = SelectionInteraction::PendingDecision {
          press_pos: pos,
          snapped_window: snapped,
        };
        controller.overlay_state = OverlayState::new_idle();
      },
      HitResult::Inside => {
        let offset = start_move(&rect, pos);
        controller.interaction = SelectionInteraction::Confirmed {
          rect,
          from_snap,
          moving: Some(offset),
          resizing: None,
        };
        controller.overlay_state = OverlayState::OnHold { rect };
      },
      HitResult::Handle(h) => {
        let (handle, start_rect) = start_resize(rect, h, pos);
        controller.interaction = SelectionInteraction::Confirmed {
          rect,
          from_snap,
          moving: None,
          resizing: Some((handle, start_rect)),
        };
        controller.overlay_state = OverlayState::OnHold { rect };
      },
    },
    SelectionInteraction::Confirmed { .. } => {},
  }
}

fn resolve_idle_snap_rect(
  controller: &mut SelectionControllerState,
  pos: (f64, f64),
) -> Option<Rect> {
  detect_window_in_snapshot_at_overlay_point(
    &controller.window_snapshot,
    pos.0 as i32,
    pos.1 as i32,
    controller.vd_offset_x,
    controller.vd_offset_y,
  )
  .and_then(|window| {
    // DWM frame bounds can extend beyond the capture desktop. Intersect rather
    // than move the rectangle: snapping must retain only the visible portion.
    let bounds = window.bounds;
    let left = bounds.x.max(0.0);
    let top = bounds.y.max(0.0);
    let right = (bounds.x + bounds.w).min(controller.width as f64);
    let bottom = (bounds.y + bounds.h).min(controller.height as f64);
    (right > left && bottom > top).then_some(Rect {
      x: left,
      y: top,
      w: right - left,
      h: bottom - top,
    })
  })
}

pub fn handle_selection_lbutton_up(
  controller: &mut SelectionControllerState,
  hwnd: HWND,
) -> SelectionMouseUpOutcome {
  unsafe { ReleaseCapture() };
  let outcome = finish_selection_gesture(controller);
  let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
  outcome
}

fn finish_selection_gesture(controller: &mut SelectionControllerState) -> SelectionMouseUpOutcome {
  let hold = controller.modifiers.alt || controller.hold_on_release;
  let ww = controller.width as f64;
  let wh = controller.height as f64;

  match controller.interaction.clone() {
    SelectionInteraction::PendingDecision {
      snapped_window: Some(rect),
      ..
    } => {
      if hold {
        controller.interaction = SelectionInteraction::Confirmed {
          rect,
          from_snap: true,
          moving: None,
          resizing: None,
        };
        controller.overlay_state = OverlayState::OnHold { rect };
        SelectionMouseUpOutcome::None
      } else if let Some(sel) = finalize_rect_at_least(rect, ww, wh, controller.minimum_size) {
        SelectionMouseUpOutcome::Confirm(sel)
      } else {
        controller.rejected_too_small = true;
        reset_selection_to_idle(controller);
        SelectionMouseUpOutcome::None
      }
    },
    SelectionInteraction::PendingDecision {
      snapped_window: None,
      ..
    } => {
      reset_selection_to_idle(controller);
      SelectionMouseUpOutcome::None
    },
    SelectionInteraction::FreeDraw { start, current } => {
      let rect = Rect::from_x1y1x2y2(start.0, start.1, current.0, current.1);
      if rect.w >= controller.minimum_size && rect.h >= controller.minimum_size {
        if hold {
          controller.interaction = SelectionInteraction::Confirmed {
            rect,
            from_snap: false,
            moving: None,
            resizing: None,
          };
          controller.overlay_state = OverlayState::OnHold { rect };
          SelectionMouseUpOutcome::None
        } else if let Some(sel) = finalize_rect_at_least(rect, ww, wh, controller.minimum_size) {
          SelectionMouseUpOutcome::Confirm(sel)
        } else {
          controller.rejected_too_small = true;
          reset_selection_to_idle(controller);
          SelectionMouseUpOutcome::None
        }
      } else {
        controller.rejected_too_small = true;
        reset_selection_to_idle(controller);
        SelectionMouseUpOutcome::None
      }
    },
    SelectionInteraction::Confirmed {
      rect,
      from_snap,
      moving: Some(_),
      resizing: None,
    } => {
      controller.interaction = SelectionInteraction::Confirmed {
        rect,
        from_snap,
        moving: None,
        resizing: None,
      };
      controller.overlay_state = OverlayState::OnHold { rect };
      SelectionMouseUpOutcome::None
    },
    SelectionInteraction::Confirmed {
      rect,
      from_snap,
      moving: None,
      resizing: Some(_),
    } => {
      controller.interaction = SelectionInteraction::Confirmed {
        rect,
        from_snap,
        moving: None,
        resizing: None,
      };
      controller.overlay_state = OverlayState::OnHold { rect };
      SelectionMouseUpOutcome::None
    },
    _ => SelectionMouseUpOutcome::None,
  }
}

pub fn handle_selection_keydown(
  controller: &mut SelectionControllerState,
  hwnd: HWND,
  vk: u32,
) -> SelectionKeyOutcome {
  if vk == VK_BACK && !matches!(controller.interaction, SelectionInteraction::Idle { .. }) {
    unsafe {
      SetCursor(Some(cursor_for_hit(HitResult::Outside, true)));
    }
  }
  selection_keydown(controller, vk, || {
    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
  })
}

fn selection_keydown(
  controller: &mut SelectionControllerState,
  vk: u32,
  mut invalidate: impl FnMut(),
) -> SelectionKeyOutcome {
  let ww = controller.width as f64;
  let wh = controller.height as f64;

  if vk == VK_ESCAPE {
    return SelectionKeyOutcome::CancelOverlay;
  }

  if vk == VK_RETURN {
    if let SelectionInteraction::Confirmed {
      rect,
      moving: None,
      resizing: None,
      ..
    } = &controller.interaction
      && let Some(sel) = finalize_rect_at_least(*rect, ww, wh, controller.minimum_size)
    {
      return SelectionKeyOutcome::Confirm(sel);
    }
    return SelectionKeyOutcome::None;
  }

  if vk == VK_BACK {
    if !matches!(controller.interaction, SelectionInteraction::Idle { .. }) {
      reset_selection_to_idle(controller);
      invalidate();
    }
    return SelectionKeyOutcome::None;
  }

  if vk == VK_SHIFT {
    controller.modifiers.shift = true;
    return SelectionKeyOutcome::None;
  }

  if vk == VK_MENU {
    controller.modifiers.alt = true;
    return SelectionKeyOutcome::None;
  }

  let dir = match vk {
    VK_UP => Some(NudgeDirection::Up),
    VK_DOWN => Some(NudgeDirection::Down),
    VK_LEFT => Some(NudgeDirection::Left),
    VK_RIGHT => Some(NudgeDirection::Right),
    _ => None,
  };

  if let Some(d) = dir
    && let SelectionInteraction::Confirmed {
      rect,
      from_snap,
      moving: None,
      resizing: None,
    } = &controller.interaction
  {
    let new_rect = nudge(*rect, d, controller.modifiers.shift, ww, wh);
    controller.interaction = SelectionInteraction::Confirmed {
      rect: new_rect,
      from_snap: *from_snap,
      moving: None,
      resizing: None,
    };
    controller.overlay_state = OverlayState::OnHold { rect: new_rect };
    invalidate();
  }

  SelectionKeyOutcome::None
}

pub fn handle_selection_keyup(controller: &mut SelectionControllerState, vk: u32) {
  if vk == VK_SHIFT {
    controller.modifiers.shift = false;
  }
  if vk == VK_MENU {
    controller.modifiers.alt = false;
  }
}

pub fn reset_selection_to_idle(controller: &mut SelectionControllerState) {
  let fullscreen = Rect {
    x: 0.0,
    y: 0.0,
    w: controller.width as f64,
    h: controller.height as f64,
  };
  controller.interaction = SelectionInteraction::Idle {
    snapped_window: Some(fullscreen),
  };
  controller.overlay_state = OverlayState::new_idle();
  controller.snap_anim.update_target(Some(fullscreen));
}

pub fn draw_selection_scene(
  shared: &mut SharedSelectionOverlayState,
  label_font: HFONT,
  colors: SelectionDrawColors,
) {
  draw_selection_scene_with_indicator(shared, label_font, colors, true);
}

pub(crate) fn draw_selection_scene_with_indicator(
  shared: &mut SharedSelectionOverlayState,
  _label_font: HFONT,
  colors: SelectionDrawColors,
  show_indicator: bool,
) {
  shared
    .resources
    .border_feedback
    .update(&shared.controller, show_indicator, Instant::now());
  shared.resources.handle_motion.update(
    shared.controller.interaction.is_confirmed_idle(),
    0,
    None,
    Instant::now(),
  );
  let shot = match &shared.resources.screenshot {
    Some(s) => s,
    None => return,
  };

  if !shared.resources.background.restore(
    shot.dc,
    shared.resources.offscreen_dc,
    shared.controller.width,
    shared.controller.height,
  ) {
    if let Err(e) = unsafe {
      BitBlt(
        shared.resources.offscreen_dc,
        0,
        0,
        shared.controller.width,
        shared.controller.height,
        Some(shot.dc),
        0,
        0,
        SRCCOPY,
      )
    } {
      tracing::warn!(error = %e, "BitBlt failed in selection overlay (screenshot blit)");
    }

    // SAFETY: live offscreen DC belongs to this overlay thread.
    if let Err(error) = unsafe {
      draw_dim_layer(
        shared.resources.offscreen_dc,
        shared.controller.width,
        shared.controller.height,
        160,
      )
    } {
      tracing::warn!(%error, "Selection dim rendering failed");
    }
  }

  let selection_rect = shared.controller.interaction.get_current_rect(
    shared.controller.width as f64,
    shared.controller.height as f64,
  );
  shared.resources.indicator_motion.update(
    show_indicator && selection_rect.is_some(),
    0,
    None,
    Instant::now(),
  );

  if let Some((x1, y1, x2, y2)) = selection_rect {
    let sel_rect = RECT {
      left: x1 as i32,
      top: y1 as i32,
      right: x2 as i32,
      bottom: y2 as i32,
    };

    unsafe {
      if let Err(error) = draw_cutout(shared.resources.offscreen_dc, shot.dc, sel_rect) {
        tracing::warn!(%error, "Selection cutout failed");
      }
      draw_selection_center_guides(shared.resources.offscreen_dc, sel_rect, colors.border);
      let border_visual = shared.resources.border_feedback.visual;
      let accent = COLORREF(
        u32::from(border_visual.accent[0])
          | (u32::from(border_visual.accent[1]) << 8)
          | (u32::from(border_visual.accent[2]) << 16),
      );
      let chrome = crate::overlay::gdi::drawing::draw_selection_chrome_strength(
        shared.resources.offscreen_dc,
        sel_rect,
        accent,
        true,
        f32::from(border_visual.edge_strength) / 255.0,
      );
      if let Err(error) = chrome {
        tracing::warn!(%error, "Selection chrome failed");
      }
      if show_indicator
        && shared.controller.interaction.is_confirmed_idle()
        && let Err(error) = crate::overlay::selection_border::draw_hover(
          &mut shared.resources.border_chrome,
          shared.resources.offscreen_dc,
          sel_rect,
          border_visual,
        )
      {
        tracing::warn!(%error, "Selection handle hover rendering failed");
      }

      if show_indicator
        && let Err(error) = draw_selection_label(
          shared.resources.offscreen_dc,
          &mut shared.resources.chrome,
          sel_rect,
          shared.controller.width,
          shared.controller.height,
          (shared.controller.vd_offset_x, shared.controller.vd_offset_y),
          shared
            .resources
            .indicator_motion
            .toolbar_alpha(Instant::now()),
          shared
            .resources
            .indicator_motion
            .toolbar_offset(Instant::now()),
        )
      {
        tracing::warn!(%error, "Selection dimensions failed");
      }
    }
  }

  if matches!(
    shared.controller.interaction,
    SelectionInteraction::Idle { .. } | SelectionInteraction::PendingDecision { .. }
  ) && let Some(preview) = shared.controller.snap_anim.get_current_rect()
  {
    let preview_rect = RECT {
      left: preview.x as i32,
      top: preview.y as i32,
      right: (preview.x + preview.w) as i32,
      bottom: (preview.y + preview.h) as i32,
    };
    unsafe {
      // The snap target shows undimmed, exactly like a dragged selection.
      if let Err(error) = draw_cutout(shared.resources.offscreen_dc, shot.dc, preview_rect) {
        tracing::warn!(%error, "Snap preview cutout failed");
      }
      let rgb = shared.resources.border_feedback.visual.accent;
      let accent =
        COLORREF(u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16));
      if let Err(error) = crate::overlay::gdi::drawing::draw_selection_chrome(
        shared.resources.offscreen_dc,
        preview_rect,
        accent,
        true,
      ) {
        tracing::warn!(%error, "Snap preview chrome failed");
      }
    }
  }
}

#[cfg(test)]
mod interaction_tests;

#[cfg(test)]
mod tests {
  use super::*;

  fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { x, y, w, h }
  }

  #[test]
  fn each_gesture_honours_the_overlay_minimum() {
    let recording = crate::overlay::selection::RECORDING_MIN_SELECTION_SIZE;
    let small = rect(100.0, 100.0, 20.0, 20.0);
    let confirmed = |minimum: f64, interaction: SelectionInteraction| {
      let mut controller = SelectionControllerState::new(800, 600, 0, 0);
      controller.minimum_size = minimum;
      controller.interaction = interaction;
      matches!(
        finish_selection_gesture(&mut controller),
        SelectionMouseUpOutcome::Confirm(_)
      )
    };
    let drawn = || SelectionInteraction::FreeDraw {
      start: (100.0, 100.0),
      current: (120.0, 120.0),
    };
    let snapped = || SelectionInteraction::PendingDecision {
      press_pos: (110.0, 110.0),
      snapped_window: Some(small),
    };
    assert!(confirmed(MIN_SELECTION_SIZE_FOR_TESTS, drawn()));
    assert!(!confirmed(recording, drawn()));
    assert!(confirmed(MIN_SELECTION_SIZE_FOR_TESTS, snapped()));
    assert!(!confirmed(recording, snapped()));
    // Enter on a held selection (Alt, then resized below the minimum) confirms nothing.
    let mut controller = SelectionControllerState::new(800, 600, 0, 0);
    controller.minimum_size = recording;
    controller.interaction = SelectionInteraction::Confirmed {
      rect: small,
      from_snap: false,
      moving: None,
      resizing: None,
    };
    assert!(matches!(
      handle_selection_keydown(&mut controller, HWND::default(), VK_RETURN),
      SelectionKeyOutcome::None
    ));
    controller.minimum_size = MIN_SELECTION_SIZE_FOR_TESTS;
    assert!(matches!(
      handle_selection_keydown(&mut controller, HWND::default(), VK_RETURN),
      SelectionKeyOutcome::Confirm(_)
    ));
  }

  const MIN_SELECTION_SIZE_FOR_TESTS: f64 = crate::overlay::selection::MIN_SELECTION_SIZE;

  #[test]
  fn resolve_idle_snap_rect_uses_frozen_window_snapshot() {
    let mut controller = SelectionControllerState::new(1920, 1080, 0, 0);
    controller.window_snapshot = vec![DetectedWindow {
      hwnd: 1,
      bounds: rect(50.0, 50.0, 200.0, 200.0),
      source_app: Some("Code".to_string()),
    }];

    let snapped = resolve_idle_snap_rect(&mut controller, (100.0, 100.0))
      .expect("snapshot should contain a matching window");

    assert_eq!(snapped.x, 50.0);
    assert_eq!(snapped.y, 50.0);
    assert_eq!(snapped.w, 200.0);
    assert_eq!(snapped.h, 200.0);
  }

  #[test]
  fn resolve_idle_snap_rect_returns_none_without_match() {
    let mut controller = SelectionControllerState::new(1920, 1080, 0, 0);
    controller.window_snapshot = vec![DetectedWindow {
      hwnd: 1,
      bounds: rect(50.0, 50.0, 200.0, 200.0),
      source_app: Some("Code".to_string()),
    }];

    assert!(resolve_idle_snap_rect(&mut controller, (10.0, 10.0)).is_none());
  }
  #[test]
  fn feedback_clock_animates_snap_but_direct_manipulation_is_pointer_driven() {
    let mut controller = SelectionControllerState::new(800, 600, 0, 0);
    assert!(!should_invalidate_selection_timer(&controller));
    controller
      .snap_anim
      .update_target(Some(rect(10.0, 10.0, 90.0, 70.0)));
    controller.snap_anim.start_time = Some(Instant::now() - std::time::Duration::from_secs(1));
    assert!(!controller.snap_anim.is_animating());
    assert!(should_invalidate_selection_timer(&controller));
    controller.interaction = SelectionInteraction::FreeDraw {
      start: (10.0, 10.0),
      current: (100.0, 80.0),
    };
    assert!(!should_invalidate_selection_timer(&controller));
    controller.interaction = SelectionInteraction::Confirmed {
      rect: rect(10.0, 10.0, 90.0, 70.0),
      from_snap: false,
      moving: None,
      resizing: None,
    };
    assert!(!should_invalidate_selection_timer(&controller));
    controller.interaction = SelectionInteraction::Confirmed {
      rect: rect(10.0, 10.0, 90.0, 70.0),
      from_snap: false,
      moving: Some((1.0, 1.0)),
      resizing: None,
    };
    assert!(!should_invalidate_selection_timer(&controller));
  }
  #[test]
  fn snap_intersects_each_capture_edge_without_shifting_visible_content() {
    for (window, cursor, expected) in [
      (
        rect(-40.0, 50.0, 200.0, 100.0),
        (10.0, 60.0),
        rect(0.0, 50.0, 160.0, 100.0),
      ),
      (
        rect(700.0, 50.0, 200.0, 100.0),
        (750.0, 60.0),
        rect(700.0, 50.0, 100.0, 100.0),
      ),
      (
        rect(50.0, -40.0, 100.0, 200.0),
        (60.0, 10.0),
        rect(50.0, 0.0, 100.0, 160.0),
      ),
      (
        rect(50.0, 500.0, 100.0, 200.0),
        (60.0, 550.0),
        rect(50.0, 500.0, 100.0, 100.0),
      ),
      (
        rect(-100.0, -100.0, 1000.0, 800.0),
        (60.0, 60.0),
        rect(0.0, 0.0, 800.0, 600.0),
      ),
    ] {
      let mut controller = SelectionControllerState::new(800, 600, 0, 0);
      controller.window_snapshot = vec![DetectedWindow {
        hwnd: 1,
        bounds: window,
        source_app: None,
      }];
      let actual = resolve_idle_snap_rect(&mut controller, cursor).expect("visible window");
      assert_eq!(
        (actual.x, actual.y, actual.w, actual.h),
        (expected.x, expected.y, expected.w, expected.h)
      );
      let selection =
        crate::overlay::selection::finalize_rect(actual, 800.0, 600.0).expect("clipped selection");
      assert_eq!(
        (selection.x, selection.y, selection.width, selection.height),
        (
          expected.x as u32,
          expected.y as u32,
          expected.w as u32,
          expected.h as u32
        )
      );
    }
  }

  #[test]
  fn snap_preserves_cross_monitor_window_with_negative_desktop_origin() {
    let mut controller = SelectionControllerState::new(3840, 1080, -1920, 0);
    controller.window_snapshot = vec![DetectedWindow {
      hwnd: 1,
      bounds: rect(-100.0, 50.0, 300.0, 200.0),
      source_app: None,
    }];
    let actual = resolve_idle_snap_rect(&mut controller, (1870.0, 60.0))
      .expect("window spans monitor boundary");
    assert_eq!(
      (actual.x, actual.y, actual.w, actual.h),
      (1820.0, 50.0, 300.0, 200.0)
    );
  }
}
