//! Pointer metadata is uploaded; desktop pixels are never mapped or read by the CPU.
#[cfg(test)]
#[path = "cursor_pixels.rs"]
mod pixel_tests;
use super::{capture::PointerShape, gpu::Gpu};
use crate::{EngineError, Result};
use windows::{
  Win32::{
    Graphics::{Direct3D::Fxc::*, Direct3D11::*, Dxgi::Common::*, Gdi::*},
    UI::WindowsAndMessaging::*,
  },
  core::s,
};

/// A display's place on the desktop: left, top, width, height.
pub type DesktopArea = (i32, i32, u32, u32);
pub type PointerRead = fn(DesktopArea) -> Result<Option<PointerShape>>;
/// 32-bit top-down pixels of one cursor bitmap.
struct Raster {
  width: u32,
  height: u32,
  pixels: Vec<u8>,
}
struct IconBitmaps(ICONINFO);
impl Drop for IconBitmaps {
  fn drop(&mut self) {
    // SAFETY: GetIconInfo gave this owner both bitmaps; an absent color bitmap is skipped.
    unsafe {
      for bitmap in [self.0.hbmColor, self.0.hbmMask] {
        if !bitmap.is_invalid() {
          let _ = DeleteObject(bitmap.into());
        }
      }
    }
  }
}
struct MemoryDc(HDC);
impl Drop for MemoryDc {
  fn drop(&mut self) {
    // SAFETY: created by CreateCompatibleDC for this owner alone.
    let _ = unsafe { DeleteDC(self.0) };
  }
}
fn read_bitmap(bitmap: HBITMAP) -> Result<Raster> {
  let mut header = BITMAP::default();
  // SAFETY: the buffer is exactly one BITMAP; the handle stays owned by the caller.
  let copied = unsafe {
    GetObjectW(
      bitmap.into(),
      size_of::<BITMAP>() as i32,
      Some((&raw mut header).cast()),
    )
  };
  let (width, height) = (header.bmWidth, header.bmHeight);
  if copied == 0 || !(1..=1024).contains(&width) || !(1..=2048).contains(&height) {
    return Err(EngineError::protocol("Unsupported cursor bitmap"));
  }
  let mut info = BITMAPINFO {
    bmiHeader: BITMAPINFOHEADER {
      biSize: size_of::<BITMAPINFOHEADER>() as u32,
      biWidth: width,
      biHeight: -height,
      biPlanes: 1,
      biBitCount: 32,
      biCompression: BI_RGB.0,
      ..Default::default()
    },
    ..Default::default()
  };
  let (width, height) = (width as u32, height as u32);
  let mut pixels = vec![0; width as usize * height as usize * 4];
  // SAFETY: a null source DC requests a memory DC, which its guard releases.
  let dc = MemoryDc(unsafe { CreateCompatibleDC(None) });
  // SAFETY: the buffer holds exactly the 32-bit rows requested in `info`.
  let lines = unsafe {
    GetDIBits(
      dc.0,
      bitmap,
      0,
      height,
      Some(pixels.as_mut_ptr().cast()),
      &mut info,
      DIB_RGB_COLORS,
    )
  };
  if lines != height as i32 {
    return Err(EngineError::protocol("Cursor bitmap could not be read"));
  }
  Ok(Raster {
    width,
    height,
    pixels,
  })
}
/// Convert a cursor's bitmaps to the shape layouts duplication delivers. Color pixels become the
/// premultiplied form the compositor blends; cursors without alpha take it from their mask.
fn shape_from_rasters(color: Option<Raster>, mask: Raster) -> Result<PointerShape> {
  let Some(mut color) = color else {
    if !mask.height.is_multiple_of(2) {
      return Err(EngineError::protocol("Odd monochrome cursor bitmap"));
    }
    let pitch = mask.width.div_ceil(8);
    let mut bytes = vec![0; pitch as usize * mask.height as usize];
    for (index, pixel) in mask.pixels.as_chunks::<4>().0.iter().enumerate() {
      if pixel[0] != 0 {
        let (y, x) = (index / mask.width as usize, index % mask.width as usize);
        bytes[y * pitch as usize + x / 8] |= 0x80 >> (x % 8);
      }
    }
    return Ok(PointerShape {
      kind: 1,
      width: mask.width,
      height: mask.height,
      pitch,
      bytes,
      ..Default::default()
    });
  };
  if mask.pixels.len() < color.pixels.len() {
    return Err(EngineError::protocol("Cursor mask smaller than its image"));
  }
  let has_alpha = color
    .pixels
    .as_chunks::<4>()
    .0
    .iter()
    .any(|pixel| pixel[3] != 0);
  for (pixel, mask) in color
    .pixels
    .as_chunks_mut::<4>()
    .0
    .iter_mut()
    .zip(mask.pixels.as_chunks::<4>().0)
  {
    if !has_alpha {
      pixel[3] = if mask[0] == 0 { 255 } else { 0 };
    }
    let alpha = u32::from(pixel[3]);
    for channel in &mut pixel[..3] {
      *channel = (u32::from(*channel) * alpha / 255) as u8;
    }
  }
  Ok(PointerShape {
    kind: 2,
    width: color.width,
    height: color.height,
    pitch: color.width * 4,
    bytes: color.pixels,
    ..Default::default()
  })
}
/// The cursor Windows shows now, placed on `area`. Duplication delivers a shape only when it
/// changes, so a recording that starts with an unchanged cursor would otherwise have none to
/// draw. `None` means no cursor is showing.
pub fn system_pointer(area: DesktopArea) -> Result<Option<PointerShape>> {
  let mut cursor = CURSORINFO {
    cbSize: size_of::<CURSORINFO>() as u32,
    ..Default::default()
  };
  // SAFETY: `cursor` is initialized with its size and written only during this call.
  unsafe { GetCursorInfo(&mut cursor) }.map_err(|e| EngineError::native("cursor info", e))?;
  pointer_from(&cursor, area)
}
/// The shape of `cursor` as Windows reported it, placed on `area`.
fn pointer_from(cursor: &CURSORINFO, area: DesktopArea) -> Result<Option<PointerShape>> {
  if cursor.flags.0 & CURSOR_SHOWING.0 == 0 || cursor.hCursor.is_invalid() {
    return Ok(None);
  }
  let mut icon = ICONINFO::default();
  // SAFETY: the system handle is only read; returned bitmaps pass to the guard below.
  unsafe { GetIconInfo(HICON(cursor.hCursor.0), &mut icon) }
    .map_err(|e| EngineError::native("cursor bitmaps", e))?;
  let bitmaps = IconBitmaps(icon);
  let mask = read_bitmap(bitmaps.0.hbmMask)?;
  let color = if bitmaps.0.hbmColor.is_invalid() {
    None
  } else {
    Some(read_bitmap(bitmaps.0.hbmColor)?)
  };
  let (left, top, width, height) = area;
  let point = cursor.ptScreenPos;
  let inside = |value: i32, start: i32, length: u32| {
    value >= start && i64::from(value) < i64::from(start) + i64::from(length)
  };
  Ok(Some(PointerShape {
    x: point
      .x
      .saturating_sub(left)
      .saturating_sub_unsigned(bitmaps.0.xHotspot),
    y: point
      .y
      .saturating_sub(top)
      .saturating_sub_unsigned(bitmaps.0.yHotspot),
    visible: inside(point.x, left, width) && inside(point.y, top, height),
    ..shape_from_rasters(color, mask)?
  }))
}

