//! Tray menu rows and placement. Pure, so every taskbar edge and DPI case is testable quietly.

use windows::Win32::Foundation::{POINT, RECT};

/// Window padding and row metrics, in logical pixels; they match `tray-menu.slint`.
const PADDING: f32 = 4.0;
const ROW: f32 = 32.0;
const SEPARATOR: f32 = 9.0;
/// The submenu's first row lines up with the row that opened it.
const SUBMENU_PADDING: f32 = 4.0;
const MIN_WIDTH: f32 = 232.0;
const CHOICE_MIN_WIDTH: f32 = 160.0;
/// Row padding on each side, icon, and the spacing between a row's parts.
const ROW_PADDING: f32 = 10.0;
const ICON: f32 = 16.0;
const SPACING: f32 = 10.0;
const CHEVRON: f32 = 12.0;
const RECENT_ROW: f32 = 52.0;
const RECENT_EMPTY: f32 = 36.0;
/// Text is measured to whole pixels; this keeps elision from trimming a label that just fits.
const TEXT_SLACK: f32 = 2.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
  pub id: &'static str,
  pub label: String,
  /// A short value shown after the label, such as the current Capture Delay.
  pub detail: String,
  pub icon: &'static str,
  pub separator: bool,
  pub accent: bool,
  pub submenu: bool,
}

impl Row {
  fn new(id: &'static str, label: &str, icon: &'static str) -> Self {
    Self {
      id,
      label: label.to_owned(),
      detail: String::new(),
      icon,
      separator: false,
      accent: false,
      submenu: false,
    }
  }

  fn after_separator(mut self) -> Self {
    self.separator = true;
    self
  }
}

/// The menu for a recording state: None is idle, Some(false) recording, Some(true) paused.
/// Same actions and order as the native fallback menu. Hotkeys aren't shown beside the actions,
/// which keeps the menu narrow; the Hotkeys row opens that settings page instead.
pub(crate) fn rows(labels: &crate::i18n::TrayLabels, paused: Option<bool>, delay: u32) -> Vec<Row> {
  let mut rows = vec![
    Row::new("screenshot", labels.screenshot, "camera"),
    Row::new("last-region", labels.last_region, "scan"),
    Row::new("capture-edit", labels.capture_edit, "pencil"),
    Row::new("copy-text", labels.copy_text, "type"),
    Row::new("pick-color", labels.color_picker, "pipette"),
  ];
  match paused {
    None => rows.push(Row::new("record", labels.record_start, "record")),
    Some(is_paused) => {
      let mut stop = Row::new("record", labels.record_stop, "stop");
      stop.accent = true;
      let pause = if is_paused {
        Row::new("pause", labels.record_resume, "resume")
      } else {
        Row::new("pause", labels.record_pause, "pause")
      };
      if is_paused {
        rows.extend([pause, stop]);
      } else {
        rows.extend([stop, pause]);
      }
    },
  }
  let mut timer = Row::new("delay", labels.delay, "timer").after_separator();
  timer.submenu = true;
  timer.detail = delay_label(labels, delay).to_owned();
  rows.push(timer);
  let mut recent = Row::new("recent", labels.recent, "history");
  recent.submenu = true;
  rows.push(recent);
  rows.push(Row::new("hotkeys", labels.hotkeys, "keyboard").after_separator());
  rows.push(Row::new("settings", labels.settings, "settings"));
  rows.push(Row::new("quit", labels.quit, "power"));
  rows
}

/// Top of a row inside the menu window, in logical pixels.
pub(crate) fn row_top(rows: &[Row], index: usize) -> f32 {
  let above: f32 = rows
    .iter()
    .take(index)
    .map(|row| ROW + if row.separator { SEPARATOR } else { 0.0 })
    .sum();
  let own = rows
    .get(index)
    .filter(|row| row.separator)
    .map_or(0.0, |_| SEPARATOR);
  PADDING + above + own
}

/// Menu height in logical pixels.
pub(crate) fn menu_height(rows: &[Row]) -> f32 {
  row_top(rows, rows.len()) + PADDING
}

