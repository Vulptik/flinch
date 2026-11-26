//! Rectangle and ellipse editing in frozen screenshot coordinates.
use super::annotations::{Annotation, Label, Tool};

/// Windows provides four resize axes; choose the nearest axis after rotation.
pub(crate) fn cursor_axis(handle: usize, rotation: i32) -> usize {
  let base = match handle {
    0 | 4 | 8 => 45.0,
    2 | 6 => 135.0,
    1 | 5 => 90.0,
    _ => 0.0,
  };
  ((base + rotation as f64 / 1000.0) / 45.0)
    .round()
    .rem_euclid(4.0) as usize
}

pub(crate) fn rotate(annotation: &Annotation, point: (f64, f64), inverse: bool) -> (f64, f64) {
  if annotation.tool.freehand() {
    return if inverse {
      match super::path::inverse_point(annotation.path_transform, point) {
        Some(point) => point,
        None => {
          tracing::error!("Invalid pen transform in shape geometry");
          (f64::NAN, f64::NAN)
        },
      }
    } else {
      super::path::world(annotation, point)
    };
  }
  let Some((l, t, r, b)) = bounds(annotation) else {
    return point;
  };
  let angle = (annotation.rotation_millidegrees as f64 / 1000.0).to_radians()
    * if inverse { -1.0 } else { 1.0 };
  let (sin, cos) = angle.sin_cos();
  let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
  let (x, y) = (point.0 - cx, point.1 - cy);
  (cx + x * cos - y * sin, cy + x * sin + y * cos)
}

pub(crate) fn rotation_handle(annotation: &Annotation) -> Option<(f64, f64)> {
  // Blur regions cover capture pixels on an upright grid.
  if annotation.tool == Tool::Blur {
    return None;
  }
  if annotation.tool.freehand() {
    if annotation.points.len() < 2 {
      return None;
    }
    let h = frame_handles(annotation)?;
    let (c, t) = ((h[0].0 + h[4].0) / 2.0, (h[0].1 + h[4].1) / 2.0);
    let (dx, dy) = (h[1].0 - c, h[1].1 - t);
    let length = dx.hypot(dy).max(0.001);
    return Some((h[1].0 + 28.0 * dx / length, h[1].1 + 28.0 * dy / length));
  }
  let (l, t, r, _) = bounds(annotation)?;
  Some(rotate(annotation, ((l + r) / 2.0, t - 28.0), false))
}

pub(crate) fn radius(annotation: &Annotation) -> f64 {
  bounds(annotation)
    .filter(|_| annotation.tool == Tool::Rectangle)
    .map_or(0.0, |(l, t, r, b)| {
      (annotation.corner_radius.max(0) as f64).min((r - l).min(b - t) / 2.0)
    })
}

/// Inset diagonal control remains separate from the top-left resize handle at zero radius.
pub(crate) fn radius_handle(annotation: &Annotation) -> Option<(f64, f64)> {
  let (l, t, r, b) = bounds(annotation)?;
  if annotation.tool != Tool::Rectangle || (r - l).min(b - t) < 48.0 {
    return None;
  }
  let inset = 18.0 + radius(annotation) * (1.0 - std::f64::consts::FRAC_1_SQRT_2);
  Some(rotate(annotation, (l + inset, t + inset), false))
}

pub(crate) fn rectangle_distance(annotation: &Annotation, position: (f64, f64)) -> Option<f64> {
  let position = rotate(annotation, position, true);
  let (l, t, r, b) = bounds(annotation)?;
  let radius = radius(annotation);
  let x = (position.0 - (l + r) / 2.0).abs() - (r - l) / 2.0 + radius;
  let y = (position.1 - (t + b) / 2.0).abs() - (b - t) / 2.0 + radius;
  Some(x.max(0.0).hypot(y.max(0.0)) + x.max(y).min(0.0) - radius)
}

