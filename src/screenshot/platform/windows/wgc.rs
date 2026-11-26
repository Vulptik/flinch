// src/screenshot/platform/windows/wgc.rs
// Windows Graphics Capture API — zero-latency implementation.
//
// - Asynchronous sync: ID3D11Fence + context.Signal after CopyResource; GetCompletedValue
//   before Map. No Flush (latency killer).
// - Double staging: two ID3D11Texture2D staging buffers so GPU and CPU work in parallel;
//   we Map the previous frame's buffer while the current CopyResource is in flight.
// - Persistent pipeline: device, WinRT device, frame pool, session, fence, stagings reused.
// - Event-driven: FrameArrived + Condvar; ID3D11DeviceContext only on DispatcherQueue (MTA-safe).
// - Zero-alloc: re-use buffer_to_use from caller; BGRA→RGBA via Rayon after Unmap.
// - Warm on boot: pipeline primed with CopyResource+Signal during warmup; textures allocated at init.
//
// Thread safety: ID3D11DeviceContext/fence/stagings accessed only from DispatcherQueue.
// capture() never touches D3D11; it sets requested and buffer_to_use.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::IInspectable;
use windows::{
  Foundation::TypedEventHandler,
  Graphics::{
    Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem},
    DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
  },
  System::DispatcherQueueHandler,
  Win32::{
    Foundation::HWND,
    Graphics::{
      Direct3D11::{
        D3D11_BIND_FLAG, D3D11_CPU_ACCESS_READ, D3D11_FENCE_FLAG_NONE, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_FLAG, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, ID3D11Device, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence,
        ID3D11Texture2D,
      },
      Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
      Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromWindow},
    },
    System::WinRT::{
      CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_DEDICATED, Direct3D11::*,
      DispatcherQueueOptions, Graphics::Capture::IGraphicsCaptureItemInterop,
    },
    UI::WindowsAndMessaging::GetDesktopWindow,
  },
  core::Interface,
};

use super::conversion::convert_bgra_to_rgba_in_place_rayon;
use super::utils::{create_d3d11_device, format_hresult};
use super::{CaptureBackendKind, get_active_recording_backend, set_last_capture_method};

const DXGI_ERROR_DEVICE_REMOVED: u32 = 0x887A0005;
const DXGI_ERROR_DEVICE_HUNG: u32 = 0x887A0006;
const DXGI_ERROR_DEVICE_RESET: u32 = 0x887A0007;
const WARMUP_FRAMES: u8 = 3;
const WGC_FRAME_WAIT_TIMEOUT_SECS: u64 = 10;
/// Short timeout for video frame capture: 50 ms. Static screens return Ok(None);
/// caller repeats last frame. Must not block a 30 fps capture loop.
#[cfg(test)]
const WGC_VIDEO_FRAME_TIMEOUT_MS: u64 = 50;
const WGC_WARMUP_TIMEOUT_SECS: u64 = 15;
const FENCE_WAIT_TIMEOUT_MS: u64 = 50;

fn is_recreate_needed(e: &anyhow::Error) -> bool {
  let s = e.to_string();
  s.contains("resolution changed") || s.contains("device removed")
}

fn is_device_lost_code(code: u32) -> bool {
  code == DXGI_ERROR_DEVICE_REMOVED
    || code == DXGI_ERROR_DEVICE_HUNG
    || code == DXGI_ERROR_DEVICE_RESET
}

/// Shared state between the FrameArrived handler and capture().
struct FrameWaitState {
  warmup: u8,
  requested: bool,
  request_id: u64,
  result: Option<Result<(u32, u32, Vec<u8>)>>,
  /// Provided by capture(); FrameArrived uses it for copy+convert. Zero-alloc: no fallback.
  buffer_to_use: Option<Vec<u8>>,
  last_timestamp: Option<i64>,
}

type CapturedFrame = (u32, u32, Vec<u8>);

impl FrameWaitState {
  fn begin_request(&mut self, buffer: Vec<u8>) -> Result<()> {
    anyhow::ensure!(!self.requested, "WGC frame request already in progress");
    self.request_id = self
      .request_id
      .checked_add(1)
      .ok_or_else(|| anyhow::anyhow!("WGC frame request identifiers exhausted"))?;
    self.requested = true;
    self.result = None;
    self.buffer_to_use = Some(buffer);
    Ok(())
  }

  fn take_request_buffer(&mut self, request_id: u64) -> Option<Vec<u8>> {
    if !self.requested || self.request_id != request_id {
      return None;
    }
    self.buffer_to_use.take()
  }

  fn finish(&mut self, request_id: u64, result: Result<CapturedFrame>) -> bool {
    if self.request_id != request_id
      || self.result.is_some()
      || (!self.requested && self.warmup >= WARMUP_FRAMES)
    {
      return false;
    }
    self.result = Some(result);
    self.requested = false;
    true
  }
}

fn request_frame(
  state: &(Mutex<FrameWaitState>, Condvar),
  reusable: &mut Option<Vec<u8>>,
  width: u32,
  height: u32,
  timeout: Duration,
) -> Result<CapturedFrame> {
  let need = (width as usize)
    .checked_mul(height as usize)
    .and_then(|n| n.checked_mul(4))
    .filter(|n| *n <= isize::MAX as usize)
    .ok_or_else(|| anyhow::anyhow!("WGC buffer size overflow"))?;
  anyhow::ensure!(need > 0, "WGC frame dimensions are empty");
  let mut guard = state
    .0
    .lock()
    .map_err(|_| anyhow::anyhow!("WGC frame state mutex poisoned"))?;
  anyhow::ensure!(!guard.requested, "WGC frame request already in progress");
  let buffer = match reusable.take() {
    Some(mut b) if b.len() >= need => {
      b.truncate(need);
      b
    },
    _ => vec![0; need],
  };
  guard.begin_request(buffer)?;
  state.1.notify_all();
  let (mut guard, wait) = state
    .1
    .wait_timeout_while(guard, timeout, |s| s.result.is_none())
    .map_err(|_| anyhow::anyhow!("WGC frame state wait poisoned"))?;
  if wait.timed_out() && guard.result.is_none() {
    guard.requested = false;
    *reusable = guard.buffer_to_use.take();
    anyhow::bail!("WGC frame wait timeout");
  }
  guard.requested = false;
  let frame = guard
    .result
    .take()
    .ok_or_else(|| anyhow::anyhow!("WGC frame result missing after wait"))??;
  anyhow::ensure!(
    frame.0 == width && frame.1 == height && frame.2.len() == need,
    "WGC returned inconsistent frame dimensions or pixel data"
  );
  Ok(frame)
}

