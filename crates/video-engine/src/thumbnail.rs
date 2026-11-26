//! A small picture of a recording's last frame, made by the engine at Stop so the app can show
//! the saved recording's notification at once. Asking the Windows Shell instead meant decoding the
//! published file: measured on a 90 s 1440p60 recording, 456 ms for the first video
//! in a process and about 200 ms after that, longer than the notification waits.
//!
//! The picture is the last encoded frame, cursor and click highlights included, box-downscaled
//! to at most [`LONG_SIDE`] pixels. It is written as raw straight RGBA beside the session's
//! other files, because protocol messages stay under 64 KiB; the outcome names it.

use crate::gif::Nv12;
use crate::types::{EngineError, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Longest side of a thumbnail, in pixels; smaller pictures keep their size.
pub const LONG_SIDE: u32 = 256;
/// File name in the session directory.
pub const FILE: &str = "thumbnail.rgba";

/// Where a session's thumbnail was written, and its size.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Thumbnail {
  pub path: PathBuf,
  pub width: u32,
  pub height: u32,
}

/// Opaque straight RGBA pixels, row by row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixels {
  pub width: u32,
  pub height: u32,
  pub rgba: Vec<u8>,
}

/// The thumbnail size for a picture: the aspect kept, the longer side at most [`LONG_SIDE`].
pub fn extent(width: u32, height: u32) -> Result<(u32, u32)> {
  if width == 0 || height == 0 {
    return Err(EngineError::protocol(
      "A thumbnail needs a non-empty picture",
    ));
  }
  let long = width.max(height);
  if long <= LONG_SIDE {
    return Ok((width, height));
  }
  let scale = |value: u32| {
    let scaled = (u64::from(value) * u64::from(LONG_SIDE) + u64::from(long) / 2) / u64::from(long);
    scaled.max(1) as u32
  };
  Ok((scale(width), scale(height)))
}

/// The source pixels `[start, end)` that output pixel `index` of `count` covers in `length`.
fn span(index: u32, count: u32, length: u32) -> (usize, usize) {
  let edge = |i: u32| (u64::from(i) * u64::from(length) / u64::from(count)) as usize;
  let start = edge(index);
  (start, edge(index + 1).max(start + 1))
}

fn to_u32(value: usize) -> Result<u32> {
  u32::try_from(value).map_err(|_| EngineError::protocol("Thumbnail source is too large"))
}

/// Downscale one BT.709 limited-range NV12 picture, the encoder's input.
pub fn from_nv12(picture: &Nv12<'_>) -> Result<Pixels> {
  let (source_width, source_height) = picture.extent();
  let (source_width, source_height) = (to_u32(source_width)?, to_u32(source_height)?);
  let (width, height) = extent(source_width, source_height)?;
  let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
  for y in 0..height {
    let (y0, y1) = span(y, height, source_height);
    for x in 0..width {
      let (x0, x1) = span(x, width, source_width);
      let [r, g, b] = picture.average(x0, x1, y0, y1);
      rgba.extend_from_slice(&[r, g, b, u8::MAX]);
    }
  }
  Ok(Pixels {
    width,
    height,
    rgba,
  })
}

