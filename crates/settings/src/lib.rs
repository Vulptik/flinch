//! Transport-independent settings contracts. The UI is optional so capture code
//! can use these types without depending on Slint or another window runtime.
pub mod catalog;
pub mod dispatch;
pub mod edit;
pub mod microphones;
pub mod model;
pub mod preview;
pub mod save;

#[cfg(feature = "ui")]
pub mod ui;

#[cfg(feature = "ui")]
pub mod interaction_check;

#[cfg(feature = "ui")]
pub mod windows;

#[cfg(feature = "ui")]
pub use slint;
