use std::sync::Arc;
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
  BeginPaint, BitBlt, EndPaint, InvalidateRect, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::overlay::selection::cursor_for_hit;
use crate::overlay::selection::{OverlayState, Selection};
use crate::overlay::selection_core::{
  SelectionDrawColors, SelectionKeyOutcome, SelectionMouseUpOutcome, SharedSelectionOverlayState,
  destroy_selection_window, draw_selection_scene_with_indicator, handle_selection_keydown,
  handle_selection_keyup, handle_selection_lbutton_down, handle_selection_lbutton_up,
  handle_selection_mouse_move, initialize_selection_window, reset_selection_to_idle,
  selection_hit_for_cursor, should_invalidate_selection_timer, spawn_selection_overlay_thread,
};

/// Per-capture choices for a selection overlay.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectionOptions {
  pub hold_on_release: bool,
  pub magnifier: bool,
  pub too_small: &'static str,
  /// Shown until the selection is chosen, only when the screenshot editor is available; see
  /// `edit_hint_for`.
  pub edit_hint: &'static str,
  /// The Color Picker: the undimmed frozen screen and the picker lens, where a press, Enter or
  /// Space confirms the single pixel under the pointer. Nothing else of a selection applies.
  pub pick: bool,
}

impl SelectionOptions {
  pub fn for_app(app: &crate::app::context::AppHandle) -> Self {
    let language = crate::i18n::get_lang(app);
    Self {
      hold_on_release: false,
      magnifier: crate::config::Config::current(app).selection_magnifier,
      too_small: crate::i18n::selection_too_small(&language),
      edit_hint: crate::i18n::selection_edit_hint(&language),
      pick: false,
    }
  }
}

/// Alt opens the editor only where one exists, and Capture and Edit needs no reminder.
fn edit_hint_for(options: &SelectionOptions, has_editor: bool) -> Option<&'static str> {
  (has_editor && !options.hold_on_release && !options.edit_hint.is_empty())
    .then_some(options.edit_hint)
}

pub trait SelectionOverlayAdapter: 'static {
  type Result: 'static;

  const SUPPORTS_ANNOTATIONS: bool = false;
  /// Smallest width and height a selection confirms with.
  const MINIMUM_SELECTION_SIZE: f64 = crate::overlay::selection::MIN_SELECTION_SIZE;
  const THREAD_NAME: &'static str;
  const WINDOW_TITLE: &'static str;
  const METRICS_PREFIX: &'static str;

  fn colors() -> SelectionDrawColors;

  fn map_selection(state: &SelectionOverlayWindowState<Self>, selection: Selection) -> Self::Result
  where
    Self: Sized;

  fn finalize_result(state: &SelectionOverlayWindowState<Self>, result: &mut Self::Result)
  where
    Self: Sized;

  /// Marks a confirmed result to be pinned instead of saved (F3); `false` when the overlay
  /// cannot pin.
  fn pin(_result: &mut Self::Result) -> bool {
    false
  }
}

pub struct SelectionOverlayWindowState<A: SelectionOverlayAdapter> {
  // Creator owns the allocation until CreateWindowEx succeeds. WM_DESTROY can run on failure.
  owned_by_window: bool,
  pub diagnostics: Option<crate::overlay::performance::Diagnostics>,
  pub shared: SharedSelectionOverlayState,
  pub editor: Option<crate::overlay::screenshot::editor::AnnotationEditor>,
  magnifier: crate::overlay::magnifier::Magnifier,
  hint: crate::overlay::selection_hint::SelectionHint,
  edit_hint: crate::overlay::edit_hint::EditHint,
  pick: bool,
  pub response_tx: Option<std::sync::mpsc::Sender<Option<A::Result>>>,
}

pub fn show_selection_overlay<A: SelectionOverlayAdapter>(
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  options: SelectionOptions,
) -> anyhow::Result<Option<A::Result>> {
  show_selection_overlay_with_editor::<A>(
    width,
    height,
    rgba,
    vd_offset_x,
    vd_offset_y,
    None,
    options,
  )
}

pub fn show_selection_overlay_with_editor<A: SelectionOverlayAdapter>(
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  editor_language: Option<String>,
  options: SelectionOptions,
) -> anyhow::Result<Option<A::Result>> {
  let (response_tx, response_rx) = std::sync::mpsc::channel();

  let mut shared = SharedSelectionOverlayState::new(
    width,
    height,
    rgba.clone(),
    vd_offset_x,
    vd_offset_y,
    Instant::now(),
  );
  shared.controller.minimum_size = A::MINIMUM_SELECTION_SIZE;
  shared.controller.hold_on_release = options.hold_on_release;
  let editor_language = editor_language.filter(|_| A::SUPPORTS_ANNOTATIONS);
  shared.controller.edit_hint = edit_hint_for(&options, editor_language.is_some());
  let edit_hint = shared.controller.edit_hint;
  let state = Box::new(SelectionOverlayWindowState::<A> {
    owned_by_window: false,
    diagnostics: crate::overlay::performance::Diagnostics::from_env(A::METRICS_PREFIX),
    shared,
    editor: editor_language.map(|language| {
      crate::overlay::screenshot::editor::AnnotationEditor::new(language).with_frozen_pixels(
        crate::overlay::screenshot::editor::FrozenPixels {
          rgba: rgba.clone(),
          width,
          height,
        },
      )
    }),
    magnifier: if options.pick {
      crate::overlay::magnifier::Magnifier::picker()
    } else {
      crate::overlay::magnifier::Magnifier::new(options.magnifier)
    },
    pick: options.pick,
    hint: crate::overlay::selection_hint::SelectionHint::new(options.too_small),
    edit_hint: crate::overlay::edit_hint::EditHint::new(edit_hint),
    response_tx: Some(response_tx),
  });

  spawn_selection_overlay_thread(
    A::THREAD_NAME,
    A::WINDOW_TITLE,
    width,
    height,
    state,
    wnd_proc::<A>,
    run_selection_loop::<A>,
    selection_overlay_error::<A>,
    selection_overlay_error::<A>,
  )?;

  crate::overlay::channel::recv_overlay_result(A::WINDOW_TITLE, response_rx)
}

