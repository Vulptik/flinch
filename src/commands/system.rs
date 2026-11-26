// src/commands/system.rs
// System information and logging commands

use crate::app::context::AppHandle;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The latest screenshot shortcut press still waiting for its overlay.
static SHORTCUT_PRESSED: Mutex<Option<Instant>> = Mutex::new(None);
/// Set by each press and cleared by the first frame it freezes.
static FREEZE_PENDING: AtomicBool = AtomicBool::new(false);
/// A press this old never reached an overlay, such as after a failed capture.
const SHORTCUT_OVERLAY_LIMIT: Duration = Duration::from_secs(5);

pub async fn set_global_shortcut_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
  let state = app
    .try_state::<crate::shortcuts::ShortcutStateHolder>()
    .ok_or_else(|| "Shortcut state not initialized".to_string())?;

  if enabled {
    let new_depth = state.decrement_disable_depth();
    if new_depth == 0 {
      // All requesters released; actually re-register shortcuts from config.
      // Conflict warnings remain once-per-session, so closing settings does not
      // re-arm the startup warning dialog.
      crate::shortcuts::setup_global_shortcuts(&app);
      tracing::info!(
        operation = "toggle_shortcuts",
        enabled = true,
        "Global shortcuts enabled (disable depth reached 0)"
      );
    } else {
      tracing::debug!(
        operation = "toggle_shortcuts",
        enabled = true,
        remaining_depth = new_depth,
        "Disable depth decremented but shortcuts remain disabled"
      );
    }
  } else {
    let new_depth = state.increment_disable_depth();
    if new_depth == 1 {
      // First requester; actually unregister.
      crate::shortcuts::unregister_all_shortcuts(&app)?;
      tracing::info!(
        operation = "toggle_shortcuts",
        enabled = false,
        "Global shortcuts disabled"
      );
    } else {
      tracing::debug!(
        operation = "toggle_shortcuts",
        enabled = false,
        new_depth,
        "Disable depth incremented; shortcuts already disabled"
      );
    }
  }

  Ok(())
}

pub fn get_log_file_path() -> Result<String, String> {
  crate::logging::get_log_file_path()
    .map(|p| p.to_string_lossy().to_string())
    .map_err(|e| crate::commands::ipc_context("Failed to get log file path", e))
}

/// Register PrintScreen with a no-op handler to suppress OS default behavior.
/// This is only needed when recording shortcuts in the settings window.
pub fn register_printscreen_noop(app: AppHandle) -> Result<(), String> {
  crate::shortcuts::register_printscreen_noop(&app)
}

/// Unregister the PrintScreen no-op handler.
/// This is only needed when recording shortcuts in the settings window.
pub fn unregister_printscreen_noop(app: AppHandle) -> Result<(), String> {
  crate::shortcuts::unregister_printscreen_noop(&app)
}

/// Log a structured speed snapshot for one measured operation or stage.
pub fn log_speed_snapshot(context: &str, elapsed: Duration) {
  let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
  tracing::info!(
    operation = "speed_snapshot",
    context,
    elapsed_ms = format!("{elapsed_ms:.3}"),
    "Speed snapshot {}: {:.3}ms",
    context,
    elapsed_ms,
  );
}

/// Remember when a screenshot shortcut arrived, for `log_shortcut_to_overlay`.
pub fn mark_shortcut_pressed(pressed: Instant) {
  match SHORTCUT_PRESSED.lock() {
    Ok(mut slot) => {
      *slot = Some(pressed);
      FREEZE_PENDING.store(true, Ordering::Release);
    },
    Err(error) => {
      tracing::warn!(operation = "speed_snapshot", %error, "Cannot record shortcut press");
    },
  }
}

/// A Capture Delay countdown just ended: time the pending press from now, so speed snapshots
/// measure Flinch rather than the wait the user asked for. Does nothing without a pending press.
pub fn restart_shortcut_timer() {
  if !FREEZE_PENDING.load(Ordering::Acquire) {
    return;
  }
  match SHORTCUT_PRESSED.lock() {
    Ok(mut slot) => *slot = Some(Instant::now()),
    Err(error) => {
      tracing::warn!(operation = "speed_snapshot", %error, "Cannot restart shortcut timing");
    },
  }
}

