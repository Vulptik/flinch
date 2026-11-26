//! Direct2D/DirectWrite control chrome, owned by the existing overlay thread.
//! Draw into a bounded transparent bitmap, then composite through the existing DC.
//! Only the drawing quality changes; presentation stays on GDI.

use anyhow::{Context, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
  AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BLENDFUNCTION, GdiFlush, HDC,
};
use windows::core::w;
use windows_numerics::Matrix3x2;

use super::gdi::layered::LayeredBitmap;
pub use super::icons::Icon;

pub const LABEL_SIZE: f32 = 12.0;
/// Corner radius of every overlay panel: toolbars, popovers, the magnifier and notifications.
pub const PANEL_RADIUS: f32 = 14.0;

/// Corner radius for a button, highlight or image inset `gap` logical pixels inside a panel.
/// Its corners stay concentric with the panel's, so the space between them is even all the way
/// around the curve instead of pinching or flaring where a button sits near the edge.
pub fn inner_radius(gap: f32) -> f32 {
  (PANEL_RADIUS - gap).max(0.0)
}

#[derive(Clone, Copy, PartialEq)]
pub struct TextStyle {
  pub size: f32,
  pub semibold: bool,
  pub leading: bool,
}

impl TextStyle {
  pub const LABEL: Self = Self {
    size: LABEL_SIZE,
    semibold: false,
    leading: false,
  };
  pub const DIMENSIONS: Self = Self {
    size: 18.0,
    semibold: true,
    leading: false,
  };
  pub const COORDINATES: Self = Self {
    size: 13.0,
    semibold: false,
    leading: false,
  };
  pub const TITLE: Self = Self {
    size: 15.0,
    semibold: true,
    leading: true,
  };
  pub const DETAILS: Self = Self {
    size: 13.0,
    semibold: false,
    leading: true,
  };

  /// The same style at a monitor's UI scale, as a scaled scene records it.
  pub fn scaled(self, scale: f32) -> Self {
    Self {
      size: self.size * scale,
      ..self
    }
  }
}
const MAX_SURFACE_BYTES: i64 = 16 * 1024 * 1024;
const MAX_UI_SCALE: f32 = 5.0;
const SHADOW_LAYERS: u8 = 8;
const SHADOW_LAYER_ALPHA: f32 = 7.0 / 255.0;
/// Soft shadow spread at 100% scaling; callers reserve it around elevated panels.
pub const SHADOW_SPREAD: f32 = 10.0;

enum Command {
  Outline(RECT, f32, [u8; 3], u8),
  Rounded(RECT, f32, [u8; 3], u8),
  Fill(RECT, [u8; 3]),
  Text(RECT, Vec<u16>, TextStyle, [u8; 3]),
  /// Icon centered in the rect at its design size, or at an explicit design size.
  Icon(RECT, Icon, [u8; 3], Option<f32>),
  Shadow(RECT, f32, f32),
}

/// Positions are physical pixels. A scaled scene enlarges only intrinsic sizes it owns:
/// label text, icons and the shared panel radius. Callers scale their own geometry.
pub struct Scene {
  commands: Vec<Command>,
  scale: f32,
}

impl Default for Scene {
  fn default() -> Self {
    Self {
      commands: Vec::new(),
      scale: 1.0,
    }
  }
}

impl Scene {
  pub fn scaled(scale: f32) -> Result<Self> {
    anyhow::ensure!(
      scale.is_finite() && (1.0..=MAX_UI_SCALE).contains(&scale),
      "Invalid chrome UI scale {scale}"
    );
    Ok(Self {
      commands: Vec::new(),
      scale,
    })
  }

  pub fn scale(&self) -> f32 {
    self.scale
  }

  /// A soft drop shadow extending `SHADOW_SPREAD * scale` beyond `rect`, mostly downward.
  pub fn shadow(&mut self, rect: RECT, radius: f32) {
    self
      .commands
      .push(Command::Shadow(rect, radius, SHADOW_SPREAD * self.scale));
  }

