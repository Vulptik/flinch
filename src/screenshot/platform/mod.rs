// src/screenshot/platform/mod.rs

pub(crate) mod windows;
pub use windows::{
  // Capture backend conflict tracking and video/GIF fallbacks
  CaptureBackendKind,
  capture_active_window_rgba,
  capture_virtual_desktop_rgba,
  clear_cached_screenshot_buffer,
  get_active_recording_backend,
  invalidate_stale_captures,
  release_capture_idle_memory,
  release_dxgi_idle_memory,
  return_screenshot_buffer,
  warm_up_desktop_duplication,
  warm_up_wgc,
};

use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum CaptureError {
  #[error("Platform not supported")]
  UnsupportedPlatform,
}
