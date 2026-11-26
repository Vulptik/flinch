// Library entry point for testing and module exposure
// This file allows cargo test to access internal modules

// Re-export public modules
pub(crate) mod autostart;
pub(crate) mod capture_artifact;
pub mod capture_delay;
pub mod capture_plan;
pub mod capture_subfolder;
pub mod commands;
pub mod compositing;
pub mod config;
pub mod dialogs;
pub mod error;
pub mod filename;
pub mod gif;
pub mod i18n;
pub(crate) mod last_region;
pub mod monitors;
pub mod notifications;
pub(crate) mod ocr;
pub mod overlay;
pub mod probe_profile;
pub(crate) mod recent_captures;
pub mod recording;
pub mod recording_visibility;
pub mod screenshot;
pub mod settings;
pub mod shortcuts;
pub mod state;
pub(crate) mod thumbnails;
pub mod video;

// Re-export internal modules needed for testing
pub(crate) mod app;
pub(crate) mod logging;

#[cfg(test)]
mod test_tracing;

/// Start the native application and its owned worker runtime.
pub fn run() -> anyhow::Result<()> {
  if let Err(error) = logging::init_logging() {
    eprintln!("Logging initialization failed: {error}");
  }
  logging::cleanup_old_logs();
  std::panic::set_hook(Box::new(|info| {
    tracing::error!(panic_info = %info, "Process panicked");
  }));
  settings::native_host::run()
}
