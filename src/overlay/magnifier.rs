//! Optional pixel lens for every selection overlay, owned by the selection window thread.
//!
//! The lens reads only the frozen capture and is painted after the scene and its cache, so
//! capture timing never changes and no export can contain it. Before a press the crosshair
//! marks the pixel a selection would start from. During a drag or resize it marks the last
//! selected pixel at the active edge or corner: right and bottom boundaries are exclusive,
//! so the marked pixel is the one the capture keeps, not the one past it.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
  AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BLENDFUNCTION, COLORONCOLOR, CreateSolidBrush,
  DeleteObject, FillRect, HDC, SRCCOPY, STRETCH_BLT_MODE, SetStretchBltMode, StretchBlt,
};

use crate::error::ErrorExt;
use crate::overlay::chrome::{Renderer, Scene};
use crate::overlay::gdi::bitmap::ScreenshotBitmap;
use crate::overlay::gdi::drawing::selection_label_bounds;
use crate::overlay::gdi::layered::LayeredBitmap;
use crate::overlay::motion::Tween;
use crate::overlay::selection::{Handle, SelectionInteraction};
use crate::overlay::selection_core::SelectionControllerState;
use crate::overlay::theme;

/// Odd scales keep the marked pixel exactly centered in the image.
const ZOOM_LEVELS: [i32; 6] = [3, 5, 7, 9, 13, 17];
const DEFAULT_ZOOM_LEVEL: usize = 1;
const WHEEL_NOTCH: i32 = 120;
const IMAGE_SIZE: i32 = 125;
const PANEL_INSET: i32 = 5;
const LENS_SIZE: i32 = IMAGE_SIZE + PANEL_INSET * 2;
/// The Color Picker's row under the image: a swatch of the marked pixel and its HEX.
const SWATCH_ROW: i32 = 20;
const SWATCH_SIZE: i32 = 12;
/// Between the swatch and its HEX.
const SWATCH_GAP: i32 = 6;
const SWATCH_LABEL: crate::overlay::chrome::TextStyle = crate::overlay::chrome::TextStyle::DETAILS;
/// Keeps the lens off the pointer and the active handle.
const POINTER_GAP: i32 = 24;
/// Room needed past a monitor edge before a flipped lens returns, so it cannot flicker there.
const EDGE_HYSTERESIS: i32 = 32;
const TICK_LENGTH: i32 = 12;
const GRID_ALPHA: u8 = 34;
/// Below this scale grid lines would hide more of a pixel than they reveal.
const GRID_MIN_ZOOM: i32 = 5;
/// Concentric with the panel corner around it.
const IMAGE_RADIUS: f32 = 9.0;
const RISE: i32 = 4;
const FADE_IN: Duration = Duration::from_millis(140);
const FADE_OUT: Duration = Duration::from_millis(100);
const GLIDE: Duration = Duration::from_millis(120);

