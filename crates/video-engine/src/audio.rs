//! Bounded audio placement and fractional resampling, independent of WASAPI and codecs.
pub mod mix;
pub mod normalize;
use crate::{
  EngineError, ErrorKind, Result,
  time::{self, SECOND},
};

#[derive(Debug)]
pub struct PcmBlock {
  samples: Vec<f32>,
  recycle: Option<std::sync::mpsc::SyncSender<Vec<f32>>>,
}
impl PcmBlock {
  pub fn owned(samples: Vec<f32>) -> Self {
    Self {
      samples,
      recycle: None,
    }
  }
  pub(crate) fn pooled(samples: Vec<f32>, recycle: std::sync::mpsc::SyncSender<Vec<f32>>) -> Self {
    Self {
      samples,
      recycle: Some(recycle),
    }
  }
}
impl std::ops::Deref for PcmBlock {
  type Target = [f32];
  fn deref(&self) -> &[f32] {
    &self.samples
  }
}
impl std::ops::DerefMut for PcmBlock {
  fn deref_mut(&mut self) -> &mut [f32] {
    &mut self.samples
  }
}
impl Drop for PcmBlock {
  fn drop(&mut self) {
    if let Some(recycle) = &self.recycle {
      match recycle.try_send(std::mem::take(&mut self.samples)) {
        Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}, // Closing an owner releases its remaining packet buffers normally.
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
          eprintln!("PCM pool rejected a returned buffer")
        },
      }
    }
  }
}
#[derive(Debug)]
pub struct AudioPacket {
  pub data: PcmBlock,
  pub raw_device_position: u64,
  pub device_rate: u32,
  /// Rate of `raw_device_position`: the shared engine's mix rate.
  pub position_rate: u32,
  pub qpc_100ns: u64,
  /// QPC when the packet was read from the endpoint: an upper bound on its capture time.
  pub read_qpc_100ns: Option<u64>,
  pub client_rate: u32,
  pub silent: bool,
  pub discontinuity: bool,
  pub wasapi_flags: u32,
  pub drift_ppm: f64,
}
/// Drift is measured between the mean position-to-QPC offsets of consecutive blocks of this
/// length. VirtualBox HDA packet dates jitter by ±1 ms (p99) with isolated 3–11 ms outliers:
/// single dates one second apart read that as 4546 ppm, while a block mean over
/// hundreds of packets is stable to microseconds.
const DRIFT_WINDOW: u64 = 5 * SECOND;
/// Offsets are summarised as medians of short runs, which jitter and isolated outliers cannot
/// move. A shift of the run median beyond `TIMELINE_STEP` is a timeline step, not drift: drift of
/// 2000 ppm moves a 16-packet (~160 ms) run by about 0.3 ms. The VM's emulated HDA timeline stepped
/// ~10 ms at a time, which a 5 s window read as 2007 ppm.
const RUN: usize = 16;
const TIMELINE_STEP: i128 = (SECOND as i128) * 3 / 1000;
#[derive(Clone, Copy, Debug, Default)]
struct OffsetBlock {
  start: i128,
  coordinates: i128,
  offsets: i128,
  count: i128,
}
#[derive(Clone, Debug, Default)]
pub struct EndpointClock {
  offset: Option<i128>,
  /// Last observation (position, bracket midpoint), for reversal checks.
  previous: Option<(u64, u64)>,
  block: Option<OffsetBlock>,
  /// The current run of raw offsets and the sum of its endpoint coordinates.
  run: [i128; RUN],
  run_coordinates: i128,
  run_length: usize,
  /// Previous run median, and the sum of timeline steps removed from the drift series.
  last_median: Option<i128>,
  steps: i128,
  /// Timeline steps seen (offset jumps between consecutive packets beyond `TIMELINE_STEP`).
  pub timeline_steps: u64,
  /// Mean endpoint coordinate and offset of the last complete block.
  window: Option<(i128, i128)>,
  drift_measured: bool,
  pub drift_ppm: f64,
  rate: Option<u32>,
  last: Option<(u64, u64)>,
}
impl EndpointClock {
  pub fn observe(&mut self, position: u64, rate: u32, before: u64, after: u64) -> Result<()> {
    if after < before || !(8000..=384000).contains(&rate) {
      return Err(EngineError::protocol("Invalid raw audio clock observation"));
    }
    if self.rate.is_some_and(|previous| previous != rate)
      || self.last.is_some_and(|(p, q)| position < p || before < q)
    {
      return Err(EngineError::new(
        ErrorKind::DeviceLost,
        "audio clock",
        "Endpoint position, rate or QPC changed discontinuously",
      ));
    }
    self.rate = Some(rate);
    self.last = Some((position, before));
    if self.offset.is_none() && after - before > 50_000 {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "audio clock",
        "Initial clock correlation uncertainty exceeds 5 ms",
      ));
    }
    let coordinate = i128::from(time::scale(position, SECOND, u64::from(rate))?);
    let frame = i128::from(SECOND.div_ceil(u64::from(rate)));
    let lower = i128::from(before) - coordinate - frame;
    let upper = i128::from(after) - coordinate + frame;
    self.offset = Some(
      self
        .offset
        .map_or((lower + upper) / 2, |old| old.clamp(lower, upper)),
    );
    let now = before + (after - before) / 2;
    if self
      .previous
      .is_some_and(|(old_position, old_now)| position < old_position || now < old_now)
    {
      return Err(EngineError::new(
        ErrorKind::DeviceLost,
        "audio clock",
        "Raw endpoint clock reset or reversed",
      ));
    }
    self.previous = Some((position, now));
    self.run[self.run_length] = i128::from(now) - coordinate;
    self.run_coordinates += coordinate;
    self.run_length += 1;
    if self.run_length < RUN {
      return Ok(());
    }
    let mut sorted = self.run;
    sorted.sort_unstable();
    let median = (sorted[RUN / 2 - 1] + sorted[RUN / 2]) / 2;
    let center = self.run_coordinates / RUN as i128;
    self.run_length = 0;
    self.run_coordinates = 0;
    if let Some(last) = self.last_median
      && (median - last).abs() > TIMELINE_STEP
    {
      self.steps += median - last;
      self.timeline_steps = self.timeline_steps.saturating_add(1);
    }
    self.last_median = Some(median);
    let block = self.block.get_or_insert(OffsetBlock {
      start: center,
      ..Default::default()
    });
    block.coordinates += center;
    block.offsets += median - self.steps;
    block.count += 1;
    if center - block.start < i128::from(DRIFT_WINDOW) {
      return Ok(());
    }
    let mean = (block.coordinates / block.count, block.offsets / block.count);
    self.block = None;
    let Some((start, start_offset)) = self.window.replace(mean) else {
      return Ok(());
    };
    let span = mean.0 - start;
    if span <= 0 {
      return Err(EngineError::protocol("Audio drift window did not advance"));
    }
    // A fast endpoint advances its positions ahead of QPC, shrinking the offset.
    let ppm = -((mean.1 - start_offset) as f64) / span as f64 * 1_000_000.0;
    if ppm.abs() > 2000.0 {
      return Err(EngineError::new(
        ErrorKind::DeviceLost,
        "audio clock",
        format!(
          "Raw endpoint drift exceeds 2000 ppm: {ppm:.0} ppm over {} us of positions at {rate} Hz",
          span / 10
        ),
      ));
    }
    self.drift_ppm = if self.drift_measured {
      self.drift_ppm * 0.9 + ppm * 0.1
    } else {
      ppm
    };
    self.drift_measured = true;
    Ok(())
  }
  pub fn date(&self, position: u64, rate: u32) -> Result<u64> {
    if self.rate != Some(rate) {
      return Err(EngineError::protocol(
        "Packet endpoint rate differs from its clock",
      ));
    }
    let offset = self
      .offset
      .ok_or_else(|| EngineError::protocol("No endpoint correlation"))?;
    u64::try_from(i128::from(time::scale(position, SECOND, u64::from(rate))?) + offset)
      .map_err(|_| EngineError::protocol("Audio position outside QPC epoch"))
  }
}

