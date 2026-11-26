use super::*;
use crate::overlay::gdi::window::{
  create_hud_window, register_overlay_class, unregister_overlay_class,
};
use crate::overlay::selection::{Rect, SelectionInteraction};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static NEXT: AtomicUsize = AtomicUsize::new(0);

#[test]
fn hidden_annotation_host_rejects_stale_child_messages_and_resets_crop_without_capture() {
  let mut f = Fixture::new();
  let rect = Rect {
    x: 30.0,
    y: 30.0,
    w: 200.0,
    h: 140.0,
  };
  f.state().shared.controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  f.state().shared.controller.overlay_state = OverlayState::OnHold { rect };
  f.state().editor = Some(crate::overlay::screenshot::editor::AnnotationEditor::new(
    "en".into(),
  ));
  let point = LPARAM((90 | (100 << 16)) as isize);
  // SAFETY: messages go synchronously to this thread's hidden HWND; no global input.
  unsafe {
    SendMessageW(f.hwnd, WM_MOUSEMOVE, None, Some(point));
    SendMessageW(
      f.hwnd,
      WM_COMMAND,
      Some(WPARAM((EN_CHANGE as usize) << 16)),
      Some(LPARAM(1)),
    );
    SendMessageW(
      f.hwnd,
      crate::overlay::screenshot::text_field::CHANGED,
      None,
      Some(LPARAM(1)),
    );
    SendMessageW(
      f.hwnd,
      crate::overlay::screenshot::text_field::COMPLETE,
      Some(WPARAM(1)),
      Some(LPARAM(1)),
    );
    SendMessageW(f.hwnd, WM_CAPTURECHANGED, None, Some(LPARAM(1)));
  }
  assert!(f.state().shared.controller.interaction.is_confirmed_idle());
  assert_eq!(f.state().shared.controller.cursor_pos, (90.0, 100.0));
  f.send(WM_KEYUP, 0x10);
  f.send(WM_TIMER, crate::overlay::selection_core::SNAP_ANIM_TIMER_ID);
  f.send(WM_CANCELMODE, 0);
  f.send(WM_RBUTTONUP, 0);
  assert!(matches!(
    f.state().shared.controller.interaction,
    SelectionInteraction::Idle { .. }
  ));
  assert!(f.response.try_recv().is_err());
  // SAFETY: the hidden fixture must remain unpresented after repaint requests.
  assert!(!unsafe { IsWindowVisible(f.hwnd) }.as_bool());
  f.send(WM_DISPLAYCHANGE, 0);
  assert!(f.result().is_none());
  assert_eq!(Arc::strong_count(&f.pixels), 1);
}
#[test]
fn side_button_messages_commit_the_pending_crop_then_step_crop_history() {
  let mut f = Fixture::new();
  let crop = |w| Rect {
    x: 30.0,
    y: 30.0,
    w,
    h: 140.0,
  };
  let confirmed = |rect| SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let width = |f: &mut Fixture| {
    let state = f.state();
    if let Some(editor) = &mut state.editor {
      editor.sync_crop(&mut state.shared.controller);
    }
    match state.shared.controller.interaction {
      SelectionInteraction::Confirmed { rect, .. } => rect.w,
      _ => 0.0,
    }
  };
  f.state().shared.controller.interaction = confirmed(crop(200.0));
  f.state().editor = Some(crate::overlay::screenshot::editor::AnnotationEditor::new(
    "en".into(),
  ));
  let back = (XBUTTON1 as usize) << 16;
  let forward = (XBUTTON2 as usize) << 16;
  assert_eq!(f.send(WM_XBUTTONUP, forward), LRESULT(1));
  f.state().shared.controller.interaction = confirmed(crop(120.0));
  assert_eq!(f.send(WM_XBUTTONUP, back), LRESULT(1));
  assert_eq!(width(&mut f), 200.0);
  assert_eq!(f.send(WM_XBUTTONUP, forward), LRESULT(1));
  assert_eq!(width(&mut f), 120.0);
  assert!(f.response.try_recv().is_err());
}
struct Adapter;
impl SelectionOverlayAdapter for Adapter {
  type Result = (Selection, bool);
  const THREAD_NAME: &'static str = "hidden-selection-messages";
  const WINDOW_TITLE: &'static str = "Hidden selection messages";
  const METRICS_PREFIX: &'static str = "test.hidden_selection";
  fn colors() -> SelectionDrawColors {
    SelectionDrawColors::DEFAULT
  }
  fn map_selection(_: &SelectionOverlayWindowState<Self>, selection: Selection) -> Self::Result {
    (selection, false)
  }
  fn finalize_result(_: &SelectionOverlayWindowState<Self>, result: &mut Self::Result) {
    result.1 = true;
  }
}
unsafe extern "system" fn hidden_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
  // SAFETY: skip the visible initialization; attach uniquely owned state after creation.
  if msg == WM_CREATE {
    LRESULT(0)
  } else if msg == WM_TIMER && w.0 == 0xcafe {
    // The fixture's bounded owner-thread timer closes only its hidden window.
    unsafe { DestroyWindow(hwnd) }.unwrap();
    LRESULT(0)
  } else {
    unsafe { wnd_proc::<Adapter>(hwnd, msg, w, l) }
  }
}

