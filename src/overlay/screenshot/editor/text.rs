//! Native text editing, wrapping, formatting and numbered markers.

use super::layout::{
  invalidate, invalidate_region, text_border_hit, text_box_bounds, text_input_rect, toolbar_bounds,
};
use super::{AnnotationEditor, COLORS, DrawingLimit, ObjectMove, TextEdit};
use crate::overlay::screenshot::{cache, history, text_field};
use crate::overlay::selection::Rect;
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::{Annotation, Label, MAX_ANNOTATIONS, Tool, text_data};
use anyhow::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};

impl AnnotationEditor {
  pub(super) fn selected_text(&self) -> Option<&Annotation> {
    if let Some(edit) = &self.text_edit {
      return edit.preview.as_ref();
    }
    self
      .text_edit
      .as_ref()
      .and_then(|edit| edit.preview.as_ref())
      .or_else(|| self.moving.as_ref().map(|moving| &moving.preview))
      .or_else(|| {
        self
          .annotations
          .iter()
          .find(|drawing| Some(drawing.id) == self.selected)
      })
      .filter(|drawing| drawing.tool == Tool::Text)
  }

  pub(super) fn text_resize_hit(&self, pos: (f64, f64)) -> bool {
    let pos = if self.text_edit.is_none() {
      self
        .selected_text()
        .map_or(pos, |a| crate::screenshot::shape::rotate(a, pos, true))
    } else {
      pos
    };
    self.group_selection.len() < 2
      && self.text_selection_bounds().is_some_and(|bounds| {
        (pos.0 - bounds.right as f64)
          .hypot(pos.1 - (bounds.top as f64 + bounds.bottom as f64) / 2.0)
          <= 9.0
      })
  }

  pub(super) fn text_selection_bounds(&self) -> Option<RECT> {
    if let Some(edit) = &self.text_edit {
      return Some(RECT {
        left: edit.input_rect.left - 6,
        top: edit.input_rect.top - 6,
        right: edit.input_rect.right + 6,
        bottom: edit.input_rect.bottom + 6,
      });
    }
    self.selected_text().and_then(text_box_bounds)
  }

  pub(super) fn text_border_hit(&self, bounds: RECT, pos: (f64, f64)) -> bool {
    let pos = if self.text_edit.is_none() {
      self
        .selected_text()
        .map_or(pos, |a| crate::screenshot::shape::rotate(a, pos, true))
    } else {
      pos
    };
    text_border_hit(bounds, pos)
  }

