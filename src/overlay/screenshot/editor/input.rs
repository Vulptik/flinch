//! Pointer and keyboard dispatch on the selection window owner thread.

use super::layout::{
  appearance_cell, color_cell, combine_bounds, contains, error_badge_rect, font_cell, invalidate,
  invalidate_region, style_menu_button, toolbar_bounds, toolbar_grip, toolbar_index, width_row,
};
use super::objects::shortcut_tool;
use super::{AnnotationEditor, COLORS, DrawingLimit, FONT_SIZES, ObjectMove, TOOLS};
use crate::overlay::selection::{HitResult, hit_test};
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::{Annotation, MAX_ANNOTATIONS, TextAppearance, Tool};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
  GetCapture, GetKeyState, ReleaseCapture, SetCapture,
};
use windows::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};

impl AnnotationEditor {
  pub fn pointer_move(
    &mut self,
    controller: &mut SelectionControllerState,
    hwnd: HWND,
    lparam: LPARAM,
  ) -> bool {
    if let Some((start, offset)) = self.toolbar_drag {
      controller.cursor_pos = (
        (lparam.0 & 0xffff) as i16 as f64,
        ((lparam.0 >> 16) & 0xffff) as i16 as f64,
      );
      if let Some(rect) = Self::confirmed(controller) {
        let bounds = toolbar_bounds(controller, rect, &self.monitors);
        let left = (controller.cursor_pos.0.round() as i32 - offset.0).clamp(
          bounds.left,
          (bounds.right - (start.right - start.left)).max(bounds.left),
        );
        let top = (controller.cursor_pos.1.round() as i32 - offset.1).clamp(
          bounds.top,
          (bounds.bottom - (start.bottom - start.top)).max(bounds.top),
        );
        self.toolbar_position = Some((left, top));
      }
      // Windows sends no WM_SETCURSOR while the overlay holds capture, so the closed hand is
      // applied on every move of the drag.
      match self.grip_hand() {
        // SAFETY: the editor owns this live cursor on the overlay thread.
        Ok(cursor) => unsafe {
          windows::Win32::UI::WindowsAndMessaging::SetCursor(Some(cursor));
        },
        Err(error) => tracing::warn!(%error, "Cannot load the toolbar grip hand"),
      }
      invalidate(hwnd);
      return true;
    }
    if self
      .text_edit
      .as_ref()
      .is_some_and(|edit| edit.moving.is_some())
    {
      controller.cursor_pos = (
        (lparam.0 & 0xffff) as i16 as f64,
        ((lparam.0 >> 16) & 0xffff) as i16 as f64,
      );
      if let Some(edit) = &mut self.text_edit
        && let Some((start, point)) = edit.moving
      {
        edit.point = (
          point.0 + controller.cursor_pos.0.round() as i32 - start.0,
          point.1 + controller.cursor_pos.1.round() as i32 - start.1,
        );
      }
      self.text_changed(controller, hwnd, None);
      return true;
    }
    if self
      .text_edit
      .as_ref()
      .is_some_and(|edit| edit.resizing.is_some())
    {
      controller.cursor_pos = (
        (lparam.0 & 0xffff) as i16 as f64,
        ((lparam.0 >> 16) & 0xffff) as i16 as f64,
      );
      if let Some(edit) = &mut self.text_edit
        && let Some((start, width)) = edit.resizing
      {
        edit.wrap_width = (width + controller.cursor_pos.0.round() as i32 - start)
          .clamp(64.max(self.text_size * 2), 8192);
      }
      self.text_changed(controller, hwnd, None);
      return true;
    }
    if self.draft.is_none()
      && self.moving.is_none()
      && !self.toolbar_press
      && Self::confirmed(controller).is_some()
    {
      controller.cursor_pos = (
        (lparam.0 & 0xffff) as i16 as f64,
        ((lparam.0 >> 16) & 0xffff) as i16 as f64,
      );
      if self.hover_changed(controller)
        && let Some(rect) = Self::confirmed(controller)
      {
        invalidate_region(hwnd, self.chrome_region(controller, rect));
        // Reaching Redact All Text starts detection, so the click usually finds it done.
        if self.blur_mode() && self.hover_key == Some(11) {
          self.prefetch_redaction(controller, hwnd);
        }
      }
      return true;
    }
    if self.draft.is_none() && self.moving.is_none() && !self.toolbar_press {
      return false;
    }
    controller.cursor_pos = (
      (lparam.0 & 0xffff) as i16 as f64,
      ((lparam.0 >> 16) & 0xffff) as i16 as f64,
    );
    if self.hover_changed(controller)
      && let Some(rect) = Self::confirmed(controller)
    {
      invalidate_region(hwnd, self.chrome_region(controller, rect));
    }
    let old = self.active_bounds();
    self.update_draft(controller);
    self.update_move(controller);
    if let Some(damage) = combine_bounds(old, self.active_bounds()) {
      invalidate_region(hwnd, damage);
    } else if let Some(rect) = Self::confirmed(controller) {
      invalidate_region(hwnd, self.chrome_region(controller, rect));
    }
    true
  }

