//! A pin's animations as plain values of time, free of Win32 so they can be tested directly.
//!
//! The pin host drives every animation itself, one frame at a time: each frame places the window
//! and sets its visuals from these values, so a pin's window and its pixels always move together.
//! Every animation can be redirected midway: it continues from the value it shows.

use std::time::{Duration, Instant};

pub(crate) const ZOOM_TIME: Duration = Duration::from_millis(160);
pub(crate) const OPACITY_TIME: Duration = Duration::from_millis(140);
pub(crate) const OPEN_TIME: Duration = Duration::from_millis(180);
pub(crate) const CLOSE_TIME: Duration = Duration::from_millis(130);
pub(crate) const SLIDE_TIME: Duration = Duration::from_millis(280);
/// Close All closes the newest pin first, the others in quick succession, within this.
const CLOSE_ALL_SPREAD: Duration = Duration::from_millis(200);
const CLOSE_ALL_STEP: Duration = Duration::from_millis(25);
/// A pin opens from slightly smaller and closes to slightly smaller, around its center.
const OPEN_SCALE: f32 = 0.94;
const CLOSE_SCALE: f32 = 0.97;
/// The zoom or opacity label: in quickly, a moment to read it, then out.
const LABEL_IN: Duration = Duration::from_millis(60);
const LABEL_HOLD: Duration = Duration::from_millis(650);
const LABEL_OUT: Duration = Duration::from_millis(250);

/// `base`, or no time at all when Windows animations are off.
pub(crate) fn duration(base: Duration, enabled: bool) -> Duration {
  if enabled { base } else { Duration::ZERO }
}

/// A value easing from `from` to `to` (fast, then settling: cubic ease-out).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Ramp {
  from: f32,
  to: f32,
  start: Instant,
  duration: Duration,
}

impl Ramp {
  pub(crate) fn still(value: f32, now: Instant) -> Self {
    Self::new(value, value, now, Duration::ZERO)
  }

  pub(crate) fn new(from: f32, to: f32, start: Instant, duration: Duration) -> Self {
    Self {
      from,
      to,
      start,
      duration,
    }
  }

  /// Eased progress, 0 until `start` and 1 from its end.
  pub(crate) fn progress(&self, now: Instant) -> f32 {
    if self.duration.is_zero() {
      return if now >= self.start { 1.0 } else { 0.0 };
    }
    let t = (now.saturating_duration_since(self.start).as_secs_f32() / self.duration.as_secs_f32())
      .clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
  }

  pub(crate) fn value(&self, now: Instant) -> f32 {
    self.from + (self.to - self.from) * self.progress(now)
  }

  pub(crate) fn target(&self) -> f32 {
    self.to
  }

  pub(crate) fn done(&self, now: Instant) -> bool {
    now >= self.start + self.duration
  }

  /// Heads for `to` from whatever value shows now.
  pub(crate) fn retarget(&mut self, to: f32, now: Instant, duration: Duration) {
    *self = Self::new(self.value(now), to, now, duration);
  }
}

/// Where a pin's image is and how large: zooming keeps the point under the cursor in place.
///
/// The image's top-left corner `c` at zoom `z` is `anchor - (z / z0) * (anchor - c0)`, with `c0`
/// the corner when the zoom left `z0`. Both follow one eased value, so the anchor never drifts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct View {
  zoom: Ramp,
  anchor: (f32, f32),
  corner: (f32, f32),
}

impl View {
  pub(crate) fn settled(zoom: f32, corner: (f32, f32), now: Instant) -> Self {
    Self {
      zoom: Ramp::still(zoom, now),
      anchor: corner,
      corner,
    }
  }

  /// The image's top-left and zoom now.
  pub(crate) fn at(&self, now: Instant) -> ((f32, f32), f32) {
    let zoom = self.zoom.value(now);
    let start = self.zoom.from;
    let k = if start > 0.0 { zoom / start } else { 1.0 };
    (
      (
        self.anchor.0 - k * (self.anchor.0 - self.corner.0),
        self.anchor.1 - k * (self.anchor.1 - self.corner.1),
      ),
      zoom,
    )
  }

  pub(crate) fn zoom_target(&self) -> f32 {
    self.zoom.target()
  }

  pub(crate) fn zooming(&self, now: Instant) -> bool {
    !self.zoom.done(now)
  }

  /// Zooms towards `zoom`, keeping `anchor` (a screen point) in place, from what shows now.
  pub(crate) fn zoom_to(&mut self, zoom: f32, anchor: (f32, f32), now: Instant, time: Duration) {
    let (corner, current) = self.at(now);
    self.corner = corner;
    self.anchor = anchor;
    self.zoom = Ramp::new(current, zoom, now, time);
  }

