use super::*;

#[test]
fn rectangle_radius_handle_cancels_and_commits_as_one_undo_step() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.draft = Some(Annotation {
    id: 0,
    tool: Tool::Rectangle,
    points: vec![(100, 100), (300, 200)],
    color: COLORS[0],
    width: 3,
    label: None,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
  });
  editor.finish_gesture();
  let original = editor.annotations[0].clone();
  editor.selected = Some(original.id);
  editor.object_mode = true;
  assert_eq!(editor.geometry_handle_hit((118.0, 118.0)), Some((0, 8)));
  assert_eq!(editor.geometry_handle_hit((100.0, 100.0)), Some((0, 0)));
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  for cancel in [true, false] {
    editor.geometry_handle = Some(8);
    editor.moving = Some(ObjectMove {
      index: 0,
      before: original.clone(),
      preview: original.clone(),
      start: (118, 118),
    });
    for position in [(125.0, 125.0), (140.0, 140.0)] {
      controller.cursor_pos = position;
      editor.update_move(&controller);
      assert_eq!(editor.annotations[0], original);
    }
    if cancel {
      editor.cancel_gesture();
      assert_eq!(editor.annotations[0], original);
    } else {
      editor.finish_gesture();
      let rounded = editor.annotations[0].clone();
      assert_eq!(rounded.corner_radius, 50);
      assert_eq!(rounded.points, original.points);
      editor.undo();
      assert_eq!(editor.annotations[0], original);
      editor.redo();
      assert_eq!(editor.annotations[0], rounded);
    }
    assert_eq!(editor.geometry_handle, None);
  }
}

#[test]
fn delete_selected_preserves_other_objects_and_undo_restores_z_order() {
  let mut editor = AnnotationEditor::new("en".into());
  for x in [20, 40] {
    editor.draft = Some(Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 0,
      tool: Tool::Pen,
      points: vec![(x, 50)],
      color: COLORS[0],
      width: 3,
    });
    editor.finish_gesture();
  }
  let original = editor.annotations.clone();
  editor.selected = Some(original[0].id);
  editor.delete_selected();
  assert_eq!(editor.annotations, [original[1].clone()]);
  editor.undo();
  assert_eq!(editor.annotations, original);
  editor.redo();
  assert_eq!(editor.annotations, [original[1].clone()]);
  editor.delete_selected();
  assert_eq!(
    editor.annotations.len(),
    1,
    "empty selection cannot delete another drawing"
  );
}

#[test]
fn drawing_tools_consume_crop_boundary_clicks() {
  use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
  };
  struct Parent(HWND);
  impl Drop for Parent {
    fn drop(&mut self) {
      // SAFETY: the hidden test parent is owned exclusively by this thread.
      if let Err(error) = unsafe { DestroyWindow(self.0) } {
        tracing::warn!(%error, "Test parent cleanup failed");
      }
    }
  }
  // SAFETY: built-in class and hidden test window; no global input injection.
  let parent = Parent(
    unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        windows::core::w!("STATIC"),
        windows::core::w!(""),
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
    .expect("hidden test parent"),
  );
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 50.0,
      y: 50.0,
      w: 300.0,
      h: 200.0,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  controller.cursor_pos = (50.0, 50.0);
  let mut editor = AnnotationEditor::new("en".into());
  for tool in [
    Tool::Pen,
    Tool::Arrow,
    Tool::Rectangle,
    Tool::Ellipse,
    Tool::Text,
    Tool::Marker,
  ] {
    editor.tool = tool;
    assert!(
      editor.pointer_down(&controller, parent.0),
      "drawing tool must not delegate to crop handles"
    );
    assert!(editor.draft.is_none());
    assert_eq!(crop_tuple(&controller), (50.0, 50.0, 300.0, 200.0));
  }
  editor.tool = Tool::Select;
  editor.object_mode = true;
  assert!(editor.pointer_down(&controller, parent.0));
  editor.object_mode = false;
  assert!(
    !editor.pointer_down(&controller, parent.0),
    "crop tool delegates to the shared crop controls"
  );
}

