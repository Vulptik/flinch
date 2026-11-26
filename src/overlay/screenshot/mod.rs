// Screenshot overlay module - screenshot-specific selection adapter
//
// ## Module Structure
// - `manager` - Win32 WndProc window and event handling

mod cache;
pub(crate) mod cursors;
pub(crate) mod editor;
mod history;
pub mod manager;
mod pen;
pub(crate) mod performance;

use anyhow::Result;

pub use crate::overlay::selection::Selection;

/// Show the screenshot selection overlay
pub async fn show_selection_overlay(
  width: u32,
  height: u32,
  rgba: Option<std::sync::Arc<Vec<u8>>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  options: crate::overlay::selection_host::SelectionOptions,
) -> Result<Option<Selection>> {
  let rgba_data = rgba.ok_or_else(|| anyhow::anyhow!("No screenshot data provided"))?;

  tokio::task::spawn_blocking(move || {
    manager::show_selection_overlay(width, height, rgba_data, vd_offset_x, vd_offset_y, options)
  })
  .await
  .map_err(|e| anyhow::anyhow!("Task join error: {}", e))?
}

pub(crate) async fn show_annotated_selection_overlay(
  screenshot: crate::screenshot::ScreenshotData,
  editor_language: Option<String>,
  options: crate::overlay::selection_host::SelectionOptions,
) -> Result<Option<crate::screenshot::annotations::AnnotatedSelection>> {
  show_annotated(screenshot, editor_language, options).await
}

/// Same overlay, but a finished selection stays open in the editor.
pub(crate) async fn show_capture_edit_overlay(
  screenshot: crate::screenshot::ScreenshotData,
  editor_language: Option<String>,
  options: crate::overlay::selection_host::SelectionOptions,
) -> Result<Option<crate::screenshot::annotations::AnnotatedSelection>> {
  let options = crate::overlay::selection_host::SelectionOptions {
    hold_on_release: true,
    ..options
  };
  show_annotated(screenshot, editor_language, options).await
}

async fn show_annotated(
  screenshot: crate::screenshot::ScreenshotData,
  editor_language: Option<String>,
  options: crate::overlay::selection_host::SelectionOptions,
) -> Result<Option<crate::screenshot::annotations::AnnotatedSelection>> {
  tokio::task::spawn_blocking(move || {
    manager::show_annotated_selection_overlay(
      screenshot.width,
      screenshot.height,
      screenshot.rgba,
      screenshot.vd_offset_x,
      screenshot.vd_offset_y,
      editor_language,
      options,
    )
  })
  .await
  .map_err(|error| anyhow::anyhow!("Annotation overlay task failed: {error}"))?
}
pub(crate) mod text_field;
