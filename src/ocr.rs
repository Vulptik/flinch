//! Offline text recognition with the bundled PP-OCRv6 small models on RTen.
//!
//! Images never leave this process. Models are parsed per job and dropped with
//! it, so the idle tray app does not keep them resident.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, anyhow};
use image::{RgbImage, imageops};
use rten::Model;
use rten_tensor::Tensor;
use rten_tensor::prelude::*;

static DETECT_MODEL: &[u8] = include_bytes!("../resources/ocr/detect.onnx");
static RECOGNIZE_MODEL: &[u8] = include_bytes!("../resources/ocr/recognize.onnx");
const KEYS: &str = include_str!("../resources/ocr/keys.txt");

const MAX_IMAGE_PIXELS: u64 = 64_000_000;
const MAX_DETECT_SIDE: f32 = 2048.0;
const MIN_DETECT_SIDE: f32 = 64.0;
const DETECT_STRIDE: f32 = 32.0;
const DETECT_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const DETECT_STD: [f32; 3] = [0.229, 0.224, 0.225];
const TEXT_THRESHOLD: f32 = 0.3;
const REGION_THRESHOLD: f32 = 0.5;
const MIN_REGION_SIDE: f32 = 3.0;
const REGION_GROWTH: f32 = 1.5;
const LINE_HEIGHT: u32 = 48;
const MIN_LINE_WIDTH: u32 = 320;
const MAX_LINE_WIDTH: u32 = 3200;
const MIN_LONE_CONFIDENCE: f32 = 0.9;
const MIN_TILT_DEGREES: f32 = 2.0;
const MAX_TILT_DEGREES: f32 = 60.0;
const MIN_TILT_ASPECT: f32 = 1.5;
const MAX_ROW_TILT_DEGREES: f32 = 20.0;
const ROW_OVERLAP: f32 = 0.5;

#[derive(Debug, thiserror::Error)]
pub(crate) enum OcrError {
  #[error("Text recognition was cancelled")]
  Cancelled,
  #[error("Text recognition exceeded its time limit")]
  TimedOut,
  #[error("The selection is too large for text recognition")]
  TooLarge,
  #[error(transparent)]
  Failed(#[from] anyhow::Error),
}

/// Stops a job between model runs; a single run cannot be interrupted.
#[derive(Clone)]
pub(crate) struct OcrLimits {
  pub(crate) cancel: Arc<AtomicBool>,
  pub(crate) deadline: Instant,
}

impl OcrLimits {
  fn check(&self) -> Result<(), OcrError> {
    if self.cancel.load(Ordering::Acquire) {
      return Err(OcrError::Cancelled);
    }
    if Instant::now() >= self.deadline {
      return Err(OcrError::TimedOut);
    }
    Ok(())
  }
}

type Point = (f32, f32);

const HORIZONTAL: Point = (1.0, 0.0);

/// A text line in image coordinates. `axis` is the unit reading direction.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Region {
  center: Point,
  axis: Point,
  length: f32,
  thickness: f32,
}

impl Region {
  fn normal(&self) -> Point {
    (-self.axis.1, self.axis.0)
  }

  fn degrees(&self) -> f32 {
    self.axis.1.atan2(self.axis.0).to_degrees()
  }
}

#[derive(Debug)]
struct Line {
  region: Region,
  text: String,
}

/// Returns the recognized text in reading order; empty when none was found.
pub(crate) fn recognize(
  rgba: &[u8],
  width: u32,
  height: u32,
  limits: &OcrLimits,
) -> Result<String, OcrError> {
  let image = rgb_image(rgba, width, height)?;
  limits.check()?;
  let regions = {
    let model = Model::load_static_slice(DETECT_MODEL).context("Load text detection model")?;
    detect(&model, &image)?
  };
  limits.check()?;
  if regions.is_empty() {
    return Ok(String::new());
  }
  let model = Model::load_static_slice(RECOGNIZE_MODEL).context("Load text recognition model")?;
  let keys: Vec<&str> = KEYS.lines().collect();
  let mut lines = Vec::with_capacity(regions.len());
  for region in regions {
    limits.check()?;
    let (text, confidence) = read_line(&model, &image, region, &keys)?;
    if let Some(text) = clean(&text, confidence) {
      lines.push(Line { region, text });
    }
  }
  Ok(reading_order(lines))
}

