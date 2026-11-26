//! Main screenshot capture commands
//!
//! ## Purpose
//! Provides the primary screenshot capture commands:
//! - `take_screenshot` - Area selection screenshot
//! - `take_fullscreen_screenshot` - Fullscreen capture
//! - `take_active_window_screenshot` - Active window capture
//! - `take_capture_and_edit` - Area selection that opens the annotation editor
//! - `take_copy_text` - Area selection whose text is read into a review panel
//! - `take_last_region` - The last picked area again, without the overlay
//!
//! ## Debouncing
//! All commands are debounced (500ms) and mutex-protected to prevent
//! multiple concurrent screenshots from overlapping.
//!
use super::post_capture::handle_capture_result;
use crate::app::context::AppHandle;
use crate::capture_delay::Trigger;
use crate::config::{CaptureType, Config};
use crate::screenshot::Screenshot;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

// Screenshot debouncing/locking to prevent multiple concurrent screenshots
static SCREENSHOT_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static LAST_SCREENSHOT_TIME: Mutex<Option<std::time::Instant>> = Mutex::new(None);

struct ScreenshotGuard;

impl ScreenshotGuard {
  fn acquire() -> Result<Self, String> {
    if SCREENSHOT_IN_PROGRESS.swap(true, Ordering::SeqCst) {
      return Err("Screenshot already in progress".to_string());
    }
    Ok(Self)
  }
}

impl Drop for ScreenshotGuard {
  fn drop(&mut self) {
    SCREENSHOT_IN_PROGRESS.store(false, Ordering::SeqCst);
  }
}

/// Debounce check - returns error if too soon after last screenshot
fn check_debounce(operation: &str) -> Result<(), String> {
  let mut last_time = LAST_SCREENSHOT_TIME
    .lock()
    .map_err(|e| crate::commands::ipc_display_context("Debounce state poisoned", e))?;
  if let Some(time) = *last_time
    && time.elapsed() < std::time::Duration::from_millis(500)
  {
    tracing::debug!(
      operation = operation,
      elapsed_ms = time.elapsed().as_millis(),
      "Request ignored: too soon after last one"
    );
    return Err(format!("{} request ignored (debounced)", operation));
  }
  *last_time = Some(std::time::Instant::now());
  Ok(())
}

#[derive(Clone, Copy)]
enum ScreenshotMode {
  Fullscreen,
  Area,
  ActiveWindow,
  CaptureAndEdit,
  LastRegion,
}

impl ScreenshotMode {
  fn operation_name(self) -> &'static str {
    match self {
      Self::Fullscreen => "fullscreen_screenshot",
      Self::Area => "screenshot",
      Self::ActiveWindow => "active_window_screenshot",
      Self::CaptureAndEdit => "capture_and_edit",
      Self::LastRegion => "capture_last_region",
    }
  }
}

async fn execute_screenshot_mode(
  app: AppHandle,
  mode: ScreenshotMode,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  let config = Config::current(&app);
  let save_path = config.resolve_capture_path(CaptureType::Screenshot);
  capture_normally(app, config, save_path, mode).await
}

async fn capture_normally(
  app: AppHandle,
  config: Config,
  save_path: PathBuf,
  mode: ScreenshotMode,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  let screenshot = Screenshot::new(save_path, config.effective_filename_template().to_string());

  let capture_result = match mode {
    ScreenshotMode::Fullscreen => {
      screenshot
        .capture_fullscreen(
          &app,
          config.screenshot_format.as_str(),
          config.screenshot_quality,
        )
        .await
    },
    ScreenshotMode::Area => {
      screenshot
        .capture(
          &app,
          config.screenshot_format.as_str(),
          config.screenshot_quality,
        )
        .await
    },
    ScreenshotMode::ActiveWindow => {
      screenshot
        .capture_active_window(
          &app,
          config.screenshot_format.as_str(),
          config.screenshot_quality,
        )
        .await
    },
    ScreenshotMode::LastRegion => {
      screenshot
        .capture_last_region(
          &app,
          config.screenshot_format.as_str(),
          config.screenshot_quality,
        )
        .await
    },
    ScreenshotMode::CaptureAndEdit => {
      screenshot
        .capture_and_edit(
          &app,
          config.screenshot_format.as_str(),
          config.screenshot_quality,
        )
        .await
    },
  };

  finish_capture_result(app, config, mode, capture_result).await
}

