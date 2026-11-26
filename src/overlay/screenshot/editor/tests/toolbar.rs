use super::*;

#[test]
fn rendered_popovers_keep_palette_samples_font_styles_and_error_details_inside_the_crop_view() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use std::time::{Duration, Instant};
  let mut controller = SelectionControllerState::new(1280, 800, 0, 0);
  let rect = Rect {
    x: 80.0,
    y: 80.0,
    w: 1050.0,
    h: 400.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  for lang in ["en", "fr", "de", "es", "it"] {
    for mode in 0..4 {
      let mut editor = AnnotationEditor::new(lang.into());
      editor.monitors.clear();
      editor.tool = if mode >= 2 { Tool::Text } else { Tool::Pen };
      editor.color_open = mode == 0;
      editor.width_open = mode != 0;
      editor.text_style_open = mode == 3;
      let popup = if mode == 0 {
        1
      } else if mode == 3 {
        3
      } else {
        2
      };
      let age = Duration::from_secs(1);
      editor.motion = crate::overlay::motion::Motion::preview(age, age, age, popup, None);
      let panel = editor.picker_rect(&controller, rect);
      assert!(panel.left >= 0 && panel.top >= 0 && panel.right <= 1280 && panel.bottom <= 800);
      let mut image = ScreenshotBitmap::from_rgba(&vec![0; 1280 * 800 * 4], 1280, 800).unwrap();
      editor
        .draw_toolbar(&controller, image.dc, HFONT::default(), rect)
        .unwrap();
      let pixels = image.to_rgba().unwrap();
      assert!(
        pixels
          .as_chunks::<4>()
          .0
          .iter()
          .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
      );
      if mode == 0 {
        for index in 0..12 {
          let cell = color_cell(panel, index, UNSCALED);
          let (x, y) = ((cell.left + cell.right) / 2, (cell.top + cell.bottom) / 2);
          controller.cursor_pos = (x as f64, y as f64);
          assert_eq!(editor.current_hover_key(&controller), Some(20 + index));
          let at = (y as usize * 1280 + x as usize) * 4;
          assert_eq!(&pixels[at..at + 3], &COLORS[index as usize]);
        }
      } else if mode == 3 {
        for index in 0..3 {
          let cell = appearance_cell(panel, index, UNSCALED);
          controller.cursor_pos = (
            ((cell.left + cell.right) / 2) as f64,
            ((cell.top + cell.bottom) / 2) as f64,
          );
          assert_eq!(editor.current_hover_key(&controller), Some(46 + index));
        }
      } else if mode == 2 {
        for index in 0..6 {
          let cell = font_cell(panel, index, UNSCALED);
          controller.cursor_pos = (
            ((cell.left + cell.right) / 2) as f64,
            ((cell.top + cell.bottom) / 2) as f64,
          );
          assert_eq!(editor.current_hover_key(&controller), Some(40 + index));
        }
        let button = style_menu_button(panel, UNSCALED);
        controller.cursor_pos = (
          ((button.left + button.right) / 2) as f64,
          ((button.top + button.bottom) / 2) as f64,
        );
        assert_eq!(editor.current_hover_key(&controller), Some(50));
      }
    }
    for error in 0..7 {
      let mut editor = AnnotationEditor::new(lang.into());
      editor.monitors.clear();
      match error {
        0 => editor.text_error = true,
        1 | 2 => {
          editor.render_failed = true;
          editor.error_button = Some(if error == 1 { 5 } else { 6 });
        },
        _ => {
          editor.limited = Some(match error {
            3 => DrawingLimit::Drawings,
            4 => DrawingLimit::Stroke,
            5 => DrawingLimit::Marker,
            _ => DrawingLimit::Identifier,
          })
        },
      }
      editor.error_details_pinned = true;
      editor.error_fade = crate::overlay::motion::Tween::new(1.0, Duration::ZERO, Instant::now());
      let age = Duration::from_secs(1);
      editor.motion = crate::overlay::motion::Motion::preview(age, age, age, 0, None);
      let mut image = ScreenshotBitmap::from_rgba(&vec![0; 1280 * 800 * 4], 1280, 800).unwrap();
      editor
        .draw_toolbar(&controller, image.dc, HFONT::default(), rect)
        .unwrap();
      let pixels = image.to_rgba().unwrap();
      assert!(
        pixels
          .as_chunks::<4>()
          .0
          .iter()
          .any(|p| p[0] > 180 && p[1] < 140 && p[2] < 140),
        "error badge must remain visible in {lang}"
      );
    }
  }
}