fn wait_for_warmup(state: &(Mutex<FrameWaitState>, Condvar), timeout: Duration) -> Result<()> {
  let guard = state
    .0
    .lock()
    .map_err(|_| anyhow::anyhow!("WGC frame state mutex poisoned"))?;
  let (mut guard, _) = state
    .1
    .wait_timeout_while(guard, timeout, |s| {
      s.warmup < WARMUP_FRAMES && s.result.is_none()
    })
    .map_err(|_| anyhow::anyhow!("WGC warmup wait poisoned"))?;
  if let Some(result) = guard.result.take() {
    return match result {
      Err(error) => Err(error.context("WGC warmup failed")),
      Ok(_) => Err(anyhow::anyhow!(
        "WGC returned frame data before warmup completed"
      )),
    };
  }
  anyhow::ensure!(guard.warmup >= WARMUP_FRAMES, "WGC warmup timeout");
  Ok(())
}

/// Fence state for double-buffering: last Signalled value and which staging holds that frame.
struct FenceState {
  last_value: u64,
  last_staging_index: u8, // 0 or 1
}

type FrameResources = (
  ID3D11Device,
  ID3D11DeviceContext4,
  ID3D11Texture2D,
  ID3D11Texture2D,
  ID3D11Fence,
  Arc<Mutex<FenceState>>,
  u32,
  u32,
);

