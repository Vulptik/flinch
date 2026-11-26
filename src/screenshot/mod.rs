// src/screenshot/mod.rs

use crate::app::context::AppHandle;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[cfg(test)]
mod pipeline_tests;

pub mod encoding;
pub mod platform;
pub mod types;

use platform::capture_active_window_rgba;

// Re-export shared types
pub use types::{ScreenshotData, SelectionResult};
pub(crate) mod annotations;

/// Result of a screenshot capture, containing the in-memory RGBA data
/// and metadata needed for saving. The caller decides whether/when to save.
pub struct CaptureResult {
  /// The file path to save to (file may not exist yet if save hasn't been called)
  pub path: PathBuf,
  pub width: u32,
  pub height: u32,
  pub rgba: Vec<u8>,
  pub format: String,
  pub quality: u8,
  /// Exe stem of the source process (e.g. "Code", "chrome"). None if unknown.
  pub source_app: Option<String>,
  /// F3 pinned the selection instead of saving it: the image's top-left in virtual-screen
  /// pixels, where the pin opens.
  pub pin: Option<(i32, i32)>,
}

/// Returns a non-colliding path by appending a Windows-style numeric suffix.
///
/// Examples:
/// - `image.png` -> `image (1).png`
/// - `capture` -> `capture (1)`
pub fn next_available_path(path: &Path) -> PathBuf {
  crate::capture_artifact::next_available_path(path, "capture")
}

/// Warms up the Desktop Duplication (DXGI) pipeline so the first
/// user-triggered capture is fast. Idempotent.
pub fn warm_up_desktop_duplication() {
  platform::warm_up_desktop_duplication();
}

/// Warms up the WGC pipeline in the background. Idempotent.
#[allow(dead_code)]
pub fn warm_up_wgc() {
  platform::warm_up_wgc();
}

// Re-export capture backend symbols for use by video and GIF modules.
#[allow(unused_imports)]
pub use platform::get_active_recording_backend;
#[allow(unused_imports)]
pub use platform::{
  CaptureBackendKind, invalidate_stale_captures, release_capture_idle_memory,
  release_dxgi_idle_memory,
};

#[derive(Error, Debug)]
pub enum ScreenshotError {
  #[allow(dead_code)]
  #[error("Screen capture failed: {0}")]
  CaptureFailed(String),
  #[error("Screenshot cancelled by user")]
  Cancelled,
}

#[derive(Clone)]
pub struct Screenshot {
  save_dir: PathBuf,
  filename_template: String,
}

impl Screenshot {
  pub fn new(save_dir: PathBuf, filename_template: String) -> Self {
    Self {
      save_dir,
      filename_template,
    }
  }

  /// Capture fullscreen → show overlay → crop → save
  ///
  /// IMPORTANT:
  /// - Screenshot is captured ONCE
  /// - Overlay displays the exact same image
  /// - Cropping uses physical pixels
  pub async fn capture(&self, app: &AppHandle, format: &str, quality: u8) -> Result<CaptureResult> {
    let options = crate::overlay::selection_host::SelectionOptions::for_app(app);
    self
      .capture_with(
        app,
        format,
        quality,
        platform::capture_virtual_desktop_rgba,
        remembering(app, move |screenshot, language| {
          crate::overlay::screenshot::show_annotated_selection_overlay(
            screenshot, language, options,
          )
        }),
      )
      .await
  }

  /// Capture Last Region: the same frozen capture, cropped to the last picked area without
  /// showing the overlay. With no area, or one that no longer fits the monitors, the normal area
  /// overlay opens and its pick becomes the last region.
  pub async fn capture_last_region(
    &self,
    app: &AppHandle,
    format: &str,
    quality: u8,
  ) -> Result<CaptureResult> {
    let options = crate::overlay::selection_host::SelectionOptions::for_app(app);
    let region = crate::last_region::recall(app);
    self
      .capture_last_region_with(
        app,
        format,
        quality,
        region,
        platform::capture_virtual_desktop_rgba,
        |center_x, center_y| {
          crate::overlay::core::window_detection::detect_window_at_point(center_x, center_y, None)
            .and_then(|window| window.source_app)
        },
        move |screenshot, language| {
          crate::overlay::screenshot::show_annotated_selection_overlay(
            screenshot, language, options,
          )
        },
      )
      .await
  }

