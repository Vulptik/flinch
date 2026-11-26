//! Script the duplication interface and map real software D3D textures, never a desktop output.
use super::*;
use std::{
  collections::VecDeque,
  ffi::c_void,
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  },
};
use windows::Win32::Graphics::{Direct3D::D3D_DRIVER_TYPE_WARP, Direct3D11::*, Dxgi::*};
use windows_core::{GUID, HRESULT, IUnknown, OutRef, Ref, implement};

enum Event {
  Frame {
    present: i64,
    metadata: u32,
    resource: bool,
  },
  Error(HRESULT),
}
struct Script {
  events: Mutex<VecDeque<Event>>,
  held: AtomicBool,
  released: AtomicUsize,
  timeouts: Mutex<Vec<u32>>,
}
#[implement(IDXGIOutputDuplication)]
struct Duplication {
  texture: ID3D11Texture2D,
  script: Arc<Script>,
}
fn unsupported() -> windows_core::Error {
  windows_core::Error::from_hresult(HRESULT(0x80004001u32 as i32))
}
impl IDXGIObject_Impl for Duplication_Impl {
  fn SetPrivateData(&self, _: *const GUID, _: u32, _: *const c_void) -> windows_core::Result<()> {
    Err(unsupported())
  }
  fn SetPrivateDataInterface(&self, _: *const GUID, _: Ref<IUnknown>) -> windows_core::Result<()> {
    Err(unsupported())
  }
  fn GetPrivateData(
    &self,
    _: *const GUID,
    _: *mut u32,
    _: *mut c_void,
  ) -> windows_core::Result<()> {
    Err(unsupported())
  }
  fn GetParent(&self, _: *const GUID, _: *mut *mut c_void) -> windows_core::Result<()> {
    Err(unsupported())
  }
}
impl IDXGIOutputDuplication_Impl for Duplication_Impl {
  fn GetDesc(&self, desc: *mut DXGI_OUTDUPL_DESC) {
    // SAFETY: the COM ABI caller supplies writable descriptor storage.
    unsafe {
      desc.write(Default::default());
    }
  }
  fn AcquireNextFrame(
    &self,
    timeout: u32,
    info: *mut DXGI_OUTDUPL_FRAME_INFO,
    resource: OutRef<IDXGIResource>,
  ) -> windows_core::Result<()> {
    assert!(
      !self.script.held.load(Ordering::Acquire),
      "previous frame was not released"
    );
    self.script.timeouts.lock().unwrap().push(timeout);
    let event = self
      .script
      .events
      .lock()
      .unwrap()
      .pop_front()
      .unwrap_or(Event::Error(dxgi_api::DXGI_ERROR_WAIT_TIMEOUT));
    match event {
      Event::Error(error) => Err(windows_core::Error::from_hresult(error)),
      Event::Frame {
        present,
        metadata,
        resource: provide,
      } => {
        self.script.held.store(true, Ordering::Release);
        // SAFETY: the ABI caller owns this frame-info output through the call.
        unsafe {
          info.write(DXGI_OUTDUPL_FRAME_INFO {
            LastPresentTime: present,
            TotalMetadataBufferSize: metadata,
            AccumulatedFrames: 1,
            ..Default::default()
          });
        }
        resource.write(if provide {
          Some(self.texture.cast()?)
        } else {
          None
        })
      },
    }
  }
  fn GetFrameDirtyRects(
    &self,
    _: u32,
    _: *mut windows::Win32::Foundation::RECT,
    _: *mut u32,
  ) -> windows_core::Result<()> {
    Err(unsupported())
  }
  fn GetFrameMoveRects(
    &self,
    _: u32,
    _: *mut DXGI_OUTDUPL_MOVE_RECT,
    _: *mut u32,
  ) -> windows_core::Result<()> {
    Err(unsupported())
  }
  fn GetFramePointerShape(
    &self,
    _: u32,
    _: *mut c_void,
    _: *mut u32,
    _: *mut DXGI_OUTDUPL_POINTER_SHAPE_INFO,
  ) -> windows_core::Result<()> {
    Err(unsupported())
  }
  fn MapDesktopSurface(&self) -> windows_core::Result<DXGI_MAPPED_RECT> {
    Err(unsupported())
  }
  fn UnMapDesktopSurface(&self) -> windows_core::Result<()> {
    Err(unsupported())
  }
  fn ReleaseFrame(&self) -> windows_core::Result<()> {
    assert!(
      self.script.held.swap(false, Ordering::AcqRel),
      "unacquired frame released"
    );
    self.script.released.fetch_add(1, Ordering::Relaxed);
    Ok(())
  }
}

