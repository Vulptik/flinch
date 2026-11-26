//! Shared arrow geometry. The optional middle point lies on the curve at t=0.5.
use super::annotations::{Annotation, Tool};

pub(crate) type Point = (f64, f64);

pub(crate) struct Arrow {
  curves: Vec<[Point; 4]>,
  pub start: Point,
  pub middle: Point,
  pub end: Point,
  pub control: Point,
}

impl Arrow {
  pub fn from_annotation(annotation: &Annotation) -> Option<Self> {
    if annotation.tool != Tool::Arrow
      || annotation.points.len() < 2
      || annotation.points.len() > super::path::MAX_NODES
      || (annotation.curve_controls.is_empty() && annotation.points.len() > 3)
      || (!annotation.curve_controls.is_empty()
        && annotation.curve_controls.len() != annotation.points.len())
    {
      return None;
    }
    let point = |index: usize| {
      let (x, y) = annotation.points[index];
      (x as f64, y as f64)
    };
    let start = point(0);
    let end = point(annotation.points.len() - 1);
    if start == end {
      return None;
    }
    let midpoint = ((start.0 + end.0) / 2.0, (start.1 + end.1) / 2.0);
    let curved = annotation.points.len() == 3;
    let middle = if curved { point(1) } else { midpoint };
    Some(Self {
      curves: if annotation.curve_controls.is_empty() {
        Vec::new()
      } else {
        (0..annotation.points.len() - 1)
          .map(|i| {
            [
              point(i),
              (
                annotation.curve_controls[i][1].0 as f64,
                annotation.curve_controls[i][1].1 as f64,
              ),
              (
                annotation.curve_controls[i + 1][0].0 as f64,
                annotation.curve_controls[i + 1][0].1 as f64,
              ),
              point(i + 1),
            ]
          })
          .collect()
      },
      start,
      middle,
      end,
      control: (2.0 * middle.0 - midpoint.0, 2.0 * middle.1 - midpoint.1),
    })
  }

  pub fn at(&self, t: f64) -> Point {
    if !self.curves.is_empty() {
      let scaled = t.clamp(0.0, 1.0) * self.curves.len() as f64;
      let i = (scaled as usize).min(self.curves.len() - 1);
      return cubic(self.curves[i], scaled - i as f64);
    }
    let u = 1.0 - t;
    (
      u * u * self.start.0 + 2.0 * u * t * self.control.0 + t * t * self.end.0,
      u * u * self.start.1 + 2.0 * u * t * self.control.1 + t * t * self.end.1,
    )
  }

  pub fn head(&self, width: i32) -> [Point; 3] {
    let control = self.curves.last().map_or(self.control, |c| c[2]);
    let mut tangent = (self.end.0 - control.0, self.end.1 - control.1);
    if tangent.0.hypot(tangent.1) < 0.001 {
      tangent = (self.end.0 - self.start.0, self.end.1 - self.start.1);
    }
    let angle = tangent.1.atan2(tangent.0);
    let length = ((self.end.0 - self.start.0).hypot(self.end.1 - self.start.1) * 0.35)
      .min(width as f64 * 4.0 + 8.0);
    let tip = |offset: f64| {
      (
        self.end.0 - length * (angle + offset).cos(),
        self.end.1 - length * (angle + offset).sin(),
      )
    };
    [tip(0.5), self.end, tip(-0.5)]
  }

  pub fn handles(&self) -> [Point; 3] {
    [self.start, self.middle, self.end]
  }

  pub fn hit_test(&self, position: Point, tolerance: f64, width: i32) -> bool {
    if !self.curves.is_empty() {
      for curve in &self.curves {
        let mut previous = curve[0];
        for i in 1..=128 {
          let next = cubic(*curve, i as f64 / 128.0);
          if segment_distance(position, previous, next) <= tolerance {
            return true;
          }
          previous = next;
        }
      }
      return self
        .head(width)
        .windows(2)
        .any(|p| segment_distance(position, p[0], p[1]) <= tolerance);
    }
    // A bounded subdivision count keeps long or strongly bent arrows selectable.
    let curvature = (self.start.0 - 2.0 * self.control.0 + self.end.0)
      .hypot(self.start.1 - 2.0 * self.control.1 + self.end.1);
    let steps = (curvature.sqrt().ceil() as usize).clamp(1, 512);
    let mut previous = self.start;
    for index in 1..=steps {
      let next = self.at(index as f64 / steps as f64);
      if segment_distance(position, previous, next) <= tolerance {
        return true;
      }
      previous = next;
    }
    let head = self.head(width);
    head
      .windows(2)
      .any(|points| segment_distance(position, points[0], points[1]) <= tolerance)
  }
}