  #[allow(clippy::too_many_arguments)]
  async fn capture_last_region_with<C, A, S, F>(
    &self,
    app: &AppHandle,
    format: &str,
    quality: u8,
    region: Option<crate::last_region::Region>,
    capture: C,
    source_app: A,
    pick: S,
  ) -> Result<CaptureResult>
  where
    C: FnOnce() -> Result<(i32, i32, u32, u32, Vec<u8>)>,
    A: FnOnce(i32, i32) -> Option<String> + Send + 'static,
    S: FnOnce(ScreenshotData, Option<String>) -> F,
    F: std::future::Future<Output = Result<Option<annotations::AnnotatedSelection>>>
      + Send
      + 'static,
  {
    let fallback = remembering(app, pick);
    self
      .capture_with(
        app,
        format,
        quality,
        capture,
        move |screenshot, language| {
          let replay = region.and_then(|region| {
            region
              .within(
                screenshot.vd_offset_x,
                screenshot.vd_offset_y,
                screenshot.width,
                screenshot.height,
              )
              .map(|local| (region, local))
          });
          let Some((region, local)) = replay else {
            tracing::debug!(
              operation = "capture_last_region",
              has_region = region.is_some(),
              "No usable last region; opening the area overlay"
            );
            return fallback(screenshot, language);
          };
          crate::commands::log_shortcut_to_region();
          let (center_x, center_y) = region.center();
          let picked = annotations::AnnotatedSelection {
            selection: crate::overlay::selection::Selection {
              x: local.x as u32,
              y: local.y as u32,
              width: local.width,
              height: local.height,
              source_app: source_app(center_x, center_y),
            },
            annotations: Vec::new(),
            pin: false,
          };
          Box::pin(async move { Ok(Some(picked)) })
        },
      )
      .await
  }

  /// Same single frozen capture as `capture`, opening the editor once a selection is made.
  pub async fn capture_and_edit(
    &self,
    app: &AppHandle,
    format: &str,
    quality: u8,
  ) -> Result<CaptureResult> {
    let options = crate::overlay::selection_host::SelectionOptions::for_app(app);
    self
      .capture_with(
        app,
        format,
        quality,
        platform::capture_virtual_desktop_rgba,
        remembering(app, move |screenshot, language| {
          crate::overlay::screenshot::show_capture_edit_overlay(screenshot, language, options)
        }),
      )
      .await
  }

  /// Same frozen capture and selection overlay, without the drawing tools.
  pub async fn capture_for_text(&self, app: &AppHandle) -> Result<CaptureResult> {
    let options = crate::overlay::selection_host::SelectionOptions::for_app(app);
    self
      .capture_with(
        app,
        "png",
        100,
        platform::capture_virtual_desktop_rgba,
        remembering(app, move |screenshot, _language| async move {
          let selection = crate::overlay::screenshot::show_selection_overlay(
            screenshot.width,
            screenshot.height,
            Some(screenshot.rgba),
            screenshot.vd_offset_x,
            screenshot.vd_offset_y,
            options,
          )
          .await?;
          Ok(selection.map(|selection| annotations::AnnotatedSelection {
            selection,
            annotations: Vec::new(),
            pin: false,
          }))
        }),
      )
      .await
  }

  /// The Color Picker: the same frozen capture, returning the single pixel the user picks.
  pub async fn capture_for_color(&self, app: &AppHandle) -> Result<CaptureResult> {
    let options = crate::overlay::selection_host::SelectionOptions {
      pick: true,
      ..crate::overlay::selection_host::SelectionOptions::for_app(app)
    };
    self
      .capture_with(
        app,
        "png",
        100,
        platform::capture_virtual_desktop_rgba,
        move |screenshot, _language| async move {
          let selection = crate::overlay::screenshot::show_selection_overlay(
            screenshot.width,
            screenshot.height,
            Some(screenshot.rgba),
            screenshot.vd_offset_x,
            screenshot.vd_offset_y,
            options,
          )
          .await?;
          Ok(selection.map(|selection| annotations::AnnotatedSelection {
            selection,
            annotations: Vec::new(),
            pin: false,
          }))
        },
      )
      .await
  }

