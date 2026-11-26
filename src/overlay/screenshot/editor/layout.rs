//! Shared geometry for rendering, hit testing and invalidation.

use super::{
  ACTIONS, BUTTONS, CELL, CELL_GAP, ERROR_BADGE_SIZE, GRIP_WIDTH, GROUP_GAP, GROUP_STARTS,
  PANEL_PAD, TOOLBAR_VERTICAL_PAD,
};
use crate::overlay::screenshot::cache;
use crate::overlay::selection::Rect;
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::{Annotation, Label};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::InvalidateRect;

/// Toolbar and popover lengths in physical pixels at one monitor's UI scale.
/// Every length is rounded once here, so layout and hit testing invert exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Metrics {
  pub(super) scale: f32,
  pub(super) cell: i32,
  pub(super) gap: i32,
  pub(super) pad: i32,
  pub(super) grip: i32,
  pub(super) vertical_pad: i32,
  pub(super) group_gap: i32,
  pub(super) badge: i32,
}

impl Metrics {
  /// Highest Windows display scaling (500%); larger reported values are clamped to it.
  const MAX_SCALE: f64 = 5.0;

  pub(super) fn new(dpi_scale: f64) -> Self {
    let scale = if dpi_scale.is_finite() {
      dpi_scale.clamp(1.0, Self::MAX_SCALE) as f32
    } else {
      tracing::warn!(dpi_scale, "Invalid monitor DPI scale; toolbar uses 100%");
      1.0
    };
    let px = |value: i32| (value as f32 * scale).round() as i32;
    Self {
      scale,
      cell: px(CELL),
      gap: px(CELL_GAP),
      pad: px(PANEL_PAD),
      grip: px(GRIP_WIDTH),
      vertical_pad: px(TOOLBAR_VERTICAL_PAD),
      group_gap: px(GROUP_GAP),
      badge: px(ERROR_BADGE_SIZE),
    }
  }

  /// A design length at 100% scaling, in physical pixels.
  pub(super) fn px(self, value: i32) -> i32 {
    (value as f32 * self.scale).round() as i32
  }

  pub(super) fn radius(self, value: f32) -> f32 {
    value * self.scale
  }

  /// Extra width a single-row toolbar reserves for its group separators.
  fn separators(self) -> i32 {
    GROUP_STARTS.len() as i32 * self.group_gap
  }
}

/// 100% scaling, matching `Metrics::new(1.0)`, for fixtures on unscaled test monitors.
#[cfg(test)]
pub(super) const UNSCALED: Metrics = Metrics {
  scale: 1.0,
  cell: CELL,
  gap: CELL_GAP,
  pad: PANEL_PAD,
  grip: GRIP_WIDTH,
  vertical_pad: TOOLBAR_VERTICAL_PAD,
  group_gap: GROUP_GAP,
  badge: ERROR_BADGE_SIZE,
};

#[cfg(test)]
pub(super) fn toolbar_rect(
  controller: &SelectionControllerState,
  rect: Rect,
  monitors: &[crate::monitors::MonitorInfo],
) -> RECT {
  layout_toolbar(
    rect,
    toolbar_bounds(controller, rect, monitors),
    toolbar_metrics(controller, rect, monitors),
  )
}

fn toolbar_monitor<'a>(
  controller: &SelectionControllerState,
  rect: Rect,
  monitors: &'a [crate::monitors::MonitorInfo],
) -> Option<&'a crate::monitors::MonitorInfo> {
  let cx = rect.x as i32 + rect.w as i32 / 2 + controller.vd_offset_x;
  let cy = rect.y as i32 + rect.h as i32 / 2 + controller.vd_offset_y;
  monitors.iter().min_by_key(|m| {
    let dx = cx as i64 - (cx as i64).clamp(m.x as i64, m.x as i64 + m.width as i64 - 1);
    let dy = cy as i64 - (cy as i64).clamp(m.y as i64, m.y as i64 + m.height as i64 - 1);
    dx * dx + dy * dy
  })
}

/// The toolbar stays clamped to the crop's monitor, so that monitor's scale sizes it.
pub(super) fn toolbar_metrics(
  controller: &SelectionControllerState,
  rect: Rect,
  monitors: &[crate::monitors::MonitorInfo],
) -> Metrics {
  Metrics::new(toolbar_monitor(controller, rect, monitors).map_or(1.0, |m| m.dpi_scale))
}

