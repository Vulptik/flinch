use crate::compositing::{CompositorConfig, MultiMonitorCompositor, probe};
use crate::monitors::{self, MonitorInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureCoordinateSpace {
  Logical,
  Physical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedCapturePlanInput {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
  pub show_cursor: bool,
  pub coordinate_space: CaptureCoordinateSpace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedCapturePlanOptions {
  pub min_monitors_for_compositor: usize,
}

impl SharedCapturePlanOptions {
  pub const VIDEO: Self = Self {
    min_monitors_for_compositor: 2,
  };

  pub const GIF: Self = Self {
    min_monitors_for_compositor: 1,
  };
}

#[derive(Debug, Clone, PartialEq)]
pub struct SharedCapturePlan {
  pub capture_x: i32,
  pub capture_y: i32,
  pub capture_width: u32,
  pub capture_height: u32,
  pub monitor_ids: Option<Vec<String>>,
  pub compositor_region: Option<(i32, i32, u32, u32)>,
  pub output_width: u32,
  pub output_height: u32,
}

impl SharedCapturePlan {
  pub fn single(input: SharedCapturePlanInput) -> Self {
    Self {
      capture_x: input.x,
      capture_y: input.y,
      capture_width: input.width,
      capture_height: input.height,
      monitor_ids: None,
      compositor_region: None,
      output_width: input.width,
      output_height: input.height,
    }
  }
}

pub fn build_shared_capture_plan(
  input: SharedCapturePlanInput,
  options: SharedCapturePlanOptions,
  monitors: &[MonitorInfo],
) -> SharedCapturePlan {
  let overlapping = overlapping_monitors(monitors, input);
  if overlapping.len() <= 1 {
    return SharedCapturePlan::single(input);
  }

  let probe_result = probe::probe_monitors(&overlapping);
  build_plan_from_probe(input, options, overlapping, probe_result)
}

fn build_plan_from_probe(
  input: SharedCapturePlanInput,
  options: SharedCapturePlanOptions,
  overlapping: Vec<MonitorInfo>,
  probe_result: probe::ProbeResult,
) -> SharedCapturePlan {
  let mut capture_x = input.x;
  let mut capture_y = input.y;
  let mut capture_width = input.width;
  let mut capture_height = input.height;

  let supported_monitors = if probe_result.all_supported() {
    Some(overlapping)
  } else if probe_result.none_supported() {
    None
  } else {
    auto_crop_supported_region(
      &probe_result.supported,
      input.coordinate_space,
      capture_x,
      capture_y,
      capture_width,
      capture_height,
    )
    .map(|(x, y, width, height)| {
      capture_x = x;
      capture_y = y;
      capture_width = width;
      capture_height = height;
      probe_result.supported
    })
  };

  let Some(supported_monitors) = supported_monitors else {
    return SharedCapturePlan::single(input);
  };

  let Some(compositor_region) = physical_rect_to_logical_region(
    &supported_monitors,
    input.coordinate_space,
    capture_x,
    capture_y,
    capture_width,
    capture_height,
  ) else {
    return SharedCapturePlan {
      capture_x,
      capture_y,
      capture_width,
      capture_height,
      monitor_ids: None,
      compositor_region: None,
      output_width: capture_width,
      output_height: capture_height,
    };
  };

  let compositor_config = CompositorConfig {
    selection_x: compositor_region.0,
    selection_y: compositor_region.1,
    selection_width: compositor_region.2,
    selection_height: compositor_region.3,
    show_cursor: input.show_cursor,
  };

  match MultiMonitorCompositor::compute_slices(&supported_monitors, &compositor_config) {
    Ok((_slices, output_width, output_height))
      if supported_monitors.len() >= options.min_monitors_for_compositor =>
    {
      SharedCapturePlan {
        capture_x,
        capture_y,
        capture_width,
        capture_height,
        monitor_ids: Some(
          supported_monitors
            .iter()
            .map(|monitor| monitor.id.clone())
            .collect(),
        ),
        compositor_region: Some(compositor_region),
        output_width,
        output_height,
      }
    },
    Ok(_) | Err(_) => SharedCapturePlan {
      capture_x,
      capture_y,
      capture_width,
      capture_height,
      monitor_ids: None,
      compositor_region: None,
      output_width: capture_width,
      output_height: capture_height,
    },
  }
}

fn overlapping_monitors(
  monitors: &[MonitorInfo],
  input: SharedCapturePlanInput,
) -> Vec<MonitorInfo> {
  match input.coordinate_space {
    CaptureCoordinateSpace::Logical => {
      monitors::monitors_in_rect(monitors, input.x, input.y, input.width, input.height)
    },
    CaptureCoordinateSpace::Physical => {
      let rect_right = input.x.saturating_add(input.width as i32);
      let rect_bottom = input.y.saturating_add(input.height as i32);

      let mut result: Vec<_> = monitors
        .iter()
        .filter(|monitor| {
          let monitor_right = monitor.x.saturating_add(monitor.width as i32);
          let monitor_bottom = monitor.y.saturating_add(monitor.height as i32);
          let overlap_x = input.x.max(monitor.x) < rect_right.min(monitor_right);
          let overlap_y = input.y.max(monitor.y) < rect_bottom.min(monitor_bottom);
          overlap_x && overlap_y
        })
        .cloned()
        .collect();

      result.sort_by(|left, right| left.x.cmp(&right.x).then(left.y.cmp(&right.y)));
      result
    },
  }
}

fn auto_crop_supported_region(
  supported: &[MonitorInfo],
  coordinate_space: CaptureCoordinateSpace,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
) -> Option<(i32, i32, u32, u32)> {
  match coordinate_space {
    CaptureCoordinateSpace::Logical => {
      probe::auto_crop_to_supported(supported, x, y, width, height)
    },
    CaptureCoordinateSpace::Physical => {
      auto_crop_physical_to_supported(supported, x, y, width, height)
    },
  }
}

fn auto_crop_physical_to_supported(
  supported: &[MonitorInfo],
  rect_x: i32,
  rect_y: i32,
  rect_width: u32,
  rect_height: u32,
) -> Option<(i32, i32, u32, u32)> {
  let rect_right = rect_x.saturating_add(rect_width as i32);
  let rect_bottom = rect_y.saturating_add(rect_height as i32);
  let mut min_x = i32::MAX;
  let mut min_y = i32::MAX;
  let mut max_x = i32::MIN;
  let mut max_y = i32::MIN;

  for monitor in supported {
    let intersect_x = rect_x.max(monitor.x);
    let intersect_y = rect_y.max(monitor.y);
    let intersect_right = rect_right.min(monitor.x.saturating_add(monitor.width as i32));
    let intersect_bottom = rect_bottom.min(monitor.y.saturating_add(monitor.height as i32));
    if intersect_x < intersect_right && intersect_y < intersect_bottom {
      min_x = min_x.min(intersect_x);
      min_y = min_y.min(intersect_y);
      max_x = max_x.max(intersect_right);
      max_y = max_y.max(intersect_bottom);
    }
  }

  if min_x >= max_x || min_y >= max_y {
    return None;
  }

  Some((min_x, min_y, (max_x - min_x) as u32, (max_y - min_y) as u32))
}

fn physical_rect_to_logical_region(
  monitors: &[MonitorInfo],
  coordinate_space: CaptureCoordinateSpace,
  rect_x: i32,
  rect_y: i32,
  rect_width: u32,
  rect_height: u32,
) -> Option<(i32, i32, u32, u32)> {
  match coordinate_space {
    CaptureCoordinateSpace::Logical => Some((rect_x, rect_y, rect_width, rect_height)),
    CaptureCoordinateSpace::Physical => {
      let reference_dpi = monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .map(|monitor| monitor.dpi_scale)
        .or_else(|| monitors.first().map(|monitor| monitor.dpi_scale))?;

      let logical_width = (rect_width as f64 / reference_dpi).round() as u32;
      let logical_height = (rect_height as f64 / reference_dpi).round() as u32;
      if logical_width == 0 || logical_height == 0 {
        return None;
      }

      Some((
        (rect_x as f64 / reference_dpi).round() as i32,
        (rect_y as f64 / reference_dpi).round() as i32,
        logical_width,
        logical_height,
      ))
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn partial_backend_support_crops_to_supported_pixels_and_respects_video_threshold() {
    let good = make_monitor("good", -100, -50, 100, 100, 1.0, false);
    let bad = make_monitor("bad", 0, -50, 100, 100, 1.0, true);
    for coordinate_space in [
      CaptureCoordinateSpace::Physical,
      CaptureCoordinateSpace::Logical,
    ] {
      let input = SharedCapturePlanInput {
        x: -30,
        y: -20,
        width: 60,
        height: 40,
        show_cursor: false,
        coordinate_space,
      };
      for (options, composited) in [
        (SharedCapturePlanOptions::GIF, true),
        (SharedCapturePlanOptions::VIDEO, false),
      ] {
        let plan = build_plan_from_probe(
          input,
          options,
          vec![good.clone(), bad.clone()],
          probe::ProbeResult {
            supported: vec![good.clone()],
            unsupported: vec![bad.clone()],
          },
        );
        assert_eq!(
          (
            plan.capture_x,
            plan.capture_y,
            plan.capture_width,
            plan.capture_height
          ),
          (-30, -20, 30, 40)
        );
        assert_eq!(plan.monitor_ids.is_some(), composited);
        assert_eq!((plan.output_width, plan.output_height), (30, 40));
      }
      for supported in [
        vec![],
        vec![make_monitor("disjoint", 1000, 1000, 100, 100, 1.0, false)],
      ] {
        let plan = build_plan_from_probe(
          input,
          SharedCapturePlanOptions::GIF,
          vec![good.clone(), bad.clone()],
          probe::ProbeResult {
            supported,
            unsupported: vec![bad.clone()],
          },
        );
        assert_eq!(plan, SharedCapturePlan::single(input));
      }
    }
    assert_eq!(
      auto_crop_physical_to_supported(std::slice::from_ref(&good), -200, -100, 300, 300),
      Some((-100, -50, 100, 100))
    );
    assert_eq!(
      physical_rect_to_logical_region(&[], CaptureCoordinateSpace::Physical, 0, 0, 10, 10),
      None
    );
    assert_eq!(
      physical_rect_to_logical_region(
        &[make_monitor("high", 0, 0, 10, 10, 4.0, false)],
        CaptureCoordinateSpace::Physical,
        0,
        0,
        1,
        1
      ),
      None
    );
    let input = SharedCapturePlanInput {
      x: -200,
      y: -100,
      width: 500,
      height: 300,
      show_cursor: false,
      coordinate_space: CaptureCoordinateSpace::Physical,
    };
    assert_eq!(
      overlapping_monitors(&[bad.clone(), good.clone()], input)
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>(),
      vec!["good", "bad"]
    );
  }

  fn make_monitor(
    id: &str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    dpi_scale: f64,
    is_primary: bool,
  ) -> MonitorInfo {
    MonitorInfo {
      id: id.to_string(),
      name: id.to_string(),
      x,
      y,
      width,
      height,
      is_primary,
      dpi_scale,
      logical_x: (x as f64 / dpi_scale).round() as i32,
      logical_y: (y as f64 / dpi_scale).round() as i32,
      logical_width: (width as f64 / dpi_scale).round() as u32,
      logical_height: (height as f64 / dpi_scale).round() as u32,
    }
  }

  #[test]
  fn single_monitor_plan_stays_single() {
    let monitors = vec![make_monitor("primary", 0, 0, 1920, 1080, 1.0, true)];
    let plan = build_shared_capture_plan(
      SharedCapturePlanInput {
        x: 100,
        y: 100,
        width: 500,
        height: 300,
        show_cursor: true,
        coordinate_space: CaptureCoordinateSpace::Logical,
      },
      SharedCapturePlanOptions::VIDEO,
      &monitors,
    );

    assert_eq!(plan.monitor_ids, None);
    assert_eq!(plan.compositor_region, None);
    assert_eq!(plan.capture_width, 500);
    assert_eq!(plan.output_width, 500);
  }

  #[test]
  fn logical_multi_monitor_plan_uses_compositor_when_all_supported() {
    // Use build_plan_from_probe directly to avoid real DXGI hardware probing in unit tests.
    let left = make_monitor("left", -1920, 0, 1920, 1080, 1.0, false);
    let primary = make_monitor("primary", 0, 0, 1920, 1080, 1.0, true);
    let overlapping = vec![left.clone(), primary.clone()];
    let probe_result = probe::ProbeResult {
      supported: overlapping.clone(),
      unsupported: vec![],
    };
    let input = SharedCapturePlanInput {
      x: -400,
      y: 100,
      width: 1000,
      height: 500,
      show_cursor: true,
      coordinate_space: CaptureCoordinateSpace::Logical,
    };

    let plan = build_plan_from_probe(
      input,
      SharedCapturePlanOptions::VIDEO,
      overlapping,
      probe_result,
    );

    assert_eq!(plan.monitor_ids.as_ref().map(Vec::len), Some(2));
    assert_eq!(plan.compositor_region, Some((-400, 100, 1000, 500)));
    assert_eq!(plan.capture_width, 1000);
    assert!(plan.output_width >= 1000);
  }

  #[test]
  fn gif_plan_uses_single_supported_monitor_after_partial_crop() {
    let monitors = [
      make_monitor("primary", 0, 0, 2560, 1440, 1.5, true),
      make_monitor("secondary", 2560, 0, 1920, 1080, 1.0, false),
    ];

    let plan = build_shared_capture_plan(
      SharedCapturePlanInput {
        x: 2400,
        y: 100,
        width: 400,
        height: 300,
        show_cursor: true,
        coordinate_space: CaptureCoordinateSpace::Physical,
      },
      SharedCapturePlanOptions::GIF,
      &monitors[..1],
    );

    assert_eq!(plan.monitor_ids, None);
    assert_eq!(plan.capture_x, 2400);
  }

  #[test]
  fn physical_region_converts_to_logical_using_primary_dpi() {
    let monitors = vec![
      make_monitor("primary", 0, 0, 2560, 1440, 1.5, true),
      make_monitor("secondary", 2560, 0, 1920, 1080, 1.0, false),
    ];

    let region = physical_rect_to_logical_region(
      &monitors,
      CaptureCoordinateSpace::Physical,
      2400,
      100,
      400,
      300,
    );

    assert_eq!(region, Some((1600, 67, 267, 200)));
  }
}