  async fn capture_with<C, S, F>(
    &self,
    app: &AppHandle,
    format: &str,
    quality: u8,
    capture: C,
    select: S,
  ) -> Result<CaptureResult>
  where
    C: FnOnce() -> Result<(i32, i32, u32, u32, Vec<u8>)>,
    S: FnOnce(ScreenshotData, Option<String>) -> F,
    F: std::future::Future<Output = Result<Option<annotations::AnnotatedSelection>>>,
  {
    let _start_time = std::time::Instant::now();
    tracing::debug!("Screenshot capture started");

    let _timestamp = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
      Ok(duration) => duration.as_secs(),
      Err(e) => {
        tracing::warn!(
          operation = "screenshot_timestamp",
          error = %e,
          "System clock before UNIX epoch, using monotonic fallback"
        );

        use std::sync::OnceLock;
        use std::time::Instant;
        static BASELINE: OnceLock<u64> = OnceLock::new();
        static START_TIME: OnceLock<Instant> = OnceLock::new();

        let baseline = BASELINE.get_or_init(|| 1577836800u64); // Jan 1, 2020
        let start = START_TIME.get_or_init(Instant::now);

        baseline + start.elapsed().as_secs()
      },
    };

    let ext = match format {
      "jpg" | "jpeg" => "jpg",
      "webp" => "webp",
      _ => "png",
    };

    // 1. Capture virtual desktop (all monitors, RGBA, physical pixels)
    let capture_start = std::time::Instant::now();
    let (vd_offset_x, vd_offset_y, width, height, rgba) =
      { capture().map_err(|e| e.context("Failed to capture virtual desktop"))? };
    let capture_duration = capture_start.elapsed();
    encoding::error::validate_rgba(width, height, rgba.len())?;
    crate::commands::log_speed_snapshot("screenshot.capture.virtual_desktop", capture_duration);
    tracing::debug!(
      "Virtual desktop captured in {:.2}ms ({}x{} at ({},{}), {} bytes)",
      capture_duration.as_secs_f64() * 1000.0,
      width,
      height,
      vd_offset_x,
      vd_offset_y,
      rgba.len()
    );

    // 2. Store screenshot data in memory
    let memory_store_start = std::time::Instant::now();
    let rgba = std::sync::Arc::new(rgba);
    let screenshot_data = ScreenshotData {
      width,
      height,
      rgba: rgba.clone(),
      format: format.to_string(),
      quality,
      vd_offset_x,
      vd_offset_y,
    };
    let memory_store_duration = memory_store_start.elapsed();
    crate::commands::log_speed_snapshot(
      "screenshot.capture.overlay_input_prepare",
      memory_store_duration,
    );
    tracing::debug!(
      "Screenshot data stored in memory in {:.2}ms",
      memory_store_duration.as_secs_f64() * 1000.0
    );

    // 3. Show overlay immediately (no disk I/O)
    // Store start time for overlay_ready command to use
    // crate::overlay::set_capture_start_time(start_time);

    // let overlay_start = std::time::Instant::now();
    // eprintln!("[TIMER] Opening overlay window...");
    let overlay_start = std::time::Instant::now();
    let selection = match select(screenshot_data, Some(crate::i18n::get_lang(app))).await {
      Ok(selection) => selection,
      Err(error) => {
        if let Ok(buf) = std::sync::Arc::try_unwrap(rgba) {
          crate::screenshot::platform::return_screenshot_buffer(buf);
        }
        crate::screenshot::platform::clear_cached_screenshot_buffer();
        return Err(error.context("Overlay failed"));
      },
    };
    crate::commands::log_speed_snapshot(
      "screenshot.capture.overlay_roundtrip",
      overlay_start.elapsed(),
    );

    let selection = match selection {
      Some(sel) => sel,
      None => {
        if let Ok(buf) = std::sync::Arc::try_unwrap(rgba) {
          crate::screenshot::platform::return_screenshot_buffer(buf);
        }
        crate::screenshot::platform::clear_cached_screenshot_buffer();
        return Err(ScreenshotError::Cancelled.into());
      },
    };

    let annotations = selection.annotations;
    let pin = selection.pin.then(|| {
      (
        vd_offset_x.saturating_add(selection.selection.x as i32),
        vd_offset_y.saturating_add(selection.selection.y as i32),
      )
    });
    let selection = selection.selection;