/// Downscale a BGRA picture with `pitch` bytes per row, the GPU-less tier's encoder input. Its
/// alpha is ignored: a recorded frame is opaque.
pub fn from_bgra(bgra: &[u8], pitch: usize, width: u32, height: u32) -> Result<Pixels> {
  let row = width as usize * 4;
  let needed = pitch
    .checked_mul(height.saturating_sub(1) as usize)
    .and_then(|rows| rows.checked_add(row))
    .ok_or_else(|| EngineError::protocol("Thumbnail source size overflow"))?;
  if width == 0 || height == 0 || pitch < row || bgra.len() < needed {
    return Err(EngineError::protocol(
      "Thumbnail BGRA source is smaller than its extent",
    ));
  }
  let (out_width, out_height) = extent(width, height)?;
  let mut rgba = Vec::with_capacity(out_width as usize * out_height as usize * 4);
  for y in 0..out_height {
    let (y0, y1) = span(y, out_height, height);
    for x in 0..out_width {
      let (x0, x1) = span(x, out_width, width);
      let mut sum = [0u64; 3];
      for source_y in y0..y1 {
        let line = &bgra[source_y * pitch + x0 * 4..source_y * pitch + x1 * 4];
        for pixel in line.as_chunks::<4>().0 {
          sum[0] += u64::from(pixel[2]);
          sum[1] += u64::from(pixel[1]);
          sum[2] += u64::from(pixel[0]);
        }
      }
      let count = ((x1 - x0) * (y1 - y0)) as u64;
      let mean = |value: u64| ((value + count / 2) / count) as u8;
      rgba.extend_from_slice(&[mean(sum[0]), mean(sum[1]), mean(sum[2]), u8::MAX]);
    }
  }
  Ok(Pixels {
    width: out_width,
    height: out_height,
    rgba,
  })
}

/// Write the pixels into the session directory and describe the file.
pub fn write(directory: &Path, pixels: &Pixels) -> Result<Thumbnail> {
  check(pixels.width, pixels.height, pixels.rgba.len() as u64)?;
  let path = directory.join(FILE);
  std::fs::write(&path, &pixels.rgba).map_err(|e| EngineError::io("thumbnail write", e))?;
  Ok(Thumbnail {
    path,
    width: pixels.width,
    height: pixels.height,
  })
}

/// Read a thumbnail the engine wrote, refusing any file that does not match its description
/// before reading it, so a damaged or foreign file never costs more than a thumbnail's memory.
pub fn read(thumbnail: &Thumbnail) -> Result<Pixels> {
  let length = std::fs::metadata(&thumbnail.path)
    .map_err(|e| EngineError::io("thumbnail size", e))?
    .len();
  check(thumbnail.width, thumbnail.height, length)?;
  let rgba = std::fs::read(&thumbnail.path).map_err(|e| EngineError::io("thumbnail read", e))?;
  check(thumbnail.width, thumbnail.height, rgba.len() as u64)?;
  Ok(Pixels {
    width: thumbnail.width,
    height: thumbnail.height,
    rgba,
  })
}

