use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::app::context::AppHandle;

use crate::capture_plan::{
  CaptureCoordinateSpace, SharedCapturePlanInput, SharedCapturePlanOptions,
  build_shared_capture_plan,
};
use crate::commands::{ActiveRecordingKind, VideoRecorderState};
use crate::config::{CaptureType, Config};
use crate::recording_visibility::RecordingPresentationMode;
use crate::screenshot::CaptureBackendKind;
use crate::video::capture::{CaptureConfig, RecordingMode};
use crate::video::config::{RecordingProfile, VideoConfig};
use flinch_video_engine::gif::GifOptions;

const RECORDING_TEMP_MARKER: &str = ".flinch-part";

#[derive(Debug, Clone)]
pub(crate) struct PreparedVideoStart {
  pub output_path: PathBuf,
  pub temp_output_path: PathBuf,
  pub capture_config: CaptureConfig,
  pub video_config: VideoConfig,
  pub gif_options: Option<GifOptions>,
  pub active_kind: ActiveRecordingKind,
}

#[derive(Debug, Clone, Default)]
pub(super) struct VideoStartOverrides {
  pub forced_backend: Option<CaptureBackendKind>,
  pub output_path: Option<PathBuf>,
  pub record_audio: Option<bool>,
}

#[derive(Clone, Default)]
pub(super) struct RecordingHudRuntime {
  pub controls: Option<Arc<crate::state::RecordingControls>>,
}

pub(super) fn validate_recording_request(width: u32, height: u32, fps: u32) -> Result<(), String> {
  if width == 0 || height == 0 {
    return Err("Invalid recording dimensions: width and height must be greater than 0".into());
  }
  if width > 16384 || height > 16384 {
    return Err(
      "Invalid recording dimensions: exceeds maximum supported resolution (16384x16384)".into(),
    );
  }
  if fps != 0 && fps > 60 {
    return Err(format!(
      "Invalid fps: {fps} exceeds maximum supported value (60)"
    ));
  }
  Ok(())
}

pub(super) fn should_show_recording_hud(
  mode: RecordingMode,
  presentation_mode: RecordingPresentationMode,
) -> bool {
  !presentation_mode.is_silent()
    && matches!(
      mode,
      RecordingMode::Fullscreen | RecordingMode::Area | RecordingMode::ActiveWindow
    )
}

