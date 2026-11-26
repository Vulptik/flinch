//! Bounded sampling: exact collinear reduction, then at most two quarter-pixel compactions.
//! Endpoints survive; cumulative simplification error is <= half a capture pixel.

use crate::screenshot::annotations::MAX_POINTS;

pub(super) const MAX_COMPACTIONS: u8 = 2;

pub(super) fn append(
  points: &mut Vec<(i32, i32)>,
  point: (i32, i32),
  compactions: &mut u8,
) -> bool {
  if points.last() == Some(&point) {
    return true;
  }
  if points.len() >= 2 {
    let a = points[points.len() - 2];
    let b = points[points.len() - 1];
    // i128 keeps cross/dot products exact even for opposite i32 coordinate extremes.
    let ab = (b.0 as i128 - a.0 as i128, b.1 as i128 - a.1 as i128);
    let bc = (point.0 as i128 - b.0 as i128, point.1 as i128 - b.1 as i128);
    if ab.0 * bc.1 == ab.1 * bc.0 && ab.0 * bc.0 + ab.1 * bc.1 >= 0 {
      if let Some(last) = points.last_mut() {
        *last = point;
      }
      return true;
    }
  }
  if points.len() == MAX_POINTS && *compactions < MAX_COMPACTIONS {
    compact(points);
    *compactions += 1;
  }
  if points.len() >= MAX_POINTS {
    return false;
  }
  points.push(point);
  true
}

fn distance_squared(p: (i32, i32), a: (i32, i32), b: (i32, i32)) -> f64 {
  let dx = b.0 as f64 - a.0 as f64;
  let dy = b.1 as f64 - a.1 as f64;
  let px = p.0 as f64 - a.0 as f64;
  let py = p.1 as f64 - a.1 as f64;
  let length = dx * dx + dy * dy;
  let t = if length == 0.0 {
    0.0
  } else {
    ((px * dx + py * dy) / length).clamp(0.0, 1.0)
  };
  (px - t * dx).powi(2) + (py - t * dy).powi(2)
}

fn compact(points: &mut Vec<(i32, i32)>) {
  if points.len() < 3 {
    return;
  }
  let mut keep = vec![false; points.len()];
  keep[0] = true;
  keep[points.len() - 1] = true;
  let mut work = vec![(0, points.len() - 1)];
  // Every iteration splits a strictly smaller interval. Storage and work are bounded by MAX_POINTS.
  while let Some((start, end)) = work.pop() {
    let mut furthest = None;
    let mut distance = 0.25_f64.powi(2);
    for index in start + 1..end {
      let candidate = distance_squared(points[index], points[start], points[end]);
      if candidate > distance {
        distance = candidate;
        furthest = Some(index);
      }
    }
    if let Some(index) = furthest {
      keep[index] = true;
      work.push((start, index));
      work.push((index, end));
    }
  }
  let mut index = 0;
  points.retain(|_| {
    let retained = keep[index];
    index += 1;
    retained
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn long_straight_input_is_exact_and_reversals_survive() {
    let mut points = vec![(0, 0)];
    let mut passes = 0;
    for x in 1..100_000 {
      assert!(append(&mut points, (x, 0), &mut passes));
    }
    assert_eq!(points, vec![(0, 0), (99_999, 0)]);
    assert!(append(&mut points, (99_998, 0), &mut passes));
    assert_eq!(points, vec![(0, 0), (99_999, 0), (99_998, 0)]);
    assert_eq!(passes, 0);
  }

  #[test]
  fn compaction_preserves_endpoints_and_quarter_pixel_error() {
    let source: Vec<_> = (0..MAX_POINTS as i32).map(|x| (x, x / 10)).collect();
    let mut points = source.clone();
    compact(&mut points);
    assert!(points.len() < source.len());
    assert_eq!(points.first(), source.first());
    assert_eq!(points.last(), source.last());
    for point in source {
      assert!(
        points
          .windows(2)
          .any(|line| distance_squared(point, line[0], line[1]) <= 0.25_f64.powi(2) + 1e-9)
      );
    }
  }

  #[test]
  fn irreducible_input_stops_without_losing_existing_points() {
    let mut points: Vec<_> = (0..MAX_POINTS as i32).map(|x| (x, x % 2 * 10)).collect();
    let original = points.clone();
    let mut passes = 0;
    assert!(!append(&mut points, (MAX_POINTS as i32, 0), &mut passes));
    assert_eq!(points, original);
    assert!(passes <= MAX_COMPACTIONS);
  }
}