fn run_selection_loop<A: SelectionOverlayAdapter>(hwnd: HWND) {
  // SAFETY: CreateWindowEx succeeded; ownership now transfers from the creator to WM_DESTROY.
  if let Some(state) = unsafe { get_state::<A>(hwnd) } {
    state.owned_by_window = true;
  }

  let clock = match crate::overlay::animation_clock::AnimationClock::new() {
    Ok(clock) => clock,
    Err(error) => {
      tracing::warn!(%error, "High-resolution selection clock unavailable; using Win32 timer");
      run_legacy_selection_loop(hwnd);
      return;
    },
  };
  let mut cadence = crate::overlay::animation_clock::MonitorCadence::default();
  let result = clock.run(
    hwnd,
    crate::overlay::selection_core::SNAP_ANIM_TIMER_ID,
    |last_tick| {
      // SAFETY: callback runs only between dispatches on the live window's owning thread.
      let state = unsafe { get_state::<A>(hwnd) }?;
      let controller = &state.shared.controller;
      if controller.snap_anim.is_animating()
        || state.shared.resources.handle_motion.active()
        || state.shared.resources.indicator_motion.active()
        || state.shared.resources.border_feedback.active()
        || state.magnifier.animating()
        || state.hint.active()
        || state.edit_hint.active()
        || state
          .editor
          .as_ref()
          .is_some_and(|editor| editor.animation_active())
      {
        Some(last_tick + cadence.at_cursor())
      } else {
        None
      }
    },
  );
  if let Err(error) = result {
    tracing::error!(%error, "Selection animation clock failed; cancelling overlay");
  }
  close_selection_window(hwnd);
}

fn run_legacy_selection_loop(hwnd: HWND) {
  // SAFETY: timer belongs to this thread's live selection window; destruction removes it.
  if unsafe {
    SetTimer(
      Some(hwnd),
      crate::overlay::selection_core::SNAP_ANIM_TIMER_ID,
      16,
      None,
    )
  } == 0
  {
    tracing::error!("Legacy selection animation timer creation failed; cancelling overlay");
    // SAFETY: owning thread releases native resources and sends the cancellation result.
    if let Err(error) = unsafe { DestroyWindow(hwnd) } {
      tracing::error!(%error, "Selection cleanup after timer failure failed");
    }
    return;
  }
  crate::overlay::gdi::window::run_message_loop();
  close_selection_window(hwnd);
}

fn close_selection_window(hwnd: HWND) {
  // SAFETY: owning thread cancels and releases state even when WM_QUIT precedes WM_DESTROY.
  if unsafe { IsWindow(Some(hwnd)) }.as_bool()
    && let Err(error) = unsafe { DestroyWindow(hwnd) }
  {
    tracing::error!(%error, "Selection window cleanup failed");
  }
}

fn selection_overlay_error<A: SelectionOverlayAdapter>(state: Box<SelectionOverlayWindowState<A>>) {
  crate::overlay::channel::send_overlay_result(A::WINDOW_TITLE, state.response_tx, None);
}

