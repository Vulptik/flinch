//! The isolated profile of a `speed-probe` build, used only by `cargo xtask test:speed:desktop`.
//!
//! A probe build started with [`ENV`] set keeps its settings, logs and recent captures in that
//! folder and takes its own single-instance identity, so it runs beside an installed or running
//! Flinch without reading or changing its data or activating it. Shipped builds never enable the
//! feature: [`folder`] is always `None` there and the variable is ignored.
use std::path::PathBuf;

/// Names the profile folder of a probe build.
pub const ENV: &str = "FLINCH_PROBE_PROFILE";

/// The probe profile folder, when this is a probe build started with one.
pub fn folder() -> Option<PathBuf> {
  #[cfg(feature = "speed-probe")]
  {
    std::env::var_os(ENV)
      .filter(|value| !value.is_empty())
      .map(PathBuf::from)
  }
  #[cfg(not(feature = "speed-probe"))]
  {
    None
  }
}

/// Suffix that keeps a probe's single-instance identity apart from the real app's.
pub fn identity_suffix() -> &'static str {
  if folder().is_some() {
    ".speed-probe"
  } else {
    ""
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn shipped_builds_ignore_the_probe_profile() {
    if cfg!(feature = "speed-probe") {
      return;
    }
    // Set or not, the variable has no effect without the feature.
    assert_eq!(folder(), None);
    assert_eq!(identity_suffix(), "");
  }
}