// Called only on the capture owner thread; GPU mapping never holds the frame-state mutex.
fn process_wgc_frame(
  resources: FrameResources,
  frame_state: &(Mutex<FrameWaitState>, Condvar),
  texture: &ID3D11Texture2D,
  warmup: u8,
  request_id: u64,
  ts: Option<i64>,
) -> windows::core::Result<()> {
  let (device, context4, s0, s1, fence, fence_state, width, height) = resources;
  let need = (width as usize)
    .checked_mul(height as usize)
    .and_then(|p| p.checked_mul(4));

  // Warmup: CopyResource+Signal to prime the pipeline. No Map/deliver.
  if warmup < WARMUP_FRAMES {
    let (v, idx) = {
      let Ok(fs) = fence_state.lock() else {
        return Ok(());
      };
      (fs.last_value, fs.last_staging_index)
    };
    let next = if v == 0 { 0 } else { 1 - idx };
    let st = if next == 0 { &s0 } else { &s1 };
    // SAFETY: Both textures belong to this owner-thread device and have matching capture dimensions.
    unsafe { context4.CopyResource(st, texture) };
    // SAFETY: The context and owned fence belong to the same device and remain alive through submission.
    if let Err(e) = unsafe { context4.Signal(&fence, v + 1) } {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(request_id, Err(anyhow::anyhow!("Signal failed: {}", e)));
        frame_state.1.notify_one();
      }
      return Ok(());
    }
    // SAFETY: Read device status through a live owned COM interface on the capture thread.
    if unsafe { device.GetDeviceRemovedReason() }
      .err()
      .map(|e| is_device_lost_code(e.code().0 as u32))
      .unwrap_or(false)
    {
      spawn_recreate_if_needed();
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(
          request_id,
          Err(anyhow::anyhow!("device removed").context("Signal or earlier")),
        );
        frame_state.1.notify_one();
      }
      return Ok(());
    }
    if let Ok(mut fs) = fence_state.lock() {
      fs.last_value = v + 1;
      fs.last_staging_index = next;
    }
    if let Ok(mut g) = frame_state.0.lock() {
      g.warmup = g.warmup.saturating_add(1);
      frame_state.1.notify_one();
    }
    return Ok(());
  }

  // Requested: require buffer_to_use from caller (zero-alloc).
  let need = match need {
    Some(n) => n,
    None => {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(request_id, Err(anyhow::anyhow!("width*height*4 overflow")));
        frame_state.1.notify_one();
      }
      return Ok(());
    },
  };
  let buf = {
    let Ok(mut g) = frame_state.0.lock() else {
      return Ok(());
    };
    if !g.requested || g.request_id != request_id {
      return Ok(());
    }
    g.take_request_buffer(request_id)
  };
  let mut buf = match buf {
    Some(b) if b.len() >= need => b,
    Some(b) => {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(
          request_id,
          Err(anyhow::anyhow!(
            "buffer_to_use len {} < need {}",
            b.len(),
            need
          )),
        );
        frame_state.1.notify_one();
      }
      return Ok(());
    },
    None => {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(
          request_id,
          Err(anyhow::anyhow!("buffer_to_use not provided by caller")),
        );
        frame_state.1.notify_one();
      }
      return Ok(());
    },
  };

  let (v, idx) = {
    let Ok(fs) = fence_state.lock() else {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(
          request_id,
          Err(anyhow::anyhow!("fence_state lock poisoned")),
        );
        frame_state.1.notify_one();
      }
      return Ok(());
    };
    (fs.last_value, fs.last_staging_index)
  };

  let (to_map, to_fill) = if v > 0 {
    // Previous frame ready: Map that staging, then CopyResource current to the other.
    (idx, 1 - idx)
  } else {
    // First frame: CopyResource current to s0, wait, Map s0.
    (0u8, 0u8)
  };

  let st_map = if to_map == 0 { &s0 } else { &s1 };
  let st_fill = if to_fill == 0 { &s0 } else { &s1 };

  if v == 0 {
    // SAFETY: Both textures belong to this owner-thread device and have matching capture dimensions.
    unsafe { context4.CopyResource(st_fill, texture) };
    // SAFETY: The context and owned fence belong to the same device and remain alive through submission.
    if let Err(e) = unsafe { context4.Signal(&fence, 1) } {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(request_id, Err(anyhow::anyhow!("Signal failed: {}", e)));
        frame_state.1.notify_one();
      }
      return Ok(());
    }
  }

  let expected = if v > 0 { v } else { 1 };
  let deadline = Instant::now() + Duration::from_millis(FENCE_WAIT_TIMEOUT_MS);
  // SAFETY: Poll the owned fence without retaining a frame-state lock; the loop has a fixed deadline.
  while unsafe { fence.GetCompletedValue() } < expected {
    if Instant::now() >= deadline {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(
          request_id,
          Err(anyhow::anyhow!(
            "fence GetCompletedValue timeout ({}/{}ms)",
            FENCE_WAIT_TIMEOUT_MS,
            expected
          )),
        );
        frame_state.1.notify_one();
      }
      return Ok(());
    }
    std::thread::yield_now();
  }

  // SAFETY: Read device status through a live owned COM interface on the capture thread.
  if unsafe { device.GetDeviceRemovedReason() }
    .err()
    .map(|e| is_device_lost_code(e.code().0 as u32))
    .unwrap_or(false)
  {
    spawn_recreate_if_needed();
    if let Ok(mut g) = frame_state.0.lock() {
      g.finish(
        request_id,
        Err(anyhow::anyhow!("device removed").context("fence wait")),
      );
      frame_state.1.notify_one();
    }
    return Ok(());
  }

  let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
  // SAFETY: The live owner-thread staging texture is CPU-readable and the mapped output is writable.
  if let Err(e) = unsafe { context4.Map(st_map, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) } {
    let code = e.code().0 as u32;
    let err = if is_device_lost_code(code) {
      anyhow::anyhow!("device removed").context("Map failed")
    } else {
      anyhow::anyhow!("Map failed: {}", e)
    };
    if let Ok(mut g) = frame_state.0.lock() {
      g.finish(request_id, Err(err));
      frame_state.1.notify_one();
    }
    if is_device_lost_code(code) {
      spawn_recreate_if_needed();
    }
    return Ok(());
  }

  let row_pitch_usize = mapped.RowPitch as usize;
  let w = width as usize;
  let h = height as usize;
  let row_width = match w.checked_mul(4) {
    Some(rw) => rw,
    None => {
      // SAFETY: Balances the successful map of this live owner-thread staging texture.
      unsafe { context4.Unmap(st_map, 0) };
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(request_id, Err(anyhow::anyhow!("width*4 overflow")));
        frame_state.1.notify_one();
      }
      return Ok(());
    },
  };
  if row_pitch_usize < row_width {
    // SAFETY: Balances the successful map of this live owner-thread staging texture.
    unsafe { context4.Unmap(st_map, 0) };
    if let Ok(mut g) = frame_state.0.lock() {
      g.finish(
        request_id,
        Err(anyhow::anyhow!(
          "RowPitch {} < width*4 {}",
          row_pitch_usize,
          row_width
        )),
      );
      frame_state.1.notify_one();
    }
    return Ok(());
  }
  let ptr = mapped.pData;
  if ptr.is_null() {
    // SAFETY: Balances the successful map of this live owner-thread staging texture.
    unsafe { context4.Unmap(st_map, 0) };
    if let Ok(mut g) = frame_state.0.lock() {
      g.finish(request_id, Err(anyhow::anyhow!("Map returned null pData")));
      frame_state.1.notify_one();
    }
    return Ok(());
  }
  // SAFETY: Map succeeded and returned a non-null pointer with sufficient row pitch; borrow ends before Unmap.
  let bgra_slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, row_pitch_usize * h) };
  for y in 0..h {
    let src_start = y * row_pitch_usize;
    let dst_start = y * row_width;
    buf[dst_start..dst_start + row_width]
      .copy_from_slice(&bgra_slice[src_start..src_start + row_width]);
  }
  // SAFETY: Balances the successful map of this live owner-thread staging texture.
  unsafe { context4.Unmap(st_map, 0) };

  // BGRA→RGBA via Rayon after Unmap to minimize GPU hold time.
  convert_bgra_to_rgba_in_place_rayon(&mut buf[..need]);

  if v > 0 {
    // SAFETY: Both textures belong to this owner-thread device and have matching capture dimensions.
    unsafe { context4.CopyResource(st_fill, texture) };
    let new_v = v + 1;
    // SAFETY: The context and owned fence belong to the same device and remain alive through submission.
    if let Err(e) = unsafe { context4.Signal(&fence, new_v) } {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(request_id, Err(anyhow::anyhow!("Signal failed: {}", e)));
        frame_state.1.notify_one();
      }
      return Ok(());
    }
    if let Ok(mut fs) = fence_state.lock() {
      fs.last_value = new_v;
      fs.last_staging_index = to_fill;
    }
  } else if let Ok(mut fs) = fence_state.lock() {
    fs.last_value = 1;
    fs.last_staging_index = 0;
  }

  if let Ok(mut g) = frame_state.0.lock() {
    if g.finish(request_id, Ok((width, height, buf))) {
      g.last_timestamp = ts;
    }
    frame_state.1.notify_one();
  }
  Ok(())
}