unsafe extern "system" fn wnd_proc<A: SelectionOverlayAdapter>(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  match msg {
    WM_COMMAND if (wparam.0 >> 16) as u32 == EN_CHANGE => {
      // SAFETY: EDIT may notify synchronously during editor mutation. Post only; never borrow state here.
      if let Err(error) = unsafe {
        PostMessageW(
          Some(hwnd),
          crate::overlay::screenshot::text_field::CHANGED,
          WPARAM(0),
          lparam,
        )
      } {
        tracing::error!(%error, "Failed to post text preview update");
      }
      LRESULT(0)
    },
    crate::overlay::screenshot::text_field::CHANGED => {
      // SAFETY: posted update runs after native notification unwinds; stale child handles are rejected.
      if let Some(state) = unsafe { get_state::<A>(hwnd) }
        && let Some(editor) = &mut state.editor
      {
        editor.text_changed(
          &state.shared.controller,
          hwnd,
          Some(HWND(lparam.0 as *mut _)),
        );
      }
      LRESULT(0)
    },
    WM_CTLCOLOREDIT => {
      if let Some(result) = crate::overlay::screenshot::text_field::background(
        windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut _),
        HWND(lparam.0 as *mut _),
      ) {
        return result;
      }
      // SAFETY: default color handling for unrelated controls; no editor state is borrowed.
      unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    },
    crate::overlay::screenshot::text_field::COMPLETE => {
      // SAFETY: completion is posted by a child; the editor checks its HWND to reject stale actions.
      if let Some(state) = unsafe { get_state::<A>(hwnd) }
        && let Some(editor) = &mut state.editor
      {
        editor.complete_text(hwnd, wparam.0 != 0, Some(HWND(lparam.0 as *mut _)));
      }
      LRESULT(0)
    },
    crate::overlay::screenshot::editor::REDACT_READY => {
      // SAFETY: posted by a finished detection thread; the editor rejects stale job ids.
      if let Some(state) = unsafe { get_state::<A>(hwnd) }
        && let Some(editor) = &mut state.editor
      {
        editor.redaction_ready(&state.shared.controller, hwnd, wparam.0 as u64);
      }
      LRESULT(0)
    },
    WM_CREATE => unsafe { handle_create::<A>(hwnd, lparam) },
    WM_DISPLAYCHANGE => {
      // SAFETY: cancel the frozen selection rather than use stale capture/display bounds.
      if let Err(error) = unsafe { DestroyWindow(hwnd) } {
        tracing::error!(%error, "Cannot cancel selection after display topology change");
      }
      LRESULT(0)
    },
    WM_DESTROY => unsafe { handle_destroy::<A>(hwnd) },
    WM_ERASEBKGND => LRESULT(1),
    WM_PAINT => {
      if let Some(state) = unsafe { get_state::<A>(hwnd) } {
        draw_frame::<A>(state, hwnd);
      }
      LRESULT(0)
    },
    WM_MOUSEMOVE => {
      if let Some(state) = unsafe { get_state::<A>(hwnd) } {
        if let Some(diagnostics) = &mut state.diagnostics {
          diagnostics.input();
        }
        if state.pick {
          // Only the lens follows the pointer: no window snapping while picking a color.
          state.shared.controller.cursor_pos = client_point(lparam);
          sync_magnifier::<A>(state, hwnd);
          return LRESULT(0);
        }
        if state
          .editor
          .as_mut()
          .is_some_and(|editor| editor.pointer_move(&mut state.shared.controller, hwnd, lparam))
        {
          // SAFETY: thread-local capture query for this overlay's own window.
          if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() } == hwnd {
            apply_editor_cursor(state);
          }
        } else {
          handle_selection_mouse_move(&mut state.shared.controller, hwnd, lparam);
        }
        let show_indicator = state
          .editor
          .as_ref()
          .is_none_or(|editor| editor.shows_selection_indicator(&state.shared.controller));
        if (show_indicator && state.shared.controller.interaction.is_confirmed_idle())
          || state.shared.resources.border_feedback.active()
        {
          // Hover may be consumed by the annotation editor without invalidating crop chrome.
          // Repaint the whole scene so a departing handle glow is cleared as well.
          if !unsafe { InvalidateRect(Some(hwnd), None, false) }.as_bool() {
            tracing::warn!("Failed to repaint selection border feedback");
          }
        }
        sync_magnifier::<A>(state, hwnd);
      }
      LRESULT(0)
    },
    WM_RBUTTONUP if unsafe { get_state::<A>(hwnd) }.is_some_and(|state| state.pick) => {
      // SAFETY: cancelling the picker on its owning thread; WM_DESTROY sends no result.
      if let Err(error) = unsafe { DestroyWindow(hwnd) } {
        tracing::error!(%error, "Cannot cancel the color picker");
      }
      LRESULT(0)
    },
    WM_RBUTTONUP => {
      let mut reset = false;
      let mut consumed = false;
      if let Some(state) = unsafe { get_state::<A>(hwnd) }
        && let Some(editor) = &mut state.editor
        && state.shared.controller.interaction.is_confirmed_idle()
      {
        state.shared.controller.cursor_pos = (
          (lparam.0 & 0xffff) as i16 as f64,
          ((lparam.0 >> 16) & 0xffff) as i16 as f64,
        );
        consumed = editor.secondary_click(&state.shared.controller, hwnd);
      }
      if consumed {
        // SAFETY: release only this overlay's capture held by the cancelled drawing gesture.
        if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() } == hwnd
          && let Err(error) =
            unsafe { windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture() }
        {
          tracing::warn!(%error, "Failed to release capture after cancelling a drawing");
        }
        return LRESULT(0);
      }
      if let Some(state) = unsafe { get_state::<A>(hwnd) }
        && state.editor.is_some()
        && matches!(
          state.shared.controller.interaction,
          crate::overlay::selection::SelectionInteraction::Confirmed { .. }
        )
      {
        if let Some(editor) = &mut state.editor {
          editor.cancel_gesture();
          editor.complete_text(hwnd, false, None);
          editor.clear();
        }
        reset_selection_to_idle(&mut state.shared.controller);
        // Resolve the frozen window snapshot immediately at the click position.
        handle_selection_mouse_move(&mut state.shared.controller, hwnd, lparam);
        reset = true;
      }
      if reset {
        // SAFETY: release only this overlay's capture, after releasing editor-state borrows.
        if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() } == hwnd
          && let Err(error) =
            unsafe { windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture() }
        {
          tracing::warn!(%error, "Failed to release capture when returning to window snapping");
        }
      }
      LRESULT(0)
    },
    WM_XBUTTONUP => {
      if let Some(state) = unsafe { get_state::<A>(hwnd) }
        && let Some(editor) = &mut state.editor
      {
        editor.sync_crop(&mut state.shared.controller);
        // The high word of wParam names the side button; handled ones must return TRUE.
        if editor.side_button(&state.shared.controller, hwnd, (wparam.0 >> 16) as u16) {
          return LRESULT(1);
        }
      }
      // SAFETY: default processing for a side-button message on this live window.
      unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    },
    WM_LBUTTONDOWN => {
      if let Some(state) = unsafe { get_state::<A>(hwnd) } {
        if state.pick {
          pick_pixel::<A>(state, hwnd, client_point(lparam));
          return LRESULT(0);
        }
        if state.hint.active() {
          let animate = state.shared.resources.handle_motion.enabled();
          state.hint.dismiss(Instant::now(), animate);
          sync_hint::<A>(state, hwnd);
        }
        if let Some(editor) = &mut state.editor {
          editor.sync_crop(&mut state.shared.controller);
        }
        if state.editor.is_some() && state.shared.controller.interaction.is_confirmed_idle() {
          // Only the editor consumes click coordinates; ordinary selection keeps its existing path.
          state.shared.controller.cursor_pos = (
            (lparam.0 & 0xffff) as i16 as f64,
            ((lparam.0 >> 16) & 0xffff) as i16 as f64,
          );
        }
        if state
          .editor
          .as_mut()
          .is_some_and(|editor| editor.pointer_down(&state.shared.controller, hwnd))
        {
          // The press took capture, so no WM_SETCURSOR will close the grab hand.
          apply_editor_cursor(state);
        } else {
          let editing = state.editor.is_some()
            && matches!(
              state.shared.controller.interaction,
              crate::overlay::selection::SelectionInteraction::Confirmed { .. }
            );
          handle_selection_lbutton_down(&mut state.shared.controller, hwnd);
          if !matches!(
            state.shared.controller.interaction,
            crate::overlay::selection::SelectionInteraction::Confirmed { .. }
          ) && let Some(editor) = &mut state.editor
          {
            if editing {
              // Selecting again from the editor keeps the drawings and lands back in it.
              state.shared.controller.hold_on_release = true;
              editor.reselect();
            } else {
              editor.clear();
            }
          }
        }
        sync_magnifier::<A>(state, hwnd);
      }
      LRESULT(0)
    },
    WM_LBUTTONUP => {
      if let Some(state) = unsafe { get_state::<A>(hwnd) } {
        if state.editor.is_some() && state.shared.controller.interaction.is_confirmed_idle() {
          state.shared.controller.cursor_pos = (
            (lparam.0 & 0xffff) as i16 as f64,
            ((lparam.0 >> 16) & 0xffff) as i16 as f64,
          );
        }
        let consumed = state
          .editor
          .as_mut()
          .is_some_and(|editor| editor.pointer_up(&state.shared.controller, hwnd));
        if !consumed
          && let SelectionMouseUpOutcome::Confirm(selection) =
            handle_selection_lbutton_up(&mut state.shared.controller, hwnd)
        {
          let result = A::map_selection(state, selection);
          send_result_and_destroy::<A>(state, hwnd, Some(result));
        } else {
          if consumed {
            // Letting go reopens the grab hand without waiting for the next move.
            apply_editor_cursor(state);
          }
          show_too_small_hint::<A>(state, hwnd);
          sync_magnifier::<A>(state, hwnd);
        }
      }
      LRESULT(0)
    },
    WM_CAPTURECHANGED | WM_CANCELMODE => {
      // ReleaseCapture sends a synchronous notification with no new window. Do not
      // reborrow editor state from inside its own mouse-up handler in that case.
      if (msg == WM_CANCELMODE || lparam.0 != 0)
        && let Some(state) = unsafe { get_state::<A>(hwnd) }
        && let Some(editor) = &mut state.editor
      {
        editor.cancel_gesture();
        editor.text_changed(&state.shared.controller, hwnd, None);
        // SAFETY: this thread owns hwnd; schedule a repaint after cancelling the draft.
        if !unsafe { InvalidateRect(Some(hwnd), None, false) }.as_bool() {
          tracing::warn!("Failed to repaint cancelled annotation gesture");
        }
      }
      if (msg == WM_CANCELMODE || lparam.0 != 0)
        && let Some(state) = unsafe { get_state::<A>(hwnd) }
      {
        sync_magnifier::<A>(state, hwnd);
      }
      // SAFETY: default processing handles Win32 capture cancellation for this live window.
      unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    },
    WM_SETCURSOR => {
      if wparam.0 != hwnd.0 as usize {
        // SAFETY: let child controls choose their own text cursor without editor-state reentry.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
      }
      if let Some(state) = unsafe { get_state::<A>(hwnd) } {
        if state.editor.is_some() {
          // WM_SETCURSOR precedes WM_MOUSEMOVE: use the current position so entering a
          // drawing surface updates the cursor immediately rather than one event later.
          let mut point = POINT::default();
          // SAFETY: thread-local input query and coordinate conversion for the live overlay.
          match unsafe { GetCursorPos(&mut point) } {
            Ok(()) => {
              if unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point) }
                .as_bool()
              {
                state.shared.controller.cursor_pos = (point.x as f64, point.y as f64);
              } else {
                tracing::warn!("Failed to convert tool cursor position");
              }
            },
            Err(error) => tracing::warn!(%error, "Failed to query tool cursor position"),
          }
          if apply_editor_cursor(state) {
            return LRESULT(1);
          }
        }
        let is_idle = matches!(state.shared.controller.overlay_state, OverlayState::Idle);
        unsafe {
          SetCursor(Some(cursor_for_hit(
            selection_hit_for_cursor(&state.shared.controller),
            is_idle,
          )))
        };
        LRESULT(1)
      } else {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
      }
    },
    WM_TIMER => {
      if wparam.0 == crate::overlay::selection_core::SNAP_ANIM_TIMER_ID
        && let Some(state) = unsafe { get_state::<A>(hwnd) }
      {
        if state.magnifier.animating() {
          sync_magnifier::<A>(state, hwnd);
        }
        if state.hint.active() {
          sync_hint::<A>(state, hwnd);
        }
        if state.edit_hint.active() {
          sync_edit_hint::<A>(state, hwnd);
        }
        if should_invalidate_selection_timer(&state.shared.controller)
          || state.shared.resources.handle_motion.active()
          || state.shared.resources.indicator_motion.active()
          || state.shared.resources.border_feedback.active()
        {
          let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        } else if let Some(editor) = &mut state.editor
          && editor.animation_active()
        {
          editor.tick_animation(&state.shared.controller, hwnd);
        }
      }
      LRESULT(0)
    },
    WM_MOUSEWHEEL => {
      if let Some(state) = unsafe { get_state::<A>(hwnd) } {
        // The high word of wParam is the signed wheel distance.
        let delta = (wparam.0 >> 16) as u16 as i16;
        if state.magnifier.wheel(i32::from(delta)) {
          sync_magnifier::<A>(state, hwnd);
          return LRESULT(0);
        }
      }
      // SAFETY: default processing for a wheel message on this live window.
      unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    },
    WM_KEYDOWN | WM_SYSKEYDOWN => {
      if let Some(state) = unsafe { get_state::<A>(hwnd) } {
        if state.pick {
          const VK_SHIFT: i32 = 0x10;
          // SAFETY: GetKeyState reads this thread's input state and has no preconditions.
          let shift = unsafe { GetKeyState(VK_SHIFT) } < 0;
          match pick_key(wparam.0 as u32, shift) {
            Some(PickKey::Pick) => {
              let pointer = state.shared.controller.cursor_pos;
              pick_pixel::<A>(state, hwnd, pointer);
            },
            Some(PickKey::Cancel) => {
              // SAFETY: cancelling the picker on its owning thread; WM_DESTROY sends no result.
              if let Err(error) = unsafe { DestroyWindow(hwnd) } {
                tracing::error!(%error, "Cannot cancel the color picker");
              }
            },
            Some(PickKey::Nudge(dx, dy)) => nudge_pointer(dx, dy),
            None => {},
          }
          return LRESULT(0);
        }
        if let Some(editor) = &mut state.editor {
          editor.sync_crop(&mut state.shared.controller);
        }
        let vk = wparam.0 as u32;
        if state
          .editor
          .as_mut()
          .is_some_and(|editor| editor.keydown(&state.shared.controller, hwnd, vk))
        {
          return LRESULT(0);
        }
        // 0x72 is VK_F3.
        if vk == 0x72 && state.editor.is_some() {
          // F3 pins the confirmed selection, drawings included, instead of saving it.
          if let SelectionKeyOutcome::Confirm(selection) = handle_selection_keydown(
            &mut state.shared.controller,
            hwnd,
            crate::overlay::selection_core::VK_RETURN,
          ) {
            let mut result = A::map_selection(state, selection);
            if A::pin(&mut result) {
              send_result_and_destroy::<A>(state, hwnd, Some(result));
            }
          }
          return LRESULT(0);
        }
        if msg == WM_KEYDOWN && is_magnifier_toggle(vk, lparam) {
          state.magnifier.toggle();
          sync_magnifier::<A>(state, hwnd);
          return LRESULT(0);
        }
        if vk == crate::overlay::selection_core::VK_BACK
          && let Some(editor) = &mut state.editor
        {
          editor.clear();
        }
        match handle_selection_keydown(&mut state.shared.controller, hwnd, vk) {
          SelectionKeyOutcome::Confirm(selection) => {
            let result = A::map_selection(state, selection);
            send_result_and_destroy::<A>(state, hwnd, Some(result));
          },
          SelectionKeyOutcome::CancelOverlay => unsafe {
            let _ = DestroyWindow(hwnd);
          },
          SelectionKeyOutcome::None => {
            if vk == 0x10
              && let Some(editor) = &mut state.editor
            {
              editor.modifiers_changed(&state.shared.controller, hwnd);
            }
            sync_magnifier::<A>(state, hwnd);
          },
        }
      }
      LRESULT(0)
    },
    WM_KEYUP | WM_SYSKEYUP => {
      if let Some(state) = unsafe { get_state::<A>(hwnd) } {
        handle_selection_keyup(&mut state.shared.controller, wparam.0 as u32);
        if wparam.0 == 0x10
          && let Some(editor) = &mut state.editor
        {
          editor.modifiers_changed(&state.shared.controller, hwnd);
        }
      }
      LRESULT(0)
    },
    _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickKey {
  Pick,
  Cancel,
  /// Move the pointer by this many physical pixels.
  Nudge(i32, i32),
}

