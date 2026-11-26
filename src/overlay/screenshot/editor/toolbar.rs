//! Toolbar state, error notices, animation and chrome rendering.

use super::layout::{
  Metrics, appearance_cell, avoid_text_region, color_cell, contains, error_badge_rect,
  error_tooltip_rect, font_cell, inset, invalidate_region, layout_popover_at, layout_toolbar,
  style_menu_button, toolbar_bounds, toolbar_button, toolbar_columns, toolbar_grip,
  toolbar_grip_highlight, toolbar_index, toolbar_metrics, width_row,
};
use super::{
  ACTIONS, AnnotationEditor, BUTTONS, COLORS, DrawingLimit, ERROR_TOOLTIP_HEIGHT,
  ERROR_TOOLTIP_WIDTH, FONT_SIZES, TOOLBAR_VERTICAL_PAD, TOOLS,
};
use crate::overlay::chrome::Icon;
use crate::overlay::screenshot::cache;
use crate::overlay::selection::{HitResult, Rect, SelectionInteraction, hit_test};
use crate::overlay::selection_core::SelectionControllerState;
use crate::overlay::theme;

/// Picker carets draw the 12px chevron at 10px, matching the reviewed toolbar mockup.
const CARET_SIZE: f32 = 10.0;

/// The reviewed Fluent icon for a toolbar button; the selected tool shows its Filled variant.
pub(super) fn toolbar_icon(index: i32, selected: bool) -> Option<Icon> {
  let (rest, filled) = match index {
    0 => (Icon::AdjustArea, Icon::AdjustAreaSelected),
    12 => (Icon::SelectDrawing, Icon::SelectDrawingSelected),
    1 => (Icon::Pen, Icon::PenSelected),
    8 => (Icon::Highlighter, Icon::HighlighterSelected),
    2 => (Icon::Arrow, Icon::ArrowSelected),
    3 => (Icon::Rectangle, Icon::RectangleSelected),
    4 => (Icon::Ellipse, Icon::EllipseSelected),
    16 => (Icon::Blur, Icon::BlurSelected),
    13 => (Icon::Text, Icon::TextSelected),
    14 => (Icon::StepMarker, Icon::StepMarkerSelected),
    // Undo and redo are never selected, so they have no Filled variant.
    5 => return Some(Icon::Undo),
    6 => return Some(Icon::Redo),
    _ => return None,
  };
  Some(if selected { filled } else { rest })
}
use crate::screenshot::annotations::{TextAppearance, Tool};
use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{HDC, HFONT};

impl AnnotationEditor {
  pub(super) fn hover_changed(&mut self, controller: &SelectionControllerState) -> bool {
    let key = self.current_hover_key(controller);
    if key == self.hover_key {
      return false;
    }
    self.hover_key = key;
    true
  }

  pub fn animation_active(&self) -> bool {
    self.motion.active() || self.error_frame_pending || self.button_tip.active()
  }

  pub fn tick_animation(&mut self, controller: &SelectionControllerState, hwnd: HWND) {
    let due = self.motion.active()
      || self.error_frame_pending
      || self.button_tip.repaint_due(std::time::Instant::now());
    if due && let Some(rect) = Self::confirmed(controller) {
      invalidate_region(hwnd, self.chrome_region(controller, rect));
    }
  }

  pub(super) fn update_motion(&mut self, controller: &SelectionControllerState) {
    let popup = if self.color_open {
      1
    } else if self.width_open {
      if self.text_style_open { 3 } else { 2 }
    } else {
      0
    };
    self.motion.update(
      controller.interaction.is_confirmed_idle(),
      popup,
      self.current_hover_key(controller),
      std::time::Instant::now(),
    );
    self.update_error_motion(controller, std::time::Instant::now());
    self.update_button_tip(controller, std::time::Instant::now());
  }

