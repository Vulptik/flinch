//! GDI drawing primitives for selection overlays.
//!
//! All functions take a destination HDC and draw into it.
//! Resources (pens, brushes) are created and deleted within each call.

use crate::error::rich_display_context;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
  AlphaBlend, BLENDFUNCTION, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen,
  CreateSolidBrush, DeleteDC, DeleteObject, FillRect, GetStockObject, HALFTONE, HDC, NULL_BRUSH,
  PS_SOLID, Rectangle, SRCCOPY, SelectObject, SetStretchBltMode,
};

/// Maximum width in pixels for the X/Y/W/H selection label.
const LABEL_MAX_WIDTH: i32 = 300;

pub const SELECTION_LABEL_HEIGHT: i32 = 56;
pub const SELECTION_LABEL_GAP: i32 = 10;

const DASH_LENGTH: i32 = 6;
const DASH_GAP: i32 = 4;
const DASH_CYCLE: i32 = DASH_LENGTH + DASH_GAP;

/// Compact dimensions, independent of desktop origin.
pub fn format_selection_label(_x: i32, _y: i32, w: i32, h: i32) -> String {
  format!("{w} × {h}")
}

pub fn format_selection_coordinates(x: i32, y: i32) -> String {
  format!("X: {x}   Y: {y}")
}

fn selection_label_rect(sel: RECT, text_width: i32, win_width: i32, win_height: i32) -> RECT {
  let panel_width = (text_width + 28)
    .clamp(148, LABEL_MAX_WIDTH + 28)
    .min(win_width);
  let panel_height = SELECTION_LABEL_HEIGHT.min(win_height);
  let left = (sel.left + (sel.right - sel.left - panel_width) / 2)
    .clamp(0, (win_width - panel_width).max(0));
  let top = if sel.top >= panel_height + SELECTION_LABEL_GAP {
    sel.top - panel_height - SELECTION_LABEL_GAP
  } else if sel.bottom + SELECTION_LABEL_GAP + panel_height <= win_height {
    sel.bottom + SELECTION_LABEL_GAP
  } else {
    sel.top + SELECTION_LABEL_GAP
  }
  .clamp(0, (win_height - panel_height).max(0));
  RECT {
    left,
    top,
    right: left + panel_width,
    bottom: top + panel_height,
  }
}

/// Widest area the dimensions panel can take for this selection.
pub fn selection_label_bounds(sel: RECT, win_width: i32, win_height: i32) -> RECT {
  selection_label_rect(sel, LABEL_MAX_WIDTH, win_width, win_height)
}

/// Pixel-aligned corner brackets and optional midpoint marks, inside capture bounds.
/// Visual dimensions do not change the selection interaction's hit areas.
pub fn selection_mark_rects(sel: RECT, handles: bool) -> Vec<RECT> {
  let width = sel.right - sel.left;
  let height = sel.bottom - sel.top;
  if width <= 0 || height <= 0 {
    return Vec::new();
  }
  let thickness = 3.min(width).min(height);
  let corner_x = 14.min((width / 3).max(thickness));
  let corner_y = 14.min((height / 3).max(thickness));
  let mut marks = Vec::with_capacity(12);
  for x in [sel.left, sel.right - corner_x] {
    for y in [sel.top, sel.bottom - thickness] {
      marks.push(RECT {
        left: x,
        top: y,
        right: x + corner_x,
        bottom: y + thickness,
      });
    }
  }
  for x in [sel.left, sel.right - thickness] {
    for y in [sel.top, sel.bottom - corner_y] {
      marks.push(RECT {
        left: x,
        top: y,
        right: x + thickness,
        bottom: y + corner_y,
      });
    }
  }
  if handles && width >= 48 {
    let cx = sel.left + width / 2;
    for y in [sel.top, sel.bottom - thickness] {
      marks.push(RECT {
        left: cx - 8,
        top: y,
        right: cx + 8,
        bottom: y + thickness,
      });
    }
  }
  if handles && height >= 48 {
    let cy = sel.top + height / 2;
    for x in [sel.left, sel.right - thickness] {
      marks.push(RECT {
        left: x,
        top: cy - 8,
        right: x + thickness,
        bottom: cy + 8,
      });
    }
  }
  marks
}

