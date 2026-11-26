use super::layout::*;
use super::*;
use crate::overlay::selection::SelectionInteraction;
use crate::overlay::selection_core::SelectionControllerState;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::HFONT;

struct HiddenParent(HWND);
impl HiddenParent {
  fn new() -> Self {
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WINDOW_EX_STYLE, WS_POPUP};
    // SAFETY: built-in class, hidden thread-owned fixture; no focus or desktop input.
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
      .expect("hidden parent"),
    )
  }
}
impl Drop for HiddenParent {
  fn drop(&mut self) {
    // SAFETY: releases only this fixture's owned hidden window on its creating thread.
    if let Err(error) = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0) } {
      tracing::warn!(%error, "Error fixture cleanup failed");
    }
  }
}

fn fixture() -> (AnnotationEditor, SelectionControllerState, Rect) {
  let rect = Rect {
    x: 60.0,
    y: 50.0,
    w: 600.0,
    h: 350.0,
  };
  let mut controller = SelectionControllerState::new(800, 600, 0, 0);
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  editor.tool = Tool::Text;
  (editor, controller, rect)
}

#[test]
fn error_badge_keeps_toolbar_geometry_and_repeated_failures_do_not_restart_tooltip() {
  let (mut editor, mut controller, rect) = fixture();
  let normal = editor.toolbar_rect(&controller, rect);
  editor.text_error = true;
  assert_eq!(editor.toolbar_rect(&controller, rect), normal);
  let badge = error_badge_rect(normal, 13, editor.metrics(&controller, rect));
  controller.cursor_pos = (
    (badge.left + badge.right) as f64 / 2.0,
    (badge.top + badge.bottom) as f64 / 2.0,
  );
  assert_eq!(editor.current_hover_key(&controller), Some(13));
  let now = std::time::Instant::now();
  editor.motion = crate::overlay::motion::Motion::preview(
    std::time::Duration::from_secs(1),
    std::time::Duration::ZERO,
    std::time::Duration::ZERO,
    0,
    None,
  );
  editor.update_error_motion(&controller, now);
  assert_eq!(editor.error_fade.value(now), 0.0);
  assert!(editor.animation_active());
  let mid = now + std::time::Duration::from_millis(70);
  let alpha = editor.error_fade.value(mid);
  assert!(alpha > 0.0 && alpha < 1.0);
  for _ in 0..100 {
    editor.text_error = true;
    editor.update_error_motion(&controller, mid);
    assert_eq!(editor.error_fade.value(mid), alpha);
  }
  let end = now + std::time::Duration::from_millis(140);
  editor.update_error_motion(&controller, end);
  assert_eq!(editor.error_fade.value(end), 1.0);
  assert!(!editor.error_frame_pending);
  controller.cursor_pos = (0.0, 0.0);
  editor.update_error_motion(&controller, end);
  editor.update_error_motion(&controller, end + std::time::Duration::from_millis(140));
  assert_eq!(
    editor
      .error_fade
      .value(end + std::time::Duration::from_millis(140)),
    0.0
  );
  assert!(!editor.error_frame_pending);
}

#[test]
fn badge_click_keyboard_dismissal_tool_switch_and_success_preserve_drawings() {
  let parent = HiddenParent::new();
  let (mut editor, mut controller, rect) = fixture();
  editor.text_error = true;
  let m = editor.metrics(&controller, rect);
  let toolbar = editor.toolbar_rect(&controller, rect);
  let text = toolbar_button(toolbar, 13, m);
  controller.cursor_pos = (
    (text.left + text.right) as f64 / 2.0,
    (text.top + text.bottom) as f64 / 2.0,
  );
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.error_details_pinned);
  assert!(editor.pointer_up(&controller, parent.0));
  assert!(editor.keydown(&controller, parent.0, 0x1b));
  assert!(!editor.error_details_pinned);
  assert!(editor.toolbar_error().is_some());
  assert!(editor.keydown(&controller, parent.0, 0x70));
  assert!(editor.error_details_pinned);
  let pen = toolbar_button(toolbar, 1, m);
  controller.cursor_pos = (
    (pen.left + pen.right) as f64 / 2.0,
    (pen.top + pen.bottom) as f64 / 2.0,
  );
  assert!(editor.pointer_down(&controller, parent.0));
  assert!(editor.pointer_up(&controller, parent.0));
  assert_eq!(editor.tool, Tool::Pen);
  assert_eq!(editor.toolbar_error(), None);
  editor.report_render_failure();
  assert_eq!(editor.toolbar_error().expect("failure").1, 1);
  editor.place_marker((100, 100));
  assert_eq!(editor.toolbar_error(), None);
  assert_eq!(editor.annotations.len(), 1);
  assert!(editor.history.can_undo());
}

