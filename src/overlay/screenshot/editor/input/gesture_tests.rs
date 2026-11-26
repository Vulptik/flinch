use super::super::layout::{
  appearance_cell, error_badge_rect, font_cell, style_menu_button, toolbar_button, width_row,
};
use super::*;
use crate::overlay::gdi::window::{
  class_name_for_thread, create_hud_window, register_overlay_class, unregister_overlay_class,
};
use crate::overlay::selection::{Rect, SelectionInteraction};
use windows::Win32::Foundation::{LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, DestroyWindow, IsWindowVisible};

unsafe extern "system" fn hidden(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
  // SAFETY: this fixture owns no message-state pointer and never displays its HWND.
  unsafe { DefWindowProcW(hwnd, message, w, l) }
}
struct Window {
  hwnd: HWND,
  class: String,
}
impl Window {
  fn new() -> Self {
    let class = format!("{}-annotation-input", class_name_for_thread());
    register_overlay_class(&class, Some(hidden)).unwrap();
    let hwnd = create_hud_window(
      &class,
      "Hidden annotation input",
      0,
      0,
      1000,
      800,
      std::ptr::null_mut(),
    )
    .unwrap();
    Self { hwnd, class }
  }
}
impl Drop for Window {
  fn drop(&mut self) {
    // SAFETY: destroy only the test-owned, never-shown window on its owner thread.
    unsafe {
      assert!(!IsWindowVisible(self.hwnd).as_bool());
      DestroyWindow(self.hwnd).unwrap();
    }
    unregister_overlay_class(&self.class);
  }
}
fn controller() -> SelectionControllerState {
  let mut state = SelectionControllerState::new(1000, 800, 0, 0);
  state.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 100.,
      y: 100.,
      w: 600.,
      h: 400.,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  state
}
fn center(rect: windows::Win32::Foundation::RECT) -> (f64, f64) {
  (
    (rect.left + rect.right) as f64 / 2.,
    (rect.top + rect.bottom) as f64 / 2.,
  )
}
fn click(
  editor: &mut AnnotationEditor,
  state: &mut SelectionControllerState,
  hwnd: HWND,
  point: (f64, f64),
) {
  state.cursor_pos = point;
  assert!(editor.pointer_down_with(state, hwnd, |_| true));
  assert!(editor.pointer_up_with(state, hwnd, || Ok(())));
}
fn action(
  editor: &mut AnnotationEditor,
  state: &mut SelectionControllerState,
  hwnd: HWND,
  index: i32,
) {
  let toolbar = editor.toolbar_rect(state, AnnotationEditor::confirmed(state).unwrap());
  let m = editor.metrics(state, AnnotationEditor::confirmed(state).unwrap());
  click(
    editor,
    state,
    hwnd,
    center(toolbar_button(toolbar, index, m)),
  );
}
fn position(x: i32, y: i32) -> LPARAM {
  LPARAM(((y as u16 as u32) << 16 | x as u16 as u32) as isize)
}

#[test]
fn inline_text_backdrop_move_and_commit_preserve_unicode_without_showing_or_focusing_the_parent() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use windows::{
    Win32::{
      Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject, HFONT},
      UI::WindowsAndMessaging::SetWindowTextW,
    },
    core::w,
  };
  let window = Window::new();
  let mut state = controller();
  state.cursor_pos = (200., 200.);
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  editor.start_text(&state, window.hwnd, None).unwrap();
  let child = editor.text_edit.as_ref().unwrap().field.hwnd;
  unsafe { SetWindowTextW(child, w!("Café 日本語\r\nSecond line")) }.unwrap();
  editor.text_changed(&state, window.hwnd, Some(child));
  let bitmap =
    ScreenshotBitmap::from_rgba(&[20, 30, 40, 255].repeat(1000 * 800), 1000, 800).unwrap();
  let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
  editor.draw(&state, bitmap.dc, font);
  assert!(!editor.render_failed);
  assert!(
    editor.annotations.is_empty(),
    "text remains a draft until commit"
  );
  let edit = editor.text_edit.as_mut().unwrap();
  edit.moving = Some(((200, 200), edit.point));
  edit.point = (230, 220);
  editor.text_changed(&state, window.hwnd, Some(child));
  editor.draw(&state, bitmap.dc, font);
  assert!(!editor.render_failed);
  assert!(editor.active_bounds().is_none());
  editor.text_edit.as_mut().unwrap().moving = None;
  editor.text_changed(&state, window.hwnd, Some(child));
  editor.complete_text(window.hwnd, true, Some(child));
  assert!(editor.text_edit.is_none());
  assert_eq!(editor.annotations.len(), 1);
  let drawing = &editor.annotations[0];
  assert_eq!(drawing.points, vec![(230, 220)]);
  assert!(
    matches!(&drawing.label,Some(crate::screenshot::annotations::Label::Text(data)) if data.content.contains("Café 日本語") && data.content.contains("Second line"))
  );
  editor.undo();
  assert!(editor.annotations.is_empty());
  editor.redo();
  assert_eq!(editor.annotations.len(), 1);
  assert!(!unsafe { IsWindowVisible(window.hwnd) }.as_bool());
}

