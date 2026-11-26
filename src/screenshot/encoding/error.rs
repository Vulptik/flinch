// src/screenshot/encoding/error.rs
// Granular error types for image encoding operations

use thiserror::Error;

#[derive(Error, Debug)]
pub enum EncodingError {
  #[error(
    "Invalid buffer dimensions: {width}x{height} with {buffer_len} bytes (expected {expected})"
  )]
  InvalidDimensions {
    width: u32,
    height: u32,
    buffer_len: usize,
    expected: u128,
  },

  #[error("PNG encoding failed: {0}")]
  PngEncode(String),

  #[error("JPEG encoding failed: {0}")]
  JpegEncode(String),

  #[error("WebP encoding failed: {0}")]
  WebpEncode(String),

  #[error("File I/O failed for '{path}': {source}")]
  IoError {
    path: String,
    #[source]
    source: std::io::Error,
  },

  #[error("Unsupported format: {0}")]
  UnsupportedFormat(String),

  #[error("Invalid screenshot crop: {0}")]
  InvalidCrop(String),
}

impl EncodingError {
  /// Create an InvalidDimensions error with computed expected size
  pub fn invalid_dimensions(width: u32, height: u32, buffer_len: usize) -> Self {
    Self::InvalidDimensions {
      width,
      height,
      buffer_len,
      expected: u128::from(width) * u128::from(height) * 4,
    }
  }

  /// Create an IoError with path context
  pub fn io_error<P: AsRef<std::path::Path>>(path: P, source: std::io::Error) -> Self {
    Self::IoError {
      path: path.as_ref().display().to_string(),
      source,
    }
  }
}

pub type Result<T> = std::result::Result<T, EncodingError>;

pub(crate) fn validate_rgba(width: u32, height: u32, len: usize) -> Result<()> {
  let expected = u128::from(width) * u128::from(height) * 4;
  if width == 0 || height == 0 || expected > isize::MAX as u128 || expected != len as u128 {
    return Err(EncodingError::invalid_dimensions(width, height, len));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_invalid_dimensions_display() {
    let err = EncodingError::invalid_dimensions(100, 200, 1000);
    let msg = err.to_string();
    assert!(msg.contains("100"));
    assert!(msg.contains("200"));
    assert!(msg.contains("1000"));
    // expected = 100 * 200 * 4 = 80000
    assert!(msg.contains("80000"));
  }

  #[test]
  fn test_invalid_dimensions_computes_expected() {
    // Verify the expected field is computed as width * height * 4
    let err = EncodingError::invalid_dimensions(10, 20, 50);
    if let EncodingError::InvalidDimensions { expected, .. } = err {
      assert_eq!(expected, 10 * 20 * 4);
    } else {
      panic!("Expected InvalidDimensions variant");
    }
  }

  #[test]
  fn test_png_encode_error_display() {
    let err = EncodingError::PngEncode("deflate error".to_string());
    assert!(err.to_string().contains("PNG encoding failed"));
    assert!(err.to_string().contains("deflate error"));
  }

  #[test]
  fn test_jpeg_encode_error_display() {
    let err = EncodingError::JpegEncode("quality out of range".to_string());
    assert!(err.to_string().contains("JPEG encoding failed"));
    assert!(err.to_string().contains("quality out of range"));
  }

  #[test]
  fn test_webp_encode_error_display() {
    let err = EncodingError::WebpEncode("invalid dimensions".to_string());
    assert!(err.to_string().contains("WebP encoding failed"));
    assert!(err.to_string().contains("invalid dimensions"));
  }

  #[test]
  fn test_unsupported_format_display() {
    let err = EncodingError::UnsupportedFormat("bmp".to_string());
    assert!(err.to_string().contains("Unsupported format"));
    assert!(err.to_string().contains("bmp"));
  }

  #[test]
  fn test_io_error_display() {
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
    let err = EncodingError::io_error("/path/to/file.png", io_err);
    let msg = err.to_string();
    assert!(msg.contains("/path/to/file.png"));
    assert!(msg.contains("File I/O failed"));
  }

  #[test]
  fn test_io_error_path_constructor() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
    let err = EncodingError::io_error(std::path::Path::new("/tmp/test.png"), io_err);
    assert!(err.to_string().contains("/tmp/test.png"));
  }
}