fn fixture(events: Vec<Event>, bgra: [u8; 4]) -> (DxgiCapture, Arc<Script>) {
  fixture_with_size(events, bgra, 3, 2)
}
fn fixture_with_size(
  events: Vec<Event>,
  bgra: [u8; 4],
  width: u32,
  height: u32,
) -> (DxgiCapture, Arc<Script>) {
  let (mut device, mut context) = (None, None);
  // SAFETY: creates a software device only; no adapter output or desktop duplication is opened.
  unsafe {
    D3D11CreateDevice(
      None,
      D3D_DRIVER_TYPE_WARP,
      HMODULE::default(),
      D3D11_CREATE_DEVICE_FLAG::default(),
      Some(&[D3D_FEATURE_LEVEL_11_0]),
      D3D11_SDK_VERSION,
      Some(&mut device),
      None,
      Some(&mut context),
    )
    .unwrap();
  }
  let device = device.unwrap();
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
    Usage: D3D11_USAGE_DEFAULT,
    ..Default::default()
  };
  let pixels = bgra.repeat((width * height) as usize);
  let input = D3D11_SUBRESOURCE_DATA {
    pSysMem: pixels.as_ptr().cast(),
    SysMemPitch: width * 4,
    SysMemSlicePitch: width * height * 4,
  };
  let mut texture = None;
  // SAFETY: the complete 3x2 source buffer remains alive while D3D copies it.
  unsafe {
    device
      .CreateTexture2D(&desc, Some(&input), Some(&mut texture))
      .unwrap();
  }
  let script = Arc::new(Script {
    events: Mutex::new(events.into()),
    held: AtomicBool::new(false),
    released: AtomicUsize::new(0),
    timeouts: Mutex::new(Vec::new()),
  });
  let duplication: IDXGIOutputDuplication = Duplication {
    texture: texture.unwrap(),
    script: script.clone(),
  }
  .into();
  // SAFETY: factory creation enumerates nothing and cannot acquire a desktop frame.
  let factory = unsafe { CreateDXGIFactory1() }.unwrap();
  let capture = DxgiCapture {
    inner: Some(DxgiCaptureInner {
      factory,
      device,
      context: context.unwrap(),
      duplication,
      staging: None,
      width,
      height,
      monitor_x: 0,
      monitor_y: 0,
      cached_bgra: Vec::new(),
      cache_valid: AtomicBool::new(false),
    }),
    ..DxgiCapture::new_for_monitor(0, 0, width, height)
  };
  (capture, script)
}
fn frame(present: i64) -> Event {
  Event::Frame {
    present,
    metadata: 16,
    resource: true,
  }
}