/// The Color Picker's keys: Enter or Space picks, Esc cancels, arrows move the pointer one
/// pixel, or ten with Shift. Everything else, including the magnifier toggle, does nothing.
fn pick_key(vk: u32, shift: bool) -> Option<PickKey> {
  const VK_RETURN: u32 = 0x0D;
  const VK_ESCAPE: u32 = 0x1B;
  const VK_SPACE: u32 = 0x20;
  const VK_LEFT: u32 = 0x25;
  const VK_UP: u32 = 0x26;
  const VK_RIGHT: u32 = 0x27;
  const VK_DOWN: u32 = 0x28;
  let step = if shift { 10 } else { 1 };
  match vk {
    VK_RETURN | VK_SPACE => Some(PickKey::Pick),
    VK_ESCAPE => Some(PickKey::Cancel),
    VK_LEFT => Some(PickKey::Nudge(-step, 0)),
    VK_RIGHT => Some(PickKey::Nudge(step, 0)),
    VK_UP => Some(PickKey::Nudge(0, -step)),
    VK_DOWN => Some(PickKey::Nudge(0, step)),
    _ => None,
  }
}

/// Moves the real pointer, so the lens and the next pick follow through the usual mouse message.
fn nudge_pointer(dx: i32, dy: i32) {
  let mut point = POINT::default();
  // SAFETY: thread-local input query and an ordinary pointer move; Windows clamps to the desktop.
  let moved = unsafe { GetCursorPos(&mut point) }
    .and_then(|()| unsafe { SetCursorPos(point.x + dx, point.y + dy) });
  if let Err(error) = moved {
    tracing::warn!(%error, "Cannot move the pointer for the color picker");
  }
}

