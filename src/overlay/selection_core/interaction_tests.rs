use super::*;

#[test]
fn selection_native_surfaces_initialize_and_release_without_presenting_or_clipping_input() {
  use windows::{Win32::UI::WindowsAndMessaging::*, core::w};
  // A built-in hidden STATIC parent needs no application callback or message loop.
  // SAFETY: thread-owned hidden window; no activation, input capture or presentation.
  let hwnd = unsafe {
    CreateWindowExW(
      WINDOW_EX_STYLE::default(),
      w!("STATIC"),
      w!(""),
      WS_POPUP,
      0,
      0,
      64,
      48,
      None,
      None,
      None,
      None,
    )
  }
  .unwrap();
  let mut shared = SharedSelectionOverlayState::new(
    64,
    48,
    Arc::new([12, 34, 56, 255].repeat(64 * 48)),
    0,
    0,
    Instant::now(),
  );
  let mut presented = false;
  initialize_selection_window_with(&mut shared, hwnd, "test.quiet_init", |target| {
    assert_eq!(target, hwnd);
    presented = true;
    false
  })
  .unwrap();
  assert!(presented);
  assert!(!shared.resources.offscreen_dc.is_invalid());
  assert!(!shared.resources.offscreen_bmp.is_invalid());
  assert!(!shared.resources.hfont.is_invalid());
  assert!(shared.resources.screenshot.is_some());
  assert!(!shared.resources.cursor_clipped);
  // SAFETY: query and destroy only this test's hidden window and native resources.
  unsafe {
    assert!(!IsWindowVisible(hwnd).as_bool());
    destroy_selection_window(&mut shared);
    DestroyWindow(hwnd).unwrap();
  }
  let mut missing = SharedSelectionOverlayState::new(1, 1, Arc::new(vec![]), 0, 0, Instant::now());
  missing.resources.rgba_data = None;
  assert!(
    initialize_selection_window_with(&mut missing, HWND::default(), "test.missing", |_| panic!(
      "invalid data must never present"
    ))
    .unwrap_err()
    .to_string()
    .contains("No RGBA data")
  );
  missing.resources.rgba_data = Some(Arc::new(vec![0]));
  assert!(
    initialize_selection_window_with(&mut missing, HWND::default(), "test.short", |_| panic!(
      "invalid data must never present"
    ))
    .unwrap_err()
    .to_string()
    .contains("Failed to create ScreenshotBitmap")
  );
}

