//! Scene caching, damage bounds and annotation selection rendering.

use super::layout::{combine_bounds, inset, text_box_bounds, text_width_handle, toolbar_bounds};
use super::{AnnotationEditor, ERROR_TOOLTIP_HEIGHT, ERROR_TOOLTIP_WIDTH};
use crate::overlay::screenshot::{cache, performance};
use crate::overlay::selection::{Rect, SelectionInteraction};
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::{Annotation, Tool, draw_annotations};
use anyhow::Result;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{DeleteObject, HDC, HFONT, SelectObject};

impl AnnotationEditor {
  pub(super) fn active_bounds(&self) -> Option<RECT> {
    let bounds = combine_bounds(
      self.draft.as_ref().and_then(Annotation::paint_bounds),
      self.moving.as_ref().and_then(|moving| {
        (if moving.preview.rotation_millidegrees != 0 {
          None
        } else {
          text_box_bounds(&moving.preview)
        })
        .map(|bounds| RECT {
          left: bounds.left - 40,
          top: bounds.top - 40,
          right: bounds.right + 40,
          bottom: bounds.bottom + 40,
        })
        .or_else(|| {
          moving.preview.paint_bounds().map(|bounds| RECT {
            left: bounds.left.saturating_sub(40),
            top: bounds.top.saturating_sub(40),
            right: bounds.right.saturating_add(40),
            bottom: bounds.bottom.saturating_add(40),
          })
        })
      }),
    );
    self.group_move.iter().fold(bounds, |bounds, moving| {
      combine_bounds(
        bounds,
        moving.preview.paint_bounds().map(|rect| RECT {
          left: rect.left - 10,
          top: rect.top - 10,
          right: rect.right + 10,
          bottom: rect.bottom + 10,
        }),
      )
    })
  }

  pub(crate) fn shows_selection_indicator(&self, controller: &SelectionControllerState) -> bool {
    Self::confirmed(controller).is_none() || (self.tool == Tool::Select && !self.object_mode)
  }

  pub(super) fn cache_key(&self, controller: &SelectionControllerState) -> Option<cache::Key> {
    let rect = Self::confirmed(controller)?;
    let crop = RECT {
      left: rect.x as i32 - 80,
      top: rect.y as i32 - 80,
      right: (rect.x + rect.w) as i32 + 80,
      bottom: (rect.y + rect.h) as i32 + 80,
    };
    // Keep the cache extent stable throughout native width gestures. A changing
    // text/toolbar rectangle would rebuild the complete scene on every mouse move.
    let region = if let Some(edit) = &self.text_edit {
      cache::union(
        crop,
        cache::union(
          toolbar_bounds(controller, rect, &self.monitors),
          toolbar_bounds(
            controller,
            Rect {
              x: edit.point.0 as f64,
              y: edit.point.1 as f64,
              w: 1.0,
              h: 1.0,
            },
            &self.monitors,
          ),
        ),
      )
    } else {
      cache::union(crop, self.chrome_region(controller, rect))
    };
    let region = cache::intersect(
      region,
      RECT {
        left: 0,
        top: 0,
        right: controller.width,
        bottom: controller.height,
      },
    );
    Some(cache::Key {
      crop: [
        rect.x.to_bits(),
        rect.y.to_bits(),
        rect.w.to_bits(),
        rect.h.to_bits(),
      ],
      region: [region.left, region.top, region.right, region.bottom],
      revision: self.document_revision,
      show_indicator: self.shows_selection_indicator(controller),
      excluded: self
        .moving
        .as_ref()
        .map(|moving| moving.before.id)
        .or_else(|| {
          self
            .text_edit
            .as_ref()
            .and_then(|edit| edit.before.as_ref().map(|a| a.id))
        }),
    })
  }

