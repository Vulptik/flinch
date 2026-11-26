//! Bounded-memory ISO BMFF inspection. Structural validation is not decoding.
use crate::{EngineError, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::{
  collections::BTreeMap,
  fs::{File, OpenOptions},
  io::{Read, Seek, SeekFrom, Write},
  path::Path,
};

const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_FRAGMENT_SAMPLES: u32 = 1_000_000;
/// Sample starts an [`inspect_timeline`] keeps across all tracks (8 bytes each, 32 MiB): six
/// hours at 120 FPS plus its audio packets.
const MAX_TIMELINE_SAMPLES: usize = 4_000_000;
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct RangeStatistics {
  pub count: u64,
  pub minimum: u64,
  pub maximum: u64,
  pub total: u128,
}
impl RangeStatistics {
  fn observe(&mut self, value: u64) -> Result<()> {
    let count = self
      .count
      .checked_add(1)
      .ok_or_else(|| malformed("Fragment statistics count overflow"))?;
    let total = self
      .total
      .checked_add(u128::from(value))
      .ok_or_else(|| malformed("Fragment statistics sum overflow"))?;
    self.minimum = if self.count == 0 {
      value
    } else {
      self.minimum.min(value)
    };
    self.maximum = self.maximum.max(value);
    self.count = count;
    self.total = total;
    Ok(())
  }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct FragmentStatistics {
  pub media_span_100ns: RangeStatistics,
  pub frontier_advance_100ns: RangeStatistics,
  pub bytes: RangeStatistics,
}
fn reserve_samples(total: &mut u32, count: u32) -> Result<()> {
  *total = total
    .checked_add(count)
    .filter(|n| *n <= MAX_FRAGMENT_SAMPLES)
    .ok_or_else(|| malformed("Fragment sample budget exceeded"))?;
  Ok(())
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Inspection {
  pub bytes: u64,
  pub valid_prefix: u64,
  pub fragments: u64,
  pub duration_100ns: u64,
  pub tracks: usize,
  pub truncated: bool,
  pub decoded: bool,
  #[serde(default)]
  pub damage_reason: Option<String>,
  #[serde(default)]
  pub fragment_statistics: FragmentStatistics,
  #[serde(default)]
  pub track_media: Vec<TrackMedia>,
  /// Fragments a Windows MPEG-4 reader would stop at (see [`crate::interop`]).
  #[serde(default)]
  pub chunk_straddles: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrackMedia {
  pub id: u32,
  pub handler: Option<[u8; 4]>,
  pub codec: Option<[u8; 4]>,
  pub samples: u64,
  pub start_100ns: Option<u64>,
  pub end_100ns: u64,
  /// Ticks per second of the track's media timeline.
  #[serde(default)]
  pub timescale: u32,
  /// Presentation start of every sample in `timescale` ticks, in container order. Kept only
  /// by [`inspect_timeline`]; never serialized, so reports stay small.
  #[serde(skip)]
  pub sample_starts: Vec<u64>,
}
impl Inspection {
  /// Each compressed native packet is submitted as one container sample.
  /// Call after validate_recording has checked track identities and cardinality.
  pub fn validate_packet_counts(&self, video: u64, audio: u64) -> Result<()> {
    for track in &self.track_media {
      let expected = match track.handler {
        Some(handler) if &handler == b"vide" => video,
        Some(handler) if &handler == b"soun" => audio,
        _ => {
          return Err(malformed(
            "Cannot account packets for an unknown track handler",
          ));
        },
      };
      if track.samples != expected {
        return Err(malformed(&format!(
          "Final track {} retained {} samples from {expected} submitted packets",
          track.id, track.samples
        )));
      }
    }
    Ok(())
  }
  /// Checks the requested native recording contract; still does not decode sample payloads.
  pub fn validate_recording(&self, audio: bool, fps: u32, cutoff: u64) -> Result<()> {
    if self.truncated
      || self.fragments == 0
      || cutoff == 0
      || self.tracks != self.track_media.len()
      || self.track_media.len() != if audio { 2 } else { 1 }
    {
      return Err(malformed(&format!(
        "Final recording contract failed: truncated={}, fragments={}, cutoff={cutoff}, tracks={}, observed={}, expected={}",
        self.truncated,
        self.fragments,
        self.tracks,
        self.track_media.len(),
        if audio { 2 } else { 1 }
      )));
    }
    let video_tolerance = crate::time::frame_time(1, fps)? + 1;
    let mut video_tracks = 0;
    let mut audio_tracks = 0;
    for track in &self.track_media {
      let tolerance = match (track.handler, track.codec) {
        (Some(handler), Some(codec)) if &handler == b"vide" && &codec == b"avc1" => {
          video_tracks += 1;
          video_tolerance
        },
        (Some(handler), Some(codec)) if audio && &handler == b"soun" && &codec == b"mp4a" => {
          audio_tracks += 1;
          video_tolerance + crate::time::scale(1024, crate::time::SECOND, 48000)? + 1
        },
        _ => {
          return Err(malformed(&format!(
            "Unexpected final track {}: handler={:?}, codec={:?}, audio_requested={audio}",
            track.id, track.handler, track.codec
          )));
        },
      };
      if track.samples == 0
        || track.start_100ns.is_none_or(|start| start > tolerance)
        || track.end_100ns.abs_diff(cutoff) > tolerance
      {
        return Err(malformed(&format!(
          "Final track {} misses recording boundary: samples={}, start={:?}, end={}, cutoff={cutoff}, tolerance={tolerance} (100 ns)",
          track.id, track.samples, track.start_100ns, track.end_100ns
        )));
      }
    }
    if video_tracks != 1 || audio_tracks != usize::from(audio) {
      return Err(malformed(
        "Final recording requires exactly one video and the requested audio track",
      ));
    }
    Ok(())
  }
}
struct FragmentTimes {
  start: u64,
  end: u64,
  /// (track, presentation start in track ticks) per sample, when the caller keeps a timeline.
  samples: Vec<(u32, u64)>,
}
#[derive(Clone, Copy)]
struct Header {
  start: u64,
  size: u64,
  header: u64,
  kind: [u8; 4],
}
fn malformed(reason: &str) -> EngineError {
  EngineError::new(ErrorKind::Io, "mp4 structure", reason)
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
  let slice = bytes
    .get(at..at.saturating_add(4))
    .ok_or_else(|| malformed("Truncated box field"))?;
  Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}
fn u64_at(bytes: &[u8], at: usize) -> Result<u64> {
  Ok((u64::from(u32_at(bytes, at)?) << 32) | u64::from(u32_at(bytes, at + 4)?))
}
fn header(reader: &mut File, at: u64, length: u64) -> Result<Option<Header>> {
  if length.saturating_sub(at) < 8 {
    return Ok(None);
  }
  reader
    .seek(SeekFrom::Start(at))
    .map_err(|e| EngineError::io("mp4 seek", e))?;
  let mut bytes = [0u8; 16];
  reader
    .read_exact(&mut bytes[..8])
    .map_err(|e| EngineError::io("mp4 header", e))?;
  let size = u32_at(&bytes, 0)?;
  let kind = [bytes[4], bytes[5], bytes[6], bytes[7]];
  let (size, hdr) = if size == 1 {
    if length - at < 16 {
      return Ok(None);
    }
    reader
      .read_exact(&mut bytes[8..])
      .map_err(|e| EngineError::io("mp4 extended header", e))?;
    (u64_at(&bytes, 8)?, 16)
  } else if size == 0 {
    return Ok(None);
  } else {
    (u64::from(size), 8)
  };
  if size < hdr {
    return Err(malformed("Box smaller than its header"));
  }
  if size > length - at {
    return Ok(None);
  }
  Ok(Some(Header {
    start: at,
    size,
    header: hdr,
    kind,
  }))
}
fn children(bytes: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
  let mut offset = 0usize;
  let mut out = Vec::new();
  while offset < bytes.len() {
    let size = u32_at(bytes, offset)?;
    let kind = bytes
      .get(offset + 4..offset + 8)
      .ok_or_else(|| malformed("Truncated child"))?;
    let (size, hdr) = if size == 1 {
      (
        usize::try_from(u64_at(bytes, offset + 8)?).map_err(|_| malformed("Box overflow"))?,
        16,
      )
    } else {
      (size as usize, 8)
    };
    if size < hdr || size > bytes.len() - offset {
      return Err(malformed("Invalid child extent"));
    }
    out.push((
      [kind[0], kind[1], kind[2], kind[3]],
      &bytes[offset + hdr..offset + size],
    ));
    offset += size;
    if out.len() > 65_536 {
      return Err(malformed("Too many metadata boxes"));
    }
  }
  Ok(out)
}
#[derive(Clone, Default)]
struct TrackInfo {
  scale: u32,
  duration: u32,
  size: u32,
  next: u64,
  handler: Option<[u8; 4]>,
  codec: Option<[u8; 4]>,
  samples: u64,
  first: Option<u64>,
}
fn sample_codec(minf: &[u8]) -> Result<Option<[u8; 4]>> {
  for (kind, data) in children(minf)? {
    if &kind != b"stbl" {
      continue;
    }
    for (kind, data) in children(data)? {
      if &kind != b"stsd" {
        continue;
      }
      let count = u32_at(data, 4)?;
      let entries = children(
        data
          .get(8..)
          .ok_or_else(|| malformed("Truncated sample description"))?,
      )?;
      if count != 1 || entries.len() != 1 {
        return Err(malformed("Expected one fixed sample description per track"));
      }
      return Ok(Some(entries[0].0));
    }
  }
  Ok(None)
}
fn movie(bytes: &[u8]) -> Result<BTreeMap<u32, TrackInfo>> {
  let mut tracks = BTreeMap::new();
  let mut defaults = Vec::new();
  for (kind, data) in children(bytes)? {
    if &kind == b"trak" {
      let mut id = None;
      let mut scale = None;
      let mut handler = None;
      let mut codec = None;
      for (kind, data) in children(data)? {
        if &kind == b"tkhd" {
          id = Some(u32_at(
            data,
            if data.first() == Some(&1) { 20 } else { 12 },
          )?);
        }
        if &kind == b"mdia" {
          for (kind, data) in children(data)? {
            if &kind == b"mdhd" {
              scale = Some(u32_at(
                data,
                if data.first() == Some(&1) { 20 } else { 12 },
              )?);
            }
            if &kind == b"hdlr" {
              handler = Some(u32_at(data, 8)?.to_be_bytes());
            }
            if &kind == b"minf" {
              codec = sample_codec(data)?;
            }
          }
        }
      }
      let id = id.ok_or_else(|| malformed("Track has no id"))?;
      let scale = scale
        .filter(|n| *n > 0)
        .ok_or_else(|| malformed("Track has no valid timebase"))?;
      if tracks
        .insert(
          id,
          TrackInfo {
            scale,
            handler,
            codec,
            ..Default::default()
          },
        )
        .is_some()
      {
        return Err(malformed("Duplicate track id"));
      }
    }
    if &kind == b"mvex" {
      for (kind, data) in children(data)? {
        if &kind == b"trex" {
          defaults.push((u32_at(data, 4)?, u32_at(data, 12)?, u32_at(data, 16)?));
        }
      }
    }
  }
  for (id, duration, size) in defaults {
    if let Some(info) = tracks.get_mut(&id) {
      info.duration = duration;
      info.size = size;
    }
  }
  if tracks.is_empty() || tracks.len() > 2 {
    return Err(malformed(
      "Expected one video track and at most one audio track",
    ));
  }
  Ok(tracks)
}
fn fragment(
  bytes: &[u8],
  tracks: &mut BTreeMap<u32, TrackInfo>,
  moof: u64,
  mdat: Header,
  timeline: bool,
) -> Result<FragmentTimes> {
  let mut end = 0;
  let mut start = u64::MAX;
  let mut trafs = 0;
  let mut samples = 0;
  let mut starts = Vec::new();
  for (kind, data) in children(bytes)? {
    if &kind != b"traf" {
      continue;
    }
    trafs += 1;
    let boxes = children(data)?;
    let tfhd = boxes
      .iter()
      .find(|(k, _)| k == b"tfhd")
      .ok_or_else(|| malformed("Fragment has no track header"))?
      .1;
    let flags = u32_at(tfhd, 0)? & 0xffffff;
    let id = u32_at(tfhd, 4)?;
    let info = tracks
      .get_mut(&id)
      .ok_or_else(|| malformed("Unknown fragment track"))?;
    let mut p = 8;
    let base = if flags & 1 != 0 {
      let value = u64_at(tfhd, p)?;
      p += 8;
      value
    } else {
      moof
    };
    if flags & 2 != 0 {
      p += 4;
    }
    let duration = if flags & 8 != 0 {
      let n = u32_at(tfhd, p)?;
      p += 4;
      n
    } else {
      info.duration
    };
    let size = if flags & 16 != 0 {
      u32_at(tfhd, p)?
    } else {
      info.size
    };
    let mut time = if let Some((_, tfdt)) = boxes.iter().find(|(k, _)| k == b"tfdt") {
      if tfdt.first() == Some(&1) {
        u64_at(tfdt, 4)?
      } else {
        u64::from(u32_at(tfdt, 4)?)
      }
    } else {
      info.next
    };
    if time < info.next {
      return Err(malformed("Fragment decode time moved backwards"));
    }
    start = start.min(crate::time::scale(
      time,
      crate::time::SECOND,
      u64::from(info.scale),
    )?);
    let mut position = mdat.start + mdat.header;
    for (kind, data) in boxes {
      if &kind != b"trun" {
        continue;
      }
      let flags = u32_at(data, 0)? & 0xffffff;
      let count = u32_at(data, 4)?;
      let mut p = 8;
      reserve_samples(&mut samples, count)?;
      if flags & 1 != 0 {
        let delta = u32_at(data, p)? as i32;
        p += 4;
        position = base
          .checked_add_signed(i64::from(delta))
          .ok_or_else(|| malformed("Sample offset overflow"))?;
      }
      if flags & 4 != 0 {
        p += 4;
      }
      let fields = ((flags >> 8) & 0xf).count_ones() as usize;
      let required = (count as usize)
        .checked_mul(fields * 4)
        .and_then(|bytes| p.checked_add(bytes))
        .ok_or_else(|| malformed("Sample table extent overflow"))?;
      if required > data.len() {
        return Err(malformed("Truncated sample table"));
      }
      for _ in 0..count {
        let d = if flags & 0x100 != 0 {
          let n = u32_at(data, p)?;
          p += 4;
          n
        } else {
          duration
        };
        let s = if flags & 0x200 != 0 {
          let n = u32_at(data, p)?;
          p += 4;
          n
        } else {
          size
        };
        if flags & 0x400 != 0 {
          u32_at(data, p)?;
          p += 4;
        }
        // Composition offset: unsigned in version 0, signed in version 1.
        let offset = if flags & 0x800 != 0 {
          let n = u32_at(data, p)?;
          p += 4;
          if data.first() == Some(&1) {
            i64::from(n as i32)
          } else {
            i64::from(n)
          }
        } else {
          0
        };
        if d == 0 || s == 0 || position < mdat.start + mdat.header {
          return Err(malformed("Invalid sample duration, size or position"));
        }
        position = position
          .checked_add(u64::from(s))
          .ok_or_else(|| malformed("Sample size overflow"))?;
        if position > mdat.start + mdat.size {
          return Err(malformed("Sample extends beyond complete mdat"));
        }
        info.first.get_or_insert(time);
        if timeline {
          starts.push((
            id,
            time
              .checked_add_signed(offset)
              .ok_or_else(|| malformed("Sample presentation time overflow"))?,
          ));
        }
        info.samples = info
          .samples
          .checked_add(1)
          .ok_or_else(|| malformed("Track sample count overflow"))?;
        time = time
          .checked_add(u64::from(d))
          .ok_or_else(|| malformed("Decode time overflow"))?;
      }
      if p > data.len() {
        return Err(malformed("Truncated sample table"));
      }
    }
    info.next = time;
    end = end.max(crate::time::scale(
      time,
      crate::time::SECOND,
      u64::from(info.scale),
    )?);
  }
  if trafs == 0 || samples == 0 {
    return Err(malformed("Empty fragment"));
  }
  Ok(FragmentTimes {
    start,
    end,
    samples: starts,
  })
}
fn metadata(file: &mut File, h: Header) -> Result<Vec<u8>> {
  let n = h.size - h.header;
  if n > MAX_METADATA {
    return Err(malformed("Metadata exceeds 8 MiB"));
  }
  file
    .seek(SeekFrom::Start(h.start + h.header))
    .map_err(|e| EngineError::io("metadata seek", e))?;
  let mut bytes = vec![0; n as usize];
  file
    .read_exact(&mut bytes)
    .map_err(|e| EngineError::io("metadata read", e))?;
  Ok(bytes)
}
pub fn inspect(path: &Path) -> Result<Inspection> {
  inspect_with(path, false)
}
/// [`inspect`], also keeping every retained sample's presentation start, so a verifier can date
/// decoded output by its container position instead of a decoder's own timestamps.
pub fn inspect_timeline(path: &Path) -> Result<Inspection> {
  inspect_with(path, true)
}
fn inspect_with(path: &Path, timeline: bool) -> Result<Inspection> {
  let mut starts: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
  let mut kept = 0usize;
  let mut file = File::open(path).map_err(|e| EngineError::io("inspect open", e))?;
  let bytes = file
    .metadata()
    .map_err(|e| EngineError::io("inspect metadata", e))?
    .len();
  let mut out = Inspection {
    bytes,
    ..Default::default()
  };
  let mut at = 0;
  let mut ftyp = false;
  let mut tracks = None;
  while at < bytes {
    let Some(h) = header(&mut file, at, bytes)? else {
      out.truncated = true;
      out.damage_reason = Some(format!("Incomplete box at byte {at}"));
      break;
    };
    match &h.kind {
      b"ftyp" if at == 0 => {
        ftyp = true;
      },
      b"moov" if ftyp && tracks.is_none() => {
        let parsed = movie(&metadata(&mut file, h)?)?;
        out.tracks = parsed.len();
        tracks = Some(parsed);
      },
      b"moof" => {
        let tracks = tracks
          .as_mut()
          .ok_or_else(|| malformed("Fragment precedes initialization"))?;
        let next = at + h.size;
        let Some(mdat) = header(&mut file, next, bytes)? else {
          out.truncated = true;
          out.damage_reason = Some(format!("Incomplete media data at byte {next}"));
          break;
        };
        if &mdat.kind != b"mdat" {
          // Damage after verified fragments keeps them recoverable; publication still refuses.
          if out.fragments > 0 {
            out.truncated = true;
            out.damage_reason = Some(format!("Expected mdat after moof at byte {next}"));
            break;
          }
          return Err(malformed("Expected mdat after moof"));
        }
        if crate::interop::straddles(h.start, mdat.start + mdat.header) {
          out.chunk_straddles += 1;
        }
        // A damaged later track must not advance the summaries of the retained prefix.
        let mut candidate = tracks.clone();
        match fragment(
          &metadata(&mut file, h)?,
          &mut candidate,
          h.start,
          mdat,
          timeline,
        ) {
          Ok(times) => {
            kept = kept
              .checked_add(times.samples.len())
              .filter(|kept| *kept <= MAX_TIMELINE_SAMPLES)
              .ok_or_else(|| malformed("Sample timeline exceeds its budget"))?;
            for (id, start) in &times.samples {
              starts.entry(*id).or_default().push(*start);
            }
            *tracks = candidate;
            let frontier = out.duration_100ns.max(times.end);
            out.fragment_statistics.media_span_100ns.observe(
              times
                .end
                .checked_sub(times.start)
                .ok_or_else(|| malformed("Fragment media range reversed"))?,
            )?;
            out
              .fragment_statistics
              .frontier_advance_100ns
              .observe(frontier - out.duration_100ns)?;
            out.fragment_statistics.bytes.observe(h.size + mdat.size)?;
            out.duration_100ns = frontier;
            out.fragments += 1;
            at = mdat.start + mdat.size;
            out.valid_prefix = at;
            continue;
          },
          Err(error) if out.fragments > 0 => {
            out.truncated = true;
            out.damage_reason = Some(error.to_string());
            break;
          },
          Err(error) => return Err(error),
        }
      },
      b"free" | b"skip" | b"sidx" | b"mfra" | b"styp" | b"uuid" | b"pdin" => {},
      _ if out.fragments > 0 => {
        out.truncated = true;
        out.damage_reason = Some(format!(
          "Unexpected top-level box {:?} at byte {at}",
          h.kind
        ));
        break;
      },
      _ => return Err(malformed(&format!("Unexpected top-level box {:?}", h.kind))),
    }
    at += h.size;
  }
  if !ftyp || tracks.is_none() || out.fragments == 0 {
    return Err(malformed("No recoverable initialized fragments"));
  }
  if let Some(tracks) = tracks {
    for (id, track) in tracks {
      out.track_media.push(TrackMedia {
        id,
        handler: track.handler,
        codec: track.codec,
        samples: track.samples,
        start_100ns: track
          .first
          .map(|first| crate::time::scale(first, crate::time::SECOND, u64::from(track.scale)))
          .transpose()?,
        end_100ns: crate::time::scale(track.next, crate::time::SECOND, u64::from(track.scale))?,
        timescale: track.scale,
        sample_starts: starts.remove(&id).unwrap_or_default(),
      });
    }
  }
  Ok(out)
}
pub fn recover(input: &Path, output: &Path) -> Result<Inspection> {
  let report = inspect(input)?;
  let source = File::open(input).map_err(|e| EngineError::io("recover open", e))?;
  let mut destination = OpenOptions::new()
    .create_new(true)
    .write(true)
    .open(output)
    .map_err(|e| EngineError::io("recover destination", e))?;
  let copied = std::io::copy(&mut source.take(report.valid_prefix), &mut destination)
    .map_err(|e| EngineError::io("recover copy", e))?;
  if copied != report.valid_prefix {
    return Err(malformed("Source changed during recovery"));
  }
  destination
    .flush()
    .and_then(|_| destination.sync_all())
    .map_err(|e| EngineError::io("recover flush", e))?;
  drop(destination);
  // The recovered prefix must play to its end in Windows players too.
  crate::interop::realign(output, || Ok(()))?;
  Ok(report)
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn damaged_fragment_cannot_advance_retained_track_counts_or_endpoints() {
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("partial.mp4");
    let good = fixture();
    let moof = good
      .windows(4)
      .position(|bytes| bytes == b"moof")
      .expect("fragment")
      - 4;
    let mut damaged_fragment = good[moof..].to_vec();
    let tfdt = damaged_fragment
      .windows(4)
      .position(|bytes| bytes == b"tfdt")
      .expect("decode time");
    damaged_fragment[tfdt + 8..tfdt + 12].copy_from_slice(&1u32.to_be_bytes());
    let trun = damaged_fragment
      .windows(4)
      .position(|bytes| bytes == b"trun")
      .expect("sample run");
    // First sample is complete; the second extends beyond mdat after mutating track accounting.
    damaged_fragment[trun + 8..trun + 12].copy_from_slice(&2u32.to_be_bytes());
    let mut bytes = good.clone();
    bytes.extend(damaged_fragment);
    std::fs::write(&path, bytes).expect("damaged second fragment");
    let report = inspect(&path).expect("recoverable first fragment");
    assert!(report.truncated);
    assert_eq!(report.valid_prefix, good.len() as u64);
    assert_eq!(report.track_media[0].samples, 1);
    assert_eq!(report.track_media[0].start_100ns, Some(0));
    assert_eq!(report.track_media[0].end_100ns, crate::time::SECOND / 60);
  }
  #[test]
  fn publication_requires_populated_tracks_near_both_committed_boundaries() {
    let cutoff = crate::time::SECOND;
    let valid = Inspection {
      tracks: 1,
      fragments: 1,
      track_media: vec![TrackMedia {
        id: 1,
        handler: Some(*b"vide"),
        codec: Some(*b"avc1"),
        samples: 60,
        start_100ns: Some(0),
        end_100ns: cutoff,
        ..Default::default()
      }],
      ..Default::default()
    };
    valid
      .validate_recording(false, 60, cutoff)
      .expect("valid boundary contract");
    assert!(valid.validate_recording(false, 0, cutoff).is_err());
    for changed in 0..7 {
      let mut report = valid.clone();
      match changed {
        0 => report.truncated = true,
        1 => report.track_media[0].samples = 0,
        2 => report.track_media[0].start_100ns = None,
        3 => report.track_media[0].start_100ns = Some(cutoff / 2),
        4 => report.track_media[0].end_100ns = cutoff / 2,
        5 => report.track_media[0].end_100ns = cutoff * 2,
        _ => report.track_media[0].codec = None,
      }
      assert!(
        report.validate_recording(false, 60, cutoff).is_err(),
        "case {changed}"
      );
    }
    let mut duplicate = valid.clone();
    duplicate.tracks = 2;
    duplicate.track_media.push(duplicate.track_media[0].clone());
    assert!(
      duplicate.validate_recording(true, 60, cutoff).is_err(),
      "two video tracks do not satisfy an audio request"
    );
    for data in [
      vec![],
      vec![0; 8],
      [vec![0, 0, 0, 0, 0, 0, 0, 2], atom(b"avc1", &[])].concat(),
    ] {
      assert!(sample_codec(&atom(b"stbl", &atom(b"stsd", &data))).is_err());
    }
  }
  #[test]
  fn sample_budget_covers_all_runs_and_tracks_in_a_fragment() {
    let mut total = 0;
    reserve_samples(&mut total, 600000).expect("first run");
    assert!(reserve_samples(&mut total, 600000).is_err());
    assert_eq!(total, 600000);
    reserve_samples(&mut total, 400000).expect("exact budget");
    assert!(reserve_samples(&mut total, 1).is_err());
    assert!(reserve_samples(&mut total, u32::MAX).is_err());
  }
  fn atom(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = ((data.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(data);
    out
  }
  pub(crate) fn fixture() -> Vec<u8> {
    let mut tkhd = vec![0; 16];
    tkhd[12..].copy_from_slice(&1u32.to_be_bytes());
    let mut mdhd = vec![0; 16];
    mdhd[12..].copy_from_slice(&60u32.to_be_bytes());
    let mut track = atom(b"tkhd", &tkhd);
    track.extend(atom(b"mdia", &atom(b"mdhd", &mdhd)));
    let mut tfhd = vec![0, 0, 0, 0x18];
    tfhd.extend(1u32.to_be_bytes());
    tfhd.extend(1u32.to_be_bytes());
    tfhd.extend(4u32.to_be_bytes());
    let mut traf = atom(b"tfhd", &tfhd);
    traf.extend(atom(b"tfdt", &[0; 8]));
    let mut trun = vec![0; 4];
    trun.extend(1u32.to_be_bytes());
    traf.extend(atom(b"trun", &trun));
    let mut out = atom(b"ftyp", b"isom0000");
    out.extend(atom(b"moov", &atom(b"trak", &track)));
    out.extend(atom(b"moof", &atom(b"traf", &traf)));
    out.extend(atom(b"mdat", &[1, 2, 3, 4]));
    out
  }
  #[test]
  fn retains_complete_prefix_and_never_claims_decode() {
    let t = tempfile::tempdir().expect("valid test fixture");
    let source = t.path().join("in.mp4");
    let output = t.path().join("out.mp4");
    let good = fixture();
    let mut damaged = good.clone();
    damaged.extend([0, 0, 0, 100, b'm', b'o', b'o', b'f']);
    std::fs::write(&source, damaged).expect("valid test fixture");
    let r = recover(&source, &output).expect("valid test fixture");
    assert_eq!(r.valid_prefix, good.len() as u64);
    assert_eq!(r.fragments, 1);
    assert_eq!(r.fragment_statistics.media_span_100ns.count, 1);
    assert_eq!(
      r.fragment_statistics.media_span_100ns.minimum,
      crate::time::SECOND / 60
    );
    assert_eq!(
      r.fragment_statistics.frontier_advance_100ns.total,
      u128::from(r.duration_100ns)
    );
    assert_eq!(r.fragment_statistics.bytes.count, 1);
    assert!(r.fragment_statistics.bytes.minimum > 4);
    assert!(r.truncated);
    assert!(!r.decoded);
    assert_eq!(std::fs::read(&output).expect("valid test fixture"), good);
    assert!(recover(&source, &output).is_err());
  }
  #[test]
  fn a_damaged_later_box_keeps_the_verified_prefix_recoverable() {
    let t = tempfile::tempdir().expect("temp");
    let good = fixture();
    let moof = good
      .windows(4)
      .position(|bytes| bytes == b"moof")
      .expect("moof")
      - 4;
    // A second complete fragment whose media box type is damaged, then a junk top-level box.
    let mut wrong_media = good.clone();
    let mut later = good[moof..].to_vec();
    let mdat = later.len() - 12 + 4;
    later[mdat..mdat + 4].copy_from_slice(b"xdat");
    wrong_media.extend(&later);
    let mut junk = good.clone();
    junk.extend([0, 0, 0, 16, b'j', b'u', b'n', b'k', 0, 0, 0, 0, 0, 0, 0, 0]);
    for (name, bytes) in [("wrong-media", wrong_media), ("junk", junk)] {
      let source = t.path().join(format!("{name}.mp4"));
      let output = t.path().join(format!("{name}-recovered.mp4"));
      std::fs::write(&source, bytes).expect("damaged partial");
      let inspection = inspect(&source).expect("prefix inspected");
      assert!(inspection.truncated, "{name}");
      assert!(inspection.damage_reason.is_some(), "{name}");
      let recovered = recover(&source, &output).expect("prefix recovered");
      assert_eq!(recovered.valid_prefix, good.len() as u64, "{name}");
      assert_eq!(recovered.fragments, 1, "{name}");
      assert_eq!(std::fs::read(&output).expect("recovered"), good, "{name}");
    }
    // Without a verified fragment the same damage is still malformed.
    let mut first = good[..moof].to_vec();
    let mut only = good[moof..].to_vec();
    let mdat = only.len() - 12 + 4;
    only[mdat..mdat + 4].copy_from_slice(b"xdat");
    first.extend(only);
    let source = t.path().join("no-prefix.mp4");
    std::fs::write(&source, first).expect("damaged first fragment");
    assert!(inspect(&source).is_err());
  }
  #[test]
  fn fragment_ranges_distinguish_repeated_sizes_and_empty_runs_are_not_recoverable() {
    let mut range = RangeStatistics::default();
    for value in [12, 3, 30, 7] {
      range.observe(value).expect("range observation");
    }
    assert_eq!(
      (range.count, range.minimum, range.maximum, range.total),
      (4, 3, 30, 52)
    );
    range.count = u64::MAX;
    assert!(range.observe(1).is_err());
    range.count = 1;
    range.total = u128::MAX;
    assert!(range.observe(1).is_err());
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("empty-fragment.mp4");
    let mut bytes = fixture();
    let trun = bytes
      .windows(4)
      .position(|bytes| bytes == b"trun")
      .expect("sample run");
    bytes[trun + 8..trun + 12].copy_from_slice(&0u32.to_be_bytes());
    std::fs::write(&path, bytes).expect("empty run fixture");
    assert!(
      inspect(&path)
        .expect_err("empty sample fragment")
        .reason
        .contains("Empty fragment")
    );
  }
  fn atom64(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = 1u32.to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(((data.len() + 16) as u64).to_be_bytes());
    out.extend(data);
    out
  }
  fn track(id: u32, scale: u32, handler: &[u8; 4], codec: &[u8; 4], wide: bool) -> Vec<u8> {
    let (mut tkhd, mut mdhd) = if wide {
      (vec![0; 24], vec![0; 24])
    } else {
      (vec![0; 16], vec![0; 16])
    };
    let at = if wide { 20 } else { 12 };
    tkhd[0] = u8::from(wide);
    mdhd[0] = u8::from(wide);
    tkhd[at..at + 4].copy_from_slice(&id.to_be_bytes());
    mdhd[at..at + 4].copy_from_slice(&scale.to_be_bytes());
    let mut hdlr = vec![0; 8];
    hdlr.extend(handler);
    let mut stsd = vec![0; 4];
    stsd.extend(1u32.to_be_bytes());
    stsd.extend(atom(codec, &[0; 8]));
    let mut mdia = atom(b"mdhd", &mdhd);
    mdia.extend(atom(b"hdlr", &hdlr));
    mdia.extend(atom(
      b"minf",
      &[atom(b"vmhd", &[0; 4]), atom(b"stbl", &atom(b"stsd", &stsd))].concat(),
    ));
    let mut trak = atom(b"tkhd", &tkhd);
    trak.extend(atom(b"mdia", &mdia));
    trak
  }
  fn trex(id: u32, duration: u32, size: u32) -> Vec<u8> {
    let mut data = vec![0; 4];
    for value in [id, 1, duration, size, 0] {
      data.extend(value.to_be_bytes());
    }
    atom(b"trex", &data)
  }
  /// One video track using explicit base offsets and every per-sample trun field, one audio
  /// track using trex defaults and a 64-bit decode time; 64-bit box sizes at file and child level.
  fn two_track_moof(moof_at: u64, data_offset: i32, video_time: u64) -> Vec<u8> {
    let mut tfhd = vec![0, 0, 0, 0x03];
    tfhd.extend(1u32.to_be_bytes());
    tfhd.extend(moof_at.to_be_bytes());
    tfhd.extend(1u32.to_be_bytes());
    let mut trun = vec![0, 0, 0x0f, 0x05];
    trun.extend(2u32.to_be_bytes());
    trun.extend(data_offset.to_be_bytes());
    trun.extend(0u32.to_be_bytes());
    for _ in 0..2 {
      for value in [1u32, 4, 0, 0] {
        trun.extend(value.to_be_bytes());
      }
    }
    let mut tfdt = vec![0; 4];
    tfdt.extend((video_time as u32).to_be_bytes());
    let mut video = atom(b"tfhd", &tfhd);
    video.extend(atom(b"tfdt", &tfdt));
    video.extend(atom(b"trun", &trun));
    let mut audio_tfhd = vec![0; 4];
    audio_tfhd.extend(2u32.to_be_bytes());
    let mut audio_tfdt = vec![1, 0, 0, 0];
    audio_tfdt.extend((video_time * 800).to_be_bytes());
    let mut audio_trun = vec![0; 4];
    audio_trun.extend(1u32.to_be_bytes());
    let mut audio = atom(b"tfhd", &audio_tfhd);
    audio.extend(atom(b"tfdt", &audio_tfdt));
    audio.extend(atom(b"trun", &audio_trun));
    atom(
      b"moof",
      &[
        atom(b"mfhd", &[0; 8]),
        atom(b"traf", &video),
        atom(b"traf", &audio),
      ]
      .concat(),
    )
  }
  fn two_track_file() -> Vec<u8> {
    let mut moov = atom64(b"trak", &track(1, 60, b"vide", b"avc1", true));
    moov.extend(atom(b"trak", &track(2, 48000, b"soun", b"mp4a", false)));
    moov.extend(atom(b"mvex", &[trex(1, 1, 4), trex(2, 1024, 4)].concat()));
    let mut out = atom64(b"ftyp", b"isom0000");
    out.extend(atom(b"moov", &moov));
    out.extend(atom(b"free", &[0; 4]));
    let moof_at = out.len() as u64;
    let size = two_track_moof(moof_at, 0, 0).len() as i32;
    out.extend(two_track_moof(moof_at, size + 8, 0));
    out.extend(atom(b"mdat", &[7; 8]));
    out
  }
  #[test]
  fn only_a_timeline_inspection_keeps_sample_presentation_starts() {
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("timeline.mp4");
    let mut bytes = two_track_file();
    std::fs::write(&path, &bytes).expect("fixture");
    let plain = inspect(&path).expect("plain");
    assert!(plain.track_media.iter().all(|t| t.sample_starts.is_empty()));
    assert_eq!(
      (
        plain.track_media[0].timescale,
        plain.track_media[1].timescale
      ),
      (60, 48000)
    );
    let timeline = inspect_timeline(&path).expect("timeline");
    assert_eq!(timeline.track_media[0].sample_starts, [0, 1]);
    assert_eq!(timeline.track_media[1].sample_starts, [0]);
    // Starts never reach a serialized report.
    let json = serde_json::to_string(&timeline).expect("json");
    assert!(!json.contains("sample_starts"));
    // A composition offset moves a presentation start: unsigned in version 0, signed in 1.
    let trun = bytes
      .windows(4)
      .position(|kind| kind == b"trun")
      .expect("video trun")
      + 4;
    let second_offset = trun + 16 + 16 + 12;
    bytes[second_offset..second_offset + 4].copy_from_slice(&5u32.to_be_bytes());
    std::fs::write(&path, &bytes).expect("offset");
    let offset = inspect_timeline(&path).expect("version 0 offset");
    assert_eq!(offset.track_media[0].sample_starts, [0, 6]);
    bytes[trun] = 1;
    bytes[second_offset..second_offset + 4].copy_from_slice(&(-1i32).to_be_bytes());
    std::fs::write(&path, &bytes).expect("signed offset");
    let signed = inspect_timeline(&path).expect("version 1 offset");
    assert_eq!(signed.track_media[0].sample_starts, [0, 0]);
  }
  #[test]
  fn two_track_fragments_honor_extended_sizes_defaults_and_every_sample_field() {
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("two-track.mp4");
    let good = two_track_file();
    std::fs::write(&path, &good).expect("fixture");
    let report = inspect(&path).expect("complete two-track fragment");
    assert_eq!((report.tracks, report.fragments), (2, 1));
    assert!(!report.truncated && !report.decoded);
    assert_eq!(report.valid_prefix, good.len() as u64);
    let video = &report.track_media[0];
    assert_eq!(
      (video.handler, video.codec),
      (Some(*b"vide"), Some(*b"avc1"))
    );
    assert_eq!((video.samples, video.start_100ns), (2, Some(0)));
    assert_eq!(video.end_100ns, crate::time::SECOND / 30);
    let audio = &report.track_media[1];
    assert_eq!(
      (audio.handler, audio.codec),
      (Some(*b"soun"), Some(*b"mp4a"))
    );
    assert_eq!(audio.samples, 1);
    assert_eq!(
      audio.end_100ns,
      crate::time::scale(1024, crate::time::SECOND, 48000).expect("AAC frame")
    );
    report
      .validate_packet_counts(2, 1)
      .expect("conserved packets");
    assert!(report.validate_packet_counts(3, 1).is_err());
    assert!(report.validate_packet_counts(2, 0).is_err());
    let mut unknown = report.clone();
    unknown.track_media[1].handler = Some(*b"text");
    assert!(
      unknown
        .validate_packet_counts(2, 1)
        .expect_err("unknown handler")
        .reason
        .contains("unknown track handler")
    );
    // A later fragment that rewinds decode time is damage after the retained prefix.
    let mut rewound = good.clone();
    let second = rewound.len() as u64;
    let size = two_track_moof(second, 0, 0).len() as i32;
    rewound.extend(two_track_moof(second, size + 8, 1));
    rewound.extend(atom(b"mdat", &[7; 8]));
    std::fs::write(&path, &rewound).expect("rewound fixture");
    let damaged = inspect(&path).expect("first fragment retained");
    assert!(damaged.truncated);
    assert_eq!(damaged.valid_prefix, good.len() as u64);
    assert!(
      damaged
        .damage_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("moved backwards"))
    );
    assert_eq!(damaged.track_media[0].samples, 2);
  }
  #[test]
  fn structural_attacks_fail_explicitly_without_reading_past_their_bounds() {
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("attack.mp4");
    let good = two_track_file();
    let ftyp = atom(b"ftyp", b"isom0000");
    let duplicate = atom(
      b"moov",
      &[
        atom(b"trak", &track(1, 60, b"vide", b"avc1", false)),
        atom(b"trak", &track(1, 48000, b"soun", b"mp4a", false)),
      ]
      .concat(),
    );
    let three = atom(
      b"moov",
      &[
        atom(b"trak", &track(1, 60, b"vide", b"avc1", false)),
        atom(b"trak", &track(2, 48000, b"soun", b"mp4a", false)),
        atom(b"trak", &track(3, 48000, b"soun", b"mp4a", false)),
      ]
      .concat(),
    );
    let unnumbered = atom(b"moov", &atom(b"trak", &atom(b"mdia", &[])));
    let zero_scale = track(1, 0, b"vide", b"avc1", false);
    let fixture = fixture();
    let moof = fixture
      .windows(4)
      .position(|bytes| bytes == b"moof")
      .expect("fragment")
      - 4;
    let mut not_mdat = fixture[..moof].to_vec();
    not_mdat.extend(&fixture[moof..fixture.len() - 12]);
    not_mdat.extend(atom(b"free", &[1, 2, 3, 4]));
    let mut negative_offset = good.clone();
    let video_trun = negative_offset
      .windows(4)
      .position(|bytes| bytes == b"trun")
      .expect("video run");
    negative_offset[video_trun + 12..video_trun + 16].copy_from_slice(&i32::MIN.to_be_bytes());
    let mut huge_metadata = ftyp.clone();
    huge_metadata.extend(((MAX_METADATA + 9) as u32).to_be_bytes());
    huge_metadata.extend(b"moov");
    huge_metadata.resize(huge_metadata.len() + MAX_METADATA as usize + 1, 0);
    let cases: [(&str, Vec<u8>, &str); 9] = [
      (
        "duplicate track",
        [ftyp.clone(), duplicate].concat(),
        "Duplicate track id",
      ),
      (
        "three tracks",
        [ftyp.clone(), three].concat(),
        "at most one audio track",
      ),
      ("no track id", [ftyp.clone(), unnumbered].concat(), "no id"),
      (
        "no timebase",
        [ftyp.clone(), atom(b"moov", &atom(b"trak", &zero_scale))].concat(),
        "valid timebase",
      ),
      (
        "fragment first",
        [ftyp.clone(), fixture[moof..].to_vec()].concat(),
        "precedes initialization",
      ),
      ("missing mdat", not_mdat, "Expected mdat"),
      (
        "unknown box",
        [ftyp.clone(), atom(b"wide", &[])].concat(),
        "Unexpected top-level box",
      ),
      (
        "header too small",
        [ftyp.clone(), vec![0, 0, 0, 4, b'f', b'r', b'e', b'e']].concat(),
        "smaller than its header",
      ),
      ("oversized metadata", huge_metadata, "exceeds 8 MiB"),
    ];
    for (name, bytes, reason) in cases {
      std::fs::write(&path, bytes).expect(name);
      let error = inspect(&path).expect_err(name);
      assert!(error.reason.contains(reason), "{name}: {}", error.reason);
    }
    std::fs::write(&path, negative_offset).expect("negative offset");
    assert!(
      inspect(&path).is_err(),
      "sample offset before the base must fail"
    );
    // A zero-length box means "to end of file"; this writer never emits one, so it is incomplete.
    std::fs::write(
      &path,
      [good.clone(), vec![0, 0, 0, 0, b'f', b'r', b'e', b'e']].concat(),
    )
    .expect("open-ended box");
    let open = inspect(&path).expect("retained prefix");
    assert!(open.truncated && open.valid_prefix == good.len() as u64);
    let destination = root.path().join("recovered.mp4");
    std::fs::write(&destination, b"existing").expect("existing destination");
    assert!(recover(&path, &destination).is_err());
    assert_eq!(std::fs::read(&destination).expect("untouched"), b"existing");
  }
  #[test]
  fn every_single_byte_corruption_or_truncation_is_bounded_and_never_overstates_media() {
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("mutated.mp4");
    let good = two_track_file();
    for length in 0..good.len() {
      std::fs::write(&path, &good[..length]).expect("truncated");
      assert!(
        inspect(&path).is_err(),
        "a file without one complete fragment must not inspect successfully ({length} bytes)"
      );
    }
    for index in 0..good.len() {
      for value in [0x00, 0xff, good[index] ^ 0x01] {
        let mut bytes = good.clone();
        bytes[index] = value;
        std::fs::write(&path, &bytes).expect("mutated");
        if let Ok(report) = inspect(&path) {
          assert!(report.valid_prefix <= bytes.len() as u64, "byte {index}");
          assert!(report.fragments <= 1 && report.tracks <= 2, "byte {index}");
          assert!(
            report.track_media.iter().all(|track| track.samples <= 2),
            "byte {index} invented samples"
          );
          assert!(!report.decoded);
        }
      }
    }
  }
  #[test]
  fn rejects_truncated_samples_and_malicious_metadata() {
    let t = tempfile::tempdir().expect("valid test fixture");
    let path = t.path().join("bad.mp4");
    for data in [
      vec![],
      vec![0; 8],
      fixture()[..fixture().len() - 1].to_vec(),
      atom(b"moof", &[0; 8]),
    ] {
      std::fs::write(&path, data).expect("valid test fixture");
      assert!(inspect(&path).is_err());
    }
    assert!(children(&[0; 8]).is_err());
  }
}
