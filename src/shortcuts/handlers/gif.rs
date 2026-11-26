//! GIF recording shortcut handler
//!
//! ## Handler
//! - `register_gif_shortcut` - Starts the GIF recording overlay with area selection
//!
//! ## GIF Recording Flow
//! 1. User presses shortcut
//! 2. Overlay appears with area selection
//! 3. User selects region and clicks Start
//! 4. Recording begins with visible timer
//! 5. User clicks Stop to end recording
//! 6. GIF is encoded in background and saved

use std::sync::Mutex;
use std::time::Instant;

use crate::app::context::AppHandle;
use crate::app::hotkeys::{GlobalShortcutExt, Shortcut, ShortcutEvent};

use super::common::debounce_check;

pub(crate) fn register_gif_shortcut(app: &AppHandle, shortcut_str: String) -> Result<(), String> {
  let app_handle = app.clone();

  let shortcut: Shortcut = match shortcut_str.parse() {
    Ok(s) => s,
    Err(e) => {
      let err_msg = format!("Failed to parse GIF shortcut '{}': {}", shortcut_str, e);
      tracing::error!(
          operation = "register_gif_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to parse GIF shortcut"
      );
      return Err(err_msg);
    },
  };

  let app_handle2 = app_handle.clone();
  let handler = move |_app: &AppHandle, _shortcut: &Shortcut, _event: ShortcutEvent| {
    static LAST_TRIGGER: Mutex<Option<Instant>> = Mutex::new(None);
    if !debounce_check(&LAST_TRIGGER)
      || crate::capture_delay::intercept(crate::capture_delay::Trigger::Gif)
    {
      return;
    }

    let app = app_handle2.clone();
    crate::app::runtime::spawn(async move {
      if let Some(session) = crate::commands::current_recording_session(&app) {
        let stop_result = crate::commands::stop_active_recording_if_any(app.clone()).await;
        match stop_result {
          Ok(Some(_)) => {
            tracing::info!(
              operation = "shortcut_handler",
              shortcut_type = "gif",
              active_kind = ?session.kind,
              "Active recording detected, stopping current recording instead of launching GIF selection"
            );
          },
          Ok(None) => {},
          Err(error) => {
            tracing::error!(
              operation = "shortcut_handler",
              shortcut_type = "gif",
              error = %error,
              "Failed to stop active recording"
            );
          },
        }
        return;
      }

      // Check overlay activity state
      let overlay_state = match app.try_state::<crate::commands::OverlayActivityState>() {
        Some(state) => state,
        None => {
          tracing::error!(
            operation = "shortcut_handler",
            shortcut_type = "gif",
            "OverlayActivityState not found"
          );
          return;
        },
      };

      // If GIF overlay is already active, toggle (stop it)
      if overlay_state.is_gif_active() {
        tracing::info!(
          operation = "shortcut_handler",
          shortcut_type = "gif",
          "GIF recording active, stopping via shortcut toggle"
        );
        crate::overlay::gif::stop_gif_recording();
        return;
      }

      // Check if any OTHER overlay is active
      if overlay_state.is_any_active() {
        tracing::debug!(
          operation = "shortcut_handler",
          shortcut_type = "gif",
          "Another overlay already active, ignoring shortcut"
        );
        return;
      }

      let result = crate::commands::start_gif_selection(app.clone()).await;

      match result {
        Ok(_) => {},
        Err(e) => {
          tracing::error!(
              operation = "shortcut_handler",
              shortcut_type = "gif",
              error = %e,
              "GIF selection failed"
          );
        },
      }
    });
  };

  match app.global_shortcut().on_shortcut(shortcut, handler) {
    Ok(_) => {
      tracing::info!(
          operation = "register_gif_shortcut",
          shortcut = %shortcut_str,
          "Registered GIF shortcut"
      );
      Ok(())
    },
    Err(e) => {
      let err_msg = format!("Failed to register GIF shortcut '{}': {}", shortcut_str, e);
      tracing::error!(
          operation = "register_gif_shortcut",
          shortcut = %shortcut_str,
          error = %e,
          "Failed to register GIF shortcut"
      );
      Err(err_msg)
    },
  }
}
