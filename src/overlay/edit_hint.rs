//! The "Hold Alt to edit" pill at the top center of the pointer's monitor, owned by the selection
//! window thread. It shows before a press too, since snapping a window or the screen accepts Alt
//! like a drag does. It is chrome only: painted after the scene, so no capture can contain it.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::HDC;

use crate::overlay::chrome::Renderer;
use crate::overlay::motion::Tween;
use crate::overlay::selection_hint::{HEIGHT, paint_pill, pill_width};

/// Distance from the monitor's top or bottom edge.
const EDGE_GAP: i32 = 24;
/// The pointer this close to the pill sends it to the other edge.
const CLEARANCE: i32 = 64;
/// It returns to the top only once the pointer is this far from it, so it never flaps.
const RETURN_CLEARANCE: i32 = 160;
const RISE: i32 = 4;
const FADE_IN: Duration = Duration::from_millis(140);
const FADE_OUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Frame {
  rect: RECT,
  alpha: u8,
}

pub(crate) struct EditHint {
  text: &'static str,
  width: Option<i32>,
  failed: bool,
  shown: bool,
  monitor: Option<RECT>,
  bottom: bool,
  rect: Option<RECT>,
  fade: Tween,
  frame: Option<Frame>,
  active: bool,
  chrome: Renderer,
}

impl EditHint {
  pub fn new(text: Option<&'static str>) -> Self {
    Self {
      text: text.unwrap_or_default(),
      width: None,
      failed: false,
      shown: false,
      monitor: None,
      bottom: false,
      rect: None,
      fade: Tween::new(0.0, FADE_IN, Instant::now()),
      frame: None,
      active: false,
      chrome: Renderer::default(),
    }
  }

  /// True until a fade has finished, so the overlay clock can complete it.
  pub fn active(&self) -> bool {
    self.active
  }

  #[cfg(test)]
  pub fn frame_rect(&self) -> Option<RECT> {
    self.frame.map(|frame| frame.rect)
  }

  /// A paint failure ends the pill for this overlay rather than retrying every frame.
  pub fn turn_off(&mut self) {
    self.failed = true;
    self.shown = false;
  }

  fn set_shown(&mut self, shown: bool, now: Instant, animate: bool) {
    if self.shown == shown {
      return;
    }
    self.shown = shown;
    let mut fade = Tween::new(
      self.fade.value(now),
      if shown { FADE_IN } else { FADE_OUT },
      now,
    );
    fade.target(f32::from(u8::from(shown)), now, animate);
    self.fade = fade;
  }

  /// Advances the pill and returns the areas to repaint. `avoid` holds chrome it must not cover.
  pub fn update(
    &mut self,
    visible: bool,
    pointer: (i32, i32),
    monitor_at: impl FnOnce((i32, i32)) -> RECT,
    avoid: &[Option<RECT>],
    now: Instant,
    animate: bool,
  ) -> Result<[Option<RECT>; 2], String> {
    let visible = visible && !self.failed && !self.text.is_empty();
    if visible {
      let width = match self.width {
        Some(width) => width,
        None => {
          let width = pill_width(&mut self.chrome, self.text)?;
          self.width = Some(width);
          width
        },
      };
      if self
        .monitor
        .is_none_or(|monitor| !contains(monitor, pointer))
      {
        self.monitor = Some(monitor_at(pointer));
        self.bottom = false;
        self.rect = None;
      }
      let monitor = self.monitor.unwrap_or_default();
      let bottom = choose_bottom(self.bottom, pointer, width, monitor, avoid);
      let rect = place(monitor, width, bottom);
      if self.rect.is_some_and(|old| old != rect) && animate {
        // A new spot starts a fresh entrance rather than sliding across the screen.
        self.fade = Tween::new(0.0, FADE_IN, now);
        self.shown = false;
      }
      self.bottom = bottom;
      self.rect = Some(rect);
    }
    self.set_shown(visible, now, animate);

    let previous = self.frame;
    let opacity = if self.failed {
      0.0
    } else {
      self.fade.value(now)
    };
    let fading = !self.failed && self.fade.active(now);
    self.active = fading;
    self.frame = self.rect.filter(|_| opacity > 0.0).map(|rect| {
      let monitor = self.monitor.unwrap_or(rect);
      let rise = ((1.0 - opacity) * RISE as f32).round() as i32;
      // It rises toward its resting place from the monitor edge it sits against.
      let rise = if self.bottom {
        rise.min((monitor.bottom - rect.bottom).max(0))
      } else {
        -rise.min((rect.top - monitor.top).max(0))
      };
      Frame {
        rect: RECT {
          top: rect.top + rise,
          bottom: rect.bottom + rise,
          ..rect
        },
        alpha: (opacity * 255.0).round() as u8,
      }
    });
    if previous == self.frame && !fading {
      return Ok([None, None]);
    }
    Ok([
      previous.map(|frame| frame.rect),
      self.frame.map(|frame| frame.rect),
    ])
  }

