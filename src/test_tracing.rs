//! Test-only: a catch-all tracing subscriber for the library's unit-test binary.
//!
//! Without a subscriber, `tracing` macros skip building their events, so the field
//! expressions inside every `warn!`/`error!`/`debug!` never run under test. A bug in one
//! (a failing `Display`, a wrong field) would only surface in the shipped app, and coverage
//! counts those lines as unexecuted even when the surrounding branch is tested. This
//! subscriber enables every level and formats each event into a sink, so the code runs.

/// Installed before `main` by the MSVC C runtime, the same mechanism as the `ctor` crate,
/// so it is in place before the test harness starts any test thread.
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static INSTALL: extern "C" fn() = install;

extern "C" fn install() {
  // The isolated logger tests start this binary as a child that installs the production
  // logger itself and checks how a conflicting global subscriber is reported.
  if std::env::var_os("FLINCH_TEST_LOG_PATH").is_some() {
    return;
  }
  let subscriber = tracing_subscriber::fmt()
    .with_max_level(tracing::Level::TRACE)
    .with_writer(std::io::sink)
    .finish();
  // A failure means a subscriber already exists; tests then keep that one.
  let _ = tracing::subscriber::set_global_default(subscriber);
}

#[cfg(test)]
mod tests {
  #[test]
  fn every_level_is_enabled_before_any_test_runs() {
    assert!(
      tracing::enabled!(tracing::Level::TRACE),
      "the catch-all test subscriber was not installed"
    );
  }
}
