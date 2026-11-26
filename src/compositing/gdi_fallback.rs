//! GDI-based fallback capture backends for the compositing pipeline.
//!
//! Used when DXGI Desktop Duplication is unavailable (e.g. RDP sessions,
//! certain virtualized environments, network-streamed virtual displays).  Uses
//! `capture_virtual_desktop_into` which calls `GetDC(None)` so it covers
//! all monitors including those at negative virtual-desktop coordinates.

use anyhow::Result;

use crate::monitors::MonitorInfo;
use crate::screenshot::platform::windows::capture_virtual_desktop_into;

use super::backend::{CaptureBackend, FrameAcquireResult};

/// Pixel threshold above which we cap recommended FPS to 30 to reduce CPU load.
const HIGH_PIXEL_THRESHOLD: u64 = 16_600_000;

pub struct GdiDesktopBackend {
  virt_x: i32,
  virt_y: i32,
  width: u32,
  height: u32,
  /// Pre-allocated frame buffer (RGBA, width * height * 4 bytes).
  buffer: Vec<u8>,
}

impl GdiDesktopBackend {
  /// Create a new backend covering the given virtual-desktop rectangle.
  ///
  /// `virt_x` / `virt_y` are the top-left corner in virtual-desktop
  /// coordinates (may be negative for monitors left of / above the primary).
  pub fn new(virt_x: i32, virt_y: i32, virt_width: u32, virt_height: u32) -> Self {
    let capacity = (virt_width as usize) * (virt_height as usize) * 4;
    Self {
      virt_x,
      virt_y,
      width: virt_width,
      height: virt_height,
      buffer: vec![0u8; capacity],
    }
  }

  /// Returns 30 FPS for regions larger than ~16.6 MP, 60 FPS otherwise.
  ///
  /// GDI capture is CPU-bound; capping at 30 on large regions prevents the
  /// capture thread from starving other work.
  pub fn recommended_max_fps(&self) -> u32 {
    let total = (self.width as u64) * (self.height as u64);
    if total > HIGH_PIXEL_THRESHOLD { 30 } else { 60 }
  }
}

/// Per-monitor GDI capture backend for the compositing pipeline.
///
/// Captures the monitor's region of the virtual desktop using GDI BitBlt.
/// Works on monitors that do not support DXGI Desktop Duplication — e.g.
/// network-streamed virtual displays, RDP sessions, and software-rendered adapters.
///
/// Captures at logical (DPI-scaled) resolution. For monitors at 100% DPI
/// (scale = 1.0), logical dimensions equal physical dimensions so the
/// compositor receives the correct resolution. For HiDPI monitors the frame
/// is at reduced resolution; the compositor's existing resolution-change
/// path handles the crop gracefully.
pub struct GdiMonitorBackend {
  logical_x: i32,
  logical_y: i32,
  logical_width: u32,
  logical_height: u32,
  buffer: Vec<u8>,
}

impl GdiMonitorBackend {
  /// Create a new backend targeting `monitor`.
  ///
  /// Uses the monitor's logical (DPI-scaled) bounds so the GDI virtual
  /// desktop coordinate system is respected.
  pub fn new(monitor: &MonitorInfo) -> Self {
    let capacity = (monitor.logical_width as usize) * (monitor.logical_height as usize) * 4;
    Self {
      logical_x: monitor.logical_x,
      logical_y: monitor.logical_y,
      logical_width: monitor.logical_width,
      logical_height: monitor.logical_height,
      buffer: vec![0u8; capacity],
    }
  }
}

impl CaptureBackend for GdiMonitorBackend {
  fn acquire_frame(&mut self) -> Result<FrameAcquireResult<'_>> {
    capture_virtual_desktop_into(
      &mut self.buffer,
      self.logical_x,
      self.logical_y,
      self.logical_width,
      self.logical_height,
    )?;

    let row_pitch = self.logical_width * 4;
    Ok(FrameAcquireResult::NewFrame {
      data: &self.buffer,
      width: self.logical_width,
      height: self.logical_height,
      row_pitch,
    })
  }

  fn release(&mut self) {
    self.buffer = Vec::new();
  }
}

impl CaptureBackend for GdiDesktopBackend {
  fn acquire_frame(&mut self) -> Result<FrameAcquireResult<'_>> {
    capture_virtual_desktop_into(
      &mut self.buffer,
      self.virt_x,
      self.virt_y,
      self.width,
      self.height,
    )?;

    // GDI always produces a new frame — there is no change-detection.
    let row_pitch = self.width * 4;
    Ok(FrameAcquireResult::NewFrame {
      data: &self.buffer,
      width: self.width,
      height: self.height,
      row_pitch,
    })
  }

  fn release(&mut self) {
    // Drop the frame buffer to free memory; shrink to zero so any accidental
    // subsequent call to acquire_frame fails clearly in capture_virtual_desktop_into.
    self.buffer = Vec::new();
  }
}
