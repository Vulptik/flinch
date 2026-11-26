//! Optional screenshot-only input and chrome, owned by the selection window thread.
//!
//! This module owns editor state and initialization. Its private `editor/` modules
//! implement document history, input, objects, text, toolbar and rendering.
//! All state and native resources remain on the selection window owner thread.

use windows::Win32::Foundation::RECT;

use crate::overlay::selection::Rect;
use crate::screenshot::annotations::{Annotation, TextAppearance, Tool};

pub(crate) use redact::READY as REDACT_READY;

const FONT_SIZES: [i32; 6] = [12, 16, 24, 32, 48, 64];

#[derive(Clone, Copy)]
enum DrawingLimit {
  Drawings,
  Stroke,
  Marker,
  Identifier,
}
const CELL: i32 = 38;
const CELL_GAP: i32 = 4;
const BUTTONS: i32 = 14;
/// Toolbar order of the action indices; 8 is the Highlighter, added after the others were numbered.
const ACTIONS: [i32; 14] = [0, 12, 1, 8, 2, 3, 4, 16, 13, 14, 7, 11, 5, 6];
const GROUP_STARTS: [i32; 3] = [2, 10, 12];
const GROUP_GAP: i32 = 8;
/// The same as the vertical padding, so the end buttons sit as far from the edge as the others.
const PANEL_PAD: i32 = 6;
const GRIP_WIDTH: i32 = 32;
const TOOLBAR_VERTICAL_PAD: i32 = 6;
const ERROR_BADGE_SIZE: i32 = 18;
const ERROR_TOOLTIP_WIDTH: i32 = 300;
const ERROR_TOOLTIP_HEIGHT: i32 = 100;
const COLORS: [[u8; 3]; 12] = [
  [239, 68, 68],
  [250, 204, 21],
  [59, 130, 246],
  [255, 255, 255],
  [0, 0, 0],
  [156, 163, 175],
  [249, 115, 22],
  [34, 197, 94],
  [6, 182, 212],
  [168, 85, 247],
  [236, 72, 153],
  [132, 204, 22],
];
const TOOLS: [Tool; 5] = [
  Tool::Select,
  Tool::Pen,
  Tool::Arrow,
  Tool::Rectangle,
  Tool::Ellipse,
];

struct ObjectMove {
  index: usize,
  before: Annotation,
  preview: Annotation,
  start: (i32, i32),
}

struct TextEdit {
  field: super::text_field::TextField,
  before: Option<Annotation>,
  point: (i32, i32),
  wrap_width: i32,
  preview: Option<Annotation>,
  resizing: Option<(i32, i32)>,
  moving: Option<((i32, i32), (i32, i32))>,
  input_rect: RECT,
  preview_point: (i32, i32),
}

/// The capture under the drawings, which blur regions read.
pub(crate) struct FrozenPixels {
  pub(crate) rgba: std::sync::Arc<Vec<u8>>,
  pub(crate) width: u32,
  pub(crate) height: u32,
}

pub struct AnnotationEditor {
  frozen: Option<FrozenPixels>,
  crop_anchor: Option<Rect>,
  group_selection: Vec<u64>,
  group_move: Vec<ObjectMove>,
  text_edit: Option<TextEdit>,
  text_size: i32,
  text_appearance: TextAppearance,
  resizing_text: bool,
  last_text_click: Option<(u64, std::time::Instant, (f64, f64))>,
  text_error: bool,
  scene_cache: super::cache::SceneCache,
  hover_key: Option<i32>,
  motion: crate::overlay::motion::Motion,
  error_fade: crate::overlay::motion::Tween,
  error_frame_pending: bool,
  error_details_pinned: bool,
  error_button: Option<i32>,
  error_chrome: crate::overlay::chrome::Renderer,
  button_tip: tooltip::ButtonTip,
  object_mode: bool,
  selected: Option<u64>,
  moving: Option<ObjectMove>,
  geometry_handle: Option<usize>,
  node_mode: Option<u64>,
  node_indices: Vec<usize>,
  selected_node: Option<usize>,
  last_path_click: Option<(u64, std::time::Instant, (f64, f64))>,
  pub annotations: Vec<Annotation>,
  history: super::history::History,
  next_id: u64,
  document_revision: u64,
  draft: Option<Annotation>,
  tool: Tool,
  color: usize,
  width: i32,
  toolbar_press: bool,
  toolbar_position: Option<(i32, i32)>,
  toolbar_drag: Option<(RECT, (i32, i32))>,
  width_open: bool,
  text_style_open: bool,
  color_open: bool,
  limited: Option<DrawingLimit>,
  /// A Redact All Text outcome shown on its button: nothing found, too large or failed.
  redact_notice: Option<&'static str>,
  redaction: redact::Redaction,
  pen_compactions: u8,
  render_failed: bool,
  lang: String,
  monitors: Vec<crate::monitors::MonitorInfo>,
  cursors: Option<super::cursors::ToolCursors>,
  chrome: crate::overlay::chrome::Renderer,
  popup_chrome: crate::overlay::chrome::Renderer,
  handle_chrome: crate::overlay::chrome::Renderer,
  pub(crate) diagnostics: Option<super::performance::Diagnostics>,
  pub(crate) paint_sample: super::performance::PaintSample,
}

