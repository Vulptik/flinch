use crate::{EngineError, ErrorKind, Result, types::Track};
use std::{
  collections::VecDeque,
  time::{Duration, Instant},
};

pub const TEXTURE_LIMIT: u64 = 256 * 1024 * 1024;
/// The smallest encode-surface pool, and the whole pool for the CPU conversion, whose software
/// encoder releases each surface as soon as its readback copy is queued.
pub const MIN_SURFACES: usize = 8;
/// Every encoder's slot table, native bridges included, is sized for this many surfaces.
pub const MAX_SURFACES: usize = 16;
/// Encoder turnaround the pool absorbs. AMF held surfaces 65 ms at p50 and up to about 120 ms at
/// p99 while another app kept the GPU busy (measured); eight surfaces cover this at
/// 60 FPS but only 67 ms at 120 FPS, which lost about 6% of slots.
const SURFACE_COVERAGE_MS: u32 = 133;
pub const PACKET_LIMIT: usize = 64 * 1024 * 1024;
pub const PCM_FRAMES_LIMIT: usize = 48_000;
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(15);
pub const START_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait during Prepare for an automatic capture backend's first frame while another
/// backend remains to try. A fresh duplication normally delivers the desktop at once.
pub const CAPTURE_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
/// While Windows refuses desktop duplication because the secure desktop is showing (a UAC prompt,
/// Ctrl+Alt+Del), recreation is retried this often and the recording holds its last picture.
/// A real recording ended at a UAC prompt when both attempts ran at once.
pub const CAPTURE_DENIED_RETRY: Duration = Duration::from_millis(250);
/// Spacing of the two ordinary recreation attempts after duplication access is lost, so both fit
/// in the [`CONTROL_TIMEOUT`] window instead of running in the same instant.
pub const CAPTURE_RETRY_SPACING: Duration = Duration::from_secs(1);
/// Ordinary recreation attempts after duplication access is lost; refusals while the secure
/// desktop shows are not counted.
pub const CAPTURE_RECREATION_ATTEMPTS: u8 = 2;
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for the next encoded packet while inputs are pending. A slow encoder is not a
/// failure: its refused inputs are lost slots and the recording is Degraded and keeps
/// going. This bound only detects a hung encoder; at two
/// seconds a game starving AMF failed recordings that were still producing frames.
pub const ENCODER_STALL_TIMEOUT: Duration = Duration::from_secs(10);
pub const ABORT_TIMEOUT: Duration = Duration::from_secs(3);
/// WGC dates a frame to its composition target, observed 2-4 ms after it is already accepted.
/// One refresh down to 20 Hz still fits; anything larger is a clock-domain error.
pub const SOURCE_LEAD_LIMIT: Duration = Duration::from_millis(50);

/// Enforce the record bound during serialization, before a complete JSON value
/// can allocate an arbitrarily large temporary buffer.
pub(crate) fn json_record(value: &impl serde::Serialize, limit: usize) -> Result<Vec<u8>> {
  let mut output = JsonRecord {
    bytes: Vec::with_capacity(limit.min(512)),
    limit,
  };
  serde_json::to_writer(&mut output, value)
    .map_err(|error| EngineError::protocol(error.to_string()))?;
  Ok(output.bytes)
}
struct JsonRecord {
  bytes: Vec<u8>,
  limit: usize,
}
impl std::io::Write for JsonRecord {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
      return Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "JSON record exceeds its byte limit",
      ));
    }
    let needed = self.bytes.len() + bytes.len();
    if needed > self.bytes.capacity() {
      let capacity = self
        .bytes
        .capacity()
        .saturating_mul(2)
        .max(needed)
        .min(self.limit);
      self.bytes.reserve_exact(capacity - self.bytes.len());
    }
    self.bytes.extend_from_slice(bytes);
    Ok(bytes.len())
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

