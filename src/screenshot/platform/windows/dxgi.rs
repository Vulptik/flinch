// src/screenshot/platform/windows/dxgi.rs
// Desktop Duplication API (DXGI) — persistent pipeline, optimized for ≤15 ms capture latency.
//
// IMPORTANT: IDXGIOutput1::DuplicateOutput allows only one IDXGIOutputDuplication per output
// per process. We use a process-wide (OnceLock<Mutex<DxgiCapture>>) pipeline so all threads
// reuse the same duplication. Thread-local storage caused the second capture (on a different
// async/thread) to call DuplicateOutput again → E_INVALIDARG "already duplicating".

use anyhow::{Context, Result};
#[cfg(test)]
mod software_tests;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Dxgi as dxgi_api;
use windows::{
  Win32::{
    Foundation::HMODULE,
    Graphics::{
      Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
      Direct3D11::{
        D3D11_BIND_FLAG, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_RESOURCE_MISC_FLAG, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice,
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
      },
      Dxgi::{
        Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
        CreateDXGIFactory1, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1,
        IDXGIOutputDuplication, IDXGIResource,
      },
    },
    System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
  },
  core::Interface,
};

use super::conversion::convert_bgra_to_rgba_optimized_direct;
use super::set_last_capture_method;
use super::utils::{
  find_adapter_with_output_for_monitor, find_hardware_adapter_with_output, format_hresult,
};
use crate::monitors::MonitorInfo;

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct DxgiMemoryDiagnostics {
  pub pipeline_count: usize,
  pub staging_count: usize,
  pub cached_bgra_bytes: usize,
}

// DXGI error codes
const DXGI_ERROR_WAIT_TIMEOUT: u32 = dxgi_api::DXGI_ERROR_WAIT_TIMEOUT.0 as u32;
const DXGI_ERROR_WAS_STILL_DRAWING: u32 = dxgi_api::DXGI_ERROR_WAS_STILL_DRAWING.0 as u32;
const DXGI_ERROR_DEVICE_REMOVED: u32 = dxgi_api::DXGI_ERROR_DEVICE_REMOVED.0 as u32;
const DXGI_ERROR_ACCESS_LOST: u32 = dxgi_api::DXGI_ERROR_ACCESS_LOST.0 as u32;
const SCREENSHOT_FRAME_WAIT_BUDGET: Duration = Duration::from_millis(100);
/// How long a cached image must go unchallenged before it counts as the current desktop.
/// A zero wait, or trusting one pointer-only event, froze a frame 1.7 s old on a busy
/// desktop. This spans a refresh at 60 Hz and only delays captures of an unchanged screen.
const CACHE_CONFIRM_WAIT_MS: u32 = 25;
/// Pointer-only events to skip before accepting the cache, so a moving mouse stays bounded.
const CACHE_POINTER_ONLY_RETRIES: u32 = 3;
const _: () = assert!(CACHE_POINTER_ONLY_RETRIES > 0);

fn first_acquire_wait_ms(has_cache: bool, remaining_ms: u32) -> u32 {
  if has_cache {
    CACHE_CONFIRM_WAIT_MS.min(remaining_ms)
  } else {
    remaining_ms
  }
}

fn remaining_frame_wait_ms(elapsed: Duration) -> u32 {
  SCREENSHOT_FRAME_WAIT_BUDGET
    .saturating_sub(elapsed)
    .as_millis() as u32
}

fn desktop_image_updated(info: &DXGI_OUTDUPL_FRAME_INFO) -> bool {
  info.LastPresentTime != 0
}

fn update_topology(previous: &mut Option<HashSet<String>>, current: HashSet<String>) -> bool {
  let changed = previous.as_ref().is_some_and(|old| old != &current);
  *previous = Some(current);
  changed
}

