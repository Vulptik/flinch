//! Brief message near the pointer for a selection that was too small, owned by the selection
//! window thread. It is chrome only: painted after the scene, so no capture can contain it.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::HDC;

use crate::error::ErrorExt;
use crate::overlay::chrome::{Renderer, Scene, TextStyle};
use crate::overlay::motion::Tween;
use crate::overlay::theme;

pub(crate) const HEIGHT: i32 = 38;
const PADDING_X: i32 = 16;
const POINTER_GAP: i32 = 24;
const RISE: i32 = 4;
const HOLD: Duration = Duration::from_millis(1800);
const FADE_IN: Duration = Duration::from_millis(140);
const FADE_OUT: Duration = Duration::from_millis(160);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Frame {
  rect: RECT,
  alpha: u8,
}

pub(crate) struct SelectionHint {
  text: &'static str,
  width: Option<i32>,
  shown: bool,
  failed: bool,
  expires: Option<Instant>,
  rect: Option<RECT>,
  fade: Tween,
  frame: Option<Frame>,
  active: bool,
  chrome: Renderer,
}

impl Default for SelectionHint {
  fn default() -> Self {
    Self::new("")
  }
}

impl SelectionHint {
  pub fn new(text: &'static str) -> Self {
    Self {
      text,
      width: None,
      shown: false,
      failed: false,
      expires: None,
      rect: None,
      fade: Tween::new(0.0, FADE_IN, Instant::now()),
      frame: None,
      active: false,
      chrome: Renderer::default(),
    }
  }

  /// True until the message has fully gone, so the overlay clock can finish it.
  pub fn active(&self) -> bool {
    self.active
  }

  #[cfg(test)]
  pub fn frame_rect(&self) -> Option<RECT> {
    self.frame.map(|frame| frame.rect)
  }

  fn set_shown(&mut self, shown: bool, now: Instant, animate: bool) {
    if self.shown == shown {
      return;
    }
    self.shown = shown;
    let duration = if shown { FADE_IN } else { FADE_OUT };
    let mut fade = Tween::new(self.fade.value(now), duration, now);
    fade.target(f32::from(u8::from(shown)), now, animate);
    self.fade = fade;
  }

  pub fn show(
    &mut self,
    pointer: (i32, i32),
    monitor: RECT,
    avoid: Option<RECT>,
    now: Instant,
    animate: bool,
  ) -> Result<(), String> {
    if self.text.is_empty() || self.failed {
      return Ok(());
    }
    let width = match self.width {
      Some(width) => width,
      None => {
        let width = pill_width(&mut self.chrome, self.text)?;
        self.width = Some(width);
        width
      },
    };
    self.rect = Some(place(pointer, width, monitor, avoid));
    self.expires = Some(now + HOLD);
    self.set_shown(true, now, animate);
    self.active = true;
    Ok(())
  }

  pub fn dismiss(&mut self, now: Instant, animate: bool) {
    self.expires = None;
    self.set_shown(false, now, animate);
  }

  /// A paint failure ends the message for this selection rather than retrying every frame.
  pub fn turn_off(&mut self) {
    self.failed = true;
    self.shown = false;
    self.expires = None;
  }