pub fn texture_bytes(width: u32, height: u32) -> Result<u64> {
  pool_bytes(
    width,
    height,
    width,
    height,
    crate::types::ConversionKind::Gpu,
    MIN_SURFACES,
  )
}
/// Encode surfaces covering `SURFACE_COVERAGE_MS` of frames at `fps`, within the slot tables.
pub fn wanted_surfaces(fps: u32) -> usize {
  let frames = fps.saturating_mul(SURFACE_COVERAGE_MS).div_ceil(1000) as usize;
  frames.clamp(MIN_SURFACES, MAX_SURFACES)
}
/// Texture bytes and surface count for a session. GPU conversion gets the surfaces its frame
/// rate wants, trimmed (never below `MIN_SURFACES`) while they would exceed the texture budget;
/// the count is reported in the session snapshot.
pub fn pool(
  source_width: u32,
  source_height: u32,
  width: u32,
  height: u32,
  conversion: crate::types::ConversionKind,
  fps: u32,
) -> Result<(u64, usize)> {
  let wanted = match conversion {
    crate::types::ConversionKind::Gpu => wanted_surfaces(fps),
    crate::types::ConversionKind::Cpu => MIN_SURFACES,
  };
  let mut surfaces = wanted;
  loop {
    match pool_bytes(
      source_width,
      source_height,
      width,
      height,
      conversion,
      surfaces,
    ) {
      Ok(bytes) => return Ok((bytes, surfaces)),
      Err(error) if error.kind == ErrorKind::Capability && surfaces > MIN_SURFACES => {
        surfaces -= 1;
      },
      Err(error) => return Err(error),
    }
  }
}
/// Two BGRA source-size caches plus `surfaces` encode surfaces of the (possibly cropped) output:
/// NV12 for GPU conversion, BGRA for the CPU conversion. Only the output must be even; displays
/// such as 2556×1179 have odd BGRA sources.
pub fn pool_bytes(
  source_width: u32,
  source_height: u32,
  width: u32,
  height: u32,
  conversion: crate::types::ConversionKind,
  surfaces: usize,
) -> Result<u64> {
  if source_width < 2 || source_height < 2 {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "textures",
      "Display dimensions must be at least 2x2",
    ));
  }
  if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "textures",
      "Dimensions must be positive and even",
    ));
  }
  let source = u64::from(source_width).checked_mul(u64::from(source_height));
  let output = u64::from(width).checked_mul(u64::from(height));
  let bytes = source
    .and_then(|p| p.checked_mul(8))
    .zip(output.and_then(|p| {
      // NV12 is 1.5 bytes per pixel (even widths keep it whole), BGRA 4.
      let per_surface = match conversion {
        crate::types::ConversionKind::Gpu => p.checked_mul(3).map(|n| n / 2),
        crate::types::ConversionKind::Cpu => p.checked_mul(4),
      };
      per_surface.and_then(|n| n.checked_mul(surfaces as u64))
    }))
    .and_then(|(source, output)| source.checked_add(output))
    .filter(|n| *n <= TEXTURE_LIMIT)
    .ok_or_else(|| {
      EngineError::new(
        ErrorKind::Capability,
        "textures",
        "GPU texture budget exceeded",
      )
    })?;
  Ok(bytes)
}

pub struct BoundedQueue<T> {
  items: VecDeque<(T, usize, u64)>,
  bytes: usize,
  max_bytes: usize,
  max_span: u64,
  count: usize,
}
impl<T> BoundedQueue<T> {
  pub fn new(count: usize, max_bytes: usize, max_span: u64) -> Self {
    Self {
      items: VecDeque::with_capacity(count),
      bytes: 0,
      max_bytes,
      max_span,
      count,
    }
  }
  pub fn push(&mut self, item: T, bytes: usize, pts: u64) -> Result<()> {
    if self.items.len() >= self.count
      || bytes > self.max_bytes.saturating_sub(self.bytes)
      || self
        .items
        .front()
        .is_some_and(|v| pts < v.2 || pts - v.2 > self.max_span)
    {
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "queue",
        "Bounded media queue capacity exceeded",
      ));
    }
    self.items.push_back((item, bytes, pts));
    self.bytes += bytes;
    Ok(())
  }
  pub fn pop(&mut self) -> Option<T> {
    self.items.pop_front().map(|(v, size, _)| {
      self.bytes -= size;
      v
    })
  }
  pub fn len(&self) -> usize {
    self.items.len()
  }
  pub fn is_empty(&self) -> bool {
    self.items.is_empty()
  }
  pub fn bytes(&self) -> usize {
    self.bytes
  }
}

