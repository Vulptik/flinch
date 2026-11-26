//! Entrance at 0.4×, with a 400 ms opening reveal before the logo moves.
pub const FADE_IN_MS: u64 = 400;
pub const EMPTY_WINDOW_MS: u64 = 300;
// Extend only the original 137.5 ms reveal; subsequent movement retains 0.4x timing.
pub const DURATION_MS: u64 = EMPTY_WINDOW_MS + 2763;
const LOGO: &str = include_str!("../../settings/ui/assets/logo.svg");

/// Keep renderer startup or a stalled frame from consuming the opening fade.
/// Normal 16 ms playback keeps the 0.4x speed; slow frames stretch playback.
#[derive(Default)]
pub struct PlaybackClock {
  previous: Option<u64>,
  position: u64,
}

impl PlaybackClock {
  pub fn advance(&mut self, now_ms: u64) -> u64 {
    if let Some(previous) = self.previous {
      self.position = self
        .position
        .saturating_add(now_ms.saturating_sub(previous).min(32))
        .min(DURATION_MS);
    }
    self.previous = Some(now_ms);
    self.position
  }
}

#[derive(Debug)]
pub struct Pose {
  pub x: f32,
  pub y: f32,
  pub size: f32,
  pub content: f32,
  pub content_y: f32,
  pub bolt_x: f32,
  pub bolt_y: f32,
  pub bolt_opacity: f32,
  pub arrow_x: f32,
  pub arrow_scale: f32,
  pub arrow_opacity: f32,
  pub energy_offset: f32,
  pub energy_opacity: f32,
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
  a + (b - a) * t
}
fn segment(t: f32, a: f32, b: f32) -> f32 {
  ((t - a) / (b - a)).clamp(0.0, 1.0)
}
fn curve(t: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
  fn axis(t: f32, a: f32, b: f32) -> f32 {
    3.0 * (1.0 - t).powi(2) * t * a + 3.0 * (1.0 - t) * t * t * b + t.powi(3)
  }
  // Fixed bisection budget, including flat curve endpoints; never an unbounded solver.
  let (mut lo, mut hi) = (0.0, 1.0);
  for _ in 0..20 {
    let mid = (lo + hi) / 2.0;
    if axis(mid, x1, x2) < t {
      lo = mid;
    } else {
      hi = mid;
    }
  }
  axis((lo + hi) / 2.0, y1, y2)
}

pub fn pose(elapsed_ms: u64) -> Pose {
  let elapsed = elapsed_ms.min(DURATION_MS).saturating_sub(EMPTY_WINDOW_MS) as f32;
  let t = if elapsed <= FADE_IN_MS as f32 {
    elapsed / FADE_IN_MS as f32 * 0.055
  } else {
    (0.055 + (elapsed - FADE_IN_MS as f32) / 2500.0).min(1.0)
  };
  let dock = curve(segment(t, 0.48, 0.90), 0.76, 0.0, 0.24, 1.0);
  let strike = curve(segment(t, 0.055, 0.19), 0.6, 0.0, 1.0, 0.6);
  let settle = curve(segment(t, 0.19, 0.27), 0.16, 1.0, 0.3, 1.0);
  let snap = curve(segment(t, 0.17, 0.28), 0.16, 1.0, 0.3, 1.0);
  let arrow_settle = curve(segment(t, 0.28, 0.38), 0.2, 0.7, 0.3, 1.0);
  let content = curve(segment(t, 0.75, 1.0), 0.16, 1.0, 0.3, 1.0);
  let (bolt_x, bolt_y) = if t < 0.19 {
    (mix(78.0, -3.0, strike), mix(-130.0, 5.0, strike))
  } else {
    (mix(-3.0, 0.0, settle), mix(5.0, 0.0, settle))
  };
  let (arrow_x, arrow_scale) = if t < 0.28 {
    (mix(-62.0, 13.0, snap), mix(0.72, 1.06, snap))
  } else {
    (mix(13.0, 0.0, arrow_settle), mix(1.06, 1.0, arrow_settle))
  };
  let (energy_offset, energy_opacity) = if t < 0.19 {
    (1.0, 0.0)
  } else if t < 0.23 {
    let s = segment(t, 0.19, 0.23);
    (mix(1.0, 0.9, s), mix(0.0, 0.9, s))
  } else if t < 0.43 {
    let s = segment(t, 0.23, 0.43);
    (mix(0.9, 0.0, s), mix(0.9, 0.65, s))
  } else {
    let s = segment(t, 0.43, 0.48);
    (mix(0.0, -0.1, s), mix(0.65, 0.0, s))
  };
  Pose {
    x: mix(248.0, 32.0, dock),
    y: mix(138.0, 32.0, dock),
    size: mix(144.0, 36.0, dock),
    content,
    content_y: mix(7.0, 0.0, content),
    bolt_x,
    bolt_y,
    bolt_opacity: segment(t, 0.0, 0.055),
    arrow_x,
    arrow_scale,
    arrow_opacity: snap,
    energy_offset,
    energy_opacity,
  }
}