fn process_wgc_surface(
  surface: windows::core::Result<windows::Graphics::DirectX::Direct3D11::IDirect3DSurface>,
  frame_state: &(Mutex<FrameWaitState>, Condvar),
  warmup: u8,
  request_id: u64,
  ts: Option<i64>,
  resources: impl FnOnce() -> Option<FrameResources>,
) -> windows::core::Result<()> {
  let surface: windows::Graphics::DirectX::Direct3D11::IDirect3DSurface = match surface {
    Ok(s) => s,
    Err(e) => {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(
          request_id,
          Err(anyhow::anyhow!("frame.Surface() failed: {}", e)),
        );
        frame_state.1.notify_one();
      }
      return Ok(());
    },
  };

  let dxgi_access: IDirect3DDxgiInterfaceAccess = match surface.cast() {
    Ok(a) => a,
    Err(e) => {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(
          request_id,
          Err(anyhow::anyhow!(
            "surface.cast to IDirect3DDxgiInterfaceAccess failed: {}",
            e
          )),
        );
        frame_state.1.notify_one();
      }
      return Ok(());
    },
  };

  let texture: ID3D11Texture2D = match unsafe { dxgi_access.GetInterface::<ID3D11Texture2D>() } {
    Ok(t) => t,
    Err(e) => {
      if let Ok(mut g) = frame_state.0.lock() {
        g.finish(
          request_id,
          Err(anyhow::anyhow!(
            "GetInterface(ID3D11Texture2D) failed: {}",
            e
          )),
        );
        frame_state.1.notify_one();
      }
      return Ok(());
    },
  };

  let mut desc = D3D11_TEXTURE2D_DESC::default();
  unsafe { texture.GetDesc(&mut desc) };

  if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
    if let Ok(mut g) = frame_state.0.lock() {
      g.finish(
        request_id,
        Err(anyhow::anyhow!(
          "WGC texture format is not B8G8R8A8_UNORM: {:?}",
          desc.Format
        )),
      );
      frame_state.1.notify_one();
    }
    return Ok(());
  }

  let Some(resources) = resources() else {
    return Ok(());
  };
  if desc.Width != resources.6 || desc.Height != resources.7 {
    if let Ok(mut state) = frame_state.0.lock() {
      state.finish(request_id, Err(anyhow::anyhow!("resolution changed")));
      frame_state.1.notify_one();
    }
    return Ok(());
  }
  process_wgc_frame(resources, frame_state, &texture, warmup, request_id, ts)
}
#[cfg(test)]
mod software_tests;

/// Inner pipeline: D3D11, WinRT device, frame pool, session, double staging, fence. Recreated on
/// resolution change, device lost, or explicit failure. Context/fence/stagings only on DispatcherQueue.
struct WgcCaptureInner {
  device: ID3D11Device,
  context: ID3D11DeviceContext4,
  #[allow(dead_code)] // kept alive for frame_pool
  direct3d_device: IDirect3DDevice,
  frame_pool: Direct3D11CaptureFramePool,
  session: windows::Graphics::Capture::GraphicsCaptureSession,
  staging: (ID3D11Texture2D, ID3D11Texture2D),
  fence: ID3D11Fence,
  fence_state: Arc<Mutex<FenceState>>,
  width: u32,
  height: u32,
  frame_state: Arc<(Mutex<FrameWaitState>, Condvar)>,
  frame_arrived_token: i64,
  closed: AtomicBool,
}
unsafe impl Send for WgcCaptureInner {}

impl Drop for WgcCaptureInner {
  fn drop(&mut self) {
    self.closed.store(true, Ordering::Release);
    let _ = self.frame_pool.RemoveFrameArrived(self.frame_arrived_token);
    std::thread::sleep(Duration::from_millis(100));
    let _ = self.session.Close();
    let _ = self.frame_pool.Close();
    tracing::debug!(operation = "wgc_inner_drop", "WGC pipeline closed");
  }
}

/// Long-lived WGC capture. Create once, reuse; recreate only on resolution change,
/// device lost, or explicit failure.
struct WgcCapture {
  inner: Option<Arc<Mutex<WgcCaptureInner>>>,
  /// Reusable buffer recycled between captures to avoid per-frame allocation (~8MB at 1080p).
  reusable_buffer: Option<Vec<u8>>,
}

static WGC_CAPTURE: OnceLock<Mutex<WgcCapture>> = OnceLock::new();
/// DispatcherQueueController is never dropped or replaced; it stays active for the app
/// lifetime so the capture stack remains primed for the next session.
static WGC_DISPATCHER: OnceLock<windows::System::DispatcherQueueController> = OnceLock::new();
/// Set when a background thread is spawned to tear down on device-removed; avoids duplicate spawns.
static DEVICE_LOST_SPAWNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct WgcMemoryDiagnostics {
  pub initialized: bool,
  pub pipeline_active: bool,
  pub reusable_buffer_bytes: usize,
}

/// Spawns a thread to call recreate_if_needed on WGC_CAPTURE so the pipeline is torn down
/// promptly on device-removed/hung/reset. The callback only sets the error and spawns; the
/// actual teardown runs on a separate thread.
fn spawn_recreate_if_needed() {
  if DEVICE_LOST_SPAWNED
    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
    .is_ok()
  {
    std::thread::spawn(|| {
      if let Some(cap) = WGC_CAPTURE.get() {
        let _ = cap.lock().map(|mut g| g.recreate_if_needed());
      }
      DEVICE_LOST_SPAWNED.store(false, Ordering::Release);
    });
  }
}

struct StagingResources {
  width: u32,
  height: u32,
  first: ID3D11Texture2D,
  second: ID3D11Texture2D,
  fence: ID3D11Fence,
}

