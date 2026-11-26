//! Click highlights: which marks a recorded frame shows for recent mouse clicks.
//!
//! The recorder samples the button state and pointer position each tick, without a global mouse
//! hook, and asks for the marks of the moment a frame is sampled. A press starts a ring that
//! grows and fades over `RING_100NS`; a button held down also shows a soft dot under the
//! pointer, so drags read as drags. Times are QPC 100 ns units; positions are in the captured
//! source's pixels.

/// How long a click's ring grows and fades.
pub const RING_100NS: u64 = 3_500_000;
/// Rings still animating at once; a burst of clicks drops the oldest.
const MAX_RINGS: usize = 8;
const RING_START: f32 = 6.0;
const RING_END: f32 = 28.0;
const RING_THICKNESS: f32 = 3.0;
const RING_ALPHA: f32 = 0.9;
const HELD_RADIUS: f32 = 9.0;
const HELD_ALPHA: f32 = 0.35;
/// Flinch red for the left button, blue for the right.
const LEFT: [f32; 3] = [239.0 / 255.0, 68.0 / 255.0, 68.0 / 255.0];
const RIGHT: [f32; 3] = [59.0 / 255.0, 130.0 / 255.0, 246.0 / 255.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
  Left,
  Right,
}

impl Button {
  fn color(self) -> [f32; 3] {
    match self {
      Self::Left => LEFT,
      Self::Right => RIGHT,
    }
  }
}

/// One mark to draw: a ring (`thickness > 0`) or a filled dot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
  pub x: f32,
  pub y: f32,
  pub radius: f32,
  /// Ring width; 0 draws a filled dot.
  pub thickness: f32,
  pub color: [f32; 3],
  pub alpha: f32,
}

#[derive(Clone, Copy, Debug)]
struct Ring {
  x: f32,
  y: f32,
  button: Button,
  start: u64,
}

#[derive(Clone, Debug)]
pub struct Clicks {
  scale: f32,
  rings: Vec<Ring>,
  /// Buttons down at the last sample: left, right.
  down: [bool; 2],
  pointer: (f32, f32),
  /// No sample since the start or a reset: a button already down then is no click.
  fresh: bool,
}

impl Clicks {
  /// `scale` is the recorded display's DPI scale (1.0 at 100%).
  pub fn new(scale: f32) -> Self {
    Self {
      scale: if scale.is_finite() {
        scale.clamp(1.0, 4.0)
      } else {
        1.0
      },
      rings: Vec::new(),
      down: [false; 2],
      pointer: (0.0, 0.0),
      fresh: true,
    }
  }

  /// One sample of the buttons (left, right) and the pointer. `pressed` says a button went down
  /// since the last sample, which catches a click shorter than the time between samples. A press
  /// starts a ring, except in the first sample after the start or a reset.
  pub fn sample(&mut self, now: u64, down: [bool; 2], pressed: [bool; 2], pointer: (f32, f32)) {
    self.pointer = pointer;
    let fresh = std::mem::take(&mut self.fresh);
    for (index, button) in [Button::Left, Button::Right].into_iter().enumerate() {
      if !fresh && (pressed[index] || (down[index] && !self.down[index])) {
        if self.rings.len() == MAX_RINGS {
          self.rings.remove(0);
        }
        self.rings.push(Ring {
          x: pointer.0,
          y: pointer.1,
          button,
          start: now,
        });
      }
    }
    self.down = down;
    self
      .rings
      .retain(|ring| now.saturating_sub(ring.start) < RING_100NS);
  }

  /// Forgets every ring and held button, as on pause: nothing carries over into later frames,
  /// and a button still held at the next sample is no new click.
  pub fn reset(&mut self) {
    self.rings.clear();
    self.down = [false; 2];
    self.fresh = true;
  }

  /// Whether a frame at `now` shows any mark.
  pub fn active(&self, now: u64) -> bool {
    self.down.iter().any(|down| *down)
      || self
        .rings
        .iter()
        .any(|ring| now.saturating_sub(ring.start) < RING_100NS)
  }

