use crate::app::context::AppHandle;

#[cfg(test)]
mod tests;

pub struct RecordingCoordinator;

struct AreaSelectionClaim<'a>(&'a crate::state::OverlayActivityState);
impl Drop for AreaSelectionClaim<'_> {
  fn drop(&mut self) {
    self.0.release_video_area_selection();
  }
}

impl RecordingCoordinator {
  pub fn current_session(app: &AppHandle) -> Option<crate::state::ActiveRecordingSession> {
    crate::state::current_recording_session(app)
  }

  async fn stop_active_recording(
    app: AppHandle,
    operation: &'static str,
    trigger: &'static str,
  ) -> bool {
    match crate::commands::stop_active_recording_if_any(app).await {
      Ok(Some(_)) => true,
      Ok(None) => false,
      Err(error) => {
        tracing::error!(
          operation = operation,
          shortcut_type = trigger,
          error = %error,
          "Failed to stop active recording"
        );
        true
      },
    }
  }

  pub async fn toggle_tray_record(app: AppHandle) {
    match Self::current_session(&app) {
      Some(crate::state::ActiveRecordingSession {
        kind: crate::state::ActiveRecordingKind::Video,
        ..
      }) => {
        if let Err(error) = crate::commands::video::stop::stop_video_recording(app).await {
          tracing::error!(
            operation = "tray_menu",
            action = "stop_recording",
            error = %error,
            "Failed to stop video recording"
          );
        }
      },
      Some(crate::state::ActiveRecordingSession {
        kind: crate::state::ActiveRecordingKind::Gif,
        ..
      }) => {
        crate::overlay::gif::stop_gif_recording();
      },
      None => Self::start_fullscreen_recording(app, "tray_menu").await,
    }
  }

  pub async fn toggle_tray_pause(app: AppHandle) {
    let Some(session) = Self::current_session(&app) else {
      return;
    };

    match (session.kind, session.paused) {
      (crate::state::ActiveRecordingKind::Video, false) => {
        if let Err(error) = crate::commands::video::control::pause_video_recording(app).await {
          tracing::error!(
            operation = "tray_menu",
            action = "pause_recording",
            error = %error,
            "Failed to pause video recording"
          );
        }
      },
      (crate::state::ActiveRecordingKind::Video, true) => {
        if let Err(error) = crate::commands::video::control::resume_video_recording(app).await {
          tracing::error!(
            operation = "tray_menu",
            action = "resume_recording",
            error = %error,
            "Failed to resume video recording"
          );
        }
      },
      (crate::state::ActiveRecordingKind::Gif, false) => crate::overlay::gif::pause_gif_recording(),
      (crate::state::ActiveRecordingKind::Gif, true) => {
        crate::overlay::gif::resume_gif_recording();
      },
    }
  }

  pub async fn toggle_area_shortcut(app: AppHandle) {
    if Self::stop_active_recording(app.clone(), "shortcut_handler", "record_area").await {
      return;
    }

    let options = crate::overlay::selection_host::SelectionOptions::for_app(&app);
    Self::select_area_with(
      app,
      crate::recording_visibility::ensure_recording_visibility_policy,
      move || crate::overlay::OverlayWindow::show_area_recording_selection(options),
      |app, result, presentation_mode| async move {
        crate::commands::video::start::start_video_capture_session(
          &app,
          result.x,
          result.y,
          result.width,
          result.height,
          0,
          result.source_app,
          crate::commands::RecordingMode::Area,
          presentation_mode,
        )
        .await
      },
    )
    .await;
  }

