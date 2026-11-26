// src/screenshot/encoding/mod.rs
// Optimized image encoding pipeline with zero-copy where possible

pub mod conversion;
pub mod error;
pub mod jpeg;
pub mod png;
pub mod webp;

pub use error::{EncodingError, Result};
pub use jpeg::encode_jpeg;
pub use png::{PngCompression, encode_png};
pub use webp::encode_webp;

use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod save_tests;

/// Configuration for image encoding operations.
#[derive(Debug, Clone)]
pub struct EncodingConfig {
  /// Quality level (1-100). Interpretation depends on format:
  /// - JPEG: Compression quality (higher = better quality, larger file)
  /// - WebP: Compression quality (100 = lossless)
  /// - PNG: Mapped to compression preset (0-2: Fast, 3-6: Default, 7-9: Best)
  pub quality: u8,
  /// PNG-specific compression level (0-9). If None, derived from quality.
  pub png_compression: Option<u8>,
}

impl Default for EncodingConfig {
  fn default() -> Self {
    Self {
      quality: 90,
      png_compression: None,
    }
  }
}

impl EncodingConfig {
  /// Create a new config with the specified quality.
  pub fn with_quality(quality: u8) -> Self {
    Self {
      quality,
      ..Default::default()
    }
  }

  /// Get the PNG compression preset based on config.
  fn png_compression(&self) -> PngCompression {
    match self.png_compression {
      Some(level) => PngCompression::from_level(level),
      None => {
        // Map quality to compression level
        // High quality (90-100) -> Fast (prioritize speed)
        // Medium quality (50-89) -> Default
        // Low quality (0-49) -> Best (prioritize size)
        match self.quality {
          90..=100 => PngCompression::Fast,
          50..=89 => PngCompression::Default,
          _ => PngCompression::Best,
        }
      },
    }
  }
}

/// Save RGBA image to file with optimized encoding.
///
/// This is the primary API for saving screenshots. It:
/// - Operates on the borrowed RGBA buffer (PNG copies opaque images once, without alpha)
/// - Properly applies quality settings to all formats
/// - Encodes in memory, then stages, flushes and publishes the new file
///
/// # Arguments
/// * `path` - Output file path
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `rgba` - Borrowed RGBA buffer
/// * `format` - Output format ("png", "jpg", "jpeg", "webp")
/// * `quality` - Quality level (1-100)
///
/// # Quality Semantics by Format
///
/// | Format | Quality 100 | Quality 50 | Quality 1 |
/// |--------|------------|------------|-----------|
/// | **PNG** | Fast encoding (lossless) | Default compression | Best compression (smaller) |
/// | **JPEG** | Highest quality | Medium quality | Lowest quality |
/// | **WebP** | Lossless | Lossy (medium) | Lossy (low) |
///
/// Note: PNG is always lossless. The quality setting only affects compression
/// effort (speed vs. file size), not visual quality. Fast is strengthened to Default for images
/// up to one megapixel (`PngCompression::for_image`).
///
/// # Memory Usage
/// - PNG: One RGB buffer allocation (width * height * 3) when every pixel is opaque
/// - WebP: No additional buffer allocation
/// - JPEG: One RGB buffer allocation (width * height * 3)
pub fn save_rgba_image_optimized(
  path: &Path,
  width: u32,
  height: u32,
  rgba: &[u8],
  format: &str,
  quality: u8,
) -> Result<()> {
  error::validate_rgba(width, height, rgba.len())?;
  let format_lower = supported_format(format)?;
  let bytes = encode(path, width, height, rgba, &format_lower, quality)?;
  write_new_file(path, &bytes)
}

/// PNG size above which "Save Large Screenshots as JPG" saves a JPG instead: 2048 KB, what
/// Explorer shows as 2,048 KB.
pub const LARGE_PNG_BYTES: usize = 2 * 1024 * 1024;

