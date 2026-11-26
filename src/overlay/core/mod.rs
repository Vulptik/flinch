//! Core overlay infrastructure - shared utilities for all overlay types
//!
//! ## Module Structure
//! - `window_detection` - Detect windows under cursor for snap-to-window
//! - `placement` - Shared monitor-aware placement helpers for overlay chrome

pub mod placement;
pub mod window_detection;

pub use placement::place_toolbar_near_selection;
pub use window_detection::{
  DetectedWindow, detect_window_at_overlay_point, detect_window_at_point,
};
