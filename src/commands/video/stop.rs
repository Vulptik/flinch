//! Video recording stop command

use crate::app::context::AppHandle;
use crate::commands::VideoRecorderState;
use crate::commands::capture_finalize::finalize_saved_artifact_with_notifications;
use crate::config::Config;
use std::path::PathBuf;
use std::sync::Arc;

fn can_stop_recording(status: crate::video::controller::RecordingStatus) -> bool {
  matches!(
    status,
    crate::video::controller::RecordingStatus::Recording
      | crate::video::controller::RecordingStatus::Paused
      | crate::video::controller::RecordingStatus::Error
  )
}

fn idle_stop_response() -> crate::commands::video::RecordingStopResponse {
  crate::commands::video::RecordingStopResponse {
    status: crate::commands::video::RecordingLifecycleStatus::Idle,
    message: "Not currently recording".to_string(),
  }
}

fn processing_stop_response() -> crate::commands::video::RecordingStopResponse {
  crate::commands::video::RecordingStopResponse {
    status: crate::commands::video::RecordingLifecycleStatus::Processing,
    message: "Recording is being finalized in the background".to_string(),
  }
}

pub(crate) fn stop_video_recorder_blocking(app: &AppHandle) -> Result<Option<PathBuf>, String> {
  let recorder = Arc::clone(&app.state::<VideoRecorderState>().inner().0);

  let (result, microphone_lost) = {
    let mut guard = crate::state::lock_recorder(&recorder)?;
    if !can_stop_recording(guard.status()) {
      return Ok(None);
    }
    let result = guard
      .stop_recording()
      .map(Some)
      .map_err(|e| crate::commands::ipc_context("Failed to stop recording", e));
    (result, guard.take_microphone_lost())
  };

  crate::screenshot::release_capture_idle_memory();
  // Said after the recording, never during it: notifications would land in the video.
  if microphone_lost && result.is_ok() {
    match crate::i18n::native_settings(&crate::i18n::get_lang(app), "recording_microphone_lost") {
      Ok(title) => {
        crate::notifications::show_error(app, &title, "notification_error_microphone_lost");
      },
      Err(error) => tracing::error!(%error, "Microphone notice localization failed"),
    }
  }
  result
}

pub(crate) async fn stop_video_recorder_raw(app: &AppHandle) -> Result<Option<PathBuf>, String> {
  let app = app.clone();
  tokio::task::spawn_blocking(move || stop_video_recorder_blocking(&app))
    .await
    .map_err(|e| crate::commands::ipc_context("Stop task failed", e))?
}

/// Fatal control failures cancel the session; partial output is never reported as a saved capture.
pub(crate) async fn abort_failed_recording(app: &AppHandle) {
  if let Some(hud) = app.try_state::<crate::commands::RecordingHudState>() {
    hud.signal_stop();
    hud.clear();
  }
  crate::commands::clear_recording_session(app);
  if let Err(error) = crate::overlay::OverlayWindow::hide_recording_hud().await {
    tracing::warn!(%error, "Failed to close recording HUD after fatal failure");
  }
  match tokio::time::timeout(
    std::time::Duration::from_secs(45),
    stop_video_recorder_raw(app),
  )
  .await
  {
    Ok(Ok(Some(path))) => {
      crate::capture_artifact::remove_file_best_effort(
        &path,
        "abort_failed_recording",
        "discard failed recording output",
      );
    },
    Ok(Ok(None)) => {},
    Ok(Err(error)) => tracing::error!(%error, "Failed recording cleanup failed"),
    Err(error) => tracing::error!(%error, "Failed recording cleanup exceeded its deadline"),
  }
  notify_recording_failure(app);
}

pub(super) fn notify_recording_failure(app: &AppHandle) {
  match crate::i18n::native_settings(&crate::i18n::get_lang(app), "recording_failed") {
    Ok(title) => crate::notifications::show_error(app, &title, recording_failure_body(app)),
    Err(error) => tracing::error!(%error, "Recording failure notification localization failed"),
  }
}

/// Body of a recording or GIF failure notification. A session where no capture backend can record,
/// or whose microphone is blocked or missing, says so; every other failure points at the logs,
/// which keep the full cause either way.
pub(crate) fn recording_failure_body(app: &AppHandle) -> &'static str {
  // Never waits: a recorder busy elsewhere keeps the generic body rather than blocking a toast.
  app
    .try_state::<VideoRecorderState>()
    .and_then(|state| state.0.try_lock().ok().map(|r| r.start_failure()))
    .unwrap_or_default()
    .body()
}

