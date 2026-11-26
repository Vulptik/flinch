use super::*;

#[test]
fn selected_curves_pen_nodes_and_rotated_text_render_edit_handles_without_mutating_drawings() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
  let source = [20, 30, 40, 255].repeat(800 * 600);
  let base = Annotation {
    id: 1,
    tool: Tool::Arrow,
    points: vec![(180, 180), (360, 280)],
    color: COLORS[0],
    width: 4,
    label: None,
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: vec![],
    pen_nodes: vec![],
  };
  let mut cases = vec![base.clone()];
  let mut curve = base.clone();
  curve.curve_controls = vec![[(180, 180), (210, 160)], [(300, 300), (360, 280)]];
  cases.push(curve);
  let mut pen = base.clone();
  pen.tool = Tool::Pen;
  pen.points.push((420, 210));
  pen.pen_nodes = vec![0, 1, 2];
  cases.push(pen);
  let mut rectangle = base.clone();
  rectangle.tool = Tool::Rectangle;
  rectangle.corner_radius = 20;
  cases.push(rectangle);
  for rotation in [0, 30000] {
    let mut text = base.clone();
    text.tool = Tool::Text;
    text.points.truncate(1);
    text.rotation_millidegrees = rotation;
    text.label = Some(Label::Text(
      text_data("Café 日本語".into(), 24, 180).unwrap(),
    ));
    cases.push(text);
  }
  for drawing in cases {
    let mut editor = AnnotationEditor::new("en".into());
    editor.diagnostics = Some(Default::default());
    editor.monitors.clear();
    editor.annotations = vec![drawing.clone()];
    editor.selected = Some(1);
    editor.object_mode = true;
    if drawing.tool == Tool::Pen {
      editor.node_mode = Some(1);
      editor.node_indices = vec![0, 1, 2];
    }
    if matches!(drawing.tool, Tool::Pen | Tool::Arrow) {
      editor.selected_node = Some(0);
    }
    let mut controller = SelectionControllerState::new(800, 600, 0, 0);
    controller.interaction = SelectionInteraction::Confirmed {
      rect: Rect {
        x: 60.,
        y: 60.,
        w: 650.,
        h: 430.,
      },
      from_snap: false,
      moving: None,
      resizing: None,
    };
    let mut bitmap = ScreenshotBitmap::from_rgba(&source, 800, 600).unwrap();
    let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
    editor.draw(&controller, bitmap.dc, font);
    assert!(!editor.render_failed, "{:?}", drawing.tool);
    assert_eq!(editor.annotations, vec![drawing.clone()]);
    assert_eq!(editor.paint_sample.drawings, 1);
    assert_eq!(editor.paint_sample.points, drawing.points.len());
    let pixels = bitmap.to_rgba().unwrap();
    assert!(
      pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[..3] == [96, 165, 250])
    );
    if matches!(drawing.tool, Tool::Pen | Tool::Arrow) {
      assert!(
        pixels
          .as_chunks::<4>()
          .0
          .iter()
          .any(|p| p[..3] == [245, 185, 65])
      );
    }
    if drawing.tool == Tool::Text {
      editor.moving = Some(ObjectMove {
        index: 0,
        before: drawing.clone(),
        preview: drawing.translated(10, 15).unwrap(),
        start: (180, 180),
      });
      let bounds = editor.active_bounds().unwrap();
      assert!(bounds.right > bounds.left && bounds.bottom > bounds.top);
      editor.draw(&controller, bitmap.dc, font);
      assert!(!editor.render_failed);
      assert_eq!(editor.annotations, vec![drawing.clone()]);
    }
    let full = RECT {
      left: 0,
      top: 0,
      right: 800,
      bottom: 600,
    };
    editor.paint_cached(&controller, bitmap.dc, font, full, false);
    let invalid_dc = HDC((-1isize) as *mut _);
    assert!(
      editor
        .restore_scene(&controller, invalid_dc, full)
        .is_none()
    );
    editor.paint_cached(&controller, invalid_dc, font, full, true);
    assert!(
      editor.render_failed,
      "invalid native DC must report a paint failure"
    );
    editor.clear_toolbar_error();
    editor.draw(&controller, invalid_dc, font);
    assert!(editor.render_failed);
    assert_eq!(editor.annotations, vec![drawing]);
  }
}

