// tests/helpers/fixtures.rs
// Test data fixtures and factories

use flinch::config::{Config, ScreenshotFormat};
use std::path::PathBuf;

/// Create a default test config
pub fn default_config() -> Config {
  Config::default()
}

/// Create a config with PNG format
pub fn config_with_png() -> Config {
  Config {
    screenshot_format: ScreenshotFormat::Png,
    screenshot_quality: 90,
    ..Default::default()
  }
}

/// Create a config with JPEG format
pub fn config_with_jpeg() -> Config {
  Config {
    screenshot_format: ScreenshotFormat::Jpg,
    screenshot_quality: 85,
    ..Default::default()
  }
}

/// Generate test RGBA image data (gradient pattern)
pub fn test_rgba_image(width: u32, height: u32) -> Vec<u8> {
  (0..(width * height))
    .flat_map(|i| {
      let x = i % width;
      let y = i / width;
      let r = ((x * 255) / width.max(1)) as u8;
      let g = ((y * 255) / height.max(1)) as u8;
      let b = (((x + y) * 128) / (width + height).max(1)) as u8;
      [r, g, b, 255]
    })
    .collect()
}

/// Generate solid color RGBA image
pub fn test_rgba_solid(width: u32, height: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
  [r, g, b, 255].repeat((width * height) as usize)
}

/// Get a unique test file path in temp directory
pub fn temp_test_file(prefix: &str, extension: &str) -> PathBuf {
  use std::sync::atomic::{AtomicUsize, Ordering};
  static COUNTER: AtomicUsize = AtomicUsize::new(0);

  let id = COUNTER.fetch_add(1, Ordering::Relaxed);
  let timestamp = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();

  std::env::temp_dir().join(format!(
    "flinch_test_{}_{}.{}",
    prefix,
    timestamp + id as u64,
    extension
  ))
}

/// Create a temporary directory for tests
pub fn temp_test_dir(prefix: &str) -> PathBuf {
  use std::sync::atomic::{AtomicUsize, Ordering};
  static COUNTER: AtomicUsize = AtomicUsize::new(0);

  let id = COUNTER.fetch_add(1, Ordering::Relaxed);
  let timestamp = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();

  let dir = std::env::temp_dir().join(format!("flinch_test_{}_{}", prefix, timestamp + id as u64));

  std::fs::create_dir_all(&dir).expect("Failed to create temp directory");
  dir
}

/// Cleanup test files matching pattern
pub fn cleanup_test_files(pattern: &str) {
  let temp_dir = std::env::temp_dir();
  if let Ok(entries) = std::fs::read_dir(temp_dir) {
    for entry in entries.flatten() {
      if let Ok(name) = entry.file_name().into_string()
        && name.contains(pattern)
      {
        let _ = std::fs::remove_file(entry.path());
      }
    }
  }
}
