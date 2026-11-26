//! Desktop Duplication COM contract doubles. No desktop or graphics source is opened.
#![allow(clippy::expect_used)]
use super::*;
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, AtomicUsize, Ordering},
};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL};
use windows_core::implement;
pub(crate) struct Shared {
  pub info: Mutex<DXGI_OUTDUPL_FRAME_INFO>,
  resource: Mutex<Option<IDXGIResource>>,
  held: AtomicBool,
  releases: AtomicUsize,
  pub timeout: AtomicBool,
  pub acquire_failure: Mutex<Option<windows::core::HRESULT>>,
  release_failure: AtomicBool,
  shape_failure: AtomicBool,
  /// Live duplication interfaces for this output; Windows allows one per process.
  live: AtomicUsize,
}
impl Shared {
  /// Whether this output's duplication interface is still held.
  pub fn duplicated(&self) -> bool {
    self.live.load(Ordering::Acquire) > 0
  }
}
#[implement(IDXGIOutputDuplication)]
struct Duplication(Arc<Shared>);
impl Duplication {
  fn new(state: Arc<Shared>) -> Self {
    state.live.fetch_add(1, Ordering::AcqRel);
    Self(state)
  }
}
impl Drop for Duplication {
  fn drop(&mut self) {
    self.0.live.fetch_sub(1, Ordering::AcqRel);
  }
}
impl IDXGIObject_Impl for Duplication_Impl {
  fn SetPrivateData(
    &self,
    _name: *const windows_core::GUID,
    _datasize: u32,
    _pdata: *const core::ffi::c_void,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn SetPrivateDataInterface(
    &self,
    _name: *const windows_core::GUID,
    _punknown: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetPrivateData(
    &self,
    _name: *const windows_core::GUID,
    _pdatasize: *mut u32,
    _pdata: *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetParent(
    &self,
    _riid: *const windows_core::GUID,
    _ppparent: *mut *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
}
impl IDXGIOutputDuplication_Impl for Duplication_Impl {
  fn GetDesc(&self, _pdesc: *mut DXGI_OUTDUPL_DESC) {}
  fn AcquireNextFrame(
    &self,
    _timeoutinmilliseconds: u32,
    _pframeinfo: *mut DXGI_OUTDUPL_FRAME_INFO,
    _ppdesktopresource: windows_core::OutRef<IDXGIResource>,
  ) -> windows_core::Result<()> {
    if _timeoutinmilliseconds != 0 {
      return Err(E_FAIL.into());
    }
    if let Some(error) = *self.0.acquire_failure.lock().expect("scripted acquisition") {
      return Err(error.into());
    }
    if self.0.timeout.load(Ordering::Acquire) {
      return Err(DXGI_ERROR_WAIT_TIMEOUT.into());
    }
    if self.0.held.swap(true, Ordering::AcqRel) {
      return Err(E_FAIL.into());
    }
    unsafe {
      *_pframeinfo = *self.0.info.lock().expect("frame metadata");
    }
    _ppdesktopresource.write(self.0.resource.lock().expect("frame resource").clone())
  }
  fn GetFrameDirtyRects(
    &self,
    _dirtyrectsbuffersize: u32,
    _pdirtyrectsbuffer: *mut windows::Win32::Foundation::RECT,
    _pdirtyrectsbuffersizerequired: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetFrameMoveRects(
    &self,
    _moverectsbuffersize: u32,
    _pmoverectbuffer: *mut DXGI_OUTDUPL_MOVE_RECT,
    _pmoverectsbuffersizerequired: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetFramePointerShape(
    &self,
    _pointershapebuffersize: u32,
    _ppointershapebuffer: *mut core::ffi::c_void,
    _ppointershapebuffersizerequired: *mut u32,
    _ppointershapeinfo: *mut DXGI_OUTDUPL_POINTER_SHAPE_INFO,
  ) -> windows_core::Result<()> {
    if self.0.shape_failure.load(Ordering::Acquire) {
      return Err(E_FAIL.into());
    }
    if _pointershapebuffersize < 64 {
      return Err(E_FAIL.into());
    }
    unsafe {
      std::ptr::write_bytes(_ppointershapebuffer, 255, 64);
      *_ppointershapebuffersizerequired = 64;
      *_ppointershapeinfo = DXGI_OUTDUPL_POINTER_SHAPE_INFO {
        Type: 2,
        Width: 4,
        Height: 4,
        Pitch: 16,
        ..Default::default()
      };
    }
    Ok(())
  }
  fn MapDesktopSurface(&self) -> windows_core::Result<DXGI_MAPPED_RECT> {
    Err(E_NOTIMPL.into())
  }
  fn UnMapDesktopSurface(&self) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn ReleaseFrame(&self) -> windows_core::Result<()> {
    self.0.releases.fetch_add(1, Ordering::AcqRel);
    if !self.0.held.swap(false, Ordering::AcqRel) || self.0.release_failure.load(Ordering::Acquire)
    {
      return Err(E_FAIL.into());
    }
    Ok(())
  }
}

fn frame(gpu: &Gpu, width: u32) -> IDXGIResource {
  let desc = D3D11_TEXTURE2D_DESC {
    Width: width,
    Height: gpu.height,
    MipLevels: 1,
    ArraySize: 1,
    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
    SampleDesc: DXGI_SAMPLE_DESC {
      Count: 1,
      Quality: 0,
    },
    Usage: D3D11_USAGE_DEFAULT,
    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
    ..Default::default()
  };
  let mut texture = None;
  unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.expect("frame texture");
  texture.expect("frame").cast().expect("DXGI resource")
}
pub(crate) fn fixture(gpu: &Gpu) -> (Capture, Arc<Shared>) {
  let state = Arc::new(Shared {
    info: Mutex::new(DXGI_OUTDUPL_FRAME_INFO::default()),
    resource: Mutex::new(Some(frame(gpu, gpu.width))),
    held: AtomicBool::new(false),
    releases: AtomicUsize::new(0),
    timeout: AtomicBool::new(false),
    acquire_failure: Mutex::new(None),
    release_failure: AtomicBool::new(false),
    shape_failure: AtomicBool::new(false),
    live: AtomicUsize::new(0),
  });
  let source = Capture {
    backend: Backend::Dxgi(Duplication::new(state.clone()).into()),
    valid: false,
    pointer: PointerShape::default(),
    seed: Default::default(),
    width: gpu.width,
    height: gpu.height,
    origin: (0, 0),
    presents: 0,
    pass: None,
  };
  (source, state)
}
fn arrow(_: super::super::cursor::DesktopArea) -> crate::Result<Option<PointerShape>> {
  Ok(Some(PointerShape {
    kind: 2,
    width: 1,
    height: 1,
    pitch: 4,
    bytes: vec![255; 4],
    x: 7,
    y: 9,
    visible: true,
    ..Default::default()
  }))
}
#[test]
fn a_recording_shows_the_cursor_before_windows_reports_any_shape_change() {
  let (gpu, _driver) = super::super::gpu::mock::fixture();
  let (mut source, frames) = fixture(&gpu);
  source.seed.pending = Some(((0, 0, 64, 64), arrow));
  source.acquire(&gpu).expect("first frame, no pointer data");
  assert_eq!(source.pointer.bytes, vec![255; 4]);
  assert_eq!(
    (source.pointer.x, source.pointer.y, source.pointer.visible),
    (7, 9, true)
  );
  assert_eq!(source.pointer.generation, 1);
  assert!(source.seed.pending.is_none(), "read once");
  {
    let mut info = frames.info.lock().expect("metadata");
    info.LastMouseUpdateTime = 1;
    info.PointerPosition.Visible = true.into();
    info.PointerPosition.Position.x = 20;
  }
  source.acquire(&gpu).expect("moved without a new shape");
  assert_eq!((source.pointer.x, source.pointer.bytes.len()), (20, 4));
}
#[test]
fn a_first_frame_position_from_windows_outranks_the_read_one() {
  let (gpu, _driver) = super::super::gpu::mock::fixture();
  let (mut source, frames) = fixture(&gpu);
  source.seed.pending = Some(((0, 0, 64, 64), arrow));
  {
    let mut info = frames.info.lock().expect("metadata");
    info.LastMouseUpdateTime = 1;
    info.PointerPosition.Visible = true.into();
    info.PointerPosition.Position.x = 30;
    info.PointerPosition.Position.y = 31;
  }
  source.acquire(&gpu).expect("positioned first frame");
  assert_eq!(source.pointer.bytes, vec![255; 4]);
  assert_eq!((source.pointer.x, source.pointer.y), (30, 31));
}
#[test]
fn a_shape_from_windows_needs_no_read_and_a_hidden_cursor_is_read_again() {
  let (gpu, _driver) = super::super::gpu::mock::fixture();
  let (mut source, frames) = fixture(&gpu);
  source.seed.pending = Some(((0, 0, 64, 64), |_| Ok(None)));
  source.acquire(&gpu).expect("hidden cursor");
  assert!(source.pointer.bytes.is_empty());
  assert!(source.seed.pending.is_some(), "still waiting for a cursor");
  source.seed.pending = Some(((0, 0, 64, 64), |_| {
    Err(EngineError::protocol("a delivered shape needs no read"))
  }));
  frames.info.lock().expect("metadata").PointerShapeBufferSize = 64;
  source.acquire(&gpu).expect("delivered shape");
  assert_eq!(source.pointer.bytes.len(), 64);
  assert!(source.seed.pending.is_none() && source.seed.failure.is_none());
}
#[test]
fn a_failed_cursor_read_keeps_recording_and_is_reported_once() {
  let (gpu, _driver) = super::super::gpu::mock::fixture();
  let (mut source, _frames) = fixture(&gpu);
  source.seed.pending = Some(((0, 0, 64, 64), |_| {
    Err(EngineError::protocol("cursor unreadable"))
  }));
  source
    .acquire(&gpu)
    .expect("frame survives the failed read");
  assert!(source.valid && source.pointer.bytes.is_empty());
  let mut composite = Composite::single(source);
  let failures = composite.take_pointer_failures();
  assert_eq!(failures.len(), 1);
  assert!(failures[0].reason.contains("cursor unreadable"));
  assert!(composite.take_pointer_failures().is_empty());
}
#[test]
fn unchanged_desktop_and_pointer_updates_remain_distinct_and_every_frame_is_returned() {
  let (gpu, driver) = super::super::gpu::mock::fixture();
  let (mut source, frames) = fixture(&gpu);
  frames.timeout.store(true, Ordering::Release);
  assert!(matches!(
    source.acquire(&gpu).expect("no new frame"),
    Acquisition::Unchanged
  ));
  assert_eq!(frames.releases.load(Ordering::Acquire), 0);
  frames.timeout.store(false, Ordering::Release);
  assert!(matches!(
    source.acquire(&gpu).expect("initial static frame"),
    Acquisition::New {
      source_qpc_100ns: None
    }
  ));
  assert!(source.valid);
  assert_eq!(frames.releases.load(Ordering::Acquire), 1);
  assert!(matches!(
    source.acquire(&gpu).expect("unchanged desktop"),
    Acquisition::Unchanged
  ));
  {
    let mut info = frames.info.lock().expect("metadata");
    info.LastMouseUpdateTime = 1;
    info.PointerShapeBufferSize = 64;
    info.PointerPosition.Visible = true.into();
    info.PointerPosition.Position.x = 20;
    info.PointerPosition.Position.y = 40;
  }
  assert!(matches!(
    source.acquire(&gpu).expect("pointer only"),
    Acquisition::Unchanged
  ));
  assert_eq!(source.pointer.bytes.len(), 64);
  assert_eq!(
    (source.pointer.x, source.pointer.y, source.pointer.visible),
    (20, 40, true)
  );
  assert_eq!(source.pointer.generation, 1);
  assert_eq!(
    driver
      .calls
      .lock()
      .expect("driver")
      .iter()
      .filter(|s| **s == "CopyResource")
      .count(),
    1
  );
  assert_eq!(frames.releases.load(Ordering::Acquire), 3);
  assert!(driver.unexpected.lock().expect("no readback").is_empty());
}
#[test]
fn missing_resources_bad_shapes_device_changes_and_cleanup_failures_keep_the_original_cause() {
  let (gpu, driver) = super::super::gpu::mock::fixture();
  let (mut source, frames) = fixture(&gpu);
  *frames.resource.lock().expect("resource") = None;
  assert!(source.acquire(&gpu).is_err());
  assert_eq!(frames.releases.load(Ordering::Acquire), 1);
  let (mut source, frames) = fixture(&gpu);
  frames.info.lock().expect("metadata").PointerShapeBufferSize = 1024 * 1024 + 1;
  assert!(source.acquire(&gpu).is_err());
  assert_eq!(frames.releases.load(Ordering::Acquire), 1);
  let (mut source, frames) = fixture(&gpu);
  frames.info.lock().expect("metadata").PointerShapeBufferSize = 64;
  frames.shape_failure.store(true, Ordering::Release);
  assert!(source.acquire(&gpu).is_err());
  assert_eq!(frames.releases.load(Ordering::Acquire), 1);
  let (mut source, frames) = fixture(&gpu);
  *frames.resource.lock().expect("resource") = Some(frame(&gpu, 512));
  frames.release_failure.store(true, Ordering::Release);
  let error = source.acquire(&gpu).err().expect("device failure");
  assert_eq!(error.kind, ErrorKind::DeviceLost);
  assert_eq!(error.stage, "capture format");
  assert!(error.reason.contains("DXGI release"));
  assert_eq!(frames.releases.load(Ordering::Acquire), 1);
  assert!(!frames.held.load(Ordering::Acquire));
  let (mut source, frames) = fixture(&gpu);
  frames.release_failure.store(true, Ordering::Release);
  assert_eq!(
    source.acquire(&gpu).err().expect("release failure").stage,
    "DXGI release"
  );
  assert_eq!(frames.releases.load(Ordering::Acquire), 1);
  assert!(driver.unexpected.lock().expect("no readback").is_empty());
}
#[test]
fn rotated_duplication_frames_join_the_cache_upright() {
  use super::super::frame_pass::{FramePass, Tone};
  use super::super::rotate::{Orientation, fixtures::*};
  let _com = super::super::Apartment::new().expect("COM");
  // A portrait desktop on a landscape panel (and the upside-down landscape case), on WARP.
  for (orientation, width, height) in [
    (Orientation::Rotate90, 24u32, 40u32),
    (Orientation::Rotate180, 40, 24),
    (Orientation::Rotate270, 24, 40),
  ] {
    let gpu = warp_gpu(width, height);
    let (panel_width, panel_height) = orientation.panel(width, height);
    let panel: IDXGIResource = numbered(&gpu, panel_width, panel_height)
      .cast()
      .expect("DXGI resource");
    let (mut source, frames) = fixture(&gpu);
    source.pass =
      Some(FramePass::new(&gpu.device, width, height, orientation, Tone::Sdr).expect("frame pass"));
    *frames.resource.lock().expect("resource") = Some(panel);
    frames.info.lock().expect("metadata").LastPresentTime = 1;
    assert!(matches!(
      source.acquire(&gpu).expect("rotated frame"),
      Acquisition::New { .. }
    ));
    assert_eq!(frames.releases.load(Ordering::Acquire), 1);
    let image = pixels(&gpu, &gpu.desktop, width, height);
    for y in 0..height {
      for x in 0..width {
        let (px, py) = orientation.panel_texel(width, height, x, y);
        assert_eq!(
          image[(y * width + x) as usize],
          coordinate(px, py),
          "{orientation:?} desktop ({x}, {y})"
        );
      }
    }
    // A frame at the desktop extent is not this display's scan-out and is refused.
    if panel_width != width {
      let (mut source, frames) = fixture(&gpu);
      source.pass = Some(
        FramePass::new(&gpu.device, width, height, orientation, Tone::Sdr).expect("frame pass"),
      );
      *frames.resource.lock().expect("resource") =
        Some(numbered(&gpu, width, height).cast().expect("DXGI resource"));
      assert_eq!(
        source
          .acquire(&gpu)
          .err()
          .expect("upright-sized frame")
          .stage,
        "capture format"
      );
      assert_eq!(frames.releases.load(Ordering::Acquire), 1);
    }
  }
}
#[test]
fn hdr_duplication_frames_join_the_cache_as_the_sdr_pixels_windows_composed() {
  use super::super::frame_pass::{FramePass, Tone};
  use super::super::rotate::{Orientation, fixtures::*};
  let _com = super::super::Apartment::new().expect("COM");
  let (width, height, nits) = (40u32, 24u32, 240.0f32);
  let gpu = warp_gpu(width, height);
  // SDR levels red = x, green = y, blue = 128, as Windows composes them into the FP16 desktop.
  let composed = |level: u32| {
    let c = level as f32 / 255.0;
    let linear = if c <= 0.040_45 {
      c / 12.92
    } else {
      ((c + 0.055) / 1.055).powf(2.4)
    };
    linear * nits / 80.0
  };
  let texels: Vec<[f32; 3]> = (0..height)
    .flat_map(|y| (0..width).map(move |x| [composed(x), composed(y), composed(128)]))
    .collect();
  let hdr_pass = || {
    FramePass::new(
      &gpu.device,
      width,
      height,
      Orientation::Upright,
      Tone::Hdr {
        sdr_white_nits: nits,
      },
    )
    .expect("HDR pass")
  };
  let (mut source, frames) = fixture(&gpu);
  source.pass = Some(hdr_pass());
  *frames.resource.lock().expect("resource") = Some(
    scrgb(&gpu, width, height, &texels)
      .cast()
      .expect("DXGI resource"),
  );
  frames.info.lock().expect("metadata").LastPresentTime = 1;
  assert!(matches!(
    source.acquire(&gpu).expect("HDR frame"),
    Acquisition::New { .. }
  ));
  assert_eq!(frames.releases.load(Ordering::Acquire), 1);
  let image = pixels(&gpu, &gpu.desktop, width, height);
  for y in 0..height {
    for x in 0..width {
      assert_eq!(
        image[(y * width + x) as usize],
        [128, y as u8, x as u8, 255],
        "desktop ({x}, {y})"
      );
    }
  }
  // A BGRA frame from an HDR display means its desktop format changed: refused, and released.
  let (mut source, frames) = fixture(&gpu);
  source.pass = Some(hdr_pass());
  *frames.resource.lock().expect("resource") =
    Some(numbered(&gpu, width, height).cast().expect("DXGI resource"));
  assert_eq!(
    source.acquire(&gpu).err().expect("SDR frame").stage,
    "capture format"
  );
  assert_eq!(frames.releases.load(Ordering::Acquire), 1);
}
