//! Native hotkey ownership, with bounded dispatch to the platform owner. The
//! Slint dispatch is the owner-thread boundary.

use crate::app::context::AppHandle;
#[cfg(feature = "native-settings")]
pub use global_hotkey::HotKeyState as ShortcutState;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
pub use global_hotkey::{GlobalHotKeyEvent as ShortcutEvent, hotkey::HotKey as Shortcut};
use std::{
  cell::RefCell,
  collections::HashMap,
  sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
  },
  time::Duration,
};
#[cfg(test)]
mod test_registry;
#[cfg(test)]
pub(crate) use test_registry::TestRegistry;

type Handler = Arc<dyn Fn(ShortcutEvent) + Send + Sync>;
#[derive(Clone)]
struct Entry {
  shortcut: Shortcut,
  handler: Handler,
}
static ENTRIES: LazyLock<Mutex<HashMap<u32, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static OWNER: AtomicU32 = AtomicU32::new(0);
static PENDING: AtomicUsize = AtomicUsize::new(0);
thread_local! { static MANAGER: RefCell<Option<GlobalHotKeyManager>> = const { RefCell::new(None) }; }

pub fn initialize() -> Result<(), String> {
  let manager = GlobalHotKeyManager::new().map_err(|error| error.to_string())?;
  // SAFETY: thread identity is queried without transferring window ownership.
  let owner = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
  OWNER
    .compare_exchange(0, owner, Ordering::AcqRel, Ordering::Acquire)
    .map_err(|_| "Native hotkey owner already initialized")?;
  MANAGER.with(|slot| *slot.borrow_mut() = Some(manager));
  GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
    let handler = match ENTRIES.lock() {
      Ok(entries) => entries.get(&event.id).map(|entry| entry.handler.clone()),
      Err(error) => {
        tracing::error!(%error, "Cannot dispatch native hotkey");
        None
      },
    };
    // Never invoke capture callbacks while retaining the registry lock.
    if let Some(handler) = handler {
      handler(event);
    }
  }));
  Ok(())
}

pub fn shutdown() {
  // SAFETY: owner thread identity is read before taking its window-owning manager.
  let owner = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
  if OWNER.load(Ordering::Acquire) != owner {
    tracing::error!("Native hotkey shutdown reached the wrong thread");
    return;
  }
  if let Err(error) = remove_all() {
    tracing::error!(%error, "Cannot unregister every hotkey during shutdown");
  }
  MANAGER.with(|manager| {
    manager.borrow_mut().take();
  });
  let retired = match ENTRIES.lock() {
    Ok(mut entries) => Some(std::mem::take(&mut *entries)),
    Err(error) => {
      tracing::error!(%error, "Cannot clear native hotkey callbacks during shutdown");
      None
    },
  };
  drop(retired);
  OWNER.store(0, Ordering::Release);
}

fn with_manager<T>(
  operation: impl FnOnce(&GlobalHotKeyManager) -> Result<T, String>,
) -> Result<T, String> {
  MANAGER.with(|manager| {
    let manager = manager.borrow();
    operation(
      manager
        .as_ref()
        .ok_or("Native hotkey manager is unavailable")?,
    )
  })
}

fn remove(shortcut: Shortcut) -> Result<(), String> {
  remove_with(&ENTRIES, shortcut, |shortcut| {
    with_manager(|manager| {
      manager
        .unregister(shortcut)
        .map_err(|error| error.to_string())
    })
  })
}

fn remove_with(
  registry: &Mutex<HashMap<u32, Entry>>,
  shortcut: Shortcut,
  unregister: impl FnOnce(Shortcut) -> Result<(), String>,
) -> Result<(), String> {
  let exists = registry
    .lock()
    .map_err(|error| error.to_string())?
    .contains_key(&shortcut.id());
  if !exists {
    return Ok(());
  }
  unregister(shortcut)?;
  let retired = registry
    .lock()
    .map_err(|error| error.to_string())?
    .remove(&shortcut.id());
  // Handlers may own services whose destructors reenter this registry.
  // Release the registry guard before releasing the callback's last owner.
  drop(retired);
  Ok(())
}
fn remove_all() -> Result<(), String> {
  let shortcuts: Vec<_> = ENTRIES
    .lock()
    .map_err(|error| error.to_string())?
    .values()
    .map(|entry| entry.shortcut)
    .collect();
  let mut failures = Vec::new();
  for shortcut in shortcuts {
    if let Err(error) = remove(shortcut) {
      failures.push(error);
    }
  }
  if failures.is_empty() {
    Ok(())
  } else {
    Err(failures.join("\n"))
  }
}