async fn finish_capture_result(
  app: AppHandle,
  config: Config,
  mode: ScreenshotMode,
  capture_result: anyhow::Result<crate::screenshot::CaptureResult>,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  match capture_outcome(capture_result) {
    Ok(Some(result)) if let Some((x, y)) = result.pin => {
      crate::overlay::pin::open(
        &app,
        crate::overlay::pin::PinImage {
          rgba: std::sync::Arc::new(result.rgba),
          width: result.width,
          height: result.height,
        },
        crate::overlay::pin::Placement::At(x, y),
      );
      Ok(crate::commands::capture_finalize::CaptureResponse::cancelled())
    },
    Ok(Some(result)) => handle_capture_result(&app, result, &config, mode.operation_name()).await,
    Ok(None) => Ok(crate::commands::capture_finalize::CaptureResponse::cancelled()),
    Err(error) => Err(report_capture_failure(&app, mode.operation_name(), &error)),
  }
}

fn report_capture_failure(app: &AppHandle, operation: &str, error: &anyhow::Error) -> String {
  let error_msg = crate::error::ErrorExt::to_rich_string(error);
  tracing::error!(
    operation,
    action = "capture_failed",
    error = %error_msg,
    "Screenshot capture failed"
  );
  let lang = crate::i18n::get_lang(app);
  let strings = crate::i18n::get(&lang);
  crate::notifications::show_error(app, strings.capture_failed, "notification_error_capture");
  error_msg
}

/// Runs the Capture Delay countdown before the desktop is frozen. Returns whether to capture.
async fn delay(app: &AppHandle, trigger: Trigger) -> bool {
  let outcome = crate::capture_delay::wait(app, trigger).await;
  if outcome.waited() {
    crate::commands::restart_shortcut_timer();
  }
  outcome.proceeds()
}

fn capture_outcome<T>(result: anyhow::Result<T>) -> anyhow::Result<Option<T>> {
  match result {
    Ok(result) => Ok(Some(result)),
    Err(error)
      if matches!(
        error.downcast_ref::<crate::screenshot::ScreenshotError>(),
        Some(crate::screenshot::ScreenshotError::Cancelled)
      ) =>
    {
      Ok(None)
    },
    Err(error) => Err(error),
  }
}

pub async fn take_fullscreen_screenshot(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  check_debounce("Fullscreen screenshot")?;
  let _guard = ScreenshotGuard::acquire()?;
  if !delay(&app, Trigger::Fullscreen).await {
    return Ok(crate::commands::capture_finalize::CaptureResponse::cancelled());
  }
  execute_screenshot_mode(app, ScreenshotMode::Fullscreen).await
}

pub async fn take_screenshot(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  check_debounce("Screenshot")?;
  let _guard = ScreenshotGuard::acquire()?;
  if !delay(&app, Trigger::Area).await {
    return Ok(crate::commands::capture_finalize::CaptureResponse::cancelled());
  }
  execute_screenshot_mode(app, ScreenshotMode::Area).await
}

pub async fn take_active_window_screenshot(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  check_debounce("Active window screenshot")?;
  let _guard = ScreenshotGuard::acquire()?;
  if !delay(&app, Trigger::ActiveWindow).await {
    return Ok(crate::commands::capture_finalize::CaptureResponse::cancelled());
  }
  execute_screenshot_mode(app, ScreenshotMode::ActiveWindow).await
}

pub async fn take_capture_and_edit(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  check_debounce("Capture and edit")?;
  let _guard = ScreenshotGuard::acquire()?;
  if !delay(&app, Trigger::CaptureEdit).await {
    return Ok(crate::commands::capture_finalize::CaptureResponse::cancelled());
  }
  execute_screenshot_mode(app, ScreenshotMode::CaptureAndEdit).await
}

pub async fn take_last_region(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  check_debounce("Capture last region")?;
  let _guard = ScreenshotGuard::acquire()?;
  if !delay(&app, Trigger::LastRegion).await {
    return Ok(crate::commands::capture_finalize::CaptureResponse::cancelled());
  }
  execute_screenshot_mode(app, ScreenshotMode::LastRegion).await
}

