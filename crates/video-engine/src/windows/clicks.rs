//! Click highlights in the recorded picture: button sampling without a global hook, and the GPU
//! pass that blends the marks of `crate::clicks` onto the frame. Only mark metadata is uploaded;
//! frame pixels are never read by the CPU.
#[cfg(test)]
#[path = "clicks_tests.rs"]
mod tests;
use super::{
  cursor::{ShaderCompile, compile_hlsl},
  gpu::Gpu,
};
use crate::{
  EngineError, Result,
  clicks::{Clicks, Mark},
};
use windows::Win32::{
  Foundation::POINT,
  Graphics::{Direct3D::*, Direct3D11::*, Gdi::*},
  UI::{
    HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
    Input::KeyboardAndMouse::{GetAsyncKeyState, VIRTUAL_KEY, VK_LBUTTON, VK_RBUTTON},
    WindowsAndMessaging::{GetCursorPos, GetSystemMetrics, SM_SWAPBUTTON},
  },
};

/// Marks one pass can draw; `crate::clicks` never asks for more than nine.
pub const MAX_MARKS: usize = 16;
/// Header (extent, count) plus 16 marks of two float4 each.
const CONSTANT_FLOATS: usize = 4 + MAX_MARKS * 8;

/// The primary and secondary buttons and the pointer, in virtual-screen pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
  pub down: [bool; 2],
  /// Went down since the previous read, for clicks shorter than the time between samples.
  pub pressed: [bool; 2],
  pub pointer: (i32, i32),
}

/// Reads the buttons as the user sees them: with swapped buttons the physical right button is the
/// primary one, drawn as a left click.
pub fn sample() -> Result<Sample> {
  // SAFETY: plain reads of a system metric, the asynchronous key state and the pointer position.
  let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
  let (primary, secondary) = if swapped {
    (VK_RBUTTON, VK_LBUTTON)
  } else {
    (VK_LBUTTON, VK_RBUTTON)
  };
  let state = |key: VIRTUAL_KEY| {
    // SAFETY: as above; the call has no preconditions.
    let value = unsafe { GetAsyncKeyState(i32::from(key.0)) } as u16;
    (value & 0x8000 != 0, value & 1 != 0)
  };
  let (first, second) = (state(primary), state(secondary));
  let mut point = POINT::default();
  // SAFETY: `point` is written only during this call.
  unsafe { GetCursorPos(&mut point) }.map_err(|e| EngineError::native("pointer position", e))?;
  Ok(Sample {
    down: [first.0, second.0],
    pressed: [first.1, second.1],
    pointer: (point.x, point.y),
  })
}

/// The display scale at a virtual-screen point (1.0 at 100%), for sizing the marks.
pub fn scale_at(x: i32, y: i32) -> Result<f32> {
  // SAFETY: a point query that returns the nearest monitor; no handle is owned.
  let monitor = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
  let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
  // SAFETY: both outputs are stack-owned and written only during this call.
  unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
    .map_err(|e| EngineError::native("display scale", e))?;
  Ok(dpi_x.max(96) as f32 / 96.0)
}

/// Vertex and pixel shader bytecode.
pub struct Shaders {
  vertex: Vec<u8>,
  pixel: Vec<u8>,
}
pub fn compile() -> Result<Shaders> {
  let code = include_str!("clicks.hlsl");
  Ok(Shaders {
    vertex: compile_hlsl(code, c"vs", c"vs_5_0", "click shader compile")?,
    pixel: compile_hlsl(code, c"ps", c"ps_5_0", "click shader compile")?,
  })
}
/// Compiles the click shaders off the Prepare thread.
pub fn start_compile() -> Result<ShaderCompile<Shaders>> {
  ShaderCompile::spawn("video click shader", "click shader compile", compile)
}

