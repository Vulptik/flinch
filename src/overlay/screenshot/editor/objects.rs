//! Object selection, path nodes, transforms and drawing drafts.

use super::{AnnotationEditor, COLORS, DrawingLimit};
use crate::overlay::screenshot::{history, pen};
use crate::overlay::selection::Rect;
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::{Annotation, Label, Tool, text_data};

impl AnnotationEditor {
  pub(super) fn refresh_nodes(&mut self) {
    self.selected_node = None;
    if let Some(drawing) = self
      .annotations
      .iter()
      .find(|a| Some(a.id) == self.node_mode && Some(a.id) == self.selected)
    {
      self.node_indices = crate::screenshot::path::nodes(drawing);
    } else {
      self.node_mode = None;
      self.node_indices.clear();
    }
  }

  pub(super) fn reconcile_selection(&mut self) {
    if self.node_mode.is_some()
      && (self.node_mode != self.selected || self.group_selection.len() >= 2)
    {
      self.node_mode = None;
      self.node_indices.clear();
      self.selected_node = None;
    }
    self
      .group_selection
      .retain(|id| self.annotations.iter().any(|a| a.id == *id));
    if let Some(drawing) = self
      .annotations
      .iter()
      .find(|drawing| Some(drawing.id) == self.selected)
    {
      self.width = drawing.width;
      if let Some(Label::Text(data)) = &drawing.label {
        self.text_size = data.size;
        self.text_appearance = data.appearance;
      }
      if let Some(color) = COLORS.iter().position(|color| *color == drawing.color) {
        self.color = color;
      }
    }
    if self
      .selected
      .is_some_and(|id| !self.annotations.iter().any(|drawing| drawing.id == id))
    {
      self.selected = None;
    }
  }

  pub(super) fn toggle_selection(&mut self, id: u64) {
    if self.group_selection.is_empty()
      && let Some(selected) = self.selected
    {
      self.group_selection.push(selected);
    }
    if let Some(index) = self
      .group_selection
      .iter()
      .position(|selected| *selected == id)
    {
      self.group_selection.remove(index);
    } else {
      self.group_selection.push(id);
    }
    self.selected = self.group_selection.last().copied();
  }

  /// Whether the active drawing tool selects, moves and reshapes its own kind of drawing:
  /// the arrow tool edits arrows, the rectangle tool rectangles. Text keeps its own
  /// editing flow, and Select edits every drawing.
  pub(super) fn edits_own_drawings(&self) -> bool {
    !self.object_mode
      && matches!(
        self.tool,
        Tool::Pen
          | Tool::Highlighter
          | Tool::Arrow
          | Tool::Rectangle
          | Tool::Ellipse
          | Tool::Blur
          | Tool::Marker
      )
  }

  /// Whether the active tool can select, move and reshape this drawing.
  pub(super) fn editable(&self, drawing: &Annotation) -> bool {
    self.object_mode || self.edits_own_drawings() && drawing.tool == self.tool
  }

  /// The topmost drawing of the active drawing tool's own kind under the pointer.
  pub(super) fn hit_own_drawing(&self, pos: (f64, f64)) -> Option<usize> {
    if !self.edits_own_drawings() {
      return None;
    }
    self
      .annotations
      .iter()
      .rposition(|drawing| drawing.tool == self.tool && drawing.hit(pos))
  }

  pub(super) fn hit_drawing(&self, pos: (f64, f64)) -> Option<usize> {
    self.annotations.iter().rposition(|drawing| {
      drawing.hit(pos)
        || self.object_mode
          && Some(drawing.id) == self.selected
          && crate::screenshot::shape::contains(drawing, pos)
    })
  }

