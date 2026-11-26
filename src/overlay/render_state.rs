// Render state types for overlay system

use std::time::Instant;

use crate::overlay::selection::Rect;

/// Animation duration for snap preview transitions (in milliseconds)
pub const SNAP_ANIMATION_DURATION_MS: f64 = 120.0;

/// Animation state for snap preview transitions (owned by render thread)
pub struct SnapAnimationState {
  /// Previous snap rect (animation start)
  pub from_rect: Option<Rect>,
  /// Target snap rect (animation end)
  pub to_rect: Option<Rect>,
  /// When the animation started
  pub start_time: Option<Instant>,
}

impl SnapAnimationState {
  pub fn new() -> Self {
    Self {
      from_rect: None,
      to_rect: None,
      start_time: None,
    }
  }

  /// Update animation target. Returns true if a new animation was started.
  pub fn update_target(&mut self, new_target: Option<Rect>) -> bool {
    let target_changed = match (&self.to_rect, &new_target) {
      (None, None) => false,
      (Some(_), None) | (None, Some(_)) => true,
      (Some(old), Some(new)) => {
        // Check if bounds changed significantly (more than 1px)
        (old.x - new.x).abs() > 1.0
          || (old.y - new.y).abs() > 1.0
          || (old.w - new.w).abs() > 1.0
          || (old.h - new.h).abs() > 1.0
      },
    };

    if target_changed {
      // Get current interpolated position as the new "from" for smooth chaining
      self.from_rect = self.get_current_rect();
      self.to_rect = new_target;
      self.start_time = Some(Instant::now());
      true
    } else {
      false
    }
  }

  /// Get the current interpolated rect for rendering
  pub fn get_current_rect(&self) -> Option<Rect> {
    let to = self.to_rect.as_ref()?;

    let Some(from) = &self.from_rect else {
      // No "from" rect - just use target directly (first snap or after reset)
      return Some(*to);
    };

    let Some(start) = self.start_time else {
      return Some(*to);
    };

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let progress = (elapsed_ms / SNAP_ANIMATION_DURATION_MS).min(1.0);

    // Use ease-out cubic for snappy but smooth animation
    let t = 1.0 - (1.0 - progress).powi(3);

    Some(Rect {
      x: lerp(from.x, to.x, t),
      y: lerp(from.y, to.y, t),
      w: lerp(from.w, to.w, t),
      h: lerp(from.h, to.h, t),
    })
  }

  /// Check if animation is currently in progress
  pub fn is_animating(&self) -> bool {
    if let Some(start) = self.start_time {
      start.elapsed().as_secs_f64() * 1000.0 < SNAP_ANIMATION_DURATION_MS
    } else {
      false
    }
  }
}

/// Linear interpolation helper
fn lerp(a: f64, b: f64, t: f64) -> f64 {
  a + (b - a) * t
}

#[cfg(test)]
mod tests {
  use std::time::{Duration, Instant};

  use super::*;

  fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { x, y, w, h }
  }

  fn assert_close(actual: f64, expected: f64, tolerance: f64) {
    assert!(
      (actual - expected).abs() <= tolerance,
      "expected {expected} +/- {tolerance}, got {actual}"
    );
  }

  fn assert_rect_eq(actual: Rect, expected: Rect) {
    assert_close(actual.x, expected.x, 0.0001);
    assert_close(actual.y, expected.y, 0.0001);
    assert_close(actual.w, expected.w, 0.0001);
    assert_close(actual.h, expected.h, 0.0001);
  }

  fn assert_rect_close(actual: Rect, expected: Rect, tolerance: f64) {
    assert_close(actual.x, expected.x, tolerance);
    assert_close(actual.y, expected.y, tolerance);
    assert_close(actual.w, expected.w, tolerance);
    assert_close(actual.h, expected.h, tolerance);
  }

  #[test]
  fn new_state_is_idle() {
    let state = SnapAnimationState::new();

    assert!(state.from_rect.is_none());
    assert!(state.to_rect.is_none());
    assert!(state.start_time.is_none());
    assert!(!state.is_animating());
    assert!(state.get_current_rect().is_none());
  }

  #[test]
  fn first_target_uses_target_rect_directly() {
    let mut state = SnapAnimationState::new();
    let target = rect(10.0, 20.0, 100.0, 50.0);

    assert!(state.update_target(Some(target)));
    assert!(state.from_rect.is_none());
    assert!(
      state.to_rect.is_some(),
      "target rect should be stored after first update"
    );
    assert_rect_eq(
      state.to_rect.expect("target rect should be present"),
      target,
    );
    assert_rect_eq(
      state
        .get_current_rect()
        .expect("current rect should be present after first update"),
      target,
    );
    assert!(state.is_animating());
  }

  #[test]
  fn identical_target_does_not_restart_animation() {
    let mut state = SnapAnimationState::new();
    let target = rect(10.0, 20.0, 100.0, 50.0);

    assert!(state.update_target(Some(target)));
    let first_start = state.start_time;

    assert!(!state.update_target(Some(target)));
    assert_eq!(state.start_time, first_start);
  }

  #[test]
  fn tiny_changes_within_one_pixel_do_not_restart_animation() {
    let mut state = SnapAnimationState::new();

    assert!(state.update_target(Some(rect(10.0, 20.0, 100.0, 50.0))));
    let first_start = state.start_time;

    assert!(!state.update_target(Some(rect(10.5, 20.5, 100.5, 50.5))));
    assert_eq!(state.start_time, first_start);
  }

  #[test]
  fn large_target_change_restarts_from_current_rect() {
    let mut state = SnapAnimationState {
      from_rect: Some(rect(0.0, 0.0, 100.0, 100.0)),
      to_rect: Some(rect(100.0, 100.0, 200.0, 200.0)),
      start_time: Some(Instant::now() - Duration::from_millis(60)),
    };

    let current_before_change = state
      .get_current_rect()
      .expect("current rect should be available");

    assert!(state.update_target(Some(rect(250.0, 250.0, 300.0, 300.0))));
    assert_rect_close(
      state.from_rect.expect("from rect should be updated"),
      current_before_change,
      0.1,
    );
    assert_rect_eq(
      state.to_rect.expect("target rect should be updated"),
      rect(250.0, 250.0, 300.0, 300.0),
    );
    assert!(state.is_animating());
  }

  #[test]
  fn completed_animation_returns_target_rect_and_is_not_animating() {
    let state = SnapAnimationState {
      from_rect: Some(rect(0.0, 0.0, 50.0, 50.0)),
      to_rect: Some(rect(100.0, 100.0, 150.0, 150.0)),
      start_time: Some(
        Instant::now() - Duration::from_millis(SNAP_ANIMATION_DURATION_MS as u64 + 5),
      ),
    };

    assert_rect_eq(
      state
        .get_current_rect()
        .expect("completed animation should still yield target rect"),
      rect(100.0, 100.0, 150.0, 150.0),
    );
    assert!(!state.is_animating());
  }

  #[test]
  fn lerp_interpolates_linearly() {
    assert_eq!(lerp(10.0, 20.0, 0.0), 10.0);
    assert_eq!(lerp(10.0, 20.0, 1.0), 20.0);
    assert_eq!(lerp(10.0, 20.0, 0.5), 15.0);
  }
}
