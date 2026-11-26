//! Global shortcut registration module
//!
//! Uses the native Windows hotkey adapter on the platform owner thread.
//!
//! ## Structure
//! - `handlers/` - Individual shortcut handler implementations
//! - `normalize` - Shortcut string normalization utilities
//! - `registration` - Shortcut registration and lifecycle management
//! - `state` - Shortcut registration state tracking

mod handlers;
mod normalize;
mod registration;
pub(crate) mod registry;
pub mod state;

// Re-export public API
pub use handlers::{register_printscreen_noop, unregister_printscreen_noop};
pub use registration::{
  setup_global_shortcuts, setup_global_shortcuts_delayed, test_shortcut_registration,
  unregister_all_shortcuts,
};
pub use state::{ShortcutInfo, ShortcutStateHolder};
