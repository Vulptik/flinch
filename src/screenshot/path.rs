//! Bounded path transforms and sparse pen controls; capture samples remain intact on entry.
use super::annotations::{Annotation, MAX_POINTS};
use anyhow::{Result, ensure};
pub(crate) const IDENTITY: [i64; 6] = [1_000_000, 0, 0, 1_000_000, 0, 0];
pub(crate) const MAX_NODES: usize = 64;
const AUTO_NODES: usize = 8;
pub(crate) fn matrix(fixed: [i64; 6]) -> [f64; 6] {
  fixed.map(|v| v as f64 / 1_000_000.0)
}
pub(crate) fn apply(m: [f64; 6], p: (f64, f64)) -> (f64, f64) {
  (
    m[0] * p.0 + m[2] * p.1 + m[4],
    m[1] * p.0 + m[3] * p.1 + m[5],
  )
}
pub(crate) fn world(a: &Annotation, p: (f64, f64)) -> (f64, f64) {
  if a.tool.freehand() {
    apply(matrix(a.path_transform), p)
  } else {
    p
  }
}
pub(crate) fn inverse(m: [i64; 6]) -> Option<[f64; 6]> {
  let m = matrix(m);
  let d = m[0] * m[3] - m[1] * m[2];
  if !d.is_finite() || d.abs() < 0.000_001 {
    return None;
  }
  let (a, b, c, e) = (m[3] / d, -m[1] / d, -m[2] / d, m[0] / d);
  Some([a, b, c, e, -a * m[4] - c * m[5], -b * m[4] - e * m[5]])
}
pub(crate) fn inverse_point(m: [i64; 6], p: (f64, f64)) -> Option<(f64, f64)> {
  Some(apply(inverse(m)?, p))
}
pub(crate) fn product(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
  [
    a[0] * b[0] + a[2] * b[1],
    a[1] * b[0] + a[3] * b[1],
    a[0] * b[2] + a[2] * b[3],
    a[1] * b[2] + a[3] * b[3],
    a[0] * b[4] + a[2] * b[5] + a[4],
    a[1] * b[4] + a[3] * b[5] + a[5],
  ]
}
pub(crate) fn fixed(m: [f64; 6]) -> Result<[i64; 6]> {
  ensure!(
    m.iter().all(|v| v.is_finite() && v.abs() < 1e12),
    "Path transform exceeds coordinate budget"
  );
  let result = m.map(|v| (v * 1_000_000.0).round() as i64);
  ensure!(inverse(result).is_some(), "Path transform is singular");
  Ok(result)
}
pub(crate) fn compose(a: [f64; 6], b: [i64; 6]) -> Result<[i64; 6]> {
  fixed(product(a, matrix(b)))
}
pub(crate) fn raw_bounds(a: &Annotation) -> Option<(f64, f64, f64, f64)> {
  let &(x, y) = a.points.first()?;
  let (mut l, mut t, mut r, mut b) = (x as f64, y as f64, x as f64, y as f64);
  for &(x, y) in &a.points {
    l = l.min(x as f64);
    t = t.min(y as f64);
    r = r.max(x as f64);
    b = b.max(y as f64);
  }
  if l == r {
    l -= 0.5;
    r += 0.5;
  }
  if t == b {
    t -= 0.5;
    b += 0.5;
  }
  Some((l, t, r, b))
}
pub(crate) fn edit_transform(
  a: &Annotation,
  handle: usize,
  p: (i32, i32),
  shift: bool,
) -> Result<Annotation> {
  let (l, t, r, b) = raw_bounds(a).ok_or_else(|| anyhow::anyhow!("Empty pen path"))?;
  let mut result = a.clone();
  if handle == 9 {
    let center = world(a, ((l + r) / 2.0, (t + b) / 2.0));
    let dx = p.0 as f64 - center.0;
    let dy = p.1 as f64 - center.1;
    if dx.hypot(dy) < 4.0 {
      return Ok(result);
    }
    let mut angle = dy.atan2(dx).to_degrees() + 90.0;
    if shift {
      angle = (angle / 15.0).round() * 15.0;
    }
    let delta = (angle - a.rotation_millidegrees as f64 / 1000.0).to_radians();
    let (s, c) = delta.sin_cos();
    result.path_transform = compose(
      [
        c,
        s,
        -s,
        c,
        center.0 - c * center.0 + s * center.1,
        center.1 - s * center.0 - c * center.1,
      ],
      a.path_transform,
    )?;
    result.rotation_millidegrees = (angle.rem_euclid(360.0) * 1000.0).round() as i32 % 360_000;
  } else {
    ensure!(handle < 8, "Invalid pen resize handle");
    let local = inverse_point(a.path_transform, (p.0 as f64, p.1 as f64))
      .ok_or_else(|| anyhow::anyhow!("Invalid pen transform"))?;
    let left = matches!(handle, 0 | 6 | 7);
    let right = matches!(handle, 2..=4);
    let top = matches!(handle, 0..=2);
    let bottom = matches!(handle, 4..=6);
    let ax = if left {
      r
    } else if right {
      l
    } else {
      (l + r) / 2.0
    };
    let ay = if top {
      b
    } else if bottom {
      t
    } else {
      (t + b) / 2.0
    };
    let mut sx = if left {
      (ax - local.0) / (r - l)
    } else if right {
      (local.0 - ax) / (r - l)
    } else {
      1.0
    };
    let mut sy = if top {
      (ay - local.1) / (b - t)
    } else if bottom {
      (local.1 - ay) / (b - t)
    } else {
      1.0
    };
    if shift {
      let scale = if !(left || right) {
        sy
      } else if !(top || bottom) {
        sx
      } else {
        sx.max(sy)
      };
      sx = scale;
      sy = scale;
    }
    sx = sx.clamp(1.0 / 256.0, 256.0);
    sy = sy.clamp(1.0 / 256.0, 256.0);
    result.path_transform = fixed(product(
      matrix(a.path_transform),
      [sx, 0.0, 0.0, sy, ax * (1.0 - sx), ay * (1.0 - sy)],
    ))?;
  }
  ensure!(
    result.points.iter().all(|&(x, y)| {
      let p = world(&result, (x as f64, y as f64));
      p.0.abs() < i32::MAX as f64 && p.1.abs() < i32::MAX as f64
    }),
    "Transformed pen exceeds coordinate range"
  );
  Ok(result)
}
pub(crate) fn nearest(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> (f64, (f64, f64), f64) {
  let (dx, dy) = (b.0 - a.0, b.1 - a.1);
  let len = dx * dx + dy * dy;
  let t = if len == 0.0 {
    0.0
  } else {
    ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len
  }
  .clamp(0.0, 1.0);
  let q = (a.0 + t * dx, a.1 + t * dy);
  (t, q, (p.0 - q.0).hypot(p.1 - q.1))
}
pub(crate) fn nodes(a: &Annotation) -> Vec<usize> {
  if !a.pen_nodes.is_empty() {
    return a.pen_nodes.clone();
  }
  if a.points.len() < 2 {
    return (0..a.points.len()).collect();
  }
  let mut nodes = vec![0, a.points.len() - 1];
  let points: Vec<_> = a
    .points
    .iter()
    .map(|&(x, y)| world(a, (x as f64, y as f64)))
    .collect();
  let mut length = 0.0;
  let (mut l, mut t, mut r, mut b) = (points[0].0, points[0].1, points[0].0, points[0].1);
  for p in &points {
    l = l.min(p.0);
    t = t.min(p.1);
    r = r.max(p.0);
    b = b.max(p.1);
  }
  for pair in points.windows(2) {
    length += (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1);
  }
  let tolerance = 6.0_f64.max((r - l).hypot(b - t) * 0.025);
  let spacing = (length / 4.0).clamp(12.0, 40.0);
  // Endpoints plus at most six major bends; no captured samples are removed.
  while nodes.len() < AUTO_NODES {
    let mut candidate = None;
    let mut distance = tolerance;
    for pair in nodes.windows(2) {
      let pa = points[pair[0]];
      let pb = points[pair[1]];
      for index in pair[0] + 1..pair[1] {
        let p = points[index];
        if nodes
          .iter()
          .any(|&i| (p.0 - points[i].0).hypot(p.1 - points[i].1) < spacing)
        {
          continue;
        }
        let d = nearest(p, pa, pb).2;
        if d > distance {
          distance = d;
          candidate = Some(index);
        }
      }
    }
    let Some(index) = candidate else {
      break;
    };
    nodes.push(index);
    nodes.sort_unstable();
  }
  nodes
}
pub(crate) fn edit_node(
  a: &Annotation,
  controls: &[usize],
  index: usize,
  p: (i32, i32),
) -> Result<Annotation> {
  let point = *a
    .points
    .get(index)
    .ok_or_else(|| anyhow::anyhow!("Invalid pen node"))?;
  let local = inverse_point(a.path_transform, (p.0 as f64, p.1 as f64))
    .ok_or_else(|| anyhow::anyhow!("Invalid pen transform"))?;
  let (dx, dy) = (local.0 - point.0 as f64, local.1 - point.1 as f64);
  let prev = controls
    .iter()
    .copied()
    .rfind(|i| *i < index)
    .unwrap_or(index);
  let next = controls
    .iter()
    .copied()
    .find(|i| *i > index)
    .unwrap_or(index);
  let mut result = a.clone();
  for i in prev..=next {
    let weight = if i == index {
      1.0
    } else if i < index {
      (i - prev) as f64 / (index - prev) as f64
    } else {
      (next - i) as f64 / (next - index) as f64
    };
    let point = a.points[i];
    let (x, y) = (point.0 as f64 + dx * weight, point.1 as f64 + dy * weight);
    ensure!(
      x.abs() < i32::MAX as f64 && y.abs() < i32::MAX as f64,
      "Pen node exceeds coordinate range"
    );
    result.points[i] = (x.round() as i32, y.round() as i32);
  }
  result.pen_nodes = controls.to_vec();
  Ok(result)
}
pub(crate) fn insert_node(a: &Annotation, p: (f64, f64)) -> Result<(Annotation, usize)> {
  ensure!(a.points.len() < MAX_POINTS, "Pen sample limit reached");
  let mut best = None;
  for (i, pair) in a.points.windows(2).enumerate() {
    let x = world(a, (pair[0].0 as f64, pair[0].1 as f64));
    let y = world(a, (pair[1].0 as f64, pair[1].1 as f64));
    let (_, q, d) = nearest(p, x, y);
    if best.as_ref().is_none_or(|(_, _, old)| d < *old) {
      best = Some((i, q, d));
    }
  }
  let (i, q, d) = best.ok_or_else(|| anyhow::anyhow!("A single-point pen has no segment"))?;
  ensure!(d <= 9.0, "No path segment near this point");
  let local =
    inverse_point(a.path_transform, q).ok_or_else(|| anyhow::anyhow!("Invalid pen transform"))?;
  let mut result = a.clone();
  result
    .points
    .insert(i + 1, (local.0.round() as i32, local.1.round() as i32));
  Ok((result, i + 1))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::screenshot::annotations::Tool;

  /// Corner and edge handles scale the pen path about the opposite side; Shift keeps its
  /// proportions; the rotation handle snaps to 15 degrees with Shift.
  #[test]
  fn pen_resize_handles_scale_about_the_opposite_side_and_shift_keeps_proportions() {
    let mut a = drawing();
    a.points = vec![(100, 100), (200, 150), (300, 200)];
    let bounds = |a: &Annotation| {
      let corners: Vec<_> = [(100., 100.), (300., 200.)]
        .into_iter()
        .map(|p| world(a, p))
        .collect();
      (corners[0], corners[1])
    };
    // Dragging the bottom-right corner (4) doubles both sides; the top-left stays put.
    let grown = edit_transform(&a, 4, (500, 300), false).unwrap();
    let (top_left, bottom_right) = bounds(&grown);
    assert!((top_left.0 - 100.).abs() < 1. && (top_left.1 - 100.).abs() < 1.);
    assert!((bottom_right.0 - 500.).abs() < 1. && (bottom_right.1 - 300.).abs() < 1.);
    // The top-left corner (0) scales about the bottom-right one.
    let moved = edit_transform(&a, 0, (0, 0), false).unwrap();
    let (top_left, bottom_right) = bounds(&moved);
    assert!((bottom_right.0 - 300.).abs() < 1. && (bottom_right.1 - 200.).abs() < 1.);
    assert!(top_left.0 < 100. && top_left.1 < 100.);
    // Edge handles change one axis only.
    let wider = edit_transform(&a, 3, (400, 999), false).unwrap();
    let (top_left, bottom_right) = bounds(&wider);
    assert!((bottom_right.0 - 400.).abs() < 1.);
    assert!((top_left.1 - 100.).abs() < 1. && (bottom_right.1 - 200.).abs() < 1.);
    let taller = edit_transform(&a, 1, (999, 0), false).unwrap();
    let (top_left, bottom_right) = bounds(&taller);
    assert!((top_left.0 - 100.).abs() < 1. && (bottom_right.0 - 300.).abs() < 1.);
    assert!(top_left.1 < 100.);
    // Shift scales both axes by the same factor, from a corner or an edge.
    for handle in [4, 3, 5] {
      let even = edit_transform(&a, handle, (500, 260), true).unwrap();
      let m = matrix(even.path_transform);
      assert!((m[0] - m[3]).abs() < 1e-3, "handle {handle}: {m:?}");
    }
    // A collapsed drag clamps instead of mirroring or vanishing.
    let tiny = edit_transform(&a, 4, (100, 100), false).unwrap();
    assert!(matrix(tiny.path_transform)[0] > 0.);
    assert!(edit_transform(&a, 8, (0, 0), false).is_err());
    // The rotation handle: a press near the center changes nothing, Shift snaps to 15 degrees.
    assert_eq!(edit_transform(&a, 9, (200, 150), false).unwrap(), a);
    let rotated = edit_transform(&a, 9, (300, 160), true).unwrap();
    assert_eq!(rotated.rotation_millidegrees % 15_000, 0);
    assert!(
      edit_transform(
        &Annotation {
          points: Vec::new(),
          ..a
        },
        4,
        (0, 0),
        false
      )
      .is_err()
    );
  }

  #[test]
  fn automatic_nodes_ignore_wobbles_and_preserve_manual_controls() {
    let mut a = drawing();
    a.points = (0..1000)
      .map(|x| (x, 100 + ((x as f64 / 5.0).sin() * 3.0).round() as i32))
      .collect();
    let original = a.points.clone();
    assert_eq!(nodes(&a), vec![0, 999]);
    assert_eq!(a.points, original);
    a.points = (0..1000)
      .map(|x| (x, 100 + ((x as f64 / 25.0).sin() * 80.0).round() as i32))
      .collect();
    let controls = nodes(&a);
    assert!(controls.len() <= 8);
    assert!(controls.len() > 2);
    assert_eq!(controls[0], 0);
    assert_eq!(controls.last(), Some(&999));
    for pair in controls.windows(2) {
      let p = a.points[pair[0]];
      let q = a.points[pair[1]];
      assert!((p.0 as f64 - q.0 as f64).hypot(p.1 as f64 - q.1 as f64) >= 40.0);
    }
    a.pen_nodes = vec![0, 1, 2, 999];
    assert_eq!(
      nodes(&a),
      a.pen_nodes,
      "manually inserted nearby nodes remain editable"
    );
  }
  fn drawing() -> Annotation {
    Annotation {
      id: 1,
      tool: Tool::Pen,
      points: vec![(100, 100), (150, 130), (200, 100)],
      color: [255, 0, 0],
      width: 3,
      label: None,
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    }
  }
  #[test]
  fn pen_transforms_keep_width_and_hit_testing_attached() {
    let original = drawing();
    let resized = edit_transform(&original, 3, (300, 115), false).expect("resize");
    assert_eq!(resized.width, 3);
    assert_eq!(resized.points, original.points);
    assert_eq!(world(&resized, (100.0, 100.0)), (100.0, 100.0));
    assert_eq!(world(&resized, (200.0, 100.0)), (300.0, 100.0));
    assert!(resized.hit(world(&resized, (150.0, 130.0))));
    let rotated = edit_transform(&resized, 9, (300, 115), true).expect("rotation");
    assert_eq!(rotated.rotation_millidegrees, 90_000);
    assert!(rotated.hit(world(&rotated, (150.0, 130.0))));
    let moved = rotated.translated(20, 30).expect("move");
    let a = world(&rotated, (100.0, 100.0));
    let b = world(&moved, (100.0, 100.0));
    assert!((b.0 - a.0 - 20.0).abs() < 0.001 && (b.1 - a.1 - 30.0).abs() < 0.001);
  }
  #[test]
  fn sparse_nodes_preserve_samples_and_drag_only_neighbor_intervals() {
    let mut original = drawing();
    original.points = (0..1000)
      .map(|x| (x, if x < 500 { x / 10 } else { 100 - x / 10 }))
      .collect();
    let controls = nodes(&original);
    assert!(controls.len() <= MAX_NODES);
    assert_eq!(original.points.len(), 1000);
    assert_eq!(controls.first(), Some(&0));
    assert_eq!(controls.last(), Some(&999));
    let index = controls[1];
    let point = original.points[index];
    let edited =
      edit_node(&original, &controls, index, (point.0, point.1 + 30)).expect("node drag");
    assert_eq!(edited.points[0], original.points[0]);
    assert_eq!(edited.points[999], original.points[999]);
    assert_eq!(edited.points[index].1, point.1 + 30);
    let (inserted, index) = insert_node(&drawing(), (125.0, 115.0)).expect("insert");
    assert_eq!(inserted.points[index], (125, 115));
    let mut enlarged = drawing();
    enlarged.points = vec![(0, 0), (100, 1), (200, 0)];
    assert_eq!(nodes(&enlarged), vec![0, 2]);
    enlarged.path_transform = fixed([1.0, 0.0, 0.0, 10.0, 0.0, 0.0]).expect("enlarge");
    assert_eq!(
      nodes(&enlarged),
      vec![0, 1, 2],
      "node density follows the visible transformed path"
    );
  }
}
