// src/shortcuts/state.rs
// Shortcut registration state tracking

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// The registration status of a shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutStatus {
  /// Shortcut is registered and active
  Registered,
  /// Shortcut could not be registered (already in use by OS or another app)
  Conflicted,
  /// Shortcut is intentionally empty/disabled by user
  Disabled,
}

/// Information about a single shortcut's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutInfo {
  /// The shortcut key combination (e.g., "Ctrl+PrintScreen")
  pub shortcut: String,
  /// Current registration status
  pub status: ShortcutStatus,
  /// Human-readable label for this shortcut type
  pub label: String,
}

/// Canonical shortcut type identifiers matching config keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutType {
  ScreenshotArea,
  ScreenshotFullscreen,
  ScreenshotActiveWindow,
  ScreenshotEdit,
  CopyText,
  PickColor,
  LastRegion,
  RecordArea,
  RecordFullscreen,
  RecordActiveWindow,
  Gif,
}

impl ShortcutType {
  pub fn config_key(&self) -> &'static str {
    crate::shortcuts::registry::definition_for(*self).config_key
  }

  pub fn human_label(&self) -> &'static str {
    crate::shortcuts::registry::definition_for(*self).human_label
  }
}

/// Thread-safe state holder for shortcut registration status.
/// Owned by the typed application context.
#[derive(Debug)]
pub struct ShortcutStateHolder {
  inner: Arc<RwLock<HashMap<ShortcutType, ShortcutInfo>>>,
  /// Whether the startup conflict notification has been shown.
  /// Only show the notification once per app session.
  startup_notification_shown: std::sync::atomic::AtomicBool,
  /// Number of callers currently requesting shortcuts to be disabled.
  /// Shortcuts are only re-registered when this counter returns to 0.
  /// This allows multiple windows (e.g. the settings window) to independently
  /// manage the disabled state without stomping each other.
  disable_depth: std::sync::atomic::AtomicI32,
}

impl Default for ShortcutStateHolder {
  fn default() -> Self {
    Self::new()
  }
}

impl ShortcutStateHolder {
  pub fn new() -> Self {
    Self {
      inner: Arc::new(RwLock::new(HashMap::new())),
      startup_notification_shown: std::sync::atomic::AtomicBool::new(false),
      disable_depth: std::sync::atomic::AtomicI32::new(0),
    }
  }

  /// Check if the startup notification has been shown, and mark it as shown.
  /// Returns true if this is the first call (notification should be shown),
  /// false if it was already shown.
  pub fn try_mark_startup_notification_shown(&self) -> bool {
    !self
      .startup_notification_shown
      .swap(true, std::sync::atomic::Ordering::SeqCst)
  }

  /// Increment the disable depth counter.
  /// Returns the new depth value. When the returned value is 1 the caller
  /// is the first requester and should actually unregister shortcuts.
  pub fn increment_disable_depth(&self) -> i32 {
    self
      .disable_depth
      .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
      + 1
  }

  /// Atomically reset the disable depth to 0. Returns the previous depth value.
  /// Use only for emergency cleanup (e.g. when the settings window is destroyed
  /// externally via Alt+F4, bypassing ordinary owner cleanup).
  pub fn drain_disable_depth(&self) -> i32 {
    self
      .disable_depth
      .swap(0, std::sync::atomic::Ordering::SeqCst)
  }

  /// Decrement the disable depth counter (floor 0).
  /// Returns the new depth value. When the returned value is 0 the caller
  /// was the last requester and should actually re-register shortcuts.
  pub fn decrement_disable_depth(&self) -> i32 {
    match self.disable_depth.try_update(
      std::sync::atomic::Ordering::SeqCst,
      std::sync::atomic::Ordering::SeqCst,
      |v| if v > 0 { Some(v - 1) } else { None },
    ) {
      Ok(old) => old - 1,
      Err(current) => current,
    }
  }

  /// Returns true if at least one caller has requested shortcuts to be disabled.
  pub fn is_shortcuts_disabled(&self) -> bool {
    self.disable_depth.load(std::sync::atomic::Ordering::SeqCst) > 0
  }