// Thread-local retry counter for DXGI_ERROR_ACCESS_LOST escalating fallback
thread_local! {
  static DXGI_ACCESS_LOST_RETRY_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Converts a windows::core::Error into an HRESULT for format_hresult.
#[inline]
fn error_to_hresult(e: &windows::core::Error) -> windows::core::HRESULT {
  windows::core::HRESULT(e.code().0)
}

/// RAII guard: calls `ReleaseFrame` on drop when `AcquireNextFrame` has succeeded.
/// Ensures the frame is always released even if Map, CopyResource, or later steps fail.
struct FrameGuard {
  duplication: IDXGIOutputDuplication,
}

impl Drop for FrameGuard {
  fn drop(&mut self) {
    // SAFETY: The guard is constructed only after a successful acquisition.
    if let Err(error) = unsafe { self.duplication.ReleaseFrame() } {
      tracing::warn!(operation = "dxgi_capture", %error, "Failed to release acquired DXGI frame");
    }
  }
}

/// RAII guard: calls `Unmap` on the staging texture on drop when `Map` has succeeded.
/// Ensures the staging is always unmapped even if later steps fail.
struct MapGuard {
  context: ID3D11DeviceContext,
  staging: ID3D11Texture2D,
}

impl Drop for MapGuard {
  fn drop(&mut self) {
    // SAFETY: The guard owns one successful map and keeps its context/resource alive.
    unsafe { self.context.Unmap(&self.staging, 0) };
  }
}

/// Inner state: D3D11 device, duplication, staging texture. Reused across captures.
/// Recreated only on resolution change, output change, or DXGI_ERROR_DEVICE_REMOVED.
struct DxgiCaptureInner {
  #[allow(dead_code)]
  factory: IDXGIFactory1,
  #[allow(dead_code)] // Kept alive so context/duplication/staging remain valid
  device: ID3D11Device,
  context: ID3D11DeviceContext,
  duplication: IDXGIOutputDuplication,
  /// CPU-readable copy of the desktop texture. Created lazily via `ensure_staging()`,
  /// can be released via `release_dxgi_staging_textures()` to reclaim ~8 MB/1080p
  /// (~33 MB/4K) per monitor without affecting the duplication pipeline.
  staging: Option<ID3D11Texture2D>,
  width: u32,
  height: u32,
  /// Monitor position in virtual screen coordinates (for validation/debugging)
  #[allow(dead_code)]
  monitor_x: i32,
  #[allow(dead_code)]
  monitor_y: i32,
  /// Packed BGRA (width*4 bytes per row), reused when LastPresentTime is zero.
  cached_bgra: Vec<u8>,
  /// Atomic flag for cache validity (avoids race with length check during concurrent access)
  cache_valid: std::sync::atomic::AtomicBool,
}

impl DxgiCaptureInner {
  /// Ensure the staging texture exists, creating it if it was released.
  /// `CreateTexture2D` for a staging texture is ~1 ms (no shader compilation).
  fn ensure_staging(&mut self) -> Result<&ID3D11Texture2D> {
    if self.staging.is_none() {
      let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: self.width,
        Height: self.height,
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
      let mut staging: Option<ID3D11Texture2D> = None;
      unsafe {
        self
          .device
          .CreateTexture2D(&staging_desc, None, Some(&mut staging))
      }
      .context("Failed to recreate staging texture")?;
      self.staging = Some(
        staging.ok_or_else(|| anyhow::anyhow!("Staging texture is None after CreateTexture2D"))?,
      );
      tracing::trace!(
        operation = "dxgi_capture",
        width = self.width,
        height = self.height,
        "Recreated staging texture on demand"
      );
    }
    self
      .staging
      .as_ref()
      .ok_or_else(|| anyhow::anyhow!("Staging texture missing after ensure"))
  }
}

/// Long-lived DXGI capture pipeline. Create once, reuse for all captures.
/// Avoids per-capture creation of factory, adapter, device, duplication, and staging texture.
pub struct DxgiCapture {
  inner: Option<DxgiCaptureInner>,
  /// Target monitor bounds (used for initialization)
  target_x: i32,
  target_y: i32,
  target_width: u32,
  target_height: u32,
}

/// Per-monitor DXGI capture instances. Each monitor gets its own IDXGIOutputDuplication
/// since Windows allows only one duplication per output per process.
/// Key format: "x,y,wxh" (e.g., "0,0,1920x1080")
static DXGI_CAPTURES: OnceLock<Mutex<HashMap<String, DxgiCapture>>> = OnceLock::new();
// Independent of lazy pipeline creation/reset. Never hold this lock with DXGI_CAPTURES.
static DXGI_TOPOLOGY: Mutex<Option<HashSet<String>>> = Mutex::new(None);

static WARMUP_ONCE: Once = Once::new();

/// Release staging textures from all DXGI capture instances.
///
/// The D3D11 device, context, and output duplication stay alive, avoiding pipeline
/// re-initialization. Only the staging texture (CPU-readable copy buffer) is
/// dropped, and `CreateTexture2D` recreates it lazily (~1 ms).
///
/// The BGRA cache stays: without it, a capture on an unchanged desktop waits out
/// the whole frame budget before recovering, because no new frame ever arrives.
pub fn release_dxgi_idle_memory() {
  if let Some(captures) = DXGI_CAPTURES.get()
    && let Ok(mut map) = captures.lock()
  {
    let mut staging_released = 0u32;
    for capture in map.values_mut() {
      if let Some(ref mut inner) = capture.inner
        && inner.staging.take().is_some()
      {
        staging_released += 1;
      }
    }
    if staging_released > 0 {
      tracing::debug!(
        operation = "dxgi_idle_memory",
        staging_released,
        "Released DXGI staging textures"
      );
    }
  }
}

pub fn memory_diagnostics() -> DxgiMemoryDiagnostics {
  let Some(captures) = DXGI_CAPTURES.get() else {
    return DxgiMemoryDiagnostics {
      pipeline_count: 0,
      staging_count: 0,
      cached_bgra_bytes: 0,
    };
  };

  let Ok(map) = captures.lock() else {
    return DxgiMemoryDiagnostics {
      pipeline_count: 0,
      staging_count: 0,
      cached_bgra_bytes: 0,
    };
  };

  let mut staging_count = 0usize;
  let mut cached_bgra_bytes = 0usize;
  for capture in map.values() {
    if let Some(inner) = capture.inner.as_ref() {
      if inner.staging.is_some() {
        staging_count += 1;
      }
      cached_bgra_bytes += inner.cached_bgra.capacity();
    }
  }

  DxgiMemoryDiagnostics {
    pipeline_count: map.len(),
    staging_count,
    cached_bgra_bytes,
  }
}

/// Invalidate DXGI capture pipelines when the monitor topology has changed.
///
/// When a monitor is hot-plugged or unplugged, Windows invalidates ALL existing
/// `IDXGIOutputDuplication` handles (returning `DXGI_ERROR_ACCESS_LOST` on next use).
/// This affects every cached pipeline, not just the one for the changed monitor.
///
/// We detect topology changes by comparing complete monitor enumeration snapshots.
/// On any mismatch (added, removed, or repositioned monitors) we flush ALL pipelines
/// so they are recreated fresh with warmup on next capture.
pub fn invalidate_stale_captures() {
  let current_monitors = crate::monitors::list_monitors();
  let valid_keys: HashSet<String> = current_monitors
    .iter()
    .map(|m| monitor_key(m.x, m.y, m.width, m.height))
    .collect();

  let changed = match DXGI_TOPOLOGY.lock() {
    Ok(mut previous) => update_topology(&mut previous, valid_keys.clone()),
    Err(poisoned) => {
      tracing::warn!("DXGI topology mutex poisoned; recovering snapshot");
      update_topology(&mut poisoned.into_inner(), valid_keys.clone())
    },
  };
  if changed && let Some(captures) = DXGI_CAPTURES.get() {
    let mut map = match captures.lock() {
      Ok(map) => map,
      Err(poisoned) => {
        tracing::warn!("DXGI captures mutex poisoned during topology invalidation; recovering");
        poisoned.into_inner()
      },
    };
    // Topology changed — flush ALL pipelines, not just removed ones.
    // Even monitors that are still present need fresh DXGI handles because
    // Windows invalidates all OutputDuplication instances on display changes.
    let old_count = map.len();
    map.clear();
    tracing::info!(
      operation = "dxgi_capture",
      step = "invalidate_topology_change",
      old_pipelines = old_count,
      current_monitors = valid_keys.len(),
      "Display topology changed — flushed all DXGI pipelines"
    );
  }
}

/// Flush all cached DXGI capture pipelines.
///
/// This is heavier than `invalidate_stale_captures()` and should be used only when a
/// caller strongly suspects the existing duplications are poisoned, such as after a
/// monitor power-cycle that returned a successful-but-black frame.
pub fn reset_dxgi_captures() {
  if let Some(captures) = DXGI_CAPTURES.get()
    && let Ok(mut map) = captures.lock()
  {
    let old_count = map.len();
    if old_count > 0 {
      map.clear();
      tracing::info!(
        operation = "dxgi_capture",
        step = "reset_all",
        old_pipelines = old_count,
        "Flushed all DXGI pipelines"
      );
    }
  }
}

/// Generate a key for the per-monitor capture map
fn monitor_key(x: i32, y: i32, width: u32, height: u32) -> String {
  format!("{},{},{}x{}", x, y, width, height)
}

fn build_dxgi_pipeline(
  factory: IDXGIFactory1,
  device: ID3D11Device,
  device_context: ID3D11DeviceContext,
  bounds: (i32, i32, u32, u32),
  warmup: (Instant, u32),
  duplicate: impl FnOnce(&ID3D11Device) -> windows::core::Result<IDXGIOutputDuplication>,
) -> Result<DxgiCaptureInner> {
  let (monitor_x, monitor_y, width, height) = bounds;
  let (initialization_started, warmup_timeout_ms) = warmup;
  let duplication = duplicate(&device)
    .inspect_err(|e| {
      let h = error_to_hresult(e);
      match e.code() {
        dxgi_api::DXGI_ERROR_ACCESS_DENIED => {
          tracing::debug!(
            reason = "access_denied",
            "DuplicateOutput: DXGI_ERROR_ACCESS_DENIED"
          );
        },
        dxgi_api::DXGI_ERROR_SESSION_DISCONNECTED => {
          tracing::debug!(
            reason = "session_disconnected",
            "DuplicateOutput: DXGI_ERROR_SESSION_DISCONNECTED"
          );
        },
        dxgi_api::DXGI_ERROR_UNSUPPORTED => {
          tracing::debug!(
            reason = "unsupported",
            "DuplicateOutput: DXGI_ERROR_UNSUPPORTED"
          );
        },
        _ => {},
      }
      tracing::error!(
          operation = "dxgi_capture",
          step = "duplicate_output",
          hresult = %format_hresult(h),
          "DuplicateOutput failed"
      );
    })
    .context("Failed to duplicate output")?;

  let staging_desc = D3D11_TEXTURE2D_DESC {
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
    BindFlags: D3D11_BIND_FLAG(0).0 as u32,
    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
    MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
  };

  let mut staging: Option<ID3D11Texture2D> = None;
  unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
    .inspect_err(|e| {
      let h = error_to_hresult(e);
      tracing::error!(
          operation = "dxgi_capture",
          step = "create_staging",
          hresult = %format_hresult(h),
          "CreateTexture2D staging failed"
      );
    })
    .context("Failed to create staging texture")?;

  let staging =
    staging.ok_or_else(|| anyhow::anyhow!("Staging texture is None after CreateTexture2D"))?;

  tracing::debug!(
    operation = "dxgi_capture",
    step = "ensure_initialized",
    width = width,
    height = height,
    "DXGI pipeline initialized (factory, device, duplication, staging)"
  );

  // The first event can be pointer-only with a black surface. Never seed
  // the desktop cache from it. Share the caller's remaining wait budget.
  let mut initial_cache: Option<Vec<u8>> = None;
  {
    let mut warmup_info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut warmup_resource: Option<IDXGIResource> = None;
    // SAFETY: Duplication and both output pointers remain valid for this call.
    let warmup_timeout_ms = Duration::from_millis(u64::from(warmup_timeout_ms))
      .saturating_sub(initialization_started.elapsed())
      .as_millis() as u32;
    match unsafe {
      duplication.AcquireNextFrame(warmup_timeout_ms, &mut warmup_info, &mut warmup_resource)
    } {
      Ok(()) => {
        let _frame_guard = FrameGuard {
          duplication: duplication.clone(),
        };
        let cache_result = (|| -> Result<Option<Vec<u8>>> {
          if !desktop_image_updated(&warmup_info) {
            return Ok(None);
          }
          let resource =
            warmup_resource.ok_or_else(|| anyhow::anyhow!("Warmup desktop resource missing"))?;
          let texture = resource
            .cast::<ID3D11Texture2D>()
            .context("Invalid warmup desktop texture")?;
          // SAFETY: Both textures belong to this device with matching BGRA dimensions.
          unsafe { device_context.CopyResource(&staging, &texture) };
          let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
          // SAFETY: The staging resource is CPU-readable and mapped once here.
          unsafe { device_context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .context("Failed to map warmup staging texture")?;
          let _map_guard = MapGuard {
            context: device_context.clone(),
            staging: staging.clone(),
          };
          let row_pitch = mapped.RowPitch as usize;
          let data_len = row_pitch
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow::anyhow!("Warmup texture size overflow"))?;
          if mapped.pData.is_null() || row_pitch < width as usize * 4 {
            anyhow::bail!("Invalid warmup texture pointer or row pitch");
          }
          // SAFETY: Map succeeded, stride is validated, and MapGuard keeps it mapped.
          let bgra = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), data_len) };
          validated_warmup_cache(&warmup_info, bgra, row_pitch, width, height)
        })();
        match cache_result {
          Ok(cache) => initial_cache = cache,
          Err(error) => {
            tracing::warn!(operation = "dxgi_capture", step = "warmup_cache", %error, "Warmup cache unavailable; next capture must acquire a desktop image")
          },
        }
        tracing::debug!(
          operation = "dxgi_capture",
          step = "warmup",
          cached = initial_cache.is_some(),
          last_present_time = warmup_info.LastPresentTime,
          total_metadata = warmup_info.TotalMetadataBufferSize,
          accumulated_frames = warmup_info.AccumulatedFrames,
          protected_content = warmup_info.ProtectedContentMaskedOut.as_bool(),
          "Pipeline warmup: frame inspected and released"
        );
      },
      Err(error) => {
        tracing::debug!(operation = "dxgi_capture", step = "warmup", %error, timeout_ms = warmup_timeout_ms, "Warmup frame unavailable; cache remains empty")
      },
    }
  }

  let warmup_cache_valid = initial_cache.is_some();
  Ok(DxgiCaptureInner {
    factory,
    device,
    context: device_context,
    duplication,
    staging: Some(staging),
    width,
    height,
    monitor_x,
    monitor_y,
    cached_bgra: initial_cache.unwrap_or_default(),
    cache_valid: std::sync::atomic::AtomicBool::new(warmup_cache_valid),
  })
}