#[test]
fn cached_damage_paints_match_reference_through_editing_and_crop_changes() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use windows::Win32::Graphics::Gdi::{BitBlt, DEFAULT_GUI_FONT, GetStockObject, SRCCOPY};
  let mut source = vec![255; 800 * 600 * 4];
  for (index, pixel) in source.as_chunks_mut::<4>().0.iter_mut().enumerate() {
    pixel.copy_from_slice(&[(index % 190) as u8, (index / 800 % 180) as u8, 100, 255]);
  }
  let frozen = ScreenshotBitmap::from_rgba(&source, 800, 600).expect("frozen");
  let mut cached = ScreenshotBitmap::from_rgba(&source, 800, 600).expect("cached target");
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  let full = RECT {
    left: 0,
    top: 0,
    right: 800,
    bottom: 600,
  };
  let mut rect = Rect {
    x: 80.0,
    y: 80.0,
    w: 600.0,
    h: 330.0,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  editor.draft = Some(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: 0,
    tool: Tool::Pen,
    points: vec![(120, 140), (240, 200)],
    width: 6,
    color: COLORS[0],
  });
  editor.finish_gesture();
  // SAFETY: borrowed stock font, thread-owned test DCs.
  assert!(
    editor.apply_change(crate::overlay::screenshot::history::Change {
      counter: None,
      index: 1,
      before: None,
      after: Some(Annotation {
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
        label: Some(Label::Text(
          text_data("Café 日本語\nWrapped text".into(), 24, 160).expect("text layout")
        )),
        id: 2,
        tool: Tool::Text,
        points: vec![(350, 120)],
        color: COLORS[3],
        width: 3
      })
    })
  );
  assert!(
    editor.apply_change(crate::overlay::screenshot::history::Change {
      counter: Some((0, 1)),
      index: 2,
      before: None,
      after: Some(Annotation {
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
        label: Some(Label::Number(1)),
        id: 3,
        tool: Tool::Marker,
        points: vec![(500, 280)],
        color: COLORS[1],
        width: 3
      })
    })
  );
  // SAFETY: stock font is borrowed for these native renders.
  let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
  let mut hits = 0;
  for step in 0..28 {
    controller.interaction = SelectionInteraction::Confirmed {
      rect,
      from_snap: false,
      moving: None,
      resizing: None,
    };
    let mut damage = full;
    match step {
      1 => {
        let button = toolbar_button(
          toolbar_rect(&controller, rect, &editor.monitors),
          12,
          editor.metrics(&controller, rect),
        );
        controller.cursor_pos = ((button.left + 10) as f64, (button.top + 10) as f64);
        damage = editor.chrome_region(&controller, rect);
      },
      2 => {
        editor.width_open = true;
        damage = editor.chrome_region(&controller, rect);
      },
      3 => {
        editor.width_open = false;
        damage = editor.chrome_region(&controller, rect);
      },
      4 => {
        editor.draft = Some(Annotation {
          corner_radius: 0,
          rotation_millidegrees: 0,
          path_transform: crate::screenshot::path::IDENTITY,
          curve_controls: Vec::new(),
          pen_nodes: Vec::new(),
          label: None,
          id: 0,
          tool: Tool::Arrow,
          points: vec![(140, 240), (300, 150)],
          width: 10,
          color: COLORS[1],
        });
        damage = editor.active_bounds().expect("draft bounds");
      },
      5 => {
        let old = editor.active_bounds();
        editor.draft.as_mut().expect("draft").points[1] = (480, 320);
        damage = combine_bounds(old, editor.active_bounds()).expect("damage");
      },
      6 => {
        damage = editor.active_bounds().expect("old draft");
        editor.cancel_gesture();
      },
      7 => {
        editor.selected = Some(editor.annotations[0].id);
        editor.object_mode = true;
      },
      8 => {
        let before = editor.annotations[0].clone();
        editor.moving = Some(ObjectMove {
          index: 0,
          preview: before.clone(),
          before,
          start: (120, 140),
        });
      },
      9 => {
        let old = editor.active_bounds();
        controller.cursor_pos = (280.0, 250.0);
        editor.update_move(&controller);
        damage = combine_bounds(old, editor.active_bounds()).expect("move damage");
      },
      10 => editor.cancel_gesture(),
      11 => {
        editor.width = 10;
        editor.color = 2;
        editor.edit_selected_style();
      },
      12 => editor.undo(),
      13 => editor.redo(),
      14 => editor.delete_selected(),
      15 => editor.undo(),
      16 => {
        rect = Rect {
          x: 20.0,
          y: 20.0,
          w: 500.0,
          h: 380.0,
        };
        controller.interaction = SelectionInteraction::Confirmed {
          rect,
          from_snap: false,
          moving: None,
          resizing: None,
        };
      },
      18 => {
        editor.object_mode = true;
        editor.selected = Some(editor.annotations[0].id);
        editor.group_selection = vec![editor.annotations[0].id, editor.annotations[2].id];
        editor.moving = Some(ObjectMove {
          index: 0,
          before: editor.annotations[0].clone(),
          preview: editor.annotations[0].clone(),
          start: (120, 140),
        });
        editor.group_move = vec![ObjectMove {
          index: 2,
          before: editor.annotations[2].clone(),
          preview: editor.annotations[2].clone(),
          start: (120, 140),
        }];
        editor.scene_cache.invalidate();
      },
      19 => {
        let old = editor.active_bounds();
        controller.cursor_pos = (90.0, 160.0);
        editor.update_move(&controller);
        damage = combine_bounds(old, editor.active_bounds()).expect("group damage");
      },
      20 => editor.cancel_gesture(),
      21 => {
        editor.group_selection.clear();
        assert!(
          editor.apply_change(crate::overlay::screenshot::history::Change {
            counter: None,
            index: editor.annotations.len(),
            before: None,
            after: Some(Annotation {
              id: 4,
              tool: Tool::Arrow,
              points: vec![(100, 300), (400, 140)],
              width: 6,
              color: COLORS[0],
              label: None,
              corner_radius: 0,
              rotation_millidegrees: 0,
              path_transform: crate::screenshot::path::IDENTITY,
              curve_controls: Vec::new(),
              pen_nodes: Vec::new(),
            }),
          })
        );
        editor.selected = Some(4);
      },
      22 | 25 => {
        let index = editor
          .annotations
          .iter()
          .position(|a| a.id == 4)
          .expect("arrow");
        let before = editor.annotations[index].clone();
        editor.moving = Some(ObjectMove {
          index,
          preview: before.clone(),
          before,
          start: (250, 220),
        });
        editor.geometry_handle = Some(if step == 22 { 1 } else { 2 });
        editor.scene_cache.invalidate();
      },
      23 | 26 => {
        let old = editor.active_bounds();
        controller.cursor_pos = if step == 23 {
          (440.0, 280.0)
        } else {
          (460.0, 80.0)
        };
        editor.update_move(&controller);
        damage = combine_bounds(old, editor.active_bounds()).expect("arrow handle damage");
      },
      24 => editor.finish_gesture(),
      27 => editor.cancel_gesture(),
      _ => {},
    }
    if editor.hover_changed(&controller) {
      damage =
        crate::overlay::screenshot::cache::union(damage, editor.chrome_region(&controller, rect));
    }
    let restored = editor.restore_scene(&controller, cached.dc, damage);
    if restored.is_some() {
      hits += 1;
    } else {
      // SAFETY: live full-sized test DIBs; mirrors a full reference scene refresh on a miss.
      unsafe { BitBlt(cached.dc, 0, 0, 800, 600, Some(frozen.dc), 0, 0, SRCCOPY) }
        .expect("restore source");
    }
    editor.paint_cached(&controller, cached.dc, font, damage, restored.is_some());
    let mut reference = ScreenshotBitmap::from_rgba(&source, 800, 600).expect("reference");
    editor.draw(&controller, reference.dc, font);
    assert!(!editor.render_failed);
    let actual = cached.to_rgba().expect("cached pixels");
    let expected = reference.to_rgba().expect("reference pixels");
    assert!(
      actual == expected,
      "step {step}: stale or repeatedly blended pixels; first mismatch {:?}",
      actual.iter().zip(&expected).position(|(a, b)| a != b)
    );
  }
  assert!(
    hits >= 7,
    "draft/hover/chrome paints must use completed scene cache"
  );
}

