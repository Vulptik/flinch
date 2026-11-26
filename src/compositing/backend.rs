//! CaptureBackend trait and supporting types for the compositing pipeline.

use anyhow::Result;

/// Result of a single frame acquisition from a capture backend.
pub enum FrameAcquireResult<'a> {
  /// A new frame is available with the given pixel data and dimensions.
  NewFrame {
    /// BGRA/RGBA pixel data
    data: &'a [u8],
    width: u32,
    height: u32,
    /// Bytes per row (may include padding)
    row_pitch: u32,
  },
  /// The frame has not changed since the last acquisition.
  NoChange,
}

/// Abstraction over a single-monitor capture source.
///
/// Implementations must be Send so they can be handed to the compositor thread.
pub trait CaptureBackend: Send {
  /// Acquire the next frame from this backend.
  ///
  /// The returned data reference is valid until the next call to `acquire_frame`
  /// or until `release` is called.
  fn acquire_frame(&mut self) -> Result<FrameAcquireResult<'_>>;

  /// Release all resources held by this backend.
  ///
  /// After calling this the backend must not be used again.
  fn release(&mut self);
}

/// A simple mock backend for unit testing.
///
/// Returns a single static frame on every call unless `return_no_change` is set.
pub struct MockCaptureBackend {
  /// BGRA/RGBA pixel data to return on `NewFrame`.
  pub frame_data: Vec<u8>,
  pub width: u32,
  pub height: u32,
  /// When `true`, `acquire_frame` returns `NoChange` instead of `NewFrame`.
  pub return_no_change: bool,
}

impl MockCaptureBackend {
  /// Create a mock backend filled with a solid colour.
  ///
  /// `fill` should be a 4-byte BGRA value that is tiled across the frame.
  pub fn new_solid(width: u32, height: u32, fill: [u8; 4]) -> Self {
    let pixel_count = (width as usize) * (height as usize);
    let mut frame_data = Vec::with_capacity(pixel_count * 4);
    for _ in 0..pixel_count {
      frame_data.extend_from_slice(&fill);
    }
    Self {
      frame_data,
      width,
      height,
      return_no_change: false,
    }
  }
}

impl CaptureBackend for MockCaptureBackend {
  fn acquire_frame(&mut self) -> Result<FrameAcquireResult<'_>> {
    if self.return_no_change {
      return Ok(FrameAcquireResult::NoChange);
    }
    let row_pitch = self.width * 4;
    Ok(FrameAcquireResult::NewFrame {
      data: &self.frame_data,
      width: self.width,
      height: self.height,
      row_pitch,
    })
  }

  fn release(&mut self) {
    // Nothing to release for the mock backend.
  }
}