pub(crate) fn bounds(annotation: &Annotation) -> Option<(f64, f64, f64, f64)> {
  if annotation.tool.freehand() {
    return super::path::raw_bounds(annotation);
  }
  if annotation.tool == Tool::Text {
    let &(x, y) = annotation.points.first()?;
    let Some(Label::Text(data)) = &annotation.label else {
      return None;
    };
    return Some((
      x as f64,
      y as f64,
      x as f64 + data.wrap_width as f64,
      y as f64 + data.extent.1.max(data.size) as f64,
    ));
  }
  if !matches!(
    annotation.tool,
    Tool::Rectangle | Tool::Ellipse | Tool::Blur
  ) || annotation.points.len() != 2
  {
    return None;
  }
  let (a, b) = (annotation.points[0], annotation.points[1]);
  let bounds = (
    a.0.min(b.0) as f64,
    a.1.min(b.1) as f64,
    a.0.max(b.0) as f64,
    a.1.max(b.1) as f64,
  );
  (bounds.0 < bounds.2 && bounds.1 < bounds.3).then_some(bounds)
}

/// Clockwise from top left, alternating corners and edge midpoints.
pub(crate) fn handles(annotation: &Annotation) -> Option<[(f64, f64); 8]> {
  if annotation.tool.freehand() && annotation.points.len() < 2 {
    return None;
  }
  if !matches!(
    annotation.tool,
    Tool::Rectangle | Tool::Ellipse | Tool::Pen | Tool::Highlighter | Tool::Blur
  ) {
    return None;
  }
  frame_handles(annotation)
}

pub(crate) fn frame_handles(annotation: &Annotation) -> Option<[(f64, f64); 8]> {
  let (mut l, mut t, mut r, mut b) = bounds(annotation)?;
  if annotation.tool == Tool::Text {
    l -= 7.0;
    t -= 7.0;
    r += 7.0;
    b += 7.0;
  }
  let (x, y) = ((l + r) / 2.0, (t + b) / 2.0);
  Some(
    [
      (l, t),
      (x, t),
      (r, t),
      (r, y),
      (r, b),
      (x, b),
      (l, b),
      (l, y),
    ]
    .map(|point| rotate(annotation, point, false)),
  )
}

pub(crate) fn contains(annotation: &Annotation, position: (f64, f64)) -> bool {
  let Some((l, t, r, b)) = bounds(annotation) else {
    return false;
  };
  if annotation.tool == Tool::Ellipse {
    let position = rotate(annotation, position, true);
    let x = (position.0 - (l + r) / 2.0) / ((r - l) / 2.0);
    let y = (position.1 - (t + b) / 2.0) / ((b - t) / 2.0);
    x * x + y * y <= 1.0
  } else {
    rectangle_distance(annotation, position).is_some_and(|distance| distance <= 0.0)
  }
}