#[test]
fn dragging_a_drawing_far_outside_the_crop_leaves_no_trail() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use windows::Win32::Graphics::Gdi::{BitBlt, DEFAULT_GUI_FONT, GetStockObject, SRCCOPY};
  let (width, height) = (1000, 800);
  let mut source = vec![255; width * height * 4];
  for (index, pixel) in source.as_chunks_mut::<4>().0.iter_mut().enumerate() {
    pixel.copy_from_slice(&[(index % 190) as u8, (index / width % 180) as u8, 100, 255]);
  }
  let frozen = ScreenshotBitmap::from_rgba(&source, width as u32, height as u32).expect("frozen");
  let mut cached =
    ScreenshotBitmap::from_rgba(&source, width as u32, height as u32).expect("cached target");
  let mut controller = SelectionControllerState::new(width as u32, height as u32, 0, 0);
  let rect = Rect {
    x: 400.0,
    y: 300.0,
    w: 200.0,
    h: 150.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  assert!(
    editor.apply_change(crate::overlay::screenshot::history::Change {
      counter: None,
      index: 0,
      before: None,
      after: Some(Annotation {
        id: 1,
        tool: Tool::Rectangle,
        points: vec![(420, 320), (480, 360)],
        width: 6,
        color: COLORS[0],
        label: None,
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
      }),
    })
  );
  editor.object_mode = true;
  editor.selected = Some(1);
  // SAFETY: stock font is borrowed for these native renders.
  let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
  let full = RECT {
    left: 0,
    top: 0,
    right: width as i32,
    bottom: height as i32,
  };
  let paint = |editor: &mut AnnotationEditor,
               controller: &SelectionControllerState,
               cached: &mut ScreenshotBitmap,
               damage: RECT,
               step: &str| {
    let restored = editor.restore_scene(controller, cached.dc, damage);
    if restored.is_none() {
      // SAFETY: live full-sized test DIBs; mirrors a full reference scene refresh on a miss.
      unsafe {
        BitBlt(
          cached.dc,
          0,
          0,
          width as i32,
          height as i32,
          Some(frozen.dc),
          0,
          0,
          SRCCOPY,
        )
      }
      .expect("restore source");
    }
    editor.paint_cached(controller, cached.dc, font, damage, restored.is_some());
    let mut reference =
      ScreenshotBitmap::from_rgba(&source, width as u32, height as u32).expect("reference");
    editor.draw(controller, reference.dc, font);
    let actual = cached.to_rgba().expect("cached pixels");
    let expected = reference.to_rgba().expect("reference pixels");
    // The host presents only the damage, so that is what must match the reference.
    let mismatch = (damage.top.max(0)..damage.bottom.min(height as i32))
      .flat_map(|y| (damage.left.max(0)..damage.right.min(width as i32)).map(move |x| (x, y)))
      .find(|&(x, y)| {
        let at = (y as usize * width + x as usize) * 4;
        actual[at..at + 4] != expected[at..at + 4]
      });
    assert!(
      mismatch.is_none(),
      "{step}: stale selection chrome at pixel {:?}",
      mismatch
    );
  };
  paint(&mut editor, &controller, &mut cached, full, "settled");
  let before = editor.annotations[0].clone();
  editor.moving = Some(ObjectMove {
    index: 0,
    preview: before.clone(),
    before,
    start: (450, 340),
  });
  // Up and to the left, well past the cached area around the crop and toolbar.
  for (index, pointer) in [(400.0, 260.0), (300.0, 180.0), (180.0, 100.0), (90.0, 60.0)]
    .into_iter()
    .enumerate()
  {
    let old = editor.active_bounds();
    controller.cursor_pos = pointer;
    editor.update_move(&controller);
    let mut damage = combine_bounds(old, editor.active_bounds()).expect("move damage");
    if editor.hover_changed(&controller) {
      damage =
        crate::overlay::screenshot::cache::union(damage, editor.chrome_region(&controller, rect));
    }
    paint(
      &mut editor,
      &controller,
      &mut cached,
      damage,
      &format!("move {index}"),
    );
  }
}

