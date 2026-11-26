// src/screenshot/encoding/webp.rs
// WebP encoder with lossy and lossless support via libwebp
//
// Uses the `webp` crate which wraps libwebp for full quality control.

use super::error::{EncodingError, Result};
use std::io::Write;
use webp::{Encoder as WebpEncoder, PixelLayout};

/// Default WebP quality (1-100)
#[allow(dead_code)]
pub const DEFAULT_WEBP_QUALITY: u8 = 85;

/// Encode RGBA buffer to WebP format with quality control.
///
/// Uses libwebp via the `webp` crate for both lossy and lossless encoding.
///
/// # Arguments
/// * `rgba` - Borrowed RGBA buffer (no copy made by this function)
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `writer` - Output destination
/// * `quality` - WebP quality (1-100). 100 = lossless, <100 = lossy
///
/// # Quality Behavior
/// * `quality >= 100` - Lossless compression (larger files, perfect quality)
/// * `quality < 100` - Lossy compression (quality affects size/quality tradeoff)
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(EncodingError)` with details on failure
pub fn encode_webp<W: Write>(
  rgba: &[u8],
  width: u32,
  height: u32,
  mut writer: W,
  quality: u8,
) -> Result<()> {
  // Validate buffer size
  super::error::validate_rgba(width, height, rgba.len())?;

  // Create encoder from RGBA data - operates on borrowed slice
  let encoder = WebpEncoder::new(rgba, PixelLayout::Rgba, width, height);

  // Encode based on quality setting
  let webp_data = if quality >= 100 {
    encoder.encode_lossless()
  } else {
    // Clamp quality to valid range and convert to f32
    let q = quality.clamp(1, 99) as f32;
    encoder.encode(q)
  };

  // Write to output
  writer
    .write_all(&webp_data)
    .map_err(|e| EncodingError::WebpEncode(format!("Failed to write WebP data: {}", e)))
}

/// Encode RGBA buffer to WebP with lossless compression.
///
/// Lossless WebP produces perfect quality but larger files.
/// Good for screenshots that will be edited later.
#[allow(dead_code)]
#[inline]
pub fn encode_webp_lossless<W: Write>(
  rgba: &[u8],
  width: u32,
  height: u32,
  writer: W,
) -> Result<()> {
  encode_webp(rgba, width, height, writer, 100)
}

/// Encode RGBA buffer to WebP with default quality (lossy, 85%).
#[allow(dead_code)]
#[inline]
pub fn encode_webp_default<W: Write>(
  rgba: &[u8],
  width: u32,
  height: u32,
  writer: W,
) -> Result<()> {
  encode_webp(rgba, width, height, writer, DEFAULT_WEBP_QUALITY)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_webp_encode_basic() {
    // 2x2 red square
    let rgba = [255u8, 0, 0, 255].repeat(4);
    let mut output = Vec::new();

    encode_webp(&rgba, 2, 2, &mut output, 85).expect("WebP encode failed");

    // Verify RIFF/WEBP header
    assert!(output.starts_with(b"RIFF"));
    assert!(output.len() > 12);
    assert_eq!(&output[8..12], b"WEBP");
  }

  #[test]
  fn test_webp_quality_affects_size() {
    // 16x16 gradient for meaningful compression differences
    let rgba: Vec<u8> = (0..256)
      .flat_map(|i| {
        let v = (i % 256) as u8;
        vec![v, v / 2, v / 4, 255]
      })
      .collect();

    let mut low_quality = Vec::new();
    let mut high_quality = Vec::new();
    let mut lossless = Vec::new();

    encode_webp(&rgba, 16, 16, &mut low_quality, 30).expect("Low quality failed");
    encode_webp(&rgba, 16, 16, &mut high_quality, 95).expect("High quality failed");
    encode_webp(&rgba, 16, 16, &mut lossless, 100).expect("Lossless failed");

    // All should produce valid WebP files
    assert!(low_quality.starts_with(b"RIFF"));
    assert!(high_quality.starts_with(b"RIFF"));
    assert!(lossless.starts_with(b"RIFF"));

    // Low quality should be smaller than high quality (usually)
    // Lossless should be larger than lossy
    println!(
      "WebP sizes - low(30): {} bytes, high(95): {} bytes, lossless: {} bytes",
      low_quality.len(),
      high_quality.len(),
      lossless.len()
    );

    // For non-trivial images, low quality should produce smaller files
    assert!(
      low_quality.len() <= high_quality.len(),
      "Low quality should produce smaller or equal file size"
    );
  }

  #[test]
  fn test_webp_invalid_dimensions() {
    let rgba = vec![0u8; 100]; // Wrong size
    let mut output = Vec::new();

    let result = encode_webp(&rgba, 10, 10, &mut output, 85);

    assert!(matches!(
      result,
      Err(EncodingError::InvalidDimensions { .. })
    ));
  }

  #[test]
  fn test_webp_lossless() {
    let rgba = [128u8, 64, 32, 200].repeat(4);
    let mut output = Vec::new();

    encode_webp_lossless(&rgba, 2, 2, &mut output).expect("Lossless encode failed");

    assert!(output.starts_with(b"RIFF"));
  }
}