#[test]
fn indicator_visibility_tracks_crop_mode_and_invalidates_cached_scene() {
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  let mut editor = AnnotationEditor::new("en".into());
  editor.tool = Tool::Pen;
  assert!(
    editor.shows_selection_indicator(&controller),
    "initial selection retains measurements"
  );
  controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 60.0,
      y: 100.0,
      w: 680.0,
      h: 330.0,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  editor.tool = Tool::Select;
  let area_key = editor.cache_key(&controller).expect("confirmed cache key");
  for tool in [
    Tool::Pen,
    Tool::Arrow,
    Tool::Rectangle,
    Tool::Ellipse,
    Tool::Blur,
    Tool::Text,
    Tool::Marker,
  ] {
    editor.tool = tool;
    assert!(!editor.shows_selection_indicator(&controller));
    assert_ne!(editor.cache_key(&controller), Some(area_key));
  }
  editor.tool = Tool::Select;
  editor.object_mode = true;
  assert!(!editor.shows_selection_indicator(&controller));
  assert_ne!(editor.cache_key(&controller), Some(area_key));
  editor.object_mode = false;
  assert!(editor.shows_selection_indicator(&controller));
  assert_eq!(editor.cache_key(&controller), Some(area_key));
}

#[test]
fn crop_changes_restore_automatic_toolbar_placement() {
  let mut controller = SelectionControllerState::new(1400, 900, 0, 0);
  let original = Rect {
    x: 100.0,
    y: 100.0,
    w: 500.0,
    h: 300.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect: original,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  editor.sync_crop(&mut controller);
  editor.toolbar_position = Some((10, 10));
  editor.sync_crop(&mut controller);
  assert_eq!(
    editor.toolbar_position,
    Some((10, 10)),
    "unchanged crop keeps manual placement"
  );
  let moved = Rect {
    x: 700.0,
    y: 400.0,
    ..original
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect: moved,
    from_snap: false,
    moving: Some((0.0, 0.0)),
    resizing: None,
  };
  editor.sync_crop(&mut controller);
  assert!(
    editor.toolbar_position.is_none(),
    "follow the crop during movement"
  );
  assert_eq!(editor.toolbar_rect(&controller, moved).top, 712);
  controller.interaction = SelectionInteraction::Confirmed {
    rect: moved,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  editor.sync_crop(&mut controller);
  editor.toolbar_position = Some((10, 10));
  editor.undo();
  editor.sync_crop(&mut controller);
  assert!(
    editor.toolbar_position.is_none(),
    "undo also restores automatic placement"
  );
  assert_eq!(crop_tuple(&controller), (100.0, 100.0, 500.0, 300.0));
  editor.toolbar_position = Some((10, 10));
  editor.redo();
  editor.sync_crop(&mut controller);
  assert!(editor.toolbar_position.is_none());
  assert_eq!(crop_tuple(&controller), (700.0, 400.0, 500.0, 300.0));
  let resized = Rect { w: 400.0, ..moved };
  controller.interaction = SelectionInteraction::Confirmed {
    rect: resized,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  editor.toolbar_position = Some((10, 10));
  editor.sync_crop(&mut controller);
  assert!(
    editor.toolbar_position.is_none(),
    "resizing also updates placement"
  );
  assert!(editor.annotations.is_empty());
}

#[test]
fn toolbar_relocation_clamps_and_cancel_restores_without_drawing_changes() {
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  let rect = Rect {
    x: 100.0,
    y: 100.0,
    w: 500.0,
    h: 300.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  let original = editor.toolbar_rect(&controller, rect);
  assert_eq!(
    toolbar_index(
      original,
      (original.left as f64 + 16.0, original.top as f64 + 22.0),
      UNSCALED
    ),
    None
  );
  for position in [(-500, -500), (2000, 2000)] {
    editor.toolbar_position = Some(position);
    let moved = editor.toolbar_rect(&controller, rect);
    assert!(moved.left >= 0 && moved.top >= 0 && moved.right <= 800 && moved.bottom <= 600);
    assert_eq!(moved.right - moved.left, original.right - original.left);
    assert!(toolbar_button(moved, 0, UNSCALED).left >= toolbar_grip(moved, UNSCALED).right);
  }
  editor.toolbar_drag = Some((original, (16, 22)));
  editor.toolbar_press = true;
  editor.cancel_gesture();
  assert_eq!(editor.toolbar_position, Some((original.left, original.top)));
  assert!(editor.toolbar_drag.is_none());
  assert!(!editor.toolbar_press);
  assert!(editor.annotations.is_empty());
  assert!(!editor.history.can_undo());
}

#[test]
fn buttons_sit_evenly_inside_the_toolbar_so_their_corners_follow_its_corners() {
  use crate::overlay::chrome::{PANEL_RADIUS, inner_radius};
  use crate::overlay::screenshot::editor::layout::{Metrics, layout_toolbar};
  let bounds = RECT {
    left: 0,
    top: 0,
    right: 3000,
    bottom: 2000,
  };
  let crop = Rect {
    x: 200.0,
    y: 100.0,
    w: 1200.0,
    h: 600.0,
  };
  for scale in [1.0, 1.5, 2.0] {
    let m = Metrics::new(scale);
    let toolbar = layout_toolbar(crop, bounds, m);
    let first = toolbar_button(toolbar, ACTIONS[0], m);
    let last = toolbar_button(toolbar, ACTIONS[ACTIONS.len() - 1], m);
    let gap = first.top - toolbar.top;
    assert_eq!(gap, m.vertical_pad);
    assert_eq!(toolbar.bottom - first.bottom, gap, "{scale}");
    assert_eq!(
      toolbar.right - last.right,
      gap,
      "the end button too, at {scale}"
    );
    let grip = crate::overlay::screenshot::editor::layout::toolbar_grip_highlight(toolbar, m);
    assert_eq!(grip.left - toolbar.left, gap, "grip highlight at {scale}");
    assert_eq!(first.left - grip.right, gap, "as far from the first button");
    assert_eq!(
      (grip.top, grip.bottom),
      (first.top, first.bottom),
      "level with the buttons"
    );
    let radius = inner_radius(TOOLBAR_VERTICAL_PAD as f32);
    assert_eq!(
      m.radius(radius) + gap as f32,
      m.radius(PANEL_RADIUS),
      "{scale}"
    );
  }
}

#[test]
fn relocated_toolbar_anchors_all_pickers_and_error_chrome() {
  let mut controller = SelectionControllerState::new(1200, 900, -1200, -100);
  let rect = Rect {
    x: 100.0,
    y: 100.0,
    w: 700.0,
    h: 400.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  let bounds = toolbar_bounds(&controller, rect, &[]);
  for (tool, color, style, width, height) in [
    (Tool::Pen, true, false, 176, 128),
    (Tool::Pen, false, false, 176, 128),
    (Tool::Text, true, false, 176, 128),
    (Tool::Text, false, false, 192, 200),
    (Tool::Text, false, true, 192, 128),
  ] {
    editor.tool = tool;
    editor.color_open = color;
    editor.width_open = !color;
    editor.text_style_open = style;
    editor.toolbar_position = None;
    let original = editor.picker_rect(&controller, rect);
    for position in [(200, 700), (0, 0), (1100, 890)] {
      editor.toolbar_position = Some(position);
      let toolbar = editor.toolbar_rect(&controller, rect);
      let panel = editor.picker_rect(&controller, rect);
      assert_eq!(
        panel,
        layout_popover_at(
          toolbar,
          toolbar_button(toolbar, if color { 7 } else { 11 }, UNSCALED),
          bounds,
          width,
          height,
          UNSCALED,
        )
      );
      assert_ne!(panel, original, "picker must leave the initial anchor");
      assert!(panel.left >= bounds.left && panel.right <= bounds.right);
      assert!(panel.top >= bounds.top && panel.bottom <= bounds.bottom);
      controller.cursor_pos = ((panel.left + 10) as f64, (panel.top + 10) as f64);
      assert!(
        editor.toolbar_cursor(&controller),
        "moved picker receives pointer input"
      );
      editor.render_failed = true;
      editor.error_button = Some(1);
      editor.color_open = false;
      editor.width_open = false;
      let badge = error_badge_rect(toolbar, 1, UNSCALED);
      controller.cursor_pos = (
        (badge.left + badge.right) as f64 / 2.0,
        (badge.top + badge.bottom) as f64 / 2.0,
      );
      assert_eq!(editor.current_hover_key(&controller), Some(1));
      editor.color_open = color;
      editor.width_open = !color;
      let tooltip = error_tooltip_rect(toolbar, 1, bounds, 300, 60, UNSCALED);
      let region = editor.chrome_region(&controller, rect);
      for chrome in [panel, badge, tooltip] {
        assert!(
          region.left <= chrome.left
            && region.top <= chrome.top
            && region.right >= chrome.right
            && region.bottom >= chrome.bottom
        );
      }
    }
  }
}

#[test]
fn compact_toolbar_and_palette_have_distinct_hit_targets() {
  let toolbar = layout_toolbar(
    Rect {
      x: 100.0,
      y: 100.0,
      w: 600.0,
      h: 300.0,
    },
    RECT {
      left: 0,
      top: 0,
      right: 1000,
      bottom: 800,
    },
    UNSCALED,
  );
  for action in ACTIONS {
    let cell = toolbar_button(toolbar, action, UNSCALED);
    assert_eq!(
      toolbar_index(
        toolbar,
        (
          (cell.left + cell.right) as f64 / 2.0,
          (cell.top + cell.bottom) as f64 / 2.0
        ),
        UNSCALED
      ),
      Some(action)
    );
  }
  let panel = RECT {
    left: 0,
    top: 0,
    right: 176,
    bottom: 128,
  };
  for index in 0..12 {
    let cell = color_cell(panel, index, UNSCALED);
    assert!(
      cell.left >= panel.left
        && cell.right <= panel.right
        && cell.top >= panel.top
        && cell.bottom <= panel.bottom
    );
  }
}

#[test]
fn width_picker_stays_inside_monitor_and_padding_is_not_a_choice() {
  let mut controller = SelectionControllerState::new(300, 600, 0, 0);
  controller.vd_offset_x = -300;
  let rect = Rect {
    x: 250.0,
    y: 560.0,
    w: 40.0,
    h: 30.0,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  let panel = editor.picker_rect(&controller, rect);
  assert!(panel.left >= 0 && panel.right <= 300 && panel.top >= 0 && panel.bottom <= 600);
  for index in 0..3 {
    let row = width_row(panel, index, UNSCALED);
    assert!(row.left > panel.left && row.right < panel.right);
    assert!(row.top > panel.top && row.bottom < panel.bottom);
  }
  let padding = (panel.left as f64 + 2.0, panel.top as f64 + 2.0);
  assert!(!(0..3).any(|index| contains(width_row(panel, index, UNSCALED), padding)));
}

#[test]
fn tool_shortcuts_use_only_the_advertised_letters() {
  for (key, tool) in [
    (b'V', Tool::Select),
    (b'P', Tool::Pen),
    (b'H', Tool::Highlighter),
    (b'A', Tool::Arrow),
    (b'R', Tool::Rectangle),
    (b'E', Tool::Ellipse),
    (b'B', Tool::Blur),
    (b'T', Tool::Text),
    (b'N', Tool::Marker),
  ] {
    assert_eq!(shortcut_tool(u32::from(key)), Some(tool));
  }
  for key in [0x0D, 0x1B, 0x25, 0x5A, 0x59, 0x10, 0x12, 0x58] {
    assert_eq!(shortcut_tool(key), None);
  }
}

#[test]
fn toolbar_wraps_and_stays_on_negative_origin_monitor() {
  let bounds = RECT {
    left: -300,
    top: 0,
    right: 0,
    bottom: 600,
  };
  let toolbar = layout_toolbar(
    Rect {
      x: -20.0,
      y: 570.0,
      w: 10.0,
      h: 10.0,
    },
    bounds,
    UNSCALED,
  );
  assert!(toolbar.left >= bounds.left && toolbar.right <= bounds.right);
  assert!(toolbar.top >= bounds.top && toolbar.bottom <= bounds.bottom);
  assert!(toolbar.bottom - toolbar.top > CELL + 24);
}

#[test]
fn styled_toolbar_hit_targets_match_buttons_in_single_and_wrapped_layouts() {
  for width in [280, 800] {
    let panel = layout_toolbar(
      Rect {
        x: 20.0,
        y: 20.0,
        w: 180.0,
        h: 120.0,
      },
      RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: 600,
      },
      UNSCALED,
    );
    for index in ACTIONS {
      let cell = toolbar_button(panel, index, UNSCALED);
      assert!(cell.left >= panel.left && cell.right <= panel.right);
      assert!(cell.top >= panel.top && cell.bottom < panel.bottom);
      assert_eq!(
        toolbar_index(
          panel,
          (
            (cell.left + cell.right) as f64 / 2.0,
            (cell.top + cell.bottom) as f64 / 2.0
          ),
          UNSCALED
        ),
        Some(index)
      );
    }
    assert_eq!(
      toolbar_index(
        panel,
        (panel.left as f64 + 2.0, panel.top as f64 + 2.0),
        UNSCALED
      ),
      None
    );
  }
}

#[test]
fn toolbar_preserves_room_for_dimensions_at_top_and_bottom_edges() {
  let bounds = RECT {
    left: 0,
    top: 0,
    right: 960,
    bottom: 640,
  };
  let top_crop = Rect {
    x: 100.0,
    y: 0.0,
    w: 600.0,
    h: 100.0,
  };
  let top_toolbar = layout_toolbar(top_crop, bounds, UNSCALED);
  assert!(top_toolbar.top >= 174);
  let bottom_crop = Rect {
    x: 100.0,
    y: 500.0,
    w: 600.0,
    h: 140.0,
  };
  let bottom_toolbar = layout_toolbar(bottom_crop, bounds, UNSCALED);
  assert!(bottom_toolbar.bottom <= 426);
}

#[test]
fn full_screen_crop_puts_toolbar_inside_its_bottom_edge_clear_of_dimensions() {
  let bounds = RECT {
    left: 0,
    top: 0,
    right: 960,
    bottom: 640,
  };
  let crop = Rect {
    x: 0.0,
    y: 0.0,
    w: 960.0,
    h: 640.0,
  };
  let toolbar = layout_toolbar(crop, bounds, UNSCALED);
  assert_eq!(
    toolbar.bottom,
    640 - 12,
    "12px inside the crop's bottom edge"
  );
  let label = crate::overlay::gdi::drawing::selection_label_bounds(bounds, 960, 640);
  assert!(
    toolbar.top >= label.bottom,
    "toolbar {toolbar:?} must not cover dimensions {label:?}"
  );
}

#[test]
fn metrics_scale_toolbar_lengths_and_reject_invalid_dpi() {
  assert_eq!(Metrics::new(1.0), UNSCALED);
  assert_eq!(
    Metrics::new(0.5),
    UNSCALED,
    "Windows never scales below 100%"
  );
  assert_eq!(Metrics::new(f64::NAN), UNSCALED);
  assert_eq!(Metrics::new(f64::INFINITY), UNSCALED);
  assert_eq!(Metrics::new(9.0).scale, 5.0);
  let m = Metrics::new(1.5);
  assert_eq!((m.cell, m.gap, m.pad, m.grip), (57, 6, 9, 48));
  assert_eq!((m.vertical_pad, m.group_gap, m.badge), (9, 12, 27));
  assert_eq!(m.px(34), 51);
}

#[test]
fn scaled_toolbar_layout_and_hit_testing_invert_at_common_dpi_scales() {
  for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 3.0, 3.5, 5.0] {
    let m = Metrics::new(scale);
    for (width, single_row) in [(m.px(800), true), (m.px(280), false)] {
      let bounds = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: m.px(600),
      };
      let crop = Rect {
        x: 20.0,
        y: 20.0,
        w: 180.0,
        h: 120.0,
      };
      let toolbar = layout_toolbar(crop, bounds, m);
      assert!(toolbar.left >= 0 && toolbar.right <= width, "{scale}x fits");
      assert_eq!(
        toolbar_columns(toolbar, m) == BUTTONS,
        single_row,
        "{scale}x"
      );
      if single_row {
        assert_eq!(
          toolbar.bottom - toolbar.top,
          m.cell - m.gap + m.vertical_pad * 2,
          "{scale}x height"
        );
      }
      let grip = toolbar_grip(toolbar, m);
      let cells: Vec<RECT> = ACTIONS
        .into_iter()
        .map(|index| toolbar_button(toolbar, index, m))
        .collect();
      for (cell, index) in cells.iter().zip(ACTIONS) {
        assert_eq!(
          cell.right - cell.left,
          m.cell - m.gap,
          "{scale}x button width"
        );
        assert!(cell.left >= grip.right && cell.right <= toolbar.right - m.pad + 1);
        assert!(cell.top >= toolbar.top && cell.bottom <= toolbar.bottom);
        let center = (
          (cell.left + cell.right) as f64 / 2.0,
          (cell.top + cell.bottom) as f64 / 2.0,
        );
        assert_eq!(toolbar_index(toolbar, center, m), Some(index), "{scale}x");
      }
      for (a, first) in cells.iter().enumerate() {
        for second in &cells[a + 1..] {
          let overlap = crate::overlay::screenshot::cache::intersect(*first, *second);
          assert!(
            overlap.right <= overlap.left || overlap.bottom <= overlap.top,
            "{scale}x buttons overlap"
          );
        }
      }
    }
  }
}

#[test]
fn editor_toolbar_and_pickers_follow_the_crop_monitor_dpi() {
  let monitor = |x: i32, width: u32, dpi_scale: f64| crate::monitors::MonitorInfo {
    id: format!("test-{x}"),
    name: format!("Test {x}"),
    x,
    y: 0,
    width,
    height: 1440,
    is_primary: x == 0,
    dpi_scale,
    logical_x: (f64::from(x) / dpi_scale) as i32,
    logical_y: 0,
    logical_width: (f64::from(width) / dpi_scale) as u32,
    logical_height: (1440.0 / dpi_scale) as u32,
  };
  let mut controller = SelectionControllerState::new(5120, 1440, 0, 0);
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors = vec![monitor(0, 2560, 1.0), monitor(2560, 2560, 1.5)];
  let confirm = |controller: &mut SelectionControllerState, x: f64| {
    let rect = Rect {
      x,
      y: 200.0,
      w: 1200.0,
      h: 600.0,
    };
    controller.interaction = SelectionInteraction::Confirmed {
      rect,
      from_snap: false,
      moving: None,
      resizing: None,
    };
    rect
  };
  let unscaled_rect = confirm(&mut controller, 400.0);
  let unscaled = editor.toolbar_rect(&controller, unscaled_rect);
  assert_eq!(editor.metrics(&controller, unscaled_rect), UNSCALED);
  let scaled_rect = confirm(&mut controller, 3000.0);
  let m = editor.metrics(&controller, scaled_rect);
  assert_eq!(m, Metrics::new(1.5));
  let scaled = editor.toolbar_rect(&controller, scaled_rect);
  assert_eq!(
    scaled.bottom - scaled.top,
    m.cell - m.gap + m.vertical_pad * 2
  );
  assert!(
    (scaled.right - scaled.left) * 2 >= (unscaled.right - unscaled.left) * 3 - 4,
    "150% toolbar is half again as wide"
  );
  assert!(scaled.left >= 2560, "toolbar stays on the crop monitor");
  editor.color_open = true;
  let panel = editor.picker_rect(&controller, scaled_rect);
  assert_eq!(panel.right - panel.left, m.px(176));
  let swatch = color_cell(panel, 5, m);
  controller.cursor_pos = (
    (swatch.left + swatch.right) as f64 / 2.0,
    (swatch.top + swatch.bottom) as f64 / 2.0,
  );
  assert_eq!(editor.current_hover_key(&controller), Some(25));
  let pen = toolbar_button(scaled, 1, m);
  editor.color_open = false;
  controller.cursor_pos = (pen.right as f64 - 2.0, pen.bottom as f64 - 2.0);
  assert_eq!(
    editor.current_hover_key(&controller),
    Some(1),
    "scaled hit targets reach the far corner of each button"
  );
}

#[test]
fn popovers_center_on_their_button_and_carets_point_where_they_open() {
  let mut controller = SelectionControllerState::new(1280, 800, 0, 0);
  let rect = Rect {
    x: 200.0,
    y: 200.0,
    w: 880.0,
    h: 300.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  let m = editor.metrics(&controller, rect);
  for (button, tool) in [(7, Tool::Pen), (11, Tool::Pen), (11, Tool::Text)] {
    editor.tool = tool;
    editor.color_open = button == 7;
    editor.width_open = button == 11;
    for position in [None, Some((300, 0))] {
      editor.toolbar_position = position;
      let toolbar = editor.toolbar_rect(&controller, rect);
      let cell = toolbar_button(toolbar, button, m);
      let panel = editor.picker_rect(&controller, rect);
      assert!(
        ((panel.left + panel.right) / 2 - (cell.left + cell.right) / 2).abs() <= 1,
        "button {button} popover centers on it"
      );
      let above = panel.bottom <= toolbar.top;
      assert_eq!(above, position.is_none(), "top-edge toolbar opens below");
      assert!(above || panel.top >= toolbar.bottom);
      assert_eq!(editor.popover_opens_up(&controller, rect, button), above);
    }
  }
  // Near the monitor edge the popover clamps instead of leaving the screen.
  let bounds = RECT {
    left: 0,
    top: 0,
    right: 1280,
    bottom: 800,
  };
  let toolbar = RECT {
    left: 900,
    top: 700,
    right: 1280,
    bottom: 746,
  };
  for (anchor_left, expected_left) in [(1240, 1280 - 176), (-10, 0)] {
    let anchor = RECT {
      left: anchor_left,
      top: 706,
      right: anchor_left + 34,
      bottom: 740,
    };
    let panel = layout_popover_at(toolbar, anchor, bounds, 176, 128, UNSCALED);
    assert_eq!(panel.left, expected_left);
    assert_eq!(panel.right - panel.left, 176);
  }
}

#[test]
fn every_toolbar_button_uses_its_reviewed_icon_and_selected_tools_fill() {
  use super::super::toolbar::toolbar_icon;
  for index in ACTIONS {
    match index {
      // The color swatch and size label draw their own content.
      7 | 11 => assert_eq!(toolbar_icon(index, false), None),
      5 | 6 => {
        let icon = toolbar_icon(index, true).expect("history icon");
        assert_eq!(toolbar_icon(index, false), Some(icon));
        assert!(icon.file().ends_with("_regular.svg"));
      },
      _ => {
        let rest = toolbar_icon(index, false).expect("tool icon");
        let selected = toolbar_icon(index, true).expect("selected tool icon");
        assert!(rest.file().ends_with("_20_regular.svg"), "{}", rest.file());
        assert_eq!(
          selected.file(),
          rest.file().replace("_regular", "_filled"),
          "selected tool fills the same icon"
        );
      },
    }
  }
  assert_eq!(toolbar_icon(99, false), None);
}

#[test]
fn the_font_sample_previews_the_hovered_size_and_returns_to_the_selected_one() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use std::time::Duration;
  let mut controller = SelectionControllerState::new(1280, 800, 0, 0);
  let rect = Rect {
    x: 80.0,
    y: 80.0,
    w: 1050.0,
    h: 400.0,
  };
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  editor.tool = Tool::Text;
  editor.width_open = true;
  let age = Duration::from_secs(1);
  editor.motion = crate::overlay::motion::Motion::preview(age, age, age, 2, None);
  let panel = editor.picker_rect(&controller, rect);
  let sample = |editor: &mut AnnotationEditor, controller: &SelectionControllerState| {
    let mut image = ScreenshotBitmap::from_rgba(&vec![0; 1280 * 800 * 4], 1280, 800).unwrap();
    editor
      .draw_toolbar(controller, image.dc, HFONT::default(), rect)
      .unwrap();
    let pixels = image.to_rgba().unwrap();
    let (left, right) = (panel.left + 8, panel.right - 8);
    (panel.top + 8..panel.top + 88)
      .flat_map(|y| (left..right).map(move |x| (y, x)))
      .map(|(y, x)| {
        let at = (y as usize * 1280 + x as usize) * 4;
        [pixels[at], pixels[at + 1], pixels[at + 2]]
      })
      .collect::<Vec<_>>()
  };
  let hover = |controller: &mut SelectionControllerState, index: i32| {
    let cell = font_cell(panel, index, UNSCALED);
    controller.cursor_pos = (
      ((cell.left + cell.right) / 2) as f64,
      ((cell.top + cell.bottom) / 2) as f64,
    );
  };
  assert_eq!(editor.text_size, 24);
  controller.cursor_pos = (0.0, 0.0);
  let resting = sample(&mut editor, &controller);
  // Hovering 64 shows 64 before any click; the selection itself is unchanged.
  hover(&mut controller, 5);
  assert_eq!(
    editor.font_sample_size(editor.current_hover_key(&controller)),
    64
  );
  let previewed = sample(&mut editor, &controller);
  assert_ne!(previewed, resting);
  assert_eq!(editor.text_size, 24);
  // Hovering the selected size, or leaving the choices, shows the selected size again.
  hover(&mut controller, 2);
  assert_eq!(sample(&mut editor, &controller), resting);
  let style = style_menu_button(panel, UNSCALED);
  controller.cursor_pos = (
    ((style.left + style.right) / 2) as f64,
    ((style.top + style.bottom) / 2) as f64,
  );
  assert_eq!(
    editor.font_sample_size(editor.current_hover_key(&controller)),
    24
  );
  assert_eq!(editor.font_sample_size(Some(20)), 24);
  assert_eq!(editor.font_sample_size(None), 24);
}