#[test]
fn unchanged_hover_needs_no_repaint_even_after_native_cursor_updates() {
  let mut editor = AnnotationEditor::new("en".into());
  let mut controller = SelectionControllerState::new(800, 500, 0, 0);
  let rect = Rect {
    x: 50.0,
    y: 50.0,
    w: 500.0,
    h: 250.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  controller.cursor_pos = (150.0, 150.0);
  assert!(!editor.hover_changed(&controller));
  let button = toolbar_button(
    toolbar_rect(&controller, rect, &editor.monitors),
    12,
    editor.metrics(&controller, rect),
  );
  controller.cursor_pos = ((button.left + 10) as f64, (button.top + 10) as f64);
  assert!(editor.hover_changed(&controller));
  controller.cursor_pos.0 += 1.0;
  assert!(!editor.hover_changed(&controller));
  controller.cursor_pos = (150.0, 150.0);
  assert!(editor.hover_changed(&controller));
  assert!(!editor.hover_changed(&controller));
}

#[test]
fn native_preview_and_export_use_the_same_drawings_without_toolbar_pixels() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use crate::overlay::selection::Selection;
  use crate::screenshot::annotations::composite;
  use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};

  let mut source = vec![255u8; 800 * 500 * 4];
  for y in 0..500usize {
    for x in 0..800usize {
      let pixel = &mut source[(y * 800 + x) * 4..(y * 800 + x) * 4 + 4];
      pixel.copy_from_slice(&[35 + (x / 8) as u8, 40 + (y / 5) as u8, 85, 255]);
    }
  }
  let mut bitmap = ScreenshotBitmap::from_rgba(&source, 800, 500).expect("preview bitmap");
  let mut controller = SelectionControllerState::new(800, 500, 0, 0);
  let rect = Rect {
    x: 60.0,
    y: 80.0,
    w: 620.0,
    h: 300.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.tool = Tool::Arrow;
  editor.annotations = vec![
    Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 0,
      tool: Tool::Pen,
      points: vec![(90, 140), (115, 125), (140, 155), (175, 130), (210, 145)],
      color: COLORS[3],
      width: 3,
    },
    Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 0,
      tool: Tool::Arrow,
      points: vec![(280, 300), (440, 245), (440, 150)],
      color: COLORS[0],
      width: 6,
    },
    Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 0,
      tool: Tool::Rectangle,
      points: vec![(90, 200), (230, 320)],
      color: COLORS[1],
      width: 3,
    },
    Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 0,
      tool: Tool::Ellipse,
      points: vec![(480, 110), (640, 290)],
      color: COLORS[2],
      width: 6,
    },
  ];
  editor.annotations.push(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: Some(Label::Text(
      text_data("Café — 日本語\nMultiline 🙂".into(), 24, 230).expect("Unicode layout"),
    )),
    id: 5,
    tool: Tool::Text,
    points: vec![(100, 80)],
    color: COLORS[3],
    width: 3,
  });
  editor.annotations.push(Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: Some(Label::Number(123)),
    id: 6,
    tool: Tool::Marker,
    points: vec![(400, 260)],
    color: COLORS[1],
    width: 3,
  });

  for (index, drawing) in editor.annotations.iter_mut().enumerate() {
    drawing.id = index as u64 + 1;
    if drawing.tool == Tool::Rectangle {
      drawing.corner_radius = 30;
      drawing.rotation_millidegrees = 330_000;
    }
    if drawing.tool == Tool::Ellipse {
      drawing.rotation_millidegrees = 25_000;
    }
    if drawing.tool == Tool::Text {
      drawing.rotation_millidegrees = 25_000;
    }
  }
  // SAFETY: stock font is borrowed; the test bitmap owns its live DC on this thread.
  let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
  editor.draw(&controller, bitmap.dc, font);
  assert!(!editor.render_failed);
  let preview = bitmap.to_rgba().expect("read preview");
  let selection = Selection {
    x: 60,
    y: 80,
    width: 620,
    height: 300,
    source_app: None,
  };
  let crop = crate::screenshot::encoding::crop_rgba_direct(&source, 800, 60, 80, 620, 300)
    .expect("valid fixture crop");
  let output = composite(crop, &selection, &editor.annotations).expect("export drawings");
  assert_ne!(preview, source);
  let panel = toolbar_rect(&controller, rect, &editor.monitors);
  let pixel = |image: &[u8], x: i32, y: i32| {
    let offset = (y as usize * 800 + x as usize) * 4;
    [image[offset], image[offset + 1], image[offset + 2]]
  };
  // The soft shadow only darkens, is strongest just below the panel and ends within its spread.
  for (x, y) in [
    (panel.left - 2, panel.top + 20),
    (panel.left + 20, panel.bottom + 3),
  ] {
    let (shaded, plain) = (pixel(&preview, x, y), pixel(&source, x, y));
    assert!(
      shaded.iter().zip(plain).all(|(s, p)| *s <= p),
      "toolbar shadow must never lighten ({x}, {y})"
    );
  }
  assert_ne!(
    pixel(&preview, panel.left + 20, panel.bottom + 3),
    pixel(&source, panel.left + 20, panel.bottom + 3),
    "toolbar casts a visible shadow below it"
  );
  let spread =
    (crate::overlay::chrome::SHADOW_SPREAD * editor.metrics(&controller, rect).scale).ceil() as i32;
  for (x, y) in [
    (panel.left - spread - 2, panel.top + 20),
    (panel.left + 20, panel.bottom + spread * 2),
  ] {
    assert_eq!(
      pixel(&preview, x, y),
      pixel(&source, x, y),
      "toolbar shadow must end within its spread ({x}, {y})"
    );
  }
  // Interior preview pixels must match the final crop; the toolbar is outside this crop.
  for y in 2..298usize {
    let begin = ((y + 80) * 800 + 62) * 4;
    let out = (y * 620 + 2) * 4;
    for x in 0..616 {
      assert_eq!(
        &preview[begin + x * 4..begin + x * 4 + 4],
        &output[out + x * 4..out + x * 4 + 4],
        "preview/export mismatch at ({}, {})",
        x + 62,
        y + 80
      );
    }
  }
  editor.tool = Tool::Select;
  editor.object_mode = true;
  editor.selected = Some(editor.annotations[1].id);
  let mut handles_preview = ScreenshotBitmap::from_rgba(&source, 800, 500).expect("handles bitmap");
  editor.draw(&controller, handles_preview.dc, font);
  assert!(!editor.render_failed);
  let handles_pixels = handles_preview.to_rgba().expect("handles pixels");
  let handles = crate::screenshot::arrow::Arrow::from_annotation(&editor.annotations[1])
    .expect("curved arrow")
    .handles();
  for (x, y) in handles {
    let offset = (y as usize * 800 + x as usize) * 4;
    assert_eq!(&handles_pixels[offset..offset + 3], &[96, 165, 250]);
    assert_ne!(
      &handles_pixels[offset..offset + 3],
      &preview[offset..offset + 3]
    );
  }
  let crop = crate::screenshot::encoding::crop_rgba_direct(&source, 800, 60, 80, 620, 300)
    .expect("valid fixture crop");
  assert_eq!(
    composite(crop, &selection, &editor.annotations).expect("handle-free export"),
    output
  );
  for tool in [Tool::Rectangle, Tool::Ellipse] {
    let index = editor
      .annotations
      .iter()
      .position(|a| a.tool == tool)
      .expect("shape fixture");
    editor.selected = Some(editor.annotations[index].id);
    let mut shape_preview =
      ScreenshotBitmap::from_rgba(&source, 800, 500).expect("shape handles bitmap");
    editor.draw(&controller, shape_preview.dc, font);
    assert!(!editor.render_failed);
    let pixels = shape_preview.to_rgba().expect("shape handle pixels");
    if let Some((x, y)) = crate::screenshot::shape::radius_handle(&editor.annotations[index]) {
      let offset = (y.round() as usize * 800 + x.round() as usize) * 4;
      assert_eq!(&pixels[offset..offset + 3], &[245, 185, 65]);
    }
    for (x, y) in
      crate::screenshot::shape::handles(&editor.annotations[index]).expect("shape handles")
    {
      let offset = (y.round() as usize * 800 + x.round() as usize) * 4;
      assert_eq!(&pixels[offset..offset + 3], &[96, 165, 250]);
    }
    let crop = crate::screenshot::encoding::crop_rgba_direct(&source, 800, 60, 80, 620, 300)
      .expect("valid fixture crop");
    assert_eq!(
      composite(crop, &selection, &editor.annotations).expect("shape handle-free export"),
      output
    );
    if let Ok(folder) = std::env::var("FLINCH_ANNOTATION_PREVIEW_DIR") {
      std::fs::create_dir_all(&folder).expect("preview directory");
      image::save_buffer(
        std::path::Path::new(&folder).join(format!("{tool:?}-resize-handles.png")),
        &pixels,
        800,
        500,
        image::ColorType::Rgba8,
      )
      .expect("shape preview PNG");
    }
  }
  editor.selected = None;
  editor.object_mode = false;
  editor.tool = Tool::Arrow;
  if let Ok(folder) = std::env::var("FLINCH_ANNOTATION_PREVIEW_DIR") {
    editor.selected = editor
      .annotations
      .iter()
      .find(|a| a.tool == Tool::Text)
      .map(|a| a.id);
    editor.object_mode = true;
    let mut text_preview =
      ScreenshotBitmap::from_rgba(&source, 800, 500).expect("text handle bitmap");
    editor.draw(&controller, text_preview.dc, font);
    assert!(!editor.render_failed);
    let pixels = text_preview.to_rgba().expect("text handle pixels");
    let bounds = editor.text_selection_bounds().expect("text handle bounds");
    let point = crate::screenshot::shape::rotate(
      editor.selected_text().expect("selected text"),
      (
        bounds.right as f64,
        (bounds.top + bounds.bottom) as f64 / 2.0,
      ),
      false,
    );
    let offset = (point.1.round() as usize * 800 + point.0.round() as usize) * 4;
    assert_eq!(&pixels[offset..offset + 3], &[96, 165, 250]);
    image::save_buffer(
      std::path::Path::new(&folder).join("Text-width-handle.png"),
      &pixels,
      800,
      500,
      image::ColorType::Rgba8,
    )
    .expect("text handle PNG");
    editor.selected = None;
    editor.object_mode = false;
  }
  if let Ok(folder) = std::env::var("FLINCH_ANNOTATION_PREVIEW_DIR") {
    let folder = std::path::PathBuf::from(&folder);
    std::fs::create_dir_all(&folder).expect("preview output directory");
    image::save_buffer(
      folder.join("annotation-preview.png"),
      &preview,
      800,
      500,
      image::ColorType::Rgba8,
    )
    .expect("preview PNG");
    image::save_buffer(
      folder.join("annotation-output.png"),
      &output,
      620,
      300,
      image::ColorType::Rgba8,
    )
    .expect("output PNG");
    editor.width_open = true;
    let mut popup = ScreenshotBitmap::from_rgba(&source, 800, 500).expect("picker preview");
    editor.draw(&controller, popup.dc, font);
    assert!(!editor.render_failed);
    image::save_buffer(
      folder.join("annotation-width-picker.png"),
      &popup.to_rgba().expect("picker pixels"),
      800,
      500,
      image::ColorType::Rgba8,
    )
    .expect("picker PNG");
    editor.width_open = false;
    editor.color_open = true;
    let mut palette = ScreenshotBitmap::from_rgba(&source, 800, 500).expect("palette preview");
    editor.draw(&controller, palette.dc, font);
    assert!(!editor.render_failed);
    image::save_buffer(
      folder.join("annotation-color-picker.png"),
      &palette.to_rgba().expect("palette pixels"),
      800,
      500,
      image::ColorType::Rgba8,
    )
    .expect("palette PNG");
    editor.color_open = false;
    editor.width_open = false;
    editor.object_mode = true;
    editor.selected = Some(editor.annotations[1].id);
    let mut selected = ScreenshotBitmap::from_rgba(&source, 800, 500).expect("selection preview");
    editor.draw(&controller, selected.dc, font);
    assert!(!editor.render_failed);
    image::save_buffer(
      folder.join("annotation-arrow-handles.png"),
      &selected.to_rgba().expect("selection pixels"),
      800,
      500,
      image::ColorType::Rgba8,
    )
    .expect("selection PNG");
    let crop = crate::screenshot::encoding::crop_rgba_direct(&source, 800, 60, 80, 620, 300)
      .expect("valid fixture crop");
    assert_eq!(
      composite(crop, &selection, &editor.annotations).expect("selected export"),
      output
    );
    let original_arrow = editor.annotations[1].clone();
    let p = crate::screenshot::arrow::Arrow::from_annotation(&original_arrow)
      .expect("arrow")
      .at(0.25);
    editor.annotations[1] = crate::screenshot::arrow::insert_node(&original_arrow, p)
      .expect("arrow node")
      .0;
    let p = crate::screenshot::arrow::Arrow::from_annotation(&editor.annotations[1])
      .expect("multi arrow")
      .at(0.7);
    editor.annotations[1] = crate::screenshot::arrow::insert_node(&editor.annotations[1], p)
      .expect("second arrow node")
      .0;
    editor.scene_cache.invalidate();
    editor.selected_node = Some(1);
    let mut multi = ScreenshotBitmap::from_rgba(&source, 800, 500).expect("multi arrow preview");
    editor.draw(&controller, multi.dc, font);
    assert!(!editor.render_failed);
    image::save_buffer(
      folder.join("arrow-editable-nodes.png"),
      &multi.to_rgba().expect("arrow node pixels"),
      800,
      500,
      image::ColorType::Rgba8,
    )
    .expect("arrow node preview");
    let pen = editor.annotations[0].clone();
    editor.annotations[0] =
      crate::screenshot::path::edit_transform(&pen, 9, (250, 100), true).expect("pen rotation");
    editor.selected = Some(editor.annotations[0].id);
    editor.selected_node = None;
    editor.node_mode = None;
    editor.scene_cache.invalidate();
    let mut frame = ScreenshotBitmap::from_rgba(&source, 800, 500).expect("pen transform preview");
    editor.draw(&controller, frame.dc, font);
    assert!(!editor.render_failed);
    image::save_buffer(
      folder.join("pen-transform-handles.png"),
      &frame.to_rgba().expect("pen transform pixels"),
      800,
      500,
      image::ColorType::Rgba8,
    )
    .expect("pen transform preview");
    editor.node_mode = editor.selected;
    editor.node_indices = crate::screenshot::path::nodes(&editor.annotations[0]);
    editor.selected_node = editor.node_indices.get(1).copied();
    let mut nodes = ScreenshotBitmap::from_rgba(&source, 800, 500).expect("pen node preview");
    editor.draw(&controller, nodes.dc, font);
    assert!(!editor.render_failed);
    image::save_buffer(
      folder.join("pen-editable-nodes.png"),
      &nodes.to_rgba().expect("pen node pixels"),
      800,
      500,
      image::ColorType::Rgba8,
    )
    .expect("pen node preview");
  }
}

