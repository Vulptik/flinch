//! Screenshot shortcut handlers
//!
//! ## Handlers
//! - `register_area_shortcut` - Area selection screenshot (default: Ctrl+PrintScreen)
//! - `register_copy_text_shortcut` - Area selection that reads its text (default unassigned)
//! - `register_color_picker_shortcut` - Picks one pixel's color as HEX (default unassigned)
//! - `register_last_region_shortcut` - Screenshots the last picked area again (default unassigned)
//! - `register_capture_edit_shortcut` - Area selection that opens the editor (default: unassigned)
//! - `register_fullscreen_shortcut` - Fullscreen capture
//! - `register_active_window_shortcut` - Active window capture
//!
//! ## Registration Conflicts
//! Registration failures preserve the configured binding and report its conflict.
//! Never register an unrequested plain PrintScreen binding as a substitute.

use std::sync::Mutex;
use std::time::Instant;

use crate::app::context::AppHandle;
use crate::app::hotkeys::{GlobalShortcutExt, Shortcut, ShortcutEvent};

use super::common::debounce_check;
use crate::capture_delay::Trigger;

pub(crate) fn register_area_shortcut(app: &AppHandle, shortcut_str: String) -> Result<(), String> {
  register_selection_shortcut(
    app,
    shortcut_str,
    "area",
    "screenshot_area",
    Trigger::Area,
    crate::commands::take_screenshot_for_setup,
  )
}

pub(crate) fn register_capture_edit_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  register_selection_shortcut(
    app,
    shortcut_str,
    "capture and edit",
    "screenshot_edit",
    Trigger::CaptureEdit,
    crate::commands::take_capture_and_edit_for_setup,
  )
}

pub(crate) fn register_copy_text_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  register_selection_shortcut(
    app,
    shortcut_str,
    "copy text",
    "copy_text",
    Trigger::CopyText,
    crate::commands::take_copy_text_for_setup,
  )
}

pub(crate) fn register_color_picker_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  register_selection_shortcut(
    app,
    shortcut_str,
    "pick color",
    "pick_color",
    Trigger::PickColor,
    crate::commands::take_color_pick_for_setup,
  )
}

pub(crate) fn register_last_region_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  register_selection_shortcut(
    app,
    shortcut_str,
    "last region",
    "last_region",
    Trigger::LastRegion,
    crate::commands::take_last_region_for_setup,
  )
}

fn register_selection_shortcut<F, Fut, T>(
  app: &AppHandle,
  shortcut_str: String,
  label: &'static str,
  shortcut_type: &'static str,
  trigger: Trigger,
  take: F,
) -> Result<(), String>
where
  F: Fn(AppHandle) -> Fut + Copy + Send + Sync + 'static,
  Fut: Future<Output = Result<T, String>> + Send + 'static,
{
  let try_shortcut = shortcut_str.clone();
  let app_handle = app.clone();

  let shortcut: Shortcut = match try_shortcut.parse() {
    Ok(s) => s,
    Err(e) => {
      let err_msg = format!("Failed to parse {label} shortcut '{}': {}", try_shortcut, e);
      tracing::error!(
          operation = "register_selection_shortcut",
          shortcut_type,
          shortcut = %try_shortcut,
          error = %e,
          "Failed to parse selection shortcut"
      );
      return Err(err_msg);
    },
  };

  let app_handle2 = app_handle.clone();
  let last_trigger: Mutex<Option<Instant>> = Mutex::new(None);
  let handler = move |_app: &AppHandle, _shortcut: &Shortcut, _event: ShortcutEvent| {
    let pressed = Instant::now();
    if !debounce_check(&last_trigger) {
      return;
    }
    if crate::capture_delay::intercept(trigger) {
      return;
    }

    let app = app_handle2.clone();
    crate::app::runtime::spawn(async move {
      // Check if any overlay is already active (prevent spam)
      let overlay_state = match app.try_state::<crate::commands::OverlayActivityState>() {
        Some(state) => state,
        None => {
          tracing::error!(
            operation = "shortcut_handler",
            shortcut_type,
            "OverlayActivityState not found"
          );
          return;
        },
      };

      // Try to claim the screenshot overlay
      if !overlay_state.try_claim_screenshot() {
        tracing::debug!(
          operation = "shortcut_handler",
          shortcut_type,
          "Overlay already active, ignoring shortcut"
        );
        return;
      }

      crate::commands::mark_shortcut_pressed(pressed);
      let result = take(app.clone()).await;

      // Release the overlay state
      overlay_state.release_screenshot();

      if let Err(e) = result {
        tracing::error!(
            operation = "shortcut_handler",
            shortcut_type,
            error = %e,
            "Screenshot failed"
        );
      }
    });
  };

  match app.global_shortcut().on_shortcut(shortcut, handler) {
    Ok(_) => {
      tracing::info!(
          operation = "register_selection_shortcut",
          shortcut_type,
          shortcut = %try_shortcut,
          "Registered selection shortcut"
      );
      Ok(())
    },
    Err(error) => {
      let err_msg = format!(
        "Failed to register {label} shortcut '{}': {}",
        shortcut_str, error
      );
      tracing::error!(
        operation = "register_selection_shortcut",
        shortcut_type,
        shortcut = %shortcut_str,
        error = %error,
        "Failed to register selection shortcut"
      );
      Err(err_msg)
    },
  }
}