#[test]
fn keyboard_undo_redo_delete_and_dismissal_preserve_completed_drawings_and_crop() {
  use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
  struct Keys([u8; 256]);
  impl Drop for Keys {
    fn drop(&mut self) {
      unsafe { SetKeyboardState(&self.0) }.unwrap();
    }
  }
  let mut original_keys = [0; 256];
  unsafe { GetKeyboardState(&mut original_keys) }.unwrap();
  let _keys = Keys(original_keys);
  // Only this fixture thread's input queue changes; no physical input is injected.
  unsafe { SetKeyboardState(&[0; 256]) }.unwrap();
  let window = Window::new();
  let mut state = controller();
  let crop = AnnotationEditor::confirmed(&state).unwrap();
  let mut editor = AnnotationEditor::new("en".into());
  action(&mut editor, &mut state, window.hwnd, 1);
  state.cursor_pos = (160., 160.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  editor.pointer_move(&mut state, window.hwnd, position(240, 240));
  editor.pointer_up_with(&state, window.hwnd, || Ok(()));
  let drawing = editor.annotations[0].clone();
  let mut ctrl = [0; 256];
  ctrl[0x11] = 0x80;
  unsafe { SetKeyboardState(&ctrl) }.unwrap();
  assert!(editor.keydown(&state, window.hwnd, 0x5a));
  assert!(editor.annotations.is_empty());
  assert!(editor.keydown(&state, window.hwnd, 0x59));
  assert_eq!(editor.annotations, vec![drawing.clone()]);
  unsafe { SetKeyboardState(&[0; 256]) }.unwrap();
  editor.color_open = true;
  editor.width_open = true;
  assert!(editor.keydown(&state, window.hwnd, 0x1b));
  assert!(!editor.color_open && !editor.width_open);
  assert!(editor.keydown(&state, window.hwnd, 0x53));
  editor.selected = Some(drawing.id);
  assert!(editor.keydown(&state, window.hwnd, 0x2e));
  assert!(editor.annotations.is_empty());
  editor.undo();
  assert_eq!(editor.annotations, vec![drawing]);
  editor.report_render_failure();
  assert!(editor.keydown(&state, window.hwnd, 0x70));
  assert!(editor.error_details_pinned);
  assert!(editor.keydown(&state, window.hwnd, 0x1b));
  assert!(!editor.error_details_pinned);
  let actual = AnnotationEditor::confirmed(&state).unwrap();
  assert_eq!(
    (actual.x, actual.y, actual.w, actual.h),
    (crop.x, crop.y, crop.w, crop.h)
  );
  assert!(!editor.keydown(
    &SelectionControllerState::new(1000, 800, 0, 0),
    window.hwnd,
    0x25
  ));
}

#[test]
fn mouse_side_buttons_step_history_only_between_gestures() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  action(&mut editor, &mut state, window.hwnd, 1);
  state.cursor_pos = (160., 160.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  editor.pointer_move(&mut state, window.hwnd, position(240, 240));
  editor.pointer_up_with(&state, window.hwnd, || Ok(()));
  let drawing = editor.annotations[0].clone();
  assert!(editor.side_button(&state, window.hwnd, XBUTTON1));
  assert!(editor.annotations.is_empty());
  assert!(editor.side_button(&state, window.hwnd, XBUTTON2));
  assert_eq!(editor.annotations, vec![drawing.clone()]);

  editor.draft = Some(drawing.clone());
  assert!(editor.side_button(&state, window.hwnd, XBUTTON1));
  editor.draft = None;
  editor.moving = Some(ObjectMove {
    index: 0,
    before: drawing.clone(),
    preview: drawing.clone(),
    start: (160, 160),
  });
  assert!(editor.side_button(&state, window.hwnd, XBUTTON1));
  editor.moving = None;
  state.cursor_pos = (200., 200.);
  editor.start_text(&state, window.hwnd, None).unwrap();
  assert!(editor.side_button(&state, window.hwnd, XBUTTON1));
  editor.complete_text(window.hwnd, false, None);
  assert_eq!(editor.annotations, vec![drawing.clone()]);

  assert!(!editor.side_button(&state, window.hwnd, 3));
  assert!(!editor.side_button(
    &SelectionControllerState::new(1000, 800, 0, 0),
    window.hwnd,
    XBUTTON1
  ));
  assert_eq!(editor.annotations, vec![drawing]);
}

#[test]
fn pointer_gestures_commit_real_drawings_and_toolbar_history_without_capturing_global_input() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  for (button, tool) in [
    (1, Tool::Pen),
    (2, Tool::Arrow),
    (3, Tool::Rectangle),
    (4, Tool::Ellipse),
  ] {
    action(&mut editor, &mut state, window.hwnd, button);
    assert_eq!(editor.tool, tool);
    state.cursor_pos = (160., 160.);
    assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
    for (x, y) in [(180, 180), (220, 210), (300, 260)] {
      assert!(editor.pointer_move(&mut state, window.hwnd, position(x, y)));
    }
    assert!(editor.pointer_up_with(&state, window.hwnd, || Ok(())));
    let drawing = editor.annotations.last().unwrap();
    assert_eq!(drawing.tool, tool);
    assert_eq!(drawing.points.first(), Some(&(160, 160)));
    assert_eq!(drawing.points.last(), Some(&(300, 260)));
  }
  let original = editor.annotations.clone();
  action(&mut editor, &mut state, window.hwnd, 5);
  assert_eq!(editor.annotations.len(), original.len() - 1);
  action(&mut editor, &mut state, window.hwnd, 6);
  assert_eq!(editor.annotations, original);
  action(&mut editor, &mut state, window.hwnd, 14);
  click(&mut editor, &mut state, window.hwnd, (400., 300.));
  assert_eq!(editor.annotations.last().unwrap().tool, Tool::Marker);
  assert_eq!(
    editor.annotations.last().unwrap().label,
    Some(crate::screenshot::annotations::Label::Number(1))
  );
  action(&mut editor, &mut state, window.hwnd, 12);
  assert!(editor.object_mode);
  action(&mut editor, &mut state, window.hwnd, 0);
  assert!(!editor.object_mode);
  assert_eq!(editor.tool, Tool::Select);
  state.cursor_pos = (600., 400.);
  assert!(!editor.pointer_down_with(&state, window.hwnd, |_| panic!(
    "crop tool belongs to selection controller"
  )));
  assert!(!editor.pointer_up_with(&state, window.hwnd, || panic!(
    "idle gesture must not release capture"
  )));
}