  pub fn pointer_down(&mut self, controller: &SelectionControllerState, hwnd: HWND) -> bool {
    self.pointer_down_with(controller, hwnd, |hwnd| {
      // SAFETY: pointer capture is confined to this overlay owner thread.
      unsafe {
        SetCapture(hwnd);
        GetCapture() == hwnd
      }
    })
  }

  fn pointer_down_with(
    &mut self,
    controller: &SelectionControllerState,
    hwnd: HWND,
    capture: impl Fn(HWND) -> bool,
  ) -> bool {
    if !self.object_mode || self.node_mode.is_some() && self.node_mode != self.selected {
      self.node_mode = None;
      self.node_indices.clear();
      self.selected_node = None;
    }
    let Some(rect) = Self::confirmed(controller) else {
      return false;
    };
    if self.draft.is_some() || self.moving.is_some() || self.toolbar_press {
      return true;
    }
    let pos = controller.cursor_pos;
    let toolbar = self.toolbar_rect(controller, rect);
    let m = self.metrics(controller, rect);
    if self.toolbar_error().is_some_and(|(_, button)| {
      toolbar_index(toolbar, pos, m) == Some(button)
        || contains(error_badge_rect(toolbar, button, m), pos)
    }) {
      self.error_details_pinned = !self.error_details_pinned;
      self.toolbar_press = true;
      // SAFETY: capture belongs to this overlay thread and is released on up/cancellation.
      if !capture(hwnd) {
        self.toolbar_press = false;
        tracing::warn!("Error badge could not capture pointer");
      }
      invalidate(hwnd);
      return true;
    }
    self.error_details_pinned = false;
    if contains(toolbar_grip(toolbar, m), pos) {
      self.color_open = false;
      self.width_open = false;
      self.text_style_open = false;
      invalidate(hwnd);
      // SAFETY: this owner thread captures its own live overlay window.
      if capture(hwnd) {
        self.toolbar_drag = Some((
          toolbar,
          (
            pos.0.round() as i32 - toolbar.left,
            pos.1.round() as i32 - toolbar.top,
          ),
        ));
        self.toolbar_press = true;
      } else {
        tracing::warn!("Annotation toolbar could not capture pointer for dragging");
      }
      return true;
    }
    let formatting = contains(toolbar, pos)
      && matches!(toolbar_index(toolbar, pos, m), Some(7 | 11))
      || (self.width_open || self.color_open) && contains(self.picker_rect(controller, rect), pos);
    if self.object_mode
      && !controller.modifiers.shift
      && !contains(toolbar, pos)
      && !formatting
      && !self.width_open
      && !self.color_open
      && (self.geometry_handle_hit(pos).is_none()
        || self.annotations.iter().any(|a| {
          Some(a.id) == self.selected && a.tool.freehand() && self.node_mode != self.selected
        }))
      && self.path_click(pos)
    {
      invalidate(hwnd);
      return true;
    }
    if self.text_resize_hit(pos) && !contains(toolbar, pos) && !formatting {
      if let Some(edit) = &mut self.text_edit {
        edit.resizing = Some((pos.0.round() as i32, edit.wrap_width));
      } else if let Some(index) = self
        .annotations
        .iter()
        .position(|a| Some(a.id) == self.selected)
      {
        let before = self.annotations[index].clone();
        self.moving = Some(ObjectMove {
          index,
          preview: before.clone(),
          before,
          start: (pos.0.round() as i32, pos.1.round() as i32),
        });
        self.resizing_text = true;
      }
      self.toolbar_press = true;
      // SAFETY: thread-owned parent retains width dragging through mouse release.
      capture(hwnd);
      return true;
    }
    if self
      .text_selection_bounds()
      .is_some_and(|bounds| self.text_border_hit(bounds, pos))
      && self.group_selection.len() < 2
      && matches!(hit_test(&rect, pos), HitResult::Inside)
      && !contains(toolbar, pos)
      && !formatting
    {
      if let Some(edit) = &mut self.text_edit {
        edit.moving = Some(((pos.0.round() as i32, pos.1.round() as i32), edit.point));
      } else if let Some(index) = self
        .annotations
        .iter()
        .position(|a| Some(a.id) == self.selected)
      {
        let before = self.annotations[index].clone();
        self.moving = Some(ObjectMove {
          index,
          preview: before.clone(),
          before,
          start: (pos.0.round() as i32, pos.1.round() as i32),
        });
      }
      self.toolbar_press = true;
      // SAFETY: border dragging is owned by this parent thread until mouse release/cancellation.
      capture(hwnd);
      return true;
    }
    if self.text_edit.is_some() && !formatting {
      self.complete_text(hwnd, true, None);
      if self.text_edit.is_some() {
        return true;
      }
      // Continue handling the same click, so switching tools requires only one click.
    }
    if self.color_open && contains(self.picker_rect(controller, rect), pos) {
      let panel = self.picker_rect(controller, rect);
      if let Some(index) = (0..12).find(|i| contains(color_cell(panel, *i, m), pos)) {
        self.color = index as usize;
        self.edit_selected_style();
        self.color_open = false;
      }
      self.toolbar_press = true;
    } else if self.width_open && contains(self.picker_rect(controller, rect), pos) {
      let panel = self.picker_rect(controller, rect);
      if self.font_mode() {
        if self.text_style_open {
          if let Some(index) = (0..3).find(|index| contains(appearance_cell(panel, *index, m), pos))
          {
            self.text_appearance = [
              TextAppearance::Plain,
              TextAppearance::Outline,
              TextAppearance::Background,
            ][index as usize];
            self.width_open = false;
          }
        } else if contains(style_menu_button(panel, m), pos) {
          self.text_style_open = true;
        } else if let Some(index) = (0..6).find(|index| contains(font_cell(panel, *index, m), pos))
        {
          self.text_size = FONT_SIZES[index as usize];
          self.width_open = false;
        }
        self.edit_selected_style();
      } else if let Some(index) = (0..3).find(|index| contains(width_row(panel, *index, m), pos)) {
        self.width = [3, 6, 10][index as usize];
        self.edit_selected_style();
        self.width_open = false;
      }
      self.toolbar_press = true;
    } else if contains(toolbar, pos) {
      let action = toolbar_index(toolbar, pos, m);
      // Preserve only the clicked picker's toggle state, including clicks on panel spacing.
      if action != Some(7) {
        self.color_open = false;
      }
      if action != Some(11) {
        self.width_open = false;
      }
      if let Some(index) = action {
        if matches!(index, 0..=4 | 8 | 12..=14 | 16) && index != self.active_tool_button() {
          self.clear_toolbar_error();
        }
        if self.group_selection.len() > 1 && matches!(index, 7 | 11) {
          self.toolbar_press = true;
          return true;
        }
        if matches!(index, 0..=4 | 8 | 13 | 14 | 16) {
          self.group_selection.clear();
        }
        match index {
          0..=4 | 8 => {
            self.tool = if index == 8 {
              Tool::Highlighter
            } else {
              TOOLS[index as usize]
            };
            self.object_mode = false;
            self.selected = None;
          },
          12 => {
            self.object_mode = true;
            self.tool = Tool::Select;
          },
          13 | 14 | 16 => {
            self.tool = match index {
              13 => Tool::Text,
              14 => Tool::Marker,
              _ => Tool::Blur,
            };
            self.object_mode = false;
            self.selected = None;
            self.width_open = false;
            if self.tool == Tool::Blur {
              self.prefetch_redaction(controller, hwnd);
            }
          },
          5 => self.undo(),
          6 => self.redo(),
          7 if !self.blur_mode() => {
            self.color_open = !self.color_open;
            self.width_open = false;
            self.text_style_open = false;
          },
          11 if self.blur_mode() => self.redact_all_text(controller, hwnd),
          11 if !self.marker_mode() => {
            self.width_open = !self.width_open;
            self.text_style_open = false;
          },
          _ => {},
        }
      }
      self.toolbar_press = true;
    } else if self.width_open || self.color_open {
      // The first outside click dismisses the popover without starting a gesture.
      self.width_open = false;
      self.text_style_open = false;
      self.color_open = false;
      self.toolbar_press = true;
    } else if self.geometry_handle_hit(pos).is_some() {
      if let Some((index, handle)) = self.geometry_handle_hit(pos) {
        let before = self.annotations[index].clone();
        self.group_move.clear();
        self.geometry_handle = Some(handle);
        self.selected_node = if before.tool == Tool::Arrow {
          Some(handle)
        } else if before.tool.freehand() && handle >= 16 {
          Some(handle - 16)
        } else {
          None
        };
        self.moving = Some(ObjectMove {
          index,
          preview: before.clone(),
          before,
          start: (pos.0.round() as i32, pos.1.round() as i32),
        });
        self.scene_cache.invalidate();
      }
    } else if self.object_mode && matches!(hit_test(&rect, pos), HitResult::Inside) {
      if controller.modifiers.shift {
        if let Some(index) = self.hit_drawing(pos) {
          let id = self.annotations[index].id;
          self.toggle_selection(id);
        }
        self.toolbar_press = true;
        invalidate(hwnd);
        return true;
      }
      self.selected = self
        .hit_drawing(pos)
        .map(|index| self.annotations[index].id);
      if let Some(index) = self.hit_drawing(pos) {
        if !self.group_selection.contains(&self.annotations[index].id) {
          self.group_selection.clear();
        }
        if self.annotations[index].tool == Tool::Text
          && self.group_selection.len() < 2
          && self.begin_text_object(controller, hwnd, index)
        {
          return true;
        }
        let before = self.annotations[index].clone();
        self.group_move = self
          .annotations
          .iter()
          .enumerate()
          .filter(|(other, a)| *other != index && self.group_selection.contains(&a.id))
          .map(|(index, a)| ObjectMove {
            index,
            before: a.clone(),
            preview: a.clone(),
            start: (pos.0.round() as i32, pos.1.round() as i32),
          })
          .collect();
        self.scene_cache.invalidate();
        self.width = before.width;
        if let Some(color) = COLORS.iter().position(|color| *color == before.color) {
          self.color = color;
        }
        self.moving = Some(ObjectMove {
          index,
          preview: before.clone(),
          before,
          start: (pos.0.round() as i32, pos.1.round() as i32),
        });
      } else {
        self.group_selection.clear();
        self.toolbar_press = true;
      }
    } else if matches!(hit_test(&rect, pos), HitResult::Inside)
      && let Some(index) = self.hit_own_drawing(pos)
    {
      // The active tool grabs its own kind of drawing instead of starting a new one.
      let before = self.annotations[index].clone();
      self.selected = Some(before.id);
      self.group_selection.clear();
      self.group_move.clear();
      self.width = before.width;
      if let Some(color) = COLORS.iter().position(|color| *color == before.color) {
        self.color = color;
      }
      self.moving = Some(ObjectMove {
        index,
        preview: before.clone(),
        before,
        start: (pos.0.round() as i32, pos.1.round() as i32),
      });
      self.scene_cache.invalidate();
    } else if self.tool == Tool::Text && matches!(hit_test(&rect, pos), HitResult::Inside) {
      if let Some(index) = self
        .hit_drawing(pos)
        .filter(|index| self.annotations[*index].tool == Tool::Text)
      {
        self.begin_text_object(controller, hwnd, index);
        return true;
      }
      if self.annotations.len() >= MAX_ANNOTATIONS {
        self.limited = Some(DrawingLimit::Drawings);
        invalidate(hwnd);
        return true;
      }
      if let Err(error) = self.start_text(controller, hwnd, None) {
        tracing::error!(%error,"Failed to open inline text field");
        self.report_render_failure();
        invalidate(hwnd);
      }
      return true;
    } else if self.tool == Tool::Marker && matches!(hit_test(&rect, pos), HitResult::Inside) {
      self.selected = None;
      self.place_marker((pos.0.round() as i32, pos.1.round() as i32));
      self.toolbar_press = true;
    } else if self.tool != Tool::Select && matches!(hit_test(&rect, pos), HitResult::Inside) {
      if self.annotations.len() >= MAX_ANNOTATIONS {
        self.limited = Some(DrawingLimit::Drawings);
        self.toolbar_press = true;
        tracing::warn!("Screenshot annotation count limit reached");
      } else {
        let point = (pos.0.round() as i32, pos.1.round() as i32);
        // Pressing away from the selected drawing lets it go and starts the next one.
        self.selected = None;
        self.limited = None;
        self.pen_compactions = 0;
        self.draft = Some(Annotation {
          corner_radius: 0,
          rotation_millidegrees: 0,
          path_transform: crate::screenshot::path::IDENTITY,
          curve_controls: Vec::new(),
          pen_nodes: Vec::new(),
          label: None,
          id: 0,
          tool: self.tool,
          points: if self.tool.freehand() {
            vec![point]
          } else {
            vec![point, point]
          },
          color: COLORS[self.color],
          width: self.width,
        });
      }
    } else {
      // A press outside the crop starts a new one with every tool; crop movement and handles
      // belong exclusively to the crop tool.
      return !matches!(hit_test(&rect, pos), HitResult::Outside)
        && (self.tool != Tool::Select || self.object_mode);
    }
    // SAFETY: hwnd is this thread's live overlay. Capture is released on up/cancellation.
    capture(hwnd);
    if self.text_edit.is_some() {
      self.text_changed(controller, hwnd, None);
    }
    invalidate(hwnd);
    true
  }

