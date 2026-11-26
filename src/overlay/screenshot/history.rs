//! Bounded reversible changes, with multi-object edits grouped into one gesture.

use std::collections::VecDeque;

use anyhow::{Context, Result, ensure};

use crate::screenshot::annotations::Annotation;

const MAX_COMMANDS: usize = 512;
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct Change {
  pub counter: Option<(u16, u16)>,
  pub index: usize,
  pub before: Option<Annotation>,
  pub after: Option<Annotation>,
}

impl Change {
  fn bytes(&self) -> usize {
    std::mem::size_of::<Self>()
      + self
        .before
        .iter()
        .chain(self.after.iter())
        .map(|a| {
          a.points.capacity() * std::mem::size_of::<(i32, i32)>()
            + a.curve_controls.capacity() * std::mem::size_of::<[(i32, i32); 2]>()
            + a.pen_nodes.capacity() * std::mem::size_of::<usize>()
            + match &a.label {
              Some(crate::screenshot::annotations::Label::Text(t)) => t.content.capacity(),
              _ => 0,
            }
        })
        .sum::<usize>()
  }

  fn apply(&self, drawings: &mut Vec<Annotation>, forward: bool) -> Result<()> {
    let (expected, replacement) = if forward {
      (&self.before, &self.after)
    } else {
      (&self.after, &self.before)
    };
    ensure!(
      self.index <= drawings.len(),
      "Drawing history index is invalid"
    );
    if let Some(expected) = expected {
      ensure!(
        drawings.get(self.index) == Some(expected),
        "Drawing history does not match document"
      );
    }
    // Validate before mutating so a failed operation leaves the visible document intact.
    if expected.is_some() {
      drawings.remove(self.index);
    }
    if let Some(replacement) = replacement {
      drawings.insert(self.index, replacement.clone());
    }
    Ok(())
  }
}

struct Batch(
  Vec<Change>,
  Option<(
    crate::overlay::selection::Rect,
    crate::overlay::selection::Rect,
  )>,
);
impl Batch {
  fn bytes(&self) -> usize {
    self.0.iter().map(Change::bytes).sum::<usize>()
      + self.1.map_or(0, |_| {
        std::mem::size_of::<(
          crate::overlay::selection::Rect,
          crate::overlay::selection::Rect,
        )>()
      })
  }
  fn apply(&self, drawings: &mut Vec<Annotation>, forward: bool) -> Result<()> {
    if self.0.is_empty() {
      return Ok(());
    }
    if self.0.len() == 1 {
      return self.0[0].apply(drawings, forward);
    }
    // Validate the complete batch before exposing any mutations.
    let mut staged = drawings.clone();
    if forward {
      for change in &self.0 {
        change.apply(&mut staged, true)?;
      }
    } else {
      for change in self.0.iter().rev() {
        change.apply(&mut staged, false)?;
      }
    }
    *drawings = staged;
    Ok(())
  }
  fn counter(&self) -> Option<(u16, u16)> {
    self.0.iter().find_map(|change| change.counter)
  }
}

#[derive(Default)]
pub(super) struct History {
  pub pending_crop: Option<crate::overlay::selection::Rect>,
  marker_counter: u16,
  undo: VecDeque<Batch>,
  redo: Vec<Batch>,
  bytes: usize,
  pub limited: bool,
}

impl History {
  pub fn next_marker(&self) -> Option<u16> {
    (self.marker_counter < 999).then(|| self.marker_counter + 1)
  }
  pub fn can_undo(&self) -> bool {
    !self.undo.is_empty()
  }
  pub fn can_redo(&self) -> bool {
    !self.redo.is_empty()
  }

