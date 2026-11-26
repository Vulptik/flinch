//! Process-local high-resolution wait. Does not change the machine's timer period.
use crate::{EngineError, Result};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows::Win32::{
  Foundation::{HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT},
  System::Threading::*,
};
/// How long past its due time a wait may stay unsignalled before it is a fault. Virtual machines
/// deschedule guest threads for tens of milliseconds and more under load (a 100 ms bound failed in
/// a two-core VirtualBox guest); a late wake is a scheduling delay, not a timer
/// failure, and callers already absorb gaps by dropping or redating slots.
const LATE_BOUND_MS: u32 = 2_000;
fn bound_ms(microseconds: u32) -> u32 {
  (microseconds / 1000).saturating_add(LATE_BOUND_MS)
}
fn outcome(result: WAIT_EVENT) -> Result<()> {
  if result == WAIT_OBJECT_0 {
    Ok(())
  } else if result == WAIT_TIMEOUT {
    Err(EngineError::new(
      crate::ErrorKind::Timeout,
      "high-resolution timer",
      "Timer did not signal within its bounded wait",
    ))
  } else {
    Err(EngineError::native(
      "wait for high-resolution timer",
      windows::core::Error::from_thread(),
    ))
  }
}
pub struct Waiter(OwnedHandle);
impl Waiter {
  pub fn new() -> Result<Self> {
    // SAFETY: unnamed, non-inherited local synchronization object with one owning handle.
    let handle = unsafe {
      CreateWaitableTimerExW(
        None,
        None,
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
        TIMER_ALL_ACCESS.0,
      )
    }
    .map_err(|e| EngineError::native("high-resolution timer", e))?;
    // SAFETY: CreateWaitableTimerExW returned a unique handle and ownership transfers once.
    Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle.0) }))
  }
  pub fn wait(&self, microseconds: u32) -> Result<()> {
    let due = -i64::from(microseconds.max(1)) * 10;
    let handle = HANDLE(self.0.as_raw_handle());
    // SAFETY: finite relative due-time, no callback; only the owner arms and waits on this timer.
    unsafe { SetWaitableTimerEx(handle, &due, 0, None, None, None, 0) }
      .map_err(|e| EngineError::native("arm high-resolution timer", e))?;
    // SAFETY: valid timer stays alive throughout this bounded wait.
    outcome(unsafe { WaitForSingleObject(handle, bound_ms(microseconds)) })
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn named_global_timer_period_is_not_needed() {
    let t = Waiter::new().expect("local timer");
    t.wait(100).expect("finite local wait");
  }
  #[test]
  fn late_wakes_within_a_vm_stall_are_not_faults() {
    // Regression: a ~101 ms bound turned VM descheduling into a failed fixture (and could fail a
    // recording through the runtime's 1 ms poll).
    assert!(bound_ms(1000) >= 2_000);
    assert_eq!(bound_ms(250), LATE_BOUND_MS);
    // u32::MAX is INFINITE to WaitForSingleObject; the bound must stay finite.
    assert!(bound_ms(u32::MAX) < windows::Win32::System::Threading::INFINITE);
    assert!(outcome(WAIT_OBJECT_0).is_ok());
    let late = outcome(WAIT_TIMEOUT).expect_err("timeout");
    assert_eq!(late.kind, crate::ErrorKind::Timeout);
    assert!(outcome(windows::Win32::Foundation::WAIT_FAILED).is_err());
  }
}
