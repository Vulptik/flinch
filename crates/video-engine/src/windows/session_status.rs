//! Read-only session status. No window, focus, input hook or desktop capture.
use crate::{EngineError, ErrorKind, Result};
use windows::{Win32::System::RemoteDesktop::*, core::PWSTR};
fn validate(level: u32, state: WTS_CONNECTSTATE_CLASS, flags: i32) -> Result<()> {
  if level != 1 {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "Windows session",
      "Unsupported WTS information level",
    ));
  }
  if state != WTSActive || flags == WTS_SESSIONSTATE_LOCK as i32 {
    return Err(EngineError::new(
      ErrorKind::DeviceLost,
      "Windows session",
      "Recording target session is locked or disconnected",
    ));
  }
  if flags != WTS_SESSIONSTATE_UNLOCK as i32 {
    return Err(EngineError::new(
      ErrorKind::Native,
      "Windows session",
      "WTS could not determine session lock state",
    ));
  }
  Ok(())
}
pub fn check() -> Result<()> {
  let mut pointer = PWSTR::null();
  let mut bytes = 0;
  // SAFETY: read-only query for this process session; one allocation is released on every branch below.
  unsafe {
    WTSQuerySessionInformationW(
      None,
      WTS_CURRENT_SESSION,
      WTSSessionInfoEx,
      &mut pointer,
      &mut bytes,
    )
  }
  .map_err(|e| EngineError::native("Windows session query", e))?;
  let result = (|| {
    if pointer.is_null() || bytes < std::mem::size_of::<WTSINFOEXW>() as u32 {
      return Err(EngineError::protocol("Truncated Windows session status"));
    }
    // SAFETY: exact documented structure after size validation; level selects the union arm.
    let info = unsafe { &*pointer.0.cast::<WTSINFOEXW>() };
    if info.Level != 1 {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "Windows session",
        "Unsupported WTS information level",
      ));
    }
    let status = unsafe { info.Data.WTSInfoExLevel1 };
    validate(info.Level, status.SessionState, status.SessionFlags)
  })();
  // SAFETY: WTSQuerySessionInformation allocated the buffer, freed exactly once after all borrows expire.
  unsafe {
    WTSFreeMemory(pointer.0.cast());
  }
  result
}
#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn locked_disconnected_and_unknown_states_are_explicit_failures() {
    assert!(validate(1, WTSActive, WTS_SESSIONSTATE_UNLOCK as i32).is_ok());
    assert_eq!(
      validate(1, WTSActive, WTS_SESSIONSTATE_LOCK as i32)
        .err()
        .map(|e| e.kind),
      Some(ErrorKind::DeviceLost)
    );
    assert!(validate(1, WTSDisconnected, 1).is_err());
    assert!(validate(1, WTSActive, -1).is_err());
    assert!(validate(2, WTSActive, 1).is_err());
  }
  #[test]
  fn real_session_query_releases_its_buffer_and_reports_one_consistent_classified_state() {
    // Read-only query of this process's own session; it never locks or changes the desktop.
    let first = check();
    for _ in 0..200 {
      let repeated = check();
      assert_eq!(
        repeated.is_ok(),
        first.is_ok(),
        "session state changed mid-test"
      );
      if let (Err(a), Err(b)) = (&first, &repeated) {
        assert_eq!((a.kind, &a.stage), (b.kind, &b.stage));
      }
    }
    if let Err(error) = first {
      // Service or disconnected CI sessions must fail explicitly, never as a successful capture state.
      assert!(
        error.stage.starts_with("Windows session"),
        "unclassified session failure: {error}"
      );
    }
  }
}