  pub(super) fn chrome_region(&self, controller: &SelectionControllerState, rect: Rect) -> RECT {
    // Reserve the tooltip's bounded envelope even when hidden, so dismissal restores pixels.
    // The tooltip envelope also covers the toolbar shadow, which spreads far less.
    let toolbar = self.toolbar_rect(controller, rect);
    let m = self.metrics(controller, rect);
    let toolbar_region = RECT {
      left: (toolbar.left - m.px(ERROR_TOOLTIP_WIDTH)).max(0),
      right: (toolbar.right + m.px(ERROR_TOOLTIP_WIDTH)).min(controller.width),
      top: (toolbar.top - m.px(ERROR_TOOLTIP_HEIGHT + 16)).max(0),
      bottom: (toolbar.bottom + m.px(ERROR_TOOLTIP_HEIGHT + 16)).min(controller.height),
    };
    let region = cache::union(toolbar_region, self.picker_region(controller, rect));
    let region = self
      .selected_text()
      .and_then(Annotation::paint_bounds)
      .map_or(region, |bounds| {
        cache::union(
          region,
          RECT {
            left: bounds.left.saturating_sub(40),
            top: bounds.top.saturating_sub(40),
            right: bounds.right.saturating_add(40),
            bottom: bounds.bottom.saturating_add(40),
          },
        )
      });
    // Entrance motion moves only rendered chrome, within this bounded damage envelope.
    let region = RECT {
      top: region.top - 6,
      bottom: region.bottom + 6,
      ..region
    };
    self.text_selection_bounds().map_or(region, |bounds| {
      cache::union(
        region,
        RECT {
          left: bounds.left - 8,
          top: bounds.top - 8,
          right: bounds.right + 8,
          bottom: bounds.bottom + 8,
        },
      )
    })
  }

  pub fn restore_scene(
    &mut self,
    controller: &SelectionControllerState,
    dc: HDC,
    damage: RECT,
  ) -> Option<u64> {
    let key = self.cache_key(controller)?;
    // Only the overlay's own pixels can be damaged; the cache is clipped to them too.
    let damage = cache::intersect(
      damage,
      RECT {
        left: 0,
        top: 0,
        right: controller.width,
        bottom: controller.height,
      },
    );
    match self.scene_cache.restore(key, dc, damage) {
      Ok(restored) => restored,
      Err(error) => {
        tracing::warn!(%error, "Screenshot scene cache restore failed; rebuilding reference scene");
        self.scene_cache.invalidate();
        None
      },
    }
  }

  pub fn paint_cached(
    &mut self,
    controller: &SelectionControllerState,
    dc: HDC,
    font: HFONT,
    damage: RECT,
    restored: bool,
  ) {
    self.draw_scene(controller, dc, font, Some(damage), restored);
  }

  #[cfg(test)]
  pub fn draw(&mut self, controller: &SelectionControllerState, dc: HDC, font: HFONT) {
    self.draw_scene(controller, dc, font, None, false);
  }

  /// Kept drawings stay visible inside a new crop while it is dragged out from the editor.
  fn draw_reselection(&mut self, controller: &SelectionControllerState, dc: HDC) {
    let Some((left, top, right, bottom)) = controller
      .interaction
      .get_current_rect(controller.width as f64, controller.height as f64)
      .filter(|_| !self.annotations.is_empty())
    else {
      return;
    };
    let clip = RECT {
      left: left as i32 + 2,
      top: top as i32 + 2,
      right: right as i32 - 2,
      bottom: bottom as i32 - 2,
    };
    if clip.right <= clip.left || clip.bottom <= clip.top {
      return;
    }
    let painted = paint_blur(dc, self.frozen.as_ref(), self.annotations.iter(), clip)
      .and_then(|()| draw_annotations(dc, self.annotations.iter(), (0, 0), clip));
    if let Err(error) = painted {
      tracing::error!(%error, "Failed to render drawings during a new selection");
      self.report_render_failure();
    }
  }