  pub fn outline(&mut self, rect: RECT, radius: f32, color: [u8; 3], alpha: u8) {
    self
      .commands
      .push(Command::Outline(rect, radius, color, alpha));
  }
  pub fn panel(&mut self, rect: RECT) -> Result<()> {
    let radius = PANEL_RADIUS * self.scale;
    self.rounded(rect, radius, super::theme::OUTLINE, 255)?;
    self.rounded(
      RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
      },
      radius - 1.0,
      super::theme::SURFACE,
      255,
    )
  }
  pub fn rounded(&mut self, rect: RECT, radius: f32, color: [u8; 3], alpha: u8) -> Result<()> {
    self
      .commands
      .push(Command::Rounded(rect, radius, color, alpha));
    Ok(())
  }

  pub fn fill(&mut self, rect: RECT, color: [u8; 3]) -> Result<()> {
    self.commands.push(Command::Fill(rect, color));
    Ok(())
  }

  pub fn text(&mut self, rect: RECT, label: &str, color: [u8; 3]) -> Result<()> {
    self.styled_text(rect, label, TextStyle::LABEL, color)
  }

  pub fn styled_text(
    &mut self,
    rect: RECT,
    label: &str,
    style: TextStyle,
    color: [u8; 3],
  ) -> Result<()> {
    self.push_text(rect, label, style.scaled(self.scale), color)
  }

  fn push_text(&mut self, rect: RECT, label: &str, style: TextStyle, color: [u8; 3]) -> Result<()> {
    anyhow::ensure!(
      label.len() <= 8192 && style.size.is_finite() && style.size > 0.0 && style.size <= 128.0,
      "Invalid chrome label or font size"
    );
    self.commands.push(Command::Text(
      rect,
      label.encode_utf16().collect(),
      style,
      color,
    ));
    Ok(())
  }

  /// An actual-size sample in capture pixels, so it never takes the UI scale.
  pub fn font_sample(&mut self, rect: RECT, size: i32) -> Result<()> {
    self.push_text(
      rect,
      "Aa",
      TextStyle {
        size: size as f32,
        ..TextStyle::LABEL
      },
      super::theme::TEXT,
    )
  }

  /// Draws `icon` at its design size (its view box, times the scene scale), centered in `rect`.
  pub fn icon(&mut self, rect: RECT, icon: Icon, color: [u8; 3]) -> Result<()> {
    self.commands.push(Command::Icon(rect, icon, color, None));
    Ok(())
  }

  /// Draws `icon` at `size` design pixels instead of its own, centered in `rect`.
  pub fn icon_sized(&mut self, rect: RECT, icon: Icon, color: [u8; 3], size: f32) -> Result<()> {
    anyhow::ensure!(
      size.is_finite() && size > 0.0 && size <= 64.0,
      "Invalid icon size"
    );
    self
      .commands
      .push(Command::Icon(rect, icon, color, Some(size)));
    Ok(())
  }
}

#[derive(Default)]
pub struct Renderer {
  backend: Option<Backend>,
  bitmap: Option<LayeredBitmap>,
  pub surface_misses: u64,
  pub icon_misses: u64,
}

struct Backend {
  factory: ID2D1Factory,
  write: IDWriteFactory,
  target: ID2D1DCRenderTarget,
  brush: ID2D1SolidColorBrush,
  formats: Vec<(TextStyle, IDWriteTextFormat)>,
  icons: Vec<(Icon, ID2D1PathGeometry, f32)>,
}

/// One renderer setup made ahead of time. Overlays run on a fresh thread each time, and a new
/// Direct2D DC render target plus its first text draw cost about 3 ms (measured),
/// which landed on the first frame of whichever chrome drew first (the Color Picker's lens).
/// A spare built off the overlay path is taken instantly and a replacement is built behind it,
/// like the standby recording worker.
struct Spare(Backend);
// SAFETY: a single-threaded Direct2D factory and the objects it creates have no thread
// affinity; they only lack internal locking. A spare is built on the warm-up thread, then moved
// to and used by exactly one overlay thread, never by two threads at once.
unsafe impl Send for Spare {}

/// The app's spare. Inactive until [`warm_up`], so tests and tools build their own renderers.
static SPARES: SparePool = SparePool::new();

struct SparePool {
  slot: std::sync::Mutex<Option<Spare>>,
  /// At most one replacement is being built at a time.
  refilling: std::sync::atomic::AtomicBool,
  active: std::sync::atomic::AtomicBool,
}

impl SparePool {
  const fn new() -> Self {
    Self {
      slot: std::sync::Mutex::new(None),
      refilling: std::sync::atomic::AtomicBool::new(false),
      active: std::sync::atomic::AtomicBool::new(false),
    }
  }

  fn activate(&'static self) -> Option<std::thread::JoinHandle<()>> {
    self
      .active
      .store(true, std::sync::atomic::Ordering::Release);
    self.refill()
  }