#[test]
fn the_highlighter_draws_freehand_and_grabs_only_highlighter_strokes() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  action(&mut editor, &mut state, window.hwnd, 1);
  state.cursor_pos = (160., 400.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  assert!(editor.pointer_move(&mut state, window.hwnd, position(300, 400)));
  assert!(editor.pointer_up_with(&state, window.hwnd, || Ok(())));
  let pen = editor.annotations.last().unwrap().clone();

  action(&mut editor, &mut state, window.hwnd, 8);
  assert_eq!(editor.tool, Tool::Highlighter);
  assert_eq!(editor.active_tool_button(), 8);
  state.cursor_pos = (160., 160.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  for (x, y) in [(200, 170), (250, 150), (300, 160)] {
    assert!(editor.pointer_move(&mut state, window.hwnd, position(x, y)));
  }
  assert!(editor.pointer_up_with(&state, window.hwnd, || Ok(())));
  let stroke = editor.annotations.last().unwrap().clone();
  assert_eq!(stroke.tool, Tool::Highlighter);
  assert_eq!(stroke.width, 3);
  assert_eq!(stroke.stroke_width(), 9);
  assert!(stroke.points.len() > 2, "freehand keeps its samples");
  assert_eq!(editor.selected, Some(stroke.id));
  assert_eq!(editor.painted_width(6), 18);

  // Pressing on its own stroke moves it instead of starting another.
  state.cursor_pos = (225., 160.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  assert!(editor.draft.is_none() && editor.moving.is_some());
  assert!(editor.pointer_move(&mut state, window.hwnd, position(225, 220)));
  assert!(editor.pointer_up_with(&state, window.hwnd, || Ok(())));
  assert_eq!(editor.annotations.len(), 2);
  let moved = editor.annotations.last().unwrap();
  assert_eq!(moved.id, stroke.id);
  let start = moved.points[0];
  assert_eq!(
    crate::screenshot::path::world(moved, (start.0 as f64, start.1 as f64)),
    (160., 220.)
  );

  // A pen stroke is not the highlighter's: pressing on it starts a new highlight.
  state.cursor_pos = (230., 400.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  assert!(editor.moving.is_none() && editor.draft.is_some());
  assert!(editor.pointer_up_with(&state, window.hwnd, || Ok(())));
  assert_eq!(editor.annotations.len(), 3);
  assert_eq!(editor.annotations[0], pen);
  assert_eq!(editor.annotations[2].tool, Tool::Highlighter);
}

fn draw_rectangle(
  editor: &mut AnnotationEditor,
  state: &mut SelectionControllerState,
  hwnd: HWND,
) -> Annotation {
  action(editor, state, hwnd, 3);
  state.cursor_pos = (160., 160.);
  assert!(editor.pointer_down_with(state, hwnd, |_| true));
  editor.pointer_move(state, hwnd, position(260, 240));
  editor.pointer_up_with(state, hwnd, || Ok(()));
  editor.annotations.last().unwrap().clone()
}

#[test]
fn dragging_the_toolbar_grip_moves_it_within_the_monitor_and_release_ends_the_drag() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  let rect = AnnotationEditor::confirmed(&state).unwrap();
  let toolbar = editor.toolbar_rect(&state, rect);
  let m = editor.metrics(&state, rect);
  state.cursor_pos = center(toolbar_grip(toolbar, m));
  editor.color_open = true;
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  assert!(editor.toolbar_drag.is_some() && editor.toolbar_press);
  assert!(!editor.color_open, "grabbing the grip closes the pickers");
  assert!(editor.pointer_move(&mut state, window.hwnd, position(300, 700)));
  let moved = editor.toolbar_position.expect("dragged toolbar position");
  assert_ne!(moved, (toolbar.left, toolbar.top));
  // Far past the monitor edge, the toolbar stays inside it.
  assert!(editor.pointer_move(&mut state, window.hwnd, position(-30000, -30000)));
  let (left, top) = editor.toolbar_position.unwrap();
  assert!(left >= 0 && top >= 0, "{left},{top}");
  // A failed capture release is logged; the drag still ends.
  assert!(editor.pointer_up_with(&state, window.hwnd, || Err(windows::core::Error::empty())));
  assert!(editor.toolbar_drag.is_none() && !editor.toolbar_press);
}

#[test]
fn moving_and_widening_inline_text_follow_the_pointer_until_release() {
  let window = Window::new();
  let mut state = controller();
  state.cursor_pos = (200., 200.);
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  editor.start_text(&state, window.hwnd, None).unwrap();
  let start = editor.text_edit.as_ref().unwrap().point;
  editor.text_edit.as_mut().unwrap().moving = Some(((200, 200), start));
  assert!(editor.pointer_move(&mut state, window.hwnd, position(230, 215)));
  assert_eq!(
    editor.text_edit.as_ref().unwrap().point,
    (start.0 + 30, start.1 + 15)
  );
  state.cursor_pos = (240., 220.);
  assert!(!editor.pointer_up_with(&state, window.hwnd, || Ok(())));
  let edit = editor.text_edit.as_ref().unwrap();
  assert!(edit.moving.is_none());
  assert_eq!(edit.point, (start.0 + 40, start.1 + 20));

  let width = edit.wrap_width;
  editor.text_edit.as_mut().unwrap().resizing = Some((240, width));
  assert!(editor.pointer_move(&mut state, window.hwnd, position(340, 220)));
  assert_eq!(editor.text_edit.as_ref().unwrap().wrap_width, width + 100);
  // Narrower than the minimum clamps to it; release keeps the final width.
  state.cursor_pos = (-9000., 220.);
  editor.pointer_up_with(&state, window.hwnd, || Ok(()));
  let edit = editor.text_edit.as_ref().unwrap();
  assert!(edit.resizing.is_none());
  assert_eq!(edit.wrap_width, 64.max(editor.text_size * 2));
  editor.complete_text(window.hwnd, false, None);
}

#[test]
fn the_error_badge_toggles_its_details_even_when_capture_fails() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  editor.report_render_failure();
  let (_, button) = editor.toolbar_error().expect("toolbar error");
  let rect = AnnotationEditor::confirmed(&state).unwrap();
  let toolbar = editor.toolbar_rect(&state, rect);
  let m = editor.metrics(&state, rect);
  state.cursor_pos = center(error_badge_rect(toolbar, button, m));
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| false));
  assert!(editor.error_details_pinned);
  assert!(!editor.toolbar_press, "an uncaptured press is not held");
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  assert!(!editor.error_details_pinned && editor.toolbar_press);
  assert!(editor.pointer_up_with(&state, window.hwnd, || Ok(())));
  // F1 toggles the same details from the keyboard.
  assert!(editor.keydown(&state, window.hwnd, 0x70));
  assert!(editor.error_details_pinned);
}

