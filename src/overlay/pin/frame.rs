//! A pin's geometry and pixels, free of Win32 so it can be tested directly: zoom steps, the
//! anchored zoom, the shadowed frame and its slices, and where a pin lands.

/// Zoom steps; `ORIGINAL` is 1:1, one capture pixel per screen pixel.
pub(crate) const ZOOMS: [f32; 12] = [
  0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0,
];
pub(crate) const ORIGINAL: usize = 6;
/// Opacity steps in percent; Ctrl+wheel moves between them.
pub(crate) const OPACITIES: [u8; 9] = [20, 30, 40, 50, 60, 70, 80, 90, 100];
/// A zoomed pin's window stays within these, so 400% of a large screenshot cannot ask for
/// gigabytes of window bitmap.
const MAX_SIDE: u64 = 16_384;
const MAX_PIXELS: u64 = 64_000_000;
/// Shadow width at 100% scaling.
const SHADOW: i32 = 12;
const SHADOW_ALPHA: f32 = 70.0;
/// A thin gray ring separates the pin from a background of the same color, dark or light.
const OUTLINE_GRAY: u8 = 128;
const OUTLINE_ALPHA: u8 = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
  pub(crate) left: i32,
  pub(crate) top: i32,
  pub(crate) right: i32,
  pub(crate) bottom: i32,
}

impl Bounds {
  fn intersects(self, other: Bounds) -> bool {
    self.left < other.right
      && other.left < self.right
      && self.top < other.bottom
      && other.top < self.bottom
  }
}

pub(crate) fn scaled_size(width: u32, height: u32, zoom: f32) -> (u32, u32) {
  let side = |value: u32| ((value as f32 * zoom).round() as u32).max(1);
  (side(width), side(height))
}

fn fits(width: u32, height: u32, zoom: f32, margin: i32) -> bool {
  let (w, h) = scaled_size(width, height, zoom);
  let (w, h) = (
    u64::from(w) + 2 * margin.max(0) as u64,
    u64::from(h) + 2 * margin.max(0) as u64,
  );
  w <= MAX_SIDE && h <= MAX_SIDE && w * h <= MAX_PIXELS
}

/// The zoom step after `notches` wheel notches (positive zooms in), never past what fits.
pub(crate) fn zoom_after(
  current: usize,
  notches: i32,
  width: u32,
  height: u32,
  margin: i32,
) -> usize {
  let target = (current as i32 + notches).clamp(0, ZOOMS.len() as i32 - 1) as usize;
  if target <= current {
    return target;
  }
  (current..=target)
    .rev()
    .find(|step| fits(width, height, ZOOMS[*step], margin))
    .unwrap_or(current)
}

/// The largest step at or below 1:1 that fits, for a pin first shown.
pub(crate) fn opening_zoom(width: u32, height: u32, margin: i32) -> usize {
  (0..=ORIGINAL)
    .rev()
    .find(|step| fits(width, height, ZOOMS[*step], margin))
    .unwrap_or(0)
}

pub(crate) fn opacity_after(current: usize, notches: i32) -> usize {
  (current as i32 + notches).clamp(0, OPACITIES.len() as i32 - 1) as usize
}

/// Shadow width for a monitor's DPI.
pub(crate) fn margin(dpi: u32) -> i32 {
  (SHADOW * dpi.clamp(96, 480) as i32 + 48) / 96
}

/// Resizes straight RGBA: nearest pixel when enlarging and the average of each covered block when
/// shrinking. The CPU pin's scaling, kept as the reference its GPU replacement is measured against.
#[cfg(test)]
pub(crate) fn scale(
  source: &[u8],
  width: u32,
  height: u32,
  to_width: u32,
  to_height: u32,
) -> Vec<u8> {
  let (w, h, tw, th) = (
    width as usize,
    height as usize,
    to_width as usize,
    to_height as usize,
  );
  if (w, h) == (tw, th) {
    return source.to_vec();
  }
  let mut out = vec![0u8; tw * th * 4];
  let span = |index: usize, from: usize, to: usize| {
    let start = index * from / to;
    let end = ((index + 1) * from / to).max(start + 1).min(from);
    (start, end)
  };
  for y in 0..th {
    let (top, bottom) = span(y, h, th);
    for x in 0..tw {
      let (left, right) = span(x, w, tw);
      let mut sum = [0u32; 4];
      for row in top..bottom {
        let base = row * w * 4;
        for column in left..right {
          let pixel = &source[base + column * 4..base + column * 4 + 4];
          for channel in 0..4 {
            sum[channel] += u32::from(pixel[channel]);
          }
        }
      }
      let count = ((bottom - top) * (right - left)) as u32;
      let target = (y * tw + x) * 4;
      for channel in 0..4 {
        out[target + channel] = ((sum[channel] + count / 2) / count) as u8;
      }
    }
  }
  out
}

