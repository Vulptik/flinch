// Shared overlay selection state machine, hit-test, and geometry

/// Minimum selection size in pixels
pub const MIN_SELECTION_SIZE: f64 = 10.0;
/// Minimum video and GIF selection: the smallest area the native engine's encoders record
/// (`Region::MIN_SIZE`, measured with `cargo xtask test:video:smallest`).
pub const RECORDING_MIN_SELECTION_SIZE: f64 = flinch_video_engine::types::Region::MIN_SIZE as f64;
/// Edge margin in pixels (clamp, do not reject). Reserved for future use.
#[allow(dead_code)]
pub const EDGE_MARGIN: f64 = 5.0;
/// Resize handle size in pixels
pub const HANDLE_SIZE: f64 = 8.0;
/// Hit area for handles (>= HANDLE_SIZE)
pub const HANDLE_HIT_MARGIN: f64 = 8.0;

/// Distance threshold in pixels to distinguish click from drag.
/// Chosen to accommodate minor hand tremor while being responsive.
pub const DRAG_THRESHOLD_PX: f64 = 5.0;

/// Final selection result
#[derive(Debug, serde::Serialize, Clone)]
pub struct Selection {
  pub x: u32,
  pub y: u32,
  pub width: u32,
  pub height: u32,
  /// Exe stem of the process whose window covers the selection center (e.g. "Code", "chrome").
  /// `None` for free-draw selections where no window is detected, or on lookup failure.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub source_app: Option<String>,
}

/// Rectangle in f64 (x, y, width, height)
#[derive(Debug, Clone, Copy)]
pub struct Rect {
  pub x: f64,
  pub y: f64,
  pub w: f64,
  pub h: f64,
}

impl Rect {
  pub fn to_x1y1x2y2(self) -> (f64, f64, f64, f64) {
    (self.x, self.y, self.x + self.w, self.y + self.h)
  }

  pub fn from_x1y1x2y2(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
    let x_min = x1.min(x2);
    let y_min = y1.min(y2);
    let x_max = x1.max(x2);
    let y_max = y1.max(y2);
    Self {
      x: x_min,
      y: y_min,
      w: (x_max - x_min).max(1.0),
      h: (y_max - y_min).max(1.0),
    }
  }
}

/// Resize handles (corners + edges)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
  N,
  S,
  E,
  W,
  NW,
  NE,
  SW,
  SE,
}

/// Interaction mode for area selection with window snapping support.
///
/// State transitions:
/// - Idle(snapped=None) -> [cursor move, window detected] -> Idle(snapped=Some(rect))
/// - Idle(snapped=Some) -> [cursor move, no window] -> Idle(snapped=None)
/// - Idle(any) -> [mouse down] -> PendingDecision(press_pos, snapped)
/// - PendingDecision -> [mouse move > threshold] -> FreeDraw(start=press_pos, current=cursor)
/// - PendingDecision -> [mouse up, snapped=Some] -> Confirmed(rect=snapped, from_snap=true)
/// - PendingDecision -> [mouse up, snapped=None] -> Idle (click on empty area)
/// - FreeDraw -> [mouse up, size >= minimum] -> Confirmed(rect, from_snap=false)
/// - FreeDraw -> [mouse up, size < minimum] -> Idle (10 px screenshots, 34 px recordings)
/// - Confirmed -> [click inside] -> Confirmed(moving=Some)
/// - Confirmed -> [click on handle] -> Confirmed(resizing=Some)
/// - Confirmed -> [click outside] -> FreeDraw (restart)
#[derive(Debug, Clone)]
pub enum SelectionInteraction {
  /// No interaction - cursor moving freely, may show window snap preview
  Idle {
    /// Window bounds detected under cursor (screen coordinates)
    snapped_window: Option<Rect>,
  },

  /// Mouse button pressed, waiting to determine click vs drag
  PendingDecision {
    /// Position where mouse was pressed
    press_pos: (f64, f64),
    /// Window that was snapped at press time (if any)
    snapped_window: Option<Rect>,
  },

  /// User is dragging to draw a free-form rectangle
  FreeDraw {
    /// Starting corner of the selection
    start: (f64, f64),
    /// Current corner (follows cursor)
    current: (f64, f64),
  },

  /// Selection confirmed, showing resize handles
  Confirmed {
    rect: Rect,
    /// Whether this came from window snap (vs free-draw)
    #[allow(dead_code)]
    from_snap: bool,
    moving: Option<(f64, f64)>,
    resizing: Option<(Handle, Rect)>,
  },
}

