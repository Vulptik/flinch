// src/commands/mod.rs
// Typed operation functions and lightweight service helpers

use std::fmt::Display;

mod capture_finalize;
pub(crate) mod clipboard;
pub(crate) mod config;
mod gif;
mod notification;

pub mod screenshot;
mod support;
mod system;
mod validation;
pub mod video;
mod window;

use crate::app::context::AppHandle;

// Re-export all commands (clipboard uses pub(crate), so re-export at same visibility)
pub use crate::state::{
  ActiveRecordingKind, ClipboardState, LanguageState, OverlayActivityState, RecordingHudState,
  RecordingSessionState, VideoRecorderState, current_recording_session,
};
pub(crate) use clipboard::*;
pub use config::*;
pub use gif::*;
pub use notification::*;
pub use screenshot::*;
pub use system::*;
pub(crate) use validation::*;
pub use video::RecordingMode;
pub use window::*;

pub(crate) fn ipc_context<E>(context: &str, error: E) -> String
where
  E: Into<anyhow::Error>,
{
  crate::error::rich_context(context, error)
}

pub(crate) fn ipc_display_context(context: &str, error: impl Display) -> String {
  crate::error::rich_display_context(context, error)
}

pub(crate) fn build_recording_stop_redirect_response()
-> crate::commands::video::RecordingStopResponse {
  crate::commands::video::RecordingStopResponse {
    status: crate::commands::video::RecordingLifecycleStatus::Processing,
    message: "Recording is being finalized in the background".to_string(),
  }
}

pub(crate) async fn stop_active_recording_if_any(
  app: AppHandle,
) -> Result<Option<crate::commands::video::RecordingStopResponse>, String> {
  let Some(session) = current_recording_session(&app) else {
    return Ok(None);
  };

  match session.kind {
    ActiveRecordingKind::Video => crate::commands::video::stop::stop_video_recording(app)
      .await
      .map(Some),
    ActiveRecordingKind::Gif => {
      crate::overlay::gif::stop_gif_recording();
      Ok(Some(build_recording_stop_redirect_response()))
    },
  }
}

pub(crate) fn set_recording_session_paused(app: &AppHandle, paused: bool) {
  if let Some(session_state) = app.try_state::<RecordingSessionState>() {
    session_state.set_paused(paused);
  }
  crate::app::update_tray_recording_state(app);
}

pub(crate) fn set_recording_session(
  app: &AppHandle,
  kind: ActiveRecordingKind,
  presentation_mode: crate::recording_visibility::RecordingPresentationMode,
) {
  if let Some(session_state) = app.try_state::<RecordingSessionState>() {
    session_state.set(kind, presentation_mode);
  }
  crate::app::update_tray_recording_state(app);
}

pub(crate) fn clear_recording_session(app: &AppHandle) {
  if let Some(session_state) = app.try_state::<RecordingSessionState>() {
    session_state.clear();
  }
  crate::app::update_tray_recording_state(app);
}