/// Draw restrained connecting edges and crisp accent brackets without selecting GDI objects.
/// # Safety
/// `dc` must be a valid destination DC owned by the calling overlay thread.
pub unsafe fn draw_selection_chrome(
  dc: HDC,
  sel: RECT,
  color: COLORREF,
  handles: bool,
) -> Result<(), String> {
  // SAFETY: the caller supplies this owner-thread DC and bounded selection.
  unsafe { draw_selection_chrome_strength(dc, sel, color, handles, 0.25) }
}

/// # Safety
/// `dc` must be a valid destination DC owned by the selection thread.
pub unsafe fn draw_selection_chrome_strength(
  dc: HDC,
  sel: RECT,
  color: COLORREF,
  handles: bool,
  edge_strength: f32,
) -> Result<(), String> {
  if sel.right <= sel.left || sel.bottom <= sel.top {
    return Ok(());
  }
  for (shade, rectangles) in [
    (
      tint_color(color, edge_strength.clamp(0.0, 1.0)),
      vec![
        RECT {
          bottom: sel.top + 2.min(sel.bottom - sel.top),
          ..sel
        },
        RECT {
          top: sel.bottom - 2.min(sel.bottom - sel.top),
          ..sel
        },
        RECT {
          right: sel.left + 2.min(sel.right - sel.left),
          ..sel
        },
        RECT {
          left: sel.right - 2.min(sel.right - sel.left),
          ..sel
        },
      ],
    ),
    (color, selection_mark_rects(sel, handles)),
  ] {
    // SAFETY: the brush is local, never selected into a DC, and released after bounded fills.
    let brush = unsafe { CreateSolidBrush(shade) };
    if brush.is_invalid() {
      return Err("Selection chrome brush creation failed".into());
    }
    let mut failed = false;
    for bounds in rectangles {
      // SAFETY: live thread-owned DC and valid local brush with finite in-bounds geometry.
      failed |= unsafe { FillRect(dc, &bounds, brush) } == 0;
    }
    // SAFETY: FillRect borrows the brush; it is no longer in use here.
    if !unsafe { DeleteObject(brush.into()) }.as_bool() {
      return Err("Selection chrome brush release failed".into());
    }
    if failed {
      return Err("Selection chrome drawing failed".into());
    }
  }
  Ok(())
}

/// Contrast-backed active feedback. `phase` animates edge segments without fixed corner brackets.
/// # Safety
/// The destination DC must belong to the current overlay thread.
pub unsafe fn draw_selection_feedback(
  dc: HDC,
  sel: RECT,
  color: COLORREF,
  _phase: Option<i32>,
  _snap: bool,
) -> Result<(), String> {
  // SAFETY: caller supplies the live owner-thread DC and bounded selection.
  unsafe { draw_selection_chrome(dc, sel, color, true) }
}
/// Returns the 8 handle RECTs for a confirmed selection rectangle.
/// Order: NW, NE, SW, SE, N, S, W, E
/// Each handle is `handle_size` × `handle_size`, centered on its corner/midpoint.
pub fn handle_rects(sel: RECT, handle_size: i32) -> [RECT; 8] {
  let half = handle_size / 2;
  let cx = (sel.left + sel.right) / 2;
  let cy = (sel.top + sel.bottom) / 2;
  let make = |cx: i32, cy: i32| RECT {
    left: cx - half,
    top: cy - half,
    right: cx + half,
    bottom: cy + half,
  };
  [
    make(sel.left, sel.top),     // NW
    make(sel.right, sel.top),    // NE
    make(sel.left, sel.bottom),  // SW
    make(sel.right, sel.bottom), // SE
    make(cx, sel.top),           // N
    make(cx, sel.bottom),        // S
    make(sel.left, cy),          // W
    make(sel.right, cy),         // E
  ]
}

fn colorref_to_rgb(color: COLORREF) -> (u8, u8, u8) {
  let value = color.0;
  (
    (value & 0xFF) as u8,
    ((value >> 8) & 0xFF) as u8,
    ((value >> 16) & 0xFF) as u8,
  )
}

