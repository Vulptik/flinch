use super::frame_pass::{FramePass, Tone};
use super::gpu::{Gpu, Output};
use super::rotate::Orientation;
use crate::{EngineError, ErrorKind, Result, types::CaptureKind};
use windows::{
  Foundation::TypedEventHandler,
  Graphics::{
    Capture::*,
    DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
  },
  Win32::{
    Graphics::{
      Direct3D11::*,
      Dxgi::{Common::*, *},
    },
    System::WinRT::{Direct3D11::*, Graphics::Capture::IGraphicsCaptureItemInterop},
  },
  core::{IInspectable, Interface},
};

#[derive(Clone, Debug, Default)]
pub struct PointerShape {
  pub kind: u32,
  pub width: u32,
  pub height: u32,
  pub pitch: u32,
  pub bytes: Vec<u8>,
  pub x: i32,
  pub y: i32,
  pub visible: bool,
  pub generation: u64,
}
/// A duplicated display's first shape, read from Windows until one is available.
#[derive(Default)]
struct PointerSeed {
  pending: Option<(super::cursor::DesktopArea, super::cursor::PointerRead)>,
  failure: Option<EngineError>,
}
pub enum Acquisition {
  New { source_qpc_100ns: Option<u64> },
  Unchanged,
}
fn desktop_copy_required(valid: bool, last_present: i64) -> bool {
  !valid || last_present > 0
}

