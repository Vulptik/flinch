// src/screenshot/platform/windows/mod.rs
// Windows platform screenshot capture - modular implementation

use anyhow::Result;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

// Declare submodules
mod conversion;
mod dxgi;
mod gdi;
mod utils;
mod wgc;

// Re-export public APIs
pub use dxgi::{
  capture_frame_for_video_from_monitor, invalidate_stale_captures, release_dxgi_idle_memory,
  reset_dxgi_captures, try_desktop_duplication_for_monitor,
};
pub use gdi::capture_virtual_desktop_into;
pub use wgc::warm_up_wgc;

#[derive(Debug, Clone, serde::Serialize)]
pub struct CaptureMemoryDiagnostics {
  pub active_recording_backend: Option<String>,
  pub last_capture_method: Option<String>,
  pub fallback_screenshot_buffer_bytes: usize,
  pub dxgi_pipeline_count: usize,
  pub dxgi_staging_count: usize,
  pub dxgi_cached_bgra_bytes: usize,
  pub wgc_initialized: bool,
  pub wgc_pipeline_active: bool,
  pub wgc_reusable_buffer_bytes: usize,
}

/// Which capture backend an active recording is using.
/// Stored as AtomicU8 for cheap, lock-free reads from screenshot code.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaptureBackendKind {
  /// Desktop Duplication API (DXGI). Default for video.
  Dxgi = 1,
  /// Windows Graphics Capture. Fallback when DXGI unavailable.
  Wgc = 2,
  /// GDI BitBlt. Last-resort fallback (RDP, headless, no GPU).
  Gdi = 3,
}

/// Tracks which backend the currently active recording session is using.
/// `0` means no active recording. Written once on recording start, cleared on stop.
static ACTIVE_RECORDING_BACKEND: AtomicU8 = AtomicU8::new(0);
static FALLBACK_SCREENSHOT_BUFFER: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();

/// Record which backend the active recording session is using.
/// Pass `None` when recording stops.
#[cfg(test)]
pub fn set_active_recording_backend(b: Option<CaptureBackendKind>) {
  let v = b.map(|k| k as u8).unwrap_or(0);
  ACTIVE_RECORDING_BACKEND.store(v, Ordering::Release);
}

/// Return the backend currently used by an active recording, or `None` if idle.
#[allow(dead_code)]
pub fn get_active_recording_backend() -> Option<CaptureBackendKind> {
  match ACTIVE_RECORDING_BACKEND.load(Ordering::Acquire) {
    0 => None,
    1 => Some(CaptureBackendKind::Dxgi),
    2 => Some(CaptureBackendKind::Wgc),
    3 => Some(CaptureBackendKind::Gdi),
    _ => None,
  }
}

// Thread-local storage to track the last capture method used
thread_local! {
    static LAST_CAPTURE_METHOD: std::cell::Cell<Option<String>> = const { std::cell::Cell::new(None) };
}

/// Get the last capture method that was used
#[allow(dead_code)]
pub fn get_last_capture_method() -> Option<String> {
  LAST_CAPTURE_METHOD.with(|cell| cell.take())
}

/// Set the last capture method
pub(crate) fn set_last_capture_method(method: &str) {
  LAST_CAPTURE_METHOD.with(|cell| cell.set(Some(method.to_string())));
}

/// Warms up the DXGI pipeline with one throwaway capture in the background. Idempotent.
pub fn warm_up_desktop_duplication() {
  dxgi::warm_up_desktop_duplication();
}

/// Capture a GDI frame for native backend validation.
#[cfg(test)]
fn try_gdi_capture_for_video() -> Result<(u32, u32, Vec<u8>)> {
  init_thread_pool();
  // SAFETY: GDI resources are owned and released by capture_with_dib.
  unsafe { gdi::capture_with_dib(init_thread_pool) }
}