pub(crate) fn edit_handle(
  annotation: &Annotation,
  handle: usize,
  position: (i32, i32),
  proportional: bool,
  viewport: (i32, i32),
) -> Option<Annotation> {
  let (l, t, r, b) = bounds(annotation)?;
  if handle == 9 {
    let (dx, dy) = (
      position.0 as f64 - (l + r) / 2.0,
      position.1 as f64 - (t + b) / 2.0,
    );
    if dx.hypot(dy) < 4.0 {
      return None;
    }
    let mut angle = dy.atan2(dx).to_degrees() + 90.0;
    if proportional {
      angle = (angle / 15.0).round() * 15.0;
    }
    let mut edited = annotation.clone();
    edited.rotation_millidegrees = (angle.rem_euclid(360.0) * 1000.0).round() as i32 % 360_000;
    return Some(edited);
  }
  if annotation.rotation_millidegrees != 0 {
    let local = rotate(annotation, (position.0 as f64, position.1 as f64), true);
    let mut straight = annotation.clone();
    straight.rotation_millidegrees = 0;
    let mut edited = edit_handle(
      &straight,
      handle,
      (local.0.round() as i32, local.1.round() as i32),
      proportional,
      viewport,
    )?;
    let (el, et, er, eb) = bounds(&edited)?;
    let center = ((el + er) / 2.0, (et + eb) / 2.0);
    let rotated_center = rotate(annotation, center, false);
    let (dx, dy) = (
      (rotated_center.0 - center.0).round() as i32,
      (rotated_center.1 - center.1).round() as i32,
    );
    for point in &mut edited.points {
      point.0 = point.0.saturating_add(dx);
      point.1 = point.1.saturating_add(dy);
    }
    edited.rotation_millidegrees = annotation.rotation_millidegrees;
    return Some(edited);
  }
  if handle == 8 && annotation.tool == Tool::Rectangle {
    let mut edited = annotation.clone();
    let diagonal = ((position.0 as f64 - l) + (position.1 as f64 - t)) / 2.0 - 18.0;
    edited.corner_radius = (diagonal / (1.0 - std::f64::consts::FRAC_1_SQRT_2))
      .clamp(0.0, (r - l).min(b - t) / 2.0)
      .floor() as i32;
    return Some(edited);
  }
  if handle >= 8 || viewport.0 < 2 || viewport.1 < 2 {
    return None;
  }
  let left = matches!(handle, 0 | 6 | 7);
  let right = matches!(handle, 2..=4);
  let top = matches!(handle, 0..=2);
  let bottom = matches!(handle, 4..=6);
  let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
  let (ax, ay) = (
    if left {
      r
    } else if right {
      l
    } else {
      cx
    },
    if top {
      b
    } else if bottom {
      t
    } else {
      cy
    },
  );
  let (max_x, max_y) = ((viewport.0 - 1) as f64, (viewport.1 - 1) as f64);
  let mut w = if left {
    (ax - position.0 as f64).clamp(1.0, ax.max(1.0))
  } else if right {
    (position.0 as f64 - ax).clamp(1.0, (max_x - ax).max(1.0))
  } else {
    r - l
  };
  let mut h = if top {
    (ay - position.1 as f64).clamp(1.0, ay.max(1.0))
  } else if bottom {
    (position.1 as f64 - ay).clamp(1.0, (max_y - ay).max(1.0))
  } else {
    b - t
  };
  if proportional {
    let sx = w / (r - l);
    let sy = h / (b - t);
    let requested = if !(left || right) {
      sy
    } else if !(top || bottom) {
      sx
    } else {
      sx.max(sy)
    };
    let available_w = if left {
      ax
    } else if right {
      max_x - ax
    } else {
      2.0 * ax.min(max_x - ax)
    };
    let available_h = if top {
      ay
    } else if bottom {
      max_y - ay
    } else {
      2.0 * ay.min(max_y - ay)
    };
    let limit = (available_w / (r - l)).min(available_h / (b - t));
    let minimum = (1.0 / (r - l)).max(1.0 / (b - t));
    if limit < minimum {
      return None;
    }
    let scale = requested.clamp(minimum, limit);
    w = (r - l) * scale;
    h = (b - t) * scale;
  }
  let x = if left {
    ax - w
  } else if right {
    ax
  } else {
    cx - w / 2.0
  };
  let y = if top {
    ay - h
  } else if bottom {
    ay
  } else {
    cy - h / 2.0
  };
  let mut edited = annotation.clone();
  edited.points = vec![
    (x.round() as i32, y.round() as i32),
    ((x + w).round() as i32, (y + h).round() as i32),
  ];
  edited.corner_radius = radius(&edited).floor() as i32;
  Some(edited)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resize_cursor_axes_follow_rotation() {
    assert_eq!(cursor_axis(3, 0), 0);
    assert_eq!(cursor_axis(3, 45_000), 1);
    assert_eq!(cursor_axis(3, 90_000), 2);
    assert_eq!(cursor_axis(3, 135_000), 3);
    assert_eq!(cursor_axis(3, 180_000), 0);
    assert_eq!(cursor_axis(0, 45_000), 2);
    assert_eq!(cursor_axis(1, 90_000), 0);
    assert_eq!(cursor_axis(8, 90_000), 3);
  }

  fn drawing(tool: Tool) -> Annotation {
    Annotation {
      id: 1,
      tool,
      points: vec![(300, 200), (100, 100)],
      color: [255, 0, 0],
      width: 3,
      label: None,
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    }
  }

  #[test]
  fn rotation_snaps_and_rotated_resize_keeps_opposite_corner_fixed() {
    for tool in [Tool::Rectangle, Tool::Ellipse] {
      let original = drawing(tool);
      let rotated = edit_handle(&original, 9, (300, 100), true, (800, 600)).expect("rotation");
      assert_eq!(rotated.rotation_millidegrees, 60_000);
      assert_eq!(rotated.points, original.points);
      let handles = handles(&rotated).expect("rotated handles");
      let target = rotate(&rotated, (350.0, 240.0), false);
      let resized = edit_handle(
        &rotated,
        4,
        (target.0.round() as i32, target.1.round() as i32),
        false,
        (800, 600),
      )
      .expect("resize");
      let fixed = super::handles(&resized).expect("handles")[0];
      assert!((fixed.0 - handles[0].0).hypot(fixed.1 - handles[0].1) < 2.0);
      assert_eq!(resized.rotation_millidegrees, 60_000);
      assert!(contains(&rotated, (200.0, 150.0)));
      let point = rotate(&rotated, (200.0, 95.0), false);
      assert!(!contains(&rotated, point));
    }
  }

  #[test]
  fn radius_drag_clamps_and_rounded_outline_matches_selection() {
    let original = drawing(Tool::Rectangle);
    let square = edit_handle(&original, 8, (0, 0), false, (800, 600)).expect("square");
    assert_eq!(square.corner_radius, 0);
    let rounded = edit_handle(&original, 8, (500, 500), false, (800, 600)).expect("rounded");
    assert_eq!(rounded.corner_radius, 50);
    assert_eq!(rounded.points, original.points);
    assert!(!contains(&rounded, (100.0, 100.0)));
    assert!(contains(&rounded, (200.0, 150.0)));
    assert!(
      rectangle_distance(&rounded, (100.0, 150.0))
        .expect("distance")
        .abs()
        < 0.01
    );
    let resized = edit_handle(&rounded, 4, (120, 120), false, (800, 600)).expect("resize");
    assert_eq!(resized.corner_radius, 10);
    assert!(radius_handle(&resized).is_none());
    assert!(radius_handle(&drawing(Tool::Ellipse)).is_none());
  }

  #[test]
  fn all_shape_handles_keep_the_opposite_edge_and_nonzero_dimensions() {
    for tool in [Tool::Rectangle, Tool::Ellipse] {
      let original = drawing(tool);
      let points = handles(&original).expect("handles");
      for (handle, point) in points.into_iter().enumerate() {
        let same = edit_handle(
          &original,
          handle,
          (point.0 as i32, point.1 as i32),
          false,
          (800, 600),
        )
        .expect("resize");
        assert_eq!(bounds(&same), bounds(&original));
        let edited = edit_handle(&original, handle, (400, 350), false, (800, 600)).expect("resize");
        let (l, t, r, b) = bounds(&edited).expect("nonzero");
        assert!(l >= 0.0 && t >= 0.0 && r < 800.0 && b < 600.0);
        match handle {
          0 => {
            assert_eq!(r, 300.0);
            assert_eq!(b, 200.0);
          },
          1 => {
            assert_eq!(l, 100.0);
            assert_eq!(r, 300.0);
            assert_eq!(b, 200.0);
          },
          2 => {
            assert_eq!(l, 100.0);
            assert_eq!(b, 200.0);
          },
          3 => {
            assert_eq!(l, 100.0);
            assert_eq!(t, 100.0);
            assert_eq!(b, 200.0);
          },
          4 => {
            assert_eq!(l, 100.0);
            assert_eq!(t, 100.0);
          },
          5 => {
            assert_eq!(l, 100.0);
            assert_eq!(r, 300.0);
            assert_eq!(t, 100.0);
          },
          6 => {
            assert_eq!(r, 300.0);
            assert_eq!(t, 100.0);
          },
          _ => {
            assert_eq!(r, 300.0);
            assert_eq!(t, 100.0);
            assert_eq!(b, 200.0);
          },
        }
      }
    }
  }

  #[test]
  fn proportional_corners_and_edges_preserve_ratio_at_viewport_limits() {
    let original = drawing(Tool::Ellipse);
    for handle in 0..8 {
      for position in [(-500, -500), (400, 350), (2000, 2000)] {
        let edited =
          edit_handle(&original, handle, position, true, (800, 600)).expect("proportional resize");
        let (l, t, r, b) = bounds(&edited).expect("bounds");
        assert!(((r - l) - 2.0 * (b - t)).abs() <= 2.0);
        assert!(l >= 0.0 && t >= 0.0 && r < 800.0 && b < 600.0);
      }
    }
    assert!(contains(&original, (200.0, 150.0)));
    assert!(!contains(&original, (100.0, 100.0)));
    assert!(edit_handle(&original, 8, (0, 0), false, (800, 600)).is_none());
  }
}
