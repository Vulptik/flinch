use super::layout::*;
use super::objects::shortcut_tool;
use super::tooltip::{ButtonTip, RESHOW_GRACE, TOOLTIP_DELAY, tooltip_key};
use super::*;
use crate::overlay::selection::SelectionInteraction;
use crate::overlay::selection_core::SelectionControllerState;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::RECT;

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
  (editor, controller, rect)
}

fn hover(editor: &AnnotationEditor, controller: &mut SelectionControllerState, button: i32) {
  let rect = AnnotationEditor::confirmed(controller).expect("confirmed crop");
  let cell = toolbar_button(
    editor.toolbar_rect(controller, rect),
    button,
    editor.metrics(controller, rect),
  );
  controller.cursor_pos = (
    f64::from(cell.left + cell.right) / 2.0,
    f64::from(cell.top + cell.bottom) / 2.0,
  );
}

#[test]
fn a_tooltip_waits_for_the_hover_delay_then_switches_between_buttons_at_once() {
  let mut tip = ButtonTip::new();
  let start = Instant::now();
  tip.update(Some(1), false, start, false);
  assert_eq!(tip.shown(), None);
  assert!(tip.active());
  assert!(!tip.repaint_due(start + TOOLTIP_DELAY - Duration::from_millis(1)));
  assert!(tip.repaint_due(start + TOOLTIP_DELAY));
  tip.update(Some(1), false, start + TOOLTIP_DELAY, false);
  assert_eq!(tip.shown(), Some(1));
  assert!(!tip.active());

  let shown = start + TOOLTIP_DELAY;
  tip.update(Some(2), false, shown, false);
  assert_eq!(tip.shown(), Some(2));
  // The gap between buttons keeps tooltips shown.
  tip.update(None, false, shown, false);
  assert_eq!(tip.shown(), None);
  tip.update(Some(3), false, shown + RESHOW_GRACE / 2, false);
  assert_eq!(tip.shown(), Some(3));
  // After the grace, the next button waits again.
  tip.update(None, false, shown, false);
  let later = shown + RESHOW_GRACE;
  tip.update(Some(4), false, later, false);
  assert_eq!(tip.shown(), None);
  tip.update(Some(4), false, later + TOOLTIP_DELAY, false);
  assert_eq!(tip.shown(), Some(4));
}

#[test]
fn clicking_hides_the_tooltip_until_the_pointer_leaves_the_button() {
  let mut tip = ButtonTip::new();
  let start = Instant::now();
  tip.update(Some(5), false, start, false);
  tip.update(Some(5), false, start + TOOLTIP_DELAY, false);
  assert_eq!(tip.shown(), Some(5));
  tip.update(Some(5), true, start + TOOLTIP_DELAY, false);
  assert_eq!(tip.shown(), None);
  assert!(!tip.active());
  let released = start + TOOLTIP_DELAY * 4;
  tip.update(Some(5), false, released, false);
  tip.update(Some(5), false, released + TOOLTIP_DELAY, false);
  assert_eq!(tip.shown(), None);
  assert!(!tip.active());
  tip.update(None, false, released + TOOLTIP_DELAY, false);
  tip.update(Some(5), false, released + TOOLTIP_DELAY * 2, false);
  tip.update(Some(5), false, released + TOOLTIP_DELAY * 3, false);
  assert_eq!(tip.shown(), Some(5));
}

#[test]
fn tooltip_fades_in_and_out_and_stops_scheduling_frames_when_settled() {
  let mut tip = ButtonTip::new();
  let start = Instant::now();
  let due = start + TOOLTIP_DELAY;
  tip.update(Some(1), false, start, true);
  tip.update(Some(1), false, due, true);
  assert!(tip.active() && tip.repaint_due(due));
  tip.update(Some(1), false, due + Duration::from_millis(140), true);
  assert!(!tip.active());
  let gone = due + Duration::from_millis(140);
  tip.update(None, false, gone, true);
  assert!(tip.active());
  tip.update(None, false, gone + Duration::from_millis(140), true);
  assert!(!tip.active());
  assert_eq!(tip.shown(), None);
}

#[test]
fn only_an_idle_toolbar_button_without_other_chrome_gets_a_tooltip() {
  let (mut editor, mut controller, _) = fixture();
  hover(&editor, &mut controller, 1);
  let start = Instant::now();
  editor.update_button_tip(&controller, start);
  editor.update_button_tip(&controller, start + TOOLTIP_DELAY);
  assert_eq!(editor.button_tip.shown(), Some(1));

  editor.color_open = true;
  editor.update_button_tip(&controller, start + TOOLTIP_DELAY);
  assert_eq!(editor.button_tip.shown(), None);
  editor.color_open = false;

  // An errored button shows its error details instead.
  editor.render_failed = true;
  editor.error_button = Some(1);
  editor.update_button_tip(&controller, start + TOOLTIP_DELAY * 3);
  editor.update_button_tip(&controller, start + TOOLTIP_DELAY * 5);
  assert_eq!(editor.button_tip.shown(), None);
  editor.error_button = Some(13);
  editor.error_details_pinned = true;
  editor.update_button_tip(&controller, start + TOOLTIP_DELAY * 7);
  editor.update_button_tip(&controller, start + TOOLTIP_DELAY * 9);
  assert_eq!(editor.button_tip.shown(), None);
  editor.clear_toolbar_error();

  controller.cursor_pos = (0.0, 0.0);
  editor.update_button_tip(&controller, start + TOOLTIP_DELAY * 11);
  assert_eq!(editor.button_tip.shown(), None);
}

