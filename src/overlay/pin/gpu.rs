//! Pin to Screen on the GPU: each pin window is a DirectComposition target, so zooming, fading and
//! moving a pin are compositor work, never a redraw of its pixels.
//!
//! A pin's visuals: a root (open/close fade and scale) holding its content (the user's opacity:
//! the outline and shadow in eight slices, then the image) and the zoom or opacity label. The
//! image surface is drawn once per zoom below 100% (high-quality downscaling keeps text legible)
//! and once at 100% for every zoom above it, which the compositor enlarges pixel for pixel.
//! The source image is split into bitmaps no larger than the device allows, with a small overlap
//! so downscaling reads real neighbours across each seam.
//!
//! Everything belongs to the pin host thread. One device draws every pin; when it is lost (driver
//! update, GPU reset), the host builds a new one and redraws each pin from its kept RGBA copy.

use windows::Win32::Foundation::{HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
  D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
  D2D1_ANTIALIAS_MODE_ALIASED, D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_PROPERTIES1,
  D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
  D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
  D2D1_UNIT_MODE_PIXELS, D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1,
};
use windows::Win32::Graphics::Direct3D::{
  D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
  D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_9_2, D3D_FEATURE_LEVEL_9_3, D3D_FEATURE_LEVEL_10_0,
  D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
  D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
  DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR,
  DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, DCompositionCreateDevice2,
  IDCompositionDesktopDevice, IDCompositionEffectGroup, IDCompositionSurface, IDCompositionTarget,
  IDCompositionVirtualSurface, IDCompositionVisual, IDCompositionVisual2,
};
use windows::Win32::Graphics::Dxgi::Common::{
  DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;
use windows_numerics::{Matrix3x2, Vector2};

use super::frame::{self, Area};

/// The largest region drawn into a surface at once: within every feature level's texture limit.
const SURFACE_TILE: i32 = 2048;
/// Source bitmaps stay at or below this side, whatever larger size the device allows.
const SOURCE_TILE: u32 = 4096;
/// Pixels each source bitmap reads beyond the part it draws, so seams never show.
const OVERLAP: u32 = 8;
const LEVELS: [D3D_FEATURE_LEVEL; 7] = [
  D3D_FEATURE_LEVEL_11_1,
  D3D_FEATURE_LEVEL_11_0,
  D3D_FEATURE_LEVEL_10_1,
  D3D_FEATURE_LEVEL_10_0,
  D3D_FEATURE_LEVEL_9_3,
  D3D_FEATURE_LEVEL_9_2,
  D3D_FEATURE_LEVEL_9_1,
];
const CLEAR: D2D1_COLOR_F = D2D1_COLOR_F {
  r: 0.0,
  g: 0.0,
  b: 0.0,
  a: 0.0,
};

fn failed(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
  move |error| format!("{what} failed: {error}")
}

fn bitmap_properties() -> D2D1_BITMAP_PROPERTIES1 {
  D2D1_BITMAP_PROPERTIES1 {
    pixelFormat: D2D1_PIXEL_FORMAT {
      format: DXGI_FORMAT_B8G8R8A8_UNORM,
      alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    },
    dpiX: 96.0,
    dpiY: 96.0,
    bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
    ..Default::default()
  }
}

fn rect_f(area: Area) -> D2D_RECT_F {
  D2D_RECT_F {
    left: area.x as f32,
    top: area.y as f32,
    right: (area.x + area.width) as f32,
    bottom: (area.y + area.height) as f32,
  }
}

/// A scale then a move, as one matrix (row vectors: x' = x * M11 + M31).
fn placed(scale: (f32, f32), at: (f32, f32)) -> Matrix3x2 {
  Matrix3x2 {
    M11: scale.0,
    M12: 0.0,
    M21: 0.0,
    M22: scale.1,
    M31: at.0,
    M32: at.1,
  }
}

/// The devices every pin draws with.
pub(crate) struct Gpu {
  d3d: ID3D11Device,
  context: ID2D1DeviceContext,
  dcomp: IDCompositionDesktopDevice,
  tile: u32,
  #[cfg(test)]
  lost: std::cell::Cell<bool>,
}

impl Gpu {
  /// The GPU, or Windows' software renderer (WARP) when there is no usable one. A machine
  /// without a GPU driver already gets a software adapter as its hardware device.
  pub(crate) fn new() -> Result<Self, String> {
    match Self::with_driver(D3D_DRIVER_TYPE_HARDWARE) {
      Ok(gpu) => Ok(gpu),
      Err(error) => {
        tracing::warn!(%error, "No Direct3D hardware device for pins; drawing them with WARP");
        Self::with_driver(D3D_DRIVER_TYPE_WARP).map_err(|warp| format!("{error}; WARP: {warp}"))
      },
    }
  }

  /// Windows' software renderer: deterministic, and present on every machine.
  #[cfg(test)]
  pub(crate) fn warp() -> Result<Self, String> {
    Self::with_driver(D3D_DRIVER_TYPE_WARP)
  }

  fn with_driver(driver: D3D_DRIVER_TYPE) -> Result<Self, String> {
    // SAFETY: the devices are created here and only used by the calling (pin host) thread; every
    // output is checked before use.
    unsafe {
      let mut device = None;
      D3D11CreateDevice(
        None,
        driver,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&LEVELS),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
      )
      .map_err(failed("D3D11CreateDevice"))?;
      let d3d = device.ok_or("D3D11CreateDevice returned no device")?;
      let dxgi: IDXGIDevice = d3d.cast().map_err(failed("IDXGIDevice"))?;
      let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
        .map_err(failed("D2D1CreateFactory"))?;
      let d2d = factory
        .CreateDevice(&dxgi)
        .map_err(failed("ID2D1Factory1::CreateDevice"))?;
      let context = d2d
        .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
        .map_err(failed("CreateDeviceContext"))?;
      let dcomp: IDCompositionDesktopDevice =
        DCompositionCreateDevice2(&d2d).map_err(failed("DCompositionCreateDevice2"))?;
      let tile = context.GetMaximumBitmapSize().clamp(256, SOURCE_TILE);
      Ok(Self {
        d3d,
        context,
        dcomp,
        tile,
        #[cfg(test)]
        lost: std::cell::Cell::new(false),
      })
    }
  }

  /// Whether the device stopped working: every pin must then be rebuilt on a new one.
  pub(crate) fn lost(&self) -> bool {
    #[cfg(test)]
    if self.lost.get() {
      return true;
    }
    // Direct2D and DirectComposition draw through this device; when it is removed, so are they.
    // SAFETY: a plain state query on this thread's device.
    unsafe { self.d3d.GetDeviceRemovedReason() }.is_err()
  }

  /// Makes the device report itself lost, as a removed GPU would.
  #[cfg(test)]
  pub(crate) fn simulate_loss(&self) {
    self.lost.set(true);
  }

  /// Shows every change made since the last commit, all pins at once.
  pub(crate) fn commit(&self) -> Result<(), String> {
    // SAFETY: this thread's composition device.
    unsafe { self.dcomp.Commit() }.map_err(failed("IDCompositionDevice::Commit"))
  }

  fn bitmap(
    &self,
    pixels: &[u8],
    width: u32,
    height: u32,
    pitch: u32,
  ) -> Result<ID2D1Bitmap1, String> {
    let needed = (pitch as usize) * (height.max(1) as usize - 1) + width as usize * 4;
    if pixels.len() < needed {
      return Err(format!(
        "{width}×{height} bitmap needs {needed} bytes, got {}",
        pixels.len()
      ));
    }
    // SAFETY: `pixels` covers every row at `pitch` (checked above) and outlives the call, which
    // copies them.
    unsafe {
      self.context.CreateBitmap(
        D2D_SIZE_U { width, height },
        Some(pixels.as_ptr().cast()),
        pitch,
        &bitmap_properties(),
      )
    }
    .map_err(failed("CreateBitmap"))
  }
}

