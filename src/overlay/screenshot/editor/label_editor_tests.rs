use super::layout::*;
use super::*;
use crate::overlay::selection::SelectionInteraction;
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::Label;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{HDC, HFONT};
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;
struct Parent(HWND);

#[test]
fn pen_and_arrow_double_click_nodes_drag_delete_and_escape_are_transactional() {
  // SAFETY: hidden fixture window and synthetic coordinates stay on this test thread.
  let parent = Parent(
    unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        0,
        0,
        800,
        600,
        None,
        None,
        None,
        None,
      )
    }
    .expect("hidden path parent"),
  );
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 50.0,
      y: 50.0,
      w: 700.0,
      h: 400.0,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.tool = Tool::Select;
  editor.object_mode = true;
  let original = Annotation {
    id: 1,
    tool: Tool::Pen,
    points: vec![(100, 100), (150, 150), (200, 100)],
    color: COLORS[0],
    width: 3,
    label: None,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
  };
  editor.annotations.push(original.clone());
  editor.selected = Some(1);
  controller.cursor_pos = (125.0, 125.0);
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.pointer_up(&controller, parent.0));
  assert!(editor.pointer_down(&controller, parent.0));
  assert_eq!(editor.node_mode, Some(1));
  assert_eq!(
    editor.annotations[0], original,
    "entering node mode preserves captured samples"
  );
  controller.cursor_pos = (175.0, 125.0);
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.pointer_up(&controller, parent.0));
  assert!(editor.pointer_down(&controller, parent.0));
  let inserted = editor.annotations[0].clone();
  assert_eq!(inserted.points.len(), 4);
  assert_eq!(inserted.pen_nodes, vec![0, 1, 2, 3]);
  assert!(editor.pointer_down(&controller, parent.0));
  assert_eq!(editor.geometry_handle, Some(18));
  controller.cursor_pos = (175.0, 165.0);
  assert!(editor.pointer_up(&controller, parent.0));
  let moved = editor.annotations[0].clone();
  assert_eq!(moved.points[2], (175, 165));
  editor.undo();
  assert_eq!(editor.annotations[0], inserted);
  editor.redo();
  assert_eq!(editor.annotations[0], moved);
  assert!(editor.node_indices.contains(&2));
  editor.selected_node = Some(2);
  assert!(editor.keydown(&controller, parent.0, 0x2e));
  assert_eq!(editor.annotations[0].points.len(), 3);
  editor.undo();
  assert_eq!(editor.annotations[0], moved);
  assert!(editor.pointer_down(&controller, parent.0));
  controller.cursor_pos = (175.0, 180.0);
  editor.update_move(&controller);
  assert!(editor.keydown(&controller, parent.0, 0x1b));
  assert_eq!(editor.annotations[0], moved);
  assert!(editor.node_mode.is_some());
  assert!(editor.keydown(&controller, parent.0, 0x1b));
  assert!(editor.node_mode.is_none());
  let mut arrow = original.clone();
  arrow.id = 2;
  arrow.tool = Tool::Arrow;
  arrow.points = vec![(300, 100), (400, 160), (500, 100)];
  editor.annotations.push(arrow.clone());
  editor.selected = Some(2);
  controller.cursor_pos = crate::screenshot::arrow::Arrow::from_annotation(&arrow)
    .expect("arrow")
    .at(0.25);
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.pointer_up(&controller, parent.0));
  assert!(editor.pointer_down(&controller, parent.0));
  assert_eq!(editor.annotations[1].points.len(), 4);
  assert_eq!(editor.annotations[1].curve_controls.len(), 4);
  let inserted_arrow = editor.annotations[1].clone();
  assert!(editor.keydown(&controller, parent.0, 0x2e));
  assert_eq!(editor.annotations[1].points.len(), 3);
  editor.undo();
  assert_eq!(editor.annotations[1], inserted_arrow);
}
unsafe extern "system" fn text_preview_parent(
  hwnd: HWND,
  msg: u32,
  wparam: windows::Win32::Foundation::WPARAM,
  lparam: LPARAM,
  id: usize,
  _: usize,
) -> windows::Win32::Foundation::LRESULT {
  if msg == WM_CTLCOLOREDIT
    && let Some(result) =
      super::super::text_field::background(HDC(wparam.0 as *mut _), HWND(lparam.0 as *mut _))
  {
    return result;
  }
  // SAFETY: subclass only serves this test-owned parent; default processing preserves native input behavior.
  unsafe {
    if msg == WM_NCDESTROY {
      assert!(
        windows::Win32::UI::Shell::RemoveWindowSubclass(hwnd, Some(text_preview_parent), id)
          .as_bool()
      );
    }
    windows::Win32::UI::Shell::DefSubclassProc(hwnd, msg, wparam, lparam)
  }
}
#[test]
fn live_text_formatting_resize_and_double_click_preserve_transactions() {
  use windows::Win32::UI::Controls::{EM_GETSEL, EM_SETSEL};
  // SAFETY: hidden window and all synthetic messages belong only to this test thread.
  let parent = Parent(
    unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        0,
        0,
        1200,
        900,
        None,
        None,
        None,
        None,
      )
    }
    .expect("hidden parent"),
  );
  // SAFETY: subclass applies the production child colors only to this hidden test parent.
  assert!(
    unsafe {
      windows::Win32::UI::Shell::SetWindowSubclass(parent.0, Some(text_preview_parent), 2, 0)
    }
    .as_bool()
  );
  let mut controller = SelectionControllerState::new(1200, 900, 0, 0);
  let rect = Rect {
    x: 20.0,
    y: 20.0,
    w: 1100.0,
    h: 600.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  controller.cursor_pos = (80.0, 80.0);
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  editor
    .start_text(&controller, parent.0, None)
    .expect("start text");
  assert!(!editor.text_error, "empty input is a valid draft");
  let field = editor.text_edit.as_ref().expect("edit").field.hwnd;
  let mut initial_input = RECT::default();
  // SAFETY: read only the test's live native child.
  unsafe { GetClientRect(field, &mut initial_input) }.expect("initial input size");
  // SAFETY: only bounded content/selection on the test-owned EDIT.
  unsafe {
    SetWindowTextW(field, w!("Café 日本語\r\nSecond line")).expect("input");
    SendMessageW(
      field,
      EM_SETSEL,
      Some(windows::Win32::Foundation::WPARAM(2)),
      Some(LPARAM(6)),
    );
  }
  editor.text_changed(&controller, parent.0, Some(field));
  assert!(
    editor.annotations.is_empty(),
    "preview must not mutate committed document"
  );
  assert!(editor.text_edit.as_ref().expect("draft").preview.is_some());
  let click =
    |editor: &mut AnnotationEditor, controller: &mut SelectionControllerState, cell: RECT| {
      controller.cursor_pos = (
        (cell.left + cell.right) as f64 / 2.0,
        (cell.top + cell.bottom) as f64 / 2.0,
      );
      assert!(editor.pointer_down(controller, parent.0));
      assert!(editor.pointer_up(controller, parent.0));
    };
  let toolbar = toolbar_rect(&controller, rect, &[]);
  click(
    &mut editor,
    &mut controller,
    toolbar_button(toolbar, 11, UNSCALED),
  );
  let panel = editor.picker_rect(&controller, rect);
  click(&mut editor, &mut controller, font_cell(panel, 4, UNSCALED));
  assert_eq!(editor.text_size, 48);
  click(
    &mut editor,
    &mut controller,
    toolbar_button(toolbar, 11, UNSCALED),
  );
  let panel = editor.picker_rect(&controller, rect);
  click(
    &mut editor,
    &mut controller,
    style_menu_button(panel, UNSCALED),
  );
  assert!(editor.text_style_open);
  let panel = editor.picker_rect(&controller, rect);
  click(
    &mut editor,
    &mut controller,
    appearance_cell(panel, 1, UNSCALED),
  );
  click(
    &mut editor,
    &mut controller,
    toolbar_button(toolbar, 7, UNSCALED),
  );
  let panel = editor.picker_rect(&controller, rect);
  click(&mut editor, &mut controller, color_cell(panel, 2, UNSCALED));
  assert_eq!(
    editor.text_edit.as_ref().expect("same edit").field.hwnd,
    field
  );
  assert_eq!(editor.color, 2);
  let mut grown_input = RECT::default();
  // SAFETY: read the same child after multiline/font updates.
  unsafe { GetClientRect(field, &mut grown_input) }.expect("grown input size");
  assert!(
    grown_input.bottom > initial_input.bottom,
    "input grows with multiline text"
  );
  let (mut start, mut end) = (0u32, 0u32);
  // SAFETY: pointers to live local selection outputs for this thread-owned child.
  unsafe {
    SendMessageW(
      field,
      EM_GETSEL,
      Some(windows::Win32::Foundation::WPARAM(
        &mut start as *mut _ as usize,
      )),
      Some(LPARAM(&mut end as *mut _ as isize)),
    )
  };
  assert_eq!(
    (start, end),
    (2, 6),
    "formatting must retain native caret/selection"
  );
  let edit = editor.text_edit.as_ref().expect("draft");
  assert!(
    matches!(&edit.preview.as_ref().expect("preview").label, Some(Label::Text(data)) if data.size == 48 && data.appearance == TextAppearance::Outline)
  );
  let width = edit.wrap_width;
  let handle = text_width_handle(editor.text_selection_bounds().expect("text bounds"));
  controller.cursor_pos = (
    (handle.left + handle.right) as f64 / 2.0,
    (handle.top + handle.bottom) as f64 / 2.0,
  );
  assert!(editor.pointer_down(&controller, parent.0));
  controller.cursor_pos.0 += 100.0;
  assert!(editor.pointer_up(&controller, parent.0));
  assert_eq!(
    editor.text_edit.as_ref().expect("resized draft").wrap_width,
    width + 100
  );
  let handle = text_width_handle(editor.text_selection_bounds().expect("resized bounds"));
  controller.cursor_pos = (
    (handle.left + handle.right) as f64 / 2.0,
    (handle.top + handle.bottom) as f64 / 2.0,
  );
  assert!(editor.pointer_down(&controller, parent.0));
  let x = controller.cursor_pos.0 as i32 + 50;
  let y = controller.cursor_pos.1 as i32;
  let cache_before_resize = editor.cache_key(&controller);
  assert!(editor.pointer_move(
    &mut controller,
    parent.0,
    LPARAM(((y << 16) | (x & 0xffff)) as isize)
  ));
  assert_eq!(
    editor.cache_key(&controller),
    cache_before_resize,
    "native resizing must reuse the clean scene cache"
  );
  editor.cancel_gesture();
  editor.text_changed(&controller, parent.0, None);
  assert_eq!(
    editor
      .text_edit
      .as_ref()
      .expect("cancelled width gesture")
      .wrap_width,
    width + 100
  );
  // SAFETY: hidden test parent owns the cancelled synthetic width gesture.
  unsafe { ReleaseCapture() }.expect("release cancelled capture");
  let bounds = editor.text_selection_bounds().expect("draft bounds");
  controller.cursor_pos = ((bounds.left + 2) as f64, (bounds.top + 2) as f64);
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.text_edit.as_ref().expect("draft").moving.is_some());
  let mut child_before_move = RECT::default();
  // SAFETY: read geometry of the test-owned child before the preview gesture.
  unsafe { GetWindowRect(field, &mut child_before_move) }.expect("native geometry");
  let x = controller.cursor_pos.0 as i32 + 30;
  let y = controller.cursor_pos.1 as i32 + 40;
  assert!(editor.pointer_move(
    &mut controller,
    parent.0,
    LPARAM(((y << 16) | (x & 0xffff)) as isize)
  ));
  assert_eq!(
    editor.text_edit.as_ref().expect("moving draft").point,
    (110, 120)
  );
  assert_eq!(
    editor.text_edit.as_ref().expect("same field").field.hwnd,
    field
  );
  let mut child_during_move = RECT::default();
  // SAFETY: read-only query proves native child repositioning is deferred.
  unsafe { GetWindowRect(field, &mut child_during_move) }.expect("native geometry during drag");
  assert_eq!(
    (
      child_during_move.left,
      child_during_move.top,
      child_during_move.right,
      child_during_move.bottom
    ),
    (
      child_before_move.left,
      child_before_move.top,
      child_before_move.right,
      child_before_move.bottom
    )
  );
  assert_eq!(
    editor
      .text_edit
      .as_ref()
      .expect("moving preview")
      .preview
      .as_ref()
      .expect("text preview")
      .points,
    vec![(110, 120)]
  );
  editor.cancel_gesture();
  editor.text_changed(&controller, parent.0, None);
  assert_eq!(
    editor.text_edit.as_ref().expect("restored draft").point,
    (80, 80)
  );
  // SAFETY: release capture belonging to this hidden test parent.
  unsafe { ReleaseCapture() }.expect("release cancelled move");
  // Render the live scene and actual native input into an optional visual QA artifact.
  if let Ok(folder) = std::env::var("FLINCH_ANNOTATION_PREVIEW_DIR") {
    use crate::overlay::gdi::bitmap::ScreenshotBitmap;
    use windows::Win32::Graphics::Gdi::{
      BitBlt, ClientToScreen, DEFAULT_GUI_FONT, GetStockObject, SRCCOPY, ScreenToClient,
    };
    let source: Vec<u8> = (0..1200 * 900)
      .flat_map(|i| {
        if (i / 1200 / 80 + i % 1200 / 80) % 2 == 0 {
          [45, 55, 70, 255]
        } else {
          [210, 215, 220, 255]
        }
      })
      .collect();
    // SAFETY: collapse selection on the test-owned child to exercise native style backing.
    unsafe {
      SendMessageW(
        field,
        EM_SETSEL,
        Some(windows::Win32::Foundation::WPARAM(0)),
        Some(LPARAM(0)),
      )
    };
    let mut style_pixels = Vec::new();
    for (appearance, name) in [
      (TextAppearance::Plain, "text-editor-live.png"),
      (TextAppearance::Outline, "text-editor-outline.png"),
      (TextAppearance::Background, "text-editor-background.png"),
    ] {
      editor.text_appearance = appearance;
      editor.text_changed(&controller, parent.0, None);
      let mut bitmap = ScreenshotBitmap::from_rgba(&source, 1200, 900).expect("scene");
      // SAFETY: stock font is borrowed for the test-owned DC.
      let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
      editor.draw(&controller, bitmap.dc, font);
      assert!(!editor.render_failed);
      let mut native_rect = RECT::default();
      // SAFETY: read live child geometry on this thread.
      unsafe { GetClientRect(field, &mut native_rect) }.expect("field size");
      let native = ScreenshotBitmap::from_rgba(
        &vec![255; (native_rect.right * native_rect.bottom * 4) as usize],
        native_rect.right as u32,
        native_rect.bottom as u32,
      )
      .expect("native target");
      // SAFETY: print only this test-owned child into its live test DC.
      unsafe {
        SendMessageW(
          field,
          WM_PRINTCLIENT,
          Some(windows::Win32::Foundation::WPARAM(native.dc.0 as usize)),
          Some(LPARAM((PRF_CLIENT | PRF_ERASEBKGND) as isize)),
        )
      };
      let mut position = windows::Win32::Foundation::POINT::default();
      // SAFETY: child and parent belong to this thread; convert geometry without moving windows.
      assert!(
        unsafe { ClientToScreen(field, &mut position) }.as_bool(),
        "child origin"
      );
      // SAFETY: converting the same live thread-owned parent coordinates.
      assert!(
        unsafe { ScreenToClient(parent.0, &mut position) }.as_bool(),
        "parent origin"
      );
      // SAFETY: test-owned DCs have matching validated dimensions.
      unsafe {
        BitBlt(
          bitmap.dc,
          position.x,
          position.y,
          native_rect.right,
          native_rect.bottom,
          Some(native.dc),
          0,
          0,
          SRCCOPY,
        )
      }
      .expect("native input preview");
      let folder = std::path::PathBuf::from(&folder);
      std::fs::create_dir_all(&folder).expect("visual directory");
      let pixels = bitmap.to_rgba().expect("pixels");
      style_pixels.push(pixels.clone());
      image::save_buffer(
        folder.join(name),
        &pixels,
        1200,
        900,
        image::ColorType::Rgba8,
      )
      .expect("live PNG");
      editor.width_open = true;
      let mut bitmap = ScreenshotBitmap::from_rgba(&source, 1200, 900).expect("picker scene");
      editor.draw(&controller, bitmap.dc, font);
      // SAFETY: composite the same native child into the picker QA image.
      unsafe {
        BitBlt(
          bitmap.dc,
          position.x,
          position.y,
          native_rect.right,
          native_rect.bottom,
          Some(native.dc),
          0,
          0,
          SRCCOPY,
        )
      }
      .expect("picker input preview");
      image::save_buffer(
        folder.join("text-format-picker.png"),
        &bitmap.to_rgba().expect("pixels"),
        1200,
        900,
        image::ColorType::Rgba8,
      )
      .expect("picker PNG");
      editor.width_open = false;
    }
    assert_ne!(
      style_pixels[0], style_pixels[1],
      "native outline is visible"
    );
    assert_ne!(
      style_pixels[0], style_pixels[2],
      "native background is visible"
    );
  }
  editor.complete_text(parent.0, true, Some(field));
  assert_eq!(editor.annotations.len(), 1);
  let committed = editor.annotations[0].clone();
  editor.undo();
  assert!(
    editor.annotations.is_empty(),
    "whole draft is one undo step"
  );
  editor.redo();
  assert_eq!(editor.annotations, vec![committed.clone()]);
  editor
    .start_text(&controller, parent.0, Some(committed.clone()))
    .expect("reopen");
  editor.text_size = 64;
  editor.text_appearance = TextAppearance::Background;
  editor.color = 4;
  editor.text_changed(&controller, parent.0, None);
  editor.complete_text(parent.0, false, None);
  assert_eq!(
    editor.annotations[0], committed,
    "cancel restores all style/content changes"
  );
  let handle = text_width_handle(editor.text_selection_bounds().expect("selection"));
  controller.cursor_pos = (
    (handle.left + handle.right) as f64 / 2.0,
    (handle.top + handle.bottom) as f64 / 2.0,
  );
  assert!(editor.pointer_down(&controller, parent.0));
  controller.cursor_pos.0 += 120.0;
  assert!(editor.pointer_up(&controller, parent.0));
  assert_ne!(editor.annotations[0], committed);
  editor.undo();
  assert_eq!(editor.annotations[0], committed);
  controller.cursor_pos = (90.0, 95.0);
  assert!(editor.pointer_down(&controller, parent.0));
  controller.cursor_pos = (150.0, 125.0);
  assert!(editor.pointer_up(&controller, parent.0));
  assert_eq!(
    editor.annotations[0].points[0],
    (140, 110),
    "text drags from its original anchor"
  );
  editor.undo();
  assert_eq!(editor.annotations[0], committed);
  controller.cursor_pos = (90.0, 95.0);
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.pointer_up(&controller, parent.0));
  assert!(editor.text_edit.is_none());
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.text_edit.is_some(), "second click reopens text");
  let old_child = field;
  editor.text_changed(&controller, parent.0, Some(old_child));
  editor.complete_text(parent.0, false, None);
  let mut rotated = editor.annotations[0].clone();
  rotated.rotation_millidegrees = 30_000;
  editor.annotations[0] = rotated.clone();
  editor
    .start_text(&controller, parent.0, Some(rotated))
    .expect("reopen rotated text");
  editor.text_changed(&controller, parent.0, None);
  editor.complete_text(parent.0, true, None);
  assert_eq!(
    editor.annotations[0].rotation_millidegrees, 30_000,
    "upright native editing preserves saved rotation"
  );
}
impl Drop for Parent {
  fn drop(&mut self) {
    // SAFETY: hidden test parent belongs to the current test thread.
    if let Err(error) = unsafe { DestroyWindow(self.0) } {
      tracing::warn!(%error,"Test parent cleanup failed");
    }
  }
}
#[test]
fn toolbar_clicks_dismiss_other_pickers_and_apply_tools_immediately() {
  // SAFETY: hidden test-owned window; no desktop input injection.
  let parent = Parent(
    unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        0,
        0,
        1000,
        800,
        None,
        None,
        None,
        None,
      )
    }
    .expect("hidden parent"),
  );
  let mut controller = SelectionControllerState::new(1000, 800, 0, 0);
  let rect = Rect {
    x: 100.0,
    y: 100.0,
    w: 600.0,
    h: 300.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  for picker in [7, 11] {
    for action in ACTIONS.into_iter().chain(std::iter::once(-1)) {
      let mut editor = AnnotationEditor::new("en".into());
      editor.color_open = picker == 7;
      editor.width_open = picker == 11;
      let toolbar = toolbar_rect(&controller, rect, &editor.monitors);
      let m = editor.metrics(&controller, rect);
      let cell = if action == -1 {
        inset(toolbar, 1)
      } else {
        toolbar_button(toolbar, action, m)
      };
      controller.cursor_pos = if action == -1 {
        ((cell.left + m.grip) as f64, cell.top as f64)
      } else {
        (
          (cell.left + cell.right) as f64 / 2.0,
          (cell.top + cell.bottom) as f64 / 2.0,
        )
      };
      assert!(editor.pointer_down(&controller, parent.0));
      assert_eq!(
        editor.color_open,
        action == 7 && picker != 7,
        "picker {picker}, action {action}"
      );
      assert_eq!(
        editor.width_open,
        action == 11 && picker != 11,
        "picker {picker}, action {action}"
      );
      if (0..=4).contains(&action) {
        assert_eq!(editor.tool, TOOLS[action as usize]);
      }
      if action == 8 {
        assert_eq!(
          editor.tool,
          crate::screenshot::annotations::Tool::Highlighter
        );
        assert_eq!(editor.active_tool_button(), 8);
      }
      assert!(editor.pointer_up(&controller, parent.0));
    }
  }
}

