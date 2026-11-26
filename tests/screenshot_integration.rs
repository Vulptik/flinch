// tests/screenshot_integration.rs
// Screenshot integration tests

mod helpers;

use flinch::screenshot::save_rgba_image;
use helpers::test_context::{TestContext, create_test_rgba_image};
use std::fs;

/// Test screenshot encoding to PNG
#[tokio::test]
async fn test_screenshot_encode_png() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(100, 100);

  let screenshot_path = ctx.temp_path("screenshot.png");

  let result = save_rgba_image(&screenshot_path, 100, 100, &rgba, "png", 95);

  assert!(result.is_ok(), "Failed to encode PNG: {:?}", result.err());
  assert!(screenshot_path.exists(), "PNG file not created");

  let metadata = fs::metadata(&screenshot_path).expect("Failed to read metadata");
  assert!(metadata.len() > 0, "PNG file is empty");
}

/// Test screenshot encoding to JPEG
#[tokio::test]
async fn test_screenshot_encode_jpeg() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(100, 100);

  let screenshot_path = ctx.temp_path("screenshot.jpg");

  let result = save_rgba_image(&screenshot_path, 100, 100, &rgba, "jpeg", 85);

  assert!(result.is_ok(), "Failed to encode JPEG: {:?}", result.err());
  assert!(screenshot_path.exists(), "JPEG file not created");

  let metadata = fs::metadata(&screenshot_path).expect("Failed to read metadata");
  assert!(metadata.len() > 0, "JPEG file is empty");
}

/// Test screenshot encoding to WebP
#[tokio::test]
async fn test_screenshot_encode_webp() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(100, 100);

  let screenshot_path = ctx.temp_path("screenshot.webp");

  let result = save_rgba_image(&screenshot_path, 100, 100, &rgba, "webp", 90);

  assert!(result.is_ok(), "Failed to encode WebP: {:?}", result.err());
  assert!(screenshot_path.exists(), "WebP file not created");

  let metadata = fs::metadata(&screenshot_path).expect("Failed to read metadata");
  assert!(metadata.len() > 0, "WebP file is empty");
}

/// Test screenshot quality settings affect file size
#[tokio::test]
async fn test_screenshot_quality_affects_size() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(200, 200);

  // High quality
  let high_quality_path = ctx.temp_path("high_quality.jpg");
  save_rgba_image(&high_quality_path, 200, 200, &rgba, "jpeg", 100)
    .expect("Failed to save high quality");

  // Low quality
  let low_quality_path = ctx.temp_path("low_quality.jpg");
  save_rgba_image(&low_quality_path, 200, 200, &rgba, "jpeg", 50)
    .expect("Failed to save low quality");

  let high_size = fs::metadata(&high_quality_path).unwrap().len();
  let low_size = fs::metadata(&low_quality_path).unwrap().len();

  assert!(
    high_size > low_size,
    "High quality file should be larger than low quality"
  );
}

/// Test different screenshot dimensions
#[tokio::test]
async fn test_screenshot_various_dimensions() {
  let ctx = TestContext::new();
  let dimensions = vec![
    (100, 100),   // Small square
    (1920, 1080), // Full HD
    (3840, 2160), // 4K
    (800, 600),   // 4:3 aspect
  ];

  for (width, height) in dimensions {
    let rgba = create_test_rgba_image(width, height);
    let screenshot_path = ctx.temp_path(&format!("screenshot_{}x{}.png", width, height));

    let result = save_rgba_image(&screenshot_path, width, height, &rgba, "png", 95);

    assert!(
      result.is_ok(),
      "Failed to encode {}x{}: {:?}",
      width,
      height,
      result.err()
    );
    assert!(
      screenshot_path.exists(),
      "File not created for {}x{}",
      width,
      height
    );
  }
}

/// Test screenshot with various quality values
#[tokio::test]
async fn test_screenshot_quality_range() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(100, 100);
  let qualities = vec![0, 25, 50, 75, 100];

  for quality in qualities {
    let screenshot_path = ctx.temp_path(&format!("quality_{}.jpg", quality));

    let result = save_rgba_image(&screenshot_path, 100, 100, &rgba, "jpeg", quality);

    assert!(
      result.is_ok(),
      "Failed to encode with quality {}: {:?}",
      quality,
      result.err()
    );
    assert!(
      screenshot_path.exists(),
      "File not created for quality {}",
      quality
    );
  }
}

