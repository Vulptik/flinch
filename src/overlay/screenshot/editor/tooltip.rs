//! Hover tooltips naming each toolbar button and its shortcut, as Windows toolbars show them.

use super::layout::{Metrics, button_tooltip_rect, inset, toolbar_bounds};
use super::{ACTIONS, AnnotationEditor, ERROR_TOOLTIP_WIDTH};
use crate::overlay::motion::Tween;
use crate::overlay::selection::Rect;
use crate::overlay::selection_core::SelectionControllerState;
use crate::overlay::theme;
use anyhow::Result;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::HDC;

/// The Windows tooltip initial delay; moving to a neighbor while one shows switches at once.
pub(super) const TOOLTIP_DELAY: Duration = Duration::from_millis(500);
/// Crossing the gap between buttons keeps tooltips in their shown state.
pub(super) const RESHOW_GRACE: Duration = Duration::from_millis(300);

/// Catalog copy for a toolbar button: its name, then its shortcut in parentheses when it has one.
pub(super) fn tooltip_key(button: i32) -> Option<&'static str> {
  Some(match button {
    0 => "annotation_tool_adjust_area",
    12 => "annotation_tool_select",
    1 => "annotation_tool_pen",
    8 => "annotation_tool_highlighter",
    2 => "annotation_tool_arrow",
    3 => "annotation_tool_rectangle",
    4 => "annotation_tool_ellipse",
    16 => "annotation_tool_blur",
    13 => "annotation_tool_text",
    14 => "annotation_tool_marker",
    7 => "annotation_tool_color",
    11 => "annotation_tool_size",
    5 => "annotation_tool_undo",
    6 => "annotation_tool_redo",
    _ => return None,
  })
}

pub(super) struct ButtonTip {
  /// The hovered button and when the hover began.
  hovered: Option<(i32, Instant)>,
  /// The button whose tooltip is drawn, kept while it fades out.
  shown: Option<i32>,
  showing: bool,
  /// When a shown tooltip last lost its button, for crossing gaps without the delay.
  hidden_at: Option<Instant>,
  /// A clicked button stays quiet until the pointer leaves it.
  dismissed: Option<i32>,
  fade: Tween,
  frame_pending: bool,
  chrome: crate::overlay::chrome::Renderer,
}

impl ButtonTip {
  pub fn new() -> Self {
    Self {
      hovered: None,
      shown: None,
      showing: false,
      hidden_at: None,
      dismissed: None,
      fade: Tween::new(0.0, Duration::from_millis(140), Instant::now()),
      frame_pending: false,
      chrome: crate::overlay::chrome::Renderer::default(),
    }
  }

  /// Advances the tooltip for the button under the pointer, or `None` when none may show one.
  pub fn update(&mut self, candidate: Option<i32>, pressed: bool, now: Instant, animate: bool) {
    if pressed {
      self.dismissed = candidate;
      self.hovered = None;
      self.shown = None;
      self.showing = false;
      self.hidden_at = None;
      self.fade.target(0.0, now, false);
      self.frame_pending = false;
      return;
    }
    if self.dismissed.is_some() && candidate != self.dismissed {
      self.dismissed = None;
    }
    match candidate.filter(|button| Some(*button) != self.dismissed) {
      None => {
        self.hovered = None;
        if self.showing {
          self.hidden_at = Some(now);
        }
        self.showing = false;
      },
      Some(button) => {
        if self.hovered.map(|(hovered, _)| hovered) != Some(button) {
          self.hovered = Some((button, now));
          let recent = self
            .hidden_at
            .is_some_and(|hidden| now.saturating_duration_since(hidden) < RESHOW_GRACE);
          if self.showing || recent {
            self.shown = Some(button);
            self.showing = true;
          }
        }
        if self
          .hovered
          .is_some_and(|(_, since)| now.saturating_duration_since(since) >= TOOLTIP_DELAY)
        {
          self.shown = Some(button);
          self.showing = true;
        }
      },
    }
    self
      .fade
      .target(u8::from(self.showing) as f32, now, animate);
    if !self.showing && self.fade.value(now) == 0.0 {
      self.shown = None;
    }
    self.frame_pending = animate && self.fade.active(now);
  }

