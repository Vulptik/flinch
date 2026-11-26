//! Per-app OS adapter double. Never creates a manager, registers a key or retains callbacks.
use super::*;

#[derive(Default)]
pub(crate) struct TestRegistry {
  registered: Mutex<HashMap<u32, Shortcut>>,
  failures: Mutex<HashMap<u32, String>>,
  calls: Mutex<Vec<Shortcut>>,
}
impl TestRegistry {
  pub(crate) fn reject(&self, shortcut: Shortcut, error: &str) {
    self
      .failures
      .lock()
      .unwrap()
      .insert(shortcut.id(), error.into());
  }
  pub(crate) fn calls(&self) -> Vec<Shortcut> {
    self.calls.lock().unwrap().clone()
  }
  pub(super) fn register(&self, shortcut: Shortcut) -> Result<(), String> {
    self.calls.lock().unwrap().push(shortcut);
    if let Some(error) = self.failures.lock().unwrap().get(&shortcut.id()) {
      return Err(error.clone());
    }
    let mut registered = self.registered.lock().unwrap();
    if registered.contains_key(&shortcut.id()) {
      return Err("Native hotkey is already registered".into());
    }
    registered.insert(shortcut.id(), shortcut);
    Ok(())
  }
  pub(super) fn remove(&self, shortcut: Shortcut) {
    self.registered.lock().unwrap().remove(&shortcut.id());
  }
  pub(super) fn clear(&self) {
    self.registered.lock().unwrap().clear();
  }
  pub(super) fn contains(&self, shortcut: Shortcut) -> bool {
    self.registered.lock().unwrap().contains_key(&shortcut.id())
  }
}
