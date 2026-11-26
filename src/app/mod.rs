// src/app/mod.rs
// Application setup and lifecycle management

pub mod bootstrap;
pub mod context;
#[cfg(test)]
pub(crate) mod headless;
pub mod hotkeys;
pub mod icon;
pub mod instance;
pub mod platform;
pub mod runtime;
pub mod tray;
pub(crate) mod tray_menu;
mod tray_theme;

// Re-export public API
pub use tray::{apply_tray_labels, setup_tray_icon, update_tray_recording_state};