/// A detected text line's upright bounds in image pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextBox {
  pub(crate) left: i32,
  pub(crate) top: i32,
  pub(crate) right: i32,
  pub(crate) bottom: i32,
}

/// Finds every text line without reading it, for Redact All Text. Recognition is skipped, so
/// lines the reader would drop as noise (icons, low confidence) are still covered: redaction
/// errs toward hiding. A tilted line gets the upright box around its rotated rectangle.
pub(crate) fn detect_lines(
  rgba: &[u8],
  width: u32,
  height: u32,
  limits: &OcrLimits,
) -> Result<Vec<TextBox>, OcrError> {
  let image = rgb_image(rgba, width, height)?;
  limits.check()?;
  let model = Model::load_static_slice(DETECT_MODEL).context("Load text detection model")?;
  limits.check()?;
  let regions = detect(&model, &image)?;
  limits.check()?;
  Ok(
    regions
      .iter()
      .filter_map(|region| upright(region, width, height))
      .collect(),
  )
}

fn upright(region: &Region, width: u32, height: u32) -> Option<TextBox> {
  let (axis, normal) = (region.axis, region.normal());
  let half_x = (axis.0 * region.length).abs() / 2.0 + (normal.0 * region.thickness).abs() / 2.0;
  let half_y = (axis.1 * region.length).abs() / 2.0 + (normal.1 * region.thickness).abs() / 2.0;
  let (width, height) = (width as f32, height as f32);
  let left = (region.center.0 - half_x).floor().clamp(0.0, width);
  let top = (region.center.1 - half_y).floor().clamp(0.0, height);
  let right = (region.center.0 + half_x).ceil().clamp(0.0, width);
  let bottom = (region.center.1 + half_y).ceil().clamp(0.0, height);
  (right > left && bottom > top).then_some(TextBox {
    left: left as i32,
    top: top as i32,
    right: right as i32,
    bottom: bottom as i32,
  })
}

fn rgb_image(rgba: &[u8], width: u32, height: u32) -> Result<RgbImage, OcrError> {
  let pixels = u64::from(width) * u64::from(height);
  if pixels > MAX_IMAGE_PIXELS {
    return Err(OcrError::TooLarge);
  }
  if pixels == 0 || rgba.len() as u64 != pixels * 4 {
    return Err(anyhow!("Invalid text recognition image {width}x{height}").into());
  }
  let rgb = rgba
    .as_chunks::<4>()
    .0
    .iter()
    .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
    .collect();
  RgbImage::from_raw(width, height, rgb)
    .ok_or_else(|| anyhow!("Invalid text recognition buffer").into())
}

fn run(model: &Model, shape: [usize; 4], data: Vec<f32>) -> anyhow::Result<(Vec<usize>, Vec<f32>)> {
  let input = Tensor::from_data(&shape, data);
  let output = model
    .run_one(input.view().into(), None)
    .context("Run text model")?;
  let tensor: Tensor<f32> = output.try_into().context("Read text model output")?;
  Ok((tensor.shape().to_vec(), tensor.to_vec()))
}

/// Keeps small selections readable and large ones within the detector budget.
fn detection_size(width: u32, height: u32) -> (u32, u32) {
  let (width, height) = (width as f32, height as f32);
  let limit = MAX_DETECT_SIDE / width.max(height);
  let scale = (MIN_DETECT_SIDE / width.min(height)).max(1.0).min(limit);
  let round = |side: f32| ((side * scale / DETECT_STRIDE).round().max(1.0) * DETECT_STRIDE) as u32;
  (round(width), round(height))
}