/// Straight RGBA to premultiplied BGRA, the GPU's format.
pub(crate) fn premultiplied_bgra(rgba: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(rgba.len());
  for &[r, g, b, a] in rgba.as_chunks::<4>().0 {
    if a == 255 {
      out.extend_from_slice(&[b, g, r, 255]);
    } else {
      let premultiply = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
      out.extend_from_slice(&[premultiply(b), premultiply(g), premultiply(r), a]);
    }
  }
  out
}

struct Tile {
  /// The part of the image this bitmap draws.
  core: Area,
  /// The part it holds: `core` plus up to `OVERLAP` on each side.
  covers: Area,
  bitmap: ID2D1Bitmap1,
}

/// A pin's image on the GPU, in bitmaps the device can hold.
pub(crate) struct Source {
  width: u32,
  height: u32,
  tiles: Vec<Tile>,
}

impl Source {
  pub(crate) fn new(gpu: &Gpu, rgba: &[u8], width: u32, height: u32) -> Result<Self, String> {
    Self::tiled(gpu, rgba, width, height, gpu.tile)
  }

  /// Splits the image into bitmaps of at most `side` pixels each way.
  pub(crate) fn tiled(
    gpu: &Gpu,
    rgba: &[u8],
    width: u32,
    height: u32,
    side: u32,
  ) -> Result<Self, String> {
    if width == 0 || height == 0 || rgba.len() < width as usize * height as usize * 4 {
      return Err(format!(
        "Pin image {width}×{height} has {} bytes",
        rgba.len()
      ));
    }
    let core = side.saturating_sub(2 * OVERLAP).max(1);
    let pixels = premultiplied_bgra(&rgba[..width as usize * height as usize * 4]);
    let pitch = width * 4;
    let mut tiles = Vec::new();
    for top in (0..height).step_by(core as usize) {
      for left in (0..width).step_by(core as usize) {
        let part = Area {
          x: left as i32,
          y: top as i32,
          width: core.min(width - left) as i32,
          height: core.min(height - top) as i32,
        };
        let (x0, y0) = (left.saturating_sub(OVERLAP), top.saturating_sub(OVERLAP));
        let x1 = (left + part.width as u32 + OVERLAP).min(width);
        let y1 = (top + part.height as u32 + OVERLAP).min(height);
        let offset = (y0 * pitch + x0 * 4) as usize;
        let bitmap = gpu.bitmap(&pixels[offset..], x1 - x0, y1 - y0, pitch)?;
        tiles.push(Tile {
          core: part,
          covers: Area {
            x: x0 as i32,
            y: y0 as i32,
            width: (x1 - x0) as i32,
            height: (y1 - y0) as i32,
          },
          bitmap,
        });
      }
    }
    Ok(Self {
      width,
      height,
      tiles,
    })
  }

