//! Drawings are anchored to pixels in the original, frozen screenshot.
//! Preview and output share Direct2D vectors and DirectWrite labels on the existing GDI surface.
//! Selection chrome is never exported.

use anyhow::{Context, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::HDC;

use crate::overlay::gdi::bitmap::ScreenshotBitmap;
use crate::overlay::selection::Selection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
  Select,
  Pen,
  /// A see-through freehand stroke, multiplied onto the capture so dark text under it stays dark.
  Highlighter,
  Arrow,
  Rectangle,
  Ellipse,
  Text,
  Marker,
  Blur,
}

/// A highlighter stroke is this many times as thick as a pen stroke of the same chosen width.
pub const HIGHLIGHTER_SCALE: i32 = 3;

impl Tool {
  /// Pen and Highlighter share freehand geometry: sampled points, path transforms and nodes.
  pub fn freehand(self) -> bool {
    matches!(self, Self::Pen | Self::Highlighter)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextData {
  pub content: String,
  pub size: i32,
  pub wrap_width: i32,
  pub extent: (i32, i32),
  pub appearance: TextAppearance,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextAppearance {
  #[default]
  Plain,
  Outline,
  Background,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Label {
  Text(TextData),
  Number(u16),
}

pub fn text_data(content: String, size: i32, wrap_width: i32) -> Result<TextData> {
  super::text::measure_text(content, size, wrap_width)
}

pub const MAX_POINTS: usize = 4096;
pub const MAX_ANNOTATIONS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
  pub path_transform: [i64; 6],
  pub curve_controls: Vec<[(i32, i32); 2]>,
  pub pen_nodes: Vec<usize>,
  pub rotation_millidegrees: i32,
  pub corner_radius: i32,
  pub label: Option<Label>,
  pub id: u64,
  pub tool: Tool,
  pub points: Vec<(i32, i32)>,
  pub color: [u8; 3],
  pub width: i32,
}

impl Annotation {
  /// The painted stroke thickness in capture pixels; `width` stays the chosen 1–10 setting.
  pub fn stroke_width(&self) -> i32 {
    if self.tool == Tool::Highlighter {
      self.width.saturating_mul(HIGHLIGHTER_SCALE)
    } else {
      self.width
    }
  }

  pub fn translated(&self, dx: i32, dy: i32) -> Result<Self> {
    let mut moved = self.clone();
    if self.tool.freehand() && self.path_transform != super::path::IDENTITY {
      moved.path_transform = super::path::compose(
        [1.0, 0.0, 0.0, 1.0, dx as f64, dy as f64],
        self.path_transform,
      )?;
      return Ok(moved);
    }
    for pair in &mut moved.curve_controls {
      for point in pair {
        point.0 = point
          .0
          .checked_add(dx)
          .context("Arrow control move overflow")?;
        point.1 = point
          .1
          .checked_add(dy)
          .context("Arrow control move overflow")?;
      }
    }
    for point in &mut moved.points {
      point.0 = point
        .0
        .checked_add(dx)
        .context("Drawing move exceeds coordinate range")?;
      point.1 = point
        .1
        .checked_add(dy)
        .context("Drawing move exceeds coordinate range")?;
    }
    Ok(moved)
  }

  pub fn hit(&self, position: (f64, f64)) -> bool {
    if self.label.is_some() {
      if let Some(Label::Text(data)) = &self.label {
        let p = super::shape::rotate(self, position, true);
        let Some(&(x, y)) = self.points.first() else {
          return false;
        };
        return p.0 >= x as f64 - 7.0
          && p.0 < x as f64 + data.extent.0 as f64 + 7.0
          && p.1 >= y as f64 - 7.0
          && p.1 < y as f64 + data.extent.1 as f64 + 7.0;
      }
      return self.paint_bounds().is_some_and(|b| {
        position.0 >= b.left as f64
          && position.0 < b.right as f64
          && position.1 >= b.top as f64
          && position.1 < b.bottom as f64
      });
    }
    let tolerance = self.stroke_width() as f64 / 2.0 + 5.0;
    let distance = |a: (i32, i32), b: (i32, i32)| {
      let a = super::path::world(self, (a.0 as f64, a.1 as f64));
      let b = super::path::world(self, (b.0 as f64, b.1 as f64));
      let (dx, dy) = (b.0 - a.0, b.1 - a.1);
      let t = if dx == 0.0 && dy == 0.0 {
        0.0
      } else {
        ((position.0 - a.0) * dx + (position.1 - a.1) * dy) / (dx * dx + dy * dy)
      }
      .clamp(0.0, 1.0);
      (position.0 - a.0 - t * dx).hypot(position.1 - a.1 - t * dy)
    };
    let (Some(&a), Some(&b)) = (self.points.first(), self.points.last()) else {
      return false;
    };
    match self.tool {
      Tool::Select | Tool::Text | Tool::Marker => false,
      Tool::Pen | Tool::Highlighter => {
        if self.points.len() == 1 {
          distance(a, a) <= tolerance
        } else {
          self
            .points
            .windows(2)
            .any(|pair| distance(pair[0], pair[1]) <= tolerance)
        }
      },
      Tool::Arrow => super::arrow::Arrow::from_annotation(self)
        .is_some_and(|arrow| arrow.hit_test(position, tolerance, self.width)),
      Tool::Rectangle => super::shape::rectangle_distance(self, position)
        .is_some_and(|distance| distance.abs() <= tolerance),
      Tool::Blur => {
        (a.0.min(b.0) as f64 - tolerance..=a.0.max(b.0) as f64 + tolerance).contains(&position.0)
          && (a.1.min(b.1) as f64 - tolerance..=a.1.max(b.1) as f64 + tolerance)
            .contains(&position.1)
      },
      Tool::Ellipse => {
        let (rx, ry) = (
          (a.0 as f64 - b.0 as f64).abs() / 2.0,
          (a.1 as f64 - b.1 as f64).abs() / 2.0,
        );
        if rx == 0.0 || ry == 0.0 {
          return false;
        }
        let (cx, cy) = (
          (a.0 as f64 + b.0 as f64) / 2.0,
          (a.1 as f64 + b.1 as f64) / 2.0,
        );
        let point = |index: usize| {
          let angle = index as f64 * std::f64::consts::TAU / 128.0;
          let p = super::shape::rotate(self, (cx + rx * angle.cos(), cy + ry * angle.sin()), false);
          (p.0.round() as i32, p.1.round() as i32)
        };
        (0..128).any(|index| distance(point(index), point(index + 1)) <= tolerance)
      },
    }
  }

  /// Conservative bounds include arrowheads, stroke radius and a small paint margin.
  pub fn paint_bounds(&self) -> Option<RECT> {
    let &(x, y) = self.points.first()?;
    if let Some(label) = &self.label {
      if self.tool == Tool::Text && self.rotation_millidegrees != 0 {
        let handles = super::shape::frame_handles(self)?;
        let mut bounds = RECT {
          left: i32::MAX,
          top: i32::MAX,
          right: i32::MIN,
          bottom: i32::MIN,
        };
        for (x, y) in handles {
          bounds.left = bounds.left.min(x.floor() as i32);
          bounds.top = bounds.top.min(y.floor() as i32);
          bounds.right = bounds.right.max(x.ceil() as i32);
          bounds.bottom = bounds.bottom.max(y.ceil() as i32);
        }
        return Some(RECT {
          left: bounds.left.saturating_sub(10),
          top: bounds.top.saturating_sub(10),
          right: bounds.right.saturating_add(11),
          bottom: bounds.bottom.saturating_add(11),
        });
      }
      return Some(match label {
        Label::Text(data) => RECT {
          left: x.saturating_sub(7),
          top: y.saturating_sub(7),
          right: x.saturating_add(data.extent.0).saturating_add(7),
          bottom: y.saturating_add(data.extent.1).saturating_add(7),
        },
        Label::Number(_) => RECT {
          left: x.saturating_sub(21),
          top: y.saturating_sub(21),
          right: x.saturating_add(22),
          bottom: y.saturating_add(22),
        },
      });
    }
    let (mut left, mut top, mut right, mut bottom) = (x, y, x, y);
    for &(x, y) in &self.points {
      left = left.min(x);
      top = top.min(y);
      right = right.max(x);
      bottom = bottom.max(y);
    }
    if let Some(arrow) = super::arrow::Arrow::from_annotation(self) {
      left = left.min(arrow.control.0.floor() as i32);
      top = top.min(arrow.control.1.floor() as i32);
      right = right.max(arrow.control.0.ceil() as i32);
      bottom = bottom.max(arrow.control.1.ceil() as i32);
    }
    for pair in &self.curve_controls {
      for &(x, y) in pair {
        left = left.min(x);
        top = top.min(y);
        right = right.max(x);
        bottom = bottom.max(y);
      }
    }
    if let Some(handles) = super::shape::handles(self) {
      left = i32::MAX;
      top = i32::MAX;
      right = i32::MIN;
      bottom = i32::MIN;
      for (x, y) in handles {
        left = left.min(x.floor() as i32);
        top = top.min(y.floor() as i32);
        right = right.max(x.ceil() as i32);
        bottom = bottom.max(y.ceil() as i32);
      }
    }
    let extra = match self.tool {
      Tool::Arrow => self.width.saturating_mul(4).saturating_add(8),
      Tool::Blur => super::blur::SOFT_EDGE,
      _ => 0,
    };
    let margin = self
      .stroke_width()
      .max(1)
      .div_euclid(2)
      .saturating_add(extra)
      .saturating_add(3);
    Some(RECT {
      left: left.saturating_sub(margin),
      top: top.saturating_sub(margin),
      right: right.saturating_add(margin).saturating_add(1),
      bottom: bottom.saturating_add(margin).saturating_add(1),
    })
  }
}

pub struct AnnotatedSelection {
  pub selection: Selection,
  pub annotations: Vec<Annotation>,
  /// F3 asked to pin the selection instead of saving it.
  pub pin: bool,
}

/// Preview and output share the antialiased renderer and screenshot-pixel geometry.
pub fn draw_annotations<'a>(
  dc: HDC,
  annotations: impl Iterator<Item = &'a Annotation>,
  origin: (i32, i32),
  clip: RECT,
) -> Result<()> {
  super::smooth::draw(dc, annotations, origin, clip)
}
/// Composite onto the existing crop. Does not capture or read the desktop.
pub fn composite(
  rgba: Vec<u8>,
  selection: &Selection,
  annotations: &[Annotation],
) -> Result<Vec<u8>> {
  let clip = RECT {
    left: selection.x as i32,
    top: selection.y as i32,
    right: (selection.x + selection.width) as i32,
    bottom: (selection.y + selection.height) as i32,
  };
  let visible = |drawing: &&Annotation| {
    drawing.paint_bounds().is_some_and(|bounds| {
      bounds.left < clip.right
        && bounds.right > clip.left
        && bounds.top < clip.bottom
        && bounds.bottom > clip.top
    })
  };
  if !annotations.iter().any(|drawing| visible(&drawing)) {
    return Ok(rgba);
  }
  let started = crate::overlay::screenshot::performance::enabled().then(std::time::Instant::now);
  let mut bitmap = ScreenshotBitmap::from_rgba(&rgba, selection.width, selection.height)
    .map_err(anyhow::Error::msg)
    .context("Failed to prepare annotated screenshot")?;
  draw_annotations(
    bitmap.dc,
    annotations.iter().filter(visible),
    (selection.x as i32, selection.y as i32),
    RECT {
      left: 0,
      top: 0,
      right: selection.width as i32,
      bottom: selection.height as i32,
    },
  )?;
  let result = bitmap.to_rgba_into(rgba).map_err(anyhow::Error::msg);
  if let Some(started) = started {
    tracing::info!(
      duration_us = crate::overlay::screenshot::performance::elapsed_us(started),
      width = selection.width,
      height = selection.height,
      drawings = annotations.len(),
      succeeded = result.is_ok(),
      "Screenshot annotation export diagnostics"
    );
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn transformed_translation_keeps_hit_testing_and_curve_controls_aligned() {
    let drawing = Annotation {
      id: 1,
      tool: Tool::Pen,
      points: vec![(20, 20), (100, 20), (100, 60), (160, 60)],
      color: [240, 20, 20],
      width: 4,
      label: None,
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: super::super::path::fixed([1.5, 0., 0., 1.5, 30., 10.]).unwrap(),
      curve_controls: vec![],
      pen_nodes: vec![],
    };
    let transformed_point = super::super::path::world(&drawing, (20., 20.));
    assert!(drawing.hit(transformed_point));
    let moved = drawing.translated(12, -5).unwrap();
    assert_eq!(
      super::super::path::world(&moved, (20., 20.)),
      (transformed_point.0 + 12., transformed_point.1 - 5.)
    );
    let mut curve = drawing.clone();
    curve.tool = Tool::Arrow;
    curve.path_transform = super::super::path::IDENTITY;
    curve.points = vec![(20, 20), (100, 60)];
    curve.curve_controls = vec![[(20, 20), (40, 10)], [(80, 70), (100, 60)]];
    let moved = curve.translated(5, 7).unwrap();
    assert_eq!(
      moved.curve_controls,
      vec![[(25, 27), (45, 17)], [(85, 77), (105, 67)]]
    );
    curve.curve_controls[0][0].0 = i32::MAX;
    assert!(
      curve
        .translated(1, 0)
        .unwrap_err()
        .to_string()
        .contains("Arrow control move overflow")
    );
  }

  #[test]
  fn text_readability_styles_match_export_and_stay_within_paint_bounds() {
    let source: Vec<u8> = (0..800 * 400)
      .flat_map(|i| {
        if (i / 800 / 40 + i % 800 / 40) % 2 == 0 {
          [35, 45, 55, 255]
        } else {
          [220, 220, 220, 255]
        }
      })
      .collect();
    let mut previews = Vec::new();
    for appearance in [
      TextAppearance::Plain,
      TextAppearance::Outline,
      TextAppearance::Background,
    ] {
      for size in [12, 16, 24, 32, 48, 64] {
        let mut data =
          text_data("Café 日本語\nMultiline 🙂".into(), size, 600).expect("text layout");
        data.appearance = appearance;
        let drawing = Annotation {
          id: 1,
          tool: Tool::Text,
          label: Some(Label::Text(data)),
          points: vec![(60, 40)],
          color: [255, 255, 255],
          width: 3,
          corner_radius: 0,
          rotation_millidegrees: 0,
          path_transform: crate::screenshot::path::IDENTITY,
          curve_controls: Vec::new(),
          pen_nodes: Vec::new(),
        };
        let mut bitmap = ScreenshotBitmap::from_rgba(&source, 800, 400).expect("preview surface");
        draw_annotations(
          bitmap.dc,
          std::iter::once(&drawing),
          (0, 0),
          RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 400,
          },
        )
        .expect("preview");
        let pixels = bitmap.to_rgba().expect("preview pixels");
        let output = composite(
          source.clone(),
          &Selection {
            x: 0,
            y: 0,
            width: 800,
            height: 400,
            source_app: None,
          },
          std::slice::from_ref(&drawing),
        )
        .expect("export");
        assert_eq!(
          pixels, output,
          "preview/export differ at {size}px with {appearance:?}"
        );
        let bounds = drawing.paint_bounds().expect("bounds");
        for (index, (before, after)) in source
          .as_chunks::<4>()
          .0
          .iter()
          .zip(pixels.as_chunks::<4>().0)
          .enumerate()
        {
          if before != after {
            let (x, y) = (index as i32 % 800, index as i32 / 800);
            assert!(
              x >= bounds.left && x < bounds.right && y >= bounds.top && y < bounds.bottom,
              "text backing exceeds paint bounds"
            );
          }
        }
        if size == 48 {
          previews.push(pixels);
        }
      }
    }
    assert_ne!(previews[0], previews[1]);
    assert_ne!(previews[1], previews[2]);
    if let Ok(folder) = std::env::var("FLINCH_ANNOTATION_PREVIEW_DIR") {
      let folder = std::path::PathBuf::from(folder);
      std::fs::create_dir_all(&folder).expect("visual directory");
      for (name, pixels) in ["plain", "outline", "background"].into_iter().zip(previews) {
        image::save_buffer(
          folder.join(format!("text-{name}.png")),
          &pixels,
          800,
          400,
          image::ColorType::Rgba8,
        )
        .expect("style PNG");
      }
    }
  }

  fn selection() -> Selection {
    Selection {
      x: 100,
      y: 50,
      width: 30,
      height: 20,
      source_app: None,
    }
  }

  #[test]
  fn diagonal_strokes_have_partial_edge_coverage_and_opaque_output() {
    for tool in [Tool::Pen, Tool::Arrow, Tool::Ellipse] {
      let drawing = Annotation {
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
        label: None,
        id: 1,
        tool,
        points: vec![(103, 53), (124, 65)],
        color: [0, 0, 0],
        width: 3,
      };
      let output =
        composite(vec![255; 30 * 20 * 4], &selection(), &[drawing]).expect("antialiased drawing");
      let partial = output
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] > 0 && pixel[0] < 255)
        .count();
      assert!(
        partial > 10,
        "{tool:?} needs blended edge coverage, got {partial}"
      );
      assert!(
        output
          .as_chunks::<4>()
          .0
          .iter()
          .all(|pixel| pixel[3] == 255)
      );
    }
  }

