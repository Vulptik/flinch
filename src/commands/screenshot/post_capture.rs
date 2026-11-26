//! Post-capture handling for screenshots
//!
//! ## Purpose
//! Handles what happens after a screenshot is captured:
//! - Save to disk for local captures
//! - Copy to clipboard (RGBA data)
//! - Show notifications

use crate::app::context::AppHandle;
use crate::commands::capture_finalize::{CaptureResponse, build_capture_response};
use crate::commands::copy_screenshot_to_clipboard_with_retry;
use crate::config::Config;
use crate::error::ErrorExt;
use crate::screenshot::CaptureResult;
use std::sync::Arc;

fn build_saved_notification_body(saved_to_prefix: &str, path: &str) -> String {
  format!("{saved_to_prefix}{path}")
}

fn release_screenshot_rgba(rgba: Vec<u8>) {
  crate::screenshot::platform::return_screenshot_buffer(rgba);
  crate::screenshot::platform::clear_cached_screenshot_buffer();
}

fn save_screenshot_to_disk_blocking(
  save_path: std::path::PathBuf,
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  format: String,
  quality: u8,
  large_png_as_jpg: bool,
) -> Result<std::path::PathBuf, String> {
  let save_started = std::time::Instant::now();
  if let Some(parent) = save_path.parent()
    && let Err(e) = std::fs::create_dir_all(parent)
  {
    tracing::error!(
      operation = "save_screenshot",
      error = %e,
      "Failed to create screenshot directory"
    );
    return Err(crate::commands::ipc_context(
      "Failed to create screenshot directory",
      e,
    ));
  }

  let result = crate::screenshot::save_screenshot(
    &save_path,
    width,
    height,
    &rgba,
    &format,
    quality,
    large_png_as_jpg,
  );
  if let Err(ref e) = result {
    tracing::error!(
      operation = "save_screenshot",
      error = %e,
      "Failed to save screenshot to disk"
    );
  }

  crate::commands::log_speed_snapshot(
    "screenshot.post_capture.save_to_disk",
    save_started.elapsed(),
  );

  result.map_err(|e| e.to_rich_string())
}

fn show_screenshot_notification(
  app: &AppHandle,
  title: &str,
  body: &str,
  thumbnail: Option<crate::overlay::notification::ThumbnailImage>,
  path: &std::path::Path,
) {
  crate::notifications::show_rich(
    app,
    title,
    body,
    thumbnail,
    Some(crate::overlay::notification::ClickAction::OpenFolder(
      path.to_path_buf(),
    )),
  );
}

/// Handle the post-capture flow for screenshots.
/// Screenshots are saved to disk and copied to the clipboard when possible.
pub async fn handle_capture_result(
  app: &AppHandle,
  result: CaptureResult,
  config: &Config,
  operation_name: &str,
) -> Result<CaptureResponse, String> {
  let source_app = result.source_app.clone();
  let lang = crate::i18n::get_lang(app);
  let titles = crate::i18n::screenshot_notif_titles(&lang);

  copy_rgba_and_maybe_save(app, result, source_app, config, operation_name, &titles).await
}

/// Save RGBA to disk, then copy the image and saved file to the clipboard, or the file alone when
/// the image is too large.
pub async fn copy_rgba_and_maybe_save(
  app: &AppHandle,
  result: CaptureResult,
  source_app: Option<String>,
  config: &Config,
  operation_name: &str,
  titles: &crate::i18n::CaptureNotifTitles,
) -> Result<CaptureResponse, String> {
  let (width, height) = (result.width, result.height);
  copy_rgba_and_maybe_save_with(
    app,
    result,
    source_app,
    config,
    operation_name,
    titles,
    |pixels, path| copy_screenshot_to_clipboard_with_retry(app, width, height, pixels, path),
  )
  .await
}