/// Initialize Rayon thread pool with optimal settings for minimum latency
fn init_thread_pool() {
  static INIT: OnceLock<()> = OnceLock::new();
  INIT.get_or_init(|| {
    // BGRA→RGBA is memory-bandwidth bound, not CPU bound. Beyond ~6 threads
    // the memory bus is saturated and more threads just waste stack RAM.
    let num_threads = std::thread::available_parallelism()
      .map_or(1, |n| n.get())
      .min(6);

    rayon::ThreadPoolBuilder::new()
      .num_threads(num_threads)
      .stack_size(512 * 1024) // 512KB stack per thread (BGRA→RGBA is shallow byte shuffling)
      .thread_name(|i| format!("flinch-simd-{}", i))
      .build_global()
      .ok();

    tracing::debug!(
      operation = "thread_pool_init",
      num_threads = num_threads,
      "Initialized Rayon thread pool"
    );
  });
}

/// Capture the currently active window using GDI
/// Falls back to fullscreen if no valid active window is found
pub fn capture_active_window_rgba() -> Result<(u32, u32, Vec<u8>)> {
  // Pre-warm thread pool on first call
  init_thread_pool();

  unsafe {
    let result = gdi::capture_active_window(init_thread_pool)?;
    set_last_capture_method("Windows GDI (Active Window)");
    Ok(result)
  }
}

/// Capture the entire virtual desktop (all monitors combined).
/// Returns (vd_x, vd_y, width, height, rgba) where (vd_x, vd_y) is the virtual desktop origin.
pub fn capture_virtual_desktop_rgba() -> Result<(i32, i32, u32, u32, Vec<u8>)> {
  use crate::monitors::{get_virtual_desktop_bounds, list_monitors};

  init_thread_pool();

  // Prune DXGI pipelines for monitors that no longer exist (hot-plug support).
  // This ensures we create fresh pipelines with warmup for newly connected monitors
  // instead of reusing stale ones that would produce black frames.
  dxgi::invalidate_stale_captures();

  let (vd_x, vd_y, vd_width, vd_height) = get_virtual_desktop_bounds();
  let monitors = list_monitors();

  capture_virtual_desktop_with(
    (vd_x, vd_y, vd_width, vd_height),
    &monitors,
    capture_monitor_rgba_reliably,
  )
}

fn capture_virtual_desktop_with(
  bounds: (i32, i32, u32, u32),
  monitors: &[crate::monitors::MonitorInfo],
  mut capture: impl FnMut(&crate::monitors::MonitorInfo) -> Result<(u32, u32, Vec<u8>)>,
) -> Result<(i32, i32, u32, u32, Vec<u8>)> {
  let (vd_x, vd_y, vd_width, vd_height) = bounds;

  tracing::debug!(
    operation = "capture_virtual_desktop",
    vd_x = vd_x,
    vd_y = vd_y,
    vd_width = vd_width,
    vd_height = vd_height,
    monitor_count = monitors.len(),
    "Capturing virtual desktop"
  );

  // Reuse the last returned screenshot buffer when possible so repeated
  // open/cancel loops do not keep committing fresh full-screen allocations.
  anyhow::ensure!(
    vd_width > 0 && vd_height > 0 && !monitors.is_empty(),
    "Virtual desktop has no captureable monitor area"
  );
  let output_len = (vd_width as usize)
    .checked_mul(vd_height as usize)
    .and_then(|n| n.checked_mul(4))
    .ok_or_else(|| anyhow::anyhow!("Virtual desktop buffer dimensions overflow"))?;
  let mut output = take_reusable_screenshot_buffer(output_len)?;

  // Capture each monitor and blit to the correct position
  for monitor in monitors {
    let (m_width, m_height, rgba) = capture(monitor)
      .map_err(|e| e.context(format!("Failed to capture monitor '{}'", monitor.id)))?;

    anyhow::ensure!(
      (m_width, m_height) == (monitor.width, monitor.height),
      "Monitor '{}' changed dimensions during capture",
      monitor.id
    );
    let dst_x = usize::try_from(i64::from(monitor.x) - i64::from(vd_x))
      .map_err(|_| anyhow::anyhow!("Monitor '{}' moved outside the virtual desktop", monitor.id))?;
    let dst_y = usize::try_from(i64::from(monitor.y) - i64::from(vd_y))
      .map_err(|_| anyhow::anyhow!("Monitor '{}' moved outside the virtual desktop", monitor.id))?;

    tracing::trace!(
      operation = "capture_virtual_desktop",
      monitor_id = %monitor.id,
      src_width = m_width,
      src_height = m_height,
      dst_x = dst_x,
      dst_y = dst_y,
      "Blitting monitor to virtual desktop"
    );

    blit_monitor_rgba(
      &mut output,
      vd_width,
      dst_x,
      dst_y,
      m_width,
      m_height,
      &rgba,
    )?;
  }

  set_last_capture_method("Virtual Desktop (Multi-Monitor)");
  Ok((vd_x, vd_y, vd_width, vd_height, output))
}