fn segment_distance(p: Point, a: Point, b: Point) -> f64 {
  let delta = (b.0 - a.0, b.1 - a.1);
  let length = delta.0 * delta.0 + delta.1 * delta.1;
  let t = if length == 0.0 {
    0.0
  } else {
    (((p.0 - a.0) * delta.0 + (p.1 - a.1) * delta.1) / length).clamp(0.0, 1.0)
  };
  (p.0 - a.0 - t * delta.0).hypot(p.1 - a.1 - t * delta.1)
}

pub(crate) fn edit_handle(
  annotation: &Annotation,
  handle: usize,
  position: (i32, i32),
) -> Option<Annotation> {
  if !annotation.curve_controls.is_empty() {
    let old = *annotation.points.get(handle)?;
    let (dx, dy) = (
      position.0.checked_sub(old.0)?,
      position.1.checked_sub(old.1)?,
    );
    let mut edited = annotation.clone();
    edited.points[handle] = position;
    for point in &mut edited.curve_controls[handle] {
      point.0 = point.0.checked_add(dx)?;
      point.1 = point.1.checked_add(dy)?;
    }
    return Arrow::from_annotation(&edited).map(|_| edited);
  }
  let arrow = Arrow::from_annotation(annotation)?;
  let mut handles = arrow.handles();
  *handles.get_mut(handle)? = (position.0 as f64, position.1 as f64);
  if handles[0] == handles[2] {
    return None;
  }
  let midpoint = (
    (handles[0].0 + handles[2].0) / 2.0,
    (handles[0].1 + handles[2].1) / 2.0,
  );
  // Moving an endpoint keeps the existing bend relative to the chord midpoint.
  if handle != 1 {
    handles[1] = (
      midpoint.0 + arrow.middle.0 - (arrow.start.0 + arrow.end.0) / 2.0,
      midpoint.1 + arrow.middle.1 - (arrow.start.1 + arrow.end.1) / 2.0,
    );
  }
  let mut edited = annotation.clone();
  let integer = |p: Point| (p.0.round() as i32, p.1.round() as i32);
  edited.points = if (handles[1].0 - midpoint.0).hypot(handles[1].1 - midpoint.1) <= 4.0 {
    vec![integer(handles[0]), integer(handles[2])]
  } else {
    vec![
      integer(handles[0]),
      integer(handles[1]),
      integer(handles[2]),
    ]
  };
  Some(edited)
}