/// Save a screenshot and return the file written. With `large_png_as_jpg`, a PNG that would exceed
/// `LARGE_PNG_BYTES` is saved as a JPG at `quality` instead, at the free path `jpg_path` picks from
/// `path`; the PNG is abandoned as soon as it passes the limit. Its 4:2:0 JPG came out smaller than such a PNG even for noise at quality
/// 100. Other formats save as `save_rgba_image_optimized` does.
#[allow(clippy::too_many_arguments)]
pub fn save_screenshot(
  path: &Path,
  width: u32,
  height: u32,
  rgba: &[u8],
  format: &str,
  quality: u8,
  large_png_as_jpg: bool,
  jpg_path: impl FnOnce(&Path) -> PathBuf,
) -> Result<PathBuf> {
  error::validate_rgba(width, height, rgba.len())?;
  let format_lower = supported_format(format)?;
  if !(large_png_as_jpg && format_lower == "png") {
    let bytes = encode(path, width, height, rgba, &format_lower, quality)?;
    write_new_file(path, &bytes)?;
    return Ok(path.to_path_buf());
  }
  // One RGB copy serves both: the PNG of an opaque screenshot and the JPG.
  let rgb = conversion::convert_rgba_to_rgb(rgba, width, height);
  let pixels = if png::is_opaque(rgba) {
    png::PngPixels::Rgb(&rgb)
  } else {
    png::PngPixels::Rgba(rgba)
  };
  let compression = EncodingConfig::with_quality(quality)
    .png_compression()
    .for_image(width, height);
  let speculate = png_may_exceed_limit(width, height) && png_likely_large(width, height, rgba);
  let (png, speculative) = if speculate {
    png_with_speculative_jpg(pixels, &rgb, width, height, compression, quality)?
  } else {
    (
      png::encode_png_limited(pixels, width, height, compression, LARGE_PNG_BYTES)?,
      None,
    )
  };
  match png {
    png::PngOutcome::Encoded(bytes) => {
      write_new_file(path, &bytes)?;
      Ok(path.to_path_buf())
    },
    png::PngOutcome::OverLimit => {
      let jpg = match speculative {
        Some(jpg) => jpg,
        None => jpeg::encode_rgb_jpeg(&rgb, width, height, quality)?,
      };
      let target = jpg_path(path);
      tracing::info!(
        operation = "save_rgba_image",
        png_limit_bytes = LARGE_PNG_BYTES,
        jpg_bytes = jpg.len(),
        path = %target.display(),
        "Large PNG saved as JPG"
      );
      write_new_file(&target, &jpg)?;
      Ok(target)
    },
  }
}

/// Whether a PNG of this size could pass `LARGE_PNG_BYTES`: deflate never grows data by more than
/// a few bytes per block, so a PNG stays within its RGB bytes, one filter byte per row and its
/// chunks. Smaller screenshots never start a speculative JPG.
fn png_may_exceed_limit(width: u32, height: u32) -> bool {
  u64::from(width)
    .saturating_mul(u64::from(height))
    .saturating_mul(3)
    .saturating_add(u64::from(height) + 64 * 1024)
    > LARGE_PNG_BYTES as u64
}

/// Rows sampled to estimate a PNG's size before deciding to start a speculative JPG.
const SAMPLE_ROW_STEP: usize = 16;

/// Whether the PNG will probably pass `LARGE_PNG_BYTES`, from every 16th row encoded with the
/// fastest settings (about a sixteenth of the PNG's cost). Sampled rows lose their vertical
/// neighbours, so the estimate runs high and large screenshots are not missed. A wrong guess costs
/// only speed: the full PNG still decides, and a JPG is then encoded after it.
fn png_likely_large(width: u32, height: u32, rgba: &[u8]) -> bool {
  let row = width as usize * 4;
  let sampled: Vec<u8> = rgba
    .chunks_exact(row)
    .step_by(SAMPLE_ROW_STEP)
    .flatten()
    .copied()
    .collect();
  let rows = sampled.len() / row.max(1);
  let Ok(rows_u32) = u32::try_from(rows) else {
    return true;
  };
  let mut bytes = Vec::new();
  if rows == 0 || encode_png(&sampled, width, rows_u32, &mut bytes, PngCompression::Fast).is_err() {
    return true;
  }
  let estimate = bytes.len() as u64 * u64::from(height) / rows as u64;
  estimate > LARGE_PNG_BYTES as u64
}

/// The PNG check, and a JPG encoded beside it on a second thread, kept only when the PNG passes
/// `LARGE_PNG_BYTES`: a large screenshot then waits for the slower of the two instead of both, and
/// a small one stops the JPG part-way (smallpeg checks between stages) instead of waiting for it.
/// The PNG itself stops as soon as it passes the limit.
fn png_with_speculative_jpg(
  pixels: png::PngPixels<'_>,
  rgb: &[u8],
  width: u32,
  height: u32,
  compression: PngCompression,
  quality: u8,
) -> Result<(png::PngOutcome, Option<Vec<u8>>)> {
  use std::sync::atomic::{AtomicBool, Ordering};
  let cancel = AtomicBool::new(false);
  std::thread::scope(|scope| {
    let jpg = scope.spawn(|| jpeg::encode_rgb_jpeg_until(rgb, width, height, quality, &cancel));
    let png = png::encode_png_limited(pixels, width, height, compression, LARGE_PNG_BYTES);
    let keep_jpg = matches!(png, Ok(png::PngOutcome::OverLimit));
    if !keep_jpg {
      cancel.store(true, Ordering::Release);
    }
    // Bounded: a cancelled encode stops at its next stage; a kept one runs to its end.
    let jpg = jpg
      .join()
      .map_err(|_| EncodingError::JpegEncode("The JPG encoder thread panicked".into()))?;
    let png = png?;
    // A cancelled or failed JPG is not needed when the PNG stays.
    Ok((png, if keep_jpg { jpg? } else { None }))
  })
}

