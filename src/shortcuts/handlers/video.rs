//! Video recording shortcut handlers
//!
//! ## Handlers
//! - `register_record_area_shortcut` - Area selection video recording
//! - `register_record_fullscreen_shortcut` - Fullscreen video recording
//! - `register_record_active_window_shortcut` - Active window video recording
//!
//! ## Toggle Behavior
//! All video handlers act as toggles: if recording is active, the shortcut stops
//! the recording. If not recording, the shortcut starts a new recording.
//!
//! ## Area Recording Flow
//! The area recording handler shows a self-contained overlay that:
//! 1. Allows the user to select a region
//! 2. Transitions to recording mode with timer and stop/pause buttons
//! 3. Starts the video recorder with the selected region

use std::sync::Mutex;
use std::time::Instant;

use crate::app::context::AppHandle;
use crate::app::hotkeys::{GlobalShortcutExt, Shortcut, ShortcutEvent};

use super::common::debounce_check;

pub(crate) fn register_record_area_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  let app_handle = app.clone();

  let shortcut: Shortcut = match shortcut_str.parse() {
    Ok(s) => s,
    Err(e) => {
      let err_msg = format!(
        "Failed to parse record area shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_record_area_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to parse record area shortcut"
      );
      return Err(err_msg);
    },
  };

  let app_handle2 = app_handle.clone();
  let handler = move |_app: &AppHandle, _shortcut: &Shortcut, _event: ShortcutEvent| {
    static LAST_TRIGGER: Mutex<Option<Instant>> = Mutex::new(None);
    if !debounce_check(&LAST_TRIGGER)
      || crate::capture_delay::intercept(crate::capture_delay::Trigger::RecordArea)
    {
      return;
    }

    let app = app_handle2.clone();
    crate::app::runtime::spawn(async move {
      crate::recording::coordinator::RecordingCoordinator::toggle_area_shortcut(app).await;
    });
  };

  match app.global_shortcut().on_shortcut(shortcut, handler) {
    Ok(_) => {
      tracing::info!(
          operation = "register_record_area_shortcut",
          shortcut = %shortcut_str,
          "Registered record area shortcut"
      );
      Ok(())
    },
    Err(e) => {
      let err_msg = format!(
        "Failed to register record area shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_record_area_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to register record area shortcut"
      );
      Err(err_msg)
    },
  }
}

pub(crate) fn register_record_fullscreen_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  let app_handle = app.clone();

  let shortcut: Shortcut = match shortcut_str.parse() {
    Ok(s) => s,
    Err(e) => {
      let err_msg = format!(
        "Failed to parse record fullscreen shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_record_fullscreen_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to parse record fullscreen shortcut"
      );
      return Err(err_msg);
    },
  };

  let app_handle2 = app_handle.clone();
  let handler = move |_app: &AppHandle, _shortcut: &Shortcut, _event: ShortcutEvent| {
    static LAST_TRIGGER: Mutex<Option<Instant>> = Mutex::new(None);
    if !debounce_check(&LAST_TRIGGER)
      || crate::capture_delay::intercept(crate::capture_delay::Trigger::RecordFullscreen)
    {
      return;
    }

    let app = app_handle2.clone();
    crate::app::runtime::spawn(async move {
      crate::recording::coordinator::RecordingCoordinator::toggle_fullscreen_shortcut(app).await;
    });
  };

  match app.global_shortcut().on_shortcut(shortcut, handler) {
    Ok(_) => {
      tracing::info!(
          operation = "register_record_fullscreen_shortcut",
          shortcut = %shortcut_str,
          "Registered record fullscreen shortcut"
      );
      Ok(())
    },
    Err(e) => {
      let err_msg = format!(
        "Failed to register record fullscreen shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_record_fullscreen_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to register record fullscreen shortcut"
      );
      Err(err_msg)
    },
  }
}

pub(crate) fn register_record_active_window_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  let app_handle = app.clone();

  let shortcut: Shortcut = match shortcut_str.parse() {
    Ok(s) => s,
    Err(e) => {
      let err_msg = format!(
        "Failed to parse record active window shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_record_active_window_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to parse record active window shortcut"
      );
      return Err(err_msg);
    },
  };

  let app_handle2 = app_handle.clone();
  let handler = move |_app: &AppHandle, _shortcut: &Shortcut, _event: ShortcutEvent| {
    static LAST_TRIGGER: Mutex<Option<Instant>> = Mutex::new(None);
    if !debounce_check(&LAST_TRIGGER)
      || crate::capture_delay::intercept(crate::capture_delay::Trigger::RecordActiveWindow)
    {
      return;
    }

    let app = app_handle2.clone();
    crate::app::runtime::spawn(async move {
      crate::recording::coordinator::RecordingCoordinator::toggle_active_window_shortcut(app).await;
    });
  };

  match app.global_shortcut().on_shortcut(shortcut, handler) {
    Ok(_) => {
      tracing::info!(
          operation = "register_record_active_window_shortcut",
          shortcut = %shortcut_str,
          "Registered record active window shortcut"
      );
      Ok(())
    },
    Err(e) => {
      let err_msg = format!(
        "Failed to register record active window shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_record_active_window_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to register record active window shortcut"
      );
      Err(err_msg)
    },
  }
}
