use flinch::monitors::MonitorInfo;

fn make_monitor(x: i32, y: i32, w: u32, h: u32, dpi: f64) -> MonitorInfo {
  MonitorInfo {
    id: format!("{},{},{}x{}", x, y, w, h),
    name: format!("Monitor@{},{}", x, y),
    x,
    y,
    width: w,
    height: h,
    is_primary: x == 0 && y == 0,
    dpi_scale: dpi,
    logical_x: (x as f64 / dpi).round() as i32,
    logical_y: (y as f64 / dpi).round() as i32,
    logical_width: (w as f64 / dpi).round() as u32,
    logical_height: (h as f64 / dpi).round() as u32,
  }
}

#[test]
fn test_single_monitor_full_overlap() {
  let monitors = vec![make_monitor(0, 0, 1920, 1080, 1.0)];
  let result = flinch::monitors::monitors_in_rect(&monitors, 100, 100, 800, 600);
  assert_eq!(result.len(), 1);
}

#[test]
fn test_two_monitors_selection_spans_both() {
  let monitors = vec![
    make_monitor(0, 0, 1920, 1080, 1.0),
    make_monitor(1920, 0, 2560, 1440, 1.0),
  ];
  let result = flinch::monitors::monitors_in_rect(&monitors, 1800, 200, 400, 600);
  assert_eq!(result.len(), 2);
}

#[test]
fn test_no_overlap_returns_empty() {
  let monitors = vec![make_monitor(0, 0, 1920, 1080, 1.0)];
  let result = flinch::monitors::monitors_in_rect(&monitors, 2000, 0, 800, 600);
  assert!(result.is_empty());
}

#[test]
fn test_touching_edge_only_no_overlap() {
  let monitors = vec![
    make_monitor(0, 0, 1920, 1080, 1.0),
    make_monitor(1920, 0, 2560, 1440, 1.0),
  ];
  let result = flinch::monitors::monitors_in_rect(&monitors, 1920, 0, 800, 600);
  assert_eq!(result.len(), 1); // Only monitor 2
}

#[test]
fn test_negative_coordinates() {
  let monitors = vec![
    make_monitor(-1920, 0, 1920, 1080, 1.0),
    make_monitor(0, 0, 1920, 1080, 1.0),
  ];
  let result = flinch::monitors::monitors_in_rect(&monitors, -100, 200, 200, 400);
  assert_eq!(result.len(), 2);
}

#[test]
fn test_sorted_left_to_right_top_to_bottom() {
  let monitors = vec![
    make_monitor(1920, 0, 1920, 1080, 1.0),
    make_monitor(0, 0, 1920, 1080, 1.0),
  ];
  let result = flinch::monitors::monitors_in_rect(&monitors, 0, 0, 3840, 1080);
  assert_eq!(result.len(), 2);
  assert_eq!(result[0].logical_x, 0);
  assert_eq!(result[1].logical_x, 1920);
}
