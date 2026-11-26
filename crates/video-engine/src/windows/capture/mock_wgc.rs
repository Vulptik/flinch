//! ABI-compatible WinRT doubles. No factory, capture session or GPU is opened.
#![allow(clippy::expect_used)]
#![allow(non_snake_case)] // Match the published native vtable method names.
use super::*;
use std::{
  collections::VecDeque,
  ffi::c_void,
  rc::Rc,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
};
use windows::{
  Foundation::{IClosable, IClosable_Impl, TimeSpan},
  Graphics::{
    DirectX::Direct3D11::{Direct3DSurfaceDescription, IDirect3DSurface, IDirect3DSurface_Impl},
    SizeInt32,
  },
  Win32::Foundation::{E_FAIL, E_NOTIMPL},
};
use windows_core::{HRESULT, HSTRING, IUnknown, IUnknown_Vtbl, implement, interface};

// windows-rs exposes these capture interfaces as consumption-only. These test
// declarations preserve their published IIDs, IInspectable prefix and ABI order.
#[interface("af86e2e0-b12d-4c6a-9c5a-d7aa65101e90")]
unsafe trait InspectableAbi: IUnknown {
  fn GetIids(&self, count: *mut u32, values: *mut *mut windows_core::GUID) -> HRESULT;
  fn GetRuntimeClassName(&self, name: *mut HSTRING) -> HRESULT;
  fn GetTrustLevel(&self, level: *mut i32) -> HRESULT;
}
#[interface("fa50c623-38da-4b32-acf3-fa9734ad800e")]
unsafe trait FrameAbi: InspectableAbi {
  fn Surface(&self, output: *mut *mut c_void) -> windows_core::Result<()>;
  fn SystemRelativeTime(&self, output: *mut TimeSpan) -> windows_core::Result<()>;
  fn ContentSize(&self, output: *mut SizeInt32) -> windows_core::Result<()>;
}
#[interface("24eb6d22-1975-422e-82e7-780dbd8ddf24")]
unsafe trait PoolAbi: InspectableAbi {
  fn Recreate(
    &self,
    device: *mut c_void,
    format: DirectXPixelFormat,
    count: i32,
    size: SizeInt32,
  ) -> HRESULT;
  fn TryGetNextFrame(&self, output: *mut *mut c_void) -> windows_core::Result<()>;
  fn FrameArrived(&self, handler: *mut c_void, token: *mut i64) -> windows_core::Result<()>;
  fn RemoveFrameArrived(&self, token: i64) -> windows_core::Result<()>;
  fn CreateCaptureSession(
    &self,
    item: *mut c_void,
    output: *mut *mut c_void,
  ) -> windows_core::Result<()>;
  fn DispatcherQueue(&self, output: *mut *mut c_void) -> HRESULT;
}
#[interface("814e42a9-f70f-4ad7-939b-fddcc6eb880d")]
unsafe trait SessionAbi: InspectableAbi {
  fn StartCapture(&self) -> windows_core::Result<()>;
}
#[interface("2c39ae40-7d2e-5044-804e-8b6799d4cf9e")]
unsafe trait Session2Abi: InspectableAbi {
  fn IsCursorCaptureEnabled(&self, output: *mut bool) -> HRESULT;
  fn SetIsCursorCaptureEnabled(&self, enabled: bool) -> windows_core::Result<()>;
}
#[interface("79c3f95b-31f7-4ec2-a464-632ef5d30760")]
unsafe trait ItemAbi: InspectableAbi {
  fn DisplayName(&self, output: *mut *mut c_void) -> HRESULT;
  fn Size(&self, output: *mut SizeInt32) -> HRESULT;
  fn Closed(&self, handler: *mut c_void, token: *mut i64) -> windows_core::Result<()>;
  fn RemoveClosed(&self, token: i64) -> windows_core::Result<()>;
}
macro_rules! inspectable {
  ($($ty:ty),+) => { $(impl InspectableAbi_Impl for $ty {
    unsafe fn GetIids(&self, _: *mut u32, _: *mut *mut windows_core::GUID) -> HRESULT { E_NOTIMPL }
    unsafe fn GetRuntimeClassName(&self, _: *mut HSTRING) -> HRESULT { E_NOTIMPL }
    unsafe fn GetTrustLevel(&self, _: *mut i32) -> HRESULT { E_NOTIMPL }
  })+ };
}
#[derive(Default)]
struct Shared {
  calls: Mutex<Vec<&'static str>>,
  failure: Mutex<Option<&'static str>>,
  secondary_failure: Mutex<Option<&'static str>>,
  frames: Mutex<VecDeque<Direct3D11CaptureFrame>>,
  handler: Mutex<Option<TypedEventHandler<Direct3D11CaptureFramePool, IInspectable>>>,
  closed_handler: Mutex<Option<TypedEventHandler<GraphicsCaptureItem, IInspectable>>>,
  cursor: AtomicBool,
  close_on_surface: AtomicBool,
}
#[implement(ItemAbi)]
struct Item(Rc<Shared>);
inspectable!(Item_Impl);
impl ItemAbi_Impl for Item_Impl {
  unsafe fn DisplayName(&self, _: *mut *mut c_void) -> HRESULT {
    E_NOTIMPL
  }
  unsafe fn Size(&self, _: *mut SizeInt32) -> HRESULT {
    E_NOTIMPL
  }
  unsafe fn Closed(&self, handler: *mut c_void, token: *mut i64) -> windows_core::Result<()> {
    self.0.call("closed.register")?;
    // SAFETY: retain a clone of the borrowed ABI delegate and write the caller's event token.
    unsafe {
      *self.0.closed_handler.lock().expect("closed handler") = Some(
        TypedEventHandler::from_raw_borrowed(&handler)
          .expect("closed delegate")
          .clone(),
      );
      *token = 29;
    }
    Ok(())
  }
  unsafe fn RemoveClosed(&self, token: i64) -> windows_core::Result<()> {
    assert_eq!(token, 29);
    self.0.closed_handler.lock().expect("closed handler").take();
    self.0.call("closed.remove")
  }
}
fn item(state: &Rc<Shared>) -> GraphicsCaptureItem {
  assert_eq!(
    std::mem::size_of::<ItemAbi_Vtbl>(),
    std::mem::size_of::<IGraphicsCaptureItem_Vtbl>()
  );
  let abi: ItemAbi = Item(state.clone()).into();
  // SAFETY: identical published IID and IInspectable-prefixed ABI; transfer its owned reference.
  unsafe { GraphicsCaptureItem::from_raw(abi.into_raw()) }
}
impl Shared {
  fn call(&self, name: &'static str) -> windows_core::Result<()> {
    self.calls.lock().expect("calls").push(name);
    if *self.failure.lock().expect("failure") == Some(name)
      || *self.secondary_failure.lock().expect("secondary failure") == Some(name)
    {
      Err(E_FAIL.into())
    } else {
      Ok(())
    }
  }
  fn count(&self, name: &str) -> usize {
    self
      .calls
      .lock()
      .expect("calls")
      .iter()
      .filter(|v| **v == name)
      .count()
  }
}
#[implement(FrameAbi, IClosable)]
struct Frame {
  state: Rc<Shared>,
  texture: ID3D11Texture2D,
  timestamp: i64,
  width: i32,
  height: i32,
}
inspectable!(Frame_Impl);
impl FrameAbi_Impl for Frame_Impl {
  unsafe fn Surface(&self, output: *mut *mut c_void) -> windows_core::Result<()> {
    self.state.call("surface")?;
    if self.state.close_on_surface.load(Ordering::Acquire) {
      self
        .state
        .closed_handler
        .lock()
        .expect("closed handler")
        .as_ref()
        .expect("registered closure")
        .Invoke(None, None)?;
    }
    let surface: IDirect3DSurface = Surface {
      state: self.state.clone(),
      texture: self.texture.clone(),
    }
    .into();
    // SAFETY: the caller supplies an interface output; transfer one owned reference.
    unsafe {
      *output = surface.into_raw();
    }
    Ok(())
  }
  unsafe fn SystemRelativeTime(&self, output: *mut TimeSpan) -> windows_core::Result<()> {
    self.state.call("timestamp")?;
    // SAFETY: the ABI caller provides a live TimeSpan output.
    unsafe {
      *output = TimeSpan {
        Duration: self.timestamp,
      };
    }
    Ok(())
  }
  unsafe fn ContentSize(&self, output: *mut SizeInt32) -> windows_core::Result<()> {
    self.state.call("size")?;
    // SAFETY: the ABI caller provides a live SizeInt32 output.
    unsafe {
      *output = SizeInt32 {
        Width: self.width,
        Height: self.height,
      };
    }
    Ok(())
  }
}
impl IClosable_Impl for Frame_Impl {
  fn Close(&self) -> windows_core::Result<()> {
    self.state.call("frame.close")
  }
}
#[implement(IDirect3DSurface, IClosable, IDirect3DDxgiInterfaceAccess)]
struct Surface {
  state: Rc<Shared>,
  texture: ID3D11Texture2D,
}
impl IDirect3DSurface_Impl for Surface_Impl {
  fn Description(&self) -> windows_core::Result<Direct3DSurfaceDescription> {
    Ok(Default::default())
  }
}
impl IClosable_Impl for Surface_Impl {
  fn Close(&self) -> windows_core::Result<()> {
    Ok(())
  }
}
impl IDirect3DDxgiInterfaceAccess_Impl for Surface_Impl {
  fn GetInterface(
    &self,
    iid: *const windows_core::GUID,
    output: *mut *mut c_void,
  ) -> windows_core::Result<()> {
    self.state.call("texture")?;
    // SAFETY: the consumer requests the texture IID and transfers one owned reference.
    unsafe {
      assert_eq!(*iid, ID3D11Texture2D::IID);
      *output = self.texture.clone().into_raw();
    }
    Ok(())
  }
}
#[implement(PoolAbi, IClosable)]
struct Pool(Rc<Shared>);
inspectable!(Pool_Impl);
impl PoolAbi_Impl for Pool_Impl {
  unsafe fn Recreate(
    &self,
    _: *mut c_void,
    _: DirectXPixelFormat,
    _: i32,
    _: SizeInt32,
  ) -> HRESULT {
    E_NOTIMPL
  }
  unsafe fn TryGetNextFrame(&self, output: *mut *mut c_void) -> windows_core::Result<()> {
    if *self.0.failure.lock().expect("failure") == Some("next.pointer") {
      return Err(windows::Win32::Foundation::E_POINTER.into());
    }
    self.0.call("next")?;
    // SAFETY: the ABI output receives one owned reference or successful null for an empty pool.
    unsafe {
      *output = self
        .0
        .frames
        .lock()
        .expect("frames")
        .pop_front()
        .map_or(std::ptr::null_mut(), Interface::into_raw);
    }
    Ok(())
  }
  unsafe fn FrameArrived(&self, handler: *mut c_void, token: *mut i64) -> windows_core::Result<()> {
    self.0.call("register")?;
    // SAFETY: clone the borrowed delegate; the pool retains it until removal.
    unsafe {
      *self.0.handler.lock().expect("handler") = Some(
        TypedEventHandler::from_raw_borrowed(&handler)
          .expect("non-null handler")
          .clone(),
      );
      *token = 17;
    }
    Ok(())
  }
  unsafe fn RemoveFrameArrived(&self, token: i64) -> windows_core::Result<()> {
    assert_eq!(token, 17);
    self.0.handler.lock().expect("handler").take();
    self.0.call("remove")
  }
  unsafe fn CreateCaptureSession(
    &self,
    _: *mut c_void,
    output: *mut *mut c_void,
  ) -> windows_core::Result<()> {
    self.0.call("session")?;
    let session: SessionAbi = Session(self.0.clone()).into();
    // SAFETY: the ABI output receives this session's single owned reference.
    unsafe {
      *output = session.into_raw();
    }
    Ok(())
  }
  unsafe fn DispatcherQueue(&self, _: *mut *mut c_void) -> HRESULT {
    E_NOTIMPL
  }
}
impl IClosable_Impl for Pool_Impl {
  fn Close(&self) -> windows_core::Result<()> {
    self.0.call("pool.close")
  }
}
#[implement(SessionAbi, Session2Abi, IClosable)]
struct Session(Rc<Shared>);
inspectable!(Session_Impl);
impl SessionAbi_Impl for Session_Impl {
  unsafe fn StartCapture(&self) -> windows_core::Result<()> {
    self.0.call("start")
  }
}
impl Session2Abi_Impl for Session_Impl {
  unsafe fn IsCursorCaptureEnabled(&self, _: *mut bool) -> HRESULT {
    E_NOTIMPL
  }
  unsafe fn SetIsCursorCaptureEnabled(&self, enabled: bool) -> windows_core::Result<()> {
    self.0.call("cursor")?;
    self.0.cursor.store(enabled, Ordering::Release);
    Ok(())
  }
}
impl IClosable_Impl for Session_Impl {
  fn Close(&self) -> windows_core::Result<()> {
    self.0.call("session.close")
  }
}
fn pool(state: &Rc<Shared>) -> Direct3D11CaptureFramePool {
  assert_eq!(
    std::mem::size_of::<PoolAbi_Vtbl>(),
    std::mem::size_of::<IDirect3D11CaptureFramePool_Vtbl>()
  );
  let abi: PoolAbi = Pool(state.clone()).into();
  // SAFETY: identical IID and published ABI, including the IInspectable prefix.
  unsafe { Direct3D11CaptureFramePool::from_raw(abi.into_raw()) }
}
fn queue(state: &Rc<Shared>, gpu: &Gpu, timestamp: i64, width: i32) {
  queue_texture(state, &gpu.desktop, timestamp, (width, 720));
}
fn queue_texture(
  state: &Rc<Shared>,
  texture: &ID3D11Texture2D,
  timestamp: i64,
  (width, height): (i32, i32),
) {
  assert_eq!(
    std::mem::size_of::<FrameAbi_Vtbl>(),
    std::mem::size_of::<IDirect3D11CaptureFrame_Vtbl>()
  );
  let abi: FrameAbi = Frame {
    state: state.clone(),
    texture: texture.clone(),
    timestamp,
    width,
    height,
  }
  .into();
  // SAFETY: identical IID and ABI; the queue owns this single COM reference.
  let frame = unsafe { Direct3D11CaptureFrame::from_raw(abi.into_raw()) };
  state.frames.lock().expect("frames").push_back(frame);
}
fn fixture() -> (
  Gpu,
  Capture,
  Rc<Shared>,
  Arc<super::super::gpu::mock::State>,
) {
  let (gpu, driver) = super::super::gpu::mock::fixture();
  let state = Rc::new(Shared::default());
  let resources =
    WgcResources::prepare(pool(&state), Some(&item(&state)), true).expect("mock WGC preparation");
  let capture = Capture {
    backend: Backend::Wgc(resources),
    valid: false,
    pointer: PointerShape::default(),
    seed: Default::default(),
    width: 1280,
    height: 720,
    origin: (0, 0),
    presents: 0,
    pass: None,
  };
  (gpu, capture, state, driver)
}
fn signal(state: &Shared) {
  let handler = state
    .handler
    .lock()
    .expect("handler")
    .clone()
    .expect("registered callback");
  handler.Invoke(None, None).expect("availability callback");
}
pub(crate) fn shutdown_failure_fixture(gpu: &Gpu) -> Capture {
  let state = Rc::new(Shared::default());
  let resources =
    WgcResources::prepare(pool(&state), Some(&item(&state)), false).expect("source double");
  *state.failure.lock().expect("close failure") = Some("session.close");
  Capture {
    backend: Backend::Wgc(resources),
    valid: true,
    pointer: PointerShape::default(),
    seed: Default::default(),
    width: gpu.width,
    height: gpu.height,
    origin: (0, 0),
    presents: 0,
    pass: None,
  }
}
#[test]
fn explicit_shutdown_revokes_cached_pixels_closes_once_and_retains_every_cleanup_failure() {
  for (failure, stage) in [
    ("closed.remove", "WGC closure callback removal"),
    ("remove", "WGC callback removal"),
    ("session.close", "WGC close"),
    ("pool.close", "WGC pool close"),
  ] {
    let (gpu, mut capture, state, _) = fixture();
    capture.valid = true;
    capture.pointer.visible = true;
    let callback = state
      .handler
      .lock()
      .expect("handler")
      .clone()
      .expect("registered callback");
    *state.failure.lock().expect("failure") = Some(failure);
    if failure == "session.close" {
      *state
        .secondary_failure
        .lock()
        .expect("second close failure") = Some("pool.close");
    }
    let error = capture
      .stop()
      .expect_err("cleanup failure must be observable");
    assert_eq!(error.stage, stage);
    assert_eq!(error.native_code, Some(E_FAIL.0));
    if failure == "session.close" {
      assert!(error.reason.contains("WGC pool close"));
    }
    assert!(!capture.valid && !capture.pointer.visible);
    assert_eq!(
      capture
        .stop()
        .expect_err("failed shutdown stays failed")
        .stage,
      stage
    );
    assert!(capture.start().is_err());
    assert!(capture.acquire(&gpu).is_err());
    drop(capture);
    for operation in ["closed.remove", "remove", "session.close", "pool.close"] {
      assert_eq!(state.count(operation), 1);
    }
    callback
      .Invoke(None, None)
      .expect("late callback retains only its atomic signal");
  }
  let (gpu, mut capture, state, _) = fixture();
  capture.stop().expect("normal close");
  capture.stop().expect("normal close is idempotent");
  assert!(capture.start().is_err());
  assert!(capture.acquire(&gpu).is_err());
  assert_eq!(state.count("pool.close"), 1);
}
#[test]
fn closed_source_cannot_keep_encoding_cached_content_and_late_callbacks_are_safe() {
  let (gpu, mut capture, state, driver) = fixture();
  queue(&state, &gpu, 10, 1280);
  signal(&state);
  capture.acquire(&gpu).expect("seed valid cached image");
  let closed = state
    .closed_handler
    .lock()
    .expect("closed handler")
    .clone()
    .expect("registered closure callback");
  let calls = driver.calls.lock().expect("GPU calls").len();
  closed.Invoke(None, None).expect("closure signal");
  assert_eq!(
    driver.calls.lock().expect("GPU calls").len(),
    calls,
    "callback performs no native work"
  );
  let error = capture
    .acquire(&gpu)
    .err()
    .expect("cached image cannot conceal closure");
  assert_eq!(error.kind, ErrorKind::DeviceLost);
  assert_eq!(error.stage, "WGC source closed");
  assert!(
    capture.start().is_err(),
    "known closure cannot acknowledge Start"
  );
  drop(capture);
  assert_eq!(state.count("closed.remove"), 1);
  assert!(
    state
      .closed_handler
      .lock()
      .expect("removed closure delegate")
      .is_none()
  );
  let calls = state.calls.lock().expect("native calls").len();
  closed
    .Invoke(None, None)
    .expect("late callback only changes its retained atomic");
  assert_eq!(state.calls.lock().expect("native calls").len(), calls);
}
#[test]
fn closure_during_frame_work_closes_the_frame_and_prevents_success() {
  let (gpu, mut capture, state, _) = fixture();
  queue(&state, &gpu, 10, 1280);
  state.close_on_surface.store(true, Ordering::Release);
  signal(&state);
  let error = capture
    .acquire(&gpu)
    .err()
    .expect("closure while processing frame");
  assert_eq!(error.kind, ErrorKind::DeviceLost);
  assert_eq!(state.count("frame.close"), 1);
  assert!(!capture.valid);
}
#[test]
fn callback_only_signals_and_frames_are_copied_in_order_then_closed_without_cpu_access() {
  let (gpu, mut capture, state, driver) = fixture();
  assert!(state.cursor.load(Ordering::Acquire));
  assert_eq!(state.count("start"), 0);
  capture.start().expect("explicit start");
  queue(&state, &gpu, 10, 1280);
  queue(&state, &gpu, 20, 1280);
  signal(&state);
  assert_eq!(state.count("next"), 0);
  assert!(matches!(
    capture.acquire(&gpu).expect("latest GPU content"),
    Acquisition::New {
      source_qpc_100ns: Some(20)
    }
  ));
  assert!(capture.valid);
  assert_eq!(state.count("frame.close"), 2);
  let copies = driver
    .calls
    .lock()
    .expect("driver")
    .iter()
    .filter(|v| **v == "CopyResource")
    .count();
  assert_eq!(copies, 2);
  queue(&state, &gpu, 20, 1280);
  signal(&state);
  assert!(matches!(
    capture.acquire(&gpu).expect("same timestamp"),
    Acquisition::Unchanged
  ));
  assert_eq!(
    driver
      .calls
      .lock()
      .expect("driver")
      .iter()
      .filter(|v| **v == "CopyResource")
      .count(),
    copies
  );
  assert!(matches!(
    capture.acquire(&gpu).expect("no signal"),
    Acquisition::Unchanged
  ));
  assert!(
    driver
      .unexpected
      .lock()
      .expect("no CPU readback")
      .is_empty()
  );
  drop(capture);
  assert_eq!(state.count("remove"), 1);
  assert_eq!(state.count("session.close"), 1);
  assert_eq!(state.count("pool.close"), 1);
}
#[test]
fn partial_preparation_cleans_registered_callbacks_sessions_and_pool_without_starting_capture() {
  for (failure, removed, closed) in [
    ("closed.register", 0, 0),
    ("register", 0, 0),
    ("session", 1, 0),
    ("cursor", 1, 1),
  ] {
    let state = Rc::new(Shared::default());
    *state.failure.lock().expect("failure") = Some(failure);
    let error = WgcResources::prepare(pool(&state), Some(&item(&state)), true)
      .err()
      .expect("preparation failure");
    assert_eq!(error.native_code, Some(E_FAIL.0));
    assert_eq!(state.count("remove"), removed);
    assert_eq!(state.count("session.close"), closed);
    assert_eq!(state.count("pool.close"), 1);
    assert_eq!(state.count("start"), 0);
    assert!(state.handler.lock().expect("removed delegate").is_none());
    assert!(
      state
        .closed_handler
        .lock()
        .expect("removed closure delegate")
        .is_none()
    );
    assert_eq!(
      state.count("closed.remove"),
      usize::from(failure != "closed.register")
    );
  }
}
#[test]
fn only_successful_null_means_empty_and_cleanup_continues_after_a_native_close_failure() {
  let (gpu, mut capture, state, _) = fixture();
  signal(&state);
  assert!(matches!(
    capture.acquire(&gpu).expect("successful null frame"),
    Acquisition::Unchanged
  ));
  assert!(!capture.valid);
  *state.failure.lock().expect("failure") = Some("next.pointer");
  signal(&state);
  let error = capture.acquire(&gpu).err().expect("genuine failed HRESULT");
  assert_eq!(
    error.native_code,
    Some(windows::Win32::Foundation::E_POINTER.0)
  );
  for failure in ["closed.remove", "remove", "session.close", "pool.close"] {
    let (_, capture, state, _) = fixture();
    let retained_handler = state
      .handler
      .lock()
      .expect("handler")
      .clone()
      .expect("callback");
    *state.failure.lock().expect("failure") = Some(failure);
    drop(capture);
    assert_eq!(state.count("remove"), 1);
    assert_eq!(state.count("session.close"), 1);
    assert_eq!(state.count("pool.close"), 1);
    assert_eq!(state.count("closed.remove"), 1);
    let calls = state.calls.lock().expect("calls").len();
    retained_handler
      .Invoke(None, None)
      .expect("late notification only sets a flag");
    assert_eq!(state.calls.lock().expect("calls").len(), calls);
  }
}
#[test]
fn failed_frames_are_closed_and_native_causes_or_changed_dimensions_are_explicit() {
  for failure in [
    "size",
    "surface",
    "texture",
    "timestamp",
    "frame.close",
    "dimensions",
    "negative",
    "reversed",
    "next",
    "start",
  ] {
    let (gpu, mut capture, state, _) = fixture();
    if failure == "start" {
      *state.failure.lock().expect("failure") = Some(failure);
      assert!(capture.start().is_err());
      continue;
    }
    if failure == "reversed" {
      queue(&state, &gpu, 20, 1280);
      signal(&state);
      capture.acquire(&gpu).expect("first frame");
    }
    queue(
      &state,
      &gpu,
      if failure == "negative" { -1 } else { 10 },
      if failure == "dimensions" { 1282 } else { 1280 },
    );
    if !["dimensions", "negative", "reversed"].contains(&failure) {
      *state.failure.lock().expect("failure") = Some(failure);
    }
    signal(&state);
    let error = capture.acquire(&gpu).err().expect("explicit frame failure");
    if !["dimensions", "negative", "reversed"].contains(&failure) {
      assert_eq!(error.native_code, Some(E_FAIL.0));
    }
    assert_eq!(
      state.count("frame.close"),
      if failure == "next" {
        0
      } else if failure == "reversed" {
        2
      } else {
        1
      }
    );
    // Clear an unconsumed fixture frame on failed retrieval to release test references.
    state.frames.lock().expect("frames").clear();
  }
}
#[test]
fn hdr_wgc_frames_join_the_cache_as_the_sdr_pixels_windows_composed() {
  use super::super::frame_pass::{FramePass, Tone};
  use super::super::rotate::{Orientation, fixtures::*};
  let _com = super::super::Apartment::new().expect("COM");
  let (width, height, nits) = (40u32, 24u32, 480.0f32);
  let gpu = warp_gpu(width, height);
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
    .flat_map(|y| (0..width).map(move |x| [composed(x * 6), composed(y * 10), composed(64)]))
    .collect();
  let state = Rc::new(Shared::default());
  let resources =
    WgcResources::prepare(pool(&state), Some(&item(&state)), true).expect("mock WGC preparation");
  // WGC composes the monitor upright, so an HDR display needs only the tone mapping.
  let mut capture = Capture {
    backend: Backend::Wgc(resources),
    valid: false,
    pointer: PointerShape::default(),
    seed: Default::default(),
    width,
    height,
    origin: (0, 0),
    presents: 0,
    pass: Some(
      FramePass::new(
        &gpu.device,
        width,
        height,
        Orientation::Upright,
        Tone::Hdr {
          sdr_white_nits: nits,
        },
      )
      .expect("HDR pass"),
    ),
  };
  let frame = scrgb(&gpu, width, height, &texels);
  queue_texture(&state, &frame, 10, (width as i32, height as i32));
  signal(&state);
  assert!(matches!(
    capture.acquire(&gpu).expect("HDR WGC frame"),
    Acquisition::New { .. }
  ));
  let image = pixels(&gpu, &gpu.desktop, width, height);
  for y in 0..height {
    for x in 0..width {
      assert_eq!(
        image[(y * width + x) as usize],
        [64, (y * 10) as u8, (x * 6) as u8, 255],
        "desktop ({x}, {y})"
      );
    }
  }
  // A BGRA frame from an HDR display means its desktop format changed.
  queue_texture(&state, &gpu.desktop, 20, (width as i32, height as i32));
  signal(&state);
  assert_eq!(
    capture.acquire(&gpu).err().expect("SDR frame").stage,
    "capture format"
  );
  capture.stop().expect("close");
}
