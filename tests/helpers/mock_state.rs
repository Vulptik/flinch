// tests/helpers/mock_state.rs
// Mock state structures for testing native operations

use flinch::commands::OverlayActivityState;
use flinch::config::Config;
use std::sync::{Arc, RwLock};

/// Mock application state for testing commands
pub struct MockAppState {
  pub config: Arc<RwLock<Config>>,
  pub overlay_activity: Arc<OverlayActivityState>,
}

impl MockAppState {
  /// Create a new mock app state with default values
  pub fn new() -> Self {
    Self::with_config(Config::default())
  }

  /// Create a new mock app state with custom config
  pub fn with_config(config: Config) -> Self {
    Self {
      config: Arc::new(RwLock::new(config)),
      overlay_activity: Arc::new(OverlayActivityState::new()),
    }
  }

  /// Get a clone of the current config
  pub fn get_config(&self) -> Config {
    self.config.read().unwrap().clone()
  }

  /// Update the config
  pub fn set_config(&self, config: Config) {
    let mut cfg = self.config.write().unwrap();
    *cfg = config;
  }

  /// Check if screenshot overlay is active
  pub fn is_screenshot_overlay_active(&self) -> bool {
    self.overlay_activity.is_screenshot_active()
  }

  /// Check if video area selection overlay is active
  pub fn is_video_area_selection_active(&self) -> bool {
    self.overlay_activity.is_video_area_selection_active()
  }

  /// Check if GIF overlay is active
  pub fn is_gif_overlay_active(&self) -> bool {
    self.overlay_activity.is_gif_active()
  }

  /// Check if any overlay is active
  pub fn is_any_overlay_active(&self) -> bool {
    self.overlay_activity.is_any_active()
  }
}

impl Default for MockAppState {
  fn default() -> Self {
    Self::new()
  }
}