  #[test]
  fn empty_annotations_preserve_every_byte() {
    let rgba = vec![123; 30 * 20 * 4];
    assert_eq!(
      composite(rgba.clone(), &selection(), &[]).expect("unchanged crop"),
      rgba
    );
  }

  #[test]
  fn output_draws_original_pixels_with_crop_offset_and_opaque_alpha() {
    let rgba = vec![255; 30 * 20 * 4];
    let original_buffer = rgba.as_ptr();
    let drawing = Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 0,
      tool: Tool::Pen,
      points: vec![(102, 55), (120, 55)],
      color: [230, 20, 30],
      width: 3,
    };
    let output = composite(rgba, &selection(), &[drawing]).expect("composite");
    assert_eq!(
      output.as_ptr(),
      original_buffer,
      "reuse the crop readback buffer"
    );
    assert_eq!(
      &output[(5 * 30 + 10) * 4..(5 * 30 + 10) * 4 + 4],
      &[230, 20, 30, 255]
    );
    assert_eq!(&output[..4], &[255; 4]);
    assert!(
      output
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[3] == 255)
    );
  }

  #[test]
  fn drawings_outside_crop_do_not_change_it() {
    let rgba = vec![255; 30 * 20 * 4];
    let drawing = Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 0,
      tool: Tool::Rectangle,
      points: vec![(0, 0), (10, 10)],
      color: [255, 0, 0],
      width: 3,
    };
    assert_eq!(
      composite(rgba.clone(), &selection(), &[drawing]).expect("clipped crop"),
      rgba
    );
  }
  #[test]
  fn pen_click_draws_a_dot_and_zero_sized_shapes_are_harmless() {
    let rgba = vec![255; 30 * 20 * 4];
    let dot = Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id: 0,
      tool: Tool::Pen,
      points: vec![(110, 60)],
      color: [255, 0, 0],
      width: 3,
    };
    let output = composite(rgba.clone(), &selection(), &[dot]).expect("pen dot");
    assert_eq!(
      &output[(10 * 30 + 10) * 4..(10 * 30 + 10) * 4 + 4],
      &[255, 0, 0, 255]
    );
    for tool in [Tool::Arrow, Tool::Rectangle, Tool::Ellipse] {
      let empty = Annotation {
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
        label: None,
        id: 0,
        tool,
        points: vec![(110, 60), (110, 60)],
        color: [255, 0, 0],
        width: 3,
      };
      assert_eq!(
        composite(rgba.clone(), &selection(), &[empty]).expect("empty shape"),
        rgba
      );
    }
  }
}

