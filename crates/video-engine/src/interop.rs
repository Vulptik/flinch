//! Player interoperability for fragmented MP4.
//!
//! Windows' MPEG-4 source (Media Foundation: Media Player, Photos, Explorer, and this engine's
//! own verifier) reads files in 256 KiB chunks. If a chunk boundary falls between the end of a
//! fragment's `moof` box header and the start of its media payload, that source ends the
//! stream there and silently drops every later frame. A boundary inside the `moof` box's own
//! 8-byte header is harmless. Measured on boundaries from 512 KiB to 4 MiB. The
//! first two fragments are parsed when the file opens and are not affected; every later one is.
//!
//! The engine's writer aligns fragments as it writes (`windows::aligned`). Finished and
//! recovered files are also checked here and, if needed, realigned: a `free` box moves each
//! affected fragment to start exactly on the boundary, and the absolute offsets that point past
//! it (`tfhd` base data offsets, `tfra` entries) follow.

use crate::types::{EngineError, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const READ_CHUNK: u64 = 256 * 1024;
/// `moof` and `mfra` boxes are rewritten in memory; anything larger is refused as malformed.
const METADATA_LIMIT: u64 = 16 * 1024 * 1024;
/// Copy granularity, and how often a long copy offers cancellation.
const COPY_BLOCK: usize = 1024 * 1024;

/// True when a read-chunk boundary lies in `[moof + 8, payload)`, where `payload` is the first
/// media byte of the `mdat` that follows the `moof`.
pub fn straddles(moof: u64, payload: u64) -> bool {
  straddles_in(moof, payload, READ_CHUNK)
}
/// [`straddles`] for an arbitrary chunk size (tests exercise small chunks).
pub fn straddles_in(moof: u64, payload: u64, chunk: u64) -> bool {
  boundary_after(moof, chunk) < payload
}
/// The first chunk boundary a fragment starting at `moof` could be split by.
pub fn boundary_after(moof: u64, chunk: u64) -> u64 {
  (moof + 8).div_ceil(chunk) * chunk
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Realignment {
  pub fragments: u64,
  /// Fragments that straddled a boundary and were moved.
  pub moved: u64,
  pub padding_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
  start: u64,
  size: u64,
  header: u64,
  kind: [u8; 4],
}

fn malformed(reason: impl Into<String>) -> EngineError {
  EngineError::new(ErrorKind::Io, "mp4 realignment", reason)
}

fn boxes(file: &mut File, length: u64) -> Result<Vec<Entry>> {
  let mut out = Vec::new();
  let mut at = 0;
  while at < length {
    if length - at < 8 {
      return Err(malformed(format!("Incomplete box at byte {at}")));
    }
    file
      .seek(SeekFrom::Start(at))
      .map_err(|e| EngineError::io("realign seek", e))?;
    let mut bytes = [0u8; 16];
    file
      .read_exact(&mut bytes[..8])
      .map_err(|e| EngineError::io("realign header", e))?;
    let small = u64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
    let kind = [bytes[4], bytes[5], bytes[6], bytes[7]];
    let (size, header) = match small {
      0 => (length - at, 8),
      1 => {
        file
          .read_exact(&mut bytes[8..16])
          .map_err(|e| EngineError::io("realign header", e))?;
        let mut large = [0u8; 8];
        large.copy_from_slice(&bytes[8..16]);
        (u64::from_be_bytes(large), 16)
      },
      size => (size, 8),
    };
    if size < header || size > length - at {
      return Err(malformed(format!("Box at byte {at} overruns the file")));
    }
    out.push(Entry {
      start: at,
      size,
      header,
      kind,
    });
    at += size;
  }
  Ok(out)
}

/// Fragments whose `moof`/`mdat` headers a Media Foundation read chunk would split.
pub fn count_straddles(path: &Path) -> Result<u64> {
  count_straddles_in(path, READ_CHUNK)
}
/// [`count_straddles`] for an arbitrary chunk size.
pub fn count_straddles_in(path: &Path, chunk: u64) -> Result<u64> {
  let mut file = File::open(path).map_err(|e| EngineError::io("realign open", e))?;
  let length = file
    .metadata()
    .map_err(|e| EngineError::io("realign size", e))?
    .len();
  let list = boxes(&mut file, length)?;
  Ok(
    fragments(&list)?
      .iter()
      .filter(|(moof, mdat)| straddles_in(moof.start, mdat.start + mdat.header, chunk))
      .count() as u64,
  )
}

fn fragments(list: &[Entry]) -> Result<Vec<(Entry, Entry)>> {
  let mut out = Vec::new();
  for (index, item) in list.iter().enumerate() {
    if &item.kind == b"moof" {
      let mdat = list
        .get(index + 1)
        .filter(|next| &next.kind == b"mdat")
        .ok_or_else(|| malformed("Expected mdat after moof"))?;
      out.push((*item, *mdat));
    }
  }
  Ok(out)
}

fn read_box(file: &mut File, item: &Entry) -> Result<Vec<u8>> {
  if item.size > METADATA_LIMIT {
    return Err(malformed("Fragment metadata exceeds its bound"));
  }
  file
    .seek(SeekFrom::Start(item.start))
    .map_err(|e| EngineError::io("realign seek", e))?;
  let mut bytes = vec![0u8; item.size as usize];
  file
    .read_exact(&mut bytes)
    .map_err(|e| EngineError::io("realign read", e))?;
  Ok(bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
  let field = bytes
    .get(at..at + 4)
    .ok_or_else(|| malformed("Truncated box field"))?;
  Ok(u32::from_be_bytes([field[0], field[1], field[2], field[3]]))
}
fn u64_at(bytes: &[u8], at: usize) -> Result<u64> {
  let field = bytes
    .get(at..at + 8)
    .ok_or_else(|| malformed("Truncated box field"))?;
  let mut value = [0u8; 8];
  value.copy_from_slice(field);
  Ok(u64::from_be_bytes(value))
}

/// Visit the child boxes of `bytes[start..end]` as `(offset, size, kind)`.
fn children(bytes: &[u8], start: usize, end: usize) -> Result<Vec<(usize, usize, [u8; 4])>> {
  let mut out = Vec::new();
  let mut at = start;
  while at < end {
    let size = u32_at(bytes, at)? as usize;
    let kind = bytes
      .get(at + 4..at + 8)
      .ok_or_else(|| malformed("Truncated child box"))?;
    if size < 8 || at + size > end {
      return Err(malformed("Child box overruns its parent"));
    }
    out.push((at, size, [kind[0], kind[1], kind[2], kind[3]]));
    at += size;
  }
  Ok(out)
}

/// Add `shift` to every explicit `tfhd` base data offset in a `moof`.
pub(crate) fn shift_moof(moof: &mut [u8], shift: u64) -> Result<()> {
  for (traf, size, kind) in children(moof, 8, moof.len())? {
    if &kind != b"traf" {
      continue;
    }
    for (tfhd, _, kind) in children(moof, traf + 8, traf + size)? {
      // Flag 0x000001: base-data-offset-present (absolute file position of the track's data).
      if &kind == b"tfhd" && u32_at(moof, tfhd + 8)? & 1 != 0 {
        let base = u64_at(moof, tfhd + 16)?
          .checked_add(shift)
          .ok_or_else(|| malformed("Base data offset overflow"))?;
        moof[tfhd + 16..tfhd + 24].copy_from_slice(&base.to_be_bytes());
      }
    }
  }
  Ok(())
}

/// Point every `tfra` entry at its fragment's new `moof` position.
pub(crate) fn shift_mfra(mfra: &mut [u8], moved: &[(u64, u64)]) -> Result<()> {
  for (tfra, _, kind) in children(mfra, 8, mfra.len())? {
    if &kind != b"tfra" {
      continue;
    }
    let version = mfra[tfra + 8];
    let lengths = u32_at(mfra, tfra + 16)?;
    let entries = u32_at(mfra, tfra + 20)? as usize;
    let field = if version == 1 { 8 } else { 4 };
    let tail = ((lengths >> 4 & 3) + 1 + (lengths >> 2 & 3) + 1 + (lengths & 3) + 1) as usize;
    let mut at = tfra + 24;
    for _ in 0..entries {
      at += field;
      let old = if version == 1 {
        u64_at(mfra, at)?
      } else {
        u64::from(u32_at(mfra, at)?)
      };
      let new = moved
        .binary_search_by_key(&old, |(from, _)| *from)
        .map(|index| moved[index].1)
        .map_err(|_| malformed("Random-access entry names no fragment"))?;
      if version == 1 {
        mfra[at..at + 8].copy_from_slice(&new.to_be_bytes());
      } else {
        let new = u32::try_from(new).map_err(|_| malformed("Random-access offset overflow"))?;
        mfra[at..at + 4].copy_from_slice(&new.to_be_bytes());
      }
      at += field + tail;
    }
  }
  Ok(())
}

/// Rewrite `path` so no fragment straddles a read-chunk boundary. A file that needs nothing is
/// left untouched (header scan only); otherwise a realigned copy replaces it after a sync.
/// `checkpoint` runs between copied blocks so a long rewrite stays cancellable.
pub fn realign(path: &Path, mut checkpoint: impl FnMut() -> Result<()>) -> Result<Realignment> {
  let mut file = File::open(path).map_err(|e| EngineError::io("realign open", e))?;
  let length = file
    .metadata()
    .map_err(|e| EngineError::io("realign size", e))?
    .len();
  let list = boxes(&mut file, length)?;
  let pairs = fragments(&list)?;
  let mut report = Realignment {
    fragments: pairs.len() as u64,
    ..Default::default()
  };
  if !pairs
    .iter()
    .any(|(moof, mdat)| straddles(moof.start, mdat.start + mdat.header))
  {
    return Ok(report);
  }
  // Chunk offsets in the initialization segment would also move; fragmented output has none.
  if let Some(moov) = list.iter().find(|item| &item.kind == b"moov") {
    let bytes = read_box(&mut file, moov)?;
    for kind in [b"stco", b"co64"] {
      let mut from = 0;
      while let Some(found) = bytes[from..].windows(4).position(|w| w == kind) {
        let at = from + found;
        if u32_at(&bytes, at + 8)? != 0 {
          return Err(malformed("Initialization segment has chunk offsets"));
        }
        from = at + 4;
      }
    }
  }
  let temporary = path.with_extension("realign.partial");
  // Only a copy this call created is ever removed; an existing name is someone else's file.
  let output = OpenOptions::new()
    .write(true)
    .create_new(true)
    .open(&temporary)
    .map_err(|e| EngineError::io("realign create", e))?;
  let result = (|| {
    let mut writer = BufWriter::with_capacity(COPY_BLOCK, output);
    let mut shift = 0u64;
    let mut moved: Vec<(u64, u64)> = Vec::with_capacity(pairs.len());
    let mut block = vec![0u8; COPY_BLOCK];
    for (index, item) in list.iter().enumerate() {
      let mut start = item.start + shift;
      match &item.kind {
        b"moof" => {
          let mdat = list
            .get(index + 1)
            .ok_or_else(|| malformed("Expected mdat after moof"))?;
          if straddles(start, start + item.size + mdat.header) {
            let boundary = (start + 8).div_ceil(READ_CHUNK) * READ_CHUNK;
            let pad = boundary - start;
            let mut free = vec![0u8; pad as usize];
            free[..4].copy_from_slice(&(pad as u32).to_be_bytes());
            free[4..8].copy_from_slice(b"free");
            writer
              .write_all(&free)
              .map_err(|e| EngineError::io("realign write", e))?;
            shift += pad;
            start = boundary;
            report.moved += 1;
            report.padding_bytes += pad;
          }
          let mut bytes = read_box(&mut file, item)?;
          shift_moof(&mut bytes, shift)?;
          moved.push((item.start, start));
          writer
            .write_all(&bytes)
            .map_err(|e| EngineError::io("realign write", e))?;
        },
        b"mfra" => {
          let mut bytes = read_box(&mut file, item)?;
          shift_mfra(&mut bytes, &moved)?;
          writer
            .write_all(&bytes)
            .map_err(|e| EngineError::io("realign write", e))?;
        },
        _ => {
          file
            .seek(SeekFrom::Start(item.start))
            .map_err(|e| EngineError::io("realign seek", e))?;
          let mut left = item.size;
          while left > 0 {
            let take = left.min(COPY_BLOCK as u64) as usize;
            file
              .read_exact(&mut block[..take])
              .map_err(|e| EngineError::io("realign read", e))?;
            writer
              .write_all(&block[..take])
              .map_err(|e| EngineError::io("realign write", e))?;
            left -= take as u64;
            checkpoint()?;
          }
        },
      }
    }
    let output = writer
      .into_inner()
      .map_err(|e| EngineError::io("realign flush", e.into_error()))?;
    output
      .sync_all()
      .map_err(|e| EngineError::io("realign sync", e))?;
    drop(output);
    if count_straddles(&temporary)? != 0 {
      return Err(malformed("Realigned file still straddles a read chunk"));
    }
    Ok(())
  })();
  drop(file);
  if let Err(error) = result {
    if let Err(cleanup) = fs::remove_file(&temporary) {
      eprintln!("Realignment cleanup failed: {cleanup}");
    }
    return Err(error);
  }
  fs::rename(&temporary, path).map_err(|e| EngineError::io("realign replace", e))?;
  Ok(report)
}

#[cfg(test)]
#[path = "interop/tests.rs"]
mod tests;