fn rgb_to_colorref(r: u8, g: u8, b: u8) -> COLORREF {
  COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

fn scale_channel(channel: u8, factor: f32) -> u8 {
  ((channel as f32 * factor).round()).clamp(0.0, 255.0) as u8
}

fn tint_color(color: COLORREF, factor: f32) -> COLORREF {
  let (r, g, b) = colorref_to_rgb(color);
  rgb_to_colorref(
    scale_channel(r, factor),
    scale_channel(g, factor),
    scale_channel(b, factor),
  )
}

fn mix_with_white(color: COLORREF, amount: f32) -> COLORREF {
  let (r, g, b) = colorref_to_rgb(color);
  let mix = |channel: u8| -> u8 {
    (channel as f32 + (255.0 - channel as f32) * amount)
      .round()
      .clamp(0.0, 255.0) as u8
  };
  rgb_to_colorref(mix(r), mix(g), mix(b))
}

fn dash_phase() -> i32 {
  let elapsed_ms = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_millis() as i32)
    .unwrap_or(0);
  (elapsed_ms / 75) % DASH_CYCLE
}

unsafe fn draw_dashed_horizontal(
  dc: HDC,
  left: i32,
  right: i32,
  y: i32,
  color: COLORREF,
  phase: i32,
) {
  if right <= left {
    return;
  }

  let brush = unsafe { CreateSolidBrush(color) };
  let mut start = left - phase;
  while start < right {
    let seg_left = start.max(left);
    let seg_right = (start + DASH_LENGTH).min(right);
    if seg_right > seg_left {
      let rect = RECT {
        left: seg_left,
        top: y,
        right: seg_right,
        bottom: y + 1,
      };
      let _ = unsafe { FillRect(dc, &rect, brush) };
    }
    start += DASH_CYCLE;
  }
  let _ = unsafe { DeleteObject(brush.into()) };
}

unsafe fn draw_dashed_vertical(
  dc: HDC,
  x: i32,
  top: i32,
  bottom: i32,
  color: COLORREF,
  phase: i32,
) {
  if bottom <= top {
    return;
  }

  let brush = unsafe { CreateSolidBrush(color) };
  let mut start = top - phase;
  while start < bottom {
    let seg_top = start.max(top);
    let seg_bottom = (start + DASH_LENGTH).min(bottom);
    if seg_bottom > seg_top {
      let rect = RECT {
        left: x,
        top: seg_top,
        right: x + 1,
        bottom: seg_bottom,
      };
      let _ = unsafe { FillRect(dc, &rect, brush) };
    }
    start += DASH_CYCLE;
  }
  let _ = unsafe { DeleteObject(brush.into()) };
}

/// Draw a semi-transparent dark dim over the entire DC.
/// alpha: 0=transparent, 255=opaque. Typical use: 160.
///
/// # Safety
/// Caller must ensure `dc` is a valid HDC.
pub unsafe fn draw_dim_layer(dc: HDC, width: i32, height: i32, alpha: u8) -> Result<(), String> {
  let tmp_dc = unsafe { CreateCompatibleDC(Some(dc)) };
  if tmp_dc.is_invalid() {
    return Err("draw_dim_layer: CreateCompatibleDC failed".to_string());
  }
  let tmp_bmp = unsafe { CreateCompatibleBitmap(dc, 1, 1) };
  if tmp_bmp.is_invalid() {
    let _ = unsafe { DeleteDC(tmp_dc) };
    return Err("draw_dim_layer: CreateCompatibleBitmap failed".to_string());
  }
  let old = unsafe { SelectObject(tmp_dc, tmp_bmp.into()) };

  let black_brush = unsafe { CreateSolidBrush(COLORREF(0x00000000)) };
  let fill_rect = RECT {
    left: 0,
    top: 0,
    right: 1,
    bottom: 1,
  };
  unsafe {
    let _ = FillRect(tmp_dc, &fill_rect, black_brush);
    let _ = DeleteObject(black_brush.into());
    SetStretchBltMode(dc, HALFTONE);
  }

  let blend = BLENDFUNCTION {
    BlendOp: 0,
    BlendFlags: 0,
    SourceConstantAlpha: alpha,
    AlphaFormat: 0,
  };
  let ok = unsafe { AlphaBlend(dc, 0, 0, width, height, tmp_dc, 0, 0, 1, 1, blend) };

  unsafe {
    let _ = SelectObject(tmp_dc, old);
    let _ = DeleteObject(tmp_bmp.into());
    let _ = DeleteDC(tmp_dc);
  }

  if ok.as_bool() {
    Ok(())
  } else {
    Err("draw_dim_layer: AlphaBlend failed".to_string())
  }
}

