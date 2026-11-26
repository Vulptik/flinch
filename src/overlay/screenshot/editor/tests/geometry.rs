use super::*;

#[test]
fn rotated_text_rotation_and_wrapping_preserve_transactions_and_anchor() {
  let mut editor = AnnotationEditor::new("en".into());
  let original = Annotation {
    id: 1,
    tool: Tool::Text,
    points: vec![(100, 100)],
    color: COLORS[0],
    width: 3,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: Some(Label::Text(
      text_data("Wrapping without changing size".into(), 24, 280).expect("text"),
    )),
  };
  editor.annotations.push(original.clone());
  editor.selected = Some(1);
  editor.object_mode = true;
  let handle = crate::screenshot::shape::rotation_handle(&original).expect("rotation handle");
  assert_eq!(editor.geometry_handle_hit(handle), Some((0, 9)));
  let (l, t, r, b) = crate::screenshot::shape::bounds(&original).expect("bounds");
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  for cancel in [true, false] {
    editor.geometry_handle = Some(9);
    editor.moving = Some(ObjectMove {
      index: 0,
      before: original.clone(),
      preview: original.clone(),
      start: (handle.0.round() as i32, handle.1.round() as i32),
    });
    controller.cursor_pos = ((l + r) / 2.0 + 100.0, (t + b) / 2.0);
    editor.update_move(&controller);
    if cancel {
      editor.cancel_gesture();
      assert_eq!(editor.annotations[0], original);
    } else {
      editor.finish_gesture();
      assert_eq!(editor.annotations[0].rotation_millidegrees, 90_000);
      editor.undo();
      assert_eq!(editor.annotations[0], original);
      editor.redo();
    }
  }
  let rotated = editor.annotations[0].clone();
  let bounds = editor.text_selection_bounds().expect("selected bounds");
  let handle = crate::screenshot::shape::rotate(
    &rotated,
    (
      bounds.right as f64,
      (bounds.top + bounds.bottom) as f64 / 2.0,
    ),
    false,
  );
  assert!(editor.text_resize_hit(handle));
  editor.resizing_text = true;
  editor.moving = Some(ObjectMove {
    index: 0,
    before: rotated.clone(),
    preview: rotated.clone(),
    start: (handle.0.round() as i32, handle.1.round() as i32),
  });
  let anchor = crate::screenshot::shape::rotate(&rotated, (100.0, 100.0), false);
  for delta in [120, 80] {
    controller.cursor_pos = (handle.0, handle.1 - delta as f64);
    editor.update_move(&controller);
    let preview = &editor.moving.as_ref().expect("wrapping").preview;
    assert!(
      matches!(&preview.label,Some(Label::Text(data)) if data.size==24 && data.wrap_width==280-delta)
    );
    let point = preview.points[0];
    let fixed = crate::screenshot::shape::rotate(preview, (point.0 as f64, point.1 as f64), false);
    assert!((fixed.0 - anchor.0).hypot(fixed.1 - anchor.1) < 2.0);
  }
  editor.finish_gesture();
  editor.undo();
  assert_eq!(editor.annotations[0], rotated);
}