fn client_point(lparam: LPARAM) -> (f64, f64) {
  (
    (lparam.0 & 0xffff) as i16 as f64,
    ((lparam.0 >> 16) & 0xffff) as i16 as f64,
  )
}

/// The single pixel under the pointer, returned as a 1×1 selection so the capture's own crop
/// delivers it.
fn pick_pixel<A: SelectionOverlayAdapter>(
  state: &mut SelectionOverlayWindowState<A>,
  hwnd: HWND,
  pointer: (f64, f64),
) {
  let selection = picked_pixel(
    pointer,
    state.shared.controller.width,
    state.shared.controller.height,
  );
  let result = A::map_selection(state, selection);
  send_result_and_destroy::<A>(state, hwnd, Some(result));
}

/// The capture pixel containing `pointer`, kept inside the capture.
pub(crate) fn picked_pixel(pointer: (f64, f64), width: i32, height: i32) -> Selection {
  let clamp =
    |value: f64, size: i32| (value.floor() as i64).clamp(0, i64::from(size.max(1) - 1)) as u32;
  Selection {
    x: clamp(pointer.0, width),
    y: clamp(pointer.1, height),
    width: 1,
    height: 1,
    source_app: None,
  }
}

/// `M` alone, once per press. Text entry never reaches this: the editor consumes its keys first.
fn is_magnifier_toggle(vk: u32, lparam: LPARAM) -> bool {
  const VK_M: u32 = 0x4D;
  const VK_CONTROL: i32 = 0x11;
  const REPEAT: isize = 1 << 30;
  // SAFETY: GetKeyState reads this thread's input state and has no preconditions.
  vk == VK_M && lparam.0 & REPEAT == 0 && unsafe { GetKeyState(VK_CONTROL) } >= 0
}

/// An open overlay gets no pointer message until the mouse moves, so a lens shown from the start
/// must find the pointer itself.
fn seed_pointer(
  controller: &mut crate::overlay::selection_core::SelectionControllerState,
  hwnd: HWND,
) {
  let mut point = POINT::default();
  // SAFETY: thread-local input query and coordinate conversion for the live overlay.
  let located = unsafe { GetCursorPos(&mut point) }.is_ok()
    && unsafe { windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point) }.as_bool();
  if located {
    controller.cursor_pos = (point.x as f64, point.y as f64);
  } else {
    tracing::warn!("Failed to locate the pointer for the magnifier");
  }
}