impl AnnotationEditor {
  pub(crate) fn with_frozen_pixels(mut self, frozen: FrozenPixels) -> Self {
    self.frozen = Some(frozen);
    self
  }

  pub fn new(lang: String) -> Self {
    let monitors = crate::monitors::list_monitors();
    if monitors.is_empty() {
      tracing::warn!("No monitor bounds available for annotation toolbar; using capture bounds");
    }
    Self {
      frozen: None,
      crop_anchor: None,
      group_selection: Vec::new(),
      group_move: Vec::new(),
      text_edit: None,
      text_size: 24,
      text_appearance: TextAppearance::Plain,
      resizing_text: false,
      last_text_click: None,
      text_error: false,
      scene_cache: super::cache::SceneCache::default(),
      hover_key: None,
      motion: crate::overlay::motion::Motion::new(),
      error_fade: crate::overlay::motion::Tween::new(
        0.0,
        std::time::Duration::from_millis(140),
        std::time::Instant::now(),
      ),
      error_frame_pending: false,
      error_details_pinned: false,
      error_button: None,
      error_chrome: crate::overlay::chrome::Renderer::default(),
      button_tip: tooltip::ButtonTip::new(),
      object_mode: false,
      selected: None,
      moving: None,
      geometry_handle: None,
      node_mode: None,
      node_indices: Vec::new(),
      selected_node: None,
      last_path_click: None,
      annotations: Vec::new(),
      history: super::history::History::default(),
      next_id: 1,
      document_revision: 0,
      draft: None,
      tool: Tool::Select,
      color: 0,
      width: 3,
      toolbar_press: false,
      toolbar_position: None,
      toolbar_drag: None,
      width_open: false,
      text_style_open: false,
      color_open: false,
      limited: None,
      redact_notice: None,
      redaction: redact::Redaction::default(),
      pen_compactions: 0,
      render_failed: false,
      lang,
      monitors,
      cursors: None,
      chrome: crate::overlay::chrome::Renderer::default(),
      popup_chrome: crate::overlay::chrome::Renderer::default(),
      handle_chrome: crate::overlay::chrome::Renderer::default(),
      diagnostics: super::performance::enabled().then(super::performance::Diagnostics::default),
      paint_sample: Default::default(),
    }
  }

  /// A new crop started from inside the editor. Gestures, selection and popovers end, while the
  /// drawings, undo history, tool and crop anchor stay, so the new crop is one undoable step.
  pub fn reselect(&mut self) {
    self.redaction.cancel();
    self.group_selection.clear();
    self.group_move.clear();
    self.text_style_open = false;
    self.resizing_text = false;
    self.geometry_handle = None;
    self.last_text_click = None;
    self.node_mode = None;
    self.node_indices.clear();
    self.selected_node = None;
    self.last_path_click = None;
    self.selected = None;
    self.moving = None;
    self.draft = None;
    self.width_open = false;
    self.color_open = false;
    self.toolbar_press = false;
    self.toolbar_drag = None;
    self.error_details_pinned = false;
    self.scene_cache.invalidate();
  }

  pub fn clear(&mut self) {
    self.redaction.cancel();
    self.clear_toolbar_error();
    self.crop_anchor = None;
    self.group_selection.clear();
    self.group_move.clear();
    self.text_style_open = false;
    self.resizing_text = false;
    self.geometry_handle = None;
    self.last_text_click = None;
    self.node_mode = None;
    self.node_indices.clear();
    self.selected_node = None;
    self.last_path_click = None;
    self.text_edit = None;
    self.text_error = false;
    self.object_mode = false;
    self.selected = None;
    self.moving = None;
    self.group_move.clear();
    self.scene_cache.invalidate();
    self.annotations.clear();
    self.history = super::history::History::default();
    self.next_id = 1;
    self.document_changed();
    self.draft = None;
    self.tool = Tool::Select;
    self.width_open = false;
    self.color_open = false;
    self.toolbar_press = false;
    self.toolbar_position = None;
    self.toolbar_drag = None;
    self.limited = None;
  }
}

mod cursor;
mod document;
mod input;
mod layout;
mod objects;
mod redact;
mod render;
mod text;
mod toolbar;
mod tooltip;

#[cfg(test)]
#[path = "replay_frames.rs"]
mod replay_frames;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod label_editor_tests;

#[cfg(test)]
mod limit_tests;

#[cfg(test)]
mod tooltip_tests;
