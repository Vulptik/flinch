// src/screenshot/encoding/png.rs
// PNG encoder with compression control over a borrowed RGBA buffer; opaque images are written as RGB

use super::error::{EncodingError, Result};
use image::codecs::png::{
  CompressionType as PngCompressionType, FilterType as PngFilterType, PngEncoder,
};
use image::{ExtendedColorType, ImageEncoder};
use std::io::Write;

/// Largest image (one megapixel) that Fast saves with Default compression instead. Measured
/// on 247 Flinch screenshots: up to this size, desktop screenshots got 22–24% smaller
/// in a median 1–10 ms (155 ms at worst); full-screen ones gained only 7–8% for 0.3–0.85 s.
/// With flate2's zlib-rs backend (root `Cargo.toml`) the same small images take a median
/// 1.7 ms and at most 72 ms, and full-screen ones at most 0.25 s.
pub const STRONGER_COMPRESSION_PIXELS: u64 = 1_000_000;

/// PNG compression preset for balancing speed vs. file size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PngCompression {
  /// Fastest encoding, larger files. Good for high-frequency captures.
  Fast,
  /// Balanced speed and size. Default for most use cases.
  #[default]
  Default,
  /// Best compression, slower encoding. Good for archival.
  Best,
}

impl PngCompression {
  /// Convert from a 0-9 compression level (like zlib).
  /// 0-2 -> Fast, 3-6 -> Default, 7-9 -> Best
  pub fn from_level(level: u8) -> Self {
    match level {
      0..=2 => Self::Fast,
      3..=6 => Self::Default,
      _ => Self::Best,
    }
  }

  /// The preset for an image of this size: Fast becomes Default up to `STRONGER_COMPRESSION_PIXELS`,
  /// where the stronger compression stays quick. Default and Best are kept.
  pub fn for_image(self, width: u32, height: u32) -> Self {
    if self == Self::Fast && u64::from(width) * u64::from(height) <= STRONGER_COMPRESSION_PIXELS {
      Self::Default
    } else {
      self
    }
  }

  fn to_compression_type(self) -> PngCompressionType {
    match self {
      Self::Fast => PngCompressionType::Fast,
      Self::Default => PngCompressionType::Default,
      Self::Best => PngCompressionType::Best,
    }
  }

  /// Every preset picks the best filter row by row. Measured on 2560×1440 game
  /// screenshots: with Fast compression it saved up to a further 16% over the Sub filter for at
  /// most 5 ms, so it costs no speed worth keeping Sub for.
  fn to_filter_type(self) -> PngFilterType {
    PngFilterType::Adaptive
  }
}

/// The pixels without their alpha bytes when every pixel is opaque, which screenshots always
/// are: a constant alpha channel still costs 10–15% of a PNG. `None` keeps the RGBA encoding,
/// because a pixel is translucent or the copy could not be allocated (logged).
fn opaque_rgb(rgba: &[u8]) -> Option<Vec<u8>> {
  let pixels = rgba.as_chunks::<4>().0;
  if pixels.iter().any(|pixel| pixel[3] != u8::MAX) {
    return None;
  }
  let mut rgb = Vec::new();
  if let Err(error) = rgb.try_reserve_exact(pixels.len() * 3) {
    tracing::warn!(
      bytes = pixels.len() * 3,
      %error,
      "No memory to drop the PNG alpha channel; saving RGBA instead"
    );
    return None;
  }
  for pixel in pixels {
    rgb.extend_from_slice(&pixel[..3]);
  }
  Some(rgb)
}