  pub fn commit(&mut self, drawings: &mut Vec<Annotation>, change: Change) -> Result<bool> {
    if change.before == change.after {
      return Ok(false);
    }
    if let Some(after) = &change.after {
      ensure!(
        after.id != 0,
        "Drawing identifier must be assigned before commit"
      );
      ensure!(
        after.paint_bounds().is_some(),
        "Cannot commit an empty drawing"
      );
      ensure!(
        matches!(after.width, 3 | 6 | 10),
        "Invalid drawing stroke width"
      );
      ensure!(
        after.points.len() <= crate::screenshot::annotations::MAX_POINTS,
        "Drawing exceeds point limit"
      );
      ensure!(
        after.corner_radius >= 0
          && (after.tool == crate::screenshot::annotations::Tool::Rectangle
            || after.corner_radius == 0),
        "Invalid rectangle corner radius"
      );
      ensure!(
        (0..360_000).contains(&after.rotation_millidegrees)
          && (matches!(
            after.tool,
            crate::screenshot::annotations::Tool::Rectangle
              | crate::screenshot::annotations::Tool::Ellipse
              | crate::screenshot::annotations::Tool::Text
              | crate::screenshot::annotations::Tool::Pen
              | crate::screenshot::annotations::Tool::Highlighter
          ) || after.rotation_millidegrees == 0),
        "Invalid shape rotation"
      );
      ensure!(
        crate::screenshot::path::inverse(after.path_transform).is_some()
          && (after.tool.freehand() || after.path_transform == crate::screenshot::path::IDENTITY),
        "Invalid path transform"
      );
      ensure!(
        after.curve_controls.len() <= crate::screenshot::path::MAX_NODES
          && (after.tool == crate::screenshot::annotations::Tool::Arrow
            || after.curve_controls.is_empty()),
        "Invalid path controls"
      );
      ensure!(
        after.pen_nodes.len() <= crate::screenshot::path::MAX_NODES
          && (after.tool.freehand() || after.pen_nodes.is_empty())
          && after.pen_nodes.iter().all(|i| *i < after.points.len())
          && after.pen_nodes.windows(2).all(|p| p[0] < p[1]),
        "Invalid pen nodes"
      );
      ensure!(
        after.tool != crate::screenshot::annotations::Tool::Arrow
          || crate::screenshot::arrow::Arrow::from_annotation(after).is_some(),
        "Invalid arrow geometry"
      );
      match (&after.tool, &after.label) {
        (
          crate::screenshot::annotations::Tool::Text,
          Some(crate::screenshot::annotations::Label::Text(t)),
        ) => {
          ensure!(
            (12..=64).contains(&t.size)
              && (1..=8192).contains(&t.wrap_width)
              && t.extent.0 > 0
              && t.extent.1 > 0
              && t.content.chars().count() <= 2048,
            "Invalid text annotation"
          );
        },
        (
          crate::screenshot::annotations::Tool::Marker,
          Some(crate::screenshot::annotations::Label::Number(n)),
        ) => ensure!((1..=999).contains(n), "Invalid marker number"),
        (
          crate::screenshot::annotations::Tool::Text | crate::screenshot::annotations::Tool::Marker,
          _,
        ) => anyhow::bail!("Label drawing lacks its payload"),
        (_, Some(_)) => anyhow::bail!("Stroke drawing cannot contain a label"),
        _ => {},
      }
      if let Some(before) = &change.before {
        ensure!(
          before.id == after.id,
          "An edit cannot change a drawing identifier"
        );
      } else {
        ensure!(
          drawings.len() < crate::screenshot::annotations::MAX_ANNOTATIONS,
          "Drawing count limit reached"
        );
        ensure!(
          !drawings.iter().any(|drawing| drawing.id == after.id),
          "Drawing identifier already exists"
        );
      }
    }
    ensure!(
      change.bytes() <= MAX_BYTES,
      "Drawing edit exceeds history budget"
    );
    if let Some((before, after)) = change.counter {
      ensure!(
        before == self.marker_counter && after == before + 1 && after <= 999,
        "Marker counter does not match history"
      );
    }
    change.apply(drawings, true)?;
    if let Some((_, after)) = change.counter {
      self.marker_counter = after;
    }
    for abandoned in self.redo.drain(..) {
      self.bytes -= abandoned.bytes();
    }
    self.bytes += change.bytes();
    self.undo.push_back(Batch(vec![change], None));
    while self.undo.len() > MAX_COMMANDS || self.bytes > MAX_BYTES {
      // Each iteration consumes one command; inconsistent accounting must never spin.
      let oldest = self
        .undo
        .pop_front()
        .context("Drawing history budget accounting is inconsistent")?;
      self.bytes -= oldest.bytes();
      self.limited = true;
    }
    Ok(true)
  }

