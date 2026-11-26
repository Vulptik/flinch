//! Reading the last encoded surface back for the session thumbnail ([`crate::thumbnail`]).
//!
//! It runs at Stop, once the encoder has drained, so nothing writes the surface meanwhile. One
//! staging copy of the surface (5.5 MB for a 1440p NV12 frame) is mapped once and released before
//! returning; the CPU then reads each source pixel once for the box downscale.

use super::gpu::Gpu;
use crate::gif::Nv12;
use crate::thumbnail::{self, Thumbnail};
use crate::{EngineError, ErrorKind, Result};
use std::path::Path;
use windows::Win32::Graphics::{
  Direct3D11::*,
  Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12},
};

/// Write a thumbnail of encoder surface `slot` into `directory`.
pub fn last_frame(gpu: &Gpu, slot: u32, directory: &Path) -> Result<Thumbnail> {
  let surface = gpu
    .surfaces
    .get(slot as usize)
    .ok_or_else(|| EngineError::protocol("Thumbnail surface index is outside the pool"))?;
  let mut desc = D3D11_TEXTURE2D_DESC::default();
  // SAFETY: writes the description of a live pool texture into a local.
  unsafe { surface.GetDesc(&mut desc) };
  if desc.Width < gpu.width || desc.Height < gpu.height {
    return Err(EngineError::protocol(
      "Thumbnail surface is smaller than the encoded extent",
    ));
  }
  let staging_desc = D3D11_TEXTURE2D_DESC {
    Usage: D3D11_USAGE_STAGING,
    BindFlags: 0,
    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
    MiscFlags: 0,
    ..desc
  };
  let mut staging = None;
  // SAFETY: a CPU-readable copy of the surface's own description on the same device.
  unsafe {
    gpu
      .device
      .CreateTexture2D(&staging_desc, None, Some(&mut staging))
  }
  .map_err(|e| EngineError::native("thumbnail staging texture", e))?;
  let staging = staging.ok_or_else(|| {
    EngineError::new(
      ErrorKind::Native,
      "thumbnail staging texture",
      "Native call returned no object",
    )
  })?;
  let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
  // SAFETY: same-description resources on one device; the map is released by `Mapped` below.
  unsafe {
    gpu.context.CopyResource(&staging, surface);
    gpu
      .context
      .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
  }
  .map_err(|e| EngineError::native("thumbnail readback", e))?;
  struct Mapped<'a>(&'a ID3D11DeviceContext, &'a ID3D11Texture2D);
  impl Drop for Mapped<'_> {
    fn drop(&mut self) {
      // SAFETY: paired with the successful Map above, on the same context.
      unsafe { self.0.Unmap(self.1, 0) };
    }
  }
  let _mapped = Mapped(&gpu.context, &staging);
  let pitch = mapped.RowPitch as usize;
  if mapped.pData.is_null() {
    return Err(EngineError::protocol("Thumbnail readback mapped no memory"));
  }
  let pixels = match desc.Format {
    DXGI_FORMAT_NV12 => {
      let rows = desc.Height as usize + (desc.Height as usize).div_ceil(2);
      // SAFETY: a mapped NV12 staging subresource spans `pitch` bytes for each luma and chroma row.
      let plane = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * rows) };
      let picture = Nv12::new(
        plane,
        pitch,
        desc.Height as usize,
        gpu.width as usize,
        gpu.height as usize,
      )?;
      thumbnail::from_nv12(&picture)?
    },
    DXGI_FORMAT_B8G8R8A8_UNORM => {
      // SAFETY: a mapped BGRA staging subresource spans `pitch` bytes for each of its rows.
      let bgra = unsafe {
        std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * desc.Height as usize)
      };
      thumbnail::from_bgra(bgra, pitch, gpu.width, gpu.height)?
    },
    other => {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "thumbnail",
        format!("Encoder surfaces in format {} have no thumbnail", other.0),
      ));
    },
  };
  thumbnail::write(directory, &pixels)
}