  pub(super) fn begin_text_object(
    &mut self,
    controller: &SelectionControllerState,
    hwnd: HWND,
    index: usize,
  ) -> bool {
    let before = self.annotations[index].clone();
    let pos = controller.cursor_pos;
    // SAFETY: read-only system double-click interval; no input injection or message wait.
    let interval = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() };
    let double = self.last_text_click.is_some_and(|(id, time, point)| {
      id == before.id
        && time.elapsed().as_millis() <= interval as u128
        && (point.0 - pos.0).abs() <= 4.0
        && (point.1 - pos.1).abs() <= 4.0
    });
    self.selected = Some(before.id);
    self.reconcile_selection();
    if double {
      self.last_text_click = None;
      if let Err(error) = self.start_text(controller, hwnd, Some(before)) {
        tracing::error!(%error, "Failed to reopen text by double click");
        self.report_render_failure();
      }
    } else {
      self.last_text_click = Some((before.id, std::time::Instant::now(), pos));
      self.moving = Some(ObjectMove {
        index,
        preview: before.clone(),
        before,
        start: (pos.0.round() as i32, pos.1.round() as i32),
      });
      // SAFETY: parent owns this bounded move gesture until mouse release/cancellation.
      unsafe { SetCapture(hwnd) };
    }
    invalidate(hwnd);
    true
  }

  /// Blur regions have no color or stroke to change.
  pub(super) fn blur_mode(&self) -> bool {
    self.tool == Tool::Blur
      || self.selected.is_some_and(|id| {
        self
          .annotations
          .iter()
          .any(|a| a.id == id && a.tool == Tool::Blur)
      })
  }

  /// The size picker shows the Highlighter's painted thicknesses instead of the pen's.
  pub(super) fn highlighter_mode(&self) -> bool {
    self.tool == Tool::Highlighter
      || self.selected.is_some_and(|id| {
        self
          .annotations
          .iter()
          .any(|a| a.id == id && a.tool == Tool::Highlighter)
      })
  }

  pub(super) fn marker_mode(&self) -> bool {
    self.tool == Tool::Marker
      || self.selected.is_some_and(|id| {
        self
          .annotations
          .iter()
          .any(|a| a.id == id && a.tool == Tool::Marker)
      })
  }

  pub(super) fn start_text(
    &mut self,
    controller: &SelectionControllerState,
    hwnd: HWND,
    before: Option<Annotation>,
  ) -> Result<()> {
    let rect = Self::confirmed(controller)
      .ok_or_else(|| anyhow::anyhow!("Text requires a confirmed crop"))?;
    self.group_selection.clear();
    let point = before
      .as_ref()
      .and_then(|a| a.points.first().copied())
      .unwrap_or((
        controller.cursor_pos.0.round() as i32,
        controller.cursor_pos.1.round() as i32,
      ));
    let existing = before.as_ref().and_then(|a| match &a.label {
      Some(Label::Text(t)) => Some(t),
      _ => None,
    });
    let wrap_width = existing
      .map(|t| t.wrap_width)
      .unwrap_or(((rect.x + rect.w) as i32 - point.0 - 3).clamp(120, 320));
    let bounds = toolbar_bounds(
      controller,
      Rect {
        x: point.0 as f64,
        y: point.1 as f64,
        w: 1.0,
        h: 1.0,
      },
      &self.monitors,
    );
    let wrap_width = wrap_width.min((bounds.right - bounds.left).max(1));
    let size = existing.map(|t| t.size).unwrap_or(self.text_size);
    self.text_size = size;
    if let Some(data) = existing {
      self.text_appearance = data.appearance;
    }
    if let Some(drawing) = &before {
      self.selected = Some(drawing.id);
      if let Some(index) = COLORS.iter().position(|color| *color == drawing.color) {
        self.color = index;
      }
    } else {
      self.selected = None;
    }
    let input_rect = text_input_rect(
      point,
      wrap_width,
      (1, size * 2),
      bounds,
      self.toolbar_rect(controller, rect),
    );
    let field = text_field::TextField::new(
      hwnd,
      input_rect,
      size,
      existing.map(|t| t.content.as_str()).unwrap_or(""),
    )?;
    self.text_edit = Some(TextEdit {
      field,
      before,
      point,
      wrap_width,
      preview: None,
      resizing: None,
      moving: None,
      input_rect,
      preview_point: point,
    });
    self.tool = Tool::Text;
    self.object_mode = false;
    self.width_open = false;
    self.text_style_open = false;
    self.text_error = false;
    self.text_changed(controller, hwnd, None);
    invalidate(hwnd);
    Ok(())
  }

  pub fn text_changed(
    &mut self,
    controller: &SelectionControllerState,
    hwnd: HWND,
    child: Option<HWND>,
  ) {
    let Some(rect) = Self::confirmed(controller) else {
      return;
    };
    let toolbar = self.toolbar_rect(controller, rect);
    let old_text_bounds = self.text_selection_bounds();
    let old_chrome = self.chrome_region(controller, rect);
    let Some(edit) = &mut self.text_edit else {
      return;
    };
    if child.is_some_and(|child| child != edit.field.hwnd) {
      return;
    }
    if edit.moving.is_some() {
      let previous = edit.preview_point;
      let (dx, dy) = (edit.point.0 - previous.0, edit.point.1 - previous.1);
      edit.input_rect = RECT {
        left: edit.input_rect.left + dx,
        right: edit.input_rect.right + dx,
        top: edit.input_rect.top + dy,
        bottom: edit.input_rect.bottom + dy,
      };
      if let Some(preview) = &mut edit.preview {
        preview.points = vec![edit.point];
      }
      edit.preview_point = edit.point;
      edit.field.suspend_for_move();
      let damage = cache::union(old_chrome, self.chrome_region(controller, rect));
      invalidate_region(hwnd, damage);
      return;
    }
    let result = (|| -> Result<()> {
      let content = edit.field.content()?;
      let bounds = toolbar_bounds(
        controller,
        Rect {
          x: edit.point.0 as f64,
          y: edit.point.1 as f64,
          w: 1.0,
          h: 1.0,
        },
        &self.monitors,
      );
      let mut data = match edit.preview.as_ref().and_then(|a| match &a.label {
        Some(Label::Text(data)) => Some(data),
        _ => None,
      }) {
        Some(data)
          if data.content == content
            && data.size == self.text_size
            && (data.wrap_width == edit.wrap_width || edit.resizing.is_some()) =>
        {
          data.clone()
        },
        _ => text_data(content.clone(), self.text_size, edit.wrap_width)?,
      };
      data.appearance = self.text_appearance;
      let extent = data.extent;
      edit.preview = (!content.trim().is_empty()).then(|| Annotation {
        id: edit
          .before
          .as_ref()
          .map(|drawing| drawing.id)
          .unwrap_or(self.next_id),
        tool: Tool::Text,
        points: vec![edit.point],
        color: COLORS[self.color],
        width: 3,
        label: Some(Label::Text(data)),
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
      });
      edit
        .field
        .style(self.text_size, COLORS[self.color], self.text_appearance)?;
      // Synchronize a child suspended during movement even if its model rect already matches.
      edit.field.synchronize_after_move(edit.input_rect)?;
      // Apply width before measuring native wraps, without changing height twice per keystroke.
      let input_rect = text_input_rect(
        edit.point,
        edit.wrap_width,
        (
          extent.0,
          (edit.input_rect.bottom - edit.input_rect.top - 8).max(1),
        ),
        bounds,
        toolbar,
      );
      if input_rect != edit.input_rect {
        edit.field.layout(input_rect)?;
        edit.input_rect = input_rect;
      }
      let input_rect = text_input_rect(
        edit.point,
        edit.wrap_width,
        (extent.0, edit.field.content_height()?),
        bounds,
        toolbar,
      );
      if input_rect != edit.input_rect {
        edit.field.layout(input_rect)?;
        edit.input_rect = input_rect;
      }
      edit.preview_point = edit.point;
      Ok(())
    })();
    match result {
      Ok(()) => self.clear_toolbar_error(),
      Err(error) => {
        tracing::warn!(%error, "Failed to update live text preview");
        self.text_error = true;
      },
    }
    let mut damage = cache::union(old_chrome, self.chrome_region(controller, rect));
    for bounds in [old_text_bounds, self.text_selection_bounds()]
      .into_iter()
      .flatten()
    {
      damage = cache::union(
        damage,
        RECT {
          left: bounds.left - 8,
          top: bounds.top - 8,
          right: bounds.right + 8,
          bottom: bounds.bottom + 8,
        },
      );
    }
    invalidate_region(hwnd, damage);
  }

  pub fn complete_text(&mut self, hwnd: HWND, commit: bool, child: Option<HWND>) {
    let Some(edit) = &self.text_edit else {
      return;
    };
    if child.is_some_and(|child| child != edit.field.hwnd) {
      return;
    }
    let prepared = if commit {
      match edit.field.content() {
        Ok(content) if content.trim().is_empty() => None,
        Ok(content) => match text_data(content, self.text_size, edit.wrap_width) {
          Ok(mut data) => {
            data.appearance = self.text_appearance;
            Some(data)
          },
          Err(error) => {
            tracing::warn!(%error,"Failed to prepare annotation text");
            self.report_render_failure();
            invalidate(hwnd);
            return;
          },
        },
        Err(error) => {
          tracing::warn!(%error,"Inline annotation text exceeds input limit or could not be read");
          self.text_error = true;
          invalidate(hwnd);
          return;
        },
      }
    } else {
      None
    };
    let Some(edit) = self.text_edit.take() else {
      return;
    };
    if edit.resizing.is_some() || edit.moving.is_some() {
      self.toolbar_press = false;
      // SAFETY: this thread owns the active width gesture; completion releases its capture.
      if let Err(error) = unsafe { ReleaseCapture() } {
        tracing::warn!(%error, "Text resize capture release failed");
      }
    }
    let before = edit.before;
    if commit {
      let index = before
        .as_ref()
        .and_then(|a| self.annotations.iter().position(|item| item.id == a.id))
        .unwrap_or(self.annotations.len());
      let id = before.as_ref().map(|a| a.id).unwrap_or(self.next_id);
      let rotation = before.as_ref().map_or(0, |a| a.rotation_millidegrees);
      let after = prepared.map(|data| Annotation {
        corner_radius: 0,
        rotation_millidegrees: rotation,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
        label: Some(Label::Text(data)),
        id,
        tool: Tool::Text,
        points: vec![edit.point],
        color: COLORS[self.color],
        width: 3,
      });
      let is_new = before.is_none() && after.is_some();
      if self.apply_change(history::Change {
        counter: None,
        index,
        before,
        after,
      }) && is_new
      {
        self.next_id = self.next_id.saturating_add(1);
      }
      self.selected = self
        .annotations
        .iter()
        .find(|drawing| drawing.id == id)
        .map(|drawing| drawing.id);
    }
    drop(edit.field);
    // SAFETY: restores input to this thread's live parent after child disposal.
    if let Err(error) = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd)) }
      && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() } != hwnd
    {
      tracing::warn!(%error,"Failed to restore overlay text focus");
    }
    self.text_error = false;
    self.reconcile_selection();
    invalidate(hwnd);
  }

  pub(super) fn place_marker(&mut self, point: (i32, i32)) {
    let Some(next_id) = self.next_id.checked_add(1) else {
      self.limited = Some(DrawingLimit::Identifier);
      tracing::error!("Annotation identifier limit reached");
      return;
    };
    let Some(number) = self.history.next_marker() else {
      self.limited = Some(DrawingLimit::Marker);
      return;
    };
    if self.annotations.len() >= MAX_ANNOTATIONS {
      self.limited = Some(DrawingLimit::Drawings);
      return;
    }
    let annotation = Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: Some(Label::Number(number)),
      id: self.next_id,
      tool: Tool::Marker,
      points: vec![point],
      color: COLORS[self.color],
      width: 3,
    };
    let id = annotation.id;
    if self.apply_change(history::Change {
      counter: Some((number - 1, number)),
      index: self.annotations.len(),
      before: None,
      after: Some(annotation),
    }) {
      self.next_id = next_id;
      self.selected = Some(id);
    }
  }
}
