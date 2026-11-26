//! Blur regions for screenshots, built from the frozen capture so preview and export match.
//!
//! The region is reduced to coarse cell averages with per-cell noise, then
//! smoothed. Fine detail is gone from the output, unlike a light Gaussian blur or a
//! plain mosaic, which can sometimes be matched back to the original text. The
//! soft edge lies entirely outside the area the user drew, which stays fully covered.
//!
//! Its own crate so debug builds still compile it optimized: a drag repaints a region every
//! frame, which an unoptimized build cannot keep up with.

const MIN_CELL: i32 = 16;
const MAX_CELL: i32 = 64;
const CELLS_ACROSS: i32 = 4;
const NOISE: i32 = 6;
const FEATHER: f32 = 14.0;
const CORNER_RADIUS: f32 = 10.0;
/// Grows the rounded shape until its corners clear the drawn rectangle's corners.
const CORNER_CLEARANCE: f32 = 3.0;
/// How far a region's pixels reach beyond the drawn rectangle.
pub const SOFT_EDGE: i32 = 18;

/// One region: two opposite corners of the drawn rectangle, and a seed that keeps its noise
/// stable between repaints and different between regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
  pub from: (i32, i32),
  pub to: (i32, i32),
  pub seed: u64,
}

/// Image-space pixels for one region, in RGBA order; alpha is the soft edge.
#[derive(Debug, PartialEq, Eq)]
pub struct Patch {
  pub left: i32,
  pub top: i32,
  pub width: i32,
  pub height: i32,
  pub rgba: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Drawn {
  left: f32,
  top: f32,
  right: f32,
  bottom: f32,
}

impl Drawn {
  fn coverage(self, x: f32, y: f32) -> f32 {
    let distance = self.distance(x, y);
    if distance <= 0.0 {
      return 1.0;
    }
    let amount = (1.0 - distance / FEATHER).clamp(0.0, 1.0);
    amount * amount * (3.0 - 2.0 * amount)
  }

  /// Distance outside the rounded shape; zero or less is fully covered.
  fn distance(self, x: f32, y: f32) -> f32 {
    let (half_width, half_height) = (
      (self.right - self.left) / 2.0 + CORNER_CLEARANCE,
      (self.bottom - self.top) / 2.0 + CORNER_CLEARANCE,
    );
    let across = (x - (self.left + self.right) / 2.0).abs() - (half_width - CORNER_RADIUS);
    let down = (y - (self.top + self.bottom) / 2.0).abs() - (half_height - CORNER_RADIUS);
    across.max(0.0).hypot(down.max(0.0)) + across.max(down).min(0.0) - CORNER_RADIUS
  }
}

pub fn patch(
  source: &[u8],
  source_width: u32,
  source_height: u32,
  region: Region,
) -> Option<Patch> {
  if source.len() as u64 != u64::from(source_width) * u64::from(source_height) * 4 {
    return None;
  }
  let (a, b) = (region.from, region.to);
  let (image_width, image_height) = (source_width as i32, source_height as i32);
  let (drawn_left, drawn_top) = (a.0.min(b.0), a.1.min(b.1));
  let (drawn_right, drawn_bottom) = (a.0.max(b.0), a.1.max(b.1));
  let left = (drawn_left - SOFT_EDGE).max(0);
  let top = (drawn_top - SOFT_EDGE).max(0);
  let width = (drawn_right + SOFT_EDGE).min(image_width) - left;
  let height = (drawn_bottom + SOFT_EDGE).min(image_height) - top;
  if width <= 0
    || height <= 0
    || drawn_left >= image_width
    || drawn_top >= image_height
    || drawn_right <= 0
    || drawn_bottom <= 0
  {
    return None;
  }
  let cell = ((drawn_right - drawn_left).min(drawn_bottom - drawn_top) / CELLS_ACROSS)
    .clamp(MIN_CELL, MAX_CELL);
  let (columns, rows) = ((width + cell - 1) / cell, (height + cell - 1) / cell);
  let cells = averages(
    source,
    image_width,
    (left, top, width, height),
    cell,
    (columns, rows),
    region.seed,
  );
  let drawn = Drawn {
    left: drawn_left as f32,
    top: drawn_top as f32,
    right: drawn_right as f32,
    bottom: drawn_bottom as f32,
  };
  // Dragging repaints this every frame, so the loops stay plain: smooth across at cell-row
  // resolution, then blend three of those rows per output row.
  let line = width as usize;
  let across: Vec<_> = (0..width).map(|x| spline(x, cell, columns)).collect();
  let mut smoothed = vec![[0f32; 3]; rows as usize * line];
  for (row, out) in smoothed.chunks_exact_mut(line).enumerate() {
    let cells = &cells[row * columns as usize..][..columns as usize];
    for (pixel, (indices, weights)) in out.iter_mut().zip(&across) {
      for channel in 0..3 {
        pixel[channel] = weights[0] * cells[indices[0]][channel]
          + weights[1] * cells[indices[1]][channel]
          + weights[2] * cells[indices[2]][channel];
      }
    }
  }
  // Pixels inside the drawn rectangle are always fully covered; only the soft edge needs
  // the rounded-shape distance.
  let covered = (drawn_left - left).max(0) as usize..(drawn_right - left).clamp(0, width) as usize;
  let mut rgba = vec![0u8; line * height as usize * 4];
  for (y, out) in rgba.chunks_exact_mut(line * 4).enumerate() {
    let image_y = top + y as i32;
    let (indices, weights) = spline(y as i32, cell, rows);
    let [a, b, c] = indices.map(|row| &smoothed[row * line..][..line]);
    let inside_row = (drawn_top..drawn_bottom).contains(&image_y);
    for (x, pixel) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
      for channel in 0..3 {
        let value =
          weights[0] * a[x][channel] + weights[1] * b[x][channel] + weights[2] * c[x][channel];
        pixel[channel] = value.round().clamp(0.0, 255.0) as u8;
      }
      pixel[3] = if inside_row && covered.contains(&x) {
        255
      } else {
        let coverage = drawn.coverage((left + x as i32) as f32 + 0.5, image_y as f32 + 0.5);
        (coverage * 255.0).round() as u8
      };
    }
  }
  Some(Patch {
    left,
    top,
    width,
    height,
    rgba,
  })
}