  #[cfg(test)]
  pub(crate) fn tile_count(&self) -> usize {
    self.tiles.len()
  }
}

/// Draws `source` resized to `size`, its top-left at `origin`: pixel for pixel when enlarging
/// (or at 1:1), high-quality cubic when shrinking.
pub(crate) fn draw_image(
  context: &ID2D1DeviceContext,
  source: &Source,
  size: (u32, u32),
  origin: (f32, f32),
) {
  let scale = (
    size.0 as f32 / source.width as f32,
    size.1 as f32 / source.height as f32,
  );
  let mode = if scale.0 >= 1.0 && scale.1 >= 1.0 {
    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
  } else {
    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC
  };
  for tile in &source.tiles {
    let core = tile.core;
    // Shrunken tiles meet on whole pixels: each resamples on its own, and two that met inside a
    // pixel would both blend into it. Enlarged ones repeat pixels and meet exactly as they are.
    let snap = mode == D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC;
    let edge = |position: i32, factor: f32, start: f32| {
      let value = start + position as f32 * factor;
      if snap { value.round() } else { value }
    };
    let target = D2D_RECT_F {
      left: edge(core.x, scale.0, origin.0),
      top: edge(core.y, scale.1, origin.1),
      right: edge(core.x + core.width, scale.0, origin.0),
      bottom: edge(core.y + core.height, scale.1, origin.1),
    };
    let from = rect_f(Area {
      x: core.x - tile.covers.x,
      y: core.y - tile.covers.y,
      ..core
    });
    // SAFETY: drawing with this thread's context into its current target.
    unsafe {
      context.DrawBitmap(&tile.bitmap, Some(&target), 1.0, mode, Some(&from), None);
    }
  }
}