fn create_staging_resources(
  device: &ID3D11Device,
  device5: &ID3D11Device5,
  staging_width: i32,
  staging_height: i32,
) -> Result<StagingResources> {
  if staging_width <= 0 || staging_height <= 0 {
    return Err(anyhow::anyhow!(
      "Invalid content size: {}x{}",
      staging_width,
      staging_height
    ));
  }
  let staging_width = staging_width as u32;
  let staging_height = staging_height as u32;
  let staging_desc = D3D11_TEXTURE2D_DESC {
    Width: staging_width,
    Height: staging_height,
    MipLevels: 1,
    ArraySize: 1,
    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
    SampleDesc: DXGI_SAMPLE_DESC {
      Count: 1,
      Quality: 0,
    },
    Usage: D3D11_USAGE_STAGING,
    BindFlags: D3D11_BIND_FLAG(0).0 as u32,
    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
    MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
  };
  let mut s0 = None::<ID3D11Texture2D>;
  unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut s0)) }
    .context("CreateTexture2D staging0 failed")?;
  let staging0 = s0.ok_or_else(|| anyhow::anyhow!("CreateTexture2D staging0 returned None"))?;
  let mut s1 = None::<ID3D11Texture2D>;
  unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut s1)) }
    .context("CreateTexture2D staging1 failed")?;
  let staging1 = s1.ok_or_else(|| anyhow::anyhow!("CreateTexture2D staging1 returned None"))?;

  let mut fence_out = None::<ID3D11Fence>;
  unsafe { device5.CreateFence::<ID3D11Fence>(0u64, D3D11_FENCE_FLAG_NONE, &mut fence_out) }
    .context("CreateFence failed")?;
  let fence = fence_out.ok_or_else(|| anyhow::anyhow!("CreateFence returned None"))?;

  Ok(StagingResources {
    width: staging_width,
    height: staging_height,
    first: staging0,
    second: staging1,
    fence,
  })
}

fn process_frame_arrival<F>(
  frame_state: &(Mutex<FrameWaitState>, Condvar),
  next: impl FnOnce() -> windows::core::Result<F>,
  timestamp: impl FnOnce(&F) -> Option<i64>,
  surface: impl FnOnce(
    &F,
  ) -> windows::core::Result<
    windows::Graphics::DirectX::Direct3D11::IDirect3DSurface,
  >,
  resources: impl FnOnce() -> Option<FrameResources>,
) -> windows::core::Result<()> {
  let (warmup, requested, request_id) = {
    let Ok(g) = frame_state.0.lock() else {
      return Ok(());
    };
    (g.warmup, g.requested, g.request_id)
  };
  if !(warmup < WARMUP_FRAMES || requested) {
    let _ = next();
    return Ok(());
  }

  let frame = match next() {
    Ok(f) => f,
    Err(_) => return Ok(()),
  };

  // Stale-frame check: skip if SystemRelativeTime equals the last processed frame.
  let ts: Option<i64> = timestamp(&frame);
  if let Some(ts_val) = ts {
    let last = {
      let Ok(g) = frame_state.0.lock() else {
        return Ok(());
      };
      g.last_timestamp
    };
    if last == Some(ts_val) {
      return Ok(());
    }
  }

  process_wgc_surface(
    surface(&frame),
    frame_state,
    warmup,
    request_id,
    ts,
    resources,
  )
}

