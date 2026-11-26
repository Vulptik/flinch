//! Blur regions for screenshots. Preview and export both read the frozen capture,
//! so a region looks the same on screen and in the saved image. The pixel work lives in
//! `flinch-blur`, which debug builds still compile optimized; see that crate for the method.

use super::annotations::{Annotation, Tool};
use crate::overlay::Selection;
pub(crate) use flinch_blur::{Layer, Patch, SOFT_EDGE};

/// The region a blur drawing covers; other drawings have none.
fn region(annotation: &Annotation) -> Option<flinch_blur::Region> {
  if annotation.tool != Tool::Blur {
    return None;
  }
  Some(flinch_blur::Region {
    from: *annotation.points.first()?,
    to: *annotation.points.last()?,
    seed: annotation.id,
  })
}

pub(crate) fn patch(
  source: &[u8],
  source_width: u32,
  source_height: u32,
  annotation: &Annotation,
) -> Option<Patch> {
  flinch_blur::patch(source, source_width, source_height, region(annotation)?)
}

/// Writes every blur region into a crop of the frozen capture.
pub(crate) fn apply(
  crop: &mut [u8],
  selection: &Selection,
  source: &[u8],
  source_width: u32,
  source_height: u32,
  annotations: &[Annotation],
) {
  let (crop_width, crop_height) = (selection.width as i32, selection.height as i32);
  if crop.len() != crop_width as usize * crop_height as usize * 4 {
    return;
  }
  for patch in annotations
    .iter()
    .filter_map(|drawing| patch(source, source_width, source_height, drawing))
  {
    flinch_blur::blend(
      crop,
      (selection.x as i32, selection.y as i32),
      crop_width,
      &patch,
    );
  }
}

