pub mod backend;
pub mod compositor;
pub mod dxgi_backend;
pub mod gdi_fallback;
pub mod probe;

pub use backend::{CaptureBackend, FrameAcquireResult, MockCaptureBackend};
pub use compositor::{
  CaptureResult, CompositorConfig, MonitorSlice, MultiMonitorCompositor, SliceGeometry,
};
pub use gdi_fallback::GdiMonitorBackend;
pub use probe::{ProbeResult, auto_crop_to_supported, probe_backend_for_monitor, probe_monitors};