  fn draw_scene(
    &mut self,
    controller: &SelectionControllerState,
    dc: HDC,
    font: HFONT,
    damage: Option<RECT>,
    restored: bool,
  ) {
    self.update_motion(controller);
    let SelectionInteraction::Confirmed { rect, .. } = controller.interaction else {
      self.draw_reselection(controller, dc);
      return;
    };
    let clip = RECT {
      left: rect.x as i32 + 2,
      top: rect.y as i32 + 2,
      right: (rect.x + rect.w) as i32 - 2,
      bottom: (rect.y + rect.h) as i32 - 2,
    };
    let started = self.diagnostics.as_ref().map(|d| d.start());
    let surface_misses = self.chrome.surface_misses;
    let icon_misses = self.chrome.icon_misses;
    if !restored {
      self.paint_sample.completed_layer_rebuilds += 1;
      let completed: Vec<&Annotation> = self
        .annotations
        .iter()
        .filter(|drawing| {
          self
            .text_edit
            .as_ref()
            .and_then(|edit| edit.before.as_ref())
            .is_none_or(|before| before.id != drawing.id)
            && self
              .moving
              .as_ref()
              .is_none_or(|moving| moving.before.id != drawing.id)
            && !self
              .group_move
              .iter()
              .any(|moving| moving.before.id == drawing.id)
        })
        .collect();
      let painted = paint_blur(dc, self.frozen.as_ref(), completed.iter().copied(), clip)
        .and_then(|()| draw_annotations(dc, completed.iter().copied(), (0, 0), clip));
      if let Err(error) = painted {
        tracing::error!(%error, "Failed to render completed screenshot annotations");
        self.report_render_failure();
      }
      if damage.is_some()
        && let Some(key) = self.cache_key(controller)
        && !self.render_failed
        && let Err(error) = self.scene_cache.capture(key, dc)
      {
        tracing::warn!(%error, "Screenshot scene cache unavailable; using reference repaint path");
      }
    }
    let saved = if let Some(damage) = damage {
      use windows::Win32::Graphics::Gdi::{IntersectClipRect, RestoreDC, SaveDC};
      // SAFETY: thread-owned offscreen DC; snapshot precedes clipping and state is restored below.
      let saved = unsafe { SaveDC(dc) };
      if saved == 0 {
        tracing::error!("Failed to save screenshot damage clipping state");
        self.report_render_failure();
        return;
      }
      // SAFETY: this paint owns the saved DC state and a client-coordinate damage rectangle.
      if unsafe { IntersectClipRect(dc, damage.left, damage.top, damage.right, damage.bottom) }.0
        == 0
      {
        tracing::error!("Failed to clip screenshot damage region");
        // SAFETY: restores the matching saved state before returning on the error path.
        if !unsafe { RestoreDC(dc, saved) }.as_bool() {
          tracing::error!("Failed to restore screenshot damage clipping after error");
        }
        self.report_render_failure();
        return;
      }
      Some(saved)
    } else {
      None
    };
    if let Some(edit) = &mut self.text_edit
      && edit.moving.is_none()
      && let Err(error) = edit.field.backdrop(
        dc,
        edit.input_rect,
        self.document_revision,
        self.text_appearance,
      )
    {
      tracing::error!(%error, "Failed to prepare inline text screenshot backing");
      self.report_render_failure();
    }
    let previews: Vec<&Annotation> = self
      .moving
      .iter()
      .map(|moving| &moving.preview)
      .chain(self.group_move.iter().map(|moving| &moving.preview))
      .chain(self.draft.iter())
      .chain(
        self
          .text_edit
          .iter()
          .filter(|edit| edit.moving.is_some())
          .filter_map(|edit| edit.preview.as_ref()),
      )
      .collect();
    let painted = paint_blur(dc, self.frozen.as_ref(), previews.iter().copied(), clip)
      .and_then(|()| draw_annotations(dc, previews.iter().copied(), (0, 0), clip));
    if let Err(error) = painted {
      tracing::error!(%error, "Failed to render screenshot drawing draft");
      self.report_render_failure();
    }
    // Editing controls may extend beyond the crop; only annotation pixels are crop-clipped.
    let clip = RECT {
      left: 0,
      top: 0,
      right: controller.width,
      bottom: controller.height,
    };
    for id in self.group_selection.iter().filter(|_| self.object_mode) {
      if Some(*id) == self.selected {
        continue;
      }
      let drawing = self
        .group_move
        .iter()
        .find(|m| m.before.id == *id)
        .map(|m| &m.preview)
        .or_else(|| self.annotations.iter().find(|a| a.id == *id));
      if let Some(bounds) = drawing.and_then(chrome_bounds)
        && let Err(error) = draw_object_chrome(dc, bounds, clip, true)
      {
        tracing::error!(%error, "Failed to render grouped selection");
        self.render_failed = true;
        self.error_button = Some(12);
      }
    }
    let selected = self
      .moving
      .as_ref()
      .map(|moving| &moving.preview)
      .or_else(|| {
        self
          .annotations
          .iter()
          .find(|drawing| Some(drawing.id) == self.selected)
      });
    let text_bounds = self.text_selection_bounds();
    let bounds = text_bounds.or_else(|| {
      if self.text_edit.is_none() {
        selected.and_then(chrome_bounds)
      } else {
        None
      }
    });
    let geometry_handles = if self.group_selection.len() < 2 {
      selected
        .and_then(crate::screenshot::arrow::Arrow::from_annotation)
        .map(|arrow| {
          if let Some(a) = selected.filter(|a| !a.curve_controls.is_empty()) {
            a.points
              .iter()
              .map(|&(x, y)| (x as f64, y as f64))
              .collect::<Vec<_>>()
          } else {
            arrow.handles().to_vec()
          }
        })
    } else {
      None
    };
    let shape_handles = if self.group_selection.len() < 2 {
      selected
        .filter(|a| self.node_mode != Some(a.id))
        .and_then(crate::screenshot::shape::handles)
    } else {
      None
    };
    let radius_handle = if self.group_selection.len() < 2 {
      selected.and_then(crate::screenshot::shape::radius_handle)
    } else {
      None
    };
    let rotation_handle = if self.group_selection.len() < 2 && self.text_edit.is_none() {
      selected
        .filter(|a| self.node_mode != Some(a.id))
        .and_then(crate::screenshot::shape::rotation_handle)
    } else {
      None
    };
    let text_frame = selected
      .filter(|a| a.tool == Tool::Text && a.rotation_millidegrees != 0 && self.text_edit.is_none())
      .and_then(crate::screenshot::shape::frame_handles);
    let pen_nodes = selected
      .filter(|a| {
        a.tool.freehand() && self.node_mode == Some(a.id) && self.group_selection.len() < 2
      })
      .map(|a| {
        self
          .node_indices
          .iter()
          .filter_map(|i| a.points.get(*i))
          .map(|&(x, y)| crate::screenshot::path::world(a, (x as f64, y as f64)))
          .collect::<Vec<_>>()
      });
    let pen_node_mode = pen_nodes.is_some();
    let selected_node_point = selected.and_then(|a| {
      self.selected_node.and_then(|i| {
        if a.tool == Tool::Arrow && a.curve_controls.is_empty() {
          crate::screenshot::arrow::Arrow::from_annotation(a)?
            .handles()
            .get(i)
            .copied()
        } else {
          let &(x, y) = a.points.get(i)?;
          Some(crate::screenshot::path::world(a, (x as f64, y as f64)))
        }
      })
    });
    let text_handle_bounds = text_bounds.map(|bounds| {
      if let Some(annotation) =
        selected.filter(|a| a.tool == Tool::Text && self.text_edit.is_none())
      {
        let point = crate::screenshot::shape::rotate(
          annotation,
          (
            bounds.right as f64,
            (bounds.top + bounds.bottom) as f64 / 2.0,
          ),
          false,
        );
        text_width_handle(RECT {
          right: point.0.round() as i32,
          top: point.1.round() as i32,
          bottom: point.1.round() as i32,
          left: 0,
        })
      } else {
        text_width_handle(bounds)
      }
    });
    if let Some(points) = text_frame
      && let Err(error) = draw_object_outline(dc, clip, clip, true, Some(&points))
    {
      tracing::error!(%error,"Failed to render rotated text selection");
      self.report_render_failure();
    }
    if let Some(handles) = shape_handles
      && let Err(error) = draw_object_outline(
        dc,
        RECT {
          left: handles[0].0.round() as i32,
          top: handles[0].1.round() as i32,
          right: handles[4].0.round() as i32,
          bottom: handles[4].1.round() as i32,
        },
        clip,
        true,
        Some(&handles),
      )
    {
      tracing::error!(%error, "Failed to render shape selection");
      self.report_render_failure();
    }
    let mut handles: Vec<_> = geometry_handles
      .as_ref()
      .cloned()
      .or_else(|| shape_handles.map(|h| h.to_vec()))
      .or(pen_nodes)
      .unwrap_or_default();
    if let Some(point) = radius_handle {
      handles.push(point);
    }
    if let Some(point) = rotation_handle {
      handles.push(point);
    }
    for (x, y) in handles {
      let radius_control = radius_handle == Some((x, y));
      let node_selected = selected_node_point == Some((x, y));
      let rotation_control = rotation_handle == Some((x, y));
      let x = x.round() as i32;
      let y = y.round() as i32;
      let bounds = RECT {
        left: x.saturating_sub(6),
        top: y.saturating_sub(6),
        right: x.saturating_add(7),
        bottom: y.saturating_add(7),
      };
      let visible = cache::intersect(bounds, clip);
      if visible.right <= visible.left || visible.bottom <= visible.top {
        continue;
      }
      let mut scene = crate::overlay::chrome::Scene::default();
      let result = scene
        .rounded(bounds, 6.5, [245, 245, 245], 255)
        .and_then(|()| {
          scene.rounded(
            inset(bounds, 2),
            4.5,
            if radius_control || node_selected {
              [245, 185, 65]
            } else if rotation_control {
              [100, 210, 160]
            } else {
              [96, 165, 250]
            },
            255,
          )
        })
        .and_then(|()| self.handle_chrome.render(dc, visible, &scene));
      if let Err(error) = result {
        tracing::error!(%error, "Failed to render drawing handles");
        self.report_render_failure();
      }
    }
    if let Some(bounds) =
      bounds.filter(|_| geometry_handles.is_none() && shape_handles.is_none() && !pen_node_mode)
    {
      let result = (if text_frame.is_none() {
        draw_object_chrome(dc, bounds, clip, true)
      } else {
        Ok(())
      })
      .and_then(|()| {
        if text_bounds.is_some() && self.group_selection.len() < 2 {
          let handle = text_handle_bounds.unwrap_or_else(|| text_width_handle(bounds));
          let visible = cache::intersect(handle, clip);
          if visible.right > visible.left && visible.bottom > visible.top {
            let mut scene = crate::overlay::chrome::Scene::default();
            scene.rounded(handle, 6.5, [245, 245, 245], 255)?;
            scene.rounded(inset(handle, 2), 4.5, [96, 165, 250], 255)?;
            self.handle_chrome.render(dc, visible, &scene)?;
          }
        }
        Ok(())
      });
      if let Err(error) = result {
        tracing::error!(%error, "Failed to render drawing selection");
        self.report_render_failure();
      }
    }
    if let Some(started) = started {
      self.paint_sample.annotations_us = performance::elapsed_us(started);
    }
    let toolbar_started = self.diagnostics.as_ref().map(|d| d.start());
    if controller.interaction.is_confirmed_idle()
      && let Err(error) = self.draw_toolbar(controller, dc, font, rect)
    {
      tracing::error!(%error, "Failed to render screenshot annotation toolbar");
      self.report_render_failure();
    }
    if let Some(saved) = saved
      && saved != 0
    {
      // SAFETY: restores the matching SaveDC on the same live DC and overlay thread.
      if !unsafe { windows::Win32::Graphics::Gdi::RestoreDC(dc, saved) }.as_bool() {
        tracing::error!("Failed to restore screenshot damage clipping state");
        self.report_render_failure();
      }
    }
    self.paint_sample.cache_bytes = self.scene_cache.bytes();
    if let Some(started) = toolbar_started {
      self.paint_sample.toolbar_us = performance::elapsed_us(started);
      self.paint_sample.drawings = self.annotations.len();
      self.paint_sample.points = self
        .annotations
        .iter()
        .chain(self.draft.iter())
        .map(|a| a.points.len())
        .sum();
      self.paint_sample.surface_misses = self.chrome.surface_misses - surface_misses;
      self.paint_sample.icon_misses = self.chrome.icon_misses - icon_misses;
    }
  }
}