impl Default for SelectionInteraction {
  fn default() -> Self {
    Self::Idle {
      snapped_window: None,
    }
  }
}

impl SelectionInteraction {
  /// Get the current selection rectangle if any exists
  pub fn get_current_rect(
    &self,
    window_width: f64,
    window_height: f64,
  ) -> Option<(f64, f64, f64, f64)> {
    match self {
      Self::Idle { .. } | Self::PendingDecision { .. } => None,
      Self::FreeDraw { start, current } => {
        let (x1, y1) = *start;
        let (x2, y2) = *current;
        let x_min = x1.min(x2).max(0.0).min(window_width);
        let y_min = y1.min(y2).max(0.0).min(window_height);
        let x_max = x1.max(x2).max(0.0).min(window_width);
        let y_max = y1.max(y2).max(0.0).min(window_height);
        Some((x_min, y_min, x_max, y_max))
      },
      Self::Confirmed { rect, .. } => Some(rect.to_x1y1x2y2()),
    }
  }

  /// Check if we're in a confirmed state without any active manipulation
  pub fn is_confirmed_idle(&self) -> bool {
    matches!(
      self,
      Self::Confirmed {
        moving: None,
        resizing: None,
        ..
      }
    )
  }
}

/// Calculate Euclidean distance between two points
pub fn distance(p1: (f64, f64), p2: (f64, f64)) -> f64 {
  let dx = p2.0 - p1.0;
  let dy = p2.1 - p1.1;
  (dx * dx + dy * dy).sqrt()
}

/// Result of hit-testing the selection
#[derive(Debug, Clone, Copy)]
pub enum HitResult {
  Outside,
  Inside,
  Handle(Handle),
}

/// Modifiers passed from app (shift, alt only)
#[derive(Debug, Clone, Copy, Default)]
pub struct Modifiers {
  pub shift: bool,
  pub alt: bool,
}

/// Nudge direction for arrow keys
#[derive(Debug, Clone, Copy)]
pub enum NudgeDirection {
  Up,
  Down,
  Left,
  Right,
}

/// Parameters for the renderer
#[derive(Debug, Default)]
pub struct RenderParams {
  pub selection_rect: Option<(f64, f64, f64, f64)>,
  pub draw_handles: bool,
  /// Size label: (x, y, width, height) in pixels
  pub size_label: Option<(u32, u32, u32, u32)>,
  pub cursor_pos: Option<(f64, f64)>,
  /// Position of the size label (calculated to anchor to selection)
  pub label_position: Option<(f64, f64)>,
  /// Snapped window preview rect for hover-to-snap rendering (Idle/PendingDecision states)
  pub snapped_preview: Option<Rect>,
  /// Monitor rects in overlay-local coords for multi-monitor dead zone masking
  pub monitor_rects: Vec<crate::overlay::types::MonitorRect>,
}

/// Overlay state machine. Cursor position is held in ActiveOverlay (app) and passed in.
#[derive(Debug, Clone)]
pub enum OverlayState {
  Idle,
  Drawing {
    #[allow(dead_code)]
    start: (f64, f64),
    #[allow(dead_code)]
    current: (f64, f64),
  },
  OnHold {
    rect: Rect,
  },
}

impl OverlayState {
  pub fn new_idle() -> Self {
    Self::Idle
  }
}

// --- Hit-test: handles first, then inside, else outside ---

pub fn hit_test(rect: &Rect, pos: (f64, f64)) -> HitResult {
  let (px, py) = pos;
  let r = rect;
  let hs = HANDLE_HIT_MARGIN / 2.0; // half

  // Corners first (squares of side HANDLE_HIT_MARGIN centered on corner)
  if px >= r.x - hs && px <= r.x + hs && py >= r.y - hs && py <= r.y + hs {
    return HitResult::Handle(Handle::NW);
  }
  if px >= r.x + r.w - hs && px <= r.x + r.w + hs && py >= r.y - hs && py <= r.y + hs {
    return HitResult::Handle(Handle::NE);
  }
  if px >= r.x - hs && px <= r.x + hs && py >= r.y + r.h - hs && py <= r.y + r.h + hs {
    return HitResult::Handle(Handle::SW);
  }
  if px >= r.x + r.w - hs && px <= r.x + r.w + hs && py >= r.y + r.h - hs && py <= r.y + r.h + hs {
    return HitResult::Handle(Handle::SE);
  }
  // Edges (exclude corner zones to avoid double-hit)
  if py >= r.y - hs && py <= r.y + hs && px >= r.x + hs && px <= r.x + r.w - hs {
    return HitResult::Handle(Handle::N);
  }
  if py >= r.y + r.h - hs && py <= r.y + r.h + hs && px >= r.x + hs && px <= r.x + r.w - hs {
    return HitResult::Handle(Handle::S);
  }
  if px >= r.x - hs && px <= r.x + hs && py >= r.y + hs && py <= r.y + r.h - hs {
    return HitResult::Handle(Handle::W);
  }
  if px >= r.x + r.w - hs && px <= r.x + r.w + hs && py >= r.y + hs && py <= r.y + r.h - hs {
    return HitResult::Handle(Handle::E);
  }
  // Inside
  if px >= r.x && px <= r.x + r.w && py >= r.y && py <= r.y + r.h {
    return HitResult::Inside;
  }
  HitResult::Outside
}