  /// Moves the image by a whole offset, even midway through a zoom.
  pub(crate) fn translate(&mut self, dx: f32, dy: f32) {
    self.anchor = (self.anchor.0 + dx, self.anchor.1 + dy);
    self.corner = (self.corner.0 + dx, self.corner.1 + dy);
  }
}

/// A move that eases out over the destination: the offset still to cover shrinks to zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Slide {
  from: (f32, f32),
  ramp: Ramp,
}

impl Slide {
  /// Starts `offset` away from where the pin now belongs.
  pub(crate) fn new(offset: (f32, f32), now: Instant, time: Duration) -> Self {
    Self {
      from: offset,
      ramp: Ramp::new(1.0, 0.0, now, time),
    }
  }

  pub(crate) fn offset(&self, now: Instant) -> (f32, f32) {
    let remaining = self.ramp.value(now);
    (self.from.0 * remaining, self.from.1 * remaining)
  }

  pub(crate) fn done(&self, now: Instant) -> bool {
    self.ramp.done(now)
  }
}

/// Opening (0 to 1) or closing (1 to 0): the pin's whole-window fade and scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Presence {
  ramp: Ramp,
  closing: bool,
}

impl Presence {
  pub(crate) fn opening(now: Instant, time: Duration) -> Self {
    Self {
      ramp: Ramp::new(0.0, 1.0, now, time),
      closing: false,
    }
  }

  /// Closes from what shows now, starting after `delay`.
  pub(crate) fn close(&mut self, now: Instant, delay: Duration, time: Duration) {
    let from = self.ramp.value(now);
    self.ramp = Ramp::new(from, 0.0, now + delay, time);
    self.closing = true;
  }

  pub(crate) fn closing(&self) -> bool {
    self.closing
  }

  /// Opacity and scale (around the window's center).
  pub(crate) fn at(&self, now: Instant) -> (f32, f32) {
    let value = self.ramp.value(now);
    let smallest = if self.closing {
      CLOSE_SCALE
    } else {
      OPEN_SCALE
    };
    (value, smallest + (1.0 - smallest) * value)
  }

  pub(crate) fn done(&self, now: Instant) -> bool {
    self.ramp.done(now)
  }
}

/// How long the `index`-th pin (0 the newest) of `count` waits before Close All closes it.
pub(crate) fn close_all_delay(index: usize, count: usize) -> Duration {
  if count <= 1 {
    return Duration::ZERO;
  }
  let step = CLOSE_ALL_STEP.min(CLOSE_ALL_SPREAD / (count as u32 - 1).max(1));
  step * index.min(count - 1) as u32
}

