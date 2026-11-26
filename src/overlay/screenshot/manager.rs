// Screenshot overlay manager - adapter over the shared selection overlay host

use std::sync::Arc;

use crate::overlay::selection::Selection;
use crate::overlay::selection_core::SelectionDrawColors;
use crate::overlay::selection_host::{
  SelectionOverlayAdapter, SelectionOverlayWindowState, show_selection_overlay as show_host,
};
use crate::screenshot::annotations::AnnotatedSelection;

struct ScreenshotSelectionAdapter;

impl SelectionOverlayAdapter for ScreenshotSelectionAdapter {
  type Result = AnnotatedSelection;

  const SUPPORTS_ANNOTATIONS: bool = true;

  const THREAD_NAME: &'static str = "screenshot-selection-overlay";
  const WINDOW_TITLE: &'static str = "FlinchScreenshotOverlay";
  const METRICS_PREFIX: &'static str = "overlay.screenshot";

  fn colors() -> SelectionDrawColors {
    SelectionDrawColors::DEFAULT
  }

  fn map_selection(
    state: &SelectionOverlayWindowState<Self>,
    selection: Selection,
  ) -> Self::Result {
    AnnotatedSelection {
      selection,
      annotations: state
        .editor
        .as_ref()
        .map(|editor| editor.annotations.clone())
        .unwrap_or_default(),
      pin: false,
    }
  }

  fn pin(result: &mut Self::Result) -> bool {
    result.pin = true;
    true
  }

  fn finalize_result(state: &SelectionOverlayWindowState<Self>, result: &mut Self::Result) {
    let result = &mut result.selection;
    let center_x = result.x as i32 + result.width as i32 / 2;
    let center_y = result.y as i32 + result.height as i32 / 2;
    result.source_app =
      crate::overlay::core::window_detection::detect_window_in_snapshot_at_overlay_point(
        &state.shared.controller.window_snapshot,
        center_x,
        center_y,
        state.shared.controller.vd_offset_x,
        state.shared.controller.vd_offset_y,
      )
      .and_then(|w| w.source_app);
  }
}

pub fn show_selection_overlay(
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  options: crate::overlay::selection_host::SelectionOptions,
) -> anyhow::Result<Option<Selection>> {
  show_host::<ScreenshotSelectionAdapter>(width, height, rgba, vd_offset_x, vd_offset_y, options)
    .map(|result| result.map(|annotated| annotated.selection))
}

pub fn show_annotated_selection_overlay(
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  editor_language: Option<String>,
  options: crate::overlay::selection_host::SelectionOptions,
) -> anyhow::Result<Option<AnnotatedSelection>> {
  crate::overlay::selection_host::show_selection_overlay_with_editor::<ScreenshotSelectionAdapter>(
    width,
    height,
    rgba,
    vd_offset_x,
    vd_offset_y,
    editor_language,
    options,
  )
}