/// The shader constants for `marks` on a `width` x `height` frame. Mark centers move to pixel
/// centers: the pointer's hotspot pixel `x` is centered on `x + 0.5`.
fn constants(marks: &[Mark], width: u32, height: u32) -> Result<[f32; CONSTANT_FLOATS]> {
  if marks.len() > MAX_MARKS {
    return Err(EngineError::protocol("Too many click marks for one pass"));
  }
  let mut out = [0f32; CONSTANT_FLOATS];
  out[0] = width as f32;
  out[1] = height as f32;
  out[2] = f32::from_bits(marks.len() as u32);
  for (index, mark) in marks.iter().enumerate() {
    let base = 4 + index * 8;
    out[base..base + 8].copy_from_slice(&[
      mark.x + 0.5,
      mark.y + 0.5,
      mark.radius,
      mark.thickness,
      mark.color[0],
      mark.color[1],
      mark.color[2],
      mark.alpha,
    ]);
  }
  Ok(out)
}

pub struct ClickPainter {
  vertex: ID3D11VertexShader,
  pixel: ID3D11PixelShader,
  constants: ID3D11Buffer,
  blend: ID3D11BlendState,
  raster: ID3D11RasterizerState,
  target: ID3D11RenderTargetView,
}
impl ClickPainter {
  pub fn new(gpu: &Gpu, shaders: &Shaders) -> Result<Self> {
    let mut vertex = None;
    let mut pixel = None;
    let mut constants = None;
    let mut blend = None;
    let mut raster = None;
    let mut target = None;
    let mut targets = [D3D11_RENDER_TARGET_BLEND_DESC::default(); 8];
    // Straight alpha over the frame; the frame's own alpha stays opaque.
    targets[0] = D3D11_RENDER_TARGET_BLEND_DESC {
      BlendEnable: true.into(),
      SrcBlend: D3D11_BLEND_SRC_ALPHA,
      DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
      BlendOp: D3D11_BLEND_OP_ADD,
      SrcBlendAlpha: D3D11_BLEND_ZERO,
      DestBlendAlpha: D3D11_BLEND_ONE,
      BlendOpAlpha: D3D11_BLEND_OP_ADD,
      RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    // SAFETY: D3D copies bytecode and descriptions during creation. The view retains scratch.
    unsafe {
      let device = &gpu.device;
      device
        .CreateVertexShader(&shaders.vertex, None, Some(&mut vertex))
        .map_err(|e| EngineError::native("click vertex shader", e))?;
      device
        .CreatePixelShader(&shaders.pixel, None, Some(&mut pixel))
        .map_err(|e| EngineError::native("click pixel shader", e))?;
      device
        .CreateBuffer(
          &D3D11_BUFFER_DESC {
            ByteWidth: (CONSTANT_FLOATS * 4) as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
          },
          None,
          Some(&mut constants),
        )
        .map_err(|e| EngineError::native("click constants", e))?;
      device
        .CreateBlendState(
          &D3D11_BLEND_DESC {
            AlphaToCoverageEnable: false.into(),
            IndependentBlendEnable: false.into(),
            RenderTarget: targets,
          },
          Some(&mut blend),
        )
        .map_err(|e| EngineError::native("click blend", e))?;
      device
        .CreateRasterizerState(
          &D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
          },
          Some(&mut raster),
        )
        .map_err(|e| EngineError::native("click rasterizer", e))?;
      device
        .CreateRenderTargetView(&gpu.scratch, None, Some(&mut target))
        .map_err(|e| EngineError::native("click target view", e))?;
    }
    Ok(Self {
      vertex: vertex.ok_or_else(|| EngineError::protocol("No click vertex shader"))?,
      pixel: pixel.ok_or_else(|| EngineError::protocol("No click pixel shader"))?,
      constants: constants.ok_or_else(|| EngineError::protocol("No click constants"))?,
      blend: blend.ok_or_else(|| EngineError::protocol("No click blend"))?,
      raster: raster.ok_or_else(|| EngineError::protocol("No click rasterizer"))?,
      target: target.ok_or_else(|| EngineError::protocol("No click target view"))?,
    })
  }
  /// Draws `marks` over `source` (the desktop cache, or scratch holding the composed pointer)
  /// and returns the texture to convert. Marks are drawn on the whole display before any crop,
  /// so an area recording clips them at its edges. No marks: `source` is returned untouched.
  pub fn paint<'a>(
    &self,
    gpu: &'a Gpu,
    source: &'a ID3D11Texture2D,
    marks: &[Mark],
  ) -> Result<&'a ID3D11Texture2D> {
    if marks.is_empty() {
      return Ok(source);
    }
    let from_desktop =
      windows::core::Interface::as_raw(source) == windows::core::Interface::as_raw(&gpu.desktop);
    if !from_desktop
      && windows::core::Interface::as_raw(source) != windows::core::Interface::as_raw(&gpu.scratch)
    {
      return Err(EngineError::protocol(
        "Click source is not an owned GPU cache",
      ));
    }
    let constants = constants(marks, gpu.source_width, gpu.source_height)?;
    // SAFETY: same-device textures and states created above; every binding is cleared after
    // the draw so scratch can be copied and converted.
    unsafe {
      let context = &gpu.context;
      if from_desktop {
        context.CopyResource(&gpu.scratch, &gpu.desktop);
      }
      context.UpdateSubresource(&self.constants, 0, None, constants.as_ptr().cast(), 0, 0);
      context.IASetInputLayout(None);
      context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
      context.VSSetShader(&self.vertex, None);
      context.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
      context.PSSetShader(&self.pixel, None);
      context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
      context.RSSetState(&self.raster);
      context.RSSetViewports(Some(&[D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: gpu.source_width as f32,
        Height: gpu.source_height as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
      }]));
      context.OMSetBlendState(&self.blend, None, u32::MAX);
      context.OMSetRenderTargets(Some(&[Some(self.target.clone())]), None);
      context.DrawInstanced(4, marks.len() as u32, 0, 0);
      context.OMSetRenderTargets(None, None);
      context.OMSetBlendState(None, None, u32::MAX);
      context.RSSetState(None);
      context.VSSetShader(None, None);
      context.PSSetShader(None, None);
    }
    Ok(&gpu.scratch)
  }
}