pub(super) fn toolbar_bounds(
  controller: &SelectionControllerState,
  rect: Rect,
  monitors: &[crate::monitors::MonitorInfo],
) -> RECT {
  toolbar_monitor(controller, rect, monitors)
    .map(|m| RECT {
      left: m.x - controller.vd_offset_x,
      top: m.y - controller.vd_offset_y,
      right: m.x - controller.vd_offset_x + m.width as i32,
      bottom: m.y - controller.vd_offset_y + m.height as i32,
    })
    .unwrap_or(RECT {
      left: 0,
      top: 0,
      right: controller.width,
      bottom: controller.height,
    })
}

/// Centers a popover on the button that opened it, above the toolbar when it fits and
/// below otherwise, clamped to the monitor.
pub(super) fn layout_popover_at(
  toolbar: RECT,
  anchor: RECT,
  bounds: RECT,
  requested_width: i32,
  requested_height: i32,
  m: Metrics,
) -> RECT {
  let width = requested_width.min(bounds.right - bounds.left).max(1);
  let height = requested_height.min(bounds.bottom - bounds.top).max(1);
  let spacing = m.px(8);
  let left = ((anchor.left + anchor.right - width) / 2).clamp(bounds.left, bounds.right - width);
  let top = if toolbar.top - height - spacing >= bounds.top {
    toolbar.top - height - spacing
  } else {
    (toolbar.bottom + spacing).clamp(bounds.top, bounds.bottom - height)
  };
  RECT {
    left,
    top,
    right: left + width,
    bottom: top + height,
  }
}

pub(super) fn avoid_text_region(preferred: RECT, bounds: RECT, text: RECT) -> RECT {
  let area = |rect: RECT| {
    let overlap = cache::intersect(rect, text);
    (overlap.right - overlap.left).max(0) as i64 * (overlap.bottom - overlap.top).max(0) as i64
  };
  if area(preferred) == 0 {
    return preferred;
  }
  let (width, height) = (
    preferred.right - preferred.left,
    preferred.bottom - preferred.top,
  );
  [
    (preferred.left, text.top - height - 4),
    (preferred.left, text.bottom + 4),
    (text.left - width - 4, preferred.top),
    (text.right + 4, preferred.top),
  ]
  .into_iter()
  .map(|(left, top)| {
    let left = left.clamp(bounds.left, (bounds.right - width).max(bounds.left));
    let top = top.clamp(bounds.top, (bounds.bottom - height).max(bounds.top));
    RECT {
      left,
      top,
      right: left + width,
      bottom: top + height,
    }
  })
  .min_by_key(|rect| {
    (
      area(*rect),
      (rect.left - preferred.left).abs() + (rect.top - preferred.top).abs(),
    )
  })
  .unwrap_or(preferred)
}

pub(super) fn font_cell(panel: RECT, index: i32, m: Metrics) -> RECT {
  let pad = m.px(8);
  let width = (panel.right - panel.left - pad * 2) / 3;
  RECT {
    left: panel.left + pad + index % 3 * width,
    right: panel.left + pad + (index % 3 + 1) * width - m.px(3),
    top: panel.top + m.px(96) + index / 3 * m.px(30),
    bottom: panel.top + m.px(124) + index / 3 * m.px(30),
  }
}

pub(super) fn style_menu_button(panel: RECT, m: Metrics) -> RECT {
  RECT {
    left: panel.left + m.px(8),
    right: panel.right - m.px(8),
    top: panel.bottom - m.px(36),
    bottom: panel.bottom - m.px(8),
  }
}

pub(super) fn appearance_cell(panel: RECT, index: i32, m: Metrics) -> RECT {
  width_row(panel, index, m)
}

pub(super) fn text_box_bounds(annotation: &Annotation) -> Option<RECT> {
  let Label::Text(data) = annotation.label.as_ref()? else {
    return None;
  };
  let &(x, y) = annotation.points.first()?;
  Some(RECT {
    left: x - 7,
    top: y - 7,
    right: x + data.wrap_width + 7,
    bottom: y + data.extent.1.max(data.size) + 7,
  })
}

pub(super) fn text_width_handle(bounds: RECT) -> RECT {
  let center = (bounds.top + bounds.bottom) / 2;
  RECT {
    left: bounds.right - 6,
    right: bounds.right + 7,
    top: center - 6,
    bottom: center + 7,
  }
}

pub(super) fn text_border_hit(bounds: RECT, pos: (f64, f64)) -> bool {
  contains(bounds, pos) && !contains(inset(bounds, 5), pos)
}