fn supported_format(format: &str) -> Result<String> {
  let format_lower = format.to_lowercase();
  if !matches!(format_lower.as_str(), "png" | "jpg" | "jpeg" | "webp") {
    return Err(EncodingError::UnsupportedFormat(format_lower));
  }
  Ok(format_lower)
}

/// The encoded file for `format_lower`, already checked by `supported_format`.
fn encode(
  path: &Path,
  width: u32,
  height: u32,
  rgba: &[u8],
  format_lower: &str,
  quality: u8,
) -> Result<Vec<u8>> {
  let config = EncodingConfig::with_quality(quality);
  let mut bytes = Vec::new();
  tracing::debug!(
      operation = "save_rgba_image",
      format = %format_lower,
      quality = quality,
      width = width,
      height = height,
      path = %path.display(),
      "Encoding image"
  );

  match format_lower {
    "png" => {
      let compression = config.png_compression().for_image(width, height);
      tracing::debug!(
          format = "png",
          compression = ?compression,
          "PNG compression preset selected"
      );
      encode_png(rgba, width, height, &mut bytes, compression)
    },
    "jpg" | "jpeg" => {
      tracing::debug!(
        format = "jpeg",
        quality = config.quality,
        "JPEG quality applied"
      );
      encode_jpeg(rgba, width, height, &mut bytes, config.quality)
    },
    "webp" => {
      let mode = if config.quality >= 100 {
        "lossless"
      } else {
        "lossy"
      };
      tracing::debug!(
        format = "webp",
        quality = config.quality,
        mode = mode,
        "WebP encoding mode"
      );
      encode_webp(rgba, width, height, &mut bytes, config.quality)
    },
    other => Err(EncodingError::UnsupportedFormat(other.to_string())),
  }?;
  Ok(bytes)
}

/// Write `bytes` as the new file `path`: staged beside it, flushed, and published without
/// replacing an existing file.
fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
  let parent = path
    .parent()
    .filter(|p| !p.as_os_str().is_empty())
    .unwrap_or(Path::new("."));
  let mut temporary =
    tempfile::NamedTempFile::new_in(parent).map_err(|e| EncodingError::io_error(path, e))?;
  temporary
    .as_file_mut()
    .write_all(bytes)
    .map_err(|e| EncodingError::io_error(path, e))?;
  temporary
    .as_file()
    .sync_all()
    .map_err(|e| EncodingError::io_error(path, e))?;
  temporary
    .persist_noclobber(path)
    .map_err(|e| EncodingError::io_error(path, e.error))?;
  Ok(())
}