impl DxgiCapture {
  /// Create a new capture pipeline for the primary/first monitor (legacy).
  #[allow(dead_code)]
  pub fn new() -> Self {
    Self {
      inner: None,
      target_x: 0,
      target_y: 0,
      target_width: 0,
      target_height: 0,
    }
  }

  /// Create a new capture pipeline targeting a specific monitor.
  pub fn new_for_monitor(x: i32, y: i32, width: u32, height: u32) -> Self {
    Self {
      inner: None,
      target_x: x,
      target_y: y,
      target_width: width,
      target_height: height,
    }
  }

  /// Drop the pipeline so the next capture() will re-initialize. Call on DEVICE_REMOVED
  /// or when the caller knows the output/resolution changed.
  pub fn recreate_if_needed(&mut self) {
    if self.inner.take().is_some() {
      tracing::debug!(
        operation = "dxgi_capture",
        step = "recreate_if_needed",
        "Dropped DXGI pipeline; will re-initialize on next capture"
      );
    }
  }

  /// Optional: release resources. recreate_if_needed() is sufficient for recovery.
  #[allow(dead_code)]
  pub fn shutdown(&mut self) {
    self.recreate_if_needed();
  }

  /// Initialize once and retain only a validated desktop image from warmup.
  /// Do not consume a second update without updating the cache.
  pub fn warm_up(&mut self) -> Result<()> {
    self.ensure_initialized(2000)?;
    Ok(())
  }

  /// Ensure COM and the full pipeline (factory, adapter, device, duplication, staging) exist.
  /// Performed once; subsequent captures skip this.
  fn ensure_initialized(&mut self, warmup_timeout_ms: u32) -> Result<&mut DxgiCaptureInner> {
    if self.inner.is_some() {
      return self
        .inner
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("DXGI pipeline missing after presence check"));
    }

    let initialization_started = Instant::now();

    // COM init (cheap if already initialized)
    let com_result: windows::core::HRESULT = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com_result.is_ok() {
      tracing::trace!(
        operation = "dxgi_capture",
        step = "com_init",
        "COM initialized"
      );
    } else if com_result.0 as u32 != 0x80010106 {
      tracing::warn!(
          operation = "dxgi_capture",
          step = "com_init",
          hresult = %format_hresult(com_result),
          "COM init warning"
      );
    }

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
      .inspect_err(|e| {
        let h = error_to_hresult(e);
        tracing::error!(
            operation = "dxgi_capture",
            step = "create_factory",
            hresult = %format_hresult(h),
            "CreateDXGIFactory1 failed"
        );
      })
      .context("Failed to create DXGI factory")?;

    // Find adapter and output - use target monitor if specified, otherwise first available
    let (adapter1, output) = if self.target_width > 0 && self.target_height > 0 {
      unsafe {
        find_adapter_with_output_for_monitor(
          &factory,
          self.target_x,
          self.target_y,
          self.target_width,
          self.target_height,
        )
      }
      .context("Failed to find adapter with output for specified monitor")?
    } else {
      unsafe { find_hardware_adapter_with_output(&factory) }
        .context("Failed to find hardware adapter with outputs")?
    };

    let adapter: IDXGIAdapter = adapter1
      .cast()
      .inspect_err(|e| {
        let h = error_to_hresult(e);
        tracing::error!(
            operation = "dxgi_capture",
            step = "cast_adapter",
            hresult = %format_hresult(h),
            "Adapter cast failed"
        );
      })
      .context("Failed to cast adapter")?;

    let output1: IDXGIOutput1 = output
      .cast()
      .inspect_err(|e| {
        let h = error_to_hresult(e);
        tracing::error!(
            operation = "dxgi_capture",
            step = "cast_output",
            hresult = %format_hresult(h),
            "Output cast failed"
        );
      })
      .context("Failed to cast to IDXGIOutput1")?;

    let output_desc = unsafe { output1.GetDesc() }
      .inspect_err(|e| {
        let h = error_to_hresult(e);
        tracing::error!(
            operation = "dxgi_capture",
            step = "get_output_desc",
            hresult = %format_hresult(h),
            "GetDesc failed"
        );
      })
      .context("Failed to get output description")?;

    let monitor_x = output_desc.DesktopCoordinates.left;
    let monitor_y = output_desc.DesktopCoordinates.top;
    let width = (output_desc.DesktopCoordinates.right - output_desc.DesktopCoordinates.left) as u32;
    let height =
      (output_desc.DesktopCoordinates.bottom - output_desc.DesktopCoordinates.top) as u32;