    // 4. Crop using in-memory data
    let filename_ctx = crate::filename::FilenameContext {
      capture_type: "screenshot",
      app_name: selection.source_app.as_deref(),
      now: chrono::Local::now(),
    };
    let output_path = self.save_dir.join(format!(
      "{}.{}",
      crate::filename::render_template(&self.filename_template, &filename_ctx),
      ext
    ));

    let crop_start = std::time::Instant::now();
    let cropped_rgba = encoding::crop_rgba_direct(
      rgba.as_ref().as_slice(),
      width,
      selection.x,
      selection.y,
      selection.width,
      selection.height,
    );
    crate::commands::log_speed_snapshot("screenshot.capture.crop_rgba", crop_start.elapsed());
    let cropped_rgba = cropped_rgba.map(|mut crop| {
      blur::apply(
        &mut crop,
        &selection,
        rgba.as_ref(),
        width,
        height,
        &annotations,
      );
      crop
    });
    if let Ok(buf) = std::sync::Arc::try_unwrap(rgba) {
      crate::screenshot::platform::return_screenshot_buffer(buf);
    }
    crate::screenshot::platform::clear_cached_screenshot_buffer();

    let cropped_rgba = cropped_rgba.context("Invalid screenshot selection")?;
    let cropped_rgba = if annotations.is_empty() {
      cropped_rgba
    } else {
      let crop_selection = selection.clone();
      tokio::task::spawn_blocking(move || {
        annotations::composite(cropped_rgba, &crop_selection, &annotations)
      })
      .await
      .context("Screenshot drawing task failed")?
      .context("Failed to apply screenshot drawings")?
    };

    Ok(CaptureResult {
      path: output_path,
      width: selection.width,
      height: selection.height,
      rgba: cropped_rgba,
      format: format.to_string(),
      quality,
      source_app: selection.source_app.clone(),
      pin,
    })
  }

  /// Capture fullscreen RGBA. Does NOT save to disk — caller decides via save_to_disk().
  pub async fn capture_fullscreen(
    &self,
    _app: &AppHandle,
    format: &str,
    quality: u8,
  ) -> Result<CaptureResult> {
    self
      .capture_fullscreen_with(format, quality, platform::capture_virtual_desktop_rgba)
      .await
  }

  async fn capture_fullscreen_with<C>(
    &self,
    format: &str,
    quality: u8,
    capture: C,
  ) -> Result<CaptureResult>
  where
    C: FnOnce() -> Result<(i32, i32, u32, u32, Vec<u8>)>,
  {
    let _timestamp = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
      Ok(duration) => duration.as_secs(),
      Err(e) => {
        tracing::warn!(
          operation = "screenshot_timestamp",
          error = %e,
          "System clock before UNIX epoch, using monotonic fallback"
        );

        use std::sync::OnceLock;
        use std::time::Instant;
        static BASELINE: OnceLock<u64> = OnceLock::new();
        static START_TIME: OnceLock<Instant> = OnceLock::new();

        let baseline = BASELINE.get_or_init(|| 1577836800u64); // Jan 1, 2020
        let start = START_TIME.get_or_init(Instant::now);

        baseline + start.elapsed().as_secs()
      },
    };

    let ext = match format {
      "jpg" | "jpeg" => "jpg",
      "webp" => "webp",
      _ => "png",
    };

    let filename_ctx = crate::filename::FilenameContext {
      capture_type: "screenshot",
      app_name: None,
      now: chrono::Local::now(),
    };
    let output_path = self.save_dir.join(format!(
      "{}.{}",
      crate::filename::render_template(&self.filename_template, &filename_ctx),
      ext
    ));

    // Capture entire virtual desktop (all monitors composited into one image)
    let capture_start = std::time::Instant::now();
    let (_vd_x, _vd_y, width, height, rgba) =
      capture().map_err(|e| e.context("Failed to capture screen"))?;
    encoding::error::validate_rgba(width, height, rgba.len())?;
    crate::commands::log_speed_snapshot(
      "screenshot.capture_fullscreen.backend_capture",
      capture_start.elapsed(),
    );

    Ok(CaptureResult {
      path: output_path,
      width,
      height,
      rgba,
      format: format.to_string(),
      quality,
      source_app: None,
      pin: None,
    })
  }

  /// Capture active window RGBA. Does NOT save to disk — caller decides via save_to_disk().
  /// Falls back to fullscreen capture if no valid active window is found.
  pub async fn capture_active_window(
    &self,
    _app: &AppHandle,
    format: &str,
    quality: u8,
  ) -> Result<CaptureResult> {
    let source_app = crate::video::capture::windows::get_foreground_window_handle()
      .and_then(crate::overlay::core::window_detection::get_process_name_for_hwnd);
    self
      .capture_active_window_with(format, quality, source_app, capture_active_window_rgba)
      .await
  }

  async fn capture_active_window_with<C>(
    &self,
    format: &str,
    quality: u8,
    source_app: Option<String>,
    capture: C,
  ) -> Result<CaptureResult>
  where
    C: FnOnce() -> Result<(u32, u32, Vec<u8>)>,
  {
    let _timestamp = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
      Ok(duration) => duration.as_secs(),
      Err(e) => {
        tracing::warn!(
          operation = "screenshot_timestamp",
          error = %e,
          "System clock before UNIX epoch, using monotonic fallback"
        );

        use std::sync::OnceLock;
        use std::time::Instant;
        static BASELINE: OnceLock<u64> = OnceLock::new();
        static START_TIME: OnceLock<Instant> = OnceLock::new();

        let baseline = BASELINE.get_or_init(|| 1577836800u64); // Jan 1, 2020
        let start = START_TIME.get_or_init(Instant::now);

        baseline + start.elapsed().as_secs()
      },
    };

    let ext = match format {
      "jpg" | "jpeg" => "jpg",
      "webp" => "webp",
      _ => "png",
    };

    let filename_ctx = crate::filename::FilenameContext {
      capture_type: "screenshot",
      app_name: source_app.as_deref(),
      now: chrono::Local::now(),
    };
    let output_path = self.save_dir.join(format!(
      "{}.{}",
      crate::filename::render_template(&self.filename_template, &filename_ctx),
      ext
    ));

    // Capture active window RGBA (falls back to fullscreen internally if needed)
    let capture_start = std::time::Instant::now();
    let (width, height, rgba) =
      capture().map_err(|e| e.context("Failed to capture active window"))?;
    encoding::error::validate_rgba(width, height, rgba.len())?;
    crate::commands::log_speed_snapshot(
      "screenshot.capture_active_window.backend_capture",
      capture_start.elapsed(),
    );

    Ok(CaptureResult {
      path: output_path,
      width,
      height,
      rgba,
      format: format.to_string(),
      quality,
      source_app,
      pin: None,
    })
  }
}