fn take_reusable_screenshot_buffer(need: usize) -> Result<Vec<u8>> {
  let pool = FALLBACK_SCREENSHOT_BUFFER.get_or_init(|| Mutex::new(None));
  let mut buf = pool
    .lock()
    .map_err(|error| anyhow::anyhow!("Screenshot reuse buffer poisoned: {error}"))?
    .take()
    .unwrap_or_default();
  if buf.capacity() < need {
    buf
      .try_reserve_exact(need.saturating_sub(buf.len()))
      .map_err(|error| anyhow::anyhow!("Cannot allocate virtual desktop buffer: {error}"))?;
  }

  buf.resize(need, 0);
  buf.fill(0);
  Ok(buf)
}

/// Return a screenshot pixel buffer to the reuse pool.
///
/// WGC keeps its own internal reuse slot, so prefer that when it is active.
/// Otherwise retain one generic full-screen RGBA buffer for DXGI/GDI-backed
/// screenshot overlays to avoid repeated large heap commits on cancel loops.
pub fn return_screenshot_buffer(buf: Vec<u8>) {
  if wgc_capture_initialized() {
    wgc::return_screenshot_buffer(buf);
    return;
  }

  if let Some(pool) = FALLBACK_SCREENSHOT_BUFFER.get()
    && let Ok(mut guard) = pool.lock()
  {
    *guard = Some(buf);
    return;
  }

  let pool = FALLBACK_SCREENSHOT_BUFFER.get_or_init(|| Mutex::new(None));
  if let Ok(mut guard) = pool.lock() {
    *guard = Some(buf);
  }
}

/// Drop any cached non-WGC screenshot buffer so the caller can force a full
/// allocator purge after screenshot flows complete.
pub fn clear_cached_screenshot_buffer() {
  if let Some(pool) = FALLBACK_SCREENSHOT_BUFFER.get()
    && let Ok(mut guard) = pool.lock()
  {
    let _ = guard.take();
  }
  release_capture_idle_memory();
}

/// Release idle capture resources that are safe to reconstruct on demand.
///
/// Keep DXGI duplication pipelines alive between screenshot overlays so repeated
/// opens do not pay the full cold-start / warmup cost on every capture attempt.
/// We still trim staging textures and cached frame buffers to bound idle memory.
/// WGC tears down its pipeline only when it is not the active recording backend.
pub fn release_capture_idle_memory() {
  let before = capture_memory_diagnostics();
  wgc::release_wgc_idle_memory();
  release_dxgi_idle_memory();
  let after = capture_memory_diagnostics();

  tracing::info!(
    operation = "capture_idle_memory",
    active_backend = before.active_recording_backend.as_deref().unwrap_or("none"),
    last_method = before.last_capture_method.as_deref().unwrap_or("unknown"),
    before_fallback_bytes = before.fallback_screenshot_buffer_bytes,
    before_dxgi_pipelines = before.dxgi_pipeline_count,
    before_dxgi_staging = before.dxgi_staging_count,
    before_dxgi_cache_bytes = before.dxgi_cached_bgra_bytes,
    before_wgc_initialized = before.wgc_initialized,
    before_wgc_pipeline = before.wgc_pipeline_active,
    before_wgc_buffer_bytes = before.wgc_reusable_buffer_bytes,
    after_fallback_bytes = after.fallback_screenshot_buffer_bytes,
    after_dxgi_pipelines = after.dxgi_pipeline_count,
    after_dxgi_staging = after.dxgi_staging_count,
    after_dxgi_cache_bytes = after.dxgi_cached_bgra_bytes,
    after_wgc_initialized = after.wgc_initialized,
    after_wgc_pipeline = after.wgc_pipeline_active,
    after_wgc_buffer_bytes = after.wgc_reusable_buffer_bytes,
    "Capture idle-memory cleanup snapshot"
  );
}