    let mut device: Option<ID3D11Device> = None;
    let mut device_context: Option<ID3D11DeviceContext> = None;
    let mut feature_level = D3D_FEATURE_LEVEL_11_0;
    let feature_levels = [
      D3D_FEATURE_LEVEL_11_0,
      windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_1,
      windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_0,
    ];

    unsafe {
      D3D11CreateDevice(
        Some(&adapter),
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE(std::ptr::null_mut()),
        windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG::default(),
        Some(&feature_levels),
        windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION,
        Some(&mut device),
        Some(&mut feature_level),
        Some(&mut device_context),
      )
    }
    .inspect_err(|e| {
      let h = error_to_hresult(e);
      tracing::error!(
          operation = "dxgi_capture",
          step = "create_d3d11_device",
          hresult = %format_hresult(h),
          "D3D11CreateDevice failed"
      );
    })
    .context("Failed to create D3D11 device")?;

    let device = device.ok_or_else(|| anyhow::anyhow!("D3D11 device is None after creation"))?;
    let device_context = device_context
      .ok_or_else(|| anyhow::anyhow!("D3D11 device context is None after creation"))?;

    self.inner = Some(build_dxgi_pipeline(
      factory,
      device,
      device_context,
      (monitor_x, monitor_y, width, height),
      (initialization_started, warmup_timeout_ms),
      // SAFETY: the output and device belong to the adapter resolved above.
      |device| unsafe { output1.DuplicateOutput(device) },
    )?);
    match self.inner.as_mut() {
      Some(inner) => Ok(inner),
      None => Err(anyhow::anyhow!("Failed to initialize DXGI inner state")),
    }
  }

  /// Perform one capture. Returns (width, height, RGBA pixels).
  /// Reuses staging texture and cached frame when possible; converts to RGBA only at the end.
  pub fn capture(&mut self) -> Result<(u32, u32, Vec<u8>)> {
    const ACQUIRE_TIMEOUT_ZERO_MS: u32 = 0;
    // Shared across warmup, pointer-only events and recreation. Initialization
    // and GPU mapping time are additional; the caller retains GDI recovery.
    let wait_started = Instant::now();
    const WAS_STILL_DRAWING_RETRIES: usize = 5;

    'outer: for _attempt in 0..2 {
      let inner = self.ensure_initialized(remaining_frame_wait_ms(wait_started.elapsed()))?;
      let (w, h) = (inner.width, inner.height);

      let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
      let mut desktop_resource: Option<IDXGIResource> = None;

      // Confirm a valid cache briefly; otherwise share the remaining wait budget.
      let has_cache = inner.cache_valid.load(std::sync::atomic::Ordering::Acquire);
      let mut acquire_timeout =
        first_acquire_wait_ms(has_cache, remaining_frame_wait_ms(wait_started.elapsed()));
      let mut pointer_only_left = CACHE_POINTER_ONLY_RETRIES;
      let mut last_err = None::<windows::core::Error>;

      for _ in 0..=WAS_STILL_DRAWING_RETRIES {
        let hr = unsafe {
          inner.duplication.AcquireNextFrame(
            acquire_timeout.min(remaining_frame_wait_ms(wait_started.elapsed())),
            &mut frame_info,
            &mut desktop_resource,
          )
        };
        let frozen = match &hr {
          Ok(()) => desktop_image_updated(&frame_info),
          Err(error) => has_cache && error.code().0 as u32 == DXGI_ERROR_WAIT_TIMEOUT,
        };
        if frozen {
          crate::commands::log_shortcut_to_freeze();
        }

        match hr {
          Ok(()) => {
            let _frame_guard = FrameGuard {
              duplication: inner.duplication.clone(),
            };
            let total_meta = frame_info.TotalMetadataBufferSize;
            tracing::debug!(
              operation = "dxgi_capture",
              step = "acquire_ok",
              total_metadata = total_meta,
              last_present_time = frame_info.LastPresentTime,
              accumulated_frames = frame_info.AccumulatedFrames,
              protected_content = frame_info.ProtectedContentMaskedOut.as_bool(),
              cache_hit = (!desktop_image_updated(&frame_info) && has_cache)
            );

            // LastPresentTime distinguishes desktop updates from pointer-only events.
            if !desktop_image_updated(&frame_info) && has_cache {
              if pointer_only_left > 0 {
                pointer_only_left -= 1;
                // FrameGuard releases before the next bounded acquisition.
                continue;
              }
              tracing::debug!(
                operation = "dxgi_capture",
                step = "return_cached",
                source = "unchanged_frame"
              );
              let rgba_len = (w * h * 4) as usize;
              let mut output_rgba = vec![0u8; rgba_len];
              convert_bgra_to_rgba_optimized_direct(&inner.cached_bgra, &mut output_rgba);
              set_last_capture_method("Desktop Duplication");
              return Ok((w, h, output_rgba));
            }

            if !desktop_image_updated(&frame_info) {
              tracing::debug!(
                operation = "dxgi_capture",
                step = "skip_pointer_only",
                "No desktop image or valid cache; acquiring within remaining budget"
              );
              acquire_timeout = remaining_frame_wait_ms(wait_started.elapsed());
              // FrameGuard releases before the next bounded acquisition.
              continue;
            }

            let resource = desktop_resource
              .ok_or_else(|| anyhow::anyhow!("Desktop resource is None after AcquireNextFrame"))?;

            let texture: ID3D11Texture2D = resource
              .cast()
              .inspect_err(|e| {
                let h = error_to_hresult(e);
                tracing::error!(
                    operation = "dxgi_capture",
                    step = "cast_texture",
                    hresult = %format_hresult(h),
                    "Failed to cast to ID3D11Texture2D"
                );
              })
              .context("Failed to cast to ID3D11Texture2D")?;

            // Ensure staging texture exists (recreated lazily if released for memory savings)
            let staging = inner.ensure_staging()?.clone();
            tracing::trace!(operation = "dxgi_capture", step = "copy_resource");
            unsafe { inner.context.CopyResource(&staging, &texture) };

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
              inner
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            }
            .inspect_err(|e| {
              let h = error_to_hresult(e);
              tracing::error!(
                  operation = "dxgi_capture",
                  step = "map_texture",
                  hresult = %format_hresult(h),
                  "Failed to map staging texture"
              );
            })
            .context("Failed to map staging texture")?;

            let _map_guard = MapGuard {
              context: inner.context.clone(),
              staging: staging.clone(),
            };

            let row_pitch = mapped.RowPitch as usize;
            let data_len = row_pitch * (h as usize);
            let expected_row_bytes = (w as usize) * 4;

            // Runtime validation: row_pitch must be >= width*4
            if row_pitch < expected_row_bytes {
              return Err(
                anyhow::anyhow!(
                  "Invalid RowPitch {} < {} (width*4) - corrupt or invalid frame data",
                  row_pitch,
                  expected_row_bytes
                )
                .context("Failed to validate Desktop Duplication row pitch"),
              );
            }

            // Log stride information for debugging
            if row_pitch == expected_row_bytes {
              tracing::trace!(
                operation = "dxgi_capture",
                step = "map_ok",
                row_pitch,
                "RowPitch equals width*4 (tightly packed, optimal)"
              );
            } else {
              tracing::warn!(
                operation = "dxgi_capture",
                step = "map_ok",
                row_pitch,
                expected_row_bytes,
                padding = row_pitch - expected_row_bytes,
                "RowPitch > width*4 (padding detected, will be handled correctly)"
              );
            }

            // SAFETY: Map succeeded; pData is valid for data_len bytes until Unmap.
            // Check for null in case of a buggy driver. The slice must not be used after Unmap.
            let ptr = mapped.pData;
            if ptr.is_null() {
              return Err(
                anyhow::anyhow!("Map returned null pData").context("Failed to map staging texture"),
              );
            }
            let bgra_slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, data_len) };

            let black = is_suspicious_first_uncached_black_frame(bgra_slice, row_pitch, w, h);
            if black {
              inner
                .cache_valid
                .store(false, std::sync::atomic::Ordering::Release);
              tracing::warn!(
                operation = "dxgi_capture",
                step = "reject_suspicious_black_frame",
                last_present_time = frame_info.LastPresentTime,
                total_metadata = total_meta,
                "Black desktop image rejected; caller may retry or use GDI"
              );
              anyhow::bail!("DXGI returned a suspicious all-black desktop image");
            }

            // Update cache: packed BGRA (width*4 bytes per row) for dirty-frame skip.
            // Per-row copy when row_pitch != width*4 to prevent skew/striping.
            let cache_len = (w * h * 4) as usize;
            inner.cached_bgra.resize(cache_len, 0);
            if row_pitch == (w as usize) * 4 {
              inner.cached_bgra.copy_from_slice(bgra_slice);
            } else {
              for y in 0..(h as usize) {
                let src = &bgra_slice[y * row_pitch..][..(w as usize) * 4];
                let dst = &mut inner.cached_bgra[y * (w as usize) * 4..][..(w as usize) * 4];
                dst.copy_from_slice(src);
              }
            }
            // Mark cache as valid after successful update
            inner
              .cache_valid
              .store(true, std::sync::atomic::Ordering::Release);
            tracing::trace!(operation = "dxgi_capture", step = "cache_updated");

            // Convert cached (packed) BGRA to RGBA using existing SIMD paths.
            let mut output_rgba = vec![0u8; cache_len];
            convert_bgra_to_rgba_optimized_direct(&inner.cached_bgra, &mut output_rgba);

            // Reset retry counter on successful capture
            DXGI_ACCESS_LOST_RETRY_COUNT.with(|c| c.set(0));
            tracing::debug!(
              operation = "dxgi_capture",
              step = "return_new_frame",
              source = "fresh_desktop",
              black
            );
            set_last_capture_method("Desktop Duplication");
            return Ok((w, h, output_rgba));
          },
          Err(e) => {
            let code = e.code().0 as u32;
            last_err = Some(e);

            if code == DXGI_ERROR_WAIT_TIMEOUT {
              if inner.cache_valid.load(std::sync::atomic::Ordering::Acquire) {
                tracing::debug!(
                  operation = "dxgi_capture",
                  step = "return_cached",
                  source = "acquire_timeout"
                );
                // Static desktop: no new frame; return cached.
                let rgba_len = (w * h * 4) as usize;
                let mut output_rgba = vec![0u8; rgba_len];
                convert_bgra_to_rgba_optimized_direct(&inner.cached_bgra, &mut output_rgba);
                set_last_capture_method("Desktop Duplication");
                return Ok((w, h, output_rgba));
              }
              // Do not repeat the initial wait for every retry; surface the timeout.
              tracing::warn!(
                operation = "dxgi_capture",
                step = "acquire_initial_timeout",
                wait_budget_ms = SCREENSHOT_FRAME_WAIT_BUDGET.as_millis() as u64,
                "No desktop image within screenshot wait budget; caller may use GDI"
              );
              break;
            }

            if code == DXGI_ERROR_WAS_STILL_DRAWING {
              // GPU is still rendering; retry quickly with 0 ms so we don't hold a
              // long timeout from the initial-frame path.
              acquire_timeout = ACQUIRE_TIMEOUT_ZERO_MS;
              continue;
            }

            if code == DXGI_ERROR_DEVICE_REMOVED {
              tracing::warn!(
                operation = "dxgi_capture",
                step = "acquire",
                code = format!("0x{:08X}", code),
                "DXGI_ERROR_DEVICE_REMOVED; will recreate pipeline and retry"
              );
              // End borrow of self from inner before recreate_if_needed.
              {
                let _ = inner;
              }
              self.recreate_if_needed();
              continue 'outer;
            }

            if code == DXGI_ERROR_ACCESS_LOST {
              // Escalating fallback strategy for monitor disconnection
              let retry_count = DXGI_ACCESS_LOST_RETRY_COUNT.with(|c| {
                let count = c.get();
                c.set(count + 1);
                count
              });

              tracing::warn!(
                operation = "dxgi_capture",
                step = "acquire",
                code = format!("0x{:08X}", code),
                retry_count = retry_count,
                "DXGI_ERROR_ACCESS_LOST - monitor disconnected or graphics state changed"
              );

              // End borrow before modifying self
              {
                let _ = inner;
              }

              match retry_count {
                0..=1 => {
                  // Retry 0-1: Reinitialize DXGI with same monitor (transient error)
                  tracing::info!(
                    operation = "dxgi_capture",
                    retry_count = retry_count,
                    "Reinitializing DXGI with same monitor"
                  );
                  self.recreate_if_needed();
                  continue 'outer;
                },
                2 => {
                  // Retry 2: Force DXGI re-init for the same monitor. The handle is
                  // stale (monitor was unplugged/reconnected); recreating it may recover
                  // if the display came back. Do NOT mutate target_x/y/w/h here — that
                  // would permanently redirect this cache entry to the primary monitor,
                  // corrupting all subsequent captures for the original monitor key.
                  tracing::warn!(
                    operation = "dxgi_capture",
                    retry_count = retry_count,
                    "ACCESS_LOST retry 2 — reinitializing DXGI for same monitor"
                  );
                  self.recreate_if_needed();
                  continue 'outer;
                },
                3..=4 => {
                  // Retry 3-4: Return error to trigger GDI fallback at caller level
                  tracing::error!(
                    operation = "dxgi_capture",
                    retry_count = retry_count,
                    "DXGI repeatedly failed, caller should fallback to GDI"
                  );
                  DXGI_ACCESS_LOST_RETRY_COUNT.with(|c| c.set(0)); // Reset for next capture
                  return Err(anyhow::anyhow!(
                    "DXGI_ERROR_ACCESS_LOST persists after retry and monitor fallback. \
                     Caller should use GDI fallback."
                  ));
                },
                _ => {
                  // Retry 5+: Give up completely
                  DXGI_ACCESS_LOST_RETRY_COUNT.with(|c| c.set(0)); // Reset for next attempt
                  return Err(anyhow::anyhow!(
                    "Monitor disconnected and all fallback strategies exhausted. \
                     Please reconnect display or restart application."
                  ));
                },
              }
            }

            // Other errors: surface and do not retry.
            if let Some(ref err) = last_err {
              let h = error_to_hresult(err);
              tracing::error!(
                  operation = "dxgi_capture",
                  step = "acquire",
                  hresult = %format_hresult(h),
                  "AcquireNextFrame failed"
              );
              return Err(anyhow::anyhow!("{}", err))
                .context(format!("AcquireNextFrame failed ({})", format_hresult(h)));
            }
          },
        }
      }

      if let Some(e) = last_err {
        let h = error_to_hresult(&e);
        return Err(anyhow::anyhow!("{}", e)).context(format!(
          "AcquireNextFrame failed (timeout or retries exhausted) ({})",
          format_hresult(h)
        ));
      }
    }

    anyhow::bail!("AcquireNextFrame failed after recreate retry")
  }

  /// Capture a frame for video recording. Uses 10ms timeout and returns None on timeout.
  /// Returns (width, height, RGBA pixels) or None if no new frame available.
  /// This method is optimized for continuous video capture with low latency.
  pub fn capture_for_video(&mut self) -> Result<Option<(u32, u32, Vec<u8>)>> {
    const ACQUIRE_TIMEOUT_MS: u32 = 10;
    const WAS_STILL_DRAWING_RETRIES: usize = 3;

    'outer: for _attempt in 0..2 {
      let inner = self.ensure_initialized(2000)?;
      let (w, h) = (inner.width, inner.height);
      let had_cache = inner.cache_valid.load(std::sync::atomic::Ordering::Acquire);

      let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
      let mut desktop_resource: Option<IDXGIResource> = None;

      let mut last_err = None::<windows::core::Error>;

      for _ in 0..=WAS_STILL_DRAWING_RETRIES {
        let hr = unsafe {
          inner.duplication.AcquireNextFrame(
            ACQUIRE_TIMEOUT_MS,
            &mut frame_info,
            &mut desktop_resource,
          )
        };

        match hr {
          Ok(()) => {
            let _frame_guard = FrameGuard {
              duplication: inner.duplication.clone(),
            };

            // Desktop image unchanged on this driver — skip texture read to avoid
            // stale/zeroed data producing a black frame. FrameGuard releases on drop.
            if frame_info.TotalMetadataBufferSize == 0 {
              return Ok(None);
            }

            let resource = desktop_resource
              .ok_or_else(|| anyhow::anyhow!("Desktop resource is None after AcquireNextFrame"))?;

            let texture: ID3D11Texture2D = resource
              .cast()
              .inspect_err(|e| {
                let h = error_to_hresult(e);
                tracing::error!(
                    operation = "dxgi_capture_video",
                    step = "cast_texture",
                    hresult = %format_hresult(h),
                    "Failed to cast to ID3D11Texture2D"
                );
              })
              .context("Failed to cast to ID3D11Texture2D")?;

            // Ensure staging texture exists (recreated lazily if released for memory savings)
            let staging = inner.ensure_staging()?.clone();
            unsafe { inner.context.CopyResource(&staging, &texture) };

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
              inner
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            }
            .inspect_err(|e| {
              let h = error_to_hresult(e);
              tracing::error!(
                  operation = "dxgi_capture_video",
                  step = "map_texture",
                  hresult = %format_hresult(h),
                  "Failed to map staging texture"
              );
            })
            .context("Failed to map staging texture")?;

            let _map_guard = MapGuard {
              context: inner.context.clone(),
              staging: staging.clone(),
            };

            let row_pitch = mapped.RowPitch as usize;
            let data_len = row_pitch * (h as usize);
            let expected_row_bytes = (w as usize) * 4;

            // Runtime validation: row_pitch must be >= width*4
            if row_pitch < expected_row_bytes {
              return Err(
                anyhow::anyhow!(
                  "Invalid RowPitch {} < {} (width*4) - corrupt or invalid frame data",
                  row_pitch,
                  expected_row_bytes
                )
                .context("Failed to validate Desktop Duplication row pitch for video"),
              );
            }

            // Log stride information for debugging
            if row_pitch == expected_row_bytes {
              tracing::trace!(
                operation = "dxgi_capture_video",
                step = "map_ok",
                row_pitch,
                "RowPitch equals width*4 (tightly packed, optimal)"
              );
            } else {
              tracing::warn!(
                operation = "dxgi_capture_video",
                step = "map_ok",
                row_pitch,
                expected_row_bytes,
                padding = row_pitch - expected_row_bytes,
                "RowPitch > width*4 (padding detected, will be handled correctly)"
              );
            }

            // SAFETY: Map succeeded; pData is valid for data_len bytes until Unmap.
            let ptr = mapped.pData;
            if ptr.is_null() {
              return Err(
                anyhow::anyhow!("Map returned null pData").context("Failed to map staging texture"),
              );
            }
            let bgra_slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, data_len) };

            if !had_cache
              && frame_info.TotalMetadataBufferSize == 0
              && is_suspicious_first_uncached_black_frame(bgra_slice, row_pitch, w, h)
            {
              tracing::warn!(
                operation = "dxgi_capture_video",
                step = "reject_suspicious_black_frame",
                "Rejecting suspicious uncached all-black DXGI frame; recreating pipeline"
              );
              {
                let _ = inner;
              }
              self.recreate_if_needed();
              continue 'outer;
            }

            // Invalidate the screenshot cache so screenshots during recording get fresh frames
            // Use atomic flag instead of clearing vector to avoid race condition
            inner
              .cache_valid
              .store(false, std::sync::atomic::Ordering::Release);

            // Convert BGRA to RGBA. The caller owns the result; no persistent
            // output_rgba field is kept in DxgiCaptureInner, saving ~8MB of
            // idle RAM per monitor.
            let rgba_len = (w * h * 4) as usize;
            let mut output_rgba = vec![0u8; rgba_len];

            convert_bgra_to_rgba_video(bgra_slice, &mut output_rgba, row_pitch, w, h);

            // Reset retry counter on successful capture
            DXGI_ACCESS_LOST_RETRY_COUNT.with(|c| c.set(0));

            return Ok(Some((w, h, output_rgba)));
          },
          Err(e) => {
            let code = e.code().0 as u32;
            last_err = Some(e);

            if code == DXGI_ERROR_WAIT_TIMEOUT {
              // No new frame available (desktop static) - return None for video
              return Ok(None);
            }

            if code == DXGI_ERROR_WAS_STILL_DRAWING {
              // Retry AcquireNextFrame
              continue;
            }

            if code == DXGI_ERROR_DEVICE_REMOVED {
              tracing::warn!(
                operation = "dxgi_capture_video",
                step = "acquire",
                code = format!("0x{:08X}", code),
                "DXGI_ERROR_DEVICE_REMOVED; will recreate pipeline and retry"
              );
              {
                let _ = inner;
              }
              self.recreate_if_needed();
              continue 'outer;
            }

            if code == DXGI_ERROR_ACCESS_LOST {
              // Escalating fallback strategy for monitor disconnection during video recording
              let retry_count = DXGI_ACCESS_LOST_RETRY_COUNT.with(|c| {
                let count = c.get();
                c.set(count + 1);
                count
              });

              tracing::warn!(
                operation = "dxgi_capture_video",
                step = "acquire",
                code = format!("0x{:08X}", code),
                retry_count = retry_count,
                "DXGI_ERROR_ACCESS_LOST - monitor disconnected or graphics state changed"
              );

              // End borrow before modifying self
              {
                let _ = inner;
              }

              match retry_count {
                0..=1 => {
                  // Retry 0-1: Reinitialize DXGI with same monitor (transient error)
                  tracing::info!(
                    operation = "dxgi_capture_video",
                    retry_count = retry_count,
                    "Reinitializing DXGI with same monitor"
                  );
                  self.recreate_if_needed();
                  continue 'outer;
                },
                2 => {
                  // Retry 2: Force DXGI re-init for the same monitor. Do NOT mutate
                  // target_x/y/w/h — that would permanently redirect this cache entry
                  // to the primary monitor, corrupting all subsequent captures for the
                  // original monitor key and producing black/wrong-monitor frames.
                  tracing::warn!(
                    operation = "dxgi_capture_video",
                    retry_count = retry_count,
                    "ACCESS_LOST retry 2 — reinitializing DXGI for same monitor"
                  );
                  self.recreate_if_needed();
                  continue 'outer;
                },
                3..=4 => {
                  // Retry 3-4: Return None (no frame) instead of error to keep recording alive
                  tracing::error!(
                    operation = "dxgi_capture_video",
                    retry_count = retry_count,
                    "DXGI repeatedly failed, returning None to continue recording"
                  );
                  DXGI_ACCESS_LOST_RETRY_COUNT.with(|c| c.set(0)); // Reset for next capture
                  return Ok(None); // Return no frame instead of killing recording
                },
                _ => {
                  // Retry 5+: Give up, return error to stop recording
                  DXGI_ACCESS_LOST_RETRY_COUNT.with(|c| c.set(0)); // Reset for next attempt
                  return Err(anyhow::anyhow!(
                    "Monitor disconnected and all fallback strategies exhausted. \
                     Video recording cannot continue."
                  ));
                },
              }
            }

            // Other errors: return error
            if let Some(ref err) = last_err {
              let h = error_to_hresult(err);
              tracing::error!(
                  operation = "dxgi_capture_video",
                  step = "acquire",
                  hresult = %format_hresult(h),
                  "AcquireNextFrame failed"
              );
              return Err(anyhow::anyhow!("{}", err))
                .context(format!("AcquireNextFrame failed ({})", format_hresult(h)));
            }
          },
        }
      }

      if let Some(e) = last_err {
        let h = error_to_hresult(&e);
        return Err(anyhow::anyhow!("{}", e)).context(format!(
          "AcquireNextFrame failed (retries exhausted) ({})",
          format_hresult(h)
        ));
      }
    }

    anyhow::bail!("AcquireNextFrame failed after recreate retry")
  }
}