pub async fn stop_video_recording(
  app: AppHandle,
) -> Result<crate::commands::video::RecordingStopResponse, String> {
  let state = app.state::<VideoRecorderState>();
  let operation = match state.claim_operation() {
    Ok(operation) => operation,
    Err(_) => return Ok(processing_stop_response()),
  };
  let recorder = Arc::clone(&state.inner().0);

  // Guard against double-stop: check status before spawning the stop task.
  // The recorder's stop_recording() also checks, but this prevents spawning
  // redundant tasks on rapid double-clicks.
  let progress = tokio::task::spawn_blocking(move || {
    let mut guard = crate::state::lock_recorder(&recorder)?;
    let status = guard.status();
    if !can_stop_recording(status) {
      return Ok::<_, String>(None);
    }
    let progress = guard.finalization_progress();
    progress.store(
      crate::video::controller::FinalizationStage::Closing as u8,
      std::sync::atomic::Ordering::Release,
    );
    Ok(Some(progress))
  })
  .await
  .map_err(|e| crate::commands::ipc_context("Stop preparation failed", e))
  .and_then(|result| result);
  // A recorder that cannot even be prepared to stop must never fail silently. The session and
  // any output stay as they are: Stop can be retried, and startup recovery keeps the recording.
  let progress = match progress {
    Ok(progress) => progress,
    Err(error) => {
      tracing::error!(
        operation = "stop_video_recording",
        %error,
        "Cannot prepare the recorder to stop"
      );
      notify_recording_failure(&app);
      return Err(error);
    },
  };
  let Some(progress) = progress else {
    return Ok(idle_stop_response());
  };

  // Signal the HUD thread to close by setting its stop_flag
  if let Some(hud_state) = app.try_state::<crate::commands::RecordingHudState>() {
    hud_state.signal_stop();
    hud_state.clear();
  }

  if let Err(e) = crate::overlay::OverlayWindow::hide_recording_hud().await {
    tracing::debug!(
      operation = "stop_video_recording",
      error = %e,
      "Failed to hide HUD (may not have been shown)"
    );
  }

  let processing_notice =
    crate::notifications::recording_processing_with_progress(&app, false, progress);
  crate::commands::clear_recording_session(&app);

  let app_clone = app.clone();

  tokio::spawn(async move {
    let _operation = operation;
    let result = stop_video_recorder_raw(&app_clone).await;
    drop(processing_notice);
    match result {
      Ok(Some(output_path)) => {
        let config = Config::current(&app_clone);
        let lang = crate::i18n::get_lang(&app_clone);
        let t = crate::i18n::get(&lang);
        let video_titles = crate::i18n::video_notif_titles(&lang);
        finalize_saved_artifact_with_notifications(
          &app_clone,
          &config,
          "video",
          &output_path,
          "stop_video_recording",
          video_titles.saved,
          &format!("{}{}", t.body_saved_to_prefix, output_path.display()),
        )
        .await;
        tracing::info!(
          operation = "stop_video_recording",
          path = %output_path.display(),
          "Video recording stopped"
        );
      },
      Ok(None) => {
        tracing::debug!(
          operation = "stop_video_recording",
          "Stop task found recorder already idle"
        );
      },
      Err(e) => {
        tracing::error!(
          operation = "stop_video_recording",
          error = %e,
          "Failed to stop recording"
        );
        let lang = crate::i18n::get_lang(&app_clone);
        let t = crate::i18n::get(&lang);
        crate::notifications::show_error(
          &app_clone,
          t.video_save_failed,
          "notification_error_save",
        );
      },
    }
  });

  Ok(processing_stop_response())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn each_explained_start_failure_gets_its_own_translated_body() {
    use crate::video::controller::StartFailure;
    use flinch_video_engine::{
      EngineError, ErrorKind,
      types::{CAPTURE_UNAVAILABLE_STAGE, MICROPHONE_BLOCKED_STAGE, MICROPHONE_MISSING_STAGE},
    };
    let all = [
      StartFailure::Other,
      StartFailure::CaptureUnavailable,
      StartFailure::MicrophoneBlocked,
      StartFailure::MicrophoneMissing,
    ];
    for locale in ["en", "fr", "de", "es", "it"] {
      let bodies: std::collections::HashSet<String> = all
        .iter()
        .map(|failure| crate::i18n::native_settings(locale, failure.body()).expect("copy"))
        .collect();
      assert_eq!(bodies.len(), all.len(), "{locale}");
    }
    let failed = |stage: &str| EngineError::new(ErrorKind::Capability, stage, "cause");
    assert_eq!(
      StartFailure::of(&failed(MICROPHONE_BLOCKED_STAGE)),
      StartFailure::MicrophoneBlocked
    );
    assert_eq!(
      StartFailure::of(&failed(MICROPHONE_MISSING_STAGE)),
      StartFailure::MicrophoneMissing
    );
    assert_eq!(
      StartFailure::of(&failed(CAPTURE_UNAVAILABLE_STAGE)),
      StartFailure::CaptureUnavailable
    );
    assert_eq!(StartFailure::of(&failed("encoder")), StartFailure::Other);
  }

  #[test]
  fn can_stop_recording_only_allows_active_or_error_states() {
    assert!(can_stop_recording(
      crate::video::controller::RecordingStatus::Recording
    ));
    assert!(can_stop_recording(
      crate::video::controller::RecordingStatus::Paused
    ));
    assert!(can_stop_recording(
      crate::video::controller::RecordingStatus::Error
    ));
    assert!(!can_stop_recording(
      crate::video::controller::RecordingStatus::Idle
    ));
    assert!(!can_stop_recording(
      crate::video::controller::RecordingStatus::Starting
    ));
    assert!(!can_stop_recording(
      crate::video::controller::RecordingStatus::Stopping
    ));
  }

  #[test]
  fn idle_stop_response_matches_expected_payload() {
    let response = idle_stop_response();
    assert!(matches!(
      response.status,
      crate::commands::video::RecordingLifecycleStatus::Idle
    ));
    assert_eq!(response.message, "Not currently recording");
  }

  #[test]
  fn processing_stop_response_matches_expected_payload() {
    let response = processing_stop_response();
    assert!(matches!(
      response.status,
      crate::commands::video::RecordingLifecycleStatus::Processing
    ));
    assert_eq!(
      response.message,
      "Recording is being finalized in the background"
    );
  }
}