/// Menu width in logical pixels: its widest row, never narrower than the minimum. `measure` gives a
/// label's width.
pub(crate) fn menu_width(rows: &[Row], mut measure: impl FnMut(&str) -> f32) -> f32 {
  rows
    .iter()
    .map(|row| {
      let mut width = 2.0 * PADDING + 2.0 * ROW_PADDING + ICON + SPACING;
      width += measure(&row.label) + TEXT_SLACK;
      if !row.detail.is_empty() {
        width += SPACING + measure(&row.detail);
      }
      if row.submenu {
        width += SPACING + CHEVRON;
      }
      width.ceil()
    })
    .fold(MIN_WIDTH, f32::max)
}

/// Recent captures submenu height in logical pixels: its captures, or the empty line, then the
/// separator and the folder row.
/// The choice shown for a Capture Delay in seconds; an unknown value reads as off.
pub(crate) fn delay_label(labels: &crate::i18n::TrayLabels, seconds: u32) -> &'static str {
  let index = crate::capture_delay::CHOICES
    .iter()
    .position(|choice| *choice == seconds)
    .unwrap_or(0);
  labels
    .delay_choices
    .get(index)
    .copied()
    .unwrap_or(labels.delay_choices[0])
}

/// Height of a choice submenu with `count` rows, in logical pixels.
pub(crate) fn choice_height(count: usize) -> f32 {
  2.0 * PADDING + count as f32 * ROW
}

/// Width of a choice submenu: room for the check, the widest choice and the row padding.
pub(crate) fn choice_width(choices: &[&str], mut measure: impl FnMut(&str) -> f32) -> f32 {
  choices
    .iter()
    .map(|choice| {
      (2.0 * PADDING + 2.0 * ROW_PADDING + ICON + SPACING + measure(choice) + TEXT_SLACK).ceil()
    })
    .fold(CHOICE_MIN_WIDTH, f32::max)
}

pub(crate) fn recent_height(captures: usize) -> f32 {
  let list = if captures == 0 {
    RECENT_EMPTY
  } else {
    captures as f32 * RECENT_ROW
  };
  2.0 * PADDING + list + SEPARATOR + ROW
}

/// Shorten a file name in the middle to fit `max`, keeping a little more of its end, where the
/// app, kind and extension are.
pub(crate) fn fit_middle(name: &str, max: f32, mut measure: impl FnMut(&str) -> f32) -> String {
  if measure(name) <= max {
    return name.to_owned();
  }
  let chars: Vec<char> = name.chars().collect();
  let shortened = |kept: usize| {
    let head = kept * 2 / 5;
    let tail = kept - head;
    let mut text: String = chars[..head].iter().collect();
    text.push('…');
    text.extend(&chars[chars.len() - tail..]);
    text
  };
  // The longest shortening that fits; measured widths grow with the characters kept.
  let (mut low, mut high) = (0, chars.len().saturating_sub(1));
  while low < high {
    let middle = (low + high).div_ceil(2);
    if measure(&shortened(middle)) <= max {
      low = middle;
    } else {
      high = middle - 1;
    }
  }
  shortened(low)
}

/// Physical pixels for a logical length at a monitor scale.
pub(crate) fn physical(length: f32, scale: f32) -> i32 {
  (length * scale).round() as i32
}

/// Place the menu at the cursor like Windows menus: right and down from it when it fits, otherwise
/// flipped to the other side, then kept inside the work area. The cursor sits on the taskbar, so
/// this opens away from whichever screen edge the taskbar is on.
pub(crate) fn place_menu(cursor: POINT, size: (i32, i32), work: RECT) -> POINT {
  let x = if cursor.x + size.0 <= work.right {
    cursor.x
  } else {
    cursor.x - size.0
  };
  let y = if cursor.y + size.1 <= work.bottom {
    cursor.y
  } else {
    cursor.y - size.1
  };
  POINT {
    x: clamp(x, work.left, work.right - size.0),
    y: clamp(y, work.top, work.bottom - size.1),
  }
}

