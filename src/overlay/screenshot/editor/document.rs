//! Completed gestures, bounded history and crop synchronization.

use super::{AnnotationEditor, DrawingLimit};
use crate::overlay::screenshot::history;
use crate::overlay::selection::{Rect, SelectionInteraction};
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::Tool;

impl AnnotationEditor {
  pub fn cancel_gesture(&mut self) {
    if let Some((start, _)) = self.toolbar_drag.take() {
      self.toolbar_position = Some((start.left, start.top));
    }
    self.group_move.clear();
    if let Some(edit) = &mut self.text_edit
      && let Some((_, point)) = edit.moving.take()
    {
      edit.point = point;
    }
    if let Some(edit) = &mut self.text_edit
      && let Some((_, width)) = edit.resizing.take()
    {
      edit.wrap_width = width;
    }
    self.resizing_text = false;
    self.geometry_handle = None;

    self.moving = None;
    self.draft = None;
    self.toolbar_press = false;
  }

  pub fn finish_gesture(&mut self) {
    self.resizing_text = false;
    self.geometry_handle = None;
    if let Some(moving) = self.moving.take() {
      if moving.before != moving.preview {
        self.last_text_click = None;
      }
      let mut changes = vec![history::Change {
        counter: None,
        index: moving.index,
        before: Some(moving.before),
        after: Some(moving.preview),
      }];
      changes.extend(self.group_move.drain(..).map(|m| history::Change {
        counter: None,
        index: m.index,
        before: Some(m.before),
        after: Some(m.preview),
      }));
      match self.history.commit_batch(&mut self.annotations, changes) {
        Ok(true) => self.document_changed(),
        Ok(false) => {},
        Err(error) => {
          tracing::error!(%error, "Failed to move selected drawings");
          self.report_render_failure();
        },
      }
    }
    if let Some(mut draft) = self.draft.take() {
      let first = draft.points.first();
      let last = draft.points.last();
      let valid = match (draft.tool, first, last) {
        (Tool::Pen | Tool::Highlighter, Some(_), Some(_)) => true,
        (Tool::Arrow, Some(a), Some(b)) => a != b,
        (Tool::Rectangle | Tool::Ellipse | Tool::Blur, Some(a), Some(b)) => {
          a.0 != b.0 && a.1 != b.1
        },
        _ => false,
      };
      if valid {
        let Some(next) = self.next_id.checked_add(1) else {
          self.limited = Some(DrawingLimit::Identifier);
          tracing::error!("Screenshot annotation identifier limit reached");
          return;
        };
        draft.id = self.next_id;
        let id = draft.id;
        let change = history::Change {
          counter: None,
          index: self.annotations.len(),
          before: None,
          after: Some(draft),
        };
        if self.apply_change(change) {
          self.next_id = next;
          // A new drawing stays selected, ready to adjust with the same tool.
          self.selected = Some(id);
        }
      }
    }
  }

  pub(super) fn apply_change(&mut self, change: history::Change) -> bool {
    match self.history.commit(&mut self.annotations, change) {
      Ok(changed) => {
        if changed {
          self.document_changed();
        }
        changed
      },
      Err(error) => {
        tracing::error!(%error, "Failed to edit screenshot annotation");
        self.report_render_failure();
        false
      },
    }
  }

  pub(super) fn undo(&mut self) {
    match self.history.undo(&mut self.annotations) {
      Ok(true) => self.document_changed(),
      Ok(false) => {},
      Err(error) => {
        tracing::error!(%error, "Failed to undo screenshot annotation");
        self.report_render_failure();
        self.error_button = Some(5);
      },
    }
    self.reconcile_selection();
    self.limited = None;
    self.refresh_nodes();
  }

  pub(super) fn redo(&mut self) {
    match self.history.redo(&mut self.annotations) {
      Ok(true) => self.document_changed(),
      Ok(false) => {},
      Err(error) => {
        tracing::error!(%error, "Failed to redo screenshot annotation");
        self.report_render_failure();
        self.error_button = Some(6);
      },
    }
    self.reconcile_selection();
    self.refresh_nodes();
  }

  pub(super) fn confirmed(controller: &SelectionControllerState) -> Option<Rect> {
    match controller.interaction {
      SelectionInteraction::Confirmed {
        rect,
        moving: None,
        resizing: None,
        ..
      } => Some(rect),
      _ => None,
    }
  }

  pub fn sync_crop(&mut self, controller: &mut SelectionControllerState) {
    if !controller.interaction.is_confirmed_idle() {
      self
        .motion
        .update(false, 0, None, std::time::Instant::now());
    }
    if let Some(crop) = self.history.pending_crop.take() {
      if let SelectionInteraction::Confirmed {
        rect,
        moving: None,
        resizing: None,
        ..
      } = &mut controller.interaction
      {
        if rect.to_x1y1x2y2() != crop.to_x1y1x2y2() {
          self.toolbar_position = None;
        }
        *rect = crop;
        self.crop_anchor = Some(crop);
      } else {
        tracing::error!("Cannot restore crop during an active selection gesture");
        self.report_render_failure();
      }
    }
    if let SelectionInteraction::Confirmed { rect, .. } = controller.interaction
      && self
        .crop_anchor
        .is_some_and(|before| before.to_x1y1x2y2() != rect.to_x1y1x2y2())
    {
      // A manual toolbar placement belongs to the previous crop, not the desktop.
      self.toolbar_position = None;
    }
    if let Some(crop) = Self::confirmed(controller) {
      if let Some(before) = self.crop_anchor
        && let Err(error) = self.history.commit_crop(before, crop)
      {
        tracing::error!(%error, "Failed to record crop adjustment");
        self.report_render_failure();
      }
      self.crop_anchor = Some(crop);
    }
  }

  pub(super) fn document_changed(&mut self) {
    self.document_revision = self.document_revision.wrapping_add(1);
    self.clear_toolbar_error();
  }
}