fn detect(model: &Model, image: &RgbImage) -> anyhow::Result<Vec<Region>> {
  let (width, height) = image.dimensions();
  let (input_width, input_height) = detection_size(width, height);
  let resized = imageops::resize(
    image,
    input_width,
    input_height,
    imageops::FilterType::Triangle,
  );
  let (w, h) = (input_width as usize, input_height as usize);
  let mut data = vec![0f32; 3 * w * h];
  for (x, y, pixel) in resized.enumerate_pixels() {
    // The models were trained on BGR images.
    let bgr = [pixel[2], pixel[1], pixel[0]];
    for channel in 0..3 {
      data[channel * w * h + y as usize * w + x as usize] =
        (f32::from(bgr[channel]) / 255.0 - DETECT_MEAN[channel]) / DETECT_STD[channel];
    }
  }
  let (shape, map) = run(model, [1, 3, h, w], data)?;
  let [.., map_height, map_width] = shape[..] else {
    return Err(anyhow!("Unexpected text detection output {shape:?}"));
  };
  anyhow::ensure!(
    map_width > 0 && map_height > 0 && map.len() >= map_width * map_height,
    "Unexpected text detection output {shape:?}"
  );
  Ok(regions(&map, map_width, map_height, width, height))
}

/// Finds connected text areas in the probability map, in image coordinates.
fn regions(
  map: &[f32],
  map_width: usize,
  map_height: usize,
  width: u32,
  height: u32,
) -> Vec<Region> {
  let mut seen = vec![false; map_width * map_height];
  let mut found = Vec::new();
  let scale = (
    width as f32 / map_width as f32,
    height as f32 / map_height as f32,
  );
  for start in 0..map_width * map_height {
    if seen[start] || map[start] <= TEXT_THRESHOLD {
      continue;
    }
    seen[start] = true;
    let mut stack = vec![start];
    let mut pixels = Vec::new();
    let mut sum = 0f32;
    while let Some(index) = stack.pop() {
      let (x, y) = (index % map_width, index / map_width);
      pixels.push((x, y));
      sum += map[index];
      let mut visit = |next: usize| {
        if !seen[next] && map[next] > TEXT_THRESHOLD {
          seen[next] = true;
          stack.push(next);
        }
      };
      if x > 0 {
        visit(index - 1);
      }
      if x + 1 < map_width {
        visit(index + 1);
      }
      if y > 0 {
        visit(index - map_width);
      }
      if y + 1 < map_height {
        visit(index + map_width);
      }
    }
    if sum / (pixels.len() as f32) < REGION_THRESHOLD {
      continue;
    }
    let hull = convex_hull(outline(&pixels));
    if let Some(region) = fit_region(&hull, scale, (width as f32, height as f32)) {
      found.push(region);
    }
  }
  found
}

/// Reduces a component to the corners of each row's outermost pixels.
fn outline(pixels: &[(usize, usize)]) -> Vec<Point> {
  let top = pixels.iter().map(|pixel| pixel.1).min().unwrap_or(0);
  let bottom = pixels.iter().map(|pixel| pixel.1).max().unwrap_or(0);
  let mut spans = vec![(usize::MAX, 0); bottom - top + 1];
  for (x, y) in pixels {
    let span = &mut spans[y - top];
    *span = (span.0.min(*x), span.1.max(*x));
  }
  let mut points = Vec::with_capacity(spans.len() * 4);
  for (row, (left, right)) in spans.into_iter().enumerate() {
    let y = (top + row) as f32;
    let (left, right) = (left as f32, right as f32 + 1.0);
    points.extend([(left, y), (left, y + 1.0), (right, y), (right, y + 1.0)]);
  }
  points
}

fn convex_hull(mut points: Vec<Point>) -> Vec<Point> {
  points.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
  points.dedup();
  if points.len() < 3 {
    return points;
  }
  let mut hull = Vec::with_capacity(points.len());
  extend_hull(&mut hull, points.iter().copied());
  extend_hull(&mut hull, points.iter().rev().copied());
  hull
}

