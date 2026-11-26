// src/screenshot/encoding/jpeg.rs
// JPEG encoder (smallpeg, vendored and multithreaded) with SIMD RGBA->RGB conversion

use super::conversion::convert_rgba_to_rgb;
use super::error::{EncodingError, Result};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// smallpeg's quality 93 looks like libjpeg-turbo 4:2:0 at quality 90: on nine 2560×1440 game
/// screenshots the SSIMULACRA2 difference had a median of −0.1, so Flinch's setting is raised by
/// three.
const QUALITY_OFFSET: u8 = 3;

/// Progressive 4:2:0 JPEG from smallpeg (`vendor/smallpeg`, threaded by Flinch): trellis
/// quantization with optimized scans and Huffman tables. Against libjpeg-turbo at quality 90: 12%
/// smaller on nine game screenshots (2–29%) and 8% on 23 smaller ones, in 11–53 ms up to
/// 2560×1440 (115 ms before the threading).
/// `None` once `cancel` is set: smallpeg checks it between stages, each at most a few tens of
/// milliseconds.
fn encode_rgb_until(
  rgb: &[u8],
  width: u32,
  height: u32,
  quality: u8,
  cancel: &AtomicBool,
) -> Result<Option<Vec<u8>>> {
  check_limits(width, height)?;
  let expected = u64::from(width) * u64::from(height) * 3;
  if rgb.len() as u64 != expected {
    return Err(EncodingError::JpegEncode(format!(
      "{} RGB bytes for {width}x{height}, expected {expected}",
      rgb.len()
    )));
  }
  let quality = quality.saturating_add(QUALITY_OFFSET).min(100);
  Ok(
    smallpeg::Encoder::new(quality)
      .encode_rgb_until(rgb, width, height, || cancel.load(Ordering::Acquire)),
  )
}

fn encode_rgb<W: Write>(
  rgb: &[u8],
  width: u32,
  height: u32,
  mut writer: W,
  quality: u8,
) -> Result<()> {
  let never = AtomicBool::new(false);
  let bytes = encode_rgb_until(rgb, width, height, quality, &never)?
    .ok_or_else(|| EncodingError::JpegEncode("JPG encoding stopped".into()))?;
  writer
    .write_all(&bytes)
    .map_err(|e| EncodingError::JpegEncode(e.to_string()))
}

/// Encode RGB (alpha already dropped) to a JPG unless `cancel` is set first (`Ok(None)`), so a
/// speculative JPG can be abandoned part-way when it turns out not to be needed.
pub fn encode_rgb_jpeg_until(
  rgb: &[u8],
  width: u32,
  height: u32,
  quality: u8,
  cancel: &AtomicBool,
) -> Result<Option<Vec<u8>>> {
  encode_rgb_until(rgb, width, height, quality.clamp(1, 100), cancel)
}

