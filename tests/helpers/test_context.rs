// Isolated native service test context and temporary capture fixtures.

use super::mock_state::MockAppState;
use flinch::config::Config;
use std::path::PathBuf;
use tempfile::TempDir;

/// Test context that manages temporary directories and mock state
pub struct TestContext {
  pub temp_dir: TempDir,
  pub mock_state: MockAppState,
}

impl TestContext {
  /// Create a new test context with default config
  pub fn new() -> Self {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_state = MockAppState::new();

    Self {
      temp_dir,
      mock_state,
    }
  }

  /// Create a new test context with custom config
  pub fn with_config(config: Config) -> Self {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_state = MockAppState::with_config(config);

    Self {
      temp_dir,
      mock_state,
    }
  }

  /// Get path within the temp directory
  pub fn temp_path(&self, filename: &str) -> PathBuf {
    self.temp_dir.path().join(filename)
  }

  /// Get the temp directory path
  pub fn temp_dir_path(&self) -> &std::path::Path {
    self.temp_dir.path()
  }

  /// Create a test file with given content
  pub fn create_test_file(&self, filename: &str, content: &[u8]) -> PathBuf {
    let path = self.temp_path(filename);
    std::fs::write(&path, content).expect("Failed to write test file");
    path
  }

  /// Create a test config file
  pub fn create_test_config(&self, config: &Config) -> PathBuf {
    let path = self.temp_path("test_config.json");
    let json = serde_json::to_string_pretty(config).expect("Failed to serialize config");
    std::fs::write(&path, json).expect("Failed to write config file");
    path
  }
}

impl Default for TestContext {
  fn default() -> Self {
    Self::new()
  }
}

/// Helper to create a minimal test RGBA image buffer
pub fn create_test_rgba_image(width: u32, height: u32) -> Vec<u8> {
  let size = (width * height * 4) as usize;
  let mut buffer = Vec::with_capacity(size);

  // Create a gradient pattern
  for y in 0..height {
    for x in 0..width {
      let r = ((x * 255) / width) as u8;
      let g = ((y * 255) / height) as u8;
      let b = 128u8;
      let a = 255u8;

      buffer.push(r);
      buffer.push(g);
      buffer.push(b);
      buffer.push(a);
    }
  }

  buffer
}

/// Helper to create a test PNG file
pub fn create_test_png(
  path: &std::path::Path,
  width: u32,
  height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
  let rgba = create_test_rgba_image(width, height);

  // Use image crate to encode PNG
  let img = image::RgbaImage::from_raw(width, height, rgba)
    .ok_or("Failed to create image from raw data")?;

  img.save(path)?;
  Ok(())
}

/// Mock event emitter for testing event-driven code
pub struct MockEventEmitter {
  events: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
}

impl MockEventEmitter {
  pub fn new() -> Self {
    Self {
      events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    }
  }

  /// Emit a mock event
  pub fn emit<T: serde::Serialize>(&self, event: &str, payload: T) {
    let mut events = self.events.lock().unwrap();
    let payload_value = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    events.push((event.to_string(), payload_value));
  }

  /// Get all emitted events
  pub fn get_events(&self) -> Vec<(String, serde_json::Value)> {
    self.events.lock().unwrap().clone()
  }

  /// Get events by name
  pub fn get_events_by_name(&self, event_name: &str) -> Vec<serde_json::Value> {
    self
      .events
      .lock()
      .unwrap()
      .iter()
      .filter(|(name, _)| name == event_name)
      .map(|(_, payload)| payload.clone())
      .collect()
  }

  /// Clear all events
  pub fn clear(&self) {
    self.events.lock().unwrap().clear();
  }

  /// Check if an event was emitted
  pub fn has_event(&self, event_name: &str) -> bool {
    self
      .events
      .lock()
      .unwrap()
      .iter()
      .any(|(name, _)| name == event_name)
  }

  /// Count events by name
  pub fn count_events(&self, event_name: &str) -> usize {
    self
      .events
      .lock()
      .unwrap()
      .iter()
      .filter(|(name, _)| name == event_name)
      .count()
  }
}

impl Default for MockEventEmitter {
  fn default() -> Self {
    Self::new()
  }
}

/// Helper for async test timing
pub async fn wait_for<F>(mut condition: F, timeout_ms: u64) -> bool
where
  F: FnMut() -> bool,
{
  let start = std::time::Instant::now();
  let timeout = std::time::Duration::from_millis(timeout_ms);

  while start.elapsed() < timeout {
    if condition() {
      return true;
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
  }

  false
}

/// Helper to run a test with timeout
pub async fn with_timeout<F, T>(future: F, timeout_ms: u64) -> Result<T, &'static str>
where
  F: std::future::Future<Output = T>,
{
  let timeout = tokio::time::Duration::from_millis(timeout_ms);
  tokio::time::timeout(timeout, future)
    .await
    .map_err(|_| "Test timed out")
}
