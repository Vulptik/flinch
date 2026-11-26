//! Combine only the owned lab and worker counters, keeping sampling skew explicit.
use crate::{EngineError, Result, time::SECOND};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct GpuMemory {
  pub local_usage_bytes: u64,
  pub local_budget_bytes: u64,
  pub local_reservation_bytes: u64,
  pub local_available_reservation_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ProcessResources {
  pub qpc_100ns: u64,
  pub cpu_total_100ns: u64,
  pub private_bytes: u64,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct CombinedResources {
  pub mean_core_equivalents: Option<f64>,
  pub private_bytes: u64,
  pub peak_observed_private_bytes: u64,
  pub sample_span_100ns: u64,
  pub sample_skew_100ns: u64,
  pub maximum_sample_skew_100ns: u64,
  pub samples: u64,
  pub diagnostics_lost: u64,
}
#[derive(Default)]
pub struct CombinedTracker {
  first: Option<(ProcessResources, ProcessResources)>,
  last: Option<(ProcessResources, ProcessResources)>,
  peak: u64,
  maximum_skew: u64,
  samples: u64,
}
impl CombinedTracker {
  pub fn observe(
    &mut self,
    worker: ProcessResources,
    supervisor: ProcessResources,
  ) -> Result<CombinedResources> {
    if supervisor.qpc_100ns < worker.qpc_100ns {
      return Err(EngineError::protocol(
        "Supervisor resource sample precedes its worker event",
      ));
    }
    if let Some((old_worker, old_supervisor)) = self.last {
      for (now, before) in [(worker, old_worker), (supervisor, old_supervisor)] {
        if now.qpc_100ns < before.qpc_100ns || now.cpu_total_100ns < before.cpu_total_100ns {
          return Err(EngineError::protocol(
            "Owned-process resource clock or CPU counter reversed",
          ));
        }
      }
    }
    let private = worker
      .private_bytes
      .checked_add(supervisor.private_bytes)
      .ok_or_else(|| EngineError::protocol("Combined private memory overflow"))?;
    let skew = supervisor.qpc_100ns - worker.qpc_100ns;
    let (base_worker, base_supervisor) = *self.first.get_or_insert((worker, supervisor));
    let elapsed = worker.qpc_100ns - base_worker.qpc_100ns;
    let supervisor_elapsed = supervisor.qpc_100ns - base_supervisor.qpc_100ns;
    // Cumulative CPU observations use aligned endpoints, never sums of averages with different epochs.
    let aligned = skew <= SECOND / 20
      && base_supervisor.qpc_100ns - base_worker.qpc_100ns <= SECOND / 20
      && elapsed.abs_diff(supervisor_elapsed) <= SECOND / 20;
    let cpu = if elapsed > 0 && aligned {
      let total = u128::from(worker.cpu_total_100ns - base_worker.cpu_total_100ns)
        + u128::from(supervisor.cpu_total_100ns - base_supervisor.cpu_total_100ns);
      Some(total as f64 / elapsed as f64)
    } else {
      None
    };
    self.peak = self.peak.max(private);
    self.maximum_skew = self.maximum_skew.max(skew);
    self.samples = self.samples.saturating_add(1);
    self.last = Some((worker, supervisor));
    Ok(CombinedResources {
      mean_core_equivalents: cpu,
      private_bytes: private,
      peak_observed_private_bytes: self.peak,
      sample_span_100ns: elapsed,
      sample_skew_100ns: skew,
      maximum_sample_skew_100ns: self.maximum_skew,
      samples: self.samples,
      diagnostics_lost: 0,
    })
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  fn process(qpc: u64, cpu: u64, memory: u64) -> ProcessResources {
    ProcessResources {
      qpc_100ns: qpc,
      cpu_total_100ns: cpu,
      private_bytes: memory,
    }
  }
  #[test]
  fn independent_cpu_origins_are_combined_as_deltas_over_the_same_observation_window() {
    let mut tracker = CombinedTracker::default();
    let first = tracker
      .observe(process(SECOND, 9000, 100), process(SECOND + 10, 40000, 50))
      .expect("baseline");
    assert!(first.mean_core_equivalents.is_none());
    let end = tracker
      .observe(
        process(2 * SECOND, 9000 + SECOND / 10, 110),
        process(2 * SECOND + 10, 40000 + SECOND / 20, 60),
      )
      .expect("combined sample");
    assert!((end.mean_core_equivalents.expect("aligned CPU") - 0.15).abs() < 1e-9);
    assert_eq!(end.private_bytes, 170);
    assert_eq!(end.peak_observed_private_bytes, 170);
    assert_eq!(end.sample_span_100ns, SECOND);
  }
  #[test]
  fn delayed_samples_are_unmeasured_instead_of_reporting_zero_cpu_and_resets_are_errors() {
    let mut tracker = CombinedTracker::default();
    tracker
      .observe(process(SECOND, 100, 10), process(SECOND + 1, 200, 20))
      .expect("baseline");
    let late = tracker
      .observe(process(2 * SECOND, 1000, 20), process(3 * SECOND, 2000, 30))
      .expect("late event");
    assert!(late.mean_core_equivalents.is_none());
    assert_eq!(late.maximum_sample_skew_100ns, SECOND);
    assert!(
      tracker
        .observe(process(3 * SECOND, 2, 20), process(4 * SECOND, 2001, 30))
        .is_err()
    );
    assert!(
      CombinedTracker::default()
        .observe(process(2, 1, 0), process(1, 1, 0))
        .is_err()
    );
    assert!(
      CombinedTracker::default()
        .observe(process(1, 1, u64::MAX), process(2, 1, 1))
        .is_err()
    );
  }
}