fn mix(a: Point, b: Point, t: f64) -> Point {
  (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}
fn split(c: [Point; 4], t: f64) -> ([Point; 4], [Point; 4]) {
  let a = mix(c[0], c[1], t);
  let b = mix(c[1], c[2], t);
  let d = mix(c[2], c[3], t);
  let e = mix(a, b, t);
  let f = mix(b, d, t);
  let p = mix(e, f, t);
  ([c[0], a, e, p], [p, f, d, c[3]])
}
fn cubic(c: [Point; 4], t: f64) -> Point {
  split(c, t).0[3]
}
fn integer(p: Point) -> (i32, i32) {
  (p.0.round() as i32, p.1.round() as i32)
}
pub(crate) fn insert_node(
  annotation: &Annotation,
  position: Point,
) -> anyhow::Result<(Annotation, usize)> {
  use anyhow::{Context, ensure};
  let arrow = Arrow::from_annotation(annotation).context("Invalid arrow path")?;
  ensure!(
    annotation.points.len() < super::path::MAX_NODES,
    "Arrow node limit reached"
  );
  let mut curves = arrow.curves.clone();
  if curves.is_empty() {
    let c = [
      arrow.start,
      mix(arrow.start, arrow.control, 2.0 / 3.0),
      mix(arrow.end, arrow.control, 2.0 / 3.0),
      arrow.end,
    ];
    if annotation.points.len() == 3 {
      let (a, b) = split(c, 0.5);
      curves = vec![a, b];
    } else {
      curves = vec![c];
    }
  }
  let mut best = (0, 0.5, f64::INFINITY);
  for (index, c) in curves.iter().enumerate() {
    for i in 1..128 {
      let t = i as f64 / 128.0;
      let p = cubic(*c, t);
      let d = (p.0 - position.0).hypot(p.1 - position.1);
      if d < best.2 {
        best = (index, t, d);
      }
    }
  }
  ensure!(best.2 <= 9.0, "No arrow segment near this point");
  let (index, t, _) = best;
  ensure!(
    (0.02..0.98).contains(&t),
    "Arrow node too close to an endpoint"
  );
  let (a, b) = split(curves[index], t);
  curves.splice(index..=index, [a, b]);
  let mut edited = annotation.clone();
  edited.points = vec![integer(curves[0][0])];
  edited.curve_controls = vec![[integer(curves[0][0]), integer(curves[0][1])]];
  for c in curves {
    let end = integer(c[3]);
    if let Some(last) = edited.curve_controls.last_mut() {
      last[1] = integer(c[1]);
    }
    edited.points.push(end);
    edited.curve_controls.push([integer(c[2]), end]);
  }
  ensure!(
    Arrow::from_annotation(&edited).is_some(),
    "Inserted arrow node is invalid"
  );
  Ok((edited, index + 1))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn inserted_nodes_preserve_curves_and_follow_final_tangent() {
    let original = drawing(vec![(100, 100), (200, 160), (300, 100)]);
    let source = Arrow::from_annotation(&original).expect("arrow");
    let p = source.at(0.25);
    let (edited, node) = insert_node(&original, p).expect("insert");
    assert_eq!(edited.points.len(), 4);
    assert_eq!(node, 1);
    let after = Arrow::from_annotation(&edited).expect("multi-node arrow");
    for i in 0..101 {
      let point = source.at(i as f64 / 100.0);
      assert!(
        after.hit_test(point, 2.0, original.width),
        "original curve retained at {i}"
      );
    }
    let moved = edit_handle(&edited, node, (140, 220)).expect("move node");
    let curve = Arrow::from_annotation(&moved).expect("moved curve");
    assert!(curve.hit_test((140.0, 220.0), 1.0, moved.width));
    assert_eq!(moved.points.last(), original.points.last());
    assert_eq!(curve.head(3)[1], (300.0, 100.0));
  }

  fn drawing(points: Vec<(i32, i32)>) -> Annotation {
    Annotation {
      id: 1,
      tool: Tool::Arrow,
      points,
      color: [0, 140, 255],
      width: 6,
      label: None,
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
    }
  }

  #[test]
  fn middle_is_on_curve_and_head_follows_end_tangent() {
    let annotation = drawing(vec![(100, 300), (230, 200), (100, 100)]);
    let arrow = Arrow::from_annotation(&annotation).expect("arrow");
    assert_eq!(arrow.at(0.5), (230.0, 200.0));
    let head = arrow.head(6);
    let center = ((head[0].0 + head[2].0) / 2.0, (head[0].1 + head[2].1) / 2.0);
    let direction = (arrow.end.0 - center.0, arrow.end.1 - center.1);
    let tangent = (arrow.end.0 - arrow.control.0, arrow.end.1 - arrow.control.1);
    assert!((direction.0 * tangent.1 - direction.1 * tangent.0).abs() < 0.0001);
    assert!(annotation.hit((230.0, 200.0)));
    assert!(
      !annotation.hit((100.0, 200.0)),
      "original chord is not the curved shaft"
    );
    for index in 0..=100 {
      let point = arrow.at(index as f64 / 100.0);
      assert!(annotation.hit(point));
      let bounds = annotation.paint_bounds().expect("bounds");
      assert!(point.0 >= bounds.left as f64 && point.0 < bounds.right as f64);
      assert!(point.1 >= bounds.top as f64 && point.1 < bounds.bottom as f64);
    }
  }

  #[test]
  fn handle_edits_bend_straighten_and_preserve_bend_when_moving_endpoints() {
    let original = drawing(vec![(20, 20), (220, 20)]);
    let bent = edit_handle(&original, 1, (120, 100)).expect("bend");
    assert_eq!(bent.points, vec![(20, 20), (120, 100), (220, 20)]);
    let moved = edit_handle(&bent, 2, (320, 60)).expect("endpoint");
    assert_eq!(moved.points, vec![(20, 20), (170, 120), (320, 60)]);
    assert_eq!(
      edit_handle(&bent, 1, (120, 23)).expect("straighten"),
      original
    );
    assert!(edit_handle(&original, 0, (220, 20)).is_none());
    assert!(edit_handle(&original, 3, (20, 20)).is_none());
    let extreme = drawing(vec![(i32::MIN, i32::MIN), (i32::MAX, i32::MAX)]);
    assert_eq!(
      Arrow::from_annotation(&extreme).expect("extreme").middle,
      (-0.5, -0.5)
    );
  }
}
