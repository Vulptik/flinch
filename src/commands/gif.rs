// src/commands/gif.rs
// GIF recording commands

use crate::app::context::AppHandle;

use crate::commands::capture_finalize::{
  CaptureResponse, build_capture_response, finalize_saved_artifact_with_notifications,
};

/// Start the GIF selection overlay
///
/// Opens a fullscreen overlay for area selection. After selecting an area,
/// the user can click "Start" to begin recording and "Stop" to finish.
/// The recorded frames are encoded as a GIF file.
pub async fn start_gif_selection(app: AppHandle) -> Result<CaptureResponse, String> {
  start_gif_selection_with(app, |app| async move { run_gif_selection(&app).await }).await
}

struct GifSelectionClaim<'a>(&'a crate::state::OverlayActivityState);
impl Drop for GifSelectionClaim<'_> {
  fn drop(&mut self) {
    self.0.release_gif();
  }
}

async fn start_gif_selection_with<
  F: std::future::Future<Output = Result<CaptureResponse, String>>,
>(
  app: AppHandle,
  run: impl FnOnce(AppHandle) -> F,
) -> Result<CaptureResponse, String> {
  tracing::info!(
    operation = "start_gif_selection",
    "Starting GIF selection overlay"
  );

  if let Some(response) = crate::commands::stop_active_recording_if_any(app.clone()).await? {
    return Ok(CaptureResponse::processing(response.message));
  }

  let overlay_state = app.try_state::<crate::commands::OverlayActivityState>();
  if let Some(overlay_state) = &overlay_state
    && !overlay_state.try_claim_gif()
  {
    return Ok(CaptureResponse::processing(
      "GIF overlay already active".to_string(),
    ));
  }

  let _claim = overlay_state.as_ref().map(|state| GifSelectionClaim(state));
  run(app.clone()).await
}

async fn run_gif_selection(app: &AppHandle) -> Result<CaptureResponse, String> {
  run_gif_selection_with(
    app,
    crate::recording_visibility::ensure_recording_visibility_policy,
    crate::overlay::OverlayWindow::show_gif_selection,
  )
  .await
}

async fn run_gif_selection_with<
  F: std::future::Future<Output = anyhow::Result<Option<crate::overlay::GifResult>>>,
>(
  app: &AppHandle,
  visibility: impl FnOnce(
    &AppHandle,
  ) -> Result<
    Option<crate::recording_visibility::RecordingPresentationMode>,
    String,
  >,
  overlay: impl FnOnce(crate::overlay::gif::GifRecordingConfig) -> F,
) -> Result<CaptureResponse, String> {
  let Some(presentation_mode) = visibility(app)? else {
    return Ok(CaptureResponse::cancelled());
  };
  let config = crate::config::Config::current(app);

  let output_dir = config.resolve_capture_path(crate::config::CaptureType::Gif);

  let gif_config = crate::overlay::gif::GifRecordingConfig {
    fps: config.gif_fps,
    max_duration_secs: config.gif_max_duration,
    output_dir,
    show_cursor: config.gif_show_cursor,
    filename_template: config.effective_filename_template().to_string(),
    app_handle: app.clone(),
    presentation_mode,
  };
  let result = overlay(gif_config)
    .await
    .map_err(|error| crate::commands::ipc_context("Failed to show GIF selection overlay", error));
  let result = gif_selection_result(result, || {
    let lang = crate::i18n::get_lang(app);
    match crate::i18n::native_settings(&lang, "recording_gif_failed") {
      Ok(title) => crate::notifications::show_error(app, &title, "notification_error_gif_start"),
      Err(error) => tracing::error!(%error, "GIF startup failure localization failed"),
    }
  })?;
  match result {
    Some(gif_result) => {
      tracing::info!(
        operation = "start_gif_selection",
        path = %gif_result.path.display(),
        width = gif_result.width,
        height = gif_result.height,
        duration = gif_result.duration_seconds,
        "GIF recording complete"
      );

      crate::commands::clear_recording_session(app);

      let config = crate::config::Config::current(app);
      let lang = crate::i18n::get_lang(app);
      let t = crate::i18n::get(&lang);
      match crate::i18n::native_settings(&lang, "recording_gif_saved") {
        Ok(title) => {
          finalize_saved_artifact_with_notifications(
            app,
            &config,
            "gif",
            &gif_result.path,
            "start_gif_selection",
            &title,
            &format!("{}{}", t.body_saved_to_prefix, gif_result.path.display()),
          )
          .await;
        },
        Err(error) => {
          tracing::error!(%error, "GIF saved notification localization failed");
          crate::recent_captures::record(
            app,
            &gif_result.path,
            crate::recent_captures::CaptureKind::Gif,
          );
        },
      }
      Ok(
        build_capture_response(Some(&gif_result.path))
          .with_dimensions(gif_result.width, gif_result.height)
          .with_duration(gif_result.duration_seconds),
      )
    },
    None => {
      crate::commands::clear_recording_session(app);
      tracing::info!(operation = "start_gif_selection", "GIF selection cancelled");
      Ok(CaptureResponse::cancelled())
    },
  }
}

