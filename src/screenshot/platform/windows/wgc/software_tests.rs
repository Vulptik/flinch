//! Exercise the real fence, staging and map path using software textures only.
use super::*;

#[test]
fn pipeline_item_failure_preserves_cause_without_starting_capture() {
  let f = Fixture::new();
  let device5 = f.device.cast().unwrap();
  for (width, height) in [(0, 2), (3, 0), (-1, 2), (3, -1)] {
    assert!(
      create_staging_resources(&f.device, &device5, width, height)
        .err()
        .unwrap()
        .to_string()
        .contains("Invalid content size")
    );
  }
  assert!(
    create_staging_resources(&f.device, &device5, i32::MAX, 2)
      .err()
      .unwrap()
      .to_string()
      .contains("staging0")
  );
  let result = build_wgc_pipeline(f.device.clone(), f.context.cast().unwrap(), || {
    anyhow::bail!("scripted capture item permission denied")
  });
  assert!(
    format!("{:#}", result.err().unwrap()).contains("scripted capture item permission denied")
  );
}

#[test]
fn frame_arrival_drains_idle_events_skips_duplicate_timestamps_and_retries_acquisition_failures() {
  let f = Fixture::new();
  f.state.0.lock().unwrap().warmup = WARMUP_FRAMES;
  let drained = std::cell::Cell::new(false);
  process_frame_arrival(
    &f.state,
    || {
      drained.set(true);
      Ok(())
    },
    |_| panic!("idle frame needs no timestamp"),
    |_| panic!("idle frame needs no surface"),
    || panic!("idle frame needs no resources"),
  )
  .unwrap();
  assert!(drained.get());
  f.request(vec![0; 24]);
  f.state.0.lock().unwrap().last_timestamp = Some(17);
  process_frame_arrival(
    &f.state,
    || Ok(()),
    |_| Some(17),
    |_| panic!("duplicate timestamp must not read pixels"),
    || panic!("stale frame needs no resources"),
  )
  .unwrap();
  assert!(f.state.0.lock().unwrap().requested);
  process_frame_arrival(
    &f.state,
    || {
      Err::<(), _>(windows::core::Error::from_hresult(
        windows::Win32::Foundation::E_FAIL,
      ))
    },
    |_| panic!("failed acquisition has no timestamp"),
    |_| panic!("failed acquisition has no surface"),
    || panic!("failed acquisition needs no resources"),
  )
  .unwrap();
  assert!(f.state.0.lock().unwrap().requested);
  process_frame_arrival(
    &f.state,
    || Ok(()),
    |_| Some(18),
    |_| {
      Ok(IDirect3DSurface::from(Surface {
        texture: Some(f.texture.clone()),
      }))
    },
    || Some(f.resources()),
  )
  .unwrap();
  let (width, height, pixels) = f.result().unwrap();
  assert_eq!((width, height), (3, 2));
  assert_eq!(pixels, [61, 33, 17, 255].repeat(6));
  assert_eq!(f.state.0.lock().unwrap().last_timestamp, Some(18));
}
use windows::Foundation::{IClosable, IClosable_Impl};
use windows::Graphics::DirectX::Direct3D11::{
  Direct3DSurfaceDescription, IDirect3DSurface, IDirect3DSurface_Impl,
};
use windows_core::implement;

#[implement(IDirect3DSurface, IClosable)]
struct InaccessibleSurface;
impl IClosable_Impl for InaccessibleSurface_Impl {
  fn Close(&self) -> windows::core::Result<()> {
    Ok(())
  }
}
impl IDirect3DSurface_Impl for InaccessibleSurface_Impl {
  fn Description(&self) -> windows::core::Result<Direct3DSurfaceDescription> {
    Ok(Default::default())
  }
}
#[implement(IDirect3DSurface, IClosable, IDirect3DDxgiInterfaceAccess)]
struct Surface {
  texture: Option<ID3D11Texture2D>,
}
impl IClosable_Impl for Surface_Impl {
  fn Close(&self) -> windows::core::Result<()> {
    Ok(())
  }
}
impl IDirect3DSurface_Impl for Surface_Impl {
  fn Description(&self) -> windows::core::Result<Direct3DSurfaceDescription> {
    Ok(Default::default())
  }
}
impl IDirect3DDxgiInterfaceAccess_Impl for Surface_Impl {
  fn GetInterface(
    &self,
    iid: *const windows::core::GUID,
    out: *mut *mut std::ffi::c_void,
  ) -> windows::core::Result<()> {
    let texture = self
      .texture
      .as_ref()
      .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
    // SAFETY: COM provides the interface IID and writable output slot; transfer one owned reference.
    unsafe {
      assert_eq!(*iid, ID3D11Texture2D::IID);
      out.write(texture.clone().into_raw());
    }
    Ok(())
  }
}