#[test]
fn text_commit_cancel_reopen_empty_delete_and_style_undo_are_transactional() {
  // SAFETY: hidden native parent only; test edits do not display a desktop overlay.
  let parent = Parent(
    unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        0,
        0,
        600,
        400,
        None,
        None,
        None,
        None,
      )
    }
    .expect("parent"),
  );
  let mut editor = AnnotationEditor::new("en".into());
  let mut controller = SelectionControllerState::new(600, 400, 0, 0);
  controller.cursor_pos = (50.0, 70.0);
  controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 20.0,
      y: 20.0,
      w: 550.0,
      h: 340.0,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  editor
    .start_text(&controller, parent.0, None)
    .expect("open new text");
  let field = editor.text_edit.as_ref().expect("field").field.hwnd;
  // SAFETY: field belongs to this test; set bounded test content.
  unsafe { SetWindowTextW(field, w!("Café 日本語\r\nSecond line")) }.expect("write text");
  editor.complete_text(parent.0, true, Some(field));
  assert_eq!(editor.annotations.len(), 1);
  let original = editor.annotations[0].clone();
  editor
    .start_text(&controller, parent.0, Some(original.clone()))
    .expect("reopen");
  assert!(editor.keydown(&controller, parent.0, 0x1b));
  assert_eq!(editor.annotations[0], original);
  editor.selected = Some(original.id);
  editor.text_size = 32;
  editor.edit_selected_style();
  assert!(matches!(&editor.annotations[0].label,Some(Label::Text(t)) if t.size==32));
  editor.undo();
  assert_eq!(editor.annotations[0], original);
  editor.redo();
  editor
    .start_text(&controller, parent.0, Some(editor.annotations[0].clone()))
    .expect("reopen for delete");
  let field = editor.text_edit.as_ref().expect("field").field.hwnd;
  // SAFETY: replace only the test field's text, leaving the committed document intact until completion.
  unsafe { SetWindowTextW(field, w!("")) }.expect("blank text");
  editor.complete_text(parent.0, true, None);
  assert!(editor.annotations.is_empty());
  editor.undo();
  assert_eq!(editor.annotations.len(), 1);
  editor.clear();
  controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 20.0,
      y: 20.0,
      w: 10.0,
      h: 10.0,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  controller.cursor_pos = (25.0, 25.0);
  editor.monitors.clear();
  editor
    .start_text(&controller, parent.0, None)
    .expect("tiny crop text field");
  let mut client = RECT::default();
  // SAFETY: query only the test's live child client rectangle.
  unsafe {
    GetClientRect(
      editor.text_edit.as_ref().expect("field").field.hwnd,
      &mut client,
    )
  }
  .expect("field bounds");
  assert!(
    client.right - client.left >= 118,
    "tiny crop must not collapse the typing field"
  );
  assert!(
    client.bottom - client.top >= 32,
    "empty text field keeps room for a compact line"
  );
  editor.complete_text(parent.0, false, None);
  assert!(editor.text_edit.is_none());
}
#[test]
fn markers_keep_numbers_through_delete_and_restore_counter_on_undo() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.place_marker((80, 80));
  editor.place_marker((120, 80));
  assert_eq!(editor.annotations[1].label, Some(Label::Number(2)));
  editor.selected = Some(editor.annotations[1].id);
  editor.delete_selected();
  editor.place_marker((160, 80));
  assert_eq!(editor.annotations[1].label, Some(Label::Number(3)));
  editor.undo();
  editor.redo();
  assert_eq!(editor.annotations[1].label, Some(Label::Number(3)));
  editor.undo();
  editor.place_marker((180, 80));
  assert_eq!(editor.annotations[1].label, Some(Label::Number(3)));
  editor.clear();
  editor.place_marker((60, 60));
  assert_eq!(editor.annotations[0].label, Some(Label::Number(1)));
  for _ in 0..998 {
    editor.annotations.clear();
    editor.place_marker((60, 60));
  }
  assert_eq!(editor.history.next_marker(), None);
  let before = editor.annotations.clone();
  editor.place_marker((70, 70));
  assert_eq!(editor.annotations, before);
}