/// Synchronize backend locale and tray labels after native configuration changes.
/// Updates the active language (for Rust-side notifications) and re-applies
/// tray labels using the Rust-side i18n table.
pub async fn set_app_language(app: AppHandle, language: String) -> Result<(), String> {
  // Update language state used by i18n::get_lang()
  if let Some(lang_state) = app.try_state::<LanguageState>() {
    match lang_state.inner().0.lock() {
      Ok(mut l) => *l = language.clone(),
      Err(e) => {
        tracing::warn!(operation = "set_app_language", error = %e, "Language mutex poisoned")
      },
    }
  }

  // Check if currently recording so we display the right record label
  let is_recording = current_recording_session(&app).is_some();

  let tray_labels = crate::i18n::tray(&language);
  crate::app::apply_tray_labels(&app, tray_labels, is_recording);

  tracing::debug!(operation = "set_app_language", language = %language, "App language updated");
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::state::AppExitState;
  use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  };

  // --- AppExitState ---

  #[test]
  fn test_app_exit_state_new_should_not_exit() {
    let state = AppExitState::new();
    assert!(!state.should_exit());
  }

  #[test]
  fn test_app_exit_state_default_should_not_exit() {
    let state = AppExitState::default();
    assert!(!state.should_exit());
  }

  #[test]
  fn test_app_exit_state_allow_exit_enables_should_exit() {
    let state = AppExitState::new();
    state.allow_exit();
    assert!(state.should_exit());
  }

  #[test]
  fn test_app_exit_state_allow_exit_is_idempotent() {
    let state = AppExitState::new();
    state.allow_exit();
    state.allow_exit();
    assert!(state.should_exit());
  }

  #[test]
  fn test_app_exit_state_visible_across_threads() {
    use std::thread;
    let state = Arc::new(AppExitState::new());
    let s = Arc::clone(&state);
    thread::spawn(move || {
      s.allow_exit();
    })
    .join()
    .unwrap();
    assert!(state.should_exit());
  }

  #[test]
  fn test_recording_stop_redirect_response_matches_expected_payload() {
    let response = build_recording_stop_redirect_response();
    assert!(matches!(
      response.status,
      crate::commands::video::RecordingLifecycleStatus::Processing
    ));
    assert_eq!(
      response.message,
      "Recording is being finalized in the background"
    );
  }

  // --- OverlayActivityState ---

  #[test]
  fn test_overlay_new_nothing_active() {
    let s = OverlayActivityState::new();
    assert!(!s.is_screenshot_active());
    assert!(!s.is_video_area_selection_active());
    assert!(!s.is_gif_active());
    assert!(!s.is_any_active());
  }

  #[test]
  fn test_overlay_default_nothing_active() {
    let s = OverlayActivityState::default();
    assert!(!s.is_any_active());
  }

  #[test]
  fn test_overlay_claim_screenshot_succeeds_when_idle() {
    let s = OverlayActivityState::new();
    assert!(s.try_claim_screenshot());
  }

  #[test]
  fn test_overlay_claim_screenshot_marks_active() {
    let s = OverlayActivityState::new();
    s.try_claim_screenshot();
    assert!(s.is_screenshot_active());
    assert!(s.is_any_active());
  }

  #[test]
  fn test_overlay_claim_screenshot_twice_fails() {
    let s = OverlayActivityState::new();
    assert!(s.try_claim_screenshot());
    assert!(!s.try_claim_screenshot());
  }

  #[test]
  fn test_overlay_claim_video_blocked_when_screenshot_active() {
    let s = OverlayActivityState::new();
    s.try_claim_screenshot();
    assert!(!s.try_claim_video_area_selection());
  }

  #[test]
  fn test_overlay_claim_gif_blocked_when_screenshot_active() {
    let s = OverlayActivityState::new();
    s.try_claim_screenshot();
    assert!(!s.try_claim_gif());
  }

  #[test]
  fn test_overlay_claim_screenshot_blocked_when_video_active() {
    let s = OverlayActivityState::new();
    s.try_claim_video_area_selection();
    assert!(!s.try_claim_screenshot());
  }

  #[test]
  fn test_overlay_claim_screenshot_blocked_when_gif_active() {
    let s = OverlayActivityState::new();
    s.try_claim_gif();
    assert!(!s.try_claim_screenshot());
  }

  #[test]
  fn test_overlay_claim_gif_blocked_when_video_active() {
    let s = OverlayActivityState::new();
    s.try_claim_video_area_selection();
    assert!(!s.try_claim_gif());
  }

  #[test]
  fn test_overlay_claim_video_blocked_when_gif_active() {
    let s = OverlayActivityState::new();
    s.try_claim_gif();
    assert!(!s.try_claim_video_area_selection());
  }

  #[test]
  fn test_overlay_release_screenshot_clears_active() {
    let s = OverlayActivityState::new();
    s.try_claim_screenshot();
    s.release_screenshot();
    assert!(!s.is_screenshot_active());
    assert!(!s.is_any_active());
  }

  #[test]
  fn test_overlay_release_allows_reclaim() {
    let s = OverlayActivityState::new();
    s.try_claim_screenshot();
    s.release_screenshot();
    assert!(s.try_claim_screenshot());
  }

  #[test]
  fn test_overlay_video_claim_release_cycle() {
    let s = OverlayActivityState::new();
    assert!(s.try_claim_video_area_selection());
    assert!(s.is_video_area_selection_active());
    s.release_video_area_selection();
    assert!(!s.is_video_area_selection_active());
    assert!(!s.is_any_active());
  }

  #[test]
  fn test_overlay_gif_claim_release_cycle() {
    let s = OverlayActivityState::new();
    assert!(s.try_claim_gif());
    assert!(s.is_gif_active());
    s.release_gif();
    assert!(!s.is_gif_active());
    assert!(!s.is_any_active());
  }

  #[test]
  fn test_overlay_release_without_claim_is_safe() {
    let s = OverlayActivityState::new();
    // None of these were claimed — should not panic or corrupt state.
    s.release_screenshot();
    s.release_video_area_selection();
    s.release_gif();
    assert!(!s.is_any_active());
  }

  // Releasing a different overlay type must not clear an active bit it doesn't own.
  #[test]
  fn test_overlay_release_does_not_clear_other_active_bits() {
    let s = OverlayActivityState::new();
    s.try_claim_gif();
    assert!(s.is_gif_active());

    // Release screenshot (never claimed) — must not disturb gif bit.
    s.release_screenshot();
    assert!(s.is_gif_active());
    assert!(s.is_any_active());
  }

  #[test]
  fn test_overlay_claim_different_type_after_release() {
    let s = OverlayActivityState::new();
    s.try_claim_screenshot();
    s.release_screenshot();

    // After screenshot is released, a different type should succeed.
    assert!(s.try_claim_video_area_selection());
    assert!(s.is_video_area_selection_active());
    assert!(!s.is_screenshot_active());
  }

  #[test]
  fn test_overlay_is_any_active_true_when_only_gif_claimed() {
    let s = OverlayActivityState::new();
    s.try_claim_gif();
    assert!(s.is_any_active());
  }

  #[test]
  fn test_overlay_is_any_active_true_when_only_video_claimed() {
    let s = OverlayActivityState::new();
    s.try_claim_video_area_selection();
    assert!(s.is_any_active());
  }

  // Under concurrent pressure exactly one claim should win.
  #[test]
  fn test_overlay_concurrent_claims_exactly_one_wins() {
    use std::thread;

    let state = Arc::new(OverlayActivityState::new());
    let success_count = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..16)
      .map(|_| {
        let state = Arc::clone(&state);
        let count = Arc::clone(&success_count);
        thread::spawn(move || {
          if state.try_claim_screenshot() {
            count.fetch_add(1, Ordering::SeqCst);
          }
        })
      })
      .collect();

    for h in handles {
      h.join().unwrap();
    }

    assert_eq!(success_count.load(Ordering::SeqCst), 1);
  }

  // After concurrent acquisition the winner can release and a subsequent
  // single-threaded claim should succeed.
  #[test]
  fn test_overlay_concurrent_claim_then_release_allows_next() {
    use std::thread;

    let state = Arc::new(OverlayActivityState::new());

    let handles: Vec<_> = (0..8)
      .map(|_| {
        let s = Arc::clone(&state);
        thread::spawn(move || {
          s.try_claim_screenshot();
        })
      })
      .collect();

    for h in handles {
      h.join().unwrap();
    }

    // Exactly one claim is active.
    assert!(state.is_screenshot_active());

    // Releasing it allows a fresh claim.
    state.release_screenshot();
    assert!(!state.is_any_active());
    assert!(state.try_claim_video_area_selection());
  }
}