struct Submission;
impl Drop for Submission {
  fn drop(&mut self) {
    PENDING.fetch_sub(1, Ordering::AcqRel);
  }
}
fn dispatch<T: Send + 'static>(
  app: &AppHandle,
  operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
  // SAFETY: read the current thread ID; direct execution prevents owner-loop deadlock.
  if unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
    == OWNER.load(Ordering::Acquire)
  {
    return operation();
  }
  enqueue(
    |task| {
      app
        .run_on_main_thread(task)
        .map_err(|error| error.to_string())
    },
    operation,
    Duration::from_secs(5),
  )
}

type OwnerTask = Box<dyn FnOnce() + Send>;
fn enqueue<T: Send + 'static>(
  submit: impl FnOnce(OwnerTask) -> Result<(), String>,
  operation: impl FnOnce() -> Result<T, String> + Send + 'static,
  timeout: Duration,
) -> Result<T, String> {
  PENDING
    .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
      (pending < 16).then_some(pending + 1)
    })
    .map_err(|_| "Native hotkey dispatch queue is full")?;
  let submission = Submission;
  let cancelled = Arc::new(AtomicBool::new(false));
  let owner_cancelled = cancelled.clone();
  let (tx, rx) = std::sync::mpsc::sync_channel(1);
  submit(Box::new(move || {
    let _submission = submission;
    if owner_cancelled.load(Ordering::Acquire) {
      return;
    }
    let result = operation();
    if let Err(error) = tx.try_send(result) {
      tracing::error!(%error, "Native hotkey caller no longer accepts its operation acknowledgment");
    }
  }))?;
  rx.recv_timeout(timeout).map_err(|error| {
    cancelled.store(true, Ordering::Release);
    format!("Native hotkey acknowledgment failed: {error}; an already-started OS operation remains owned by its platform thread")
  })?
}

pub struct ShortcutInput(Shortcut);
impl From<Shortcut> for ShortcutInput {
  fn from(value: Shortcut) -> Self {
    Self(value)
  }
}
impl TryFrom<&str> for ShortcutInput {
  type Error = global_hotkey::hotkey::HotKeyParseError;
  fn try_from(value: &str) -> Result<Self, Self::Error> {
    value.parse().map(Self)
  }
}
pub struct GlobalShortcut(AppHandle);
pub trait GlobalShortcutExt {
  fn global_shortcut(&self) -> GlobalShortcut;
}
impl GlobalShortcutExt for AppHandle {
  fn global_shortcut(&self) -> GlobalShortcut {
    GlobalShortcut(self.clone())
  }
}