  pub fn paint(&mut self, dc: HDC, damage: RECT) -> Result<(), String> {
    let Some(frame) = self.frame.filter(|_| !self.failed) else {
      return Ok(());
    };
    paint_pill(
      &mut self.chrome,
      dc,
      damage,
      frame.rect,
      frame.alpha,
      self.text,
    )
  }
}

fn contains(rect: RECT, point: (i32, i32)) -> bool {
  point.0 >= rect.left && point.0 < rect.right && point.1 >= rect.top && point.1 < rect.bottom
}

fn overlaps(a: RECT, b: RECT) -> bool {
  a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn grow(rect: RECT, by: i32) -> RECT {
  RECT {
    left: rect.left - by,
    top: rect.top - by,
    right: rect.right + by,
    bottom: rect.bottom + by,
  }
}

/// Centered at the top of `monitor`, or at its bottom.
fn place(monitor: RECT, width: i32, bottom: bool) -> RECT {
  let width = width.min(monitor.right - monitor.left).max(1);
  let left = monitor.left + (monitor.right - monitor.left - width) / 2;
  let top = if bottom {
    (monitor.bottom - EDGE_GAP - HEIGHT).max(monitor.top)
  } else {
    (monitor.top + EDGE_GAP).min((monitor.bottom - HEIGHT).max(monitor.top))
  };
  RECT {
    left,
    top,
    right: left + width,
    bottom: top + HEIGHT,
  }
}

/// The top is home. The pill leaves for the bottom when the pointer or other chrome crowds it, and
/// comes back once the top is clearly free again.
fn choose_bottom(
  bottom: bool,
  pointer: (i32, i32),
  width: i32,
  monitor: RECT,
  avoid: &[Option<RECT>],
) -> bool {
  let crowded = |rect: RECT, clearance: i32| {
    contains(grow(rect, clearance), pointer)
      || avoid.iter().flatten().any(|other| overlaps(rect, *other))
  };
  let top = place(monitor, width, false);
  let low = place(monitor, width, true);
  if bottom {
    crowded(top, RETURN_CLEARANCE) && !crowded(low, CLEARANCE)
  } else {
    crowded(top, CLEARANCE) && !crowded(low, CLEARANCE)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const MONITOR: RECT = RECT {
    left: 0,
    top: 0,
    right: 1920,
    bottom: 1080,
  };

  fn hint() -> EditHint {
    EditHint::new(Some("Hold Alt to edit"))
  }

  fn step(hint: &mut EditHint, visible: bool, pointer: (i32, i32), now: Instant) -> Option<RECT> {
    hint
      .update(visible, pointer, |_| MONITOR, &[], now, false)
      .expect("text measures");
    hint.frame_rect()
  }

  #[test]
  fn the_pill_sits_at_the_top_center_of_the_pointer_monitor_from_the_start() {
    let mut hint = hint();
    let rect = step(&mut hint, true, (960, 700), Instant::now()).expect("shown before a press");
    assert_eq!(rect.top, EDGE_GAP);
    assert_eq!(rect.bottom - rect.top, HEIGHT);
    assert_eq!(rect.left + rect.right, 1920);
    assert!(rect.right - rect.left > 60);
  }

  #[test]
  fn a_pointer_near_the_top_sends_it_down_and_it_returns_only_when_clearly_free() {
    let mut hint = hint();
    let now = Instant::now();
    assert_eq!(
      step(&mut hint, true, (960, 700), now).map(|r| r.top),
      Some(EDGE_GAP)
    );
    let low = step(&mut hint, true, (960, 40), now).expect("moved");
    assert_eq!(low.bottom, 1080 - EDGE_GAP);
    // Just past the first clearance it stays down, so small movements never flap.
    let near = EDGE_GAP + HEIGHT + CLEARANCE + 10;
    assert_eq!(step(&mut hint, true, (960, near), now), Some(low));
    let far = EDGE_GAP + HEIGHT + RETURN_CLEARANCE + 10;
    assert_eq!(
      step(&mut hint, true, (960, far), now).map(|r| r.top),
      Some(EDGE_GAP)
    );
  }

  #[test]
  fn the_pill_avoids_the_lens_and_follows_the_pointer_to_another_monitor() {
    let mut hint = hint();
    let now = Instant::now();
    let lens = RECT {
      left: 900,
      top: 0,
      right: 1100,
      bottom: 150,
    };
    hint
      .update(true, (960, 700), |_| MONITOR, &[Some(lens)], now, false)
      .expect("text measures");
    assert_eq!(hint.frame_rect().map(|r| r.bottom), Some(1080 - EDGE_GAP));

    let second = RECT {
      left: 1920,
      top: -200,
      right: 3200,
      bottom: 824,
    };
    hint
      .update(true, (2500, 400), |_| second, &[], now, false)
      .expect("text measures");
    let rect = hint.frame_rect().expect("shown");
    assert_eq!(rect.top, second.top + EDGE_GAP);
    assert_eq!(rect.left + rect.right, second.left + second.right);
  }

  #[test]
  fn confirming_fades_it_out_and_stops_the_clock() {
    let mut hint = hint();
    let start = Instant::now();
    hint
      .update(true, (960, 700), |_| MONITOR, &[], start, true)
      .expect("text measures");
    assert!(hint.active());
    let [_, shown] = hint
      .update(true, (960, 700), |_| MONITOR, &[], start + FADE_IN, true)
      .expect("settled");
    assert!(shown.is_some());
    assert!(!hint.active());
    hint
      .update(false, (960, 700), |_| MONITOR, &[], start + FADE_IN, true)
      .expect("hiding");
    assert!(hint.active());
    let [cleared, none] = hint
      .update(
        false,
        (960, 700),
        |_| MONITOR,
        &[],
        start + FADE_IN + FADE_OUT,
        true,
      )
      .expect("hidden");
    assert!(cleared.is_some() && none.is_none());
    assert!(!hint.active());
  }

  #[test]
  fn overlays_without_an_editor_and_failed_paints_show_nothing() {
    let mut none = EditHint::new(None);
    assert_eq!(step(&mut none, true, (960, 700), Instant::now()), None);
    let mut hint = hint();
    let now = Instant::now();
    step(&mut hint, true, (960, 700), now);
    hint.turn_off();
    assert_eq!(step(&mut hint, true, (960, 700), now), None);
    assert!(!hint.active());
  }

  #[test]
  fn the_pill_paints_its_panel_and_text() {
    use crate::overlay::gdi::bitmap::ScreenshotBitmap;
    use crate::overlay::theme;
    use windows::Win32::Graphics::Gdi::GetPixel;

    let target =
      ScreenshotBitmap::from_rgba(&vec![0u8; 1920 * 120 * 4], 1920, 120).expect("synthetic target");
    let strip = RECT {
      left: 0,
      top: 0,
      right: 1920,
      bottom: 120,
    };
    let mut hint = hint();
    hint
      .update(true, (100, 100), |_| strip, &[], Instant::now(), false)
      .expect("text measures");
    let rect = hint.frame_rect().expect("shown");
    hint
      .paint(
        target.dc,
        RECT {
          left: 0,
          top: 0,
          right: 1920,
          bottom: 120,
        },
      )
      .expect("pill paints");
    // SAFETY: the target DC is live and these points lie inside it.
    let pixel = |x: i32, y: i32| unsafe { GetPixel(target.dc, x, y) };
    assert_eq!(
      pixel(rect.left + 6, rect.top + 19),
      theme::colorref(theme::SURFACE)
    );
    let text = (rect.left + 16..rect.right - 16)
      .flat_map(|x| (rect.top + 9..rect.bottom - 9).map(move |y| (x, y)))
      .filter(|(x, y)| pixel(*x, *y) != theme::colorref(theme::SURFACE))
      .count();
    assert!(text > 40, "the pill drew {text} text pixels");
  }
}