  pub(super) fn update_error_motion(
    &mut self,
    controller: &SelectionControllerState,
    now: std::time::Instant,
  ) {
    let show_error = controller.interaction.is_confirmed_idle()
      && !self.color_open
      && !self.width_open
      && self.toolbar_error().is_some_and(|(_, button)| {
        self.error_details_pinned || self.current_hover_key(controller) == Some(button)
      });
    self
      .error_fade
      .target(u8::from(show_error) as f32, now, self.motion.enabled());
    self.error_frame_pending = self.motion.enabled() && self.error_fade.active(now);
  }

  pub(super) fn current_hover_key(&self, controller: &SelectionControllerState) -> Option<i32> {
    let rect = Self::confirmed(controller)?;
    let pos = controller.cursor_pos;
    let m = self.metrics(controller, rect);
    if self.color_open {
      let panel = self.picker_rect(controller, rect);
      if let Some(index) = (0..12).find(|i| contains(color_cell(panel, *i, m), pos)) {
        return Some(20 + index);
      }
    }
    if self.width_open {
      let panel = self.picker_rect(controller, rect);
      if self.font_mode() {
        if self.text_style_open {
          if let Some(index) = (0..3).find(|index| contains(appearance_cell(panel, *index, m), pos))
          {
            return Some(46 + index);
          }
        } else {
          if let Some(index) = (0..6).find(|index| contains(font_cell(panel, *index, m), pos)) {
            return Some(40 + index);
          }
          if contains(style_menu_button(panel, m), pos) {
            return Some(50);
          }
        }
        return None;
      }
      if let Some(index) = (0..3).find(|index| contains(width_row(panel, *index, m), pos)) {
        return Some(16 + index);
      }
    }
    if self.text_resize_hit(pos)
      && self.group_selection.len() < 2
      && matches!(hit_test(&rect, pos), HitResult::Inside)
      && !self.toolbar_cursor(controller)
    {
      return Some(49);
    }
    let toolbar = self.toolbar_rect(controller, rect);
    if contains(toolbar_grip(toolbar, m), pos) {
      Some(60)
    } else {
      self
        .toolbar_error()
        .and_then(|(_, button)| {
          contains(error_badge_rect(toolbar, button, m), pos).then_some(button)
        })
        .or_else(|| toolbar_index(toolbar, pos, m))
    }
  }

  /// The size the font picker's sample shows: the size under the pointer while it rests on a
  /// choice, so the effect is visible before clicking, otherwise the selected size.
  pub(super) fn font_sample_size(&self, hover: Option<i32>) -> i32 {
    hover
      .and_then(|key| usize::try_from(key - 40).ok())
      .and_then(|index| FONT_SIZES.get(index).copied())
      .unwrap_or(self.text_size)
  }

  /// The thickness in capture pixels that a chosen width paints with the current tool.
  pub(super) fn painted_width(&self, width: i32) -> i32 {
    if self.highlighter_mode() {
      width.saturating_mul(crate::screenshot::annotations::HIGHLIGHTER_SCALE)
    } else {
      width
    }
  }

  pub(super) fn font_mode(&self) -> bool {
    self.text_edit.is_some()
      || self.tool == Tool::Text
      || self.selected.is_some_and(|id| {
        self
          .annotations
          .iter()
          .any(|a| a.id == id && a.tool == Tool::Text)
      })
  }

  pub(super) fn metrics(&self, controller: &SelectionControllerState, rect: Rect) -> Metrics {
    toolbar_metrics(controller, rect, &self.monitors)
  }

  /// The toolbar button owning the open (or next) popover: color (7) or size/style (11).
  fn picker_button(&self) -> i32 {
    if self.color_open { 7 } else { 11 }
  }

  /// Design size at 100% of the popover `button` opens in the current mode.
  fn picker_size(&self, button: i32) -> (i32, i32) {
    if button == 11 && self.font_mode() {
      (192, if self.text_style_open { 128 } else { 200 })
    } else {
      (176, 128)
    }
  }