fn gif_selection_result<T>(
  result: Result<Option<T>, String>,
  report_failure: impl FnOnce(),
) -> Result<Option<T>, String> {
  if let Err(error) = &result {
    tracing::error!(%error, "GIF selection startup failed");
    report_failure();
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  fn isolated() -> AppHandle {
    AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        gif_show_cursor: false,
        gif_fps: 24,
        gif_max_duration: 12,
        notifications_enabled: false,
        notification_sound: false,
        ..Default::default()
      },
      PathBuf::new(),
    )
  }

  #[tokio::test]
  async fn dropped_gif_selection_releases_its_overlay_claim() {
    let app = isolated();
    assert!(
      tokio::time::timeout(
        std::time::Duration::from_millis(5),
        start_gif_selection_with(app.clone(), |_| std::future::pending())
      )
      .await
      .is_err()
    );
    assert!(
      !app
        .try_state::<crate::state::OverlayActivityState>()
        .unwrap()
        .is_any_active()
    );
  }

  #[tokio::test]
  async fn gif_command_preserves_preferences_results_and_critical_failure_causes() {
    use crate::recording_visibility::RecordingPresentationMode;
    for scenario in 0..4 {
      let app = isolated();
      let result = start_gif_selection_with(app.clone(), |app| async move {
        assert!(
          app
            .try_state::<crate::state::OverlayActivityState>()
            .unwrap()
            .is_gif_active()
        );
        run_gif_selection_with(
          &app,
          |_| {
            Ok(if scenario == 0 {
              None
            } else {
              Some(RecordingPresentationMode::Silent)
            })
          },
          |config| async move {
            assert_eq!(
              (config.fps, config.max_duration_secs, config.show_cursor),
              (24, 12, false)
            );
            assert_eq!(config.presentation_mode, RecordingPresentationMode::Silent);
            match scenario {
              1 => Ok(None),
              2 => anyhow::bail!("encoder connection lost"),
              _ => Ok(Some(crate::overlay::GifResult {
                path: "fixture.gif".into(),
                width: 64,
                height: 48,
                duration_seconds: 3,
              })),
            }
          },
        )
        .await
      })
      .await;
      if scenario == 2 {
        assert!(result.unwrap_err().contains("encoder connection lost"));
        let notices = app
          .try_state::<crate::overlay::notification::NotificationManager>()
          .unwrap()
          .take_test_requests();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].critical);
      } else {
        let result = result.unwrap();
        if scenario < 2 {
          assert_eq!(result.cancelled, Some(true));
        } else {
          assert!(result.success);
          assert_eq!(
            (result.width, result.height, result.duration),
            (Some(64), Some(48), Some(3))
          );
        }
      }
      assert!(
        !app
          .try_state::<crate::state::OverlayActivityState>()
          .unwrap()
          .is_any_active()
      );
      assert!(crate::commands::current_recording_session(&app).is_none());
    }
    let app = isolated();
    let state = app
      .try_state::<crate::state::OverlayActivityState>()
      .unwrap();
    assert!(state.try_claim_screenshot());
    let result = start_gif_selection_with(app.clone(), |_| async {
      panic!("existing overlay must retain ownership")
    })
    .await
    .unwrap();
    assert!(matches!(
      result.status,
      Some(crate::commands::capture_finalize::RecordingLifecycleStatus::Processing)
    ));
    assert!(state.is_screenshot_active());
    state.release_screenshot();
  }

  #[test]
  fn gif_selection_reports_failure_but_keeps_cancellation_and_success_quiet() {
    for (result, expected_reports) in [
      (Ok(None), 0),
      (Ok(Some(1)), 0),
      (Err("Window creation failed".into()), 1),
    ] {
      let reports = std::cell::Cell::new(0);
      let expected = result.clone();
      let actual = gif_selection_result(result, || reports.set(reports.get() + 1));
      assert_eq!(actual, expected);
      assert_eq!(reports.get(), expected_reports);
    }
  }

  #[test]
  fn test_gif_command_signature() {
    let _: fn(AppHandle) -> _ = start_gif_selection;
  }

  #[test]
  fn test_gif_result_json_structure() {
    let path = PathBuf::from("/test/path/output.gif");
    let result = serde_json::json!({
      "success": true,
      "path": path.to_string_lossy(),
      "width": 800,
      "height": 600,
      "duration": 5.0,
    });

    assert_eq!(result["success"], true);
    assert_eq!(result["width"], 800);
    assert_eq!(result["height"], 600);
    assert_eq!(result["duration"], 5.0);
  }

  #[test]
  fn test_gif_cancelled_json_structure() {
    let result = serde_json::json!({
      "success": false,
      "cancelled": true,
    });

    assert_eq!(result["success"], false);
    assert_eq!(result["cancelled"], true);
  }

  #[test]
  fn test_gif_result_dimensions_are_preserved() {
    let output_dir = PathBuf::from("/test/output");
    let result = serde_json::json!({
      "success": true,
      "path": output_dir.join("clip.gif").to_string_lossy(),
      "width": 320,
      "height": 180,
      "duration": 2.0,
    });

    assert!(
      result["path"]
        .as_str()
        .is_some_and(|path| path.ends_with("clip.gif"))
    );
    assert_eq!(result["width"], 320);
    assert_eq!(result["height"], 180);
  }
}
