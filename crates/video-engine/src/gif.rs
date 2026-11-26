//! Native GIF encoding core: BT.709 decode to RGB, one global palette, dithering and
//! frame-difference writing. Platform independent; the Windows side feeds decoded NV12 frames.
//!
//! Palette entries 0..=254 hold colors; index 255 is reserved for transparency, so pixels
//! unchanged since the previous frame cost almost nothing after LZW.

use crate::types::{EngineError, ErrorKind, Result};
use std::borrow::Cow;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

/// Colors available to the picture; the last palette slot is transparency.
pub const COLORS: usize = 255;
pub const TRANSPARENT: u8 = 255;
/// Pixels kept for palette training across the whole recording (RGBA, 4 MiB).
pub const SAMPLE_BUDGET: usize = 1 << 20;
/// A frame is sampled at most this densely, so short recordings still bound training time.
const SAMPLES_PER_FRAME: usize = 1 << 15;
/// Shortest delay written. Browsers replace delays under 2 cs with about 10 cs, which would
/// slow a fast recording fivefold, so frames closer than this are coalesced instead.
pub const MIN_DELAY_CS: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Dither {
  None,
  /// Ordered 8×8 Bayer; larger scales weaken the pattern (0..=5, as FFmpeg's `bayer_scale`).
  Bayer {
    scale: u8,
  },
  FloydSteinberg,
  /// Sierra-2-4A ("filter lite"), FFmpeg's `paletteuse` default.
  Sierra24a,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GifOptions {
  pub dither: Dither,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GifReport {
  pub width: u32,
  pub height: u32,
  pub decoded_frames: u64,
  /// Frames written to the file (including the first, full frame).
  pub written_frames: u64,
  /// Frames identical to the canvas after quantization; their time extends the previous frame.
  pub unchanged_frames: u64,
  /// Frames within [`MIN_DELAY_CS`] of the previous shown frame; they are not shown.
  pub coalesced_frames: u64,
  pub duration_cs: u64,
  pub palette_colors: u32,
  pub bytes: u64,
}

/// Convert one BT.709 limited-range NV12 picture (the engine's output) to packed RGB.
pub fn nv12_to_rgb(
  plane: &[u8],
  stride: usize,
  coded_height: usize,
  width: usize,
  height: usize,
  rgb: &mut Vec<u8>,
) -> Result<()> {
  Nv12::new(plane, stride, coded_height, width, height)?.to_rgb(rgb);
  Ok(())
}

/// A checked view of one decoded BT.709 limited-range NV12 picture.
pub struct Nv12<'a> {
  plane: &'a [u8],
  stride: usize,
  /// Offset of the interleaved chroma plane.
  chroma: usize,
  width: usize,
  height: usize,
}
impl<'a> Nv12<'a> {
  pub fn new(
    plane: &'a [u8],
    stride: usize,
    coded_height: usize,
    width: usize,
    height: usize,
  ) -> Result<Self> {
    let chroma = stride
      .checked_mul(coded_height)
      .ok_or_else(|| EngineError::protocol("NV12 size overflow"))?;
    let needed = chroma
      .checked_add(stride * coded_height.div_ceil(2))
      .ok_or_else(|| EngineError::protocol("NV12 size overflow"))?;
    if width > stride
      || height > coded_height
      || !width.is_multiple_of(2)
      || !height.is_multiple_of(2)
      || plane.len() < needed
    {
      return Err(EngineError::protocol(
        "Decoded NV12 picture is smaller than its extent",
      ));
    }
    Ok(Self {
      plane,
      stride,
      chroma,
      width,
      height,
    })
  }
  pub fn pixels(&self) -> usize {
    self.width * self.height
  }
  /// Visible width and height.
  pub fn extent(&self) -> (usize, usize) {
    (self.width, self.height)
  }
  /// The average color of the block `[x0, x1) × [y0, y1)`, a non-empty block inside the picture:
  /// luma and chroma are averaged first and converted once, which a box downscale allows because
  /// the conversion is linear before clamping.
  pub fn average(&self, x0: usize, x1: usize, y0: usize, y1: usize) -> [u8; 3] {
    let mean = |sum: u64, count: u64| ((sum + count / 2) / count.max(1)) as u8;
    let mut luma = 0u64;
    for y in y0..y1 {
      let row = y * self.stride;
      luma += self.plane[row + x0..row + x1]
        .iter()
        .map(|v| u64::from(*v))
        .sum::<u64>();
    }
    let (mut cb, mut cr, mut samples) = (0u64, 0u64, 0u64);
    for cy in y0 / 2..=(y1 - 1) / 2 {
      let row = self.chroma + cy * self.stride;
      for cx in x0 / 2..=(x1 - 1) / 2 {
        cb += u64::from(self.plane[row + cx * 2]);
        cr += u64::from(self.plane[row + cx * 2 + 1]);
        samples += 1;
      }
    }
    convert(
      mean(luma, ((x1 - x0) * (y1 - y0)) as u64),
      mean(cb, samples),
      mean(cr, samples),
    )
  }
  /// The RGB of pixel `index` in row-major order (inside the picture).
  pub fn pixel(&self, index: usize) -> [u8; 3] {
    let (x, y) = (index % self.width, index / self.width);
    let uv = self.chroma + (y / 2) * self.stride;
    convert(
      self.plane[y * self.stride + x],
      self.plane[uv + (x & !1)],
      self.plane[uv + (x | 1)],
    )
  }
  pub fn to_rgb(&self, rgb: &mut Vec<u8>) {
    rgb.clear();
    rgb.reserve(self.pixels() * 3);
    for y in 0..self.height {
      let luma = &self.plane[y * self.stride..y * self.stride + self.width];
      let uv = &self.plane[self.chroma + (y / 2) * self.stride..][..self.width];
      for x in 0..self.width {
        rgb.extend_from_slice(&convert(luma[x], uv[x & !1], uv[x | 1]));
      }
    }
  }
}
fn convert(luma: u8, cb: u8, cr: u8) -> [u8; 3] {
  let l = (i32::from(luma) - 16) * 76_309;
  let cb = i32::from(cb) - 128;
  let cr = i32::from(cr) - 128;
  let channel = |v: i32| ((v + 32_768) >> 16).clamp(0, 255) as u8;
  [
    channel(l + 117_489 * cr),
    channel(l - 13_975 * cb - 34_923 * cr),
    channel(l + 138_438 * cb),
  ]
}

/// Bounded, evenly spread pixel sample for palette training.
#[derive(Default)]
pub struct PaletteSampler {
  rgba: Vec<u8>,
  /// Keep one pixel in `stride` from each frame; doubles whenever the budget fills.
  stride: usize,
}
impl PaletteSampler {
  pub fn add(&mut self, rgb: &[u8]) {
    self.add_with(rgb.len() / 3, |index| {
      [rgb[index * 3], rgb[index * 3 + 1], rgb[index * 3 + 2]]
    });
  }
  /// [`PaletteSampler::add`] straight from a decoded picture: only the sampled pixels are
  /// converted, with the same arithmetic, so the samples are identical.
  pub fn add_nv12(&mut self, picture: &Nv12<'_>) {
    self.add_with(picture.pixels(), |index| picture.pixel(index));
  }
  fn add_with(&mut self, pixels: usize, pixel: impl Fn(usize) -> [u8; 3]) {
    if pixels == 0 {
      return;
    }
    let stride = self.stride.max(pixels.div_ceil(SAMPLES_PER_FRAME)).max(1);
    // Prime stride offsets avoid sampling one column of a regular pattern.
    let mut index = (self.rgba.len() / 4 * 7919) % stride;
    while index < pixels {
      let [r, g, b] = pixel(index);
      self.rgba.extend_from_slice(&[r, g, b, 255]);
      index += stride;
    }
    while self.rgba.len() / 4 > SAMPLE_BUDGET {
      // Halve every frame's share so early frames are not favoured over late ones.
      self.rgba = self
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .step_by(2)
        .flatten()
        .copied()
        .collect();
      self.stride = self.stride.max(stride) * 2;
    }
  }
  pub fn samples(&self) -> usize {
    self.rgba.len() / 4
  }
  pub fn palette(&self) -> Result<Palette> {
    if self.rgba.is_empty() {
      return Err(EngineError::protocol(
        "No decoded pixels to build a GIF palette",
      ));
    }
    let mut histogram: std::collections::HashMap<[u8; 3], u32> = std::collections::HashMap::new();
    for pixel in self.rgba.as_chunks::<4>().0 {
      *histogram.entry([pixel[0], pixel[1], pixel[2]]).or_default() += 1;
    }
    let mut colors: Vec<([u8; 3], u32)> = histogram.into_iter().collect();
    // Deterministic order regardless of hashing.
    colors.sort_unstable();
    // Few distinct colors (UI, text, flat graphics) are kept exactly instead of being reduced.
    let colors = if colors.len() <= COLORS {
      colors.into_iter().map(|(color, _)| color).collect()
    } else {
      median_cut(colors, COLORS)
    };
    Palette::new(colors)
  }
}

/// One median-cut box: its colors with counts, and the weighted squared error of its mean.
struct ColorBox {
  colors: Vec<([u8; 3], u32)>,
  error: f64,
  mean: [u8; 3],
}
impl ColorBox {
  fn new(colors: Vec<([u8; 3], u32)>) -> Self {
    let mut count = 0f64;
    let mut sum = [0f64; 3];
    let mut squares = [0f64; 3];
    for (color, n) in &colors {
      let n = f64::from(*n);
      count += n;
      for channel in 0..3 {
        let v = f64::from(color[channel]);
        sum[channel] += v * n;
        squares[channel] += v * v * n;
      }
    }
    let mean = sum.map(|total| total / count.max(1.0));
    let error = (0..3)
      .map(|channel| squares[channel] - sum[channel] * mean[channel])
      .sum::<f64>();
    Self {
      colors,
      error: if error.is_finite() {
        error.max(0.0)
      } else {
        0.0
      },
      mean: mean.map(|v| v.round().clamp(0.0, 255.0) as u8),
    }
  }
  /// Channel with the largest weighted variance among `colors` (taken out of the box to split).
  fn axis_of(&self, colors: &[([u8; 3], u32)]) -> usize {
    let mut best = (f64::MIN, 0);
    for channel in 0..3 {
      let mean = f64::from(self.mean[channel]);
      let spread: f64 = colors
        .iter()
        .map(|(color, n)| (f64::from(color[channel]) - mean).powi(2) * f64::from(*n))
        .sum();
      if spread > best.0 {
        best = (spread, channel);
      }
    }
    best.1
  }
}

/// Weighted median cut (as FFmpeg's `palettegen`): repeatedly split the box with the largest
/// total squared error at the weighted median of its widest channel; each box yields its mean.
fn median_cut(colors: Vec<([u8; 3], u32)>, target: usize) -> Vec<[u8; 3]> {
  let mut boxes = vec![ColorBox::new(colors)];
  while boxes.len() < target {
    let Some((index, _)) = boxes
      .iter()
      .enumerate()
      .filter(|(_, b)| b.colors.len() > 1)
      .max_by(|(_, a), (_, b)| a.error.total_cmp(&b.error))
    else {
      break;
    };
    let mut colors = std::mem::take(&mut boxes[index].colors);
    let axis = boxes[index].axis_of(&colors);
    colors.sort_unstable_by_key(|(color, _)| (color[axis], *color));
    let total: u64 = colors.iter().map(|(_, n)| u64::from(*n)).sum();
    let mut running = 0u64;
    let mut split = colors.len() / 2;
    for (position, (_, n)) in colors.iter().enumerate() {
      running += u64::from(*n);
      if running * 2 >= total {
        split = position + 1;
        break;
      }
    }
    let split = split.clamp(1, colors.len() - 1);
    let upper = colors.split_off(split);
    boxes[index] = ColorBox::new(colors);
    boxes.push(ColorBox::new(upper));
  }
  boxes.into_iter().map(|b| b.mean).collect()
}

/// A cache entry not yet computed. Never a color index: a palette has at most 255 colors.
const UNKNOWN: u8 = TRANSPARENT;

/// Global palette with a lazily filled exact nearest-color cache, shared by dithering threads.
pub struct Palette {
  colors: Vec<[u8; 3]>,
  /// Colors ordered by red, then index, for the pruned exact search.
  by_red: Vec<([u8; 3], u8)>,
  /// Exact nearest index per 24-bit color, or [`UNKNOWN`] (16 MiB). Racing threads only store
  /// the same deterministic answer, so relaxed ordering suffices.
  cache: Vec<AtomicU8>,
}
impl Palette {
  pub fn new(colors: Vec<[u8; 3]>) -> Result<Self> {
    if colors.is_empty() || colors.len() > COLORS {
      return Err(EngineError::protocol("A GIF palette needs 1 to 255 colors"));
    }
    let mut by_red: Vec<([u8; 3], u8)> = colors
      .iter()
      .enumerate()
      .map(|(index, color)| (*color, index as u8))
      .collect();
    by_red.sort_unstable_by_key(|(color, index)| (color[0], *index));
    Ok(Self {
      colors,
      by_red,
      cache: std::iter::repeat_with(|| AtomicU8::new(UNKNOWN))
        .take(1 << 24)
        .collect(),
    })
  }
  pub fn len(&self) -> usize {
    self.colors.len()
  }
  pub fn is_empty(&self) -> bool {
    self.colors.is_empty()
  }
  pub fn color(&self, index: u8) -> [u8; 3] {
    self.colors[usize::from(index)]
  }
  /// 256 RGB entries: the colors, padding, and the transparent slot last.
  pub fn table(&self) -> Vec<u8> {
    let mut table: Vec<u8> = self.colors.iter().flatten().copied().collect();
    table.resize(256 * 3, 0);
    table
  }
  /// Exact nearest palette color (squared RGB distance; the lowest index wins ties).
  pub fn nearest(&self, r: u8, g: u8, b: u8) -> u8 {
    let key = (usize::from(r) << 16) | (usize::from(g) << 8) | usize::from(b);
    let cached = self.cache[key].load(Ordering::Relaxed);
    if cached != UNKNOWN {
      return cached;
    }
    let index = self.search(r, g, b);
    self.cache[key].store(index, Ordering::Relaxed);
    index
  }
  /// The exact answer, visiting colors outward from `r` in red order. A side stops once its
  /// red distance alone exceeds the best distance so far: every later color there is farther,
  /// and the best only shrinks, so a skipped color can neither win nor tie.
  fn search(&self, r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    // (distance, index): the lexicographic minimum keeps the lowest index on ties.
    let mut best = (i32::MAX, u8::MAX);
    let start = self
      .by_red
      .partition_point(|(color, _)| i32::from(color[0]) < r);
    let mut visit = |(color, index): &([u8; 3], u8)| {
      let dr = r - i32::from(color[0]);
      if dr * dr > best.0 {
        return false;
      }
      let dg = g - i32::from(color[1]);
      let db = b - i32::from(color[2]);
      // Plain RGB distance, as the palette's own median cut minimises.
      let candidate = (dr * dr + dg * dg + db * db, *index);
      if candidate < best {
        best = candidate;
      }
      true
    };
    for entry in &self.by_red[start..] {
      if !visit(entry) {
        break;
      }
    }
    for entry in self.by_red[..start].iter().rev() {
      if !visit(entry) {
        break;
      }
    }
    best.1
  }
}

const BAYER: [[i32; 8]; 8] = [
  [0, 32, 8, 40, 2, 34, 10, 42],
  [48, 16, 56, 24, 50, 18, 58, 26],
  [12, 44, 4, 36, 14, 46, 6, 38],
  [60, 28, 52, 20, 62, 30, 54, 22],
  [3, 35, 11, 43, 1, 33, 9, 41],
  [51, 19, 59, 27, 49, 17, 57, 25],
  [15, 47, 7, 39, 13, 45, 5, 37],
  [63, 31, 55, 23, 61, 29, 53, 21],
];

/// Map packed RGB to palette indices with the requested dither.
///
/// `previous` is the prior frame's RGB and indices. With error diffusion, a pixel whose source
/// color is unchanged keeps its previous index and neither absorbs nor spreads error, so a
/// small change does not ripple new dither noise across a static picture (flicker and size).
pub fn quantize(
  rgb: &[u8],
  width: usize,
  height: usize,
  palette: &Palette,
  dither: Dither,
  previous: Option<(&[u8], &[u8])>,
  indices: &mut Vec<u8>,
) -> Result<()> {
  if rgb.len() != width * height * 3
    || previous
      .is_some_and(|(prior, chosen)| prior.len() != rgb.len() || chosen.len() != width * height)
  {
    return Err(EngineError::protocol(
      "RGB picture does not match its extent",
    ));
  }
  indices.clear();
  indices.resize(width * height, 0);
  let mut ditherer = Ditherer::new(dither, width)?;
  for (y, out) in indices.chunks_exact_mut(width).enumerate() {
    let row = y * width * 3..(y + 1) * width * 3;
    let prior =
      previous.map(|(prior, chosen)| (&prior[row.clone()], &chosen[y * width..(y + 1) * width]));
    ditherer.row(y, &rgb[row], prior, palette, out);
  }
  Ok(())
}

/// One frame's dithering, advanced a row at a time so consecutive frames can overlap: a row
/// depends only on earlier rows of its own frame and the same row of the previous frame.
pub struct Ditherer {
  dither: Dither,
  /// (dx, dy, weight) of the error-diffusion kernel; empty for ordered or no dither.
  kernel: &'static [(isize, usize, i32)],
  divisor: i32,
  /// Carried error for the current and next rows, per channel, with one guard pixel each side.
  current: Vec<i32>,
  next: Vec<i32>,
}
impl Ditherer {
  pub fn new(dither: Dither, width: usize) -> Result<Self> {
    let kernel: &'static [(isize, usize, i32)] = match dither {
      Dither::None => &[],
      Dither::Bayer { scale } => {
        if scale > 5 {
          return Err(EngineError::new(
            ErrorKind::Capability,
            "gif dither",
            format!("Bayer scale {scale} is outside 0..=5"),
          ));
        }
        &[]
      },
      // (dx, dy, weight) over 16 and over 4 respectively.
      Dither::FloydSteinberg => &[(1, 0, 7), (-1, 1, 3), (0, 1, 5), (1, 1, 1)],
      Dither::Sierra24a => &[(1, 0, 2), (-1, 1, 1), (0, 1, 1)],
    };
    let row = (width + 2) * 3;
    Ok(Self {
      dither,
      kernel,
      divisor: kernel.iter().map(|k| k.2).sum(),
      current: vec![0; row],
      next: vec![0; row],
    })
  }
  /// Start a new frame of the same width.
  pub fn reset(&mut self) {
    self.current.fill(0);
    self.next.fill(0);
  }
  /// Quantize row `y` (`rgb` is that row, `out` its indices). `previous` is the same row of the
  /// prior frame's RGB and indices: under error diffusion, a pixel whose source color is
  /// unchanged keeps its previous index and neither absorbs nor spreads error.
  pub fn row(
    &mut self,
    y: usize,
    rgb: &[u8],
    previous: Option<(&[u8], &[u8])>,
    palette: &Palette,
    out: &mut [u8],
  ) {
    match self.dither {
      Dither::None => {
        for (index, pixel) in out.iter_mut().zip(rgb.as_chunks::<3>().0) {
          *index = palette.nearest(pixel[0], pixel[1], pixel[2]);
        }
        return;
      },
      Dither::Bayer { scale } => {
        for (x, (index, pixel)) in out.iter_mut().zip(rgb.as_chunks::<3>().0).enumerate() {
          // FFmpeg paletteuse strength: scale 0 spans -32..31, each step halves it.
          let offset = (BAYER[y & 7][x & 7] >> scale) - (1 << (5 - scale));
          let shift = |v: u8| (i32::from(v) + offset).clamp(0, 255) as u8;
          *index = palette.nearest(shift(pixel[0]), shift(pixel[1]), shift(pixel[2]));
        }
        return;
      },
      Dither::FloydSteinberg | Dither::Sierra24a => {},
    }
    let (current, next) = (&mut self.current, &mut self.next);
    for (x, index) in out.iter_mut().enumerate() {
      let at = x * 3;
      let carried = (x + 1) * 3;
      if let Some((prior, chosen)) = previous
        && prior[at..at + 3] == rgb[at..at + 3]
      {
        *index = chosen[x];
        continue;
      }
      let mut wanted = [0u8; 3];
      for channel in 0..3 {
        wanted[channel] = (i32::from(rgb[at + channel]) + current[carried + channel] / self.divisor)
          .clamp(0, 255) as u8;
      }
      *index = palette.nearest(wanted[0], wanted[1], wanted[2]);
      let got = palette.color(*index);
      for channel in 0..3 {
        let residual = i32::from(wanted[channel]) - i32::from(got[channel]);
        for &(dx, dy, weight) in self.kernel {
          let target = ((x as isize + 1 + dx) as usize) * 3 + channel;
          if dy == 0 {
            current[target] += residual * weight;
          } else {
            next[target] += residual * weight;
          }
        }
      }
    }
    current.copy_from_slice(next);
    next.fill(0);
  }
}