  pub(super) fn geometry_handle_hit(&self, position: (f64, f64)) -> Option<(usize, usize)> {
    if self.group_selection.len() >= 2 || self.text_edit.is_some() {
      return None;
    }
    let index = self
      .annotations
      .iter()
      .position(|a| Some(a.id) == self.selected)?;
    let drawing = &self.annotations[index];
    if !self.editable(drawing) {
      return None;
    }
    if drawing.tool.freehand() && self.node_mode == Some(drawing.id) {
      return self
        .node_indices
        .iter()
        .copied()
        .filter_map(|i| {
          drawing.points.get(i).map(|&(x, y)| {
            let p = crate::screenshot::path::world(drawing, (x as f64, y as f64));
            (i, (position.0 - p.0).hypot(position.1 - p.1))
          })
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, distance)| *distance <= 9.0)
        .map(|(i, _)| (index, 16 + i));
    }
    if let Some(point) = crate::screenshot::shape::rotation_handle(drawing)
      && (position.0 - point.0).hypot(position.1 - point.1) <= 9.0
    {
      return Some((index, 9));
    }
    if let Some(point) = crate::screenshot::shape::radius_handle(drawing)
      && (position.0 - point.0).hypot(position.1 - point.1) <= 7.0
    {
      return Some((index, 8));
    }
    let handles: Vec<_> =
      if let Some(arrow) = crate::screenshot::arrow::Arrow::from_annotation(drawing) {
        if !drawing.curve_controls.is_empty() {
          drawing
            .points
            .iter()
            .enumerate()
            .map(|(i, &(x, y))| (i, (x as f64, y as f64)))
            .collect()
        } else {
          let points = arrow.handles();
          [0, 2, 1].into_iter().map(|i| (i, points[i])).collect()
        }
      } else {
        let points = crate::screenshot::shape::handles(drawing)?;
        // Prioritize corners when handles overlap on small shapes.
        [0, 2, 4, 6, 1, 3, 5, 7]
          .into_iter()
          .map(|i| (i, points[i]))
          .collect()
      };
    if !drawing.curve_controls.is_empty() {
      return handles
        .into_iter()
        .map(|(i, p)| (i, (position.0 - p.0).hypot(position.1 - p.1)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, distance)| *distance <= 9.0)
        .map(|(i, _)| (index, i));
    }
    handles
      .into_iter()
      .find(|(_, point)| (position.0 - point.0).hypot(position.1 - point.1) <= 9.0)
      .map(|(handle, _)| (index, handle))
  }