/// The guard covers the desktop capture only; reading runs beside later captures.
pub async fn take_copy_text(app: AppHandle) -> Result<(), String> {
  check_debounce("Copy text")?;
  let guard = ScreenshotGuard::acquire()?;
  if !delay(&app, Trigger::CopyText).await {
    return Ok(());
  }
  let capture = Screenshot::new(PathBuf::new(), String::new())
    .capture_for_text(&app)
    .await;
  drop(guard);
  match capture_outcome(capture) {
    Ok(Some(result)) => super::copy_text::deliver(app, result).await,
    Ok(None) => Ok(()),
    Err(error) => Err(report_capture_failure(&app, "copy_text", &error)),
  }
}

/// The guard covers the desktop capture only, as for Copy Text.
pub async fn take_color_pick(app: AppHandle) -> Result<(), String> {
  check_debounce("Pick color")?;
  let guard = ScreenshotGuard::acquire()?;
  if !delay(&app, Trigger::PickColor).await {
    return Ok(());
  }
  let capture = Screenshot::new(PathBuf::new(), String::new())
    .capture_for_color(&app)
    .await;
  drop(guard);
  match capture_outcome(capture) {
    Ok(Some(result)) => super::color_pick::deliver(app, result).await,
    Ok(None) => Ok(()),
    Err(error) => Err(report_capture_failure(&app, "pick_color", &error)),
  }
}

// Wrapper functions for shortcut handlers
pub async fn take_screenshot_for_setup(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  take_screenshot(app).await
}

pub async fn take_fullscreen_screenshot_for_setup(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  take_fullscreen_screenshot(app).await
}

pub async fn take_active_window_screenshot_for_setup(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  take_active_window_screenshot(app).await
}

pub async fn take_capture_and_edit_for_setup(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  take_capture_and_edit(app).await
}

pub async fn take_last_region_for_setup(
  app: AppHandle,
) -> Result<crate::commands::capture_finalize::CaptureResponse, String> {
  take_last_region(app).await
}

pub async fn take_copy_text_for_setup(app: AppHandle) -> Result<(), String> {
  take_copy_text(app).await
}

pub async fn take_color_pick_for_setup(app: AppHandle) -> Result<(), String> {
  take_color_pick(app).await
}

#[cfg(test)]
mod tests {
  use super::*;
  use serial_test::serial;
  use std::thread;
  use std::time::Duration;

  fn isolated() -> AppHandle {
    AppHandle::isolated(
      Config {
        language: "en".into(),
        notifications_enabled: false,
        notification_sound: false,
        ..Default::default()
      },
      PathBuf::new(),
    )
  }

  #[tokio::test]
  async fn capture_command_errors_retain_the_original_cause_and_only_typed_cancellation_stays_quiet()
   {
    for mode in [
      ScreenshotMode::Area,
      ScreenshotMode::Fullscreen,
      ScreenshotMode::ActiveWindow,
      ScreenshotMode::CaptureAndEdit,
    ] {
      let app = isolated();
      let cancelled = finish_capture_result(
        app.clone(),
        Config::current(&app),
        mode,
        Err(crate::screenshot::ScreenshotError::Cancelled.into()),
      )
      .await
      .unwrap();
      assert_eq!(cancelled.cancelled, Some(true));
      let manager = app
        .try_state::<crate::overlay::notification::NotificationManager>()
        .unwrap();
      assert!(manager.take_test_requests().is_empty());
      let error = finish_capture_result(
        app.clone(),
        Config::current(&app),
        mode,
        Err(anyhow::anyhow!(
          "driver cancelled allocation after device removal"
        )),
      )
      .await
      .unwrap_err();
      assert!(error.contains("driver cancelled allocation after device removal"));
      let notices = manager.take_test_requests();
      assert_eq!(notices.len(), 1);
      assert!(notices[0].critical);
    }
  }