/// Blur covers capture pixels only, so regions go down before any other drawing.
/// The layer is rebuilt from the frozen capture, so repaints never compound.
pub(super) fn paint_blur<'a>(
  dc: HDC,
  frozen: Option<&super::FrozenPixels>,
  drawings: impl Iterator<Item = &'a Annotation>,
  clip: RECT,
) -> Result<()> {
  use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SetDIBitsToDevice,
  };
  let Some(frozen) = frozen else {
    return Ok(());
  };
  let Some(mut layer) = crate::screenshot::blur::layer(
    &frozen.rgba,
    frozen.width,
    frozen.height,
    drawings,
    (clip.left, clip.top, clip.right, clip.bottom),
  ) else {
    return Ok(());
  };
  layer.into_opaque_bgra();
  let info = BITMAPINFO {
    bmiHeader: BITMAPINFOHEADER {
      biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
      biWidth: layer.width,
      biHeight: -layer.height,
      biPlanes: 1,
      biBitCount: 32,
      biCompression: BI_RGB.0,
      ..Default::default()
    },
    ..Default::default()
  };
  // SAFETY: the DC is thread-owned, and the buffer holds exactly the width x height
  // top-down 32-bit pixels described by info for the duration of this call.
  let lines = unsafe {
    SetDIBitsToDevice(
      dc,
      layer.left,
      layer.top,
      layer.width as u32,
      layer.height as u32,
      0,
      0,
      0,
      layer.height as u32,
      layer.rgba.as_ptr().cast(),
      &info,
      DIB_RGB_COLORS,
    )
  };
  anyhow::ensure!(lines != 0, "Failed to paint a blur region");
  Ok(())
}

