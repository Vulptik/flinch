//! Process DPI awareness required by Desktop Duplication.
//!
//! `IDXGIOutput5::DuplicateOutput1` returns `DXGI_ERROR_UNSUPPORTED` in a process that is not
//! per-monitor DPI aware (v2): the same session succeeds in Flinch's executable (PerMonitorV2
//! manifest) and fails in an unmanifested process such as the lab. The worker therefore declares the awareness itself instead of depending on its host's manifest.
use crate::{EngineError, Result};
use windows::Win32::UI::HiDpi::{
  AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
  GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
};

/// True when the calling thread runs per-monitor DPI aware (v2).
pub fn is_per_monitor_v2() -> bool {
  // SAFETY: read-only queries of the calling thread's awareness and a constant context.
  unsafe {
    AreDpiAwarenessContextsEqual(
      GetThreadDpiAwarenessContext(),
      DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    )
    .as_bool()
  }
}

/// Make the process per-monitor DPI aware (v2), or report why it cannot be. Already-aware
/// processes (for example through an application manifest) are left unchanged.
pub fn ensure_per_monitor_v2() -> Result<()> {
  if is_per_monitor_v2() {
    return Ok(());
  }
  // SAFETY: process-wide setting made once, before this worker creates any window or device.
  unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    .map_err(|error| EngineError::native("per-monitor DPI awareness", error))?;
  if !is_per_monitor_v2() {
    return Err(EngineError::protocol(
      "Per-monitor DPI awareness was accepted but is not in effect",
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  #![allow(clippy::expect_used)]
  use super::*;

  #[test]
  fn the_process_becomes_per_monitor_aware_and_repeated_calls_are_harmless() {
    ensure_per_monitor_v2().expect("per-monitor v2");
    assert!(is_per_monitor_v2());
    // A second request (as from a manifest-aware host) is a no-op, not an access error.
    ensure_per_monitor_v2().expect("idempotent");
  }
}