/// Tells the user why a finished gesture did nothing. The message is helpful, not essential,
/// so a failure is logged and the selection carries on.
fn show_too_small_hint<A: SelectionOverlayAdapter>(
  state: &mut SelectionOverlayWindowState<A>,
  hwnd: HWND,
) {
  if !std::mem::take(&mut state.shared.controller.rejected_too_small) {
    return;
  }
  let controller = &state.shared.controller;
  let pointer = (
    controller.cursor_pos.0.floor() as i32,
    controller.cursor_pos.1.floor() as i32,
  );
  let shown = state.hint.show(
    pointer,
    crate::overlay::magnifier::monitor_at(controller, pointer),
    state.magnifier.lens().map(|lens| lens.rect),
    Instant::now(),
    state.shared.resources.handle_motion.enabled(),
  );
  if let Err(error) = shown {
    tracing::error!(%error, "Selection size message failed");
    state.hint.turn_off();
  }
  sync_hint::<A>(state, hwnd);
}

fn sync_hint<A: SelectionOverlayAdapter>(state: &mut SelectionOverlayWindowState<A>, hwnd: HWND) {
  let areas = state.hint.update(
    Instant::now(),
    state.shared.resources.handle_motion.enabled(),
    state.shared.controller.height,
  );
  for area in areas.iter().flatten() {
    // SAFETY: this thread owns hwnd and area outlives the call.
    if !unsafe { InvalidateRect(Some(hwnd), Some(area), false) }.as_bool() {
      tracing::warn!("Failed to repaint selection message");
    }
  }
}

/// Sets the annotation editor's cursor at the controller position. Windows sends no
/// WM_SETCURSOR to a window holding capture, so presses and captured drags apply it here too.
/// Returns false when the editor leaves the cursor to the selection hit test.
fn apply_editor_cursor<A: SelectionOverlayAdapter>(
  state: &mut SelectionOverlayWindowState<A>,
) -> bool {
  let Some(editor) = &mut state.editor else {
    return false;
  };
  let controller = &state.shared.controller;
  if editor.toolbar_cursor(controller) || editor.grip_cursor(controller) {
    let cursor = if editor.grip_cursor(controller) {
      editor.grip_hand().or_else(|error| {
        tracing::warn!(%error, "Failed to create the grip hand; using the Windows hand");
        crate::overlay::screenshot::cursors::system(IDC_HAND)
      })
    } else {
      crate::overlay::screenshot::cursors::system(IDC_ARROW)
    };
    match cursor {
      // SAFETY: the editor owns the hand, or Windows the shared cursor, on the overlay thread.
      Ok(cursor) => unsafe {
        SetCursor(Some(cursor));
      },
      Err(error) => tracing::warn!(%error, "Failed to load toolbar cursor"),
    }
    return true;
  }
  match editor.tool_cursor(controller) {
    Ok(Some(cursor)) => {
      // SAFETY: editor owns this live tool cursor on the overlay thread.
      unsafe {
        SetCursor(Some(cursor));
      }
      true
    },
    Ok(None) => false,
    Err(error) => {
      tracing::warn!(%error, "Failed to create tool cursors; using selection cursor");
      false
    },
  }
}

/// Repaints only the old and new lens areas; moving one must leave no stale pixels behind. The
/// edit hint follows, since it keeps clear of the lens.
fn sync_magnifier<A: SelectionOverlayAdapter>(
  state: &mut SelectionOverlayWindowState<A>,
  hwnd: HWND,
) {
  sync_lens::<A>(state, hwnd);
  sync_edit_hint::<A>(state, hwnd);
}

fn sync_lens<A: SelectionOverlayAdapter>(state: &mut SelectionOverlayWindowState<A>, hwnd: HWND) {
  if !state.magnifier.needs_update() {
    return;
  }
  let controller = &state.shared.controller;
  let areas = state.magnifier.update(
    controller,
    |focus| crate::overlay::magnifier::monitor_at(controller, focus),
    state
      .editor
      .as_ref()
      .and_then(|editor| editor.crop_toolbar_rect(controller)),
    Instant::now(),
    state.shared.resources.handle_motion.enabled(),
  );
  for area in areas.iter().flatten() {
    // SAFETY: this thread owns hwnd and area outlives the call.
    if !unsafe { InvalidateRect(Some(hwnd), Some(area), false) }.as_bool() {
      tracing::warn!("Failed to repaint magnifier area");
    }
  }
}

/// Keeps the edit hint on the pointer's monitor and clear of the lens and dimensions. It waits for
/// the first frame, so it never delays the overlay appearing. The hint is helpful, not essential,
/// so a failure is logged and the selection carries on.
fn sync_edit_hint<A: SelectionOverlayAdapter>(
  state: &mut SelectionOverlayWindowState<A>,
  hwnd: HWND,
) {
  let controller = &state.shared.controller;
  let visible =
    state.shared.resources.first_paint_logged && controller.visible_edit_hint().is_some();
  let pointer = (
    controller.cursor_pos.0.floor() as i32,
    controller.cursor_pos.1.floor() as i32,
  );
  let label = crate::overlay::magnifier::selection_rect(controller).map(|rect| {
    crate::overlay::gdi::drawing::selection_label_bounds(rect, controller.width, controller.height)
  });
  let updated = state.edit_hint.update(
    visible,
    pointer,
    |point| crate::overlay::magnifier::monitor_at(controller, point),
    &[state.magnifier.lens().map(|lens| lens.rect), label],
    Instant::now(),
    state.shared.resources.handle_motion.enabled(),
  );
  let areas = match updated {
    Ok(areas) => areas,
    Err(error) => {
      tracing::error!(%error, "Edit hint failed");
      state.edit_hint.turn_off();
      return;
    },
  };
  for area in areas.iter().flatten() {
    // SAFETY: this thread owns hwnd and area outlives the call.
    if !unsafe { InvalidateRect(Some(hwnd), Some(area), false) }.as_bool() {
      tracing::warn!("Failed to repaint edit hint");
    }
  }
}

unsafe fn get_state<A: SelectionOverlayAdapter>(
  hwnd: HWND,
) -> Option<&'static mut SelectionOverlayWindowState<A>> {
  let ptr =
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut SelectionOverlayWindowState<A>;
  if ptr.is_null() {
    None
  } else {
    Some(unsafe { &mut *ptr })
  }
}