/// Click highlights for one session: the tracker, its painter and where the picture sits on the
/// virtual screen.
pub struct ClickLayer {
  painter: ClickPainter,
  tracker: Clicks,
  origin: (i32, i32),
  /// The last converted frame showed marks, so the next one must be converted to remove them.
  shown: bool,
  /// Pointer reads are failing (the secure desktop); reported once per failing streak.
  failing: bool,
  read: fn() -> Result<Sample>,
}
impl ClickLayer {
  pub fn new(painter: ClickPainter, scale: f32, origin: (i32, i32)) -> Self {
    Self::with_reader(painter, scale, origin, sample)
  }
  fn with_reader(
    painter: ClickPainter,
    scale: f32,
    origin: (i32, i32),
    read: fn() -> Result<Sample>,
  ) -> Self {
    Self {
      painter,
      tracker: Clicks::new(scale),
      origin,
      shown: false,
      failing: false,
      read,
    }
  }
  /// One recording tick at `now` (QPC 100 ns). True when the frame must be converted again:
  /// marks show now, or showed in the last converted frame.
  pub fn tick(&mut self, now: u64) -> bool {
    match (self.read)() {
      Ok(sample) => {
        self.failing = false;
        let pointer = (
          (i64::from(sample.pointer.0) - i64::from(self.origin.0)) as f32,
          (i64::from(sample.pointer.1) - i64::from(self.origin.1)) as f32,
        );
        self
          .tracker
          .sample(now, sample.down, sample.pressed, pointer);
      },
      Err(error) => {
        // Windows withholds input state on the secure desktop; no marks until it returns.
        if !self.failing {
          eprintln!("Click highlights paused: {error}");
        }
        self.failing = true;
        self.tracker.reset();
      },
    }
    self.tracker.active(now) || self.shown
  }
  /// Paused: nothing pressed meanwhile is drawn, and a button still held on resume is no click.
  pub fn pause(&mut self) {
    self.tracker.reset();
  }
  pub fn paint<'a>(
    &mut self,
    gpu: &'a Gpu,
    source: &'a ID3D11Texture2D,
    now: u64,
  ) -> Result<&'a ID3D11Texture2D> {
    let marks = self.tracker.marks(now);
    let painted = self.painter.paint(gpu, source, &marks)?;
    self.shown = !marks.is_empty();
    Ok(painted)
  }
}