/// Selection chrome hugs the drawn area, not a blur region's soft edge.
fn chrome_bounds(drawing: &Annotation) -> Option<RECT> {
  let bounds = drawing.paint_bounds()?;
  if drawing.tool != Tool::Blur {
    return Some(bounds);
  }
  let edge = crate::screenshot::blur::SOFT_EDGE;
  Some(RECT {
    left: bounds.left + edge,
    top: bounds.top + edge,
    right: bounds.right - edge,
    bottom: bounds.bottom - edge,
  })
}

fn draw_object_chrome(dc: HDC, bounds: RECT, clip: RECT, is_text: bool) -> Result<()> {
  draw_object_outline(dc, bounds, clip, is_text, None)
}

fn draw_object_outline(
  dc: HDC,
  bounds: RECT,
  clip: RECT,
  is_text: bool,
  points: Option<&[(f64, f64); 8]>,
) -> Result<()> {
  use windows::Win32::Graphics::Gdi::{
    CreatePen, GetStockObject, IntersectClipRect, NULL_BRUSH, PS_DOT, PS_SOLID, Rectangle,
    RestoreDC, SaveDC,
  };
  // SAFETY: DC is overlay-thread owned; clipping and selected objects are restored before cleanup.
  unsafe {
    let saved = SaveDC(dc);
    anyhow::ensure!(saved != 0, "Failed to save selection chrome DC");
    let result = (|| {
      anyhow::ensure!(
        IntersectClipRect(dc, clip.left, clip.top, clip.right, clip.bottom).0 != 0,
        "Failed to clip drawing selection"
      );
      let pen = CreatePen(
        if is_text { PS_SOLID } else { PS_DOT },
        1,
        COLORREF(if is_text { 0x00faa560 } else { 0x00f5f0f0 }),
      );
      anyhow::ensure!(!pen.is_invalid(), "Failed to create drawing selection pen");
      let old = SelectObject(dc, pen.into());
      SelectObject(dc, GetStockObject(NULL_BRUSH));
      let success = if let Some(points) = points {
        let points = [0, 2, 4, 6].map(|i| windows::Win32::Foundation::POINT {
          x: points[i].0.round() as i32,
          y: points[i].1.round() as i32,
        });
        windows::Win32::Graphics::Gdi::Polygon(dc, &points).as_bool()
      } else {
        Rectangle(dc, bounds.left, bounds.top, bounds.right, bounds.bottom).as_bool()
      };
      SelectObject(dc, old);
      if !DeleteObject(pen.into()).as_bool() {
        tracing::warn!("Failed to release drawing selection pen");
      }
      anyhow::ensure!(success, "Failed to draw drawing selection");
      Ok(())
    })();
    anyhow::ensure!(
      RestoreDC(dc, saved).as_bool(),
      "Failed to restore drawing selection DC"
    );
    result
  }
}
