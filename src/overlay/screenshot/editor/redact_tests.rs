use super::super::{AnnotationEditor, DrawingLimit, FrozenPixels};
use super::*;
use crate::overlay::selection::{Rect, SelectionInteraction};
use crate::screenshot::annotations::MAX_ANNOTATIONS;
use windows::Win32::UI::WindowsAndMessaging::{
  CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
};

fn line(left: i32, top: i32, right: i32, bottom: i32) -> TextBox {
  TextBox {
    left,
    top,
    right,
    bottom,
  }
}

struct Hidden(HWND);
impl Hidden {
  fn new() -> Self {
    // SAFETY: built-in class, hidden thread-owned fixture; it never takes focus or input.
    Self(
      unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE::default(),
          windows::core::w!("STATIC"),
          windows::core::w!(""),
          WS_POPUP,
          0,
          0,
          10,
          10,
          None,
          None,
          None,
          None,
        )
      }
      .expect("hidden window"),
    )
  }
}
impl Drop for Hidden {
  fn drop(&mut self) {
    // SAFETY: destroys only this fixture's own hidden window on its creating thread.
    if let Err(error) = unsafe { DestroyWindow(self.0) } {
      tracing::warn!(%error, "Hidden window cleanup failed");
    }
  }
}

fn confirmed(rect: Rect, width: i32, height: i32) -> SelectionControllerState {
  let mut controller = SelectionControllerState::new(width as u32, height as u32, 0, 0);
  controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  controller
}

fn editor_over(image: &image::RgbaImage) -> AnnotationEditor {
  let mut editor = AnnotationEditor::new("en".into()).with_frozen_pixels(FrozenPixels {
    rgba: Arc::new(image.as_raw().clone()),
    width: image.width(),
    height: image.height(),
  });
  editor.monitors.clear();
  editor
}

/// Waits for the background job, then delivers its message as the overlay thread would.
fn deliver(editor: &mut AnnotationEditor, controller: &SelectionControllerState, hwnd: HWND) {
  let started = Instant::now();
  while editor.redaction.finished().is_none() {
    assert!(
      started.elapsed() < Duration::from_secs(60),
      "detection never finished"
    );
    std::thread::sleep(Duration::from_millis(10));
  }
  let id = editor.redaction.job.as_ref().expect("job").id;
  editor.redaction_ready(controller, hwnd, id);
}

#[test]
fn lines_become_padded_covers_inside_the_crop_in_reading_order() {
  let crop = [100, 50, 400, 250];
  let covers = plan(
    &[
      line(10, 80, 120, 96),
      line(0, 0, 40, 12),
      line(250, 190, 300, 200),
    ],
    crop,
    256,
  );
  assert_eq!(
    covers,
    [
      // Padding stops at the crop's own edges.
      [100, 50, 142, 64],
      [108, 128, 222, 148],
      [348, 238, 400, 250],
    ]
  );
}

#[test]
fn overlapping_lines_share_one_cover_but_separate_lines_stay_apart() {
  let crop = [0, 0, 1000, 1000];
  // Two detections of one line overlap; a line 10 px below stays its own cover.
  let covers = plan(
    &[
      line(10, 10, 100, 30),
      line(90, 12, 200, 28),
      line(10, 40, 100, 60),
    ],
    crop,
    256,
  );
  assert_eq!(covers, [[8, 8, 202, 32], [8, 38, 102, 62]]);
}