  pub(super) fn path_click(&mut self, pos: (f64, f64)) -> bool {
    let Some(index) = self
      .annotations
      .iter()
      .rposition(|a| matches!(a.tool, Tool::Pen | Tool::Highlighter | Tool::Arrow) && a.hit(pos))
    else {
      self.last_path_click = None;
      return false;
    };
    let drawing = &self.annotations[index];
    let id = drawing.id;
    // SAFETY: read-only Windows double-click interval; input remains on the overlay thread.
    let interval =
      unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() } as u128;
    let double = self.last_path_click.is_some_and(|(old, time, p)| {
      old == id
        && time.elapsed().as_millis() <= interval
        && (p.0 - pos.0).abs() <= 4.0
        && (p.1 - pos.1).abs() <= 4.0
    });
    self.last_path_click = Some((id, std::time::Instant::now(), pos));
    if !double {
      return false;
    }
    self.last_path_click = None;
    self.selected = Some(id);
    self.group_selection = vec![id];
    if drawing.tool.freehand() && self.node_mode != Some(id) {
      self.node_mode = Some(id);
      self.node_indices = crate::screenshot::path::nodes(drawing);
      self.selected_node = None;
      return true;
    }
    if drawing.tool.freehand()
      && (self.node_indices.len() >= crate::screenshot::path::MAX_NODES
        || drawing.points.len() >= crate::screenshot::annotations::MAX_POINTS)
      || drawing.tool == Tool::Arrow && drawing.points.len() >= crate::screenshot::path::MAX_NODES
    {
      self.limited = Some(DrawingLimit::Stroke);
      return true;
    }
    let before = drawing.clone();
    let result = if before.tool.freehand() {
      crate::screenshot::path::insert_node(&before, pos)
    } else {
      crate::screenshot::arrow::insert_node(&before, pos)
    };
    match result {
      Ok((mut after, node)) => {
        if before.tool.freehand() {
          after.pen_nodes = self
            .node_indices
            .iter()
            .map(|i| if *i >= node { i + 1 } else { *i })
            .collect();
          after.pen_nodes.push(node);
          after.pen_nodes.sort_unstable();
          after.pen_nodes.dedup();
        }
        if self.apply_change(history::Change {
          index,
          before: Some(before),
          after: Some(after),
          counter: None,
        }) {
          self.selected_node = Some(node);
          if self.annotations[index].tool.freehand() {
            self.node_mode = Some(id);
            self.node_indices = crate::screenshot::path::nodes(&self.annotations[index]);
            if !self.node_indices.contains(&node) {
              self.node_indices.push(node);
              self.node_indices.sort_unstable();
            }
          }
        }
      },
      Err(error) => {
        tracing::warn!(%error,"Could not insert path node");
        self.report_render_failure();
      },
    }
    true
  }

  pub(super) fn delete_path_node(&mut self) -> bool {
    let Some(node) = self.selected_node else {
      return false;
    };
    let Some(index) = self
      .annotations
      .iter()
      .position(|a| Some(a.id) == self.selected)
    else {
      return false;
    };
    let before = self.annotations[index].clone();
    if !matches!(before.tool, Tool::Pen | Tool::Highlighter | Tool::Arrow) {
      return false;
    }
    // Selected endpoint nodes cannot be removed; Delete must not remove the whole path.
    if before.points.len() <= 2 || node >= before.points.len() {
      return true;
    }
    let mut after = before.clone();
    if before.tool.freehand() {
      let prev = self
        .node_indices
        .iter()
        .copied()
        .rfind(|i| *i < node)
        .unwrap_or(0);
      let next = self
        .node_indices
        .iter()
        .copied()
        .find(|i| *i > node)
        .unwrap_or(before.points.len() - 1);
      let range = if node == 0 {
        0..next
      } else if node == before.points.len() - 1 {
        prev + 1..before.points.len()
      } else {
        prev + 1..next
      };
      if before.points.len() - range.len() < 2 {
        return true;
      }
      let count = range.len();
      after.points.drain(range.clone());
      after.pen_nodes = self
        .node_indices
        .iter()
        .copied()
        .filter(|i| !range.contains(i))
        .map(|i| if i >= range.end { i - count } else { i })
        .collect();
    } else {
      after.points.remove(node);
      if !after.curve_controls.is_empty() {
        after.curve_controls.remove(node);
      }
    }
    if self.apply_change(history::Change {
      index,
      before: Some(before),
      after: Some(after),
      counter: None,
    }) {
      self.selected_node = None;
      if self.annotations[index].tool.freehand() {
        self.node_indices = crate::screenshot::path::nodes(&self.annotations[index]);
      }
    }
    true
  }

  pub(super) fn edit_selected_style(&mut self) {
    if self.text_edit.is_some() {
      return;
    }
    if let Some(index) = self
      .annotations
      .iter()
      .position(|drawing| Some(drawing.id) == self.selected)
    {
      let before = self.annotations[index].clone();
      let after = Annotation {
        width: self.width,
        color: COLORS[self.color],
        ..before.clone()
      };
      let mut after = after;
      if let Some(Label::Text(data)) = &mut after.label {
        match text_data(data.content.clone(), self.text_size, data.wrap_width) {
          Ok(mut updated) => {
            updated.appearance = self.text_appearance;
            *data = updated;
          },
          Err(error) => {
            tracing::error!(%error,"Failed to change annotation font size");
            self.report_render_failure();
            return;
          },
        }
      }
      self.apply_change(history::Change {
        counter: None,
        index,
        before: Some(before),
        after: Some(after),
      });
    }
  }

  pub(super) fn update_move(&mut self, controller: &SelectionControllerState) {
    for moving in &mut self.group_move {
      let dx = controller.cursor_pos.0.round() as i32 - moving.start.0;
      let dy = controller.cursor_pos.1.round() as i32 - moving.start.1;
      match moving.before.translated(dx, dy) {
        Ok(preview) => moving.preview = preview,
        Err(error) => {
          tracing::error!(%error, "Failed to move grouped drawing");
          self.render_failed = true;
          self.error_button = Some(12);
        },
      }
    }
    if let Some(moving) = &mut self.moving {
      if let Some(handle) = self.geometry_handle {
        let position = (
          controller
            .cursor_pos
            .0
            .round()
            .clamp(0.0, controller.width.saturating_sub(1).max(0) as f64) as i32,
          controller
            .cursor_pos
            .1
            .round()
            .clamp(0.0, controller.height.saturating_sub(1).max(0) as f64) as i32,
        );
        let preview = if moving.before.tool.freehand() {
          let result = if handle >= 16 {
            crate::screenshot::path::edit_node(
              &moving.before,
              &self.node_indices,
              handle - 16,
              position,
            )
          } else {
            crate::screenshot::path::edit_transform(
              &moving.before,
              handle,
              position,
              controller.modifiers.shift,
            )
          };
          match result {
            Ok(preview) => Some(preview),
            Err(error) => {
              tracing::error!(%error,"Failed to edit pen path");
              self.render_failed = true;
              self.error_button = Some(12);
              None
            },
          }
        } else if moving.before.tool == Tool::Arrow {
          crate::screenshot::arrow::edit_handle(&moving.before, handle, position)
        } else {
          crate::screenshot::shape::edit_handle(
            &moving.before,
            handle,
            position,
            controller.modifiers.shift,
            (controller.width, controller.height),
          )
        };
        if let Some(preview) = preview {
          moving.preview = preview;
        }
        return;
      }
      let dx = controller.cursor_pos.0.round() as i32 - moving.start.0;
      let dy = controller.cursor_pos.1.round() as i32 - moving.start.1;
      if self.resizing_text {
        let angle = (moving.before.rotation_millidegrees as f64 / 1000.0).to_radians();
        let dy = controller.cursor_pos.1 - moving.start.1 as f64;
        let dx = (dx as f64 * angle.cos() + dy * angle.sin()).round() as i32;
        if let Some(Label::Text(data)) = &moving.before.label {
          match text_data(
            data.content.clone(),
            data.size,
            (data.wrap_width + dx).clamp(64.max(data.size * 2), 8192),
          ) {
            Ok(mut updated) => {
              updated.appearance = data.appearance;
              moving.preview.points = moving.before.points.clone();
              moving.preview.label = Some(Label::Text(updated));
              if moving.before.rotation_millidegrees != 0 {
                let point = moving.before.points[0];
                let old = crate::screenshot::shape::rotate(
                  &moving.before,
                  (point.0 as f64, point.1 as f64),
                  false,
                );
                let new = crate::screenshot::shape::rotate(
                  &moving.preview,
                  (point.0 as f64, point.1 as f64),
                  false,
                );
                moving.preview.points[0] = (
                  point.0.saturating_add((old.0 - new.0).round() as i32),
                  point.1.saturating_add((old.1 - new.1).round() as i32),
                );
              }
            },
            Err(error) => {
              tracing::warn!(%error, "Failed to resize annotation text");
              self.report_render_failure();
            },
          }
        }
        return;
      }
      match moving.before.translated(dx, dy) {
        Ok(preview) => moving.preview = preview,
        Err(error) => {
          tracing::error!(%error, "Failed to move drawing");
          self.report_render_failure();
        },
      }
    }
  }

  pub(super) fn delete_selected(&mut self) {
    if self.group_selection.len() > 1 {
      let changes = self
        .annotations
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, a)| self.group_selection.contains(&a.id))
        .map(|(index, a)| history::Change {
          counter: None,
          index,
          before: Some(a.clone()),
          after: None,
        })
        .collect();
      match self.history.commit_batch(&mut self.annotations, changes) {
        Ok(true) => {
          self.document_changed();
          self.group_selection.clear();
          self.selected = None;
        },
        Ok(false) => {},
        Err(error) => {
          tracing::error!(%error, "Failed to delete selected drawings");
          self.report_render_failure();
        },
      }
      return;
    }
    if let Some(index) = self
      .annotations
      .iter()
      .position(|drawing| Some(drawing.id) == self.selected)
      && self.apply_change(history::Change {
        counter: None,
        index,
        before: Some(self.annotations[index].clone()),
        after: None,
      })
    {
      self.selected = None;
    }
  }

  pub(super) fn update_draft(&mut self, controller: &SelectionControllerState) {
    let Some(rect) = Self::confirmed(controller) else {
      return;
    };
    let Some(draft) = &mut self.draft else {
      return;
    };
    let mut point = (
      (controller.cursor_pos.0.round() as i32).clamp(rect.x as i32, (rect.x + rect.w - 1.0) as i32),
      (controller.cursor_pos.1.round() as i32).clamp(rect.y as i32, (rect.y + rect.h - 1.0) as i32),
    );
    if controller.modifiers.shift
      && let Some(&start) = draft.points.first()
    {
      point = constrained_endpoint(
        draft.tool,
        start,
        (
          controller.cursor_pos.0.round() as i32,
          controller.cursor_pos.1.round() as i32,
        ),
        rect,
      );
    }
    if draft.tool.freehand() {
      if !pen::append(&mut draft.points, point, &mut self.pen_compactions) {
        if !matches!(self.limited, Some(DrawingLimit::Stroke)) {
          tracing::warn!("Screenshot annotation point limit reached after bounded compaction");
        }
        self.limited = Some(DrawingLimit::Stroke);
      }
    } else if let Some(end) = draft.points.last_mut() {
      *end = point;
    }
  }
}