/// Adds one monotone chain, leaving its last point for the next chain to start.
fn extend_hull(hull: &mut Vec<Point>, points: impl Iterator<Item = Point>) {
  let floor = hull.len() + 2;
  for point in points {
    while hull.len() >= floor {
      let (a, b) = (hull[hull.len() - 2], hull[hull.len() - 1]);
      if (b.0 - a.0) * (point.1 - a.1) - (b.1 - a.1) * (point.0 - a.0) > 0.0 {
        break;
      }
      hull.pop();
    }
    hull.push(point);
  }
  hull.pop();
}

/// Fits the smallest rotated rectangle to a detected shape. Detector scale is
/// applied first so tilt is measured in image space.
fn fit_region(hull: &[Point], scale: Point, limit: Point) -> Option<Region> {
  let points: Vec<Point> = hull
    .iter()
    .map(|point| (point.0 * scale.0, point.1 * scale.1))
    .collect();
  let bounds = extent(&points, HORIZONTAL);
  let rows = extent(&points, (0.0, 1.0));
  let (straight_length, straight_thickness) = (bounds.1 - bounds.0, rows.1 - rows.0);
  let mut region = Region {
    center: ((bounds.0 + bounds.1) / 2.0, (rows.0 + rows.1) / 2.0),
    axis: HORIZONTAL,
    length: straight_length,
    thickness: straight_thickness,
  };
  if let Some(tilted) = tightest_rectangle(&points).filter(is_tilted) {
    region = tilted;
  }
  let min_side = MIN_REGION_SIDE * scale.0.min(scale.1);
  if region.length.min(region.thickness) < min_side {
    return None;
  }
  // The detector reports shrunken text cores; grow them back to the glyphs.
  let grow =
    region.length * region.thickness * REGION_GROWTH / (2.0 * (region.length + region.thickness));
  region.length += grow * 2.0;
  region.thickness += grow * 2.0;
  (region.center.0 > 0.0
    && region.center.1 > 0.0
    && region.center.0 < limit.0
    && region.center.1 < limit.1)
    .then_some(region)
}

/// Short shapes have no reliable direction, and steep ones are usually single
/// tall glyphs; both stay axis-aligned, as does the noise of near-level lines.
fn is_tilted(region: &Region) -> bool {
  let degrees = region.degrees().abs();
  region.length >= region.thickness * MIN_TILT_ASPECT
    && (MIN_TILT_DEGREES..=MAX_TILT_DEGREES).contains(&degrees)
}

fn extent(points: &[Point], axis: Point) -> (f32, f32) {
  points.iter().fold((f32::MAX, f32::MIN), |range, point| {
    let position = dot(*point, axis);
    (range.0.min(position), range.1.max(position))
  })
}

fn dot(a: Point, b: Point) -> f32 {
  a.0 * b.0 + a.1 * b.1
}

/// One side of the smallest enclosing rectangle always lies on a hull edge.
fn tightest_rectangle(hull: &[Point]) -> Option<Region> {
  let mut best: Option<(f32, Region)> = None;
  for (index, start) in hull.iter().enumerate() {
    let end = hull[(index + 1) % hull.len()];
    let (x, y) = (end.0 - start.0, end.1 - start.1);
    let size = x.hypot(y);
    if size == 0.0 {
      continue;
    }
    let axis = (x / size, y / size);
    let normal = (-axis.1, axis.0);
    let (along, across) = (extent(hull, axis), extent(hull, normal));
    let (length, thickness) = (along.1 - along.0, across.1 - across.0);
    let area = length * thickness;
    if best.as_ref().is_none_or(|(smallest, _)| area < *smallest) {
      let (middle, side) = ((along.0 + along.1) / 2.0, (across.0 + across.1) / 2.0);
      let center = (
        axis.0 * middle + normal.0 * side,
        axis.1 * middle + normal.1 * side,
      );
      best = Some((area, reading_rectangle(center, axis, length, thickness)));
    }
  }
  best.map(|(_, region)| region)
}

/// Text runs along the long side, left to right.
fn reading_rectangle(center: Point, axis: Point, length: f32, thickness: f32) -> Region {
  let (axis, length, thickness) = if length >= thickness {
    (axis, length, thickness)
  } else {
    ((-axis.1, axis.0), thickness, length)
  };
  let axis = if axis.0 < 0.0 {
    (-axis.0, -axis.1)
  } else {
    axis
  };
  Region {
    center,
    axis,
    length,
    thickness,
  }
}

