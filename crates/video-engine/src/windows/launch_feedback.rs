//! Ending the launch cursor a worker otherwise keeps on screen.
//!
//! Windows shows the "working in background" cursor after `CreateProcess` starts a GUI-subsystem
//! executable, as every engine worker is (the app's own `flinch.exe`). It stays for up to two
//! seconds, five more after the process's first USER32 call (the worker's DPI awareness is one),
//! until the process first waits for input. A worker only reads its pipe, so without that wait
//! the cursor spins for seconds after every app start and recording. Rust's way to start a child
//! without the cursor (`startupinfo_force_feedback`) is unstable, so the worker ends it itself.
//!
//! Only a real wait counts. Measured on Windows 11 with `WaitForInputIdle` from the
//! parent: `PeekMessageW` (removing or not, once or twice) left the child busy, and a
//! `GetMessageW` that found a message at once did too; `MsgWaitForMultipleObjects` with a zero
//! timeout made it idle.

use crate::{EngineError, Result};
use windows::Win32::Foundation::WAIT_FAILED;
use windows::Win32::UI::WindowsAndMessaging::{MsgWaitForMultipleObjects, QS_ALLINPUT};

/// Tell Windows the worker waits for input, which ends its launch cursor at once. The wait has a
/// zero timeout, so it never blocks, and no message is removed.
pub fn end() -> Result<()> {
  // SAFETY: no handles are passed; the call only polls the calling thread's input queue.
  let result = unsafe { MsgWaitForMultipleObjects(None, false, 0, QS_ALLINPUT) };
  if result == WAIT_FAILED {
    return Err(EngineError::native(
      "launch cursor",
      windows::core::Error::from_thread(),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  #![allow(clippy::expect_used)]
  use super::*;

  #[test]
  fn ending_the_launch_cursor_never_blocks_and_repeats_harmlessly() {
    std::thread::spawn(|| {
      let started = std::time::Instant::now();
      end().expect("first wait");
      end().expect("repeated wait");
      assert!(started.elapsed() < std::time::Duration::from_secs(1));
    })
    .join()
    .expect("wait thread");
  }
}
