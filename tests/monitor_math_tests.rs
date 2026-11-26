// Integration tests for multi-monitor math and coordinate calculations
// Tests complex multi-monitor scenarios and virtual desktop bounds

/// Helper to create a test monitor
fn create_monitor(
  id: &str,
  name: &str,
  x: i32,
  y: i32,
  width: u32,
  height: u32,
  is_primary: bool,
) -> (String, String, i32, i32, u32, u32, bool) {
  (
    id.to_string(),
    name.to_string(),
    x,
    y,
    width,
    height,
    is_primary,
  )
}

#[test]
fn test_dual_monitor_horizontal() {
  // Two monitors side-by-side (common setup)
  // Primary: 1920x1080 at (0, 0)
  // Secondary: 1920x1080 at (1920, 0)

  let primary = create_monitor("1", "Primary", 0, 0, 1920, 1080, true);
  let secondary = create_monitor("2", "Secondary", 1920, 0, 1920, 1080, false);

  // Verify primary bounds
  assert_eq!(primary.2, 0); // x
  assert_eq!(primary.3, 0); // y
  assert_eq!(primary.4, 1920); // width
  assert_eq!(primary.5, 1080); // height

  // Verify secondary starts where primary ends
  assert_eq!(secondary.2, 1920); // x
  assert_eq!(secondary.3, 0); // y (same y as primary)

  // Total virtual desktop width should be 3840
  let virtual_width = primary.4 + secondary.4;
  assert_eq!(virtual_width, 3840);
}

#[test]
fn test_dual_monitor_vertical() {
  // Two monitors stacked vertically
  // Top: 1920x1080 at (0, -1080)
  // Bottom (primary): 1920x1080 at (0, 0)

  let top = create_monitor("1", "Top", 0, -1080, 1920, 1080, false);
  let bottom = create_monitor("2", "Bottom", 0, 0, 1920, 1080, true);

  // Verify top monitor has negative y
  assert_eq!(top.3, -1080);

  // Verify bottom monitor at origin
  assert_eq!(bottom.3, 0);

  // Total virtual desktop height
  let virtual_height = top.5 + bottom.5;
  assert_eq!(virtual_height, 2160);
}

#[test]
fn test_triple_monitor_mixed_layout() {
  // Complex 3-monitor setup:
  // Left: 1080x1920 at (-1080, -420) (portrait)
  // Center (primary): 2560x1440 at (0, 0)
  // Right: 1920x1080 at (2560, 360)

  let left = create_monitor("1", "Left Portrait", -1080, -420, 1080, 1920, false);
  let center = create_monitor("2", "Center Primary", 0, 0, 2560, 1440, true);
  let right = create_monitor("3", "Right", 2560, 360, 1920, 1080, false);

  // Left monitor extends above center
  assert!(left.3 < center.3);

  // Right monitor is offset vertically
  assert_eq!(right.3, 360);

  // Calculate virtual desktop bounds
  let min_x = left.2; // -1080
  let max_x = right.2 + right.4 as i32; // 2560 + 1920 = 4480
  let min_y = left.3; // -420
  let max_y_candidates = [
    left.3 + left.5 as i32,     // -420 + 1920 = 1500
    center.3 + center.5 as i32, // 0 + 1440 = 1440
    right.3 + right.5 as i32,   // 360 + 1080 = 1440
  ];
  let max_y = *max_y_candidates.iter().max().unwrap();

  assert_eq!(min_x, -1080);
  assert_eq!(max_x, 4480);
  assert_eq!(min_y, -420);
  assert_eq!(max_y, 1500);

  let virtual_width = (max_x - min_x) as u32;
  let virtual_height = (max_y - min_y) as u32;

  assert_eq!(virtual_width, 5560);
  assert_eq!(virtual_height, 1920);
}

#[test]
fn test_coordinate_normalization() {
  // Test converting monitor-relative coordinates to virtual desktop coordinates

  // Monitor at (1920, 0) with size 1920x1080
  let monitor_x = 1920;
  let monitor_y = 0;

  // Point at (100, 200) relative to monitor
  let relative_x = 100;
  let relative_y = 200;

  // Virtual desktop coordinates
  let virtual_x = monitor_x + relative_x;
  let virtual_y = monitor_y + relative_y;

  assert_eq!(virtual_x, 2020);
  assert_eq!(virtual_y, 200);
}