pub fn capture_memory_diagnostics() -> CaptureMemoryDiagnostics {
  let fallback_screenshot_buffer_bytes = FALLBACK_SCREENSHOT_BUFFER
    .get()
    .and_then(|pool| pool.lock().ok())
    .and_then(|guard| guard.as_ref().map(|buf| buf.capacity()))
    .unwrap_or(0);
  let dxgi = dxgi::memory_diagnostics();
  let wgc = wgc::memory_diagnostics();

  CaptureMemoryDiagnostics {
    active_recording_backend: get_active_recording_backend().map(|backend| match backend {
      CaptureBackendKind::Dxgi => "dxgi".to_string(),
      CaptureBackendKind::Wgc => "wgc".to_string(),
      CaptureBackendKind::Gdi => "gdi".to_string(),
    }),
    last_capture_method: get_last_capture_method(),
    fallback_screenshot_buffer_bytes,
    dxgi_pipeline_count: dxgi.pipeline_count,
    dxgi_staging_count: dxgi.staging_count,
    dxgi_cached_bgra_bytes: dxgi.cached_bgra_bytes,
    wgc_initialized: wgc.initialized,
    wgc_pipeline_active: wgc.pipeline_active,
    wgc_reusable_buffer_bytes: wgc.reusable_buffer_bytes,
  }
}

fn wgc_capture_initialized() -> bool {
  static WGC_CAPTURE_NAME: &str = "WGC_CAPTURE";
  let _ = WGC_CAPTURE_NAME;
  crate::screenshot::platform::windows::wgc::is_initialized()
}

fn capture_monitor_rgba_reliably(
  monitor: &crate::monitors::MonitorInfo,
) -> Result<(u32, u32, Vec<u8>)> {
  capture_monitor_with(
    monitor,
    // SAFETY: native capture and recovery run on their backend owner threads.
    || unsafe { dxgi::try_desktop_duplication_for_monitor(monitor) },
    || {
      let mut pixels = Vec::new();
      gdi::capture_virtual_desktop_into(
        &mut pixels,
        monitor.x,
        monitor.y,
        monitor.width,
        monitor.height,
      )?;
      Ok(pixels)
    },
    reset_dxgi_captures,
  )
}

fn capture_monitor_with(
  monitor: &crate::monitors::MonitorInfo,
  mut dxgi: impl FnMut() -> Result<(u32, u32, Vec<u8>)>,
  mut gdi: impl FnMut() -> Result<Vec<u8>>,
  mut reset: impl FnMut(),
) -> Result<(u32, u32, Vec<u8>)> {
  const DXGI_ATTEMPTS: usize = 2;
  const GDI_ATTEMPTS: usize = 3;

  let mut last_dxgi_error: Option<anyhow::Error> = None;
  let mut reset_performed = false;

  for attempt in 0..DXGI_ATTEMPTS {
    match dxgi() {
      Ok((width, height, rgba)) => {
        if !is_effectively_black_rgba(&rgba) {
          return Ok((width, height, rgba));
        }

        tracing::warn!(
          operation = "capture_virtual_desktop",
          monitor_id = %monitor.id,
          attempt = attempt + 1,
          "DXGI returned an all-black frame for screenshot capture"
        );

        if !reset_performed {
          reset();
          reset_performed = true;
          continue;
        }
      },
      Err(error) => {
        tracing::warn!(
          operation = "capture_virtual_desktop",
          monitor_id = %monitor.id,
          attempt = attempt + 1,
          error = %error,
          "DXGI capture failed for monitor"
        );
        last_dxgi_error = Some(error);

        if !reset_performed {
          reset();
          reset_performed = true;
          continue;
        }
      },
    }
  }

  let mut last_gdi_error: Option<anyhow::Error> = None;
  for attempt in 0..GDI_ATTEMPTS {
    match gdi() {
      Ok(gdi_buf) => {
        let is_black = is_effectively_black_rgba(&gdi_buf);
        let is_last_attempt = attempt + 1 == GDI_ATTEMPTS;

        if !is_black || is_last_attempt {
          if is_black {
            tracing::warn!(
              operation = "capture_virtual_desktop",
              monitor_id = %monitor.id,
              attempt = attempt + 1,
              "GDI also returned an all-black frame; accepting final attempt"
            );
          }
          tracing::debug!(operation = "capture_virtual_desktop", source = "gdi_recovery", monitor_id = %monitor.id, black = is_black, attempt = attempt + 1, "Returning GDI screenshot after DXGI attempts");
          return Ok((monitor.width, monitor.height, gdi_buf));
        }

        tracing::warn!(
          operation = "capture_virtual_desktop",
          monitor_id = %monitor.id,
          attempt = attempt + 1,
          "GDI returned an all-black frame; retrying"
        );
      },
      Err(error) => {
        tracing::warn!(
          operation = "capture_virtual_desktop",
          monitor_id = %monitor.id,
          attempt = attempt + 1,
          error = %error,
          "GDI fallback failed for monitor"
        );
        last_gdi_error = Some(error);
      },
    }
  }

  let dxgi_error = last_dxgi_error
    .map(|e| format!("{:#}", e))
    .unwrap_or_else(|| "DXGI returned only suspicious black frames".to_string());
  let gdi_error = last_gdi_error
    .map(|e| format!("{:#}", e))
    .unwrap_or_else(|| "GDI returned only suspicious black frames".to_string());

  Err(anyhow::anyhow!(
    "Monitor capture failed after DXGI and GDI fallback. DXGI: {}. GDI: {}",
    dxgi_error,
    gdi_error
  ))
}

