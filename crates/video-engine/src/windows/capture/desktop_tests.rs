//! Explicit desktop API diagnostics; never selected by the quiet runner.
#![allow(clippy::expect_used)]
use super::*;
use windows::Win32::{Foundation::HMODULE, Graphics::Direct3D::*};

#[test]
#[ignore = "Explicit desktop duplication initialization; acquires and releases at most one frame per device, never copies it"]
fn desktop_duplication_initialization_diagnostic() {
  let _com = super::super::Apartment::new().expect("COM");
  // As in the worker: DuplicateOutput1 is refused to processes without per-monitor DPI awareness.
  super::super::dpi::ensure_per_monitor_v2().expect("per-monitor DPI awareness");
  let output = super::super::gpu::outputs()
    .expect("display inventory")
    .into_iter()
    .find(|o| o.info.x == 0 && o.info.y == 0)
    .expect("primary attached display");
  let mut available = false;
  for (name, flags, levels) in [
    (
      "video-11.1",
      D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
      vec![D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0],
    ),
    (
      "video-11.0",
      D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
      vec![D3D_FEATURE_LEVEL_11_0],
    ),
    (
      "bgra-11.0",
      D3D11_CREATE_DEVICE_BGRA_SUPPORT,
      vec![D3D_FEATURE_LEVEL_11_0],
    ),
  ] {
    let mut device = None;
    // SAFETY: diagnostic creates a same-adapter device; it never copies a desktop frame.
    let created = unsafe {
      D3D11CreateDevice(
        &output.adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        flags,
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
      )
    };
    // A basic display adapter (no GPU, VMs) refuses video support: report it and go on.
    let Some(device) = created.as_ref().ok().and(device) else {
      eprintln!(
        "D3D11CreateDevice device={name}, HRESULT={:?}",
        created.err().map(|e| e.code().0)
      );
      continue;
    };
    for formats in [
      vec![DXGI_FORMAT_B8G8R8A8_UNORM],
      vec![DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM],
    ] {
      // SAFETY: same-adapter initialization only, released before the next API comparison.
      let result = unsafe { output.output.DuplicateOutput1(&device, 0, &formats) };
      let status = result.as_ref().err().map(|e| e.code().0);
      eprintln!("DuplicateOutput1 device={name}, formats={formats:?}, HRESULT={status:?}");
      if let Ok(duplication) = &result {
        // One acquire tells whether this session's desktop is capturable (a non-interactive
        // session initializes but never delivers); the frame is released uncopied.
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: bounded acquire on this diagnostic's own duplication; released below on success.
        let acquired = unsafe { duplication.AcquireNextFrame(500, &mut info, &mut resource) };
        eprintln!(
          "AcquireNextFrame device={name}, HRESULT={:?}, accumulated={}",
          acquired.as_ref().err().map(|e| e.code().0),
          info.AccumulatedFrames
        );
        if acquired.is_ok() {
          drop(resource);
          // SAFETY: exactly one release for the successful acquire above.
          if let Err(error) = unsafe { duplication.ReleaseFrame() } {
            eprintln!("ReleaseFrame device={name}: {error}");
          }
        }
      }
      available |= result.is_ok();
      drop(result);
    }
    // SAFETY: legacy API is diagnostic only and is never an implicit recording fallback.
    let result = unsafe { output.output.DuplicateOutput(&device) };
    eprintln!(
      "DuplicateOutput device={name}, HRESULT={:?}",
      result.as_ref().err().map(|e| e.code().0)
    );
    drop(result);
  }
  assert!(
    available,
    "No tested DuplicateOutput1 initialization succeeded; retained diagnostics distinguish device flags and API availability"
  );
}