/// One frame to write: its changed rectangle of the canvas, as palette indices (unchanged pixels
/// transparent) until [`Rectangle::compress`] replaces them with LZW data.
pub struct Rectangle {
  left: u16,
  top: u16,
  width: u16,
  height: u16,
  buffer: Vec<u8>,
  start_cs: u64,
  compressed: bool,
}
impl Rectangle {
  /// LZW-compress the indices; independent of every other frame, so any thread can do it.
  pub fn compress(&mut self) {
    if self.compressed {
      return;
    }
    let mut frame = gif::Frame {
      width: self.width,
      height: self.height,
      buffer: Cow::Owned(std::mem::take(&mut self.buffer)),
      ..gif::Frame::default()
    };
    frame.make_lzw_pre_encoded();
    self.buffer = frame.buffer.into_owned();
    self.compressed = true;
  }
}

/// Decides, in presentation order, what each quantized frame contributes: nothing (coalesced
/// or unchanged) or the rectangle that changed since the last written frame.
pub struct GifPlanner {
  width: usize,
  height: usize,
  /// The picture as written so far: the indices of the last written frame.
  canvas: Option<Vec<u8>>,
  /// Start of the last written frame, in whole centiseconds from the recording's start.
  last_start_cs: Option<u64>,
  report: GifReport,
}
impl GifPlanner {
  pub fn new(width: usize, height: usize, palette: &Palette) -> Result<Self> {
    let (w, h) = (
      u16::try_from(width).map_err(|_| too_large())?,
      u16::try_from(height).map_err(|_| too_large())?,
    );
    if w == 0 || h == 0 {
      return Err(EngineError::protocol("A GIF needs a non-empty picture"));
    }
    Ok(Self {
      width,
      height,
      canvas: None,
      last_start_cs: None,
      report: GifReport {
        width: u32::from(w),
        height: u32::from(h),
        palette_colors: palette.len() as u32,
        ..Default::default()
      },
    })
  }
  /// Picture width and height in pixels.
  pub fn size(&self) -> (usize, usize) {
    (self.width, self.height)
  }
  /// Plan one quantized frame presented at `pts_100ns` from the recording's start.
  pub fn plan(&mut self, indices: &[u8], pts_100ns: u64) -> Result<Option<Rectangle>> {
    if indices.len() != self.width * self.height {
      return Err(EngineError::protocol(
        "GIF frame does not match the picture",
      ));
    }
    self.report.decoded_frames += 1;
    let start_cs = pts_100ns / 100_000;
    if let Some(last) = self.last_start_cs {
      if start_cs < last {
        return Err(EngineError::protocol(
          "GIF frames must be pushed in time order",
        ));
      }
      if start_cs < last + MIN_DELAY_CS {
        self.report.coalesced_frames += 1;
        return Ok(None);
      }
    }
    let Some(canvas) = self.canvas.as_mut() else {
      self.canvas = Some(indices.to_vec());
      self.last_start_cs = Some(start_cs);
      return Ok(Some(Rectangle {
        left: 0,
        top: 0,
        width: self.width as u16,
        height: self.height as u16,
        buffer: indices.to_vec(),
        start_cs,
        compressed: false,
      }));
    };
    let Some((left, top, right, bottom)) = changed(canvas, indices, self.width) else {
      self.report.unchanged_frames += 1;
      return Ok(None);
    };
    let mut buffer = Vec::with_capacity((right - left) * (bottom - top));
    for y in top..bottom {
      let row = y * self.width;
      for x in left..right {
        let value = indices[row + x];
        buffer.push(if canvas[row + x] == value {
          TRANSPARENT
        } else {
          value
        });
      }
      canvas[row + left..row + right].copy_from_slice(&indices[row + left..row + right]);
    }
    self.last_start_cs = Some(start_cs);
    Ok(Some(Rectangle {
      left: left as u16,
      top: top as u16,
      width: (right - left) as u16,
      height: (bottom - top) as u16,
      buffer,
      start_cs,
      compressed: false,
    }))
  }
}

