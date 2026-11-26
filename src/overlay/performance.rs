//! Opt-in CPU submission and pacing measurements; never a rendering scheduler.

use std::collections::VecDeque;
use std::time::Instant;

const LIMIT: usize = 256;

pub(crate) struct Diagnostics {
  label: &'static str,
  samples: VecDeque<(u64, Option<u64>, Option<u64>)>,
  pending_input: Option<Instant>,
  previous: Option<Instant>,
  count: u64,
}

impl Diagnostics {
  pub fn from_env(label: &'static str) -> Option<Self> {
    (std::env::var("FLINCH_OVERLAY_DIAGNOSTICS").as_deref() == Ok("1")).then(|| {
      tracing::info!(
        overlay = label,
        "Overlay diagnostics enabled (bounded to 256 samples)"
      );
      Self {
        label,
        samples: VecDeque::with_capacity(LIMIT),
        pending_input: None,
        previous: None,
        count: 0,
      }
    })
  }

  pub fn input(&mut self) {
    // Measure from the latest dispatched pointer event; idle hover can skip paints.
    self.pending_input = Some(Instant::now());
  }

  pub fn reset_interval(&mut self) {
    self.previous = None;
  }

  pub fn record(&mut self, started: Instant) {
    let interval = self
      .previous
      .map(|previous| micros(started.saturating_duration_since(previous)));
    self.previous = Some(started);
    self.count = self.count.saturating_add(1);
    if self.samples.len() == LIMIT {
      self.samples.pop_front();
    }
    let input_delay = self
      .pending_input
      .take()
      .map(|input| micros(started.saturating_duration_since(input)));
    self
      .samples
      .push_back((micros(started.elapsed()), interval, input_delay));
  }
}

fn micros(duration: std::time::Duration) -> u64 {
  duration.as_micros().min(u64::MAX as u128) as u64
}

fn percentile(mut values: Vec<u64>, percent: usize) -> Option<u64> {
  values.sort_unstable();
  values
    .get((values.len() * percent).div_ceil(100).saturating_sub(1))
    .copied()
}

impl Drop for Diagnostics {
  fn drop(&mut self) {
    if self.count == 0 {
      return;
    }
    let costs: Vec<_> = self.samples.iter().map(|sample| sample.0).collect();
    let input_delays: Vec<_> = self.samples.iter().filter_map(|sample| sample.2).collect();
    let intervals: Vec<_> = self.samples.iter().filter_map(|sample| sample.1).collect();
    tracing::info!(
      overlay = self.label,
      count = self.count,
      retained = self.samples.len(),
      submission_p50_us = ?percentile(costs.clone(), 50),
      submission_p95_us = ?percentile(costs, 95),
      latest_pointer_to_paint_p95_us = ?percentile(input_delays, 95),
      interval_p50_us = ?percentile(intervals.clone(), 50),
      interval_p95_us = ?percentile(intervals, 95),
      "Overlay diagnostics (CPU submission; intervals include event cadence, not display FPS)"
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[ignore = "explicit Windows timer pacing probe; runs for two seconds"]
  fn windows_timer_pacing_probe() {
    use std::time::Duration;
    use windows::Win32::UI::WindowsAndMessaging::{
      KillTimer, MSG, PM_REMOVE, PeekMessageW, SetTimer, WM_TIMER,
    };
    struct Timer(usize);
    impl Drop for Timer {
      fn drop(&mut self) {
        // SAFETY: this test owns the thread-local timer until teardown.
        unsafe { KillTimer(None, self.0) }.expect("remove probe timer");
      }
    }
    for requested_ms in [16, 7] {
      // SAFETY: thread-local timer with no callback; its messages are consumed below.
      let timer = Timer(unsafe { SetTimer(None, 0, requested_ms, None) });
      assert_ne!(timer.0, 0, "create probe timer");
      let deadline = Instant::now() + Duration::from_secs(1);
      let mut previous = None;
      let mut intervals = Vec::new();
      while Instant::now() < deadline {
        let mut message = MSG::default();
        // SAFETY: initialized output and valid message range on the owning test thread.
        if unsafe { PeekMessageW(&mut message, None, WM_TIMER, WM_TIMER, PM_REMOVE) }.as_bool()
          && message.wParam.0 == timer.0
        {
          let now = Instant::now();
          if let Some(previous) = previous {
            intervals.push(micros(now.duration_since(previous)));
          }
          previous = Some(now);
        }
        // Bounded polling adds up to roughly 1 ms observation delay. No blocking GetMessage.
        std::thread::sleep(Duration::from_millis(1));
      }
      println!(
        "requested_ms={requested_ms} samples={} interval_p50_us={:?} interval_p95_us={:?}",
        intervals.len(),
        percentile(intervals.clone(), 50),
        percentile(intervals, 95)
      );
    }
  }

  #[test]
  fn samples_are_bounded_and_reset_excludes_idle_gap() {
    let mut diagnostics = Diagnostics {
      label: "test",
      samples: VecDeque::new(),
      pending_input: None,
      previous: None,
      count: 0,
    };
    for _ in 0..1000 {
      diagnostics.record(Instant::now());
    }
    assert_eq!(diagnostics.samples.len(), LIMIT);
    assert_eq!(diagnostics.count, 1000);
    diagnostics.reset_interval();
    diagnostics.record(Instant::now());
    assert_eq!(diagnostics.samples.back().and_then(|sample| sample.1), None);
    assert_eq!(percentile(vec![], 95), None);
    assert_eq!(percentile(vec![4, 1, 3, 2], 50), Some(2));
  }
}
