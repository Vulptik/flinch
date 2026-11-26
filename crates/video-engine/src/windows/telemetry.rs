use crate::{
  EngineError, Result,
  metrics::resources::ProcessResources,
  metrics::{Distribution, EncoderReport, Histogram, WriterReport},
};
use serde::{Deserialize, Serialize};
use windows::Win32::{
  Foundation::FILETIME,
  Graphics::Direct3D11::*,
  System::{
    ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
    Threading::{GetCurrentProcess, GetProcessTimes},
  },
};

fn filetime(t: FILETIME) -> u64 {
  (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}
fn process_time() -> Result<u64> {
  let mut creation = FILETIME::default();
  let mut exit = FILETIME::default();
  let mut kernel = FILETIME::default();
  let mut user = FILETIME::default();
  // SAFETY: pseudo-handle is never closed; the four stack output pointers are valid.
  unsafe {
    GetProcessTimes(
      GetCurrentProcess(),
      &mut creation,
      &mut exit,
      &mut kernel,
      &mut user,
    )
  }
  .map_err(|e| EngineError::native("process CPU metrics", e))?;
  filetime(kernel)
    .checked_add(filetime(user))
    .ok_or_else(|| EngineError::protocol("Process CPU counter overflow"))
}
pub struct ResourceSampler {
  cpu: u64,
  qpc: u64,
}
impl ResourceSampler {
  pub fn new() -> Result<Self> {
    Ok(Self {
      cpu: process_time()?,
      qpc: super::qpc()?,
    })
  }
  pub fn sample(&mut self) -> Result<(f64, u64)> {
    let (cores, sample) = self.sample_detailed()?;
    Ok((cores, sample.private_bytes))
  }
  pub fn sample_detailed(&mut self) -> Result<(f64, ProcessResources)> {
    let cpu = process_time()?;
    let qpc = super::qpc()?;
    if cpu < self.cpu || qpc < self.qpc {
      return Err(EngineError::protocol(
        "Process resource clock or CPU counter reversed",
      ));
    }
    let cores = if qpc > self.qpc {
      (cpu - self.cpu) as f64 / (qpc - self.qpc) as f64
    } else {
      0.0
    };
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
      cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
      ..Default::default()
    };
    // SAFETY: extended structure begins with PROCESS_MEMORY_COUNTERS and cb advertises its full size.
    unsafe {
      GetProcessMemoryInfo(
        GetCurrentProcess(),
        (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
        std::mem::size_of_val(&memory) as u32,
      )
    }
    .map_err(|e| EngineError::native("process RAM metrics", e))?;
    Ok((
      cores,
      ProcessResources {
        qpc_100ns: qpc,
        cpu_total_100ns: cpu,
        private_bytes: memory.PrivateUsage as u64,
      },
    ))
  }
}
#[derive(Default)]
pub struct Timings {
  pub prepare: Histogram,
  pub first_frame: Histogram,
  pub conversion_submit: Histogram,
  pub gpu_conversion: Histogram,
  pub pause: Histogram,
  pub resume: Histogram,
  pub finalize: Histogram,
  /// The Stop thumbnail: surface readback, downscale and file write.
  pub thumbnail: Histogram,
  pub gpu_disjoint: u64,
  pub gpu_samples_skipped: u64,
  pub encoder: Option<EncoderReport>,
  pub writer: Option<WriterReport>,
}
#[derive(Serialize, Deserialize)]
pub struct TimingReport {
  pub prepare: Distribution,
  pub first_frame: Distribution,
  pub conversion_submit: Distribution,
  pub gpu_conversion: Distribution,
  pub pause: Distribution,
  pub resume: Distribution,
  pub finalize: Distribution,
  #[serde(default)]
  pub thumbnail: Distribution,
  pub gpu_disjoint: u64,
  pub gpu_samples_skipped: u64,
  #[serde(default)]
  pub encoder: Option<EncoderReport>,
  #[serde(default)]
  pub writer: Option<WriterReport>,
}
impl Timings {
  pub fn report(&self) -> TimingReport {
    TimingReport {
      prepare: self.prepare.summary(),
      first_frame: self.first_frame.summary(),
      conversion_submit: self.conversion_submit.summary(),
      gpu_conversion: self.gpu_conversion.summary(),
      pause: self.pause.summary(),
      resume: self.resume.summary(),
      finalize: self.finalize.summary(),
      thumbnail: self.thumbnail.summary(),
      gpu_disjoint: self.gpu_disjoint,
      gpu_samples_skipped: self.gpu_samples_skipped,
      encoder: self.encoder,
      writer: self.writer,
    }
  }
}

struct QuerySlot {
  disjoint: ID3D11Query,
  begin: ID3D11Query,
  end: ID3D11Query,
  pending: bool,
}
pub struct GpuTimers {
  slots: Vec<QuerySlot>,
  next: usize,
}
impl GpuTimers {
  pub fn new(device: &ID3D11Device) -> Result<Self> {
    let create = |kind| -> Result<ID3D11Query> {
      let mut query = None;
      // SAFETY: query descriptors are fixed, and the returned object remains on the GPU owner.
      unsafe {
        device.CreateQuery(
          &D3D11_QUERY_DESC {
            Query: kind,
            MiscFlags: 0,
          },
          Some(&mut query),
        )
      }
      .map_err(|e| EngineError::native("GPU timing query", e))?;
      query.ok_or_else(|| EngineError::protocol("No GPU timing query"))
    };
    let mut slots = Vec::with_capacity(4);
    for _ in 0..4 {
      slots.push(QuerySlot {
        disjoint: create(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
        begin: create(D3D11_QUERY_TIMESTAMP)?,
        end: create(D3D11_QUERY_TIMESTAMP)?,
        pending: false,
      });
    }
    Ok(Self { slots, next: 0 })
  }
  pub fn begin(&mut self, context: &ID3D11DeviceContext) -> Option<usize> {
    let slot = &mut self.slots[self.next];
    if slot.pending {
      return None;
    }
    let index = self.next;
    self.next = (self.next + 1) % 4;
    // SAFETY: begin/end disjoint scopes are serialized on the immediate-context owner.
    unsafe {
      context.Begin(&slot.disjoint);
      context.End(&slot.begin);
    }
    slot.pending = true;
    Some(index)
  }
  pub fn end(&self, context: &ID3D11DeviceContext, index: usize) {
    // SAFETY: index comes only from begin and scope ends on the same owner.
    unsafe {
      context.End(&self.slots[index].end);
      context.End(&self.slots[index].disjoint);
    }
  }
  pub fn collect(&mut self, context: &ID3D11DeviceContext, timings: &mut Timings) -> Result<()> {
    for slot in &mut self.slots {
      if !slot.pending {
        continue;
      }
      let mut disjoint = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
      let mut begin = u64::MAX;
      let mut end = u64::MAX;
      // SAFETY: bounded non-flushing reads. S_FALSE leaves the initialized sentinels unchanged.
      unsafe {
        context
          .GetData(
            &slot.disjoint,
            Some((&mut disjoint as *mut D3D11_QUERY_DATA_TIMESTAMP_DISJOINT).cast()),
            std::mem::size_of_val(&disjoint) as u32,
            D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
          )
          .map_err(|e| EngineError::native("GPU disjoint timing", e))?;
        context
          .GetData(
            &slot.begin,
            Some((&mut begin as *mut u64).cast()),
            8,
            D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
          )
          .map_err(|e| EngineError::native("GPU begin timing", e))?;
        context
          .GetData(
            &slot.end,
            Some((&mut end as *mut u64).cast()),
            8,
            D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
          )
          .map_err(|e| EngineError::native("GPU end timing", e))?;
      }
      if disjoint.Frequency == 0 || begin == u64::MAX || end == u64::MAX {
        continue;
      }
      if disjoint.Disjoint.as_bool() || end < begin {
        timings.gpu_disjoint += 1;
      } else {
        timings.gpu_conversion.record(crate::time::scale(
          end - begin,
          1_000_000,
          disjoint.Frequency,
        )?);
      }
      slot.pending = false;
    }
    Ok(())
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn aggregated_session_snapshot_still_fits_the_bounded_diagnostic_journal() {
    use crate::{
      audio::normalize::AudioDiagnostics,
      metrics::resources::{CombinedResources, ProcessResources},
      types::SessionSnapshot,
    };
    let distribution = Distribution {
      count: 648000,
      mean_us: 1321.987654321,
      p50_us: 2047,
      p95_us: 8191,
      p99_us: 32767,
      maximum_us: 1_000_000,
    };
    let snapshot = SessionSnapshot {
      first_media: crate::metrics::MediaMilestones {
        start_qpc_100ns: Some(100_000_000_000_000),
        capture_qpc_100ns: Some(100_000_000_000_001),
        submit_qpc_100ns: Some(100_000_000_000_002),
        encoded_qpc_100ns: Some(100_000_000_000_003),
        sink_accept_qpc_100ns: Some(100_000_000_000_004),
      },
      gpu_memory: Some(crate::metrics::resources::GpuMemory {
        local_usage_bytes: 300 * 1024 * 1024,
        local_budget_bytes: 16 * 1024 * 1024 * 1024,
        local_reservation_bytes: 1024 * 1024 * 1024,
        local_available_reservation_bytes: 8 * 1024 * 1024 * 1024,
      }),
      media_100ns: 10800 * crate::time::SECOND,
      frames_new: 648000,
      frames_encoded: 648000,
      private_bytes: 300 * 1024 * 1024,
      bytes_written: 100_000_000_000,
      audio_frames: 10800 * 48000,
      audio_diagnostics: AudioDiagnostics {
        received_packets: 1080000,
        discarded_unmapped_frames: 100000000,
        source_silent_frames: 100000000,
        inserted_silence_frames: 100000000,
        ..Default::default()
      },
      encoder_metrics: Some(EncoderReport {
        first_input_qpc_100ns: Some(100_000_000_000_002),
        first_output_qpc_100ns: Some(100_000_000_000_003),
        input_wait: distribution,
        native_submit: distribution,
        native_query: distribution,
        encoded_output_observed: distribution,
        texture_lease_observed: distribution,
        source_age_at_accept: distribution,
        source_time_unavailable: 1000000,
        pending_outputs_peak: 256,
        ..Default::default()
      }),
      writer_metrics: Some(WriterReport {
        first_packet_qpc_100ns: Some(100_000_000_000_004),
        queue_wait: distribution,
        native_write: distribution,
        native_finalize: distribution,
        packets_written: 1000000,
        bytes_written: 100_000_000_000,
        ..Default::default()
      }),
      process_resources: Some(ProcessResources {
        qpc_100ns: 100_000_000_000_000,
        cpu_total_100ns: 99_000_000_000,
        private_bytes: 300 * 1024 * 1024,
      }),
      combined_resources: Some(CombinedResources {
        private_bytes: 300 * 1024 * 1024,
        mean_core_equivalents: Some(0.1234567890123456),
        sample_span_100ns: 10800 * crate::time::SECOND,
        ..Default::default()
      }),
      ..Default::default()
    };
    assert!(
      serde_json::to_vec(&snapshot)
        .expect("aggregated snapshot")
        .len()
        <= 4095
    );
  }
  #[test]
  fn metrics_read_only_current_process_without_capturing_devices() {
    let mut sampler = ResourceSampler::new().expect("process sampler");
    let (cores, bytes) = sampler.sample().expect("process sample");
    assert!(cores.is_finite());
    assert!(bytes > 0);
  }
}
