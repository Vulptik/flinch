//! Video recording pause/resume commands
//!
//! ## Purpose
//! Handles pausing and resuming video recordings:
//! - `pause_video_recording` - Pauses capture without stopping
//! - `resume_video_recording` - Resumes capture after pause
//!
//! ## Pause Behavior
//! When paused:
//! - The engine stops capturing video and audio; the timeline resumes without a gap
//! - Elapsed recording time stops advancing
//! - HUD shows paused state with blinking indicator

use crate::app::context::AppHandle;
use crate::commands::VideoRecorderState;
use crate::recording_visibility::RecordingPresentationMode;

fn session_presentation_mode(app: &AppHandle) -> Option<RecordingPresentationMode> {
  crate::commands::current_recording_session(app).map(|session| session.presentation_mode)
}

fn sync_hud_pause_flag(app: &AppHandle, paused: bool) {
  if let Some(hud_state) = app.try_state::<crate::commands::RecordingHudState>() {
    hud_state.sync_paused(paused);
  }
}

pub async fn pause_video_recording(app: AppHandle) -> Result<(), String> {
  let _operation = app.state::<VideoRecorderState>().claim_operation()?;
  let received_at = std::time::Instant::now();
  tracing::info!(
    operation = "pause_video_recording",
    stage = "command_received",
    "Pause command received"
  );
  let app_clone = app.clone();
  let result = tokio::task::spawn_blocking(move || {
    let recorder_state = app_clone.state::<VideoRecorderState>();
    let mut recorder = crate::state::lock_recorder(&recorder_state.inner().0)
      .map_err(|e| crate::commands::ipc_display_context("Video recorder state poisoned", e))?;
    tracing::info!(
      operation = "pause_video_recording",
      stage = "recorder_acquired",
      elapsed_ms = received_at.elapsed().as_secs_f64() * 1000.0,
      "Pause acquired recorder state"
    );
    let elapsed_ms = recorder.get_elapsed_time().as_millis() as u64;
    let pause_result = recorder
      .pause_recording()
      .map_err(|e| crate::commands::ipc_context("Failed to pause recording", e));
    Ok::<_, String>((elapsed_ms, pause_result))
  })
  .await
  .map_err(|e| crate::commands::ipc_context("Pause task panicked", e))
  .and_then(|result| result)
  .and_then(|(elapsed, result)| result.map(|()| elapsed));

  let elapsed_ms = match result {
    Ok(elapsed) => elapsed,
    Err(error) => {
      tracing::error!(%error, "Pause failure ended the recording");
      super::stop::abort_failed_recording(&app).await;
      return Err(error);
    },
  };
  sync_hud_pause_flag(&app, true);

  crate::commands::set_recording_session_paused(&app, true);

  if !session_presentation_mode(&app).is_some_and(RecordingPresentationMode::is_silent)
    && let Err(e) = crate::overlay::OverlayWindow::update_recording_hud(elapsed_ms, true).await
  {
    tracing::debug!(
        operation = "pause_video_recording",
        error = %e,
        "Failed to update HUD"
    );
  }

  tracing::info!(
    operation = "pause_video_recording",
    elapsed_ms = received_at.elapsed().as_secs_f64() * 1000.0,
    "Video recording paused"
  );

  Ok(())
}

pub async fn resume_video_recording(app: AppHandle) -> Result<(), String> {
  let _operation = app.state::<VideoRecorderState>().claim_operation()?;
  let app_clone = app.clone();
  let result = tokio::task::spawn_blocking(move || {
    let recorder_state = app_clone.state::<VideoRecorderState>();
    let mut recorder = crate::state::lock_recorder(&recorder_state.inner().0)
      .map_err(|e| crate::commands::ipc_display_context("Video recorder state poisoned", e))?;
    let elapsed_ms = recorder.get_elapsed_time().as_millis() as u64;
    let resume_result = recorder
      .resume_recording()
      .map_err(|e| crate::commands::ipc_context("Failed to resume recording", e));
    Ok::<_, String>((elapsed_ms, resume_result))
  })
  .await
  .map_err(|e| crate::commands::ipc_context("Resume task panicked", e))
  .and_then(|result| result)
  .and_then(|(elapsed, result)| result.map(|()| elapsed));

  let elapsed_ms = match result {
    Ok(elapsed) => elapsed,
    Err(error) => {
      tracing::error!(%error, "Resume failure ended the recording");
      super::stop::abort_failed_recording(&app).await;
      return Err(error);
    },
  };
  sync_hud_pause_flag(&app, false);

  crate::commands::set_recording_session_paused(&app, false);

  if !session_presentation_mode(&app).is_some_and(RecordingPresentationMode::is_silent)
    && let Err(e) = crate::overlay::OverlayWindow::update_recording_hud(elapsed_ms, false).await
  {
    tracing::debug!(
        operation = "resume_video_recording",
        error = %e,
        "Failed to update HUD"
    );
  }

  tracing::info!(
    operation = "resume_video_recording",
    "Video recording resumed"
  );

  Ok(())
}

/// Mutes or unmutes the running recording's microphone and returns the state it is now in
/// (`None` when the recorder could not be reached). A refusal is logged and never ends the
/// recording.
pub async fn set_microphone(
  app: AppHandle,
  audible: bool,
) -> Option<Option<crate::video::controller::MicrophoneState>> {
  let recorder = std::sync::Arc::clone(&app.state::<VideoRecorderState>().inner().0);
  let result = tokio::task::spawn_blocking(move || {
    let mut recorder = crate::state::lock_recorder(&recorder)?;
    if let Err(error) = recorder.set_microphone(audible) {
      tracing::error!(%error, audible, "Microphone switch refused");
    }
    Ok::<_, String>(recorder.microphone())
  })
  .await;
  match result {
    Ok(Ok(state)) => Some(state),
    Ok(Err(error)) => {
      tracing::error!(%error, "Microphone switch could not reach the recorder");
      None
    },
    Err(error) => {
      tracing::error!(%error, "Microphone switch task panicked");
      None
    },
  }
}