#[test]
fn crop_history_and_group_moves_preserve_anchors_and_transaction_order() {
  let mut editor = AnnotationEditor::new("en".into());
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  let original_crop = Rect {
    x: 20.0,
    y: 20.0,
    w: 300.0,
    h: 200.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect: original_crop,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  editor.sync_crop(&mut controller);
  for y in [60, 100] {
    editor.draft = Some(Annotation {
      id: 0,
      tool: Tool::Pen,
      points: vec![(50, y), (100, y)],
      color: COLORS[0],
      width: 3,
      label: None,
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    });
    editor.finish_gesture();
  }
  let original = editor.annotations.clone();
  if let SelectionInteraction::Confirmed { rect, .. } = &mut controller.interaction {
    rect.x += 200.0;
    rect.w += 100.0;
  }
  editor.sync_crop(&mut controller);
  assert_eq!(
    editor.annotations, original,
    "crop adjustment never translates drawings"
  );
  editor.undo();
  editor.sync_crop(&mut controller);
  assert_eq!(crop_tuple(&controller), (20.0, 20.0, 300.0, 200.0));
  editor.redo();
  editor.sync_crop(&mut controller);
  assert_eq!(crop_tuple(&controller), (220.0, 20.0, 400.0, 200.0));
  editor.selected = Some(original[0].id);
  editor.toggle_selection(original[1].id);
  assert_eq!(editor.group_selection, vec![original[0].id, original[1].id]);
  editor.toggle_selection(original[1].id);
  assert_eq!(editor.group_selection, vec![original[0].id]);
  editor.toggle_selection(original[1].id);
  let moves: Vec<_> = original
    .iter()
    .enumerate()
    .map(|(index, a)| ObjectMove {
      index,
      before: a.clone(),
      preview: a.clone(),
      start: (50, 60),
    })
    .collect();
  let mut moves = moves.into_iter();
  editor.moving = moves.next();
  editor.group_move = moves.collect();
  controller.cursor_pos = (80.0, 100.0);
  editor.update_move(&controller);
  editor.cancel_gesture();
  assert_eq!(editor.annotations, original);
  assert!(
    editor.group_move.is_empty(),
    "cancel discards every group preview"
  );
  editor.moving = Some(ObjectMove {
    index: 0,
    before: original[0].clone(),
    preview: original[0].clone(),
    start: (50, 60),
  });
  editor.group_move = vec![ObjectMove {
    index: 1,
    before: original[1].clone(),
    preview: original[1].clone(),
    start: (50, 60),
  }];
  editor.update_move(&controller);
  editor.finish_gesture();
  assert_eq!(editor.annotations[0].points[0], (80, 100));
  assert_eq!(editor.annotations[1].points[0], (80, 140));
  editor.undo();
  assert_eq!(
    editor.annotations, original,
    "one undo restores the whole group"
  );
  editor.redo();
  let moved = editor.annotations.clone();
  editor.delete_selected();
  assert!(editor.annotations.is_empty());
  editor.undo();
  assert_eq!(editor.annotations, moved);
  editor.undo();
  assert_eq!(editor.annotations, original);
  editor.undo();
  editor.sync_crop(&mut controller);
  assert_eq!(crop_tuple(&controller), (20.0, 20.0, 300.0, 200.0));
}

#[test]
fn object_moves_cancel_or_commit_once_and_style_undo_updates_controls() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Pen,
    points: vec![(50, 50), (80, 50)],
    color: COLORS[0],
    width: 3,
  });
  editor.finish_gesture();
  let original = editor.annotations[0].clone();
  editor.selected = Some(original.id);
  let mut controller = SelectionControllerState::new(400, 300, 0, 0);
  controller.cursor_pos = (150.0, 140.0);
  editor.moving = Some(ObjectMove {
    index: 0,
    before: original.clone(),
    preview: original.clone(),
    start: (50, 50),
  });
  editor.update_move(&controller);
  assert_eq!(
    editor.annotations[0], original,
    "draft move cannot mutate committed pixels"
  );
  editor.cancel_gesture();
  assert_eq!(editor.annotations[0], original);
  editor.moving = Some(ObjectMove {
    index: 0,
    before: original.clone(),
    preview: original.clone(),
    start: (50, 50),
  });
  editor.update_move(&controller);
  editor.finish_gesture();
  assert_eq!(editor.annotations[0].points[0], (150, 140));
  editor.undo();
  assert_eq!(editor.annotations[0], original);
  editor.redo();
  assert_eq!(editor.annotations[0].points[0], (150, 140));
  editor.width = 10;
  editor.edit_selected_style();
  editor.undo();
  assert_eq!(editor.width, 3);
  editor.redo();
  assert_eq!(editor.width, 10);
}