fn build_wgc_pipeline(
  device: ID3D11Device,
  context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
  item: impl FnOnce() -> Result<GraphicsCaptureItem>,
) -> Result<Arc<Mutex<WgcCaptureInner>>> {
  let device5: ID3D11Device5 = device
    .cast()
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "cast_device5",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "ID3D11Device5 not supported (Windows 10 1703+ required)"
      );
    })
    .context("ID3D11Device5 not supported (Windows 10 1703+ required)")?;
  let context4: ID3D11DeviceContext4 = context
    .cast()
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "cast_context4",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "ID3D11DeviceContext4 not supported"
      );
    })
    .context("ID3D11DeviceContext4 not supported")?;

  let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice = device
    .cast()
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "cast_dxgi_device",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "ID3D11Device cast to IDXGIDevice failed"
      );
    })
    .context("Failed to get IDXGIDevice from ID3D11Device")?;

  let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "create_direct3d_device",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "CreateDirect3D11DeviceFromDXGIDevice failed"
      );
    })
    .context("Failed to create IDirect3DDevice from IDXGIDevice")?;

  let direct3d_device: IDirect3DDevice = inspectable
    .cast()
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "cast_direct3d_device",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "IInspectable cast to IDirect3DDevice failed"
      );
    })
    .context("Failed to get IDirect3DDevice from IInspectable")?;

  let capture_item = item()?;
  let content_size = capture_item
    .Size()
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "get_content_size",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "Size() failed"
      );
    })
    .context("Failed to get content size")?;

  // Pre-allocate staging texture from content_size so FrameArrived never allocates.
  // Staging is created on the DispatcherQueue during init; lifetime tied to WgcCaptureInner.
  let StagingResources {
    width: staging_width,
    height: staging_height,
    first: staging0,
    second: staging1,
    fence,
  } = create_staging_resources(&device, &device5, content_size.Width, content_size.Height)?;
  let fence_state = Arc::new(Mutex::new(FenceState {
    last_value: 0,
    last_staging_index: 0,
  }));

  let frame_pool = Direct3D11CaptureFramePool::Create(
    &direct3d_device,
    DirectXPixelFormat::B8G8R8A8UIntNormalized,
    2,
    content_size,
  )
  .inspect_err(|e| {
    tracing::error!(
        operation = "wgc_ensure",
        step = "create_frame_pool",
        hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
        "Direct3D11CaptureFramePool::Create failed"
    );
  })
  .context("Failed to create Direct3D11CaptureFramePool")?;

  let session = frame_pool
    .CreateCaptureSession(&capture_item)
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "create_session",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "CreateCaptureSession failed"
      );
    })
    .context("Failed to create GraphicsCaptureSession")?;

  let _ = session.SetIsBorderRequired(false);

  let frame_state = Arc::new((
    Mutex::new(FrameWaitState {
      warmup: 0,
      requested: false,
      request_id: 0,
      result: None,
      buffer_to_use: None,
      last_timestamp: None,
    }),
    Condvar::new(),
  ));

  let inner_arc = Arc::new(Mutex::new(WgcCaptureInner {
    device: device.clone(),
    context: context4.clone(),
    direct3d_device: direct3d_device.clone(),
    frame_pool: frame_pool.clone(),
    session: session.clone(),
    staging: (staging0, staging1),
    fence: fence.clone(),
    fence_state: fence_state.clone(),
    width: staging_width,
    height: staging_height,
    frame_state: frame_state.clone(),
    frame_arrived_token: 0i64,
    closed: AtomicBool::new(false),
  }));

  let inner_weak = Arc::downgrade(&inner_arc);
  let frame_state_weak = Arc::downgrade(&frame_state);

  let handler =
    TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |sender, _args| {
      let (inner_arc, frame_state) = match (inner_weak.upgrade(), frame_state_weak.upgrade()) {
        (Some(inner), Some(fs)) => (inner, fs),
        _ => return Ok(()),
      };

      let pool = match sender.as_ref() {
        Some(p) => p,
        None => return Ok(()),
      };
      {
        let Ok(g) = inner_arc.lock() else {
          return Ok(());
        };
        if g.closed.load(Ordering::Acquire) {
          return Ok(());
        }
      }
      process_frame_arrival(
        &frame_state,
        || pool.TryGetNextFrame(),
        |frame| frame.SystemRelativeTime().ok().map(|t| t.Duration),
        Direct3D11CaptureFrame::Surface,
        || {
          let g = inner_arc.lock().ok()?;
          if g.closed.load(Ordering::Acquire) {
            return None;
          }
          Some((
            g.device.clone(),
            g.context.clone(),
            g.staging.0.clone(),
            g.staging.1.clone(),
            g.fence.clone(),
            g.fence_state.clone(),
            g.width,
            g.height,
          ))
        },
      )
    });

  let token = frame_pool
    .FrameArrived(&handler)
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "FrameArrived",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "FrameArrived registration failed"
      );
    })
    .context("Failed to register FrameArrived")?;

  {
    let mut g = inner_arc
      .lock()
      .map_err(|_| anyhow::anyhow!("WGC inner mutex poisoned"))?;
    g.frame_arrived_token = token;
  }

  session
    .StartCapture()
    .inspect_err(|e| {
      tracing::error!(
          operation = "wgc_ensure",
          step = "StartCapture",
          hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
          "StartCapture failed"
      );
    })
    .context("Failed to start capture session")?;

  tracing::debug!(
    operation = "wgc_ensure",
    "WGC pipeline initialized (device, frame pool, session, FrameArrived)"
  );
  Ok(inner_arc)
}

impl WgcCapture {
  fn new() -> Self {
    Self {
      inner: None,
      reusable_buffer: None,
    }
  }

  fn recreate_if_needed(&mut self) -> Result<()> {
    let old = self.inner.take();
    if let Some(arc) = old {
      {
        let g = arc
          .lock()
          .map_err(|_| anyhow::anyhow!("WGC inner mutex poisoned"))?;
        g.closed.store(true, Ordering::Release);
      }
      drop(arc);
      tracing::debug!(
        operation = "wgc_capture",
        step = "recreate_if_needed",
        "Tore down WGC pipeline; will re-initialize on next capture"
      );
    }
    Ok(())
  }

  fn ensure_initialized(&mut self) -> Result<Arc<Mutex<WgcCaptureInner>>> {
    if let Some(ref arc) = self.inner {
      return Ok(arc.clone());
    }

    let hwnd: HWND = unsafe { GetDesktopWindow() };
    let hmonitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
    if hmonitor.0.is_null() {
      return Err(anyhow::anyhow!("Failed to get primary monitor handle"));
    }

    if WGC_DISPATCHER.get().is_none() {
      let options = DispatcherQueueOptions {
        dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_DEDICATED,
        apartmentType: DQTAT_COM_STA,
      };
      let controller = unsafe { CreateDispatcherQueueController(options) }
        .context("CreateDispatcherQueueController failed")?;
      let _ = WGC_DISPATCHER.set(controller);
    }

    let (tx, rx) = sync_channel(0);
    let controller = WGC_DISPATCHER
      .get()
      .ok_or_else(|| anyhow::anyhow!("WGC_DISPATCHER not initialized"))?;
    let queue = controller
      .DispatcherQueue()
      .context("DispatcherQueue() failed")?;

    let dq_handler = DispatcherQueueHandler::new(move || {
      let result: Result<Arc<Mutex<WgcCaptureInner>>> = (|| {
        let hwnd: HWND = unsafe { GetDesktopWindow() };
        let hmonitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
        if hmonitor.0.is_null() {
          return Err(anyhow::anyhow!("Failed to get primary monitor handle"));
        }
        let (device, context) =
          unsafe { create_d3d11_device() }.context("Failed to create D3D11 device")?;

        build_wgc_pipeline(device, context, || {
          let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
              .inspect_err(|e| {
                tracing::error!(
                    operation = "wgc_ensure",
                    step = "get_interop",
                    hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
                    "IGraphicsCaptureItemInterop failed"
                );
              })
              .context("Failed to get IGraphicsCaptureItemInterop")?;
          unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(hmonitor) }
            .inspect_err(|e| {
              tracing::error!(
                  operation = "wgc_ensure",
                  step = "create_capture_item",
                  hresult = %format_hresult(windows::core::HRESULT(e.code().0)),
                  "CreateForMonitor failed"
              );
            })
            .context("Failed to create GraphicsCaptureItem for monitor")
        })
      })();
      if tx.send(result).is_err() {
        tracing::debug!(
          operation = "wgc_ensure",
          "WGC initialization receiver was dropped"
        );
      }
      Ok(())
    });