/// Writes planned rectangles in order, timed in whole centiseconds from their presentation
/// times so rounding never accumulates. A frame's delay is known once the next one arrives.
pub struct GifEmitter<W: Write> {
  encoder: gif::Encoder<W>,
  pending: Option<Rectangle>,
  report: GifReport,
}
impl<W: Write> GifEmitter<W> {
  pub fn new(output: W, planner: &GifPlanner, palette: &Palette) -> Result<Self> {
    let (w, h) = (planner.report.width as u16, planner.report.height as u16);
    let mut encoder = gif::Encoder::new(output, w, h, &palette.table()).map_err(gif_error)?;
    encoder
      .set_repeat(gif::Repeat::Infinite)
      .map_err(gif_error)?;
    Ok(Self {
      encoder,
      pending: None,
      report: GifReport::default(),
    })
  }
  /// Queue the next planned rectangle, writing the previous one now that its delay is known.
  pub fn push(&mut self, rectangle: Rectangle) -> Result<()> {
    self.flush(rectangle.start_cs)?;
    self.pending = Some(rectangle);
    Ok(())
  }
  fn flush(&mut self, end_cs: u64) -> Result<()> {
    let Some(mut pending) = self.pending.take() else {
      return Ok(());
    };
    pending.compress();
    let delay = u16::try_from(end_cs.saturating_sub(pending.start_cs).max(1)).unwrap_or(u16::MAX);
    let frame = gif::Frame {
      delay,
      dispose: gif::DisposalMethod::Keep,
      transparent: (self.report.written_frames > 0).then_some(TRANSPARENT),
      left: pending.left,
      top: pending.top,
      width: pending.width,
      height: pending.height,
      buffer: Cow::Owned(pending.buffer),
      ..gif::Frame::default()
    };
    self
      .encoder
      .write_lzw_pre_encoded_frame(&frame)
      .map_err(gif_error)?;
    self.report.written_frames += 1;
    self.report.duration_cs = pending.start_cs + u64::from(delay);
    Ok(())
  }
  /// Close the file; the last frame lasts until `end_100ns` (the recording's end). The report
  /// combines the planner's frame counts with what was written.
  pub fn finish(mut self, end_100ns: u64, planner: GifPlanner) -> Result<(W, GifReport)> {
    if self.pending.is_none() {
      return Err(EngineError::protocol("A GIF needs at least one frame"));
    }
    self.flush(end_100ns.div_ceil(100_000))?;
    let output = self.encoder.into_inner().map_err(gif_error)?;
    Ok((
      output,
      GifReport {
        written_frames: self.report.written_frames,
        duration_cs: self.report.duration_cs,
        ..planner.report
      },
    ))
  }
}

