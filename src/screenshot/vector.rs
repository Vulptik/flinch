//! Shared annotation vectors for preview and export. Resources never leave their thread.
//! A fixed tile bounds intermediate memory independently of desktop size.

use anyhow::{Context, Result, ensure};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::{Common::*, *};
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
  AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BLENDFUNCTION, BitBlt, GdiFlush, HDC, SRCCOPY,
};
use windows_numerics::{Matrix3x2, Vector2};

use super::annotations::{Annotation, Label, TextAppearance, Tool};
use crate::overlay::gdi::layered::LayeredBitmap;

const TILE: i32 = 512;
pub(super) struct Renderer {
  factory: ID2D1Factory,
  target: ID2D1DCRenderTarget,
  brush: ID2D1SolidColorBrush,
  stroke: ID2D1StrokeStyle,
  bitmap: LayeredBitmap,
  /// The pixels under a highlighter tile, allocated on the first highlighter.
  scratch: Option<LayeredBitmap>,
  write: Option<IDWriteFactory>,
  formats: Vec<(i32, bool, IDWriteTextFormat)>,
}

impl Renderer {
  pub(super) fn new() -> Result<Self> {
    // SAFETY: factory, target and bitmap are created, used and released on this thread.
    unsafe {
      let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
      let target = factory.CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT {
          format: DXGI_FORMAT_B8G8R8A8_UNORM,
          alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        ..Default::default()
      })?;
      target.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
      target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
      let brush = target.CreateSolidColorBrush(
        &D2D1_COLOR_F {
          r: 1.0,
          g: 1.0,
          b: 1.0,
          a: 1.0,
        },
        None,
      )?;
      let stroke = factory.CreateStrokeStyle(
        &D2D1_STROKE_STYLE_PROPERTIES {
          startCap: D2D1_CAP_STYLE_ROUND,
          endCap: D2D1_CAP_STYLE_ROUND,
          lineJoin: D2D1_LINE_JOIN_ROUND,
          miterLimit: 1.0,
          ..Default::default()
        },
        None,
      )?;
      let bitmap = LayeredBitmap::new(TILE, TILE).map_err(anyhow::Error::msg)?;
      Ok(Self {
        factory,
        target,
        brush,
        stroke,
        bitmap,
        scratch: None,
        write: None,
        formats: Vec::new(),
      })
    }
  }

  fn label_layout(&mut self, annotation: &Annotation) -> Result<Option<IDWriteTextLayout>> {
    let (content, size, width, centered) = match &annotation.label {
      Some(Label::Text(data)) => {
        ensure!(
          annotation.tool == Tool::Text && data.extent.0 > 0 && data.extent.1 > 0,
          "Invalid text annotation"
        );
        (data.content.clone(), data.size, data.wrap_width, false)
      },
      Some(Label::Number(number)) => {
        ensure!(
          annotation.tool == Tool::Marker && (1..=999).contains(number),
          "Invalid numbered marker"
        );
        (number.to_string(), 16, 36, true)
      },
      None => return Ok(None),
    };
    if self.write.is_none() {
      // SAFETY: factory and all derived formats/layouts stay in this synchronous caller-thread batch.
      self.write = Some(unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? });
    }
    let factory = self
      .write
      .as_ref()
      .context("Annotation text factory unavailable")?;
    if !self
      .formats
      .iter()
      .any(|(cached_size, cached_centered, _)| *cached_size == size && *cached_centered == centered)
    {
      ensure!(
        self.formats.len() < 64,
        "Annotation font cache budget exceeded"
      );
      self.formats.push((
        size,
        centered,
        super::text::format(factory, size, centered)?,
      ));
    }
    let (_, _, format) = self
      .formats
      .iter()
      .find(|(cached_size, cached_centered, _)| {
        *cached_size == size && *cached_centered == centered
      })
      .context("Annotation font format unavailable")?;
    Ok(Some(super::text::layout(
      factory, format, &content, width, centered,
    )?))
  }

  unsafe fn paint_label(
    &self,
    annotation: &Annotation,
    layout: &IDWriteTextLayout,
    anchor: Vector2,
  ) {
    let color = annotation.color;
    let light = 299 * color[0] as u32 + 587 * color[1] as u32 + 114 * color[2] as u32 > 145_000;
    let contrast = if light { [20, 20, 23] } else { [255; 3] };
    // SAFETY: caller has begun this owned target's draw batch; layout/brush remain live throughout it.
    unsafe {
      self.brush.SetColor(&paint_color(color));
      match &annotation.label {
        Some(Label::Number(_)) => {
          self.target.FillEllipse(
            &D2D1_ELLIPSE {
              point: anchor,
              radiusX: 18.0,
              radiusY: 18.0,
            },
            &self.brush,
          );
          self.brush.SetColor(&paint_color(contrast));
          self.target.DrawTextLayout(
            Vector2 {
              X: anchor.X - 18.0,
              Y: anchor.Y - 18.0,
            },
            layout,
            &self.brush,
            D2D1_DRAW_TEXT_OPTIONS_NO_SNAP,
          );
        },
        Some(Label::Text(data)) => {
          self.brush.SetColor(&paint_color(contrast));
          match data.appearance {
            TextAppearance::Background => self.target.FillRectangle(
              &D2D_RECT_F {
                left: anchor.X - 4.0,
                top: anchor.Y - 4.0,
                right: anchor.X + data.extent.0 as f32 + 4.0,
                bottom: anchor.Y + data.extent.1 as f32 + 4.0,
              },
              &self.brush,
            ),
            TextAppearance::Outline => {
              for (dx, dy) in [
                (-2.0, -2.0),
                (0.0, -2.0),
                (2.0, -2.0),
                (-2.0, 0.0),
                (2.0, 0.0),
                (-2.0, 2.0),
                (0.0, 2.0),
                (2.0, 2.0),
              ] {
                self.target.DrawTextLayout(
                  Vector2 {
                    X: anchor.X + dx,
                    Y: anchor.Y + dy,
                  },
                  layout,
                  &self.brush,
                  D2D1_DRAW_TEXT_OPTIONS_NO_SNAP,
                );
              }
            },
            TextAppearance::Plain => {},
          }
          self.brush.SetColor(&paint_color(color));
          self
            .target
            .DrawTextLayout(anchor, layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NO_SNAP);
        },
        None => {},
      }
    }
  }

  fn path(&self, points: &[Vector2]) -> Result<ID2D1PathGeometry> {
    ensure!(!points.is_empty(), "Empty annotation path");
    // SAFETY: geometry and sink belong to this thread; point storage is live for AddLines.
    unsafe {
      let path = self.factory.CreatePathGeometry()?;
      let sink = path.Open()?;
      sink.BeginFigure(points[0], D2D1_FIGURE_BEGIN_HOLLOW);
      sink.AddLines(&points[1..]);
      sink.EndFigure(D2D1_FIGURE_END_OPEN);
      sink.Close()?;
      Ok(path)
    }
  }

  pub(super) fn draw(
    &mut self,
    dc: HDC,
    annotation: &Annotation,
    origin: (i32, i32),
    clip: RECT,
  ) -> Result<()> {
    self.paint(dc, annotation, origin, clip)
  }

  fn paint(
    &mut self,
    dc: HDC,
    annotation: &Annotation,
    origin: (i32, i32),
    clip: RECT,
  ) -> Result<()> {
    ensure!(
      (1..=10).contains(&annotation.width),
      "Invalid annotation stroke width"
    );
    ensure!(
      super::path::inverse(annotation.path_transform).is_some(),
      "Invalid path transform"
    );
    ensure!(
      annotation.label.is_some() || !matches!(annotation.tool, Tool::Text | Tool::Marker),
      "Label annotation is missing its payload"
    );
    ensure!(
      annotation.points.len() <= super::annotations::MAX_POINTS,
      "Annotation point budget exceeded"
    );
    ensure!(
      annotation.tool != Tool::Arrow
        || annotation.curve_controls.is_empty() && (2..=3).contains(&annotation.points.len())
        || super::arrow::Arrow::from_annotation(annotation).is_some(),
      "Invalid arrow point count"
    );
    let Some(bounds) = annotation.paint_bounds() else {
      return Ok(());
    };
    let (left, top, right, bottom) = (
      (bounds.left as i64 - origin.0 as i64).max(clip.left as i64),
      (bounds.top as i64 - origin.1 as i64).max(clip.top as i64),
      (bounds.right as i64 - origin.0 as i64).min(clip.right as i64),
      (bounds.bottom as i64 - origin.1 as i64).min(clip.bottom as i64),
    );
    if right <= left || bottom <= top {
      return Ok(());
    }
    ensure!(
      (right - left) * (bottom - top) <= 128 * 1024 * 1024,
      "Annotation surface budget exceeded"
    );
    let bounds = RECT {
      left: left as i32,
      top: top as i32,
      right: right as i32,
      bottom: bottom as i32,
    };
    let points: Vec<_> = annotation
      .points
      .iter()
      .map(|&(x, y)| {
        let p = super::path::world(annotation, (x as f64, y as f64));
        Vector2 {
          X: (p.0 - origin.0 as f64) as f32,
          Y: (p.1 - origin.1 as f64) as f32,
        }
      })
      .collect();
    let first = points[0];
    let last = points[points.len() - 1];
    let path = if annotation.tool.freehand() && points.len() > 1 {
      Some(self.path(&points)?)
    } else {
      None
    };
    let arrow_geometry = super::arrow::Arrow::from_annotation(annotation);
    // Keep tessellation coordinates independent of crop origin. Integer translation
    // happens at painting, so preview/export flatten the exact same quadratic.
    let arrow_origin = annotation.points[0];
    let convert = |p: super::arrow::Point| Vector2 {
      X: (p.0 - arrow_origin.0 as f64) as f32,
      Y: (p.1 - arrow_origin.1 as f64) as f32,
    };
    let arrow = if let Some(geometry) = &arrow_geometry {
      Some(self.path(&geometry.head(annotation.width).map(convert))?)
    } else {
      None
    };
    let shaft = if let Some(geometry) = &arrow_geometry {
      // SAFETY: factory and geometry are owned exclusively by this render batch.
      unsafe {
        let path = self.factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        sink.BeginFigure(convert(geometry.start), D2D1_FIGURE_BEGIN_HOLLOW);
        if !annotation.curve_controls.is_empty() {
          for i in 1..annotation.points.len() {
            let p = annotation.curve_controls[i - 1][1];
            let q = annotation.curve_controls[i][0];
            let end = annotation.points[i];
            sink.AddBezier(&D2D1_BEZIER_SEGMENT {
              point1: convert((p.0 as f64, p.1 as f64)),
              point2: convert((q.0 as f64, q.1 as f64)),
              point3: convert((end.0 as f64, end.1 as f64)),
            });
          }
        } else {
          sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
            point1: convert(geometry.control),
            point2: convert(geometry.end),
          });
        }
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close()?;
        Some(path)
      }
    } else {
      None
    };
    // Layout preparation may fail; complete it before BeginDraw so every batch is paired.
    let label = self.label_layout(annotation)?;
    let color = annotation.color;
    let width = annotation.stroke_width().max(1) as f32;
    let multiply = annotation.tool == Tool::Highlighter;
    if multiply && self.scratch.is_none() {
      self.scratch = Some(LayeredBitmap::new(TILE, TILE).map_err(anyhow::Error::msg)?);
    }
    let local = RECT {
      left: 0,
      top: 0,
      right: TILE.min(bounds.right - bounds.left),
      bottom: TILE.min(bounds.bottom - bounds.top),
    };
    // SAFETY: the caller owns its DC; all render resources remain on this thread.
    unsafe {
      self.brush.SetColor(&D2D1_COLOR_F {
        r: color[0] as f32 / 255.0,
        g: color[1] as f32 / 255.0,
        b: color[2] as f32 / 255.0,
        a: 1.0,
      });
      for y in (bounds.top..bounds.bottom).step_by(TILE as usize) {
        for x in (bounds.left..bounds.right).step_by(TILE as usize) {
          self.target.BindDC(self.bitmap.dc, &local)?;
          self.target.BeginDraw();
          self.target.SetTransform(&Matrix3x2::identity());
          self.target.Clear(Some(&D2D1_COLOR_F::default()));
          self.target.SetTransform(&Matrix3x2 {
            M11: 1.0,
            M22: 1.0,
            M31: -x as f32,
            M32: -y as f32,
            ..Default::default()
          });
          if let Some(label) = &label {
            if annotation.tool == Tool::Text && annotation.rotation_millidegrees != 0 {
              let (l, t, r, b) =
                super::shape::bounds(annotation).context("Invalid rotated text bounds")?;
              let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
              let (sin, cos) = (annotation.rotation_millidegrees as f64 / 1000.0)
                .to_radians()
                .sin_cos();
              self.target.SetTransform(&Matrix3x2 {
                M11: cos as f32,
                M12: sin as f32,
                M21: -sin as f32,
                M22: cos as f32,
                M31: (cx - origin.0 as f64 - x as f64) as f32,
                M32: (cy - origin.1 as f64 - y as f64) as f32,
              });
              self.paint_label(
                annotation,
                label,
                Vector2 {
                  X: (l - cx) as f32,
                  Y: (t - cy) as f32,
                },
              );
              self.target.SetTransform(&Matrix3x2 {
                M11: 1.0,
                M22: 1.0,
                M31: -x as f32,
                M32: -y as f32,
                ..Default::default()
              });
            } else {
              self.paint_label(annotation, label, first);
            }
          } else {
            match annotation.tool {
              Tool::Pen | Tool::Highlighter => {
                if let Some(path) = &path {
                  self
                    .target
                    .DrawGeometry(path, &self.brush, width, &self.stroke);
                } else {
                  self.target.FillEllipse(
                    &D2D1_ELLIPSE {
                      point: first,
                      radiusX: width / 2.0,
                      radiusY: width / 2.0,
                    },
                    &self.brush,
                  );
                }
              },
              Tool::Arrow => {
                if let Some(arrow) = &arrow {
                  self.target.SetTransform(&Matrix3x2 {
                    M11: 1.0,
                    M22: 1.0,
                    M31: (arrow_origin.0 as f64 - origin.0 as f64 - x as f64) as f32,
                    M32: (arrow_origin.1 as f64 - origin.1 as f64 - y as f64) as f32,
                    ..Default::default()
                  });
                  if let Some(shaft) = &shaft {
                    self
                      .target
                      .DrawGeometry(shaft, &self.brush, width, &self.stroke);
                  }
                  self
                    .target
                    .DrawGeometry(arrow, &self.brush, width, &self.stroke);
                  self.target.SetTransform(&Matrix3x2 {
                    M11: 1.0,
                    M22: 1.0,
                    M31: -x as f32,
                    M32: -y as f32,
                    ..Default::default()
                  });
                }
              },
              Tool::Rectangle | Tool::Ellipse if first.X != last.X && first.Y != last.Y => {
                let angle = (annotation.rotation_millidegrees as f64 / 1000.0).to_radians();
                let (sin, cos) = angle.sin_cos();
                let (a, b) = (annotation.points[0], annotation.points[1]);
                let (cx, cy) = (
                  (a.0 as f64 + b.0 as f64) / 2.0,
                  (a.1 as f64 + b.1 as f64) / 2.0,
                );
                self.target.SetTransform(&Matrix3x2 {
                  M11: cos as f32,
                  M12: sin as f32,
                  M21: -sin as f32,
                  M22: cos as f32,
                  M31: (cx - origin.0 as f64 - x as f64) as f32,
                  M32: (cy - origin.1 as f64 - y as f64) as f32,
                });
                let rect = D2D_RECT_F {
                  left: (a.0.min(b.0) as f64 - cx) as f32,
                  top: (a.1.min(b.1) as f64 - cy) as f32,
                  right: (a.0.max(b.0) as f64 - cx) as f32,
                  bottom: (a.1.max(b.1) as f64 - cy) as f32,
                };
                if annotation.tool == Tool::Rectangle {
                  let radius = super::shape::radius(annotation) as f32;
                  self.target.DrawRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                      rect,
                      radiusX: radius,
                      radiusY: radius,
                    },
                    &self.brush,
                    width,
                    &self.stroke,
                  );
                } else {
                  self.target.DrawEllipse(
                    &D2D1_ELLIPSE {
                      point: Vector2 {
                        X: (rect.left + rect.right) / 2.0,
                        Y: (rect.top + rect.bottom) / 2.0,
                      },
                      radiusX: (rect.right - rect.left) / 2.0,
                      radiusY: (rect.bottom - rect.top) / 2.0,
                    },
                    &self.brush,
                    width,
                    &self.stroke,
                  );
                }
                self.target.SetTransform(&Matrix3x2 {
                  M11: 1.0,
                  M22: 1.0,
                  M31: -x as f32,
                  M32: -y as f32,
                  ..Default::default()
                });
              },
              _ => {},
            }
          }
          self
            .target
            .EndDraw(None, None)
            .context("Finish annotation vectors")?;
          if multiply {
            self.multiply_tile(
              dc,
              x,
              y,
              (bounds.right - x).min(TILE),
              (bounds.bottom - y).min(TILE),
            )?;
            continue;
          }
          ensure!(
            AlphaBlend(
              dc,
              x,
              y,
              (bounds.right - x).min(TILE),
              (bounds.bottom - y).min(TILE),
              self.bitmap.dc,
              0,
              0,
              (bounds.right - x).min(TILE),
              (bounds.bottom - y).min(TILE),
              BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
                BlendFlags: 0
              }
            )
            .as_bool(),
            "Composite annotation vectors failed"
          );
        }
      }
      ensure!(GdiFlush().as_bool(), "Flush annotation vectors failed");
    }
    Ok(())
  }

  /// Multiplies the drawn tile onto `dc`, like a highlighter on paper: each channel of the
  /// capture is scaled by the stroke color where the stroke covers it, so text keeps its contrast.
  /// One stroke is one coverage mask, so crossings never darken twice.
  fn multiply_tile(&mut self, dc: HDC, x: i32, y: i32, width: i32, height: i32) -> Result<()> {
    ensure!(
      (1..=TILE).contains(&width) && (1..=TILE).contains(&height),
      "Invalid highlighter tile"
    );
    let scratch = self
      .scratch
      .as_mut()
      .context("Highlighter surface unavailable")?;
    // SAFETY: both DCs are live on this thread; the scratch tile is TILE square, so the copied
    // width and height (checked above) stay inside it. GdiFlush completes pending GDI and
    // Direct2D writes before the CPU reads either DIB's memory.
    unsafe {
      ensure!(GdiFlush().as_bool(), "Flush highlighter stroke failed");
      BitBlt(scratch.dc, 0, 0, width, height, Some(dc), x, y, SRCCOPY)
        .context("Read pixels under highlighter")?;
      ensure!(GdiFlush().as_bool(), "Flush highlighter backdrop failed");
    }
    multiply_rows(
      self.bitmap.pixels_mut(),
      scratch.pixels_mut(),
      TILE as usize,
      width as usize,
      height as usize,
    )?;
    // SAFETY: as above; the blended tile is written back to the same live destination area.
    unsafe {
      BitBlt(dc, x, y, width, height, Some(scratch.dc), 0, 0, SRCCOPY)
        .context("Write highlighted pixels")?;
    }
    Ok(())
  }
}