// --- Move ---

pub fn start_move(rect: &Rect, pos: (f64, f64)) -> (f64, f64) {
  (pos.0 - rect.x, pos.1 - rect.y)
}

pub fn update_move(
  rect: Rect,
  offset: (f64, f64),
  pos: (f64, f64),
  window_width: f64,
  window_height: f64,
) -> Rect {
  let mut x = pos.0 - offset.0;
  let mut y = pos.1 - offset.1;
  x = x.max(0.0).min(window_width - rect.w);
  y = y.max(0.0).min(window_height - rect.h);
  Rect {
    x,
    y,
    w: rect.w,
    h: rect.h,
  }
}

// --- Resize ---

pub fn start_resize(rect: Rect, handle: Handle, _pos: (f64, f64)) -> (Handle, Rect) {
  (handle, rect)
}

pub fn update_resize(
  rect: Rect,
  handle: Handle,
  start_rect: Rect,
  pos: (f64, f64),
  mods: Modifiers,
  window_width: f64,
  window_height: f64,
) -> Rect {
  let mut r = rect;
  let sr = start_rect;
  let ratio = if sr.h >= 1.0 { sr.w / sr.h } else { 1.0 };
  let (px, py) = (
    pos.0.max(0.0).min(window_width),
    pos.1.max(0.0).min(window_height),
  );

  match handle {
    Handle::NW => {
      let fx = rect.x + rect.w;
      let fy = rect.y + rect.h;
      let new_w = (fx - px).max(1.0);
      let new_h = (fy - py).max(1.0);
      if mods.shift {
        let (w, h) = if new_w / new_h > ratio {
          (new_w, new_w / ratio)
        } else {
          (new_h * ratio, new_h)
        };
        r.x = fx - w;
        r.y = fy - h;
        r.w = w;
        r.h = h;
      } else {
        r.x = px.min(fx - 1.0);
        r.y = py.min(fy - 1.0);
        r.w = (fx - r.x).max(1.0);
        r.h = (fy - r.y).max(1.0);
      }
    },
    Handle::NE => {
      r.w = (px - rect.x).max(1.0);
      r.y = py.min(rect.y + rect.h - 1.0);
      r.h = (rect.y + rect.h - r.y).max(1.0);
      if mods.shift {
        r.h = r.w / ratio;
        r.y = (rect.y + rect.h) - r.h;
      }
    },
    Handle::SW => {
      r.x = px.min(rect.x + rect.w - 1.0);
      r.w = (rect.x + rect.w - r.x).max(1.0);
      r.h = (py - rect.y).max(1.0);
      if mods.shift {
        r.w = r.h * ratio;
        r.x = (rect.x + rect.w) - r.w;
      }
    },
    Handle::SE => {
      r.w = (px - rect.x).max(1.0);
      r.h = (py - rect.y).max(1.0);
      if mods.shift {
        let h_cand = r.h;
        let w_cand = r.w;
        if w_cand / ratio <= h_cand {
          r.w = w_cand;
          r.h = w_cand / ratio;
        } else {
          r.h = h_cand;
          r.w = h_cand * ratio;
        }
      }
    },
    Handle::N => {
      r.y = py.min(rect.y + rect.h - 1.0);
      r.h = (rect.y + rect.h - r.y).max(1.0);
      if mods.shift {
        let cx = rect.x + rect.w / 2.0;
        r.h = r.h.max(1.0);
        r.w = (r.h * ratio).max(1.0);
        r.x = cx - r.w / 2.0;
        r.y = (rect.y + rect.h) - r.h;
      }
    },
    Handle::S => {
      r.h = (py - rect.y).max(1.0);
      if mods.shift {
        let cx = rect.x + rect.w / 2.0;
        r.w = (r.h * ratio).max(1.0);
        r.x = cx - r.w / 2.0;
      }
    },
    Handle::W => {
      r.x = px.min(rect.x + rect.w - 1.0);
      r.w = (rect.x + rect.w - r.x).max(1.0);
      if mods.shift {
        let cy = rect.y + rect.h / 2.0;
        r.h = (r.w / ratio).max(1.0);
        r.y = cy - r.h / 2.0;
      }
    },
    Handle::E => {
      r.w = (px - rect.x).max(1.0);
      if mods.shift {
        let cy = rect.y + rect.h / 2.0;
        r.h = (r.w / ratio).max(1.0);
        r.y = cy - r.h / 2.0;
      }
    },
  }

  clamp_rect_to_window(r, window_width, window_height)
}

