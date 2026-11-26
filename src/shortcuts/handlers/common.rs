//! Common infrastructure for shortcut handlers
//!
//! ## Purpose
//! Provides shared utilities used by all shortcut handlers, primarily the
//! debounce guard that prevents double-triggers.
//!
//! ## Debounce Pattern
//! All shortcut handlers use a 300ms debounce window. If a shortcut is triggered
//! within 300ms of the last trigger, the subsequent triggers are ignored. This
//! prevents issues with:
//! - Held keys that trigger multiple times
//! - Rapid double-presses
//! - OS key repeat

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Debounce interval for all shortcut handlers (300ms)
pub const DEBOUNCE_MS: u64 = 300;

/// RAII guard for debouncing shortcut triggers.
///
/// Each handler has its own static `Mutex<Option<Instant>>` that tracks the last
/// trigger time. When `try_acquire` succeeds, the caller should proceed with the
/// handler logic. If it returns `false`, the trigger should be ignored.
///
/// # Usage
/// ```ignore
/// static LAST_TRIGGER: Mutex<Option<Instant>> = Mutex::new(None);
/// if !debounce_check(&LAST_TRIGGER) {
///     return; // Debounced
/// }
/// // ... handler logic ...
/// ```
pub fn debounce_check(last_trigger: &Mutex<Option<Instant>>) -> bool {
  let mut trigger = match last_trigger.lock() {
    Ok(guard) => guard,
    Err(poisoned) => {
      tracing::error!(
        operation = "shortcut_debounce",
        "Debounce mutex poisoned; recovering"
      );
      poisoned.into_inner()
    },
  };
  if let Some(time) = *trigger
    && time.elapsed() < Duration::from_millis(DEBOUNCE_MS)
  {
    return false; // Still within debounce window
  }
  *trigger = Some(Instant::now());
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn debounce_allows_first_trigger() {
    let last_trigger = Mutex::new(None);

    assert!(debounce_check(&last_trigger));
    assert!(last_trigger.lock().expect("mutex should lock").is_some());
  }

  #[test]
  fn debounce_blocks_immediate_repeat_trigger() {
    let last_trigger = Mutex::new(Some(Instant::now()));

    assert!(!debounce_check(&last_trigger));
  }

  #[test]
  fn debounce_allows_trigger_after_interval_expires() {
    let last_trigger = Mutex::new(Some(
      Instant::now() - Duration::from_millis(DEBOUNCE_MS + 5),
    ));

    assert!(debounce_check(&last_trigger));
  }
}