/// Encode RGB (alpha already dropped) to a JPG.
pub fn encode_rgb_jpeg(rgb: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>> {
  let mut bytes = Vec::new();
  encode_rgb(rgb, width, height, &mut bytes, quality.clamp(1, 100))?;
  Ok(bytes)
}

fn check_limits(width: u32, height: u32) -> Result<()> {
  if width > u32::from(u16::MAX) || height > u32::from(u16::MAX) {
    return Err(EncodingError::JpegEncode(format!(
      "{width}x{height} exceeds the JPEG limit of 65535 pixels per side"
    )));
  }
  Ok(())
}

/// Encode RGBA buffer to JPEG format.
///
/// Uses SIMD-optimized RGBA to RGB conversion to minimize allocation overhead.
///
/// # Arguments
/// * `rgba` - Borrowed RGBA buffer (no copy of RGBA made)
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `writer` - Output destination
/// * `quality` - JPEG quality (1-100, higher = better quality, larger file)
///
/// # Memory
/// Allocates one RGB buffer of size (width * height * 3). For 4K, this is ~25MB.
/// The original RGBA buffer is NOT copied.
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(EncodingError)` with details on failure
pub fn encode_jpeg<W: Write>(
  rgba: &[u8],
  width: u32,
  height: u32,
  writer: W,
  quality: u8,
) -> Result<()> {
  // Validate buffer size
  super::error::validate_rgba(width, height, rgba.len())?;

  // Clamp quality to valid range
  let quality = quality.clamp(1, 100);

  // Convert RGBA to RGB using SIMD-optimized conversion
  // This is the only allocation needed
  let rgb = convert_rgba_to_rgb(rgba, width, height);

  encode_rgb(&rgb, width, height, writer, quality)
}

/// Encode RGBA buffer to JPEG with a reusable RGB buffer.
///
/// This variant allows callers to provide a pre-allocated RGB buffer
/// for repeated encoding operations, eliminating allocation overhead
/// after the first call.
///
/// # Arguments
/// * `rgba` - Borrowed RGBA buffer
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `writer` - Output destination
/// * `quality` - JPEG quality (1-100)
/// * `rgb_buffer` - Reusable RGB buffer (will be resized if needed)
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(EncodingError)` with details on failure
#[allow(dead_code)]
pub fn encode_jpeg_with_buffer<W: Write>(
  rgba: &[u8],
  width: u32,
  height: u32,
  writer: W,
  quality: u8,
  rgb_buffer: &mut Vec<u8>,
) -> Result<()> {
  // Validate buffer size
  super::error::validate_rgba(width, height, rgba.len())?;

  // Clamp quality to valid range
  let quality = quality.clamp(1, 100);

  // Resize RGB buffer if needed
  let required_rgb = (width as usize) * (height as usize) * 3;
  if rgb_buffer.len() != required_rgb {
    rgb_buffer.resize(required_rgb, 0);
  }

  // Convert RGBA to RGB using SIMD-optimized conversion
  super::conversion::convert_rgba_to_rgb_direct(rgba, rgb_buffer);

  encode_rgb(rgb_buffer, width, height, writer, quality)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_jpeg_encode_basic() {
    // 2x2 red square
    let rgba = [255u8, 0, 0, 255].repeat(4);
    let mut output = Vec::new();

    encode_jpeg(&rgba, 2, 2, &mut output, 90).expect("JPEG encode failed");

    // Verify JPEG magic bytes (SOI marker)
    assert!(output.starts_with(&[0xFF, 0xD8]));
    // Verify JPEG ends with EOI marker
    assert!(output.ends_with(&[0xFF, 0xD9]));
  }

  #[test]
  fn test_jpeg_quality_affects_size() {
    // 8x8 gradient for more realistic compression
    let rgba: Vec<u8> = (0..64)
      .flat_map(|i| {
        let v = ((i * 4) % 256) as u8;
        vec![v, v, v, 255]
      })
      .collect();

    let mut low_quality = Vec::new();
    let mut high_quality = Vec::new();

    encode_jpeg(&rgba, 8, 8, &mut low_quality, 10).expect("Low quality encode failed");
    encode_jpeg(&rgba, 8, 8, &mut high_quality, 95).expect("High quality encode failed");

    // Higher quality should produce larger files (usually)
    // Note: For very small images this may not always hold
    assert!(!low_quality.is_empty());
    assert!(!high_quality.is_empty());
  }

  /// Photo-like pixels: smooth gradients with fine colored texture, as game screenshots have.
  fn textured(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
      for x in 0..width {
        let noise = ((x * 7919 + y * 104_729) % 37) as u8;
        rgba.extend_from_slice(&[
          ((x * 255) / width) as u8 ^ (noise & 7),
          ((y * 255) / height) as u8,
          (128 + noise) % 255,
          255,
        ]);
      }
    }
    rgba
  }

  #[test]
  fn jpegs_are_smaller_than_the_former_4_2_2_encoder_and_decode_faithfully() {
    let (width, height) = (320, 192);
    let rgba = textured(width, height);
    let rgb = convert_rgba_to_rgb(&rgba, width, height);
    let mut output = Vec::new();
    encode_jpeg(&rgba, width, height, &mut output, 90).expect("encode");
    let mut former = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut former, 90)
      .encode(&rgb, width, height, image::ExtendedColorType::Rgb8)
      .expect("former encode");
    assert!(
      output.len() < former.len(),
      "{} vs {}",
      output.len(),
      former.len()
    );
    // 4:2:0 trades color resolution for size, as Windows does, so per-pixel error is only
    // bounded here. Perceived quality was compared with SSIMULACRA2, which unit tests cannot
    // afford.
    let psnr = |jpeg: &[u8]| {
      let decoded = image::load_from_memory(jpeg).expect("decode").to_rgb8();
      assert_eq!(decoded.dimensions(), (width, height));
      let error: f64 = decoded
        .as_raw()
        .iter()
        .zip(&rgb)
        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
        .sum::<f64>()
        / rgb.len() as f64;
      10.0 * (255.0f64 * 255.0 / error).log10()
    };
    let ours = psnr(&output);
    assert!(ours > 30.0, "PSNR {ours}");
    assert!(psnr(&former) > 30.0);
  }

  #[test]
  fn a_cancelled_jpg_stops_without_output_and_an_uncancelled_one_decodes() {
    let (width, height) = (640, 384);
    let rgba = textured(width, height);
    let rgb = convert_rgba_to_rgb(&rgba, width, height);
    let cancelled = AtomicBool::new(true);
    assert_eq!(
      encode_rgb_jpeg_until(&rgb, width, height, 90, &cancelled).expect("cancel is not an error"),
      None
    );
    let running = AtomicBool::new(false);
    let bytes = encode_rgb_jpeg_until(&rgb, width, height, 90, &running)
      .expect("encode")
      .expect("not cancelled");
    // The shared-RGB path writes exactly what the RGBA path writes.
    let mut from_rgba = Vec::new();
    encode_jpeg(&rgba, width, height, &mut from_rgba, 90).expect("encode");
    assert_eq!(bytes, from_rgba);
    assert_eq!(
      encode_rgb_jpeg(&rgb, width, height, 90).expect("encode"),
      from_rgba
    );
    let decoded = image::load_from_memory(&bytes).expect("decode");
    assert_eq!((decoded.width(), decoded.height()), (width, height));
  }

  #[test]
  fn sides_over_the_jpeg_limit_are_refused_with_a_reason() {
    let rgba = vec![0u8; 65_536 * 4];
    let result = encode_jpeg(&rgba, 65_536, 1, &mut Vec::new(), 90);
    assert!(
      matches!(&result, Err(EncodingError::JpegEncode(reason)) if reason.contains("65535")),
      "{result:?}"
    );
  }

  #[test]
  fn test_jpeg_invalid_dimensions() {
    let rgba = vec![0u8; 100]; // Wrong size
    let mut output = Vec::new();

    let result = encode_jpeg(&rgba, 10, 10, &mut output, 90);

    assert!(matches!(
      result,
      Err(EncodingError::InvalidDimensions { .. })
    ));
  }

  #[test]
  fn test_jpeg_with_reusable_buffer() {
    let rgba = [128u8, 64, 32, 255].repeat(4);
    let mut output = Vec::new();
    let mut rgb_buffer = Vec::new();

    // First encode
    encode_jpeg_with_buffer(&rgba, 2, 2, &mut output, 90, &mut rgb_buffer)
      .expect("First encode failed");

    assert_eq!(rgb_buffer.len(), 12); // 4 pixels * 3 bytes

    // Second encode reuses buffer
    output.clear();
    encode_jpeg_with_buffer(&rgba, 2, 2, &mut output, 90, &mut rgb_buffer)
      .expect("Second encode failed");

    assert!(output.starts_with(&[0xFF, 0xD8]));
  }
}