    queue.TryEnqueue(&dq_handler).map_err(|e| {
      tracing::error!(
        operation = "wgc_ensure",
        step = "TryEnqueue",
        "TryEnqueue failed: {}",
        e
      );
      anyhow::anyhow!("WGC TryEnqueue failed: {}", e)
    })?;

    match rx.recv_timeout(Duration::from_secs(30)) {
      Ok(Ok(inner_arc)) => {
        let frame_state = {
          let g = inner_arc
            .lock()
            .map_err(|_| anyhow::anyhow!("WGC inner mutex poisoned"))?;
          g.frame_state.clone()
        };
        wait_for_warmup(&frame_state, Duration::from_secs(WGC_WARMUP_TIMEOUT_SECS))?;
        self.inner = Some(inner_arc.clone());
        Ok(inner_arc)
      },
      Ok(Err(e)) => Err(e),
      Err(RecvTimeoutError::Timeout) => Err(anyhow::anyhow!("WGC init timeout")),
      Err(RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!("WGC init channel disconnected")),
    }
  }

  fn capture(&mut self) -> Result<(u32, u32, Vec<u8>)> {
    for _attempt in 0..2 {
      let inner_arc = self.ensure_initialized()?;
      let (frame_state, width, height) = {
        let g = inner_arc
          .lock()
          .map_err(|_| anyhow::anyhow!("WGC inner mutex poisoned"))?;
        (g.frame_state.clone(), g.width, g.height)
      };

      match request_frame(
        &frame_state,
        &mut self.reusable_buffer,
        width,
        height,
        Duration::from_secs(WGC_FRAME_WAIT_TIMEOUT_SECS),
      ) {
        Ok(t) => {
          set_last_capture_method("Windows Graphics Capture");
          return Ok(t);
        },
        Err(e) => {
          if is_recreate_needed(&e) {
            self.recreate_if_needed()?;
            continue;
          }
          return Err(e);
        },
      }
    }
    Err(anyhow::anyhow!("WGC capture failed after recreate retry"))
  }

  /// Like `capture()` but with a short 50 ms timeout for video loops.
  /// Returns `Ok(None)` on timeout (static screen); the caller should repeat the last frame.
  /// Returns `Err` only on genuine failures (device lost, init failure, etc.).
  #[cfg(test)]
  fn capture_video(&mut self) -> Result<Option<(u32, u32, Vec<u8>)>> {
    for _attempt in 0..2 {
      let inner_arc = self.ensure_initialized()?;
      let (frame_state, width, height) = {
        let g = inner_arc
          .lock()
          .map_err(|_| anyhow::anyhow!("WGC inner mutex poisoned"))?;
        (g.frame_state.clone(), g.width, g.height)
      };

      let need = (width as usize)
        .checked_mul(height as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("WGC buffer size overflow"))?;

      let mut guard = frame_state
        .0
        .lock()
        .map_err(|_| anyhow::anyhow!("WGC frame state mutex poisoned"))?;
      // Reuse buffer from previous capture if available and correctly sized.
      let buf = match self.reusable_buffer.take() {
        Some(mut b) if b.len() >= need => {
          b.truncate(need);
          b
        },
        _ => vec![0u8; need],
      };
      guard.begin_request(buf)?;
      let timeout = Duration::from_millis(WGC_VIDEO_FRAME_TIMEOUT_MS);
      let (mut g, wait_res) = frame_state
        .1
        .wait_timeout_while(guard, timeout, |s| s.result.is_none())
        .map_err(|_| anyhow::anyhow!("WGC frame state wait poisoned"))?;

      if wait_res.timed_out() {
        // Static screen — no new frame arrived within 50 ms. Caller repeats last frame.
        g.requested = false;
        // Reclaim unused buffer for next call
        if let Some(buf) = g.buffer_to_use.take() {
          self.reusable_buffer = Some(buf);
        }
        return Ok(None);
      }

      let res = g.result.take().ok_or_else(|| {
        anyhow::anyhow!("WGC frame state inconsistent: result was None after wait")
      })?;

      match res {
        Ok(t) => {
          set_last_capture_method("Windows Graphics Capture");
          return Ok(Some(t));
        },
        Err(e) => {
          if is_recreate_needed(&e) {
            drop(g);
            self.recreate_if_needed()?;
            continue;
          }
          return Err(e);
        },
      }
    }
    Err(anyhow::anyhow!("WGC capture failed after recreate retry"))
  }
}

/// Return a screenshot pixel buffer back to the WGC reuse pool.
///
/// After a screenshot's RGBA data has been consumed (saved + clipboard), the
/// caller should return the buffer here so the next capture can reuse the same
/// allocation instead of committing fresh pages. Silently does nothing if WGC
/// was never initialized (e.g. DXGI or GDI backend was used instead).
pub fn return_screenshot_buffer(buf: Vec<u8>) {
  if let Some(cap) = WGC_CAPTURE.get()
    && let Ok(mut guard) = cap.lock()
  {
    guard.reusable_buffer = Some(buf);
  }
}

/// Tear down the persistent WGC pipeline when it is no longer needed.
///
/// This is intentionally conservative: if an active recording is currently using WGC,
/// we leave the pipeline untouched. Otherwise we drop both the reusable RGBA buffer and
/// the WinRT/D3D capture pipeline so post-capture memory can settle closer to cold idle.
pub fn release_wgc_idle_memory() {
  if matches!(
    get_active_recording_backend(),
    Some(CaptureBackendKind::Wgc)
  ) {
    return;
  }

  if let Some(cap) = WGC_CAPTURE.get()
    && let Ok(mut guard) = cap.lock()
  {
    let _ = guard.reusable_buffer.take();
    if let Err(error) = guard.recreate_if_needed() {
      tracing::debug!(
        operation = "wgc_release_idle_memory",
        error = %error,
        "Failed to tear down idle WGC pipeline"
      );
    }
  }
}