#[test]
fn native_and_legacy_selection_loops_destroy_the_owned_hidden_window_and_release_pixels() {
  for legacy in [false, true] {
    let f = Fixture::new();
    // SAFETY: a bounded timer belongs only to this fixture HWND; no desktop input or presentation.
    assert_ne!(unsafe { SetTimer(Some(f.hwnd), 0xcafe, 20, None) }, 0);
    let started = Instant::now();
    if legacy {
      run_legacy_selection_loop(f.hwnd);
    } else {
      run_selection_loop::<Adapter>(f.hwnd);
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(f.result().is_none());
    assert_eq!(Arc::strong_count(&f.pixels), 1);
    assert!(!unsafe { IsWindow(Some(f.hwnd)) }.as_bool());
  }
}
struct Fixture {
  hwnd: HWND,
  class: String,
  pixels: Arc<Vec<u8>>,
  response: std::sync::mpsc::Receiver<Option<(Selection, bool)>>,
  output: Option<crate::overlay::gdi::bitmap::ScreenshotBitmap>,
}
impl Fixture {
  fn new() -> Self {
    let class = format!(
      "FlinchHiddenSelection{}",
      NEXT.fetch_add(1, Ordering::Relaxed)
    );
    register_overlay_class(&class, Some(hidden_proc)).unwrap();
    let hwnd = create_hud_window(
      &class,
      Adapter::WINDOW_TITLE,
      0,
      0,
      320,
      240,
      std::ptr::null_mut(),
    )
    .unwrap();
    let pixels = Arc::new([20, 40, 70, 255].repeat(320 * 240));
    let (tx, response) = std::sync::mpsc::channel();
    let state = Box::new(SelectionOverlayWindowState::<Adapter> {
      owned_by_window: true,
      diagnostics: None,
      editor: None,
      magnifier: Default::default(),
      hint: Default::default(),
      edit_hint: crate::overlay::edit_hint::EditHint::new(None),
      pick: false,
      shared: SharedSelectionOverlayState::new(
        320,
        240,
        pixels.clone(),
        -320,
        -100,
        Instant::now(),
      ),
      response_tx: Some(tx),
    });
    // SAFETY: transfer the Box to this hidden window; production WM_DESTROY reclaims it.
    unsafe {
      SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    }
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
    Self {
      hwnd,
      class,
      pixels,
      response,
      output: None,
    }
  }
  fn send(&self, msg: u32, key: usize) -> LRESULT {
    // SAFETY: synchronous dispatch on the window owner thread, no global input injection.
    unsafe { SendMessageW(self.hwnd, msg, Some(WPARAM(key)), Some(LPARAM(0))) }
  }
  fn state(&mut self) -> &mut SelectionOverlayWindowState<Adapter> {
    unsafe { get_state::<Adapter>(self.hwnd) }.unwrap()
  }
  fn result(&self) -> Option<(Selection, bool)> {
    self
      .response
      .recv_timeout(std::time::Duration::from_secs(1))
      .unwrap()
  }
}
impl Drop for Fixture {
  fn drop(&mut self) {
    // SAFETY: destroy only the fixture's own live HWND, then clear its thread-local quit message.
    unsafe {
      if IsWindow(Some(self.hwnd)).as_bool() {
        if self.output.is_some() {
          get_state::<Adapter>(self.hwnd)
            .unwrap()
            .shared
            .resources
            .offscreen_dc = Default::default();
        }
        DestroyWindow(self.hwnd).unwrap();
      }
      let mut message = MSG::default();
      while PeekMessageW(&mut message, None, WM_QUIT, WM_QUIT, PM_REMOVE).as_bool() {}
    }
    unregister_overlay_class(&self.class);
  }
}

#[test]
fn hidden_selection_window_paints_frozen_pixels_and_editor_chrome_without_showing() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  let mut f = Fixture::new();
  let output = ScreenshotBitmap::from_rgba(&[0; 320 * 240 * 4], 320, 240).unwrap();
  let dc = output.dc;
  f.output = Some(output);
  f.state().shared.resources.offscreen_dc = dc;
  f.state().shared.resources.screenshot =
    Some(ScreenshotBitmap::from_rgba(&f.pixels, 320, 240).unwrap());
  let rect = Rect {
    x: 30.0,
    y: 30.0,
    w: 200.0,
    h: 140.0,
  };
  f.state().shared.controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  f.state().shared.controller.overlay_state = OverlayState::OnHold { rect };
  for editor in [false, true] {
    if editor {
      f.state().editor = Some(crate::overlay::screenshot::editor::AnnotationEditor::new(
        "en".into(),
      ));
    }
    for key in [0, 0x50, 0x53, 0x58] {
      if key != 0 {
        f.send(WM_KEYDOWN, key);
      }
      let presentation = ScreenshotBitmap::from_rgba(&[0; 320 * 240 * 4], 320, 240).unwrap();
      render_selection_frame::<Adapter>(
        f.state(),
        presentation.dc,
        windows::Win32::Foundation::RECT {
          left: 0,
          top: 0,
          right: 320,
          bottom: 240,
        },
      );
      let pixels = f.output.as_mut().unwrap().to_rgba().unwrap();
      assert_eq!(
        &pixels[(100 * 320 + 100) * 4..(100 * 320 + 100) * 4 + 3],
        &[20, 40, 70]
      );
      assert!(f.state().shared.resources.first_paint_logged);
      assert!(!unsafe { IsWindowVisible(f.hwnd) }.as_bool());
    }
  }
  assert!(f.response.try_recv().is_err());
}