#[test]
fn text_width_handle_preserves_size_and_cancels_as_one_transaction() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.draft = Some(Annotation {
    id: 0,
    tool: Tool::Text,
    points: vec![(100, 100)],
    color: COLORS[0],
    width: 3,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: Some(Label::Text(
      text_data(
        "Wrapping changes without changing font size".into(),
        24,
        280,
      )
      .expect("text"),
    )),
  });
  let mut text = editor.draft.take().expect("text fixture");
  text.id = 1;
  assert!(
    editor.apply_change(crate::overlay::screenshot::history::Change {
      index: 0,
      before: None,
      after: Some(text),
      counter: None
    })
  );
  let original = editor.annotations[0].clone();
  editor.selected = Some(original.id);
  editor.object_mode = true;
  let bounds = editor.text_selection_bounds().expect("text bounds");
  let center = (
    bounds.right as f64,
    (bounds.top + bounds.bottom) as f64 / 2.0,
  );
  assert!(editor.text_resize_hit((center.0 + 8.0, center.1)));
  assert!(!editor.text_resize_hit((center.0 + 10.0, center.1)));
  editor.group_selection = vec![original.id, 99];
  assert!(!editor.text_resize_hit(center));
  editor.group_selection.clear();
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  for cancel in [true, false] {
    editor.moving = Some(ObjectMove {
      index: 0,
      before: original.clone(),
      preview: original.clone(),
      start: (center.0 as i32, center.1 as i32),
    });
    editor.resizing_text = true;
    controller.cursor_pos = (center.0 - 120.0, center.1);
    editor.update_move(&controller);
    let preview = &editor.moving.as_ref().expect("resize").preview;
    assert!(
      matches!(&preview.label, Some(Label::Text(data)) if data.size == 24 && data.wrap_width == 160)
    );
    assert_eq!(editor.annotations[0], original);
    if cancel {
      editor.cancel_gesture();
      assert_eq!(editor.annotations[0], original);
    } else {
      editor.finish_gesture();
      let resized = editor.annotations[0].clone();
      editor.undo();
      assert_eq!(editor.annotations[0], original);
      editor.redo();
      assert_eq!(editor.annotations[0], resized);
    }
  }
}