type PickFuture = std::pin::Pin<
  Box<dyn std::future::Future<Output = Result<Option<annotations::AnnotatedSelection>>> + Send>,
>;

/// Wraps an area picker so its pick becomes the region Capture Last Region replays.
fn remembering<S, F>(
  app: &AppHandle,
  pick: S,
) -> impl FnOnce(ScreenshotData, Option<String>) -> PickFuture
where
  S: FnOnce(ScreenshotData, Option<String>) -> F,
  F: std::future::Future<Output = Result<Option<annotations::AnnotatedSelection>>> + Send + 'static,
{
  let app = app.clone();
  move |screenshot, language| {
    let (origin_x, origin_y) = (screenshot.vd_offset_x, screenshot.vd_offset_y);
    let picked = pick(screenshot, language);
    Box::pin(async move {
      let picked = picked.await?;
      if let Some(chosen) = &picked {
        let selection = &chosen.selection;
        crate::last_region::remember(
          &app,
          crate::last_region::Region {
            x: origin_x.saturating_add(selection.x as i32),
            y: origin_y.saturating_add(selection.y as i32),
            width: selection.width,
            height: selection.height,
          },
        );
      }
      Ok(picked)
    })
  }
}

pub fn save_rgba_image(
  path: &Path,
  width: u32,
  height: u32,
  rgba: &[u8],
  format: &str,
  quality: u8,
) -> Result<()> {
  encoding::save_rgba_image_optimized(path, width, height, rgba, format, quality)
    .map_err(|e| anyhow::anyhow!("Encoding failed: {}", e))
}