/// Where the menu's content slides in from, in logical pixels: out of the taskbar it was opened
/// from, as Windows menus do; from below when it opened upward elsewhere.
pub(crate) fn menu_slide(cursor: POINT, origin: POINT, work: RECT) -> (f32, f32) {
  const SLIDE: f32 = 8.0;
  if cursor.x < work.left {
    (-SLIDE, 0.0)
  } else if cursor.x >= work.right {
    (SLIDE, 0.0)
  } else if cursor.y < work.top || origin.y >= cursor.y {
    (0.0, -SLIDE)
  } else {
    (0.0, SLIDE)
  }
}

/// Place the submenu beside the menu, its first row level with `row_top` (physical, from the menu
/// top). It opens to the right and flips left when the right side has no room.
pub(crate) fn place_submenu(
  menu: RECT,
  row_top: i32,
  scale: f32,
  size: (i32, i32),
  work: RECT,
) -> POINT {
  let gap = physical(2.0, scale);
  let x = if menu.right + gap + size.0 <= work.right {
    menu.right + gap
  } else {
    menu.left - gap - size.0
  };
  let y = menu.top + row_top - physical(SUBMENU_PADDING, scale);
  POINT {
    x: clamp(x, work.left, work.right - size.0),
    y: clamp(y, work.top, work.bottom - size.1),
  }
}

/// Lower bound wins when the window is larger than the work area, so its top-left stays visible.
fn clamp(value: i32, low: i32, high: i32) -> i32 {
  value.min(high).max(low)
}

#[cfg(test)]
mod tests {
  use super::*;

  const WORK: RECT = RECT {
    left: 0,
    top: 0,
    right: 1920,
    bottom: 1040,
  };