/// Quadratic B-spline taps: the three cells around a pixel and their weights.
fn spline(offset: i32, cell: i32, count: i32) -> ([usize; 3], [f32; 3]) {
  let position = (offset as f32 + 0.5) / cell as f32;
  let middle = (position as i32).min(count - 1);
  let amount = (position - middle as f32).clamp(0.0, 1.0);
  let tap = |index: i32| index.clamp(0, count - 1) as usize;
  (
    [tap(middle - 1), tap(middle), tap(middle + 1)],
    [
      0.5 * (1.0 - amount) * (1.0 - amount),
      0.5 + amount * (1.0 - amount),
      0.5 * amount * amount,
    ],
  )
}

fn averages(
  source: &[u8],
  image_width: i32,
  region: (i32, i32, i32, i32),
  cell: i32,
  grid: (i32, i32),
  seed: u64,
) -> Vec<[f32; 3]> {
  let (left, top, width, height) = region;
  let (columns, rows) = grid;
  let mut cells = Vec::with_capacity((columns * rows) as usize);
  for row in 0..rows {
    for column in 0..columns {
      let (x0, y0) = (column * cell, row * cell);
      let (x1, y1) = ((x0 + cell).min(width), (y0 + cell).min(height));
      let mut sum = [0u64; 3];
      for y in y0..y1 {
        let start = ((top + y) * image_width + left + x0) as usize * 4;
        let end = start + (x1 - x0) as usize * 4;
        for pixel in source[start..end].as_chunks::<4>().0 {
          for channel in 0..3 {
            sum[channel] += u64::from(pixel[channel]);
          }
        }
      }
      let count = ((x1 - x0) * (y1 - y0)).max(1) as f32;
      let noise = noise(seed, (row * columns + column) as u64) as f32;
      cells.push(std::array::from_fn(|channel| {
        (sum[channel] as f32 / count + noise).clamp(0.0, 255.0)
      }));
    }
  }
  cells
}

/// Stable per drawing, so a region does not shimmer between repaints.
fn noise(seed: u64, index: u64) -> i32 {
  let mut value =
    seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ index.wrapping_mul(0xBF58_476D_1CE4_E5B9);
  value ^= value >> 31;
  value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
  value ^= value >> 29;
  (value % (NOISE as u64 * 2 + 1)) as i32 - NOISE
}