  /// Update the state for a specific shortcut type.
  pub fn set(&self, shortcut_type: ShortcutType, shortcut: String, status: ShortcutStatus) {
    if let Ok(mut map) = self.inner.write() {
      map.insert(
        shortcut_type,
        ShortcutInfo {
          shortcut,
          status,
          label: shortcut_type.human_label().to_string(),
        },
      );
    }
    crate::settings::native_host::shortcuts_changed();
  }

  /// Get all shortcut states.
  pub fn get_all(&self) -> HashMap<ShortcutType, ShortcutInfo> {
    self.inner.read().map(|map| map.clone()).unwrap_or_default()
  }

  /// Clear all state (used when re-registering all shortcuts).
  pub fn clear(&self) {
    if let Ok(mut map) = self.inner.write() {
      map.clear();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_shortcut_status_equality() {
    assert_eq!(ShortcutStatus::Registered, ShortcutStatus::Registered);
    assert_ne!(ShortcutStatus::Registered, ShortcutStatus::Conflicted);
  }

  #[test]
  fn test_shortcut_type_config_key() {
    assert_eq!(
      ShortcutType::ScreenshotArea.config_key(),
      "screenshot_area_shortcut"
    );
    assert_eq!(
      ShortcutType::RecordFullscreen.config_key(),
      "record_fullscreen_shortcut"
    );
    assert_eq!(ShortcutType::Gif.config_key(), "gif_shortcut");
  }

  #[test]
  fn test_shortcut_type_human_label() {
    assert_eq!(
      ShortcutType::ScreenshotArea.human_label(),
      "Screenshot area"
    );
    assert_eq!(ShortcutType::RecordArea.human_label(), "Record area");
    assert_eq!(ShortcutType::Gif.human_label(), "Record GIF");
  }

  #[test]
  fn test_shortcut_state_holder_new() {
    let holder = ShortcutStateHolder::new();
    assert!(holder.get_all().is_empty());
  }

  #[test]
  fn test_shortcut_state_holder_get_all() {
    let holder = ShortcutStateHolder::new();

    holder.set(
      ShortcutType::ScreenshotArea,
      "Ctrl+S".to_string(),
      ShortcutStatus::Registered,
    );
    holder.set(
      ShortcutType::RecordArea,
      "Ctrl+R".to_string(),
      ShortcutStatus::Conflicted,
    );

    let all = holder.get_all();
    assert_eq!(all.len(), 2);
    assert!(all.contains_key(&ShortcutType::ScreenshotArea));
    assert!(all.contains_key(&ShortcutType::RecordArea));
  }

  #[test]
  fn test_shortcut_state_holder_clear() {
    let holder = ShortcutStateHolder::new();

    holder.set(
      ShortcutType::ScreenshotArea,
      "Ctrl+S".to_string(),
      ShortcutStatus::Registered,
    );
    holder.set(
      ShortcutType::RecordArea,
      "Ctrl+R".to_string(),
      ShortcutStatus::Registered,
    );

    assert_eq!(holder.get_all().len(), 2);

    holder.clear();
    assert!(holder.get_all().is_empty());
  }

  #[test]
  fn test_startup_notification_shown() {
    let holder = ShortcutStateHolder::new();

    // First call should return true (should show)
    assert!(holder.try_mark_startup_notification_shown());

    // Second call should return false (already shown)
    assert!(!holder.try_mark_startup_notification_shown());
    assert!(!holder.try_mark_startup_notification_shown());
  }

  #[test]
  fn test_shortcut_info_clone() {
    let info = ShortcutInfo {
      shortcut: "Ctrl+S".to_string(),
      status: ShortcutStatus::Registered,
      label: "Test".to_string(),
    };

    let cloned = info.clone();
    assert_eq!(cloned.shortcut, "Ctrl+S");
    assert_eq!(cloned.status, ShortcutStatus::Registered);
    assert_eq!(cloned.label, "Test");
  }
}
