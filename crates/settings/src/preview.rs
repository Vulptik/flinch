//! Monotonic preview cooldown, retained by the application across view changes.
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct PreviewCooldown {
  until: Option<Instant>,
}
impl PreviewCooldown {
  pub fn remaining(&self, now: Instant) -> u32 {
    self
      .until
      .and_then(|until| until.checked_duration_since(now))
      .map_or(0, |remaining| {
        remaining
          .as_secs()
          .saturating_add(u64::from(remaining.subsec_nanos() != 0))
          .min(u32::MAX as u64) as u32
      })
  }
  pub fn succeeded(&mut self, now: Instant, duration_ms: u64) -> Result<(), &'static str> {
    if !(1000..=10000).contains(&duration_ms) {
      return Err("Invalid notification preview duration");
    }
    self.until = Some(
      now
        .checked_add(Duration::from_millis((duration_ms + 1000).max(3000)))
        .ok_or("Notification preview deadline overflow")?,
    );
    Ok(())
  }
}
#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn preview_has_a_minimum_cooldown_and_rounds_remaining_seconds_up() {
    let now = Instant::now();
    let mut preview = PreviewCooldown::default();
    assert_eq!(preview.remaining(now), 0);
    preview.succeeded(now, 1000).expect("preview");
    assert_eq!(preview.remaining(now), 3);
    assert_eq!(preview.remaining(now + Duration::from_millis(250)), 3);
    assert_eq!(preview.remaining(now + Duration::from_secs(3)), 0);
  }
  #[test]
  fn longest_preview_includes_animation_time_and_invalid_duration_cannot_change_deadline() {
    let now = Instant::now();
    let mut preview = PreviewCooldown::default();
    preview.succeeded(now, 10000).expect("preview");
    assert!(preview.succeeded(now, u64::MAX).is_err());
    assert_eq!(preview.remaining(now + Duration::from_secs(10)), 1);
    assert_eq!(preview.remaining(now + Duration::from_secs(11)), 0);
  }
}
