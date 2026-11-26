//! Integer media time, deliberately independent from packet arrival and UTC.
use crate::{EngineError, ErrorKind, Result};
use serde::{Deserialize, Serialize};

pub const SECOND: u64 = 10_000_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QpcTicks(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MediaTime(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameIndex(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFrame(pub u64);

pub fn scale(value: u64, numerator: u64, denominator: u64) -> Result<u64> {
  if denominator == 0 {
    return Err(EngineError::new(
      ErrorKind::Native,
      "clock",
      "Zero timebase denominator",
    ));
  }
  u64::try_from(u128::from(value) * u128::from(numerator) / u128::from(denominator))
    .map_err(|_| EngineError::new(ErrorKind::Native, "clock", "Time conversion overflow"))
}
/// Inverse of the native floor-rounded sample timestamp; a second floor would lose a sample.
pub fn sample_boundary(media_100ns: u64, rate: u32) -> Result<u64> {
  if rate == 0 || u64::from(rate) > SECOND {
    return Err(EngineError::protocol(
      "Audio rate exceeds the timestamp resolution",
    ));
  }
  let boundary = ((u128::from(media_100ns) + 1) * u128::from(rate) - 1) / u128::from(SECOND);
  u64::try_from(boundary).map_err(|_| EngineError::protocol("Audio boundary overflow"))
}
pub fn frame_time(index: u64, fps: u32) -> Result<u64> {
  scale(index, SECOND, u64::from(fps))
}
pub fn frame_ceil(time: u64, fps: u32) -> Result<u64> {
  if fps == 0 {
    return Err(EngineError::protocol("Zero FPS"));
  }
  u64::try_from((u128::from(time) * u128::from(fps)).div_ceil(u128::from(SECOND)))
    .map_err(|_| EngineError::protocol("Frame index overflow"))
}

#[derive(Clone, Debug, Default)]
pub struct Timeline {
  epoch: Option<u64>,
  paused_at: Option<u64>,
  paused_total: u64,
  last_qpc: Option<u64>,
  active_since: Option<u64>,
  frozen: Option<u64>,
}
impl Timeline {
  pub fn start(&mut self, qpc_100ns: u64) -> Result<()> {
    if self.epoch.is_some() {
      return Err(EngineError::protocol("Timeline already started"));
    }
    self.epoch = Some(qpc_100ns);
    self.last_qpc = Some(qpc_100ns);
    self.active_since = Some(qpc_100ns);
    Ok(())
  }
  pub fn at(&mut self, qpc_100ns: u64) -> Result<u64> {
    let epoch = self
      .epoch
      .ok_or_else(|| EngineError::protocol("Timeline not started"))?;
    if qpc_100ns < epoch || self.last_qpc.is_some_and(|last| qpc_100ns < last) {
      return Err(EngineError::new(
        ErrorKind::Native,
        "clock",
        "QPC moved backwards",
      ));
    }
    self.last_qpc = Some(qpc_100ns);
    Ok(
      self
        .frozen
        .unwrap_or(self.paused_at.unwrap_or(qpc_100ns) - epoch - self.paused_total),
    )
  }
  pub fn pause(&mut self, qpc_100ns: u64) -> Result<u64> {
    self.pause_at(qpc_100ns, qpc_100ns)
  }
  /// A committed media cut may precede the latest observation after preemption.
  /// Only observed_qpc is a new clock reading; cut_qpc must belong to the active interval.
  pub fn pause_at(&mut self, cut_qpc: u64, observed_qpc: u64) -> Result<u64> {
    if self.paused_at.is_some() || self.frozen.is_some() {
      return Err(EngineError::protocol("Timeline cannot pause"));
    }
    if self.active_since.is_none_or(|since| cut_qpc < since) || cut_qpc > observed_qpc {
      return Err(EngineError::protocol(
        "Pause cutoff is outside the current active interval",
      ));
    }
    self.at(observed_qpc)?;
    let at = cut_qpc
      .checked_sub(
        self
          .epoch
          .ok_or_else(|| EngineError::protocol("Timeline not started"))?,
      )
      .and_then(|elapsed| elapsed.checked_sub(self.paused_total))
      .ok_or_else(|| EngineError::protocol("Pause cutoff precedes committed media"))?;
    self.paused_at = Some(cut_qpc);
    Ok(at)
  }
  pub fn resume(&mut self, qpc_100ns: u64) -> Result<()> {
    let pause = self
      .paused_at
      .ok_or_else(|| EngineError::protocol("Timeline is not paused"))?;
    self.at(qpc_100ns)?;
    self.paused_total = self
      .paused_total
      .checked_add(qpc_100ns - pause)
      .ok_or_else(|| EngineError::protocol("Pause duration overflow"))?;
    self.paused_at = None;
    self.last_qpc = Some(qpc_100ns);
    self.active_since = Some(qpc_100ns);
    Ok(())
  }
  pub fn stop(&mut self, qpc_100ns: u64) -> Result<u64> {
    let at = self.at(qpc_100ns)?;
    self.frozen = Some(at);
    self.last_qpc = Some(qpc_100ns);
    Ok(at)
  }
  pub fn paused(&self) -> bool {
    self.paused_at.is_some()
  }
}

#[derive(Debug)]
pub struct Scheduler {
  pub fps: u32,
  next: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Slot {
  pub index: u64,
  pub pts: u64,
  pub duration: u64,
  pub skipped: u64,
}
impl Scheduler {
  pub fn new(fps: u32) -> Result<Self> {
    if !crate::types::SUPPORTED_FPS.contains(&fps) {
      return Err(EngineError::protocol("Unsupported cadence"));
    }
    Ok(Self { fps, next: 0 })
  }
  pub fn due(&mut self, elapsed: u64) -> Result<Option<Slot>> {
    if elapsed < frame_time(self.next, self.fps)? {
      return Ok(None);
    }
    let current = scale(elapsed, u64::from(self.fps), SECOND)?.max(self.next);
    let skipped = current - self.next;
    self.next = current
      .checked_add(1)
      .ok_or_else(|| EngineError::protocol("Frame counter overflow"))?;
    let pts = frame_time(current, self.fps)?;
    Ok(Some(Slot {
      index: current,
      pts,
      duration: frame_time(self.next, self.fps)? - pts,
      skipped,
    }))
  }
  pub fn next_time(&self) -> Result<u64> {
    frame_time(self.next, self.fps)
  }
}
impl Slot {
  /// Date a phase-locked slot at the instant it was sampled: its nominal time plus the lock's
  /// sampling delay (below one frame). A display is never exactly at its nominal rate against
  /// QPC (119.996 Hz measured), so keeping grid PTS made the shown content's age slide
  /// against them and wrap by one refresh. Dated here, the age stays at the margin; PTS remain
  /// strictly increasing and the nominal duration is kept.
  pub fn sampled(self, delay: u64) -> Result<Self> {
    let pts = self
      .pts
      .checked_add(delay)
      .ok_or_else(|| EngineError::protocol("Sampled slot time overflow"))?;
    Ok(Self { pts, ..self })
  }
  /// Date the slot `by` earlier, onto the earlier present it captured.
  pub fn earlier(self, by: u64) -> Result<Self> {
    let pts = self
      .pts
      .checked_sub(by)
      .ok_or_else(|| EngineError::protocol("Redated slot precedes the media origin"))?;
    Ok(Self { pts, ..self })
  }
}

/// Sampling three quarters of a refresh after the tracked present phase finds the newest present
/// delivered, still ahead of the next one. Traced at 120 Hz, a slot missed the
/// newest present and captured content one refresh older in 12% of slots with a fixed 2 ms,
/// 1.6–4.8% with half a refresh (delivery under GPU load can exceed 4 ms) and 0.1–1% with this.
/// Those remaining are vblanks DWM never composed (35 of 35 traced: one present reported, then
/// two), when the screen itself still showed the older image; waiting for them cannot help.
pub fn phase_margin(period_100ns: u64) -> u64 {
  period_100ns * 3 / 4
}
/// Consecutive rejected presents that mean the display's phase really moved (for example a mode
/// change), not jitter.
const PHASE_RESEED_OUTLIERS: u32 = 8;

/// Observations before the lock judges its own fit.
const PHASE_CHECK_OBSERVATIONS: u64 = 60;

/// Hybrid sync policy: the number of composition refreshes per frame when the frame period is a
/// whole number (1–4) of them, within 1% of a refresh (59.94 Hz included). Otherwise (144 or
/// 165 Hz at 60 FPS) no alignment can hold a constant phase, and slots keep the fixed grid.
pub fn refreshes_per_frame(fps: u32, period_100ns: u64) -> Option<u64> {
  if fps == 0 || period_100ns == 0 {
    return None;
  }
  let frame = SECOND / u64::from(fps);
  let multiple = (frame + period_100ns / 2) / period_100ns;
  ((1..=4).contains(&multiple) && frame.abs_diff(multiple * period_100ns) * 100 <= period_100ns)
    .then_some(multiple)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseLockReport {
  /// DWM's measured composition period, which stamps presents.
  pub period_100ns: u64,
  pub observations: u64,
  pub outliers: u64,
  pub reseeds: u64,
  /// The presents did not follow the composition period (more than a quarter rejected after the
  /// first second), so the session went back to the fixed grid rather than sample at a wrong phase.
  pub abandoned: bool,
  /// Slots whose late tick captured the following present and were redated onto it.
  #[serde(default)]
  pub late_captures: u64,
  /// Slots whose fresh capture was the present before the aimed one (DWM had not composed the
  /// aimed one) and were redated onto it.
  #[serde(default)]
  pub early_captures: u64,
}

/// Tracks the display's present phase in QPC time so each slot samples just after a present.
/// It moves when a slot is sampled by less than one refresh, and `Slot::sampled` dates the slot
/// there, so the shown content's age holds against its PTS. Both media clocks stay on QPC, so
/// video cannot drift from audio. Pauses do not disturb it: the phase is absolute.
#[derive(Clone, Debug)]
pub struct PhaseLock {
  /// One frame period: the delay stays below it so a pause or stop cut never loses a slot, even
  /// when a 59.94 Hz refresh is slightly longer than a 60 FPS frame.
  frame: u64,
  /// Composition refreshes per frame (1–4).
  refreshes: u64,
  phase: Option<u64>,
  outliers_in_row: u32,
  report: PhaseLockReport,
  /// The delay that dated the latest slot. With several presents per frame, later slots keep to
  /// the nearest one, so present jitter at a refresh boundary cannot flap the delay by a refresh
  /// (measured: 0 ↔ 8.3 ms flips at 120 Hz, uneven frame spacing for seconds).
  committed: Option<u64>,
}
impl PhaseLock {
  /// A lock for DWM's measured composition period, when the hybrid policy applies.
  pub fn for_composition(fps: u32, period_100ns: Option<u64>) -> Option<Self> {
    let period_100ns = period_100ns?;
    let refreshes = refreshes_per_frame(fps, period_100ns)?;
    Some(Self {
      frame: SECOND / u64::from(fps),
      refreshes,
      phase: None,
      outliers_in_row: 0,
      committed: None,
      report: PhaseLockReport {
        period_100ns,
        ..Default::default()
      },
    })
  }
  pub fn report(&self) -> PhaseLockReport {
    self.report
  }
  /// Fold one present timestamp (QPC, 100 ns) into the phase estimate with a bounded 1/8 step.
  /// A present more than a quarter period off is jitter and ignored, unless it persists.
  pub fn observe(&mut self, present: u64) {
    if self.report.abandoned {
      return;
    }
    self.observe_present(present);
    let report = &mut self.report;
    if report.observations >= PHASE_CHECK_OBSERVATIONS && report.outliers * 4 > report.observations
    {
      // Once abandoned, the session keeps the fixed grid: no flapping between policies.
      report.abandoned = true;
    }
  }
  fn observe_present(&mut self, present: u64) {
    let period = self.report.period_100ns;
    let at = present % period;
    self.report.observations += 1;
    let Some(phase) = self.phase else {
      self.phase = Some(at);
      return;
    };
    let ahead = (at + period - phase) % period;
    let (forward, distance) = if ahead <= period / 2 {
      (true, ahead)
    } else {
      (false, period - ahead)
    };
    if distance > period / 4 {
      self.report.outliers += 1;
      self.outliers_in_row += 1;
      if self.outliers_in_row >= PHASE_RESEED_OUTLIERS {
        self.phase = Some(at);
        self.outliers_in_row = 0;
        self.report.reseeds += 1;
      }
      return;
    }
    self.outliers_in_row = 0;
    let step = distance / 8;
    self.phase = Some(if forward {
      (phase + step) % period
    } else {
      (phase + period - step) % period
    });
  }
  /// Delay after a slot's nominal QPC time until a tracked present plus the delivery margin:
  /// of the presents within the frame, the one nearest the committed delay (the first before any
  /// commit). Always below one frame, and zero before any present was observed.
  pub fn delay(&self, nominal: u64) -> u64 {
    let Some(phase) = self.phase.filter(|_| !self.report.abandoned) else {
      return 0;
    };
    let period = self.report.period_100ns;
    let target = (phase + phase_margin(period)) % period;
    let first = (target + period - nominal % period) % period;
    let candidates = (0..4u64)
      .map(|n| first + n * period)
      .take_while(|delay| *delay < self.frame);
    match self.committed {
      Some(previous) => candidates.min_by_key(|delay| delay.abs_diff(previous)),
      None => candidates.min(),
    }
    .unwrap_or(self.frame - 1)
  }
  /// Record the delay that dated a slot; later slots stay nearest to it.
  pub fn commit(&mut self, delay: u64) {
    self.committed = Some(delay);
  }
  /// A slot dated with `delay` and sampled at `sample` (QPC) whose tick ran late can capture the
  /// present after the one it aimed at (measured: about 1% of slots at 120 Hz, a 120 s
  /// run failing on one marker dated a refresh early). The slot is dated a refresh later, on the
  /// present it shows. When that present is a candidate within the frame, later slots follow it,
  /// exactly like a wrap; otherwise they keep their present a full frame on, still after this one.
  /// Within an eighth of a refresh of the frame end a forced wrap may bring the next slot back to
  /// that same present, so the slot keeps its date there. Returns the PTS shift, or `None`.
  pub fn late_capture(&mut self, delay: u64, sample: u64, source: Option<u64>) -> Option<u64> {
    if self.refreshes < 2 || self.report.abandoned || self.phase.is_none() {
      return None;
    }
    let period = self.report.period_100ns;
    let expected = sample.checked_sub(phase_margin(period))?;
    let next = source.is_some_and(|source| {
      source >= expected + period / 2 && source < expected + period + period / 2
    });
    let later = delay + period;
    if !next || delay + period / 8 >= self.frame {
      return None;
    }
    if later < self.frame {
      self.committed = Some(later);
    }
    self.report.late_captures += 1;
    Some(period)
  }
  /// A slot sampled at `sample` whose fresh capture is the present before the one it aimed at:
  /// DWM had not composed the aimed present (traced: it never arrives), so the screen
  /// still showed the earlier one, which may already carry new content (a 120 s run failed on one
  /// pulse onset dated a refresh late). Returns the PTS shift back onto that present; the caller
  /// keeps it only while the slot still follows the previous one.
  pub fn early_capture(&mut self, sample: u64, source: Option<u64>) -> Option<u64> {
    if self.report.abandoned || self.phase.is_none() {
      return None;
    }
    let period = self.report.period_100ns;
    let expected = sample.checked_sub(phase_margin(period))?;
    let previous = source.is_some_and(|source| {
      source + period / 2 <= expected && source + period + period / 2 > expected
    });
    previous.then_some(period)
  }
  /// Count an early capture the caller redated.
  pub fn record_early(&mut self) {
    self.report.early_captures += 1;
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn every_observation_rejects_reversal_without_moving_the_last_valid_clock() {
    let mut timeline = Timeline::default();
    timeline.start(100).expect("epoch");
    assert_eq!(timeline.at(300).expect("active tick"), 200);
    assert!(timeline.at(200).is_err(), "between-tick reversal");
    assert!(
      timeline.pause(250).is_err(),
      "failed reads must not lower the guard"
    );
    timeline.pause(400).expect("pause");
    assert_eq!(timeline.at(600).expect("paused tick"), 300);
    assert!(
      timeline.resume(500).is_err(),
      "paused observations still order QPC"
    );
    timeline.resume(700).expect("resume");
    timeline.stop(800).expect("stop");
    assert_eq!(timeline.at(1000).expect("frozen media"), 400);
    assert!(
      timeline.at(900).is_err(),
      "frozen media still observes clock order"
    );
  }
  #[test]
  fn preempted_pause_keeps_its_committed_cut_without_backdating_clock_observations() {
    let mut timeline = Timeline::default();
    timeline.start(100).expect("epoch");
    timeline.at(500).expect("late active tick");
    assert_eq!(
      timeline
        .pause_at(400, 600)
        .expect("cut precedes observation"),
      300
    );
    assert!(
      timeline.at(550).is_err(),
      "cut must not lower the clock guard"
    );
    assert_eq!(timeline.at(700).expect("paused media"), 300);
    timeline.resume(800).expect("resume");
    assert!(
      timeline.pause_at(750, 900).is_err(),
      "cut cannot reach into the previous interval"
    );
    assert!(
      timeline.pause_at(1000, 900).is_err(),
      "future cut cannot be committed yet"
    );
    assert_eq!(
      timeline.at(900).expect("valid tick after rejected cuts"),
      400
    );
  }
  #[test]
  fn sample_cutoffs_invert_native_timestamps_without_rounding_loss() {
    for rate in [44100, 48000, 96000] {
      for index in [0, 1, 2, 31, 800, 1600, 10_000_000, 48_000 * 10800] {
        let timestamp = scale(index, SECOND, u64::from(rate)).expect("native sample timestamp");
        assert_eq!(
          sample_boundary(timestamp, rate).expect("inverse boundary"),
          index
        );
      }
    }
    for frame in 1..=120 {
      let cut = frame_time(frame, 60).expect("video cutoff");
      assert_eq!(
        sample_boundary(cut, 48000).expect("audio cutoff"),
        frame * 800
      );
    }
    assert!(sample_boundary(u64::MAX, 48000).is_ok());
    assert!(sample_boundary(1, 0).is_err());
  }
  #[test]
  fn long_timeline_has_no_accumulated_rounding() {
    for fps in crate::types::SUPPORTED_FPS {
      assert_eq!(
        frame_time(3 * 3600 * u64::from(fps), fps).expect("valid test fixture"),
        3 * 3600 * SECOND
      );
    }
    assert!(scale(u64::MAX, u64::MAX, 1).is_err());
    assert!(scale(1, 1, 0).is_err());
  }
  #[test]
  fn pauses_exclude_only_paused_intervals_and_stop_is_frozen() {
    let mut t = Timeline::default();
    t.start(500).expect("valid test fixture");
    assert_eq!(t.pause(600).expect("valid test fixture"), 100);
    assert_eq!(t.at(700).expect("valid test fixture"), 100);
    assert!(t.resume(599).is_err());
    t.resume(800).expect("valid test fixture");
    assert_eq!(t.at(900).expect("valid test fixture"), 200);
    assert_eq!(t.stop(950).expect("valid test fixture"), 250);
    assert_eq!(t.at(1000).expect("valid test fixture"), 250);
    assert!(t.pause(1000).is_err());
    assert!(t.start(1000).is_err());
  }
  #[test]
  fn delayed_scheduler_skips_slots_instead_of_catching_up() {
    let mut s = Scheduler::new(60).expect("valid test fixture");
    assert_eq!(
      s.due(0)
        .expect("valid test fixture")
        .expect("valid test fixture")
        .index,
      0
    );
    assert!(s.due(1).expect("valid test fixture").is_none());
    let slot = s
      .due(SECOND)
      .expect("valid test fixture")
      .expect("valid test fixture");
    assert_eq!(slot.index, 60);
    assert_eq!(slot.skipped, 59);
    assert!(s.due(SECOND).expect("valid test fixture").is_none());
  }
  #[test]
  fn every_supported_cadence_tiles_media_time_exactly_and_rejects_others() {
    for fps in crate::types::SUPPORTED_FPS {
      let mut scheduler = Scheduler::new(fps).expect("supported cadence");
      let (mut covered, mut slots) = (0, 0);
      // Poll at 1 ms for 2 s, as the coordinator does at most every 2 ms.
      for millisecond in 0..2000 {
        if let Some(slot) = scheduler.due(millisecond * SECOND / 1000).expect("slot") {
          assert_eq!(slot.pts, covered, "{fps} FPS slots must be contiguous");
          assert_eq!(slot.skipped, 0, "{fps} FPS never skips at 1 ms polling");
          covered += slot.duration;
          slots += 1;
        }
      }
      assert_eq!(slots, 2 * u64::from(fps));
      assert_eq!(covered, 2 * SECOND);
    }
    for fps in [0, 24, 25, 144] {
      assert!(Scheduler::new(fps).is_err(), "{fps}");
    }
  }

  /// Presents of a display with `period` (100 ns) and `phase`, with deterministic jitter.
  fn presents(period: u64, phase: u64, count: u64, jitter: u64) -> Vec<u64> {
    (0..count)
      .map(|n| {
        let wobble = (n * 7919) % (2 * jitter + 1);
        1_000_000_000 + phase + n * period + wobble - jitter
      })
      .collect()
  }
  const DELIVERY: u64 = 5_000;
  /// Content age at each 60 FPS slot when slots sample `delay` after nominal time.
  fn ages(presents: &[u64], lock: Option<&mut PhaseLock>, slots: u64) -> Vec<u64> {
    let mut lock = lock;
    let start = presents[0];
    let mut seen = 0;
    let mut out = Vec::new();
    for k in 1..slots {
      let nominal = start + k * SECOND / 60;
      let delay = lock.as_ref().map_or(0, |lock| lock.delay(nominal));
      assert!(delay < SECOND / 60, "delay below one frame");
      let sample = nominal + delay;
      // Duplication delivers a present 0.5 ms after it; the slot shows the newest delivered.
      while seen + 1 < presents.len() && presents[seen + 1] + DELIVERY <= sample {
        seen += 1;
        if let Some(lock) = lock.as_mut() {
          lock.observe(presents[seen]);
        }
      }
      out.push(sample - presents[seen]);
    }
    out
  }
  fn spread(values: &[u64]) -> u64 {
    values.iter().max().copied().unwrap_or(0) - values.iter().min().copied().unwrap_or(0)
  }

  #[test]
  fn the_hybrid_policy_locks_only_to_whole_multiples_of_the_frame_rate() {
    let ntsc = SECOND * 1001 / 60_000;
    for (fps, period, multiple) in [
      (60, SECOND / 60, Some(1)),
      (60, ntsc, Some(1)),
      (60, SECOND / 120, Some(2)),
      (60, SECOND * 1001 / 120_000, Some(2)),
      (60, SECOND / 240, Some(4)),
      (30, SECOND / 60, Some(2)),
      (15, SECOND / 60, Some(4)),
      (60, SECOND / 144, None),
      (60, SECOND / 165, None),
      (60, SECOND / 75, None),
      (30, SECOND / 144, None),
      (60, SECOND / 300, None),
      (60, 0, None),
      (0, SECOND / 60, None),
    ] {
      assert_eq!(
        refreshes_per_frame(fps, period),
        multiple,
        "{fps} FPS on a {period} period"
      );
    }
    assert!(
      PhaseLock::for_composition(60, None).is_none(),
      "unknown cadence"
    );
    assert!(PhaseLock::for_composition(60, Some(SECOND / 144)).is_none());
    let lock = PhaseLock::for_composition(60, Some(83_333)).expect("120 Hz locks");
    assert_eq!(
      lock.report().period_100ns,
      83_333,
      "the measured period is kept"
    );
    assert_eq!(lock.delay(123_456), 0, "no delay before any present");
  }

  #[test]
  fn a_lock_whose_presents_follow_another_cadence_abandons_itself() {
    // Measured: a 60 Hz indirect display whose presents follow the 144 Hz primary.
    // A lock on the wrong period rejects most presents; it must go back to the fixed grid.
    let mut lock = PhaseLock::for_composition(60, Some(SECOND / 60)).expect("lock");
    let composed_at_144 = presents(SECOND / 144, 0, 400, 0);
    let content = composed_at_144
      .iter()
      .step_by(2)
      .chain(composed_at_144.iter().skip(1).step_by(5));
    let mut content: Vec<u64> = content.copied().collect();
    content.sort_unstable();
    for (n, present) in content.iter().enumerate() {
      lock.observe(*present);
      if lock.report().abandoned {
        assert!(
          n + 1 >= PHASE_CHECK_OBSERVATIONS as usize,
          "judged only after a second"
        );
        break;
      }
    }
    let report = lock.report();
    assert!(report.abandoned, "{report:?}");
    assert_eq!(
      lock.delay(content[0] + 1),
      0,
      "fixed grid after abandonment"
    );
    let observed = report.observations;
    lock.observe(content[0]);
    assert_eq!(lock.report().observations, observed, "no further tracking");
    // A lock that fits its presents is never abandoned.
    let mut fitting = PhaseLock::for_composition(60, Some(SECOND / 60)).expect("lock");
    for present in presents(SECOND / 60, 5_000, 600, 2_000) {
      fitting.observe(present);
    }
    assert!(!fitting.report().abandoned);
  }

  #[test]
  fn a_locked_slot_samples_just_after_each_present_at_a_constant_age() {
    // 120 Hz display at an arbitrary phase with ±0.3 ms present jitter, ten seconds.
    let period = SECOND / 120;
    let display = presents(period, 31_337, 1200, 3_000);
    let mut lock = PhaseLock::for_composition(60, Some(SECOND / 120)).expect("lock");
    let locked = ages(&display, Some(&mut lock), 600);
    // Skip the first second while the estimate settles.
    let settled = &locked[60..];
    // Age is the margin, give or take the injected present jitter; never before delivery.
    assert!(
      settled
        .iter()
        .all(|age| age.abs_diff(phase_margin(period)) <= 3_500),
      "ages {:?}",
      &settled[..8]
    );
    assert!(
      spread(settled) <= 7_000,
      "locked spread {}",
      spread(settled)
    );
    assert_eq!(lock.report().reseeds, 0);
  }

  #[test]
  fn a_drifting_display_is_tracked_at_a_constant_age() {
    // A 59.94 Hz mode slips a whole frame against 60 FPS every ~16.7 s. Track it for a minute:
    // between wraps the age stays near the margin.
    let period = SECOND * 1001 / 60_000;
    let display = presents(period, 5_000, 3600, 2_000);
    let unlocked = ages(&display, None, 3590);
    let mut lock =
      PhaseLock::for_composition(60, Some(SECOND * 1001 / 60_000)).expect("59.94 Hz locks to 60");
    let locked = ages(&display, Some(&mut lock), 3590);
    // Unlocked, the age sweeps a whole refresh as the display slips against the grid.
    assert!(
      spread(&unlocked[60..]) >= 150_000,
      "unlocked {}",
      spread(&unlocked[60..])
    );
    let near_margin = locked[60..]
      .iter()
      .filter(|age| **age <= phase_margin(period) + 10_000)
      .count();
    assert!(
      near_margin * 100 >= (locked.len() - 60) * 95,
      "{near_margin} of {} slots within 1 ms of the margin",
      locked.len() - 60
    );
  }

  #[test]
  fn locked_slots_are_dated_when_sampled_so_the_shown_age_holds_against_their_pts() {
    // 119.996 Hz (DWM period 83,336 measured) for five minutes at 60 FPS. With grid
    // PTS the shown age slid ~35 ppm and wrapped by one refresh; dated when sampled it holds.
    let period = 83_336;
    let display = presents(period, 12_345, 36_100, 3_000);
    let mut lock = PhaseLock::for_composition(60, Some(period)).expect("120 Hz locks to 60");
    let mut scheduler = Scheduler::new(60).expect("scheduler");
    let start = display[0];
    let (mut seen, mut previous) = (0, None);
    let (mut sampled, mut grid) = (Vec::new(), Vec::new());
    let (mut committed, mut jumps) = (None, 0);
    for tick in 0..(300 * 10_000u64) {
      let elapsed = tick * 1_000;
      let delay = lock.delay(start + scheduler.next_time().expect("next"));
      let Some(slot) = scheduler.due(elapsed.saturating_sub(delay)).expect("due") else {
        continue;
      };
      // As the runtime does: date the slot, then acquire the newest present delivered by now.
      let nominal = slot.pts;
      let delay = lock.delay(start + nominal);
      lock.commit(delay);
      if let Some(before) = committed.replace(delay) {
        jumps += u64::from(delay.abs_diff(before) > period / 2);
      }
      let dated = slot.sampled(delay).expect("sampled");
      while seen + 1 < display.len() && display[seen + 1] + DELIVERY <= start + elapsed {
        seen += 1;
        lock.observe(display[seen]);
      }
      assert!(dated.pts < nominal + SECOND / 60, "within one frame");
      assert!(
        previous.is_none_or(|p| dated.pts > p),
        "strictly increasing"
      );
      previous = Some(dated.pts);
      if nominal >= SECOND {
        let shown = |pts: u64| (start + pts) as i64 - display[seen] as i64;
        sampled.push(shown(dated.pts));
        grid.push(shown(nominal));
      }
    }
    let range = |ages: &[i64]| {
      ages.iter().max().copied().unwrap_or(0) - ages.iter().min().copied().unwrap_or(0)
    };
    assert!(sampled.len() > 17_000);
    // Present jitter (±0.3 ms) and the 0.1 ms tick remain; no refresh-sized slide or wrap.
    assert!(
      range(&sampled) <= 10_000,
      "sampled age spread {}",
      range(&sampled)
    );
    // The delay walks ~35 ppm and only a forced wrap (about every 8 minutes) moves it a refresh.
    assert!(jumps <= 1, "{jumps} refresh-sized delay jumps");
    let grid_spread = range(&grid);
    assert!(
      grid_spread >= 80_000,
      "grid PTS sweep a refresh: {grid_spread}"
    );
  }

  #[test]
  fn jitter_at_a_refresh_boundary_does_not_flap_the_delay_between_presents() {
    // 120 Hz with the sampling target exactly on a slot boundary and ±0.3 ms present jitter.
    let period = SECOND / 120;
    let margin = phase_margin(period);
    let display = presents(period, period - margin, 2400, 3_000);
    let start = 1_000_000_000;
    let mut flapping = PhaseLock::for_composition(60, Some(period)).expect("lock");
    let mut steady = flapping.clone();
    let (mut flaps, mut jumps, mut seen) = (0, 0, 0);
    let (mut last_flapping, mut last_steady) = (None::<u64>, None::<u64>);
    for k in 0..1200u64 {
      let nominal = start + k * (SECOND / 60);
      while seen + 1 < display.len() && display[seen + 1] <= nominal {
        seen += 1;
        flapping.observe(display[seen]);
        steady.observe(display[seen]);
      }
      // Without commits each slot takes the first present, which jitter moves across the boundary.
      let free = flapping.delay(nominal);
      if let Some(before) = last_flapping.replace(free) {
        flaps += u64::from(free.abs_diff(before) > period / 2);
      }
      let kept = steady.delay(nominal);
      steady.commit(kept);
      // The first second covers the lock engaging (from no phase to the first present).
      if let Some(before) = last_steady.replace(kept)
        && k >= 60
      {
        jumps += u64::from(kept.abs_diff(before) > period / 2);
      }
    }
    assert!(flaps > 10, "the boundary really is jittery: {flaps} flips");
    assert_eq!(jumps, 0, "committed delays stay on one present");
  }

  #[test]
  fn a_late_tick_that_captured_the_next_present_moves_the_slot_onto_it() {
    let period = SECOND / 120;
    let frame = SECOND / 60;
    let mut lock = PhaseLock::for_composition(60, Some(period)).expect("lock");
    assert_eq!(
      lock.late_capture(0, frame, Some(frame)),
      None,
      "no phase yet"
    );
    lock.observe(10 * period);
    let delay = 10_000;
    let sample = 20 * period + phase_margin(period);
    // The aimed present (with jitter), an idle screen, or no capture: nothing moves.
    for source in [
      Some(20 * period),
      Some(20 * period + 3_000),
      Some(18 * period),
      None,
    ] {
      assert_eq!(lock.late_capture(delay, sample, source), None, "{source:?}");
    }
    // The following present: one refresh later, committed for the next slots.
    assert_eq!(
      lock.late_capture(delay, sample, Some(21 * period + 2_000)),
      Some(period)
    );
    assert_eq!(lock.report().late_captures, 1);
    // The next slot whose first candidate is again 10_000 keeps to the later present.
    let nominal = 30 * period + phase_margin(period) - delay;
    assert_eq!(lock.delay(nominal), delay + period);
    // In the frame's upper half the slot is redated without moving the lock: the next slot
    // keeps its present a full frame on.
    let upper = frame - period + 1_000;
    assert_eq!(
      lock.late_capture(upper, sample, Some(21 * period)),
      Some(period)
    );
    assert_eq!(lock.delay(nominal), delay + period, "commit unchanged");
    // Near the frame end a forced wrap could reuse that present: the slot keeps its date.
    assert_eq!(
      lock.late_capture(frame - period / 8, sample, Some(21 * period)),
      None
    );
    assert_eq!(lock.report().late_captures, 2);
    // One refresh per frame: the next present belongs to the next slot.
    let mut single = PhaseLock::for_composition(60, Some(frame)).expect("60 Hz lock");
    single.observe(10 * frame);
    assert_eq!(
      single.late_capture(0, 20 * frame + phase_margin(frame), Some(21 * frame)),
      None
    );
  }

  #[test]
  fn a_capture_of_the_previous_present_is_dated_back_onto_it() {
    let period = SECOND / 120;
    let mut lock = PhaseLock::for_composition(60, Some(period)).expect("lock");
    assert_eq!(
      lock.early_capture(SECOND, Some(SECOND)),
      None,
      "no phase yet"
    );
    lock.observe(10 * period);
    let sample = 20 * period + phase_margin(period);
    assert_eq!(
      lock.early_capture(sample, Some(19 * period + 1_000)),
      Some(period)
    );
    for source in [
      Some(20 * period),
      Some(21 * period),
      Some(18 * period),
      None,
    ] {
      assert_eq!(lock.early_capture(sample, source), None, "{source:?}");
    }
    lock.record_early();
    assert_eq!(lock.report().early_captures, 1);
    let slot = Slot {
      index: 3,
      pts: 500_000,
      duration: 166_667,
      skipped: 0,
    };
    assert_eq!(slot.earlier(period).expect("earlier").pts, 500_000 - period);
    let first = Slot {
      index: 0,
      pts: 10,
      duration: 166_667,
      skipped: 0,
    };
    assert!(first.earlier(period).is_err());
  }

  #[test]
  fn jitter_is_rejected_and_a_real_phase_move_reseeds_after_eight_presents() {
    let period = SECOND / 60;
    let mut lock = PhaseLock::for_composition(60, Some(SECOND / 60)).expect("lock");
    lock.observe(10 * period + 20_000);
    let settled = lock.delay(10 * period);
    // One present 6 ms off is jitter: ignored.
    lock.observe(11 * period + 80_000);
    assert_eq!(lock.delay(12 * period), settled);
    assert_eq!(lock.report().outliers, 1);
    // A small offset moves the estimate by one eighth.
    lock.observe(12 * period + 28_000);
    assert_eq!(lock.delay(13 * period), settled + 1_000);
    // The display really moved by 7 ms: eight consecutive outliers re-seed on the new phase.
    for n in 0..8 {
      lock.observe((20 + n) * period + 90_000);
    }
    let report = lock.report();
    assert_eq!(
      (report.reseeds, report.outliers, report.observations),
      (1, 9, 11)
    );
    assert_eq!(
      lock.delay(30 * period),
      (90_000 + phase_margin(period)) % period
    );
  }
}
