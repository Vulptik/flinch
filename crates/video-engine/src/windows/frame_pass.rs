//! One compute pass between a captured frame and the BGRA desktop cache, for displays whose
//! frames are not cache-ready:
//! - Rotated displays. Desktop Duplication delivers them in the panel's scan-out orientation
//!   (see `rotate.rs`).
//! - HDR displays. Duplication and WGC deliver FP16 scRGB: linear BT.709 light where 1.0 is
//!   80 nits. Windows composes SDR content into that desktop by decoding its sRGB curve and
//!   scaling it to the "SDR content brightness" setting (`sdr_white.rs`). The pass inverts
//!   exactly that, so SDR content records as the pixels the application drew. Brighter HDR
//!   highlights are scaled down to the brightest channel's 1.0, keeping their hue; colours
//!   outside BT.709 are clipped.
//!
//! A rotated HDR display needs one dispatch for both.
use super::gpu::Gpu;
use super::rotate::Orientation;
use crate::{EngineError, Result};
use windows::{
  Win32::Graphics::{Direct3D::Fxc::*, Direct3D11::*, Dxgi::Common::*},
  core::s,
};

/// What a display's captured texels mean. Recorded per display in `source-formats.json`.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "range", rename_all = "snake_case")]
pub enum Tone {
  /// BGRA sRGB texels, already as the cache holds them.
  Sdr,
  /// FP16 scRGB texels of an HDR desktop, whose SDR content white is `sdr_white_nits`.
  Hdr { sdr_white_nits: f32 },
}
impl Tone {
  /// The format to request from duplication or WGC, and to validate every frame against.
  pub fn input_format(self) -> DXGI_FORMAT {
    match self {
      Self::Sdr => DXGI_FORMAT_B8G8R8A8_UNORM,
      Self::Hdr { .. } => DXGI_FORMAT_R16G16B16A16_FLOAT,
    }
  }
}

/// SDR white of an HDR desktop in scRGB units (`sdr_white_nits` / 80), the reference for
/// `frame_pass.hlsl`. Returns sRGB-encoded red, green and blue in [0, 1].
#[cfg(test)]
pub(crate) fn sdr_from_scrgb(rgb: [f32; 3], sdr_white_nits: f32) -> [f32; 3] {
  let scale = 80.0 / sdr_white_nits;
  let mut linear = rgb.map(|value| (value * scale).max(0.0));
  let peak = linear.iter().copied().fold(0.0f32, f32::max);
  if peak > 1.0 {
    linear = linear.map(|value| value / peak);
  }
  linear.map(|l| {
    if l <= 0.003_130_8 {
      l * 12.92
    } else {
      1.055 * l.powf(1.0 / 2.4) - 0.055
    }
  })
}

#[repr(C)]
struct Constants {
  width: u32,
  height: u32,
  rotation: u32,
  hdr: u32,
  white_scale: f32,
  unused: [f32; 3],
}