#[test]
fn object_hit_testing_uses_outlines_and_topmost_order() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.annotations = vec![
    Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 1,
      tool: Tool::Rectangle,
      points: vec![(20, 20), (200, 100)],
      color: COLORS[0],
      width: 3,
    },
    Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 2,
      tool: Tool::Pen,
      points: vec![(10, 20), (220, 20)],
      color: COLORS[0],
      width: 3,
    },
  ];
  assert_eq!(editor.hit_drawing((60.0, 20.0)), Some(1));
  assert_eq!(editor.hit_drawing((20.0, 70.0)), Some(0));
  assert_eq!(editor.hit_drawing((100.0, 60.0)), None);
  let ellipse = Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 3,
    tool: Tool::Ellipse,
    points: vec![(0, 0), (200, 2)],
    color: COLORS[0],
    width: 3,
  };
  assert!(
    ellipse.hit((190.0, 6.0)),
    "thin ellipses need geometric distance, not radial normalization"
  );
  assert!(!ellipse.hit((190.0, 30.0)));
}

#[test]
fn releasing_shift_restores_free_draft_endpoint() {
  let mut controller = SelectionControllerState::new(400, 300, 0, 0);
  controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 20.0,
      y: 20.0,
      w: 300.0,
      h: 200.0,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  controller.cursor_pos = (170.0, 100.0);
  controller.modifiers.shift = true;
  let mut editor = AnnotationEditor::new("en".into());
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Rectangle,
    points: vec![(50, 50), (50, 50)],
    color: COLORS[0],
    width: 3,
  });
  editor.update_draft(&controller);
  assert_eq!(editor.draft.as_ref().expect("draft").points[1], (170, 170));
  controller.modifiers.shift = false;
  editor.update_draft(&controller);
  assert_eq!(editor.draft.as_ref().expect("draft").points[1], (170, 100));
  assert!(
    !editor.history.can_undo(),
    "pointer changes cannot add history entries"
  );
}

#[test]
fn cancelled_and_degenerate_gestures_preserve_redo_and_stable_ids() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Pen,
    points: vec![(2, 2)],
    color: COLORS[0],
    width: 3,
  });
  editor.finish_gesture();
  let first_id = editor.annotations[0].id;
  let revision = editor.document_revision;
  editor.undo();
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Arrow,
    points: vec![(3, 3), (3, 3)],
    color: COLORS[0],
    width: 3,
  });
  editor.finish_gesture();
  assert!(editor.history.can_redo());
  assert_eq!(editor.document_revision, revision + 1);
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Pen,
    points: vec![(5, 5)],
    color: COLORS[0],
    width: 3,
  });
  editor.cancel_gesture();
  editor.redo();
  assert_eq!(editor.annotations[0].id, first_id);
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Pen,
    points: vec![(8, 8)],
    color: COLORS[0],
    width: 3,
  });
  editor.finish_gesture();
  assert_ne!(editor.annotations[1].id, first_id);
}