fn check(width: u32, height: u32, bytes: u64) -> Result<()> {
  if width == 0 || height == 0 || width.max(height) > LONG_SIDE {
    return Err(EngineError::new(
      ErrorKind::Protocol,
      "thumbnail",
      format!("A thumbnail cannot be {width}x{height}"),
    ));
  }
  let expected = u64::from(width) * u64::from(height) * 4;
  if bytes != expected {
    return Err(EngineError::new(
      ErrorKind::Protocol,
      "thumbnail",
      format!("A {width}x{height} thumbnail holds {expected} bytes, not {bytes}"),
    ));
  }
  Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;

  /// BT.709 limited-range NV12 of one flat color, `stride` bytes per row.
  fn flat_nv12(width: usize, height: usize, stride: usize, yuv: [u8; 3]) -> Vec<u8> {
    let mut plane = vec![0u8; stride * height + stride * height.div_ceil(2)];
    for y in 0..height {
      plane[y * stride..y * stride + width].fill(yuv[0]);
    }
    for y in 0..height / 2 {
      let row = stride * height + y * stride;
      for x in 0..width / 2 {
        plane[row + x * 2] = yuv[1];
        plane[row + x * 2 + 1] = yuv[2];
      }
    }
    plane
  }

  #[test]
  fn sizes_keep_the_aspect_and_never_grow() {
    assert_eq!(extent(2560, 1440).expect("16:9"), (256, 144));
    assert_eq!(extent(1080, 1920).expect("portrait"), (144, 256));
    assert_eq!(extent(200, 100).expect("small"), (200, 100));
    assert_eq!(extent(10_000, 2).expect("sliver"), (256, 1));
    assert!(extent(0, 10).is_err());
  }

  #[test]
  fn nv12_frames_downscale_to_their_colors() {
    // White and pure red (BT.709 limited range), each filling half the picture.
    let (width, height, stride) = (640, 360, 704);
    let mut plane = flat_nv12(width, height, stride, [235, 128, 128]);
    let red = flat_nv12(width, height, stride, [63, 102, 240]);
    for y in 0..height {
      let row = y * stride;
      plane[row + width / 2..row + width].copy_from_slice(&red[row + width / 2..row + width]);
    }
    for y in 0..height / 2 {
      let row = stride * height + y * stride;
      plane[row + width / 2..row + width].copy_from_slice(&red[row + width / 2..row + width]);
    }
    let picture = Nv12::new(&plane, stride, height, width, height).expect("picture");
    let pixels = from_nv12(&picture).expect("thumbnail");
    assert_eq!((pixels.width, pixels.height), (256, 144));
    assert_eq!(pixels.rgba.len(), 256 * 144 * 4);
    let at = |x: usize, y: usize| {
      let i = (y * 256 + x) * 4;
      [
        pixels.rgba[i],
        pixels.rgba[i + 1],
        pixels.rgba[i + 2],
        pixels.rgba[i + 3],
      ]
    };
    assert_eq!(at(10, 70), [255, 255, 255, 255]);
    let [r, g, b, a] = at(240, 70);
    assert!(r > 245 && g < 10 && b < 10 && a == 255, "{r} {g} {b}");
  }

  #[test]
  fn bgra_frames_downscale_with_their_row_padding_and_become_opaque() {
    let (width, height, pitch) = (512u32, 288u32, 512 * 4 + 64);
    let mut bgra = vec![0xEEu8; pitch * height as usize];
    for y in 0..height as usize {
      for x in 0..width as usize {
        // Blue left half, green right half, transparent alpha.
        let pixel = if x < 256 {
          [255, 0, 0, 0]
        } else {
          [0, 255, 0, 0]
        };
        bgra[y * pitch + x * 4..y * pitch + x * 4 + 4].copy_from_slice(&pixel);
      }
    }
    let pixels = from_bgra(&bgra, pitch, width, height).expect("thumbnail");
    assert_eq!((pixels.width, pixels.height), (256, 144));
    assert_eq!(&pixels.rgba[..4], &[0, 0, 255, 255]);
    let last = pixels.rgba.len() - 4;
    assert_eq!(&pixels.rgba[last..], &[0, 255, 0, 255]);
    assert!(from_bgra(&bgra, pitch, width, height + 1).is_err());
    assert!(from_bgra(&bgra, 16, width, height).is_err());
    assert!(from_bgra(&bgra, pitch, 0, height).is_err());
  }

  #[test]
  fn written_thumbnails_read_back_and_mismatched_files_are_refused() {
    let folder = tempfile::tempdir().expect("folder");
    let pixels = Pixels {
      width: 4,
      height: 2,
      rgba: (0..32).collect(),
    };
    let thumbnail = write(folder.path(), &pixels).expect("written");
    assert_eq!(thumbnail.path, folder.path().join(FILE));
    assert_eq!(read(&thumbnail).expect("read"), pixels);
    // A description that does not match the file is refused before reading it.
    let wrong = Thumbnail {
      width: 5,
      ..thumbnail.clone()
    };
    assert!(read(&wrong).is_err());
    let oversized = Thumbnail {
      width: LONG_SIDE + 1,
      ..thumbnail.clone()
    };
    assert!(read(&oversized).is_err());
    std::fs::remove_file(&thumbnail.path).expect("remove");
    assert!(read(&thumbnail).is_err());
    assert!(
      write(
        folder.path(),
        &Pixels {
          width: 4,
          height: 2,
          rgba: vec![0; 31],
        }
      )
      .is_err()
    );
  }
}