  #[tokio::test]
  #[serial]
  async fn every_capture_entry_obeys_debounce_and_exclusive_ownership_before_accessing_the_desktop()
  {
    struct Reset(Option<std::time::Instant>);
    impl Drop for Reset {
      fn drop(&mut self) {
        *LAST_SCREENSHOT_TIME.lock().unwrap() = self.0;
      }
    }
    let _reset = Reset(*LAST_SCREENSHOT_TIME.lock().unwrap());
    let _guard = ScreenshotGuard::acquire().unwrap();
    for blocked_by_debounce in [false, true] {
      for operation in 0..10 {
        *LAST_SCREENSHOT_TIME.lock().unwrap() = blocked_by_debounce.then(std::time::Instant::now);
        let app = isolated();
        let error = match operation {
          0 => take_screenshot(app).await,
          1 => take_fullscreen_screenshot(app).await,
          2 => take_active_window_screenshot(app).await,
          3 => take_screenshot_for_setup(app).await,
          4 => take_fullscreen_screenshot_for_setup(app).await,
          5 => take_active_window_screenshot_for_setup(app).await,
          6 => take_capture_and_edit(app).await,
          7 => take_capture_and_edit_for_setup(app).await,
          8 => take_copy_text(app)
            .await
            .map(|()| crate::commands::capture_finalize::CaptureResponse::cancelled()),
          _ => take_copy_text_for_setup(app)
            .await
            .map(|()| crate::commands::capture_finalize::CaptureResponse::cancelled()),
        }
        .unwrap_err();
        assert!(error.contains(if blocked_by_debounce {
          "debounced"
        } else {
          "already in progress"
        }));
      }
    }
  }

  #[test]
  fn cancellation_uses_typed_cause_and_never_error_wording() {
    let cancelled =
      anyhow::Error::new(crate::screenshot::ScreenshotError::Cancelled).context("Selection closed");
    assert_eq!(
      capture_outcome::<()>(Err(cancelled)).expect("cancelled outcome"),
      None
    );
    for message in [
      "capture cancelled by driver",
      "cancel button renderer failed",
      "C:\\cancelled\\capture.png: access denied",
    ] {
      let result = capture_outcome::<()>(Err(anyhow::anyhow!(message)));
      assert_eq!(result.expect_err("real failure").to_string(), message);
    }
    assert_eq!(
      capture_outcome(Ok(42)).expect("capture succeeded"),
      Some(42)
    );
  }

  #[test]
  #[serial]
  fn test_screenshot_guard_basic() {
    // Should be able to acquire guard
    let guard = ScreenshotGuard::acquire();
    assert!(guard.is_ok(), "Should acquire guard when not in use");

    // Should not be able to acquire second guard
    let guard2 = ScreenshotGuard::acquire();
    assert!(
      guard2.is_err(),
      "Should not acquire guard when already held"
    );

    // Drop first guard
    drop(guard);

    // Should be able to acquire again
    let guard3 = ScreenshotGuard::acquire();
    assert!(
      guard3.is_ok(),
      "Should acquire guard after previous dropped"
    );
  }

  #[test]
  #[serial]
  fn test_screenshot_guard_drop_releases() {
    {
      let _guard = ScreenshotGuard::acquire().expect("Should acquire guard");
      // Guard is held here
    } // Guard dropped

    // Should be able to acquire after scope
    let guard = ScreenshotGuard::acquire();
    assert!(guard.is_ok(), "Guard should be available after drop");
  }

  #[test]
  #[serial]
  fn test_check_debounce_allows_first_call() {
    // Clear any previous state by waiting
    thread::sleep(Duration::from_millis(600));

    let result = check_debounce("test");
    assert!(result.is_ok(), "First call should not be debounced");
  }

  #[test]
  #[serial]
  fn test_check_debounce_blocks_rapid_calls() {
    // Clear state
    thread::sleep(Duration::from_millis(600));

    // First call
    check_debounce("test1").expect("First call should succeed");

    // Immediate second call should be blocked
    let result = check_debounce("test2");
    assert!(result.is_err(), "Second call should be debounced");
    assert!(
      result.unwrap_err().contains("debounced"),
      "Error should mention debouncing"
    );
  }

  #[test]
  #[serial]
  fn test_check_debounce_allows_after_delay() {
    // Clear state
    thread::sleep(Duration::from_millis(600));

    // First call
    check_debounce("test1").expect("First call should succeed");

    // Wait for debounce period
    thread::sleep(Duration::from_millis(550));

    // Should allow after delay
    let result = check_debounce("test2");
    assert!(result.is_ok(), "Call after debounce period should succeed");
  }
}