  /// The marks for a frame at `now`, oldest first, the held dot last.
  pub fn marks(&self, now: u64) -> Vec<Mark> {
    let mut marks: Vec<Mark> = self
      .rings
      .iter()
      .filter_map(|ring| {
        let age = now.checked_sub(ring.start)?;
        if age >= RING_100NS {
          return None;
        }
        let progress = age as f32 / RING_100NS as f32;
        // Ease out: fast at first, settling as it fades.
        let eased = 1.0 - (1.0 - progress) * (1.0 - progress);
        Some(Mark {
          x: ring.x,
          y: ring.y,
          radius: (RING_START + (RING_END - RING_START) * eased) * self.scale,
          thickness: RING_THICKNESS * self.scale,
          color: ring.button.color(),
          alpha: RING_ALPHA * (1.0 - progress),
        })
      })
      .collect();
    let held = match self.down {
      [true, _] => Some(Button::Left),
      [false, true] => Some(Button::Right),
      [false, false] => None,
    };
    if let Some(button) = held {
      marks.push(Mark {
        x: self.pointer.0,
        y: self.pointer.1,
        radius: HELD_RADIUS * self.scale,
        thickness: 0.0,
        color: button.color(),
        alpha: HELD_ALPHA,
      });
    }
    marks
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const MS: u64 = 10_000;
  const NONE: [bool; 2] = [false, false];

  fn started(scale: f32) -> Clicks {
    let mut clicks = Clicks::new(scale);
    clicks.sample(0, [false, false], NONE, (0.0, 0.0));
    clicks
  }

  #[test]
  fn a_press_starts_one_ring_that_grows_fades_and_ends() {
    let mut clicks = started(1.0);
    assert!(!clicks.active(0));
    clicks.sample(100 * MS, [true, false], NONE, (50.0, 60.0));
    // Holding the button starts no second ring.
    clicks.sample(150 * MS, [true, false], NONE, (55.0, 60.0));
    let marks = clicks.marks(150 * MS);
    assert_eq!(marks.len(), 2, "the ring and the held dot");
    let (ring, dot) = (marks[0], marks[1]);
    assert_eq!(
      (ring.x, ring.y, ring.color),
      (50.0, 60.0, LEFT),
      "the ring stays where it began"
    );
    assert!(ring.radius > RING_START && ring.radius < RING_END);
    assert!(ring.alpha < RING_ALPHA && ring.alpha > 0.0);
    assert_eq!(
      (dot.x, dot.thickness, dot.alpha),
      (55.0, 0.0, HELD_ALPHA),
      "the dot follows"
    );
    clicks.sample(200 * MS, [false, false], NONE, (55.0, 60.0));
    let later = clicks.marks(300 * MS);
    assert_eq!(later.len(), 1);
    assert!(later[0].radius > ring.radius && later[0].alpha < ring.alpha);
    let end = 100 * MS + RING_100NS;
    assert!(clicks.active(end - 1));
    assert!(!clicks.active(end));
    assert!(clicks.marks(end).is_empty());
    clicks.sample(end, [false, false], NONE, (55.0, 60.0));
    assert!(!clicks.active(end));
  }

  #[test]
  fn right_clicks_are_blue_and_a_burst_keeps_only_the_newest_rings() {
    let mut clicks = started(1.0);
    clicks.sample(1, [false, true], NONE, (1.0, 1.0));
    clicks.sample(MS, [false, false], NONE, (1.0, 1.0));
    assert_eq!(clicks.marks(MS)[0].color, RIGHT);
    for index in 0..20u64 {
      clicks.sample(
        (2 + 2 * index) * MS,
        [true, false],
        NONE,
        (index as f32, 0.0),
      );
      clicks.sample(
        (3 + 2 * index) * MS,
        [false, false],
        NONE,
        (index as f32, 0.0),
      );
    }
    let marks = clicks.marks(42 * MS);
    assert_eq!(marks.len(), MAX_RINGS);
    assert_eq!(marks.last().map(|mark| mark.x), Some(19.0));
  }

  #[test]
  fn marks_scale_with_the_display_and_pausing_forgets_them() {
    let mut normal = started(1.0);
    let mut scaled = started(2.0);
    for clicks in [&mut normal, &mut scaled] {
      clicks.sample(1, [true, false], NONE, (100.0, 100.0));
    }
    let (a, b) = (normal.marks(MS)[0], scaled.marks(MS)[0]);
    assert!((b.radius - 2.0 * a.radius).abs() < 0.001);
    assert_eq!(b.thickness, 2.0 * a.thickness);
    assert_eq!(Clicks::new(f32::NAN).scale, 1.0);
    assert_eq!(Clicks::new(9.0).scale, 4.0);
    scaled.reset();
    assert!(!scaled.active(MS));
    // A button still held after resume is no new click: only its dot shows.
    scaled.sample(2 * MS, [true, false], [true, false], (0.0, 0.0));
    let marks = scaled.marks(2 * MS);
    assert_eq!((marks.len(), marks[0].thickness), (1, 0.0));
  }

  #[test]
  fn a_button_held_when_recording_starts_is_no_click_but_a_quick_one_between_samples_is() {
    let mut clicks = Clicks::new(1.0);
    clicks.sample(0, [true, false], [true, false], (5.0, 5.0));
    assert_eq!(clicks.marks(0).len(), 1, "the held dot alone");
    clicks.sample(MS, [false, false], NONE, (5.0, 5.0));
    assert!(!clicks.active(MS));
    // Pressed and released between two samples: down never shows, the press still counts.
    clicks.sample(2 * MS, [false, false], [false, true], (7.0, 8.0));
    let marks = clicks.marks(2 * MS);
    assert_eq!(marks.len(), 1);
    assert_eq!((marks[0].x, marks[0].color), (7.0, RIGHT));
    // Released and pressed again within one interval: a second ring.
    clicks.sample(3 * MS, [false, true], NONE, (7.0, 8.0));
    clicks.sample(4 * MS, [false, true], [false, true], (7.0, 8.0));
    assert_eq!(clicks.marks(4 * MS).len(), 4, "three rings and the dot");
  }
}