/// Builds the smallest preview layer holding every blur region inside `clip`.
pub(crate) fn layer<'a>(
  source: &[u8],
  source_width: u32,
  source_height: u32,
  drawings: impl Iterator<Item = &'a Annotation>,
  clip: (i32, i32, i32, i32),
) -> Option<Layer> {
  flinch_blur::layer(
    source,
    source_width,
    source_height,
    drawings.filter_map(region),
    clip,
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  fn drawing(points: Vec<(i32, i32)>) -> Annotation {
    Annotation {
      path_transform: crate::screenshot::path::IDENTITY,
      curve_controls: Vec::new(),
      pen_nodes: Vec::new(),
      rotation_millidegrees: 0,
      corner_radius: 0,
      label: None,
      id: 7,
      tool: Tool::Blur,
      points,
      color: [0, 0, 0],
      width: 3,
    }
  }

  fn stripes(width: u32, height: u32) -> Vec<u8> {
    (0..width * height)
      .flat_map(|index| {
        let value = if (index % width / 2).is_multiple_of(2) {
          0
        } else {
          255
        };
        [value, value, value, 255]
      })
      .collect()
  }

  fn selection(x: u32, y: u32, width: u32, height: u32) -> Selection {
    Selection {
      x,
      y,
      width,
      height,
      source_app: None,
    }
  }

  fn alpha(patch: &Patch, x: i32, y: i32) -> u8 {
    patch.rgba[((y - patch.top) * patch.width + x - patch.left) as usize * 4 + 3]
  }

  #[test]
  fn regions_lose_fine_detail() {
    let source = stripes(128, 64);
    let patch = patch(&source, 128, 64, &drawing(vec![(0, 0), (128, 64)])).expect("patch");
    assert_eq!((patch.width, patch.height), (128, 64));
    let row = &patch.rgba[32 * 128 * 4..33 * 128 * 4];
    for pair in row.as_chunks::<4>().0.iter().collect::<Vec<_>>().windows(2) {
      assert!(pair[0][0].abs_diff(pair[1][0]) <= 8, "{pair:?}");
    }
  }

  #[test]
  fn the_drawn_area_is_fully_covered_and_only_the_outside_fades() {
    let source = stripes(200, 200);
    let region = drawing(vec![(60, 70), (140, 130)]);
    let patch = patch(&source, 200, 200, &region).expect("patch");
    for y in 70..130 {
      for x in 60..140 {
        assert_eq!(alpha(&patch, x, y), 255, "{x},{y}");
      }
    }
    assert!(alpha(&patch, 54, 100) > 0 && alpha(&patch, 54, 100) < 255);
    assert_eq!(alpha(&patch, patch.left, patch.top), 0);
    assert_eq!(alpha(&patch, 100, patch.top), 0);
  }

  #[test]
  fn repaints_are_identical_and_drawings_differ() {
    let source = stripes(64, 64);
    let first = drawing(vec![(0, 0), (64, 64)]);
    assert_eq!(
      patch(&source, 64, 64, &first),
      patch(&source, 64, 64, &first)
    );
    let other = Annotation {
      id: 8,
      ..first.clone()
    };
    assert_ne!(
      patch(&source, 64, 64, &first),
      patch(&source, 64, 64, &other)
    );
  }

  #[test]
  fn regions_are_clamped_and_reversed_corners_work() {
    let source = stripes(32, 32);
    let patch = patch(&source, 32, 32, &drawing(vec![(40, 20), (-8, 4)])).expect("patch");
    assert_eq!(
      (patch.left, patch.top, patch.width, patch.height),
      (0, 0, 32, 32)
    );
    assert_eq!(patch.rgba.len(), 32 * 32 * 4);
    let outside = drawing(vec![(40, 40), (60, 60)]);
    assert_eq!(super::patch(&source, 32, 32, &outside), None);
    let other_tool = Annotation {
      tool: Tool::Rectangle,
      ..outside
    };
    assert_eq!(super::patch(&source, 32, 32, &other_tool), None);
    assert_eq!(
      super::patch(&source[4..], 32, 32, &drawing(vec![(0, 0), (8, 8)])),
      None
    );
  }

  #[test]
  fn export_matches_the_preview_layer_inside_the_crop_only() {
    let source = stripes(96, 96);
    let regions = [
      drawing(vec![(24, 24), (72, 56)]),
      Annotation {
        id: 8,
        ..drawing(vec![(40, 40), (80, 80)])
      },
    ];
    let preview = layer(&source, 96, 96, regions.iter(), (0, 0, 96, 96)).expect("layer");
    let chosen = selection(32, 32, 32, 32);
    let mut crop =
      crate::screenshot::encoding::crop_rgba_direct(&source, 96, 32, 32, 32, 32).expect("crop");
    apply(&mut crop, &chosen, &source, 96, 96, &regions);
    for y in 32..64 {
      for x in 32..64 {
        let exported = &crop[((y - 32) * 32 + x - 32) as usize * 4..][..3];
        let shown = ((y - preview.top) * preview.width + x - preview.left) as usize * 4;
        assert_eq!(exported, &preview.rgba[shown..shown + 3], "{x},{y}");
      }
    }
  }

  #[test]
  fn a_soft_edge_cannot_uncover_a_neighbouring_region() {
    let source = stripes(200, 100);
    let first = drawing(vec![(20, 20), (100, 80)]);
    let second = Annotation {
      id: 8,
      ..drawing(vec![(110, 20), (180, 80)])
    };
    let alone = layer(&source, 200, 100, std::iter::once(&first), (0, 0, 200, 100)).expect("layer");
    let both = layer(
      &source,
      200,
      100,
      [&first, &second].into_iter(),
      (0, 0, 200, 100),
    )
    .expect("layer");
    let pixel = |layer: &Layer, x: i32, y: i32| {
      let at = ((y - layer.top) * layer.width + x - layer.left) as usize * 4;
      layer.rgba[at]
    };
    // Inside the first region and the second's soft edge: still blur, never stripes.
    let (a, b) = (pixel(&both, 98, 50), pixel(&both, 99, 50));
    assert!(a.abs_diff(b) <= 12, "{a} {b}");
    assert!(pixel(&alone, 98, 50) > 0);
  }

  #[test]
  fn clipped_layers_stay_in_bounds_and_empty_clips_paint_nothing() {
    let source = stripes(64, 64);
    let region = drawing(vec![(8, 8), (56, 56)]);
    let clipped = layer(&source, 64, 64, std::iter::once(&region), (0, 0, 32, 200)).expect("layer");
    assert_eq!((clipped.left, clipped.top), (0, 0));
    assert_eq!((clipped.width, clipped.height), (32, 64));
    assert_eq!(clipped.rgba.len(), 32 * 64 * 4);
    assert!(layer(&source, 64, 64, std::iter::once(&region), (70, 70, 90, 90)).is_none());
    assert!(layer(&source, 64, 64, std::iter::empty(), (0, 0, 64, 64)).is_none());
  }

  #[test]
  fn mismatched_crops_are_left_alone() {
    let source = stripes(16, 16);
    let mut crop = vec![9; 12];
    apply(
      &mut crop,
      &selection(0, 0, 16, 16),
      &source,
      16,
      16,
      &[drawing(vec![(0, 0), (8, 8)])],
    );
    assert_eq!(crop, vec![9; 12]);
  }

  #[test]
  fn regions_select_anywhere_inside_resize_and_never_rotate() {
    let region = drawing(vec![(10, 10), (50, 40)]);
    assert!(region.hit((30.0, 25.0)));
    assert!(!region.hit((70.0, 25.0)));
    assert!(crate::screenshot::shape::handles(&region).is_some());
    assert_eq!(crate::screenshot::shape::rotation_handle(&region), None);
    let bounds = region.paint_bounds().expect("bounds");
    assert!(bounds.left <= 10 - SOFT_EDGE && bounds.bottom >= 40 + SOFT_EDGE);
  }

  #[test]
  fn blurred_text_cannot_be_read_back() {
    let image = image::load_from_memory(include_bytes!("../../tests/fixtures/ocr/en-hotkeys.png"))
      .expect("fixture")
      .to_rgba8();
    let (width, height) = image.dimensions();
    let source = image.into_raw();
    let chosen = selection(0, 0, width, height);
    let mut output = source.clone();
    let region = drawing(vec![(0, 0), (width as i32, height as i32)]);
    apply(
      &mut output,
      &chosen,
      &source,
      width,
      height,
      std::slice::from_ref(&region),
    );
    let limits = crate::ocr::OcrLimits {
      cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
      deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
    };
    let text = crate::ocr::recognize(&output, width, height, &limits).expect("text");
    assert!(!text.contains("Capture"), "{text}");
    assert!(!text.contains("PrintScreen"), "{text}");
  }
}