/// Encode RGBA buffer to PNG format.
///
/// # Arguments
/// * `rgba` - Borrowed RGBA buffer; an opaque image is copied once without its alpha bytes
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `writer` - Output destination
/// * `compression` - Compression preset
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(EncodingError)` with details on failure
pub fn encode_png<W: Write>(
  rgba: &[u8],
  width: u32,
  height: u32,
  writer: W,
  compression: PngCompression,
) -> Result<()> {
  // Validate buffer size
  super::error::validate_rgba(width, height, rgba.len())?;

  let encoder = PngEncoder::new_with_quality(
    writer,
    compression.to_compression_type(),
    compression.to_filter_type(),
  );

  let written = match opaque_rgb(rgba) {
    Some(rgb) => encoder.write_image(&rgb, width, height, ExtendedColorType::Rgb8),
    None => encoder.write_image(rgba, width, height, ExtendedColorType::Rgba8),
  };
  written.map_err(|e| EncodingError::PngEncode(e.to_string()))
}

/// Pixels for `encode_png_limited`: RGB when every pixel is opaque, otherwise RGBA.
#[derive(Debug, Clone, Copy)]
pub enum PngPixels<'a> {
  Rgb(&'a [u8]),
  Rgba(&'a [u8]),
}

/// Whether every pixel of an RGBA buffer is opaque, as screenshots always are.
pub fn is_opaque(rgba: &[u8]) -> bool {
  rgba
    .as_chunks::<4>()
    .0
    .iter()
    .all(|pixel| pixel[3] == u8::MAX)
}

/// A PNG, or the knowledge that it would exceed the size limit.
#[derive(Debug, PartialEq, Eq)]
pub enum PngOutcome {
  Encoded(Vec<u8>),
  OverLimit,
}

/// Collects the PNG and refuses the write that would take it past `limit`, setting `over`: the
/// encoder then stops, and the flag (never the error's text) says why.
struct LimitedWriter {
  bytes: Vec<u8>,
  limit: usize,
  over: bool,
}

impl Write for LimitedWriter {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    if self.bytes.len().saturating_add(buf.len()) > self.limit {
      self.over = true;
      return Err(std::io::Error::other("PNG passed its size limit"));
    }
    self.bytes.extend_from_slice(buf);
    Ok(buf.len())
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

/// The PNG of `pixels`, or `OverLimit` as soon as it passes `limit` bytes. A PNG only grows as
/// it is written, so this is exactly `len > limit` for the finished file, decided early.
pub fn encode_png_limited(
  pixels: PngPixels<'_>,
  width: u32,
  height: u32,
  compression: PngCompression,
  limit: usize,
) -> Result<PngOutcome> {
  let (data, channels, color) = match pixels {
    PngPixels::Rgb(rgb) => (rgb, 3, ExtendedColorType::Rgb8),
    PngPixels::Rgba(rgba) => (rgba, 4, ExtendedColorType::Rgba8),
  };
  // The encoder panics on a mismatched buffer; refuse it here instead.
  let expected = u128::from(width) * u128::from(height) * channels;
  if width == 0 || height == 0 || data.len() as u128 != expected {
    return Err(EncodingError::PngEncode(format!(
      "{} bytes for a {width}x{height} image with {channels} channels",
      data.len()
    )));
  }
  let mut writer = LimitedWriter {
    bytes: Vec::new(),
    limit,
    over: false,
  };
  let encoder = PngEncoder::new_with_quality(
    &mut writer,
    compression.to_compression_type(),
    compression.to_filter_type(),
  );
  let written = encoder.write_image(data, width, height, color);
  // Checked before the result: the end chunk is written when the encoder is dropped, which ignores
  // a refused write, so `written` can be Ok for a PNG that passed the limit on its last bytes.
  if writer.over {
    return Ok(PngOutcome::OverLimit);
  }
  match written {
    Ok(()) => Ok(PngOutcome::Encoded(writer.bytes)),
    Err(e) => Err(EncodingError::PngEncode(e.to_string())),
  }
}

/// Encode RGBA buffer to PNG with default compression.
#[allow(dead_code)]
#[inline]
pub fn encode_png_default<W: Write>(rgba: &[u8], width: u32, height: u32, writer: W) -> Result<()> {
  encode_png(rgba, width, height, writer, PngCompression::Default)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fast_compression_is_strengthened_only_up_to_one_megapixel() {
    assert_eq!(
      PngCompression::Fast.for_image(1000, 1000),
      PngCompression::Default
    );
    assert_eq!(
      PngCompression::Fast.for_image(1, 1),
      PngCompression::Default
    );
    assert_eq!(
      PngCompression::Fast.for_image(1001, 1000),
      PngCompression::Fast
    );
    assert_eq!(
      PngCompression::Fast.for_image(2560, 1440),
      PngCompression::Fast
    );
    // No overflow on the largest dimensions.
    assert_eq!(
      PngCompression::Fast.for_image(u32::MAX, u32::MAX),
      PngCompression::Fast
    );
    for kept in [PngCompression::Default, PngCompression::Best] {
      assert_eq!(kept.for_image(2560, 1440), kept);
      assert_eq!(kept.for_image(10, 10), kept);
    }
  }

  #[test]
  fn test_png_compression_from_level() {
    assert!(matches!(
      PngCompression::from_level(0),
      PngCompression::Fast
    ));
    assert!(matches!(
      PngCompression::from_level(2),
      PngCompression::Fast
    ));
    assert!(matches!(
      PngCompression::from_level(5),
      PngCompression::Default
    ));
    assert!(matches!(
      PngCompression::from_level(9),
      PngCompression::Best
    ));
  }

  #[test]
  fn test_png_encode_basic() {
    // 2x2 red square
    let rgba = [255u8, 0, 0, 255].repeat(4);
    let mut output = Vec::new();

    encode_png(&rgba, 2, 2, &mut output, PngCompression::Fast).expect("PNG encode failed");

    // Verify PNG magic bytes
    assert!(output.starts_with(&[0x89, 0x50, 0x4E, 0x47]));
  }

  /// Opaque screenshot-like pixels: flat panels, stripes and a gradient, as UI and text produce.
  fn screen_like(width: u32, height: u32, alpha: impl Fn(u32, u32) -> u8) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
      for x in 0..width {
        let stripe = if (x / 3 + y / 5) % 4 == 0 { 30 } else { 230 };
        let gradient = ((x * 255) / width) as u8;
        rgba.extend_from_slice(&[stripe, gradient, (y % 256) as u8, alpha(x, y)]);
      }
    }
    rgba
  }

  #[test]
  fn opaque_images_are_written_as_rgb_with_identical_pixels_and_fewer_bytes() {
    let (width, height) = (320, 180);
    let rgba = screen_like(width, height, |_, _| 255);
    for compression in [
      PngCompression::Fast,
      PngCompression::Default,
      PngCompression::Best,
    ] {
      let mut output = Vec::new();
      encode_png(&rgba, width, height, &mut output, compression).expect("PNG encode");
      let decoded = image::load_from_memory(&output).expect("decode");
      assert_eq!(decoded.color(), image::ColorType::Rgb8, "{compression:?}");
      assert_eq!(decoded.to_rgba8().into_raw(), rgba, "{compression:?}");
    }
    // The former encoding: RGBA with the Sub filter at Fast compression.
    let mut former = Vec::new();
    PngEncoder::new_with_quality(&mut former, PngCompressionType::Fast, PngFilterType::Sub)
      .write_image(&rgba, width, height, ExtendedColorType::Rgba8)
      .expect("former encode");
    let mut output = Vec::new();
    encode_png(&rgba, width, height, &mut output, PngCompression::Fast).expect("PNG encode");
    assert!(
      output.len() < former.len(),
      "{} vs {}",
      output.len(),
      former.len()
    );
  }

  #[test]
  fn a_translucent_pixel_keeps_the_alpha_channel() {
    let (width, height) = (64, 32);
    let rgba = screen_like(
      width,
      height,
      |x, y| if (x, y) == (63, 31) { 254 } else { 255 },
    );
    let mut output = Vec::new();
    encode_png(&rgba, width, height, &mut output, PngCompression::Fast).expect("PNG encode");
    let decoded = image::load_from_memory(&output).expect("decode");
    assert_eq!(decoded.color(), image::ColorType::Rgba8);
    assert_eq!(decoded.to_rgba8().into_raw(), rgba);
  }

  #[test]
  fn opaque_rgb_drops_only_alpha_bytes() {
    assert_eq!(
      opaque_rgb(&[1, 2, 3, 255, 4, 5, 6, 255]),
      Some(vec![1, 2, 3, 4, 5, 6])
    );
    assert_eq!(opaque_rgb(&[1, 2, 3, 255, 4, 5, 6, 0]), None);
    assert_eq!(opaque_rgb(&[]), Some(Vec::new()));
  }

  #[test]
  fn test_png_invalid_dimensions() {
    let rgba = vec![0u8; 100]; // Wrong size for any reasonable dimensions
    let mut output = Vec::new();

    let result = encode_png(&rgba, 10, 10, &mut output, PngCompression::Default);

    assert!(matches!(
      result,
      Err(EncodingError::InvalidDimensions { .. })
    ));
  }

  /// Gradient plus xorshift noise, opaque: compresses poorly enough to test size limits.
  fn noisy_rgba(width: u32, height: u32) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..width * height)
      .flat_map(|i| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        [
          (i % 256) as u8,
          (state >> 56) as u8,
          (state >> 48) as u8,
          255,
        ]
      })
      .collect()
  }

  #[test]
  fn a_limited_png_is_identical_when_it_fits_and_refused_one_byte_over() {
    let (width, height) = (96, 64);
    let rgba = noisy_rgba(width, height);
    let mut full = Vec::new();
    encode_png(&rgba, width, height, &mut full, PngCompression::Fast).expect("PNG");
    let rgb = opaque_rgb(&rgba).expect("opaque");
    let limited = |limit| {
      encode_png_limited(
        PngPixels::Rgb(&rgb),
        width,
        height,
        PngCompression::Fast,
        limit,
      )
      .expect("not an encoding error")
    };
    assert_eq!(limited(full.len()), PngOutcome::Encoded(full.clone()));
    assert_eq!(limited(usize::MAX), PngOutcome::Encoded(full.clone()));
    assert_eq!(limited(full.len() - 1), PngOutcome::OverLimit);
    assert_eq!(limited(64), PngOutcome::OverLimit);
  }

  #[test]
  fn translucent_pixels_are_limited_as_rgba_and_real_errors_stay_errors() {
    let mut rgba = noisy_rgba(16, 16);
    rgba[3] = 7;
    assert!(!is_opaque(&rgba));
    assert!(is_opaque(&noisy_rgba(16, 16)));
    let mut full = Vec::new();
    encode_png(&rgba, 16, 16, &mut full, PngCompression::Default).expect("PNG");
    assert_eq!(
      encode_png_limited(
        PngPixels::Rgba(&rgba),
        16,
        16,
        PngCompression::Default,
        usize::MAX
      )
      .expect("PNG"),
      PngOutcome::Encoded(full)
    );
    // A buffer that does not match the size is an error, never "over the limit".
    let result = encode_png_limited(PngPixels::Rgb(&[0; 5]), 16, 16, PngCompression::Fast, 1);
    assert!(
      matches!(result, Err(EncodingError::PngEncode(_))),
      "{result:?}"
    );
  }
}