/// Save a screenshot and return the file written: a JPG beside `path`, at the next free name,
/// when `large_png_as_jpg` turns a PNG over `encoding::LARGE_PNG_BYTES` into one.
#[allow(clippy::too_many_arguments)]
pub fn save_screenshot(
  path: &Path,
  width: u32,
  height: u32,
  rgba: &[u8],
  format: &str,
  quality: u8,
  large_png_as_jpg: bool,
) -> Result<PathBuf> {
  encoding::save_screenshot(
    path,
    width,
    height,
    rgba,
    format,
    quality,
    large_png_as_jpg,
    |png| next_available_path(&png.with_extension("jpg")),
  )
  .map_err(|e| anyhow::anyhow!("Encoding failed: {}", e))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_screenshot_creation() {
    let save_dir = PathBuf::from("/tmp/screenshots");
    let screenshot = Screenshot::new(save_dir.clone(), "%TYPE%_%DATETIME%".to_string());
    assert_eq!(screenshot.save_dir, save_dir);
  }

  #[test]
  fn test_screenshot_clone() {
    let save_dir = PathBuf::from("/tmp/screenshots");
    let screenshot = Screenshot::new(save_dir.clone(), "%TYPE%_%DATETIME%".to_string());
    let cloned = screenshot.clone();
    assert_eq!(cloned.save_dir, save_dir);
  }

  #[test]
  fn test_capture_result_fields() {
    let result = CaptureResult {
      path: PathBuf::from("/tmp/test.png"),
      width: 1920,
      height: 1080,
      rgba: vec![0u8; 1920 * 1080 * 4],
      format: "png".to_string(),
      quality: 90,
      source_app: None,
      pin: None,
    };

    assert_eq!(result.width, 1920);
    assert_eq!(result.height, 1080);
    assert_eq!(result.format, "png");
    assert_eq!(result.quality, 90);
    assert_eq!(result.rgba.len(), 1920 * 1080 * 4);
  }

  #[test]
  fn test_format_extension_mapping() {
    let ext = match "jpg" {
      "jpg" | "jpeg" => "jpg",
      "webp" => "webp",
      _ => "png",
    };
    assert_eq!(ext, "jpg");

    let ext = match "jpeg" {
      "jpg" | "jpeg" => "jpg",
      "webp" => "webp",
      _ => "png",
    };
    assert_eq!(ext, "jpg");

    let ext = match "webp" {
      "jpg" | "jpeg" => "jpg",
      "webp" => "webp",
      _ => "png",
    };
    assert_eq!(ext, "webp");

    let ext = match "unknown" {
      "jpg" | "jpeg" => "jpg",
      "webp" => "webp",
      _ => "png",
    };
    assert_eq!(ext, "png");
  }

  #[test]
  fn test_rgba_buffer_size() {
    let width = 1920u32;
    let height = 1080u32;
    let expected_size = (width * height * 4) as usize;

    let buffer = vec![0u8; expected_size];
    assert_eq!(buffer.len(), expected_size);
    assert_eq!(buffer.len(), 1920 * 1080 * 4);
  }

  #[test]
  fn test_quality_range() {
    let quality = 90u8;
    assert!((0..=100).contains(&quality));

    let quality = 0u8;
    assert!((0..=100).contains(&quality));

    let quality = 100u8;
    assert!((0..=100).contains(&quality));
  }

  #[test]
  fn test_path_parent_creation() {
    let path = PathBuf::from("/tmp/screenshots/test.png");
    let parent = path.parent();
    assert!(parent.is_some());
    assert_eq!(parent.unwrap(), Path::new("/tmp/screenshots"));
  }

  #[test]
  fn test_next_available_path_appends_windows_style_suffix() {
    let dir = tempfile::tempdir().expect("temp dir should be created");
    let base = dir.path().join("capture.png");
    std::fs::write(&base, b"first").expect("base file should be written");

    let next = next_available_path(&base);
    assert_eq!(
      next.file_name().and_then(|n| n.to_str()),
      Some("capture (1).png")
    );

    std::fs::write(&next, b"second").expect("second file should be written");
    let third = next_available_path(&base);
    assert_eq!(
      third.file_name().and_then(|n| n.to_str()),
      Some("capture (2).png")
    );
  }
}
pub(crate) mod arrow;
pub(crate) mod blur;
pub(crate) mod path;
pub(crate) mod shape;
mod smooth;
mod text;
mod vector;