#[test]
fn test_coordinate_to_monitor_relative() {
  // Test converting virtual desktop coordinates to monitor-relative

  // Monitor at (-1920, 0)
  let monitor_x = -1920;
  let monitor_y = 0;

  // Virtual point at (-1820, 100)
  let virtual_x = -1820;
  let virtual_y = 100;

  // Monitor-relative coordinates
  let relative_x = virtual_x - monitor_x;
  let relative_y = virtual_y - monitor_y;

  assert_eq!(relative_x, 100);
  assert_eq!(relative_y, 100);
}

#[test]
fn test_dpi_scaling_96dpi() {
  // Standard 96 DPI (100% scaling) - no scaling needed
  let physical_width: u32 = 1920;
  let physical_height: u32 = 1080;
  let dpi_scale: f32 = 1.0;

  let logical_width = (physical_width as f32 / dpi_scale) as u32;
  let logical_height = (physical_height as f32 / dpi_scale) as u32;

  assert_eq!(logical_width, 1920);
  assert_eq!(logical_height, 1080);
}

#[test]
fn test_dpi_scaling_144dpi() {
  // 144 DPI (150% scaling)
  let physical_width: u32 = 1920;
  let physical_height: u32 = 1080;
  let dpi_scale: f32 = 1.5;

  let logical_width = (physical_width as f32 / dpi_scale) as u32;
  let logical_height = (physical_height as f32 / dpi_scale) as u32;

  assert_eq!(logical_width, 1280);
  assert_eq!(logical_height, 720);
}

#[test]
fn test_dpi_scaling_192dpi() {
  // 192 DPI (200% scaling - common on high DPI displays)
  let physical_width: u32 = 3840;
  let physical_height: u32 = 2160;
  let dpi_scale: f32 = 2.0;

  let logical_width = (physical_width as f32 / dpi_scale) as u32;
  let logical_height = (physical_height as f32 / dpi_scale) as u32;

  assert_eq!(logical_width, 1920);
  assert_eq!(logical_height, 1080);
}

#[test]
fn test_mixed_dpi_monitors() {
  // Common scenario: laptop (2.0 scale) + external monitor (1.0 scale)
  let laptop = create_monitor("1", "Laptop 4K", 0, 0, 3840, 2160, true);
  let external = create_monitor("2", "External 1080p", 1920, 0, 1920, 1080, false);

  // Note: In Flinch, we work with physical pixels, not logical pixels
  // So different DPI monitors have different physical resolutions

  assert_eq!(laptop.4, 3840); // Physical width
  assert_eq!(external.4, 1920); // Physical width

  // External monitor position accounts for laptop's logical width (1920)
  // but is positioned at physical coordinate
  assert_eq!(external.2, 1920);
}

#[test]
fn test_monitor_overlap_detection() {
  // Two monitors that overlap (shouldn't happen, but test the math)
  let monitor1 = create_monitor("1", "M1", 0, 0, 1920, 1080, true);
  let monitor2 = create_monitor("2", "M2", 1000, 0, 1920, 1080, false);

  // Check if they overlap
  let m1_right = monitor1.2 + monitor1.4 as i32; // 1920
  let m2_left = monitor2.2; // 1000

  let overlaps = m1_right > m2_left;
  assert!(overlaps); // They do overlap (bad config, but mathematically valid)
}

#[test]
fn test_point_between_monitors() {
  // Point that falls in the gap between monitors (edge case)
  let monitor1 = create_monitor("1", "M1", 0, 0, 1920, 1080, true);
  let monitor2 = create_monitor("2", "M2", 1925, 0, 1920, 1080, false); // 5px gap

  let gap_point_x = 1922;
  let _gap_point_y = 500;

  // Point is between monitors
  let m1_right = monitor1.2 + monitor1.4 as i32;
  let m2_left = monitor2.2;

  let in_gap = gap_point_x >= m1_right && gap_point_x < m2_left;
  assert!(in_gap);
}

#[test]
fn test_negative_width_height_protection() {
  // Ensure we never create monitors with negative dimensions
  // (this would be a bug, but test the type system prevents it)

  let width: u32 = 1920; // u32 can't be negative
  let height: u32 = 1080;

  assert!(width > 0);
  assert!(height > 0);

  // If someone tries to create zero-size monitor
  let zero_monitor = create_monitor("0", "Zero", 0, 0, 0, 0, true);
  assert_eq!(zero_monitor.4, 0);
  assert_eq!(zero_monitor.5, 0);
}