fn blit_monitor_rgba(
  output: &mut [u8],
  output_width: u32,
  dst_x: usize,
  dst_y: usize,
  src_width: u32,
  src_height: u32,
  src_rgba: &[u8],
) -> Result<()> {
  let src_stride = (src_width as usize)
    .checked_mul(4)
    .ok_or_else(|| anyhow::anyhow!("Monitor row size overflow"))?;
  let dst_stride = (output_width as usize)
    .checked_mul(4)
    .ok_or_else(|| anyhow::anyhow!("Virtual desktop row size overflow"))?;
  anyhow::ensure!(
    src_stride > 0 && dst_stride > 0 && output.len().is_multiple_of(dst_stride),
    "Invalid monitor or virtual desktop row size"
  );
  let src_len = src_stride
    .checked_mul(src_height as usize)
    .ok_or_else(|| anyhow::anyhow!("Monitor buffer size overflow"))?;
  anyhow::ensure!(
    src_rgba.len() >= src_len,
    "Monitor frame buffer is truncated"
  );
  anyhow::ensure!(
    dst_x
      .checked_add(src_width as usize)
      .is_some_and(|right| right <= output_width as usize)
      && dst_y
        .checked_add(src_height as usize)
        .is_some_and(|bottom| bottom <= output.len() / dst_stride),
    "Monitor frame extends outside the virtual desktop"
  );

  for row in 0..src_height as usize {
    let src_offset = row * src_stride;
    let dst_offset = (dst_y + row) * dst_stride + dst_x * 4;
    output[dst_offset..dst_offset + src_stride]
      .copy_from_slice(&src_rgba[src_offset..src_offset + src_stride]);
  }
  Ok(())
}

fn is_effectively_black_rgba(rgba: &[u8]) -> bool {
  if rgba.is_empty() {
    return true;
  }

  rgba
    .as_chunks::<4>()
    .0
    .iter()
    .all(|pixel| pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0)
}

/// Main capture function - tries Desktop Duplication first, then WGC, then DIB
#[allow(dead_code)]
pub fn capture_fullscreen_rgba() -> Result<(u32, u32, Vec<u8>)> {
  init_thread_pool();
  capture_fullscreen_with(
    get_active_recording_backend(),
    || {
      // SAFETY: backend owns its duplication pipeline and mapped resources.
      unsafe { dxgi::try_desktop_duplication() }
    },
    || {
      // SAFETY: WGC schedules native work on its dispatcher owner.
      unsafe { wgc::try_wgc_capture() }
    },
    || {
      // SAFETY: GDI owns each DC/DIB and releases them on every exit.
      unsafe { gdi::capture_with_dib(init_thread_pool) }
    },
  )
}