  async fn select_area_with<S, SF, R, RF>(
    app: AppHandle,
    visibility: impl FnOnce(
      &AppHandle,
    ) -> Result<
      Option<crate::recording_visibility::RecordingPresentationMode>,
      String,
    >,
    select: S,
    start: R,
  ) where
    S: FnOnce() -> SF,
    SF: std::future::Future<Output = anyhow::Result<Option<crate::overlay::AreaRecordingResult>>>,
    R: FnOnce(
      AppHandle,
      crate::overlay::AreaRecordingResult,
      crate::recording_visibility::RecordingPresentationMode,
    ) -> RF,
    RF: std::future::Future<Output = Result<std::path::PathBuf, String>>,
  {
    let overlay_state = match app.try_state::<crate::state::OverlayActivityState>() {
      Some(state) => state,
      None => {
        tracing::error!(
          operation = "shortcut_handler",
          shortcut_type = "record_area",
          "OverlayActivityState not found"
        );
        return;
      },
    };

    if !overlay_state.try_claim_video_area_selection() {
      tracing::debug!(
        operation = "shortcut_handler",
        shortcut_type = "record_area",
        "Overlay already active, ignoring shortcut"
      );
      return;
    }
    let claim = AreaSelectionClaim(&overlay_state);

    let presentation_mode = match visibility(&app) {
      Ok(Some(mode)) => mode,
      Ok(None) => {
        tracing::debug!("Area recording start cancelled before selection");
        return;
      },
      Err(error) => {
        tracing::error!(
          operation = "shortcut_handler",
          shortcut_type = "record_area",
          error = %error,
          "Area recording visibility policy failed"
        );
        return;
      },
    };

    let overlay_result = select().await;

    drop(claim);

    match overlay_result {
      Ok(Some(result)) => {
        tracing::info!(
          operation = "shortcut_handler",
          shortcut_type = "record_area",
          x = result.x,
          y = result.y,
          width = result.width,
          height = result.height,
          "Area selected, starting video recording"
        );
        crate::last_region::remember(
          &app,
          crate::last_region::Region {
            x: result.x,
            y: result.y,
            width: result.width,
            height: result.height,
          },
        );

        if !crate::capture_delay::wait(&app, crate::capture_delay::Trigger::RecordArea)
          .await
          .proceeds()
        {
          tracing::debug!(
            operation = "shortcut_handler",
            shortcut_type = "record_area",
            "Area recording cancelled during its countdown"
          );
          return;
        }
        match start(app.clone(), result, presentation_mode).await {
          Ok(output_path) => {
            tracing::info!(
              operation = "shortcut_handler",
              shortcut_type = "record_area",
              path = %output_path.display(),
              "Area video recording started"
            );
          },
          Err(e) => {
            if let Some(hud_state) = app.try_state::<crate::state::RecordingHudState>() {
              hud_state.sync_stopped();
              hud_state.clear();
            }
            tracing::error!(
              operation = "shortcut_handler",
              shortcut_type = "record_area",
              error = %e,
              "Failed to start video recording"
            );
          },
        }
      },
      Ok(None) => {
        tracing::debug!(
          operation = "shortcut_handler",
          shortcut_type = "record_area",
          "Area selection cancelled by user"
        );
      },
      Err(e) => {
        tracing::error!(
          operation = "shortcut_handler",
          shortcut_type = "record_area",
          error = %e,
          "Failed to show area recording selection overlay"
        );
      },
    }
  }

  pub async fn toggle_fullscreen_shortcut(app: AppHandle) {
    if Self::stop_active_recording(app.clone(), "shortcut_handler", "record_fullscreen").await {
      return;
    }

    Self::start_fullscreen_recording(app, "shortcut_handler").await;
  }

  pub async fn toggle_active_window_shortcut(app: AppHandle) {
    if Self::stop_active_recording(app.clone(), "shortcut_handler", "record_active_window").await {
      return;
    }

    let window_rect = {
      use crate::video::capture::windows::{get_foreground_window_handle, get_window_rect};
      get_foreground_window_handle().and_then(get_window_rect)
    };

    match window_rect {
      Some((x, y, width, height)) => {
        if let Err(e) = crate::commands::video::start::start_video_recording(
          app,
          x,
          y,
          width,
          height,
          0,
          "active_window".to_string(),
        )
        .await
        {
          tracing::error!(
            operation = "shortcut_handler",
            shortcut_type = "record_active_window",
            error = %e,
            "Failed to start video recording"
          );
        }
      },
      None => {
        tracing::error!(
          operation = "shortcut_handler",
          shortcut_type = "record_active_window",
          "No foreground window found"
        );
      },
    }
  }

  async fn start_fullscreen_recording(app: AppHandle, operation: &'static str) {
    let monitor = crate::monitors::monitor_under_cursor()
      .or_else(crate::monitors::get_primary_monitor)
      .unwrap_or_else(|| {
        tracing::warn!(
          operation = operation,
          action = "start_recording",
          "Could not detect monitor dimensions, using fallback 1920x1080"
        );
        crate::monitors::MonitorInfo {
          id: String::new(),
          name: String::new(),
          x: 0,
          y: 0,
          width: 1920,
          height: 1080,
          is_primary: true,
          dpi_scale: 1.0,
          logical_x: 0,
          logical_y: 0,
          logical_width: 1920,
          logical_height: 1080,
        }
      });

    if let Err(error) = crate::commands::video::start::start_video_recording(
      app,
      monitor.x,
      monitor.y,
      monitor.width,
      monitor.height,
      0,
      "fullscreen".to_string(),
    )
    .await
    {
      tracing::error!(
        operation = operation,
        action = "start_recording",
        error = %error,
        "Failed to start video recording"
      );
    }
  }
}