/// Test PNG encoding with different compression levels
#[tokio::test]
async fn test_png_compression_levels() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(100, 100);

  // PNG quality parameter maps to compression level
  // Higher compression (quality 0) = smaller file, slower
  // Lower compression (quality 100) = larger file, faster
  let path_high_compression = ctx.temp_path("png_high_compression.png");
  let path_low_compression = ctx.temp_path("png_low_compression.png");

  save_rgba_image(&path_high_compression, 100, 100, &rgba, "png", 0)
    .expect("Failed to save PNG with high compression");

  save_rgba_image(&path_low_compression, 100, 100, &rgba, "png", 100)
    .expect("Failed to save PNG with low compression");

  let size_high = fs::metadata(&path_high_compression).unwrap().len();
  let size_low = fs::metadata(&path_low_compression).unwrap().len();

  // High compression should result in smaller file
  // (Quality 0 = best compression, Quality 100 = fast compression)
  assert!(
    size_high < size_low,
    "High compression should produce smaller file"
  );
}

/// Test screenshot format extensions
#[tokio::test]
async fn test_screenshot_format_extensions() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(50, 50);

  let formats = vec![
    ("png", "test.png"),
    ("jpeg", "test-jpeg.jpg"),
    ("jpg", "test.jpg"),
    ("webp", "test.webp"),
  ];

  for (format, filename) in formats {
    let screenshot_path = ctx.temp_path(filename);

    let result = save_rgba_image(&screenshot_path, 50, 50, &rgba, format, 85);

    assert!(
      result.is_ok(),
      "Failed to encode format {}: {:?}",
      format,
      result.err()
    );
    assert!(
      screenshot_path.exists(),
      "File not created for format {}",
      format
    );
  }
}

/// Test encoding with zero dimensions (should handle gracefully)
#[tokio::test]
async fn test_screenshot_zero_dimensions() {
  let ctx = TestContext::new();
  let rgba = vec![]; // Empty buffer

  let screenshot_path = ctx.temp_path("zero_dim.png");

  let result = save_rgba_image(&screenshot_path, 0, 0, &rgba, "png", 95);

  assert!(result.is_err(), "zero-sized capture must be rejected");
  assert!(
    !screenshot_path.exists(),
    "invalid capture must not publish an image"
  );
}

/// Test encoding with invalid buffer size
#[tokio::test]
async fn test_screenshot_invalid_buffer_size() {
  let ctx = TestContext::new();

  // Buffer too small for the specified dimensions
  let rgba = vec![255u8; 100]; // Only 100 bytes, but claiming 100x100 (needs 40000)
  let screenshot_path = ctx.temp_path("invalid_buffer.png");

  let result = save_rgba_image(&screenshot_path, 100, 100, &rgba, "png", 95);

  // Should fail (buffer size doesn't match dimensions)
  assert!(result.is_err(), "Should fail with invalid buffer size");
}

/// Test screenshot with extreme dimensions
#[tokio::test]
async fn test_screenshot_large_dimensions() {
  let ctx = TestContext::new();

  // Very large screenshot (7680x4320 = 8K)
  // Only create if we have enough memory
  let width = 7680u32;
  let height = 4320u32;
  let buffer_size = (width * height * 4) as usize;

  // Skip test if we can't allocate the memory
  if buffer_size > 500_000_000 {
    // Skip if > 500MB
    return;
  }

  let rgba = create_test_rgba_image(width, height);
  let screenshot_path = ctx.temp_path("8k_screenshot.png");

  let result = save_rgba_image(&screenshot_path, width, height, &rgba, "png", 95);

  assert!(
    result.is_ok(),
    "Failed to encode 8K screenshot: {:?}",
    result.err()
  );
  assert!(screenshot_path.exists(), "8K screenshot file not created");
}

/// Test rapid successive screenshots (file locking/concurrent access)
#[tokio::test]
async fn test_rapid_successive_screenshots() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(100, 100);

  let count = 10;
  for i in 0..count {
    let screenshot_path = ctx.temp_path(&format!("rapid_{}.png", i));

    let result = save_rgba_image(&screenshot_path, 100, 100, &rgba, "png", 95);

    assert!(
      result.is_ok(),
      "Failed rapid screenshot {}: {:?}",
      i,
      result.err()
    );
    assert!(
      screenshot_path.exists(),
      "Rapid screenshot {} not created",
      i
    );
  }
}

/// Test screenshot filename sanitization (special characters)
#[tokio::test]
async fn test_screenshot_special_filenames() {
  let ctx = TestContext::new();
  let rgba = create_test_rgba_image(50, 50);

  // Test various valid filenames
  let filenames = vec![
    "screenshot-with-dashes.png",
    "screenshot_with_underscores.png",
    "screenshot.multiple.dots.png",
    "screenshot (1).png",
    "screenshot [copy].png",
  ];

  for filename in filenames {
    let screenshot_path = ctx.temp_path(filename);

    let result = save_rgba_image(&screenshot_path, 50, 50, &rgba, "png", 95);

    assert!(
      result.is_ok(),
      "Failed with filename '{}': {:?}",
      filename,
      result.err()
    );
    assert!(
      screenshot_path.exists(),
      "File not created for filename '{}'",
      filename
    );
  }
}