/// The label's opacity `elapsed` after it was shown, `None` once it has faded out.
pub(crate) fn label_alpha(elapsed: Duration) -> Option<f32> {
  if elapsed < LABEL_IN {
    return Some(elapsed.as_secs_f32() / LABEL_IN.as_secs_f32());
  }
  let shown = elapsed - LABEL_IN;
  if shown < LABEL_HOLD {
    return Some(1.0);
  }
  let out = shown - LABEL_HOLD;
  (out < LABEL_OUT).then(|| 1.0 - out.as_secs_f32() / LABEL_OUT.as_secs_f32())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
  }

  #[test]
  fn a_ramp_eases_out_and_can_be_redirected_midway() {
    let start = Instant::now();
    let ramp = Ramp::new(1.0, 2.0, start, ms(100));
    assert_eq!(ramp.value(start), 1.0);
    // Ease-out: past halfway in value at a quarter of the time.
    assert!(ramp.value(start + ms(25)) > 1.5);
    assert!(ramp.value(start + ms(50)) < 2.0);
    assert_eq!(ramp.value(start + ms(100)), 2.0);
    assert_eq!(ramp.value(start + ms(500)), 2.0);
    assert!(!ramp.done(start + ms(99)) && ramp.done(start + ms(100)));
    let mut turned = ramp;
    let midway = ramp.value(start + ms(40));
    turned.retarget(0.5, start + ms(40), ms(100));
    assert_eq!(turned.value(start + ms(40)), midway, "no jump");
    assert_eq!(turned.value(start + ms(140)), 0.5);
    // Animations off: the value is at its end at once.
    let instant = Ramp::new(0.0, 1.0, start, duration(ms(100), false));
    assert!(instant.done(start));
    assert_eq!(instant.value(start), 1.0);
    // A delayed ramp holds its start value until then.
    let delayed = Ramp::new(1.0, 0.0, start + ms(50), ms(100));
    assert_eq!(delayed.value(start), 1.0);
    assert!(!delayed.done(start + ms(100)));
  }

  #[test]
  fn zooming_keeps_the_anchor_in_place_at_every_frame() {
    let start = Instant::now();
    let mut view = View::settled(1.0, (100.0, 200.0), start);
    // The cursor sits 100 px into the image; zoom to 2×.
    view.zoom_to(2.0, (200.0, 300.0), start, ms(160));
    for step in 0..=16 {
      let now = start + ms(step * 10);
      let (corner, zoom) = view.at(now);
      // The image point under the anchor: (anchor - corner) / zoom, constant at 100 px.
      assert!(((200.0 - corner.0) / zoom - 100.0).abs() < 1e-3, "{step}");
      assert!(((300.0 - corner.1) / zoom - 100.0).abs() < 1e-3, "{step}");
    }
    assert_eq!(view.at(start + ms(160)), ((0.0, 100.0), 2.0));
    assert!(view.zooming(start + ms(100)) && !view.zooming(start + ms(160)));
    // A second notch midway, at another anchor: it continues from what shows, without a jump.
    let midway = start + ms(60);
    let shown = view.at(midway);
    view.zoom_to(3.0, (50.0, 50.0), midway, ms(160));
    assert_eq!(view.at(midway), shown);
    let ((x, y), zoom) = view.at(midway + ms(160));
    assert_eq!(zoom, 3.0);
    let image_point = ((50.0 - shown.0.0) / shown.1, (50.0 - shown.0.1) / shown.1);
    assert!((50.0 - x - image_point.0 * 3.0).abs() < 1e-3);
    assert!((50.0 - y - image_point.1 * 3.0).abs() < 1e-3);
    assert_eq!(view.zoom_target(), 3.0);
    // Dragging mid-zoom moves everything, zoom untouched.
    let before = view.at(midway + ms(80));
    view.translate(10.0, -5.0);
    let after = view.at(midway + ms(80));
    assert_eq!(after.1, before.1);
    assert!((after.0.0 - before.0.0 - 10.0).abs() < 1e-3);
    assert!((after.0.1 - before.0.1 + 5.0).abs() < 1e-3);
  }

  #[test]
  fn a_slide_covers_its_offset_and_ends_in_place() {
    let start = Instant::now();
    let slide = Slide::new((-400.0, 30.0), start, SLIDE_TIME);
    assert_eq!(slide.offset(start), (-400.0, 30.0));
    let (x, _) = slide.offset(start + SLIDE_TIME / 2);
    assert!(x > -200.0 && x < 0.0, "eased: most of the way at half time");
    assert_eq!(slide.offset(start + SLIDE_TIME), (0.0, 0.0));
    assert!(slide.done(start + SLIDE_TIME));
  }

  #[test]
  fn pins_open_and_close_with_a_fade_and_a_slight_scale() {
    let start = Instant::now();
    let mut presence = Presence::opening(start, OPEN_TIME);
    assert_eq!(presence.at(start), (0.0, OPEN_SCALE));
    assert_eq!(presence.at(start + OPEN_TIME), (1.0, 1.0));
    assert!(presence.done(start + OPEN_TIME) && !presence.closing());
    // Closed while still opening: it fades from what shows.
    let midway = start + OPEN_TIME / 3;
    let (shown, _) = presence.at(midway);
    presence.close(midway, ms(0), CLOSE_TIME);
    assert!(presence.closing());
    assert_eq!(presence.at(midway).0, shown);
    assert_eq!(presence.at(midway + CLOSE_TIME), (0.0, CLOSE_SCALE));
    assert!(presence.done(midway + CLOSE_TIME));
    // A delayed close waits.
    let mut later = Presence::opening(start, Duration::ZERO);
    later.close(start, ms(50), CLOSE_TIME);
    assert_eq!(later.at(start + ms(40)).0, 1.0);
    assert!(!later.done(start + CLOSE_TIME));
  }

  #[test]
  fn close_all_staggers_within_a_fixed_spread() {
    assert_eq!(close_all_delay(0, 1), Duration::ZERO);
    assert_eq!(close_all_delay(0, 3), Duration::ZERO);
    assert_eq!(close_all_delay(2, 3), ms(50));
    // Twenty pins still finish starting within the spread.
    assert!(close_all_delay(19, 20) <= CLOSE_ALL_SPREAD);
    assert!(close_all_delay(19, 20) > close_all_delay(18, 20));
  }

  #[test]
  fn the_label_fades_in_holds_and_fades_out() {
    assert_eq!(label_alpha(ms(0)), Some(0.0));
    assert_eq!(label_alpha(ms(30)), Some(0.5));
    assert_eq!(label_alpha(ms(60)), Some(1.0));
    assert_eq!(label_alpha(ms(700)), Some(1.0));
    let fading = label_alpha(ms(60 + 650 + 125)).unwrap_or(0.0);
    assert!((fading - 0.5).abs() < 1e-3);
    assert_eq!(label_alpha(ms(60 + 650 + 250)), None);
  }
}
