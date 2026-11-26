//! Native cursor selection for drawing tools, handles and toolbar controls.

use super::AnnotationEditor;
use super::layout::{contains, error_badge_rect, toolbar_grip};
use crate::overlay::screenshot::cursors;
use crate::overlay::selection::{HitResult, hit_test};
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::Tool;
use anyhow::{Context, Result};

impl AnnotationEditor {
  pub fn tool_cursor(
    &mut self,
    controller: &SelectionControllerState,
  ) -> Result<Option<windows::Win32::UI::WindowsAndMessaging::HCURSOR>> {
    let inside = Self::confirmed(controller)
      .is_some_and(|rect| matches!(hit_test(&rect, controller.cursor_pos), HitResult::Inside));
    let shape_handle = self
      .moving
      .as_ref()
      .and_then(|moving| crate::screenshot::shape::bounds(&moving.before).and(self.geometry_handle))
      .or_else(|| {
        self
          .geometry_handle_hit(controller.cursor_pos)
          .and_then(|(index, handle)| {
            crate::screenshot::shape::bounds(&self.annotations[index]).map(|_| handle)
          })
      });
    if let Some(handle) = shape_handle.filter(|_| {
      !self.toolbar_cursor(controller) || self.geometry_handle == Some(9) && self.moving.is_some()
    }) {
      if handle >= 16 {
        // SAFETY: Windows owns the shared precision cursor used for path nodes.
        return unsafe {
          windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
            None,
            windows::Win32::UI::WindowsAndMessaging::IDC_CROSS,
          )
        }
        .map(Some)
        .map_err(Into::into);
      }
      if handle == 9 {
        // Rotating grabs the drawing: open hand on hover, closed while turning.
        return self.hand(self.moving.is_some());
      }
      use windows::Win32::UI::WindowsAndMessaging::{
        IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, LoadCursorW,
      };
      let rotation = self
        .moving
        .as_ref()
        .map(|m| m.preview.rotation_millidegrees)
        .or_else(|| {
          self
            .annotations
            .iter()
            .find(|a| Some(a.id) == self.selected)
            .map(|a| a.rotation_millidegrees)
        })
        .unwrap_or(0);
      let cursor = match crate::screenshot::shape::cursor_axis(handle, rotation) {
        0 => IDC_SIZEWE,
        1 => IDC_SIZENWSE,
        2 => IDC_SIZENS,
        _ => IDC_SIZENESW,
      };
      // SAFETY: Windows owns the borrowed system cursor.
      return unsafe { LoadCursorW(None, cursor) }
        .map(Some)
        .map_err(Into::into);
    }
    if self
      .text_edit
      .as_ref()
      .is_some_and(|edit| edit.moving.is_some())
      || inside
        && self
          .text_selection_bounds()
          .is_some_and(|bounds| self.text_border_hit(bounds, controller.cursor_pos))
        && !self.toolbar_cursor(controller)
        && !self.text_resize_hit(controller.cursor_pos)
    {
      let dragging = self
        .text_edit
        .as_ref()
        .is_some_and(|edit| edit.moving.is_some());
      return self.hand(dragging);
    }
    if self.resizing_text
      || self
        .text_edit
        .as_ref()
        .is_some_and(|edit| edit.resizing.is_some())
      || self.group_selection.len() < 2
        && self.text_resize_hit(controller.cursor_pos)
        && !self.toolbar_cursor(controller)
    {
      // SAFETY: system cursor is borrowed; Windows owns its lifetime.
      return unsafe {
        windows::Win32::UI::WindowsAndMessaging::LoadCursorW(None, {
          use windows::Win32::UI::WindowsAndMessaging::{
            IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
          };
          let rotation = if self.text_edit.is_some() {
            0
          } else {
            self.selected_text().map_or(0, |a| a.rotation_millidegrees)
          };
          match crate::screenshot::shape::cursor_axis(3, rotation) {
            0 => IDC_SIZEWE,
            1 => IDC_SIZENWSE,
            2 => IDC_SIZENS,
            _ => IDC_SIZENESW,
          }
        })
      }
      .map(Some)
      .map_err(Into::into);
    }
    if self.moving.is_some()
      || self.geometry_handle_hit(controller.cursor_pos).is_some()
        && !self.toolbar_cursor(controller)
      || self.tool == Tool::Text
        && inside
        && self
          .hit_drawing(controller.cursor_pos)
          .is_some_and(|index| self.annotations[index].tool == Tool::Text)
        && !self.toolbar_cursor(controller)
    {
      return self.hand(self.moving.is_some());
    }
    if inside
      && !self.toolbar_cursor(controller)
      && self.hit_own_drawing(controller.cursor_pos).is_some()
    {
      // The active tool can grab its own kind of drawing.
      return self.hand(false);
    }
    if matches!(self.tool, Tool::Text | Tool::Marker) && self.drawing_cursor(controller) {
      use windows::Win32::UI::WindowsAndMessaging::{IDC_CROSS, IDC_IBEAM, LoadCursorW};
      // SAFETY: shared system cursors remain owned by Windows.
      return unsafe {
        LoadCursorW(
          None,
          if self.tool == Tool::Text {
            IDC_IBEAM
          } else {
            IDC_CROSS
          },
        )
      }
      .map(Some)
      .map_err(Into::into);
    }
    if self.object_mode
      && (self.moving.is_some()
        || Self::confirmed(controller)
          .is_some_and(|rect| matches!(hit_test(&rect, controller.cursor_pos), HitResult::Inside)))
    {
      if self.moving.is_some() || self.hit_drawing(controller.cursor_pos).is_some() {
        return self.hand(self.moving.is_some());
      }
      return cursors::system(windows::Win32::UI::WindowsAndMessaging::IDC_ARROW).map(Some);
    }
    if !self.drawing_cursor(controller) {
      // Outside the crop, every tool starts a new selection, so the selection cursor applies.
      if Self::confirmed(controller).is_some_and(|rect| {
        !matches!(hit_test(&rect, controller.cursor_pos), HitResult::Outside)
          || self.toolbar_cursor(controller)
      }) && (self.object_mode || self.tool != Tool::Select)
      {
        // SAFETY: borrowed Windows cursor; crop handles are inactive outside the crop tool.
        return unsafe {
          windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
            None,
            windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
          )
        }
        .map(Some)
        .map_err(Into::into);
      }
      return Ok(None);
    }
    match self.tool {
      // Drawing tools use the Windows crosshair, which follows the user's pointer settings.
      Tool::Pen
      | Tool::Highlighter
      | Tool::Arrow
      | Tool::Rectangle
      | Tool::Ellipse
      | Tool::Blur => cursors::system(windows::Win32::UI::WindowsAndMessaging::IDC_CROSS).map(Some),
      Tool::Select | Tool::Text | Tool::Marker => Ok(None),
    }
  }

  /// Chromium's grab hand, closed while a drawing is being dragged.
  fn hand(
    &mut self,
    closed: bool,
  ) -> Result<Option<windows::Win32::UI::WindowsAndMessaging::HCURSOR>> {
    let cursors = self.owned_cursors()?;
    Ok(Some(if closed {
      cursors.closed_hand()
    } else {
      cursors.open_hand()
    }))
  }

  /// Over the toolbar's grip: the open hand, closed while the toolbar is being dragged.
  pub fn grip_hand(&mut self) -> Result<windows::Win32::UI::WindowsAndMessaging::HCURSOR> {
    let closed = self.toolbar_drag.is_some();
    let cursors = self.owned_cursors()?;
    Ok(if closed {
      cursors.closed_hand()
    } else {
      cursors.open_hand()
    })
  }

  fn owned_cursors(&mut self) -> Result<&mut cursors::ToolCursors> {
    if self.cursors.is_none() {
      self.cursors = Some(cursors::ToolCursors::new()?);
    }
    self
      .cursors
      .as_mut()
      .context("Annotation cursors are unavailable")
  }

  fn drawing_cursor(&self, controller: &SelectionControllerState) -> bool {
    self.draft.is_some()
      || Self::confirmed(controller).is_some_and(|rect| {
        self.tool != Tool::Select
          && matches!(hit_test(&rect, controller.cursor_pos), HitResult::Inside)
          && !contains(self.toolbar_rect(controller, rect), controller.cursor_pos)
      })
  }

  pub fn grip_cursor(&self, controller: &SelectionControllerState) -> bool {
    self.toolbar_drag.is_some()
      || Self::confirmed(controller).is_some_and(|rect| {
        contains(
          toolbar_grip(
            self.toolbar_rect(controller, rect),
            self.metrics(controller, rect),
          ),
          controller.cursor_pos,
        )
      })
  }

  pub fn toolbar_cursor(&self, controller: &SelectionControllerState) -> bool {
    if self.moving.is_some() || self.draft.is_some() {
      return false;
    }
    Self::confirmed(controller).is_some_and(|rect| {
      contains(self.toolbar_rect(controller, rect), controller.cursor_pos)
        || self.toolbar_error().is_some_and(|(_, button)| {
          contains(
            error_badge_rect(
              self.toolbar_rect(controller, rect),
              button,
              self.metrics(controller, rect),
            ),
            controller.cursor_pos,
          )
        })
        || ((self.width_open || self.color_open)
          && contains(self.picker_rect(controller, rect), controller.cursor_pos))
    })
  }
}
