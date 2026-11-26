//! Packet dating, reorder reserve and DSP ownership, without devices, codecs or wall-clock waits.
use super::{AudioBuffer, AudioPacket, SincResampler};
use crate::{
  EngineError, ErrorKind, Result,
  limits::PCM_FRAMES_LIMIT,
  time::{self, SECOND},
};
use serde::{Deserialize, Serialize};

pub const REORDER_100NS: u64 = SECOND / 20;
const MAX_PACKETS: usize = 100;
const CLIENT_RATE: u64 = 48000;
/// Packet QPC noise absorbed on a position-contiguous stream. A VirtualBox HDA endpoint
/// advanced exactly 448 mix positions per packet while its packet QPCs scattered
/// ±1 ms (p99) around that line, with isolated single-packet outliers of 3–11 ms. The samples are
/// one continuous stream, so they are placed back to back. Bounded by the sync bar.
const JITTER_100NS: u64 = SECOND / 200;
/// Larger disagreements are absorbed only as isolated outliers: `MAX_OUTLIERS` in a row, then
/// the stream re-anchors on its own date, so a real offset is never carried forward.
const OUTLIER_100NS: u64 = SECOND / 50;
const MAX_OUTLIERS: u8 = 2;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct AudioDiagnostics {
  pub revision: u64,
  pub reports_coalesced: u64,
  pub received_packets: u64,
  pub reordered_packets: u64,
  pub late_packets: u64,
  pub late_frames: u64,
  pub discarded_unmapped_frames: u64,
  pub source_silent_frames: u64,
  pub discontinuities: u64,
  pub gaps: u64,
  pub overlaps: u64,
  /// Frames of a re-anchored packet that fell on samples an earlier flush already placed.
  #[serde(default)]
  pub overlap_trimmed_frames: u64,
  /// Position-contiguous packets whose jittered QPC was replaced by the previous packet's end.
  #[serde(default)]
  pub jitter_redated_packets: u64,
  /// Position-contiguous packets whose date disagreed persistently or beyond the outlier bound,
  /// re-anchored on their own QPC (later dates also count as gaps; earlier ones trim overlap).
  #[serde(default)]
  pub clock_realignments: u64,
  /// Largest amount (100 ns) by which the endpoint dated a packet after it was read; every packet
  /// date is moved earlier by it. Zero on endpoints whose dates are causal.
  #[serde(default)]
  pub causal_lead_100ns: u64,
  pub invalid_timestamp_packets: u64,
  pub queue_packets_peak: usize,
  pub queue_frames_peak: usize,
  pub queued_packets: usize,
  pub queued_frames: usize,
  pub inserted_silence_frames: u64,
  pub drift_ppm: f64,
  pub filter_lookahead_frames: usize,
  pub resampler_tail_shortfall_frames: u64,
  pub resampler_tail_trimmed_frames: u64,
}
#[derive(Clone, Copy)]
struct Mapping {
  qpc: u64,
  media: u64,
}
impl Mapping {
  fn frame(self, qpc: u64) -> Result<i128> {
    let media = i128::from(self.media) + i128::from(qpc) - i128::from(self.qpc);
    media
      .checked_add(1)
      .and_then(|value| value.checked_mul(i128::from(CLIENT_RATE)))
      .and_then(|value| value.checked_sub(1))
      .map(|value| value.div_euclid(i128::from(SECOND)))
      .ok_or_else(|| EngineError::protocol("Audio mapping overflow"))
  }
  fn source_frame(self, qpc: u64) -> Result<i128> {
    // Keep the committed media anchor exact; round only the relative endpoint sample coordinate.
    let anchor = i128::from(time::sample_boundary(self.media, CLIENT_RATE as u32)?);
    let delta = i128::from(qpc) - i128::from(self.qpc);
    delta
      .checked_mul(i128::from(CLIENT_RATE))
      .and_then(|value| value.checked_add(i128::from(SECOND / 2)))
      .and_then(|value| value.div_euclid(i128::from(SECOND)).checked_add(anchor))
      .ok_or_else(|| EngineError::protocol("Audio source coordinate overflow"))
  }
  fn cut(self, media: u64) -> Result<u64> {
    media
      .checked_sub(self.media)
      .and_then(|delta| self.qpc.checked_add(delta))
      .ok_or_else(|| EngineError::protocol("Audio cutoff precedes its mapping or overflows QPC"))
  }
}