#[test]
fn undo_redo_branch_keeps_only_completed_drawings() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Pen,
    points: vec![(2, 2)],
    color: COLORS[0],
    width: 3,
  });
  editor.finish_gesture();
  editor.undo();
  assert!(editor.annotations.is_empty());
  editor.redo();
  assert_eq!(editor.annotations.len(), 1);
  editor.undo();
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Arrow,
    points: vec![(3, 3), (9, 9)],
    color: COLORS[0],
    width: 3,
  });
  editor.finish_gesture();
  assert!(!editor.history.can_redo());
  editor.clear();
  assert!(editor.annotations.is_empty());
  assert_eq!(editor.tool, Tool::Select);
}

/// A hidden owner-thread window for gestures that take pointer capture; no input is injected.
struct HiddenParent(HWND);
impl HiddenParent {
  fn new() -> Self {
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WINDOW_EX_STYLE, WS_POPUP};
    // SAFETY: built-in class and hidden test window; no global input injection.
    Self(
      unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE::default(),
          windows::core::w!("STATIC"),
          windows::core::w!(""),
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
      .expect("hidden test parent"),
    )
  }
}
impl Drop for HiddenParent {
  fn drop(&mut self) {
    // SAFETY: the hidden test parent is owned exclusively by this thread.
    if let Err(error) = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0) } {
      tracing::warn!(%error, "Test parent cleanup failed");
    }
  }
}

#[test]
fn a_drawing_tool_selects_moves_and_deletes_its_own_kind() {
  let parent = HiddenParent::new();
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 50.0,
      y: 50.0,
      w: 600.0,
      h: 400.0,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.tool = Tool::Arrow;
  controller.cursor_pos = (100.0, 100.0);
  assert!(editor.pointer_down(&controller, parent.0));
  controller.cursor_pos = (300.0, 100.0);
  assert!(editor.pointer_up(&controller, parent.0));
  assert_eq!(editor.annotations.len(), 1);
  let arrow = editor.annotations[0].id;
  assert_eq!(editor.selected, Some(arrow), "a new drawing stays selected");
  assert!(
    editor
      .geometry_handle_hit((300.0, 100.0))
      .is_some_and(|(index, _)| index == 0),
    "its handles work with the arrow tool"
  );

  // Pressing the arrow with the arrow tool grabs it instead of drawing another.
  editor.color = 3;
  // Away from the middle handle, which bends the arrow instead.
  controller.cursor_pos = (150.0, 100.0);
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.draft.is_none() && editor.moving.is_some());
  assert!(editor.geometry_handle.is_none(), "a move, not a reshape");
  assert_eq!(
    editor.color, 0,
    "the toolbar shows the grabbed drawing's color"
  );
  controller.cursor_pos = (150.0, 140.0);
  assert!(editor.pointer_up(&controller, parent.0));
  assert_eq!(editor.annotations.len(), 1);
  assert_eq!(editor.annotations[0].points[0], (100, 140));
  editor.undo();
  assert_eq!(editor.annotations[0].points[0], (100, 100), "one undo step");
  editor.redo();

  // Another tool draws over it, and pressing elsewhere lets the selection go.
  editor.tool = Tool::Rectangle;
  editor.selected = None;
  assert!(editor.hit_own_drawing((200.0, 140.0)).is_none());
  assert!(editor.geometry_handle_hit((300.0, 140.0)).is_none());
  controller.cursor_pos = (200.0, 140.0);
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.draft.is_some() && editor.moving.is_none());
  controller.cursor_pos = (260.0, 200.0);
  assert!(editor.pointer_up(&controller, parent.0));
  assert_eq!(editor.annotations.len(), 2);
  let rectangle = editor.annotations[1].id;
  assert_eq!(editor.selected, Some(rectangle));
  controller.cursor_pos = (500.0, 300.0);
  assert!(editor.pointer_down(&controller, parent.0));
  assert_eq!(editor.selected, None, "a press on empty space deselects");
  editor.cancel_gesture();

  // Delete removes the selected drawing with its own tool, as with Select.
  editor.selected = Some(rectangle);
  assert!(editor.keydown(&controller, parent.0, 0x2E));
  assert_eq!(editor.annotations.len(), 1);
  assert_eq!(editor.annotations[0].id, arrow);
}