/// Prepares one display's frames for the cache. Owned by that display's capture.
pub struct FramePass {
  shader: ID3D11ComputeShader,
  constants: ID3D11Buffer,
  /// Shader-readable copy of the captured frame; capture surfaces carry no bind promise.
  panel: ID3D11Texture2D,
  panel_view: ID3D11ShaderResourceView,
  upright: ID3D11Texture2D,
  upright_view: ID3D11UnorderedAccessView,
  orientation: Orientation,
  tone: Tone,
  width: u32,
  height: u32,
}
impl FramePass {
  /// A pass for a `width` x `height` desktop (desktop orientation). An upright SDR display needs
  /// none and is refused.
  pub fn new(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    orientation: Orientation,
    tone: Tone,
  ) -> Result<Self> {
    if orientation == Orientation::Upright && tone == Tone::Sdr {
      return Err(EngineError::protocol(
        "An upright SDR display needs no frame pass",
      ));
    }
    if width == 0 || height == 0 {
      return Err(EngineError::protocol("Frame pass display has no extent"));
    }
    let white_scale = match tone {
      Tone::Sdr => 1.0,
      Tone::Hdr { sdr_white_nits } if sdr_white_nits.is_finite() && sdr_white_nits >= 80.0 => {
        80.0 / sdr_white_nits
      },
      Tone::Hdr { sdr_white_nits } => {
        return Err(EngineError::protocol(format!(
          "SDR white level {sdr_white_nits} nits is below Windows' 80 nit minimum"
        )));
      },
    };
    let (panel_width, panel_height) = orientation.panel(width, height);
    let bytecode = compile()?;
    let panel = Gpu::texture(
      device,
      panel_width,
      panel_height,
      tone.input_format(),
      D3D11_BIND_SHADER_RESOURCE.0 as u32,
    )?;
    let upright = Gpu::texture(
      device,
      width,
      height,
      DXGI_FORMAT_B8G8R8A8_UNORM,
      D3D11_BIND_UNORDERED_ACCESS.0 as u32,
    )?;
    let mut shader = None;
    let mut constants = None;
    let mut panel_view = None;
    let mut upright_view = None;
    let values = Constants {
      width,
      height,
      rotation: orientation.dxgi(),
      hdr: u32::from(matches!(tone, Tone::Hdr { .. })),
      white_scale,
      unused: [0.0; 3],
    };
    let initial = D3D11_SUBRESOURCE_DATA {
      pSysMem: (&raw const values).cast(),
      ..Default::default()
    };
    // SAFETY: D3D copies the bytecode and the constant data during creation; views retain
    // their textures.
    unsafe {
      device
        .CreateComputeShader(&bytecode, None, Some(&mut shader))
        .map_err(|e| EngineError::native("frame pass shader", e))?;
      device
        .CreateBuffer(
          &D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<Constants>() as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
          },
          Some(&initial),
          Some(&mut constants),
        )
        .map_err(|e| EngineError::native("frame pass constants", e))?;
      device
        .CreateShaderResourceView(&panel, None, Some(&mut panel_view))
        .map_err(|e| EngineError::native("frame pass input view", e))?;
      device
        .CreateUnorderedAccessView(&upright, None, Some(&mut upright_view))
        .map_err(|e| EngineError::native("frame pass target view", e))?;
    }
    Ok(Self {
      shader: shader.ok_or_else(|| EngineError::protocol("No frame pass shader"))?,
      constants: constants.ok_or_else(|| EngineError::protocol("No frame pass constants"))?,
      panel,
      panel_view: panel_view.ok_or_else(|| EngineError::protocol("No frame pass input view"))?,
      upright,
      upright_view: upright_view
        .ok_or_else(|| EngineError::protocol("No frame pass target view"))?,
      orientation,
      tone,
      width,
      height,
    })
  }
  /// Extent a captured frame of this display must have.
  pub fn panel_extent(&self) -> (u32, u32) {
    self.orientation.panel(self.width, self.height)
  }
  /// Format a captured frame of this display must have.
  pub fn input_format(&self) -> DXGI_FORMAT {
    self.tone.input_format()
  }
  pub fn tone(&self) -> Tone {
    self.tone
  }
  /// Prepare one frame and return the upright BGRA texture.
  ///
  /// # Safety
  /// `frame` must be a same-device texture of exactly [`Self::panel_extent`] and
  /// [`Self::input_format`].
  pub unsafe fn apply(
    &self,
    context: &ID3D11DeviceContext,
    frame: &ID3D11Texture2D,
  ) -> &ID3D11Texture2D {
    // SAFETY: guaranteed by the caller; every binding is withdrawn before return, and the
    // command stream retains the resources until it runs.
    unsafe {
      context.CopyResource(&self.panel, frame);
      context.CSSetShader(&self.shader, None);
      context.CSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
      context.CSSetShaderResources(0, Some(&[Some(self.panel_view.clone())]));
      context.CSSetUnorderedAccessViews(0, 1, Some(&Some(self.upright_view.clone())), None);
      context.Dispatch(self.width.div_ceil(8), self.height.div_ceil(8), 1);
      context.CSSetShaderResources(0, Some(&[None]));
      context.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
      context.CSSetShader(None, None);
    }
    &self.upright
  }
}

fn compile() -> Result<Vec<u8>> {
  let code = include_str!("frame_pass.hlsl");
  let mut blob = None;
  let mut errors = None;
  // SAFETY: immutable embedded HLSL; compiler receives no include handler or external file paths.
  unsafe {
    D3DCompile(
      code.as_ptr().cast(),
      code.len(),
      s!("flinch frame pass"),
      None,
      None,
      s!("main"),
      s!("cs_5_0"),
      D3DCOMPILE_OPTIMIZATION_LEVEL3,
      0,
      &mut blob,
      Some(&mut errors),
    )
  }
  .map_err(|e| {
    let mut error = EngineError::native("frame pass shader compile", e);
    // The compiler's own diagnostics name the failing line.
    if let Some(errors) = &errors {
      // SAFETY: the blob owns its message bytes until dropped after this copy.
      let message = unsafe {
        std::slice::from_raw_parts(
          errors.GetBufferPointer().cast::<u8>(),
          errors.GetBufferSize(),
        )
      };
      error.reason.push_str(&format!(
        ": {}",
        String::from_utf8_lossy(message).trim_end_matches(['\0', '\n'])
      ));
    }
    error
  })?;
  let blob = blob.ok_or_else(|| EngineError::protocol("No compiled frame pass shader"))?;
  // SAFETY: the blob owns its bytecode until dropped at the end of this copy.
  Ok(
    unsafe {
      std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
    .to_vec(),
  )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