#[test]
fn too_small_drag_shows_the_hint_and_the_next_press_dismisses_it() {
  let mut f = Fixture::new();
  f.state().hint = crate::overlay::selection_hint::SelectionHint::new("Too small");
  f.state().shared.controller.cursor_pos = (100.0, 100.0);
  f.state().shared.controller.interaction = SelectionInteraction::FreeDraw {
    start: (100.0, 100.0),
    current: (103.0, 104.0),
  };
  f.send(WM_LBUTTONUP, 0);
  assert!(f.state().hint.active());
  assert!(f.state().hint.frame_rect().is_some());
  assert!(!f.state().shared.controller.rejected_too_small);

  f.send(WM_LBUTTONDOWN, 0);
  assert!(!f.state().hint.active());
  assert!(f.state().hint.frame_rect().is_none());
}

#[test]
fn pressing_a_drawing_closes_the_grab_hand_without_waiting_for_set_cursor() {
  use crate::screenshot::annotations::{Annotation, Tool};
  use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
  let mut f = Fixture::new();
  let rect = Rect {
    x: 30.0,
    y: 30.0,
    w: 200.0,
    h: 140.0,
  };
  f.state().shared.controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  f.state().shared.controller.overlay_state = OverlayState::OnHold { rect };
  let mut editor = crate::overlay::screenshot::editor::AnnotationEditor::new("en".into());
  editor.annotations.push(Annotation {
    id: 1,
    tool: Tool::Rectangle,
    points: vec![(60, 60), (140, 120)],
    color: [255, 0, 0],
    width: 6,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: vec![],
    pen_nodes: vec![],
    label: None,
  });
  f.state().editor = Some(editor);
  // S selects drawings, then hover the top edge, clear of the mid-edge resize handle.
  f.send(WM_KEYDOWN, 0x53);
  let edge = LPARAM((80 | (60 << 16)) as isize);
  let hwnd = f.hwnd;
  // SAFETY: synchronous dispatch to this thread's hidden fixture; no global input.
  let send = |msg| unsafe { SendMessageW(hwnd, msg, None, Some(edge)) };
  send(WM_MOUSEMOVE);
  let state = f.state();
  let open = state
    .editor
    .as_mut()
    .unwrap()
    .tool_cursor(&state.shared.controller)
    .unwrap()
    .expect("grab hand over a drawing");

  // WM_SETCURSOR is never sent here, just as Windows withholds it under capture.
  send(WM_LBUTTONDOWN);
  let state = f.state();
  let closed = state
    .editor
    .as_mut()
    .unwrap()
    .tool_cursor(&state.shared.controller)
    .unwrap()
    .expect("closed hand while moving");
  assert_ne!(open, closed);
  // SAFETY: read this thread's current cursor; no pointer state changes.
  assert_eq!(unsafe { GetCursor() }, closed, "press closes the hand");
  send(WM_MOUSEMOVE);
  assert_eq!(unsafe { GetCursor() }, closed, "drag keeps it closed");

  send(WM_LBUTTONUP);
  assert_eq!(unsafe { GetCursor() }, open, "release reopens it");
  // SAFETY: drop any capture left by this thread's hidden fixture.
  if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() } == f.hwnd {
    unsafe { ReleaseCapture() }.unwrap();
  }
}

