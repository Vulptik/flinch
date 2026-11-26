use super::*;
use windows::Win32::UI::WindowsAndMessaging::*;

fn controller() -> SelectionControllerState {
  let mut controller = SelectionControllerState::new(1000, 800, 0, 0);
  controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 50.,
      y: 50.,
      w: 800.,
      h: 500.,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  controller.cursor_pos = (400., 300.);
  controller
}
#[test]
fn the_toolbar_grip_shows_an_open_hand_that_closes_while_dragging() {
  let controller = controller();
  let mut editor = AnnotationEditor::new("en".into());
  let rect = editor.toolbar_rect(
    &controller,
    AnnotationEditor::confirmed(&controller).unwrap(),
  );
  let open = editor.grip_hand().unwrap();
  let (open_hand, closed_hand) = hands(&editor);
  assert_eq!(open, open_hand, "over the grip");
  editor.toolbar_drag = Some((rect, (16, 22)));
  assert_eq!(editor.grip_hand().unwrap(), closed_hand, "while dragging");
  editor.toolbar_drag = None;
  assert_eq!(
    editor.grip_hand().unwrap(),
    open_hand,
    "open again on release"
  );
  assert_ne!(open_hand, system(IDC_HAND), "never the pointing hand");
}

/// The editor's owned grab hands, created on first use.
fn hands(editor: &AnnotationEditor) -> (HCURSOR, HCURSOR) {
  let cursors = editor.cursors.as_ref().expect("grab cursors created");
  (cursors.open_hand(), cursors.closed_hand())
}
fn system(id: windows::core::PCWSTR) -> HCURSOR {
  // SAFETY: load a borrowed cursor without changing the active cursor or pointer capture.
  unsafe { LoadCursorW(None, id).unwrap() }
}

#[test]
fn drawing_tools_use_the_windows_crosshair_and_toolbar_the_arrow_without_setting_the_pointer() {
  let mut controller = controller();
  let mut editor = AnnotationEditor::new("en".into());
  assert!(editor.tool_cursor(&controller).unwrap().is_none());
  // Every drawing tool uses the Windows crosshair; width and color never change it.
  for (tool, cursor) in [
    (Tool::Pen, IDC_CROSS),
    (Tool::Arrow, IDC_CROSS),
    (Tool::Rectangle, IDC_CROSS),
    (Tool::Ellipse, IDC_CROSS),
    (Tool::Blur, IDC_CROSS),
    (Tool::Text, IDC_IBEAM),
    (Tool::Marker, IDC_CROSS),
  ] {
    editor.tool = tool;
    assert_eq!(
      editor.tool_cursor(&controller).unwrap(),
      Some(system(cursor))
    );
  }
  editor.tool = Tool::Pen;
  editor.width = 10;
  editor.color = 2;
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(system(IDC_CROSS))
  );
  editor.tool = Tool::Select;
  editor.object_mode = true;
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(system(IDC_ARROW))
  );
  editor.object_mode = false;
  editor.tool = Tool::Pen;
  let rect = editor.toolbar_rect(
    &controller,
    AnnotationEditor::confirmed(&controller).unwrap(),
  );
  controller.cursor_pos = ((rect.left + 10) as f64, (rect.top + 10) as f64);
  assert!(editor.grip_cursor(&controller));
  assert!(editor.toolbar_cursor(&controller));
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(system(IDC_ARROW))
  );
  editor.width_open = true;
  let panel = editor.picker_rect(
    &controller,
    AnnotationEditor::confirmed(&controller).unwrap(),
  );
  controller.cursor_pos = ((panel.left + 10) as f64, (panel.top + 10) as f64);
  assert!(editor.toolbar_cursor(&controller));
  // The selection host gives toolbar_cursor priority over tool_cursor for popovers.
  // Outside the crop every tool starts a new selection, so the selection crosshair applies.
  editor.width_open = false;
  controller.cursor_pos = (950., 750.);
  assert!(editor.tool_cursor(&controller).unwrap().is_none());
  editor.object_mode = true;
  assert!(editor.tool_cursor(&controller).unwrap().is_none());
  editor.object_mode = false;
  editor.tool = Tool::Select;
  assert!(editor.tool_cursor(&controller).unwrap().is_none());
}

#[test]
fn transformed_shape_and_text_handles_choose_the_matching_native_axis() {
  let mut editor = AnnotationEditor::new("en".into());
  let mut controller = controller();
  let shape = Annotation {
    id: 1,
    tool: Tool::Rectangle,
    points: vec![(100, 100), (300, 200)],
    color: COLORS[0],
    width: 3,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: vec![],
    pen_nodes: vec![],
    label: None,
  };
  editor.annotations.push(shape.clone());
  editor.selected = Some(1);
  editor.object_mode = true;
  for (handle, cursor) in [(0, IDC_SIZENWSE), (1, IDC_SIZENS), (3, IDC_SIZEWE)] {
    editor.geometry_handle = Some(handle);
    editor.moving = Some(ObjectMove {
      index: 0,
      before: shape.clone(),
      preview: shape.clone(),
      start: (100, 100),
    });
    assert_eq!(
      editor.tool_cursor(&controller).unwrap(),
      Some(system(cursor))
    );
  }
  // Rotating holds the drawing with a closed hand.
  editor.geometry_handle = Some(9);
  let rotating = editor.tool_cursor(&controller).unwrap();
  assert_eq!(rotating, Some(hands(&editor).1));
  editor.geometry_handle = Some(16);
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(system(IDC_CROSS))
  );
  // Moving the drawing also closes the hand; letting go over it opens it again.
  editor.geometry_handle = None;
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(hands(&editor).1)
  );
  editor.moving = None;
  controller.cursor_pos = (150., 100.);
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(hands(&editor).0)
  );
  controller.cursor_pos = (400., 300.);
  editor.annotations[0].tool = Tool::Text;
  editor.annotations[0].points = vec![(100, 100)];
  editor.annotations[0].label = Some(Label::Text(
    text_data("Native text".into(), 24, 250).unwrap(),
  ));
  editor.resizing_text = true;
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(system(IDC_SIZEWE))
  );
  editor.resizing_text = false;
  editor.object_mode = false;
  editor.tool = Tool::Text;
  controller.cursor_pos = (110., 110.);
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(hands(&editor).0),
    "text can be grabbed and moved"
  );
  assert_ne!(hands(&editor).0, system(IDC_SIZEALL));
}

#[test]
fn a_drawing_tool_shows_the_open_hand_over_its_own_kind_only() {
  let mut controller = controller();
  let mut editor = AnnotationEditor::new("en".into());
  editor.annotations.push(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 1,
    tool: Tool::Arrow,
    points: vec![(200, 300), (500, 300)],
    color: COLORS[0],
    width: 3,
  });
  controller.cursor_pos = (350.0, 300.0);
  editor.tool = Tool::Arrow;
  let cursor = editor.tool_cursor(&controller).unwrap();
  assert_eq!(
    cursor,
    Some(hands(&editor).0),
    "the arrow tool can grab arrows"
  );
  editor.tool = Tool::Rectangle;
  assert_eq!(
    editor.tool_cursor(&controller).unwrap(),
    Some(system(IDC_CROSS)),
    "other tools draw over it"
  );
}