/// Copies a line upright. Level lines keep their exact pixels.
fn line_image(image: &RgbImage, region: Region) -> Option<RgbImage> {
  let (width, height) = image.dimensions();
  if region.axis == HORIZONTAL {
    let left = (region.center.0 - region.length / 2.0).max(0.0) as u32;
    let top = (region.center.1 - region.thickness / 2.0).max(0.0) as u32;
    let right = ((region.center.0 + region.length / 2.0) as u32).min(width);
    let bottom = ((region.center.1 + region.thickness / 2.0) as u32).min(height);
    return (right > left && bottom > top)
      .then(|| imageops::crop_imm(image, left, top, right - left, bottom - top).to_image());
  }
  let (line_width, line_height) = (region.length.ceil() as u32, region.thickness.ceil() as u32);
  let normal = region.normal();
  let mut line = RgbImage::new(line_width.max(1), line_height.max(1));
  for (x, y, pixel) in line.enumerate_pixels_mut() {
    let along = x as f32 + 0.5 - line_width as f32 / 2.0;
    let across = y as f32 + 0.5 - line_height as f32 / 2.0;
    *pixel = sample(
      image,
      region.center.0 + region.axis.0 * along + normal.0 * across,
      region.center.1 + region.axis.1 * along + normal.1 * across,
    );
  }
  Some(line)
}

/// Bilinear sample at a pixel-center coordinate, clamped to the image.
fn sample(image: &RgbImage, x: f32, y: f32) -> image::Rgb<u8> {
  let (width, height) = image.dimensions();
  let x = (x - 0.5).clamp(0.0, (width - 1) as f32);
  let y = (y - 0.5).clamp(0.0, (height - 1) as f32);
  let (left, top) = (x as u32, y as u32);
  let (right, bottom) = ((left + 1).min(width - 1), (top + 1).min(height - 1));
  let (fx, fy) = (x - left as f32, y - top as f32);
  let mut mixed = [0; 3];
  for (channel, value) in mixed.iter_mut().enumerate() {
    let at = |x: u32, y: u32| f32::from(image.get_pixel(x, y)[channel]);
    let upper = at(left, top) * (1.0 - fx) + at(right, top) * fx;
    let lower = at(left, bottom) * (1.0 - fx) + at(right, bottom) * fx;
    *value = (upper * (1.0 - fy) + lower * fy).round() as u8;
  }
  image::Rgb(mixed)
}

fn read_line(
  model: &Model,
  image: &RgbImage,
  region: Region,
  keys: &[&str],
) -> anyhow::Result<(String, f32)> {
  let Some(crop) = line_image(image, region) else {
    return Ok((String::new(), 0.0));
  };
  let (width, height) = crop.dimensions();
  let target =
    ((LINE_HEIGHT as f32 * width as f32 / height as f32).ceil() as u32).clamp(8, MAX_LINE_WIDTH);
  let padded = target.max(MIN_LINE_WIDTH) as usize;
  let resized = imageops::resize(&crop, target, LINE_HEIGHT, imageops::FilterType::Triangle);
  let rows = LINE_HEIGHT as usize;
  let mut data = vec![0f32; 3 * rows * padded];
  for (x, y, pixel) in resized.enumerate_pixels() {
    let bgr = [pixel[2], pixel[1], pixel[0]];
    for channel in 0..3 {
      data[channel * rows * padded + y as usize * padded + x as usize] =
        f32::from(bgr[channel]) / 127.5 - 1.0;
    }
  }
  let (shape, logits) = run(model, [1, 3, rows, padded], data)?;
  let [_, steps, classes] = shape[..] else {
    return Err(anyhow!("Unexpected text recognition output {shape:?}"));
  };
  anyhow::ensure!(
    classes > 0 && logits.len() >= steps * classes,
    "Unexpected text recognition output {shape:?}"
  );
  Ok(decode(&logits, steps, classes, keys))
}

