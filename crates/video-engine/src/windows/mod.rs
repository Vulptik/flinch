//! COM and D3D objects stay on their owner thread; the worker is the hard timeout boundary.
pub mod aligned;
pub mod amf;
pub mod audio;
pub mod audio_service;
pub mod build_identity;
pub mod calibration;
pub mod capture;
pub mod clicks;
pub mod cursor;
pub mod desktop_fixture;
pub mod dpi;
pub mod encoding;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod frame_pass;
pub mod gif;
pub mod gpu;
pub mod host;
pub mod launch_feedback;
pub mod mft;
pub mod mft_events;
pub mod mux;
pub mod nvenc;
pub mod qsv;
pub mod reencode;
pub mod rotate;
pub mod runtime;
pub mod sdr_white;
mod self_test;
pub mod session_status;
pub mod software;
pub mod supervisor;
pub mod telemetry;
pub mod thumbnail;
pub mod timer;
mod vendor;
#[cfg(test)]
mod vendor_acceptance;
pub mod verify;

use crate::{EngineError, Result, time};
use windows::Win32::{
  Media::MediaFoundation::*,
  System::{Com::*, Performance::*},
};

pub struct Apartment;
impl Apartment {
  pub fn new() -> Result<Self> {
    // SAFETY: every owner creates one apartment and destroys its COM objects before this guard.
    unsafe {
      CoInitializeEx(None, COINIT_MULTITHREADED)
        .ok()
        .map_err(|e| EngineError::native("COM initialize", e))?;
    }
    Ok(Self)
  }
}
impl Drop for Apartment {
  fn drop(&mut self) {
    // SAFETY: balances successful initialization on this thread.
    unsafe {
      CoUninitialize();
    }
  }
}
pub struct MediaRuntime;
impl MediaRuntime {
  pub fn new() -> Result<Self> {
    // SAFETY: no framework objects are used before successful startup.
    unsafe {
      MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| EngineError::native("MF startup", e))?;
    }
    Ok(Self)
  }
}
impl Drop for MediaRuntime {
  fn drop(&mut self) {
    // SAFETY: caller keeps the guard alive until its MF objects have been released.
    if let Err(error) = unsafe { MFShutdown() } {
      eprintln!("MF shutdown: {error}");
    }
  }
}
pub fn qpc() -> Result<u64> {
  let mut counter = 0i64;
  let mut frequency = 0i64;
  // SAFETY: stack integers are valid writable outputs; pointers are not retained.
  unsafe {
    QueryPerformanceCounter(&mut counter).map_err(|e| EngineError::native("QPC", e))?;
    QueryPerformanceFrequency(&mut frequency)
      .map_err(|e| EngineError::native("QPC frequency", e))?;
  }
  if counter < 0 || frequency <= 0 {
    return Err(EngineError::protocol("Invalid QPC values"));
  }
  time::scale(counter as u64, time::SECOND, frequency as u64)
}