/// Log shortcut to the moment the desktop image was fixed, once per press. This is
/// the delay a user feels when freezing a key moment; the overlay follows later.
pub fn log_shortcut_to_freeze() {
  if !FREEZE_PENDING.swap(false, Ordering::AcqRel) {
    return;
  }
  let pressed = match SHORTCUT_PRESSED.lock() {
    Ok(slot) => *slot,
    Err(error) => {
      tracing::warn!(operation = "speed_snapshot", %error, "Cannot read shortcut press");
      None
    },
  };
  if let Some(elapsed) = recent_press_elapsed(pressed, Instant::now()) {
    log_speed_snapshot("screenshot.shortcut_to_freeze", elapsed);
  }
}

/// Log shortcut to first presented overlay frame. Windows input delivery and
/// desktop composition sit outside this span. Overlays opened another way log nothing.
pub fn log_shortcut_to_overlay() {
  log_shortcut_to("screenshot.shortcut_to_overlay");
}

/// Log shortcut to the last region cut from the frozen desktop: Capture Last Region opens no
/// overlay, so this is its counterpart of `log_shortcut_to_overlay`.
pub fn log_shortcut_to_region() {
  log_shortcut_to("screenshot.shortcut_to_region");
}

fn log_shortcut_to(label: &str) {
  let pressed = match SHORTCUT_PRESSED.lock() {
    Ok(mut slot) => slot.take(),
    Err(error) => {
      tracing::warn!(operation = "speed_snapshot", %error, "Cannot read shortcut press");
      None
    },
  };
  if let Some(elapsed) = recent_press_elapsed(pressed, Instant::now()) {
    log_speed_snapshot(label, elapsed);
  }
}

fn recent_press_elapsed(pressed: Option<Instant>, now: Instant) -> Option<Duration> {
  pressed
    .map(|pressed| now.saturating_duration_since(pressed))
    .filter(|elapsed| *elapsed < SHORTCUT_OVERLAY_LIMIT)
}

/// Public support details only; never reads captures, settings, paths or logs.
pub async fn get_support_diagnostics(app: AppHandle) -> Result<serde_json::Value, String> {
  crate::app::runtime::spawn_blocking(move || support_diagnostics(app))
    .await
    .map_err(|error| super::ipc_context("Read support diagnostics worker", error))?
}

fn support_diagnostics(_app: AppHandle) -> Result<serde_json::Value, String> {
  use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
  use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
  use windows::core::{s, w};

  type RtlGetVersionFn = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;
  // SAFETY: ntdll is already loaded for the lifetime of this Windows process.
  let module = unsafe { GetModuleHandleW(w!("ntdll.dll")) }
    .map_err(|error| super::ipc_display_context("Read Windows version", error))?;
  // SAFETY: the module is valid and the export name is a static null-terminated string.
  let proc = unsafe { GetProcAddress(module, s!("RtlGetVersion")) }.ok_or_else(|| {
    super::ipc_context(
      "Read Windows version",
      anyhow::anyhow!("RtlGetVersion unavailable"),
    )
  })?;
  // SAFETY: this export has the documented RtlGetVersion ABI and signature.
  let query =
    unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, RtlGetVersionFn>(proc) };
  let mut info = OSVERSIONINFOW {
    dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
    ..Default::default()
  };
  // SAFETY: info is initialized with its size and remains writable throughout the call.
  let status = unsafe { query(&mut info) };
  if status < 0 {
    return Err(super::ipc_context(
      "Read Windows version",
      anyhow::anyhow!("RtlGetVersion failed: {status}"),
    ));
  }
  Ok(serde_json::json!({
    "version": env!("CARGO_PKG_VERSION"),
    "graphics": match super::support::graphics() {
      Ok(graphics) => graphics,
      Err(error) => {
        tracing::warn!(%error, "Graphics diagnostics unavailable");
        serde_json::json!({ "gpus": [], "outputs": [], "error": super::ipc_context("Read graphics diagnostics", error) })
      },
    },
    "windowsBuild": format!("{}.{}.{}", info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber),
    "channel": if cfg!(feature = "msix") { "msix" } else { "installer" },
  }))
}