/// Redraws all of `surface` (`width` × `height`) region by region; `paint` draws in surface
/// pixels into a region that starts transparent.
fn paint_surface(
  surface: &IDCompositionSurface,
  width: u32,
  height: u32,
  paint: impl Fn(&ID2D1DeviceContext),
) -> Result<(), String> {
  for top in (0..height as i32).step_by(SURFACE_TILE as usize) {
    for left in (0..width as i32).step_by(SURFACE_TILE as usize) {
      let region = RECT {
        left,
        top,
        right: (left + SURFACE_TILE).min(width as i32),
        bottom: (top + SURFACE_TILE).min(height as i32),
      };
      let mut offset = POINT::default();
      // SAFETY: a surface of this thread's device; every BeginDraw is paired with EndDraw.
      unsafe {
        let context: ID2D1DeviceContext = surface
          .BeginDraw(Some(&region), &mut offset)
          .map_err(failed("IDCompositionSurface::BeginDraw"))?;
        context.SetDpi(96.0, 96.0);
        context.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
        context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
        // The region lands at `offset` in an atlas the surface shares: draw only inside it.
        context.SetTransform(&Matrix3x2::translation(
          (offset.x - left) as f32,
          (offset.y - top) as f32,
        ));
        let clip = D2D_RECT_F {
          left: region.left as f32,
          top: region.top as f32,
          right: region.right as f32,
          bottom: region.bottom as f32,
        };
        context.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
        context.Clear(Some(&CLEAR));
        paint(&context);
        context.PopAxisAlignedClip();
        surface
          .EndDraw()
          .map_err(failed("IDCompositionSurface::EndDraw"))?;
      }
    }
  }
  Ok(())
}

/// What a pin shows in one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Layout {
  /// The image's size on screen; the pin is this plus the margin on every side.
  pub(crate) image: (i32, i32),
  /// The pin's top-left in its window: negative where the window is clipped to the screens.
  pub(crate) offset: (i32, i32),
  /// The user's opacity, 0 to 1.
  pub(crate) opacity: f32,
  /// The open/close fade (0 to 1) and scale around the window's center.
  pub(crate) presence: (f32, f32),
  /// The label's opacity; 0 hides it.
  pub(crate) label: f32,
}

/// One pin's visual tree.
pub(crate) struct Visuals {
  // Dropping the target detaches everything from the window.
  _target: IDCompositionTarget,
  root: IDCompositionVisual2,
  root_effect: IDCompositionEffectGroup,
  content_effect: IDCompositionEffectGroup,
  frame: Vec<IDCompositionVisual2>,
  image: IDCompositionVisual2,
  image_surface: IDCompositionVirtualSurface,
  rendered: (u32, u32),
  label: IDCompositionVisual2,
  label_effect: IDCompositionEffectGroup,
  source: Source,
  margin: i32,
  /// The label's top-left inside the image.
  label_inset: i32,
}

