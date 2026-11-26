//! Isolated Windows recording engine. No UI, global hooks or FFmpeg dependencies.
pub mod artifact;
pub mod audio;
pub mod clicks;
pub mod client;
mod control;
pub mod convert;
pub mod gif;
pub mod interop;
pub mod limits;
pub mod metrics;
pub mod protocol;
pub mod recovery;
pub mod session;
pub mod thumbnail;
pub mod time;
pub mod types;
mod watchdog;
#[cfg(windows)]
pub mod windows;

pub use types::{EngineError, ErrorKind, Result};