#[test]
fn aligned_staging_rows_preserve_every_pixel_in_warmup_screenshots_and_video() {
  for width in [64, 128] {
    for video in [false, true] {
      let (mut capture, script) =
        fixture_with_size(vec![frame(1), frame(2)], [13, 29, 47, 255], width, 2);
      let old = capture.inner.take().unwrap();
      capture.inner = Some(
        build_dxgi_pipeline(
          old.factory,
          old.device,
          old.context,
          (0, 0, width, 2),
          (Instant::now(), 100),
          |_| Ok(old.duplication),
        )
        .unwrap(),
      );
      assert_eq!(
        capture.inner.as_ref().unwrap().cached_bgra,
        [13, 29, 47, 255].repeat(width as usize * 2)
      );
      let pixels = if video {
        capture.capture_for_video().unwrap().unwrap().2
      } else {
        capture.capture().unwrap().2
      };
      assert_eq!(pixels, [47, 29, 13, 255].repeat(width as usize * 2));
      assert_eq!(script.released.load(Ordering::Acquire), 2);
      assert!(!script.held.load(Ordering::Acquire));
    }
  }
}

#[test]
fn initialization_warmup_validates_frames_and_releases_every_acquisition() {
  for (events, expected, releases) in [
    (vec![frame(1)], true, 1),
    (vec![frame(0)], false, 1),
    (
      vec![Event::Frame {
        present: 1,
        metadata: 16,
        resource: false,
      }],
      false,
      1,
    ),
    (
      vec![Event::Error(dxgi_api::DXGI_ERROR_WAIT_TIMEOUT)],
      false,
      0,
    ),
  ] {
    let (mut capture, script) = fixture(events, [10, 20, 30, 255]);
    let old = capture.inner.take().unwrap();
    let inner = build_dxgi_pipeline(
      old.factory,
      old.device,
      old.context,
      (-30, 17, 3, 2),
      (Instant::now(), 100),
      |_| Ok(old.duplication),
    )
    .unwrap();
    assert_eq!(
      (inner.monitor_x, inner.monitor_y, inner.width, inner.height),
      (-30, 17, 3, 2)
    );
    assert_eq!(inner.cache_valid.load(Ordering::Acquire), expected);
    assert!(inner.staging.is_some());
    if expected {
      assert_eq!(inner.cached_bgra, [10, 20, 30, 255].repeat(6));
    } else {
      assert!(inner.cached_bgra.is_empty());
    }
    assert_eq!(script.released.load(Ordering::Acquire), releases);
    assert!(!script.held.load(Ordering::Acquire));
    assert!(script.timeouts.lock().unwrap().iter().all(|&v| v <= 100));
  }
  for code in [
    dxgi_api::DXGI_ERROR_ACCESS_DENIED,
    dxgi_api::DXGI_ERROR_SESSION_DISCONNECTED,
    dxgi_api::DXGI_ERROR_UNSUPPORTED,
    HRESULT(0x80004005u32 as i32),
  ] {
    let (mut capture, script) = fixture(vec![], [0; 4]);
    let old = capture.inner.take().unwrap();
    let error = build_dxgi_pipeline(
      old.factory,
      old.device,
      old.context,
      (0, 0, 3, 2),
      (Instant::now(), 100),
      |_| Err(windows_core::Error::from_hresult(code)),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("Failed to duplicate output"));
    assert_eq!(
      error.downcast_ref::<windows_core::Error>().unwrap().code(),
      code
    );
    assert!(script.timeouts.lock().unwrap().is_empty());
  }
}

#[test]
fn software_texture_capture_packs_rows_reuses_cache_and_recreates_trimmed_staging() {
  let (mut capture, script) = fixture(
    vec![
      frame(1),
      // A pointer-only event never confirms the cache; the next wait times out.
      frame(0),
      Event::Error(dxgi_api::DXGI_ERROR_WAIT_TIMEOUT),
      Event::Error(dxgi_api::DXGI_ERROR_WAIT_TIMEOUT),
      frame(2),
    ],
    [10, 20, 30, 255],
  );
  for _ in 0..3 {
    let (w, h, rgba) = capture.capture().unwrap();
    assert_eq!((w, h), (3, 2));
    assert_eq!(rgba, [30, 20, 10, 255].repeat(6));
  }
  assert_eq!(script.released.load(Ordering::Relaxed), 2);
  assert_eq!(script.timeouts.lock().unwrap().len(), 4);
  assert!(!script.held.load(Ordering::Acquire));
  capture.inner.as_mut().unwrap().staging = None;
  assert_eq!(capture.capture().unwrap().2, [30, 20, 10, 255].repeat(6));
  assert!(capture.inner.as_ref().unwrap().staging.is_some());
  assert_eq!(script.released.load(Ordering::Relaxed), 3);
  assert!(
    script
      .timeouts
      .lock()
      .unwrap()
      .iter()
      .all(|&timeout| timeout <= 100)
  );
  capture.shutdown();
  assert!(capture.inner.is_none());
}

