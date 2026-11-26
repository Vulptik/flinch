//! Screenshot capture commands
//!
//! ## Purpose
//! Provides typed operations for screenshot capture including:
//! - Area selection screenshots
//! - Fullscreen screenshots
//! - Active window screenshots
//!
//! ## Module Structure
//! - `capture` - Main screenshot capture commands
//! - `post_capture` - Post-capture handling (save, clipboard)
//!
// Capture operations shared by the native host and shortcut handlers.
pub mod capture;
mod color_pick;
mod copy_text;
pub(crate) mod post_capture;

// Re-export for convenient crate-level access
pub use capture::{
  take_active_window_screenshot_for_setup, take_capture_and_edit_for_setup,
  take_color_pick_for_setup, take_copy_text_for_setup, take_fullscreen_screenshot_for_setup,
  take_last_region_for_setup, take_screenshot_for_setup,
};
/// The offscreen replay shows the same notification swatch a real pick does.
#[cfg(test)]
pub(crate) use color_pick::swatch as color_swatch;