/// Cut out the selection rect from the dim by blitting screenshot pixels back in.
/// `shot_dc` is the HDC with the screenshot bitmap selected in.
///
/// # Safety
/// Caller must ensure `dc` and `shot_dc` are valid HDCs.
pub unsafe fn draw_cutout(dc: HDC, shot_dc: HDC, sel: RECT) -> Result<(), String> {
  use windows::Win32::Graphics::Gdi::{IntersectClipRect, RestoreDC, SaveDC};
  // SAFETY: caller owns both live DCs; preserve its existing damage clip.
  let saved = unsafe { SaveDC(dc) };
  if saved == 0 {
    return Err("draw_cutout: SaveDC failed".to_string());
  }
  // SAFETY: intersect rather than replace the caller's clipping region.
  let clipped = unsafe { IntersectClipRect(dc, sel.left, sel.top, sel.right, sel.bottom) };
  let result = if clipped.0 == 0 {
    Err("draw_cutout: IntersectClipRect failed".to_string())
  } else {
    unsafe {
      BitBlt(
        dc,
        sel.left,
        sel.top,
        sel.right - sel.left,
        sel.bottom - sel.top,
        Some(shot_dc),
        sel.left,
        sel.top,
        SRCCOPY,
      )
    }
    .map_err(|e| rich_display_context("draw_cutout: BitBlt failed", e))
  };
  // SAFETY: restore the matching saved state even if clipping/copying failed.
  if !unsafe { RestoreDC(dc, saved) }.as_bool() {
    return Err("draw_cutout: RestoreDC failed".to_string());
  }
  result
}

/// Draw a two-tone border rectangle around `sel`.
///
/// # Safety
/// Caller must ensure `dc` is a valid HDC.
pub unsafe fn draw_selection_border(dc: HDC, sel: RECT, color: COLORREF) {
  let outer_pen = unsafe { CreatePen(PS_SOLID, 3, tint_color(color, 0.52)) };
  let inner_pen = unsafe { CreatePen(PS_SOLID, 1, mix_with_white(color, 0.08)) };
  let null_brush = unsafe { GetStockObject(NULL_BRUSH) };
  let old_brush = unsafe { SelectObject(dc, null_brush) };
  let old_pen = unsafe { SelectObject(dc, outer_pen.into()) };
  let _ = unsafe { Rectangle(dc, sel.left, sel.top, sel.right, sel.bottom) };
  let _ = unsafe { SelectObject(dc, inner_pen.into()) };
  let _ = unsafe { Rectangle(dc, sel.left + 1, sel.top + 1, sel.right - 1, sel.bottom - 1) };
  unsafe {
    let _ = SelectObject(dc, old_pen);
    let _ = SelectObject(dc, old_brush);
    let _ = DeleteObject(outer_pen.into());
    let _ = DeleteObject(inner_pen.into());
  }
}

/// Draw a lighter dashed border rectangle for provisional selections.
///
/// # Safety
/// Caller must ensure `dc` is a valid HDC.
pub unsafe fn draw_selection_border_dashed(dc: HDC, sel: RECT, color: COLORREF) {
  let dashed_color = mix_with_white(color, 0.18);
  let phase = dash_phase();
  unsafe {
    draw_dashed_horizontal(dc, sel.left, sel.right, sel.top, dashed_color, phase);
    draw_dashed_horizontal(dc, sel.left, sel.right, sel.bottom - 1, dashed_color, phase);
    draw_dashed_vertical(dc, sel.left, sel.top, sel.bottom, dashed_color, phase);
    draw_dashed_vertical(dc, sel.right - 1, sel.top, sel.bottom, dashed_color, phase);
  }
}

/// Draw a compact solid crosshair mark at the selection center.
///
/// # Safety
/// Caller must ensure `dc` is a valid HDC.
pub unsafe fn draw_selection_center_guides(dc: HDC, sel: RECT, color: COLORREF) {
  let width = sel.right - sel.left;
  let height = sel.bottom - sel.top;
  if width <= 2 || height <= 2 {
    return;
  }

  let guide_color = mix_with_white(color, 0.24);
  let center_x = sel.left + (width - 1) / 2;
  let center_y = sel.top + (height - 1) / 2;
  let arm_half = (width.min(height) / 5).clamp(8, 14);
  let horizontal = RECT {
    left: center_x - arm_half,
    top: center_y,
    right: center_x + arm_half + 1,
    bottom: center_y + 1,
  };
  let vertical = RECT {
    left: center_x,
    top: center_y - arm_half,
    right: center_x + 1,
    bottom: center_y + arm_half + 1,
  };
  let brush = unsafe { CreateSolidBrush(guide_color) };
  if brush.is_invalid() {
    return;
  }

  unsafe {
    let _ = FillRect(dc, &horizontal, brush);
    let _ = FillRect(dc, &vertical, brush);
    let _ = DeleteObject(brush.into());
  }
}