/// Convert BGRA to RGBA, handling row pitch padding.
fn convert_bgra_to_rgba_video(bgra: &[u8], rgba: &mut [u8], row_pitch: usize, w: u32, h: u32) {
  let expected_row_bytes = (w as usize) * 4;
  if row_pitch == expected_row_bytes {
    convert_bgra_to_rgba_optimized_direct(bgra, rgba);
  } else {
    for y in 0..(h as usize) {
      let src_row = &bgra[y * row_pitch..][..expected_row_bytes];
      let dst_row = &mut rgba[y * expected_row_bytes..][..expected_row_bytes];
      convert_bgra_to_rgba_optimized_direct(src_row, dst_row);
    }
  }
}

/// Heuristic guard for a known DXGI quirk where the first uncached frame after
/// a long-lived pipeline hiccup can map as an all-black surface.
fn is_suspicious_first_uncached_black_frame(bgra: &[u8], row_pitch: usize, w: u32, h: u32) -> bool {
  let expected_row_bytes = (w as usize) * 4;
  if row_pitch < expected_row_bytes || bgra.len() < row_pitch.saturating_mul(h as usize) {
    return false;
  }

  for y in 0..(h as usize) {
    let row = &bgra[y * row_pitch..][..expected_row_bytes];
    for pixel in row.as_chunks::<4>().0 {
      if pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0 {
        return false;
      }
    }
  }

  true
}