/// Plans, compresses and writes frames on one thread: the reference the parallel encoder must
/// match byte for byte.
pub struct GifWriter<W: Write> {
  planner: GifPlanner,
  emitter: GifEmitter<W>,
}
impl<W: Write> GifWriter<W> {
  pub fn new(output: W, width: usize, height: usize, palette: &Palette) -> Result<Self> {
    let planner = GifPlanner::new(width, height, palette)?;
    let emitter = GifEmitter::new(output, &planner, palette)?;
    Ok(Self { planner, emitter })
  }
  /// Add one quantized frame presented at `pts_100ns` from the recording's start.
  pub fn push(&mut self, indices: &[u8], pts_100ns: u64) -> Result<()> {
    if let Some(rectangle) = self.planner.plan(indices, pts_100ns)? {
      self.emitter.push(rectangle)?;
    }
    Ok(())
  }
  /// Close the file; the last frame lasts until `end_100ns` (the recording's end).
  pub fn finish(self, end_100ns: u64) -> Result<(W, GifReport)> {
    self.emitter.finish(end_100ns, self.planner)
  }
}

/// Bounding rectangle `(left, top, right, bottom)` of differing pixels, exclusive ends.
fn changed(previous: &[u8], current: &[u8], width: usize) -> Option<(usize, usize, usize, usize)> {
  let mut bounds: Option<(usize, usize, usize, usize)> = None;
  let rows = previous
    .chunks_exact(width)
    .zip(current.chunks_exact(width));
  for (y, (a, b)) in rows.enumerate() {
    let Some(first) = a.iter().zip(b).position(|(p, c)| p != c) else {
      continue;
    };
    let last = a
      .iter()
      .rev()
      .zip(b.iter().rev())
      .position(|(p, c)| p != c)
      .map_or(width, |from_end| width - from_end);
    bounds = Some(match bounds {
      None => (first, y, last, y + 1),
      Some((left, top, right, _)) => (left.min(first), top, right.max(last), y + 1),
    });
  }
  bounds
}

fn too_large() -> EngineError {
  EngineError::new(
    ErrorKind::Capability,
    "gif",
    "GIF pictures are limited to 65535 pixels per side",
  )
}
fn gif_error(error: gif::EncodingError) -> EngineError {
  EngineError::new(ErrorKind::Io, "gif write", error.to_string())
}

#[cfg(test)]
#[path = "gif/tests.rs"]
mod tests;
