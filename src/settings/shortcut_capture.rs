//! Exclusive shortcut capture lease. Acquisition and restoration are owned by
//! workers, so dropping an awaiting UI request cannot strand disabled shortcuts.
use anyhow::{Context, Result};
use std::{
  future::Future,
  sync::{Arc, LazyLock},
  time::Duration,
};

pub(crate) trait CaptureEnvironment: Clone + Send + Sync + 'static {
  fn suspend(&self) -> impl Future<Output = Result<()>> + Send;
  fn suppress_printscreen(&self) -> impl Future<Output = Result<()>> + Send;
  fn remove_printscreen(&self) -> impl Future<Output = Result<()>> + Send;
  fn restore(&self) -> impl Future<Output = Result<()>> + Send;
}

pub(crate) struct CaptureService {
  operation: Arc<tokio::sync::Mutex<()>>,
  timeout: Duration,
}
pub(crate) static CAPTURE_SERVICE: LazyLock<CaptureService> =
  LazyLock::new(|| CaptureService::new(Duration::from_secs(5)));

struct OwnedCapture<E: CaptureEnvironment> {
  environment: E,
  _operation: tokio::sync::OwnedMutexGuard<()>,
}
impl<E: CaptureEnvironment> OwnedCapture<E> {
  async fn cleanup(self) -> Result<()> {
    let _operation = self._operation;
    restore(&self.environment).await
  }
}
pub(crate) struct CaptureLease<E: CaptureEnvironment> {
  owned: Option<OwnedCapture<E>>,
  runtime: tokio::runtime::Handle,
}