#[test]
fn dismissing_error_chrome_restores_cached_pixels_without_exporting_it() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use windows::Win32::Graphics::Gdi::GdiFlush;
  let (mut editor, controller, rect) = fixture();
  let source: Vec<u8> = (0..800 * 600).flat_map(|_| [35, 38, 42, 255]).collect();
  let mut target = ScreenshotBitmap::from_rgba(&source, 800, 600).expect("DIB");
  let damage = RECT {
    left: 0,
    top: 0,
    right: 800,
    bottom: 600,
  };
  editor.paint_cached(&controller, target.dc, HFONT::default(), damage, false);
  // SAFETY: synchronize this thread's owned test DIB before pixel readback.
  assert!(unsafe { GdiFlush() }.as_bool());
  let baseline = target.to_rgba().expect("normal pixels");
  editor.text_error = true;
  editor.error_details_pinned = true;
  let region = editor.chrome_region(&controller, rect);
  assert!(
    editor
      .restore_scene(&controller, target.dc, region)
      .is_some()
  );
  editor.paint_cached(&controller, target.dc, HFONT::default(), region, true);
  // SAFETY: same owned DIB after native rendering.
  assert!(unsafe { GdiFlush() }.as_bool());
  assert_ne!(target.to_rgba().expect("error pixels"), baseline);
  assert_eq!(editor.cache_key(&controller).expect("key").revision, 0);
  assert!(
    editor.annotations.is_empty(),
    "error chrome never becomes an annotation"
  );
  editor.clear_toolbar_error();
  assert!(
    editor
      .restore_scene(&controller, target.dc, region)
      .is_some()
  );
  editor.paint_cached(&controller, target.dc, HFONT::default(), region, true);
  // SAFETY: synchronize the restored DIB before exact comparison.
  assert!(unsafe { GdiFlush() }.as_bool());
  assert_eq!(target.to_rgba().expect("restored pixels"), baseline);
}

#[test]
fn tooltip_copy_fits_all_locales_and_screen_edges_with_reduced_motion() {
  let (mut editor, mut controller, rect) = fixture();
  editor.text_error = true;
  editor.error_details_pinned = true;
  editor.update_error_motion(&controller, std::time::Instant::now());
  assert_eq!(editor.error_fade.value(std::time::Instant::now()), 1.0);
  assert!(!editor.error_frame_pending);
  for lang in ["en", "fr", "de", "es", "it"] {
    for key in [
      "annotation_error_text",
      "annotation_error_render",
      "annotation_error_undo",
      "annotation_error_redo",
      "annotation_error_drawings",
      "annotation_error_stroke",
      "annotation_error_marker",
      "annotation_error_identifier",
    ] {
      let copy = crate::i18n::native_settings(lang, key).expect("catalog");
      let (width, height) = editor
        .error_chrome
        .measure_text_box(&copy, crate::overlay::chrome::TextStyle::DETAILS, 276.0)
        .expect("measure");
      assert!(width <= 276.0 && height + 20.0 <= ERROR_TOOLTIP_HEIGHT as f32);
    }
  }
  for (width, height, position) in [
    (800, 600, (0, 0)),
    (800, 600, (700, 590)),
    (320, 240, (20, 150)),
  ] {
    controller.width = width;
    controller.height = height;
    editor.toolbar_position = Some(position);
    let toolbar = editor.toolbar_rect(&controller, rect);
    let bounds = RECT {
      left: 0,
      top: 0,
      right: width,
      bottom: height,
    };
    let panel = error_tooltip_rect(
      toolbar,
      13,
      bounds,
      300,
      60,
      editor.metrics(&controller, rect),
    );
    assert!(panel.left >= 0 && panel.top >= 0 && panel.right <= width && panel.bottom <= height);
    let region = editor.chrome_region(&controller, rect);
    assert!(
      region.left <= panel.left
        && region.right >= panel.right
        && region.top <= panel.top
        && region.bottom >= panel.bottom
    );
  }
}
#[test]
fn limits_use_shared_concise_catalogs_and_history_eviction_is_quiet() {
  for lang in ["en", "fr", "de", "es", "it"] {
    let mut editor = AnnotationEditor::new(lang.into());
    editor.history.limited = true;
    for (reason, expected) in [
      (DrawingLimit::Stroke, "annotation_error_stroke"),
      (DrawingLimit::Drawings, "annotation_error_drawings"),
      (DrawingLimit::Marker, "annotation_error_marker"),
      (DrawingLimit::Identifier, "annotation_error_identifier"),
    ] {
      editor.limited = Some(reason);
      assert_eq!(editor.toolbar_error().expect("limit").0, expected);
      let message = crate::i18n::native_settings(lang, expected).expect("catalog");
      assert!(!message.is_empty());
      assert!(message.chars().count() < 65);
    }
    editor.limited = None;
    assert_eq!(editor.toolbar_error(), None);
  }
}