  /// Right-click cancels the drawing being made, or deletes the drawing under the
  /// pointer as one undoable step. Returns false when neither applies, leaving the click to the
  /// selection.
  pub fn secondary_click(&mut self, controller: &SelectionControllerState, hwnd: HWND) -> bool {
    let Some(rect) = Self::confirmed(controller) else {
      return false;
    };
    if self.text_edit.is_some() {
      return false;
    }
    if self.draft.is_some() || self.moving.is_some() || self.geometry_handle.is_some() {
      self.cancel_gesture();
      self.scene_cache.invalidate();
      invalidate(hwnd);
      return true;
    }
    let pos = controller.cursor_pos;
    if !matches!(hit_test(&rect, pos), HitResult::Inside) || self.toolbar_cursor(controller) {
      return false;
    }
    let Some(index) = self.hit_drawing(pos) else {
      return false;
    };
    self.selected = Some(self.annotations[index].id);
    self.group_selection.clear();
    self.node_mode = None;
    self.node_indices.clear();
    self.selected_node = None;
    self.color_open = false;
    self.width_open = false;
    self.delete_selected();
    self.selected = None;
    invalidate(hwnd);
    true
  }

  pub fn pointer_up(&mut self, controller: &SelectionControllerState, hwnd: HWND) -> bool {
    self.pointer_up_with(controller, hwnd, || {
      // SAFETY: release capture on its overlay owner thread.
      unsafe { ReleaseCapture() }
    })
  }