/// Fixed polyphase windowed-sinc table. Coefficients and storage are allocated at prepare.
pub struct SincResampler {
  table: Vec<[f32; 64]>,
  input: std::collections::VecDeque<[f32; 2]>,
  position: f64,
  step: f64,
}
impl Default for SincResampler {
  fn default() -> Self {
    Self::new()
  }
}
impl SincResampler {
  pub const LOOKAHEAD_FRAMES: usize = 33;
  pub fn new() -> Self {
    let mut table = Vec::with_capacity(1024);
    for phase in 0..1024 {
      let mut taps = [0.0f32; 64];
      let mut sum = 0.0;
      for (i, tap) in taps.iter_mut().enumerate() {
        let x = i as f64 - 31.0 - phase as f64 / 1024.0;
        let sinc = if x.abs() < 1e-12 {
          1.0
        } else {
          (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
        };
        let window = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / 63.0).cos()
          + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / 63.0).cos();
        *tap = (sinc * window) as f32;
        sum += *tap;
      }
      for tap in &mut taps {
        *tap /= sum;
      }
      table.push(taps);
    }
    let mut input = std::collections::VecDeque::with_capacity(48_064);
    input.extend(std::iter::repeat_n([0.0; 2], 31));
    Self {
      table,
      input,
      position: 31.0,
      step: 1.0,
    }
  }
  pub fn set_drift(&mut self, ppm: f64) -> Result<()> {
    if !ppm.is_finite() || ppm.abs() > 2000.0 {
      return Err(EngineError::new(
        ErrorKind::Native,
        "resampler",
        "Clock drift outside 2000 ppm budget",
      ));
    }
    let wanted = 1.0 + ppm / 1_000_000.0;
    self.step += (wanted - self.step).clamp(-0.000_05, 0.000_05);
    Ok(())
  }
  pub fn reset(&mut self) {
    self.input.clear();
    self.input.extend(std::iter::repeat_n([0.0; 2], 31));
    self.position = 31.0;
  }
  /// Supply only the FIR's missing future context; callers clip the output at the real source end.
  pub fn finish(&mut self, output: &mut Vec<f32>) -> Result<()> {
    self.feed(&[0.0; Self::LOOKAHEAD_FRAMES * 2], output)?;
    self.reset();
    Ok(())
  }
  pub fn feed(&mut self, samples: &[f32], output: &mut Vec<f32>) -> Result<()> {
    if !samples.len().is_multiple_of(2) || self.input.len() + samples.len() / 2 > 48_064 {
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "resampler",
        "PCM extent exceeds one-second capacity",
      ));
    }
    self
      .input
      .extend(samples.as_chunks::<2>().0.iter().map(|p| [p[0], p[1]]));
    while (self.position.floor() as usize) + 33 < self.input.len() {
      if output.len() + 2 > output.capacity() {
        return Err(EngineError::new(
          ErrorKind::Saturation,
          "resampler",
          "Preallocated output capacity exhausted",
        ));
      }
      let center = self.position.floor() as usize;
      let phase = ((self.position.fract() * 1024.0) as usize).min(1023);
      let mut sample = [0.0f32; 2];
      for (i, &weight) in self.table[phase].iter().enumerate() {
        let source = self.input[center - 31 + i];
        sample[0] += source[0] * weight;
        sample[1] += source[1] * weight;
      }
      output.extend(sample);
      self.position += self.step;
      let remove = (self.position.floor() as usize).saturating_sub(31);
      for _ in 0..remove {
        self.input.pop_front();
      }
      self.position -= remove as f64;
    }
    Ok(())
  }
}