unsafe fn handle_create<A: SelectionOverlayAdapter>(hwnd: HWND, lparam: LPARAM) -> LRESULT {
  let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
  let state_ptr = cs.lpCreateParams as *mut SelectionOverlayWindowState<A>;
  if state_ptr.is_null() {
    tracing::error!("WM_CREATE: lpCreateParams is null");
    return LRESULT(-1);
  }

  unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };

  let state = unsafe { &mut *state_ptr };
  if state.editor.is_some() {
    // SAFETY: this thread owns the parent; exclude native edit children from parent presentation.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    if unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_CLIPCHILDREN.0 as isize) } == 0 {
      tracing::warn!("Failed to apply annotation child clipping style");
    }
  }
  if let Err(e) = initialize_selection_window(&mut state.shared, hwnd, A::METRICS_PREFIX) {
    tracing::error!(error = %e, metrics_prefix = A::METRICS_PREFIX, "Failed to initialize selection overlay");
    return LRESULT(-1);
  }
  if state.magnifier.needs_update() {
    seed_pointer(&mut state.shared.controller, hwnd);
    sync_magnifier::<A>(state, hwnd);
  }

  LRESULT(0)
}

unsafe fn handle_destroy<A: SelectionOverlayAdapter>(hwnd: HWND) -> LRESULT {
  let ptr =
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut SelectionOverlayWindowState<A>;
  if !ptr.is_null() {
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
    // SAFETY: pointer is valid until either WM_DESTROY or the creator reclaims it.
    if unsafe { (*ptr).owned_by_window } {
      // SAFETY: successful creation transferred unique allocation ownership to the window.
      let mut state = unsafe { Box::from_raw(ptr) };
      destroy_selection_window(&mut state.shared);
      crate::overlay::channel::send_overlay_result(A::WINDOW_TITLE, state.response_tx.take(), None);
    } else {
      // SAFETY: failed creation retains creator ownership; release native resources, not the Box.
      let state = unsafe { &mut *ptr };
      destroy_selection_window(&mut state.shared);
      // The creator sends cancellation when CreateWindowEx returns failure.
    }
    crate::screenshot::release_capture_idle_memory();
  }

  unsafe { PostQuitMessage(0) };
  LRESULT(0)
}

fn draw_frame<A: SelectionOverlayAdapter>(state: &mut SelectionOverlayWindowState<A>, hwnd: HWND) {
  let mut ps = PAINTSTRUCT::default();
  // SAFETY: this window's owner thread pairs BeginPaint with EndPaint on every path.
  let first = !state.shared.resources.first_paint_logged;
  let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
  if ps.rcPaint.right > ps.rcPaint.left && ps.rcPaint.bottom > ps.rcPaint.top {
    render_selection_frame::<A>(state, hdc, ps.rcPaint);
  }
  // SAFETY: balances this scope's BeginPaint, including empty update regions.
  let _ = unsafe { EndPaint(hwnd, &ps) };
  if first {
    // After the first frame is on screen, so the edit hint never delays it.
    sync_edit_hint::<A>(state, hwnd);
  }
}

fn render_selection_frame<A: SelectionOverlayAdapter>(
  state: &mut SelectionOverlayWindowState<A>,
  hdc: windows::Win32::Graphics::Gdi::HDC,
  damage: windows::Win32::Foundation::RECT,
) {
  let handles_were_animating = state.shared.resources.handle_motion.active();
  state.shared.resources.handle_motion.update(
    state.shared.controller.interaction.is_confirmed_idle(),
    0,
    None,
    Instant::now(),
  );
  let handles_need_scene = handles_were_animating
    || state.shared.resources.handle_motion.active()
    || state.shared.resources.indicator_motion.active()
    || state.shared.resources.border_feedback.active();
  if let Some(editor) = &mut state.editor {
    editor.sync_crop(&mut state.shared.controller);
  }
  let show_indicator = state
    .editor
    .as_ref()
    .is_none_or(|editor| editor.shows_selection_indicator(&state.shared.controller));
  let indicator_was_animating = state.shared.resources.indicator_motion.active();
  state.shared.resources.indicator_motion.update(
    show_indicator
      && state
        .shared
        .controller
        .interaction
        .get_current_rect(
          state.shared.controller.width as f64,
          state.shared.controller.height as f64,
        )
        .is_some(),
    0,
    None,
    Instant::now(),
  );
  let indicator_needs_scene =
    indicator_was_animating || state.shared.resources.indicator_motion.active();
  let border_needs_scene = state.shared.resources.border_feedback.update(
    &state.shared.controller,
    show_indicator,
    Instant::now(),
  );
  let is_first_paint = !state.shared.resources.first_paint_logged;
  if is_first_paint {
    state.shared.resources.first_paint_logged = true;
    crate::commands::log_speed_snapshot(
      &format!("{}.first_paint", A::METRICS_PREFIX),
      state.shared.resources.launch_started_at.elapsed(),
    );
  }

  let frame_started = Instant::now();
  let hfont = state.shared.resources.hfont;

  let measured = state
    .editor
    .as_ref()
    .is_some_and(|editor| editor.diagnostics.is_some());
  let scene_started = measured.then(Instant::now);
  let restored = state.editor.as_mut().and_then(|editor| {
    if handles_need_scene || border_needs_scene || indicator_needs_scene {
      return None;
    }
    editor.restore_scene(
      &state.shared.controller,
      state.shared.resources.offscreen_dc,
      damage,
    )
  });
  if state.pick {
    // The picker shows the frozen screen as it is, so colors are judged against true pixels.
    if let Some(shot) = &state.shared.resources.screenshot {
      // SAFETY: both are live memory DCs owned by this overlay thread.
      if let Err(error) = unsafe {
        BitBlt(
          state.shared.resources.offscreen_dc,
          damage.left,
          damage.top,
          (damage.right - damage.left).max(0),
          (damage.bottom - damage.top).max(0),
          Some(shot.dc),
          damage.left,
          damage.top,
          SRCCOPY,
        )
      } {
        tracing::warn!(%error, "Color picker frame copy failed");
      }
    }
  } else if restored.is_none() {
    let show_indicator = state
      .editor
      .as_ref()
      .is_none_or(|editor| editor.shows_selection_indicator(&state.shared.controller));
    draw_selection_scene_with_indicator(&mut state.shared, hfont, A::colors(), show_indicator);
  }
  let scene_us = scene_started
    .map(crate::overlay::screenshot::performance::elapsed_us)
    .unwrap_or_default();
  if let Some(editor) = &mut state.editor {
    editor.paint_cached(
      &state.shared.controller,
      state.shared.resources.offscreen_dc,
      hfont,
      damage,
      restored.is_some(),
    );
  }

  // After the scene and its cache, so neither ever holds lens pixels.
  if state.magnifier.lens().is_some()
    && let Some(shot) = &state.shared.resources.screenshot
  {
    let painted = state.magnifier.paint(
      state.shared.resources.offscreen_dc,
      shot.dc,
      shot.width,
      shot.height,
      damage,
    );
    if let Err(error) = painted {
      tracing::error!(%error, "Magnifier rendering failed");
      state.magnifier.turn_off();
      if let Some(editor) = &mut state.editor {
        editor.report_magnifier_failure();
      }
    }
  }

  if let Err(error) = state
    .edit_hint
    .paint(state.shared.resources.offscreen_dc, damage)
  {
    tracing::error!(%error, "Edit hint rendering failed");
    state.edit_hint.turn_off();
  }

  if let Err(error) = state
    .hint
    .paint(state.shared.resources.offscreen_dc, damage)
  {
    tracing::error!(%error, "Selection size message rendering failed");
    state.hint.turn_off();
  }

  if is_first_paint {
    crate::commands::log_speed_snapshot(
      &format!("{}.first_paint.selection_scene", A::METRICS_PREFIX),
      frame_started.elapsed(),
    );
  }

  let present_started = measured.then(Instant::now);
  if let Err(e) = unsafe {
    BitBlt(
      hdc,
      damage.left,
      damage.top,
      (damage.right - damage.left).max(0),
      (damage.bottom - damage.top).max(0),
      Some(state.shared.resources.offscreen_dc),
      damage.left,
      damage.top,
      SRCCOPY,
    )
  } {
    tracing::warn!(error = %e, metrics_prefix = A::METRICS_PREFIX, "BitBlt failed in selection overlay (present)");
  }

  if is_first_paint {
    crate::commands::log_speed_snapshot(
      &format!("{}.first_paint.present", A::METRICS_PREFIX),
      frame_started.elapsed(),
    );
    crate::commands::log_shortcut_to_overlay();
  }

  let present_us = present_started
    .map(crate::overlay::screenshot::performance::elapsed_us)
    .unwrap_or_default();
  if let Some(editor) = &mut state.editor
    && let Some(diagnostics) = &mut editor.diagnostics
  {
    let mut sample = std::mem::take(&mut editor.paint_sample);
    sample.total_us = crate::overlay::screenshot::performance::elapsed_us(frame_started);
    sample.scene_us = scene_us;
    sample.present_us = present_us;
    sample.dirty_pixels =
      (damage.right - damage.left).max(0) as u64 * (damage.bottom - damage.top).max(0) as u64;
    sample.composed_pixels = restored.unwrap_or_else(|| {
      state.shared.controller.width.max(0) as u64 * state.shared.controller.height.max(0) as u64
    });
    sample.presented_pixels = sample.dirty_pixels;
    diagnostics.record(sample);
  }
  if let Some(diagnostics) = &mut state.diagnostics {
    diagnostics.record(frame_started);
  }
  if is_first_paint {
    crate::commands::log_speed_snapshot(
      &format!("{}.first_paint.total_draw_frame", A::METRICS_PREFIX),
      frame_started.elapsed(),
    );
  }
}