impl Visuals {
  /// Builds the pin's visuals in `hwnd` with its image drawn at `rendered` pixels.
  pub(crate) fn new(
    gpu: &Gpu,
    hwnd: HWND,
    rgba: &[u8],
    size: (u32, u32),
    margin: i32,
    label_inset: i32,
    rendered: (u32, u32),
  ) -> Result<Self, String> {
    let source = Source::new(gpu, rgba, size.0, size.1)?;
    let device = &gpu.dcomp;
    // SAFETY: COM objects of this thread's composition device, attached to this thread's window.
    unsafe {
      let target = device
        .CreateTargetForHwnd(hwnd, true)
        .map_err(failed("CreateTargetForHwnd"))?;
      let visual = || device.CreateVisual().map_err(failed("CreateVisual"));
      let effect = || {
        device
          .CreateEffectGroup()
          .map_err(failed("CreateEffectGroup"))
      };
      let root = visual()?;
      let root_effect = effect()?;
      root.SetEffect(&root_effect).map_err(failed("SetEffect"))?;
      let content = visual()?;
      let content_effect = effect()?;
      content
        .SetEffect(&content_effect)
        .map_err(failed("SetEffect"))?;
      root
        .AddVisual(&content, true, None::<&IDCompositionVisual>)
        .map_err(failed("AddVisual"))?;

      // The outline and shadow: eight slices of one small template, drawn once.
      let (template_width, _) = frame::template_size(margin);
      let template = frame::template(margin);
      let (_, template_height) = frame::template_size(margin);
      let template = gpu.bitmap(
        &template,
        template_width,
        template_height,
        template_width * 4,
      )?;
      let mut frame_visuals: Vec<IDCompositionVisual2> = Vec::with_capacity(8);
      for piece in frame::slices(margin, 1, 1) {
        let source_area = piece.source;
        let surface = device
          .CreateSurface(
            source_area.width as u32,
            source_area.height as u32,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_ALPHA_MODE_PREMULTIPLIED,
          )
          .map_err(failed("CreateSurface"))?;
        paint_surface(
          &surface,
          source_area.width as u32,
          source_area.height as u32,
          |context| {
            let to = rect_f(Area {
              x: 0,
              y: 0,
              ..source_area
            });
            context.DrawBitmap(
              &template,
              Some(&to),
              1.0,
              D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
              Some(&rect_f(source_area)),
              None,
            );
          },
        )?;
        let slice = visual()?;
        slice.SetContent(&surface).map_err(failed("SetContent"))?;
        slice
          .SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR)
          .map_err(failed("SetBitmapInterpolationMode"))?;
        match frame_visuals.last() {
          None => content.AddVisual(&slice, true, None::<&IDCompositionVisual>),
          Some(below) => content.AddVisual(&slice, true, below),
        }
        .map_err(failed("AddVisual"))?;
        frame_visuals.push(slice);
      }

      let image = visual()?;
      let image_surface = device
        .CreateVirtualSurface(
          rendered.0.max(1),
          rendered.1.max(1),
          DXGI_FORMAT_B8G8R8A8_UNORM,
          DXGI_ALPHA_MODE_PREMULTIPLIED,
        )
        .map_err(failed("CreateVirtualSurface"))?;
      image
        .SetContent(&image_surface)
        .map_err(failed("SetContent"))?;
      if let Some(below) = frame_visuals.last() {
        content
          .AddVisual(&image, true, below)
          .map_err(failed("AddVisual"))?;
      }
      let label = visual()?;
      let label_effect = effect()?;
      label
        .SetEffect(&label_effect)
        .map_err(failed("SetEffect"))?;
      label_effect
        .SetOpacity2(0.0)
        .map_err(failed("SetOpacity"))?;
      root
        .AddVisual(&label, true, &content)
        .map_err(failed("AddVisual"))?;
      target.SetRoot(&root).map_err(failed("SetRoot"))?;
      let mut visuals = Self {
        _target: target,
        root,
        root_effect,
        content_effect,
        frame: frame_visuals,
        image,
        image_surface,
        rendered: (0, 0),
        label,
        label_effect,
        source,
        margin,
        label_inset,
      };
      visuals.render(rendered)?;
      Ok(visuals)
    }
  }

  /// The image surface's size.
  pub(crate) fn rendered(&self) -> (u32, u32) {
    self.rendered
  }

  /// Redraws the image surface at `size` pixels, when it differs.
  pub(crate) fn render(&mut self, size: (u32, u32)) -> Result<(), String> {
    let size = (size.0.max(1), size.1.max(1));
    if size == self.rendered {
      return Ok(());
    }
    // SAFETY: this thread's virtual surface; its old pixels are redrawn below.
    unsafe { self.image_surface.Resize(size.0, size.1) }
      .map_err(failed("IDCompositionVirtualSurface::Resize"))?;
    // Cleared first: a failure below leaves nothing drawn, so a later attempt redraws it all.
    self.rendered = (0, 0);
    let source = &self.source;
    paint_surface(&self.image_surface, size.0, size.1, |context| {
      draw_image(context, source, size, (0.0, 0.0));
    })?;
    self.rendered = size;
    Ok(())
  }

  /// Shows `pixels` (premultiplied BGRA, top-down) as the label.
  pub(crate) fn set_label(
    &mut self,
    gpu: &Gpu,
    pixels: &[u8],
    width: u32,
    height: u32,
  ) -> Result<(), String> {
    let bitmap = gpu.bitmap(pixels, width, height, width * 4)?;
    // SAFETY: a surface of this thread's device, shown by this pin's label visual.
    unsafe {
      let surface = gpu
        .dcomp
        .CreateSurface(
          width,
          height,
          DXGI_FORMAT_B8G8R8A8_UNORM,
          DXGI_ALPHA_MODE_PREMULTIPLIED,
        )
        .map_err(failed("CreateSurface"))?;
      paint_surface(&surface, width, height, |context| {
        context.DrawBitmap(
          &bitmap,
          None,
          1.0,
          D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
          None,
          None,
        );
      })?;
      self
        .label
        .SetContent(&surface)
        .map_err(failed("SetContent"))
    }
  }

  /// Sets every visual for one frame; nothing shows until the device commits.
  pub(crate) fn layout(&self, layout: Layout) -> Result<(), String> {
    let m = self.margin;
    let (width, height) = (layout.image.0.max(1), layout.image.1.max(1));
    let center = Vector2 {
      X: (width + 2 * m) as f32 / 2.0,
      Y: (height + 2 * m) as f32 / 2.0,
    };
    let (rendered_width, rendered_height) = (self.rendered.0.max(1), self.rendered.1.max(1));
    let scale = (
      width as f32 / rendered_width as f32,
      height as f32 / rendered_height as f32,
    );
    let interpolation = if scale.0 >= 1.0 && scale.1 >= 1.0 {
      DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR
    } else {
      DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR
    };
    // SAFETY: property changes on this thread's visuals.
    unsafe {
      self
        .root_effect
        .SetOpacity2(layout.presence.0.clamp(0.0, 1.0))
        .map_err(failed("SetOpacity"))?;
      // Scaled around the pin's center, then placed at its offset in the window.
      let presence = layout.presence.1;
      self
        .root
        .SetTransform2(&placed(
          (presence, presence),
          (
            center.X * (1.0 - presence) + layout.offset.0 as f32,
            center.Y * (1.0 - presence) + layout.offset.1 as f32,
          ),
        ))
        .map_err(failed("SetTransform"))?;
      self
        .content_effect
        .SetOpacity2(layout.opacity.clamp(0.0, 1.0))
        .map_err(failed("SetOpacity"))?;
      for (visual, piece) in self.frame.iter().zip(frame::slices(m, width, height)) {
        let (from, to) = (piece.source, piece.target);
        visual
          .SetTransform2(&placed(
            (
              to.width as f32 / from.width.max(1) as f32,
              to.height as f32 / from.height.max(1) as f32,
            ),
            (to.x as f32, to.y as f32),
          ))
          .map_err(failed("SetTransform"))?;
      }
      self
        .image
        .SetBitmapInterpolationMode(interpolation)
        .map_err(failed("SetBitmapInterpolationMode"))?;
      self
        .image
        .SetTransform2(&placed(scale, (m as f32, m as f32)))
        .map_err(failed("SetTransform"))?;
      let inset = (m + self.label_inset) as f32;
      self
        .label
        .SetTransform2(&Matrix3x2::translation(inset, inset))
        .map_err(failed("SetTransform"))?;
      self
        .label_effect
        .SetOpacity2(layout.label.clamp(0.0, 1.0))
        .map_err(failed("SetOpacity"))
    }
  }
}