fn validated_warmup_cache(
  info: &DXGI_OUTDUPL_FRAME_INFO,
  bgra: &[u8],
  row_pitch: usize,
  width: u32,
  height: u32,
) -> Result<Option<Vec<u8>>> {
  if !desktop_image_updated(info) {
    return Ok(None);
  }
  let row_bytes = width as usize * 4;
  let data_len = row_pitch
    .checked_mul(height as usize)
    .ok_or_else(|| anyhow::anyhow!("Warmup texture size overflow"))?;
  if width == 0 || height == 0 || row_pitch < row_bytes || bgra.len() < data_len {
    anyhow::bail!("Invalid warmup texture dimensions or row pitch");
  }
  let black = is_suspicious_first_uncached_black_frame(bgra, row_pitch, width, height);
  tracing::debug!(operation = "dxgi_capture", step = "warmup_pixels", black);
  if black {
    tracing::warn!(
      operation = "dxgi_capture",
      step = "reject_warmup_black",
      "Black warmup desktop image rejected; cache remains empty"
    );
    return Ok(None);
  }
  let mut cache = vec![0; row_bytes * height as usize];
  for (source, target) in bgra
    .chunks_exact(row_pitch)
    .zip(cache.chunks_exact_mut(row_bytes))
  {
    target.copy_from_slice(&source[..row_bytes]);
  }
  Ok(Some(cache))
}