pub(super) fn shortcut_tool(vk: u32) -> Option<Tool> {
  match vk {
    0x56 => Some(Tool::Select),
    0x50 => Some(Tool::Pen),
    0x48 => Some(Tool::Highlighter),
    0x41 => Some(Tool::Arrow),
    0x52 => Some(Tool::Rectangle),
    0x45 => Some(Tool::Ellipse),
    0x42 => Some(Tool::Blur),
    0x54 => Some(Tool::Text),
    0x4e => Some(Tool::Marker),
    _ => None,
  }
}

pub(super) fn constrained_endpoint(
  tool: Tool,
  start: (i32, i32),
  raw: (i32, i32),
  rect: Rect,
) -> (i32, i32) {
  let free = (
    raw.0.clamp(rect.x as i32, (rect.x + rect.w - 1.0) as i32),
    raw.1.clamp(rect.y as i32, (rect.y + rect.h - 1.0) as i32),
  );
  if !matches!(
    tool,
    Tool::Arrow | Tool::Rectangle | Tool::Ellipse | Tool::Blur
  ) {
    return free;
  }
  let (dx, dy) = (raw.0 as f64 - start.0 as f64, raw.1 as f64 - start.1 as f64);
  let (sx, sy) = (if dx < 0.0 { -1 } else { 1 }, if dy < 0.0 { -1 } else { 1 });
  let available = |position: i32, sign: i32, low: i32, high: i32| {
    if sign < 0 {
      position - low
    } else {
      high - position
    }
  };
  let (max_x, max_y) = (
    available(start.0, sx, rect.x as i32, (rect.x + rect.w - 1.0) as i32).max(0),
    available(start.1, sy, rect.y as i32, (rect.y + rect.h - 1.0) as i32).max(0),
  );
  if tool != Tool::Arrow {
    let side = dx.abs().max(dy.abs()).round().min(max_x.min(max_y) as f64) as i32;
    return (start.0 + sx * side, start.1 + sy * side);
  }
  let step = std::f64::consts::FRAC_PI_4;
  let direction = (dy.atan2(dx) / step).round() * step;
  let (cx, cy) = (direction.cos(), direction.sin());
  let length = dx.hypot(dy);
  let limit_x = if cx.abs() < 0.001 {
    f64::INFINITY
  } else {
    max_x as f64 / cx.abs()
  };
  let limit_y = if cy.abs() < 0.001 {
    f64::INFINITY
  } else {
    max_y as f64 / cy.abs()
  };
  let length = length.min(limit_x).min(limit_y);
  (
    start.0 + (cx * length).round() as i32,
    start.1 + (cy * length).round() as i32,
  )
}