#[test]
fn too_many_lines_merge_into_blocks_and_nothing_is_left_uncovered() {
  let crop = [0, 0, 2000, 2000];
  // Three paragraphs of 100 lines each, 20 px apart inside, 300 px between paragraphs.
  let mut lines = Vec::new();
  for paragraph in 0..3 {
    for row in 0..100 {
      let top = paragraph * 600 + row * 5;
      lines.push(line(paragraph * 650, top, paragraph * 650 + 400, top + 3));
    }
  }
  let all = plan(&lines, crop, 256);
  assert!(all.len() <= 256);
  for capacity in [256, 3, 1] {
    let covers = plan(&lines, crop, capacity);
    assert!(covers.len() <= capacity, "{capacity}: {}", covers.len());
    for text in &lines {
      assert!(
        covers.iter().any(|[l, t, r, b]| *l <= text.left
          && *t <= text.top
          && *r >= text.right
          && *b >= text.bottom),
        "{capacity}: {text:?} uncovered"
      );
    }
  }
  assert_eq!(
    plan(&lines, crop, 3).len(),
    3,
    "paragraphs merge before the page does"
  );
  assert!(plan(&[], crop, 256).is_empty());
}

#[test]
fn covers_are_one_undo_step_with_fresh_ids_and_respect_the_drawing_limit() {
  let mut editor = AnnotationEditor::new("en".into());
  editor.width = 6;
  let crop = [0, 0, 500, 500];
  editor.cover_lines(&[line(10, 10, 100, 30), line(10, 60, 100, 80)], crop);
  assert_eq!(editor.annotations.len(), 2);
  assert!(
    editor
      .annotations
      .iter()
      .all(|a| a.tool == Tool::Blur && a.width == 6)
  );
  assert_eq!(
    editor.annotations.iter().map(|a| a.id).collect::<Vec<_>>(),
    [1, 2]
  );
  assert_eq!(editor.next_id, 3);
  editor.undo();
  assert!(
    editor.annotations.is_empty(),
    "one undo removes every cover"
  );
  editor.redo();
  assert_eq!(editor.annotations.len(), 2);

  editor.cover_lines(&[], crop);
  assert_eq!(editor.redact_notice, Some("annotation_error_redact_none"));
  assert_eq!(
    editor.toolbar_error(),
    Some(("annotation_error_redact_none", 11))
  );

  // Near the limit the lines merge to fit, still covering all of them; at the limit nothing is
  // added.
  let lines: Vec<_> = (0..40)
    .map(|row| line(0, row * 20, 50, row * 20 + 8))
    .collect();
  let filler = editor.annotations[0].clone();
  while editor.annotations.len() < MAX_ANNOTATIONS - 3 {
    let mut copy = filler.clone();
    copy.id = editor.next_id;
    editor.next_id += 1;
    editor.annotations.push(copy);
  }
  let before = editor.annotations.len();
  let page = [0, 0, 1000, 1000];
  editor.cover_lines(&lines, page);
  assert!((before + 1..=MAX_ANNOTATIONS).contains(&editor.annotations.len()));
  for text in &lines {
    assert!(
      editor.annotations[before..].iter().any(|a| {
        let [(l, t), (r, b)] = [a.points[0], a.points[1]];
        l <= text.left && t <= text.top && r >= text.right && b >= text.bottom
      }),
      "{text:?} uncovered"
    );
  }
  while editor.annotations.len() < MAX_ANNOTATIONS {
    let mut copy = filler.clone();
    copy.id = editor.next_id;
    editor.next_id += 1;
    editor.annotations.push(copy);
  }
  editor.clear_toolbar_error();
  // Text already covered needs no room, so it reports nothing at the limit.
  editor.cover_lines(&lines, page);
  assert!(editor.limited.is_none());
  let elsewhere: Vec<_> = lines
    .iter()
    .map(|text| line(text.left + 600, text.top, text.right + 600, text.bottom))
    .collect();
  editor.cover_lines(&elsewhere, page);
  assert!(matches!(editor.limited, Some(DrawingLimit::Drawings)));
  assert_eq!(editor.annotations.len(), MAX_ANNOTATIONS);
}