#[test]
fn wgc_surface_failures_format_and_resolution_changes_complete_only_the_current_request() {
  for case in 0..6 {
    let f = Fixture::new();
    f.request(vec![0; 24]);
    let id = f.state.0.lock().unwrap().request_id;
    let surface = match case {
      0 => Err(windows::core::Error::from_hresult(
        windows::Win32::Foundation::E_FAIL,
      )),
      1 => Ok(IDirect3DSurface::from(InaccessibleSurface)),
      2 => Ok(IDirect3DSurface::from(Surface { texture: None })),
      _ => Ok(IDirect3DSurface::from(Surface {
        texture: Some(f.texture.clone()),
      })),
    };
    process_wgc_surface(surface, &f.state, WARMUP_FRAMES, id, Some(100), || {
      if case == 4 {
        return None;
      }
      let mut resources = f.resources();
      if case == 3 {
        resources.6 = 4;
      }
      Some(resources)
    })
    .unwrap();
    if case == 4 {
      let state = f.state.0.lock().unwrap();
      assert!(state.requested && state.result.is_none());
    } else if case == 5 {
      let (width, height, pixels) = f.result().unwrap();
      assert_eq!((width, height), (3, 2));
      assert_eq!(pixels, [61, 33, 17, 255].repeat(6));
    } else {
      let message = f.result().unwrap_err().to_string();
      assert!(message.contains(match case {
        0 => "Surface()",
        1 => "surface.cast",
        2 => "GetInterface",
        _ => "resolution changed",
      }));
    }
  }
  let f = Fixture::new();
  f.request(vec![0; 24]);
  let id = f.state.0.lock().unwrap().request_id;
  process_wgc_surface(
    Err(windows::core::Error::from_hresult(
      windows::Win32::Foundation::E_FAIL,
    )),
    &f.state,
    WARMUP_FRAMES,
    id - 1,
    None,
    || panic!("failed surface must not use resources"),
  )
  .unwrap();
  assert!(
    f.state.0.lock().unwrap().result.is_none(),
    "stale errors cannot finish a newer request"
  );
  assert!(f.state.0.lock().unwrap().requested);
}
use windows::Win32::{
  Foundation::HMODULE,
  Graphics::{
    Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0},
    Direct3D11::*,
  },
};

struct Fixture {
  device: ID3D11Device,
  context: ID3D11DeviceContext4,
  texture: ID3D11Texture2D,
  staging: (ID3D11Texture2D, ID3D11Texture2D),
  fence: ID3D11Fence,
  sync: Arc<Mutex<FenceState>>,
  state: (Mutex<FrameWaitState>, Condvar),
}
impl Fixture {
  fn new() -> Self {
    let (mut device, mut context) = (None, None);
    // SAFETY: the software device owns synthetic buffers and never opens any capture session.
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
    let context: ID3D11DeviceContext4 = context.unwrap().cast().unwrap();
    let device5: ID3D11Device5 = device.cast().unwrap();
    let resources = create_staging_resources(&device, &device5, 3, 2).unwrap();
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
    let pixels = [17u8, 33, 61, 255].repeat(6);
    let input = D3D11_SUBRESOURCE_DATA {
      pSysMem: pixels.as_ptr().cast(),
      SysMemPitch: 12,
      SysMemSlicePitch: 24,
    };
    let mut texture = None;
    // SAFETY: the complete pixel buffer remains valid until the synchronous copy returns.
    unsafe {
      device
        .CreateTexture2D(&desc, Some(&input), Some(&mut texture))
        .unwrap();
    }
    Self {
      device,
      context,
      texture: texture.unwrap(),
      staging: (resources.first, resources.second),
      fence: resources.fence,
      sync: Arc::new(Mutex::new(FenceState {
        last_value: 0,
        last_staging_index: 0,
      })),
      state: (
        Mutex::new(FrameWaitState {
          warmup: 0,
          requested: false,
          request_id: 0,
          result: None,
          buffer_to_use: None,
          last_timestamp: None,
        }),
        Condvar::new(),
      ),
    }
  }
  fn resources(&self) -> FrameResources {
    (
      self.device.clone(),
      self.context.clone(),
      self.staging.0.clone(),
      self.staging.1.clone(),
      self.fence.clone(),
      self.sync.clone(),
      3,
      2,
    )
  }
  fn process(&self, warmup: u8, timestamp: i64) {
    let id = self.state.0.lock().unwrap().request_id;
    process_wgc_frame(
      self.resources(),
      &self.state,
      &self.texture,
      warmup,
      id,
      Some(timestamp),
    )
    .unwrap();
  }
  fn request(&self, buffer: Vec<u8>) {
    self.state.0.lock().unwrap().begin_request(buffer).unwrap();
  }
  fn result(&self) -> Result<CapturedFrame> {
    self.state.0.lock().unwrap().result.take().unwrap()
  }
}

