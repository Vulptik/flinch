//! Bounded, opt-in measurements. No timer, worker, lock, or per-pointer logging.

use std::collections::VecDeque;
use std::time::Instant;

const SAMPLE_LIMIT: usize = 256;

pub(crate) fn enabled() -> bool {
  matches!(
    std::env::var("FLINCH_ANNOTATION_DIAGNOSTICS").as_deref(),
    Ok("1")
  )
}

#[derive(Clone, Default, serde::Serialize)]
pub(crate) struct PaintSample {
  pub total_us: u64,
  pub scene_us: u64,
  pub annotations_us: u64,
  pub toolbar_us: u64,
  pub present_us: u64,
  pub dirty_pixels: u64,
  pub composed_pixels: u64,
  pub presented_pixels: u64,
  pub drawings: usize,
  pub points: usize,
  pub surface_misses: u64,
  pub icon_misses: u64,
  pub completed_layer_rebuilds: u64,
  pub cache_bytes: u64,
}

#[derive(Default)]
pub(crate) struct Diagnostics {
  samples: VecDeque<PaintSample>,
  cold: Option<PaintSample>,
  paint_count: u64,
}

impl Diagnostics {
  pub fn start(&self) -> Instant {
    Instant::now()
  }

  pub fn record(&mut self, sample: PaintSample) {
    self.paint_count = self.paint_count.saturating_add(1);
    if self.cold.is_none() {
      self.cold = Some(sample);
      return;
    }
    if self.samples.len() == SAMPLE_LIMIT {
      self.samples.pop_front();
    }
    self.samples.push_back(sample);
  }

  pub fn report(&self) -> serde_json::Value {
    let mut times: Vec<_> = self.samples.iter().map(|sample| sample.total_us).collect();
    times.sort_unstable();
    let percentile = |percent: usize| {
      times
        .get((times.len() * percent).div_ceil(100).saturating_sub(1))
        .copied()
    };
    let stages = [
      (
        "scene",
        (|sample: &PaintSample| sample.scene_us) as fn(&PaintSample) -> u64,
      ),
      (
        "annotations",
        (|sample: &PaintSample| sample.annotations_us) as fn(&PaintSample) -> u64,
      ),
      (
        "toolbar",
        (|sample: &PaintSample| sample.toolbar_us) as fn(&PaintSample) -> u64,
      ),
      (
        "present",
        (|sample: &PaintSample| sample.present_us) as fn(&PaintSample) -> u64,
      ),
    ]
    .into_iter()
    .map(|(name, get)| {
      let mut values: Vec<_> = self.samples.iter().map(get).collect();
      values.sort_unstable();
      let value = |percent: usize| {
        values
          .get((values.len() * percent).div_ceil(100).saturating_sub(1))
          .copied()
      };
      (
        name.to_owned(),
        serde_json::json!({"p50_us":value(50),"p95_us":value(95)}),
      )
    })
    .collect::<serde_json::Map<String, serde_json::Value>>();
    serde_json::json!({
      "warm_stages":stages,
      "warm_peak_cache_bytes":self.samples.iter().map(|sample| sample.cache_bytes).max(),
      "warm_completed_layer_rebuilds":self.samples.iter().map(|sample| sample.completed_layer_rebuilds).sum::<u64>(),
      "warm_dirty_pixels":self.samples.iter().map(|sample| sample.dirty_pixels).sum::<u64>(),
      "warm_composed_pixels":self.samples.iter().map(|sample| sample.composed_pixels).sum::<u64>(),
      "warm_presented_pixels":self.samples.iter().map(|sample| sample.presented_pixels).sum::<u64>(),
      "warm_surface_misses":self.samples.iter().map(|sample| sample.surface_misses).sum::<u64>(),
      "warm_icon_misses":self.samples.iter().map(|sample| sample.icon_misses).sum::<u64>(),
      "paint_count": self.paint_count,
      "warm_sample_count": times.len(),
      "warm_p50_us": percentile(50),
      "warm_p95_us": percentile(95),
      "cold": self.cold,
      "warm_samples": self.samples,
    })
  }
}

impl Drop for Diagnostics {
  fn drop(&mut self) {
    if self.paint_count != 0 {
      let mut report = self.report();
      // Raw samples belong in the explicit benchmark artifact, not the normal app log.
      if let Some(object) = report.as_object_mut() {
        object.remove("warm_samples");
      }
      tracing::info!(measurements = %report, "Screenshot annotation paint diagnostics (last 256 warm paints)");
    }
  }
}

pub(crate) fn elapsed_us(started: Instant) -> u64 {
  started.elapsed().as_micros().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn diagnostics_bound_samples_and_keep_cold_separate() {
    let mut diagnostics = Diagnostics::default();
    for total_us in 0..1000 {
      diagnostics.record(PaintSample {
        total_us,
        ..Default::default()
      });
    }
    let report = diagnostics.report();
    assert_eq!(report["paint_count"], 1000);
    assert_eq!(report["warm_sample_count"], 256);
    assert_eq!(report["cold"]["total_us"], 0);
    assert_eq!(report["warm_p50_us"], 871);
    assert_eq!(report["warm_p95_us"], 987);
    assert!(Diagnostics::default().report()["warm_p95_us"].is_null());
  }
}