  fn pointer_up_with(
    &mut self,
    controller: &SelectionControllerState,
    hwnd: HWND,
    release: impl Fn() -> windows::core::Result<()>,
  ) -> bool {
    if self.toolbar_drag.take().is_some() {
      self.toolbar_press = false;
      // SAFETY: release this thread's pointer capture after clearing the drag state.
      if let Err(error) = release() {
        tracing::warn!(%error, "Annotation toolbar capture release failed");
      }
      invalidate(hwnd);
      return true;
    }
    if let Some(edit) = &mut self.text_edit
      && let Some((start, point)) = edit.moving.take()
    {
      edit.point = (
        point.0 + controller.cursor_pos.0.round() as i32 - start.0,
        point.1 + controller.cursor_pos.1.round() as i32 - start.1,
      );
      self.text_changed(controller, hwnd, None);
    }
    if let Some(edit) = &mut self.text_edit
      && let Some((start, width)) = edit.resizing.take()
    {
      edit.wrap_width = (width + controller.cursor_pos.0.round() as i32 - start)
        .clamp(64.max(self.text_size * 2), 8192);
      self.text_changed(controller, hwnd, None);
    }
    if self.draft.is_none() && self.moving.is_none() && !self.toolbar_press {
      return false;
    }
    self.update_draft(controller);
    self.update_move(controller);
    self.finish_gesture();
    self.toolbar_press = false;
    // SAFETY: releases this thread's mouse capture, including outside-window mouse-up.
    if let Err(error) = release() {
      tracing::warn!(%error, "Annotation capture release failed");
    }
    invalidate(hwnd);
    if !self.width_open
      && !self.color_open
      && let Some(edit) = &self.text_edit
    {
      edit.field.focus();
    }
    true
  }