  /// Whether a frame is still owed: a fade in progress, or a hover waiting for its delay.
  pub fn active(&self) -> bool {
    self.frame_pending || (self.hovered.is_some() && !self.showing)
  }

  /// Whether the next tick must repaint; waiting for the delay paints nothing.
  pub fn repaint_due(&self, now: Instant) -> bool {
    self.frame_pending
      || (!self.showing
        && self
          .hovered
          .is_some_and(|(_, since)| now.saturating_duration_since(since) >= TOOLTIP_DELAY))
  }

  #[cfg(test)]
  pub fn shown(&self) -> Option<i32> {
    self.shown.filter(|_| self.showing)
  }

  #[cfg(test)]
  pub fn show_now(&mut self, button: i32) {
    self.hovered = Some((button, Instant::now()));
    self.shown = Some(button);
    self.showing = true;
    self.fade.target(1.0, Instant::now(), false);
  }
}

impl AnnotationEditor {
  /// The button that may show a name tooltip: hovered, idle, and not covered by other chrome.
  fn tooltip_candidate(&self, controller: &SelectionControllerState) -> Option<i32> {
    if !controller.interaction.is_confirmed_idle()
      || self.color_open
      || self.width_open
      || self.error_details_pinned
    {
      return None;
    }
    let error_button = self.toolbar_error().map(|(_, button)| button);
    self
      .current_hover_key(controller)
      .filter(|key| ACTIONS.contains(key) && Some(*key) != error_button)
  }

  pub(super) fn update_button_tip(&mut self, controller: &SelectionControllerState, now: Instant) {
    let candidate = self.tooltip_candidate(controller);
    let animate = self.motion.enabled();
    self
      .button_tip
      .update(candidate, self.toolbar_press, now, animate);
  }

  pub(super) fn draw_button_tip(
    &mut self,
    controller: &SelectionControllerState,
    dc: HDC,
    rect: Rect,
    toolbar: RECT,
    m: Metrics,
  ) -> Result<()> {
    let Some(button) = self.button_tip.shown else {
      return Ok(());
    };
    let now = Instant::now();
    let fade = self.button_tip.fade.value(now);
    let alpha = (fade * 255.0).round() as u8;
    let key = if button == 11 && self.blur_mode() {
      Some("annotation_tool_redact_text")
    } else {
      tooltip_key(button)
    };
    let Some(key) = key.filter(|_| alpha > 0) else {
      return Ok(());
    };
    let label = crate::i18n::native_settings(&self.lang, key).map_err(anyhow::Error::msg)?;
    let bounds = toolbar_bounds(controller, rect, &self.monitors);
    let max_width = m.px(ERROR_TOOLTIP_WIDTH).min(bounds.right - bounds.left);
    let (pad_x, pad_y) = (m.px(8), m.px(5));
    let style = crate::overlay::chrome::TextStyle::COORDINATES;
    let (width, height) = self.button_tip.chrome.measure_text_box(
      &label,
      style.scaled(m.scale),
      (max_width - pad_x * 2).max(1) as f32,
    )?;
    let panel = button_tooltip_rect(
      toolbar,
      button,
      bounds,
      (width.ceil() as i32 + pad_x * 2).min(max_width),
      height.ceil() as i32 + pad_y * 2,
      m,
    );
    let mut scene = crate::overlay::chrome::Scene::scaled(m.scale)?;
    scene.rounded(panel, m.radius(6.0), theme::OUTLINE, 255)?;
    scene.rounded(inset(panel, 1), m.radius(6.0) - 1.0, theme::SURFACE, 255)?;
    scene.styled_text(
      RECT {
        left: panel.left + pad_x,
        right: panel.right - pad_x,
        top: panel.top + pad_y,
        bottom: panel.bottom - pad_y,
      },
      &label,
      style,
      theme::TEXT,
    )?;
    let offset = ((1.0 - fade) * 4.0 * m.scale).round() as i32
      * if panel.bottom <= toolbar.top { 1 } else { -1 };
    self.button_tip.chrome.render_with_offset(
      dc,
      panel,
      &scene,
      alpha,
      offset.clamp(-panel.top.max(0), (controller.height - panel.bottom).max(0)),
    )
  }
}