/// Greedy CTC decoding. Class 0 is blank; the class after the keys is a space.
fn decode(logits: &[f32], steps: usize, classes: usize, keys: &[&str]) -> (String, f32) {
  let mut text = String::new();
  let mut last = 0;
  let (mut sum, mut count) = (0f32, 0f32);
  for row in logits.chunks_exact(classes).take(steps) {
    let (best, score) = row
      .iter()
      .enumerate()
      .fold((0, f32::MIN), |top, (class, value)| {
        if *value > top.1 { (class, *value) } else { top }
      });
    if best != 0 && best != last {
      text.push_str(keys.get(best - 1).copied().unwrap_or(" "));
      sum += score;
      count += 1.0;
    }
    last = best;
  }
  (text, if count > 0.0 { sum / count } else { 0.0 })
}

/// Drops what the recognizer invents for icons: lone symbols, unsure letters and
/// a stray glyph in front of Latin text, such as the icon beside a menu label.
fn clean(text: &str, confidence: f32) -> Option<String> {
  let text: String = text
    .chars()
    .filter(|character| !character.is_control() && !is_private_use(*character))
    .collect();
  let text = strip_icon_prefix(text.trim());
  let mut characters = text.chars();
  match (characters.next(), characters.next()) {
    (None, _) => None,
    (Some(character), None)
      if !character.is_ascii_alphanumeric() || confidence < MIN_LONE_CONFIDENCE =>
    {
      None
    },
    _ => Some(text.to_owned()),
  }
}

fn strip_icon_prefix(text: &str) -> &str {
  let mut characters = text.chars();
  let Some(first) = characters.next() else {
    return text;
  };
  let rest = characters.as_str().trim_start();
  let latin_follows = rest
    .chars()
    .next()
    .is_some_and(|next| next.is_ascii_alphabetic());
  if !first.is_ascii() && !is_latin_letter(first) && latin_follows {
    rest
  } else {
    text
  }
}

fn is_latin_letter(character: char) -> bool {
  matches!(character, '\u{C0}'..='\u{24F}') && character.is_alphabetic()
}

fn is_private_use(character: char) -> bool {
  matches!(character, '\u{E000}'..='\u{F8FF}')
}

/// Joins lines that share a row, then orders rows along the page's own tilt so
/// a slanted line does not swallow a word that sits below it.
fn reading_order(mut lines: Vec<Line>) -> String {
  let mut angles: Vec<f32> = lines.iter().map(|line| line.region.degrees()).collect();
  angles.sort_by(f32::total_cmp);
  let tilt = angles
    .get(angles.len() / 2)
    .copied()
    .unwrap_or(0.0)
    .to_radians();
  let page_axis = (tilt.cos(), tilt.sin());
  let page_normal = (-page_axis.1, page_axis.0);
  lines.sort_by(|a, b| {
    dot(a.region.center, page_normal).total_cmp(&dot(b.region.center, page_normal))
  });
  let mut rows: Vec<Vec<Line>> = Vec::new();
  for line in lines {
    let row = rows.iter_mut().rev().find(|row| {
      row
        .iter()
        .any(|other| same_row(&other.region, &line.region))
    });
    match row {
      Some(row) => row.push(line),
      None => rows.push(vec![line]),
    }
  }
  rows
    .into_iter()
    .map(|mut row| {
      row.sort_by(|a, b| {
        dot(a.region.center, page_axis).total_cmp(&dot(b.region.center, page_axis))
      });
      row
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// Measured from the longer line, whose direction is the more reliable one.
fn same_row(a: &Region, b: &Region) -> bool {
  let (long, short) = if a.length >= b.length { (a, b) } else { (b, a) };
  let offset = (
    short.center.0 - long.center.0,
    short.center.1 - long.center.1,
  );
  (a.degrees() - b.degrees()).abs() <= MAX_ROW_TILT_DEGREES
    && dot(offset, long.normal()).abs() < a.thickness.min(b.thickness) * ROW_OVERLAP
}

#[cfg(test)]
#[path = "ocr_tests.rs"]
mod tests;