  fn history_shortcut(&mut self, controller: &SelectionControllerState, hwnd: HWND, redo: bool) {
    if self.draft.is_none() && self.moving.is_none() && Self::confirmed(controller).is_some() {
      if redo {
        self.redo();
      } else {
        self.undo();
      }
    }
    invalidate(hwnd);
  }

  /// Back undoes and Forward redoes, under the same guards as Ctrl+Z and Ctrl+Y.
  pub fn side_button(
    &mut self,
    controller: &SelectionControllerState,
    hwnd: HWND,
    button: u16,
  ) -> bool {
    if !matches!(button, XBUTTON1 | XBUTTON2) || Self::confirmed(controller).is_none() {
      return false;
    }
    if self.text_edit.is_none() {
      self.history_shortcut(controller, hwnd, button == XBUTTON2);
    }
    true
  }

  pub fn keydown(&mut self, controller: &SelectionControllerState, hwnd: HWND, vk: u32) -> bool {
    if vk == 0x1b && self.moving.is_some() {
      self.cancel_gesture();
      self.toolbar_press = false;
      // SAFETY: only this overlay's active owner-thread drag is cancelled.
      if let Err(error) = unsafe { ReleaseCapture() } {
        tracing::warn!(%error,"Path drag cancellation capture release failed");
      }
      invalidate(hwnd);
      return true;
    }
    if vk == 0x1b && self.node_mode.is_some() {
      self.node_mode = None;
      self.node_indices.clear();
      self.selected_node = None;
      self.last_path_click = None;
      invalidate(hwnd);
      return true;
    }
    if vk == 0x1b && self.redaction.busy() {
      // Esc stops a waiting Redact All Text, not the overlay.
      self.redaction.cancel();
      invalidate(hwnd);
      return true;
    }
    if vk == 0x70 && self.toolbar_error().is_some() {
      self.error_details_pinned = !self.error_details_pinned;
      invalidate(hwnd);
      return true;
    }
    if vk == 0x1b && self.error_details_pinned {
      self.error_details_pinned = false;
      invalidate(hwnd);
      return true;
    }
    if self.text_edit.is_some() {
      // Focus may return to the parent after activation; Escape still cancels only text.
      if vk == 0x1b {
        self.complete_text(hwnd, false, None);
      }
      // SAFETY: parent and field use the same thread-local input queue after activation.
      if vk == 0x0d && unsafe { GetKeyState(0x11) >= 0 && GetKeyState(0x10) >= 0 } {
        self.complete_text(hwnd, true, None);
      }
      return true;
    }
    if Self::confirmed(controller).is_none() {
      return false;
    }
    // SAFETY: GetKeyState reads the overlay thread's input queue; no shared mutation.
    let ctrl = unsafe { GetKeyState(0x11) } < 0;
    // SAFETY: reads the same thread-local input queue, including system ALT keys.
    let alt = unsafe { GetKeyState(0x12) } < 0;
    if vk == 0x1B && (self.width_open || self.color_open) {
      self.color_open = false;
      self.width_open = false;
      invalidate(hwnd);
      return true;
    }
    if vk == 0x2E
      && self.moving.is_none()
      && self.draft.is_none()
      && (self.object_mode
        || self.tool == Tool::Text
        || self.edits_own_drawings() && self.selected.is_some())
    {
      if self.selected_node.is_some() && self.delete_path_node() {
        invalidate(hwnd);
        return true;
      }
      self.delete_selected();
      invalidate(hwnd);
      return true;
    }
    if !ctrl && !alt && self.draft.is_none() && self.moving.is_none() && vk == 0x53 {
      self.clear_toolbar_error();
      self.color_open = false;
      self.object_mode = true;
      self.tool = Tool::Select;
      self.width_open = false;
      invalidate(hwnd);
      return true;
    }
    if !ctrl
      && !alt
      && controller.modifiers.shift
      && vk == 0x42
      && self.draft.is_none()
      && self.moving.is_none()
    {
      self.color_open = false;
      self.width_open = false;
      self.tool = Tool::Blur;
      self.group_selection.clear();
      self.object_mode = false;
      self.selected = None;
      self.redact_all_text(controller, hwnd);
      invalidate(hwnd);
      return true;
    }
    if !ctrl
      && !alt
      && self.draft.is_none()
      && self.moving.is_none()
      && let Some(tool) = shortcut_tool(vk)
    {
      if tool == Tool::Text
        && let Some(before) = self
          .annotations
          .iter()
          .find(|a| Some(a.id) == self.selected && a.tool == Tool::Text)
          .cloned()
      {
        if let Err(error) = self.start_text(controller, hwnd, Some(before)) {
          tracing::error!(%error,"Failed to reopen annotation text");
          self.report_render_failure();
        }
        return true;
      }
      if self.tool != tool || self.object_mode {
        self.clear_toolbar_error();
      }
      self.color_open = false;
      self.tool = tool;
      self.group_selection.clear();
      self.object_mode = false;
      self.selected = None;
      self.width_open = false;
      if tool == Tool::Blur {
        self.prefetch_redaction(controller, hwnd);
      }
      invalidate(hwnd);
      return true;
    }
    if ctrl && matches!(vk, 0x5A | 0x59) {
      self.history_shortcut(controller, hwnd, vk == 0x59 || controller.modifiers.shift);
      return true;
    }
    if vk == 0x0D {
      self.finish_gesture();
    }
    // Keep the crop stable during a stroke, but still allow modifiers and cancellation.
    (self.draft.is_some() || self.moving.is_some() || self.object_mode || self.tool != Tool::Select)
      && matches!(vk, 0x25..=0x28)
  }

  pub fn modifiers_changed(&mut self, controller: &SelectionControllerState, hwnd: HWND) {
    if self.draft.is_some() || self.geometry_handle.is_some() {
      let old = self.active_bounds();
      self.update_draft(controller);
      self.update_move(controller);
      if let Some(damage) = combine_bounds(old, self.active_bounds()) {
        invalidate_region(hwnd, damage);
      }
    }
  }
}

#[cfg(test)]
mod gesture_tests;