#[test]
fn text_size_and_style_pickers_apply_to_new_text() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  action(&mut editor, &mut state, window.hwnd, 13);
  assert_eq!(editor.tool, Tool::Text);
  assert!(editor.font_mode());
  action(&mut editor, &mut state, window.hwnd, 11);
  assert!(editor.width_open);
  let rect = AnnotationEditor::confirmed(&state).unwrap();
  let m = editor.metrics(&state, rect);
  let panel = editor.picker_rect(&state, rect);
  click(
    &mut editor,
    &mut state,
    window.hwnd,
    center(font_cell(panel, 5, m)),
  );
  assert_eq!(editor.text_size, FONT_SIZES[5]);
  assert!(!editor.width_open);
  action(&mut editor, &mut state, window.hwnd, 11);
  let panel = editor.picker_rect(&state, rect);
  click(
    &mut editor,
    &mut state,
    window.hwnd,
    center(style_menu_button(panel, m)),
  );
  assert!(editor.text_style_open && editor.width_open);
  let panel = editor.picker_rect(&state, rect);
  click(
    &mut editor,
    &mut state,
    window.hwnd,
    center(appearance_cell(panel, 2, m)),
  );
  assert_eq!(editor.text_appearance, TextAppearance::Background);
  assert!(!editor.width_open);
  // Marker and blur tools are picked from the toolbar too.
  action(&mut editor, &mut state, window.hwnd, 14);
  assert_eq!(editor.tool, Tool::Marker);
  action(&mut editor, &mut state, window.hwnd, 16);
  assert_eq!(editor.tool, Tool::Blur);
}

