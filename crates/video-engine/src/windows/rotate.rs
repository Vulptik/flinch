//! Rotated displays. Desktop Duplication delivers a rotated display in its panel's scan-out
//! orientation, while the desktop cache, regions and pointer positions are in desktop
//! orientation. The frame pass (`frame_pass.rs`) turns each duplicated frame upright before it
//! joins the cache.
//!
//! The texel mapping follows Microsoft's Desktop Duplication sample (`SetDirtyVert`).
use crate::{EngineError, Result};
use windows::Win32::Graphics::Dxgi::Common::*;

/// How a display's desktop is turned relative to its panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
  Upright,
  Rotate90,
  Rotate180,
  Rotate270,
}
impl Orientation {
  /// `DXGI_OUTPUT_DESC::Rotation`. Windows reports an unrotated display as identity, and
  /// unspecified is read the same way.
  pub fn from_dxgi(rotation: u32) -> Result<Self> {
    match rotation {
      r if r == DXGI_MODE_ROTATION_UNSPECIFIED.0 as u32
        || r == DXGI_MODE_ROTATION_IDENTITY.0 as u32 =>
      {
        Ok(Self::Upright)
      },
      r if r == DXGI_MODE_ROTATION_ROTATE90.0 as u32 => Ok(Self::Rotate90),
      r if r == DXGI_MODE_ROTATION_ROTATE180.0 as u32 => Ok(Self::Rotate180),
      r if r == DXGI_MODE_ROTATION_ROTATE270.0 as u32 => Ok(Self::Rotate270),
      other => Err(EngineError::protocol(format!(
        "Unknown display rotation {other}"
      ))),
    }
  }
  pub(super) fn dxgi(self) -> u32 {
    match self {
      Self::Upright => DXGI_MODE_ROTATION_IDENTITY.0 as u32,
      Self::Rotate90 => DXGI_MODE_ROTATION_ROTATE90.0 as u32,
      Self::Rotate180 => DXGI_MODE_ROTATION_ROTATE180.0 as u32,
      Self::Rotate270 => DXGI_MODE_ROTATION_ROTATE270.0 as u32,
    }
  }
  /// Panel (scan-out) extent of a `width` x `height` desktop.
  pub fn panel(self, width: u32, height: u32) -> (u32, u32) {
    match self {
      Self::Rotate90 | Self::Rotate270 => (height, width),
      Self::Upright | Self::Rotate180 => (width, height),
    }
  }
  /// Panel texel shown at desktop pixel (`x`, `y`) of a `width` x `height` desktop.
  /// `frame_pass.hlsl` computes the same mapping.
  pub fn panel_texel(self, width: u32, height: u32, x: u32, y: u32) -> (u32, u32) {
    match self {
      Self::Upright => (x, y),
      Self::Rotate90 => (y, width - 1 - x),
      Self::Rotate180 => (width - 1 - x, height - 1 - y),
      Self::Rotate270 => (height - 1 - y, x),
    }
  }
}