// --- Nudge ---

pub fn nudge(
  rect: Rect,
  dir: NudgeDirection,
  shift: bool,
  window_width: f64,
  window_height: f64,
) -> Rect {
  let d = if shift { 10.0 } else { 1.0 };
  let (mut x, mut y) = (rect.x, rect.y);
  match dir {
    NudgeDirection::Up => y = (y - d).max(0.0),
    NudgeDirection::Down => y = (y + d).min(window_height - rect.h),
    NudgeDirection::Left => x = (x - d).max(0.0),
    NudgeDirection::Right => x = (x + d).min(window_width - rect.w),
  }
  Rect {
    x,
    y,
    w: rect.w,
    h: rect.h,
  }
}

// --- Finalize ---

pub fn finalize_rect(rect: Rect, window_width: f64, window_height: f64) -> Option<Selection> {
  finalize_rect_at_least(rect, window_width, window_height, MIN_SELECTION_SIZE)
}

/// Like [`finalize_rect`], refusing selections narrower or shorter than `minimum`.
pub fn finalize_rect_at_least(
  rect: Rect,
  window_width: f64,
  window_height: f64,
  minimum: f64,
) -> Option<Selection> {
  let r = clamp_rect_to_window(rect, window_width, window_height);
  if r.w < minimum || r.h < minimum {
    return None;
  }
  let x = (r.x.max(0.0).min(window_width - 1.0)).round() as u32;
  let y = (r.y.max(0.0).min(window_height - 1.0)).round() as u32;
  let w = (r.w.max(minimum).min(window_width - x as f64)).round() as u32;
  let h = (r.h.max(minimum).min(window_height - y as f64)).round() as u32;
  Some(Selection {
    x,
    y,
    width: w,
    height: h,
    source_app: None,
  })
}