/// Public entry: capture from the monitor under the cursor.
/// Uses per-monitor DxgiCapture instances so each monitor has its own IDXGIOutputDuplication.
/// Returns (width, height, RGBA). Convert to RGBA only at this boundary.
pub unsafe fn try_desktop_duplication() -> Result<(u32, u32, Vec<u8>)> {
  // Get monitor under cursor, fall back to primary
  let monitor = crate::monitors::monitor_under_cursor()
    .or_else(crate::monitors::get_primary_monitor)
    .ok_or_else(|| anyhow::anyhow!("No monitor found"))?;

  // SAFETY: We're in an unsafe fn, delegating to another unsafe fn
  unsafe { try_desktop_duplication_for_monitor(&monitor) }
}

/// Capture from a specific monitor.
/// Returns (width, height, RGBA).
pub unsafe fn try_desktop_duplication_for_monitor(
  monitor: &MonitorInfo,
) -> Result<(u32, u32, Vec<u8>)> {
  let captures = DXGI_CAPTURES.get_or_init(|| Mutex::new(HashMap::new()));
  let mut map = match captures.lock() {
    Ok(guard) => guard,
    Err(poisoned) => {
      tracing::warn!(
        operation = "try_desktop_duplication_for_monitor",
        "DXGI captures mutex poisoned, recovering"
      );
      poisoned.into_inner()
    },
  };

  let key = monitor_key(monitor.x, monitor.y, monitor.width, monitor.height);

  // Get or create capture for this monitor
  let capture = map.entry(key).or_insert_with(|| {
    tracing::debug!(
      operation = "try_desktop_duplication_for_monitor",
      monitor_id = %monitor.id,
      x = monitor.x,
      y = monitor.y,
      width = monitor.width,
      height = monitor.height,
      "Creating new DxgiCapture for monitor"
    );
    DxgiCapture::new_for_monitor(monitor.x, monitor.y, monitor.width, monitor.height)
  });

  capture.capture()
}

/// Capture a frame for video recording from a specific monitor.
/// Returns None if no new frame is available (desktop static), Some((width, height, RGBA)) otherwise.
/// Uses 10ms timeout optimized for video capture latency.
pub unsafe fn capture_frame_for_video_from_monitor(
  monitor: &MonitorInfo,
) -> Result<Option<(u32, u32, Vec<u8>)>> {
  let captures = DXGI_CAPTURES.get_or_init(|| Mutex::new(HashMap::new()));
  let mut map = match captures.lock() {
    Ok(guard) => guard,
    Err(poisoned) => {
      tracing::warn!(
        operation = "capture_frame_for_video_from_monitor",
        "DXGI captures mutex poisoned, recovering"
      );
      poisoned.into_inner()
    },
  };

  let key = monitor_key(monitor.x, monitor.y, monitor.width, monitor.height);

  // Get or create capture for this monitor
  let capture = map.entry(key).or_insert_with(|| {
    tracing::debug!(
      operation = "capture_frame_for_video_from_monitor",
      monitor_id = %monitor.id,
      x = monitor.x,
      y = monitor.y,
      width = monitor.width,
      height = monitor.height,
      "Creating new DxgiCapture for monitor"
    );
    DxgiCapture::new_for_monitor(monitor.x, monitor.y, monitor.width, monitor.height)
  });

  capture.capture_for_video()
}