#[test]
fn covering_again_adds_only_what_is_missing() {
  let mut editor = AnnotationEditor::new("en".into());
  let crop = [0, 0, 500, 500];
  let lines = [line(10, 10, 100, 30), line(10, 60, 100, 80)];
  editor.cover_lines(&lines, crop);
  assert_eq!(editor.annotations.len(), 2);

  editor.cover_lines(&lines, crop);
  assert_eq!(editor.annotations.len(), 2, "a second press adds nothing");
  assert!(editor.redact_notice.is_none());
  editor.undo();
  assert!(editor.annotations.is_empty(), "and no undo step");
  editor.redo();

  // A deleted cover comes back alone.
  editor.annotations.remove(0);
  editor.cover_lines(&lines, crop);
  assert_eq!(editor.annotations.len(), 2);

  // A hand-drawn Blur over everything counts; a rotated one does not.
  editor.clear();
  let mut big = Annotation {
    corner_radius: 0,
    rotation_millidegrees: 0,
    path_transform: crate::screenshot::path::IDENTITY,
    curve_controls: Vec::new(),
    pen_nodes: Vec::new(),
    label: None,
    id: editor.next_id,
    tool: Tool::Blur,
    points: vec![(400, 400), (0, 0)],
    color: COLORS[0],
    width: 4,
  };
  editor.next_id += 1;
  editor.annotations.push(big.clone());
  editor.cover_lines(&lines, crop);
  assert_eq!(editor.annotations.len(), 1);
  big.rotation_millidegrees = 15_000;
  editor.annotations[0] = big;
  editor.cover_lines(&lines, crop);
  assert_eq!(editor.annotations.len(), 3);
}

#[test]
fn redact_all_text_covers_real_interface_text_from_a_background_job() {
  let image = image::load_from_memory(include_bytes!(
    "../../../../tests/fixtures/ocr/en-hotkeys.png"
  ))
  .expect("fixture")
  .to_rgba8();
  let window = Hidden::new();
  let rect = Rect {
    x: 0.0,
    y: 0.0,
    w: image.width() as f64,
    h: image.height() as f64,
  };
  let controller = confirmed(rect, image.width() as i32, image.height() as i32);
  let mut editor = editor_over(&image);
  editor.tool = Tool::Blur;
  editor.redact_all_text(&controller, window.0);
  assert!(editor.redaction.busy(), "waits for the background job");
  deliver(&mut editor, &controller, window.0);
  assert!(!editor.redaction.busy());
  assert!(
    editor.annotations.len() >= 10,
    "{}",
    editor.annotations.len()
  );
  assert!(editor.annotations.iter().all(|a| a.tool == Tool::Blur));
  let rgba = image.as_raw();
  let lines = crate::ocr::detect_lines(
    rgba,
    image.width(),
    image.height(),
    &OcrLimits {
      cancel: Arc::new(AtomicBool::new(false)),
      deadline: Instant::now() + DEADLINE,
    },
  )
  .expect("lines");
  for text in lines {
    assert!(
      editor.annotations.iter().any(|a| {
        let [(l, t), (r, b)] = [a.points[0], a.points[1]];
        l <= text.left && t <= text.top && r >= text.right && b >= text.bottom
      }),
      "{text:?} uncovered"
    );
  }
  editor.undo();
  assert!(editor.annotations.is_empty());

  // A finished job is reused: the next request covers at once, without a new job.
  let job = editor.redaction.job.as_ref().expect("job").id;
  editor.redact_all_text(&controller, window.0);
  assert!(!editor.redaction.busy());
  assert_eq!(editor.redaction.job.as_ref().expect("job").id, job);
  assert!(!editor.annotations.is_empty());
}