  /// Starts building a replacement unless the pool is inactive or one is being built. Each take
  /// starts at most one, so a failing build never retries in a loop; the next take tries again.
  fn refill(&'static self) -> Option<std::thread::JoinHandle<()>> {
    use std::sync::atomic::Ordering;
    if !self.active.load(Ordering::Acquire) || self.refilling.swap(true, Ordering::AcqRel) {
      return None;
    }
    let spawned = std::thread::Builder::new()
      .name("flinch-chrome-warmup".into())
      .spawn(move || {
        match Backend::warmed() {
          Ok(backend) => {
            let mut slot = self
              .slot
              .lock()
              .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.get_or_insert(Spare(backend));
          },
          Err(error) => tracing::warn!(
            error = %format!("{error:#}"),
            "Spare chrome renderer failed; the next overlay builds its own"
          ),
        }
        self.refilling.store(false, Ordering::Release);
      });
    match spawned {
      Ok(handle) => Some(handle),
      Err(error) => {
        self.refilling.store(false, Ordering::Release);
        tracing::warn!(%error, "Spare chrome renderer thread failed to start");
        None
      },
    }
  }

  /// The spare, if one is ready; taking it starts building the next.
  fn take(&'static self) -> (Option<Backend>, Option<std::thread::JoinHandle<()>>) {
    let spare = self
      .slot
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .take();
    (spare.map(|spare| spare.0), self.refill())
  }
}

/// Builds the first spare renderer at launch, off the overlay path. Idempotent.
pub fn warm_up() {
  drop(SPARES.activate());
}

impl Backend {
  /// The ready spare, or a new setup when none is ready yet: the same renderer either way.
  fn acquire() -> Result<Self> {
    match SPARES.take().0 {
      Some(backend) => Ok(backend),
      None => Self::new(),
    }
  }

  /// A setup whose first text draw is already done, so its target's caches are primed.
  fn warmed() -> Result<Self> {
    let mut renderer = Renderer {
      backend: Some(Self::new()?),
      ..Renderer::default()
    };
    let bounds = RECT {
      left: 0,
      top: 0,
      right: 160,
      bottom: 24,
    };
    let surface = LayeredBitmap::new(bounds.right, bounds.bottom).map_err(anyhow::Error::msg)?;
    let mut scene = Scene::default();
    scene.panel(bounds)?;
    scene.styled_text(
      bounds,
      "#0123456789ABCDEF",
      TextStyle::DETAILS,
      super::theme::TEXT,
    )?;
    renderer.render(surface.dc, bounds, &scene)?;
    renderer
      .backend
      .take()
      .context("Warmed chrome renderer lost")
  }

  fn new() -> Result<Self> {
    // SAFETY: factories and their resources are created and used only by the overlay thread.
    unsafe {
      let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
      let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
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
      // Grayscale edges remain valid when the transparent surface is composited.
      target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
      let brush = target.CreateSolidColorBrush(&color([255; 3], 255), None)?;
      Ok(Self {
        factory,
        write,
        target,
        brush,
        formats: Vec::new(),
        icons: Vec::new(),
      })
    }
  }

  fn prepare(&mut self, scene: &Scene) -> Result<u64> {
    let mut new_icons = 0;
    for command in &scene.commands {
      match command {
        Command::Text(_, _, style, _)
          if !self.formats.iter().any(|(cached, _)| cached == style) =>
        {
          anyhow::ensure!(self.formats.len() < 8, "Chrome font cache limit reached");
          // SAFETY: valid factory and static UTF-16 names; format stays on the owning thread.
          let format = unsafe {
            let format = self.write.CreateTextFormat(
              w!("Segoe UI"),
              None,
              if style.semibold {
                DWRITE_FONT_WEIGHT_SEMI_BOLD
              } else {
                DWRITE_FONT_WEIGHT_NORMAL
              },
              DWRITE_FONT_STYLE_NORMAL,
              DWRITE_FONT_STRETCH_NORMAL,
              style.size,
              w!(""),
            )?;
            format.SetTextAlignment(if style.leading {
              DWRITE_TEXT_ALIGNMENT_LEADING
            } else {
              DWRITE_TEXT_ALIGNMENT_CENTER
            })?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let ellipsis = self.write.CreateEllipsisTrimmingSign(&format)?;
            format.SetTrimming(
              &DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                ..Default::default()
              },
              &ellipsis,
            )?;
            format
          };
          self.formats.push((*style, format));
        },
        Command::Icon(_, icon, _, _) if !self.icons.iter().any(|(cached, ..)| cached == icon) => {
          // Bounded by the fixed set of vendored icons.
          anyhow::ensure!(
            self.icons.len() < Icon::ALL.len(),
            "Chrome icon cache limit reached"
          );
          let (geometry, size) = super::icons::geometry(&self.factory, *icon)?;
          self.icons.push((*icon, geometry, size));
          new_icons += 1;
        },
        _ => {},
      }
    }
    Ok(new_icons)
  }
}

impl Renderer {
  pub fn measure_text(&mut self, label: &str, style: TextStyle) -> Result<f32> {
    Ok(self.measure_text_box(label, style, 32768.0)?.0)
  }