pub(super) fn text_input_rect(
  point: (i32, i32),
  wrap_width: i32,
  extent: (i32, i32),
  bounds: RECT,
  _toolbar: RECT,
) -> RECT {
  let left = point.0.clamp(bounds.left, bounds.right - 1);
  let top = point.1.clamp(bounds.top, bounds.bottom - 1);
  // Keep the input at the text anchor; clipping does not move the saved annotation.
  let width = wrap_width.min((bounds.right - left).max(1));
  let height = (extent.1 + 8)
    .clamp(32, 320)
    .min((bounds.bottom - top).max(1));
  RECT {
    left,
    top,
    right: left + width,
    bottom: top + height,
  }
}

pub(super) fn color_cell(panel: RECT, index: i32, m: Metrics) -> RECT {
  let pad = m.px(8);
  let w = (panel.right - panel.left - pad * 2) / 4;
  let h = (panel.bottom - panel.top - pad * 2) / 3;
  let size = w.min(h) - m.px(4);
  let left = panel.left + pad + index % 4 * w + (w - size) / 2;
  let top = panel.top + pad + index / 4 * h + (h - size) / 2;
  RECT {
    left,
    top,
    right: left + size,
    bottom: top + size,
  }
}

pub(super) fn width_row(panel: RECT, index: i32, m: Metrics) -> RECT {
  let pad = m.px(8);
  let height = (panel.bottom - panel.top - pad * 2) / 3;
  RECT {
    left: panel.left + pad,
    right: panel.right - pad,
    top: panel.top + pad + index * height,
    bottom: panel.top + pad + (index + 1) * height - m.px(2),
  }
}

pub(super) fn layout_toolbar(rect: Rect, bounds: RECT, m: Metrics) -> RECT {
  use crate::overlay::gdi::drawing::{SELECTION_LABEL_GAP, SELECTION_LABEL_HEIGHT};
  let capacity =
    ((bounds.right - bounds.left - m.pad * 2 - m.grip - m.separators()) / m.cell).clamp(1, BUTTONS);
  let rows = (BUTTONS + capacity - 1) / capacity;
  let columns = (BUTTONS + rows - 1) / rows;
  let width = columns * m.cell - m.gap
    + m.pad * 2
    + m.grip
    + if columns == BUTTONS {
      m.separators()
    } else {
      0
    };
  let height = (BUTTONS + columns - 1) / columns * m.cell - m.gap + m.vertical_pad * 2;
  let left = (rect.x as i32 + (rect.w as i32 - width) / 2)
    .clamp(bounds.left, (bounds.right - width).max(bounds.left));
  // Reserve the shared dimensions pill when it must sit below a top-edge crop.
  // The pill itself is unscaled; only the toolbar's own spacing takes the UI scale.
  let label_clearance = SELECTION_LABEL_HEIGHT + SELECTION_LABEL_GAP + 8;
  let spacing = m.px(12);
  let below = (rect.y + rect.h) as i32
    + if rect.y < (SELECTION_LABEL_HEIGHT + SELECTION_LABEL_GAP) as f64 {
      label_clearance
    } else {
      spacing
    };
  let top = if below + height <= bounds.bottom {
    below
  } else if rect.y as i32 - height - label_clearance >= bounds.top {
    rect.y as i32 - height - label_clearance
  } else {
    // No outside room (e.g. a full-screen crop): sit inside the crop's bottom edge,
    // clear of the dimensions pill, which falls back to the crop's top edge.
    let inside_bottom = ((rect.y + rect.h) as i32).min(bounds.bottom) - spacing - height;
    inside_bottom.clamp(bounds.top, (bounds.bottom - height).max(bounds.top))
  };
  RECT {
    left,
    top,
    right: left + width,
    bottom: top + height,
  }
}

pub(super) fn error_badge_rect(toolbar: RECT, button: i32, m: Metrics) -> RECT {
  let cell = toolbar_button(toolbar, button, m);
  let overhang = m.px(8);
  RECT {
    left: cell.right - m.badge + overhang,
    right: cell.right + overhang,
    top: cell.top - overhang,
    bottom: cell.top - overhang + m.badge,
  }
}

pub(super) fn error_tooltip_rect(
  toolbar: RECT,
  button: i32,
  bounds: RECT,
  width: i32,
  height: i32,
  m: Metrics,
) -> RECT {
  let cell = toolbar_button(toolbar, button, m);
  let width = width.min(bounds.right - bounds.left).max(1);
  let height = height.min(bounds.bottom - bounds.top).max(1);
  let left = (cell.right - width).clamp(bounds.left, (bounds.right - width).max(bounds.left));
  tooltip_at(toolbar, bounds, left, width, height, m)
}