/// Writes the window pixels, premultiplied BGRA: the image at (`margin`, `margin`), a thin
/// outline around it and a soft shadow, slightly lower, in the margin.
pub(crate) fn compose(pixels: &mut [u8], image: &[u8], width: u32, height: u32, margin: i32) {
  let (w, h, m) = (width as i32, height as i32, margin);
  let stride = w + 2 * m;
  let drop = shadow_drop(m);
  for y in 0..h + 2 * m {
    for x in 0..stride {
      let target = ((y * stride + x) * 4) as usize;
      let (ix, iy) = (x - m, y - m);
      if (0..w).contains(&ix) && (0..h).contains(&iy) {
        let source = ((iy * w + ix) * 4) as usize;
        let [r, g, b, a] = [
          image[source],
          image[source + 1],
          image[source + 2],
          image[source + 3],
        ];
        let premultiply = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        pixels[target..target + 4].copy_from_slice(&[
          premultiply(b),
          premultiply(g),
          premultiply(r),
          a,
        ]);
        continue;
      }
      let outside =
        |position: i32, low: i32, high: i32| (low - position).max(position - high).max(0);
      let ring = outside(ix, 0, w - 1).max(outside(iy, 0, h - 1));
      let dx = outside(ix, 0, w - 1) as f32;
      let dy = outside(iy - drop, 0, h - 1) as f32;
      let distance = (dx * dx + dy * dy).sqrt();
      let falloff = (1.0 - distance / m.max(1) as f32).max(0.0);
      let shadow = (SHADOW_ALPHA * falloff * falloff).round() as u8;
      let pixel = if ring == 1 {
        let gray = ((u16::from(OUTLINE_GRAY) * u16::from(OUTLINE_ALPHA) + 127) / 255) as u8;
        [gray, gray, gray, OUTLINE_ALPHA.max(shadow)]
      } else {
        [0, 0, 0, shadow]
      };
      pixels[target..target + 4].copy_from_slice(&pixel);
    }
  }
}

/// How much lower than the image its shadow falls.
pub(crate) fn shadow_drop(margin: i32) -> i32 {
  (margin / 4).max(1)
}

/// A rectangle in pixels: left, top, width, height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Area {
  pub(crate) x: i32,
  pub(crate) y: i32,
  pub(crate) width: i32,
  pub(crate) height: i32,
}

const fn area(x: i32, y: i32, width: i32, height: i32) -> Area {
  Area {
    x,
    y,
    width,
    height,
  }
}

/// One of the eight pieces of a pin's outline and shadow: `source` in the `template`, shown
/// stretched over `target` in the window. Along an edge every row (or column) is the same, so a
/// one-pixel strip stretched to any length draws it exactly and the frame never needs redrawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
  pub(crate) source: Area,
  pub(crate) target: Area,
}

/// The template's size: the frame of an image one pixel wide and just tall enough for every
/// edge's repeated row.
pub(crate) fn template_size(margin: i32) -> (u32, u32) {
  let m = margin.max(1);
  ((2 * m + 1) as u32, (2 * m + shadow_drop(m) + 1) as u32)
}

/// The template's pixels, premultiplied BGRA: `compose` around a transparent image.
pub(crate) fn template(margin: i32) -> Vec<u8> {
  let m = margin.max(1);
  let (width, height) = template_size(m);
  let image_height = height - 2 * m as u32;
  let image = vec![0u8; (image_height * 4) as usize];
  let mut pixels = vec![0u8; (width * height * 4) as usize];
  compose(&mut pixels, &image, 1, image_height, m);
  pixels
}

/// Where each template piece goes around an image of `width` × `height` placed at
/// (`margin`, `margin`) in its window: corners, then the top, side and bottom edges.
pub(crate) fn slices(margin: i32, width: i32, height: i32) -> [Piece; 8] {
  let m = margin.max(1);
  let (w, h) = (width.max(1), height.max(1));
  let drop = shadow_drop(m);
  // The template image is `drop + 1` tall; its bottom corners start right below it.
  let below = m + drop + 1;
  // Top corners reach `drop` rows down the image's sides, never past its bottom.
  let upper = m + drop.min(h);
  let side = h - drop.min(h);
  let piece = |source, target| Piece { source, target };
  [
    piece(area(0, 0, m, upper), area(0, 0, m, upper)),
    piece(area(m + 1, 0, m, upper), area(m + w, 0, m, upper)),
    piece(area(0, below, m, m), area(0, m + h, m, m)),
    piece(area(m + 1, below, m, m), area(m + w, m + h, m, m)),
    piece(area(m, 0, 1, m), area(m, 0, w, m)),
    piece(area(0, m + drop, m, 1), area(0, upper, m, side)),
    piece(area(m + 1, m + drop, m, 1), area(m + w, upper, m, side)),
    piece(area(m, below, 1, m), area(m, m + h, w, m)),
  ]
}