fn capture_fullscreen_with(
  active: Option<CaptureBackendKind>,
  dxgi: impl FnOnce() -> Result<(u32, u32, Vec<u8>)>,
  wgc: impl FnOnce() -> Result<(u32, u32, Vec<u8>)>,
  gdi: impl FnOnce() -> Result<(u32, u32, Vec<u8>)>,
) -> Result<(u32, u32, Vec<u8>)> {
  let mut failures = Vec::new();
  let valid = |frame: (u32, u32, Vec<u8>)| -> Result<(u32, u32, Vec<u8>)> {
    let expected = (frame.0 as usize)
      .checked_mul(frame.1 as usize)
      .and_then(|n| n.checked_mul(4));
    anyhow::ensure!(
      frame.0 > 0 && frame.1 > 0 && expected == Some(frame.2.len()),
      "Capture backend returned invalid frame dimensions or pixel data"
    );
    Ok(frame)
  };
  match dxgi().and_then(valid) {
    Ok(frame) => {
      set_last_capture_method("Desktop Duplication API");
      return Ok(frame);
    },
    Err(error) => {
      tracing::warn!(%error,"DXGI capture failed; attempting fallback");
      failures.push(format!("DXGI: {error:#}"));
    },
  }
  if active != Some(CaptureBackendKind::Wgc) {
    match wgc().and_then(valid) {
      Ok(frame) => {
        set_last_capture_method("Windows Graphics Capture");
        return Ok(frame);
      },
      Err(error) => {
        tracing::warn!(%error,"WGC capture failed; attempting GDI");
        failures.push(format!("WGC: {error:#}"));
      },
    }
  }
  match gdi().and_then(valid) {
    Ok(frame) => {
      set_last_capture_method("Windows GDI");
      Ok(frame)
    },
    Err(error) => {
      failures.push(format!("GDI: {error:#}"));
      Err(anyhow::anyhow!(
        "All available capture backends failed: {}",
        failures.join("; ")
      ))
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serial_test::serial;

  fn monitor(id: &str, x: i32, width: u32) -> crate::monitors::MonitorInfo {
    crate::monitors::MonitorInfo {
      id: id.into(),
      name: id.into(),
      x,
      y: 0,
      width,
      height: 1,
      is_primary: x == 0,
      dpi_scale: 1.0,
      logical_x: x,
      logical_y: 0,
      logical_width: width,
      logical_height: 1,
    }
  }

  #[test]
  #[serial]
  fn virtual_desktop_gaps_never_retain_pixels_from_the_recycled_capture() {
    *FALLBACK_SCREENSHOT_BUFFER
      .get_or_init(|| Mutex::new(None))
      .lock()
      .unwrap() = Some(vec![77; 12]);
    let monitors = [monitor("left", -1, 1), monitor("right", 1, 1)];
    let (_, _, _, _, pixels) = capture_virtual_desktop_with((-1, 0, 3, 1), &monitors, |m| {
      Ok((1, 1, vec![if m.x < 0 { 1 } else { 2 }, 0, 0, 255]))
    })
    .unwrap();
    assert_eq!(
      &pixels[4..8],
      &[0; 4],
      "desktop gaps must not disclose pixels retained from a previous capture"
    );
    assert_eq!(&pixels[..4], &[1, 0, 0, 255]);
    assert_eq!(&pixels[8..], &[2, 0, 0, 255]);
  }

  #[test]
  fn monitor_recovery_retries_are_bounded_reset_once_and_preserve_both_backend_errors() {
    use std::cell::Cell;
    let m = monitor("fixture", 0, 1);
    for dxgi_black in [false, true] {
      let dx = Cell::new(0);
      let gd = Cell::new(0);
      let resets = Cell::new(0);
      let result = capture_monitor_with(
        &m,
        || {
          dx.set(dx.get() + 1);
          if dxgi_black {
            Ok((1, 1, vec![0, 0, 0, 255]))
          } else {
            Err(anyhow::anyhow!("original duplication failure"))
          }
        },
        || {
          gd.set(gd.get() + 1);
          if gd.get() < 3 {
            Ok(vec![0, 0, 0, 255])
          } else {
            Ok(vec![5, 6, 7, 255])
          }
        },
        || resets.set(resets.get() + 1),
      )
      .unwrap();
      assert_eq!(result, (1, 1, vec![5, 6, 7, 255]));
      assert_eq!((dx.get(), gd.get(), resets.get()), (2, 3, 1));
    }
    let gd = Cell::new(0);
    let result = capture_monitor_with(
      &m,
      || Ok((1, 1, vec![0, 0, 0, 255])),
      || {
        gd.set(gd.get() + 1);
        Ok(vec![0, 0, 0, 255])
      },
      || {},
    )
    .unwrap();
    assert_eq!(gd.get(), 3);
    assert_eq!(result.2, vec![0, 0, 0, 255]);
    let error = capture_monitor_with(
      &m,
      || Err(anyhow::anyhow!("device lost")),
      || Err(anyhow::anyhow!("GDI allocation failed")),
      || {},
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("device lost") && message.contains("GDI allocation failed"));
    let error = capture_monitor_with(
      &m,
      || Ok((1, 1, vec![0, 0, 0, 255])),
      || Err(anyhow::anyhow!("GDI denied")),
      || {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("suspicious black frames"));
    assert_eq!(
      capture_monitor_with(
        &m,
        || Ok((1, 1, vec![1, 2, 3, 255])),
        || panic!("valid DXGI frame cannot fall back"),
        || panic!("valid DXGI frame cannot reset")
      )
      .unwrap()
      .2,
      vec![1, 2, 3, 255]
    );
  }

  #[test]
  fn invalid_virtual_desktop_geometry_returns_errors_without_partial_rows_or_panics() {
    let m = monitor("stale", 0, 1);
    assert!(
      capture_virtual_desktop_with((0, 0, 0, 1), std::slice::from_ref(&m), |_| panic!(
        "zero dimensions cannot capture"
      ))
      .is_err()
    );
    assert!(
      capture_virtual_desktop_with(
        (0, 0, u32::MAX, u32::MAX),
        std::slice::from_ref(&m),
        |_| panic!("overflow cannot capture")
      )
      .is_err()
    );
    assert!(
      capture_virtual_desktop_with((0, 0, 1, 1), &[], |_| panic!(
        "empty desktop cannot capture"
      ))
      .is_err()
    );
    for case in 0..4 {
      let bounds = if case == 1 {
        (1, 0, 1, 1)
      } else {
        (0, 0, 1, 1)
      };
      let error = capture_virtual_desktop_with(bounds, std::slice::from_ref(&m), |_| match case {
        0 => Ok((2, 1, vec![1; 8])),
        2 => Ok((1, 1, vec![1])),
        3 => Err(anyhow::anyhow!("fixture failure")),
        _ => Ok((1, 1, vec![1; 4])),
      })
      .unwrap_err();
      assert!(!error.to_string().is_empty());
    }
    for (width, x, y, source_width, source_height) in [
      (0, 0, 0, 1, 1),
      (1, usize::MAX, 0, 1, 1),
      (1, 0, usize::MAX, 1, 1),
      (1, 0, 0, 1, 2),
    ] {
      let mut pixels = vec![99; 4];
      assert!(
        blit_monitor_rgba(
          &mut pixels,
          width,
          x,
          y,
          source_width,
          source_height,
          &[1; 8]
        )
        .is_err()
      );
      assert_eq!(pixels, vec![99; 4]);
    }
  }

  #[test]
  fn a_stale_monitor_frame_cannot_spill_into_the_next_virtual_desktop_row() {
    let mut output = vec![99u8; 4 * 3 * 4];
    assert!(blit_monitor_rgba(&mut output, 4, 3, 0, 3, 1, &[1u8; 12]).is_err());
    assert_eq!(
      &output[16..32],
      &[99u8; 16],
      "stale right edge overwrote the next row"
    );
  }

  /// set + get for every CaptureBackendKind variant must round-trip correctly.
  #[test]
  #[serial]
  fn test_capture_backend_kind_roundtrip() {
    for (variant, expected) in [
      (
        Some(CaptureBackendKind::Dxgi),
        Some(CaptureBackendKind::Dxgi),
      ),
      (Some(CaptureBackendKind::Wgc), Some(CaptureBackendKind::Wgc)),
      (Some(CaptureBackendKind::Gdi), Some(CaptureBackendKind::Gdi)),
      (None, None),
    ] {
      set_active_recording_backend(variant);
      assert_eq!(get_active_recording_backend(), expected);
    }
    // Leave clean for other tests
    set_active_recording_backend(None);
  }

  /// Setting to Wgc then back to None must clear the value.
  #[test]
  #[serial]
  fn test_set_active_recording_backend_none_clears() {
    set_active_recording_backend(Some(CaptureBackendKind::Wgc));
    assert_eq!(
      get_active_recording_backend(),
      Some(CaptureBackendKind::Wgc)
    );
    set_active_recording_backend(None);
    assert_eq!(get_active_recording_backend(), None);
  }

  /// GDI video capture must return a non-empty RGBA frame.
  #[test]
  #[ignore = "captures the real desktop: cargo xtask test:capture:desktop"]
  fn test_try_gdi_capture_for_video_returns_frame() {
    let (w, h, data) =
      try_gdi_capture_for_video().expect("GDI must capture this explicit desktop session");
    assert!(w > 0 && h > 0);
    assert_eq!(data.len(), (w * h * 4) as usize);
  }

  /// When ACTIVE_RECORDING_BACKEND is Wgc, capture_fullscreen_rgba must not use WGC.
  /// It should fall to DXGI (if available) or GDI, never "Windows Graphics Capture".
  #[test]
  #[serial]
  fn test_capture_fullscreen_skips_wgc_when_recording() {
    let result = capture_fullscreen_with(
      Some(CaptureBackendKind::Wgc),
      || Err(anyhow::anyhow!("DXGI unavailable")),
      || panic!("WGC is owned by recording"),
      || Ok((1, 1, vec![1, 2, 3, 255])),
    )
    .unwrap();
    assert_eq!(result.2, vec![1, 2, 3, 255]);
    assert_eq!(get_last_capture_method().as_deref(), Some("Windows GDI"));
  }

  #[test]
  #[serial]
  fn fallback_dispatch_preserves_priority_validates_frames_and_retains_all_failure_reasons() {
    for successful in 0..3 {
      let calls = std::cell::RefCell::new(Vec::new());
      let attempt = |id| {
        calls.borrow_mut().push(id);
        if id == successful {
          Ok((1, 1, vec![1, 2, 3, 255]))
        } else {
          Err(anyhow::anyhow!("backend {id} unavailable"))
        }
      };
      let result =
        capture_fullscreen_with(None, || attempt(0), || attempt(1), || attempt(2)).unwrap();
      assert_eq!(result.2, vec![1, 2, 3, 255]);
      assert_eq!(*calls.borrow(), (0..=successful).collect::<Vec<_>>());
    }
    let error = capture_fullscreen_with(
      None,
      || Err(anyhow::anyhow!("driver removed").context("DXGI startup")),
      || Ok((4, 4, vec![0; 3])),
      || Err(anyhow::anyhow!("GDI access denied")),
    )
    .unwrap_err()
    .to_string();
    for reason in [
      "driver removed",
      "DXGI startup",
      "invalid frame dimensions",
      "GDI access denied",
    ] {
      assert!(error.contains(reason), "{reason}");
    }
  }

  #[test]
  fn test_is_effectively_black_rgba_detects_black() {
    let rgba = vec![0u8, 0, 0, 255, 0, 0, 0, 0];
    assert!(is_effectively_black_rgba(&rgba));
  }

  #[test]
  fn test_is_effectively_black_rgba_rejects_visible_pixel() {
    let rgba = vec![0u8, 0, 1, 255, 0, 0, 0, 255];
    assert!(!is_effectively_black_rgba(&rgba));
  }

  #[test]
  fn test_blit_monitor_rgba_copies_expected_region() {
    let mut output = vec![0u8; 4 * 4 * 4];
    let src = vec![1u8, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];

    blit_monitor_rgba(&mut output, 4, 1, 1, 2, 2, &src).unwrap();

    let idx = (4 + 1) * 4;
    assert_eq!(&output[idx..idx + 8], &src[0..8]);
    let idx_next = ((2 * 4) + 1) * 4;
    assert_eq!(&output[idx_next..idx_next + 8], &src[8..16]);
  }
}