pub struct ProgressWatch {
  last: Instant,
  pending: bool,
  limit: Duration,
}

/// Includes the packet currently inside the native sink. Tracks may be interleaved.
/// The two-second media span is a backlog bound, so it is measured within each track: a starved
/// hardware encoder delivers video seconds behind live audio, and a cross-track span failed such
/// a recording with one packet of each queued (measured with a game on the same GPU).
pub struct PacketBudget {
  pending: VecDeque<(usize, Track, u64, Instant)>,
  bytes: usize,
  pub high_bytes: usize,
  pub high_count: usize,
}
impl Default for PacketBudget {
  fn default() -> Self {
    Self {
      pending: VecDeque::with_capacity(257),
      bytes: 0,
      high_bytes: 0,
      high_count: 0,
    }
  }
}
impl PacketBudget {
  pub fn reserve(&mut self, bytes: usize, track: Track, pts: u64, now: Instant) -> Result<()> {
    let same = || self.pending.iter().filter(|p| p.1 == track).map(|p| p.2);
    let min = same().min().unwrap_or(pts).min(pts);
    let max = same().max().unwrap_or(pts).max(pts);
    if self.pending.len() >= 257
      || bytes > PACKET_LIMIT.saturating_sub(self.bytes)
      || max - min > 2 * crate::time::SECOND
    {
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "writer budget",
        "Compressed packets exceed 64 MiB or two seconds",
      ));
    }
    self.bytes += bytes;
    self.pending.push_back((bytes, track, pts, now));
    self.high_bytes = self.high_bytes.max(self.bytes);
    self.high_count = self.high_count.max(self.pending.len());
    Ok(())
  }
  pub fn cancel_last(&mut self) {
    if let Some((bytes, ..)) = self.pending.pop_back() {
      self.bytes -= bytes;
    }
  }
  pub fn complete(&mut self) -> Result<()> {
    let (bytes, ..) = self
      .pending
      .pop_front()
      .ok_or_else(|| EngineError::protocol("Writer released an unowned packet"))?;
    self.bytes -= bytes;
    Ok(())
  }
  pub fn check(&self, now: Instant) -> Result<()> {
    if self
      .pending
      .front()
      .is_some_and(|p| now.saturating_duration_since(p.3) > CONTROL_TIMEOUT)
    {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "writer progress",
        "Native write retained a packet for over two seconds",
      ));
    }
    Ok(())
  }
}
impl ProgressWatch {
  pub fn new(now: Instant, limit: Duration) -> Self {
    Self {
      last: now,
      pending: false,
      limit,
    }
  }
  pub fn pending(&mut self, now: Instant, pending: bool) {
    if !self.pending {
      self.last = now;
    }
    self.pending = pending;
  }
  pub fn progressed(&mut self, now: Instant) {
    self.last = now;
  }
  pub fn check(&self, now: Instant, stage: &str) -> Result<()> {
    if self.pending && now.saturating_duration_since(self.last) > self.limit {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        stage,
        "Pending native stage made no progress",
      ));
    }
    Ok(())
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn json_record_limits_serialization_work_and_buffer_growth_before_a_large_value_finishes() {
    use serde::ser::SerializeSeq;
    struct Many(std::cell::Cell<usize>);
    impl serde::Serialize for Many {
      fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
      ) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(1000))?;
        for _ in 0..1000 {
          self.0.set(self.0.get() + 1);
          sequence.serialize_element("payload")?;
        }
        sequence.end()
      }
    }
    let value = Many(std::cell::Cell::new(0));
    let mut output = JsonRecord {
      bytes: Vec::with_capacity(16),
      limit: 128,
    };
    assert!(serde_json::to_writer(&mut output, &value).is_err());
    assert!(
      value.0.get() < 32,
      "serialization stops before consuming the whole value"
    );
    assert!(output.bytes.len() <= 128 && output.bytes.capacity() <= 128);
    assert!(std::io::Write::flush(&mut output).is_ok());
    assert_eq!(
      json_record(&"a\n", 5).expect("escaped record at exact bound"),
      b"\"a\\n\""
    );
    assert!(json_record(&"a\n", 4).is_err());
    assert!(json_record(&false, 0).is_err());
  }
  #[test]
  fn budgets_are_independent_and_released_after_consumption() {
    let mut q = BoundedQueue::new(2, 5, 20);
    q.push(1, 3, 0).expect("valid test fixture");
    assert!(q.push(2, 3, 1).is_err());
    assert!(q.push(2, 1, 21).is_err());
    q.push(2, 2, 20).expect("valid test fixture");
    assert!(q.push(3, 0, 20).is_err());
    assert_eq!(q.len(), 2);
    assert_eq!(q.pop(), Some(1));
    assert_eq!((q.len(), q.bytes()), (1, 2));
    assert_eq!(q.pop(), Some(2));
    assert!(q.is_empty());
    assert!(texture_bytes(3840, 2160).is_ok());
    assert!(texture_bytes(8192, 8192).is_err());
    assert!(texture_bytes(3, 4).is_err());
    // An odd display (for example 2556×1179) feeds an even crop.
    use crate::types::ConversionKind::{Cpu, Gpu};
    assert!(pool_bytes(2556, 1179, 2556, 1178, Gpu, MIN_SURFACES).is_ok());
    assert!(pool_bytes(2556, 1179, 2556, 1179, Gpu, MIN_SURFACES).is_err());
    assert!(pool_bytes(1, 1179, 0, 1178, Gpu, MIN_SURFACES).is_err());
    // CPU conversion keeps BGRA surfaces: 4 bytes per pixel instead of 1.5.
    let (source, surfaces) = (1024 * 768 * 8, 1024 * 768);
    assert_eq!(
      pool_bytes(1024, 768, 1024, 768, Gpu, 8).expect("NV12"),
      source + surfaces * 12
    );
    assert_eq!(
      pool_bytes(1024, 768, 1024, 768, Cpu, 8).expect("BGRA"),
      source + surfaces * 32
    );
    assert_eq!(
      pool_bytes(1024, 768, 1024, 768, Gpu, 16).expect("NV12"),
      source + surfaces * 24
    );
  }
  #[test]
  fn the_surface_pool_covers_loaded_encoder_turnaround_at_every_frame_rate() {
    // Regression: eight surfaces at 120 FPS covered 67 ms; AMF under another app's GPU load held
    // them 65 ms at p50 and lost about 6% of slots for want of a free surface.
    use crate::types::ConversionKind::{Cpu, Gpu};
    for (fps, wanted) in [(15, 8), (30, 8), (60, 8), (120, 16)] {
      assert_eq!(wanted_surfaces(fps), wanted, "{fps} FPS");
      assert!(
        wanted as u32 * 1000 / fps >= SURFACE_COVERAGE_MS,
        "{fps} FPS covers less than the measured turnaround"
      );
    }
    assert_eq!(wanted_surfaces(0), MIN_SURFACES);
    assert_eq!(wanted_surfaces(u32::MAX), MAX_SURFACES);
    assert_eq!(pool(1920, 1080, 1920, 1080, Gpu, 120).expect("1080p").1, 16);
    assert_eq!(pool(1920, 1080, 1920, 1080, Gpu, 60).expect("1080p").1, 8);
    assert_eq!(pool(3840, 2160, 3840, 2160, Gpu, 120).expect("4K").1, 16);
    // The software encoder copies surfaces out at once: the CPU conversion never grows its pool.
    assert_eq!(pool(1920, 1080, 1920, 1080, Cpu, 120).expect("BGRA").1, 8);
    // A display whose sixteen surfaces exceed the budget keeps as many as fit, reported.
    let (bytes, surfaces) = pool(5120, 2160, 5120, 2160, Gpu, 120).expect("ultrawide");
    assert_eq!(surfaces, 10);
    assert!(bytes <= TEXTURE_LIMIT);
    assert!(pool_bytes(5120, 2160, 5120, 2160, Gpu, 11).is_err());
    // Past the minimum the budget still refuses, and invalid extents fail at every size.
    assert!(pool(8192, 8192, 8192, 8192, Gpu, 120).is_err());
    assert!(pool(1920, 1080, 1919, 1080, Gpu, 120).is_err());
  }
  #[test]
  fn every_native_bridge_tracks_as_many_slots_as_the_largest_pool() {
    // A bridge with fewer slots reports the rest busy forever, silently capping the pool.
    for (name, source, marker) in [
      (
        "AMF",
        include_str!("../native/amf_bridge.cpp"),
        "constexpr unsigned kSlots = ",
      ),
      (
        "NVENC",
        include_str!("../native/nvenc_bridge.c"),
        "#define FLINCH_NVENC_SLOTS ",
      ),
      (
        "QSV",
        include_str!("../native/qsv_bridge.cpp"),
        "constexpr uint32_t kSlots = ",
      ),
    ] {
      let slots = source
        .lines()
        .find_map(|line| line.trim().strip_prefix(marker))
        .map(|rest| rest.trim_end_matches(';').trim().parse::<usize>());
      assert_eq!(slots, Some(Ok(MAX_SURFACES)), "{name} bridge slots");
    }
  }
  #[test]
  fn heartbeat_does_not_hide_pending_stage_stall() {
    let now = Instant::now();
    let mut w = ProgressWatch::new(now, CONTROL_TIMEOUT);
    assert!(w.check(now + Duration::from_secs(50), "idle").is_ok());
    w.pending(now, true);
    assert!(w.check(now + Duration::from_secs(3), "encoder").is_err());
    w.progressed(now + Duration::from_secs(3));
    assert!(w.check(now + Duration::from_secs(4), "encoder").is_ok());
  }
  #[test]
  fn writer_checks_interleaved_tracks_inflight_and_native_stall() {
    let now = Instant::now();
    let mut b = PacketBudget::default();
    b.reserve(5, Track::Video, 1000, now).expect("video");
    b.reserve(6, Track::Audio, 500, now)
      .expect("earlier audio is legal");
    assert!(
      b.reserve(1, Track::Audio, 500 + 2 * crate::time::SECOND + 1, now)
        .is_err()
    );
    assert!(b.reserve(PACKET_LIMIT, Track::Audio, 501, now).is_err());
    assert!(b.check(now + Duration::from_secs(3)).is_err());
    b.cancel_last();
    b.complete().expect("first released");
    assert!(b.complete().is_err());
    assert!(b.check(now + Duration::from_secs(3)).is_ok());
    assert_eq!(b.high_bytes, 11);
  }
  #[test]
  fn video_late_behind_live_audio_is_not_a_writer_backlog() {
    let now = Instant::now();
    let mut b = PacketBudget::default();
    let late = 25 * crate::time::SECOND / 10;
    b.reserve(5, Track::Audio, late + 4 * crate::time::SECOND, now)
      .expect("live audio");
    // A starved encoder's frame, 2.5 s behind the audio already queued.
    b.reserve(6, Track::Video, 4 * crate::time::SECOND, now)
      .expect("one late video packet is no backlog");
    // A real backlog still fails within a track.
    assert_eq!(
      b.reserve(7, Track::Video, late + 4 * crate::time::SECOND + 1, now)
        .expect_err("video backlog over two seconds")
        .kind,
      ErrorKind::Saturation
    );
  }
}