#[test]
fn every_button_names_itself_and_the_shortcut_it_really_uses_in_every_locale() {
  let (mut editor, _, _) = fixture();
  for lang in ["en", "fr", "de", "es", "it"] {
    for button in ACTIONS {
      let key = tooltip_key(button).expect("tooltip copy");
      let copy = crate::i18n::native_settings(lang, key).expect("catalog");
      let (width, height) = editor
        .error_chrome
        .measure_text_box(
          &copy,
          crate::overlay::chrome::TextStyle::COORDINATES,
          (ERROR_TOOLTIP_WIDTH - 16) as f32,
        )
        .expect("measure");
      assert!(width <= 220.0 && height < 24.0, "{lang} {key} is too long");
      let shortcut = copy
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once(" ("))
        .map(|(_, shortcut)| shortcut);
      match button {
        7 | 11 => assert_eq!(shortcut, None, "{lang} {key}"),
        5 => assert!(shortcut.is_some_and(|s| s.ends_with("+Z")), "{lang} {key}"),
        6 => assert!(shortcut.is_some_and(|s| s.ends_with("+Y")), "{lang} {key}"),
        _ => {
          let letter = shortcut.expect("letter shortcut");
          assert_eq!(letter.len(), 1, "{lang} {key}");
          let vk = u32::from(letter.as_bytes()[0]);
          let tool = match button {
            12 => {
              assert_eq!(vk, 0x53, "{lang} {key}");
              continue;
            },
            0..=4 => TOOLS[button as usize],
            8 => Tool::Highlighter,
            16 => Tool::Blur,
            13 => Tool::Text,
            14 => Tool::Marker,
            _ => unreachable!("every drawing button is listed"),
          };
          assert_eq!(shortcut_tool(vk), Some(tool), "{lang} {key}");
        },
      }
    }
  }
}

#[test]
fn tooltips_center_on_their_button_and_stay_inside_the_monitor_and_damage_envelope() {
  let (mut editor, mut controller, rect) = fixture();
  for (width, height, position) in [
    (800, 600, (0, 0)),
    (800, 600, (700, 590)),
    (320, 240, (20, 150)),
  ] {
    controller.width = width;
    controller.height = height;
    editor.toolbar_position = Some(position);
    let toolbar = editor.toolbar_rect(&controller, rect);
    let m = editor.metrics(&controller, rect);
    let bounds = RECT {
      left: 0,
      top: 0,
      right: width,
      bottom: height,
    };
    let region = editor.chrome_region(&controller, rect);
    for button in ACTIONS {
      let panel = button_tooltip_rect(toolbar, button, bounds, 180, 28, m);
      assert!(panel.left >= 0 && panel.top >= 0 && panel.right <= width && panel.bottom <= height);
      assert!(
        region.left <= panel.left
          && region.right >= panel.right
          && region.top <= panel.top
          && region.bottom >= panel.bottom
      );
      assert!(panel.bottom <= toolbar.top || panel.top >= toolbar.bottom);
    }
  }
  controller.width = 800;
  controller.height = 600;
  editor.toolbar_position = None;
  let toolbar = editor.toolbar_rect(&controller, rect);
  let m = editor.metrics(&controller, rect);
  let cell = toolbar_button(toolbar, 2, m);
  let panel = button_tooltip_rect(
    toolbar,
    2,
    RECT {
      left: 0,
      top: 0,
      right: 800,
      bottom: 600,
    },
    80,
    28,
    m,
  );
  assert!((panel.left + panel.right - cell.left - cell.right).abs() <= 1);
}

#[test]
fn a_shown_tooltip_paints_chrome_without_failing() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  let (mut editor, mut controller, rect) = fixture();
  hover(&editor, &mut controller, 5);
  editor.button_tip.show_now(5);
  let mut target = ScreenshotBitmap::from_rgba(&vec![0; 800 * 600 * 4], 800, 600).expect("target");
  let toolbar = editor.toolbar_rect(&controller, rect);
  let m = editor.metrics(&controller, rect);
  editor
    .draw_button_tip(&controller, target.dc, rect, toolbar, m)
    .expect("tooltip paints");
  let pixels = target.to_rgba().expect("readback");
  let panel = button_tooltip_rect(
    toolbar,
    5,
    RECT {
      left: 0,
      top: 0,
      right: 800,
      bottom: 600,
    },
    1,
    1,
    m,
  );
  let at = ((panel.top as usize + 1) * 800 + panel.left as usize) * 4;
  assert_ne!(
    &pixels[at..at + 3],
    &[0, 0, 0],
    "tooltip surface was not painted"
  );
}