/// One-time, background warm-up: initializes the DXGI pipeline for EVERY connected
/// monitor so the first user-triggered screenshot is fast for all of them.
/// The warmup cache retains only a validated desktop update, never a pointer-only event.
/// Best-effort and idempotent; failures are logged without blocking capture.
pub(crate) fn warm_up_desktop_duplication() {
  WARMUP_ONCE.call_once(|| {
    if let Err(error) = std::thread::Builder::new()
      .name("flinch-dxgi-warmup".into())
      .spawn(|| {
        let captures = DXGI_CAPTURES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut map = match captures.lock() {
          Ok(guard) => guard,
          Err(poisoned) => poisoned.into_inner(),
        };
        let monitors = crate::monitors::list_monitors();
        for monitor in &monitors {
          let key = monitor_key(monitor.x, monitor.y, monitor.width, monitor.height);
          let capture = map.entry(key.clone()).or_insert_with(|| {
            DxgiCapture::new_for_monitor(monitor.x, monitor.y, monitor.width, monitor.height)
          });
          if let Err(error) = capture.warm_up() {
            tracing::debug!(
              operation = "dxgi_warmup",
              monitor = %key,
              error = %error,
              "DXGI warm-up failed for monitor"
            );
          }
        }
        drop(map);
        release_dxgi_idle_memory();
      })
    {
      tracing::debug!(
        operation = "dxgi_warmup",
        error = %error,
        "Failed to spawn DXGI warm-up thread"
      );
    }
  });
}

#[cfg(test)]
mod tests {
  use super::is_suspicious_first_uncached_black_frame;

  #[test]
  fn warmup_pointer_only_black_frame_cannot_poison_cache() {
    let info = super::DXGI_OUTDUPL_FRAME_INFO::default();
    let black = [0, 0, 0, 255];
    assert!(
      super::validated_warmup_cache(&info, &black, 4, 1, 1)
        .expect("pointer event")
        .is_none()
    );
    // Even a pointer-only resource containing nonzero bytes is not a desktop update.
    assert!(
      super::validated_warmup_cache(&info, &[1, 2, 3, 255], 4, 1, 1)
        .expect("pointer event")
        .is_none()
    );
  }

  #[test]
  fn warmup_rejects_black_desktop_update_and_packs_visible_rows() {
    let info = super::DXGI_OUTDUPL_FRAME_INFO {
      LastPresentTime: 123,
      ..Default::default()
    };
    // Metadata size is deliberately zero: presentation time owns update detection.
    assert!(super::desktop_image_updated(&info));
    assert!(
      super::validated_warmup_cache(&info, &[0, 0, 0, 128], 4, 1, 1)
        .expect("black update")
        .is_none()
    );
    let padded = [1, 2, 3, 255, 99, 99, 99, 99, 4, 5, 6, 255, 99, 99, 99, 99];
    assert_eq!(
      super::validated_warmup_cache(&info, &padded, 8, 1, 2).expect("visible update"),
      Some(vec![1, 2, 3, 255, 4, 5, 6, 255])
    );
    assert!(super::validated_warmup_cache(&info, &padded, 3, 1, 2).is_err());
    assert!(super::validated_warmup_cache(&info, &padded[..4], 8, 1, 2).is_err());
  }

  #[test]
  fn screenshot_wait_budget_is_shared_and_saturates() {
    use std::time::Duration;
    assert_eq!(super::remaining_frame_wait_ms(Duration::ZERO), 100);
    assert_eq!(
      super::remaining_frame_wait_ms(Duration::from_millis(40)),
      60
    );
    assert_eq!(super::remaining_frame_wait_ms(Duration::from_millis(99)), 1);
    assert_eq!(
      super::remaining_frame_wait_ms(Duration::from_millis(100)),
      0
    );
    assert_eq!(super::remaining_frame_wait_ms(Duration::MAX), 0);
  }

  #[test]
  fn topology_tracks_enumeration_independently_of_pipeline_reset() {
    let first = super::HashSet::from(["0,0,2560x1440".to_owned()]);
    let mut snapshot = None;
    assert!(!super::update_topology(&mut snapshot, first.clone()));
    // A pipeline reset/lazy creation does not change the enumeration snapshot.
    assert!(!super::update_topology(&mut snapshot, first.clone()));
    let extended =
      super::HashSet::from(["0,0,2560x1440".to_owned(), "2560,0,1920x1080".to_owned()]);
    assert!(super::update_topology(&mut snapshot, extended.clone()));
    assert!(!super::update_topology(&mut snapshot, extended));
    assert!(super::update_topology(&mut snapshot, first));
    assert!(super::update_topology(&mut snapshot, super::HashSet::new()));
  }

  #[test]
  #[ignore = "desktop: captures the real screen; explicit session authorization required"]
  fn desktop_dxgi_cold_repeat_and_trimmed_capture() {
    let _ = tracing_subscriber::fmt()
      .with_max_level(tracing::Level::DEBUG)
      .with_test_writer()
      .try_init();
    let monitors = crate::monitors::list_monitors();
    assert!(!monitors.is_empty(), "desktop probe requires a monitor");
    super::reset_dxgi_captures();
    for monitor in &monitors {
      for stage in ["cold", "repeat", "trimmed"] {
        if stage == "trimmed" {
          super::release_dxgi_idle_memory();
          assert!(
            crate::screenshot::platform::windows::dxgi::memory_diagnostics().cached_bgra_bytes > 0,
            "an unchanged desktop needs the cache after trimming"
          );
        }
        let start = std::time::Instant::now();
        // SAFETY: This explicitly ignored desktop probe captures a currently enumerated monitor.
        let result = unsafe { super::try_desktop_duplication_for_monitor(monitor) };
        match result {
          Ok((w, h, pixels)) => {
            assert!(
              !super::super::is_effectively_black_rgba(&pixels),
              "raw DXGI must never return a black frame as success"
            );
            println!(
              "DXGI_PROBE stage={stage} elapsed_ms={:.3} width={w} height={h} black={}",
              start.elapsed().as_secs_f64() * 1000.0,
              super::super::is_effectively_black_rgba(&pixels)
            );
          },
          Err(error) => println!(
            "DXGI_PROBE stage={stage} elapsed_ms={:.3} error={error:#}",
            start.elapsed().as_secs_f64() * 1000.0
          ),
        }
      }
      super::reset_dxgi_captures();
      for stage in ["reliable_cold", "reliable_repeat", "reliable_trimmed"] {
        if stage == "reliable_trimmed" {
          super::release_dxgi_idle_memory();
        }
        let start = std::time::Instant::now();
        let (w, h, pixels) =
          super::super::capture_monitor_rgba_reliably(monitor).expect("reliable desktop capture");
        println!(
          "DXGI_PROBE stage={stage} elapsed_ms={:.3} width={w} height={h} black={}",
          start.elapsed().as_secs_f64() * 1000.0,
          super::super::is_effectively_black_rgba(&pixels)
        );
        assert!(
          !super::super::is_effectively_black_rgba(&pixels),
          "probe desktop must contain visible content"
        );
      }
    }
    super::reset_dxgi_captures();
  }

  #[test]
  fn a_cached_frame_is_never_accepted_without_waiting_for_a_newer_one() {
    assert_eq!(super::first_acquire_wait_ms(true, 100), 25);
    assert_eq!(super::first_acquire_wait_ms(true, 10), 10);
    assert_eq!(super::first_acquire_wait_ms(false, 100), 100);
  }

  #[test]
  fn suspicious_black_frame_detects_opaque_black() {
    let bgra = vec![0u8, 0, 0, 255, 0, 0, 0, 255];
    assert!(is_suspicious_first_uncached_black_frame(&bgra, 8, 2, 1));
  }

  #[test]
  fn suspicious_black_frame_detects_transparent_black() {
    let bgra = vec![0u8; 8];
    assert!(is_suspicious_first_uncached_black_frame(&bgra, 8, 2, 1));
  }

  #[test]
  fn suspicious_black_frame_rejects_non_black_pixels() {
    let bgra = vec![0u8, 0, 1, 255, 0, 0, 0, 255];
    assert!(!is_suspicious_first_uncached_black_frame(&bgra, 8, 2, 1));
  }

  #[test]
  fn suspicious_black_frame_ignores_padding_bytes() {
    let bgra = vec![0u8, 0, 0, 255, 0, 0, 0, 255, 12, 34, 56, 78];
    assert!(is_suspicious_first_uncached_black_frame(&bgra, 12, 2, 1));
  }
}