#[test]
fn selecting_again_from_a_drawing_tool_keeps_drawings_stays_in_the_editor_and_undoes() {
  use crate::screenshot::annotations::{Annotation, Tool};
  let mut f = Fixture::new();
  let old = Rect {
    x: 30.0,
    y: 30.0,
    w: 200.0,
    h: 140.0,
  };
  f.state().shared.controller.interaction = SelectionInteraction::Confirmed {
    rect: old,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  f.state().shared.controller.overlay_state = OverlayState::OnHold { rect: old };
  let mut editor = crate::overlay::screenshot::editor::AnnotationEditor::new("en".into());
  editor.annotations.push(Annotation {
    id: 1,
    tool: Tool::Rectangle,
    points: vec![(60, 60), (140, 120)],
    color: [255, 0, 0],
    width: 6,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: vec![],
    pen_nodes: vec![],
    label: None,
  });
  f.state().editor = Some(editor);
  f.send(WM_KEYDOWN, 0x50);
  let hwnd = f.hwnd;
  let at = |x: isize, y: isize| LPARAM(x | (y << 16));
  // SAFETY: synchronous dispatch to this thread's hidden fixture; no global input.
  let send = |msg, point| unsafe { SendMessageW(hwnd, msg, None, Some(point)) };

  // Pen is active; a press outside the crop starts a new selection instead of being swallowed.
  send(WM_MOUSEMOVE, at(250, 190));
  send(WM_LBUTTONDOWN, at(250, 190));
  assert!(!matches!(
    f.state().shared.controller.interaction,
    SelectionInteraction::Confirmed { .. }
  ));
  assert_eq!(f.state().editor.as_ref().unwrap().annotations.len(), 1);
  send(WM_MOUSEMOVE, at(310, 235));
  send(WM_LBUTTONUP, at(310, 235));

  // Released without Alt, yet the editor stays open on the new crop with the drawing kept.
  assert!(f.response.try_recv().is_err(), "no capture finished");
  let SelectionInteraction::Confirmed { rect, .. } = f.state().shared.controller.interaction else {
    panic!("the new crop lands back in the editor");
  };
  assert!(rect.x >= 249.0 && rect.w >= 50.0, "{rect:?}");
  assert_eq!(f.state().editor.as_ref().unwrap().annotations.len(), 1);

  // The new crop is one undoable step back to the old one.
  let back = (XBUTTON1 as usize) << 16;
  assert_eq!(f.send(WM_XBUTTONUP, back), LRESULT(1));
  let state = f.state();
  if let Some(editor) = &mut state.editor {
    editor.sync_crop(&mut state.shared.controller);
  }
  let SelectionInteraction::Confirmed { rect, .. } = f.state().shared.controller.interaction else {
    panic!("undo keeps a crop");
  };
  assert_eq!(rect.to_x1y1x2y2(), old.to_x1y1x2y2());
  assert_eq!(f.state().editor.as_ref().unwrap().annotations.len(), 1);
}

#[test]
fn right_click_deletes_the_drawing_under_it_cancels_one_in_progress_and_otherwise_resets() {
  use crate::screenshot::annotations::{Annotation, Tool};
  let mut f = Fixture::new();
  let crop = Rect {
    x: 30.0,
    y: 30.0,
    w: 200.0,
    h: 140.0,
  };
  f.state().shared.controller.interaction = SelectionInteraction::Confirmed {
    rect: crop,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  f.state().shared.controller.overlay_state = OverlayState::OnHold { rect: crop };
  let mut editor = crate::overlay::screenshot::editor::AnnotationEditor::new("en".into());
  editor.annotations.push(Annotation {
    id: 1,
    tool: Tool::Rectangle,
    points: vec![(60, 60), (140, 120)],
    color: [255, 0, 0],
    width: 6,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: vec![],
    pen_nodes: vec![],
    label: None,
  });
  f.state().editor = Some(editor);
  f.send(WM_KEYDOWN, 0x50);
  let hwnd = f.hwnd;
  let at = |x: isize, y: isize| LPARAM(x | (y << 16));
  // SAFETY: synchronous dispatch to this thread's hidden fixture; no global input.
  let send = |msg, point| unsafe { SendMessageW(hwnd, msg, None, Some(point)) };
  let drawings = |f: &mut Fixture| f.state().editor.as_ref().unwrap().annotations.len();

  // A pen stroke in progress is cancelled, leaving the existing drawing alone.
  send(WM_MOUSEMOVE, at(180, 140));
  send(WM_LBUTTONDOWN, at(180, 140));
  send(WM_MOUSEMOVE, at(200, 150));
  send(WM_RBUTTONUP, at(200, 150));
  send(WM_LBUTTONUP, at(200, 150));
  assert_eq!(drawings(&mut f), 1, "the stroke was dropped, not committed");
  assert!(f.state().shared.controller.interaction.is_confirmed_idle());

  // Right-clicking the rectangle's edge deletes it, and undo brings it back.
  send(WM_RBUTTONUP, at(60, 90));
  assert_eq!(drawings(&mut f), 0);
  assert!(f.state().shared.controller.interaction.is_confirmed_idle());
  assert_eq!(f.send(WM_XBUTTONUP, (XBUTTON1 as usize) << 16), LRESULT(1));
  assert_eq!(drawings(&mut f), 1);

  // Empty canvas keeps the old behavior: back to window snapping.
  send(WM_RBUTTONUP, at(200, 70));
  assert!(matches!(
    f.state().shared.controller.interaction,
    SelectionInteraction::Idle { .. }
  ));
  assert!(f.response.try_recv().is_err());
}

#[test]
fn the_edit_hint_waits_for_the_first_frame_shows_while_snapping_and_leaves_on_confirm() {
  let mut f = Fixture::new();
  f.state().shared.controller.edit_hint = Some("Hold Alt to edit");
  f.state().edit_hint = crate::overlay::edit_hint::EditHint::new(Some("Hold Alt to edit"));
  f.state().shared.controller.interaction = SelectionInteraction::Idle {
    snapped_window: None,
  };
  f.send(WM_MOUSEMOVE, 0);
  assert!(
    f.state().edit_hint.frame_rect().is_none(),
    "never before the first frame"
  );

  f.state().shared.resources.first_paint_logged = true;
  f.send(WM_MOUSEMOVE, 0);
  assert!(
    f.state().edit_hint.frame_rect().is_some(),
    "a snap click accepts Alt, so the hint shows before any press"
  );

  let rect = Rect {
    x: 30.0,
    y: 30.0,
    w: 200.0,
    h: 140.0,
  };
  f.state().shared.controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: true,
    moving: None,
    resizing: None,
  };
  f.send(WM_MOUSEMOVE, 0);
  assert!(f.state().edit_hint.frame_rect().is_none());
  assert!(!f.state().edit_hint.active());
}

#[test]
fn clicking_empty_space_shows_no_hint() {
  let mut f = Fixture::new();
  f.state().hint = crate::overlay::selection_hint::SelectionHint::new("Too small");
  f.state().shared.controller.interaction = SelectionInteraction::PendingDecision {
    press_pos: (100.0, 100.0),
    snapped_window: None,
  };
  f.send(WM_LBUTTONUP, 0);
  assert!(!f.state().hint.active());
}

#[test]
fn m_shows_the_lens_before_any_selection_and_a_held_key_does_not_flip_it() {
  // The fixture has no editor, like GIF and video selection.
  let mut f = Fixture::new();
  f.state().shared.controller.cursor_pos = (100.0, 80.0);
  f.send(WM_KEYDOWN, 0x4D);
  let lens = f.state().magnifier.lens().expect("lens at the pointer");
  assert_eq!(lens.focus, (100, 80));

  // SAFETY: synchronous dispatch on the window owner thread, no global input injection.
  unsafe {
    SendMessageW(
      f.hwnd,
      WM_KEYDOWN,
      Some(WPARAM(0x4D)),
      Some(LPARAM(1 << 30)),
    )
  };
  assert!(
    f.state().magnifier.lens().is_some(),
    "auto-repeat is ignored"
  );

  // WHEEL_DELTA in the high word of wParam, one notch up then one down.
  f.send(WM_MOUSEWHEEL, 120 << 16);
  assert_eq!(f.state().magnifier.lens().expect("zoomed").zoom, 7);
  f.send(WM_MOUSEWHEEL, (-120i16 as u16 as usize) << 16);
  assert_eq!(f.state().magnifier.lens().expect("zoomed").zoom, 5);

  f.send(WM_KEYDOWN, 0x4D);
  assert!(f.state().magnifier.lens().is_none());
  assert!(
    !f.state().magnifier.needs_update(),
    "the old lens area was cleared"
  );
}

#[test]
fn enter_finalizes_once_then_destruction_releases_state_and_sender() {
  let mut f = Fixture::new();
  let rect = Rect {
    x: 20.0,
    y: 30.0,
    w: 100.0,
    h: 80.0,
  };
  f.state().shared.controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  f.state().shared.controller.overlay_state = OverlayState::OnHold { rect };
  f.send(WM_KEYDOWN, 0x0d);
  let (selection, finalized) = f.result().unwrap();
  assert!(finalized);
  assert_eq!(
    (selection.x, selection.y, selection.width, selection.height),
    (20, 30, 100, 80)
  );
  assert_eq!(Arc::strong_count(&f.pixels), 1);
  assert!(f.response.try_recv().is_err());
  assert!(!unsafe { IsWindow(Some(f.hwnd)) }.as_bool());
}

fn pointer(x: i32, y: i32) -> LPARAM {
  LPARAM(((y as u16 as isize) << 16) | x as u16 as isize)
}

fn picker() -> Fixture {
  let mut f = Fixture::new();
  f.state().pick = true;
  f.state().magnifier = crate::overlay::magnifier::Magnifier::picker();
  f
}

#[test]
fn the_color_picker_returns_the_pixel_under_a_press_without_snapping_or_dragging() {
  let mut f = picker();
  // SAFETY: synchronous dispatch on the fixture's own hidden window.
  unsafe {
    SendMessageW(f.hwnd, WM_MOUSEMOVE, Some(WPARAM(0)), Some(pointer(57, 91)));
  }
  assert_eq!(f.state().shared.controller.cursor_pos, (57.0, 91.0));
  assert!(
    matches!(
      f.state().shared.controller.overlay_state,
      OverlayState::Idle
    ),
    "no window snapping while picking"
  );
  assert!(
    f.state().magnifier.lens().is_some(),
    "the lens is always on"
  );
  f.send(WM_KEYDOWN, 0x4D);
  assert!(
    f.state().magnifier.lens().is_some(),
    "M does not hide the picker lens"
  );
  // SAFETY: as above.
  unsafe {
    SendMessageW(
      f.hwnd,
      WM_LBUTTONDOWN,
      Some(WPARAM(1)),
      Some(pointer(58, 92)),
    );
  }
  let (selection, finalized) = f.result().unwrap();
  assert!(finalized);
  assert_eq!(
    (selection.x, selection.y, selection.width, selection.height),
    (58, 92, 1, 1)
  );
  assert!(!unsafe { IsWindow(Some(f.hwnd)) }.as_bool());
}

#[test]
fn enter_and_space_pick_where_the_pointer_is_and_escape_or_right_click_cancel() {
  for key in [0x0D, 0x20] {
    let mut f = picker();
    f.state().shared.controller.cursor_pos = (319.6, -4.0);
    f.send(WM_KEYDOWN, key);
    let (selection, _) = f.result().unwrap();
    assert_eq!(
      (selection.x, selection.y, selection.width, selection.height),
      (319, 0, 1, 1),
      "kept inside the capture"
    );
  }
  for (msg, key) in [(WM_KEYDOWN, 0x1B), (WM_RBUTTONUP, 0)] {
    let f = picker();
    f.send(msg, key);
    assert!(f.result().is_none());
    assert_eq!(Arc::strong_count(&f.pixels), 1);
  }
}

#[test]
fn the_color_picker_shows_the_frozen_screen_undimmed() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  let mut f = picker();
  let output = ScreenshotBitmap::from_rgba(&[0; 320 * 240 * 4], 320, 240).unwrap();
  let dc = output.dc;
  f.output = Some(output);
  f.state().shared.resources.offscreen_dc = dc;
  f.state().shared.resources.screenshot =
    Some(ScreenshotBitmap::from_rgba(&f.pixels, 320, 240).unwrap());
  let presentation = ScreenshotBitmap::from_rgba(&[0; 320 * 240 * 4], 320, 240).unwrap();
  render_selection_frame::<Adapter>(
    f.state(),
    presentation.dc,
    windows::Win32::Foundation::RECT {
      left: 0,
      top: 0,
      right: 320,
      bottom: 240,
    },
  );
  let pixels = f.output.as_mut().unwrap().to_rgba().unwrap();
  for (x, y) in [(5, 5), (300, 220), (160, 120)] {
    let at = (y * 320 + x) * 4;
    assert_eq!(
      &pixels[at..at + 3],
      &[20, 40, 70],
      "({x}, {y}) is not dimmed"
    );
  }
}