/// Draw a rounded dark pill behind the label rect with a subtle border.
///
/// # Safety
/// Caller must ensure `dc` is a valid HDC.
#[cfg(test)]
pub unsafe fn draw_label_pill(dc: HDC, rect: RECT, radius: i32) -> Result<(), String> {
  use windows::Win32::Graphics::Gdi::RoundRect;
  let brush = unsafe {
    CreateSolidBrush(crate::overlay::theme::colorref(
      crate::overlay::theme::SURFACE,
    ))
  };
  if brush.is_invalid() {
    return Err("draw_label_pill: CreateSolidBrush failed".to_string());
  }
  let pen = unsafe {
    CreatePen(
      PS_SOLID,
      1,
      crate::overlay::theme::colorref(crate::overlay::theme::OUTLINE),
    )
  };
  if pen.is_invalid() {
    let _ = unsafe { DeleteObject(brush.into()) };
    return Err("draw_label_pill: CreatePen failed".to_string());
  }
  let old_pen = unsafe { SelectObject(dc, pen.into()) };
  let old_brush = unsafe { SelectObject(dc, brush.into()) };
  let ok = unsafe {
    RoundRect(
      dc,
      rect.left,
      rect.top,
      rect.right,
      rect.bottom,
      radius * 2,
      radius * 2,
    )
  };
  unsafe {
    let _ = SelectObject(dc, old_pen);
    let _ = SelectObject(dc, old_brush);
    let _ = DeleteObject(pen.into());
    let _ = DeleteObject(brush.into());
  }
  if ok.as_bool() {
    #[cfg(debug_assertions)]
    crate::overlay::theme::debug_sample_surface(dc, false, rect.left + radius + 3, rect.top + 3);
    Ok(())
  } else {
    Err("draw_label_pill: RoundRect failed".to_string())
  }
}

/// Draw dimensions and coordinates with the shared control renderer and measured text width.
#[allow(clippy::too_many_arguments)]
pub fn draw_selection_label(
  dc: HDC,
  renderer: &mut crate::overlay::chrome::Renderer,
  sel: RECT,
  win_width: i32,
  win_height: i32,
  desktop_origin: (i32, i32),
  opacity: u8,
  offset_y: i32,
) -> Result<(), String> {
  use crate::error::ErrorExt;
  use crate::overlay::{
    chrome::{Scene, TextStyle},
    theme,
  };
  (|| -> anyhow::Result<()> {
    let dimensions = format_selection_label(0, 0, sel.right - sel.left, sel.bottom - sel.top);
    let coordinates =
      format_selection_coordinates(sel.left + desktop_origin.0, sel.top + desktop_origin.1);
    let width = renderer
      .measure_text(&dimensions, TextStyle::DIMENSIONS)?
      .max(renderer.measure_text(&coordinates, TextStyle::COORDINATES)?);
    let panel = selection_label_rect(sel, width.ceil() as i32, win_width, win_height);
    let mut scene = Scene::default();
    scene.panel(panel)?;
    scene.styled_text(
      RECT {
        left: panel.left + 14,
        right: panel.right - 14,
        top: panel.top + 7,
        bottom: panel.top + 29,
      },
      &dimensions,
      TextStyle::DIMENSIONS,
      theme::TEXT,
    )?;
    scene.styled_text(
      RECT {
        left: panel.left + 14,
        right: panel.right - 14,
        top: panel.top + 29,
        bottom: panel.top + 49,
      },
      &coordinates,
      TextStyle::COORDINATES,
      theme::SECONDARY_TEXT,
    )?;
    renderer.render_with_offset(
      dc,
      panel,
      &scene,
      opacity,
      offset_y.min((win_height - panel.bottom).max(0)),
    )
  })()
  .map_err(|error| error.to_rich_string())
}
#[cfg(test)]
mod tests {
  #[test]
  fn cutout_preserves_caller_damage_clip() {
    use super::*;
    use crate::overlay::gdi::layered::LayeredBitmap;
    use windows::Win32::Graphics::Gdi::{GdiFlush, GetClipBox, IntersectClipRect};
    let mut source = LayeredBitmap::new(16, 16).expect("source");
    let mut destination = LayeredBitmap::new(16, 16).expect("destination");
    source.pixels_mut().fill(200);
    destination.pixels_mut().fill(40);
    let damage = RECT {
      left: 5,
      top: 6,
      right: 9,
      bottom: 10,
    };
    let mut after = RECT::default();
    // SAFETY: live test-owned DIB DCs; flush before reading their pixels.
    unsafe {
      assert_ne!(IntersectClipRect(destination.dc, 5, 6, 9, 10).0, 0);
      draw_cutout(
        destination.dc,
        source.dc,
        RECT {
          left: 1,
          top: 1,
          right: 15,
          bottom: 15,
        },
      )
      .expect("cutout");
      assert_ne!(GetClipBox(destination.dc, &mut after).0, 0);
      assert!(GdiFlush().as_bool());
    }
    assert_eq!(after, damage);
    for (i, pixel) in destination
      .pixels_mut()
      .as_chunks::<4>()
      .0
      .iter()
      .enumerate()
    {
      let (x, y) = ((i % 16) as i32, (i / 16) as i32);
      let expected = if (5..9).contains(&x) && (6..10).contains(&y) {
        200
      } else {
        40
      };
      assert_eq!(pixel[0], expected, "pixel {x},{y}");
    }
  }
  use super::*;
  use windows::Win32::Foundation::RECT;