fn blur_region(id: u64, points: Vec<(i32, i32)>) -> Annotation {
  Annotation {
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    rotation_millidegrees: 0,
    corner_radius: 0,
    label: None,
    id,
    tool: Tool::Blur,
    points,
    color: COLORS[0],
    width: 3,
  }
}

#[test]
fn blur_regions_paint_the_frozen_capture_inside_the_clip_only() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  let (width, height) = (64u32, 64u32);
  let source: Vec<u8> = (0..width * height)
    .flat_map(|index| {
      let value = (index % width * 4) as u8;
      [value, 255 - value, 40, 255]
    })
    .collect();
  let frozen = crate::overlay::screenshot::editor::FrozenPixels {
    rgba: std::sync::Arc::new(source.clone()),
    width,
    height,
  };
  let region = blur_region(1, vec![(20, 20), (44, 44)]);
  let layer = crate::screenshot::blur::layer(
    &source,
    width,
    height,
    std::iter::once(&region),
    (0, 0, 32, 64),
  )
  .expect("layer");
  let mut bitmap =
    ScreenshotBitmap::from_rgba(&[0, 0, 0, 255].repeat(64 * 64), width, height).expect("bitmap");
  let clip = RECT {
    left: 0,
    top: 0,
    right: 32,
    bottom: 64,
  };
  super::super::render::paint_blur(bitmap.dc, Some(&frozen), std::iter::once(&region), clip)
    .expect("paint");
  let output = bitmap.to_rgba().expect("pixels");
  let at = |x: usize, y: usize| &output[(y * 64 + x) * 4..(y * 64 + x) * 4 + 3];
  let expected = |x: usize, y: usize| {
    let index = ((y - layer.top as usize) * layer.width as usize + x - layer.left as usize) * 4;
    &layer.rgba[index..index + 3]
  };
  assert_eq!(at(22, 22), expected(22, 22));
  assert_eq!(at(31, 43), expected(31, 43));
  assert_eq!(at(40, 20), &[0, 0, 0], "outside the clip");
  super::super::render::paint_blur(bitmap.dc, None, std::iter::once(&region), clip)
    .expect("no capture to read");
}

#[test]
fn the_blur_tool_turns_off_color_and_stroke_controls() {
  let mut editor = AnnotationEditor::new("en".into());
  assert!(!editor.blur_mode());
  editor.tool = Tool::Blur;
  assert!(editor.blur_mode());
  assert_eq!(editor.active_tool_button(), 16);
  editor.tool = Tool::Select;
  editor
    .annotations
    .push(blur_region(4, vec![(0, 0), (20, 20)]));
  editor.selected = Some(4);
  assert!(editor.blur_mode());
}
