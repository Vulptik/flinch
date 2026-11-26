use crate::{EngineError, ErrorKind, Result, limits, types::*};
use std::mem::ManuallyDrop;
use windows::{
  Win32::{
    Foundation::{HMODULE, RECT},
    Graphics::{
      Direct3D::*,
      Direct3D10::ID3D10Multithread,
      Direct3D11::*,
      Dxgi::{Common::*, *},
    },
    System::SystemInformation::OSVERSIONINFOW,
  },
  core::Interface,
};

#[link(name = "ntdll")]
unsafe extern "system" {
  fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
}
fn required<T>(value: Option<T>, stage: &str) -> Result<T> {
  value.ok_or_else(|| EngineError::new(ErrorKind::Native, stage, "Native call returned no object"))
}
pub fn os_build() -> Result<u32> {
  let mut info = OSVERSIONINFOW {
    dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
    ..Default::default()
  };
  // SAFETY: initialized OSVERSIONINFOW with correct ABI size, only live during call.
  let status = unsafe { RtlGetVersion(&mut info) };
  if status < 0 {
    return Err(EngineError::new(
      ErrorKind::Native,
      "OS version",
      format!("NTSTATUS {status}"),
    ));
  }
  Ok(info.dwBuildNumber)
}
pub struct Output {
  pub info: MonitorInfo,
  pub adapter: IDXGIAdapter1,
  pub output: IDXGIOutput6,
}
pub fn outputs() -> Result<Vec<Output>> {
  // SAFETY: owned COM enumerator; no device/capture session is opened here.
  let factory: IDXGIFactory1 =
    unsafe { CreateDXGIFactory1() }.map_err(|e| EngineError::native("DXGI factory", e))?;
  let mut outputs = Vec::new();
  for a in 0..32 {
    // SAFETY: bounded enumeration on a live factory.
    let adapter = match unsafe { factory.EnumAdapters1(a) } {
      Ok(v) => v,
      Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
      Err(e) => return Err(EngineError::native("DXGI adapter", e)),
    };
    // SAFETY: live adapter with writable native output descriptors.
    let desc =
      unsafe { adapter.GetDesc1() }.map_err(|e| EngineError::native("adapter description", e))?;
    let adapter_name = String::from_utf16_lossy(
      &desc.Description[..desc
        .Description
        .iter()
        .position(|x| *x == 0)
        .unwrap_or(desc.Description.len())],
    );
    let luid = ((i64::from(desc.AdapterLuid.HighPart)) << 32) | i64::from(desc.AdapterLuid.LowPart);
    // SAFETY: read-only version query; unavailable versions are represented explicitly.
    let driver = match unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) } {
      Ok(v) => format!(
        "{}.{}.{}.{}",
        (v >> 48) & 65535,
        (v >> 32) & 65535,
        (v >> 16) & 65535,
        v & 65535
      ),
      Err(e) => format!("unavailable: {e}"),
    };
    for o in 0..64 {
      // SAFETY: bounded output enumeration on this live adapter.
      let output = match unsafe { adapter.EnumOutputs(o) } {
        Ok(v) => v,
        Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
        Err(e) => return Err(EngineError::native("DXGI output", e)),
      };
      let output: IDXGIOutput6 = output
        .cast()
        .map_err(|e| EngineError::native("HDR-aware output", e))?;
      // SAFETY: immutable output query, no frame capture.
      let d =
        unsafe { output.GetDesc1() }.map_err(|e| EngineError::native("output description", e))?;
      if !d.AttachedToDesktop.as_bool() {
        continue;
      }
      let rect = d.DesktopCoordinates;
      let name = String::from_utf16_lossy(
        &d.DeviceName[..d
          .DeviceName
          .iter()
          .position(|x| *x == 0)
          .unwrap_or(d.DeviceName.len())],
      );
      outputs.push(Output {
        info: MonitorInfo {
          id: name,
          adapter_luid: luid,
          adapter: adapter_name.clone(),
          vendor_id: desc.VendorId,
          driver: driver.clone(),
          width: (rect.right - rect.left) as u32,
          height: (rect.bottom - rect.top) as u32,
          x: rect.left,
          y: rect.top,
          rotation: d.Rotation.0 as u32,
          color_space: d.ColorSpace.0 as u32,
          hdr: d.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
          refresh_hz: refresh_hz(&d.DeviceName),
        },
        adapter: adapter.clone(),
        output,
      });
    }
  }
  Ok(outputs)
}
/// DWM's composition period in 100 ns: the clock that stamps duplication presents. It can differ
/// from an output's mode rate (measured: a 60 Hz indirect display composed at the
/// 144 Hz primary's cadence). `None` when DWM reports no period.
pub fn composition_period() -> Result<Option<u64>> {
  let mut info = windows::Win32::Graphics::Dwm::DWM_TIMING_INFO {
    cbSize: std::mem::size_of::<windows::Win32::Graphics::Dwm::DWM_TIMING_INFO>() as u32,
    ..Default::default()
  };
  // SAFETY: a null window asks for the desktop composition timing; the struct is stack-owned
  // and its size is advertised.
  unsafe {
    windows::Win32::Graphics::Dwm::DwmGetCompositionTimingInfo(
      windows::Win32::Foundation::HWND::default(),
      &mut info,
    )
  }
  .map_err(|e| EngineError::native("DWM composition timing", e))?;
  if info.qpcRefreshPeriod == 0 {
    return Ok(None);
  }
  let raw = i64::try_from(info.qpcRefreshPeriod)
    .map_err(|_| EngineError::protocol("DWM refresh period overflows QPC"))?;
  Ok(Some(super::capture::qpc_raw_to_media(raw)?))
}
/// The display's current refresh rate. Windows reports 0 or 1 for "hardware default", which
/// carries no rate; the caller then treats the display as of unknown cadence.
fn refresh_hz(device: &[u16; 32]) -> Option<u32> {
  let mut mode = windows::Win32::Graphics::Gdi::DEVMODEW {
    dmSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::DEVMODEW>() as u16,
    ..Default::default()
  };
  // SAFETY: the device name is the output's NUL-terminated GDI name and the mode is stack-owned.
  let found = unsafe {
    windows::Win32::Graphics::Gdi::EnumDisplaySettingsW(
      windows::core::PCWSTR(device.as_ptr()),
      windows::Win32::Graphics::Gdi::ENUM_CURRENT_SETTINGS,
      &mut mode,
    )
  };
  (found.as_bool() && mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
}
pub fn doctor() -> Result<CapabilityReport> {
  let _com = super::Apartment::new()?;
  let mut diagnostics = Vec::new();
  let amf_runtime = match super::amf::version() {
    Ok(v) => Some(v),
    Err(e) => {
      diagnostics.push(e.to_string());
      None
    },
  };
  // Absent vendor runtimes are expected on most machines: reported as None, not diagnostics.
  let nvenc_api = super::nvenc::version().ok();
  let qsv_runtimes = match super::qsv::inventory() {
    Ok(found) => found,
    Err(e) => {
      diagnostics.push(e.to_string());
      None
    },
  };
  let os_build = os_build()?;
  if os_build < 19041 {
    diagnostics.push("Windows build 19041 or newer is required".into());
  }
  Ok(CapabilityReport {
    os_build,
    architecture: std::env::consts::ARCH.into(),
    monitors: outputs()?.into_iter().map(|o| o.info).collect(),
    amf_runtime,
    nvenc_api,
    qsv_runtimes,
    diagnostics,
  })
}

pub struct Gpu {
  pub device: ID3D11Device,
  pub context: ID3D11DeviceContext,
  pub desktop: ID3D11Texture2D,
  pub scratch: ID3D11Texture2D,
  /// Encoder surfaces at the encoded extent: NV12 from the video processor, or BGRA for the CPU
  /// conversion, which the software encoder reads back and converts itself.
  pub surfaces: Vec<ID3D11Texture2D>,
  converter: Converter,
  /// Captured display extent: desktop/scratch caches and source validation.
  pub source_width: u32,
  pub source_height: u32,
  /// Encoded extent: the region when cropping, otherwise the display.
  pub width: u32,
  pub height: u32,
  pub bytes: u64,
  pub fps: u32,
}
/// How a surface is filled from a desktop cache.
enum Converter {
  Video(VideoConverter),
  /// The area of the cache copied into a BGRA surface.
  Cpu(D3D11_BOX),
}
struct VideoConverter {
  _video_device: ID3D11VideoDevice,
  video_context: ID3D11VideoContext,
  _enumerator: ID3D11VideoProcessorEnumerator,
  processor: ID3D11VideoProcessor,
  input_view: ID3D11VideoProcessorInputView,
  scratch_view: ID3D11VideoProcessorInputView,
  output_views: Vec<ID3D11VideoProcessorOutputView>,
}
/// Encoded extent, texture budget and surface count for a display and optional crop. The region
/// must fit.
pub fn crop(
  source_width: u32,
  source_height: u32,
  region: Option<Region>,
  conversion: ConversionKind,
  fps: u32,
) -> Result<(u64, usize, u32, u32)> {
  let (width, height) = match region {
    Some(region) => {
      region.within(source_width, source_height)?;
      (region.width, region.height)
    },
    None => (source_width, source_height),
  };
  let (bytes, surfaces) =
    limits::pool(source_width, source_height, width, height, conversion, fps)?;
  Ok((bytes, surfaces, width, height))
}
impl Gpu {
  /// The video processor converts on hardware adapters. An adapter that refuses video support
  /// (the basic display adapter of machines and VMs without a GPU: DXGI_ERROR_UNSUPPORTED,
  /// measured) gets a plain device and the CPU conversion; any other failure stands.
  pub fn new(
    adapter: &IDXGIAdapter1,
    width: u32,
    height: u32,
    region: Option<Region>,
    fps: u32,
  ) -> Result<Self> {
    match Self::device(
      adapter,
      D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    ) {
      Ok((device, context)) => Self::from_device(device, context, width, height, region, fps),
      Err(error) if error.native_code == Some(DXGI_ERROR_UNSUPPORTED.0) => {
        let (device, context) = Self::device(adapter, D3D11_CREATE_DEVICE_BGRA_SUPPORT)?;
        Self::from_cpu_device(device, context, width, height, region, fps)
      },
      Err(error) => Err(error),
    }
  }
  fn device(
    adapter: &IDXGIAdapter1,
    flags: D3D11_CREATE_DEVICE_FLAG,
  ) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: matching adapter, output pointers are stack-owned; no SINGLETHREADED flag is used.
    unsafe {
      D3D11CreateDevice(
        adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        flags,
        Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
      )
    }
    .map_err(|e| EngineError::native("D3D11 device", e))?;
    Ok((
      required(device, "D3D device")?,
      required(context, "D3D context")?,
    ))
  }
  /// Whether surfaces are converted on the GPU or read back as BGRA.
  pub fn conversion(&self) -> ConversionKind {
    match self.converter {
      Converter::Video(_) => ConversionKind::Gpu,
      Converter::Cpu(_) => ConversionKind::Cpu,
    }
  }
  /// Encoders share the device across threads.
  fn protect(context: &ID3D11DeviceContext) -> Result<()> {
    let multithread: ID3D10Multithread = context
      .cast()
      .map_err(|e| EngineError::native("D3D multithread", e))?;
    // SAFETY: set before any runtime encoder begins sharing this device.
    let _previous_state = unsafe { multithread.SetMultithreadProtected(true) };
    // SAFETY: read back the enabled state rather than treating the previous-state return as success.
    if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
      return Err(EngineError::protocol(
        "D3D multithread protection was not enabled",
      ));
    }
    Ok(())
  }
  /// The desktop cache and the cursor scratch copy, both at the display's extent.
  fn caches(
    device: &ID3D11Device,
    source_width: u32,
    source_height: u32,
  ) -> Result<(ID3D11Texture2D, ID3D11Texture2D)> {
    let desktop = Self::texture(
      device,
      source_width,
      source_height,
      DXGI_FORMAT_B8G8R8A8_UNORM,
      (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
    )?;
    let scratch = Self::texture(
      device,
      source_width,
      source_height,
      DXGI_FORMAT_B8G8R8A8_UNORM,
      (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_RENDER_TARGET).0
        as u32,
    )?;
    Ok((desktop, scratch))
  }
  /// The GPU-less tier: the same caches, and BGRA surfaces filled by a copy of the encoded area.
  pub(super) fn from_cpu_device(
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    source_width: u32,
    source_height: u32,
    region: Option<Region>,
    fps: u32,
  ) -> Result<Self> {
    let (bytes, count, width, height) = crop(
      source_width,
      source_height,
      region,
      ConversionKind::Cpu,
      fps,
    )?;
    Self::protect(&context)?;
    let (desktop, scratch) = Self::caches(&device, source_width, source_height)?;
    let mut surfaces = Vec::with_capacity(count);
    for _ in 0..count {
      surfaces.push(Self::texture(
        &device,
        width,
        height,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_BIND_SHADER_RESOURCE.0 as u32,
      )?);
    }
    let area = region.map_or(
      D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: width,
        bottom: height,
        back: 1,
      },
      |region| D3D11_BOX {
        left: region.x,
        top: region.y,
        front: 0,
        right: region.x + region.width,
        bottom: region.y + region.height,
        back: 1,
      },
    );
    Ok(Self {
      device,
      context,
      desktop,
      scratch,
      surfaces,
      converter: Converter::Cpu(area),
      source_width,
      source_height,
      width,
      height,
      bytes,
      fps,
    })
  }
  pub(super) fn from_device(
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    source_width: u32,
    source_height: u32,
    region: Option<Region>,
    fps: u32,
  ) -> Result<Self> {
    let (bytes, count, width, height) = crop(
      source_width,
      source_height,
      region,
      ConversionKind::Gpu,
      fps,
    )?;
    Self::protect(&context)?;
    let video_device: ID3D11VideoDevice = device
      .cast()
      .map_err(|e| EngineError::native("video device", e))?;
    let video_context: ID3D11VideoContext = context
      .cast()
      .map_err(|e| EngineError::native("video context", e))?;
    let rate = DXGI_RATIONAL {
      Numerator: fps,
      Denominator: 1,
    };
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
      InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
      InputFrameRate: rate,
      InputWidth: source_width,
      InputHeight: source_height,
      OutputFrameRate: rate,
      OutputWidth: width,
      OutputHeight: height,
      Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    // SAFETY: descriptor describes fixed even-sized SDR progressive video.
    let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
      .map_err(|e| EngineError::native("video processor enumeration", e))?;
    // SAFETY: query support before allocating encoder surfaces.
    let input = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }
      .map_err(|e| EngineError::native("BGRA support", e))?;
    // SAFETY: same read-only format-capability query.
    let output = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }
      .map_err(|e| EngineError::native("NV12 support", e))?;
    if input & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
      || output & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
    {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "video processor",
        "GPU BGRA-to-NV12 conversion unavailable",
      ));
    }
    // SAFETY: enumerator belongs to this device; rate conversion index zero exists for supported processing.
    let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
      .map_err(|e| EngineError::native("video processor", e))?;
    let vc1: ID3D11VideoContext1 = video_context
      .cast()
      .map_err(|e| EngineError::native("video color space", e))?;
    // SAFETY: fixed SDR full-range RGB input and limited-range BT.709 NV12 output.
    unsafe {
      vc1.VideoProcessorSetStreamColorSpace1(
        &processor,
        0,
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
      );
      vc1.VideoProcessorSetOutputColorSpace1(
        &processor,
        DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
      );
      video_context.VideoProcessorSetStreamFrameFormat(
        &processor,
        0,
        D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
      );
      // The crop and color conversion share one Blt: no intermediate copy of the region.
      if let Some(region) = region {
        let source = RECT {
          left: region.x as i32,
          top: region.y as i32,
          right: (region.x + region.width) as i32,
          bottom: (region.y + region.height) as i32,
        };
        let target = RECT {
          left: 0,
          top: 0,
          right: width as i32,
          bottom: height as i32,
        };
        video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source));
        video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&target));
        video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
      }
    }
    let (desktop, scratch) = Self::caches(&device, source_width, source_height)?;
    let mut surfaces = Vec::with_capacity(count);
    for _ in 0..count {
      surfaces.push(Self::texture(
        &device,
        width,
        height,
        DXGI_FORMAT_NV12,
        (D3D11_BIND_RENDER_TARGET | D3D11_BIND_VIDEO_ENCODER).0 as u32,
      )?);
    }
    let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
      ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
      Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
        Texture2D: D3D11_TEX2D_VPIV {
          MipSlice: 0,
          ArraySlice: 0,
        },
      },
      ..Default::default()
    };
    let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
      ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
      Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
      },
    };
    let mut input_view = None;
    let mut scratch_view = None;
    let mut output_views = Vec::with_capacity(count);
    // SAFETY: cache views for the lifetime of the fixed same-device texture pool.
    unsafe {
      video_device
        .CreateVideoProcessorInputView(&desktop, &enumerator, &input_desc, Some(&mut input_view))
        .map_err(|e| EngineError::native("desktop video view", e))?;
      video_device
        .CreateVideoProcessorInputView(&scratch, &enumerator, &input_desc, Some(&mut scratch_view))
        .map_err(|e| EngineError::native("cursor video view", e))?;
      for texture in &surfaces {
        let mut view = None;
        video_device
          .CreateVideoProcessorOutputView(texture, &enumerator, &output_desc, Some(&mut view))
          .map_err(|e| EngineError::native("NV12 video view", e))?;
        output_views.push(required(view, "NV12 view")?);
      }
    }
    Ok(Self {
      device,
      context,
      desktop,
      scratch,
      surfaces,
      converter: Converter::Video(VideoConverter {
        _video_device: video_device,
        video_context,
        _enumerator: enumerator,
        processor,
        input_view: required(input_view, "desktop view")?,
        scratch_view: required(scratch_view, "cursor view")?,
        output_views,
      }),
      source_width,
      source_height,
      width,
      height,
      bytes,
      fps,
    })
  }
  pub(super) fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    bind: u32,
  ) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
      Width: width,
      Height: height,
      MipLevels: 1,
      ArraySize: 1,
      Format: format,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Usage: D3D11_USAGE_DEFAULT,
      BindFlags: bind,
      ..Default::default()
    };
    let mut texture = None;
    // SAFETY: no CPU-accessible staging resources; driver allocates checked fixed-sized texture.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
      .map_err(|e| EngineError::native("GPU texture", e))?;
    required(texture, "GPU texture")
  }
  /// Paint the desktop cache opaque black. A spanned canvas has areas no display covers; they
  /// must never show uninitialized GPU memory. Display parts overwrite only their own areas.
  pub fn clear_desktop(&self) -> Result<()> {
    let mut view = None;
    // SAFETY: the cache was created with render-target binding on this device.
    unsafe {
      self
        .device
        .CreateRenderTargetView(&self.desktop, None, Some(&mut view))
    }
    .map_err(|e| EngineError::native("desktop clear view", e))?;
    let view = required(view, "desktop clear view")?;
    // SAFETY: the view targets the live cache; the context is multithread-protected.
    unsafe {
      self
        .context
        .ClearRenderTargetView(&view, &[0.0, 0.0, 0.0, 1.0]);
    }
    Ok(())
  }
  pub fn convert(&self, source: &ID3D11Texture2D, slot: usize) -> Result<()> {
    let surface = self
      .surfaces
      .get(slot)
      .ok_or_else(|| EngineError::protocol("Invalid texture pool index"))?;
    let from_desktop = source.as_raw() == self.desktop.as_raw();
    if !from_desktop && source.as_raw() != self.scratch.as_raw() {
      return Err(EngineError::protocol(
        "Conversion source is not an owned GPU cache",
      ));
    }
    let video = match &self.converter {
      Converter::Cpu(area) => {
        // SAFETY: same-device BGRA textures; the area lies within the cache (checked by crop).
        unsafe {
          self
            .context
            .CopySubresourceRegion(surface, 0, 0, 0, 0, source, 0, Some(area));
        }
        return Ok(());
      },
      Converter::Video(video) => video,
    };
    let output = video
      .output_views
      .get(slot)
      .ok_or_else(|| EngineError::protocol("Invalid texture pool index"))?;
    let input = if from_desktop {
      &video.input_view
    } else {
      &video.scratch_view
    };
    let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
      Enable: true.into(),
      pInputSurface: ManuallyDrop::new(Some(input.clone())),
      ..Default::default()
    }];
    // SAFETY: stream holds an additional input reference until this synchronous submission returns.
    let result = unsafe {
      video
        .video_context
        .VideoProcessorBlt(&video.processor, output, 0, &streams)
    }
    .map_err(|e| EngineError::native("GPU color conversion", e));
    // ManuallyDrop fields in generated ABI structs do not release their extra COM reference.
    // SAFETY: this field was initialized once above and has not been moved or released.
    unsafe {
      ManuallyDrop::drop(&mut streams[0].pInputSurface);
    }
    result
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn doctor_inventories_without_opening_capture_or_an_encoder() {
    let report = doctor().expect("read-only capabilities");
    assert!(report.os_build >= 19041);
    assert!(!report.architecture.is_empty());
    for output in report.monitors {
      assert!(!output.id.is_empty());
      assert!(output.width > 0 && output.height > 0);
      assert!(!output.adapter.is_empty());
    }
    assert!(required::<u32>(None, "fixture").is_err());
    assert_eq!(required(Some(3), "fixture").expect("native output"), 3);
  }
  /// Read back one BGRA texture through a staging copy as packed rows.
  fn pixels(gpu: &Gpu, texture: &ID3D11Texture2D, width: u32, height: u32) -> Vec<[u8; 4]> {
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
    let staging = staging.expect("staging texture");
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
  #[test]
  fn adapters_without_a_video_processor_fall_back_to_cropped_bgra_surfaces() {
    // Regression: the basic render adapter of GPU-less machines and VMs (WARP) refuses
    // D3D11_CREATE_DEVICE_VIDEO_SUPPORT, which failed every VM recording before Prepare finished.
    // SAFETY: read-only factory for the software adapter; no output, capture or display.
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
    // SAFETY: as above.
    let adapter: IDXGIAdapter1 = unsafe { factory.EnumWarpAdapter() }.expect("WARP adapter");
    let (source_width, source_height) = (96u32, 48u32);
    let region = Region {
      x: 7,
      y: 5,
      width: 34,
      height: 34,
    };
    let gpu = Gpu::new(&adapter, source_width, source_height, Some(region), 30)
      .expect("GPU-less tier prepares");
    assert_eq!(gpu.conversion(), ConversionKind::Cpu);
    assert_eq!((gpu.width, gpu.height), (34, 34));
    assert_eq!(gpu.surfaces.len(), 8);
    assert_eq!(
      gpu.bytes,
      limits::pool_bytes(source_width, source_height, 34, 34, ConversionKind::Cpu, 8)
        .expect("pool")
    );
    // Each desktop pixel carries its own coordinates, so the copy proves the crop.
    let image: Vec<u8> = (0..source_height)
      .flat_map(|y| (0..source_width).flat_map(move |x| [x as u8, y as u8, 7, 255]))
      .collect();
    // SAFETY: the initial data spans the whole BGRA cache at its packed pitch.
    unsafe {
      gpu.context.UpdateSubresource(
        &gpu.desktop,
        0,
        None,
        image.as_ptr().cast(),
        source_width * 4,
        0,
      );
    }
    gpu.convert(&gpu.desktop, 3).expect("crop copy");
    let surface = pixels(&gpu, &gpu.surfaces[3], 34, 34);
    assert_eq!(surface[0], [7, 5, 7, 255]);
    assert_eq!(surface[33], [40, 5, 7, 255]);
    assert_eq!(surface[34 * 33 + 33], [40, 38, 7, 255]);
    // Only owned caches and existing slots are accepted.
    assert!(gpu.convert(&gpu.desktop, 8).is_err());
    let foreign = Gpu::texture(
      &gpu.device,
      source_width,
      source_height,
      DXGI_FORMAT_B8G8R8A8_UNORM,
      D3D11_BIND_SHADER_RESOURCE.0 as u32,
    )
    .expect("foreign texture");
    assert!(gpu.convert(&foreign, 0).is_err());
  }
}

#[cfg(test)]
pub(crate) mod mock;