  /// Advances the message and returns the areas to repaint.
  pub fn update(&mut self, now: Instant, animate: bool, window_height: i32) -> [Option<RECT>; 2] {
    if self.expires.is_some_and(|expires| now >= expires) {
      self.dismiss(now, animate);
    }
    let previous = self.frame;
    let opacity = self.fade.value(now);
    let fading = !self.failed && self.fade.active(now);
    self.active = !self.failed && (self.shown || fading);
    self.frame = self
      .rect
      .filter(|_| opacity > 0.0 && !self.failed)
      .map(|rect| {
        let room_below = (window_height - rect.bottom).max(0);
        let rise = (((1.0 - opacity) * RISE as f32).round() as i32).min(room_below);
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
      return [None, None];
    }
    [
      previous.map(|frame| frame.rect),
      self.frame.map(|frame| frame.rect),
    ]
  }

  pub fn paint(&mut self, dc: HDC, damage: RECT) -> Result<(), String> {
    let Some(frame) = self.frame else {
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

/// Width of a one-line message pill holding `text`.
pub(crate) fn pill_width(chrome: &mut Renderer, text: &str) -> Result<i32, String> {
  let measured = chrome
    .measure_text(text, TextStyle::LABEL)
    .map_err(|error| error.to_rich_string())?;
  Ok(measured.ceil() as i32 + PADDING_X * 2)
}

/// Paints a one-line message pill. A translucent frame lands only on fully restored damage, or
/// it would build up; a fade invalidates the whole pill every tick, so the next frame covers it.
pub(crate) fn paint_pill(
  chrome: &mut Renderer,
  dc: HDC,
  damage: RECT,
  rect: RECT,
  alpha: u8,
  text: &str,
) -> Result<(), String> {
  let covered = damage.left <= rect.left
    && damage.top <= rect.top
    && damage.right >= rect.right
    && damage.bottom >= rect.bottom;
  if alpha < u8::MAX && !covered {
    return Ok(());
  }
  let mut scene = Scene::default();
  scene.panel(rect).map_err(|error| error.to_rich_string())?;
  scene
    .styled_text(
      RECT {
        left: rect.left + PADDING_X,
        right: rect.right - PADDING_X,
        top: rect.top + 9,
        bottom: rect.bottom - 9,
      },
      text,
      TextStyle::LABEL,
      theme::TEXT,
    )
    .map_err(|error| error.to_rich_string())?;
  chrome
    .render_with_opacity(dc, rect, &scene, alpha)
    .map_err(|error| error.to_rich_string())
}

fn overlaps(a: RECT, b: RECT) -> bool {
  a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// Centered above the pointer, below it when the monitor edge or `avoid` is in the way.
fn place(pointer: (i32, i32), width: i32, monitor: RECT, avoid: Option<RECT>) -> RECT {
  let left = (pointer.0 - width / 2).clamp(monitor.left, (monitor.right - width).max(monitor.left));
  let at = |top: i32| RECT {
    left,
    top,
    right: left + width,
    bottom: top + HEIGHT,
  };
  let above = at(pointer.1 - POINTER_GAP - HEIGHT);
  let below = at(pointer.1 + POINTER_GAP);
  let usable = |rect: RECT| {
    rect.top >= monitor.top
      && rect.bottom <= monitor.bottom
      && avoid.is_none_or(|avoid| !overlaps(rect, avoid))
  };
  if usable(above) {
    above
  } else if usable(below) || above.top < monitor.top {
    below
  } else {
    above
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const DESKTOP: RECT = RECT {
    left: 0,
    top: 0,
    right: 1920,
    bottom: 1080,
  };

  #[test]
  fn hint_sits_above_the_pointer_and_stays_on_the_monitor() {
    let rect = place((960, 540), 200, DESKTOP, None);
    assert_eq!((rect.left, rect.right), (860, 1060));
    assert_eq!(rect.bottom, 540 - POINTER_GAP);

    let top = place((960, 10), 200, DESKTOP, None);
    assert_eq!(top.top, 10 + POINTER_GAP);

    let corner = place((1915, 500), 200, DESKTOP, None);
    assert_eq!(corner.right, 1920);
    let left = place((5, 500), 200, DESKTOP, None);
    assert_eq!(left.left, 0);
  }

  #[test]
  fn hint_moves_below_when_the_lens_is_above() {
    let lens = RECT {
      left: 800,
      top: 380,
      right: 1100,
      bottom: 520,
    };
    let rect = place((960, 540), 200, DESKTOP, Some(lens));
    assert_eq!(rect.top, 540 + POINTER_GAP);
  }

  fn shown(animate: bool, now: Instant) -> SelectionHint {
    let mut hint = SelectionHint::new("Too small");
    hint
      .show((960, 540), DESKTOP, None, now, animate)
      .expect("text measures");
    hint
  }

  #[test]
  fn empty_text_never_shows() {
    let now = Instant::now();
    let mut hint = SelectionHint::default();
    hint
      .show((960, 540), DESKTOP, None, now, true)
      .expect("nothing to measure");
    assert!(!hint.active());
    assert_eq!(hint.update(now, true, 1080), [None, None]);
  }

  #[test]
  fn hint_fades_in_holds_then_fades_out_and_stops_the_clock() {
    let start = Instant::now();
    let mut hint = shown(true, start);
    assert!(hint.active());

    hint.update(start + FADE_IN / 2, true, 1080);
    let fading = hint.frame.expect("fading in");
    assert!(fading.alpha > 0 && fading.alpha < u8::MAX);
    let settled = hint.rect.expect("placed");
    assert!(fading.rect.top > settled.top);

    hint.update(start + FADE_IN, true, 1080);
    let held = hint.frame.expect("held");
    assert_eq!((held.alpha, held.rect), (u8::MAX, settled));
    assert_eq!(hint.update(start + HOLD / 2, true, 1080), [None, None]);
    assert!(
      hint.active(),
      "the clock keeps watching for the end of the hold"
    );

    hint.update(start + HOLD, true, 1080);
    let [cleared, none] = hint.update(start + HOLD + FADE_OUT, true, 1080);
    assert!(cleared.is_some() && none.is_none());
    assert!(!hint.active());
  }

  #[test]
  fn a_new_drag_dismisses_the_hint_early() {
    let start = Instant::now();
    let mut hint = shown(false, start);
    hint.update(start, false, 1080);
    assert!(hint.frame.is_some());
    hint.dismiss(start, false);
    let [cleared, none] = hint.update(start, false, 1080);
    assert!(cleared.is_some() && none.is_none());
    assert!(!hint.active());
  }

  #[test]
  fn showing_again_restarts_the_hold() {
    let start = Instant::now();
    let mut hint = shown(false, start);
    let later = start + HOLD / 2;
    hint
      .show((960, 540), DESKTOP, None, later, false)
      .expect("text measures");
    hint.update(start + HOLD, false, 1080);
    assert!(
      hint.frame.is_some(),
      "the second attempt gets its full time"
    );
  }

  #[test]
  fn paint_failure_clears_without_keeping_the_clock_running() {
    let now = Instant::now();
    let mut hint = shown(false, now);
    let [_, painted] = hint.update(now, false, 1080);
    hint.turn_off();
    assert_eq!(hint.update(now, false, 1080)[0], painted);
    assert!(!hint.active());
  }

  #[test]
  fn hint_paints_text_on_the_panel_without_building_up_while_fading() {
    use crate::overlay::gdi::bitmap::ScreenshotBitmap;
    use windows::Win32::Graphics::Gdi::GetPixel;

    let target =
      ScreenshotBitmap::from_rgba(&vec![0u8; 400 * 120 * 4], 400, 120).expect("synthetic target");
    let area = RECT {
      left: 0,
      top: 0,
      right: 400,
      bottom: 120,
    };
    let now = Instant::now();
    let mut hint = SelectionHint::new("Too small");
    hint
      .show((200, 100), area, None, now, false)
      .expect("text measures");
    hint.update(now, false, 120);
    let rect = hint.frame.expect("shown").rect;
    hint.paint(target.dc, area).expect("hint paints");
    // SAFETY: the target DC is live and these points lie inside it.
    let pixel = |x: i32, y: i32| unsafe { GetPixel(target.dc, x, y) };
    assert_eq!(
      pixel(rect.left + 6, rect.top + 19),
      theme::colorref(theme::SURFACE)
    );
    let text = (rect.left + PADDING_X..rect.right - PADDING_X)
      .flat_map(|x| (rect.top + 9..rect.bottom - 9).map(move |y| (x, y)))
      .filter(|(x, y)| pixel(*x, *y) != theme::colorref(theme::SURFACE))
      .count();
    assert!(text > 40, "the message drew {text} text pixels");

    let clean =
      ScreenshotBitmap::from_rgba(&vec![0u8; 400 * 120 * 4], 400, 120).expect("synthetic target");
    hint.frame = Some(Frame { rect, alpha: 128 });
    let partial = RECT {
      left: 0,
      top: 0,
      right: 10,
      bottom: 10,
    };
    hint.paint(clean.dc, partial).expect("skips cleanly");
    // SAFETY: the clean DC is live and this point lies inside it.
    let untouched = unsafe { GetPixel(clean.dc, rect.left + 6, rect.top + 19) };
    assert_eq!(untouched.0, 0);
  }
}