fn clamp_rect_to_window(rect: Rect, w: f64, h: f64) -> Rect {
  let mut r = rect;
  r.w = r.w.max(1.0);
  r.h = r.h.max(1.0);
  r.x = r.x.max(0.0).min(w - r.w);
  r.y = r.y.max(0.0).min(h - r.h);
  r.w = r.w.min(w - r.x);
  r.h = r.h.min(h - r.y);
  r
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { x, y, w, h }
  }

  // --- Rect ---

  #[test]
  fn test_rect_to_x1y1x2y2() {
    let r = make_rect(10.0, 20.0, 100.0, 50.0);
    assert_eq!(r.to_x1y1x2y2(), (10.0, 20.0, 110.0, 70.0));
  }

  #[test]
  fn test_rect_from_x1y1x2y2_normal() {
    let r = Rect::from_x1y1x2y2(10.0, 20.0, 110.0, 70.0);
    assert_eq!(r.x, 10.0);
    assert_eq!(r.y, 20.0);
    assert_eq!(r.w, 100.0);
    assert_eq!(r.h, 50.0);
  }

  #[test]
  fn test_rect_from_x1y1x2y2_reversed() {
    // x2 < x1, y2 < y1 - should normalize
    let r = Rect::from_x1y1x2y2(110.0, 70.0, 10.0, 20.0);
    assert_eq!(r.x, 10.0);
    assert_eq!(r.y, 20.0);
    assert_eq!(r.w, 100.0);
    assert_eq!(r.h, 50.0);
  }

  #[test]
  fn test_rect_from_x1y1x2y2_same_point() {
    // Collapsed to a point - minimum size is 1
    let r = Rect::from_x1y1x2y2(50.0, 50.0, 50.0, 50.0);
    assert_eq!(r.w, 1.0);
    assert_eq!(r.h, 1.0);
  }

  // --- distance ---

  #[test]
  fn test_distance_zero() {
    assert_eq!(distance((5.0, 5.0), (5.0, 5.0)), 0.0);
  }

  #[test]
  fn test_distance_horizontal() {
    assert_eq!(distance((0.0, 0.0), (3.0, 0.0)), 3.0);
  }

  #[test]
  fn test_distance_vertical() {
    assert_eq!(distance((0.0, 0.0), (0.0, 4.0)), 4.0);
  }

  #[test]
  fn test_distance_pythagoras() {
    // 3-4-5 triangle
    assert!((distance((0.0, 0.0), (3.0, 4.0)) - 5.0).abs() < 0.0001);
  }

  #[test]
  fn test_distance_negative_coords() {
    assert!((distance((-3.0, 0.0), (0.0, -4.0)) - 5.0).abs() < 0.0001);
  }

  // --- hit_test ---

  #[test]
  fn test_hit_test_outside() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    assert!(matches!(hit_test(&r, (50.0, 50.0)), HitResult::Outside));
    assert!(matches!(hit_test(&r, (400.0, 400.0)), HitResult::Outside));
  }

  #[test]
  fn test_hit_test_inside() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    // Well inside, far from handles
    assert!(matches!(hit_test(&r, (200.0, 175.0)), HitResult::Inside));
  }

  #[test]
  fn test_hit_test_nw_corner() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    assert!(matches!(
      hit_test(&r, (100.0, 100.0)),
      HitResult::Handle(Handle::NW)
    ));
  }

  #[test]
  fn test_hit_test_ne_corner() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    assert!(matches!(
      hit_test(&r, (300.0, 100.0)),
      HitResult::Handle(Handle::NE)
    ));
  }

  #[test]
  fn test_hit_test_sw_corner() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    assert!(matches!(
      hit_test(&r, (100.0, 250.0)),
      HitResult::Handle(Handle::SW)
    ));
  }

  #[test]
  fn test_hit_test_se_corner() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    assert!(matches!(
      hit_test(&r, (300.0, 250.0)),
      HitResult::Handle(Handle::SE)
    ));
  }

  #[test]
  fn test_hit_test_n_edge() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    // Midpoint of top edge, far enough from corners
    assert!(matches!(
      hit_test(&r, (200.0, 100.0)),
      HitResult::Handle(Handle::N)
    ));
  }

  #[test]
  fn test_hit_test_s_edge() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    assert!(matches!(
      hit_test(&r, (200.0, 250.0)),
      HitResult::Handle(Handle::S)
    ));
  }

  #[test]
  fn test_hit_test_w_edge() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    assert!(matches!(
      hit_test(&r, (100.0, 175.0)),
      HitResult::Handle(Handle::W)
    ));
  }

  #[test]
  fn test_hit_test_e_edge() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    assert!(matches!(
      hit_test(&r, (300.0, 175.0)),
      HitResult::Handle(Handle::E)
    ));
  }

  // --- start_move / update_move ---

  #[test]
  fn test_start_move_offset() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let offset = start_move(&r, (150.0, 130.0));
    assert_eq!(offset, (50.0, 30.0));
  }

  #[test]
  fn test_update_move_simple() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let offset = (50.0, 30.0);
    let moved = update_move(r, offset, (200.0, 160.0), 1920.0, 1080.0);
    // new x = 200 - 50 = 150, new y = 160 - 30 = 130
    assert_eq!(moved.x, 150.0);
    assert_eq!(moved.y, 130.0);
    assert_eq!(moved.w, 200.0);
    assert_eq!(moved.h, 150.0);
  }

  #[test]
  fn test_update_move_clamps_left_boundary() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let offset = (50.0, 0.0);
    // Trying to move to x = -100 (clamped to 0)
    let moved = update_move(r, offset, (-50.0, 100.0), 1920.0, 1080.0);
    assert_eq!(moved.x, 0.0);
  }

  #[test]
  fn test_update_move_clamps_right_boundary() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let offset = (0.0, 0.0);
    // Trying to move rect right beyond window (1920 - 200 = 1720 max)
    let moved = update_move(r, offset, (1900.0, 100.0), 1920.0, 1080.0);
    assert_eq!(moved.x, 1720.0);
  }

  #[test]
  fn test_update_move_clamps_top_boundary() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let offset = (0.0, 50.0);
    let moved = update_move(r, offset, (100.0, 0.0), 1920.0, 1080.0);
    assert_eq!(moved.y, 0.0);
  }

  #[test]
  fn test_update_move_clamps_bottom_boundary() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let offset = (0.0, 0.0);
    // 1080 - 150 = 930 max y
    let moved = update_move(r, offset, (100.0, 1000.0), 1920.0, 1080.0);
    assert_eq!(moved.y, 930.0);
  }

  // --- start_resize ---

  #[test]
  fn test_start_resize_returns_handle_and_rect() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let (handle, saved_rect) = start_resize(r, Handle::SE, (300.0, 250.0));
    assert!(matches!(handle, Handle::SE));
    assert_eq!(saved_rect.x, r.x);
    assert_eq!(saved_rect.y, r.y);
    assert_eq!(saved_rect.w, r.w);
    assert_eq!(saved_rect.h, r.h);
  }

  // --- update_resize ---

  #[test]
  fn test_update_resize_se_grows() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    // Drag SE handle to 350, 300
    let result = update_resize(
      r,
      Handle::SE,
      start_rect,
      (350.0, 300.0),
      mods,
      1920.0,
      1080.0,
    );
    assert!(result.w > 200.0);
    assert!(result.h > 150.0);
    assert_eq!(result.x, 100.0); // Origin unchanged
    assert_eq!(result.y, 100.0);
  }

  #[test]
  fn test_update_resize_nw_shrinks() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    // Drag NW handle inward (toward SE)
    let result = update_resize(
      r,
      Handle::NW,
      start_rect,
      (150.0, 130.0),
      mods,
      1920.0,
      1080.0,
    );
    // x and y should increase (moved from 100,100 toward 150,130)
    assert!(result.x > 100.0);
    assert!(result.y > 100.0);
  }

  #[test]
  fn test_update_resize_n_moves_top_edge() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    let result = update_resize(
      r,
      Handle::N,
      start_rect,
      (200.0, 80.0),
      mods,
      1920.0,
      1080.0,
    );
    // Top edge moved up, height increased
    assert!(result.y <= 100.0);
    assert!(result.h >= 150.0);
  }

  #[test]
  fn test_update_resize_s_moves_bottom_edge() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    let result = update_resize(
      r,
      Handle::S,
      start_rect,
      (200.0, 300.0),
      mods,
      1920.0,
      1080.0,
    );
    assert!(result.h > 150.0);
    assert_eq!(result.y, 100.0);
  }

  #[test]
  fn test_update_resize_e_moves_right_edge() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    let result = update_resize(
      r,
      Handle::E,
      start_rect,
      (400.0, 175.0),
      mods,
      1920.0,
      1080.0,
    );
    assert!(result.w > 200.0);
    assert_eq!(result.x, 100.0);
  }

  #[test]
  fn test_update_resize_w_moves_left_edge() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    let result = update_resize(
      r,
      Handle::W,
      start_rect,
      (50.0, 175.0),
      mods,
      1920.0,
      1080.0,
    );
    assert!(result.x < 100.0);
    assert!(result.w > 200.0);
  }

  #[test]
  fn test_update_resize_ne() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    let result = update_resize(
      r,
      Handle::NE,
      start_rect,
      (350.0, 80.0),
      mods,
      1920.0,
      1080.0,
    );
    assert!(result.w > 200.0); // right edge moved right
    assert!(result.y < 100.0); // top edge moved up
  }

  #[test]
  fn test_update_resize_sw() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    let result = update_resize(
      r,
      Handle::SW,
      start_rect,
      (50.0, 300.0),
      mods,
      1920.0,
      1080.0,
    );
    assert!(result.x < 100.0); // left edge moved left
    assert!(result.h > 150.0); // bottom edge moved down
  }

  #[test]
  fn test_update_resize_clamped_to_window() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    // Drag SE far beyond window bounds
    let result = update_resize(
      r,
      Handle::SE,
      start_rect,
      (5000.0, 5000.0),
      mods,
      1920.0,
      1080.0,
    );
    assert!(result.x + result.w <= 1920.0);
    assert!(result.y + result.h <= 1080.0);
  }

  #[test]
  fn test_update_resize_minimum_size_one() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let start_rect = r;
    let mods = Modifiers::default();
    // Drag SE to before origin - should clamp to minimum size 1
    let result = update_resize(
      r,
      Handle::SE,
      start_rect,
      (90.0, 90.0),
      mods,
      1920.0,
      1080.0,
    );
    assert!(result.w >= 1.0);
    assert!(result.h >= 1.0);
  }

  // --- nudge ---

  #[test]
  fn test_nudge_up() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let result = nudge(r, NudgeDirection::Up, false, 1920.0, 1080.0);
    assert_eq!(result.y, 99.0);
    assert_eq!(result.x, 100.0);
  }

  #[test]
  fn test_nudge_down() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let result = nudge(r, NudgeDirection::Down, false, 1920.0, 1080.0);
    assert_eq!(result.y, 101.0);
  }

  #[test]
  fn test_nudge_left() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let result = nudge(r, NudgeDirection::Left, false, 1920.0, 1080.0);
    assert_eq!(result.x, 99.0);
  }

  #[test]
  fn test_nudge_right() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let result = nudge(r, NudgeDirection::Right, false, 1920.0, 1080.0);
    assert_eq!(result.x, 101.0);
  }

  #[test]
  fn test_nudge_shift_moves_10px() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let result = nudge(r, NudgeDirection::Right, true, 1920.0, 1080.0);
    assert_eq!(result.x, 110.0);
  }

  #[test]
  fn test_nudge_clamped_at_zero() {
    let r = make_rect(0.0, 0.0, 200.0, 150.0);
    let result = nudge(r, NudgeDirection::Left, false, 1920.0, 1080.0);
    assert_eq!(result.x, 0.0); // Clamped, can't go negative
  }

  #[test]
  fn test_nudge_clamped_at_window_edge() {
    let r = make_rect(1719.0, 100.0, 200.0, 150.0); // 1719 + 200 = 1919, one away from edge
    let result = nudge(r, NudgeDirection::Right, false, 1920.0, 1080.0);
    // 1920 - 200 = 1720 max x, so clamped at 1720
    assert_eq!(result.x, 1720.0);
  }

  #[test]
  fn test_nudge_preserves_size() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let result = nudge(r, NudgeDirection::Up, false, 1920.0, 1080.0);
    assert_eq!(result.w, 200.0);
    assert_eq!(result.h, 150.0);
  }

  // --- finalize_rect ---

  #[test]
  fn test_finalize_rect_valid() {
    let r = make_rect(100.0, 100.0, 200.0, 150.0);
    let sel = finalize_rect(r, 1920.0, 1080.0);
    assert!(sel.is_some());
    let s = sel.unwrap();
    assert_eq!(s.x, 100);
    assert_eq!(s.y, 100);
    assert_eq!(s.width, 200);
    assert_eq!(s.height, 150);
  }

  #[test]
  fn test_finalize_rect_too_small_width() {
    let r = make_rect(100.0, 100.0, 5.0, 100.0); // width < MIN_SELECTION_SIZE (10)
    let sel = finalize_rect(r, 1920.0, 1080.0);
    assert!(sel.is_none());
  }

  #[test]
  fn test_finalize_rect_too_small_height() {
    let r = make_rect(100.0, 100.0, 100.0, 5.0); // height < MIN_SELECTION_SIZE
    let sel = finalize_rect(r, 1920.0, 1080.0);
    assert!(sel.is_none());
  }

  #[test]
  fn test_finalize_rect_exactly_minimum_size() {
    let r = make_rect(100.0, 100.0, MIN_SELECTION_SIZE, MIN_SELECTION_SIZE);
    let sel = finalize_rect(r, 1920.0, 1080.0);
    assert!(sel.is_some());
  }

  #[test]
  fn recording_selections_need_the_smallest_area_the_encoders_record() {
    // Regression: video and GIF areas from 10 to 33 px confirmed, then failed in the encoder.
    assert_eq!(RECORDING_MIN_SELECTION_SIZE, 34.0);
    let small = make_rect(100.0, 100.0, 33.0, 200.0);
    assert!(finalize_rect_at_least(small, 1920.0, 1080.0, RECORDING_MIN_SELECTION_SIZE).is_none());
    let short = make_rect(100.0, 100.0, 200.0, 33.0);
    assert!(finalize_rect_at_least(short, 1920.0, 1080.0, RECORDING_MIN_SELECTION_SIZE).is_none());
    let smallest = make_rect(100.0, 100.0, 34.0, 34.0);
    let selection =
      finalize_rect_at_least(smallest, 1920.0, 1080.0, RECORDING_MIN_SELECTION_SIZE).unwrap();
    assert_eq!((selection.width, selection.height), (34, 34));
    // Screenshots keep their 10 px minimum.
    assert!(finalize_rect(make_rect(100.0, 100.0, 12.0, 12.0), 1920.0, 1080.0).is_some());
  }

  #[test]
  fn test_finalize_rect_clamped_to_window() {
    // Rect that extends beyond window
    let r = make_rect(1900.0, 1050.0, 200.0, 200.0);
    let sel = finalize_rect(r, 1920.0, 1080.0);
    // Should still succeed (clamped)
    assert!(sel.is_some());
    let s = sel.unwrap();
    assert!(s.x < 1920);
    assert!(s.y < 1080);
  }

  // --- OverlayState ---

  #[test]
  fn test_overlay_state_new_idle() {
    let state = OverlayState::new_idle();
    assert!(matches!(state, OverlayState::Idle));
  }

  // --- SelectionInteraction ---

  #[test]
  fn test_selection_interaction_default_is_idle() {
    let s = SelectionInteraction::default();
    assert!(matches!(
      s,
      SelectionInteraction::Idle {
        snapped_window: None
      }
    ));
  }

  #[test]
  fn test_selection_interaction_get_current_rect_idle() {
    let s = SelectionInteraction::Idle {
      snapped_window: None,
    };
    assert!(s.get_current_rect(1920.0, 1080.0).is_none());
  }

  #[test]
  fn test_selection_interaction_get_current_rect_pending() {
    let s = SelectionInteraction::PendingDecision {
      press_pos: (100.0, 100.0),
      snapped_window: None,
    };
    assert!(s.get_current_rect(1920.0, 1080.0).is_none());
  }

  #[test]
  fn test_selection_interaction_get_current_rect_freedraw() {
    let s = SelectionInteraction::FreeDraw {
      start: (100.0, 100.0),
      current: (300.0, 250.0),
    };
    let rect = s.get_current_rect(1920.0, 1080.0);
    assert!(rect.is_some());
    let (x1, y1, x2, y2) = rect.unwrap();
    assert_eq!((x1, y1, x2, y2), (100.0, 100.0, 300.0, 250.0));
  }

  #[test]
  fn test_selection_interaction_get_current_rect_confirmed() {
    let s = SelectionInteraction::Confirmed {
      rect: make_rect(100.0, 100.0, 200.0, 150.0),
      from_snap: false,
      moving: None,
      resizing: None,
    };
    let rect = s.get_current_rect(1920.0, 1080.0);
    assert!(rect.is_some());
    let (x1, y1, x2, y2) = rect.unwrap();
    assert_eq!((x1, y1, x2, y2), (100.0, 100.0, 300.0, 250.0));
  }

  #[test]
  fn test_selection_interaction_is_confirmed_idle_true() {
    let s = SelectionInteraction::Confirmed {
      rect: make_rect(100.0, 100.0, 200.0, 150.0),
      from_snap: false,
      moving: None,
      resizing: None,
    };
    assert!(s.is_confirmed_idle());
  }

  #[test]
  fn test_selection_interaction_is_confirmed_idle_false_moving() {
    let s = SelectionInteraction::Confirmed {
      rect: make_rect(100.0, 100.0, 200.0, 150.0),
      from_snap: false,
      moving: Some((10.0, 10.0)),
      resizing: None,
    };
    assert!(!s.is_confirmed_idle());
  }

  #[test]
  fn test_selection_interaction_is_confirmed_idle_false_resizing() {
    let s = SelectionInteraction::Confirmed {
      rect: make_rect(100.0, 100.0, 200.0, 150.0),
      from_snap: false,
      moving: None,
      resizing: Some((Handle::SE, make_rect(100.0, 100.0, 200.0, 150.0))),
    };
    assert!(!s.is_confirmed_idle());
  }

  #[test]
  fn test_selection_interaction_is_confirmed_idle_false_for_idle() {
    let s = SelectionInteraction::Idle {
      snapped_window: None,
    };
    assert!(!s.is_confirmed_idle());
  }

  #[test]
  fn test_selection_interaction_freedraw_reversed_direction() {
    let s = SelectionInteraction::FreeDraw {
      start: (300.0, 250.0),
      current: (100.0, 100.0),
    };
    let (x1, y1, x2, y2) = s.get_current_rect(1920.0, 1080.0).unwrap();
    assert_eq!(x1, 100.0);
    assert_eq!(y1, 100.0);
    assert_eq!(x2, 300.0);
    assert_eq!(y2, 250.0);
  }
}