pub fn svg(p: &Pose) -> Result<String, String> {
  let bolt_marker = "<path fill=\"#ff202b\"";
  let arrow_marker = "<path fill=\"#9ba3ad\"";
  let start = LOGO
    .find(bolt_marker)
    .ok_or("Installer logo bolt path is missing")?;
  let end = start
    + LOGO[start..]
      .find("/>")
      .ok_or("Installer logo bolt path is incomplete")?
    + 2;
  if !LOGO.contains(arrow_marker) || !LOGO.contains("</svg>") {
    return Err("Installer logo structure is invalid".into());
  }
  // SVG's normalized pathLength is not supported by the native SVG renderer.
  // Use the canonical bolt's measured perimeter in SVG units instead.
  const PERIMETER: f32 = 636.69745;
  let energy=LOGO[start..end].replace("fill=\"#ff202b\"",&format!("fill=\"none\" stroke=\"#ffd0d3\" stroke-width=\"2.5\" stroke-linejoin=\"round\" stroke-linecap=\"round\" stroke-dasharray=\"{} {}\" stroke-dashoffset=\"{}\" opacity=\"{}\"",0.22*PERIMETER,0.78*PERIMETER,p.energy_offset*PERIMETER,p.energy_opacity));
  // Arrow transforms are around the logo SVG's own origin.
  Ok(LOGO.replace(bolt_marker,&format!("<path transform=\"translate({} {})\" opacity=\"{}\" fill=\"#ff202b\"",p.bolt_x,p.bolt_y,p.bolt_opacity))
    .replace(arrow_marker,&format!("<path transform=\"translate({} 0) translate(200 128) scale({} 1) translate(-200 -128)\" opacity=\"{}\" fill=\"#9ba3ad\"",p.arrow_x,p.arrow_scale,p.arrow_opacity))
    .replace("viewBox=\"0 0 256 256\"", "viewBox=\"-128 -160 512 576\"")
    .replace("</svg>",&format!("{energy}</svg>")))
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn startup_and_stalls_cannot_skip_the_fade() {
    let mut clock = PlaybackClock::default();
    assert_eq!(clock.advance(600), 0);
    assert_eq!(clock.advance(616), 16);
    assert_eq!(clock.advance(1216), 48);
    let mut opacity = 0.0;
    for elapsed in [0, 80, 160, 240, 320, FADE_IN_MS] {
      let current = pose(EMPTY_WINDOW_MS + elapsed).bolt_opacity;
      assert!(current >= opacity && current <= 1.0);
      opacity = current;
    }
    assert_eq!(opacity, 1.0);
    let middle = pose(EMPTY_WINDOW_MS + FADE_IN_MS / 2);
    assert!((middle.bolt_opacity - 0.5).abs() < 0.001);
    assert!((middle.bolt_x - 78.0).abs() < 0.001 && (middle.bolt_y + 130.0).abs() < 0.001);
    for now in (1232..5000).step_by(16) {
      clock.advance(now);
    }
    assert_eq!(clock.advance(u64::MAX), DURATION_MS);
  }
  #[test]
  fn window_stays_empty_before_the_logo_reveal() {
    for elapsed in [0, 80, 160, 240, EMPTY_WINDOW_MS] {
      let p = pose(elapsed);
      assert_eq!(p.bolt_opacity, 0.0);
      assert!(p.arrow_opacity < 0.001 && p.content < 0.001);
      assert_eq!(p.energy_opacity, 0.0);
    }
    assert!(pose(EMPTY_WINDOW_MS + 80).bolt_opacity > 0.0);
  }
  #[test]
  fn entrance_finishes_and_stays_docked() {
    let start = pose(0);
    assert!(start.bolt_opacity == 0.0 && start.arrow_opacity < 0.001 && start.content < 0.001);
    for elapsed in [DURATION_MS, DURATION_MS + 1000, u64::MAX] {
      let end = pose(elapsed);
      assert!((end.x - 32.0).abs() < 0.001 && (end.y - 32.0).abs() < 0.001);
      assert!((end.size - 36.0).abs() < 0.001 && end.content > 0.999 && end.energy_opacity == 0.0);
    }
  }
  #[test]
  fn every_frame_is_finite_and_uses_the_canonical_logo() {
    for elapsed in (0..=DURATION_MS).step_by(16) {
      let p = pose(elapsed);
      assert!(p.x.is_finite() && p.y.is_finite() && p.size >= 36.0 && p.size <= 144.0);
      assert!((0.0..=1.0).contains(&p.content));
      let image = svg(&p).expect("canonical logo");
      assert!(
        image.contains("M137 20 42 139h64l-24 97L184 112h-66z")
          && image.contains("stroke-dashoffset")
      );
    }
  }
}
