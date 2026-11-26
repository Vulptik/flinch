// tests/helpers/assertions.rs
// Custom assertions for test validation

use std::path::Path;

/// Assert that a Result<T, String> is Ok
pub fn assert_result_ok<T>(result: Result<T, String>) {
  match result {
    Ok(_) => {},
    Err(e) => panic!("Expected Ok but got Err: {}", e),
  }
}

/// Assert that a Result<T, String> is Err
pub fn assert_result_err<T: std::fmt::Debug>(result: Result<T, String>) {
  if let Ok(v) = result {
    panic!("Expected Err but got Ok: {:?}", v)
  }
}

/// Assert that a Result<T, String> is Err and contains expected message
pub fn assert_result_err_contains<T: std::fmt::Debug>(
  result: Result<T, String>,
  expected_msg: &str,
) {
  match result {
    Ok(v) => panic!("Expected Err but got Ok: {:?}", v),
    Err(e) => {
      assert!(
        e.contains(expected_msg),
        "Error message '{}' does not contain expected '{}'",
        e,
        expected_msg
      );
    },
  }
}

/// Assert that a file exists
pub fn assert_file_exists(path: &Path) {
  assert!(
    path.exists(),
    "Expected file to exist at path: {}",
    path.display()
  );
}

/// Assert that a file has minimum size
pub fn assert_file_min_size(path: &Path, min_bytes: u64) {
  let metadata = std::fs::metadata(path).expect("Failed to get file metadata");
  let size = metadata.len();
  assert!(
    size >= min_bytes,
    "File size {} is less than minimum {}",
    size,
    min_bytes
  );
}

/// Assert that a file does not exist
pub fn assert_file_not_exists(path: &Path) {
  assert!(
    !path.exists(),
    "Expected file to not exist at path: {}",
    path.display()
  );
}

/// Assert that a directory exists
pub fn assert_dir_exists(path: &Path) {
  assert!(
    path.exists() && path.is_dir(),
    "Expected directory to exist at path: {}",
    path.display()
  );
}

/// Assert that an image file is valid (basic check)
pub fn assert_valid_image_file(path: &Path) {
  assert_file_exists(path);

  // Check file extension
  let ext = path
    .extension()
    .and_then(|e| e.to_str())
    .expect("File should have extension");

  assert!(
    matches!(ext, "png" | "jpg" | "jpeg" | "webp"),
    "Expected image extension (png/jpg/webp), got: {}",
    ext
  );

  // Check minimum file size (10 bytes for basic test files)
  assert_file_min_size(path, 10);
}

/// Assert that a GIF file is valid (basic check)
pub fn assert_valid_gif_file(path: &Path) {
  assert_file_exists(path);

  // Check file extension
  let ext = path
    .extension()
    .and_then(|e| e.to_str())
    .expect("File should have extension");

  assert_eq!(ext, "gif", "Expected .gif extension, got: {}", ext);

  // Check minimum file size (10 bytes for basic test files)
  assert_file_min_size(path, 10);

  // Check GIF magic bytes
  let mut file = std::fs::File::open(path).expect("Failed to open file");
  let mut magic = [0u8; 6];
  use std::io::Read;
  file
    .read_exact(&mut magic)
    .expect("Failed to read magic bytes");

  assert!(
    magic.starts_with(b"GIF87a") || magic.starts_with(b"GIF89a"),
    "Invalid GIF magic bytes"
  );
}