pub fn validate(shape: &PointerShape) -> Result<u32> {
  let height = if shape.kind == 1 {
    if !shape.height.is_multiple_of(2) {
      return Err(EngineError::protocol("Odd monochrome pointer height"));
    }
    shape.height / 2
  } else {
    shape.height
  };
  if !matches!(shape.kind, 1 | 2 | 4)
    || shape.width == 0
    || height == 0
    || shape.width > 1024
    || height > 1024
  {
    return Err(EngineError::protocol("Unsupported pointer shape metadata"));
  }
  let minimum = if shape.kind == 1 {
    shape.width.div_ceil(8)
  } else {
    shape
      .width
      .checked_mul(4)
      .ok_or_else(|| EngineError::protocol("Pointer pitch overflow"))?
  };
  if shape.pitch < minimum
    || u64::from(shape.pitch) * u64::from(shape.height) > shape.bytes.len() as u64
  {
    return Err(EngineError::protocol("Truncated pointer shape"));
  }
  Ok(height)
}
/// The pointer is drawn on the whole-display cache before any crop, so it is bounded by the
/// display, not by the encoded region.
fn shader_constants(
  shape: &PointerShape,
  height: u32,
  source_width: u32,
  source_height: u32,
) -> [u32; 8] {
  [
    shape.x as u32,
    shape.y as u32,
    shape.width,
    height,
    shape.kind,
    shape.pitch,
    source_width,
    source_height,
  ]
}
pub struct CursorCompositor {
  shader: ID3D11ComputeShader,
  constants: ID3D11Buffer,
  desktop: ID3D11ShaderResourceView,
  target: ID3D11UnorderedAccessView,
  cached: Option<(u64, ID3D11ShaderResourceView)>,
}
/// Device-independent HLSL compilation, started early so it overlaps GPU and capture creation.
pub struct ShaderCompile<T = Vec<u8>>(std::sync::mpsc::Receiver<Result<T>>, &'static str);
impl ShaderCompile {
  pub fn start() -> Result<Self> {
    Self::spawn("video cursor shader", "cursor shader compile", compile)
  }
}
impl<T: Send + 'static> ShaderCompile<T> {
  /// Runs `work` on its own thread; `stage` names it in errors.
  pub fn spawn(thread: &str, stage: &'static str, work: fn() -> Result<T>) -> Result<Self> {
    let (done, result) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
      .name(thread.into())
      .spawn(move || {
        // One result fits without waiting; an abandoned Prepare simply drops it.
        let _ = done.send(work());
      })
      .map_err(|e| EngineError::io(stage, e))?;
    Ok(Self(result, stage))
  }
  pub fn wait(self, timeout: std::time::Duration) -> Result<T> {
    let stage = self.1;
    self.0.recv_timeout(timeout).map_err(|error| {
      EngineError::new(
        if matches!(error, std::sync::mpsc::RecvTimeoutError::Timeout) {
          crate::ErrorKind::Timeout
        } else {
          crate::ErrorKind::Native
        },
        stage,
        error.to_string(),
      )
    })?
  }
}
pub fn compile() -> Result<Vec<u8>> {
  compile_hlsl(
    include_str!("cursor.hlsl"),
    c"main",
    c"cs_5_0",
    "cursor shader compile",
  )
}
/// Compiles embedded HLSL to bytecode for one entry point and shader model.
pub fn compile_hlsl(
  code: &str,
  entry: &std::ffi::CStr,
  target: &std::ffi::CStr,
  stage: &'static str,
) -> Result<Vec<u8>> {
  let mut blob = None;
  let mut errors = None;
  // SAFETY: immutable embedded HLSL; compiler receives no include handler or external file paths.
  // Both names are NUL-terminated C strings that outlive the call.
  unsafe {
    D3DCompile(
      code.as_ptr().cast(),
      code.len(),
      s!("flinch shader"),
      None,
      None,
      windows::core::PCSTR(entry.as_ptr().cast()),
      windows::core::PCSTR(target.as_ptr().cast()),
      D3DCOMPILE_OPTIMIZATION_LEVEL3,
      0,
      &mut blob,
      Some(&mut errors),
    )
  }
  .map_err(|e| EngineError::native(stage, e))?;
  let blob = blob.ok_or_else(|| EngineError::protocol("No compiled shader"))?;
  // SAFETY: the blob owns its bytecode until dropped at the end of this copy.
  Ok(
    unsafe {
      std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
    .to_vec(),
  )
}
impl CursorCompositor {
  pub fn new(gpu: &Gpu) -> Result<Self> {
    Self::with_bytecode(gpu, &compile()?)
  }
  pub fn with_bytecode(gpu: &Gpu, bytes: &[u8]) -> Result<Self> {
    let mut shader = None;
    let mut constants = None;
    let mut desktop = None;
    let mut target = None;
    // SAFETY: D3D copies the bytecode during creation. Views retain their textures.
    unsafe {
      gpu
        .device
        .CreateComputeShader(bytes, None, Some(&mut shader))
        .map_err(|e| EngineError::native("cursor shader", e))?;
      gpu
        .device
        .CreateBuffer(
          &D3D11_BUFFER_DESC {
            ByteWidth: 32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
          },
          None,
          Some(&mut constants),
        )
        .map_err(|e| EngineError::native("cursor constants", e))?;
      gpu
        .device
        .CreateShaderResourceView(&gpu.desktop, None, Some(&mut desktop))
        .map_err(|e| EngineError::native("cursor desktop view", e))?;
      gpu
        .device
        .CreateUnorderedAccessView(&gpu.scratch, None, Some(&mut target))
        .map_err(|e| EngineError::native("cursor target view", e))?;
    }
    Ok(Self {
      shader: shader.ok_or_else(|| EngineError::protocol("No cursor shader"))?,
      constants: constants.ok_or_else(|| EngineError::protocol("No cursor constants"))?,
      desktop: desktop.ok_or_else(|| EngineError::protocol("No desktop SRV"))?,
      target: target.ok_or_else(|| EngineError::protocol("No cursor UAV"))?,
      cached: None,
    })
  }
  pub fn compose<'a>(&mut self, gpu: &'a Gpu, shape: &PointerShape) -> Result<&'a ID3D11Texture2D> {
    if !shape.visible || shape.bytes.is_empty() {
      return Ok(&gpu.desktop);
    }
    let height = validate(shape)?;
    let desc = D3D11_TEXTURE2D_DESC {
      Width: shape.pitch,
      Height: shape.height,
      MipLevels: 1,
      ArraySize: 1,
      Format: DXGI_FORMAT_R8_UINT,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Usage: D3D11_USAGE_IMMUTABLE,
      BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
      ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
      pSysMem: shape.bytes.as_ptr().cast(),
      SysMemPitch: shape.pitch,
      ..Default::default()
    };
    let mut texture = None;
    let mut view = None;
    // SAFETY: shape extent was validated against its byte allocation. Only pointer metadata is uploaded.
    unsafe {
      if self
        .cached
        .as_ref()
        .is_none_or(|(generation, _)| *generation != shape.generation)
      {
        gpu
          .device
          .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
          .map_err(|e| EngineError::native("pointer upload", e))?;
        let texture = texture.ok_or_else(|| EngineError::protocol("No pointer texture"))?;
        gpu
          .device
          .CreateShaderResourceView(&texture, None, Some(&mut view))
          .map_err(|e| EngineError::native("pointer view", e))?;
        self.cached = Some((
          shape.generation,
          view.ok_or_else(|| EngineError::protocol("No pointer view"))?,
        ));
      }
      let view = self.cached.as_ref().map(|(_, v)| v.clone());
      let constants = shader_constants(shape, height, gpu.source_width, gpu.source_height);
      gpu.context.CopyResource(&gpu.scratch, &gpu.desktop);
      gpu
        .context
        .UpdateSubresource(&self.constants, 0, None, constants.as_ptr().cast(), 0, 0);
      gpu.context.CSSetShader(&self.shader, None);
      gpu
        .context
        .CSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
      gpu
        .context
        .CSSetShaderResources(0, Some(&[Some(self.desktop.clone()), view]));
      gpu
        .context
        .CSSetUnorderedAccessViews(0, 1, Some(&Some(self.target.clone())), None);
      gpu
        .context
        .Dispatch(shape.width.div_ceil(8), height.div_ceil(8), 1);
      gpu.context.CSSetShaderResources(0, Some(&[None, None]));
      gpu
        .context
        .CSSetUnorderedAccessViews(0, 1, Some(&None), None);
      gpu.context.CSSetShader(None, None);
    }
    Ok(&gpu.scratch)
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn rejects_truncated_and_invalid_cursor_metadata() {
    let mut s = PointerShape {
      kind: 1,
      width: 16,
      height: 32,
      pitch: 2,
      bytes: vec![0; 64],
      ..Default::default()
    };
    assert_eq!(validate(&s).expect("valid test fixture"), 16);
    s.bytes.pop();
    assert!(validate(&s).is_err());
    s.kind = 4;
    s.width = 2;
    s.height = 2;
    s.pitch = 8;
    s.bytes = vec![0; 16];
    assert_eq!(validate(&s).expect("valid test fixture"), 2);
    s.kind = 9;
    assert!(validate(&s).is_err());
  }
  #[test]
  fn a_cropped_recording_bounds_the_cursor_by_the_display_not_the_region() {
    let shape = PointerShape {
      kind: 2,
      width: 32,
      pitch: 128,
      x: 1700,
      y: 900,
      ..Default::default()
    };
    assert_eq!(
      shader_constants(&shape, 32, 2560, 1440),
      [1700, 900, 32, 32, 2, 128, 2560, 1440]
    );
  }
  fn raster(width: u32, height: u32, pixels: &[[u8; 4]]) -> Raster {
    Raster {
      width,
      height,
      pixels: pixels.concat(),
    }
  }
  #[test]
  fn color_cursors_become_premultiplied_shapes_the_compositor_accepts() {
    let shape = shape_from_rasters(
      Some(raster(2, 1, &[[200, 100, 50, 128], [9, 9, 9, 0]])),
      raster(2, 1, &[[0; 4], [255; 4]]),
    )
    .expect("color cursor");
    assert_eq!((shape.kind, shape.pitch), (2, 8));
    assert_eq!(shape.bytes, [100, 50, 25, 128, 0, 0, 0, 0]);
    assert_eq!(validate(&shape).expect("valid shape"), 1);
  }
  #[test]
  fn color_cursors_without_alpha_take_their_shape_from_the_mask() {
    let shape = shape_from_rasters(
      Some(raster(2, 1, &[[10, 20, 30, 0], [40, 50, 60, 0]])),
      raster(2, 1, &[[0; 4], [255; 4]]),
    )
    .expect("legacy cursor");
    assert_eq!(shape.bytes, [10, 20, 30, 255, 0, 0, 0, 0]);
  }
  #[test]
  fn monochrome_cursors_pack_both_masks_most_significant_bit_first() {
    let (on, off) = ([255; 4], [0; 4]);
    let shape = shape_from_rasters(
      None,
      raster(
        9,
        2,
        &[
          on, off, off, off, off, off, off, off, on, off, on, off, off, off, off, off, off, off,
        ],
      ),
    )
    .expect("monochrome cursor");
    assert_eq!((shape.kind, shape.pitch, shape.height), (1, 2, 2));
    assert_eq!(shape.bytes, [0x80, 0x80, 0x40, 0]);
    assert_eq!(validate(&shape).expect("valid shape"), 1);
    assert!(shape_from_rasters(None, raster(1, 3, &[off, off, off])).is_err());
  }
  #[test]
  fn a_mask_smaller_than_its_image_is_refused() {
    let result = shape_from_rasters(
      Some(raster(2, 1, &[[0; 4], [0; 4]])),
      raster(1, 1, &[[0; 4]]),
    );
    assert!(result.is_err());
  }
  #[test]
  fn cursor_bitmaps_are_read_top_down_and_unusable_handles_are_refused() {
    let bits = [0x80u8, 0, 0, 0x80];
    // SAFETY: the rows are two 16-bit-aligned 1bpp scanlines, copied during creation.
    let bitmap = unsafe { CreateBitmap(8, 2, 1, 1, Some(bits.as_ptr().cast())) };
    let raster = read_bitmap(bitmap).expect("readable bitmap");
    // SAFETY: created above and no longer selected or used.
    let _ = unsafe { DeleteObject(bitmap.into()) };
    assert_eq!((raster.width, raster.height), (8, 2));
    assert_ne!(raster.pixels[0], raster.pixels[4], "first row differs");
    assert_eq!(
      raster.pixels[8 * 4],
      raster.pixels[4],
      "second row starts off"
    );
    assert!(read_bitmap(HBITMAP::default()).is_err());
  }
  fn showing(cursor: HCURSOR, x: i32, y: i32) -> CURSORINFO {
    CURSORINFO {
      cbSize: size_of::<CURSORINFO>() as u32,
      flags: CURSOR_SHOWING,
      hCursor: cursor,
      ptScreenPos: windows::Win32::Foundation::POINT { x, y },
    }
  }
  #[test]
  fn reported_cursors_are_placed_by_their_hotspot_and_hidden_ones_have_no_shape() {
    // A 32 x 32 monochrome cursor (mask only) with its hotspot at (3, 5).
    let and = [0xffu8; 128];
    let xor = [0u8; 128];
    // SAFETY: both planes hold 32 rows of 32 1bpp pixels, copied during creation.
    let monochrome =
      unsafe { CreateCursor(None, 3, 5, 32, 32, and.as_ptr().cast(), xor.as_ptr().cast()) }
        .expect("monochrome cursor");
    // A 2 x 2 opaque color cursor with its hotspot at (1, 1).
    let pixels = [[0x10u8, 0x20, 0x30, 0xff]; 4];
    // SAFETY: four 32bpp pixels and two 16-bit-aligned 1bpp rows, copied during creation.
    let (color, mask) = unsafe {
      (
        CreateBitmap(2, 2, 1, 32, Some(pixels.as_ptr().cast())),
        CreateBitmap(2, 2, 1, 1, Some([0u8; 4].as_ptr().cast())),
      )
    };
    let icon = ICONINFO {
      fIcon: false.into(),
      xHotspot: 1,
      yHotspot: 1,
      hbmMask: mask,
      hbmColor: color,
    };
    // SAFETY: the icon copies both bitmaps, which are deleted right after.
    let colored = unsafe { CreateIconIndirect(&icon) }.expect("color cursor");
    // SAFETY: created above and copied into the cursor.
    unsafe {
      let _ = DeleteObject(color.into());
      let _ = DeleteObject(mask.into());
    }
    let area = (100, 200, 640, 360);
    let shape = pointer_from(&showing(monochrome, 110, 230), area)
      .expect("monochrome shape")
      .expect("showing");
    assert_eq!(
      (shape.kind, shape.x, shape.y, shape.visible),
      (1, 7, 25, true)
    );
    validate(&shape).expect("valid monochrome shape");
    let shape = pointer_from(&showing(HCURSOR(colored.0), 99, 230), area)
      .expect("color shape")
      .expect("showing");
    assert_eq!((shape.kind, shape.width, shape.height), (2, 2, 2));
    assert_eq!((shape.x, shape.y, shape.visible), (-2, 29, false));
    validate(&shape).expect("valid color shape");
    let hidden = CURSORINFO {
      flags: CURSORINFO_FLAGS(0),
      ..showing(monochrome, 110, 230)
    };
    assert!(pointer_from(&hidden, area).expect("hidden").is_none());
    let absent = showing(HCURSOR::default(), 110, 230);
    assert!(pointer_from(&absent, area).expect("no cursor").is_none());
    // SAFETY: both were created above and are no longer used.
    unsafe {
      DestroyCursor(monochrome).expect("destroy monochrome");
      DestroyIcon(colored).expect("destroy color");
    }
  }
  #[test]
  fn reading_the_system_cursor_never_fails_for_a_desktop_area() {
    let shape = system_pointer((0, 0, 1, 1)).expect("system cursor");
    if let Some(shape) = shape {
      validate(&shape).expect("valid shape");
    }
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod compile_tests {
  use super::*;
  #[test]
  fn early_compile_returns_the_same_bytecode_and_its_wait_is_bounded() {
    let early = ShaderCompile::start()
      .expect("compile thread")
      .wait(crate::limits::PREPARE_TIMEOUT)
      .expect("bytecode");
    assert!(!early.is_empty());
    assert_eq!(early, compile().expect("direct compile"));
    // A compile that never reports cannot hold Prepare beyond its bound.
    let (_held, result) = std::sync::mpsc::sync_channel::<Result<Vec<u8>>>(1);
    let started = std::time::Instant::now();
    let stalled = ShaderCompile(result, "cursor shader compile")
      .wait(std::time::Duration::from_millis(20))
      .expect_err("bounded wait");
    assert_eq!(stalled.kind, crate::ErrorKind::Timeout);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    // A compile thread that ends without a result is an error, never empty bytecode.
    let (lost, result) = std::sync::mpsc::sync_channel::<Result<Vec<u8>>>(1);
    drop(lost);
    let error = ShaderCompile(result, "cursor shader compile")
      .wait(crate::limits::PREPARE_TIMEOUT)
      .expect_err("lost compile");
    assert_eq!(error.stage, "cursor shader compile");
  }
}