#[test]
fn picker_keys_pick_cancel_and_nudge_one_or_ten_pixels() {
  assert_eq!(pick_key(0x0D, false), Some(PickKey::Pick));
  assert_eq!(pick_key(0x20, true), Some(PickKey::Pick));
  assert_eq!(pick_key(0x1B, false), Some(PickKey::Cancel));
  assert_eq!(pick_key(0x25, false), Some(PickKey::Nudge(-1, 0)));
  assert_eq!(pick_key(0x27, true), Some(PickKey::Nudge(10, 0)));
  assert_eq!(pick_key(0x26, true), Some(PickKey::Nudge(0, -10)));
  assert_eq!(pick_key(0x28, false), Some(PickKey::Nudge(0, 1)));
  assert_eq!(pick_key(0x4D, false), None, "no magnifier toggle");
  assert_eq!(pick_key(0x41, false), None);
}

#[test]
fn escape_and_topology_change_cancel_without_returning_stale_selection() {
  for msg in [WM_KEYDOWN, WM_DISPLAYCHANGE] {
    let f = Fixture::new();
    f.send(msg, 0x1b);
    assert!(f.result().is_none());
    assert_eq!(Arc::strong_count(&f.pixels), 1);
    assert!(f.response.try_recv().is_err());
  }
}

#[test]
fn modifiers_timer_and_nonconfirming_messages_keep_hidden_window_alive() {
  let mut f = Fixture::new();
  assert_eq!(f.send(WM_ERASEBKGND, 0), LRESULT(1));
  for (key, field) in [(0x10, 0), (0x12, 2)] {
    f.send(WM_SYSKEYDOWN, key);
    let m = &f.state().shared.controller.modifiers;
    assert!(match field {
      0 => m.shift,
      _ => m.alt,
    });
    f.send(WM_SYSKEYUP, key);
    let m = &f.state().shared.controller.modifiers;
    assert!(!match field {
      0 => m.shift,
      _ => m.alt,
    });
  }
  f.send(WM_TIMER, crate::overlay::selection_core::SNAP_ANIM_TIMER_ID);
  f.send(WM_CANCELMODE, 0);
  f.send(WM_CAPTURECHANGED, 0);
  f.send(WM_RBUTTONUP, 0);
  assert!(f.response.try_recv().is_err());
  assert!(!unsafe { IsWindowVisible(f.hwnd) }.as_bool());
  f.send(WM_DISPLAYCHANGE, 0);
  assert!(f.result().is_none());
}