/// `stroke` is premultiplied BGRA coverage; `backdrop` is replaced by backdrop × stroke color,
/// mixed by coverage: `backdrop × (255 − alpha + premultiplied) / 255`. Backdrop alpha is kept.
fn multiply_rows(
  stroke: &[u8],
  backdrop: &mut [u8],
  stride: usize,
  width: usize,
  height: usize,
) -> Result<()> {
  ensure!(width <= stride, "Highlighter row exceeds its tile");
  for row in 0..height {
    let start = row * stride * 4;
    let end = start + width * 4;
    let (Some(stroke), Some(backdrop)) = (stroke.get(start..end), backdrop.get_mut(start..end))
    else {
      anyhow::bail!("Highlighter rows exceed their tile");
    };
    for (ink, pixel) in stroke
      .as_chunks::<4>()
      .0
      .iter()
      .zip(backdrop.as_chunks_mut::<4>().0)
    {
      let coverage = ink[3] as u32;
      if coverage == 0 {
        continue;
      }
      for channel in 0..3 {
        let factor = 255 - coverage + (ink[channel] as u32).min(coverage);
        pixel[channel] = ((pixel[channel] as u32 * factor + 127) / 255) as u8;
      }
    }
  }
  Ok(())
}

fn paint_color(color: [u8; 3]) -> D2D1_COLOR_F {
  D2D1_COLOR_F {
    r: color[0] as f32 / 255.0,
    g: color[1] as f32 / 255.0,
    b: color[2] as f32 / 255.0,
    a: 1.0,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use crate::screenshot::annotations::draw_annotations;

  fn samples() -> Vec<Annotation> {
    let mut drawings = Vec::new();
    for (row, tool) in [Tool::Pen, Tool::Arrow, Tool::Rectangle, Tool::Ellipse]
      .into_iter()
      .enumerate()
    {
      for (column, width) in [3, 6, 10].into_iter().enumerate() {
        let x = 30 + column as i32 * 340;
        let y = 30 + row as i32 * 160;
        drawings.push(Annotation {
          id: drawings.len() as u64 + 1,
          tool,
          label: None,
          color: [239, 68, 68],
          width,
          points: if tool == Tool::Pen {
            vec![
              (x, y + 80),
              (x + 70, y + 30),
              (x + 150, y + 90),
              (x + 250, y + 20),
            ]
          } else {
            vec![(x, y + 20), (x + 250, y + 110)]
          },
          corner_radius: 0,
          rotation_millidegrees: 0,
          path_transform: crate::screenshot::path::IDENTITY,
          curve_controls: Vec::new(),
          pen_nodes: Vec::new(),
        });
      }
    }
    drawings
  }

  #[test]
  fn tiled_vectors_match_cropped_preview_and_damage() {
    let mut drawings = samples();
    for drawing in &mut drawings {
      if drawing.tool.freehand() {
        drawing.path_transform =
          super::super::path::fixed([1.1, 0.25, -0.15, 0.9, 20.0, -20.0]).expect("pen transform");
      }
      if drawing.tool == Tool::Rectangle {
        drawing.corner_radius = 40;
        drawing.rotation_millidegrees = 35_000;
      }
      if drawing.tool == Tool::Ellipse {
        drawing.rotation_millidegrees = 325_000;
      }
    }
    drawings.push(Annotation {
      id: 100,
      tool: Tool::Arrow,
      label: None,
      points: vec![(120, 620), (570, 330), (740, 80)],
      width: 6,
      color: [0, 140, 255],
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    });
    drawings.push(Annotation {
      id: 102,
      tool: Tool::Highlighter,
      label: None,
      color: [250, 204, 21],
      width: 6,
      // Crosses the 512px tile edge and the pen stroke below, so the read-back covers both.
      points: vec![(60, 470), (520, 560), (980, 500), (540, 450)],
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: super::super::path::fixed([1.0, 0.1, 0.0, 1.0, 0.0, 0.0])
        .expect("highlighter transform"),
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    });
    drawings.push(Annotation {
      id: 13,
      tool: Tool::Pen,
      label: None,
      color: [40, 90, 220],
      width: 10,
      points: vec![(-20, 510), (1040, 540)],
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    });
    let arrow = drawings.iter().find(|a| a.id == 100).expect("curved arrow");
    let p = super::super::arrow::Arrow::from_annotation(arrow)
      .expect("arrow geometry")
      .at(0.3);
    let (mut multi, _) = super::super::arrow::insert_node(arrow, p).expect("arrow node");
    multi.id = 101;
    drawings.push(multi);
    for (index, appearance) in [
      TextAppearance::Plain,
      TextAppearance::Outline,
      TextAppearance::Background,
    ]
    .into_iter()
    .enumerate()
    {
      let mut data =
        super::super::annotations::text_data("Café 日本語 🙂\nمرحبا بالعالم".into(), 32, 470)
          .expect("label layout");
      data.appearance = appearance;
      drawings.push(Annotation {
        id: 14 + index as u64,
        tool: Tool::Text,
        label: Some(Label::Text(data)),
        points: vec![(480, 90 + index as i32 * 175)],
        color: [60, 90, 140],
        width: 3,
        corner_radius: 0,
        rotation_millidegrees: 25_000 + index as i32 * 30_000,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
      });
    }
    for (index, (number, color)) in [
      (1, [239, 68, 68]),
      (99, [250, 204, 21]),
      (999, [20, 20, 23]),
    ]
    .into_iter()
    .enumerate()
    {
      drawings.push(Annotation {
        id: 17 + index as u64,
        tool: Tool::Marker,
        label: Some(Label::Number(number)),
        points: vec![(512, 175 + index as i32 * 160)],
        color,
        width: 3,
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
      });
    }
    let source = vec![255; 1050 * 700 * 4];
    let mut preview = ScreenshotBitmap::from_rgba(&source, 1050, 700).expect("preview");
    let full = RECT {
      left: 0,
      top: 0,
      right: 1050,
      bottom: 700,
    };
    draw_annotations(preview.dc, drawings.iter(), (0, 0), full).expect("full vectors");
    let preview = preview.to_rgba().expect("preview pixels");
    let mut crop = ScreenshotBitmap::from_rgba(&vec![255; 800 * 600 * 4], 800, 600).expect("crop");
    draw_annotations(
      crop.dc,
      drawings.iter(),
      (100, 50),
      RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
      },
    )
    .expect("crop vectors");
    let crop = crop.to_rgba().expect("crop pixels");
    for y in 0..600 {
      assert_eq!(
        &crop[y * 800 * 4..(y + 1) * 800 * 4],
        &preview[((y + 50) * 1050 + 100) * 4..((y + 50) * 1050 + 900) * 4],
        "crop row {y}"
      );
    }
    let mut damage = ScreenshotBitmap::from_rgba(&source, 1050, 700).expect("damage");
    for (left, right) in [(0, 512), (512, 1050)] {
      draw_annotations(
        damage.dc,
        drawings.iter(),
        (0, 0),
        RECT {
          left,
          top: 0,
          right,
          bottom: 700,
        },
      )
      .expect("damage vectors");
    }
    assert_eq!(damage.to_rgba().expect("damage pixels"), preview);
  }

  #[test]
  fn multiply_scales_the_backdrop_by_coverage_and_keeps_its_alpha() {
    // One row of four pixels in a stride of five: none, full, half and full-black coverage.
    let mut stroke = vec![0u8; 5 * 4];
    stroke[4..8].copy_from_slice(&[21, 204, 250, 255]);
    stroke[8..12].copy_from_slice(&[10, 102, 125, 128]);
    stroke[12..16].copy_from_slice(&[0, 0, 0, 255]);
    let mut backdrop = [200u8, 200, 200, 255].repeat(5);
    backdrop[16..20].copy_from_slice(&[9, 9, 9, 7]);
    multiply_rows(&stroke, &mut backdrop, 5, 4, 1).expect("multiply");
    assert_eq!(
      &backdrop[0..4],
      &[200, 200, 200, 255],
      "no coverage, no change"
    );
    assert_eq!(
      &backdrop[4..8],
      &[16, 160, 196, 255],
      "full coverage multiplies"
    );
    assert_eq!(
      &backdrop[8..12],
      &[107, 180, 198, 255],
      "half coverage mixes"
    );
    assert_eq!(&backdrop[12..16], &[0, 0, 0, 255], "black ink stays black");
    assert_eq!(
      &backdrop[16..20],
      &[9, 9, 9, 7],
      "pixels past the width are left alone"
    );
    assert!(multiply_rows(&stroke, &mut backdrop, 5, 6, 1).is_err());
    assert!(multiply_rows(&stroke, &mut backdrop, 5, 4, 2).is_err());
  }

  #[test]
  fn highlighter_keeps_text_dark_and_never_darkens_its_own_crossings() {
    // White page with a black "text" bar across x 40..80.
    let (width, height) = (400usize, 120usize);
    let mut source = vec![255u8; width * height * 4];
    for y in 0..height {
      for x in 40..80 {
        source[(y * width + x) * 4..(y * width + x) * 4 + 3].copy_from_slice(&[0, 0, 0]);
      }
    }
    let yellow = [250, 204, 21];
    let drawing = Annotation {
      id: 1,
      tool: Tool::Highlighter,
      label: None,
      color: yellow,
      width: 6,
      // Across, then back down through itself at (200, 50).
      points: vec![(20, 50), (380, 50), (200, 20), (200, 100)],
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    };
    assert_eq!(drawing.stroke_width(), 18);
    let selection = crate::overlay::selection::Selection {
      x: 0,
      y: 0,
      width: width as u32,
      height: height as u32,
      source_app: None,
    };
    let output = super::super::annotations::composite(
      source.clone(),
      &selection,
      std::slice::from_ref(&drawing),
    )
    .expect("export");
    let pixel = |x: usize, y: usize| &output[(y * width + x) * 4..(y * width + x) * 4 + 4];
    assert_eq!(
      pixel(120, 50),
      &[250, 204, 21, 255],
      "white turns the ink color"
    );
    assert_eq!(pixel(200, 50), pixel(120, 50), "a crossing is no darker");
    assert_eq!(pixel(200, 30), pixel(120, 50), "the second segment matches");
    assert_eq!(pixel(60, 50), &[0, 0, 0, 255], "text under it stays black");
    assert_eq!(
      pixel(120, 57),
      &[250, 204, 21, 255],
      "three times the pen's width"
    );
    assert_eq!(pixel(120, 62), &[255; 4], "nothing past the stroke");
    assert!(output.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    let bounds = drawing.paint_bounds().expect("bounds");
    for (index, (before, after)) in source
      .as_chunks::<4>()
      .0
      .iter()
      .zip(output.as_chunks::<4>().0)
      .enumerate()
    {
      if before != after {
        let (x, y) = ((index % width) as i32, (index / width) as i32);
        assert!(
          x >= bounds.left && x < bounds.right && y >= bounds.top && y < bounds.bottom,
          "highlighter paints outside its bounds at ({x}, {y})"
        );
      }
    }
    let mut preview =
      ScreenshotBitmap::from_rgba(&source, width as u32, height as u32).expect("preview surface");
    draw_annotations(
      preview.dc,
      std::iter::once(&drawing),
      (0, 0),
      RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
      },
    )
    .expect("preview");
    assert_eq!(preview.to_rgba().expect("preview pixels"), output);
    assert!(
      drawing.hit((120.0, 58.0)),
      "its whole thickness can be grabbed"
    );
    assert!(!drawing.hit((120.0, 66.0)));
  }

  #[test]
  fn label_layouts_are_reused_with_bounded_formats_and_markers_have_contrast() {
    let mut renderer = Renderer::new().expect("renderer");
    let mut bitmap =
      ScreenshotBitmap::from_rgba(&vec![200; 120 * 60 * 4], 120, 60).expect("marker surface");
    for (index, (number, color)) in [(1, [20, 20, 23]), (99, [250, 204, 21]), (999, [20, 20, 23])]
      .into_iter()
      .enumerate()
    {
      let annotation = Annotation {
        id: index as u64 + 1,
        tool: Tool::Marker,
        label: Some(Label::Number(number)),
        points: vec![(20 + index as i32 * 40, 30)],
        color,
        width: 3,
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
      };
      renderer
        .draw(
          bitmap.dc,
          &annotation,
          (0, 0),
          RECT {
            left: 0,
            top: 0,
            right: 120,
            bottom: 60,
          },
        )
        .expect("marker");
    }
    assert_eq!(renderer.formats.len(), 1, "same marker font is reused");
    let pixels = bitmap.to_rgba().expect("marker pixels");
    for column in 0..3 {
      let ink: Vec<_> = (18..42)
        .flat_map(|y| (column * 40 + 8..column * 40 + 32).map(move |x| (y * 120 + x) * 4))
        .map(|offset| pixels[offset])
        .collect();
      assert!(ink.iter().any(|value| *value < 30), "dark marker or number");
      assert!(
        ink.iter().any(|value| *value > 230),
        "light marker or number"
      );
    }
  }

  #[test]
  #[ignore = "exports production annotation label previews"]
  fn render_label_previews() {
    let output =
      std::env::var_os("FLINCH_VECTOR_PREVIEW_OUTPUT").expect("preview output directory");
    let output = std::path::PathBuf::from(output);
    std::fs::create_dir_all(&output).expect("preview directory");
    let source: Vec<_> = (0..1050 * 1000)
      .flat_map(|index| {
        if (index / 1050 / 40 + index % 1050 / 40) % 2 == 0 {
          [35, 45, 55, 255]
        } else {
          [220, 220, 220, 255]
        }
      })
      .collect();
    let mut drawings = Vec::new();
    for (row, appearance) in [
      TextAppearance::Plain,
      TextAppearance::Outline,
      TextAppearance::Background,
    ]
    .into_iter()
    .enumerate()
    {
      for (column, size) in [16, 32, 48].into_iter().enumerate() {
        let mut data = super::super::annotations::text_data(
          "Café 日本語 🙂\nWrapped text مرحبا".into(),
          size,
          300,
        )
        .expect("text");
        data.appearance = appearance;
        drawings.push(Annotation {
          id: drawings.len() as u64 + 1,
          tool: Tool::Text,
          label: Some(Label::Text(data)),
          points: vec![(15 + column as i32 * 350, 15 + row as i32 * 300)],
          color: [255; 3],
          width: 3,
          corner_radius: 0,
          rotation_millidegrees: 0,
          path_transform: crate::screenshot::path::IDENTITY,
          curve_controls: Vec::new(),
          pen_nodes: Vec::new(),
        });
      }
    }
    for (index, (number, color)) in [
      (1, [239, 68, 68]),
      (99, [250, 204, 21]),
      (999, [20, 20, 23]),
    ]
    .into_iter()
    .enumerate()
    {
      drawings.push(Annotation {
        id: drawings.len() as u64 + 1,
        tool: Tool::Marker,
        label: Some(Label::Number(number)),
        points: vec![(175 + index as i32 * 350, 950)],
        color,
        width: 3,
        corner_radius: 0,
        rotation_millidegrees: 0,
        path_transform: crate::screenshot::path::IDENTITY,
        curve_controls: Vec::new(),
        pen_nodes: Vec::new(),
      });
    }
    let mut bitmap = ScreenshotBitmap::from_rgba(&source, 1050, 1000).expect("label preview");
    draw_annotations(
      bitmap.dc,
      drawings.iter(),
      (0, 0),
      RECT {
        left: 0,
        top: 0,
        right: 1050,
        bottom: 1000,
      },
    )
    .expect("label render");
    image::save_buffer(
      output.join("drawing-labels.png"),
      &bitmap.to_rgba().expect("preview pixels"),
      1050,
      1000,
      image::ColorType::Rgba8,
    )
    .expect("label PNG");
  }

  #[test]
  #[ignore = "exports production annotation vector previews"]
  fn render_vector_previews() {
    let output =
      std::env::var_os("FLINCH_VECTOR_PREVIEW_OUTPUT").expect("preview output directory");
    let output = std::path::PathBuf::from(output);
    std::fs::create_dir_all(&output).expect("preview directory");
    let source = vec![255; 1050 * 700 * 4];
    let mut bitmap = ScreenshotBitmap::from_rgba(&source, 1050, 700).expect("preview bitmap");
    let drawings = samples();
    draw_annotations(
      bitmap.dc,
      drawings.iter(),
      (0, 0),
      RECT {
        left: 0,
        top: 0,
        right: 1050,
        bottom: 700,
      },
    )
    .expect("preview render");
    image::save_buffer(
      output.join("drawing-vectors.png"),
      &bitmap.to_rgba().expect("preview pixels"),
      1050,
      700,
      image::ColorType::Rgba8,
    )
    .expect("preview PNG");
    // Highlighter strokes over dark text on white and over a dark band, in every width.
    let (width, height) = (1050, 330);
    let mut page = vec![255u8; width * height * 4];
    for y in 250..height {
      for pixel in page[y * width * 4..(y + 1) * width * 4].chunks_mut(4) {
        pixel[..3].copy_from_slice(&[32, 34, 40]);
      }
    }
    let line = |id: u64, y: i32, content: &str, color: [u8; 3]| Annotation {
      id,
      tool: Tool::Text,
      label: Some(Label::Text(
        super::super::annotations::text_data(content.into(), 24, 1000).expect("text"),
      )),
      points: vec![(30, y)],
      color,
      width: 3,
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    };
    let stroke = |id: u64, color: [u8; 3], width: i32, points: Vec<(i32, i32)>| Annotation {
      id,
      tool: Tool::Highlighter,
      label: None,
      color,
      width,
      points,
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    };
    let drawings = vec![
      line(
        1,
        20,
        "Highlighter at 9 px over dark text stays readable",
        [20, 20, 23],
      ),
      line(
        2,
        80,
        "Highlighter at 18 px over dark text stays readable",
        [20, 20, 23],
      ),
      line(
        3,
        150,
        "Highlighter at 30 px crossing itself keeps one color",
        [20, 20, 23],
      ),
      line(4, 270, "Light text on a dark band", [235, 235, 235]),
      stroke(5, [250, 204, 21], 3, vec![(30, 44), (330, 44)]),
      stroke(6, [34, 197, 94], 6, vec![(30, 96), (420, 100), (640, 94)]),
      stroke(
        7,
        [236, 72, 153],
        10,
        vec![(30, 166), (700, 166), (560, 120), (560, 230)],
      ),
      stroke(8, [250, 204, 21], 10, vec![(30, 286), (420, 286)]),
      stroke(
        9,
        [59, 130, 246],
        6,
        vec![(720, 40), (1000, 120), (760, 220), (1010, 300)],
      ),
    ];
    let output_pixels = super::super::annotations::composite(
      page,
      &crate::overlay::selection::Selection {
        x: 0,
        y: 0,
        width: width as u32,
        height: height as u32,
        source_app: None,
      },
      &drawings,
    )
    .expect("highlighter preview");
    image::save_buffer(
      output.join("highlighter.png"),
      &output_pixels,
      width as u32,
      height as u32,
      image::ColorType::Rgba8,
    )
    .expect("highlighter PNG");
  }
}