pub(super) fn build_start_recording_response(
  output_path: &Path,
  source_app: Option<&str>,
) -> crate::commands::video::RecordingStartResponse {
  crate::commands::video::RecordingStartResponse {
    status: crate::commands::video::RecordingLifecycleStatus::Recording,
    message: None,
    path: Some(output_path.to_string_lossy().to_string()),
    source_app: source_app.map(str::to_string),
  }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_video_capture_plan(
  app: &AppHandle,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  source_app: Option<String>,
  mode: RecordingMode,
) -> Result<PreparedVideoStart, String> {
  prepare_video_capture_plan_with_overrides(
    app,
    x,
    y,
    width,
    height,
    fps,
    source_app,
    mode,
    VideoStartOverrides::default(),
  )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_gif_capture_plan(
  app: &AppHandle,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  source_app: Option<String>,
) -> Result<PreparedVideoStart, String> {
  let config = Config::current(app);
  let output_dir = config.resolve_capture_path(CaptureType::Gif);
  prepare_gif_capture_plan_in_directory(&config, x, y, width, height, fps, source_app, output_dir)
}

#[allow(clippy::too_many_arguments)]
fn prepare_gif_capture_plan_in_directory(
  config: &Config,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  source_app: Option<String>,
  output_dir: PathBuf,
) -> Result<PreparedVideoStart, String> {
  std::fs::create_dir_all(&output_dir)
    .map_err(|e| crate::commands::ipc_context("Failed to create GIF output directory", e))?;

  let effective_fps = if fps == 0 { config.gif_fps } else { fps };
  let output_path = crate::gif::build_gif_output_path(
    &output_dir,
    config.effective_filename_template(),
    source_app.as_deref(),
  );
  let temp_output_path = crate::gif::build_intermediate_output_path(&output_path);

  let capture_config = build_recording_capture_config(
    config.gif_show_cursor,
    config.gif_show_clicks,
    x,
    y,
    width,
    height,
    effective_fps,
    RecordingMode::Area,
    // The engine chooses its own capture backend during Prepare.
    None,
    SharedCapturePlanOptions::GIF,
  );

  Ok(PreparedVideoStart {
    output_path,
    temp_output_path,
    capture_config,
    video_config: crate::gif::build_gif_video_config(&output_dir, effective_fps),
    gif_options: Some(crate::video::native::gif_options()),
    active_kind: ActiveRecordingKind::Gif,
  })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_video_capture_plan_with_overrides(
  app: &AppHandle,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  source_app: Option<String>,
  mode: RecordingMode,
  overrides: VideoStartOverrides,
) -> Result<PreparedVideoStart, String> {
  let config = Config::current(app);
  let videos_path = config.resolve_capture_path(CaptureType::Video);
  prepare_video_capture_plan_in_directory(
    &config,
    x,
    y,
    width,
    height,
    fps,
    source_app,
    mode,
    overrides,
    videos_path,
  )
}

#[allow(clippy::too_many_arguments)]
fn prepare_video_capture_plan_in_directory(
  config: &Config,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  source_app: Option<String>,
  mode: RecordingMode,
  overrides: VideoStartOverrides,
  videos_path: PathBuf,
) -> Result<PreparedVideoStart, String> {
  std::fs::create_dir_all(&videos_path)
    .map_err(|e| crate::commands::ipc_context("Failed to create videos directory", e))?;

  let effective_fps = if fps == 0 { config.video_fps } else { fps };
  let filename_ctx = crate::filename::FilenameContext {
    capture_type: "recording",
    app_name: source_app.as_deref(),
    now: chrono::Local::now(),
  };
  let stem = crate::filename::render_template(config.effective_filename_template(), &filename_ctx);
  let output_path = overrides
    .output_path
    .unwrap_or_else(|| next_available_path(&videos_path.join(format!("{}.mp4", stem))));
  let temp_output_path = recording_temp_path(&output_path);

  let capture_config = build_recording_capture_config(
    config.show_cursor,
    config.show_clicks,
    x,
    y,
    width,
    height,
    effective_fps,
    mode,
    overrides.forced_backend,
    SharedCapturePlanOptions::VIDEO,
  );

  let video_config = VideoConfig {
    videos_path: Some(PathBuf::from(&videos_path)),
    fps: effective_fps,
    quality: config.video_quality,
    hardware_encoding: config.video_hardware_encoding,
    audio: crate::video::config::AudioConfig {
      enabled: overrides.record_audio.unwrap_or(config.record_audio),
    },
    microphone: config
      .record_microphone
      .then(|| crate::video::config::MicrophoneConfig {
        device: config.microphone_device.clone(),
      }),
    recording_profile: RecordingProfile::Standard,
  };

  Ok(PreparedVideoStart {
    output_path,
    temp_output_path,
    capture_config,
    video_config,
    gif_options: None,
    active_kind: ActiveRecordingKind::Video,
  })
}

#[allow(clippy::too_many_arguments)]
fn build_recording_capture_config(
  show_cursor: bool,
  show_clicks: bool,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  fps: u32,
  mode: RecordingMode,
  forced_backend: Option<CaptureBackendKind>,
  plan_options: SharedCapturePlanOptions,
) -> CaptureConfig {
  let monitors = crate::monitors::list_monitors();
  let monitor_id = crate::monitors::monitor_under_cursor()
    .or_else(crate::monitors::get_primary_monitor)
    .map(|monitor| {
      format!(
        "{},{},{}x{}",
        monitor.x, monitor.y, monitor.width, monitor.height
      )
    });
  let capture_plan = build_shared_capture_plan(
    SharedCapturePlanInput {
      x,
      y,
      width,
      height,
      show_cursor,
      coordinate_space: CaptureCoordinateSpace::Logical,
    },
    plan_options,
    &monitors,
  );

  let mut capture_config = CaptureConfig {
    fps,
    x: capture_plan.capture_x,
    y: capture_plan.capture_y,
    width: capture_plan.capture_width,
    height: capture_plan.capture_height,
    mode,
    monitor_id,
    show_cursor,
    show_clicks,
    forced_backend,
    monitor_ids: capture_plan.monitor_ids,
  };
  capture_config.align_dimensions();
  capture_config.align_coordinates();
  capture_config
}

pub(crate) fn start_prepared_recording_blocking(
  app: &AppHandle,
  prepared: &PreparedVideoStart,
) -> Result<PathBuf, String> {
  let state = app.state::<VideoRecorderState>();
  let _operation = state.claim_operation()?;
  let recorder = Arc::clone(&state.inner().0);
  let output_path = prepared.output_path.clone();
  let temp_output_path = prepared.temp_output_path.clone();
  let capture_config = prepared.capture_config.clone();
  let video_config = prepared.video_config.clone();
  let gif_options = prepared.gif_options;

  match recorder.try_lock() {
    Ok(mut guard) => {
      guard
        .set_config(video_config)
        .map_err(|e| crate::commands::ipc_display_context("Failed to update recorder config", e))?;
      guard.set_gif_options(gif_options).map_err(|e| {
        crate::commands::ipc_display_context("Failed to configure recorder finalize mode", e)
      })?;
      guard
        .start_recording(capture_config, temp_output_path, output_path.clone())
        .map_err(|e| crate::commands::ipc_display_context("Failed to start recorder runtime", e))?;
      Ok(output_path)
    },
    Err(e) => Err(crate::commands::ipc_display_context(
      "Failed to lock recorder",
      e,
    )),
  }
}

pub(super) fn launch_video_recorder(
  app: &AppHandle,
  prepared: PreparedVideoStart,
  presentation_mode: RecordingPresentationMode,
) -> impl std::future::Future<Output = Result<PathBuf, String>> {
  let app = app.clone();
  async move {
    let prepared_for_task = prepared.clone();
    let app_for_task = app.clone();

    tokio::task::spawn_blocking(move || {
      start_prepared_recording_blocking(&app_for_task, &prepared_for_task)
    })
    .await
    .map_err(|e| crate::commands::ipc_context("Failed to spawn recording task", e))?
    .map_err(|e| crate::commands::ipc_display_context("Failed to start recording", e))
    .inspect(|_| {
      crate::commands::set_recording_session(&app, prepared.active_kind, presentation_mode);
    })
  }
}

pub(super) async fn bootstrap_recording_hud(
  app: &AppHandle,
  mode: RecordingMode,
  presentation_mode: RecordingPresentationMode,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
) -> RecordingHudRuntime {
  bootstrap_recording_hud_with(
    app,
    mode,
    presentation_mode,
    x,
    y,
    width,
    height,
    |selection_rect, screen_width, screen_height, controls, lang| async move {
      crate::overlay::OverlayWindow::show_recording_hud(
        std::time::Instant::now(),
        crate::overlay::RecordingStatus::Starting,
        selection_rect,
        screen_width,
        screen_height,
        Some(controls),
        lang,
      )
      .await
    },
    start_recording_hud_supervisor,
  )
  .await
}

#[allow(clippy::too_many_arguments)]
async fn bootstrap_recording_hud_with<Fut: std::future::Future<Output = anyhow::Result<()>>>(
  app: &AppHandle,
  mode: RecordingMode,
  presentation_mode: RecordingPresentationMode,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  show: impl FnOnce(
    Option<(f64, f64, f64, f64)>,
    Option<u32>,
    Option<u32>,
    Arc<crate::state::RecordingControls>,
    String,
  ) -> Fut,
  supervise: impl FnOnce(AppHandle, Arc<crate::state::RecordingControls>),
) -> RecordingHudRuntime {
  if !should_show_recording_hud(mode, presentation_mode) {
    return RecordingHudRuntime::default();
  }

  let controls = Arc::new(crate::state::RecordingControls::new());

  if let Some(hud_state) = app.try_state::<crate::commands::RecordingHudState>() {
    hud_state.set(controls.clone());
  }

  let selection_rect = if matches!(mode, RecordingMode::Area | RecordingMode::ActiveWindow) {
    Some(build_area_selection_rect(x, y, width, height))
  } else {
    None
  };

  let (screen_width, screen_height) = if selection_rect.is_some() {
    crate::monitors::monitor_under_cursor()
      .or_else(crate::monitors::get_primary_monitor)
      .map(|m| (Some(m.width), Some(m.height)))
      .unwrap_or_else(|| {
        let (width, height) = crate::video::capture::windows::get_primary_monitor_resolution();
        (Some(width), Some(height))
      })
  } else {
    (None, None)
  };

  let show_result = show(
    selection_rect,
    screen_width,
    screen_height,
    controls.clone(),
    crate::i18n::get_lang(app),
  )
  .await;

  if let Err(e) = show_result {
    tracing::warn!(
      operation = "start_video_recording",
      error = %e,
      "Failed to show recording overlay"
    );
    return RecordingHudRuntime {
      controls: Some(controls),
    };
  }

  if mode == RecordingMode::Area {
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
  }

  supervise(app.clone(), controls.clone());

  RecordingHudRuntime {
    controls: Some(controls),
  }
}

pub(super) fn start_recording_hud_supervisor(
  app: AppHandle,
  controls: Arc<crate::state::RecordingControls>,
) {
  crate::app::runtime::spawn(supervise_recording_hud(
    app,
    controls,
    |elapsed, paused| async move {
      crate::overlay::OverlayWindow::update_recording_hud(elapsed, paused).await
    },
  ));
}

pub(super) async fn supervise_recording_hud<
  Fut: std::future::Future<Output = anyhow::Result<()>>,
>(
  app: AppHandle,
  controls: Arc<crate::state::RecordingControls>,
  mut update: impl FnMut(u64, bool) -> Fut,
) {
  let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
  let mut control_rx = controls.subscribe();
  let mut microphone_rx = controls.subscribe_microphone();
  let mut has_seen_active_recording = false;
  let startup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);

  loop {
    tokio::select! {
      _ = interval.tick() => {},
      changed = microphone_rx.changed() => {
        if changed.is_err() {
          tracing::warn!("Microphone control receiver disconnected");
          break;
        }
        let requested = *microphone_rx.borrow_and_update();
        if let Some(audible) = requested {
          // An unreachable recorder keeps the controls as they were.
          let state = crate::commands::video::control::set_microphone(app.clone(), audible)
            .await
            .unwrap_or_else(|| controls.microphone());
          controls.finish_microphone(state);
        }
      },
      changed = control_rx.changed() => {
        if changed.is_err() {
          tracing::warn!("Recording control receiver disconnected");
          break;
        }

        let event = *control_rx.borrow_and_update();
        match event {
          Some(crate::state::RecordingControlEvent::StopRequested) => {
            // The stop flag stays set while startup owns the recorder; stop after it acknowledges.
          },
          Some(crate::state::RecordingControlEvent::PauseRequested(pause_requested)) => {
            let pause_result = if pause_requested {
              crate::commands::video::control::pause_video_recording(app.clone()).await
            } else {
              crate::commands::video::control::resume_video_recording(app.clone()).await
            };

            if let Err(e) = pause_result {
              tracing::error!(
                operation = "hud_update",
                error = %e,
                "Failed to update pause state from overlay button"
              );
              break;
            }
          },
          None => {},
        }
      }
    }

    if controls
      .pause_failed
      .load(std::sync::atomic::Ordering::SeqCst)
    {
      crate::commands::video::stop::abort_failed_recording(&app).await;
      break;
    }
    let (status, elapsed_ms, is_paused) = {
      let recorder_state = app.state::<VideoRecorderState>();
      let mut recorder = match recorder_state.inner().0.try_lock() {
        Ok(recorder) => recorder,
        Err(std::sync::TryLockError::WouldBlock) => {
          if !has_seen_active_recording && std::time::Instant::now() >= startup_deadline {
            tracing::error!("Recording startup acknowledgment timed out");
            break;
          }
          continue;
        },
        Err(std::sync::TryLockError::Poisoned(error)) => {
          tracing::error!(%error, "Recorder state poisoned, stopping HUD updates");
          break;
        },
      };
      let status = recorder.status();
      let elapsed_ms = recorder.get_elapsed_time().as_millis() as u64;
      let is_paused = status == crate::video::controller::RecordingStatus::Paused;
      if !controls.microphone_pending() {
        controls.sync_microphone(recorder.microphone());
      }
      (status, elapsed_ms, is_paused)
    };

    if status == crate::video::controller::RecordingStatus::Error {
      crate::commands::video::stop::abort_failed_recording(&app).await;
      break;
    }
    if controls.stop_requested()
      && matches!(
        status,
        crate::video::controller::RecordingStatus::Recording
          | crate::video::controller::RecordingStatus::Paused
          | crate::video::controller::RecordingStatus::Error
      )
    {
      if controls.claim_stop()
        && let Err(error) = crate::commands::video::stop::stop_video_recording(app.clone()).await
      {
        tracing::error!(operation = "hud_update", %error, "Failed to stop recording from HUD supervisor");
      }
      break;
    }

    if matches!(
      status,
      crate::video::controller::RecordingStatus::Recording
        | crate::video::controller::RecordingStatus::Paused
    ) {
      controls.sync_recording(elapsed_ms, is_paused);
      has_seen_active_recording = true;
    } else if has_seen_active_recording {
      break;
    } else {
      if controls.stop_requested() || std::time::Instant::now() >= startup_deadline {
        break;
      }
      continue;
    }

    if let Err(e) = update(elapsed_ms, is_paused).await {
      tracing::debug!(
        operation = "hud_update",
        error = %e,
        "Failed to update HUD (may have been closed)"
      );
      break;
    }
  }
  controls.sync_stopped();
}

pub(super) async fn cleanup_failed_video_start(app: &AppHandle, hud: &RecordingHudRuntime) {
  if let Some(controls) = &hud.controls {
    controls.sync_stopped();
  }

  if let Some(hud_state) = app.try_state::<crate::commands::RecordingHudState>() {
    hud_state.clear();
  }

  crate::commands::clear_recording_session(app);

  if let Err(error) = crate::overlay::OverlayWindow::hide_recording_hud().await {
    tracing::debug!(
      operation = "cleanup_failed_video_start",
      error = %error,
      "Failed to hide HUD during failed recording startup cleanup"
    );
  }
}

pub(super) fn detect_source_app_at_region(
  x: i32,
  y: i32,
  width: u32,
  height: u32,
) -> Option<String> {
  let center_x = x + width as i32 / 2;
  let center_y = y + height as i32 / 2;
  crate::overlay::core::window_detection::detect_window_at_point(center_x, center_y, None)
    .and_then(|w| crate::overlay::core::window_detection::get_process_name_for_hwnd(w.hwnd))
}

fn build_area_selection_rect(x: i32, y: i32, width: u32, height: u32) -> (f64, f64, f64, f64) {
  (
    x as f64,
    y as f64,
    (x + width as i32) as f64,
    (y + height as i32) as f64,
  )
}

fn next_available_path(path: &Path) -> PathBuf {
  crate::capture_artifact::next_available_path_with_limit(path, "recording", Some(9999))
}

pub(super) fn recording_temp_path(path: &Path) -> PathBuf {
  let temp_dir = recording_temp_dir(path);
  let file_name = path
    .file_name()
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("recording.mp4"));
  let temp_file_path = temp_dir.join(file_name);
  crate::capture_artifact::temp_marker_path(&temp_file_path, RECORDING_TEMP_MARKER, "recording")
}

#[cfg(test)]
fn is_orphaned_recording_temp_file(path: &Path) -> bool {
  path
    .file_stem()
    .and_then(|stem| stem.to_str())
    .map(|stem| stem.contains(RECORDING_TEMP_MARKER))
    .unwrap_or(false)
}

pub(super) fn recording_temp_dir(path: &Path) -> PathBuf {
  crate::capture_artifact::recording_session_temp_dir(path, "recording", "video")
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The public plan builders resolve the configured save folders, create them and name the
  /// output there; a file in place of the folder fails before anything records.
  #[test]
  fn recording_and_gif_plans_use_the_configured_folders_and_report_unusable_ones() {
    let folder = tempfile::tempdir().expect("folder");
    let videos = folder.path().join("Clips");
    let gifs = folder.path().join("Shots");
    let app = AppHandle::isolated(
      Config {
        language: "en".into(),
        custom_videos_folder: Some(videos.to_string_lossy().into_owned()),
        custom_screenshots_folder: Some(gifs.to_string_lossy().into_owned()),
        video_fps: 60,
        gif_fps: 15,
        ..Default::default()
      },
      PathBuf::new(),
    );
    let video = prepare_video_capture_plan(
      &app,
      0,
      0,
      640,
      480,
      0,
      Some("Code".into()),
      RecordingMode::Area,
    )
    .expect("video plan");
    assert!(videos.is_dir(), "the videos folder is created");
    assert!(video.output_path.starts_with(&videos));
    assert_eq!(
      video.output_path.extension().and_then(|e| e.to_str()),
      Some("mp4")
    );
    assert_eq!(video.video_config.fps, 60, "0 fps uses the configured rate");
    assert!(matches!(video.active_kind, ActiveRecordingKind::Video));
    assert!(video.gif_options.is_none());

    let gif = prepare_gif_capture_plan(&app, 10, 20, 320, 240, 0, None).expect("GIF plan");
    assert!(gifs.is_dir());
    assert!(gif.output_path.starts_with(&gifs));
    assert!(matches!(gif.active_kind, ActiveRecordingKind::Gif));
    assert!(gif.gif_options.is_some());

    let blocked = folder.path().join("blocked");
    std::fs::write(&blocked, b"file").expect("blocker");
    let unusable = AppHandle::isolated(
      Config {
        language: "en".into(),
        custom_videos_folder: Some(blocked.join("Clips").to_string_lossy().into_owned()),
        custom_screenshots_folder: Some(blocked.join("Shots").to_string_lossy().into_owned()),
        ..Default::default()
      },
      PathBuf::new(),
    );
    assert!(
      prepare_video_capture_plan(&unusable, 0, 0, 64, 64, 30, None, RecordingMode::Fullscreen)
        .is_err()
    );
    assert!(prepare_gif_capture_plan(&unusable, 0, 0, 64, 64, 15, None).is_err());
  }

  #[tokio::test]
  async fn hud_bootstrap_preserves_controls_on_failure_and_starts_supervision_only_after_presentation()
   {
    use std::cell::Cell;
    for mode in [
      RecordingMode::Fullscreen,
      RecordingMode::Area,
      RecordingMode::ActiveWindow,
    ] {
      for presentation in [
        RecordingPresentationMode::Silent,
        RecordingPresentationMode::FullUi,
      ] {
        for fail in [false, true] {
          let app = AppHandle::isolated(
            Config {
              language: "fr".into(),
              ..Default::default()
            },
            PathBuf::new(),
          );
          let shown = Cell::new(0);
          let supervised = Cell::new(0);
          let hud = bootstrap_recording_hud_with(
            &app,
            mode,
            presentation,
            -100,
            -200,
            320,
            240,
            |rect, width, height, controls, language| {
              shown.set(shown.get() + 1);
              assert_eq!(language, "fr");
              assert!(!controls.ready.load(std::sync::atomic::Ordering::SeqCst));
              if mode == RecordingMode::Fullscreen {
                assert!(rect.is_none() && width.is_none() && height.is_none());
              } else {
                assert_eq!(rect, Some((-100.0, -200.0, 220.0, 40.0)));
                assert!(width.unwrap() > 0 && height.unwrap() > 0);
              }
              std::future::ready(if fail {
                Err(anyhow::anyhow!("injected presentation failure"))
              } else {
                Ok(())
              })
            },
            |_, controls| {
              assert!(!controls.stop_requested());
              supervised.set(supervised.get() + 1);
            },
          )
          .await;
          let visible = !presentation.is_silent();
          assert_eq!(shown.get(), i32::from(visible));
          assert_eq!(supervised.get(), i32::from(visible && !fail));
          assert_eq!(hud.controls.is_some(), visible);
          cleanup_failed_video_start(&app, &hud).await;
          if let Some(controls) = hud.controls {
            assert!(controls.stop_requested());
          }
          assert!(
            app
              .try_state::<crate::state::RecordingHudState>()
              .unwrap()
              .controls
              .lock()
              .unwrap()
              .is_none()
          );
        }
      }
    }
  }

  #[test]
  fn prepared_gif_and_video_keep_independent_cursor_fps_audio_and_finalize_policies() {
    let dir = tempfile::tempdir().expect("isolated output");
    for (video_cursor, gif_cursor) in [(false, true), (true, false)] {
      let config = Config {
        show_cursor: video_cursor,
        gif_show_cursor: gif_cursor,
        // Click highlights follow the opposite pattern, so each flag is proven independent.
        show_clicks: gif_cursor,
        gif_show_clicks: video_cursor,
        video_fps: 60,
        gif_fps: 15,
        record_audio: true,
        filename_template: Some("%APP%_%TYPE%".into()),
        ..Config::default()
      };
      let app = AppHandle::isolated(config, dir.path().to_path_buf());
      // An off-desktop rectangle avoids initializing any monitor capture backend.
      let gif = prepare_gif_capture_plan_in_directory(
        &Config::current(&app),
        -100001,
        -100001,
        321,
        241,
        0,
        Some("fixture".into()),
        dir.path().join("gif"),
      )
      .expect("GIF plan");
      assert_eq!(gif.capture_config.show_cursor, gif_cursor);
      assert_eq!(gif.capture_config.show_clicks, video_cursor);
      assert_eq!(gif.capture_config.fps, 15);
      assert_eq!(gif.video_config.fps, 15);
      assert!(!gif.video_config.audio.enabled);
      assert_eq!(gif.capture_config.forced_backend, None);
      assert_eq!(
        (
          gif.capture_config.x,
          gif.capture_config.y,
          gif.capture_config.width,
          gif.capture_config.height
        ),
        (-100002, -100002, 320, 240)
      );
      assert_eq!(
        gif.output_path.extension().and_then(|s| s.to_str()),
        Some("gif")
      );
      assert_eq!(
        gif.temp_output_path.extension().and_then(|s| s.to_str()),
        Some("mp4")
      );
      assert_eq!(gif.gif_options, Some(crate::video::native::gif_options()));
      assert_eq!(gif.active_kind, ActiveRecordingKind::Gif);
      let output = dir.path().join("explicit.mp4");
      let video = prepare_video_capture_plan_in_directory(
        &Config::current(&app),
        -100001,
        -100001,
        321,
        241,
        0,
        None,
        RecordingMode::Area,
        VideoStartOverrides {
          output_path: Some(output.clone()),
          record_audio: Some(false),
          forced_backend: Some(CaptureBackendKind::Wgc),
        },
        dir.path().join("video"),
      )
      .expect("video plan");
      assert_eq!(video.capture_config.show_cursor, video_cursor);
      assert_eq!(video.capture_config.show_clicks, gif_cursor);
      assert_eq!(video.capture_config.fps, 60);
      assert_eq!(video.video_config.fps, 60);
      assert!(!video.video_config.audio.enabled);
      assert_eq!(video.output_path, output);
      assert!(
        video
          .temp_output_path
          .starts_with(crate::capture_artifact::recording_temp_root())
      );
      assert_eq!(video.active_kind, ActiveRecordingKind::Video);
      assert!(video.gif_options.is_none());
      assert_eq!(
        video.capture_config.forced_backend,
        Some(CaptureBackendKind::Wgc)
      );
    }
  }

  #[test]
  fn gif_plans_leave_backend_choice_to_the_engine() {
    let dir = tempfile::tempdir().expect("isolated output");
    // An off-desktop rectangle avoids initializing any monitor capture backend.
    let gif = prepare_gif_capture_plan_in_directory(
      &Config::default(),
      -100001,
      -100001,
      321,
      241,
      15,
      None,
      dir.path().join("gif"),
    )
    .expect("GIF plan");
    assert_eq!(gif.capture_config.forced_backend, None);
    let capture = &gif.capture_config;
    let display = crate::monitors::MonitorInfo {
      id: "fixture".into(),
      name: "fixture".into(),
      x: capture.x,
      y: capture.y,
      width: 1920,
      height: 1080,
      is_primary: true,
      dpi_scale: 1.0,
      logical_x: capture.x,
      logical_y: capture.y,
      logical_width: 1920,
      logical_height: 1080,
    };
    // A forced GDI backend made the native adapter refuse every GIF before Prepare.
    let spec = crate::video::native::session_spec(
      &gif.video_config,
      capture,
      &[display],
      &gif.temp_output_path,
    )
    .expect("the native engine accepts the real GIF plan");
    assert_eq!(spec.fps, 15);
    assert_eq!(spec.capture, flinch_video_engine::types::CaptureKind::Auto);
    assert!(!spec.audio);
  }

  #[test]
  fn prepared_output_avoids_existing_files_and_reports_directory_failures() {
    let dir = tempfile::tempdir().expect("isolated output");
    let config = Config {
      filename_template: Some("fixed".into()),
      ..Config::default()
    };
    let app = AppHandle::isolated(config, dir.path().to_path_buf());
    std::fs::write(dir.path().join("fixed.mp4"), b"preserve").expect("existing capture");
    let plan = prepare_video_capture_plan_in_directory(
      &Config::current(&app),
      -100000,
      -100000,
      32,
      32,
      30,
      None,
      RecordingMode::Fullscreen,
      VideoStartOverrides::default(),
      dir.path().to_path_buf(),
    )
    .expect("collision plan");
    assert_ne!(plan.output_path, dir.path().join("fixed.mp4"));
    assert_eq!(
      std::fs::read(dir.path().join("fixed.mp4")).unwrap(),
      b"preserve"
    );
    assert_eq!(
      plan.video_config.audio.enabled,
      Config::current(&app).record_audio
    );
    std::fs::write(dir.path().join("blocked"), b"not a directory").expect("blocker");
    let blocked = dir.path().join("blocked").join("child");
    let gif = prepare_gif_capture_plan_in_directory(
      &Config::current(&app),
      0,
      0,
      32,
      32,
      30,
      None,
      blocked.clone(),
    )
    .unwrap_err();
    let video = prepare_video_capture_plan_in_directory(
      &Config::current(&app),
      0,
      0,
      32,
      32,
      30,
      None,
      RecordingMode::Area,
      VideoStartOverrides::default(),
      blocked,
    )
    .unwrap_err();
    assert!(gif.contains("Failed to create GIF output directory"));
    assert!(video.contains("Failed to create videos directory"));
    assert_eq!(
      std::fs::read(dir.path().join("blocked")).unwrap(),
      b"not a directory"
    );
  }

  #[test]
  fn validate_recording_request_rejects_zero_dimensions() {
    let error = validate_recording_request(0, 1080, 30).unwrap_err();
    assert!(error.contains("width and height must be greater than 0"));
  }

  #[test]
  fn validate_recording_request_rejects_excessive_dimensions() {
    let error = validate_recording_request(16385, 1080, 30).unwrap_err();
    assert!(error.contains("16384x16384"));
  }

  #[test]
  fn validate_recording_request_rejects_invalid_fps() {
    let error = validate_recording_request(1920, 1080, 61).unwrap_err();
    assert!(error.contains("Invalid fps"));
  }

  #[test]
  fn validate_recording_request_allows_config_fps_sentinel() {
    assert!(validate_recording_request(1920, 1080, 0).is_ok());
  }

  #[test]
  fn should_show_recording_hud_matches_supported_modes() {
    assert!(should_show_recording_hud(
      RecordingMode::Fullscreen,
      RecordingPresentationMode::FullUi
    ));
    assert!(should_show_recording_hud(
      RecordingMode::Area,
      RecordingPresentationMode::FullUi
    ));
    assert!(should_show_recording_hud(
      RecordingMode::ActiveWindow,
      RecordingPresentationMode::FullUi
    ));
    assert!(!should_show_recording_hud(
      RecordingMode::Area,
      RecordingPresentationMode::Silent
    ));
  }

  #[test]
  fn build_start_recording_response_includes_source_app_when_present() {
    let response =
      build_start_recording_response(Path::new("C:\\temp\\recording.mp4"), Some("obs64"));
    assert!(matches!(
      response.status,
      crate::commands::video::RecordingLifecycleStatus::Recording
    ));
    assert_eq!(response.path.as_deref(), Some("C:\\temp\\recording.mp4"));
    assert_eq!(response.source_app.as_deref(), Some("obs64"));
  }

  #[test]
  fn build_start_recording_response_omits_source_app_when_absent() {
    let response = build_start_recording_response(Path::new("recording.mp4"), None);
    assert_eq!(response.source_app, None);
  }

  #[test]
  fn recording_temp_path_marks_the_filename_without_changing_container_extension() {
    let path = PathBuf::from(r"C:\captures\recording.mp4");
    let temp_path = recording_temp_path(&path);

    assert!(temp_path.starts_with(crate::capture_artifact::recording_temp_root()));
    assert_eq!(
      temp_path.file_name().and_then(|name| name.to_str()),
      Some("recording.flinch-part.mp4")
    );
  }

  #[test]
  fn recording_temp_dir_uses_os_temp_recording_root() {
    let path = PathBuf::from(r"C:\captures\recording.mp4");
    assert!(recording_temp_dir(&path).starts_with(crate::capture_artifact::recording_temp_root()));
  }

  #[test]
  fn orphaned_recording_temp_detection_matches_only_flinch_temp_files() {
    assert!(is_orphaned_recording_temp_file(Path::new(
      "recording.flinch-part.mp4"
    )));
    assert!(is_orphaned_recording_temp_file(Path::new(
      "recording.flinch-part.seg0001.mp4"
    )));
    assert!(is_orphaned_recording_temp_file(Path::new(
      "recording.flinch-part.concat.txt"
    )));
    assert!(!is_orphaned_recording_temp_file(Path::new("recording.mp4")));
    assert!(!is_orphaned_recording_temp_file(Path::new(
      "recording.part.mp4"
    )));
  }
}