  pub fn commit_batch(
    &mut self,
    drawings: &mut Vec<Annotation>,
    changes: Vec<Change>,
  ) -> Result<bool> {
    ensure!(
      changes.iter().all(|change| change.counter.is_none()),
      "Batch cannot change marker numbering"
    );
    let changes: Vec<_> = changes
      .into_iter()
      .filter(|change| change.before != change.after)
      .collect();
    if changes.is_empty() {
      return Ok(false);
    }
    let batch = Batch(changes, None);
    ensure!(batch.bytes() <= MAX_BYTES, "Batch exceeds history budget");
    let mut staged = drawings.clone();
    let mut validation = History::default();
    for change in &batch.0 {
      validation.commit(&mut staged, change.clone())?;
    }
    *drawings = staged;
    for abandoned in self.redo.drain(..) {
      self.bytes -= abandoned.bytes();
    }
    self.bytes += batch.bytes();
    self.undo.push_back(batch);
    while self.undo.len() > MAX_COMMANDS || self.bytes > MAX_BYTES {
      let oldest = self
        .undo
        .pop_front()
        .context("Drawing history budget accounting is inconsistent")?;
      self.bytes -= oldest.bytes();
      self.limited = true;
    }
    Ok(true)
  }

  pub fn undo(&mut self, drawings: &mut Vec<Annotation>) -> Result<bool> {
    let Some(change) = self.undo.back() else {
      return Ok(false);
    };
    change.apply(drawings, false)?;
    self.pending_crop = change.1.map(|(before, _)| before);
    if let Some((before, _)) = change.counter() {
      self.marker_counter = before;
    }
    if let Some(change) = self.undo.pop_back() {
      self.redo.push(change);
    }
    Ok(true)
  }

  pub fn redo(&mut self, drawings: &mut Vec<Annotation>) -> Result<bool> {
    let Some(change) = self.redo.last() else {
      return Ok(false);
    };
    change.apply(drawings, true)?;
    self.pending_crop = change.1.map(|(_, after)| after);
    if let Some((_, after)) = change.counter() {
      self.marker_counter = after;
    }
    if let Some(change) = self.redo.pop() {
      self.undo.push_back(change);
    }
    Ok(true)
  }