#[test]
fn shift_click_builds_a_group_whose_style_buttons_are_held_without_opening() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  let first = draw_rectangle(&mut editor, &mut state, window.hwnd);
  action(&mut editor, &mut state, window.hwnd, 4);
  state.cursor_pos = (400., 300.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  editor.pointer_move(&mut state, window.hwnd, position(500, 380));
  editor.pointer_up_with(&state, window.hwnd, || Ok(()));
  let second = editor.annotations.last().unwrap().clone();
  action(&mut editor, &mut state, window.hwnd, 12);
  // A new drawing stays selected; start the group from nothing.
  editor.selected = None;
  state.modifiers.shift = true;
  for point in [(160., 200.), (400., 340.)] {
    state.cursor_pos = point;
    assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
    editor.pointer_up_with(&state, window.hwnd, || Ok(()));
  }
  assert!(
    editor.group_selection.contains(&first.id) && editor.group_selection.contains(&second.id),
    "{:?}",
    editor.group_selection
  );
  // Shift-clicking empty space changes nothing.
  state.cursor_pos = (650., 450.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  editor.pointer_up_with(&state, window.hwnd, || Ok(()));
  assert_eq!(editor.group_selection.len(), 2);
  state.modifiers.shift = false;
  action(&mut editor, &mut state, window.hwnd, 7);
  assert!(!editor.color_open, "a group cannot open the color picker");
  // Clicking empty space in select mode clears the group.
  click(&mut editor, &mut state, window.hwnd, (650., 450.));
  assert!(editor.group_selection.is_empty());
}

#[test]
fn the_drawing_limit_stops_new_shapes_and_text_and_says_so() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  let drawing = draw_rectangle(&mut editor, &mut state, window.hwnd);
  editor.annotations = (0..MAX_ANNOTATIONS as u64)
    .map(|id| Annotation {
      id: id + 1000,
      ..drawing.clone()
    })
    .collect();
  action(&mut editor, &mut state, window.hwnd, 4);
  state.cursor_pos = (600., 450.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  assert!(editor.draft.is_none());
  assert!(matches!(editor.limited, Some(DrawingLimit::Drawings)));
  editor.pointer_up_with(&state, window.hwnd, || Ok(()));
  editor.limited = None;
  action(&mut editor, &mut state, window.hwnd, 13);
  state.cursor_pos = (600., 450.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  assert!(editor.text_edit.is_none());
  assert!(matches!(editor.limited, Some(DrawingLimit::Drawings)));
}

#[test]
fn right_click_cancels_a_gesture_or_deletes_the_drawing_under_the_pointer() {
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  let drawing = draw_rectangle(&mut editor, &mut state, window.hwnd);
  // Empty space and the area outside the crop are left to the selection.
  state.cursor_pos = (650., 450.);
  assert!(!editor.secondary_click(&state, window.hwnd));
  state.cursor_pos = (20., 20.);
  assert!(!editor.secondary_click(&state, window.hwnd));
  // During a new stroke it cancels that stroke only.
  action(&mut editor, &mut state, window.hwnd, 3);
  state.cursor_pos = (400., 300.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  assert!(editor.secondary_click(&state, window.hwnd));
  assert!(editor.draft.is_none());
  editor.pointer_up_with(&state, window.hwnd, || Ok(()));
  assert_eq!(editor.annotations, vec![drawing.clone()]);
  // Over the drawing it is deleted as one undoable step.
  state.cursor_pos = (160., 200.);
  assert!(editor.secondary_click(&state, window.hwnd));
  assert!(editor.annotations.is_empty());
  editor.undo();
  assert_eq!(editor.annotations, vec![drawing]);
  // While typing, right-click belongs to the text field.
  state.cursor_pos = (400., 300.);
  editor.start_text(&state, window.hwnd, None).unwrap();
  assert!(!editor.secondary_click(&state, window.hwnd));
  editor.complete_text(window.hwnd, false, None);
  assert!(!editor.secondary_click(&SelectionControllerState::new(1000, 800, 0, 0), window.hwnd));
}

#[test]
fn escape_cancels_moves_node_editing_and_text_and_enter_finishes_a_stroke() {
  use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
  struct Keys([u8; 256]);
  impl Drop for Keys {
    fn drop(&mut self) {
      unsafe { SetKeyboardState(&self.0) }.unwrap();
    }
  }
  let mut original_keys = [0; 256];
  unsafe { GetKeyboardState(&mut original_keys) }.unwrap();
  let _keys = Keys(original_keys);
  unsafe { SetKeyboardState(&[0; 256]) }.unwrap();
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  let drawing = draw_rectangle(&mut editor, &mut state, window.hwnd);
  editor.moving = Some(ObjectMove {
    index: 0,
    before: drawing.clone(),
    preview: Annotation {
      points: vec![(0, 0), (10, 10)],
      ..drawing.clone()
    },
    start: (160, 160),
  });
  editor.toolbar_press = true;
  assert!(editor.keydown(&state, window.hwnd, 0x1b));
  assert!(editor.moving.is_none() && !editor.toolbar_press);
  assert_eq!(editor.annotations, vec![drawing.clone()]);
  editor.node_mode = Some(drawing.id);
  editor.selected_node = Some(0);
  assert!(editor.keydown(&state, window.hwnd, 0x1b));
  assert!(editor.node_mode.is_none() && editor.selected_node.is_none());
  // Escape discards new text, Enter commits it.
  state.cursor_pos = (400., 300.);
  editor.start_text(&state, window.hwnd, None).unwrap();
  assert!(editor.keydown(&state, window.hwnd, 0x41));
  assert!(editor.keydown(&state, window.hwnd, 0x1b));
  assert!(editor.text_edit.is_none());
  // Enter during a stroke finishes it; arrows then belong to the drawing tool.
  action(&mut editor, &mut state, window.hwnd, 4);
  state.cursor_pos = (400., 300.);
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| true));
  editor.pointer_move(&mut state, window.hwnd, position(480, 360));
  editor.modifiers_changed(&state, window.hwnd);
  assert!(editor.keydown(&state, window.hwnd, 0x25));
  assert!(!editor.keydown(&state, window.hwnd, 0x0d));
  assert!(editor.draft.is_none());
  assert_eq!(editor.annotations.len(), 2);
  // T with a selected text drawing reopens it for editing.
  editor.annotations.clear();
  state.cursor_pos = (300., 300.);
  editor.start_text(&state, window.hwnd, None).unwrap();
  let child = editor.text_edit.as_ref().unwrap().field.hwnd;
  unsafe {
    windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(child, windows::core::w!("Note"))
  }
  .unwrap();
  editor.text_changed(&state, window.hwnd, Some(child));
  editor.complete_text(window.hwnd, true, Some(child));
  editor.selected = Some(editor.annotations[0].id);
  assert!(editor.keydown(&state, window.hwnd, 0x54));
  assert!(editor.text_edit.is_some(), "T reopens the selected text");
  editor.complete_text(window.hwnd, false, None);
}

#[test]
fn pointer_popovers_apply_styles_dismiss_without_drawing_and_failed_grip_capture_never_starts_drag()
{
  let window = Window::new();
  let mut state = controller();
  let mut editor = AnnotationEditor::new("en".into());
  action(&mut editor, &mut state, window.hwnd, 1);
  action(&mut editor, &mut state, window.hwnd, 7);
  assert!(editor.color_open);
  let panel = editor.picker_rect(&state, AnnotationEditor::confirmed(&state).unwrap());
  let m = editor.metrics(&state, AnnotationEditor::confirmed(&state).unwrap());
  click(
    &mut editor,
    &mut state,
    window.hwnd,
    center(color_cell(panel, 5, m)),
  );
  assert_eq!(editor.color, 5);
  assert!(!editor.color_open);
  action(&mut editor, &mut state, window.hwnd, 11);
  let panel = editor.picker_rect(&state, AnnotationEditor::confirmed(&state).unwrap());
  let m = editor.metrics(&state, AnnotationEditor::confirmed(&state).unwrap());
  click(
    &mut editor,
    &mut state,
    window.hwnd,
    center(width_row(panel, 2, m)),
  );
  assert_eq!(editor.width, 10);
  assert!(!editor.width_open);
  action(&mut editor, &mut state, window.hwnd, 7);
  click(&mut editor, &mut state, window.hwnd, (400., 300.));
  assert!(!editor.color_open);
  assert!(editor.annotations.is_empty() && editor.draft.is_none());
  let toolbar = editor.toolbar_rect(&state, AnnotationEditor::confirmed(&state).unwrap());
  let m = editor.metrics(&state, AnnotationEditor::confirmed(&state).unwrap());
  state.cursor_pos = center(toolbar_grip(toolbar, m));
  assert!(editor.pointer_down_with(&state, window.hwnd, |_| false));
  assert!(editor.toolbar_drag.is_none());
  assert!(!editor.toolbar_press);
  assert!(!editor.pointer_up_with(&state, window.hwnd, || panic!(
    "failed capture is not owned"
  )));
}