#[allow(clippy::too_many_arguments)]
async fn copy_rgba_and_maybe_save_with<C, F>(
  app: &AppHandle,
  mut result: CaptureResult,
  source_app: Option<String>,
  config: &Config,
  operation_name: &str,
  titles: &crate::i18n::CaptureNotifTitles,
  copy: C,
) -> Result<CaptureResponse, String>
where
  C: FnOnce(Arc<Vec<u8>>, std::path::PathBuf) -> F,
  F: std::future::Future<Output = Result<(), String>>,
{
  let started = std::time::Instant::now();
  let lang = crate::i18n::get_lang(app);
  let t = crate::i18n::get(&lang);

  result.path = crate::screenshot::next_available_path(&result.path);
  let planned_path = result.path.clone();
  let save_width = result.width;
  let save_height = result.height;
  let save_format = result.format.clone();
  let save_quality = result.quality;
  let large_png_as_jpg = config.screenshot_large_png_as_jpg;

  // Preserve the final capture pixels before returning the screenshot buffer.
  let thumbnail = Some(crate::overlay::notification::build_thumbnail_image(
    &result.rgba,
    result.width,
    result.height,
  ));

  let rgba = Arc::new(result.rgba);
  let save_rgba = rgba.clone();
  let clipboard_rgba = rgba.clone();

  let save_handle = tokio::task::spawn_blocking(move || {
    save_screenshot_to_disk_blocking(
      planned_path,
      save_width,
      save_height,
      save_rgba,
      save_format,
      save_quality,
      large_png_as_jpg,
    )
  });

  let save_result = match save_handle.await {
    Ok(result) => result,
    Err(error) => Err(crate::commands::ipc_context("Save task panicked", error)),
  };
  crate::commands::log_speed_snapshot("screenshot.post_capture.wait_for_save", started.elapsed());

  // The file actually written: a large PNG may have been saved as a JPG instead.
  let save_path = match save_result {
    Ok(path) => path,
    Err(err_msg) => {
      crate::notifications::show_error(app, titles.save_failed, "notification_error_save");
      return Err(err_msg);
    },
  };
  let path_str = save_path.to_str().unwrap_or("").to_string();
  crate::recent_captures::record(
    app,
    &save_path,
    crate::recent_captures::CaptureKind::Screenshot,
  );

  let clipboard_started = std::time::Instant::now();
  let clipboard_success = copy(clipboard_rgba, save_path.clone()).await.is_ok();
  crate::commands::log_speed_snapshot(
    "screenshot.post_capture.clipboard_copy",
    clipboard_started.elapsed(),
  );

  if let Ok(buf) = Arc::try_unwrap(rgba) {
    release_screenshot_rgba(buf);
  }

  if clipboard_success {
    show_screenshot_notification(
      app,
      titles.copied,
      t.body_image_copied,
      thumbnail.clone(),
      &save_path,
    );
    crate::commands::log_speed_snapshot(
      "screenshot.post_capture.first_notification",
      started.elapsed(),
    );
  } else {
    tracing::warn!(
      operation = operation_name,
      path = %path_str,
      "Failed to copy image to clipboard"
    );
    let body = build_saved_notification_body(t.body_saved_to_prefix, &path_str);
    show_screenshot_notification(app, titles.saved, &body, thumbnail.clone(), &save_path);
  }

  Ok(build_capture_response(Some(&save_path)).with_source_app(source_app.as_deref()))
}

#[cfg(test)]
mod drive_tests;
#[cfg(test)]
mod pipeline_tests;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn saved_notification_body_uses_plain_saved_path() {
    assert_eq!(
      build_saved_notification_body("Saved to: ", "C:\\temp\\shot.png"),
      "Saved to: C:\\temp\\shot.png"
    );
  }

  #[test]
  fn build_capture_json_populates_optional_fields_consistently() {
    let source_app = "Flinch".to_string();

    let saved_json = crate::commands::capture_finalize::build_capture_response(Some(
      std::path::Path::new("C:\\temp\\shot.png"),
    ))
    .with_source_app(Some(&source_app));
    let saved_json = serde_json::to_value(saved_json).expect("capture response should serialize");
    assert_eq!(saved_json["success"], true);
    assert_eq!(saved_json["saved"], true);
    assert_eq!(saved_json["path"], "C:\\temp\\shot.png");
    assert_eq!(saved_json["source_app"], "Flinch");
    assert!(saved_json.get("url").is_none());

    let local_json = serde_json::to_value(
      crate::commands::capture_finalize::build_capture_response(Some(std::path::Path::new(
        "C:\\temp\\shot.png",
      )))
      .with_source_app(Some(&source_app)),
    )
    .expect("capture response should serialize");
    assert_eq!(local_json["success"], true);
    assert_eq!(local_json["saved"], true);
    assert_eq!(local_json["path"], "C:\\temp\\shot.png");
    assert!(local_json.get("url").is_none());
  }
}