#[cfg(test)]
mod label_tests {
  use super::*;
  #[test]
  fn unicode_wrap_metrics_and_label_export_are_shared_and_bounded() {
    let text = text_data(
      "Café 日本語 🙂\nA long line that wraps into several rows".into(),
      24,
      140,
    )
    .expect("Unicode metrics");
    assert!(text.extent.1 > 48);
    assert!(text.extent.0 <= 143);
    assert!(text_data("a".repeat(2049), 24, 140).is_err());
    let annotations = [
      Annotation {
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
        label: Some(Label::Text(text)),
        id: 1,
        tool: Tool::Text,
        points: vec![(110, 60)],
        color: [240, 20, 20],
        width: 3,
      },
      Annotation {
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
        label: Some(Label::Number(999)),
        id: 2,
        tool: Tool::Marker,
        points: vec![(250, 100)],
        color: [250, 204, 21],
        width: 3,
      },
    ];
    let selection = Selection {
      x: 100,
      y: 50,
      width: 200,
      height: 180,
      source_app: None,
    };
    let source = vec![255; 200 * 180 * 4];
    let output = composite(source.clone(), &selection, &annotations).expect("label export");
    assert_ne!(output, source);
    assert!(
      output
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[3] == 255)
    );
    assert!(annotations[0].hit((115.0, 65.0)));
    assert!(!annotations[0].hit((400.0, 65.0)));
    assert_eq!(
      annotations[1]
        .translated(20, -10)
        .expect("move marker")
        .label,
      Some(Label::Number(999))
    );
  }
}