/// Open the real log directory directly through Windows, without shell-command quoting.
pub async fn open_support_logs() -> Result<(), String> {
  crate::app::runtime::spawn_blocking(|| {
    super::support::open_logs().map_err(|error| super::ipc_context("Open support logs", error))
  })
  .await
  .map_err(|error| super::ipc_context("Open support logs worker", error))?
}

#[cfg(test)]
mod shortcut_timing_tests {
  use super::*;

  #[test]
  fn only_a_recent_press_reports_a_duration() {
    let pressed = Instant::now();
    let soon = pressed + Duration::from_millis(80);
    assert_eq!(
      recent_press_elapsed(Some(pressed), soon),
      Some(Duration::from_millis(80))
    );
    assert_eq!(recent_press_elapsed(None, soon), None);
    assert_eq!(
      recent_press_elapsed(Some(pressed), pressed + SHORTCUT_OVERLAY_LIMIT),
      None
    );
  }

  #[test]
  fn a_press_reports_its_freeze_once_and_its_overlay_once() {
    mark_shortcut_pressed(Instant::now());
    assert!(FREEZE_PENDING.load(Ordering::Acquire));
    log_shortcut_to_freeze();
    assert!(
      !FREEZE_PENDING.load(Ordering::Acquire),
      "one freeze per press"
    );
    log_shortcut_to_freeze();
    assert!(
      SHORTCUT_PRESSED.lock().unwrap().is_some(),
      "the overlay still waits"
    );
    log_shortcut_to_overlay();
    assert!(SHORTCUT_PRESSED.lock().unwrap().is_none());
    // Capture Last Region takes the press instead of an overlay.
    mark_shortcut_pressed(Instant::now());
    log_shortcut_to_freeze();
    log_shortcut_to_region();
    assert!(SHORTCUT_PRESSED.lock().unwrap().is_none());
    // Overlays opened without a shortcut log nothing.
    log_shortcut_to_overlay();
    log_speed_snapshot("test.snapshot", Duration::from_micros(1500));
    // An old press is forgotten without a misleading duration.
    mark_shortcut_pressed(Instant::now() - SHORTCUT_OVERLAY_LIMIT - Duration::from_millis(1));
    log_shortcut_to_freeze();
    log_shortcut_to_overlay();
    assert!(SHORTCUT_PRESSED.lock().unwrap().is_none());
    // A Capture Delay countdown times a pending press from its end; without one it does nothing.
    restart_shortcut_timer();
    assert!(SHORTCUT_PRESSED.lock().unwrap().is_none());
    let waited = Instant::now() - Duration::from_secs(5);
    mark_shortcut_pressed(waited);
    restart_shortcut_timer();
    assert!(
      SHORTCUT_PRESSED
        .lock()
        .unwrap()
        .is_some_and(|pressed| pressed > waited)
    );
    log_shortcut_to_freeze();
    log_shortcut_to_overlay();
  }

  /// Read-only Windows version and DXGI adapter enumeration; nothing is captured.
  #[tokio::test]
  async fn support_diagnostics_name_this_build_and_its_windows_version() {
    crate::app::runtime::test_runtime();
    let app = AppHandle::isolated(crate::config::Config::default(), std::path::PathBuf::new());
    let details = get_support_diagnostics(app).await.expect("diagnostics");
    assert_eq!(details["version"], env!("CARGO_PKG_VERSION"));
    let channel = if cfg!(feature = "msix") {
      "msix"
    } else {
      "installer"
    };
    assert_eq!(details["channel"], channel);
    let build = details["windowsBuild"].as_str().unwrap();
    let parts: Vec<u32> = build.split('.').map(|part| part.parse().unwrap()).collect();
    assert_eq!(parts.len(), 3, "{build}");
    assert!(parts[0] >= 10 && parts[2] > 0, "{build}");
    let graphics = &details["graphics"];
    assert!(graphics["gpus"].is_array() && graphics["outputs"].is_array());
  }
}
