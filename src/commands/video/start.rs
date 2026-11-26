//! Video recording start commands
//!
//! ## Purpose
//! Handles starting video recordings:
//! - `start_video_recording` - Native operation with HUD display
//! - `start_video_recorder` - Core recording logic used by shortcuts

use std::path::PathBuf;

use crate::app::context::AppHandle;

use crate::recording_visibility::RecordingPresentationMode;
use crate::video::capture::RecordingMode;

use super::startup::{
  bootstrap_recording_hud, build_start_recording_response, cleanup_failed_video_start,
  detect_source_app_at_region, launch_video_recorder, prepare_video_capture_plan,
  should_show_recording_hud, validate_recording_request,
};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_video_capture_session(
  app: &AppHandle,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  source_app: Option<String>,
  mode: RecordingMode,
  presentation_mode: RecordingPresentationMode,
) -> Result<PathBuf, String> {
  validate_recording_request(width, height, fps)?;
  let prepared = prepare_video_capture_plan(app, x, y, width, height, fps, source_app, mode)?;

  let hud_runtime =
    bootstrap_recording_hud(app, mode, presentation_mode, x, y, width, height).await;

  match launch_video_recorder(app, prepared, presentation_mode).await {
    Ok(path) => {
      if hud_runtime
        .controls
        .as_ref()
        .is_some_and(|controls| controls.claim_stop())
      {
        super::stop::stop_video_recording(app.clone()).await?;
      }
      Ok(path)
    },
    Err(error) => {
      cleanup_failed_video_start(app, &hud_runtime).await;
      super::stop::notify_recording_failure(app);
      Err(error)
    },
  }
}

pub fn cleanup_orphaned_recording_temp_files(_app: &AppHandle) -> anyhow::Result<usize> {
  cleanup_os_recording_temp_root()
}

/// Saves interrupted native recordings off the main thread, then reports each outcome.
pub fn recover_interrupted_recordings(app: &AppHandle) {
  if let Err(error) = recover_in(app, crate::capture_artifact::recording_temp_root()) {
    tracing::error!(%error, "Cannot start interrupted recording recovery");
  }
}

fn recover_in(app: &AppHandle, root: PathBuf) -> std::io::Result<std::thread::JoinHandle<()>> {
  let app = app.clone();
  std::thread::Builder::new()
    .name("flinch-recording-recovery".into())
    .spawn(move || {
      let results =
        crate::video::recovery::recover_all(&root, std::process::id(), &encode_recovered_gif);
      for (folder, result) in results {
        report_recovery(&app, &folder, result);
      }
    })
}

fn encode_recovered_gif(input: &std::path::Path, output: &std::path::Path) -> Result<(), String> {
  // Every app GIF uses the default palette settings, so recovery matches a normal finish.
  let options = crate::video::native::gif_options();
  flinch_video_engine::windows::gif::encode_with(input, output, options, || Ok(()))
    .map(|_| ())
    .map_err(|error| format!("Encode {}: {error}", output.display()))
}

fn report_recovery(
  app: &AppHandle,
  folder: &std::path::Path,
  result: Result<crate::video::recovery::Recovered, String>,
) {
  use crate::video::recovery::Recovered;
  let lang = crate::i18n::get_lang(app);
  match result {
    Ok(Recovered::Saved(path)) => {
      tracing::warn!(path = %path.display(), "Interrupted recording recovered");
      crate::recent_captures::record(app, &path, crate::recent_captures::CaptureKind::Recording);
      match crate::i18n::native_settings(&lang, "recording_recovered") {
        Ok(title) => {
          let body = format!(
            "{}{}",
            crate::i18n::get(&lang).body_saved_to_prefix,
            path.display()
          );
          crate::notifications::show_with_open_folder(app, &title, &body, path);
        },
        Err(error) => tracing::error!(%error, "Recovery notification localization failed"),
      }
    },
    Ok(Recovered::Empty) => {
      tracing::info!(path = %folder.display(), "Interrupted recording had no media; removed");
    },
    Ok(Recovered::InUse) => {
      tracing::warn!(path = %folder.display(), "Interrupted recording still open; retried next launch");
    },
    Err(error) => {
      tracing::error!(%error, path = %folder.display(), "Interrupted recording not recovered; kept");
      match crate::i18n::native_settings(&lang, "recording_recovery_failed") {
        Ok(title) => crate::notifications::show_error(app, &title, "notification_error_recovery"),
        Err(error) => tracing::error!(%error, "Recovery notification localization failed"),
      }
    },
  }
}

fn cleanup_os_recording_temp_root() -> anyhow::Result<usize> {
  let root = crate::capture_artifact::recording_temp_root();
  cleanup_recording_root(&root)
}

