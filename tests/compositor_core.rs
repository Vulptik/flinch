//! Integration tests for MultiMonitorCompositor slice computation and capture_frame.

use flinch::compositing::{
  CaptureBackend, CompositorConfig, MockCaptureBackend, MonitorSlice, MultiMonitorCompositor,
};
use flinch::monitors::MonitorInfo;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_monitor(
  id: &str,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  dpi_scale: f64,
  is_primary: bool,
) -> MonitorInfo {
  let logical_width = (width as f64 / dpi_scale).round() as u32;
  let logical_height = (height as f64 / dpi_scale).round() as u32;
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
    logical_width,
    logical_height,
  }
}

fn simple_config(x: i32, y: i32, w: u32, h: u32) -> CompositorConfig {
  CompositorConfig {
    selection_x: x,
    selection_y: y,
    selection_width: w,
    selection_height: h,
    show_cursor: false,
  }
}

// ---------------------------------------------------------------------------
// Test 1: single monitor, selection inside it
// ---------------------------------------------------------------------------

#[test]
fn test_single_monitor_single_slice() {
  // One 1920x1080 monitor at origin, 1:1 DPI.
  let monitors = vec![make_monitor("mon0", 0, 0, 1920, 1080, 1.0, true)];
  let config = simple_config(100, 50, 400, 300);

  let (slices, out_w, out_h) =
    MultiMonitorCompositor::compute_slices(&monitors, &config).expect("should succeed");

  assert_eq!(slices.len(), 1, "expected exactly one slice");

  let s = &slices[0];
  // src_rect should match the selection directly (1:1 DPI)
  assert_eq!(s.src_rect.x, 100);
  assert_eq!(s.src_rect.y, 50);
  assert_eq!(s.src_rect.width, 400);
  assert_eq!(s.src_rect.height, 300);

  // destination offset: selection starts at the selection origin => (0, 0)
  assert_eq!(s.dst_offset, (0, 0));

  // Same DPI — no scale factor needed
  assert!(
    s.scale_factor.is_none(),
    "no scale factor expected at 1:1 DPI"
  );

  // Output dimensions equal the selection
  assert_eq!(out_w, 400);
  assert_eq!(out_h, 300);
}

// ---------------------------------------------------------------------------
// Test 2: two side-by-side monitors, selection spanning both
// ---------------------------------------------------------------------------

#[test]
fn test_two_monitors_spanning_selection() {
  // Monitor A: 0..1920 (logical). Monitor B: 1920..3840 (logical).
  // Both 1920x1080 physical, 1:1 DPI.
  let monitors = vec![
    make_monitor("monA", 0, 0, 1920, 1080, 1.0, true),
    make_monitor("monB", 1920, 0, 1920, 1080, 1.0, false),
  ];

  // Selection: x=1800, y=0, w=240, h=100 — 120px on monA, 120px on monB.
  let config = simple_config(1800, 0, 240, 100);

  let (slices, out_w, out_h) =
    MultiMonitorCompositor::compute_slices(&monitors, &config).expect("should succeed");

  assert_eq!(slices.len(), 2, "expected two slices");

  // Sort by dst_offset.x to get a predictable order (left slice first).
  let mut slices = slices;
  slices.sort_by_key(|s| s.dst_offset.0);

  let left = &slices[0]; // part on monA
  let right = &slices[1]; // part on monB

  // Left slice: 120px from x=1800 inside monA (physical x=1800)
  assert_eq!(left.src_rect.x, 1800, "left src_rect.x");
  assert_eq!(left.src_rect.y, 0, "left src_rect.y");
  assert_eq!(left.src_rect.width, 120, "left src_rect.width");
  assert_eq!(left.src_rect.height, 100, "left src_rect.height");
  assert_eq!(left.dst_offset, (0, 0), "left dst_offset");

  // Right slice: 120px from x=0 inside monB
  assert_eq!(right.src_rect.x, 0, "right src_rect.x");
  assert_eq!(right.src_rect.y, 0, "right src_rect.y");
  assert_eq!(right.src_rect.width, 120, "right src_rect.width");
  assert_eq!(right.src_rect.height, 100, "right src_rect.height");
  assert_eq!(right.dst_offset, (120, 0), "right dst_offset");

  // Output is 240x100
  assert_eq!(out_w, 240);
  assert_eq!(out_h, 100);
}

// ---------------------------------------------------------------------------
// Test 3: selection outside all monitors
// ---------------------------------------------------------------------------