pub struct Normalizer {
  queue: Vec<AudioPacket>,
  queued_frames: usize,
  buffer: AudioBuffer,
  resampler: SincResampler,
  resampled: Vec<f32>,
  mapping: Option<Mapping>,
  cursor: Option<u64>,
  /// End of samples placed before the cursor was last released; a re-anchor cannot precede it.
  placed_end: Option<u64>,
  source_end: Option<u64>,
  /// Raw endpoint position (at its position rate) that continues the packet ending at `source_end`.
  next_position: Option<(u64, u32)>,
  /// Consecutive contiguous packets absorbed beyond `JITTER_100NS`.
  outliers: u8,
  source_tolerance: u64,
  ready: u64,
  observed_qpc: Option<u64>,
  diagnostics: AudioDiagnostics,
}
impl Default for Normalizer {
  fn default() -> Self {
    Self {
      queue: Vec::with_capacity(MAX_PACKETS),
      queued_frames: 0,
      buffer: AudioBuffer::default(),
      resampler: SincResampler::new(),
      resampled: Vec::with_capacity(PCM_FRAMES_LIMIT * 2 + 512),
      mapping: None,
      cursor: None,
      placed_end: None,
      source_end: None,
      next_position: None,
      outliers: 0,
      source_tolerance: 0,
      ready: 0,
      observed_qpc: None,
      diagnostics: AudioDiagnostics {
        filter_lookahead_frames: SincResampler::LOOKAHEAD_FRAMES,
        ..Default::default()
      },
    }
  }
}
impl Normalizer {
  pub fn map(&mut self, qpc: u64, media: u64) -> Result<()> {
    if self.mapping.is_some()
      || time::sample_boundary(media, CLIENT_RATE as u32)? != self.buffer.next()
    {
      return Err(EngineError::protocol(
        "Audio mapping is active or differs from the committed cutoff",
      ));
    }
    self.clear_source();
    self.buffer.discard_future();
    self.mapping = Some(Mapping { qpc, media });
    self.ready = self.buffer.next();
    Ok(())
  }
  fn clear_source(&mut self) {
    self.queue.clear();
    self.queued_frames = 0;
    self.resampler.reset();
    self.cursor = None;
    self.placed_end = None;
    self.source_end = None;
    self.next_position = None;
    self.outliers = 0;
    self.source_tolerance = 0;
    self.observed_qpc = None;
  }
  pub fn mapped(&self) -> bool {
    self.mapping.is_some()
  }
  pub fn ingest(&mut self, mut packet: AudioPacket) -> Result<()> {
    let frames = packet.data.len() / 2;
    if packet.client_rate != CLIENT_RATE as u32
      || !packet.data.len().is_multiple_of(2)
      || frames == 0
      || !(8000..=384000).contains(&packet.device_rate)
      || !(8000..=384000).contains(&packet.position_rate)
      || !packet.drift_ppm.is_finite()
      || packet.drift_ppm.abs() > 2000.0
      || packet.data.iter().any(|sample| !sample.is_finite())
    {
      return Err(EngineError::protocol(
        "Invalid normalized stereo PCM packet",
      ));
    }
    self.diagnostics.received_packets = self.diagnostics.received_packets.saturating_add(1);
    // Samples cannot be captured after they are read. VirtualBox HDA dated loopback packets 3 to
    // 155 ms after their read (varying per run), offsetting A/V sync by as much; the
    // largest such lead is a lower bound on the endpoint's dating bias and is removed.
    if let Some(read) = packet.read_qpc_100ns {
      let lead = packet.qpc_100ns.saturating_sub(read);
      self.diagnostics.causal_lead_100ns = self.diagnostics.causal_lead_100ns.max(lead);
    }
    packet.qpc_100ns = packet
      .qpc_100ns
      .saturating_sub(self.diagnostics.causal_lead_100ns);
    // AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR. The raw flag is retained, never substituted for QPC dating.
    self.diagnostics.invalid_timestamp_packets = self
      .diagnostics
      .invalid_timestamp_packets
      .saturating_add(u64::from(packet.wasapi_flags & 4 != 0));
    if self.mapping.is_none() {
      self.diagnostics.discarded_unmapped_frames = self
        .diagnostics
        .discarded_unmapped_frames
        .saturating_add(frames as u64);
      return Ok(());
    }
    if self.queue.len() >= MAX_PACKETS
      || frames > PCM_FRAMES_LIMIT.saturating_sub(self.queued_frames)
    {
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "audio reorder",
        "PCM reserve exceeds one second or 100 packets",
      ));
    }
    if self.queue.iter().any(|queued| {
      queued.raw_device_position == packet.raw_device_position
        && queued.device_rate == packet.device_rate
    }) {
      return Err(EngineError::protocol(
        "Duplicate raw endpoint packet in reorder reserve",
      ));
    }
    // Endpoint positions are the device's own order (the source rejects reversals); a noisy QPC
    // must not move a packet ahead of the samples that precede it.
    let index = self.queue.partition_point(|queued| {
      if queued.position_rate == packet.position_rate {
        queued.raw_device_position <= packet.raw_device_position
      } else {
        queued.qpc_100ns <= packet.qpc_100ns
      }
    });
    if index != self.queue.len() {
      self.diagnostics.reordered_packets = self.diagnostics.reordered_packets.saturating_add(1);
    }
    self.queue.insert(index, packet);
    self.queued_frames += frames;
    self.diagnostics.queue_packets_peak = self.diagnostics.queue_packets_peak.max(self.queue.len());
    self.diagnostics.queue_frames_peak = self.diagnostics.queue_frames_peak.max(self.queued_frames);
    Ok(())
  }
  pub fn process_ready(&mut self, now: u64) -> Result<()> {
    if self.observed_qpc.is_some_and(|previous| now < previous) {
      return Err(EngineError::protocol(
        "Audio processing QPC moved backwards",
      ));
    }
    self.observed_qpc = Some(now);
    let Some(mapping) = self.mapping else {
      return Ok(());
    };
    if self
      .queue
      .iter()
      .any(|packet| packet.qpc_100ns > now.saturating_add(SECOND))
    {
      return Err(EngineError::protocol(
        "Audio packet is dated more than one second into the future",
      ));
    }
    let settled = now.saturating_sub(REORDER_100NS);
    while self
      .queue
      .first()
      .is_some_and(|packet| packet.qpc_100ns <= settled)
    {
      let packet = self.pop()?;
      self.normalize(packet, None)?;
    }
    // A head packet that continues the stream by device position, within the offset `normalize`
    // re-dates onto the previous end, is the next audio even while its raw date is unsettled.
    // Flushing before it would let the gap be read as silence and trim its real samples later.
    let continued = self
      .queue
      .first()
      .is_some_and(|packet| self.redated_onto_end(packet));
    if !continued
      && self
        .source_end
        .is_some_and(|end| settled > end.saturating_add(self.source_tolerance))
    {
      self.flush(None)?;
    }
    let frontier = u64::try_from(mapping.frame(settled)?.max(0))
      .map_err(|_| EngineError::protocol("Audio settled frontier overflow"))?;
    // A causal FIR needs future samples. Never read its retained tail as an invented hole.
    self.ready = frontier
      .min(self.cursor.unwrap_or(frontier))
      .max(self.buffer.next());
    Ok(())
  }
  /// Whether `packet` starts where the previous packet ended in device positions. Positions
  /// advance at the mix rate while data is 48 kHz client frames: one frame of rounding is allowed.
  fn position_continues(&self, packet: &AudioPacket) -> bool {
    self.next_position.is_some_and(|(position, rate)| {
      rate == packet.position_rate && position.abs_diff(packet.raw_device_position) <= 1
    })
  }
  /// An offset from the previous end that `normalize` absorbs by re-dating the packet onto it.
  fn absorbed(&self, offset: u64) -> bool {
    offset <= JITTER_100NS || (offset <= OUTLIER_100NS && self.outliers < MAX_OUTLIERS)
  }
  /// Whether `normalize` would re-date `packet` onto the previous packet's end.
  fn redated_onto_end(&self, packet: &AudioPacket) -> bool {
    self.source_end.is_some_and(|previous| {
      !packet.discontinuity
        && self.position_continues(packet)
        && self.absorbed(packet.qpc_100ns.abs_diff(previous))
    })
  }
  fn pop(&mut self) -> Result<AudioPacket> {
    if self.queue.is_empty() {
      return Err(EngineError::protocol("Audio reserve is empty"));
    }
    // No allocation, and capacity remains fixed.
    let packet = self.queue.remove(0);
    self.queued_frames -= packet.data.len() / 2;
    Ok(packet)
  }
  fn normalize(&mut self, mut packet: AudioPacket, cut: Option<u64>) -> Result<()> {
    let mapping = self
      .mapping
      .ok_or_else(|| EngineError::protocol("No PCM mapping"))?;
    let frames = packet.data.len() / 2;
    let continues = self.position_continues(&packet);
    let mut realign = false;
    if let Some(previous) = self.source_end
      && !packet.discontinuity
      && continues
    {
      let offset = packet.qpc_100ns.abs_diff(previous);
      if offset <= SECOND.div_ceil(CLIENT_RATE) {
        self.outliers = 0;
      } else if self.absorbed(offset) {
        self.outliers = if offset <= JITTER_100NS {
          0
        } else {
          self.outliers + 1
        };
        packet.qpc_100ns = previous;
        self.diagnostics.jitter_redated_packets =
          self.diagnostics.jitter_redated_packets.saturating_add(1);
      } else {
        self.outliers = 0;
        realign = true;
        self.diagnostics.clock_realignments = self.diagnostics.clock_realignments.saturating_add(1);
      }
    }
    let next_position = time::scale(frames as u64, u64::from(packet.position_rate), CLIENT_RATE)
      .ok()
      .and_then(|advance| packet.raw_device_position.checked_add(advance))
      .map(|position| (position, packet.position_rate));
    let rate = CLIENT_RATE as f64 * (1.0 + packet.drift_ppm / 1_000_000.0);
    let source_time = |frames: usize| (frames as f64 * SECOND as f64 / rate).round() as u64;
    let mut keep = frames;
    if let Some(cut) = cut {
      keep = keep
        .min((cut.saturating_sub(packet.qpc_100ns) as f64 * rate / SECOND as f64).ceil() as usize);
    }
    let first = mapping.source_frame(packet.qpc_100ns)?;
    let mut skip = usize::try_from((i128::from(self.buffer.next()) - first).max(0))
      .unwrap_or(usize::MAX)
      .min(keep);
    if packet.qpc_100ns < mapping.qpc {
      let preroll =
        ((mapping.qpc - packet.qpc_100ns) as f64 * rate / SECOND as f64).ceil() as usize;
      skip = skip.max(preroll.min(keep));
      self.diagnostics.discarded_unmapped_frames = self
        .diagnostics
        .discarded_unmapped_frames
        .saturating_add(skip as u64);
    } else if skip > 0 {
      self.diagnostics.late_packets = self.diagnostics.late_packets.saturating_add(1);
      self.diagnostics.late_frames = self.diagnostics.late_frames.saturating_add(skip as u64);
    }
    if skip >= keep {
      return Ok(());
    }
    let start = packet
      .qpc_100ns
      .checked_add(source_time(skip))
      .ok_or_else(|| EngineError::protocol("Audio packet start overflow"))?;
    let end = packet
      .qpc_100ns
      .checked_add(source_time(keep))
      .ok_or_else(|| EngineError::protocol("Audio packet end overflow"))?;
    let tolerance =
      SECOND.div_ceil(u64::from(packet.device_rate)) * 2 + SECOND.div_ceil(CLIENT_RATE);
    if let Some(previous) = self.source_end {
      let tolerance = tolerance.max(self.source_tolerance);
      if (realign || packet.discontinuity) && start.saturating_add(tolerance) < previous {
        // The same samples dated earlier, or a flagged glitch whose own date is authoritative
        // (a short 99-frame VM packet 12.9 ms early): re-anchor and trim what was
        // already placed. Only an unflagged, non-contiguous overlap is a protocol violation.
        self.flush(None)?;
      } else if start.saturating_add(tolerance) < previous {
        self.diagnostics.overlaps = self.diagnostics.overlaps.saturating_add(1);
        return Err(EngineError::protocol(format!(
          "Overlapping dated PCM packets after reordering: overlap={}us, tolerance={}us, position={}, expected={:?}, frames={frames}, qpc={}, flags={}",
          (previous - start) / 10,
          tolerance / 10,
          packet.raw_device_position,
          self.next_position,
          packet.qpc_100ns,
          packet.wasapi_flags
        )));
      }
      if start > previous.saturating_add(tolerance) {
        self.flush(None)?;
        self.diagnostics.gaps = self.diagnostics.gaps.saturating_add(1);
      }
    }
    if packet.discontinuity {
      self.flush(None)?;
      self.diagnostics.discontinuities = self.diagnostics.discontinuities.saturating_add(1);
    }
    if self.cursor.is_none() {
      let anchor = u64::try_from(
        mapping
          .source_frame(start)?
          .max(i128::from(self.buffer.next())),
      )
      .map_err(|_| EngineError::protocol("Audio cursor overflow"))?;
      // A packet dated within tolerance before the previous end keeps its own date; the frames
      // that would land on already placed samples are dropped and counted, never shifted.
      let floor = self.placed_end.unwrap_or(0);
      let trim = usize::try_from(floor.saturating_sub(anchor))
        .unwrap_or(usize::MAX)
        .min(keep - skip);
      self.diagnostics.overlap_trimmed_frames = self
        .diagnostics
        .overlap_trimmed_frames
        .saturating_add(trim as u64);
      skip += trim;
      if skip >= keep {
        self.source_end = Some(end);
        self.next_position = next_position.filter(|_| keep == frames);
        self.source_tolerance = tolerance;
        return Ok(());
      }
      self.cursor = Some(anchor.max(floor));
    }
    self.resampler.set_drift(packet.drift_ppm)?;
    if packet.silent {
      packet.data.fill(0.0);
    }
    self.diagnostics.drift_ppm = packet.drift_ppm;
    self.diagnostics.source_silent_frames =
      self
        .diagnostics
        .source_silent_frames
        .saturating_add(if packet.silent {
          (keep - skip) as u64
        } else {
          0
        });
    self.resampled.clear();
    self
      .resampler
      .feed(&packet.data[skip * 2..keep * 2], &mut self.resampled)?;
    self.place_generated(None)?;
    self.source_end = Some(end);
    self.next_position = next_position.filter(|_| keep == frames);
    self.source_tolerance = tolerance;
    Ok(())
  }
  fn place_generated(&mut self, limit: Option<u64>) -> Result<()> {
    let cursor = self
      .cursor
      .ok_or_else(|| EngineError::protocol("No DSP cursor"))?;
    let frames = self.resampled.len() / 2;
    let keep = limit.map_or(frames, |end| {
      frames.min(end.saturating_sub(cursor) as usize)
    });
    if limit.is_some() {
      self.diagnostics.resampler_tail_trimmed_frames = self
        .diagnostics
        .resampler_tail_trimmed_frames
        .saturating_add((frames - keep) as u64);
    }
    self.buffer.place(cursor, &self.resampled[..keep * 2])?;
    let placed = cursor
      .checked_add(keep as u64)
      .ok_or_else(|| EngineError::protocol("Resampled output cursor overflow"))?;
    self.cursor = Some(placed);
    self.placed_end = Some(self.placed_end.map_or(placed, |end| end.max(placed)));
    Ok(())
  }
  fn flush(&mut self, cut: Option<u64>) -> Result<()> {
    if let (Some(end), Some(_)) = (self.source_end, self.cursor) {
      let mapping = self
        .mapping
        .ok_or_else(|| EngineError::protocol("No DSP tail mapping"))?;
      let end = u64::try_from(mapping.source_frame(end)?.max(0))
        .map_err(|_| EngineError::protocol("DSP tail extent overflow"))?;
      self.resampled.clear();
      self.resampler.finish(&mut self.resampled)?;
      let limit = cut.map_or(end, |cut| cut.min(end));
      self.place_generated(Some(limit))?;
      if let Some(cursor) = self.cursor {
        self.diagnostics.resampler_tail_shortfall_frames = self
          .diagnostics
          .resampler_tail_shortfall_frames
          .saturating_add(limit.saturating_sub(cursor));
      }
    }
    self.cursor = None;
    self.resampler.reset();
    Ok(())
  }
  /// Called after the owner collects the reorder reserve; the common cutoff is exclusive.
  pub fn cut(&mut self, media: u64) -> Result<()> {
    let target = time::sample_boundary(media, CLIENT_RATE as u32)?;
    if target < self.buffer.next() {
      return Err(EngineError::protocol(
        "Audio cutoff precedes committed samples",
      ));
    }
    let Some(mapping) = self.mapping else {
      return if target == self.buffer.next() {
        Ok(())
      } else {
        Err(EngineError::protocol(
          "Unmapped audio cutoff differs from committed samples",
        ))
      };
    };
    let cut = mapping.cut(media)?;
    while self
      .queue
      .first()
      .is_some_and(|packet| packet.qpc_100ns < cut)
    {
      let packet = self.pop()?;
      self.normalize(packet, Some(cut))?;
      // Only a packet the cut shortened leaves no next position. Everything queued behind it
      // follows it in the endpoint's own order, so it is past the cut whatever its noisy date.
      if self.source_end.is_some() && self.next_position.is_none() {
        break;
      }
    }
    self.flush(Some(target))?;
    self.mapping = None;
    self.clear_source();
    self.ready = target;
    Ok(())
  }
  pub fn synthetic_frontier(&mut self, media: u64) -> Result<()> {
    if self.mapping.is_none() || !self.queue.is_empty() || self.cursor.is_some() {
      return Err(EngineError::protocol(
        "Synthetic frontier requires an active packet-free source",
      ));
    }
    self.ready = time::sample_boundary(media, CLIENT_RATE as u32)?;
    Ok(())
  }
  pub fn next(&self) -> u64 {
    self.buffer.next()
  }
  pub fn read(&mut self, media: u64, output: &mut [i16]) -> Result<usize> {
    let requested = time::sample_boundary(media, CLIENT_RATE as u32)?;
    self.buffer.read(requested.min(self.ready), output)
  }
  /// The frame up to which `read` could return samples for a request until `media`.
  pub fn available(&self, media: u64) -> Result<u64> {
    let requested = time::sample_boundary(media, CLIENT_RATE as u32)?;
    Ok(requested.min(self.ready).max(self.buffer.next()))
  }
  /// Float frames up to frame `until` (never past what is settled), for mixing sources.
  pub fn read_float(&mut self, until: u64, output: &mut [f32]) -> Result<usize> {
    self.buffer.read_float(until.min(self.ready), output)
  }
  pub fn discard_future(&mut self) {
    self.buffer.discard_future();
    self.placed_end = None;
  }
  pub fn diagnostics(&self) -> AudioDiagnostics {
    let mut diagnostics = self.diagnostics;
    diagnostics.queued_packets = self.queue.len();
    diagnostics.queued_frames = self.queued_frames;
    diagnostics.inserted_silence_frames = self.buffer.silence;
    diagnostics.late_frames = diagnostics.late_frames.saturating_add(self.buffer.late);
    diagnostics
  }
}

#[cfg(test)]
mod tests;