  #[test]
  fn indicator_coordinates_preserve_negative_desktop_positions() {
    assert_eq!(
      format_selection_coordinates(-1920, -240),
      "X: -1920   Y: -240"
    );
  }

  #[test]
  fn indicator_placement_uses_available_space_and_stays_in_bounds() {
    for (sel, expected_top) in [
      (
        RECT {
          left: 100,
          top: 100,
          right: 500,
          bottom: 300,
        },
        34,
      ),
      (
        RECT {
          left: 0,
          top: 0,
          right: 50,
          bottom: 50,
        },
        60,
      ),
      (
        RECT {
          left: 650,
          top: 0,
          right: 720,
          bottom: 480,
        },
        10,
      ),
    ] {
      let panel = selection_label_rect(sel, 120, 720, 480);
      assert_eq!(panel.top, expected_top);
      assert!(panel.left >= 0 && panel.top >= 0 && panel.right <= 720 && panel.bottom <= 480);
    }
  }

  #[test]
  fn selection_marks_stay_inside_tiny_and_negative_origin_regions() {
    for width in [1, 2, 3, 8, 24, 48, 100] {
      for height in [1, 2, 8, 24, 48, 100] {
        let sel = RECT {
          left: -50,
          top: -20,
          right: -50 + width,
          bottom: -20 + height,
        };
        for mark in selection_mark_rects(sel, true) {
          assert!(mark.left >= sel.left && mark.top >= sel.top);
          assert!(mark.right <= sel.right && mark.bottom <= sel.bottom);
          assert!(mark.right > mark.left && mark.bottom > mark.top);
        }
      }
    }
  }

  #[test]
  fn selection_midpoints_hide_during_gestures_and_on_small_axes() {
    let sel = RECT {
      left: 0,
      top: 0,
      right: 100,
      bottom: 100,
    };
    assert_eq!(selection_mark_rects(sel, false).len(), 8);
    assert_eq!(selection_mark_rects(sel, true).len(), 12);
    assert_eq!(
      selection_mark_rects(RECT { right: 20, ..sel }, true).len(),
      10
    );
    assert!(selection_mark_rects(RECT { right: 0, ..sel }, true).is_empty());
  }

  #[test]
  fn test_handle_rects_count() {
    let sel = RECT {
      left: 100,
      top: 100,
      right: 300,
      bottom: 200,
    };
    let handles = handle_rects(sel, 8);
    assert_eq!(handles.len(), 8);
  }

  #[test]
  fn test_nw_handle_position() {
    let sel = RECT {
      left: 100,
      top: 100,
      right: 300,
      bottom: 200,
    };
    let handles = handle_rects(sel, 8);
    // NW handle (index 0) should be centered on top-left corner
    let nw = handles[0];
    assert_eq!(nw.left, 100 - 4);
    assert_eq!(nw.top, 100 - 4);
    assert_eq!(nw.right, 100 + 4);
    assert_eq!(nw.bottom, 100 + 4);
  }

  #[test]
  fn test_label_text_format() {
    assert_eq!(format_selection_label(100, 200, 640, 480), "640 × 480");
  }
}