/// Test texture helpers on a real software (WARP) device; no display or capture is opened.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) mod fixtures {
  use super::super::gpu::Gpu;
  use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*, Dxgi::*};
  /// A GPU-less tier on WARP at a `width` x `height` desktop. An odd desktop encodes its
  /// largest even area, as Prepare does for odd displays.
  pub fn warp_gpu(width: u32, height: u32) -> Gpu {
    // SAFETY: read-only factory for the software adapter; no output, capture or display.
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
    // SAFETY: as above.
    let adapter: IDXGIAdapter1 = unsafe { factory.EnumWarpAdapter() }.expect("WARP adapter");
    let even = crate::types::Region {
      x: 0,
      y: 0,
      width: width & !1,
      height: height & !1,
    };
    let region = (even.width, even.height) != (width, height);
    Gpu::new(&adapter, width, height, region.then_some(even), 30).expect("WARP GPU")
  }
  /// Every texel carries its own panel coordinates: blue = x, green = y.
  pub fn coordinate(x: u32, y: u32) -> [u8; 4] {
    [x as u8, y as u8, 9, 255]
  }
  /// A BGRA texture filled with [`coordinate`] texels.
  pub fn numbered(gpu: &Gpu, width: u32, height: u32) -> ID3D11Texture2D {
    let texture = Gpu::texture(
      &gpu.device,
      width,
      height,
      DXGI_FORMAT_B8G8R8A8_UNORM,
      D3D11_BIND_SHADER_RESOURCE.0 as u32,
    )
    .expect("numbered texture");
    let image: Vec<u8> = (0..height)
      .flat_map(|y| (0..width).flat_map(move |x| coordinate(x, y)))
      .collect();
    // SAFETY: the data spans the whole texture at its packed pitch.
    unsafe {
      gpu
        .context
        .UpdateSubresource(&texture, 0, None, image.as_ptr().cast(), width * 4, 0);
    }
    texture
  }
  /// Read back one BGRA texture through a staging copy as packed rows.
  pub fn pixels(gpu: &Gpu, texture: &ID3D11Texture2D, width: u32, height: u32) -> Vec<[u8; 4]> {
    let staging = {
      let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
          Count: 1,
          Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
      };
      let mut staging = None;
      // SAFETY: plain staging texture on the test's own device.
      unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut staging)) }.expect("staging");
      staging.expect("staging texture")
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: same-size BGRA copy, then a read map released before return.
    unsafe {
      gpu.context.CopyResource(&staging, texture);
      gpu
        .context
        .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .expect("map");
    }
    let mut out = Vec::new();
    for y in 0..height as usize {
      // SAFETY: each mapped row spans RowPitch bytes, at least width * 4.
      let row = unsafe {
        std::slice::from_raw_parts(
          mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
          width as usize * 4,
        )
      };
      out.extend(
        row
          .as_chunks::<4>()
          .0
          .iter()
          .map(|p| [p[0], p[1], p[2], p[3]]),
      );
    }
    // SAFETY: paired with the Map above.
    unsafe { gpu.context.Unmap(&staging, 0) };
    out
  }
  /// IEEE half precision, rounded to nearest even, as an FP16 texture stores `value`.
  pub fn f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x7f_ffff;
    let round = |kept: u32, rest: u32, half: u32| {
      if rest > half || (rest == half && kept & 1 == 1) {
        kept + 1
      } else {
        kept
      }
    };
    if exponent >= 31 {
      return sign | 0x7c00;
    }
    if exponent <= 0 {
      if exponent < -10 {
        return sign;
      }
      let full = mantissa | 0x80_0000;
      let shift = (14 - exponent) as u32;
      let kept = full >> shift;
      return sign | round(kept, full & ((1 << shift) - 1), 1 << (shift - 1)) as u16;
    }
    let kept = ((exponent as u32) << 10) | (mantissa >> 13);
    // A carry out of the mantissa correctly moves to the next exponent.
    sign | round(kept, mantissa & 0x1fff, 0x1000) as u16
  }
  /// The value an FP16 texel holds.
  pub fn from_f16(half: u16) -> f32 {
    let sign = if half & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((half >> 10) & 0x1f);
    let mantissa = f32::from(half & 0x3ff);
    sign
      * match exponent {
        0 => mantissa * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + mantissa / 1024.0) * 2f32.powi(exponent - 15),
      }
  }
  /// An FP16 scRGB texture of `width` x `height` texels in rows, alpha 1.
  pub fn scrgb(gpu: &Gpu, width: u32, height: u32, texels: &[[f32; 3]]) -> ID3D11Texture2D {
    assert_eq!(texels.len(), (width * height) as usize);
    let texture = Gpu::texture(
      &gpu.device,
      width,
      height,
      DXGI_FORMAT_R16G16B16A16_FLOAT,
      D3D11_BIND_SHADER_RESOURCE.0 as u32,
    )
    .expect("scRGB texture");
    let data: Vec<u16> = texels
      .iter()
      .flat_map(|&[r, g, b]| [f16(r), f16(g), f16(b), f16(1.0)])
      .collect();
    // SAFETY: the data spans the whole texture at its packed pitch of 8 bytes per texel.
    unsafe {
      gpu
        .context
        .UpdateSubresource(&texture, 0, None, data.as_ptr().cast(), width * 8, 0);
    }
    texture
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;

  #[test]
  fn dxgi_rotations_are_read_exactly_and_unknown_values_are_refused() {
    assert_eq!(
      Orientation::from_dxgi(0).expect("unspecified"),
      Orientation::Upright
    );
    assert_eq!(
      Orientation::from_dxgi(1).expect("identity"),
      Orientation::Upright
    );
    assert_eq!(
      Orientation::from_dxgi(2).expect("90"),
      Orientation::Rotate90
    );
    assert_eq!(
      Orientation::from_dxgi(3).expect("180"),
      Orientation::Rotate180
    );
    assert_eq!(
      Orientation::from_dxgi(4).expect("270"),
      Orientation::Rotate270
    );
    assert!(Orientation::from_dxgi(5).is_err());
    for orientation in [
      Orientation::Rotate90,
      Orientation::Rotate180,
      Orientation::Rotate270,
    ] {
      assert_eq!(
        Orientation::from_dxgi(orientation.dxgi()).expect("round trip"),
        orientation
      );
    }
  }

  #[test]
  fn every_desktop_pixel_maps_to_a_distinct_panel_texel() {
    let (width, height) = (6u32, 10u32);
    for orientation in [
      Orientation::Upright,
      Orientation::Rotate90,
      Orientation::Rotate180,
      Orientation::Rotate270,
    ] {
      let (panel_width, panel_height) = orientation.panel(width, height);
      let mut seen = std::collections::HashSet::new();
      for y in 0..height {
        for x in 0..width {
          let (px, py) = orientation.panel_texel(width, height, x, y);
          assert!(
            px < panel_width && py < panel_height,
            "{orientation:?} ({x}, {y})"
          );
          assert!(seen.insert((px, py)), "{orientation:?} reuses ({px}, {py})");
        }
      }
    }
    // A portrait desktop on a landscape panel: its top-left corner is the panel's
    // bottom-left (90) or top-right (270) texel, as in the Desktop Duplication sample.
    assert_eq!(Orientation::Rotate90.panel_texel(6, 10, 0, 0), (0, 5));
    assert_eq!(Orientation::Rotate270.panel_texel(6, 10, 0, 0), (9, 0));
    assert_eq!(Orientation::Rotate180.panel_texel(6, 10, 0, 0), (5, 9));
  }
}