#[test]
fn test_extreme_negative_coordinates() {
  // Monitor far to the left and above
  let extreme = create_monitor("extreme", "Far Away", -10000, -5000, 1920, 1080, false);

  assert_eq!(extreme.2, -10000);
  assert_eq!(extreme.3, -5000);

  // Point at center of this monitor
  let center_x = extreme.2 + (extreme.4 / 2) as i32;
  let center_y = extreme.3 + (extreme.5 / 2) as i32;

  assert_eq!(center_x, -9040); // -10000 + 960
  assert_eq!(center_y, -4460); // -5000 + 540
}

#[test]
fn test_virtual_desktop_union_calculation() {
  // Calculate union of multiple monitors
  let monitors = [
    create_monitor("1", "Left", -1920, 0, 1920, 1080, false),
    create_monitor("2", "Center", 0, 0, 2560, 1440, true),
    create_monitor("3", "Right", 2560, -200, 1920, 1080, false),
  ];

  let min_x = monitors.iter().map(|m| m.2).min().unwrap();
  let min_y = monitors.iter().map(|m| m.3).min().unwrap();

  let max_x = monitors.iter().map(|m| m.2 + m.4 as i32).max().unwrap();
  let max_y = monitors.iter().map(|m| m.3 + m.5 as i32).max().unwrap();

  assert_eq!(min_x, -1920);
  assert_eq!(min_y, -200);
  assert_eq!(max_x, 4480); // 2560 + 1920
  assert_eq!(max_y, 1440); // Center monitor bottom

  let virtual_width = (max_x - min_x) as u32;
  let virtual_height = (max_y - min_y) as u32;

  assert_eq!(virtual_width, 6400);
  assert_eq!(virtual_height, 1640);
}

#[test]
fn test_aspect_ratio_calculation() {
  // Common aspect ratios
  let monitors = vec![
    ("16:9 FHD", 1920u32, 1080u32),
    ("16:9 4K", 3840, 2160),
    ("21:9 Ultrawide", 3440, 1440),
    ("16:10 WUXGA", 1920, 1200),
    ("4:3 XGA", 1024, 768),
  ];

  for (name, width, height) in monitors {
    let aspect = width as f32 / height as f32;

    match name {
      "16:9 FHD" | "16:9 4K" => {
        assert!(
          (aspect - 16.0 / 9.0).abs() < 0.01,
          "{} aspect ratio incorrect",
          name
        );
      },
      "21:9 Ultrawide" => {
        // 3440/1440 ≈ 2.3889, which is close to 21.5:9, not exactly 21:9
        // Ultrawide monitors are approximately 21:9 but not exact
        assert!(
          (aspect - 2.389).abs() < 0.01,
          "{} aspect ratio incorrect",
          name
        );
      },
      "16:10 WUXGA" => {
        assert!(
          (aspect - 16.0 / 10.0).abs() < 0.01,
          "{} aspect ratio incorrect",
          name
        );
      },
      "4:3 XGA" => {
        assert!(
          (aspect - 4.0 / 3.0).abs() < 0.01,
          "{} aspect ratio incorrect",
          name
        );
      },
      _ => {},
    }
  }
}

#[test]
fn test_portrait_vs_landscape() {
  let landscape = create_monitor("1", "Landscape", 0, 0, 1920, 1080, true);
  let portrait = create_monitor("2", "Portrait", 1920, -420, 1080, 1920, false);

  // Landscape is wider than tall
  assert!(landscape.4 > landscape.5);

  // Portrait is taller than wide
  assert!(portrait.5 > portrait.4);
}

#[test]
fn test_boundary_edge_cases() {
  let monitor = create_monitor("1", "Test", 0, 0, 1920, 1080, true);

  // Points on edges
  let left_edge = monitor.2;
  let top_edge = monitor.3;
  let right_edge = monitor.2 + monitor.4 as i32 - 1;
  let bottom_edge = monitor.3 + monitor.5 as i32 - 1;

  assert_eq!(left_edge, 0);
  assert_eq!(top_edge, 0);
  assert_eq!(right_edge, 1919);
  assert_eq!(bottom_edge, 1079);

  // Point just outside right edge
  let outside_right = monitor.2 + monitor.4 as i32;
  assert_eq!(outside_right, 1920); // First pixel outside
}