  pub fn commit_crop(
    &mut self,
    before: crate::overlay::selection::Rect,
    after: crate::overlay::selection::Rect,
  ) -> Result<()> {
    ensure!(
      [before, after].iter().all(|r| r.x.is_finite()
        && r.y.is_finite()
        && r.w.is_finite()
        && r.h.is_finite()
        && r.w > 0.0
        && r.h > 0.0),
      "Invalid crop history rectangle"
    );
    if (before.x, before.y, before.w, before.h) == (after.x, after.y, after.w, after.h) {
      return Ok(());
    }
    for abandoned in self.redo.drain(..) {
      self.bytes -= abandoned.bytes();
    }
    let batch = Batch(Vec::new(), Some((before, after)));
    self.bytes += batch.bytes();
    self.undo.push_back(batch);
    while self.undo.len() > MAX_COMMANDS || self.bytes > MAX_BYTES {
      let oldest = self
        .undo
        .pop_front()
        .context("Crop history accounting is inconsistent")?;
      self.bytes -= oldest.bytes();
      self.limited = true;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::screenshot::annotations::Tool;

  fn drawing(id: u64) -> Annotation {
    Annotation {
      corner_radius: 0,
      rotation_millidegrees: 0,
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      label: None,
      id,
      tool: Tool::Pen,
      points: vec![(3, 4)],
      color: [255, 0, 0],
      width: 3,
    }
  }

  #[test]
  fn invalid_batch_preserves_document_history_and_redo() {
    let mut history = History::default();
    let mut drawings = vec![drawing(1), drawing(2)];
    history
      .commit(
        &mut drawings,
        Change {
          counter: None,
          index: 0,
          before: Some(drawing(1)),
          after: None,
        },
      )
      .expect("delete");
    history.undo(&mut drawings).expect("restore");
    let original = drawings.clone();
    assert!(
      history
        .commit_batch(
          &mut drawings,
          vec![
            Change {
              counter: None,
              index: 0,
              before: Some(drawing(1)),
              after: Some(Annotation {
                width: 6,
                ..drawing(1)
              })
            },
            Change {
              counter: None,
              index: 1,
              before: Some(drawing(99)),
              after: None
            }
          ]
        )
        .is_err()
    );
    assert_eq!(drawings, original);
    assert!(history.can_redo());
    assert!(!history.can_undo());
  }

  #[test]
  fn add_move_style_delete_restore_exact_objects_and_order() {
    let mut history = History::default();
    let mut drawings = Vec::new();
    let original = drawing(1);
    history
      .commit(
        &mut drawings,
        Change {
          counter: None,
          index: 0,
          before: None,
          after: Some(original.clone()),
        },
      )
      .expect("add");
    let moved = Annotation {
      points: vec![(-10, 20)],
      width: 10,
      ..original.clone()
    };
    history
      .commit(
        &mut drawings,
        Change {
          counter: None,
          index: 0,
          before: Some(original.clone()),
          after: Some(moved.clone()),
        },
      )
      .expect("edit");
    history
      .commit(
        &mut drawings,
        Change {
          counter: None,
          index: 0,
          before: Some(moved.clone()),
          after: None,
        },
      )
      .expect("delete");
    assert!(drawings.is_empty());
    history.undo(&mut drawings).expect("restore deletion");
    assert_eq!(drawings, [moved]);
    history.undo(&mut drawings).expect("restore edit");
    assert_eq!(drawings, [original]);
    history.undo(&mut drawings).expect("restore addition");
    assert!(drawings.is_empty());
    for _ in 0..3 {
      history.redo(&mut drawings).expect("redo");
    }
    assert!(drawings.is_empty());
  }

  #[test]
  fn no_op_and_failed_edit_preserve_redo_and_document() {
    let mut history = History::default();
    let mut drawings = Vec::new();
    history
      .commit(
        &mut drawings,
        Change {
          counter: None,
          index: 0,
          before: None,
          after: Some(drawing(1)),
        },
      )
      .expect("add");
    history.undo(&mut drawings).expect("undo");
    assert!(
      !history
        .commit(
          &mut drawings,
          Change {
            counter: None,
            index: 0,
            before: None,
            after: None
          }
        )
        .expect("no-op")
    );
    assert!(
      history
        .commit(
          &mut drawings,
          Change {
            counter: None,
            index: 0,
            before: Some(drawing(2)),
            after: None
          }
        )
        .is_err()
    );
    assert!(history.can_redo());
    assert!(drawings.is_empty());
  }

  #[test]
  fn command_and_byte_limits_preserve_visible_document() {
    for large in [false, true] {
      let mut history = History::default();
      let mut drawings = vec![drawing(1)];
      if large {
        drawings[0].points = vec![(1, 2); 4096];
      }
      for i in 0..600 {
        let before = drawings[0].clone();
        let after = Annotation {
          width: if i % 2 == 0 { 3 } else { 6 },
          ..before.clone()
        };
        history
          .commit(
            &mut drawings,
            Change {
              counter: None,
              index: 0,
              before: Some(before),
              after: Some(after),
            },
          )
          .expect("bounded edit");
      }
      assert!(history.limited);
      assert!(history.bytes <= MAX_BYTES);
      assert!(history.undo.len() <= MAX_COMMANDS);
      assert_eq!(drawings.len(), 1);
      assert_eq!(drawings[0].id, 1);
      while history.undo(&mut drawings).expect("undo retained history") {}
      assert_eq!(drawings.len(), 1, "eviction cannot remove a visible object");
    }
  }

  fn add(after: Annotation) -> Change {
    Change {
      counter: None,
      index: 0,
      before: None,
      after: Some(after),
    }
  }

  fn text(content: String, size: i32) -> Annotation {
    Annotation {
      tool: Tool::Text,
      points: vec![(10, 10)],
      label: Some(crate::screenshot::annotations::Label::Text(
        crate::screenshot::annotations::TextData {
          content,
          size,
          wrap_width: 200,
          extent: (40, 20),
          appearance: Default::default(),
        },
      )),
      ..drawing(7)
    }
  }

  #[test]
  fn every_invalid_drawing_is_refused_before_it_reaches_the_document() {
    use crate::screenshot::annotations::{Label, MAX_ANNOTATIONS, MAX_POINTS};
    let rectangle = Annotation {
      tool: Tool::Rectangle,
      points: vec![(0, 0), (40, 30)],
      ..drawing(5)
    };
    let invalid = [
      (
        "unassigned id",
        Annotation {
          id: 0,
          ..drawing(1)
        },
      ),
      (
        "empty",
        Annotation {
          points: Vec::new(),
          ..drawing(1)
        },
      ),
      (
        "stroke width",
        Annotation {
          width: 4,
          ..drawing(1)
        },
      ),
      (
        "point limit",
        Annotation {
          points: vec![(1, 1); MAX_POINTS + 1],
          ..drawing(1)
        },
      ),
      (
        "corner radius off a rectangle",
        Annotation {
          corner_radius: 4,
          ..drawing(1)
        },
      ),
      (
        "rotation out of range",
        Annotation {
          rotation_millidegrees: 360_000,
          ..drawing(1)
        },
      ),
      (
        "path transform off a pen",
        Annotation {
          path_transform: [2_000_000, 0, 0, 1_000_000, 0, 0],
          ..rectangle.clone()
        },
      ),
      (
        "curve controls off an arrow",
        Annotation {
          curve_controls: vec![[(1, 1), (2, 2)]],
          ..drawing(1)
        },
      ),
      (
        "unsorted pen nodes",
        Annotation {
          points: vec![(1, 1), (5, 5), (9, 9)],
          pen_nodes: vec![2, 1],
          ..drawing(1)
        },
      ),
      ("text size", text("Hi".into(), 4)),
      (
        "text without its label",
        Annotation {
          label: None,
          ..text("Hi".into(), 16)
        },
      ),
      (
        "stroke with a label",
        Annotation {
          label: Some(Label::Number(1)),
          ..drawing(1)
        },
      ),
      (
        "marker number",
        Annotation {
          tool: Tool::Marker,
          label: Some(Label::Number(0)),
          ..drawing(1)
        },
      ),
      (
        "history budget",
        text(
          {
            let mut content = String::with_capacity(MAX_BYTES + 1);
            content.push_str("Hi");
            content
          },
          16,
        ),
      ),
    ];
    for (case, after) in invalid {
      let mut history = History::default();
      let mut drawings = Vec::new();
      assert!(history.commit(&mut drawings, add(after)).is_err(), "{case}");
      assert!(drawings.is_empty() && !history.can_undo(), "{case}");
    }
    let mut history = History::default();
    let mut drawings = vec![drawing(1)];
    // An edit keeps its drawing's id; a new drawing needs a fresh id.
    let edit = Change {
      counter: None,
      index: 0,
      before: Some(drawing(1)),
      after: Some(Annotation {
        width: 6,
        ..drawing(2)
      }),
    };
    assert!(history.commit(&mut drawings, edit).is_err());
    assert!(history.commit(&mut drawings, add(drawing(1))).is_err());
    // An index past the end and a marker counter that skips a number are refused.
    let past_end = Change {
      index: 5,
      ..add(drawing(3))
    };
    assert!(history.commit(&mut drawings, past_end).is_err());
    let skipped = Change {
      counter: Some((0, 2)),
      ..add(drawing(3))
    };
    assert!(history.commit(&mut drawings, skipped).is_err());
    assert_eq!(drawings, vec![drawing(1)]);
    // The drawing count limit.
    let mut full: Vec<_> = (1..=MAX_ANNOTATIONS as u64).map(drawing).collect();
    assert!(history.commit(&mut full, add(drawing(9999))).is_err());
    assert_eq!(full.len(), MAX_ANNOTATIONS);
  }

  #[test]
  fn batches_and_crops_are_bounded_and_drop_the_oldest_steps() {
    let mut history = History::default();
    let mut drawings = Vec::new();
    let numbered = Change {
      counter: Some((0, 1)),
      ..add(drawing(1))
    };
    assert!(history.commit_batch(&mut drawings, vec![numbered]).is_err());
    assert!(!history.commit_batch(&mut drawings, Vec::new()).unwrap());
    // Alternately adding and removing one drawing fills the history past its step limit.
    for step in 0..(MAX_COMMANDS + 2) {
      let change = if step % 2 == 0 {
        add(drawing(1))
      } else {
        Change {
          counter: None,
          index: 0,
          before: Some(drawing(1)),
          after: None,
        }
      };
      assert!(history.commit_batch(&mut drawings, vec![change]).unwrap());
    }
    assert!(history.limited, "the oldest batch was dropped");
    assert!(drawings.is_empty());
    let crop = |w: f64| crate::overlay::selection::Rect {
      x: 0.,
      y: 0.,
      w,
      h: 10.,
    };
    let mut crops = History::default();
    assert!(crops.commit_crop(crop(10.), crop(f64::NAN)).is_err());
    assert!(crops.commit_crop(crop(10.), crop(0.)).is_err());
    crops.commit_crop(crop(10.), crop(10.)).unwrap();
    assert!(!crops.can_undo(), "an unchanged crop is not a step");
    for step in 1..=(MAX_COMMANDS + 1) {
      crops
        .commit_crop(crop(step as f64), crop(step as f64 + 1.))
        .unwrap();
    }
    assert!(crops.limited, "the oldest crop step was dropped");
    let mut nothing = Vec::new();
    assert!(crops.undo(&mut nothing).unwrap());
    assert_eq!(
      crops.pending_crop.map(|r| r.w),
      Some((MAX_COMMANDS + 1) as f64)
    );
    // A new crop after an undo abandons the redo step.
    crops.commit_crop(crop(3.), crop(4.)).unwrap();
    assert!(!crops.can_redo());
  }
}
