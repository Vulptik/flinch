//! Mixing the microphone with system audio into the one AAC track, and muting the microphone
//! from the recording controls at exact media frames.
use crate::{EngineError, ErrorKind, Result};
use std::collections::VecDeque;

/// A mute or unmute ramps over 10 ms at 48 kHz, so it never clicks.
const RAMP_FRAMES: f32 = 480.0;
/// Mute switches waiting for their frame; far more than anyone can click in the reorder window.
const MAX_SWITCHES: usize = 64;
/// Below this level samples pass unchanged; above it they bend smoothly towards full scale.
const KNEE: f32 = 0.8;

/// Two sources summed can pass full scale; instead of clipping hard, the loudest peaks bend
/// smoothly below it. Quiet and ordinary levels are untouched.
pub fn limit(sample: f32) -> f32 {
  let level = sample.abs();
  if level <= KNEE {
    return sample;
  }
  let room = 1.0 - KNEE;
  (KNEE + room * ((level - KNEE) / room).tanh()).copysign(sample)
}

/// The 16-bit sample the AAC encoder takes, rounded like the single-source path always was.
pub fn to_i16(sample: f32) -> i16 {
  (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// When the microphone is audible, by 48 kHz media frame.
#[derive(Debug)]
pub struct MuteTimeline {
  switches: VecDeque<(u64, bool)>,
  /// Gain now (0 muted, 1 audible) and where it is heading.
  gain: f32,
  target: f32,
}
impl Default for MuteTimeline {
  fn default() -> Self {
    Self {
      switches: VecDeque::new(),
      gain: 1.0,
      target: 1.0,
    }
  }
}
impl MuteTimeline {
  /// From `frame` on, the microphone is `audible` (or muted). Frames only move forward.
  pub fn switch(&mut self, frame: u64, audible: bool) -> Result<()> {
    if self
      .switches
      .back()
      .is_some_and(|(previous, _)| frame < *previous)
    {
      return Err(EngineError::protocol(
        "Microphone switch precedes an earlier one",
      ));
    }
    if self.switches.len() >= MAX_SWITCHES {
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "microphone",
        "Too many microphone switches are waiting",
      ));
    }
    self.switches.push_back((frame, audible));
    Ok(())
  }
  /// Applies the gain to interleaved stereo `pcm` whose first frame is `first`.
  pub fn apply(&mut self, first: u64, pcm: &mut [f32]) {
    for (index, pair) in pcm.as_chunks_mut::<2>().0.iter_mut().enumerate() {
      let frame = first + index as u64;
      while let Some((at, audible)) = self.switches.front().copied()
        && at <= frame
      {
        self.target = if audible { 1.0 } else { 0.0 };
        self.switches.pop_front();
      }
      if self.gain != self.target {
        let step = 1.0 / RAMP_FRAMES;
        self.gain = if self.gain < self.target {
          (self.gain + step).min(self.target)
        } else {
          (self.gain - step).max(self.target)
        };
      }
      if self.gain < 1.0 {
        pair[0] *= self.gain;
        pair[1] *= self.gain;
      }
    }
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;

  #[test]
  fn ordinary_levels_pass_and_peaks_bend_below_full_scale() {
    assert_eq!(limit(0.5), 0.5);
    assert_eq!(limit(-0.8), -0.8);
    for level in [0.9f32, 1.0, 1.5, 2.0, 10.0] {
      let bent = limit(level);
      assert!(bent > KNEE && bent <= 1.0, "{level} -> {bent}");
      assert_eq!(limit(-level), -bent);
    }
    // Monotonic: louder input is never quieter output.
    let mut previous = 0.0;
    for step in 0..400 {
      let value = limit(step as f32 / 100.0);
      assert!(value >= previous);
      previous = value;
    }
    assert_eq!(to_i16(1.0), 32767);
    assert_eq!(to_i16(-1.0), -32767);
    assert_eq!(to_i16(0.5), 16384);
  }

  #[test]
  fn muting_ramps_down_at_its_frame_and_unmuting_ramps_back() {
    let mut mutes = MuteTimeline::default();
    mutes.switch(1000, false).expect("mute");
    mutes.switch(3000, true).expect("unmute");
    let mut pcm = vec![1.0f32; 4000 * 2];
    mutes.apply(0, &mut pcm[..2000 * 2]);
    mutes.apply(2000, &mut pcm[2000 * 2..]);
    let at = |frame: usize| pcm[frame * 2];
    assert_eq!(at(999), 1.0, "before the switch");
    assert!(
      at(1000) < 1.0 && at(1000) > 0.99,
      "the ramp starts at its frame"
    );
    assert!(at(1240) > 0.4 && at(1240) < 0.6, "halfway down");
    assert_eq!(at(1480), 0.0);
    assert_eq!(at(2999), 0.0, "muted across chunks");
    assert!(at(3000) > 0.0 && at(3000) < 0.01);
    assert_eq!(at(3480), 1.0);
    assert_eq!(pcm[3480 * 2 + 1], 1.0, "both channels");
    // Switches never go back in time, and a burst is bounded.
    assert!(mutes.switch(10, true).is_ok(), "after the applied ones");
    assert!(mutes.switch(5, false).is_err());
    let mut flood = MuteTimeline::default();
    for frame in 0..MAX_SWITCHES as u64 {
      flood.switch(frame, frame % 2 == 0).expect("bounded");
    }
    assert_eq!(
      flood.switch(100, true).map_err(|error| error.kind),
      Err(ErrorKind::Saturation)
    );
  }
}
