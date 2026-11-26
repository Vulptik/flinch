//! Shortcut handler functions for screenshot, video, GIF, and system actions
//!
//! ## Purpose
//! This module contains the actual handler implementations for global shortcuts.
//! Each handler is invoked when its registered shortcut is pressed.
//!
//! ## Structure
//! - `common` - Shared debounce guard infrastructure
//! - `screenshot` - Screenshot handlers (area, capture and edit, fullscreen, active_window)
//! - `video` - Video recording handlers (area, fullscreen, active_window)
//! - `gif` - GIF recording handler
//! - `system` - PrintScreen suppression handlers
//!
//! ## Debounce Pattern
//! All handlers use a 300ms debounce via `DebounceGuard` to prevent
//! double-triggers from held keys or rapid presses.

mod common;
mod gif;
mod screenshot;
mod system;
mod video;

// Re-export all handler registration functions for use by registration.rs
pub(super) use gif::register_gif_shortcut;
pub(super) use screenshot::{
  register_active_window_shortcut, register_area_shortcut, register_capture_edit_shortcut,
  register_color_picker_shortcut, register_copy_text_shortcut, register_fullscreen_shortcut,
  register_last_region_shortcut,
};

pub(super) use video::{
  register_record_active_window_shortcut, register_record_area_shortcut,
  register_record_fullscreen_shortcut,
};

// Re-export public API items
pub use system::{register_printscreen_noop, unregister_printscreen_noop};