/// Crop RGBA buffer directly without intermediate DynamicImage allocation.
///
/// This replaces the previous implementation that:
/// 1. Copied the entire RGBA buffer to create an RgbaImage
/// 2. Created a DynamicImage wrapper
/// 3. Called crop_imm() which allocates another buffer
/// 4. Converted back to RGBA bytes
///
/// The new implementation copies only the cropped region directly.
///
/// # Memory Usage
/// Old: Full RGBA copy + cropped copy = (src_w * src_h * 4) + (w * h * 4)
/// New: Only cropped copy = (w * h * 4)
pub fn crop_rgba_direct(
  rgba: &[u8],
  src_width: u32,
  x: u32,
  y: u32,
  width: u32,
  height: u32,
) -> Result<Vec<u8>> {
  let invalid = || {
    EncodingError::InvalidCrop(format!(
      "{x},{y} {width}x{height} in a {src_width}-pixel-wide source with {} bytes",
      rgba.len()
    ))
  };
  let src_stride = (src_width as usize)
    .checked_mul(4)
    .filter(|&n| n > 0)
    .ok_or_else(invalid)?;
  if width == 0 || height == 0 || !rgba.len().is_multiple_of(src_stride) {
    return Err(invalid());
  }
  let right = x.checked_add(width).ok_or_else(invalid)?;
  let bottom = y.checked_add(height).ok_or_else(invalid)?;
  if right > src_width || bottom as usize > rgba.len() / src_stride {
    return Err(invalid());
  }
  let dst_stride = (width as usize).checked_mul(4).ok_or_else(invalid)?;
  let length = dst_stride
    .checked_mul(height as usize)
    .filter(|&n| n <= isize::MAX as usize)
    .ok_or_else(invalid)?;
  let mut cropped = Vec::with_capacity(length);

  for row in 0..height {
    let src_offset = ((y + row) as usize * src_stride) + (x as usize * 4);
    cropped.extend_from_slice(&rgba[src_offset..src_offset + dst_stride]);
  }

  Ok(cropped)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn unique_temp_path(extension: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|duration| duration.as_nanos())
      .unwrap_or(0);
    std::env::temp_dir().join(format!("flinch-encoding-{nanos}.{extension}"))
  }

  #[test]
  fn test_encoding_config_default() {
    let config = EncodingConfig::default();
    assert_eq!(config.quality, 90);
    assert!(config.png_compression.is_none());
  }

  #[test]
  fn test_encoding_config_png_compression_mapping() {
    // High quality -> Fast compression
    let config = EncodingConfig::with_quality(95);
    assert!(matches!(config.png_compression(), PngCompression::Fast));

    // Medium quality -> Default compression
    let config = EncodingConfig::with_quality(70);
    assert!(matches!(config.png_compression(), PngCompression::Default));

    // Low quality -> Best compression
    let config = EncodingConfig::with_quality(30);
    assert!(matches!(config.png_compression(), PngCompression::Best));
  }

  #[test]
  fn test_crop_rgba_direct() {
    // 4x4 image where each pixel has unique value based on position
    let mut rgba = Vec::with_capacity(64);
    for y in 0..4u8 {
      for x in 0..4u8 {
        let val = y * 4 + x;
        rgba.extend_from_slice(&[val, val, val, 255]);
      }
    }

    // Crop 2x2 region from (1,1)
    let cropped = crop_rgba_direct(&rgba, 4, 1, 1, 2, 2).expect("valid crop");

    assert_eq!(cropped.len(), 16); // 2x2x4

    // Expected pixels: (1,1)=5, (2,1)=6, (1,2)=9, (2,2)=10
    assert_eq!(cropped[0], 5); // First pixel R
    assert_eq!(cropped[4], 6); // Second pixel R
    assert_eq!(cropped[8], 9); // Third pixel R
    assert_eq!(cropped[12], 10); // Fourth pixel R
  }

  #[test]
  fn test_save_rgba_image_optimized_png() {
    let rgba = [255u8, 0, 0, 255].repeat(4);
    let path = unique_temp_path("png");

    save_rgba_image_optimized(&path, 2, 2, &rgba, "png", 90).expect("PNG save failed");

    assert!(path.exists());
    let content = std::fs::read(&path).expect("Failed to read");
    assert!(content.starts_with(&[0x89, 0x50, 0x4E, 0x47])); // PNG magic

    std::fs::remove_file(&path).ok();
  }

  #[test]
  fn test_save_rgba_image_optimized_jpeg() {
    let rgba = [0u8, 255, 0, 255].repeat(4);
    let path = unique_temp_path("jpg");

    save_rgba_image_optimized(&path, 2, 2, &rgba, "jpeg", 85).expect("JPEG save failed");

    assert!(path.exists());
    let content = std::fs::read(&path).expect("Failed to read");
    assert!(content.starts_with(&[0xFF, 0xD8])); // JPEG SOI

    std::fs::remove_file(&path).ok();
  }

  #[test]
  fn test_save_rgba_image_optimized_webp() {
    let rgba = [0u8, 0, 255, 255].repeat(4);
    let path = unique_temp_path("webp");

    save_rgba_image_optimized(&path, 2, 2, &rgba, "webp", 80).expect("WebP save failed");

    assert!(path.exists());
    let content = std::fs::read(&path).expect("Failed to read");
    assert!(content.starts_with(b"RIFF")); // RIFF container

    std::fs::remove_file(&path).ok();
  }

  #[test]
  fn test_unsupported_format() {
    let rgba = vec![0u8; 16];
    let path = unique_temp_path("tiff");

    let result = save_rgba_image_optimized(&path, 2, 2, &rgba, "tiff", 90);

    assert!(matches!(result, Err(EncodingError::UnsupportedFormat(_))));
  }
}
