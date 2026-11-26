//! DXGI Desktop Duplication capture backend for the compositing pipeline.
//!
//! Wraps the existing per-monitor DXGI capture functions into the [`CaptureBackend`] trait
//! so the multi-monitor compositor can drive each monitor independently.
//!
//! **Row pitch note**: The underlying DXGI functions (`capture_frame_for_video_from_monitor`)
//! handle DXGI stride padding internally during BGRA→RGBA conversion, so the data returned
//! is always tightly packed (row_pitch == width * 4). We pass `width * 4` as `row_pitch` in
//! `FrameAcquireResult::NewFrame`.

use anyhow::{Context, Result};

use crate::compositing::backend::{CaptureBackend, FrameAcquireResult};
use crate::monitors::MonitorInfo;
use crate::screenshot::platform::windows::{
  capture_frame_for_video_from_monitor, try_desktop_duplication_for_monitor,
};

/// A [`CaptureBackend`] backed by DXGI Desktop Duplication for a single monitor.
///
/// Frames are captured via the shared per-monitor DXGI pipeline that already exists
/// in `screenshot::platform::windows::dxgi`. This backend simply adapts that API
/// to the `CaptureBackend` trait.
pub struct DxgiCaptureBackend {
  monitor: MonitorInfo,
  /// Cached frame data from the last successful capture.
  /// Kept here so `acquire_frame` can return a borrowed slice with lifetime `'_`.
  frame_buf: Vec<u8>,
  frame_width: u32,
  frame_height: u32,
  released: bool,
}

impl DxgiCaptureBackend {
  /// Create a new backend targeting `monitor`. No DXGI resources are allocated until
  /// the first call to [`acquire_frame`](CaptureBackend::acquire_frame).
  pub fn new(monitor: MonitorInfo) -> Self {
    Self {
      monitor,
      frame_buf: Vec::new(),
      frame_width: 0,
      frame_height: 0,
      released: false,
    }
  }

  /// Probe whether DXGI Desktop Duplication is available for `monitor`.
  ///
  /// This performs a full capture (creating the OutputDuplication, grabbing one frame,
  /// and releasing it) to verify the pipeline works. The captured data is discarded.
  pub fn probe(monitor: &MonitorInfo) -> Result<()> {
    // Safety: DXGI COM calls require unsafe. The function handles all internal
    // pointer management and COM reference counting.
    unsafe {
      try_desktop_duplication_for_monitor(monitor).context("DXGI probe failed for monitor")?;
    }
    Ok(())
  }
}

impl CaptureBackend for DxgiCaptureBackend {
  fn acquire_frame(&mut self) -> Result<FrameAcquireResult<'_>> {
    if self.released {
      return Err(anyhow::anyhow!(
        "DxgiCaptureBackend::acquire_frame called after release"
      ));
    }

    // Safety: DXGI COM calls require unsafe. The underlying function manages all
    // Direct3D resources, mapped textures, and COM pointers internally.
    let result = unsafe {
      capture_frame_for_video_from_monitor(&self.monitor)
        .context("DXGI capture_frame_for_video_from_monitor failed")?
    };

    match result {
      Some((w, h, data)) => {
        self.frame_buf = data;
        self.frame_width = w;
        self.frame_height = h;
        Ok(FrameAcquireResult::NewFrame {
          data: &self.frame_buf,
          width: self.frame_width,
          height: self.frame_height,
          // Data is tightly packed — DXGI padding is stripped during BGRA→RGBA conversion.
          row_pitch: self.frame_width * 4,
        })
      },
      None => {
        // If we've never captured a real frame yet, the video path returned NoChange
        // because the probe already consumed the only pending DXGI frame. Fall back
        // to the screenshot path which can serve the cached frame the probe populated,
        // preventing the compositor output buffer from staying black on first access.
        if self.frame_buf.is_empty() {
          let (w, h, data) = unsafe {
            try_desktop_duplication_for_monitor(&self.monitor)
              .context("DXGI initial frame fallback via screenshot cache failed")?
          };
          self.frame_buf = data;
          self.frame_width = w;
          self.frame_height = h;
          Ok(FrameAcquireResult::NewFrame {
            data: &self.frame_buf,
            width: self.frame_width,
            height: self.frame_height,
            row_pitch: self.frame_width * 4,
          })
        } else {
          Ok(FrameAcquireResult::NoChange)
        }
      },
    }
  }

  fn release(&mut self) {
    self.released = true;
    self.frame_buf = Vec::new();
    // The underlying DXGI pipeline is process-global and shared; we don't tear it
    // down here. It will be cleaned up by `release_dxgi_idle_memory` or on process exit.
  }
}