fn cleanup_recording_root(root: &std::path::Path) -> anyhow::Result<usize> {
  use anyhow::Context;
  if !root.exists() {
    return Ok(0);
  }

  let mut removed = 0;
  for entry in std::fs::read_dir(root).with_context(|| {
    format!(
      "Failed to read recording temp root for orphan cleanup: {}",
      root.display()
    )
  })? {
    let entry = entry?;
    let path = entry.path();
    if entry.file_type()?.is_dir() {
      if path.join("recovery.json").exists() {
        tracing::warn!(path = %path.display(), "Interrupted recording retained for recovery");
        continue;
      }
      // Older sessions have no marker. Preserve media as well, rather than
      // interpreting an application crash as permission to erase a recording.
      // The native engine stages inside its own session directory, without recovery.json.
      let mut has_media = false;
      let mut has_native_session = false;
      for item in std::fs::read_dir(&path)? {
        let Ok(item) = item else {
          has_media = true;
          continue;
        };
        let name = item.file_name();
        if name.to_str().is_some_and(|name| {
          name.starts_with(flinch_video_engine::artifact::SESSION_DIRECTORY_PREFIX)
        }) {
          has_native_session = true;
        } else if item
          .path()
          .extension()
          .is_some_and(|ext| ext == "mp4" || ext == "pcm")
        {
          has_media = true;
        }
      }
      if has_native_session {
        tracing::warn!(path = %path.display(), "Interrupted native recording retained for recovery");
        continue;
      }
      if has_media {
        tracing::warn!(path = %path.display(), "Legacy recording media retained for recovery");
        continue;
      }
      std::fs::remove_dir_all(&path)?;
    } else {
      std::fs::remove_file(&path)?;
    }
    removed += 1;
  }
  if std::fs::read_dir(root)?.next().is_none() {
    std::fs::remove_dir(root)?;
    removed = removed.max(1);
  }
  if let Some(app_temp_dir) = root.parent() {
    crate::capture_artifact::remove_dir_if_empty_best_effort(
      app_temp_dir,
      "cleanup_orphaned_recording_temp_files",
      "app temp directory",
    );
  }

  Ok(removed)
}

/// Exposes the runtime temp-path convention to recording lifecycle integration tests.
pub fn recording_temp_path(path: &std::path::Path) -> PathBuf {
  super::startup::recording_temp_path(path)
}

pub async fn start_video_recording(
  app: AppHandle,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  mode: String,
) -> Result<crate::commands::video::RecordingStartResponse, String> {
  start_video_recording_with(
    app,
    x,
    y,
    width,
    height,
    fps,
    mode,
    crate::recording_visibility::ensure_recording_visibility_policy,
    |app, (x, y, width, height), fps, mode, presentation| async move {
      start_video_capture_session(&app, x, y, width, height, fps, None, mode, presentation).await
    },
  )
  .await
}

#[cfg(test)]
mod start_tests;

#[allow(clippy::too_many_arguments)]
async fn start_video_recording_with<F: std::future::Future<Output = Result<PathBuf, String>>>(
  app: AppHandle,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  mode: String,
  visibility: impl FnOnce(&AppHandle) -> Result<Option<RecordingPresentationMode>, String>,
  start: impl FnOnce(
    AppHandle,
    (i32, i32, u32, u32),
    u32,
    RecordingMode,
    RecordingPresentationMode,
  ) -> F,
) -> Result<crate::commands::video::RecordingStartResponse, String> {
  if let Some(response) = crate::commands::stop_active_recording_if_any(app.clone()).await? {
    return Ok(crate::commands::video::RecordingStartResponse {
      status: response.status,
      message: Some(response.message),
      path: None,
      source_app: None,
    });
  }

  let recording_mode = mode
    .parse::<RecordingMode>()
    .map_err(|_| format!("Invalid recording mode: '{mode}'"))?;
  let Some(presentation_mode) = visibility(&app)? else {
    return Ok(crate::commands::video::RecordingStartResponse {
      status: crate::commands::video::RecordingLifecycleStatus::Idle,
      message: None,
      path: None,
      source_app: None,
    });
  };
  let trigger = match recording_mode {
    RecordingMode::Area => crate::capture_delay::Trigger::RecordArea,
    RecordingMode::Fullscreen => crate::capture_delay::Trigger::RecordFullscreen,
    RecordingMode::ActiveWindow => crate::capture_delay::Trigger::RecordActiveWindow,
  };
  if !crate::capture_delay::wait(&app, trigger).await.proceeds() {
    return Ok(crate::commands::video::RecordingStartResponse {
      status: crate::commands::video::RecordingLifecycleStatus::Idle,
      message: None,
      path: None,
      source_app: None,
    });
  }
  let effective_fps = if fps == 0 {
    crate::config::Config::current(&app).video_fps
  } else {
    fps
  };
  let output_path = start(
    app.clone(),
    (x, y, width, height),
    fps,
    recording_mode,
    presentation_mode,
  )
  .await?;

  if should_show_recording_hud(recording_mode, presentation_mode) {
    tracing::info!(
      operation = "start_video_recording",
      mode = ?recording_mode,
      "Video capture started with pre-created recording HUD"
    );
  }

  let source_app = detect_source_app_at_region(x, y, width, height);

  tracing::info!(
    operation = "start_video_recording",
    x,
    y,
    width,
    height,
    fps = effective_fps,
    mode = %mode,
    path = %output_path.display(),
    source_app = ?source_app,
    "Video recording started"
  );

  Ok(build_start_recording_response(
    &output_path,
    source_app.as_deref(),
  ))
}