fn send_result_and_destroy<A: SelectionOverlayAdapter>(
  state: &mut SelectionOverlayWindowState<A>,
  hwnd: HWND,
  mut result: Option<A::Result>,
) {
  if let Some(item) = result.as_mut() {
    A::finalize_result(state, item);
  }

  crate::overlay::channel::send_overlay_result(A::WINDOW_TITLE, state.response_tx.take(), result);

  unsafe {
    let _ = DestroyWindow(hwnd);
  }
}

#[cfg(test)]
mod message_tests;

#[cfg(test)]
mod option_tests {
  use super::*;

  #[test]
  fn edit_hint_is_offered_only_for_editor_captures_that_finish_on_release() {
    let options = SelectionOptions {
      edit_hint: "Hold Alt to edit",
      ..SelectionOptions::default()
    };
    assert_eq!(edit_hint_for(&options, true), Some("Hold Alt to edit"));
    // Copy Text, GIF and video selection have no editor for Alt to open.
    assert_eq!(edit_hint_for(&options, false), None);
    let capture_and_edit = SelectionOptions {
      hold_on_release: true,
      ..options
    };
    assert_eq!(edit_hint_for(&capture_and_edit, true), None);
    assert_eq!(edit_hint_for(&SelectionOptions::default(), true), None);
  }
}

#[cfg(test)]
mod lifecycle_tests {
  use super::*;
  use std::time::Duration;

  struct FailedCreation;
  impl SelectionOverlayAdapter for FailedCreation {
    type Result = Selection;
    const THREAD_NAME: &'static str = "failed-selection-creation-test";
    const WINDOW_TITLE: &'static str = "Failed selection creation test";
    const METRICS_PREFIX: &'static str = "test.failed_selection";
    fn colors() -> SelectionDrawColors {
      SelectionDrawColors::DEFAULT
    }
    fn map_selection(
      _state: &SelectionOverlayWindowState<Self>,
      selection: Selection,
    ) -> Selection {
      selection
    }
    fn finalize_result(_state: &SelectionOverlayWindowState<Self>, _result: &mut Selection) {}
  }

  #[test]
  fn failed_native_creation_cancels_and_releases_state_once() {
    // Invalid RGBA fails inside WM_CREATE, exercising the synchronous WM_DESTROY path.
    let pixels = Arc::new(Vec::new());
    for _ in 0..3 {
      assert!(
        show_selection_overlay::<FailedCreation>(
          32,
          24,
          pixels.clone(),
          0,
          0,
          SelectionOptions::default()
        )
        .expect("creation failure should cancel")
        .is_none()
      );
      // Cancellation wakes the caller before its dedicated thread has necessarily finished Drop.
      let deadline = Instant::now() + Duration::from_secs(1);
      while Arc::strong_count(&pixels) != 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
      }
      assert_eq!(
        Arc::strong_count(&pixels),
        1,
        "failed creation retained state"
      );
    }
  }
}