fn rect() -> Rect {
  Rect {
    x: 100.0,
    y: 100.0,
    w: 200.0,
    h: 120.0,
  }
}
fn confirmed(c: &mut SelectionControllerState, r: Rect) {
  c.interaction = SelectionInteraction::Confirmed {
    rect: r,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  c.overlay_state = OverlayState::OnHold { rect: r };
}
fn coordinates(c: &SelectionControllerState) -> (f64, f64, f64, f64) {
  if let SelectionInteraction::Confirmed { rect, .. } = c.interaction {
    (rect.x, rect.y, rect.w, rect.h)
  } else {
    panic!("expected confirmed selection")
  }
}

#[test]
fn capture_and_edit_keeps_snap_and_drawn_selections_open_without_alt() {
  let mut c = SelectionControllerState::new(800, 600, -1920, -200);
  c.hold_on_release = true;
  c.cursor_pos = (150.0, 150.0);
  c.interaction = SelectionInteraction::Idle {
    snapped_window: Some(rect()),
  };
  begin_selection_gesture(&mut c);
  assert!(matches!(
    finish_selection_gesture(&mut c),
    SelectionMouseUpOutcome::None
  ));
  assert_eq!(coordinates(&c), (100.0, 100.0, 200.0, 120.0));

  c.interaction = SelectionInteraction::FreeDraw {
    start: (20.0, 30.0),
    current: (300.0, 200.0),
  };
  assert!(matches!(
    finish_selection_gesture(&mut c),
    SelectionMouseUpOutcome::None
  ));
  assert_eq!(coordinates(&c), (20.0, 30.0, 280.0, 170.0));
}

#[test]
fn edit_hint_shows_while_hovering_snapping_and_dragging_until_the_selection_is_confirmed() {
  let mut c = SelectionControllerState::new(800, 600, 0, 0);
  let drag = SelectionInteraction::FreeDraw {
    start: (20.0, 30.0),
    current: (300.0, 200.0),
  };
  c.interaction = drag.clone();
  assert_eq!(c.visible_edit_hint(), None, "overlays without an editor");

  c.edit_hint = Some("Hold Alt to edit");
  assert_eq!(c.visible_edit_hint(), Some("Hold Alt to edit"));
  // Holding Alt does not change the copy, so the line never flickers mid-drag.
  c.modifiers.alt = true;
  assert_eq!(c.visible_edit_hint(), Some("Hold Alt to edit"));
  c.modifiers.alt = false;

  for idle in [
    SelectionInteraction::Idle {
      snapped_window: Some(rect()),
    },
    SelectionInteraction::PendingDecision {
      press_pos: (150.0, 150.0),
      snapped_window: Some(rect()),
    },
  ] {
    c.interaction = idle;
    assert_eq!(
      c.visible_edit_hint(),
      Some("Hold Alt to edit"),
      "snaps accept Alt too"
    );
  }
  confirmed(&mut c, rect());
  assert_eq!(c.visible_edit_hint(), None, "the editor is already open");

  c.interaction = drag;
  c.hold_on_release = true;
  assert_eq!(c.visible_edit_hint(), None, "Capture and Edit");
}

#[test]
fn capture_and_edit_rejects_a_too_small_selection_and_can_try_again() {
  let mut c = SelectionControllerState::new(800, 600, 0, 0);
  c.hold_on_release = true;
  c.interaction = SelectionInteraction::FreeDraw {
    start: (20.0, 30.0),
    current: (21.0, 31.0),
  };
  assert!(matches!(
    finish_selection_gesture(&mut c),
    SelectionMouseUpOutcome::None
  ));
  assert!(matches!(c.interaction, SelectionInteraction::Idle { .. }));
  assert!(c.hold_on_release);
  assert!(c.rejected_too_small);
}

#[test]
fn snap_click_free_draw_minimum_size_and_alt_hold_share_confirmation_rules() {
  for alt in [false, true] {
    let mut c = SelectionControllerState::new(800, 600, -1920, -200);
    c.modifiers.alt = alt;
    c.cursor_pos = (150.0, 150.0);
    c.interaction = SelectionInteraction::Idle {
      snapped_window: Some(rect()),
    };
    begin_selection_gesture(&mut c);
    assert!(matches!(
      c.interaction,
      SelectionInteraction::PendingDecision { .. }
    ));
    match finish_selection_gesture(&mut c) {
      SelectionMouseUpOutcome::Confirm(s) => {
        assert!(!alt);
        assert_eq!((s.x, s.y, s.width, s.height), (100, 100, 200, 120));
      },
      SelectionMouseUpOutcome::None => {
        assert!(alt);
        assert_eq!(coordinates(&c), (100.0, 100.0, 200.0, 120.0));
      },
    }
    reset_selection_to_idle(&mut c);
    c.cursor_pos = (20.0, 30.0);
    begin_selection_gesture(&mut c);
    update_selection_pointer(&mut c, (21.0, 31.0));
    assert!(
      matches!(c.interaction, SelectionInteraction::PendingDecision { .. }),
      "jitter must not become a drag"
    );
    update_selection_pointer(&mut c, (200.0, 150.0));
    assert!(matches!(c.overlay_state, OverlayState::Drawing { .. }));
    update_selection_pointer(&mut c, (220.0, 160.0));
    match finish_selection_gesture(&mut c) {
      SelectionMouseUpOutcome::Confirm(s) => {
        assert!(!alt);
        assert_eq!((s.x, s.y, s.width, s.height), (20, 30, 200, 130));
      },
      SelectionMouseUpOutcome::None => {
        assert!(alt);
        assert_eq!(coordinates(&c), (20.0, 30.0, 200.0, 130.0));
      },
    }
    c.interaction = SelectionInteraction::FreeDraw {
      start: (20.0, 20.0),
      current: (24.0, 80.0),
    };
    assert!(matches!(
      finish_selection_gesture(&mut c),
      SelectionMouseUpOutcome::None
    ));
    assert!(
      matches!(c.interaction, SelectionInteraction::Idle { .. }),
      "undersized draw returns to snapping"
    );
    c.interaction = SelectionInteraction::PendingDecision {
      press_pos: (30.0, 30.0),
      snapped_window: None,
    };
    assert!(matches!(
      finish_selection_gesture(&mut c),
      SelectionMouseUpOutcome::None
    ));
  }
}

#[test]
fn confirmed_selection_moves_clamps_resizes_and_finishes_without_accidental_capture() {
  let mut c = SelectionControllerState::new(800, 600, 0, 0);
  confirmed(&mut c, rect());
  c.cursor_pos = (180.0, 150.0);
  begin_selection_gesture(&mut c);
  update_selection_pointer(&mut c, (900.0, 700.0));
  assert_eq!(coordinates(&c), (600.0, 480.0, 200.0, 120.0));
  assert!(matches!(
    finish_selection_gesture(&mut c),
    SelectionMouseUpOutcome::None
  ));
  assert!(c.interaction.is_confirmed_idle());
  confirmed(&mut c, rect());
  c.cursor_pos = (300.0, 220.0);
  assert!(matches!(selection_hit_for_cursor(&c), HitResult::Handle(_)));
  begin_selection_gesture(&mut c);
  update_selection_pointer(&mut c, (350.0, 250.0));
  assert_eq!(coordinates(&c), (100.0, 100.0, 250.0, 150.0));
  assert!(matches!(
    finish_selection_gesture(&mut c),
    SelectionMouseUpOutcome::None
  ));
  c.cursor_pos = (700.0, 500.0);
  begin_selection_gesture(&mut c);
  assert!(
    matches!(c.interaction, SelectionInteraction::PendingDecision { .. }),
    "outside starts a new gesture"
  );
}

#[test]
fn keyboard_confirm_cancel_nudge_modifiers_and_backspace_have_explicit_state_guards() {
  let mut c = SelectionControllerState::new(800, 600, 0, 0);
  let mut paints = 0;
  assert!(matches!(
    selection_keydown(&mut c, VK_ESCAPE, || paints += 1),
    SelectionKeyOutcome::CancelOverlay
  ));
  assert!(matches!(
    selection_keydown(&mut c, VK_RETURN, || paints += 1),
    SelectionKeyOutcome::None
  ));
  confirmed(&mut c, rect());
  selection_keydown(&mut c, VK_RIGHT, || paints += 1);
  selection_keydown(&mut c, VK_DOWN, || paints += 1);
  assert_eq!(coordinates(&c), (101.0, 101.0, 200.0, 120.0));
  selection_keydown(&mut c, VK_SHIFT, || paints += 1);
  selection_keydown(&mut c, VK_LEFT, || paints += 1);
  selection_keydown(&mut c, VK_UP, || paints += 1);
  assert_eq!(coordinates(&c), (91.0, 91.0, 200.0, 120.0));
  selection_keydown(&mut c, VK_MENU, || paints += 1);
  assert!(c.modifiers.shift && c.modifiers.alt);
  handle_selection_keyup(&mut c, VK_SHIFT);
  handle_selection_keyup(&mut c, VK_MENU);
  assert!(!c.modifiers.shift && !c.modifiers.alt);
  let SelectionKeyOutcome::Confirm(s) = selection_keydown(&mut c, VK_RETURN, || paints += 1) else {
    panic!("Enter must confirm")
  };
  assert_eq!((s.x, s.y, s.width, s.height), (91, 91, 200, 120));
  selection_keydown(&mut c, 0xff, || paints += 1);
  assert_eq!(paints, 4);
  selection_keydown(&mut c, VK_BACK, || paints += 1);
  assert_eq!(paints, 5);
  assert!(matches!(c.interaction, SelectionInteraction::Idle { .. }));
  selection_keydown(&mut c, VK_BACK, || paints += 1);
  assert_eq!(paints, 5, "idle Backspace must not schedule paint");
}

#[test]
fn the_dimensions_panel_never_carries_the_edit_hint() {
  let mut output =
    crate::overlay::gdi::layered::LayeredBitmap::new(800, 600).expect("offscreen DIB");
  let rgba = Arc::new([35, 80, 120, 255].repeat(800 * 600));
  let mut shared = SharedSelectionOverlayState::new(800, 600, rgba.clone(), 0, 0, Instant::now());
  shared.resources.screenshot =
    Some(ScreenshotBitmap::from_rgba(&rgba, 800, 600).expect("synthetic screenshot"));
  // The fixture owns this DC; do not call the HWND resource teardown on borrowed storage.
  shared.resources.offscreen_dc = output.dc;
  shared.resources.indicator_motion.finish();
  let drag = SelectionInteraction::FreeDraw {
    start: (200.0, 300.0),
    current: (500.0, 450.0),
  };
  // The band the panel once grew into above its usual top; the hint now has its own pill.
  let band = |output: &mut crate::overlay::gdi::layered::LayeredBitmap| {
    let (top, bottom) = (300 - 10 - 56 - 18, 300 - 10 - 56);
    let pixels = output.pixels_mut();
    (top..bottom)
      .flat_map(|y| (300..400).map(move |x| (y * 800 + x) * 4))
      .map(|i| pixels[i..i + 3].to_vec())
      .collect::<Vec<_>>()
  };
  let render = |shared: &mut SharedSelectionOverlayState| {
    draw_selection_scene_with_indicator(
      shared,
      HFONT::default(),
      SelectionDrawColors::DEFAULT,
      true,
    );
    // SAFETY: flush GDI writes before inspecting this thread-owned DIB.
    unsafe {
      windows::Win32::Graphics::Gdi::GdiFlush().expect("flush offscreen drawing");
    }
  };

  shared.controller.interaction = drag.clone();
  render(&mut shared);
  let plain = band(&mut output);
  shared.controller.edit_hint = Some("Hold Alt to edit");
  render(&mut shared);
  let hinted = band(&mut output);
  assert_eq!(
    plain, hinted,
    "the panel keeps its two lines while dragging"
  );
}

#[test]
fn production_selection_scene_renders_synthetic_pixels_and_restores_background() {
  let mut output =
    crate::overlay::gdi::layered::LayeredBitmap::new(800, 600).expect("offscreen DIB");
  let rgba = Arc::new([35, 80, 120, 255].repeat(800 * 600));
  let mut shared =
    SharedSelectionOverlayState::new(800, 600, rgba.clone(), -1920, -200, Instant::now());
  shared.resources.screenshot =
    Some(ScreenshotBitmap::from_rgba(&rgba, 800, 600).expect("synthetic screenshot"));
  // The fixture owns this DC; do not call the HWND resource teardown on borrowed storage.
  shared.resources.offscreen_dc = output.dc;
  for alt in [false, true] {
    shared.controller.modifiers.alt = alt;
    for state in [
      SelectionInteraction::Idle {
        snapped_window: Some(rect()),
      },
      SelectionInteraction::PendingDecision {
        press_pos: (10.0, 10.0),
        snapped_window: Some(rect()),
      },
      SelectionInteraction::FreeDraw {
        start: (20.0, 30.0),
        current: (300.0, 200.0),
      },
      SelectionInteraction::Confirmed {
        rect: rect(),
        from_snap: true,
        moving: None,
        resizing: None,
      },
    ] {
      shared.controller.interaction = state;
      shared.controller.snap_anim.update_target(Some(rect()));
      shared.controller.snap_anim.start_time =
        Some(Instant::now() - std::time::Duration::from_secs(1));
      for indicator in [false, true] {
        draw_selection_scene_with_indicator(
          &mut shared,
          HFONT::default(),
          SelectionDrawColors::DEFAULT,
          indicator,
        );
        // SAFETY: flush GDI writes before inspecting this thread-owned DIB.
        unsafe {
          windows::Win32::Graphics::Gdi::GdiFlush().expect("flush offscreen drawing");
        }
        assert!(
          output
            .pixels_mut()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
        );
      }
    }
  }
  confirmed(&mut shared.controller, rect());
  draw_selection_scene(&mut shared, HFONT::default(), SelectionDrawColors::DEFAULT);
  draw_selection_scene_with_indicator(
    &mut shared,
    HFONT::default(),
    SelectionDrawColors::DEFAULT,
    false,
  );
  unsafe {
    windows::Win32::Graphics::Gdi::GdiFlush().expect("flush");
  }
  let index = (150 * 800 + 180) * 4;
  assert_eq!(
    &output.pixels_mut()[index..index + 3],
    &[120, 80, 35],
    "selected interior preserves original image bytes"
  );
}

#[test]
fn snap_preview_shows_the_snapped_window_undimmed_like_a_drag() {
  let mut output =
    crate::overlay::gdi::layered::LayeredBitmap::new(800, 600).expect("offscreen DIB");
  let rgba = Arc::new([35, 80, 120, 255].repeat(800 * 600));
  let mut shared = SharedSelectionOverlayState::new(800, 600, rgba.clone(), 0, 0, Instant::now());
  shared.resources.screenshot =
    Some(ScreenshotBitmap::from_rgba(&rgba, 800, 600).expect("synthetic screenshot"));
  // The fixture owns this DC; do not call the HWND resource teardown on borrowed storage.
  shared.resources.offscreen_dc = output.dc;
  let window = Rect {
    x: 200.0,
    y: 150.0,
    w: 300.0,
    h: 200.0,
  };
  for state in [
    SelectionInteraction::Idle {
      snapped_window: Some(window),
    },
    SelectionInteraction::PendingDecision {
      press_pos: (300.0, 250.0),
      snapped_window: Some(window),
    },
  ] {
    shared.controller.interaction = state;
    shared.controller.snap_anim.update_target(Some(window));
    shared.controller.snap_anim.start_time =
      Some(Instant::now() - std::time::Duration::from_secs(1));
    draw_selection_scene_with_indicator(
      &mut shared,
      HFONT::default(),
      SelectionDrawColors::DEFAULT,
      false,
    );
    // SAFETY: flush GDI writes before inspecting this thread-owned DIB.
    unsafe {
      windows::Win32::Graphics::Gdi::GdiFlush().expect("flush offscreen drawing");
    }
    let pixel = |output: &mut crate::overlay::gdi::layered::LayeredBitmap, x: usize, y: usize| {
      let index = (y * 800 + x) * 4;
      output.pixels_mut()[index..index + 3].to_vec()
    };
    assert_eq!(
      pixel(&mut output, 350, 250),
      vec![120, 80, 35],
      "snapped window interior keeps original pixels"
    );
    assert_ne!(
      pixel(&mut output, 100, 100),
      vec![120, 80, 35],
      "outside the snapped window stays dimmed"
    );
  }
}