#[test]
fn a_new_crop_restarts_detection_and_cancelling_adds_nothing() {
  let image = image::load_from_memory(include_bytes!(
    "../../../../tests/fixtures/ocr/en-hotkeys.png"
  ))
  .expect("fixture")
  .to_rgba8();
  let window = Hidden::new();
  let (width, height) = (image.width() as i32, image.height() as i32);
  let whole = Rect {
    x: 0.0,
    y: 0.0,
    w: width as f64,
    h: height as f64,
  };
  let mut controller = confirmed(whole, width, height);
  let mut editor = editor_over(&image);
  editor.redact_all_text(&controller, window.0);
  let first = editor.redaction.job.as_ref().expect("job").id;
  // The crop moves before the result arrives: the request follows the new crop.
  let half = Rect {
    x: 0.0,
    y: 0.0,
    w: width as f64 / 2.0,
    h: height as f64,
  };
  controller = confirmed(half, width, height);
  while editor.redaction.finished().is_none() {
    std::thread::sleep(Duration::from_millis(10));
  }
  editor.redaction_ready(&controller, window.0, first);
  assert!(editor.redaction.busy(), "restarted for the new crop");
  assert_ne!(editor.redaction.job.as_ref().expect("job").id, first);
  deliver(&mut editor, &controller, window.0);
  assert!(!editor.annotations.is_empty());
  assert!(
    editor
      .annotations
      .iter()
      .all(|a| a.points[1].0 <= width / 2),
    "covers stay in the new crop"
  );

  // Esc while waiting: nothing is added, and the late result is ignored.
  editor.clear();
  editor.redact_all_text(&controller, window.0);
  let waiting = editor.redaction.job.as_ref().expect("job");
  let (id, cancel) = (waiting.id, Arc::clone(&waiting.cancel));
  editor.redaction.cancel();
  assert!(cancel.load(Ordering::Acquire));
  editor.redaction_ready(&controller, window.0, id);
  assert!(editor.annotations.is_empty());
  assert!(!editor.redaction.busy());
}

#[test]
fn detection_reports_images_too_large_and_crops_outside_the_capture() {
  let limits = OcrLimits {
    cancel: Arc::new(AtomicBool::new(false)),
    deadline: Instant::now() + DEADLINE,
  };
  let pixels = vec![255u8; 8 * 8 * 4];
  assert_eq!(
    detect(&pixels, 8, [0, 0, 16, 8], &limits),
    Some(Err(Failure::Failed))
  );
  assert_eq!(
    detect(&pixels, 8, [0, 0, 8, 8], &limits),
    Some(Ok(Vec::new()))
  );
  limits.cancel.store(true, Ordering::Release);
  assert_eq!(detect(&pixels, 8, [0, 0, 8, 8], &limits), None);
}

/// Detection time for a small and a 2560×1440 selection; run on demand with `--ignored`.
#[test]
#[ignore = "timing measurement"]
fn redaction_detection_timing() {
  let image = image::load_from_memory(include_bytes!(
    "../../../../tests/fixtures/ocr/en-hotkeys.png"
  ))
  .expect("fixture")
  .to_rgba8();
  let mut screen = image::RgbaImage::new(2560, 1440);
  for y in (0..1440).step_by(image.height() as usize) {
    for x in (0..2560).step_by(image.width() as usize) {
      image::imageops::replace(&mut screen, &image, i64::from(x), i64::from(y));
    }
  }
  let small = image::imageops::crop_imm(&image, 0, 0, 640, 200).to_image();
  for (name, picture) in [
    ("640x200", &small),
    ("fixture", &image),
    ("2560x1440", &screen),
  ] {
    let mut times = Vec::new();
    let mut found = 0;
    for _ in 0..5 {
      let limits = OcrLimits {
        cancel: Arc::new(AtomicBool::new(false)),
        deadline: Instant::now() + DEADLINE,
      };
      let started = Instant::now();
      let (width, height) = picture.dimensions();
      found = detect(
        picture.as_raw(),
        width,
        [0, 0, width as i32, height as i32],
        &limits,
      )
      .and_then(Result::ok)
      .map_or(0, |lines| lines.len());
      times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    println!(
      "{name}: {found} lines, median {:.1} ms, min {:.1} ms",
      times[2], times[0]
    );
  }
}
