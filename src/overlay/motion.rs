//! Bounded, owner-thread chrome transitions. No queued frames or background work.
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) struct Tween {
  from: f32,
  to: f32,
  started: Instant,
  duration: Duration,
}

impl Tween {
  pub fn new(value: f32, duration: Duration, now: Instant) -> Self {
    Self {
      from: value,
      to: value,
      started: now,
      duration,
    }
  }
  pub fn value(self, now: Instant) -> f32 {
    let t = (now.saturating_duration_since(self.started).as_secs_f32()
      / self.duration.as_secs_f32())
    .clamp(0.0, 1.0);
    self.from + (self.to - self.from) * t * t * (3.0 - 2.0 * t)
  }

  pub fn active(self, now: Instant) -> bool {
    self.from != self.to && now.saturating_duration_since(self.started) < self.duration
  }

  pub fn target(&mut self, target: f32, now: Instant, enabled: bool) {
    if self.to != target {
      self.from = if enabled { self.value(now) } else { target };
      self.to = target;
      self.started = now;
    }
  }
}

pub(crate) struct Motion {
  enabled: bool,
  pending_frame: bool,
  toolbar: Tween,
  popup: Tween,
  popup_key: u8,
  hover: [Tween; 61],
}

impl Motion {
  pub fn enabled(&self) -> bool {
    self.enabled
  }
  pub fn finish(&mut self) {
    self.enabled = false;
    self.pending_frame = false;
    self.toolbar.from = self.toolbar.to;
    self.popup.from = self.popup.to;
    for tween in &mut self.hover {
      tween.from = tween.to;
    }
  }
  pub fn new() -> Self {
    // Static renderer fixtures stay deterministic; timing is tested with explicit Instants.
    Self::with_enabled(windows_animates() && !cfg!(test), Instant::now())
  }

  fn with_enabled(enabled: bool, now: Instant) -> Self {
    let tween = Tween {
      from: 0.0,
      to: 0.0,
      started: now,
      duration: Duration::from_millis(120),
    };
    Self {
      enabled,
      pending_frame: false,
      toolbar: Tween {
        duration: Duration::from_millis(180),
        ..tween
      },
      popup: Tween {
        duration: Duration::from_millis(160),
        ..tween
      },
      popup_key: 0,
      hover: [tween; 61],
    }
  }

  pub fn update(&mut self, visible: bool, popup: u8, hover: Option<i32>, now: Instant) {
    self
      .toolbar
      .target(u8::from(visible) as f32, now, self.enabled);
    if !visible {
      self.toolbar.from = 0.0;
      self.toolbar.to = 0.0;
    }
    if self.popup_key != popup {
      self.popup.from = 0.0;
      self.popup.to = 0.0;
      self.popup_key = popup;
    }
    self
      .popup
      .target(u8::from(popup != 0) as f32, now, self.enabled);
    for (index, tween) in self.hover.iter_mut().enumerate() {
      tween.target(
        u8::from(visible && hover == Some(index as i32)) as f32,
        now,
        self.enabled,
      );
      if !visible {
        tween.from = 0.0;
        tween.to = 0.0;
      }
    }
    self.pending_frame = self.active_at(now);
  }

  fn active_at(&self, now: Instant) -> bool {
    self.enabled
      && (self.toolbar.active(now)
        || self.popup.active(now)
        || self.hover.iter().any(|tween| tween.active(now)))
  }

  pub fn active(&self) -> bool {
    // Keep the final repaint scheduled even if the deadline elapsed between paints.
    self.pending_frame
  }

  pub fn toolbar_alpha(&self, now: Instant) -> u8 {
    (self.toolbar.value(now) * 255.0).round() as u8
  }
  pub fn popup_alpha(&self, now: Instant) -> u8 {
    (self.popup.value(now) * 255.0).round() as u8
  }
  pub fn toolbar_offset(&self, now: Instant) -> i32 {
    ((1.0 - self.toolbar.value(now)) * 6.0).round() as i32
  }
  pub fn popup_offset(&self, now: Instant) -> i32 {
    ((1.0 - self.popup.value(now)) * 6.0).round() as i32
  }

  #[cfg(test)]
  pub fn preview(
    toolbar_age: Duration,
    popup_age: Duration,
    hover_age: Duration,
    popup: u8,
    hover: Option<i32>,
  ) -> Self {
    let now = Instant::now();
    let started = now - Duration::from_secs(1);
    let mut motion = Self::with_enabled(true, started);
    motion.update(true, popup, hover, started);
    motion.toolbar.started = now - toolbar_age;
    motion.popup.started = now - popup_age;
    for tween in &mut motion.hover {
      tween.started = now - hover_age;
    }
    motion
  }
  pub fn hover_alpha(&self, key: usize, now: Instant) -> u8 {
    self
      .hover
      .get(key)
      .map_or(0, |tween| (tween.value(now) * 255.0).round() as u8)
  }
}

pub fn blend_rgb(from: [u8; 3], to: [u8; 3], amount: u8) -> [u8; 3] {
  std::array::from_fn(|index| {
    ((u16::from(from[index]) * (255 - u16::from(amount))
      + u16::from(to[index]) * u16::from(amount)
      + 127)
      / 255) as u8
  })
}

/// Whether Windows animates controls and elements (Settings > Accessibility > Visual effects).
/// Unreadable counts as off: a transition is never worth guessing at.
pub(crate) fn windows_animates() -> bool {
  use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
  };
  use windows::core::BOOL;
  let mut enabled = BOOL(0);
  // SAFETY: Windows writes one BOOL into this live stack variable; no settings are changed.
  let result = unsafe {
    SystemParametersInfoW(
      SPI_GETCLIENTAREAANIMATION,
      0,
      Some((&mut enabled as *mut BOOL).cast()),
      SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
  };
  if let Err(error) = result {
    tracing::warn!(%error, "Animation preference unavailable; disabling overlay transitions");
    return false;
  }
  enabled.as_bool()
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn transitions_settle_and_retarget_without_jumps() {
    let now = Instant::now();
    let mut motion = Motion::with_enabled(true, now);
    motion.update(true, 1, Some(2), now);
    let mid = now + Duration::from_millis(45);
    let before = motion.hover_alpha(2, mid);
    assert!(before > 0 && before < 255);
    motion.update(true, 1, Some(3), mid);
    assert_eq!(before, motion.hover_alpha(2, mid));
    let end = now + Duration::from_secs(1);
    assert!(
      motion.active(),
      "the final frame must remain scheduled past its deadline"
    );
    motion.update(true, 1, Some(3), end);
    assert!(!motion.active());
    assert_eq!(motion.toolbar_alpha(end), 255);
    assert_eq!(motion.hover_alpha(2, end), 0);
    assert_eq!(motion.hover_alpha(3, end), 255);
    motion.update(false, 0, None, end);
    motion.update(true, 1, None, end);
    assert_eq!(motion.toolbar_alpha(end), 0);
    assert_eq!(motion.popup_alpha(end), 0);
  }
  #[test]
  fn reduced_motion_is_immediate_and_idle() {
    let now = Instant::now();
    let mut motion = Motion::with_enabled(false, now);
    motion.update(true, 1, Some(2), now);
    assert_eq!(motion.toolbar_alpha(now), 255);
    assert_eq!(motion.popup_alpha(now), 255);
    assert_eq!(motion.hover_alpha(2, now), 255);
    assert!(!motion.active());
  }
}