/// Blends a patch over RGBA pixels whose top left sits at `origin`. Regions blend
/// in drawing order, so one region's soft edge cannot uncover another region.
pub fn blend(target: &mut [u8], origin: (i32, i32), target_width: i32, patch: &Patch) {
  let target_height = target.len() as i32 / 4 / target_width.max(1);
  let left = patch.left.max(origin.0);
  let right = (patch.left + patch.width).min(origin.0 + target_width);
  for y in patch.top.max(origin.1)..(patch.top + patch.height).min(origin.1 + target_height) {
    for x in left..right {
      let from = ((y - patch.top) * patch.width + x - patch.left) as usize * 4;
      let to = ((y - origin.1) * target_width + x - origin.0) as usize * 4;
      let coverage = u32::from(patch.rgba[from + 3]);
      // Exact shortcuts of the blend below; most of a region is fully covered.
      if coverage == 0 {
        continue;
      }
      if coverage == 255 {
        target[to..to + 3].copy_from_slice(&patch.rgba[from..from + 3]);
        continue;
      }
      for channel in 0..3 {
        let (over, under) = (
          u32::from(patch.rgba[from + channel]),
          u32::from(target[to + channel]),
        );
        target[to + channel] = ((over * coverage + under * (255 - coverage) + 127) / 255) as u8;
      }
    }
  }
}

/// The frozen capture with regions blended in, for one rectangle of the preview.
pub struct Layer {
  pub left: i32,
  pub top: i32,
  pub width: i32,
  pub height: i32,
  pub rgba: Vec<u8>,
}

impl Layer {
  /// Converts to the opaque BGRA a GDI device expects, in place: a drag repaints the whole layer
  /// every frame.
  pub fn into_opaque_bgra(&mut self) {
    for pixel in self.rgba.as_chunks_mut::<4>().0 {
      pixel.swap(0, 2);
      pixel[3] = 255;
    }
  }
}