impl GlobalShortcut {
  pub fn on_shortcut<S, F>(&self, shortcut: S, handler: F) -> Result<(), String>
  where
    S: TryInto<ShortcutInput>,
    S::Error: std::fmt::Display,
    F: Fn(&AppHandle, &Shortcut, ShortcutEvent) + Send + Sync + 'static,
  {
    let shortcut = shortcut.try_into().map_err(|error| error.to_string())?.0;
    #[cfg(test)]
    if let Some(registry) = self.0.test_hotkeys() {
      return registry.register(shortcut);
    }
    let app = self.0.clone();
    let entry = Entry {
      shortcut,
      handler: Arc::new(move |event| handler(&app, &shortcut, event)),
    };
    dispatch(&self.0, move || {
      if ENTRIES
        .lock()
        .map_err(|error| error.to_string())?
        .contains_key(&shortcut.id())
      {
        return Err("Native hotkey is already registered".into());
      }
      with_manager(|manager| {
        manager
          .register(shortcut)
          .map_err(|error| error.to_string())
      })?;
      match ENTRIES.lock() {
        Ok(mut entries) => {
          entries.insert(shortcut.id(), entry);
          Ok(())
        },
        Err(error) => {
          if let Err(cleanup) = with_manager(|manager| {
            manager
              .unregister(shortcut)
              .map_err(|error| error.to_string())
          }) {
            tracing::error!(%cleanup, "Cannot roll back unpublished native hotkey");
          }
          Err(error.to_string())
        },
      }
    })
  }
  pub fn unregister<S>(&self, shortcut: S) -> Result<(), String>
  where
    S: TryInto<ShortcutInput>,
    S::Error: std::fmt::Display,
  {
    let shortcut = shortcut.try_into().map_err(|error| error.to_string())?.0;
    #[cfg(test)]
    if let Some(registry) = self.0.test_hotkeys() {
      registry.remove(shortcut);
      return Ok(());
    }
    dispatch(&self.0, move || remove(shortcut))
  }
  pub fn unregister_all(&self) -> Result<(), String> {
    #[cfg(test)]
    if let Some(registry) = self.0.test_hotkeys() {
      registry.clear();
      return Ok(());
    }
    dispatch(&self.0, remove_all)
  }
  pub fn is_registered(&self, shortcut: &str) -> Result<bool, String> {
    let shortcut: Shortcut = shortcut
      .parse()
      .map_err(|error: global_hotkey::hotkey::HotKeyParseError| error.to_string())?;
    #[cfg(test)]
    if let Some(registry) = self.0.test_hotkeys() {
      return Ok(registry.contains(shortcut));
    }
    Ok(
      ENTRIES
        .lock()
        .map_err(|error| error.to_string())?
        .contains_key(&shortcut.id()),
    )
  }
}