pub(crate) fn register_fullscreen_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  let app_handle = app.clone();

  let shortcut: Shortcut = match shortcut_str.parse() {
    Ok(s) => s,
    Err(e) => {
      let err_msg = format!(
        "Failed to parse fullscreen shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_fullscreen_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to parse fullscreen shortcut"
      );
      return Err(err_msg);
    },
  };

  let app_handle2 = app_handle.clone();
  let handler = move |_app: &AppHandle, _shortcut: &Shortcut, _event: ShortcutEvent| {
    static LAST_TRIGGER: Mutex<Option<Instant>> = Mutex::new(None);
    if !debounce_check(&LAST_TRIGGER) || crate::capture_delay::intercept(Trigger::Fullscreen) {
      return;
    }

    let app = app_handle2.clone();
    crate::app::runtime::spawn(async move {
      if let Err(e) = crate::commands::take_fullscreen_screenshot_for_setup(app).await {
        tracing::error!(
            operation = "shortcut_handler",
            shortcut_type = "screenshot_fullscreen",
            error = %e,
            "Fullscreen screenshot failed"
        );
      }
    });
  };

  match app.global_shortcut().on_shortcut(shortcut, handler) {
    Ok(_) => {
      tracing::info!(
          operation = "register_fullscreen_shortcut",
          shortcut = %shortcut_str,
          "Registered fullscreen shortcut"
      );
      Ok(())
    },
    Err(e) => {
      let err_msg = format!(
        "Failed to register fullscreen shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_fullscreen_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to register fullscreen shortcut"
      );
      Err(err_msg)
    },
  }
}

pub(crate) fn register_active_window_shortcut(
  app: &AppHandle,
  shortcut_str: String,
) -> Result<(), String> {
  let app_handle = app.clone();

  let shortcut: Shortcut = match shortcut_str.parse() {
    Ok(s) => s,
    Err(e) => {
      let err_msg = format!(
        "Failed to parse active window shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_active_window_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to parse active window shortcut"
      );
      return Err(err_msg);
    },
  };

  let app_handle2 = app_handle.clone();
  let handler = move |_app: &AppHandle, _shortcut: &Shortcut, _event: ShortcutEvent| {
    static LAST_TRIGGER: Mutex<Option<Instant>> = Mutex::new(None);
    if !debounce_check(&LAST_TRIGGER) || crate::capture_delay::intercept(Trigger::ActiveWindow) {
      return;
    }

    let app = app_handle2.clone();
    crate::app::runtime::spawn(async move {
      if let Err(e) = crate::commands::take_active_window_screenshot_for_setup(app).await {
        tracing::error!(
            operation = "shortcut_handler",
            shortcut_type = "screenshot_active_window",
            error = %e,
            "Active window screenshot failed"
        );
      }
    });
  };

  match app.global_shortcut().on_shortcut(shortcut, handler) {
    Ok(_) => {
      tracing::info!(
          operation = "register_active_window_shortcut",
          shortcut = %shortcut_str,
          "Registered active window shortcut"
      );
      Ok(())
    },
    Err(e) => {
      let err_msg = format!(
        "Failed to register active window shortcut '{}': {}",
        shortcut_str, e
      );
      tracing::error!(
          operation = "register_active_window_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to register active window shortcut"
      );
      Err(err_msg)
    },
  }
}