/// One-second placement ring. Arrival order cannot move previously dated samples.
pub struct AudioBuffer {
  samples: Vec<[f32; 2]>,
  positions: Vec<Option<u64>>,
  next: u64,
  pub silence: u64,
  pub late: u64,
}
impl Default for AudioBuffer {
  fn default() -> Self {
    Self {
      samples: vec![[0.0; 2]; 48_000],
      positions: vec![None; 48_000],
      next: 0,
      silence: 0,
      late: 0,
    }
  }
}
impl AudioBuffer {
  pub fn place(&mut self, start: u64, pcm: &[f32]) -> Result<()> {
    if !pcm.len().is_multiple_of(2) {
      return Err(EngineError::protocol("PCM frame is not stereo"));
    }
    let frames = pcm.len() / 2;
    if start
      .checked_add(frames as u64)
      .is_none_or(|end| end > self.next.saturating_add(48_000))
    {
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "audio placement",
        "Dated PCM exceeds one-second queue",
      ));
    }
    for (index, pair) in pcm.as_chunks::<2>().0.iter().enumerate() {
      let frame = start + index as u64;
      if frame < self.next {
        self.late += 1;
        continue;
      }
      let slot = frame as usize % 48_000;
      if self.positions[slot] == Some(frame) {
        return Err(EngineError::protocol("Overlapping audio packets"));
      }
      self.samples[slot] = [pair[0], pair[1]];
      self.positions[slot] = Some(frame);
    }
    Ok(())
  }
  pub fn read(&mut self, until: u64, output: &mut [i16]) -> Result<usize> {
    let capacity = output.len();
    self.take(until, capacity, |index, pair| {
      output[index * 2] = mix::to_i16(pair[0]);
      output[index * 2 + 1] = mix::to_i16(pair[1]);
    })
  }
  /// Like `read`, as float samples for mixing; frames no packet placed are silence.
  pub fn read_float(&mut self, until: u64, output: &mut [f32]) -> Result<usize> {
    let capacity = output.len();
    self.take(until, capacity, |index, pair| {
      output[index * 2] = pair[0];
      output[index * 2 + 1] = pair[1];
    })
  }
  fn take(
    &mut self,
    until: u64,
    capacity: usize,
    mut put: impl FnMut(usize, [f32; 2]),
  ) -> Result<usize> {
    if until < self.next || !capacity.is_multiple_of(2) {
      return Err(EngineError::protocol(
        "Audio cutoff reversed or invalid output",
      ));
    }
    let count = (until - self.next).min((capacity / 2) as u64) as usize;
    for i in 0..count {
      let frame = self.next + i as u64;
      let slot = frame as usize % 48_000;
      let pair = if self.positions[slot] == Some(frame) {
        self.samples[slot]
      } else {
        self.silence += 1;
        [0.0; 2]
      };
      put(i, pair);
      self.positions[slot] = None;
    }
    self.next += count as u64;
    Ok(count)
  }
  pub fn next(&self) -> u64 {
    self.next
  }
  pub fn discard_future(&mut self) {
    self.positions.fill(None);
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn packet_buffer_is_returned_only_when_its_lease_is_released() {
    let (recycle, available) = std::sync::mpsc::sync_channel(1);
    let samples = Vec::with_capacity(960);
    let address = samples.as_ptr();
    let block = PcmBlock::pooled(samples, recycle);
    assert!(available.try_recv().is_err());
    drop(block);
    let samples = available.try_recv().expect("returned PCM");
    assert_eq!(samples.as_ptr(), address);
    assert_eq!(samples.capacity(), 960);
    let owned = PcmBlock::owned(vec![0.25; 4]);
    assert_eq!(owned.len(), 4);
    assert_eq!(owned[0], 0.25);
  }
  #[test]
  fn packet_arrival_order_does_not_change_media_placement() {
    let mut b = AudioBuffer::default();
    b.place(4, &[0.5; 4]).expect("valid test fixture");
    b.place(0, &[0.25; 4]).expect("valid test fixture");
    let mut out = [0; 12];
    assert_eq!(b.read(6, &mut out).expect("valid test fixture"), 6);
    assert_eq!(out[4], 0);
    assert!(out[8] > out[0]);
    assert_eq!(b.silence, 2);
    b.place(0, &[1.0; 4]).expect("valid test fixture");
    assert_eq!(b.late, 2);
    assert!(b.place(48010, &[0.0; 2]).is_err());
  }
  #[test]
  fn clock_uses_raw_origin_and_bracket_not_delivery_time() {
    let mut c = EndpointClock::default();
    c.observe(441_000, 44100, 100 * SECOND, 100 * SECOND + 2)
      .expect("valid test fixture");
    assert!(
      c.date(441_000, 44100)
        .expect("valid test fixture")
        .abs_diff(100 * SECOND)
        <= 2
    );
    assert!(
      c.date(485_100, 44100)
        .expect("valid test fixture")
        .abs_diff(101 * SECOND)
        <= 2
    );
    assert!(c.observe(1, 44100, 101 * SECOND, 101 * SECOND + 2).is_err());
  }
  #[test]
  fn clock_guards_reject_uncertain_reversed_and_out_of_budget_endpoints() {
    let t = 100 * SECOND;
    let mut uncertain = EndpointClock::default();
    let error = uncertain
      .observe(0, 48000, t, t + 50_001)
      .expect_err("initial bracket wider than 5 ms");
    assert_eq!(error.kind, ErrorKind::Timeout);
    // The same width is acceptable once a correlation exists.
    let mut c = EndpointClock::default();
    c.observe(0, 48000, t, t + 40_000).expect("first bracket");
    let error = c
      .observe(10, 48000, t + 1, t + 1)
      .expect_err("bracket midpoint moved backwards");
    assert!(error.reason.contains("reset or reversed"), "{error}");
    for (ppm, accepted) in [
      (1900i64, true),
      (-1900, true),
      (2100, false),
      (-2100, false),
    ] {
      let mut c = EndpointClock::default();
      let mut result = Ok(());
      // 10 ms packets for eleven seconds: two complete blocks give one drift measurement.
      for packet in 0..=1100u64 {
        let position = (packet as i64 * 480 * (1_000_000 + ppm) / 1_000_000) as u64;
        let now = t + packet * SECOND / 100;
        result = c.observe(position, 48000, now, now);
        if result.is_err() {
          break;
        }
      }
      assert_eq!(result.is_ok(), accepted, "{ppm} ppm");
      if accepted {
        assert!((c.drift_ppm - ppm as f64).abs() < 5.0, "{}", c.drift_ppm);
      } else {
        assert!(result.expect_err("drift").reason.contains("2000 ppm"));
      }
    }
    let mut c = EndpointClock::default();
    c.observe(0, 48000, t, t + 2).expect("origin");
    assert_eq!(
      c.observe(48000, 44100, t + SECOND, t + SECOND + 2)
        .expect_err("rate changed")
        .kind,
      ErrorKind::DeviceLost
    );
    assert!(
      c.date(0, 44100).is_err(),
      "packets must use the clock's own rate"
    );
    assert!(EndpointClock::default().observe(0, 7999, t, t).is_err());
    assert!(
      EndpointClock::default()
        .observe(0, 48000, t, t - 1)
        .is_err()
    );
  }
  #[test]
  fn packet_date_jitter_and_outliers_do_not_read_as_drift() {
    // Regression: VirtualBox HDA packet dates jitter by a few ms around their positions; two single
    // dates one second apart read as 4546 ppm and failed the recording as a lost device.
    let t = 100 * SECOND;
    let mut c = EndpointClock::default();
    for packet in 0..3000u64 {
      let jitter: i64 = match packet % 97 {
        0 => 90_000,
        1 => -5_000,
        n if n % 2 == 0 => 25_000,
        _ => -25_000,
      };
      let now = (t + packet * SECOND / 100).saturating_add_signed(jitter);
      c.observe(packet * 441, 44100, now, now)
        .expect("jittered dates are not drift");
    }
    assert!(c.drift_ppm.abs() < 100.0, "{}", c.drift_ppm);
  }
  #[test]
  fn timeline_steps_are_not_read_as_drift_but_real_drift_still_is() {
    // Regression: a ~10 ms step of the VM's emulated timeline inside a 5 s window read as
    // 2007 ppm and failed the recording as a lost device.
    let t = 100 * SECOND;
    for (ppm, steps) in [(0i64, vec![700u64, 1600]), (1500, vec![900])] {
      let mut c = EndpointClock::default();
      for packet in 0..2200u64 {
        let shift: u64 = steps.iter().filter(|step| packet >= **step).count() as u64 * 100_000;
        let position = (packet as i64 * 441 * (1_000_000 + ppm) / 1_000_000) as u64;
        let now = t + packet * SECOND / 100 + shift;
        c.observe(position, 44100, now, now)
          .expect("steps are not drift");
      }
      assert_eq!(c.timeline_steps, steps.len() as u64);
      assert!(
        (c.drift_ppm - ppm as f64).abs() < 20.0,
        "{ppm}: {}",
        c.drift_ppm
      );
    }
    // A clock that is genuinely 2500 ppm fast still fails, step or not.
    let mut c = EndpointClock::default();
    let mut result = Ok(());
    for packet in 0..1200u64 {
      let position = (packet as i64 * 441 * 1_002_500 / 1_000_000) as u64;
      let now = t + packet * SECOND / 100 + if packet >= 300 { 100_000 } else { 0 };
      result = c.observe(position, 44100, now, now);
      if result.is_err() {
        break;
      }
    }
    assert!(result.expect_err("fast clock").reason.contains("2000 ppm"));
  }
  #[test]
  fn resampler_rejects_malformed_or_oversized_pcm_instead_of_growing() {
    let mut r = SincResampler::new();
    let mut out = Vec::with_capacity(4096);
    assert_eq!(
      r.feed(&[0.0; 3], &mut out).expect_err("half frame").kind,
      ErrorKind::Saturation
    );
    assert!(
      r.feed(&vec![0.0; 48_064 * 2], &mut out).is_err(),
      "more than one second of retained input"
    );
    assert!(out.is_empty(), "rejected input produces no output");
    let mut full = Vec::new();
    let error = r
      .feed(&vec![0.5; 256], &mut full)
      .expect_err("output storage is preallocated by the owner");
    assert!(error.reason.contains("capacity exhausted"), "{error}");
    assert_eq!(
      full.capacity(),
      0,
      "the resampler never grows caller storage"
    );
  }
  #[test]
  fn sinc_preserves_dc_and_bounds_processing_storage() {
    let mut r = SincResampler::new();
    let mut out = Vec::with_capacity(3000);
    r.feed(&vec![0.25; 2048], &mut out)
      .expect("valid test fixture");
    assert!(out.len() > 1800);
    assert!(out[200..].iter().all(|v| (*v - 0.25).abs() < 0.001));
    assert!(r.set_drift(f64::NAN).is_err());
    assert!(r.set_drift(3000.0).is_err());
    r.set_drift(1000.0).expect("valid test fixture");
  }
  #[test]
  fn ninety_minute_raw_clocks_keep_independent_origins_rates_and_both_drift_signs() {
    for rate in [44100, 48000, 96000] {
      for ppm in [-100i64, 100] {
        let mut clock = EndpointClock::default();
        let origin = 91_234_567u64;
        let epoch = 900 * SECOND;
        for seconds in 0..=5400u64 {
          let advanced = (u128::from(seconds) * u128::from(rate) * (1_000_000i64 + ppm) as u128
            / 1_000_000) as u64;
          let position = origin + advanced;
          let now = epoch + seconds * SECOND;
          clock
            .observe(position, rate, now, now + 2)
            .expect("raw endpoint fixture");
          assert!(
            clock
              .date(position, rate)
              .expect("dated packet")
              .abs_diff(now)
              <= SECOND.div_ceil(u64::from(rate)) + 2,
            "rate={rate}, ppm={ppm}, seconds={seconds}"
          );
        }
        assert!((clock.drift_ppm - ppm as f64).abs() < 3.0);
        assert!(clock.date(origin, rate + 1).is_err());
      }
    }
    let mut clock = EndpointClock::default();
    clock
      .observe(1000, 48000, SECOND, SECOND + 1)
      .expect("first");
    clock
      .observe(1100, 48000, SECOND + 100, SECOND + 101)
      .expect("second");
    assert!(
      clock
        .observe(1099, 48000, SECOND + 200, SECOND + 201)
        .is_err()
    );
  }
  #[test]
  fn audio_pause_discards_only_future_data_and_never_replays_it() {
    let mut b = AudioBuffer::default();
    b.place(0, &[0.8; 20]).expect("PCM");
    let mut pcm = [0; 8];
    b.read(4, &mut pcm).expect("before pause");
    b.discard_future();
    b.place(4, &[0.2; 8]).expect("new audio after resume");
    b.read(8, &mut pcm).expect("after resume");
    assert!(pcm.iter().all(|v| *v > 6000 && *v < 7000));
    assert!(b.read(7, &mut pcm).is_err());
    assert!(b.place(8, &[0.1; 3]).is_err());
  }
}