  pub fn measure_text_box(
    &mut self,
    label: &str,
    style: TextStyle,
    width: f32,
  ) -> Result<(f32, f32)> {
    anyhow::ensure!(
      width.is_finite() && width > 0.0,
      "Invalid chrome text width"
    );
    let mut scene = Scene::default();
    scene.styled_text(RECT::default(), label, style, [255; 3])?;
    if self.backend.is_none() {
      self.backend = Some(Backend::acquire().context("Initialize chrome text measurement")?);
    }
    let backend = self
      .backend
      .as_mut()
      .context("Chrome renderer unavailable")?;
    backend.prepare(&scene)?;
    let (_, format) = backend
      .formats
      .iter()
      .find(|(cached, _)| *cached == style)
      .context("Chrome measurement format missing")?;
    let wide: Vec<u16> = label.encode_utf16().collect();
    // SAFETY: thread-owned factory/format and bounded UTF-16 input; layout does not outlive them.
    unsafe {
      let layout = backend
        .write
        .CreateTextLayout(&wide, format, width, 256.0)?;
      let mut metrics = DWRITE_TEXT_METRICS::default();
      layout.GetMetrics(&mut metrics)?;
      Ok((metrics.widthIncludingTrailingWhitespace, metrics.height))
    }
  }
  pub fn render(&mut self, dc: HDC, bounds: RECT, scene: &Scene) -> Result<()> {
    self.render_with_opacity(dc, bounds, scene, 255)
  }

  pub fn render_with_opacity(
    &mut self,
    dc: HDC,
    bounds: RECT,
    scene: &Scene,
    opacity: u8,
  ) -> Result<()> {
    self.render_with_offset(dc, bounds, scene, opacity, 0)
  }

  pub fn render_with_offset(
    &mut self,
    dc: HDC,
    bounds: RECT,
    scene: &Scene,
    opacity: u8,
    offset_y: i32,
  ) -> Result<()> {
    anyhow::ensure!(
      scene.commands.len() <= 256,
      "Chrome scene command limit reached"
    );
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    anyhow::ensure!(
      width > 0 && height > 0 && i64::from(width) * i64::from(height) * 4 <= MAX_SURFACE_BYTES,
      "Chrome surface exceeds its 16 MiB budget"
    );
    if self.backend.is_none() {
      self.backend = Some(Backend::acquire().context("Initialize Direct2D/DirectWrite chrome")?);
    }
    if self
      .bitmap
      .as_ref()
      .is_none_or(|bitmap| bitmap.width != width || bitmap.height != height)
    {
      self.bitmap = Some(LayeredBitmap::new(width, height).map_err(anyhow::Error::msg)?);
      self.surface_misses = self.surface_misses.saturating_add(1);
    }
    let backend = self
      .backend
      .as_mut()
      .context("Chrome renderer unavailable")?;
    self.icon_misses = self.icon_misses.saturating_add(backend.prepare(scene)?);
    let bitmap = self.bitmap.as_ref().context("Chrome bitmap unavailable")?;
    let local = RECT {
      left: 0,
      top: 0,
      right: width,
      bottom: height,
    };
    let transform = translation(-bounds.left as f32, -bounds.top as f32);
    // SAFETY: the bitmap DC and D2D resources are live and thread-owned. Preparation above
    // completes all fallible allocations before BeginDraw; EndDraw always completes the batch.
    let result: Result<()> = unsafe {
      backend.target.BindDC(bitmap.dc, &local)?;
      backend.target.BeginDraw();
      backend.target.Clear(Some(&color([0; 3], 0)));
      backend.target.SetTransform(&transform);
      let drawing = (|| -> Result<()> {
        for command in &scene.commands {
          match command {
            Command::Outline(rect, radius, rgb, alpha) => {
              backend.brush.SetColor(&color(*rgb, *alpha));
              let mut bounds = float_rect(*rect);
              bounds.left += 0.5;
              bounds.top += 0.5;
              bounds.right -= 0.5;
              bounds.bottom -= 0.5;
              backend.target.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                  rect: bounds,
                  radiusX: *radius,
                  radiusY: *radius,
                },
                &backend.brush,
                1.0,
                None,
              );
            },
            Command::Rounded(rect, radius, rgb, alpha) => {
              backend.brush.SetColor(&color(*rgb, *alpha));
              backend.target.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                  rect: float_rect(*rect),
                  radiusX: *radius,
                  radiusY: *radius,
                },
                &backend.brush,
              );
            },
            Command::Fill(rect, rgb) => {
              backend.brush.SetColor(&color(*rgb, 255));
              backend
                .target
                .FillRectangle(&float_rect(*rect), &backend.brush);
            },
            Command::Shadow(rect, radius, spread) => {
              // Stacked translucent layers approximate a blur without a device-context effect.
              backend.brush.SetColor(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: SHADOW_LAYER_ALPHA,
              });
              for layer in 1..=SHADOW_LAYERS {
                backend.target.FillRoundedRectangle(
                  &shadow_layer(*rect, *radius, *spread, layer),
                  &backend.brush,
                );
              }
            },
            Command::Text(rect, label, style, rgb) => {
              let (_, format) = backend
                .formats
                .iter()
                .find(|(cached, _)| cached == style)
                .context("Prepared chrome font missing")?;
              backend.brush.SetColor(&color(*rgb, 255));
              backend.target.DrawText(
                label,
                format,
                &float_rect(*rect),
                &backend.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
              );
            },
            Command::Icon(rect, icon, rgb, size) => {
              let (_, geometry, view) = backend
                .icons
                .iter()
                .find(|(cached, ..)| cached == icon)
                .context("Prepared chrome icon missing")?;
              let drawn = size.unwrap_or(*view) * scene.scale;
              let origin = icon_origin(*rect, drawn);
              let factor = drawn / view;
              backend.target.SetTransform(&Matrix3x2 {
                M11: factor,
                M22: factor,
                ..translation(origin.0 - bounds.left as f32, origin.1 - bounds.top as f32)
              });
              backend.brush.SetColor(&color(*rgb, 255));
              backend.target.FillGeometry(geometry, &backend.brush, None);
              backend.target.SetTransform(&transform);
            },
          }
        }
        Ok(())
      })();
      let finished = backend
        .target
        .EndDraw(None, None)
        .map_err(anyhow::Error::from);
      drawing.and(finished)
    };
    if let Err(error) = result {
      // Explicit failure, no GDI fallback and no retry loop. Recreate resources on next paint.
      self.backend = None;
      return Err(error).context("Render Direct2D chrome");
    }
    // SAFETY: valid source/destination DCs and bounded premultiplied BGRA surface.
    anyhow::ensure!(
      unsafe {
        AlphaBlend(
          dc,
          bounds.left,
          bounds.top + offset_y,
          width,
          height,
          bitmap.dc,
          0,
          0,
          width,
          height,
          BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: opacity,
            AlphaFormat: AC_SRC_ALPHA as u8,
          },
        )
      }
      .as_bool(),
      "Composite Direct2D chrome failed"
    );
    // SAFETY: synchronize this thread's GDI batch before raw DIB reads or layered-window presentation.
    anyhow::ensure!(
      unsafe { GdiFlush() }.as_bool(),
      "Flush Direct2D chrome composition failed"
    );
    Ok(())
  }
}