const OUTSIDE_COLOR: COLORREF = theme::colorref(theme::SURFACE);
const CROSSHAIR_COLOR: COLORREF = theme::colorref(theme::ACCENT);
const CROSSHAIR_OUTLINE: COLORREF = COLORREF(0x0000_0000);

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Lens {
  pub focus: (i32, i32),
  pub rect: RECT,
  pub alpha: u8,
  pub zoom: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Sides {
  x: i32,
  y: i32,
}

const BOTTOM_RIGHT: Sides = Sides { x: 1, y: 1 };

struct Layers {
  image: ScreenshotBitmap,
  overlay: ScreenshotBitmap,
  zoom: i32,
}

/// Toggle, placement and transition state for one selection.
pub(crate) struct Magnifier {
  enabled: bool,
  failed: bool,
  shown: bool,
  animating: bool,
  sides: Sides,
  placed: Option<(i32, i32, RECT)>,
  fade: Tween,
  glide: Tween,
  glide_from: (i32, i32),
  zoom_level: usize,
  wheel_remainder: i32,
  lens: Option<Lens>,
  layers: Option<Layers>,
  /// This overlay's copy of the process-wide lens panel ([`panel_pixels`]).
  panel: Option<LayeredBitmap>,
  /// Direct2D chrome for the Color Picker's swatch row; the plain lens never creates it.
  chrome: Renderer,
  /// The Color Picker's lens, with its swatch and HEX row.
  swatch: bool,
}

impl Default for Magnifier {
  fn default() -> Self {
    Self::new(false)
  }
}

impl Magnifier {
  pub fn new(enabled: bool) -> Self {
    let now = Instant::now();
    Self {
      enabled,
      failed: false,
      shown: false,
      animating: false,
      sides: BOTTOM_RIGHT,
      placed: None,
      fade: Tween::new(0.0, FADE_IN, now),
      glide: Tween::new(0.0, GLIDE, now),
      glide_from: (0, 0),
      zoom_level: DEFAULT_ZOOM_LEVEL,
      wheel_remainder: 0,
      lens: None,
      layers: None,
      panel: None,
      chrome: Renderer::default(),
      swatch: false,
    }
  }

  /// The Color Picker's lens: always on, with the marked pixel's swatch and HEX under the image.
  pub fn picker() -> Self {
    Self {
      swatch: true,
      ..Self::new(true)
    }
  }

  fn size(&self) -> (i32, i32) {
    (
      LENS_SIZE,
      LENS_SIZE + if self.swatch { SWATCH_ROW } else { 0 },
    )
  }

  pub fn toggle(&mut self) {
    self.enabled = !self.enabled;
  }

  /// A paint failure ends the lens for this selection rather than retrying every frame.
  pub fn turn_off(&mut self) {
    self.enabled = false;
    self.failed = true;
  }

  /// Applies a wheel movement to a visible lens, and reports whether it took the input.
  /// High-resolution wheels send partial notches, so the remainder carries over.
  pub fn wheel(&mut self, delta: i32) -> bool {
    if !self.shown {
      return false;
    }
    self.wheel_remainder += delta;
    let notches = self.wheel_remainder / WHEEL_NOTCH;
    self.wheel_remainder -= notches * WHEEL_NOTCH;
    let level = self.zoom_level as i32 + notches;
    self.zoom_level = level.clamp(0, ZOOM_LEVELS.len() as i32 - 1) as usize;
    true
  }

  /// True while a change could add, move or remove a lens.
  pub fn needs_update(&self) -> bool {
    self.enabled || self.lens.is_some() || self.animating
  }

  /// True until the last frame of a transition has been placed.
  pub fn animating(&self) -> bool {
    self.animating
  }

  pub fn lens(&self) -> Option<Lens> {
    self.lens.filter(|_| !self.failed)
  }

  /// Places the lens for this moment and returns the areas to repaint.
  pub fn update(
    &mut self,
    controller: &SelectionControllerState,
    monitor_at: impl FnOnce((i32, i32)) -> RECT,
    toolbar: Option<RECT>,
    now: Instant,
    animate: bool,
  ) -> [Option<RECT>; 2] {
    let previous = self.lens;
    let target = (self.enabled && !self.failed)
      .then(|| focus(controller))
      .flatten()
      .map(|focus| {
        let selection = selection_rect(controller);
        let label =
          selection.map(|rect| selection_label_bounds(rect, controller.width, controller.height));
        let obstacles = [label, toolbar];
        let (rect, sides) = place_sized(
          focus,
          self.sides,
          selection,
          &obstacles,
          monitor_at(focus),
          self.size(),
        );
        (focus, rect, sides)
      });

    let shown = target.is_some();
    if shown != self.shown {
      self.shown = shown;
      let duration = if shown { FADE_IN } else { FADE_OUT };
      let mut fade = Tween::new(self.fade.value(now), duration, now);
      fade.target(f32::from(u8::from(shown)), now, animate);
      self.fade = fade;
    }
    if let Some((focus, rect, sides)) = target {
      if sides != self.sides
        && animate
        && let Some(previous) = previous
      {
        self.glide_from = (previous.rect.left - rect.left, previous.rect.top - rect.top);
        let mut glide = Tween::new(1.0, GLIDE, now);
        glide.target(0.0, now, true);
        self.glide = glide;
      }
      self.sides = sides;
      self.placed = Some((focus.0, focus.1, rect));
    }

    let opacity = self.fade.value(now);
    self.animating = !self.failed && (self.fade.active(now) || self.glide.active(now));
    let glide = self.glide.value(now);
    self.lens = self
      .placed
      .filter(|_| opacity > 0.0 && !self.failed)
      .map(|(x, y, rect)| {
        let room_below = (controller.height - rect.bottom).max(0);
        let rise = (((1.0 - opacity) * RISE as f32).round() as i32).min(room_below);
        let dx = (self.glide_from.0 as f32 * glide).round() as i32;
        let dy = (self.glide_from.1 as f32 * glide).round() as i32 + rise;
        Lens {
          focus: (x, y),
          rect: RECT {
            left: rect.left + dx,
            top: rect.top + dy,
            right: rect.right + dx,
            bottom: rect.bottom + dy,
          },
          alpha: (opacity * 255.0).round() as u8,
          zoom: ZOOM_LEVELS[self.zoom_level],
        }
      });
    if previous == self.lens && !self.animating {
      return [None, None];
    }
    [
      previous.map(|lens| lens.rect),
      self.lens.map(|lens| lens.rect),
    ]
  }

  /// Paints the lens over the finished scene.
  ///
  /// A translucent frame must not land on pixels that were not restored first, or it would
  /// build up. A transition invalidates the whole lens every tick, so the next frame covers it.
  pub fn paint(
    &mut self,
    dc: HDC,
    shot_dc: HDC,
    capture_width: i32,
    capture_height: i32,
    damage: RECT,
  ) -> Result<(), String> {
    let Some(lens) = self.lens() else {
      return Ok(());
    };
    if lens.alpha < u8::MAX && !contains(damage, lens.rect) {
      return Ok(());
    }
    if self.swatch {
      let mut scene = Scene::default();
      scene
        .panel(lens.rect)
        .map_err(|error| error.to_rich_string())?;
      let color = pixel_color(shot_dc, lens.focus)?;
      let label = hex(color);
      let width = self
        .chrome
        .measure_text(&label, SWATCH_LABEL)
        .map_err(|error| error.to_rich_string())?;
      swatch_row(&mut scene, lens.rect, color, &label, width)
        .map_err(|error| error.to_rich_string())?;
      self
        .chrome
        .render_with_opacity(dc, lens.rect, &scene, lens.alpha)
        .map_err(|error| error.to_rich_string())?;
    } else {
      self.paint_panel(dc, lens)?;
    }
    if self
      .layers
      .as_ref()
      .is_none_or(|layers| layers.zoom != lens.zoom)
    {
      self.layers = Some(Layers::new(lens.zoom)?);
    }
    let Some(layers) = &self.layers else {
      return Err("Magnifier layers unavailable".into());
    };
    // SAFETY: the layer and shot DCs are live memory DCs owned by this overlay thread.
    unsafe { compose(layers, shot_dc, capture_width, capture_height, lens.focus) }?;
    let blend = BLENDFUNCTION {
      BlendOp: AC_SRC_OVER as u8,
      BlendFlags: 0,
      SourceConstantAlpha: lens.alpha,
      AlphaFormat: 0,
    };
    // SAFETY: both DCs are live and the source rectangle is the whole image layer.
    let blended = unsafe {
      AlphaBlend(
        dc,
        lens.rect.left + PANEL_INSET,
        lens.rect.top + PANEL_INSET,
        IMAGE_SIZE,
        IMAGE_SIZE,
        layers.image.dc,
        0,
        0,
        IMAGE_SIZE,
        IMAGE_SIZE,
        blend,
      )
    };
    if !blended.as_bool() {
      return Err("Magnifier blending failed".into());
    }
    Ok(())
  }
}

impl Magnifier {
  /// The plain lens's panel: the same pixels and blend Direct2D chrome would produce, copied from
  /// the process-wide render, so an overlay never sets up Direct2D before its first frame.
  fn paint_panel(&mut self, dc: HDC, lens: Lens) -> Result<(), String> {
    if self.panel.is_none() {
      let mut panel = LayeredBitmap::new(LENS_SIZE, LENS_SIZE)?;
      panel.pixels_mut().copy_from_slice(panel_pixels()?);
      self.panel = Some(panel);
    }
    let Some(panel) = &self.panel else {
      return Err("Magnifier panel unavailable".into());
    };
    let blend = BLENDFUNCTION {
      BlendOp: AC_SRC_OVER as u8,
      BlendFlags: 0,
      SourceConstantAlpha: lens.alpha,
      AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // SAFETY: both DCs are live on this overlay thread; the source is the whole panel bitmap.
    let blended = unsafe {
      AlphaBlend(
        dc,
        lens.rect.left,
        lens.rect.top,
        LENS_SIZE,
        LENS_SIZE,
        panel.dc,
        0,
        0,
        LENS_SIZE,
        LENS_SIZE,
        blend,
      )
    };
    if !blended.as_bool() {
      return Err("Magnifier panel blending failed".into());
    }
    Ok(())
  }
}

/// The plain lens's panel as premultiplied BGRA, rendered once per process by Direct2D chrome.
/// Its size and palette never change, and plain bytes can be shared by every overlay thread.
static PANEL: OnceLock<Vec<u8>> = OnceLock::new();

fn panel_pixels() -> Result<&'static [u8], String> {
  if let Some(pixels) = PANEL.get() {
    return Ok(pixels);
  }
  let rendered = render_panel()?;
  Ok(PANEL.get_or_init(|| rendered))
}

/// Chrome composites onto a transparent surface unchanged, so these are its own pixels.
fn render_panel() -> Result<Vec<u8>, String> {
  let mut surface = LayeredBitmap::new(LENS_SIZE, LENS_SIZE)?;
  let rect = RECT {
    left: 0,
    top: 0,
    right: LENS_SIZE,
    bottom: LENS_SIZE,
  };
  let mut scene = Scene::default();
  scene.panel(rect).map_err(|error| error.to_rich_string())?;
  Renderer::default()
    .render_with_opacity(surface.dc, rect, &scene, u8::MAX)
    .map_err(|error| error.to_rich_string())?;
  Ok(surface.pixels_mut().to_vec())
}

/// Grid and corner layers per zoom level, computed once per process.
static OVERLAYS: [OnceLock<Vec<u8>>; ZOOM_LEVELS.len()] =
  [const { OnceLock::new() }; ZOOM_LEVELS.len()];

fn cached_overlay_pixels(zoom: i32) -> Vec<u8> {
  match ZOOM_LEVELS.iter().position(|level| *level == zoom) {
    Some(index) => OVERLAYS[index].get_or_init(|| overlay_pixels(zoom)).clone(),
    None => overlay_pixels(zoom),
  }
}

/// Renders the shared lens panel and the default zoom's layer at launch, off the overlay path, so
/// even the first selection after launch skips that work. Idempotent; a failure is logged and the
/// first lens renders them itself.
pub fn warm_up() {
  static STARTED: std::sync::Once = std::sync::Once::new();
  STARTED.call_once(|| {
    let spawned = std::thread::Builder::new()
      .name("flinch-magnifier-warmup".into())
      .spawn(|| {
        if let Err(error) = panel_pixels() {
          tracing::warn!(%error, "Magnifier panel warm-up failed; the first lens renders it");
        }
        drop(cached_overlay_pixels(ZOOM_LEVELS[DEFAULT_ZOOM_LEVEL]));
      });
    if let Err(error) = spawned {
      tracing::warn!(%error, "Magnifier warm-up thread failed to start");
    }
  });
}

impl Layers {
  fn new(zoom: i32) -> Result<Self, String> {
    let size = IMAGE_SIZE as u32;
    Ok(Self {
      image: ScreenshotBitmap::from_rgba(&vec![0u8; (size * size * 4) as usize], size, size)?,
      overlay: ScreenshotBitmap::from_rgba(&cached_overlay_pixels(zoom), size, size)?,
      zoom,
    })
  }
}

/// Uppercase HEX with `#`, as the Color Picker copies it.
pub(crate) fn hex(rgb: [u8; 3]) -> String {
  format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

/// The frozen capture's pixel, read from the overlay's own screenshot bitmap.
fn pixel_color(shot_dc: HDC, focus: (i32, i32)) -> Result<[u8; 3], String> {
  // SAFETY: the shot DC is a live memory DC owned by this overlay thread.
  let color = unsafe { windows::Win32::Graphics::Gdi::GetPixel(shot_dc, focus.0, focus.1) };
  if color.0 == u32::MAX {
    return Err("Cannot read the magnified pixel".into());
  }
  Ok([
    (color.0 & 0xFF) as u8,
    ((color.0 >> 8) & 0xFF) as u8,
    ((color.0 >> 16) & 0xFF) as u8,
  ])
}

/// The swatch and its HEX, centered together under the image and midway between it and the
/// panel's bottom edge. The swatch's outline is a ring painted under it, so the two never
/// disagree at the corners.
fn swatch_row(
  scene: &mut Scene,
  lens: RECT,
  color: [u8; 3],
  label: &str,
  label_width: f32,
) -> anyhow::Result<()> {
  let image_bottom = lens.top + PANEL_INSET + IMAGE_SIZE;
  let middle = image_bottom + (lens.bottom - image_bottom) / 2;
  let label_width = label_width.ceil() as i32;
  let group = SWATCH_SIZE + SWATCH_GAP + label_width;
  let left = lens.left + (lens.right - lens.left - group) / 2;
  let ring = RECT {
    left,
    top: middle - SWATCH_SIZE / 2,
    right: left + SWATCH_SIZE,
    bottom: middle - SWATCH_SIZE / 2 + SWATCH_SIZE,
  };
  scene.rounded(ring, 3.5, theme::OUTLINE, u8::MAX)?;
  let fill = RECT {
    left: ring.left + 1,
    top: ring.top + 1,
    right: ring.right - 1,
    bottom: ring.bottom - 1,
  };
  scene.rounded(fill, 2.5, color, u8::MAX)?;
  let text = RECT {
    left: ring.right + SWATCH_GAP,
    top: middle - SWATCH_ROW / 2,
    right: ring.right + SWATCH_GAP + label_width + 2,
    bottom: middle + SWATCH_ROW / 2,
  };
  scene.styled_text(text, label, SWATCH_LABEL, theme::TEXT)
}

/// Offset of the marked pixel's cell inside the image.
fn center_cell(zoom: i32) -> i32 {
  (IMAGE_SIZE - zoom) / 2
}

/// How much of this pixel the rounded image covers, antialiased at its corners.
fn image_coverage(x: i32, y: i32) -> f32 {
  let edge = |value: i32| {
    let center = value as f32 + 0.5;
    let far = IMAGE_SIZE as f32 - IMAGE_RADIUS;
    if center < IMAGE_RADIUS {
      IMAGE_RADIUS - center
    } else if center > far {
      center - far
    } else {
      0.0
    }
  };
  let (dx, dy) = (edge(x), edge(y));
  if dx == 0.0 || dy == 0.0 {
    return 1.0;
  }
  (IMAGE_RADIUS - dx.hypot(dy) + 0.5).clamp(0.0, 1.0)
}

/// Premultiplied layer over the image: the grid, then panel color outside the rounded corners.
/// Painting the panel's own color there matches a true cutout, because the image sits on it.
fn overlay_pixels(zoom: i32) -> Vec<u8> {
  let size = IMAGE_SIZE as usize;
  let on_grid =
    |value: i32| zoom >= GRID_MIN_ZOOM && (value - center_cell(zoom)).rem_euclid(zoom) == 0;
  let mut pixels = vec![0u8; size * size * 4];
  for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
    let (x, y) = ((index % size) as i32, (index / size) as i32);
    let grid = if on_grid(x) || on_grid(y) {
      f32::from(GRID_ALPHA) / 255.0
    } else {
      0.0
    };
    let panel = 1.0 - image_coverage(x, y);
    for (channel, color) in pixel.iter_mut().zip(theme::SURFACE) {
      *channel = (f32::from(color) * panel).round() as u8;
    }
    pixel[3] = ((1.0 - (1.0 - grid) * (1.0 - panel)) * 255.0).round() as u8;
  }
  pixels
}

fn contains(outer: RECT, inner: RECT) -> bool {
  outer.left <= inner.left
    && outer.top <= inner.top
    && outer.right >= inner.right
    && outer.bottom >= inner.bottom
}

pub(crate) fn selection_rect(controller: &SelectionControllerState) -> Option<RECT> {
  controller
    .interaction
    .get_current_rect(controller.width as f64, controller.height as f64)
    .map(|(x1, y1, x2, y2)| RECT {
      left: x1 as i32,
      top: y1 as i32,
      right: x2 as i32,
      bottom: y2 as i32,
    })
}

/// Overlay-local bounds of the monitor showing `point`, or the whole capture across a gap.
pub(crate) fn monitor_at(controller: &SelectionControllerState, point: (i32, i32)) -> RECT {
  let monitors = crate::monitors::list_monitors()
    .into_iter()
    .map(|monitor| RECT {
      left: monitor.x - controller.vd_offset_x,
      top: monitor.y - controller.vd_offset_y,
      right: monitor.x - controller.vd_offset_x + monitor.width as i32,
      bottom: monitor.y - controller.vd_offset_y + monitor.height as i32,
    });
  containing_monitor(monitors, point).unwrap_or(RECT {
    left: 0,
    top: 0,
    right: controller.width,
    bottom: controller.height,
  })
}

fn containing_monitor(monitors: impl Iterator<Item = RECT>, point: (i32, i32)) -> Option<RECT> {
  monitors.into_iter().find(|monitor| {
    point.0 >= monitor.left
      && point.1 >= monitor.top
      && point.0 < monitor.right
      && point.1 < monitor.bottom
  })
}

/// The pixel to magnify, or `None` once a finished selection is being moved, edited or annotated.
pub(crate) fn focus(controller: &SelectionControllerState) -> Option<(i32, i32)> {
  let (x, y) = match &controller.interaction {
    SelectionInteraction::Idle { .. } | SelectionInteraction::PendingDecision { .. } => (
      controller.cursor_pos.0.floor() as i32,
      controller.cursor_pos.1.floor() as i32,
    ),
    SelectionInteraction::FreeDraw { start, current } => (
      drawn_edge(start.0, current.0),
      drawn_edge(start.1, current.1),
    ),
    SelectionInteraction::Confirmed {
      rect,
      moving: None,
      resizing: Some((handle, _)),
      ..
    } => {
      // Follow the crop boundary, not the pointer, which may have grabbed the handle off-center.
      let left = rect.x.floor() as i32;
      let top = rect.y.floor() as i32;
      let right = (rect.x + rect.w).floor() as i32 - 1;
      let bottom = (rect.y + rect.h).floor() as i32 - 1;
      let along_x = (controller.cursor_pos.0.floor() as i32).clamp(left, right.max(left));
      let along_y = (controller.cursor_pos.1.floor() as i32).clamp(top, bottom.max(top));
      match handle {
        Handle::N => (along_x, top),
        Handle::S => (along_x, bottom),
        Handle::W => (left, along_y),
        Handle::E => (right, along_y),
        Handle::NW => (left, top),
        Handle::NE => (right, top),
        Handle::SW => (left, bottom),
        Handle::SE => (right, bottom),
      }
    },
    _ => return None,
  };
  Some((
    x.clamp(0, (controller.width - 1).max(0)),
    y.clamp(0, (controller.height - 1).max(0)),
  ))
}

/// A drag toward higher coordinates ends on the exclusive boundary; the other way ends on a
/// selected pixel.
fn drawn_edge(start: f64, current: f64) -> i32 {
  let edge = current.floor() as i32;
  if current > start { edge - 1 } else { edge }
}

fn overlap(a: RECT, b: RECT) -> i64 {
  let width = i64::from(a.right.min(b.right) - a.left.max(b.left)).max(0);
  let height = i64::from(a.bottom.min(b.bottom) - a.top.max(b.top)).max(0);
  width * height
}

fn start(focus: i32, side: i32, size: i32) -> i32 {
  if side > 0 {
    focus + POINTER_GAP
  } else {
    focus - POINTER_GAP - size
  }
}

fn fits(focus: i32, side: i32, low: i32, high: i32, size: i32) -> bool {
  let start = start(focus, side, size);
  start >= low && start + size <= high
}

/// Before a selection the lens rests after the pointer, and only an edge moves it.
fn resting_side(side: i32, focus: i32, low: i32, high: i32, size: i32) -> i32 {
  if side > 0 {
    if fits(focus, 1, low, high, size) {
      1
    } else {
      -1
    }
  } else if fits(focus, 1, low, high - EDGE_HYSTERESIS, size) || !fits(focus, -1, low, high, size) {
    1
  } else {
    -1
  }
}

/// Which way points out of the selection on this axis, if the focus is on one of its edges.
fn outward_side(focus: i32, low: i32, high: i32, current: i32) -> i32 {
  if focus >= high - 1 {
    1
  } else if focus <= low {
    -1
  } else {
    current
  }
}

/// Keeps a steady side so the lens is predictable, and leaves it only for an edge or obstacle.
#[cfg(test)]
fn place(
  focus: (i32, i32),
  sides: Sides,
  selection: Option<RECT>,
  obstacles: &[Option<RECT>],
  monitor: RECT,
) -> (RECT, Sides) {
  place_sized(
    focus,
    sides,
    selection,
    obstacles,
    monitor,
    (LENS_SIZE, LENS_SIZE),
  )
}

fn place_sized(
  focus: (i32, i32),
  sides: Sides,
  selection: Option<RECT>,
  obstacles: &[Option<RECT>],
  monitor: RECT,
  size: (i32, i32),
) -> (RECT, Sides) {
  let wanted = match selection {
    Some(selection) => Sides {
      x: outward_side(focus.0, selection.left, selection.right, sides.x),
      y: outward_side(focus.1, selection.top, selection.bottom, sides.y),
    },
    None => Sides {
      x: resting_side(sides.x, focus.0, monitor.left, monitor.right, size.0),
      y: resting_side(sides.y, focus.1, monitor.top, monitor.bottom, size.1),
    },
  };
  let rect_for = |sides: Sides| {
    let left = start(focus.0, sides.x, size.0);
    let top = start(focus.1, sides.y, size.1);
    RECT {
      left,
      top,
      right: left + size.0,
      bottom: top + size.1,
    }
  };
  let on_monitor = |sides: Sides| {
    fits(focus.0, sides.x, monitor.left, monitor.right, size.0)
      && fits(focus.1, sides.y, monitor.top, monitor.bottom, size.1)
  };
  let clear = |sides: Sides| {
    obstacles
      .iter()
      .flatten()
      .all(|obstacle| overlap(rect_for(sides), *obstacle) == 0)
  };
  let candidates = [
    wanted,
    Sides {
      x: wanted.x,
      y: -wanted.y,
    },
    Sides {
      x: -wanted.x,
      y: wanted.y,
    },
    Sides {
      x: -wanted.x,
      y: -wanted.y,
    },
  ];
  if let Some(sides) = candidates
    .into_iter()
    .find(|sides| on_monitor(*sides) && clear(*sides))
    .or_else(|| candidates.into_iter().find(|sides| on_monitor(*sides)))
  {
    return (rect_for(sides), sides);
  }
  // A monitor too small for any side still keeps the lens as close to it as possible.
  let rect = rect_for(wanted);
  let left = rect
    .left
    .clamp(monitor.left, (monitor.right - size.0).max(monitor.left));
  let top = rect
    .top
    .clamp(monitor.top, (monitor.bottom - size.1).max(monitor.top));
  (
    RECT {
      left,
      top,
      right: left + size.0,
      bottom: top + size.1,
    },
    wanted,
  )
}

#[derive(Debug, PartialEq)]
struct SourceClip {
  source: RECT,
  image_x: i32,
  image_y: i32,
}

/// The part of the sampled square inside the capture. The focus stays centered, so a lens at a
/// desktop edge shows empty space rather than sliding its crosshair. Cells cut by the image
/// edge are clipped when painted.
fn source_clip(focus: (i32, i32), width: i32, height: i32, zoom: i32) -> Option<SourceClip> {
  let reach = IMAGE_SIZE / 2 / zoom + 1;
  let source = RECT {
    left: (focus.0 - reach).max(0),
    top: (focus.1 - reach).max(0),
    right: (focus.0 + reach + 1).min(width),
    bottom: (focus.1 + reach + 1).min(height),
  };
  (source.right > source.left && source.bottom > source.top).then_some(SourceClip {
    image_x: center_cell(zoom) + (source.left - focus.0) * zoom,
    image_y: center_cell(zoom) + (source.top - focus.1) * zoom,
    source,
  })
}

/// # Safety
/// `dc` and `shot_dc` must be live memory DCs owned by the calling overlay thread.
unsafe fn fill(dc: HDC, rect: RECT, color: COLORREF) -> Result<(), String> {
  // SAFETY: the brush is created, used and deleted on this thread.
  let brush = unsafe { CreateSolidBrush(color) };
  if brush.is_invalid() {
    return Err("Magnifier brush creation failed".into());
  }
  // SAFETY: the caller owns dc and the brush is live until the delete below.
  let filled = unsafe { FillRect(dc, &rect, brush) };
  // SAFETY: the brush is not selected into any DC.
  let _ = unsafe { DeleteObject(brush.into()) };
  if filled == 0 {
    return Err("Magnifier fill failed".into());
  }
  Ok(())
}

/// # Safety
/// `dc` must be a live memory DC owned by the calling overlay thread.
unsafe fn crosshair(dc: HDC, image: RECT, zoom: i32) -> Result<(), String> {
  let pixel_left = image.left + center_cell(zoom);
  let pixel_top = image.top + center_cell(zoom);
  let center_x = pixel_left + zoom / 2;
  let center_y = pixel_top + zoom / 2;
  let box_rect = |inset: i32| RECT {
    left: pixel_left - inset,
    top: pixel_top - inset,
    right: pixel_left + zoom + inset,
    bottom: pixel_top + zoom + inset,
  };
  let frame = |rect: RECT| {
    [
      RECT {
        bottom: rect.top + 1,
        ..rect
      },
      RECT {
        top: rect.bottom - 1,
        ..rect
      },
      RECT {
        right: rect.left + 1,
        ..rect
      },
      RECT {
        left: rect.right - 1,
        ..rect
      },
    ]
  };
  let reach = 2;
  let ticks = |inset: i32| {
    [
      RECT {
        left: center_x - inset,
        right: center_x + 1 + inset,
        top: pixel_top - reach - TICK_LENGTH,
        bottom: pixel_top - reach,
      },
      RECT {
        left: center_x - inset,
        right: center_x + 1 + inset,
        top: pixel_top + zoom + reach,
        bottom: pixel_top + zoom + reach + TICK_LENGTH,
      },
      RECT {
        left: pixel_left - reach - TICK_LENGTH,
        right: pixel_left - reach,
        top: center_y - inset,
        bottom: center_y + 1 + inset,
      },
      RECT {
        left: pixel_left + zoom + reach,
        right: pixel_left + zoom + reach + TICK_LENGTH,
        top: center_y - inset,
        bottom: center_y + 1 + inset,
      },
    ]
  };
  // A dark outline keeps the marks readable over any capture content, red included.
  for rect in frame(box_rect(2)).into_iter().chain(ticks(1)) {
    // SAFETY: forwarded from this function's contract.
    unsafe { fill(dc, rect, CROSSHAIR_OUTLINE) }?;
  }
  for rect in frame(box_rect(1)).into_iter().chain(ticks(0)) {
    // SAFETY: forwarded from this function's contract.
    unsafe { fill(dc, rect, CROSSHAIR_COLOR) }?;
  }
  Ok(())
}

/// Builds the magnified image in its own layer so the whole lens can fade as one.
///
/// # Safety
/// `shot_dc` and the layer DCs must be live memory DCs owned by the calling overlay thread.
unsafe fn compose(
  layers: &Layers,
  shot_dc: HDC,
  capture_width: i32,
  capture_height: i32,
  focus: (i32, i32),
) -> Result<(), String> {
  let dc = layers.image.dc;
  let zoom = layers.zoom;
  let image = RECT {
    left: 0,
    top: 0,
    right: IMAGE_SIZE,
    bottom: IMAGE_SIZE,
  };
  // SAFETY: forwarded from this function's contract.
  unsafe { fill(dc, image, OUTSIDE_COLOR) }?;
  if let Some(clip) = source_clip(focus, capture_width, capture_height, zoom) {
    let width = clip.source.right - clip.source.left;
    let height = clip.source.bottom - clip.source.top;
    // SAFETY: dc is live; the previous mode is restored below on every path.
    let previous = unsafe { SetStretchBltMode(dc, COLORONCOLOR) };
    // SAFETY: both DCs are live and the source rectangle lies inside the capture.
    let result = unsafe {
      StretchBlt(
        dc,
        clip.image_x,
        clip.image_y,
        width * zoom,
        height * zoom,
        Some(shot_dc),
        clip.source.left,
        clip.source.top,
        width,
        height,
        SRCCOPY,
      )
    };
    // SAFETY: restores the mode saved above on the same DC.
    unsafe { SetStretchBltMode(dc, STRETCH_BLT_MODE(previous)) };
    if !result.as_bool() {
      return Err("Magnifier sampling failed".into());
    }
  }
  let blend = BLENDFUNCTION {
    BlendOp: AC_SRC_OVER as u8,
    BlendFlags: 0,
    SourceConstantAlpha: u8::MAX,
    AlphaFormat: AC_SRC_ALPHA as u8,
  };
  // SAFETY: both layers are live and share the image size.
  let gridded = unsafe {
    AlphaBlend(
      dc,
      0,
      0,
      IMAGE_SIZE,
      IMAGE_SIZE,
      layers.overlay.dc,
      0,
      0,
      IMAGE_SIZE,
      IMAGE_SIZE,
      blend,
    )
  };
  if !gridded.as_bool() {
    return Err("Magnifier overlay failed".into());
  }
  // SAFETY: forwarded from this function's contract.
  unsafe { crosshair(dc, image, zoom) }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::overlay::selection::Rect;
  use windows::Win32::Graphics::Gdi::GetPixel;

  const ZOOM: i32 = 5;
  const SOURCE_PIXELS: i32 = 25;

  const DESKTOP: RECT = RECT {
    left: 0,
    top: 0,
    right: 1920,
    bottom: 1080,
  };

  fn controller(interaction: SelectionInteraction) -> SelectionControllerState {
    let mut controller = SelectionControllerState::new(1920, 1080, 0, 0);
    controller.interaction = interaction;
    controller
  }

  fn resizing(handle: Handle, cursor: (f64, f64)) -> SelectionControllerState {
    let rect = Rect {
      x: 100.0,
      y: 200.0,
      w: 300.0,
      h: 150.0,
    };
    let mut controller = controller(SelectionInteraction::Confirmed {
      rect,
      from_snap: false,
      moving: None,
      resizing: Some((handle, rect)),
    });
    controller.cursor_pos = cursor;
    controller
  }

  #[test]
  fn focus_marks_the_last_selected_pixel_in_either_drag_direction() {
    let forward = controller(SelectionInteraction::FreeDraw {
      start: (10.0, 20.0),
      current: (110.0, 220.0),
    });
    assert_eq!(focus(&forward), Some((109, 219)));
    let reversed = controller(SelectionInteraction::FreeDraw {
      start: (110.0, 220.0),
      current: (10.0, 20.0),
    });
    assert_eq!(focus(&reversed), Some((10, 20)));
  }

  #[test]
  fn focus_follows_the_crop_boundary_for_every_handle_despite_pointer_offset() {
    let cursor = (103.0, 197.0);
    for (handle, expected) in [
      (Handle::NW, (100, 200)),
      (Handle::NE, (399, 200)),
      (Handle::SW, (100, 349)),
      (Handle::SE, (399, 349)),
      (Handle::N, (103, 200)),
      (Handle::S, (103, 349)),
      (Handle::W, (100, 200)),
      (Handle::E, (399, 200)),
    ] {
      assert_eq!(
        focus(&resizing(handle, cursor)),
        Some(expected),
        "{handle:?}"
      );
    }
  }

  #[test]
  fn focus_is_the_pointer_pixel_before_a_selection_starts() {
    for interaction in [
      SelectionInteraction::Idle {
        snapped_window: None,
      },
      SelectionInteraction::PendingDecision {
        press_pos: (640.0, 360.0),
        snapped_window: None,
      },
    ] {
      let mut controller = controller(interaction);
      controller.cursor_pos = (640.0, 360.0);
      assert_eq!(focus(&controller), Some((640, 360)));
      controller.cursor_pos = (5000.0, -10.0);
      assert_eq!(focus(&controller), Some((1919, 0)));
    }
  }

  #[test]
  fn monitor_lookup_uses_the_monitor_under_the_point_and_reports_gaps() {
    let left = RECT {
      left: 0,
      top: 300,
      right: 1280,
      bottom: 1020,
    };
    let right = RECT {
      left: 1280,
      top: 0,
      right: 3200,
      bottom: 1080,
    };
    let monitors = || [left, right].into_iter();
    assert_eq!(containing_monitor(monitors(), (1279, 400)), Some(left));
    assert_eq!(containing_monitor(monitors(), (1280, 10)), Some(right));
    assert_eq!(containing_monitor(monitors(), (10, 10)), None);
  }

  #[test]
  fn focus_stays_inside_the_capture_and_hides_outside_selection_adjustment() {
    let edge = controller(SelectionInteraction::FreeDraw {
      start: (1000.0, 500.0),
      current: (5000.0, -40.0),
    });
    assert_eq!(focus(&edge), Some((1919, 0)));
    let rect = Rect {
      x: 10.0,
      y: 10.0,
      w: 50.0,
      h: 50.0,
    };
    for interaction in [
      SelectionInteraction::Confirmed {
        rect,
        from_snap: false,
        moving: None,
        resizing: None,
      },
      SelectionInteraction::Confirmed {
        rect,
        from_snap: false,
        moving: Some((1.0, 1.0)),
        resizing: None,
      },
    ] {
      assert_eq!(focus(&controller(interaction)), None);
    }
  }

  #[test]
  fn lens_rests_bottom_right_and_flips_only_at_monitor_edges() {
    let at = |focus| place(focus, BOTTOM_RIGHT, None, &[], DESKTOP);
    let (rect, sides) = at((960, 540));
    assert_eq!(sides, BOTTOM_RIGHT);
    assert_eq!(
      (rect.left, rect.top),
      (960 + POINTER_GAP, 540 + POINTER_GAP)
    );
    for (focus, expected) in [
      ((1915, 500), Sides { x: -1, y: 1 }),
      ((500, 1075), Sides { x: 1, y: -1 }),
      ((1915, 1075), Sides { x: -1, y: -1 }),
    ] {
      let (rect, sides) = at(focus);
      assert_eq!(sides, expected, "{focus:?}");
      assert!(rect.left >= 0 && rect.top >= 0 && rect.right <= 1920 && rect.bottom <= 1080);
    }
  }

  #[test]
  fn flipped_lens_needs_clear_room_before_returning() {
    let flipped = Sides { x: -1, y: 1 };
    let limit = 1920 - POINTER_GAP - LENS_SIZE;
    assert_eq!(
      place((limit - 1, 500), flipped, None, &[], DESKTOP).1,
      flipped
    );
    assert_eq!(
      place((limit - EDGE_HYSTERESIS, 500), flipped, None, &[], DESKTOP).1,
      BOTTOM_RIGHT
    );
  }

  #[test]
  fn lens_sits_outside_the_selection_in_every_drag_direction() {
    let selection = RECT {
      left: 500,
      top: 400,
      right: 900,
      bottom: 700,
    };
    for (focus, expected) in [
      ((899, 699), Sides { x: 1, y: 1 }),
      ((500, 400), Sides { x: -1, y: -1 }),
      ((899, 400), Sides { x: 1, y: -1 }),
      ((500, 699), Sides { x: -1, y: 1 }),
    ] {
      let (rect, sides) = place(focus, BOTTOM_RIGHT, Some(selection), &[], DESKTOP);
      assert_eq!(sides, expected, "{focus:?}");
      assert_eq!(overlap(rect, selection), 0, "{focus:?}");
    }
    // An edge handle only decides its own axis.
    let edge = Sides { x: -1, y: 1 };
    assert_eq!(
      place((700, 400), edge, Some(selection), &[], DESKTOP).1,
      Sides { x: -1, y: -1 }
    );
  }

  #[test]
  fn lens_moves_off_the_toolbar_and_a_covered_edge_wins_over_outward() {
    let selection = RECT {
      left: 500,
      top: 400,
      right: 900,
      bottom: 700,
    };
    let toolbar = RECT {
      left: 700,
      top: 710,
      right: 1300,
      bottom: 900,
    };
    let (rect, _) = place(
      (899, 699),
      BOTTOM_RIGHT,
      Some(selection),
      &[Some(toolbar)],
      DESKTOP,
    );
    assert_eq!(overlap(rect, toolbar), 0);
    let (rect, sides) = place((1915, 1075), BOTTOM_RIGHT, Some(selection), &[], DESKTOP);
    assert_eq!(sides, Sides { x: -1, y: -1 });
    assert!(rect.right <= 1920 && rect.bottom <= 1080);
  }

  #[test]
  fn monitor_smaller_than_the_lens_keeps_it_at_the_monitor() {
    let tiny = RECT {
      left: 10,
      top: 10,
      right: 60,
      bottom: 60,
    };
    let (rect, _) = place((20, 20), BOTTOM_RIGHT, None, &[], tiny);
    assert_eq!((rect.left, rect.top), (10, 10));
  }

  #[test]
  fn source_clip_keeps_the_focus_centered_at_desktop_edges() {
    assert_eq!(
      source_clip((0, 0), 100, 100, ZOOM),
      Some(SourceClip {
        source: RECT {
          left: 0,
          top: 0,
          right: 14,
          bottom: 14,
        },
        image_x: 12 * ZOOM,
        image_y: 12 * ZOOM,
      })
    );
    assert_eq!(
      source_clip((99, 50), 100, 100, ZOOM).map(|clip| (clip.source.right, clip.image_x)),
      Some((100, -ZOOM))
    );
    assert_eq!(source_clip((500, 500), 100, 100, ZOOM), None);
  }

  fn drawing(current: (f64, f64)) -> SelectionControllerState {
    controller(SelectionInteraction::FreeDraw {
      start: (100.0, 100.0),
      current,
    })
  }

  fn hover(cursor: (f64, f64)) -> SelectionControllerState {
    let mut controller = controller(SelectionInteraction::Idle {
      snapped_window: None,
    });
    controller.cursor_pos = cursor;
    controller
  }

  fn step(
    magnifier: &mut Magnifier,
    controller: &SelectionControllerState,
    now: Instant,
    animate: bool,
  ) -> [Option<RECT>; 2] {
    magnifier.update(controller, |_| DESKTOP, None, now, animate)
  }

  #[test]
  fn update_reports_old_and_new_areas_and_clears_when_turned_off() {
    let now = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let [old, new] = step(&mut magnifier, &drawing((300.0, 300.0)), now, false);
    assert!(old.is_none() && new.is_some());
    assert_eq!(
      step(&mut magnifier, &drawing((300.0, 300.0)), now, false),
      [None, None],
      "an unchanged lens repaints nothing"
    );
    let [moved_from, moved_to] = step(&mut magnifier, &drawing((340.0, 300.0)), now, false);
    assert_eq!(moved_from, new);
    assert!(moved_to.is_some());

    magnifier.toggle();
    let [cleared, none] = step(&mut magnifier, &drawing((340.0, 300.0)), now, false);
    assert_eq!(cleared, moved_to);
    assert!(none.is_none());
    assert!(magnifier.lens().is_none());
    assert!(!magnifier.needs_update());
  }

  #[test]
  fn disabled_magnifier_never_places_a_lens() {
    let mut magnifier = Magnifier::new(false);
    let now = Instant::now();
    assert_eq!(
      step(&mut magnifier, &drawing((300.0, 300.0)), now, true),
      [None, None]
    );
    assert!(magnifier.lens().is_none());
    assert!(!magnifier.animating());
  }

  #[test]
  fn paint_failure_stops_the_lens_without_losing_the_area_to_clear() {
    let now = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let drawn = drawing((300.0, 300.0));
    let [_, painted] = step(&mut magnifier, &drawn, now, true);
    magnifier.turn_off();
    assert!(magnifier.lens().is_none());
    assert_eq!(step(&mut magnifier, &drawn, now, true)[0], painted);
    assert!(
      !magnifier.animating(),
      "a failed lens never keeps the clock running"
    );
    assert!(!magnifier.needs_update());
  }

  #[test]
  fn lens_fades_and_rises_in_then_settles_exactly() {
    let start = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let hovering = hover((600.0, 400.0));
    step(&mut magnifier, &hovering, start, true);
    assert!(magnifier.animating());
    assert!(
      magnifier.lens().is_none(),
      "the first frame is fully transparent"
    );

    let halfway = start + FADE_IN / 2;
    step(&mut magnifier, &hovering, halfway, true);
    let lens = magnifier.lens().expect("visible while fading");
    assert!(lens.alpha > 0 && lens.alpha < u8::MAX);
    let settled_top = 400 + POINTER_GAP;
    assert!(lens.rect.top > settled_top && lens.rect.top <= settled_top + RISE);

    step(&mut magnifier, &hovering, start + FADE_IN, true);
    let lens = magnifier.lens().expect("settled");
    assert_eq!((lens.alpha, lens.rect.top), (u8::MAX, settled_top));
    assert!(
      !magnifier.animating(),
      "the clock stops after the last frame"
    );
  }

  #[test]
  fn lens_fades_out_where_it_was_and_then_clears() {
    let start = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let hovering = hover((600.0, 400.0));
    step(&mut magnifier, &hovering, start, false);
    let shown = magnifier.lens().expect("shown");
    magnifier.toggle();
    step(&mut magnifier, &hovering, start, true);
    step(&mut magnifier, &hovering, start + FADE_OUT / 2, true);
    let fading = magnifier.lens().expect("still fading");
    assert!(fading.alpha < u8::MAX);
    assert_eq!(fading.focus, shown.focus);
    assert!(magnifier.needs_update());

    let [cleared, none] = step(&mut magnifier, &hovering, start + FADE_OUT, true);
    assert_eq!(cleared, Some(fading.rect));
    assert!(none.is_none());
    assert!(!magnifier.needs_update());
  }

  #[test]
  fn reversing_midway_continues_from_the_current_opacity() {
    let start = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let hovering = hover((600.0, 400.0));
    step(&mut magnifier, &hovering, start, true);
    let halfway = start + FADE_IN / 2;
    step(&mut magnifier, &hovering, halfway, true);
    let alpha = magnifier.lens().expect("fading in").alpha;
    magnifier.toggle();
    step(&mut magnifier, &hovering, halfway, true);
    assert_eq!(magnifier.lens().expect("no jump").alpha, alpha);
  }

  #[test]
  fn crossing_a_monitor_edge_glides_instead_of_jumping() {
    let start = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let edge = 1920 - POINTER_GAP - LENS_SIZE;
    step(&mut magnifier, &hover((edge as f64, 400.0)), start, false);
    let before = magnifier.lens().expect("shown").rect.left;

    let past = hover(((edge + 1) as f64, 400.0));
    step(&mut magnifier, &past, start, true);
    assert_eq!(magnifier.lens().expect("gliding").rect.left, before);
    assert!(magnifier.animating());

    step(&mut magnifier, &past, start + GLIDE / 2, true);
    let settled = edge + 1 - POINTER_GAP - LENS_SIZE;
    let middle = magnifier.lens().expect("gliding").rect.left;
    assert!(middle < before && middle > settled);

    step(&mut magnifier, &past, start + GLIDE, true);
    assert_eq!(magnifier.lens().expect("settled").rect.left, settled);
    assert!(!magnifier.animating());
  }

  #[test]
  fn animations_off_means_the_lens_appears_and_flips_at_once() {
    let now = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let edge = 1920 - POINTER_GAP - LENS_SIZE;
    step(&mut magnifier, &hover((edge as f64, 400.0)), now, false);
    assert_eq!(magnifier.lens().expect("instant").alpha, u8::MAX);
    assert!(!magnifier.animating());
    step(
      &mut magnifier,
      &hover(((edge + 1) as f64, 400.0)),
      now,
      false,
    );
    assert_eq!(
      magnifier.lens().expect("instant").rect.left,
      edge + 1 - POINTER_GAP - LENS_SIZE
    );
    assert!(!magnifier.animating());
  }

  fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF(u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16))
  }

  const FULL: RECT = RECT {
    left: 0,
    top: 0,
    right: 200,
    bottom: 200,
  };

  fn painted(focus: (i32, i32), rgba: &[u8], alpha: u8, damage: RECT) -> (ScreenshotBitmap, RECT) {
    let shot = ScreenshotBitmap::from_rgba(rgba, 64, 64).expect("synthetic capture");
    let target =
      ScreenshotBitmap::from_rgba(&vec![0u8; 200 * 200 * 4], 200, 200).expect("synthetic target");
    let rect = RECT {
      left: 10,
      top: 10,
      right: 10 + LENS_SIZE,
      bottom: 10 + LENS_SIZE,
    };
    let mut magnifier = Magnifier::new(true);
    magnifier.lens = Some(Lens {
      focus,
      rect,
      alpha,
      zoom: ZOOM,
    });
    magnifier
      .paint(target.dc, shot.dc, 64, 64, damage)
      .expect("lens paints");
    (target, rect)
  }

  /// The shared panel must look exactly like the Direct2D chrome it replaced, at any opacity
  /// and over any scene, and a plain lens must never set up Direct2D (that setup
  /// cost about 1.9 ms on every overlay's first frame).
  #[test]
  fn the_shared_panel_matches_direct2d_chrome_and_skips_its_setup() {
    let size = 200u32;
    let scene_pixels: Vec<u8> = (0..size * size)
      .flat_map(|i| [(i % 251) as u8, (i % 197) as u8, (i % 83) as u8, 255])
      .collect();
    let rect = RECT {
      left: 17,
      top: 23,
      right: 17 + LENS_SIZE,
      bottom: 23 + LENS_SIZE,
    };
    for alpha in [u8::MAX, 140, 9] {
      let cached = ScreenshotBitmap::from_rgba(&scene_pixels, size, size).expect("scene");
      let mut magnifier = Magnifier::new(true);
      magnifier
        .paint_panel(
          cached.dc,
          Lens {
            focus: (0, 0),
            rect,
            alpha,
            zoom: ZOOM,
          },
        )
        .expect("shared panel");
      assert_eq!(magnifier.chrome.surface_misses, 0, "no Direct2D surface");
      let direct = ScreenshotBitmap::from_rgba(&scene_pixels, size, size).expect("scene");
      let mut scene = Scene::default();
      scene.panel(rect).expect("panel");
      Renderer::default()
        .render_with_opacity(direct.dc, rect, &scene, alpha)
        .expect("Direct2D panel");
      let mut cached = cached;
      let mut direct = direct;
      assert!(
        cached.to_rgba().expect("cached pixels") == direct.to_rgba().expect("direct pixels"),
        "alpha {alpha}"
      );
    }
    // The plain lens paints fully without chrome; the Color Picker still uses it for its HEX.
    let shot = ScreenshotBitmap::from_rgba(&vec![120u8; 64 * 64 * 4], 64, 64).expect("capture");
    let target = ScreenshotBitmap::from_rgba(&vec![0u8; 200 * 200 * 4], 200, 200).expect("target");
    let mut plain = Magnifier::new(true);
    plain.lens = Some(Lens {
      focus: (10, 10),
      rect: RECT {
        left: 10,
        top: 10,
        right: 10 + LENS_SIZE,
        bottom: 10 + LENS_SIZE,
      },
      alpha: u8::MAX,
      zoom: ZOOM,
    });
    plain
      .paint(target.dc, shot.dc, 64, 64, FULL)
      .expect("plain lens");
    assert_eq!(plain.chrome.surface_misses, 0);
    let mut picker = Magnifier::picker();
    picker.lens = Some(Lens {
      focus: (10, 10),
      rect: RECT {
        left: 10,
        top: 10,
        right: 10 + LENS_SIZE,
        bottom: 10 + LENS_SIZE + SWATCH_ROW,
      },
      alpha: u8::MAX,
      zoom: ZOOM,
    });
    picker
      .paint(target.dc, shot.dc, 64, 64, FULL)
      .expect("picker lens");
    assert_eq!(picker.chrome.surface_misses, 1);
  }

  #[test]
  fn cached_layers_equal_freshly_computed_ones_at_every_zoom() {
    for zoom in ZOOM_LEVELS {
      assert_eq!(
        cached_overlay_pixels(zoom),
        overlay_pixels(zoom),
        "zoom {zoom}"
      );
      assert_eq!(
        cached_overlay_pixels(zoom),
        overlay_pixels(zoom),
        "cached zoom {zoom}"
      );
    }
    assert_eq!(
      cached_overlay_pixels(4),
      overlay_pixels(4),
      "uncached level"
    );
  }

  /// The Color Picker lens's first paint on a fresh overlay thread, without and with the spare
  /// chrome renderer the app keeps ready. Offscreen.
  #[test]
  #[ignore = "timing probe; run explicitly"]
  fn picker_first_paint_cost_probe() {
    let first_paint = || {
      std::thread::spawn(|| {
        let shot = ScreenshotBitmap::from_rgba(&vec![90u8; 64 * 64 * 4], 64, 64).expect("capture");
        let target =
          ScreenshotBitmap::from_rgba(&vec![0u8; 300 * 300 * 4], 300, 300).expect("target");
        let mut picker = Magnifier::picker();
        picker.lens = Some(Lens {
          focus: (10, 10),
          rect: RECT {
            left: 10,
            top: 10,
            right: 10 + LENS_SIZE,
            bottom: 10 + LENS_SIZE + SWATCH_ROW,
          },
          alpha: u8::MAX,
          zoom: ZOOM,
        });
        let full = RECT {
          left: 0,
          top: 0,
          right: 300,
          bottom: 300,
        };
        let started = Instant::now();
        picker
          .paint(target.dc, shot.dc, 64, 64, full)
          .expect("picker");
        started.elapsed().as_secs_f64() * 1000.0
      })
      .join()
      .expect("overlay thread")
    };
    let cold: Vec<f64> = (0..4).map(|_| first_paint()).collect();
    crate::overlay::chrome::warm_up();
    let warm: Vec<f64> = (0..4)
      .map(|_| {
        // Give the replacement spare time to build between overlays, as real use does.
        std::thread::sleep(Duration::from_millis(100));
        first_paint()
      })
      .collect();
    eprintln!("picker first paint without spare: {cold:.3?} ms; with spare: {warm:.3?} ms");
  }

  /// Where a fresh lens's first paint spends its time over a 1440p capture. Offscreen.
  #[test]
  #[ignore = "timing probe; run explicitly"]
  fn first_paint_cost_probe() {
    let (width, height) = (2560u32, 1440u32);
    let shot =
      ScreenshotBitmap::from_rgba(&vec![90u8; (width * height * 4) as usize], width, height)
        .expect("capture");
    let target =
      ScreenshotBitmap::from_rgba(&vec![0u8; (width * height * 4) as usize], width, height)
        .expect("target");
    let rect = RECT {
      left: 600,
      top: 400,
      right: 600 + LENS_SIZE,
      bottom: 400 + LENS_SIZE,
    };
    let damage = RECT {
      left: 0,
      top: 0,
      right: width as i32,
      bottom: height as i32,
    };
    let lens = Lens {
      focus: (580, 380),
      rect,
      alpha: u8::MAX,
      zoom: ZOOM,
    };
    for round in 0..5 {
      let mut magnifier = Magnifier::new(true);
      magnifier.lens = Some(lens);
      let started = Instant::now();
      magnifier
        .paint(target.dc, shot.dc, width as i32, height as i32, damage)
        .expect("first paint");
      let first = started.elapsed();
      let started = Instant::now();
      magnifier
        .paint(target.dc, shot.dc, width as i32, height as i32, damage)
        .expect("second paint");
      let second = started.elapsed();
      let started = Instant::now();
      let mut renderer = Renderer::default();
      let mut scene = Scene::default();
      scene.panel(rect).expect("panel");
      renderer
        .render_with_opacity(target.dc, rect, &scene, u8::MAX)
        .expect("chrome");
      let chrome = started.elapsed();
      let started = Instant::now();
      let _ = Layers::new(ZOOM).expect("layers");
      let layers = started.elapsed();
      eprintln!(
        "round {round}: first {:.3} ms, second {:.3} ms, fresh chrome panel {:.3} ms, layers {:.3} ms",
        first.as_secs_f64() * 1000.0,
        second.as_secs_f64() * 1000.0,
        chrome.as_secs_f64() * 1000.0,
        layers.as_secs_f64() * 1000.0
      );
    }
  }

  #[test]
  fn sampling_is_sharp_gridded_and_leaves_the_desktop_edge_empty() {
    let (width, height) = (64usize, 64usize);
    let mut rgba = vec![255u8; width * height * 4];
    for y in 0..height {
      for x in 0..width {
        let pixel = (y * width + x) * 4;
        let on = (x + y).is_multiple_of(2);
        rgba[pixel] = if on { 200 } else { 10 };
        rgba[pixel + 1] = 40;
        rgba[pixel + 2] = if on { 10 } else { 200 };
      }
    }
    let (target, rect) = painted((0, 30), &rgba, u8::MAX, FULL);
    let image = (rect.left + PANEL_INSET, rect.top + PANEL_INSET);
    // SAFETY: the target DC is live and these points lie inside it.
    let at = |x: i32, y: i32| unsafe { GetPixel(target.dc, x, y) };

    assert_eq!(
      at(rect.left + 2, rect.top + 40),
      OUTSIDE_COLOR,
      "panel surface"
    );
    assert_eq!(
      at(image.0 + 2, image.1 + 2),
      OUTSIDE_COLOR,
      "left of the desktop stays empty"
    );
    // Source column 2, row 18 sits away from the crosshair at lens cell (14, 0).
    let cell = |column: i32, dx: i32| at(image.0 + column * ZOOM + dx, image.1 + 2);
    let expected = |x: usize, y: usize| {
      if (x + y).is_multiple_of(2) {
        rgb(200, 40, 10)
      } else {
        rgb(10, 40, 200)
      }
    };
    assert_eq!(cell(14, 1), expected(2, 18));
    assert_eq!(
      cell(14, ZOOM - 1),
      expected(2, 18),
      "one source pixel is one flat block"
    );
    assert_eq!(cell(15, 1), expected(3, 18), "neighbours are not blended");
    assert_ne!(
      cell(14, 0),
      cell(14, 1),
      "the grid marks the pixel boundary"
    );
  }

  #[test]
  fn crosshair_is_flinch_red_and_leaves_the_marked_pixel_visible() {
    let (target, rect) = painted((30, 30), &vec![255u8; 64 * 64 * 4], u8::MAX, FULL);
    let corner = PANEL_INSET + (SOURCE_PIXELS / 2) * ZOOM;
    // SAFETY: the target DC is live and these points lie inside it.
    let at = |x: i32, y: i32| unsafe { GetPixel(target.dc, rect.left + x, rect.top + y) };
    assert_eq!(at(corner - 1, corner - 1), CROSSHAIR_COLOR);
    assert_eq!(at(corner - 2, corner - 2), CROSSHAIR_OUTLINE);
    assert_eq!(at(corner + 2, corner + 2), rgb(255, 255, 255));
  }

  #[test]
  fn image_corners_are_rounded_into_the_panel() {
    let (target, rect) = painted((30, 30), &vec![255u8; 64 * 64 * 4], u8::MAX, FULL);
    let image = (rect.left + PANEL_INSET, rect.top + PANEL_INSET);
    // SAFETY: the target DC is live and these points lie inside it.
    let at = |x: i32, y: i32| unsafe { GetPixel(target.dc, x, y) };
    let far = IMAGE_SIZE - 1;
    for (x, y) in [(0, 0), (far, 0), (0, far), (far, far)] {
      assert_eq!(
        at(image.0 + x, image.1 + y),
        OUTSIDE_COLOR,
        "corner {x},{y}"
      );
    }
    assert_eq!(
      at(image.0 + 11, image.1 + 2),
      rgb(255, 255, 255),
      "past the curve"
    );
    let edge = image_coverage(3, 2);
    assert!(
      edge > 0.0 && edge < 1.0,
      "the curve is antialiased, was {edge}"
    );
  }

  #[test]
  fn grid_follows_the_zoom_and_steps_aside_when_pixels_are_tiny() {
    let line =
      |zoom: i32, x: usize, y: usize| overlay_pixels(zoom)[(y * IMAGE_SIZE as usize + x) * 4 + 3];
    let center = center_cell(9) as usize;
    assert_eq!(line(9, center, 62), GRID_ALPHA);
    assert_eq!(line(9, center + 9, 62), GRID_ALPHA);
    assert_eq!(line(9, center + 4, 60), 0);
    assert_eq!(line(3, center, 62), 0);
  }

  #[test]
  fn every_zoom_keeps_the_marked_pixel_centered_and_the_image_filled() {
    for zoom in ZOOM_LEVELS {
      assert_eq!(zoom % 2, 1);
      let center = center_cell(zoom) * 2 + zoom;
      assert_eq!(center, IMAGE_SIZE, "{zoom}");
      let clip = source_clip((500, 500), 1000, 1000, zoom).expect("inside the capture");
      assert!(clip.image_x <= 0 && clip.image_y <= 0, "{zoom}");
      let width = clip.source.right - clip.source.left;
      assert!(clip.image_x + width * zoom >= IMAGE_SIZE, "{zoom}");
    }
  }

  #[test]
  fn higher_zoom_shows_the_same_pixel_larger() {
    let mut rgba = vec![255u8; 64 * 64 * 4];
    let marked = (30 * 64 + 30) * 4;
    rgba[marked..marked + 3].copy_from_slice(&[10, 200, 40]);
    let shot = ScreenshotBitmap::from_rgba(&rgba, 64, 64).expect("synthetic capture");
    let target =
      ScreenshotBitmap::from_rgba(&vec![0u8; 200 * 200 * 4], 200, 200).expect("synthetic target");
    let rect = RECT {
      left: 10,
      top: 10,
      right: 10 + LENS_SIZE,
      bottom: 10 + LENS_SIZE,
    };
    let mut magnifier = Magnifier::new(true);
    magnifier.lens = Some(Lens {
      focus: (30, 30),
      rect,
      alpha: u8::MAX,
      zoom: 17,
    });
    magnifier
      .paint(target.dc, shot.dc, 64, 64, FULL)
      .expect("lens paints");
    let cell = center_cell(17) + PANEL_INSET;
    // SAFETY: the target DC is live and these points lie inside it.
    let at = |x: i32, y: i32| unsafe { GetPixel(target.dc, rect.left + x, rect.top + y) };
    assert_eq!(at(cell + 8, cell + 8), rgb(10, 200, 40));
    assert_eq!(at(cell + 15, cell + 15), rgb(10, 200, 40));
    assert_eq!(at(cell + 20, cell + 20), rgb(255, 255, 255));
  }

  #[test]
  fn the_picker_lens_is_taller_flips_at_the_bottom_and_shows_the_marked_pixel_swatch() {
    let now = Instant::now();
    let mut picker = Magnifier::picker();
    step(&mut picker, &hover((600.0, 400.0)), now, false);
    let lens = picker.lens().expect("always on");
    assert_eq!(lens.rect.right - lens.rect.left, LENS_SIZE);
    assert_eq!(lens.rect.bottom - lens.rect.top, LENS_SIZE + SWATCH_ROW);
    // Near the bottom the taller lens flips above the pointer where a square one would fit.
    let near = (1080 - POINTER_GAP - LENS_SIZE - SWATCH_ROW / 2) as f64;
    step(&mut picker, &hover((600.0, near)), now, false);
    assert!(picker.lens().expect("on").rect.bottom <= near as i32);

    let mut rgba = vec![0u8; 64 * 64 * 4];
    let at = (20 * 64 + 30) * 4;
    rgba[at..at + 4].copy_from_slice(&[217, 47, 47, 255]);
    let shot = ScreenshotBitmap::from_rgba(&rgba, 64, 64).expect("synthetic capture");
    let target =
      ScreenshotBitmap::from_rgba(&vec![0u8; 200 * 200 * 4], 200, 200).expect("synthetic target");
    let rect = RECT {
      left: 10,
      top: 10,
      right: 10 + LENS_SIZE,
      bottom: 10 + LENS_SIZE + SWATCH_ROW,
    };
    let mut picker = Magnifier::picker();
    picker.lens = Some(Lens {
      focus: (30, 20),
      rect,
      alpha: u8::MAX,
      zoom: ZOOM,
    });
    picker
      .paint(target.dc, shot.dc, 64, 64, FULL)
      .expect("picker lens paints");
    let image_bottom = rect.top + PANEL_INSET + IMAGE_SIZE;
    let middle = image_bottom + (rect.bottom - image_bottom) / 2;
    // SAFETY: the target DC is live and every point read lies inside it.
    let at = |x: i32, y: i32| unsafe { GetPixel(target.dc, x, y) };
    let marked = rgb(217, 47, 47);
    let swatch: Vec<i32> = (rect.left..rect.right)
      .filter(|x| at(*x, middle) == marked)
      .collect();
    assert_eq!(swatch.len() as i32, SWATCH_SIZE - 2, "the swatch's fill");
    let outline = rgb(theme::OUTLINE[0], theme::OUTLINE[1], theme::OUTLINE[2]);
    assert_eq!(
      at(swatch[0] - 1, middle),
      outline,
      "a ring, not a stroke over the fill"
    );
    let surface = rgb(theme::SURFACE[0], theme::SURFACE[1], theme::SURFACE[2]);
    // Inside the panel outline, which also differs from the surface.
    let text: Vec<i32> = (swatch[swatch.len() - 1] + 2..rect.right - PANEL_INSET)
      .filter(|x| (middle - 6..middle + 6).any(|y| at(*x, y) != surface))
      .collect();
    let (first, last) = (text[0], text[text.len() - 1]);
    assert!(
      first - (swatch[swatch.len() - 1] + 1) <= SWATCH_GAP + 2,
      "the HEX sits right beside the swatch"
    );
    let (left_margin, right_margin) = (swatch[0] - 1 - rect.left, rect.right - 1 - last);
    assert!(text.len() > 10, "the HEX is drawn");
    assert!(
      (left_margin - right_margin).abs() <= 4,
      "centered: {left_margin} vs {right_margin}"
    );
    assert_eq!(hex([217, 47, 47]), "#D92F2F");
  }

  #[test]
  #[ignore = "explicit offscreen Color Picker lens preview"]
  fn render_picker_lens_preview() {
    let output = std::path::PathBuf::from(
      std::env::var("FLINCH_MAGNIFIER_PREVIEW_OUTPUT").expect("output directory"),
    );
    std::fs::create_dir_all(&output).expect("output");
    for (name, color) in [
      ("red", [217u8, 47, 47]),
      ("black", [0, 0, 0]),
      ("white", [255, 255, 255]),
    ] {
      let mut rgba = vec![0u8; 64 * 64 * 4];
      for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let shade = ((index % 64 + index / 64) * 3) as u8;
        pixel.copy_from_slice(&[shade, 90, 160 - shade / 2, 255]);
      }
      let at = (20 * 64 + 30) * 4;
      rgba[at..at + 4].copy_from_slice(&[color[0], color[1], color[2], 255]);
      let shot = ScreenshotBitmap::from_rgba(&rgba, 64, 64).expect("synthetic capture");
      let mut target =
        ScreenshotBitmap::from_rgba(&vec![255u8; 200 * 200 * 4], 200, 200).expect("target");
      let mut picker = Magnifier::picker();
      picker.lens = Some(Lens {
        focus: (30, 20),
        rect: RECT {
          left: 20,
          top: 20,
          right: 20 + LENS_SIZE,
          bottom: 20 + LENS_SIZE + SWATCH_ROW,
        },
        alpha: u8::MAX,
        zoom: 9,
      });
      picker
        .paint(target.dc, shot.dc, 64, 64, FULL)
        .expect("lens paints");
      let pixels = target.to_rgba().expect("pixels");
      image::RgbaImage::from_raw(200, 200, pixels)
        .expect("image")
        .save(output.join(format!("picker-{name}.png")))
        .expect("save");
    }
  }

  #[test]
  fn wheel_steps_through_levels_and_stops_at_each_end() {
    let now = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let hovering = hover((600.0, 400.0));
    assert!(
      !magnifier.wheel(WHEEL_NOTCH),
      "no lens yet, so the wheel stays free"
    );
    step(&mut magnifier, &hovering, now, false);
    assert_eq!(magnifier.lens().expect("shown").zoom, 5);

    assert!(magnifier.wheel(WHEEL_NOTCH));
    let [old, new] = step(&mut magnifier, &hovering, now, false);
    assert!(
      old.is_some() && new.is_some(),
      "a new zoom repaints the lens"
    );
    assert_eq!(magnifier.lens().expect("shown").zoom, 7);

    for _ in 0..20 {
      assert!(magnifier.wheel(WHEEL_NOTCH));
    }
    step(&mut magnifier, &hovering, now, false);
    assert_eq!(magnifier.lens().expect("shown").zoom, 17);
    for _ in 0..20 {
      magnifier.wheel(-WHEEL_NOTCH);
    }
    step(&mut magnifier, &hovering, now, false);
    assert_eq!(magnifier.lens().expect("shown").zoom, 3);
  }

  #[test]
  fn high_resolution_wheels_add_up_to_a_notch() {
    let now = Instant::now();
    let mut magnifier = Magnifier::new(true);
    let hovering = hover((600.0, 400.0));
    step(&mut magnifier, &hovering, now, false);
    magnifier.wheel(40);
    magnifier.wheel(40);
    step(&mut magnifier, &hovering, now, false);
    assert_eq!(magnifier.lens().expect("shown").zoom, 5);
    magnifier.wheel(40);
    step(&mut magnifier, &hovering, now, false);
    assert_eq!(magnifier.lens().expect("shown").zoom, 7);
  }

  #[test]
  fn fading_lens_blends_with_the_scene() {
    let (target, rect) = painted((30, 30), &vec![255u8; 64 * 64 * 4], 128, FULL);
    let x = rect.left + PANEL_INSET + 2 * ZOOM + 2;
    let y = rect.top + PANEL_INSET + 2 * ZOOM + 2;
    // SAFETY: the target DC is live and this point lies inside it.
    let red = unsafe { GetPixel(target.dc, x, y) }.0 & 0xFF;
    assert!(
      (100..=156).contains(&red),
      "half-faded white over black was {red}"
    );
  }

  #[test]
  fn translucent_frame_waits_for_damage_that_covers_it() {
    let partial = RECT {
      left: 0,
      top: 0,
      right: 40,
      bottom: 40,
    };
    let (target, rect) = painted((30, 30), &vec![255u8; 64 * 64 * 4], 128, partial);
    // SAFETY: the target DC is live and this point lies inside it.
    let pixel = unsafe { GetPixel(target.dc, rect.left + 60, rect.top + 60) };
    assert_eq!(pixel, rgb(0, 0, 0), "nothing built up over stale pixels");
  }
}