#[test]
fn native_failure_paths_release_successfully_acquired_frames_and_bound_retry_counts() {
  let (mut missing, script) = fixture(
    vec![Event::Frame {
      present: 1,
      metadata: 16,
      resource: false,
    }],
    [1, 2, 3, 255],
  );
  assert!(
    missing
      .capture()
      .unwrap_err()
      .to_string()
      .contains("resource is None")
  );
  assert_eq!(script.released.load(Ordering::Relaxed), 1);
  assert!(!script.held.load(Ordering::Acquire));
  let (mut black, script) = fixture(vec![frame(1)], [0, 0, 0, 255]);
  assert!(
    black
      .capture()
      .unwrap_err()
      .to_string()
      .contains("all-black")
  );
  assert_eq!(script.released.load(Ordering::Relaxed), 1);
  assert!(
    !black
      .inner
      .as_ref()
      .unwrap()
      .cache_valid
      .load(Ordering::Acquire)
  );
  let (mut pointer, script) = fixture(vec![frame(0), frame(1)], [1, 2, 3, 255]);
  assert_eq!(pointer.capture().unwrap().2, [3, 2, 1, 255].repeat(6));
  assert_eq!(script.released.load(Ordering::Relaxed), 2);
  for code in [
    dxgi_api::DXGI_ERROR_WAIT_TIMEOUT,
    dxgi_api::DXGI_ERROR_WAS_STILL_DRAWING,
    HRESULT(0x80004005u32 as i32),
  ] {
    let (mut capture, script) =
      fixture((0..6).map(|_| Event::Error(code)).collect(), [1, 2, 3, 255]);
    assert!(capture.capture().is_err());
    assert_eq!(
      script.timeouts.lock().unwrap().len(),
      if code == dxgi_api::DXGI_ERROR_WAS_STILL_DRAWING {
        6
      } else {
        1
      }
    );
    assert_eq!(script.released.load(Ordering::Relaxed), 0);
  }
}

#[test]
fn video_software_frames_skip_unchanged_updates_invalidate_cache_and_release_each_frame() {
  let (mut capture, script) = fixture(
    vec![
      Event::Frame {
        present: 1,
        metadata: 0,
        resource: true,
      },
      frame(2),
      Event::Error(dxgi_api::DXGI_ERROR_WAIT_TIMEOUT),
      Event::Error(dxgi_api::DXGI_ERROR_WAS_STILL_DRAWING),
      frame(3),
    ],
    [30, 40, 50, 255],
  );
  assert!(capture.capture_for_video().unwrap().is_none());
  let frame = capture.capture_for_video().unwrap().unwrap();
  assert_eq!(frame.2, [50, 40, 30, 255].repeat(6));
  assert!(
    !capture
      .inner
      .as_ref()
      .unwrap()
      .cache_valid
      .load(Ordering::Acquire)
  );
  assert!(capture.capture_for_video().unwrap().is_none());
  assert_eq!(capture.capture_for_video().unwrap().unwrap().2, frame.2);
  assert_eq!(script.released.load(Ordering::Relaxed), 3);
  assert!(!script.held.load(Ordering::Acquire));
}