  /// The popover a picker button opens, before avoiding a native text field.
  fn button_popover(&self, controller: &SelectionControllerState, rect: Rect, button: i32) -> RECT {
    let m = self.metrics(controller, rect);
    let (width, height) = self.picker_size(button);
    let toolbar = self.toolbar_rect(controller, rect);
    layout_popover_at(
      toolbar,
      toolbar_button(toolbar, button, m),
      toolbar_bounds(controller, rect, &self.monitors),
      m.px(width),
      m.px(height),
      m,
    )
  }

  /// Whether `button`'s popover opens above the toolbar, which its caret shows.
  pub(super) fn popover_opens_up(
    &self,
    controller: &SelectionControllerState,
    rect: Rect,
    button: i32,
  ) -> bool {
    self.button_popover(controller, rect, button).bottom <= self.toolbar_rect(controller, rect).top
  }

  pub(super) fn picker_rect(&self, controller: &SelectionControllerState, rect: Rect) -> RECT {
    let bounds = toolbar_bounds(controller, rect, &self.monitors);
    let preferred = self.button_popover(controller, rect, self.picker_button());
    self
      .text_edit
      .as_ref()
      .map(|edit| avoid_text_region(preferred, bounds, edit.input_rect))
      .unwrap_or(preferred)
  }

  pub(super) fn active_tool_button(&self) -> i32 {
    if self.object_mode {
      12
    } else {
      match self.tool {
        Tool::Text => 13,
        Tool::Marker => 14,
        Tool::Highlighter => 8,
        Tool::Blur => 16,
        _ => TOOLS
          .iter()
          .position(|tool| *tool == self.tool)
          .map_or(0, |i| i as i32),
      }
    }
  }

  pub(super) fn clear_toolbar_error(&mut self) {
    self.text_error = false;
    self.render_failed = false;
    self.limited = None;
    self.redact_notice = None;
    self.error_button = None;
    self.error_details_pinned = false;
  }

  pub(super) fn report_render_failure(&mut self) {
    self.render_failed = true;
    self.error_button = Some(self.active_tool_button());
  }

  pub(super) fn toolbar_error(&self) -> Option<(&'static str, i32)> {
    if self.text_error {
      Some(("annotation_error_text", 13))
    } else if self.render_failed {
      let button = self
        .error_button
        .unwrap_or_else(|| self.active_tool_button());
      Some((
        match button {
          5 => "annotation_error_undo",
          6 => "annotation_error_redo",
          _ => "annotation_error_render",
        },
        button,
      ))
    } else if let Some(key) = self.redact_notice {
      Some((key, 11))
    } else if let Some(limit) = self.limited {
      Some((
        match limit {
          DrawingLimit::Drawings => "annotation_error_drawings",
          DrawingLimit::Stroke => "annotation_error_stroke",
          DrawingLimit::Marker => "annotation_error_marker",
          DrawingLimit::Identifier => "annotation_error_identifier",
        },
        match limit {
          DrawingLimit::Stroke => self.active_tool_button(),
          DrawingLimit::Marker => 14,
          _ => self.active_tool_button(),
        },
      ))
    } else {
      None
    }
  }

  /// The toolbar's area while a confirmed crop shows it.
  pub(crate) fn crop_toolbar_rect(&self, controller: &SelectionControllerState) -> Option<RECT> {
    match controller.interaction {
      SelectionInteraction::Confirmed { rect, .. } => Some(self.toolbar_rect(controller, rect)),
      _ => None,
    }
  }

  pub(crate) fn report_magnifier_failure(&mut self) {
    self.report_render_failure();
  }