#[test]
fn shape_rotation_cancels_and_commits_as_one_undo_step() {
  for tool in [Tool::Rectangle, Tool::Ellipse] {
    let mut editor = AnnotationEditor::new("en".into());
    editor.draft = Some(Annotation {
      id: 0,
      tool,
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
    assert_eq!(editor.geometry_handle_hit((200.0, 72.0)), Some((0, 9)));
    let mut controller = SelectionControllerState::new(800, 600, 0, 0);
    for cancel in [true, false] {
      editor.geometry_handle = Some(9);
      editor.moving = Some(ObjectMove {
        index: 0,
        before: original.clone(),
        preview: original.clone(),
        start: (200, 72),
      });
      controller.cursor_pos = (300.0, 100.0);
      controller.modifiers.shift = true;
      editor.update_move(&controller);
      assert_eq!(editor.annotations[0], original);
      assert_eq!(
        editor
          .moving
          .as_ref()
          .expect("rotation")
          .preview
          .rotation_millidegrees,
        60_000
      );
      controller.modifiers.shift = false;
      editor.update_move(&controller);
      assert_ne!(
        editor
          .moving
          .as_ref()
          .expect("rotation")
          .preview
          .rotation_millidegrees,
        60_000
      );
      if cancel {
        editor.cancel_gesture();
        assert_eq!(editor.annotations[0], original);
      } else {
        editor.finish_gesture();
        let rotated = editor.annotations[0].clone();
        editor.undo();
        assert_eq!(editor.annotations[0], original);
        editor.redo();
        assert_eq!(editor.annotations[0], rotated);
      }
      assert_eq!(editor.geometry_handle, None);
    }
  }
}

#[test]
fn shape_handles_cancel_commit_and_undo_each_drag_as_one_change() {
  for tool in [Tool::Rectangle, Tool::Ellipse] {
    let mut editor = AnnotationEditor::new("en".into());
    editor.draft = Some(Annotation {
      id: 0,
      tool,
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
    assert_eq!(editor.geometry_handle_hit((300.0, 200.0)), Some((0, 4)));
    assert_eq!(editor.hit_drawing((200.0, 150.0)), Some(0));
    let mut controller = SelectionControllerState::new(800, 600, 0, 0);
    for cancel in [true, false] {
      editor.geometry_handle = Some(4);
      editor.moving = Some(ObjectMove {
        index: 0,
        before: original.clone(),
        preview: original.clone(),
        start: (300, 200),
      });
      for position in [(350.0, 250.0), (400.0, 300.0)] {
        controller.cursor_pos = position;
        editor.update_move(&controller);
        assert_eq!(editor.annotations[0], original);
      }
      controller.modifiers.shift = true;
      editor.update_move(&controller);
      assert_eq!(
        editor.moving.as_ref().expect("moving").preview.points,
        vec![(100, 100), (500, 300)]
      );
      controller.modifiers.shift = false;
      editor.update_move(&controller);
      if cancel {
        editor.cancel_gesture();
        assert_eq!(editor.annotations[0], original);
      } else {
        editor.finish_gesture();
        let resized = editor.annotations[0].clone();
        assert_eq!(resized.points, vec![(100, 100), (400, 300)]);
        editor.undo();
        assert_eq!(editor.annotations[0], original);
        editor.redo();
        assert_eq!(editor.annotations[0], resized);
      }
      assert_eq!(editor.geometry_handle, None);
    }
    editor.group_selection = vec![original.id, 99];
    assert_eq!(editor.geometry_handle_hit((400.0, 300.0)), None);
  }
}

#[test]
fn arrow_handles_cancel_commit_and_undo_each_drag_as_one_change() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.draft = Some(Annotation {
    id: 0,
    tool: Tool::Arrow,
    points: vec![(100, 100), (300, 100)],
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
  assert_eq!(editor.geometry_handle_hit((200.0, 100.0)), Some((0, 1)));
  assert_eq!(editor.geometry_handle_hit((100.0, 100.0)), Some((0, 0)));
  assert_eq!(editor.geometry_handle_hit((300.0, 100.0)), Some((0, 2)));
  assert_eq!(editor.geometry_handle_hit((200.0, 120.0)), None);
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  for cancel in [true, false] {
    editor.geometry_handle = Some(1);
    editor.moving = Some(ObjectMove {
      index: 0,
      before: original.clone(),
      preview: original.clone(),
      start: (200, 100),
    });
    for y in [120.0, 160.0, 200.0] {
      controller.cursor_pos = (200.0, y);
      editor.update_move(&controller);
      assert_eq!(editor.annotations[0], original);
    }
    if cancel {
      editor.cancel_gesture();
      assert_eq!(editor.annotations[0], original);
      assert_eq!(editor.geometry_handle, None);
    } else {
      editor.finish_gesture();
      let curved = editor.annotations[0].clone();
      assert_eq!(curved.points, vec![(100, 100), (200, 200), (300, 100)]);
      assert_eq!(editor.geometry_handle_hit((200.0, 200.0)), Some((0, 1)));
      editor.undo();
      assert_eq!(editor.annotations[0], original);
      editor.redo();
      assert_eq!(editor.annotations[0], curved);
      editor.geometry_handle = Some(2);
      editor.moving = Some(ObjectMove {
        index: 0,
        before: curved.clone(),
        preview: curved.clone(),
        start: (300, 100),
      });
      controller.cursor_pos = (400.0, 140.0);
      editor.update_move(&controller);
      editor.finish_gesture();
      assert_eq!(
        editor.annotations[0].points,
        vec![(100, 100), (250, 220), (400, 140)]
      );
      editor.undo();
      assert_eq!(editor.annotations[0], curved);
      editor.undo();
      assert_eq!(editor.annotations[0], original);
    }
  }
  editor.group_selection = vec![original.id, 99];
  assert_eq!(editor.geometry_handle_hit((200.0, 100.0)), None);
}

#[test]
fn constrained_shapes_preserve_ratio_and_crop_bounds_in_every_direction() {
  let rect = Rect {
    x: -40.0,
    y: -30.0,
    w: 100.0,
    h: 80.0,
  };
  for start in [(-39, -29), (58, 48), (0, 0)] {
    for target in [
      (-200, -150),
      (-200, 150),
      (200, -150),
      (200, 150),
      (0, 80),
      (90, 0),
    ] {
      for tool in [Tool::Rectangle, Tool::Ellipse, Tool::Arrow] {
        let point = constrained_endpoint(tool, start, target, rect);
        assert!((-40..60).contains(&point.0) && (-30..50).contains(&point.1));
        let (dx, dy) = (point.0 - start.0, point.1 - start.1);
        if tool == Tool::Arrow {
          assert!(dx == 0 || dy == 0 || dx.abs() == dy.abs());
        } else {
          assert_eq!(dx.abs(), dy.abs());
        }
      }
    }
  }
}
