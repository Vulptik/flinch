use crate::monitors::MonitorInfo;

/// Place a toolbar near a selection while keeping it fully reachable on the
/// monitor that owns the selection.
#[allow(clippy::too_many_arguments)]
pub fn place_toolbar_near_selection(
  monitors: &[MonitorInfo],
  vd_offset_x: i32,
  vd_offset_y: i32,
  selection: (i32, i32, i32, i32),
  toolbar_size: (i32, i32),
  gap: i32,
  margin: i32,
  overlay_size: (i32, i32),
) -> (i32, i32) {
  let (sel_l, sel_t, sel_r, sel_b) = selection;
  let (toolbar_w, toolbar_h) = toolbar_size;
  let (overlay_w, overlay_h) = overlay_size;

  let monitor = pick_monitor_for_selection(monitors, vd_offset_x, vd_offset_y, selection)
    .map(|monitor| {
      (
        monitor.x - vd_offset_x,
        monitor.y - vd_offset_y,
        monitor.x - vd_offset_x + monitor.width as i32,
        monitor.y - vd_offset_y + monitor.height as i32,
      )
    })
    .unwrap_or((0, 0, overlay_w, overlay_h));

  let try_pos = |left: i32, top: i32| -> Option<(i32, i32)> {
    if left >= monitor.0
      && top >= monitor.1
      && left + toolbar_w <= monitor.2
      && top + toolbar_h <= monitor.3
    {
      Some((left, top))
    } else {
      None
    }
  };

  try_pos(sel_r - toolbar_w, sel_b + gap)
    .or_else(|| try_pos(sel_r - toolbar_w, sel_t - gap - toolbar_h))
    .or_else(|| try_pos(sel_l, sel_b + gap))
    .or_else(|| try_pos(sel_l, sel_t - gap - toolbar_h))
    .or_else(|| try_pos(sel_r + gap, sel_b - toolbar_h))
    .or_else(|| try_pos(sel_l - gap - toolbar_w, sel_b - toolbar_h))
    .unwrap_or_else(|| {
      let max_left = (monitor.2 - toolbar_w).max(monitor.0);
      let max_top = (monitor.3 - toolbar_h).max(monitor.1);
      (
        (sel_r - toolbar_w - margin).clamp(monitor.0, max_left),
        (sel_t + margin).clamp(monitor.1, max_top),
      )
    })
}

fn pick_monitor_for_selection(
  monitors: &[MonitorInfo],
  vd_offset_x: i32,
  vd_offset_y: i32,
  selection: (i32, i32, i32, i32),
) -> Option<MonitorInfo> {
  if monitors.is_empty() {
    return None;
  }

  let (sel_l, sel_t, sel_r, sel_b) = selection;
  let screen_l = sel_l + vd_offset_x;
  let screen_t = sel_t + vd_offset_y;
  let screen_r = sel_r + vd_offset_x;
  let screen_b = sel_b + vd_offset_y;
  let center_x = screen_l + (screen_r - screen_l) / 2;
  let center_y = screen_t + (screen_b - screen_t) / 2;

  if let Some(monitor) = monitors
    .iter()
    .find(|monitor| monitor.contains_point(center_x, center_y))
  {
    return Some(monitor.clone());
  }

  let mut best_overlap: Option<(i64, &MonitorInfo)> = None;
  for monitor in monitors {
    let overlap_w =
      (screen_r.min(monitor.x + monitor.width as i32) - screen_l.max(monitor.x)).max(0);
    let overlap_h =
      (screen_b.min(monitor.y + monitor.height as i32) - screen_t.max(monitor.y)).max(0);
    let overlap_area = i64::from(overlap_w) * i64::from(overlap_h);
    if overlap_area > 0 {
      match best_overlap {
        Some((best_area, _)) if overlap_area <= best_area => {},
        _ => best_overlap = Some((overlap_area, monitor)),
      }
    }
  }
  if let Some((_, monitor)) = best_overlap {
    return Some(monitor.clone());
  }

  monitors
    .iter()
    .min_by_key(|monitor| squared_distance_to_rect(center_x, center_y, monitor))
    .cloned()
}

fn squared_distance_to_rect(x: i32, y: i32, monitor: &MonitorInfo) -> i64 {
  let left = monitor.x;
  let top = monitor.y;
  let right = monitor.x + monitor.width as i32;
  let bottom = monitor.y + monitor.height as i32;

  let clamped_x = x.clamp(left, right.saturating_sub(1));
  let clamped_y = y.clamp(top, bottom.saturating_sub(1));
  let dx = i64::from(x - clamped_x);
  let dy = i64::from(y - clamped_y);
  dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
  use super::place_toolbar_near_selection;
  use crate::monitors::MonitorInfo;

  fn monitor(id: &str, x: i32, y: i32, width: u32, height: u32, is_primary: bool) -> MonitorInfo {
    MonitorInfo {
      id: id.to_string(),
      name: id.to_string(),
      x,
      y,
      width,
      height,
      is_primary,
      dpi_scale: 1.0,
      logical_x: x,
      logical_y: y,
      logical_width: width,
      logical_height: height,
    }
  }

  #[test]
  fn tall_regions_use_free_space_beside_the_capture() {
    let monitors = [monitor("primary", 0, 0, 1920, 1080, true)];
    assert_eq!(
      place_toolbar_near_selection(
        &monitors,
        0,
        0,
        (100, 0, 800, 1080),
        (428, 46),
        6,
        4,
        (1920, 1080)
      ),
      (806, 1034)
    );
    assert_eq!(
      place_toolbar_near_selection(
        &monitors,
        0,
        0,
        (1100, 0, 1800, 1080),
        (428, 46),
        6,
        4,
        (1920, 1080)
      ),
      (666, 1034)
    );
  }

  #[test]
  fn prefers_bottom_right_when_it_fits_on_selection_monitor() {
    let monitors = vec![monitor("primary", 0, 0, 1920, 1080, true)];

    let position = place_toolbar_near_selection(
      &monitors,
      0,
      0,
      (100, 100, 900, 700),
      (230, 34),
      6,
      4,
      (1920, 1080),
    );

    assert_eq!(position, (670, 706));
  }

  #[test]
  fn keeps_toolbar_on_shorter_monitor_in_mixed_height_layout() {
    let monitors = vec![
      monitor("left", 0, 0, 2560, 1440, true),
      monitor("right", 2560, 0, 1920, 1080, false),
    ];

    let position = place_toolbar_near_selection(
      &monitors,
      0,
      0,
      (3000, 900, 3800, 1070),
      (230, 34),
      6,
      4,
      (4480, 1440),
    );

    assert_eq!(position, (3570, 860));
    assert!(position.1 + 34 <= 1080);
  }

  #[test]
  fn fallback_is_clamped_inside_monitor_when_selection_is_too_small() {
    let monitors = vec![monitor("primary", 0, 0, 1920, 1080, true)];

    let position = place_toolbar_near_selection(
      &monitors,
      0,
      0,
      (1800, 1050, 1910, 1075),
      (230, 34),
      6,
      4,
      (1920, 1080),
    );

    assert_eq!(position, (1680, 1010));
    assert!(position.0 >= 0);
    assert!(position.1 >= 0);
    assert!(position.0 + 230 <= 1920);
    assert!(position.1 + 34 <= 1080);
  }
}