  pub(super) fn toolbar_rect(&self, controller: &SelectionControllerState, rect: Rect) -> RECT {
    let preferred = layout_toolbar(
      rect,
      toolbar_bounds(controller, rect, &self.monitors),
      self.metrics(controller, rect),
    );
    if let Some((left, top)) = self.toolbar_position {
      let bounds = toolbar_bounds(controller, rect, &self.monitors);
      let width = preferred.right - preferred.left;
      let height = preferred.bottom - preferred.top;
      let left = left.clamp(bounds.left, (bounds.right - width).max(bounds.left));
      let top = top.clamp(bounds.top, (bounds.bottom - height).max(bounds.top));
      return RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
      };
    }
    self
      .text_edit
      .as_ref()
      .map(|edit| {
        avoid_text_region(
          preferred,
          toolbar_bounds(controller, rect, &self.monitors),
          edit.input_rect,
        )
      })
      .unwrap_or(preferred)
  }

  /// Paint bounds for an elevated panel: the panel plus its shadow, kept on the surface.
  fn elevated_bounds(controller: &SelectionControllerState, panel: RECT, m: Metrics) -> RECT {
    let spread = (crate::overlay::chrome::SHADOW_SPREAD * m.scale).ceil() as i32;
    // The shadow drops 30% of its spread below the panel; one pixel absorbs rounding.
    let drop = (spread * 3 + 9) / 10 + 1;
    RECT {
      left: (panel.left - spread).max(0),
      top: (panel.top - spread).max(0),
      right: (panel.right + spread).min(controller.width),
      bottom: (panel.bottom + spread + drop).min(controller.height),
    }
  }

  pub(super) fn picker_region(&self, controller: &SelectionControllerState, rect: Rect) -> RECT {
    Self::elevated_bounds(
      controller,
      self.picker_rect(controller, rect),
      self.metrics(controller, rect),
    )
  }

  fn toolbar_disabled(&self, index: i32) -> bool {
    (index == 5 && !self.history.can_undo())
      || (self.group_selection.len() > 1 && matches!(index, 7 | 11))
      || (index == 6 && !self.history.can_redo())
      || (index == 11 && self.marker_mode() && !self.blur_mode())
      || (index == 7 && self.blur_mode())
  }

  fn toolbar_selected(&self, index: i32) -> bool {
    let drawing = !self.object_mode;
    (index < 5 && drawing && self.tool == TOOLS[index as usize])
      || (index == 12 && self.object_mode)
      || (index == 8 && drawing && self.tool == Tool::Highlighter)
      || (index == 13 && self.tool == Tool::Text)
      || (index == 14 && self.tool == Tool::Marker)
      || (index == 16 && drawing && self.tool == Tool::Blur)
  }

  pub(super) fn draw_toolbar(
    &mut self,
    controller: &SelectionControllerState,
    dc: HDC,
    _font: HFONT,
    rect: Rect,
  ) -> Result<()> {
    let m = self.metrics(controller, rect);
    let toolbar = self.toolbar_rect(controller, rect);
    let font_mode = self.font_mode();
    let mut scene = crate::overlay::chrome::Scene::scaled(m.scale)?;
    let now = std::time::Instant::now();
    self.hover_key = self.current_hover_key(controller);
    let pressed_key = self.hover_key.filter(|_| self.toolbar_press);
    scene.shadow(toolbar, m.radius(crate::overlay::chrome::PANEL_RADIUS));
    scene.panel(toolbar)?;
    let grip = toolbar_grip_highlight(toolbar, m);
    let grip_alpha = self.motion.hover_alpha(60, now);
    if grip_alpha > 0 {
      scene.rounded(
        grip,
        m.radius(crate::overlay::chrome::inner_radius(
          TOOLBAR_VERTICAL_PAD as f32,
        )),
        theme::HOVER,
        grip_alpha,
      )?;
    }
    scene.icon(grip, Icon::Grip, theme::MUTED)?;
    if toolbar_columns(toolbar, m) == BUTTONS {
      for index in [1, 7, 5] {
        let cell = toolbar_button(toolbar, index, m);
        let previous_right = cell.left - m.gap - m.group_gap;
        let center = (previous_right + cell.left) / 2;
        scene.fill(
          RECT {
            left: center - 1,
            right: center - 1 + m.px(1).max(1),
            top: cell.top + m.px(8),
            bottom: cell.bottom - m.px(8),
          },
          theme::SEPARATOR,
        )?;
      }
    }
    let radius = m.radius(crate::overlay::chrome::inner_radius(
      TOOLBAR_VERTICAL_PAD as f32,
    ));
    for index in ACTIONS {
      let cell = toolbar_button(toolbar, index, m);
      let selected = self.toolbar_selected(index);
      let disabled = self.toolbar_disabled(index);
      let pressed = !disabled && pressed_key == Some(index);
      // A picker button stays lit while its popover is open, and Redact All Text while it waits.
      let open = (index == 7 && self.color_open)
        || (index == 11 && self.width_open)
        || (index == 11 && self.blur_mode() && self.redaction.busy());
      let hover_alpha = if disabled {
        0
      } else {
        self.motion.hover_alpha(index as usize, now)
      };
      if selected {
        // One strong signal: a solid accent tile, lifted on hover and deepened on press.
        let fill = if pressed {
          crate::overlay::motion::blend_rgb(theme::SELECTED, [0; 3], 40)
        } else {
          let lift = (u16::from(hover_alpha) * 36 / 255) as u8;
          crate::overlay::motion::blend_rgb(theme::SELECTED, [255; 3], lift)
        };
        scene.rounded(cell, radius, fill, 255)?;
      } else {
        scene.rounded(cell, radius, theme::TILE, if disabled { 120 } else { 255 })?;
        if pressed {
          scene.rounded(cell, radius, theme::ACTIVE, 255)?;
        } else if open {
          scene.rounded(cell, radius, theme::HOVER, 255)?;
        } else if hover_alpha > 0 {
          scene.rounded(cell, radius, theme::HOVER, hover_alpha)?;
        }
      }
      if index == 7 {
        let alpha = if self.blur_mode() { 80 } else { 255 };
        let ring = inset(cell, m.px(7));
        let swatch = inset(cell, m.px(9));
        scene.rounded(
          ring,
          (ring.right - ring.left) as f32 / 2.0,
          theme::TEXT,
          alpha,
        )?;
        scene.rounded(
          swatch,
          (swatch.right - swatch.left) as f32 / 2.0,
          COLORS[self.color],
          alpha,
        )?;
      } else {
        let color = if disabled {
          theme::DISABLED
        } else if selected {
          theme::SELECTED_ICON
        } else {
          theme::TEXT
        };
        if index == 11 && self.blur_mode() {
          scene.icon(cell, Icon::RedactText, color)?;
        } else if index == 11 {
          scene.text(
            cell,
            &if self.marker_mode() {
              "—".into()
            } else if font_mode {
              self.text_size.to_string()
            } else {
              self.painted_width(self.width).to_string()
            },
            color,
          )?;
        } else {
          let icon = toolbar_icon(index, selected).context("Toolbar button has no icon")?;
          scene.icon(cell, icon, color)?;
        }
      }
      if index == 7 || (index == 11 && !self.blur_mode()) {
        // A corner caret marks buttons that open a popover and points to where it opens.
        let opens_up = self.popover_opens_up(controller, rect, index);
        let (x, y) = (cell.right - m.px(6), cell.bottom - m.px(6));
        let half = m.px(6);
        scene.icon_sized(
          RECT {
            left: x - half,
            top: y - half,
            right: x + half,
            bottom: y + half,
          },
          if opens_up {
            Icon::ChevronUp
          } else {
            Icon::ChevronDown
          },
          if disabled {
            theme::DISABLED
          } else if open || hover_alpha > 0 {
            theme::TEXT
          } else {
            theme::MUTED
          },
          CARET_SIZE,
        )?;
      }
    }
    let strings = crate::i18n::annotations(&self.lang);
    let mut toolbar_scene =
      std::mem::replace(&mut scene, crate::overlay::chrome::Scene::scaled(m.scale)?);
    // Options sit 8px inside the popover.
    let option_radius = m.radius(crate::overlay::chrome::inner_radius(8.0));
    let option_fill = |selected: bool| {
      if selected {
        theme::SELECTED
      } else {
        theme::HOVER
      }
    };
    let option_ink = |selected: bool| {
      if selected {
        theme::SELECTED_ICON
      } else {
        theme::TEXT
      }
    };
    if self.color_open || self.width_open {
      let panel = self.picker_rect(controller, rect);
      let panel_radius = m.radius(crate::overlay::chrome::PANEL_RADIUS);
      scene.shadow(panel, panel_radius);
      scene.rounded(panel, panel_radius, theme::OUTLINE, 255)?;
      scene.rounded(inset(panel, 1), panel_radius - 1.0, theme::SURFACE, 255)?;
    }
    if self.color_open {
      let panel = self.picker_rect(controller, rect);
      let swatch_inset = m.px(3);
      for index in 0..12 {
        let cell = color_cell(panel, index, m);
        let alpha = self.motion.hover_alpha(20 + index as usize, now);
        if self.color == index as usize || alpha > 0 {
          scene.rounded(
            cell,
            (cell.right - cell.left) as f32 / 2.0,
            theme::TEXT,
            if self.color == index as usize {
              255
            } else {
              alpha
            },
          )?;
        }
        scene.rounded(
          inset(cell, swatch_inset),
          (cell.right - cell.left - swatch_inset * 2) as f32 / 2.0,
          COLORS[index as usize],
          255,
        )?;
      }
    }
    if self.width_open {
      let panel = self.picker_rect(controller, rect);
      if font_mode {
        if self.text_style_open {
          for (index, appearance) in [
            TextAppearance::Plain,
            TextAppearance::Outline,
            TextAppearance::Background,
          ]
          .into_iter()
          .enumerate()
          {
            let cell = appearance_cell(panel, index as i32, m);
            let selected = self.text_appearance == appearance;
            let alpha = self.motion.hover_alpha(46 + index, now);
            if selected || alpha > 0 {
              scene.rounded(
                cell,
                option_radius,
                option_fill(selected),
                if selected { 255 } else { alpha },
              )?;
            }
            scene.text(cell, strings.text_appearances[index], option_ink(selected))?;
          }
        } else {
          let pad = m.px(8);
          scene.font_sample(
            RECT {
              left: panel.left + pad,
              right: panel.right - pad,
              top: panel.top + pad,
              bottom: panel.top + m.px(88),
            },
            self.font_sample_size(self.hover_key),
          )?;
          for (index, size) in FONT_SIZES.into_iter().enumerate() {
            let cell = font_cell(panel, index as i32, m);
            let selected = self.text_size == size;
            let alpha = self.motion.hover_alpha(40 + index, now);
            if selected || alpha > 0 {
              scene.rounded(
                cell,
                option_radius,
                option_fill(selected),
                if selected { 255 } else { alpha },
              )?;
            }
            scene.text(cell, &size.to_string(), option_ink(selected))?;
          }
          let style = style_menu_button(panel, m);
          let alpha = self.motion.hover_alpha(50, now);
          if alpha > 0 {
            scene.rounded(style, option_radius, theme::HOVER, alpha)?;
          }
          scene.text(style, strings.text_style, theme::MUTED)?;
        }
      } else {
        for (index, width) in [3, 6, 10].into_iter().enumerate() {
          let row = width_row(panel, index as i32, m);
          let selected = self.width == width;
          let alpha = self.motion.hover_alpha(16 + index, now);
          if selected || alpha > 0 {
            scene.rounded(
              row,
              option_radius,
              option_fill(selected),
              if selected { 255 } else { alpha },
            )?;
          }
          let ink = option_ink(selected);
          // Samples show actual capture-pixel sizes, so only their placement scales.
          let center = (row.top + row.bottom) / 2;
          let painted = self.painted_width(width);
          scene.fill(
            RECT {
              left: row.left + m.px(12),
              right: row.left + m.px(48),
              top: center - painted / 2,
              bottom: center + (painted + 1) / 2,
            },
            ink,
          )?;
          scene.text(
            RECT {
              left: row.left + m.px(55),
              ..row
            },
            if self.highlighter_mode() {
              strings.highlighter_width_options[index]
            } else {
              strings.width_options[index]
            },
            ink,
          )?;
        }
      }
    }
    if let Some((_, button)) = self.toolbar_error() {
      let badge = error_badge_rect(toolbar, button, m);
      toolbar_scene.rounded(badge, m.badge as f32 / 2.0, theme::SURFACE, 255)?;
      let ring = m.px(2);
      toolbar_scene.rounded(
        inset(badge, ring),
        (m.badge - ring * 2) as f32 / 2.0,
        theme::ACCENT,
        255,
      )?;
      toolbar_scene.styled_text(
        badge,
        "!",
        crate::overlay::chrome::TextStyle {
          size: 11.0,
          semibold: true,
          leading: false,
        },
        [255; 3],
      )?;
    }
    let paint = Self::elevated_bounds(
      controller,
      self.toolbar_error().map_or(toolbar, |(_, button)| {
        cache::union(toolbar, error_badge_rect(toolbar, button, m))
      }),
      m,
    );
    let toolbar_offset = self
      .motion
      .toolbar_offset(now)
      .min((controller.height - paint.bottom).max(0));
    self.chrome.render_with_offset(
      dc,
      paint,
      &toolbar_scene,
      self.motion.toolbar_alpha(now),
      toolbar_offset,
    )?;
    if self.color_open || self.width_open {
      let panel = self.picker_rect(controller, rect);
      let paint = self.picker_region(controller, rect);
      let offset = self.motion.popup_offset(now);
      let offset = if panel.bottom <= toolbar.top {
        offset
      } else {
        -offset
      };
      let offset = offset.clamp(-paint.top.max(0), (controller.height - paint.bottom).max(0));
      self.popup_chrome.render_with_offset(
        dc,
        paint,
        &scene,
        self.motion.popup_alpha(now),
        offset,
      )?;
    }
    if let Some((key, button)) = self.toolbar_error() {
      let alpha = (self.error_fade.value(now) * 255.0).round() as u8;
      if alpha > 0 {
        let message = crate::i18n::native_settings(&self.lang, key).map_err(anyhow::Error::msg)?;
        let bounds = toolbar_bounds(controller, rect, &self.monitors);
        let max_width = m.px(ERROR_TOOLTIP_WIDTH).min(bounds.right - bounds.left);
        let (pad_x, pad_y) = (m.px(12), m.px(10));
        let style = crate::overlay::chrome::TextStyle::DETAILS;
        let (width, height) = self.error_chrome.measure_text_box(
          &message,
          style.scaled(m.scale),
          (max_width - pad_x * 2).max(1) as f32,
        )?;
        let panel = error_tooltip_rect(
          toolbar,
          button,
          bounds,
          (width.ceil() as i32 + pad_x * 2).min(max_width),
          (height.ceil() as i32 + pad_y * 2).min(m.px(ERROR_TOOLTIP_HEIGHT)),
          m,
        );
        let mut tooltip = crate::overlay::chrome::Scene::scaled(m.scale)?;
        tooltip.rounded(panel, m.radius(8.0), theme::OUTLINE, 255)?;
        tooltip.rounded(inset(panel, 1), m.radius(8.0) - 1.0, theme::SURFACE, 255)?;
        tooltip.styled_text(
          RECT {
            left: panel.left + pad_x,
            right: panel.right - pad_x,
            top: panel.top + pad_y,
            bottom: panel.bottom - pad_y,
          },
          &message,
          style,
          theme::TEXT,
        )?;
        let offset = ((1.0 - self.error_fade.value(now)) * 4.0 * m.scale).round() as i32
          * if panel.bottom <= toolbar.top { 1 } else { -1 };
        self.error_chrome.render_with_offset(
          dc,
          panel,
          &tooltip,
          alpha,
          offset.clamp(-panel.top.max(0), (controller.height - panel.bottom).max(0)),
        )?;
      }
    }
    self.draw_button_tip(controller, dc, rect, toolbar, m)?;
    #[cfg(debug_assertions)]
    theme::debug_sample_surface(dc, true, toolbar.left + m.px(20), toolbar.top + m.px(4));
    Ok(())
  }
}