#[test]
fn test_no_overlap_returns_error() {
  let monitors = vec![make_monitor("mon0", 0, 0, 1920, 1080, 1.0, true)];
  // Selection is entirely to the right of the monitor.
  let config = simple_config(2000, 0, 100, 100);

  let result = MultiMonitorCompositor::compute_slices(&monitors, &config);
  assert!(
    result.is_err(),
    "expected an error when selection has no overlap"
  );
  let msg = result.unwrap_err().to_string();
  assert!(
    msg.contains("overlap") || msg.contains("monitor"),
    "error message should mention overlap/monitor: {msg}"
  );
}

// ---------------------------------------------------------------------------
// Test 4: empty monitor list
// ---------------------------------------------------------------------------

#[test]
fn test_empty_monitors_returns_error() {
  let config = simple_config(0, 0, 100, 100);
  let result = MultiMonitorCompositor::compute_slices(&[], &config);
  assert!(result.is_err(), "expected an error for empty monitor list");
}

// ---------------------------------------------------------------------------
// Test 5: capture_frame with mock backends — pixel data composited correctly
// ---------------------------------------------------------------------------

#[test]
fn test_capture_frame_with_mock_backends() {
  // Two side-by-side monitors, each 4x4 physical pixels.
  // Left monitor: solid red [0, 0, 255, 255] (BGRA: B=0, G=0, R=255, A=255)
  // Right monitor: solid blue [255, 0, 0, 255]
  let monitors = vec![
    make_monitor("left", 0, 0, 4, 4, 1.0, true),
    make_monitor("right", 4, 0, 4, 4, 1.0, false),
  ];
  // Select the full 8x4 virtual desktop.
  let config = simple_config(0, 0, 8, 4);

  let (geoms, out_w, out_h) =
    MultiMonitorCompositor::compute_slices(&monitors, &config).expect("compute_slices failed");

  assert_eq!(geoms.len(), 2);
  assert_eq!(out_w, 8);
  assert_eq!(out_h, 4);

  // Build MonitorSlices: pair each geometry with a matching mock backend.
  let slices: Vec<MonitorSlice> = geoms
    .into_iter()
    .map(|geom| {
      // Choose fill colour based on which monitor this geometry belongs to.
      let fill: [u8; 4] = if geom.monitor.id == "left" {
        [0, 0, 255, 255] // BGRA red
      } else {
        [255, 0, 0, 255] // BGRA blue
      };
      let backend: Box<dyn CaptureBackend> = Box::new(MockCaptureBackend::new_solid(
        geom.src_rect.width,
        geom.src_rect.height,
        fill,
      ));
      MonitorSlice::new(geom, backend, false)
    })
    .collect();

  let mut compositor = MultiMonitorCompositor::new(slices, out_w, out_h);
  let result = compositor.capture_frame().expect("capture_frame failed");

  assert!(
    result.changed,
    "capture_frame should report changed=true on first frame"
  );

  let data = result.data;
  assert_eq!(data.len(), (out_w * out_h * 4) as usize);

  // Pixel at (0,0) should be red (BGRA [0,0,255,255]) — left monitor
  assert_eq!(&data[0..4], &[0, 0, 255, 255], "pixel (0,0) should be red");

  // Pixel at (4,0) should be blue (BGRA [255,0,0,255]) — right monitor
  let px4_offset = 4 * 4;
  assert_eq!(
    &data[px4_offset..px4_offset + 4],
    &[255, 0, 0, 255],
    "pixel (4,0) should be blue"
  );
}

// ---------------------------------------------------------------------------
// Test 6: capture_frame with NoChange backend — changed flag is false
// ---------------------------------------------------------------------------

#[test]
fn test_capture_frame_no_change_flag() {
  let monitors = vec![make_monitor("mon0", 0, 0, 4, 4, 1.0, true)];
  let config = simple_config(0, 0, 4, 4);

  let (geoms, out_w, out_h) =
    MultiMonitorCompositor::compute_slices(&monitors, &config).expect("compute_slices failed");

  let slices: Vec<MonitorSlice> = geoms
    .into_iter()
    .map(|geom| {
      let mut mock =
        MockCaptureBackend::new_solid(geom.src_rect.width, geom.src_rect.height, [128; 4]);
      mock.return_no_change = true;
      MonitorSlice::new(geom, Box::new(mock), false)
    })
    .collect();

  let mut compositor = MultiMonitorCompositor::new(slices, out_w, out_h);
  let result = compositor.capture_frame().expect("capture_frame failed");

  assert!(
    !result.changed,
    "capture_frame should report changed=false when backend returns NoChange"
  );
}
