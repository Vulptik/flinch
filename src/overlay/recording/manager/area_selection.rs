// Area recording selection overlay - adapter over the shared selection overlay host

use std::sync::Arc;

use super::super::commands::AreaRecordingResult;
use crate::overlay::selection::Selection;
use crate::overlay::selection_core::SelectionDrawColors;
use crate::overlay::selection_host::{
  SelectionOverlayAdapter, SelectionOverlayWindowState, show_selection_overlay as show_host,
};
use windows::Win32::Foundation::COLORREF;

struct AreaSelectionAdapter;

impl SelectionOverlayAdapter for AreaSelectionAdapter {
  type Result = AreaRecordingResult;

  const THREAD_NAME: &'static str = "area-recording-selection-overlay";
  const WINDOW_TITLE: &'static str = "FlinchAreaRecordingOverlay";
  const METRICS_PREFIX: &'static str = "overlay.area_recording";
  const MINIMUM_SELECTION_SIZE: f64 = crate::overlay::selection::RECORDING_MIN_SELECTION_SIZE;

  fn colors() -> SelectionDrawColors {
    SelectionDrawColors {
      border: COLORREF(0x002F2FD9),
      preview: COLORREF(0x006666FF),
    }
  }

  fn map_selection(
    state: &SelectionOverlayWindowState<Self>,
    selection: Selection,
  ) -> Self::Result {
    AreaRecordingResult {
      x: selection.x as i32 + state.shared.controller.vd_offset_x,
      y: selection.y as i32 + state.shared.controller.vd_offset_y,
      width: selection.width,
      height: selection.height,
      screen_width: state.shared.controller.width as u32,
      screen_height: state.shared.controller.height as u32,
      source_app: None,
    }
  }

  fn finalize_result(state: &SelectionOverlayWindowState<Self>, result: &mut Self::Result) {
    let center_x = result.x - state.shared.controller.vd_offset_x + result.width as i32 / 2;
    let center_y = result.y - state.shared.controller.vd_offset_y + result.height as i32 / 2;
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

pub fn show_area_recording_selection(
  width: u32,
  height: u32,
  rgba: Arc<Vec<u8>>,
  vd_offset_x: i32,
  vd_offset_y: i32,
  options: crate::overlay::selection_host::SelectionOptions,
) -> anyhow::Result<Option<AreaRecordingResult>> {
  show_host::<AreaSelectionAdapter>(width, height, rgba, vd_offset_x, vd_offset_y, options)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn area_recordings_require_the_recording_minimum() {
    assert_eq!(
      <AreaSelectionAdapter as SelectionOverlayAdapter>::MINIMUM_SELECTION_SIZE,
      crate::overlay::selection::RECORDING_MIN_SELECTION_SIZE
    );
  }
}