fn translation(x: f32, y: f32) -> Matrix3x2 {
  Matrix3x2 {
    M11: 1.0,
    M12: 0.0,
    M21: 0.0,
    M22: 1.0,
    M31: x,
    M32: y,
  }
}

fn color(rgb: [u8; 3], alpha: u8) -> D2D1_COLOR_F {
  D2D1_COLOR_F {
    r: rgb[0] as f32 / 255.0,
    g: rgb[1] as f32 / 255.0,
    b: rgb[2] as f32 / 255.0,
    a: alpha as f32 / 255.0,
  }
}

fn float_rect(rect: RECT) -> D2D_RECT_F {
  D2D_RECT_F {
    left: rect.left as f32,
    top: rect.top as f32,
    right: rect.right as f32,
    bottom: rect.bottom as f32,
  }
}

/// Top-left of an icon box of `size` pixels centered in `rect`.
fn icon_origin(rect: RECT, size: f32) -> (f32, f32) {
  (
    (rect.left + rect.right) as f32 / 2.0 - size / 2.0,
    (rect.top + rect.bottom) as f32 / 2.0 - size / 2.0,
  )
}

/// One shadow ring: each layer grows by an equal share of the spread and sits lower,
/// so overlapping layers darken toward the panel edge and below it.
fn shadow_layer(rect: RECT, radius: f32, spread: f32, layer: u8) -> D2D1_ROUNDED_RECT {
  let grow = spread * f32::from(layer) / f32::from(SHADOW_LAYERS);
  let drop = spread * 0.3;
  let rect = float_rect(rect);
  D2D1_ROUNDED_RECT {
    rect: D2D_RECT_F {
      left: rect.left - grow,
      top: rect.top - grow + drop,
      right: rect.right + grow,
      bottom: rect.bottom + grow + drop,
    },
    radiusX: radius + grow,
    radiusY: radius + grow,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::Graphics::Gdi::GdiFlush;

  #[test]
  fn the_spare_pool_stays_off_until_activated_then_hands_out_one_and_refills_once() {
    static POOL: SparePool = SparePool::new();
    let (spare, refill) = POOL.take();
    assert!(
      spare.is_none() && refill.is_none(),
      "inactive pools build nothing"
    );
    POOL.activate().expect("first build").join().expect("built");
    let (spare, refill) = POOL.take();
    assert!(spare.is_some(), "the warmed spare");
    // A second take while the replacement is still being built neither waits nor builds twice.
    POOL
      .refilling
      .store(true, std::sync::atomic::Ordering::Release);
    assert!(POOL.refill().is_none(), "one build at a time");
    POOL
      .refilling
      .store(false, std::sync::atomic::Ordering::Release);
    refill.expect("replacement").join().expect("rebuilt");
    assert!(
      POOL.slot.lock().expect("slot").as_ref().is_some(),
      "the next spare is ready"
    );
    assert!(POOL.refill().expect("refill").join().is_ok());
    let held = POOL.slot.lock().expect("slot").take();
    assert!(held.is_some(), "a ready spare is kept, not replaced twice");
  }

  #[test]
  fn a_spare_built_on_another_thread_draws_exactly_like_a_fresh_renderer() {
    let bounds = RECT {
      left: 0,
      top: 0,
      right: 135,
      bottom: 155,
    };
    let mut scene = Scene::default();
    scene.panel(bounds).expect("panel");
    scene
      .rounded(
        RECT {
          left: 30,
          top: 135,
          right: 42,
          bottom: 147,
        },
        2.5,
        [217, 47, 47],
        255,
      )
      .expect("swatch");
    scene
      .styled_text(
        RECT {
          left: 48,
          top: 131,
          right: 120,
          bottom: 151,
        },
        "#D92F2F",
        TextStyle::DETAILS,
        super::super::theme::TEXT,
      )
      .expect("hex");
    let draw = |backend: Backend| {
      let mut renderer = Renderer {
        backend: Some(backend),
        ..Renderer::default()
      };
      let mut surface = LayeredBitmap::new(bounds.right, bounds.bottom).expect("surface");
      renderer.render(surface.dc, bounds, &scene).expect("drawn");
      // SAFETY: flush this thread's GDI batch before reading the DIB directly.
      unsafe { GdiFlush() }.ok().expect("flush");
      surface.pixels_mut().to_vec()
    };
    let spare = std::thread::spawn(|| Spare(Backend::warmed().expect("warmed")))
      .join()
      .expect("warm-up thread")
      .0;
    assert!(draw(spare) == draw(Backend::new().expect("fresh")));
  }

  /// Where a fresh renderer's setup goes, step by step, on a new thread as each overlay is.
  #[test]
  #[ignore = "timing probe; run explicitly"]
  fn backend_setup_cost_probe() {
    for round in 0..4 {
      std::thread::spawn(move || {
        let ms = |started: std::time::Instant| started.elapsed().as_secs_f64() * 1000.0;
        let started = std::time::Instant::now();
        let factory: ID2D1Factory =
          unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }.expect("factory");
        let d2d = ms(started);
        let started = std::time::Instant::now();
        let write: IDWriteFactory =
          unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.expect("write");
        let dwrite = ms(started);
        let started = std::time::Instant::now();
        let target = unsafe {
          factory.CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
              format: DXGI_FORMAT_B8G8R8A8_UNORM,
              alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
          })
        }
        .expect("target");
        let dc_target = ms(started);
        let started = std::time::Instant::now();
        let format = unsafe {
          write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            12.0,
            w!(""),
          )
        }
        .expect("format");
        let text_format = ms(started);
        let bitmap = LayeredBitmap::new(135, 155).expect("bitmap");
        let rect = RECT {
          left: 0,
          top: 0,
          right: 135,
          bottom: 155,
        };
        let started = std::time::Instant::now();
        unsafe {
          target.BindDC(bitmap.dc, &rect).expect("bind");
          target.BeginDraw();
          target.Clear(Some(&color([0; 3], 0)));
          let brush = target
            .CreateSolidColorBrush(&color([255; 3], 255), None)
            .expect("brush");
          target.FillRectangle(&float_rect(rect), &brush);
          let text: Vec<u16> = "#D92F2F".encode_utf16().collect();
          target.DrawText(
            &text,
            &format,
            &float_rect(rect),
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
          );
          target.EndDraw(None, None).expect("end");
        }
        let first_draw = ms(started);
        eprintln!(
          "round {round}: d2d factory {d2d:.3}, dwrite {dwrite:.3}, dc target {dc_target:.3}, text format {text_format:.3}, first draw {first_draw:.3} ms"
        );
      })
      .join()
      .expect("probe thread");
    }
  }

  #[test]
  fn inner_corners_stay_concentric_with_the_panel() {
    for gap in [4.0, 5.0, 6.0, 8.0] {
      assert_eq!(inner_radius(gap) + gap, PANEL_RADIUS);
    }
    assert_eq!(
      inner_radius(20.0),
      0.0,
      "deep insets are square, never negative"
    );
  }

  #[test]
  fn transparent_edges_are_premultiplied_and_resources_are_reused() {
    let bounds = RECT {
      left: 0,
      top: 0,
      right: 120,
      bottom: 46,
    };
    let mut scene = Scene::default();
    scene
      .rounded(bounds, 14.0, super::super::theme::SURFACE, 255)
      .expect("panel");
    scene
      .icon(
        RECT {
          left: 6,
          top: 6,
          right: 40,
          bottom: 40,
        },
        Icon::Pen,
        [240; 3],
      )
      .expect("icon");
    scene
      .text(
        RECT {
          left: 40,
          top: 6,
          right: 114,
          bottom: 40,
        },
        "24 px",
        [240; 3],
      )
      .expect("label");
    let destination = LayeredBitmap::new(120, 46).expect("destination");
    let mut renderer = Renderer::default();
    renderer
      .render(destination.dc, bounds, &scene)
      .expect("first paint");
    renderer
      .render(destination.dc, bounds, &scene)
      .expect("warm paint");
    assert_eq!(renderer.surface_misses, 1);
    assert_eq!(renderer.icon_misses, 1);
    assert_eq!(renderer.backend.as_ref().expect("backend").formats.len(), 1);
    // SAFETY: finish GDI presentation before inspecting the thread-owned source DIB.
    assert!(unsafe { GdiFlush() }.as_bool());
    let pixels = renderer.bitmap.as_mut().expect("surface").pixels_mut();
    assert_eq!(&pixels[..4], &[0, 0, 0, 0], "transparent panel corner");
    assert!(
      pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] > 0 && pixel[3] < 255)
    );
    assert!(
      pixels
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[..3].iter().all(|channel| *channel <= pixel[3]))
    );
    assert!(
      pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[0] > 100 && pixel[3] > 100),
      "visible icon/text"
    );
  }

  #[test]
  fn opacity_blends_without_changing_source_or_allocating_each_frame() {
    let bounds = RECT {
      left: 0,
      top: 0,
      right: 20,
      bottom: 20,
    };
    let mut scene = Scene::default();
    scene.fill(bounds, [200; 3]).expect("fill");
    let mut renderer = Renderer::default();
    for opacity in [0, 128, 255] {
      let mut destination = LayeredBitmap::new(20, 20).expect("destination");
      destination.pixels_mut().fill(0);
      renderer
        .render_with_opacity(destination.dc, bounds, &scene, opacity)
        .expect("paint");
      // SAFETY: flush this thread's GDI writes before reading its owned DIB.
      assert!(unsafe { GdiFlush() }.as_bool());
      let pixel = &destination.pixels_mut()[..4];
      let expected = (200.0 * f32::from(opacity) / 255.0).round() as u8;
      assert!(pixel[0].abs_diff(expected) <= 1);
      assert_eq!(pixel[3], opacity);
    }
    assert_eq!(renderer.surface_misses, 1);
  }

  #[test]
  fn surface_budget_is_checked_before_allocation() {
    let mut renderer = Renderer::default();
    assert!(
      renderer
        .render(
          HDC::default(),
          RECT {
            left: 0,
            top: 0,
            right: 8192,
            bottom: 8192
          },
          &Scene::default()
        )
        .is_err()
    );
    assert!(renderer.backend.is_none());
    assert!(renderer.bitmap.is_none());
  }

  #[test]
  fn icons_are_centered_in_actual_buttons_and_every_vendored_icon_builds() {
    let cell = RECT {
      left: 16,
      top: 32,
      right: 50,
      bottom: 66,
    };
    assert_eq!(icon_origin(cell, 20.0), (23.0, 39.0));
    assert_eq!(icon_origin(cell, 40.0), (13.0, 29.0));
    let factory = Backend::new().expect("backend").factory;
    for icon in Icon::ALL {
      let (_, size) = super::super::icons::geometry(&factory, *icon)
        .unwrap_or_else(|error| panic!("{}: {error}", icon.file()));
      assert!(size == 12.0 || size == 20.0, "{}", icon.file());
    }
    let mut scene = Scene::default();
    for invalid in [0.0, -1.0, 65.0, f32::NAN] {
      assert!(
        scene
          .icon_sized(cell, Icon::Pen, [255; 3], invalid)
          .is_err()
      );
    }
  }

  fn alpha_extent(renderer: &mut Renderer, width: usize) -> (usize, usize) {
    // SAFETY: finish GDI presentation before inspecting the thread-owned source DIB.
    assert!(unsafe { GdiFlush() }.as_bool());
    let pixels = renderer.bitmap.as_mut().expect("surface").pixels_mut();
    let columns: Vec<usize> = pixels
      .as_chunks::<4>()
      .0
      .iter()
      .enumerate()
      .filter(|(_, pixel)| pixel[3] > 0)
      .map(|(index, _)| index % width)
      .collect();
    (
      columns.iter().copied().min().expect("visible pixels"),
      columns.iter().copied().max().expect("visible pixels"),
    )
  }

  #[test]
  fn scaled_scenes_enlarge_icons_and_labels_but_not_actual_size_samples() {
    for invalid in [0.5, 5.5, f32::NAN, f32::INFINITY] {
      assert!(Scene::scaled(invalid).is_err(), "{invalid}");
    }
    let bounds = RECT {
      left: 0,
      top: 0,
      right: 80,
      bottom: 80,
    };
    let mut widths = Vec::new();
    for scale in [1.0, 2.0] {
      let mut scene = Scene::scaled(scale).expect("scale");
      scene
        .icon(bounds, Icon::RectangleSelected, [240; 3])
        .expect("icon");
      let destination = LayeredBitmap::new(80, 80).expect("destination");
      let mut renderer = Renderer::default();
      renderer
        .render(destination.dc, bounds, &scene)
        .expect("paint");
      let (left, right) = alpha_extent(&mut renderer, 80);
      widths.push(right - left + 1);
      assert!(
        (left + right) / 2 >= 38 && (left + right) / 2 <= 41,
        "centered"
      );
    }
    // The rectangle icon spans 16 units plus its stroke; doubling the scale doubles both.
    assert!(
      widths[1] >= widths[0] * 2 - 2 && widths[1] <= widths[0] * 2 + 2,
      "{widths:?}"
    );
    let mut scene = Scene::scaled(2.0).expect("scale");
    scene.text(bounds, "24", [240; 3]).expect("label");
    scene.font_sample(bounds, 24).expect("sample");
    let sizes: Vec<f32> = scene
      .commands
      .iter()
      .filter_map(|command| match command {
        Command::Text(_, _, style, _) => Some(style.size),
        _ => None,
      })
      .collect();
    assert_eq!(sizes, vec![LABEL_SIZE * 2.0, 24.0]);
    assert_eq!(
      TextStyle::DETAILS.scaled(1.5).size,
      TextStyle::DETAILS.size * 1.5
    );
  }

  #[test]
  fn shadow_darkens_only_within_its_spread_and_scales_with_the_scene() {
    for scale in [1.0, 2.0] {
      let panel = RECT {
        left: 40,
        top: 40,
        right: 120,
        bottom: 80,
      };
      let bounds = RECT {
        left: 0,
        top: 0,
        right: 160,
        bottom: 140,
      };
      let mut scene = Scene::scaled(scale).expect("scale");
      scene.shadow(panel, 14.0 * scale);
      scene
        .rounded(panel, 14.0 * scale, super::super::theme::SURFACE, 255)
        .expect("panel");
      let destination = LayeredBitmap::new(160, 140).expect("destination");
      let mut renderer = Renderer::default();
      renderer
        .render(destination.dc, bounds, &scene)
        .expect("paint");
      // SAFETY: finish GDI presentation before inspecting the thread-owned source DIB.
      assert!(unsafe { GdiFlush() }.as_bool());
      let pixels = renderer.bitmap.as_mut().expect("surface").pixels_mut();
      let alpha = |x: i32, y: i32| pixels[(y as usize * 160 + x as usize) * 4 + 3];
      let pixel = |x: i32, y: i32| &pixels[(y as usize * 160 + x as usize) * 4..][..3];
      let spread = (SHADOW_SPREAD * scale).ceil() as i32;
      let reach = spread + (spread * 3 + 9) / 10 + 1;
      assert!(
        alpha(80, panel.bottom + 2) > 0,
        "{scale}x shadow below the panel"
      );
      assert!(
        alpha(80, panel.bottom + 2) > alpha(80, panel.bottom + reach - 2),
        "{scale}x shadow fades outward"
      );
      assert_eq!(alpha(80, panel.bottom + reach), 0, "{scale}x shadow ends");
      assert_eq!(
        alpha(panel.left - spread - 1, 60),
        0,
        "{scale}x side extent"
      );
      assert!(
        pixel(80, panel.bottom + 2)
          .iter()
          .all(|channel| *channel == 0),
        "shadow is black"
      );
      assert_eq!(alpha(80, 60), 255, "{scale}x panel stays opaque");
    }
  }
}