#[test]
fn software_wgc_warmup_and_double_staging_preserve_pixels_buffers_timestamps_and_fence_order() {
  let f = Fixture::new();
  for warmup in 0..WARMUP_FRAMES {
    f.process(warmup, i64::from(warmup));
    assert_eq!(f.state.0.lock().unwrap().warmup, warmup + 1);
    assert!(f.state.0.lock().unwrap().result.is_none());
  }
  for timestamp in [10, 11, 12] {
    let buffer = vec![0u8; 24];
    let allocation = buffer.as_ptr();
    f.request(buffer);
    f.process(WARMUP_FRAMES, timestamp);
    let (w, h, pixels) = f.result().unwrap();
    assert_eq!((w, h), (3, 2));
    assert_eq!(pixels, [61, 33, 17, 255].repeat(6));
    assert_eq!(
      pixels.as_ptr(),
      allocation,
      "delivery must reuse the caller-owned allocation"
    );
    assert_eq!(f.state.0.lock().unwrap().last_timestamp, Some(timestamp));
  }
  assert_eq!(f.sync.lock().unwrap().last_value, 6);
}

#[test]
fn first_software_frame_can_map_without_warmup_and_invalid_requests_do_not_publish_pixels() {
  let f = Fixture::new();
  f.request(vec![0; 24]);
  f.process(WARMUP_FRAMES, 1);
  assert_eq!(f.result().unwrap().2, [61, 33, 17, 255].repeat(6));
  assert_eq!(f.sync.lock().unwrap().last_value, 1);
  f.request(vec![0; 23]);
  f.process(WARMUP_FRAMES, 2);
  assert!(
    f.result()
      .unwrap_err()
      .to_string()
      .contains("len 23 < need 24")
  );
  assert_eq!(f.state.0.lock().unwrap().last_timestamp, Some(1));
  f.request(vec![0; 24]);
  f.state.0.lock().unwrap().buffer_to_use = None;
  f.process(WARMUP_FRAMES, 3);
  assert!(f.result().unwrap_err().to_string().contains("not provided"));
  f.request(Vec::new());
  let mut resources = f.resources();
  resources.6 = u32::MAX;
  resources.7 = u32::MAX;
  let id = f.state.0.lock().unwrap().request_id;
  process_wgc_frame(resources, &f.state, &f.texture, WARMUP_FRAMES, id, Some(4)).unwrap();
  assert!(f.result().unwrap_err().to_string().contains("overflow"));
}

#[test]
fn unsignalled_fence_fails_with_a_bounded_error_and_does_not_publish_or_reuse_partial_pixels() {
  let f = Fixture::new();
  f.sync.lock().unwrap().last_value = 1;
  f.request(vec![99; 24]);
  let started = Instant::now();
  f.process(WARMUP_FRAMES, 77);
  let error = f.result().unwrap_err().to_string();
  assert!(error.contains("fence GetCompletedValue timeout"), "{error}");
  assert!(started.elapsed() < Duration::from_secs(2));
  assert!(!f.state.0.lock().unwrap().requested);
  assert!(f.state.0.lock().unwrap().last_timestamp.is_none());
  assert!(f.state.0.lock().unwrap().buffer_to_use.is_none());
  assert_eq!(f.sync.lock().unwrap().last_value, 1);
}
