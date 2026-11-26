use super::backend::CaptureBackend;
use super::dxgi_backend::DxgiCaptureBackend;
use super::gdi_fallback::GdiMonitorBackend;
use crate::monitors::MonitorInfo;

pub struct ProbeResult {
  pub supported: Vec<MonitorInfo>,
  pub unsupported: Vec<MonitorInfo>,
}

impl ProbeResult {
  pub fn all_supported(&self) -> bool {
    self.unsupported.is_empty()
  }
  pub fn none_supported(&self) -> bool {
    self.supported.is_empty()
  }
}

/// Probe each monitor for DXGI support. ~1-5ms per monitor.
pub fn probe_monitors(monitors: &[MonitorInfo]) -> ProbeResult {
  let mut supported = Vec::new();
  let mut unsupported = Vec::new();
  for monitor in monitors {
    match DxgiCaptureBackend::probe(monitor) {
      Ok(_) => supported.push(monitor.clone()),
      Err(e) => {
        tracing::warn!("DXGI probe failed for '{}': {:#}", monitor.name, e);
        unsupported.push(monitor.clone());
      },
    }
  }
  ProbeResult {
    supported,
    unsupported,
  }
}

/// Select the best available capture backend for a single monitor.
///
/// Probes DXGI Desktop Duplication first. If the probe fails (e.g. virtual
/// display, software-rendered adapter, network-streamed display), falls back to GDI BitBlt
/// which works on all Windows outputs.
///
/// Returns `(backend, is_gdi)` where `is_gdi` is `true` when the GDI
/// fallback was selected. Callers should pass this flag to
/// [`MonitorSlice::new`] so the compositor's escalation path knows the
/// slice is already at the terminal backend.
///
/// This runs once at recording start — roughly 5–50 ms per monitor.
pub fn probe_backend_for_monitor(monitor: &MonitorInfo) -> (Box<dyn CaptureBackend>, bool) {
  match DxgiCaptureBackend::probe(monitor) {
    Ok(()) => {
      tracing::info!(
          monitor = %monitor.name,
          "Monitor backend: DXGI"
      );
      (Box::new(DxgiCaptureBackend::new(monitor.clone())), false)
    },
    Err(e) => {
      tracing::warn!(
          monitor = %monitor.name,
          error = %e,
          "Monitor backend: DXGI unavailable, using GDI fallback"
      );
      (Box::new(GdiMonitorBackend::new(monitor)), true)
    },
  }
}

/// Auto-crop selection to only cover supported monitors.
/// Returns new (x, y, width, height) in logical coords, or None.
pub fn auto_crop_to_supported(
  supported: &[MonitorInfo],
  sel_x: i32,
  sel_y: i32,
  sel_width: u32,
  sel_height: u32,
) -> Option<(i32, i32, u32, u32)> {
  if supported.is_empty() {
    return None;
  }
  let sel_right = sel_x + sel_width as i32;
  let sel_bottom = sel_y + sel_height as i32;
  let mut min_x = i32::MAX;
  let mut min_y = i32::MAX;
  let mut max_x = i32::MIN;
  let mut max_y = i32::MIN;
  for m in supported {
    let ix = sel_x.max(m.logical_x);
    let iy = sel_y.max(m.logical_y);
    let ix_right = sel_right.min(m.logical_x + m.logical_width as i32);
    let iy_bottom = sel_bottom.min(m.logical_y + m.logical_height as i32);
    if ix < ix_right && iy < iy_bottom {
      min_x = min_x.min(ix);
      min_y = min_y.min(iy);
      max_x = max_x.max(ix_right);
      max_y = max_y.max(iy_bottom);
    }
  }
  if min_x >= max_x || min_y >= max_y {
    return None;
  }
  Some((min_x, min_y, (max_x - min_x) as u32, (max_y - min_y) as u32))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_monitor(
    id: &str,
    logical_x: i32,
    logical_y: i32,
    logical_width: u32,
    logical_height: u32,
  ) -> MonitorInfo {
    MonitorInfo {
      id: id.to_string(),
      name: id.to_string(),
      x: logical_x,
      y: logical_y,
      width: logical_width,
      height: logical_height,
      is_primary: logical_x == 0 && logical_y == 0,
      dpi_scale: 1.0,
      logical_x,
      logical_y,
      logical_width,
      logical_height,
    }
  }

  #[test]
  fn probe_result_helpers_reflect_supported_sets() {
    let left = make_monitor("left", -1920, 0, 1920, 1080);
    let right = make_monitor("right", 0, 0, 1920, 1080);

    let all_supported = ProbeResult {
      supported: vec![left.clone(), right.clone()],
      unsupported: vec![],
    };
    assert!(all_supported.all_supported());
    assert!(!all_supported.none_supported());

    let none_supported = ProbeResult {
      supported: vec![],
      unsupported: vec![left, right],
    };
    assert!(!none_supported.all_supported());
    assert!(none_supported.none_supported());
  }

  #[test]
  fn auto_crop_returns_none_when_no_supported_monitors_exist() {
    assert_eq!(auto_crop_to_supported(&[], 0, 0, 640, 480), None);
  }

  #[test]
  fn auto_crop_returns_none_when_selection_does_not_intersect_supported_monitors() {
    let supported = vec![make_monitor("primary", 0, 0, 1920, 1080)];

    assert_eq!(auto_crop_to_supported(&supported, 2500, 50, 200, 200), None);
  }

  #[test]
  fn auto_crop_clamps_selection_to_single_supported_monitor() {
    let supported = vec![make_monitor("primary", 0, 0, 1920, 1080)];

    assert_eq!(
      auto_crop_to_supported(&supported, -50, 100, 300, 200),
      Some((0, 100, 250, 200))
    );
  }

  #[test]
  fn auto_crop_merges_overlaps_across_multiple_supported_monitors() {
    let supported = vec![
      make_monitor("left", -1280, 0, 1280, 1024),
      make_monitor("right", 0, 0, 1920, 1080),
    ];

    assert_eq!(
      auto_crop_to_supported(&supported, -400, 100, 900, 700),
      Some((-400, 100, 900, 700))
    );
  }

  #[test]
  fn auto_crop_ignores_monitors_without_area_intersection() {
    let supported = vec![
      make_monitor("top", 0, -900, 1600, 900),
      make_monitor("primary", 0, 0, 1920, 1080),
      make_monitor("far-right", 2500, 0, 1600, 900),
    ];

    assert_eq!(
      auto_crop_to_supported(&supported, 100, -100, 600, 300),
      Some((100, -100, 600, 300))
    );
  }
}