/// A name tooltip centered on its button, on the same side of the toolbar as error details.
pub(super) fn button_tooltip_rect(
  toolbar: RECT,
  button: i32,
  bounds: RECT,
  width: i32,
  height: i32,
  m: Metrics,
) -> RECT {
  let cell = toolbar_button(toolbar, button, m);
  let width = width.min(bounds.right - bounds.left).max(1);
  let height = height.min(bounds.bottom - bounds.top).max(1);
  let left = ((cell.left + cell.right - width) / 2)
    .clamp(bounds.left, (bounds.right - width).max(bounds.left));
  tooltip_at(toolbar, bounds, left, width, height, m)
}

/// Below the toolbar when it fits, otherwise above it, kept within `bounds`.
fn tooltip_at(toolbar: RECT, bounds: RECT, left: i32, width: i32, height: i32, m: Metrics) -> RECT {
  let spacing = m.px(8);
  let top = if toolbar.bottom + spacing + height <= bounds.bottom {
    toolbar.bottom + spacing
  } else {
    (toolbar.top - height - spacing).clamp(bounds.top, (bounds.bottom - height).max(bounds.top))
  };
  RECT {
    left,
    top,
    right: left + width,
    bottom: top + height,
  }
}

pub(super) fn toolbar_columns(toolbar: RECT, m: Metrics) -> i32 {
  ((toolbar.right - toolbar.left - m.pad * 2 - m.grip + m.gap) / m.cell).clamp(1, BUTTONS)
}

pub(super) fn toolbar_button(toolbar: RECT, index: i32, m: Metrics) -> RECT {
  let index = ACTIONS
    .iter()
    .position(|action| *action == index)
    .map_or(0, |i| i as i32);
  let columns = toolbar_columns(toolbar, m);
  let group_gap = if columns == BUTTONS {
    GROUP_STARTS
      .into_iter()
      .filter(|start| index >= *start)
      .count() as i32
      * m.group_gap
  } else {
    0
  };
  let row = index / columns;
  let row_count = (BUTTONS - row * columns).min(columns);
  let row_offset = (columns - row_count) * m.cell / 2;
  let left = toolbar.left + m.grip + m.pad + row_offset + index % columns * m.cell + group_gap;
  let top = toolbar.top + m.vertical_pad + row * m.cell;
  RECT {
    left,
    top,
    right: left + m.cell - m.gap,
    bottom: top + m.cell - m.gap,
  }
}

pub(super) fn toolbar_grip(toolbar: RECT, m: Metrics) -> RECT {
  RECT {
    right: toolbar.left + m.grip,
    bottom: toolbar.top + m.cell - m.gap + m.vertical_pad * 2,
    ..toolbar
  }
}

/// The grip's hover highlight, which also centers its dots: level with the first row of
/// buttons, as far from the panel's edge as they are, and as far from the first button.
pub(super) fn toolbar_grip_highlight(toolbar: RECT, m: Metrics) -> RECT {
  let grip = toolbar_grip(toolbar, m);
  RECT {
    left: grip.left + m.vertical_pad,
    top: grip.top + m.vertical_pad,
    right: grip.right + m.pad - m.vertical_pad,
    bottom: grip.bottom - m.vertical_pad,
  }
}

pub(super) fn toolbar_index(toolbar: RECT, pos: (f64, f64), m: Metrics) -> Option<i32> {
  ACTIONS
    .into_iter()
    .find(|index| contains(toolbar_button(toolbar, *index, m), pos))
}

pub(super) fn inset(rect: RECT, amount: i32) -> RECT {
  RECT {
    left: rect.left + amount,
    top: rect.top + amount,
    right: rect.right - amount,
    bottom: rect.bottom - amount,
  }
}

pub(super) fn contains(rect: RECT, pos: (f64, f64)) -> bool {
  pos.0 >= rect.left as f64
    && pos.0 < rect.right as f64
    && pos.1 >= rect.top as f64
    && pos.1 < rect.bottom as f64
}

pub(super) fn combine_bounds(old: Option<RECT>, new: Option<RECT>) -> Option<RECT> {
  match (old, new) {
    (Some(a), Some(b)) => Some(cache::union(a, b)),
    (a, b) => a.or(b),
  }
}

pub(super) fn invalidate_region(hwnd: HWND, damage: RECT) {
  if cache::area(damage) == 0 {
    return;
  }
  // SAFETY: live overlay HWND; Windows copies the rectangle and clips to the client area.
  if !unsafe { InvalidateRect(Some(hwnd), Some(&damage), false) }.as_bool() {
    tracing::warn!("Failed to invalidate screenshot damage region");
  }
}

pub(super) fn invalidate(hwnd: HWND) {
  // SAFETY: called only for this thread's live overlay window.
  if !unsafe { InvalidateRect(Some(hwnd), None, false) }.as_bool() {
    tracing::warn!("Failed to invalidate annotation overlay");
  }
}