pub struct Capture {
  backend: Backend,
  pub valid: bool,
  pub pointer: PointerShape,
  seed: PointerSeed,
  width: u32,
  height: u32,
  /// This output's offset in the shared desktop cache; (0, 0) for a single display.
  origin: (u32, u32),
  /// Presents DWM reported since the last `take_presents` (duplication only): whether a present
  /// a slot missed was late or never composed. Diagnostic; saturates when never taken.
  presents: u32,
  /// Present when frames need turning upright (rotated duplication) or mapping to SDR (HDR).
  pass: Option<FramePass>,
}
enum Backend {
  Stopped(Option<EngineError>),
  Dxgi(IDXGIOutputDuplication),
  Wgc(WgcResources),
  Synthetic { frame: u64 },
}
impl Capture {
  pub fn new(gpu: &Gpu, output: &Output, kind: CaptureKind, cursor: bool) -> Result<Self> {
    Self::new_at(gpu, output, kind, cursor, (0, 0))
  }
  /// A capture of `output` copied into the desktop cache at `origin` (a composite's part).
  pub fn new_at(
    gpu: &Gpu,
    output: &Output,
    kind: CaptureKind,
    cursor: bool,
    origin: (u32, u32),
  ) -> Result<Self> {
    let (width, height) = (output.info.width, output.info.height);
    if origin
      .0
      .checked_add(width)
      .is_none_or(|right| right > gpu.source_width)
      || origin
        .1
        .checked_add(height)
        .is_none_or(|bottom| bottom > gpu.source_height)
    {
      return Err(EngineError::protocol(
        "Capture part leaves the desktop cache",
      ));
    }
    let orientation = Orientation::from_dxgi(output.info.rotation)?;
    // An HDR desktop is FP16 scRGB for both real backends; synthetic frames are always SDR.
    let tone = match kind {
      CaptureKind::Dxgi | CaptureKind::Wgc if output.info.hdr => Tone::Hdr {
        sdr_white_nits: super::sdr_white::sdr_white_nits(&output.info.id)?,
      },
      CaptureKind::DxgiLegacy if output.info.hdr => {
        return Err(EngineError::new(
          ErrorKind::Capability,
          "capture",
          "Legacy desktop duplication cannot capture HDR displays",
        ));
      },
      _ => Tone::Sdr,
    };
    let backend = match kind {
      CaptureKind::Dxgi => {
        // SAFETY: device and output belong to the same adapter; only the tone's format
        // (BGRA, or FP16 on an HDR desktop) is negotiated.
        Backend::Dxgi(
          unsafe {
            output
              .output
              .DuplicateOutput1(&gpu.device, 0, &[tone.input_format()])
          }
          .map_err(duplication_error)?,
        )
      },
      CaptureKind::DxgiLegacy => {
        // SAFETY: explicitly selected legacy duplication uses the same output adapter and BGRA cache.
        // It is never attempted after another backend fails.
        Backend::Dxgi(
          unsafe { output.output.DuplicateOutput(&gpu.device) }.map_err(|error| {
            let mut converted = duplication_error(error);
            converted.stage = "DuplicateOutput (explicit legacy backend)".into();
            converted
          })?,
        )
      },
      CaptureKind::Wgc => {
        // Windows reports sessions without capture support (some RDP or server desktops).
        if !GraphicsCaptureSession::IsSupported()
          .map_err(|e| EngineError::native("WGC support", e))?
        {
          return Err(EngineError::new(
            ErrorKind::Capability,
            "WGC support",
            "Windows Graphics Capture is not supported in this session",
          ));
        }
        let interop: IGraphicsCaptureItemInterop =
          windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| EngineError::native("WGC interop", e))?;
        // SAFETY: monitor is from the live same-adapter DXGI output.
        let desc =
          unsafe { output.output.GetDesc1() }.map_err(|e| EngineError::native("WGC monitor", e))?;
        // SAFETY: returned item has independent WinRT ownership.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(desc.Monitor) }
          .map_err(|e| EngineError::native("WGC item", e))?;
        let dxgi: IDXGIDevice = gpu
          .device
          .cast()
          .map_err(|e| EngineError::native("WGC DXGI device", e))?;
        // SAFETY: wrapper holds its own device reference and shares the same adapter.
        let inspectable: IInspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
          .map_err(|e| EngineError::native("WinRT D3D device", e))?;
        let device: IDirect3DDevice = inspectable
          .cast()
          .map_err(|e| EngineError::native("WinRT D3D cast", e))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
          &device,
          match tone {
            Tone::Sdr => DirectXPixelFormat::B8G8R8A8UIntNormalized,
            Tone::Hdr { .. } => DirectXPixelFormat::R16G16B16A16Float,
          },
          2,
          item
            .Size()
            .map_err(|e| EngineError::native("WGC size", e))?,
        )
        .map_err(|e| EngineError::native("WGC pool", e))?;
        Backend::Wgc(WgcResources::prepare(pool, Some(&item), cursor)?)
      },
      CaptureKind::Synthetic => Backend::Synthetic { frame: 0 },
      CaptureKind::Auto => {
        return Err(EngineError::protocol(
          "Prepare resolves automatic capture before creating a backend",
        ));
      },
    };
    // Duplication scans out in the panel's orientation. WGC composes the monitor item upright,
    // and its frames are validated at the desktop extent.
    let orientation = match &backend {
      Backend::Dxgi(_) => orientation,
      _ => Orientation::Upright,
    };
    let pass = match (orientation, tone) {
      (Orientation::Upright, Tone::Sdr) => None,
      _ => Some(FramePass::new(
        &gpu.device,
        width,
        height,
        orientation,
        tone,
      )?),
    };
    let seed = PointerSeed {
      pending: (cursor && matches!(backend, Backend::Dxgi(_))).then_some((
        (output.info.x, output.info.y, width, height),
        super::cursor::system_pointer as super::cursor::PointerRead,
      )),
      failure: None,
    };
    Ok(Self {
      backend,
      valid: false,
      pointer: PointerShape::default(),
      seed,
      width,
      height,
      origin,
      presents: 0,
      pass,
    })
  }
  /// What this display's frames mean: SDR, or HDR at its SDR white level.
  pub fn tone(&self) -> Tone {
    self.pass.as_ref().map_or(Tone::Sdr, FramePass::tone)
  }
  /// Format every captured frame must have: FP16 on an HDR desktop, else BGRA.
  fn frame_format(&self) -> DXGI_FORMAT {
    self
      .pass
      .as_ref()
      .map_or(DXGI_FORMAT_B8G8R8A8_UNORM, FramePass::input_format)
  }
  pub fn start(&self) -> Result<()> {
    if let Backend::Stopped(error) = &self.backend {
      return Err(
        error
          .clone()
          .unwrap_or_else(|| EngineError::protocol("Capture source has been stopped")),
      );
    }
    if let Backend::Wgc(resources) = &self.backend {
      check_wgc_open(&resources.closed)?;
      resources
        .session
        .as_ref()
        .ok_or_else(|| EngineError::protocol("WGC session not prepared"))?
        .StartCapture()
        .map_err(|e| EngineError::native("WGC start", e))?;
    }
    Ok(())
  }
  /// Owner-thread shutdown. Cache validity is withdrawn before any native cleanup.
  pub fn stop(&mut self) -> Result<()> {
    self.valid = false;
    self.pointer.visible = false;
    let result = match std::mem::replace(&mut self.backend, Backend::Stopped(None)) {
      Backend::Wgc(mut resources) => resources.close(),
      // Dropping the duplication reference releases the owned native source.
      Backend::Dxgi(_) | Backend::Synthetic { .. } => Ok(()),
      Backend::Stopped(error) => error.map_or(Ok(()), Err),
    };
    if let Err(error) = &result {
      self.backend = Backend::Stopped(Some(error.clone()));
    }
    result
  }
  pub fn synthetic_index(&mut self, index: u64) {
    if let Backend::Synthetic { frame } = &mut self.backend {
      *frame = index;
    }
  }
  pub fn acquire(&mut self, gpu: &Gpu) -> Result<Acquisition> {
    let part = (self.origin, self.width, self.height);
    let format = self.frame_format();
    match &mut self.backend {
      Backend::Stopped(error) => Err(
        error
          .clone()
          .unwrap_or_else(|| EngineError::protocol("Capture source has been stopped")),
      ),
      Backend::Dxgi(duplication) => {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: nonblocking acquisition on this source's owner; every acquired frame is released below.
        match unsafe { duplication.AcquireNextFrame(0, &mut info, &mut resource) } {
          Ok(()) => {},
          Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Acquisition::Unchanged),
          Err(e) => {
            return Err(EngineError {
              kind: ErrorKind::DeviceLost,
              ..EngineError::native("DXGI acquire", e)
            });
          },
        }
        self.presents = self.presents.saturating_add(info.AccumulatedFrames);
        let result = (|| {
          let texture: ID3D11Texture2D = resource
            .ok_or_else(|| EngineError::protocol("DXGI frame without resource"))?
            .cast()
            .map_err(|e| EngineError::native("DXGI texture", e))?;
          let (frame_width, frame_height) = self
            .pass
            .as_ref()
            .map_or((self.width, self.height), FramePass::panel_extent);
          validate_texture(&texture, frame_width, frame_height, format)?;
          if info.PointerShapeBufferSize > 0 {
            if info.PointerShapeBufferSize > 1024 * 1024 {
              return Err(EngineError::protocol("Pointer shape exceeds 1 MiB"));
            }
            self
              .pointer
              .bytes
              .resize(info.PointerShapeBufferSize as usize, 0);
            let mut required = 0;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: exact capacity is passed; metadata and shape bytes remain owned by this source.
            unsafe {
              duplication.GetFramePointerShape(
                self.pointer.bytes.len() as u32,
                self.pointer.bytes.as_mut_ptr().cast(),
                &mut required,
                &mut shape,
              )
            }
            .map_err(|e| EngineError::native("pointer shape", e))?;
            if required as usize > self.pointer.bytes.len() {
              return Err(EngineError::protocol(
                "Pointer shape changed beyond allocated capacity",
              ));
            }
            self.pointer.bytes.truncate(required as usize);
            self.pointer.kind = shape.Type;
            self.pointer.width = shape.Width;
            self.pointer.height = shape.Height;
            self.pointer.pitch = shape.Pitch;
            self.pointer.generation = self
              .pointer
              .generation
              .checked_add(1)
              .ok_or_else(|| EngineError::protocol("Pointer revision overflow"))?;
          }
          if info.LastMouseUpdateTime > 0 {
            self.pointer.x = info.PointerPosition.Position.x;
            self.pointer.y = info.PointerPosition.Position.y;
            self.pointer.visible = info.PointerPosition.Visible.as_bool();
          }
          if let Some((area, read)) = self.seed.pending.take()
            && self.pointer.bytes.is_empty()
          {
            match read(area) {
              Ok(Some(found)) => {
                let generation = self
                  .pointer
                  .generation
                  .checked_add(1)
                  .ok_or_else(|| EngineError::protocol("Pointer revision overflow"))?;
                let placed = info.LastMouseUpdateTime > 0;
                let (x, y, visible) = (self.pointer.x, self.pointer.y, self.pointer.visible);
                self.pointer = PointerShape {
                  generation,
                  ..found
                };
                if placed {
                  (self.pointer.x, self.pointer.y) = (x, y);
                  self.pointer.visible = visible;
                }
              },
              // Nothing is showing yet: look again on the next frame.
              Ok(None) => self.seed.pending = Some((area, read)),
              Err(error) => self.seed.failure = Some(error),
            }
          }
          if desktop_copy_required(self.valid, info.LastPresentTime) {
            // SAFETY: same-device textures validated at the panel extent and format, and
            // upright BGRA at the part's extent; the command stream retains resource
            // references until it runs.
            unsafe {
              match &self.pass {
                Some(pass) => copy_part(gpu, pass.apply(&gpu.context, &texture), part),
                None => copy_part(gpu, &texture, part),
              }
            }
            self.valid = true;
            let source_qpc_100ns = if info.LastPresentTime > 0 {
              Some(qpc_raw_to_media(info.LastPresentTime)?)
            } else {
              None // A first/static seed has pixels but no native presentation date.
            };
            Ok(Acquisition::New { source_qpc_100ns })
          } else {
            Ok(Acquisition::Unchanged)
          }
        })();
        // SAFETY: exactly one release for every successful AcquireNextFrame, including failure paths.
        let release =
          unsafe { duplication.ReleaseFrame() }.map_err(|e| EngineError::native("DXGI release", e));
        finish_acquisition(result, release)
      },
      Backend::Wgc(WgcResources {
        pool,
        wake,
        last,
        closed,
        ..
      }) => {
        if let Err(error) = check_wgc_open(closed) {
          self.valid = false;
          return Err(error);
        }
        if !wake.swap(false, std::sync::atomic::Ordering::AcqRel) {
          return Ok(Acquisition::Unchanged);
        }
        let mut result = Acquisition::Unchanged;
        for _ in 0..8 {
          let Some(frame) = next_wgc_frame(pool)? else {
            break;
          };
          let work = (|| {
            let size = frame
              .ContentSize()
              .map_err(|e| EngineError::native("WGC content size", e))?;
            if size.Width != self.width as i32 || size.Height != self.height as i32 {
              return Err(EngineError::new(
                ErrorKind::DeviceLost,
                "WGC",
                "Capture dimensions changed",
              ));
            }
            let surface = frame
              .Surface()
              .map_err(|e| EngineError::native("WGC surface", e))?;
            let access: IDirect3DDxgiInterfaceAccess = surface
              .cast()
              .map_err(|e| EngineError::native("WGC native surface", e))?;
            // SAFETY: WinRT surface owns a texture on the shared capture device.
            let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
              .map_err(|e| EngineError::native("WGC texture", e))?;
            validate_texture(&texture, self.width, self.height, format)?;
            let timestamp = frame
              .SystemRelativeTime()
              .map_err(|e| EngineError::native("WGC timestamp", e))?
              .Duration;
            if timestamp < 0 || timestamp < *last {
              return Err(EngineError::protocol("WGC timestamp reversed"));
            }
            if timestamp != *last {
              // SAFETY: validated physical extent and format, same-adapter frame pool; the
              // pass returns upright BGRA at the part's extent.
              unsafe {
                match &self.pass {
                  Some(pass) => copy_part(gpu, pass.apply(&gpu.context, &texture), part),
                  None => copy_part(gpu, &texture, part),
                }
              }
              *last = timestamp;
              self.valid = true;
              result = Acquisition::New {
                source_qpc_100ns: Some(timestamp as u64),
              };
            }
            Ok(())
          })();
          let close = frame
            .Close()
            .map_err(|e| EngineError::native("WGC release", e));
          finish_acquisition(work, close)?;
        }
        if let Err(error) = check_wgc_open(closed) {
          self.valid = false;
          return Err(error);
        }
        Ok(result)
      },
      Backend::Synthetic { frame } => {
        let mut view = None;
        // SAFETY: synthetic offscreen texture belongs to this device; no desktop pixels are read.
        unsafe {
          gpu
            .device
            .CreateRenderTargetView(&gpu.scratch, None, Some(&mut view))
        }
        .map_err(|e| EngineError::native("synthetic view", e))?;
        let view = view.ok_or_else(|| EngineError::protocol("No synthetic target view"))?;
        // Markers begin after 500 ms of genuine fixture silence, so AAC filter
        // startup attenuation is not mistaken for a changing capture clock.
        let color = [
          ((*frame % 251) as f32) / 250.0,
          if *frame % u64::from(gpu.fps) == u64::from(gpu.fps) / 2 {
            1.0
          } else {
            0.0
          },
          0.25,
          1.0,
        ];
        // SAFETY: owner submits clear and copy on the same immediate context.
        unsafe {
          gpu.context.ClearRenderTargetView(&view, &color);
          gpu.context.CopyResource(&gpu.desktop, &gpu.scratch);
        }
        let context1: ID3D11DeviceContext1 = gpu
          .context
          .cast()
          .map_err(|e| EngineError::native("synthetic numbered marker", e))?;
        for bit in 0..32 {
          let rectangle = windows::Win32::Foundation::RECT {
            left: bit * 16,
            top: 0,
            right: bit * 16 + 16,
            bottom: 16,
          };
          let value = if (*frame >> bit) & 1 == 0 { 0.0 } else { 1.0 };
          // SAFETY: small in-bounds marker rectangles on the offscreen render target.
          unsafe {
            context1.ClearView(&view, &[value, value, value, 1.0], Some(&[rectangle]));
          }
        }
        // SAFETY: numbered marker and full-frame pulse are now on the same GPU command stream.
        unsafe {
          gpu.context.CopyResource(&gpu.desktop, &gpu.scratch);
        }
        self.valid = true;
        Ok(Acquisition::New {
          source_qpc_100ns: Some(super::qpc()?),
        })
      },
    }
  }
}
pub(super) fn qpc_raw_to_media(value: i64) -> Result<u64> {
  let mut frequency = 0;
  // SAFETY: stack output pointer is live for this frequency query.
  unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) }
    .map_err(|e| EngineError::native("DXGI QPC frequency", e))?;
  if value < 0 || frequency <= 0 {
    return Err(EngineError::protocol("Invalid DXGI presentation clock"));
  }
  crate::time::scale(value as u64, crate::time::SECOND, frequency as u64)
}
fn duplication_error(error: windows::core::Error) -> EngineError {
  let unavailable = error.code() == DXGI_ERROR_UNSUPPORTED;
  let mut error = EngineError::native("DuplicateOutput1", error);
  if unavailable {
    error.kind = ErrorKind::Capability;
  }
  error
}
fn next_wgc_frame(pool: &Direct3D11CaptureFramePool) -> Result<Option<Direct3D11CaptureFrame>> {
  // SAFETY: TryGetNextFrame permits S_OK with a null frame for an empty pool.
  // The generated non-null projection cannot represent that result. Take the
  // ABI's single owned reference, if present, and preserve genuine HRESULTs.
  unsafe {
    let mut raw = std::ptr::null_mut();
    let status = (Interface::vtable(pool).TryGetNextFrame)(pool.as_raw(), &mut raw);
    let frame = if raw.is_null() {
      None
    } else {
      Some(Direct3D11CaptureFrame::from_raw(raw))
    };
    status
      .ok()
      .map_err(|e| EngineError::native("WGC frame", e))?;
    Ok(frame)
  }
}
/// One capture per output, each copied into its offset of the shared desktop cache (the bounding
/// box of the outputs, cleared to black so uncovered areas never show stale memory). A
/// single-display session is a composite of one. Every part must have a frame before the
/// composite is valid, and a part's failure is the session's: a frozen part is never shown.
pub struct Composite {
  parts: Vec<Capture>,
  pub valid: bool,
  /// The pointer of the part that shows it, in desktop-cache coordinates.
  pub pointer: PointerShape,
  /// (part, part generation) behind `pointer`; a change bumps `pointer.generation`.
  pointer_source: Option<(usize, u64)>,
}
impl Composite {
  pub fn single(capture: Capture) -> Self {
    Self::new(vec![capture])
  }
  /// Each display's tone, in part order.
  pub fn tones(&self) -> Vec<Tone> {
    self.parts.iter().map(Capture::tone).collect()
  }
  pub fn new(parts: Vec<Capture>) -> Self {
    let mut composite = Self {
      parts,
      valid: false,
      pointer: PointerShape::default(),
      pointer_source: None,
    };
    composite.refresh();
    composite
  }
  pub fn start(&self) -> Result<()> {
    self.parts.iter().try_for_each(Capture::start)
  }
  /// Stop every part, even after one fails; the first failure is kept and later ones appended.
  pub fn stop(&mut self) -> Result<()> {
    self.valid = false;
    self.pointer.visible = false;
    let mut failure: Option<EngineError> = None;
    for part in &mut self.parts {
      if let Err(error) = part.stop() {
        match &mut failure {
          Some(first) => first
            .reason
            .push_str(&format!("; another display: {error}")),
          None => failure = Some(error),
        }
      }
    }
    failure.map_or(Ok(()), Err)
  }
  pub fn synthetic_index(&mut self, index: u64) {
    for part in &mut self.parts {
      part.synthetic_index(index);
    }
  }
  /// Presents reported by every part since the last call, for frame traces.
  pub fn take_presents(&mut self) -> u32 {
    self
      .parts
      .iter_mut()
      .map(|part| std::mem::take(&mut part.presents))
      .fold(0, u32::saturating_add)
  }
  /// Cursor reads that failed since the last call; the recording goes on without a first shape.
  pub fn take_pointer_failures(&mut self) -> Vec<EngineError> {
    self
      .parts
      .iter_mut()
      .filter_map(|part| part.seed.failure.take())
      .collect()
  }
  /// Acquire every part. New content anywhere is new; its source time is the oldest new part's,
  /// so the slot's age is never understated.
  pub fn acquire(&mut self, gpu: &Gpu) -> Result<Acquisition> {
    let mut changed = false;
    let mut oldest: Option<u64> = None;
    let mut undated = false;
    for part in &mut self.parts {
      if let Acquisition::New { source_qpc_100ns } = part.acquire(gpu)? {
        changed = true;
        match source_qpc_100ns {
          Some(time) => oldest = Some(oldest.map_or(time, |old| old.min(time))),
          None => undated = true,
        }
      }
    }
    self.refresh();
    Ok(if changed {
      Acquisition::New {
        source_qpc_100ns: if undated { None } else { oldest },
      }
    } else {
      Acquisition::Unchanged
    })
  }
  /// Recompute validity and the visible pointer from the parts.
  fn refresh(&mut self) {
    self.valid = !self.parts.is_empty() && self.parts.iter().all(|part| part.valid);
    let shown = self
      .parts
      .iter()
      .enumerate()
      .find(|(_, part)| part.pointer.visible);
    let Some((index, part)) = shown else {
      self.pointer.visible = false;
      return;
    };
    let source = (index, part.pointer.generation);
    if self.pointer_source != Some(source) {
      // A display the pointer just entered may not have delivered a shape yet: keep the last
      // shape and move it, instead of hiding the cursor until one arrives.
      if !part.pointer.bytes.is_empty() {
        let generation = self.pointer.generation.wrapping_add(1);
        self.pointer = part.pointer.clone();
        self.pointer.generation = generation;
      }
      self.pointer_source = Some(source);
    }
    let (x, y) = part.origin;
    self.pointer.x = part.pointer.x.saturating_add_unsigned(x);
    self.pointer.y = part.pointer.y.saturating_add_unsigned(y);
    self.pointer.visible = true;
  }
}
/// Copy a validated frame into its part of the desktop cache: the whole cache for a single display.
///
/// # Safety
/// `texture` must be a same-device BGRA texture of exactly the part's validated extent, which
/// lies inside the cache (checked when the capture was created).
unsafe fn copy_part(gpu: &Gpu, texture: &ID3D11Texture2D, part: ((u32, u32), u32, u32)) {
  let ((x, y), width, height) = part;
  // SAFETY: guaranteed by the caller; the command stream retains both resources until it runs.
  unsafe {
    if (x, y, width, height) == (0, 0, gpu.source_width, gpu.source_height) {
      gpu.context.CopyResource(&gpu.desktop, texture);
    } else {
      gpu
        .context
        .CopySubresourceRegion(&gpu.desktop, 0, x, y, 0, texture, 0, None);
    }
  }
}
fn finish_acquisition<T>(work: Result<T>, release: Result<()>) -> Result<T> {
  match (work, release) {
    (Err(mut original), Err(cleanup)) => {
      original.reason.push_str(&format!("; cleanup: {cleanup}"));
      Err(original)
    },
    (Err(original), Ok(())) => Err(original),
    (Ok(_), Err(cleanup)) => Err(cleanup),
    (Ok(value), Ok(())) => Ok(value),
  }
}
fn validate_texture(
  texture: &ID3D11Texture2D,
  width: u32,
  height: u32,
  format: DXGI_FORMAT,
) -> Result<()> {
  let mut desc = D3D11_TEXTURE2D_DESC::default();
  // SAFETY: read-only descriptor query into an initialized output.
  unsafe {
    texture.GetDesc(&mut desc);
  }
  if desc.Width != width || desc.Height != height || desc.Format != format {
    return Err(EngineError::new(
      ErrorKind::DeviceLost,
      "capture format",
      "Display format or dimensions changed",
    ));
  }
  Ok(())
}
struct WgcResources {
  pool: Direct3D11CaptureFramePool,
  session: Option<GraphicsCaptureSession>,
  arrived: Option<i64>,
  item: Option<GraphicsCaptureItem>,
  closed_token: Option<i64>,
  closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
  wake: std::sync::Arc<std::sync::atomic::AtomicBool>,
  last: i64,
  pool_close_attempted: bool,
}
impl WgcResources {
  fn prepare(
    pool: Direct3D11CaptureFramePool,
    item: Option<&GraphicsCaptureItem>,
    cursor: bool,
  ) -> Result<Self> {
    // Own the pool before the first fallible registration so partial preparation
    // closes it and removes any registered callback, even without a Capture.
    let mut resources = Self {
      pool,
      session: None,
      arrived: None,
      item: item.cloned(),
      closed_token: None,
      closed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
      wake: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
      last: -1,
      pool_close_attempted: false,
    };
    if let Some(item) = &resources.item {
      let closed = resources.closed.clone();
      resources.closed_token = Some(
        item
          .Closed(&TypedEventHandler::new(move |_, _| {
            closed.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
          }))
          .map_err(|e| EngineError::native("WGC closure callback", e))?,
      );
    }
    let signal = resources.wake.clone();
    resources.arrived = Some(
      resources
        .pool
        .FrameArrived(&TypedEventHandler::new(move |_, _| {
          signal.store(true, std::sync::atomic::Ordering::Release);
          Ok(())
        }))
        .map_err(|e| EngineError::native("WGC callback", e))?,
    );
    resources.session = Some(
      resources
        .pool
        .CreateCaptureSession(item)
        .map_err(|e| EngineError::native("WGC session", e))?,
    );
    resources
      .session
      .as_ref()
      .ok_or_else(|| EngineError::protocol("No WGC session"))?
      .SetIsCursorCaptureEnabled(cursor)
      .map_err(|e| EngineError::native("WGC cursor", e))?;
    Ok(resources)
  }
  fn close(&mut self) -> Result<()> {
    self
      .closed
      .store(true, std::sync::atomic::Ordering::Release);
    self.wake.store(false, std::sync::atomic::Ordering::Release);
    let mut failure: Option<EngineError> = None;
    let mut check = |result: windows::core::Result<()>, stage| {
      if let Err(error) = result {
        let error = EngineError::native(stage, error);
        if let Some(first) = &mut failure {
          first.reason.push_str(&format!("; cleanup: {error}"));
        } else {
          failure = Some(error);
        }
      }
    };
    if let (Some(item), Some(token)) = (&self.item, self.closed_token.take()) {
      check(item.RemoveClosed(token), "WGC closure callback removal");
    }
    if let Some(token) = self.arrived.take() {
      check(self.pool.RemoveFrameArrived(token), "WGC callback removal");
    }
    if let Some(session) = self.session.take() {
      check(session.Close(), "WGC close");
    }
    if !self.pool_close_attempted {
      self.pool_close_attempted = true;
      check(self.pool.Close(), "WGC pool close");
    }
    match failure {
      Some(error) => Err(error),
      None => Ok(()),
    }
  }
}
impl Drop for WgcResources {
  fn drop(&mut self) {
    if let Err(error) = self.close() {
      eprintln!("WGC cleanup: {error}");
    }
  }
}
fn check_wgc_open(closed: &std::sync::atomic::AtomicBool) -> Result<()> {
  if closed.load(std::sync::atomic::Ordering::Acquire) {
    return Err(EngineError::new(
      ErrorKind::DeviceLost,
      "WGC source closed",
      "The selected capture source closed; cached content is no longer a live source",
    ));
  }
  Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod source_tests {
  use super::*;
  fn part(origin: (u32, u32), valid: bool, failure: Option<&str>) -> Capture {
    Capture {
      backend: Backend::Stopped(
        failure.map(|reason| EngineError::new(ErrorKind::DeviceLost, "part", reason)),
      ),
      valid,
      pointer: PointerShape::default(),
      seed: Default::default(),
      width: 640,
      height: 360,
      origin,
      presents: 0,
      pass: None,
    }
  }
  fn shape(x: i32, y: i32, visible: bool, generation: u64) -> PointerShape {
    PointerShape {
      kind: 2,
      width: 4,
      height: 4,
      pitch: 16,
      bytes: if generation > 0 {
        vec![generation as u8; 64]
      } else {
        Vec::new()
      },
      x,
      y,
      visible,
      generation,
    }
  }
  #[test]
  fn a_part_must_fit_the_cache_before_any_source_opens_and_hdr_needs_a_modern_backend() {
    let _com = super::super::Apartment::new().expect("COM");
    // A read-only descriptor of a real output; no duplication or WGC source is opened.
    let Some(mut output) = super::super::gpu::outputs()
      .expect("read-only adapter inventory")
      .into_iter()
      .next()
    else {
      return;
    };
    let (gpu, driver) = super::super::gpu::mock::fixture();
    (output.info.width, output.info.height) = (gpu.source_width / 2, gpu.source_height);
    (output.info.hdr, output.info.rotation) = (false, DXGI_MODE_ROTATION_IDENTITY.0 as u32);
    let half = gpu.source_width / 2;
    // Parts that leave the cache, including by overflow, are refused.
    for origin in [(half + 1, 0), (0, 1), (u32::MAX, 0)] {
      let error = Capture::new_at(&gpu, &output, CaptureKind::Synthetic, false, origin)
        .err()
        .expect("outside the cache");
      assert!(
        error.reason.contains("leaves the desktop cache"),
        "{origin:?}"
      );
    }
    let part =
      Capture::new_at(&gpu, &output, CaptureKind::Synthetic, false, (half, 0)).expect("right half");
    assert_eq!(
      (part.origin, part.width, part.height),
      ((half, 0), half, gpu.source_height)
    );
    assert!(Capture::new(&gpu, &output, CaptureKind::Synthetic, false).is_ok());
    assert!(
      Capture::new(&gpu, &output, CaptureKind::Auto, false)
        .err()
        .expect("unresolved automatic capture")
        .reason
        .contains("resolves automatic capture")
    );
    // Rotation is no longer refused; a source that needs no duplication pass opens as is.
    output.info.rotation = 2;
    assert!(
      Capture::new(&gpu, &output, CaptureKind::Synthetic, false)
        .expect("rotated synthetic part")
        .pass
        .is_none()
    );
    output.info.rotation = 9;
    assert!(Capture::new(&gpu, &output, CaptureKind::Synthetic, false).is_err());
    // HDR is no longer refused. Synthetic frames are SDR and need no pass; legacy duplication
    // cannot negotiate FP16 and is refused before any source opens.
    (output.info.rotation, output.info.hdr) = (1, true);
    assert!(
      Capture::new(&gpu, &output, CaptureKind::Synthetic, false)
        .expect("HDR synthetic part")
        .pass
        .is_none()
    );
    let legacy = Capture::new(&gpu, &output, CaptureKind::DxgiLegacy, false)
      .err()
      .expect("legacy HDR");
    assert_eq!(legacy.kind, ErrorKind::Capability);
    assert!(legacy.reason.contains("HDR"), "{legacy}");
    assert!(driver.unexpected.lock().expect("no driver work").is_empty());
  }
  #[test]
  fn the_composition_period_is_read_without_opening_any_source() {
    // Read-only DWM timing; a session without composition reports None, never a guess.
    match super::super::gpu::composition_period() {
      Ok(Some(period)) => assert!(
        (10_000..=1_000_000).contains(&period),
        "{period} is between 10 and 1000 Hz"
      ),
      Ok(None) => {},
      Err(error) => assert_eq!(error.stage, "DWM composition timing"),
    }
  }
  #[test]
  fn a_composite_is_valid_only_when_every_display_has_a_frame() {
    assert!(Composite::single(part((0, 0), true, None)).valid);
    assert!(!Composite::new(vec![part((0, 0), true, None), part((640, 0), false, None)]).valid);
    assert!(Composite::new(vec![part((0, 0), true, None), part((640, 0), true, None)]).valid);
    assert!(
      !Composite::new(Vec::new()).valid,
      "no display is never valid"
    );
  }
  #[test]
  fn the_pointer_follows_the_display_showing_it_in_canvas_coordinates() {
    let mut composite = Composite::new(vec![part((0, 40), true, None), part((640, 0), true, None)]);
    assert!(!composite.pointer.visible);
    composite.parts[1].pointer = shape(10, 20, true, 1);
    composite.refresh();
    assert_eq!((composite.pointer.x, composite.pointer.y), (650, 20));
    assert!(composite.pointer.visible);
    let first = composite.pointer.generation;
    // Moving within the display keeps the shape generation.
    composite.parts[1].pointer.x = 30;
    composite.refresh();
    assert_eq!(
      (composite.pointer.x, composite.pointer.generation),
      (670, first)
    );
    // Entering a display that has no shape yet keeps the last shape and moves it.
    composite.parts[1].pointer.visible = false;
    composite.parts[0].pointer = shape(5, 6, true, 0);
    composite.refresh();
    assert_eq!((composite.pointer.x, composite.pointer.y), (5, 46));
    assert_eq!(
      composite.pointer.bytes,
      vec![1u8; 64],
      "previous shape kept"
    );
    assert_eq!(composite.pointer.generation, first);
    // Its own shape replaces it with a new generation.
    composite.parts[0].pointer = shape(5, 6, true, 3);
    composite.refresh();
    assert_eq!(composite.pointer.bytes, vec![3u8; 64]);
    assert!(composite.pointer.generation > first);
    composite.parts[0].pointer.visible = false;
    composite.refresh();
    assert!(!composite.pointer.visible, "hidden everywhere");
  }
  #[test]
  fn stopping_a_composite_stops_every_display_and_keeps_every_failure() {
    let mut composite = Composite::new(vec![
      part((0, 0), true, Some("first")),
      part((640, 0), true, None),
      part((1280, 0), true, Some("third")),
    ]);
    let error = composite.stop().expect_err("failures");
    assert!(error.reason.starts_with("first"), "{}", error.reason);
    assert!(error.reason.contains("third"), "{}", error.reason);
    assert!(!composite.valid && !composite.pointer.visible);
    assert!(
      Composite::new(vec![part((0, 0), true, None)])
        .stop()
        .is_ok()
    );
  }
  #[test]
  fn first_static_or_pointer_only_acquisition_seeds_the_cache() {
    assert!(desktop_copy_required(false, 0));
    assert!(!desktop_copy_required(true, 0));
    assert!(desktop_copy_required(true, 1));
  }
  #[test]
  fn unavailable_duplication_is_a_capability_error_without_hiding_its_hresult() {
    let error = duplication_error(windows::core::Error::from_hresult(DXGI_ERROR_UNSUPPORTED));
    assert_eq!(error.kind, ErrorKind::Capability);
    assert_eq!(error.native_code, Some(DXGI_ERROR_UNSUPPORTED.0));
    assert_eq!(
      duplication_error(windows::core::Error::from_hresult(
        DXGI_ERROR_DEVICE_REMOVED
      ))
      .kind,
      ErrorKind::DeviceLost
    );
    assert_eq!(
      duplication_error(windows::core::Error::from_hresult(
        windows::Win32::Foundation::E_ACCESSDENIED
      ))
      .kind,
      ErrorKind::Native
    );
  }
}
#[cfg(test)]
#[path = "capture/desktop_tests.rs"]
mod desktop_tests;
#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
#[path = "capture/mock_wgc.rs"]
mod mock_wgc;
#[cfg(test)]
pub(crate) use mock_wgc::shutdown_failure_fixture;
