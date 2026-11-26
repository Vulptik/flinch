use crate::{EngineError, Result};
use serde::{Deserialize, Serialize};
pub mod resources;
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct MediaMilestones {
  pub start_qpc_100ns: Option<u64>,
  pub capture_qpc_100ns: Option<u64>,
  pub submit_qpc_100ns: Option<u64>,
  pub encoded_qpc_100ns: Option<u64>,
  pub sink_accept_qpc_100ns: Option<u64>,
}
impl MediaMilestones {
  pub fn validate(&self) -> Result<()> {
    let mut previous = None;
    for stamp in [
      self.start_qpc_100ns,
      self.capture_qpc_100ns,
      self.submit_qpc_100ns,
      self.encoded_qpc_100ns,
      self.sink_accept_qpc_100ns,
    ]
    .into_iter()
    .flatten()
    {
      if previous.is_some_and(|previous| stamp < previous) {
        return Err(EngineError::protocol(
          "First-media milestone clock reversed",
        ));
      }
      previous = Some(stamp);
    }
    Ok(())
  }
}
/// Worker-side Prepare cost, split by the stage that owns it. Skipped stages stay `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareStages {
  pub validate_us: u64,
  pub display_us: u64,
  pub artifact_us: u64,
  pub gpu_us: u64,
  pub capture_us: u64,
  pub cursor_us: Option<u64>,
  pub encoder_us: u64,
  pub audio_us: Option<u64>,
  pub reports_us: u64,
  pub total_us: u64,
}
impl PrepareStages {
  pub fn stage_sum_us(&self) -> Option<u64> {
    [
      Some(self.validate_us),
      Some(self.display_us),
      Some(self.artifact_us),
      Some(self.gpu_us),
      Some(self.capture_us),
      self.cursor_us,
      Some(self.encoder_us),
      self.audio_us,
      Some(self.reports_us),
    ]
    .into_iter()
    .flatten()
    .try_fold(0u64, u64::checked_add)
  }
}
use std::sync::{
  Mutex, TryLockError,
  atomic::{AtomicU64, Ordering},
};

/// Cumulative owner reports. Contention coalesces a report; no capture/native owner waits on a lock.
pub struct ReportSlot<T: Copy> {
  latest: Mutex<Option<T>>,
  publish_skipped: AtomicU64,
  read_skipped: AtomicU64,
}
impl<T: Copy> Default for ReportSlot<T> {
  fn default() -> Self {
    Self {
      latest: Mutex::new(None),
      publish_skipped: AtomicU64::new(0),
      read_skipped: AtomicU64::new(0),
    }
  }
}
impl<T: Copy> ReportSlot<T> {
  pub fn publish(&self, report: T) -> Result<bool> {
    match self.latest.try_lock() {
      Ok(mut latest) => {
        *latest = Some(report);
        Ok(true)
      },
      Err(TryLockError::WouldBlock) => {
        self.publish_skipped.fetch_add(1, Ordering::Relaxed);
        Ok(false)
      },
      Err(TryLockError::Poisoned(_)) => Err(EngineError::protocol("Owner report slot poisoned")),
    }
  }
  pub fn read(&self) -> Result<Option<T>> {
    match self.latest.try_lock() {
      Ok(latest) => Ok(*latest),
      Err(TryLockError::WouldBlock) => {
        self.read_skipped.fetch_add(1, Ordering::Relaxed);
        Ok(None)
      },
      Err(TryLockError::Poisoned(_)) => Err(EngineError::protocol("Owner report slot poisoned")),
    }
  }
  pub fn coalesced(&self) -> (u64, u64) {
    (
      self.publish_skipped.load(Ordering::Relaxed),
      self.read_skipped.load(Ordering::Relaxed),
    )
  }
}

/// Fixed logarithmic histogram: instrumentation memory never grows with duration.
#[derive(Clone, Debug)]
pub struct Histogram {
  buckets: [u64; 64],
  count: u64,
  sum: u128,
  maximum: u64,
}
impl Default for Histogram {
  fn default() -> Self {
    Self {
      buckets: [0; 64],
      count: 0,
      sum: 0,
      maximum: 0,
    }
  }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default)]