pub fn memory_diagnostics() -> WgcMemoryDiagnostics {
  let Some(cap) = WGC_CAPTURE.get() else {
    return WgcMemoryDiagnostics {
      initialized: false,
      pipeline_active: false,
      reusable_buffer_bytes: 0,
    };
  };

  let Ok(guard) = cap.lock() else {
    return WgcMemoryDiagnostics {
      initialized: true,
      pipeline_active: false,
      reusable_buffer_bytes: 0,
    };
  };

  WgcMemoryDiagnostics {
    initialized: true,
    pipeline_active: guard.inner.is_some(),
    reusable_buffer_bytes: guard
      .reusable_buffer
      .as_ref()
      .map(|buf| buf.capacity())
      .unwrap_or(0),
  }
}

/// Try Windows Graphics Capture API (WGC). Reuses a persistent pipeline; event-driven
/// FrameArrived with warm-up and GPU sync. Assume MTA; no CoInitializeEx here.
pub unsafe fn try_wgc_capture() -> Result<(u32, u32, Vec<u8>)> {
  let cap = WGC_CAPTURE.get_or_init(|| Mutex::new(WgcCapture::new()));
  let mut guard = cap
    .lock()
    .map_err(|_| anyhow::anyhow!("WGC capture mutex poisoned"))?;
  guard.capture()
}

/// Test-only WGC helper that exercises the short video timeout path.
///
/// # Safety
///
/// Must be called from a thread that has initialized a COM apartment (MTA or STA).
#[cfg(test)]
pub unsafe fn try_wgc_capture_video() -> Result<Option<(u32, u32, Vec<u8>)>> {
  let cap = WGC_CAPTURE.get_or_init(|| Mutex::new(WgcCapture::new()));
  let mut guard = cap
    .lock()
    .map_err(|_| anyhow::anyhow!("WGC capture mutex poisoned"))?;
  guard.capture_video()
}

/// Synchronous warm-up — meant to be called from within an existing background thread.
/// Idempotent: safe to call multiple times; the pipeline is only initialized once.
pub(crate) fn warm_up_wgc_sync() {
  let cap = WGC_CAPTURE.get_or_init(|| Mutex::new(WgcCapture::new()));
  if let Ok(mut g) = cap.lock() {
    let _ = g.ensure_initialized();
  }
  tracing::debug!(operation = "wgc_warmup", "WGC pipeline warmed up");
}

/// Warm up the WGC pipeline in the background so the first video recording starts instantly.
/// Idempotent — safe to call multiple times; the pipeline is only initialized once.
pub fn warm_up_wgc() {
  static WARMUP_ONCE: OnceLock<()> = OnceLock::new();
  WARMUP_ONCE.get_or_init(|| {
    std::thread::Builder::new()
      .name("flinch-wgc-warmup".into())
      .spawn(warm_up_wgc_sync)
      .ok();
  });
}

pub(crate) fn is_initialized() -> bool {
  WGC_CAPTURE.get().is_some()
}

#[cfg(test)]
mod frame_tests;

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Instant;

  /// Verify try_wgc_capture_video does not panic and returns a valid Result variant.
  #[test]
  #[ignore = "captures the real desktop: cargo xtask test:capture:desktop"]
  fn test_try_wgc_capture_video_returns_ok_or_err() {
    let result =
      unsafe { try_wgc_capture_video() }.expect("desktop WGC must initialize successfully");
    match result {
      Some((w, h, data)) => {
        assert!(w > 0, "width must be positive");
        assert!(h > 0, "height must be positive");
        assert_eq!(data.len(), (w * h * 4) as usize, "data must be w*h*4 bytes");
      },
      None => {
        // Static screen — timeout fired correctly; acceptable.
      },
    }
  }

  /// After initialization, each call must complete within 500 ms.
  /// The 50 ms timeout ensures static screens return quickly rather than hanging.
  #[test]
  #[ignore = "captures the real desktop: cargo xtask test:capture:desktop"]
  fn test_wgc_video_timeout_is_short() {
    // First call may include pipeline initialization time (up to ~15 s on cold start).
    // We don't time it — only the second call is subject to the 500 ms bound.
    // SAFETY: this helper establishes its pipeline on the dedicated capture owner.
    // An unavailable interactive WGC session cannot establish the warm-pipeline
    // precondition. Measuring a second initialization retry as a frame wait would
    // incorrectly compare the cold-start budget to the 50 ms frame budget.
    unsafe { try_wgc_capture_video() }.expect("WGC warm-pipeline precondition");

    let start = Instant::now();
    unsafe { try_wgc_capture_video() }.expect("warm WGC frame request");
    let elapsed = start.elapsed();

    assert!(
      elapsed.as_millis() < 500,
      "WGC video capture took {}ms after initialization (expected ≤500ms)",
      elapsed.as_millis()
    );
  }

  /// Calling warm_up_wgc twice must not panic and must not duplicate initialization.
  #[test]
  #[ignore = "initializes real desktop capture: cargo xtask test:capture:desktop"]
  fn test_warm_up_wgc_is_idempotent() {
    warm_up_wgc_sync();
    let first = WGC_CAPTURE
      .get()
      .expect("initialized WGC")
      .lock()
      .unwrap()
      .inner
      .clone()
      .expect("live pipeline");
    warm_up_wgc_sync();
    let second = WGC_CAPTURE
      .get()
      .unwrap()
      .lock()
      .unwrap()
      .inner
      .clone()
      .expect("live reused pipeline");
    assert!(
      Arc::ptr_eq(&first, &second),
      "warmup must retain the same initialized pipeline"
    );
  }
}