#[test]
fn repeated_access_loss_reports_terminal_recovery_errors_without_opening_another_desktop_source() {
  for video in [false, true] {
    for retries in [3, 5] {
      let (mut capture, script) = fixture(
        vec![Event::Error(dxgi_api::DXGI_ERROR_ACCESS_LOST)],
        [1, 2, 3, 255],
      );
      DXGI_ACCESS_LOST_RETRY_COUNT.with(|counter| counter.set(retries));
      if video && retries == 3 {
        assert!(capture.capture_for_video().unwrap().is_none());
      } else {
        let error = if video {
          capture.capture_for_video().unwrap_err()
        } else {
          capture.capture().unwrap_err()
        };
        assert!(!error.to_string().is_empty());
      }
      assert_eq!(script.timeouts.lock().unwrap().len(), 1);
      assert_eq!(script.released.load(Ordering::Relaxed), 0);
      assert!(
        capture.inner.is_some(),
        "terminal error must not initialize a real capture pipeline"
      );
      DXGI_ACCESS_LOST_RETRY_COUNT.with(|counter| assert_eq!(counter.get(), 0));
      assert_eq!(
        (
          capture.target_x,
          capture.target_y,
          capture.target_width,
          capture.target_height
        ),
        (0, 0, 3, 2)
      );
    }
  }
}

#[test]
fn video_errors_preserve_missing_resource_failures_and_exhaust_gpu_busy_retries() {
  let (mut missing, script) = fixture(
    vec![Event::Frame {
      present: 1,
      metadata: 16,
      resource: false,
    }],
    [1, 2, 3, 255],
  );
  assert!(
    missing
      .capture_for_video()
      .unwrap_err()
      .to_string()
      .contains("resource is None")
  );
  assert_eq!(script.released.load(Ordering::Relaxed), 1);
  for code in [
    dxgi_api::DXGI_ERROR_WAS_STILL_DRAWING,
    HRESULT(0x80004005u32 as i32),
  ] {
    let (mut capture, script) =
      fixture((0..4).map(|_| Event::Error(code)).collect(), [1, 2, 3, 255]);
    assert!(capture.capture_for_video().is_err());
    assert_eq!(
      script.timeouts.lock().unwrap().len(),
      if code == dxgi_api::DXGI_ERROR_WAS_STILL_DRAWING {
        4
      } else {
        1
      }
    );
    assert_eq!(script.released.load(Ordering::Relaxed), 0);
  }
}

#[test]
fn failed_staging_map_releases_the_frame_and_a_fresh_staging_texture_can_recover() {
  for video in [false, true] {
    let (mut capture, script) = fixture(vec![frame(1), frame(2)], [7, 13, 21, 255]);
    let inner = capture.inner.as_mut().unwrap();
    let desc = D3D11_TEXTURE2D_DESC {
      Width: 3,
      Height: 2,
      MipLevels: 1,
      ArraySize: 1,
      Format: DXGI_FORMAT_B8G8R8A8_UNORM,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Usage: D3D11_USAGE_DEFAULT,
      ..Default::default()
    };
    // SAFETY: create a same-device GPU-only resource to exercise a rejected CPU map.
    unsafe {
      inner
        .device
        .CreateTexture2D(&desc, None, Some(&mut inner.staging))
        .unwrap();
    }
    let error = if video {
      capture.capture_for_video().unwrap_err()
    } else {
      capture.capture().unwrap_err()
    };
    assert!(format!("{error:#}").contains("map staging texture"));
    assert_eq!(script.released.load(Ordering::Relaxed), 1);
    assert!(!script.held.load(Ordering::Acquire));
    capture.inner.as_mut().unwrap().staging = None;
    let pixels = if video {
      capture.capture_for_video().unwrap().unwrap().2
    } else {
      capture.capture().unwrap().2
    };
    assert_eq!(pixels, [21, 13, 7, 255].repeat(6));
    assert_eq!(script.released.load(Ordering::Relaxed), 2);
  }
}