/// The window's size for a frame, given the size this frame's image needs, the size the zoom
/// heads to and the size last placed. While zooming the window only grows, straight to what the
/// target needs, so a zoom resizes it at most once at its start (zooming in) or once it settles
/// (zooming out). The taskbar jumps in front of a topmost window resized every frame; moves are
/// fine, so the window still follows the image's corner each frame.
pub(crate) fn window_size(
  frame: (i32, i32),
  target: (i32, i32),
  placed: Option<(i32, i32)>,
  zooming: bool,
) -> (i32, i32) {
  if !zooming {
    return frame;
  }
  let (width, height) = placed.unwrap_or(frame);
  (
    width.max(frame.0).max(target.0),
    height.max(frame.1).max(target.1),
  )
}

/// The part of a pin's rectangle its window shows: clipped to the screens' bounding box, and a
/// pixel short of any monitor it would cover entirely. Windows takes a topmost window covering a
/// monitor for a full-screen app and keeps raising the taskbar in front of it while it moves.
/// Clipped, a pin larger than the screen also stops moving its window while it zooms or drags.
pub(crate) fn shown(pin: Bounds, monitors: &[Bounds]) -> Bounds {
  let Some(screen) = monitors.iter().copied().reduce(|a, b| Bounds {
    left: a.left.min(b.left),
    top: a.top.min(b.top),
    right: a.right.max(b.right),
    bottom: a.bottom.max(b.bottom),
  }) else {
    return pin;
  };
  let mut shown = Bounds {
    left: pin.left.max(screen.left),
    top: pin.top.max(screen.top),
    right: pin.right.min(screen.right),
    bottom: pin.bottom.min(screen.bottom),
  };
  // Off every screen: left whole, for `rehome` to bring back.
  if shown.right - shown.left < 2 || shown.bottom - shown.top < 2 {
    return pin;
  }
  for monitor in monitors {
    let covers = shown.left <= monitor.left
      && shown.top <= monitor.top
      && shown.right >= monitor.right
      && shown.bottom >= monitor.bottom;
    if !covers {
      continue;
    }
    // The side that ends on this monitor, so nothing on a neighbor is cut. Trimming only
    // shrinks, so it never makes another monitor covered. A monitor with neighbors on all four
    // sides stays covered rather than hiding a row of a neighbor.
    if shown.bottom == monitor.bottom {
      shown.bottom -= 1;
    } else if shown.top == monitor.top {
      shown.top += 1;
    } else if shown.right == monitor.right {
      shown.right -= 1;
    } else if shown.left == monitor.left {
      shown.left += 1;
    }
  }
  shown
}

/// Where a window that no longer overlaps any monitor should move: fully inside the nearest
/// one. `None` when it still shows somewhere.
pub(crate) fn rehome(window: Bounds, monitors: &[Bounds]) -> Option<(i32, i32)> {
  if monitors.is_empty() || monitors.iter().any(|monitor| window.intersects(*monitor)) {
    return None;
  }
  let center = (
    (window.left + window.right) / 2,
    (window.top + window.bottom) / 2,
  );
  let distance = |monitor: &Bounds| {
    let dx = (monitor.left - center.0)
      .max(center.0 - monitor.right)
      .max(0) as i64;
    let dy = (monitor.top - center.1)
      .max(center.1 - monitor.bottom)
      .max(0) as i64;
    dx * dx + dy * dy
  };
  let nearest = monitors.iter().min_by_key(|monitor| distance(monitor))?;
  Some(inside(window, *nearest))
}

/// The window's top-left kept fully inside `monitor` (its top-left when it is larger).
pub(crate) fn inside(window: Bounds, monitor: Bounds) -> (i32, i32) {
  let (width, height) = (window.right - window.left, window.bottom - window.top);
  let clamp = |position: i32, low: i32, high: i32, size: i32| position.min(high - size).max(low);
  (
    clamp(window.left, monitor.left, monitor.right, width),
    clamp(window.top, monitor.top, monitor.bottom, height),
  )
}

/// A pin opened from a file: centered on the monitor.
pub(crate) fn centered(width: i32, height: i32, monitor: Bounds) -> (i32, i32) {
  let window = Bounds {
    left: monitor.left + (monitor.right - monitor.left - width) / 2,
    top: monitor.top + (monitor.bottom - monitor.top - height) / 2,
    right: 0,
    bottom: 0,
  };
  inside(
    Bounds {
      right: window.left + width,
      bottom: window.top + height,
      ..window
    },
    monitor,
  )
}