/// Draws into a new `width` × `height` bitmap and reads it back, premultiplied BGRA top-down.
#[cfg(test)]
pub(crate) fn render_offscreen(
  gpu: &Gpu,
  width: u32,
  height: u32,
  paint: impl Fn(&ID2D1DeviceContext),
) -> Result<Vec<u8>, String> {
  use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_CPU_READ, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_MAP_OPTIONS_READ,
  };
  let size = D2D_SIZE_U { width, height };
  // SAFETY: bitmaps of this thread's context; the mapped rows are read before Unmap.
  unsafe {
    let target = gpu
      .context
      .CreateBitmap(
        size,
        None,
        0,
        &D2D1_BITMAP_PROPERTIES1 {
          bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
          ..bitmap_properties()
        },
      )
      .map_err(failed("CreateBitmap (target)"))?;
    let context = &gpu.context;
    context.SetTarget(&target);
    context.SetDpi(96.0, 96.0);
    context.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
    context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
    context.SetTransform(&Matrix3x2::identity());
    context.BeginDraw();
    context.Clear(Some(&CLEAR));
    paint(context);
    let drawn = context.EndDraw(None, None).map_err(failed("EndDraw"));
    context.SetTarget(None::<&windows::Win32::Graphics::Direct2D::ID2D1Image>);
    drawn?;
    let readback = context
      .CreateBitmap(
        size,
        None,
        0,
        &D2D1_BITMAP_PROPERTIES1 {
          bitmapOptions: D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
          ..bitmap_properties()
        },
      )
      .map_err(failed("CreateBitmap (readback)"))?;
    readback
      .CopyFromBitmap(None, &target, None)
      .map_err(failed("CopyFromBitmap"))?;
    let mapped = readback.Map(D2D1_MAP_OPTIONS_READ).map_err(failed("Map"))?;
    let row = width as usize * 4;
    let mut pixels = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
      pixels.extend_from_slice(std::slice::from_raw_parts(
        mapped.bits.add(y * mapped.pitch as usize),
        row,
      ));
    }
    readback.Unmap().map_err(failed("Unmap"))?;
    Ok(pixels)
  }
}

#[cfg(test)]
#[path = "gpu_tests.rs"]
mod tests;