#[cfg(test)]
mod tests {
  #[test]
  fn empty_native_manager_keeps_ownership_on_its_hidden_window_thread_without_registering_keys() {
    use super::*;
    assert!(ENTRIES.lock().unwrap().is_empty());
    assert!(with_manager(|_| Ok(())).is_err());
    initialize().unwrap();
    struct Owner;
    impl Drop for Owner {
      fn drop(&mut self) {
        shutdown();
      }
    }
    let _owner = Owner;
    let owner = OWNER.load(Ordering::Acquire);
    assert_ne!(owner, 0);
    assert!(initialize().unwrap_err().contains("already initialized"));
    assert_eq!(with_manager(|_| Ok(42)).unwrap(), 42);
    let app = AppHandle::isolated(crate::config::Config::default(), std::path::PathBuf::new());
    assert_eq!(dispatch(&app, || Ok(17)).unwrap(), 17);
    std::thread::spawn(shutdown).join().unwrap();
    assert_eq!(
      OWNER.load(Ordering::Acquire),
      owner,
      "a foreign thread cannot destroy the owner window"
    );
    assert!(
      ENTRIES.lock().unwrap().is_empty(),
      "this fixture must never register an OS hotkey"
    );
    remove_all().unwrap();
    shutdown();
    assert_eq!(OWNER.load(Ordering::Acquire), 0);
    assert!(with_manager(|_| Ok(())).is_err());
  }
  use super::*;
  #[test]
  fn unregister_releases_callback_ownership_outside_registry_lock() {
    struct ReentrantDrop {
      registry: Arc<Mutex<HashMap<u32, Entry>>>,
      released_outside_lock: Arc<AtomicBool>,
    }
    impl Drop for ReentrantDrop {
      fn drop(&mut self) {
        self
          .released_outside_lock
          .store(self.registry.try_lock().is_ok(), Ordering::Release);
      }
    }
    let registry = Arc::new(Mutex::new(HashMap::new()));
    let released = Arc::new(AtomicBool::new(false));
    let captured = ReentrantDrop {
      registry: registry.clone(),
      released_outside_lock: released.clone(),
    };
    let shortcut: Shortcut = "Ctrl+Shift+F8".parse().unwrap();
    registry.lock().unwrap().insert(
      shortcut.id(),
      Entry {
        shortcut,
        handler: Arc::new(move |_| {
          let _ = &captured;
        }),
      },
    );
    assert!(
      remove_with(&registry, shortcut, |_| Err(
        "OS rejected unregister".into()
      ))
      .is_err()
    );
    assert!(registry.lock().unwrap().contains_key(&shortcut.id()));
    remove_with(&registry, shortcut, |_| Ok(())).unwrap();
    assert!(
      released.load(Ordering::Acquire),
      "callback destructor must be able to reenter the registry"
    );
    assert!(registry.lock().unwrap().is_empty());
    remove_with(&registry, shortcut, |_| {
      panic!("absent key must not reach OS")
    })
    .unwrap();
  }
  #[test]
  fn timed_out_queued_registration_cannot_run_late_and_failed_submission_releases_capacity() {
    assert_eq!(
      enqueue(
        |task| {
          task();
          Ok(())
        },
        || Ok(42),
        Duration::from_secs(1)
      ),
      Ok(42)
    );
    assert_eq!(
      enqueue(
        |task| {
          task();
          Ok(())
        },
        || Err::<(), _>("native operation failed".into()),
        Duration::from_secs(1)
      ),
      Err("native operation failed".into())
    );
    assert!(
      enqueue(|_| Ok(()), || Ok(()), Duration::from_secs(1))
        .unwrap_err()
        .contains("Native hotkey acknowledgment failed")
    );
    let mut queued = None;
    let ran = Arc::new(AtomicBool::new(false));
    let operation_ran = ran.clone();
    let result = enqueue(
      |task| {
        queued = Some(task);
        Ok(())
      },
      move || {
        operation_ran.store(true, Ordering::Release);
        Ok(())
      },
      Duration::from_millis(5),
    );
    assert!(result.is_err());
    queued.take().expect("owner task")();
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(PENDING.load(Ordering::Acquire), 0);
    let result = enqueue(
      |_| Err("owner disconnected".into()),
      || Ok(()),
      Duration::from_millis(5),
    );
    assert_eq!(result, Err("owner disconnected".into()));
    assert_eq!(PENDING.load(Ordering::Acquire), 0);
    let mut saturated = Vec::new();
    for _ in 0..16 {
      assert!(
        enqueue(
          |task| {
            saturated.push(task);
            Ok(())
          },
          || Ok(()),
          Duration::from_millis(1)
        )
        .is_err()
      );
    }
    assert_eq!(PENDING.load(Ordering::Acquire), 16);
    assert_eq!(
      enqueue(
        |_| panic!("full queue must never submit"),
        || Ok(()),
        Duration::from_millis(1)
      ),
      Err("Native hotkey dispatch queue is full".into())
    );
    for task in saturated {
      task();
    }
    assert_eq!(PENDING.load(Ordering::Acquire), 0);
  }

  #[test]
  fn native_hotkey_owner_can_initialize_query_empty_registry_and_shutdown_without_registering_keys()
  {
    std::thread::spawn(|| {
      assert_eq!(OWNER.load(Ordering::Acquire), 0);
      initialize().unwrap();
      struct Shutdown;
      impl Drop for Shutdown {
        fn drop(&mut self) {
          shutdown();
        }
      }
      let _shutdown = Shutdown;
      assert!(initialize().unwrap_err().contains("already initialized"));
      let app = AppHandle::isolated(crate::config::Config::default(), std::path::PathBuf::new());
      let shortcuts = app.global_shortcut();
      assert!(!shortcuts.is_registered("Ctrl+Shift+F8").unwrap());
      assert!(shortcuts.is_registered("no-such-shortcut").is_err());
      assert!(
        shortcuts
          .on_shortcut("no-such-shortcut", |_, _, _| panic!(
            "invalid shortcut callback"
          ))
          .is_err()
      );
      assert!(shortcuts.unregister("no-such-shortcut").is_err());
      shortcuts.unregister("Ctrl+Shift+F8").unwrap();
      shortcuts.unregister_all().unwrap();
      assert!(ENTRIES.lock().unwrap().is_empty());
    })
    .join()
    .unwrap();
    assert_eq!(OWNER.load(Ordering::Acquire), 0);
  }
}