  fn ids(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|row| row.id).collect()
  }

  #[test]
  fn rows_follow_the_recording_state() {
    let labels = crate::i18n::tray("en");
    let idle = rows(labels, None, 0);
    assert_eq!(
      ids(&idle),
      [
        "screenshot",
        "last-region",
        "capture-edit",
        "copy-text",
        "pick-color",
        "record",
        "delay",
        "recent",
        "hotkeys",
        "settings",
        "quit"
      ]
    );
    assert_eq!(
      (idle[1].label.as_str(), idle[1].icon),
      ("Capture Last Region", "scan")
    );
    assert_eq!(idle[4].label, "Color Picker");
    assert_eq!(idle[4].icon, "pipette");
    assert_eq!(idle[8].label, labels.hotkeys);
    assert_eq!(idle[8].icon, "keyboard");
    assert_eq!(idle[5].label, labels.record_start);
    assert!(!idle[5].accent);
    assert!(idle[6].submenu && idle[6].separator);
    assert_eq!(
      (idle[6].label.as_str(), idle[6].icon),
      (labels.delay, "timer")
    );
    assert_eq!(idle[6].detail, "Off", "the row shows the current delay");
    assert!(idle[7].submenu && !idle[7].separator);
    assert!(idle[8].separator && !idle[9].separator && !idle[10].separator);
    assert_eq!(
      idle.iter().filter(|row| row.submenu).count(),
      2,
      "Capture Delay and Recent Captures open submenus"
    );
    assert!(
      idle
        .iter()
        .all(|row| row.id == "delay" || row.detail.is_empty())
    );
    for (seconds, label) in crate::capture_delay::CHOICES
      .into_iter()
      .zip(labels.delay_choices)
    {
      assert_eq!(rows(labels, None, seconds)[6].detail, label);
    }
    assert_eq!(
      rows(labels, None, 7)[6].detail,
      "Off",
      "an unknown delay reads as off"
    );

    let recording = rows(labels, Some(false), 0);
    assert_eq!(ids(&recording)[5..7], ["record", "pause"]);
    assert_eq!(recording[5].label, labels.record_stop);
    assert!(recording[5].accent);
    assert_eq!(recording[6].label, labels.record_pause);

    let paused = rows(labels, Some(true), 0);
    assert_eq!(ids(&paused)[5..7], ["pause", "record"]);
    assert_eq!(paused[5].label, labels.record_resume);
    assert_eq!(paused[5].icon, "resume");
  }

  #[test]
  fn sizes_fit_the_widest_row_and_count_every_row() {
    let rows = rows(crate::i18n::tray("en"), None, 0);
    assert_eq!(menu_height(&rows), 4.0 + 11.0 * 32.0 + 2.0 * 9.0 + 4.0);
    // Twenty logical pixels per character: "Capture Last Region" is the widest row, ahead of
    // "Capture Delay" with its "Off" and its chevron.
    let measure = |text: &str| text.chars().count() as f32 * 20.0;
    let delay = 8.0 + 20.0 + 16.0 + 10.0 + 260.0 + 2.0 + 10.0 + 60.0 + 10.0 + 12.0;
    let last_region = 8.0 + 20.0 + 16.0 + 10.0 + 380.0 + 2.0;
    assert!(last_region > delay);
    assert_eq!(menu_width(&rows, measure), last_region);
    let mut without_last_region = rows.clone();
    without_last_region.retain(|row| row.id != "last-region");
    assert_eq!(menu_width(&without_last_region, measure), delay);
    assert_eq!(
      menu_width(&rows, |_| 1.0),
      232.0,
      "never narrower than the minimum"
    );
    let mut only_recent = rows.clone();
    only_recent.retain(|row| row.id == "recent");
    assert_eq!(
      menu_width(&only_recent, |_| 300.0),
      (8.0f32 + 20.0 + 16.0 + 10.0 + 302.0 + 10.0 + 12.0).ceil(),
      "a submenu row leaves room for its chevron"
    );
    assert_eq!(recent_height(0), 8.0 + 36.0 + 9.0 + 32.0);
    assert_eq!(recent_height(5), 8.0 + 5.0 * 52.0 + 9.0 + 32.0);
    assert_eq!(choice_height(4), 8.0 + 4.0 * 32.0);
    assert_eq!(
      choice_width(&["Off", "10 seconds"], measure),
      8.0 + 20.0 + 16.0 + 10.0 + 200.0 + 2.0,
      "room for the check and the widest choice"
    );
    assert_eq!(choice_width(&["Off"], |_| 1.0), 160.0);
  }

  #[test]
  fn long_names_lose_their_middle_and_keep_more_of_the_end() {
    let measure = |text: &str| text.chars().count() as f32 * 10.0;
    assert_eq!(fit_middle("short.png", 200.0, measure), "short.png");
    let name = "2026-11-08_14-32-05_Discord_screenshot.png";
    let fitted = fit_middle(name, 200.0, measure);
    assert_eq!(fitted, "2026-11…reenshot.png");
    assert_eq!(fitted.chars().count(), 20, "as long as fits");
    assert_eq!(
      fit_middle("ab", 5.0, measure),
      "…",
      "nothing fits but the ellipsis"
    );
    assert_eq!(
      fit_middle("日本語のスクリーンショット.png", 100.0, measure)
        .chars()
        .count(),
      10
    );
  }

  #[test]
  fn row_tops_count_padding_rows_and_separators() {
    let rows = rows(crate::i18n::tray("en"), None, 0);
    assert_eq!(row_top(&rows, 0), 4.0);
    assert_eq!(row_top(&rows, 4), 4.0 + 4.0 * 32.0);
    assert_eq!(
      row_top(&rows, 6),
      4.0 + 6.0 * 32.0 + 9.0,
      "its own separator"
    );
    assert_eq!(row_top(&rows, 7), 4.0 + 7.0 * 32.0 + 9.0);
    assert_eq!(row_top(&rows, 8), 4.0 + 8.0 * 32.0 + 2.0 * 9.0);
    assert_eq!(physical(row_top(&rows, 6), 1.5), 308);
  }

  #[test]
  fn the_menu_opens_away_from_every_taskbar_edge() {
    let size = (260, 300);
    // Bottom taskbar: the cursor is below the work area, so the menu opens upward.
    let bottom = place_menu(POINT { x: 1700, y: 1060 }, size, WORK);
    assert_eq!((bottom.x, bottom.y), (1440, 740));
    // Top taskbar.
    let top_work = RECT {
      top: 40,
      bottom: 1080,
      ..WORK
    };
    let top = place_menu(POINT { x: 1700, y: 20 }, size, top_work);
    assert_eq!((top.x, top.y), (1440, 40));
    // Left taskbar.
    let left_work = RECT {
      left: 60,
      right: 1920,
      ..WORK
    };
    let left = place_menu(POINT { x: 30, y: 900 }, size, left_work);
    assert_eq!((left.x, left.y), (60, 600));
    // Right taskbar.
    let right_work = RECT {
      right: 1860,
      ..WORK
    };
    let right = place_menu(POINT { x: 1890, y: 900 }, size, right_work);
    assert_eq!((right.x, right.y), (1600, 600));
    // Plenty of room: down and right from the cursor.
    let free = place_menu(POINT { x: 100, y: 100 }, size, WORK);
    assert_eq!((free.x, free.y), (100, 100));
  }

  #[test]
  fn the_menu_slides_out_of_the_taskbar_it_opened_from() {
    let bottom = POINT { x: 1700, y: 1060 };
    assert_eq!(
      menu_slide(bottom, place_menu(bottom, (260, 300), WORK), WORK),
      (0.0, 8.0)
    );
    let top_work = RECT {
      top: 40,
      bottom: 1080,
      ..WORK
    };
    let top = POINT { x: 1700, y: 20 };
    assert_eq!(
      menu_slide(top, place_menu(top, (260, 300), top_work), top_work),
      (0.0, -8.0)
    );
    let left_work = RECT { left: 60, ..WORK };
    let left = POINT { x: 30, y: 900 };
    assert_eq!(
      menu_slide(left, place_menu(left, (260, 300), left_work), left_work),
      (-8.0, 0.0)
    );
    let right_work = RECT {
      right: 1860,
      ..WORK
    };
    let right = POINT { x: 1890, y: 900 };
    assert_eq!(
      menu_slide(right, place_menu(right, (260, 300), right_work), right_work),
      (8.0, 0.0)
    );
    // A menu opened inside the work area: from above when it opens down, below when it opens up.
    let inside = POINT { x: 500, y: 200 };
    assert_eq!(
      menu_slide(inside, place_menu(inside, (260, 300), WORK), WORK),
      (0.0, -8.0)
    );
    let low = POINT { x: 500, y: 1000 };
    assert_eq!(
      menu_slide(low, place_menu(low, (260, 300), WORK), WORK),
      (0.0, 8.0)
    );
  }

  #[test]
  fn the_menu_stays_on_negative_origin_monitors_and_keeps_its_top_left_when_oversized() {
    let work = RECT {
      left: -2560,
      top: -300,
      right: 0,
      bottom: 1100,
    };
    let corner = place_menu(POINT { x: -5, y: 1130 }, (390, 450), work);
    assert_eq!((corner.x, corner.y), (-395, 650));
    let oversized = place_menu(POINT { x: -100, y: 0 }, (3000, 2000), work);
    assert_eq!((oversized.x, oversized.y), (-2560, -300));
  }

  #[test]
  fn the_submenu_opens_beside_its_row_flips_at_the_screen_edge_and_stays_inside() {
    let menu = RECT {
      left: 1000,
      top: 600,
      right: 1260,
      bottom: 900,
    };
    let right = place_submenu(menu, 141, 1.0, (340, 200), WORK);
    assert_eq!((right.x, right.y), (1262, 737));
    let edge = RECT {
      left: 1600,
      right: 1860,
      ..menu
    };
    let flipped = place_submenu(edge, 141, 1.0, (340, 200), WORK);
    assert_eq!((flipped.x, flipped.y), (1258, 737));
    // Near the bottom of the work area the submenu rises to fit; at 150% the gaps scale.
    let low = RECT {
      top: 900,
      bottom: 1030,
      ..menu
    };
    let raised = place_submenu(low, 100, 1.5, (510, 300), WORK);
    assert_eq!((raised.x, raised.y), (1263, 740));
  }
}