async fn restore<E: CaptureEnvironment>(environment: &E) -> Result<()> {
  // Always resume even when removal fails; report both failures explicitly.
  let removed = environment.remove_printscreen().await;
  let resumed = environment.restore().await;
  match (removed, resumed) {
    (Ok(()), Ok(())) => Ok(()),
    (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
    (Err(removed), Err(resumed)) => anyhow::bail!(
      "PrintScreen cleanup failed: {removed:#}; shortcut restoration failed: {resumed:#}"
    ),
  }
}

impl CaptureService {
  fn new(timeout: Duration) -> Self {
    Self {
      operation: Arc::new(tokio::sync::Mutex::new(())),
      timeout,
    }
  }
  pub(crate) async fn acquire<E: CaptureEnvironment>(
    &self,
    environment: E,
  ) -> Result<CaptureLease<E>> {
    let operation = tokio::time::timeout(self.timeout, self.operation.clone().lock_owned())
      .await
      .context("Shortcut capture acquisition timed out")?;
    tokio::spawn(async move {
      let owned = OwnedCapture {
        environment,
        _operation: operation,
      };
      // suspend increments disable depth before unregistering; undo it even
      // when that unregister fails. A worker retains ownership through rollback.
      if let Err(error) = owned.environment.suspend().await {
        let rollback = owned.environment.restore().await;
        return match rollback {
          Ok(()) => Err(error),
          Err(rollback) => Err(anyhow::anyhow!(
            "Capture suspension failed: {error:#}; restoration failed: {rollback:#}"
          )),
        };
      }
      if let Err(error) = owned.environment.suppress_printscreen().await {
        let rollback = restore(&owned.environment).await;
        return match rollback {
          Ok(()) => Err(error),
          Err(rollback) => Err(anyhow::anyhow!(
            "PrintScreen suppression failed: {error:#}; restoration failed: {rollback:#}"
          )),
        };
      }
      Ok(CaptureLease {
        owned: Some(owned),
        runtime: tokio::runtime::Handle::current(),
      })
    })
    .await
    .context("Shortcut capture acquisition worker failed")?
  }
}

impl<E: CaptureEnvironment> CaptureLease<E> {
  pub(crate) async fn release(mut self) -> Result<()> {
    let owned = self
      .owned
      .take()
      .context("Shortcut lease already released")?;
    self
      .runtime
      .spawn(async move { owned.cleanup().await })
      .await
      .context("Shortcut restoration worker failed")?
  }
}
impl<E: CaptureEnvironment> Drop for CaptureLease<E> {
  fn drop(&mut self) {
    if let Some(owned) = self.owned.take() {
      self.runtime.spawn(async move {
        if let Err(error) = owned.cleanup().await {
          tracing::error!(%error, "Dropped shortcut capture lease could not restore shortcuts");
        }
      });
    }
  }
}

#[derive(Clone)]
pub(crate) struct PlatformCapture(pub(crate) crate::app::context::AppHandle);
impl CaptureEnvironment for PlatformCapture {
  async fn suspend(&self) -> Result<()> {
    if super::native_host::is_probe() {
      return Ok(());
    }
    crate::commands::set_global_shortcut_enabled(self.0.clone(), false)
      .await
      .map_err(anyhow::Error::msg)
  }
  async fn suppress_printscreen(&self) -> Result<()> {
    if super::native_host::is_probe() {
      return Ok(());
    }
    crate::shortcuts::register_printscreen_noop(&self.0).map_err(anyhow::Error::msg)
  }
  async fn remove_printscreen(&self) -> Result<()> {
    if super::native_host::is_probe() {
      return Ok(());
    }
    use crate::app::hotkeys::GlobalShortcutExt;
    self
      .0
      .global_shortcut()
      .unregister("PrintScreen")
      .map_err(anyhow::Error::msg)
      .context("Cannot remove capture PrintScreen handler")
  }
  async fn restore(&self) -> Result<()> {
    if super::native_host::is_probe() {
      return Ok(());
    }
    crate::commands::set_global_shortcut_enabled(self.0.clone(), true)
      .await
      .map_err(anyhow::Error::msg)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  fn platform_app() -> crate::app::context::AppHandle {
    let config = crate::config::Config {
      screenshot_area_shortcut: "Ctrl+Shift+F1".into(),
      screenshot_fullscreen_shortcut: "Ctrl+Shift+F2".into(),
      screenshot_active_window_shortcut: "Ctrl+Shift+F3".into(),
      record_area_shortcut: "Ctrl+Shift+F4".into(),
      record_fullscreen_shortcut: "Ctrl+Shift+F5".into(),
      record_active_window_shortcut: "Ctrl+Shift+F6".into(),
      gif_shortcut: "Ctrl+Shift+F7".into(),
      screenshot_edit_shortcut: "Ctrl+Shift+F8".into(),
      copy_text_shortcut: "Ctrl+Shift+F9".into(),
      ..Default::default()
    };
    crate::app::context::AppHandle::isolated(config, std::path::PathBuf::new())
  }
  #[tokio::test]
  async fn platform_capture_restores_real_configuration_registration_and_nested_disable_ownership()
  {
    use crate::app::hotkeys::GlobalShortcutExt;
    let app = platform_app();
    crate::shortcuts::setup_global_shortcuts(&app);
    assert!(
      app
        .global_shortcut()
        .is_registered("Ctrl+Shift+F1")
        .unwrap()
    );
    crate::commands::set_global_shortcut_enabled(app.clone(), false)
      .await
      .unwrap();
    crate::commands::set_global_shortcut_enabled(app.clone(), false)
      .await
      .unwrap();
    let state = app
      .try_state::<crate::shortcuts::ShortcutStateHolder>()
      .unwrap();
    assert!(state.is_shortcuts_disabled());
    let service = CaptureService::new(Duration::from_secs(1));
    let lease = service.acquire(PlatformCapture(app.clone())).await.unwrap();
    assert!(app.global_shortcut().is_registered("PrintScreen").unwrap());
    assert!(
      !app
        .global_shortcut()
        .is_registered("Ctrl+Shift+F1")
        .unwrap()
    );
    lease.release().await.unwrap();
    assert!(!app.global_shortcut().is_registered("PrintScreen").unwrap());
    assert!(
      state.is_shortcuts_disabled(),
      "capture cannot release another caller's ownership"
    );
    crate::commands::set_global_shortcut_enabled(app.clone(), true)
      .await
      .unwrap();
    assert!(state.is_shortcuts_disabled());
    crate::commands::set_global_shortcut_enabled(app.clone(), true)
      .await
      .unwrap();
    assert!(!state.is_shortcuts_disabled());
    assert!(
      app
        .global_shortcut()
        .is_registered("Ctrl+Shift+F1")
        .unwrap()
    );
    crate::commands::register_printscreen_noop(app.clone()).unwrap();
    crate::commands::unregister_printscreen_noop(app.clone()).unwrap();
    crate::commands::unregister_printscreen_noop(app).unwrap();
  }
  #[tokio::test]
  async fn platform_capture_native_rejection_rolls_back_and_restores_the_configured_bindings() {
    use crate::app::hotkeys::{GlobalShortcutExt, Shortcut};
    let app = platform_app();
    let printscreen: Shortcut = "PrintScreen".parse().unwrap();
    app
      .test_hotkeys()
      .unwrap()
      .reject(printscreen, "native suppression conflict");
    let error = CaptureService::new(Duration::from_secs(1))
      .acquire(PlatformCapture(app.clone()))
      .await
      .err()
      .unwrap();
    assert!(format!("{error:#}").contains("native suppression conflict"));
    assert!(
      !app
        .try_state::<crate::shortcuts::ShortcutStateHolder>()
        .unwrap()
        .is_shortcuts_disabled()
    );
    assert!(!app.global_shortcut().is_registered("PrintScreen").unwrap());
    assert!(
      app
        .global_shortcut()
        .is_registered("Ctrl+Shift+F1")
        .unwrap()
    );
  }
  use std::sync::Mutex;

  #[derive(Clone)]
  struct PausedAcquire {
    fake: Fake,
    entered: Arc<tokio::sync::Notify>,
    proceed: Arc<tokio::sync::Notify>,
  }
  impl CaptureEnvironment for PausedAcquire {
    async fn suspend(&self) -> Result<()> {
      self.fake.call("suspend")?;
      self.entered.notify_one();
      self.proceed.notified().await;
      Ok(())
    }
    async fn suppress_printscreen(&self) -> Result<()> {
      self.fake.call("suppress")
    }
    async fn remove_printscreen(&self) -> Result<()> {
      self.fake.call("remove")
    }
    async fn restore(&self) -> Result<()> {
      self.fake.call("restore")
    }
  }

  #[tokio::test]
  async fn cancelled_acquisition_caller_cannot_strand_shortcut_suspension() {
    let service = Arc::new(CaptureService::new(Duration::from_secs(1)));
    let environment = PausedAcquire {
      fake: Fake::default(),
      entered: Arc::new(tokio::sync::Notify::new()),
      proceed: Arc::new(tokio::sync::Notify::new()),
    };
    let worker_service = service.clone();
    let worker_environment = environment.clone();
    let caller = tokio::spawn(async move { worker_service.acquire(worker_environment).await });
    tokio::time::timeout(Duration::from_secs(1), environment.entered.notified())
      .await
      .expect("acquisition entered");
    caller.abort();
    let _ = caller.await;
    environment.proceed.notify_one();
    let next = service
      .acquire(Fake::default())
      .await
      .expect("next owner after cleanup");
    assert_eq!(
      environment.fake.events(),
      ["suspend", "suppress", "remove", "restore"]
    );
    next.release().await.expect("release next owner");
  }
  #[derive(Clone, Default)]
  struct Fake {
    events: Arc<Mutex<Vec<&'static str>>>,
    fail: &'static str,
  }
  impl Fake {
    fn call(&self, event: &'static str) -> Result<()> {
      self.events.lock().expect("events").push(event);
      anyhow::ensure!(self.fail != event, "Injected {event} failure");
      Ok(())
    }
    fn events(&self) -> Vec<&'static str> {
      self.events.lock().expect("events").clone()
    }
  }
  impl CaptureEnvironment for Fake {
    async fn suspend(&self) -> Result<()> {
      self.call("suspend")
    }
    async fn suppress_printscreen(&self) -> Result<()> {
      self.call("suppress")
    }
    async fn remove_printscreen(&self) -> Result<()> {
      self.call("remove")
    }
    async fn restore(&self) -> Result<()> {
      self.call("restore")
    }
  }
  #[tokio::test]
  async fn release_removes_temporary_handler_before_restoring_shortcuts() {
    let fake = Fake::default();
    let service = CaptureService::new(Duration::from_secs(1));
    service
      .acquire(fake.clone())
      .await
      .expect("acquire")
      .release()
      .await
      .expect("release");
    assert_eq!(fake.events(), ["suspend", "suppress", "remove", "restore"]);
  }
  #[tokio::test]
  async fn failed_suspension_still_undoes_disable_depth() {
    let fake = Fake {
      fail: "suspend",
      ..Fake::default()
    };
    assert!(
      CaptureService::new(Duration::from_secs(1))
        .acquire(fake.clone())
        .await
        .is_err()
    );
    assert_eq!(fake.events(), ["suspend", "restore"]);
  }
  #[tokio::test]
  async fn failed_suppression_rolls_back_and_next_owner_can_acquire() {
    let service = CaptureService::new(Duration::from_secs(1));
    let fake = Fake {
      fail: "suppress",
      ..Fake::default()
    };
    assert!(service.acquire(fake.clone()).await.is_err());
    assert_eq!(fake.events(), ["suspend", "suppress", "remove", "restore"]);
    service
      .acquire(Fake::default())
      .await
      .expect("second acquire")
      .release()
      .await
      .expect("release");
  }
  #[tokio::test]
  async fn cleanup_failure_never_skips_restoration() {
    let fake = Fake {
      fail: "remove",
      ..Fake::default()
    };
    let lease = CaptureService::new(Duration::from_secs(1))
      .acquire(fake.clone())
      .await
      .expect("acquire");
    assert!(lease.release().await.is_err());
    assert_eq!(fake.events().last(), Some(&"restore"));
  }
  #[tokio::test]
  async fn dropped_lease_finishes_cleanup_before_next_owner() {
    let service = CaptureService::new(Duration::from_secs(1));
    let fake = Fake::default();
    drop(service.acquire(fake.clone()).await.expect("acquire"));
    let second = service.acquire(fake.clone()).await.expect("second acquire");
    assert_eq!(
      fake.events(),
      [
        "suspend", "suppress", "remove", "restore", "suspend", "suppress"
      ]
    );
    second.release().await.expect("release");
  }
  #[tokio::test]
  async fn waiting_owner_times_out_without_touching_platform_state() {
    let service = CaptureService::new(Duration::from_millis(5));
    let lease = service
      .acquire(Fake::default())
      .await
      .expect("first acquire");
    let second = Fake::default();
    assert!(service.acquire(second.clone()).await.is_err());
    assert!(second.events().is_empty());
    lease.release().await.expect("release");
  }
}