/// Builds the smallest layer holding every region inside `clip`.
pub fn layer(
  source: &[u8],
  source_width: u32,
  source_height: u32,
  regions: impl Iterator<Item = Region>,
  clip: (i32, i32, i32, i32),
) -> Option<Layer> {
  let patches: Vec<Patch> = regions
    .filter_map(|region| patch(source, source_width, source_height, region))
    .collect();
  let left = patches.iter().map(|patch| patch.left).min()?.max(clip.0);
  let top = patches.iter().map(|patch| patch.top).min()?.max(clip.1);
  let right = patches
    .iter()
    .map(|patch| patch.left + patch.width)
    .max()?
    .min(clip.2)
    .min(source_width as i32);
  let bottom = patches
    .iter()
    .map(|patch| patch.top + patch.height)
    .max()?
    .min(clip.3)
    .min(source_height as i32);
  if right <= left || bottom <= top {
    return None;
  }
  let (width, height) = (right - left, bottom - top);
  let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
  for y in top..bottom {
    let start = (y as usize * source_width as usize + left as usize) * 4;
    rgba.extend_from_slice(&source[start..start + width as usize * 4]);
  }
  for patch in &patches {
    blend(&mut rgba, (left, top), width, patch);
  }
  Some(Layer {
    left,
    top,
    width,
    height,
    rgba,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn drawing(points: &[(i32, i32)]) -> Region {
    Region {
      from: points[0],
      to: points[points.len() - 1],
      seed: 7,
    }
  }

  #[test]
  fn spline_weights_stay_balanced_at_every_edge() {
    for offset in [0, 1, 7, 15, 16, 47, 63] {
      let (indices, weights) = spline(offset, 16, 4);
      assert!((weights.iter().sum::<f32>() - 1.0).abs() < 0.001);
      assert!(indices.iter().all(|index| *index < 4));
    }
  }

  /// The straightforward per-pixel form `patch` must match byte for byte.
  fn reference_patch(source: &[u8], width: u32, height: u32, region: Region) -> Patch {
    let (a, b) = (region.from, region.to);
    let (image_width, image_height) = (width as i32, height as i32);
    let (drawn_left, drawn_top) = (a.0.min(b.0), a.1.min(b.1));
    let (drawn_right, drawn_bottom) = (a.0.max(b.0), a.1.max(b.1));
    let left = (drawn_left - SOFT_EDGE).max(0);
    let top = (drawn_top - SOFT_EDGE).max(0);
    let width = (drawn_right + SOFT_EDGE).min(image_width) - left;
    let height = (drawn_bottom + SOFT_EDGE).min(image_height) - top;
    let cell = ((drawn_right - drawn_left).min(drawn_bottom - drawn_top) / CELLS_ACROSS)
      .clamp(MIN_CELL, MAX_CELL);
    let (columns, rows) = ((width + cell - 1) / cell, (height + cell - 1) / cell);
    let cells = averages(
      source,
      image_width,
      (left, top, width, height),
      cell,
      (columns, rows),
      region.seed,
    );
    let drawn = Drawn {
      left: drawn_left as f32,
      top: drawn_top as f32,
      right: drawn_right as f32,
      bottom: drawn_bottom as f32,
    };
    let across: Vec<_> = (0..width).map(|x| spline(x, cell, columns)).collect();
    let mut smoothed = vec![[0f32; 3]; rows as usize * width as usize];
    for row in 0..rows as usize {
      for (x, (indices, weights)) in across.iter().enumerate() {
        smoothed[row * width as usize + x] = std::array::from_fn(|channel| {
          (0..3)
            .map(|tap| weights[tap] * cells[row * columns as usize + indices[tap]][channel])
            .sum()
        });
      }
    }
    let mut rgba = Vec::new();
    for y in 0..height {
      let (indices, weights) = spline(y, cell, rows);
      for x in 0..width as usize {
        let pixel: [f32; 3] = std::array::from_fn(|channel| {
          (0..3)
            .map(|tap| weights[tap] * smoothed[indices[tap] * width as usize + x][channel])
            .sum()
        });
        rgba.extend(pixel.map(|value| value.round().clamp(0.0, 255.0) as u8));
        let coverage = drawn.coverage((left + x as i32) as f32 + 0.5, (top + y) as f32 + 0.5);
        rgba.push((coverage * 255.0).round() as u8);
      }
    }
    Patch {
      left,
      top,
      width,
      height,
      rgba,
    }
  }

  fn photo(width: u32, height: u32) -> Vec<u8> {
    (0..width * height)
      .flat_map(|index| {
        let (x, y) = (index % width, index / width);
        [
          (x * 7 + y) as u8,
          ((y * 5) ^ x) as u8,
          (x * y / 3) as u8,
          255,
        ]
      })
      .collect()
  }

  #[test]
  fn fast_patch_matches_the_per_pixel_reference_exactly() {
    let source = photo(300, 200);
    for points in [
      vec![(40, 30), (260, 170)],
      vec![(0, 0), (300, 200)],
      vec![(-20, -10), (50, 40)],
      vec![(280, 190), (310, 230)],
      vec![(100, 100), (103, 102)],
      vec![(150, 20), (90, 120)],
    ] {
      let region = drawing(&points);
      assert_eq!(
        patch(&source, 300, 200, region).expect("patch"),
        reference_patch(&source, 300, 200, region),
        "{points:?}"
      );
    }
  }

  /// Times one preview frame of a large region on a 1440p capture, as a drag repaints it.
  /// `cargo test -p flinch-blur drag_frame_cost_probe -- --ignored --nocapture`
  #[test]
  #[ignore = "timing probe; run explicitly"]
  fn drag_frame_cost_probe() {
    let (width, height) = (2560, 1440);
    let source = photo(width, height);
    let frames = 10;
    let region = |frame: i32| drawing(&[(300 + frame, 200 + frame), (1500 + frame, 1000 + frame)]);
    let started = std::time::Instant::now();
    for frame in 0..frames {
      reference_patch(&source, width, height, region(frame));
    }
    let reference = started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
    let started = std::time::Instant::now();
    for frame in 0..frames {
      let layer = layer(
        &source,
        width,
        height,
        std::iter::once(region(frame)),
        (0, 0, width as i32, height as i32),
      );
      assert!(layer.is_some());
    }
    let current = started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
    println!(
      "blur drag frame ({}): layer {current:.2} ms, reference patch alone {reference:.2} ms",
      if cfg!(debug_assertions) {
        "debug"
      } else {
        "release"
      }
    );
  }
}