pub struct Distribution {
  pub count: u64,
  pub mean_us: f64,
  pub p50_us: u64,
  pub p95_us: u64,
  pub p99_us: u64,
  pub maximum_us: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct EncoderReport {
  #[serde(default)]
  pub first_input_qpc_100ns: Option<u64>,
  #[serde(default)]
  pub first_output_qpc_100ns: Option<u64>,
  pub input_wait: Distribution,
  pub native_submit: Distribution,
  pub native_query: Distribution,
  pub encoded_output_observed: Distribution,
  pub texture_lease_observed: Distribution,
  #[serde(default)]
  pub source_age_at_accept: Distribution,
  /// Frames dated after acceptance (within `SOURCE_LEAD_LIMIT`); their lead is not an age.
  #[serde(default)]
  pub source_lead_at_accept: Distribution,
  #[serde(default)]
  pub source_time_unavailable: u64,
  pub backpressure: u64,
  pub pending_outputs_peak: usize,
  pub texture_leases_outstanding: usize,
  pub final_report: bool,
  pub reports_coalesced: u64,
  pub report_reads_contended: u64,
}
#[derive(Default)]
pub struct EncoderMeasurements {
  pub first_input_qpc_100ns: Option<u64>,
  pub first_output_qpc_100ns: Option<u64>,
  pub input_wait: Histogram,
  pub native_submit: Histogram,
  pub native_query: Histogram,
  pub encoded_output_observed: Histogram,
  pub texture_lease_observed: Histogram,
  pub source_age_at_accept: Histogram,
  pub source_lead_at_accept: Histogram,
  pub source_time_unavailable: u64,
  pub backpressure: u64,
  pub pending_outputs_peak: usize,
  pub texture_leases_outstanding: usize,
}
impl EncoderMeasurements {
  pub fn accepted_source(
    &mut self,
    source_qpc_100ns: Option<u64>,
    accepted_qpc_100ns: u64,
  ) -> Result<()> {
    if let Some(source) = source_qpc_100ns {
      match accepted_qpc_100ns.checked_sub(source) {
        Some(age) => self.source_age_at_accept.record(age / 10),
        None => {
          let lead_us = (source - accepted_qpc_100ns) / 10;
          if u128::from(lead_us) > crate::limits::SOURCE_LEAD_LIMIT.as_micros() {
            return Err(EngineError::protocol(format!(
              "Capture source timestamp is later than encoder acceptance by {lead_us} us, beyond the {} ms presentation-target bound (source {source}, accepted {accepted_qpc_100ns}, 100 ns QPC)",
              crate::limits::SOURCE_LEAD_LIMIT.as_millis()
            )));
          }
          self.source_lead_at_accept.record(lead_us);
        },
      }
    } else {
      self.source_time_unavailable = self.source_time_unavailable.saturating_add(1);
    }
    Ok(())
  }
  pub fn report(&self, final_report: bool) -> EncoderReport {
    EncoderReport {
      first_input_qpc_100ns: self.first_input_qpc_100ns,
      first_output_qpc_100ns: self.first_output_qpc_100ns,
      input_wait: self.input_wait.summary(),
      native_submit: self.native_submit.summary(),
      native_query: self.native_query.summary(),
      encoded_output_observed: self.encoded_output_observed.summary(),
      texture_lease_observed: self.texture_lease_observed.summary(),
      source_age_at_accept: self.source_age_at_accept.summary(),
      source_lead_at_accept: self.source_lead_at_accept.summary(),
      source_time_unavailable: self.source_time_unavailable,
      backpressure: self.backpressure,
      pending_outputs_peak: self.pending_outputs_peak,
      texture_leases_outstanding: self.texture_leases_outstanding,
      final_report,
      ..Default::default()
    }
  }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct WriterReport {
  #[serde(default)]
  pub first_packet_qpc_100ns: Option<u64>,
  pub queue_wait: Distribution,
  pub native_write: Distribution,
  pub native_finalize: Distribution,
  pub packets_written: u64,
  pub bytes_written: u64,
  pub finalized: bool,
  pub reports_coalesced: u64,
  pub report_reads_contended: u64,
}
#[derive(Default)]
pub struct WriterMeasurements {
  pub first_packet_qpc_100ns: Option<u64>,
  pub queue_wait: Histogram,
  pub native_write: Histogram,
  pub native_finalize: Histogram,
  pub packets_written: u64,
  pub bytes_written: u64,
}
impl WriterMeasurements {
  pub fn report(&self, finalized: bool) -> WriterReport {
    WriterReport {
      first_packet_qpc_100ns: self.first_packet_qpc_100ns,
      queue_wait: self.queue_wait.summary(),
      native_write: self.native_write.summary(),
      native_finalize: self.native_finalize.summary(),
      packets_written: self.packets_written,
      bytes_written: self.bytes_written,
      finalized,
      ..Default::default()
    }
  }
}
impl Histogram {
  pub fn record(&mut self, microseconds: u64) {
    let index = (64 - microseconds.leading_zeros()).min(63) as usize;
    self.buckets[index] = self.buckets[index].saturating_add(1);
    self.count = self.count.saturating_add(1);
    self.sum = self.sum.saturating_add(u128::from(microseconds));
    self.maximum = self.maximum.max(microseconds);
  }
  pub fn summary(&self) -> Distribution {
    let quantile = |percent: u128| {
      let needed = (u128::from(self.count) * percent).div_ceil(100);
      let mut seen = 0u128;
      for (i, &count) in self.buckets.iter().enumerate() {
        seen += u128::from(count);
        if seen >= needed {
          return ((1u64 << i).saturating_sub(1)).min(self.maximum);
        }
      }
      self.maximum
    };
    if self.count == 0 {
      return Distribution::default();
    }
    Distribution {
      count: self.count,
      mean_us: self.sum as f64 / self.count as f64,
      p50_us: quantile(50),
      p95_us: quantile(95),
      p99_us: quantile(99),
      maximum_us: self.maximum,
    }
  }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Acceptance {
  pub sync_measured: bool,
  pub maximum_sync_ms: Option<f64>,
  pub drift_ms: Option<f64>,
  pub game_impact_measured: bool,
  pub median_fps_loss_percent: Option<f64>,
  pub p99_regression_percent: Option<f64>,
}
impl Acceptance {
  pub fn failures(&self) -> Vec<&'static str> {
    let mut failures = Vec::new();
    if !self.sync_measured {
      failures.push("synchronization_not_measured");
    }
    if self
      .maximum_sync_ms
      .is_none_or(|x| !x.is_finite() || x.abs() > 25.0)
    {
      failures.push("synchronization_limit");
    }
    if self
      .drift_ms
      .is_none_or(|x| !x.is_finite() || x.abs() > 5.0)
    {
      failures.push("drift_limit");
    }
    if !self.game_impact_measured {
      failures.push("game_impact_not_measured");
    }
    if self
      .median_fps_loss_percent
      .is_none_or(|x| !x.is_finite() || x > 3.0)
    {
      failures.push("fps_impact_limit");
    }
    if self
      .p99_regression_percent
      .is_none_or(|x| !x.is_finite() || x > 5.0)
    {
      failures.push("p99_impact_limit");
    }
    failures
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn presentation_targets_ahead_of_acceptance_are_recorded_as_lead_within_their_bound() {
    let mut measurements = EncoderMeasurements::default();
    // Leads observed from WGC, then the bound itself.
    for (source, accepted) in [
      (1_426_049_487_602, 1_426_049_449_445),
      (1_426_052_056_527, 1_426_052_033_547),
      (10_500_000, 10_000_000),
    ] {
      measurements
        .accepted_source(Some(source), accepted)
        .expect("presentation target within one low refresh");
    }
    let beyond = measurements
      .accepted_source(Some(10_500_010), 10_000_000)
      .expect_err("1 us past the bound");
    assert!(
      beyond.reason.contains("by 50001 us, beyond the 50 ms"),
      "{}",
      beyond.reason
    );
    let report = measurements.report(true);
    assert_eq!(report.source_lead_at_accept.count, 3);
    assert_eq!(report.source_lead_at_accept.maximum_us, 50_000);
    // A lead is never folded into age, and a refused frame records neither.
    assert_eq!(report.source_age_at_accept.count, 0);
    assert_eq!(report.source_time_unavailable, 0);
  }
  #[test]
  fn capture_age_uses_source_qpc_and_keeps_static_content_and_unknown_provenance_distinct() {
    let mut measurements = EncoderMeasurements::default();
    measurements
      .accepted_source(Some(10_000_000), 10_500_000)
      .expect("50 ms source delay");
    measurements
      .accepted_source(Some(10_000_000), 11_000_000)
      .expect("static content keeps its original source time");
    measurements
      .accepted_source(None, 12_000_000)
      .expect("unknown stays unmeasured");
    let future = measurements
      .accepted_source(Some(14_000_000), 13_000_000)
      .expect_err("source dated after acceptance");
    // The magnitude separates a sub-refresh presentation target from a clock-domain error.
    assert!(future.reason.contains("by 100000 us"), "{}", future.reason);
    let report = measurements.report(true);
    assert_eq!(report.source_age_at_accept.count, 2);
    assert_eq!(report.source_age_at_accept.maximum_us, 100_000);
    assert_eq!(report.source_age_at_accept.mean_us, 75_000.0);
    assert_eq!(report.source_time_unavailable, 1);
  }
  #[test]
  fn first_media_milestones_keep_unknown_values_and_reject_reversed_owner_clocks() {
    let mut milestones = MediaMilestones::default();
    milestones
      .validate()
      .expect("unknown milestones are unmeasured");
    milestones.start_qpc_100ns = Some(100);
    milestones.capture_qpc_100ns = Some(100);
    milestones.submit_qpc_100ns = Some(120);
    milestones.encoded_qpc_100ns = Some(150);
    milestones.sink_accept_qpc_100ns = Some(180);
    milestones.validate().expect("common QPC ordering");
    milestones.encoded_qpc_100ns = Some(110);
    assert!(milestones.validate().is_err());
  }
  #[test]
  fn cumulative_reports_never_wait_and_expose_coalescing_without_losing_histogram_samples() {
    let slot = ReportSlot::default();
    let mut owner = WriterMeasurements::default();
    owner.native_write.record(10);
    slot.publish(owner.report(false)).expect("report");
    let guard = slot.latest.lock().expect("simulate a concurrent snapshot");
    owner.native_write.record(200);
    assert!(
      !slot
        .publish(owner.report(false))
        .expect("coalesced, never waited")
    );
    assert!(slot.read().expect("contended reader").is_none());
    drop(guard);
    assert_eq!(slot.coalesced(), (1, 1));
    slot
      .publish(owner.report(true))
      .expect("final cumulative report");
    let report = slot.read().expect("report").expect("available");
    assert_eq!(report.native_write.count, 2);
    assert_eq!(report.native_write.maximum_us, 200);
    assert!(report.finalized);
  }
  #[test]
  fn histogram_is_bounded_and_reports_tail() {
    let mut h = Histogram::default();
    assert_eq!(h.summary().count, 0);
    for i in 0..1000 {
      h.record(i);
    }
    let d = h.summary();
    assert_eq!(d.count, 1000);
    assert_eq!(d.maximum_us, 999);
    assert!(d.p99_us >= 990);
    assert_eq!(d.mean_us, 499.5);
  }
  #[test]
  fn unmeasured_does_not_mean_passed() {
    let a = Acceptance {
      sync_measured: false,
      maximum_sync_ms: None,
      drift_ms: None,
      game_impact_measured: false,
      median_fps_loss_percent: None,
      p99_regression_percent: None,
    };
    assert_eq!(a.failures().len(), 6);
  }
}
