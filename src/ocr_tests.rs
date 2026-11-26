use super::*;
use std::time::Duration;

fn limits() -> OcrLimits {
  OcrLimits {
    cancel: Arc::new(AtomicBool::new(false)),
    deadline: Instant::now() + Duration::from_secs(120),
  }
}

fn fixture() -> image::RgbaImage {
  image::load_from_memory(include_bytes!("../tests/fixtures/ocr/en-hotkeys.png"))
    .expect("fixture")
    .to_rgba8()
}

fn level(left: f32, top: f32, right: f32, bottom: f32, text: &str) -> Line {
  Line {
    region: Region {
      center: ((left + right) / 2.0, (top + bottom) / 2.0),
      axis: HORIZONTAL,
      length: right - left,
      thickness: bottom - top,
    },
    text: text.into(),
  }
}

fn tilted(center: Point, degrees: f32, length: f32, thickness: f32, text: &str) -> Line {
  let (sin, cos) = degrees.to_radians().sin_cos();
  Line {
    region: Region {
      center,
      axis: (cos, sin),
      length,
      thickness,
    },
    text: text.into(),
  }
}

fn rotated(image: &image::RgbaImage, degrees: f32) -> image::RgbaImage {
  let (width, height) = image.dimensions();
  let rgb = rgb_image(image.as_raw(), width, height).expect("image");
  let (sin, cos) = degrees.to_radians().sin_cos();
  let size = (width.max(height) as f32 * (cos.abs() + sin.abs())).ceil() as u32;
  let middle = size as f32 / 2.0;
  image::RgbaImage::from_fn(size, size, |x, y| {
    let (dx, dy) = (x as f32 + 0.5 - middle, y as f32 + 0.5 - middle);
    let source = (
      dx * cos + dy * sin + width as f32 / 2.0,
      dy * cos - dx * sin + height as f32 / 2.0,
    );
    if source.0 < 0.0 || source.1 < 0.0 || source.0 >= width as f32 || source.1 >= height as f32 {
      return image::Rgba([28, 28, 31, 255]);
    }
    let pixel = sample(&rgb, source.0, source.1);
    image::Rgba([pixel[0], pixel[1], pixel[2], 255])
  })
}

#[test]
fn bundled_models_read_real_interface_text_in_reading_order() {
  let image = fixture();
  let text = recognize(image.as_raw(), image.width(), image.height(), &limits()).expect("text");
  for label in [
    "SCREENSHOTS",
    "Capture and Edit",
    "Set shortcut",
    "GIF RECORDING",
    "PrintScreen",
    "Active Window",
  ] {
    assert!(text.contains(label), "missing {label} in {text}");
  }
  let (screenshots, videos) = (text.find("SCREENSHOTS"), text.find("VIDEOS"));
  assert!(screenshots < videos, "{text}");
  assert!(!text.contains('画'), "icon junk in {text}");
}

#[test]
fn tilted_text_is_straightened_before_reading() {
  let image = fixture();
  for degrees in [-25.0, -8.0, 8.0, 15.0, 40.0] {
    let turned = rotated(&image, degrees);
    let text =
      recognize(turned.as_raw(), turned.width(), turned.height(), &limits()).expect("text");
    for label in ["Capture and Edit", "GIF RECORDING", "PrintScreen"] {
      assert!(text.contains(label), "{degrees}: missing {label} in {text}");
    }
  }
}

#[test]
fn small_selections_are_enlarged_before_reading() {
  let image = fixture();
  let crop = imageops::crop_imm(&image, 200, 95, 200, 30).to_image();
  let text = recognize(crop.as_raw(), crop.width(), crop.height(), &limits()).expect("text");
  assert_eq!(text, "Area");
}

#[test]
fn a_blank_selection_reads_as_empty_without_the_recognizer() {
  let blank = vec![255; 200 * 100 * 4];
  assert_eq!(recognize(&blank, 200, 100, &limits()).expect("text"), "");
}

#[test]
fn jobs_stop_at_their_cancel_flag_and_deadline() {
  let image = fixture();
  let cancelled = limits();
  cancelled.cancel.store(true, Ordering::Release);
  assert!(matches!(
    recognize(image.as_raw(), image.width(), image.height(), &cancelled),
    Err(OcrError::Cancelled)
  ));
  let expired = OcrLimits {
    deadline: Instant::now(),
    ..limits()
  };
  assert!(matches!(
    recognize(image.as_raw(), image.width(), image.height(), &expired),
    Err(OcrError::TimedOut)
  ));
}

#[test]
fn image_limits_are_checked_before_any_allocation() {
  assert!(matches!(
    rgb_image(&[], 10_000, 10_000),
    Err(OcrError::TooLarge)
  ));
  assert!(matches!(rgb_image(&[], 0, 10), Err(OcrError::Failed(_))));
  assert!(matches!(rgb_image(&[0; 3], 1, 1), Err(OcrError::Failed(_))));
  let image = rgb_image(&[1, 2, 3, 255], 1, 1).expect("image");
  assert_eq!(image.as_raw(), &[1, 2, 3]);
}

#[test]
fn detector_input_stays_aligned_and_within_budget() {
  assert_eq!(detection_size(640, 640), (640, 640));
  assert_eq!(detection_size(200, 30), (416, 64));
  assert_eq!(detection_size(3840, 2160), (2048, 1152));
  assert_eq!(detection_size(7680, 16), (2048, 32));
  assert_eq!(detection_size(1, 1), (64, 64));
  let (width, height) = detection_size(1023, 517);
  assert_eq!((width % 32, height % 32), (0, 0));
}

#[test]
fn connected_text_areas_are_grown_scaled_and_filtered() {
  let (width, height) = (32, 16);
  let mut map = vec![0f32; width * height];
  for y in 6..10 {
    for x in 8..24 {
      map[y * width + x] = 0.9;
    }
  }
  map[0] = 0.9; // Below the minimum region size.
  for y in 12..16 {
    for x in 26..30 {
      map[y * width + x] = 0.4; // Weak on average.
    }
  }
  let found = regions(&map, width, height, 64, 32);
  assert_eq!(found.len(), 1);
  let region = found[0];
  assert_eq!(region.axis, HORIZONTAL);
  assert_eq!(region.center, (32.0, 16.0));
  let grow = 32.0 * 8.0 * REGION_GROWTH / (2.0 * 40.0);
  assert!((region.length - (32.0 + grow * 2.0)).abs() < 0.01);
  assert!((region.thickness - (8.0 + grow * 2.0)).abs() < 0.01);
}

#[test]
fn slanted_areas_keep_their_direction() {
  let (width, height) = (200, 120);
  let mut map = vec![0f32; width * height];
  let (sin, cos) = 12f32.to_radians().sin_cos();
  for step in 0..1400 {
    for across in 0..80 {
      let (along, side) = (step as f32 / 10.0 - 70.0, across as f32 / 10.0 - 4.0);
      let x = (100.0 + along * cos - side * sin).round() as usize;
      let y = (60.0 + along * sin + side * cos).round() as usize;
      map[y * width + x] = 0.9;
    }
  }
  let found = regions(&map, width, height, 200, 120);
  assert_eq!(found.len(), 1);
  assert!((found[0].degrees() - 12.0).abs() < 1.5, "{:?}", found[0]);
  assert!(found[0].length > 140.0);
}

#[test]
fn hull_and_rectangle_fit_a_rotated_square_corner_set() {
  let hull = convex_hull(vec![
    (0.0, 0.0),
    (4.0, 0.0),
    (2.0, 1.0),
    (4.0, 2.0),
    (0.0, 2.0),
    (0.0, 0.0),
  ]);
  assert_eq!(hull.len(), 4);
  let rectangle = tightest_rectangle(&hull).expect("rectangle");
  assert_eq!(rectangle.center, (2.0, 1.0));
  assert_eq!((rectangle.length, rectangle.thickness), (4.0, 2.0));
  assert!(rectangle.axis.0 > 0.0);
  assert_eq!(convex_hull(vec![(1.0, 1.0), (1.0, 1.0)]).len(), 1);
  assert_eq!(tightest_rectangle(&[(1.0, 1.0)]), None);
}

#[test]
fn only_clear_long_slants_count_as_tilt() {
  let region = |degrees: f32, length: f32, thickness: f32| {
    tilted((0.0, 0.0), degrees, length, thickness, "").region
  };
  assert!(is_tilted(&region(12.0, 100.0, 20.0)));
  assert!(is_tilted(&region(-40.0, 100.0, 20.0)));
  assert!(!is_tilted(&region(1.0, 100.0, 20.0)));
  assert!(!is_tilted(&region(85.0, 100.0, 20.0)));
  assert!(!is_tilted(&region(30.0, 24.0, 20.0)));
}

#[test]
fn level_lines_keep_exact_pixels_and_tilted_lines_are_sampled_upright() {
  let image = RgbImage::from_fn(40, 40, |x, y| image::Rgb([x as u8, y as u8, 0]));
  let level = level(10.0, 20.0, 30.0, 30.0, "").region;
  let crop = line_image(&image, level).expect("crop");
  assert_eq!(crop.dimensions(), (20, 10));
  assert_eq!(crop.get_pixel(0, 0), &image::Rgb([10, 20, 0]));
  let outside = Region {
    center: (-50.0, -50.0),
    ..level
  };
  assert!(line_image(&image, outside).is_none());
  let turned = tilted((20.0, 20.0), 90.0, 10.0, 4.0, "").region;
  let upright = line_image(&image, turned).expect("line");
  assert_eq!(upright.dimensions(), (10, 4));
  let first = upright.get_pixel(0, 0);
  let last = upright.get_pixel(9, 0);
  assert!(first[1] < last[1], "reading direction follows the axis");
  assert_eq!(first[0], last[0]);
}

#[test]
fn ctc_decoding_collapses_repeats_and_maps_blank_and_space() {
  let keys = ["a", "b"];
  let classes = 4;
  let step = |class: usize, score: f32| {
    let mut row = vec![0f32; classes];
    row[class] = score;
    row
  };
  let logits = [
    step(1, 0.8),
    step(1, 0.8),
    step(0, 0.9),
    step(1, 0.6),
    step(3, 1.0),
    step(2, 1.0),
  ]
  .concat();
  let (text, confidence) = decode(&logits, 6, classes, &keys);
  assert_eq!(text, "aa b");
  assert!((confidence - 0.85).abs() < 0.001);
  assert_eq!(
    decode(&step(0, 1.0), 1, classes, &keys),
    (String::new(), 0.0)
  );
}

#[test]
fn keys_match_the_recognizer_classes() {
  let keys: Vec<&str> = KEYS.lines().collect();
  assert_eq!(keys.len(), 18708);
  assert!(keys.iter().all(|key| key.chars().count() == 1));
}

#[test]
fn icon_noise_is_removed_without_touching_real_text() {
  assert_eq!(clean("画", 1.0), None);
  assert_eq!(clean("十", 1.0), None);
  assert_eq!(clean("\u{E0A1}", 1.0), None);
  assert_eq!(clean("Q", 0.5), None);
  assert_eq!(clean("  ", 1.0), None);
  assert_eq!(clean("7", 0.99).as_deref(), Some("7"));
  assert_eq!(clean("》 Flinch", 0.9).as_deref(), Some("Flinch"));
  assert_eq!(clean("凶Import JSON", 0.9).as_deref(), Some("Import JSON"));
  assert_eq!(clean("À propos", 0.9).as_deref(), Some("À propos"));
  assert_eq!(clean("Überblick", 0.9).as_deref(), Some("Überblick"));
  assert_eq!(clean("中文 text", 0.9).as_deref(), Some("中文 text"));
  assert_eq!(clean("文字", 0.9).as_deref(), Some("文字"));
}

#[test]
fn rows_read_left_to_right_then_top_to_bottom() {
  let lines = vec![
    level(200.0, 42.0, 260.0, 60.0, "fourth"),
    level(100.0, 10.0, 160.0, 30.0, "second"),
    level(10.0, 12.0, 80.0, 32.0, "first"),
    level(10.0, 40.0, 80.0, 60.0, "third"),
  ];
  assert_eq!(reading_order(lines), "first second\nthird fourth");
  assert_eq!(reading_order(Vec::new()), "");
}

#[test]
fn a_word_below_a_slanted_line_starts_its_own_row() {
  // The axis-aligned boxes of the first two overlap, but "au" sits below "PAIN"'s slope.
  let lines = vec![
    tilted((470.0, 1290.0), 10.0, 300.0, 70.0, "LIFE IS"),
    tilted((525.0, 1360.0), 10.0, 190.0, 70.0, "PAIN"),
    level(305.0, 1375.0, 380.0, 1415.0, "au"),
    tilted((460.0, 1480.0), 3.0, 300.0, 80.0, "Chocolat"),
  ];
  assert_eq!(reading_order(lines), "LIFE IS\nPAIN\nau\nChocolat");
}

#[test]
fn words_along_a_slanted_row_stay_together_in_order() {
  let lines = vec![
    tilted((300.0, 153.0), 10.0, 100.0, 30.0, "right"),
    tilted((100.0, 118.0), 10.0, 100.0, 30.0, "left"),
    tilted((100.0, 178.0), 10.0, 100.0, 30.0, "below"),
  ];
  assert_eq!(reading_order(lines), "left right\nbelow");
}

#[test]
fn redaction_finds_every_detected_line_without_reading_it() {
  let image = fixture();
  let (width, height) = image.dimensions();
  let boxes = detect_lines(image.as_raw(), width, height, &limits()).expect("lines");
  let model = Model::load_static_slice(DETECT_MODEL).expect("model");
  let rgb = rgb_image(image.as_raw(), width, height).expect("image");
  let regions = detect(&model, &rgb).expect("regions");
  assert_eq!(boxes.len(), regions.len(), "one box per detected line");
  assert!(boxes.len() >= 10, "{boxes:?}");
  for (bounds, region) in boxes.iter().zip(&regions) {
    assert!(bounds.left >= 0 && bounds.top >= 0);
    assert!(bounds.right <= width as i32 && bounds.bottom <= height as i32);
    let (x, y) = region.center;
    assert!(
      (bounds.left as f32..=bounds.right as f32).contains(&x)
        && (bounds.top as f32..=bounds.bottom as f32).contains(&y),
      "{bounds:?} misses {region:?}"
    );
  }
  let crop = imageops::crop_imm(&image, 200, 95, 200, 30).to_image();
  let small = detect_lines(crop.as_raw(), crop.width(), crop.height(), &limits()).expect("lines");
  assert!(
    !small.is_empty(),
    "the enlarged small selection still finds its label"
  );
  let blank = vec![255; 200 * 100 * 4];
  assert_eq!(
    detect_lines(&blank, 200, 100, &limits()).expect("lines"),
    []
  );
  let cancelled = limits();
  cancelled.cancel.store(true, Ordering::Release);
  assert!(matches!(
    detect_lines(image.as_raw(), width, height, &cancelled),
    Err(OcrError::Cancelled)
  ));
}

#[test]
fn tilted_lines_get_the_upright_box_around_their_corners() {
  let level = level(10.0, 20.0, 110.0, 40.0, "").region;
  assert_eq!(
    upright(&level, 500, 500),
    Some(TextBox {
      left: 10,
      top: 20,
      right: 110,
      bottom: 40
    })
  );
  // 100×20 turned 30°: half extents 50cos+10sin = 48.3 across, 50sin+10cos = 33.7 down.
  let turned = tilted((200.0, 200.0), 30.0, 100.0, 20.0, "").region;
  assert_eq!(
    upright(&turned, 500, 500),
    Some(TextBox {
      left: 151,
      top: 166,
      right: 249,
      bottom: 234
    })
  );
  // Clamped to the image; a line entirely outside it has no box.
  let edge = tilted((5.0, 5.0), 0.0, 40.0, 10.0, "").region;
  assert_eq!(
    upright(&edge, 500, 500),
    Some(TextBox {
      left: 0,
      top: 0,
      right: 25,
      bottom: 10
    })
  );
  let outside = tilted((-50.0, 5.0), 0.0, 40.0, 10.0, "").region;
  assert_eq!(upright(&outside, 500, 500), None);
}

#[test]
#[ignore = "timing measurement"]
fn detection_stage_timing() {
  let image = fixture();
  let mut screen = image::RgbaImage::new(2560, 1440);
  for y in (0..1440).step_by(image.height() as usize) {
    for x in (0..2560).step_by(image.width() as usize) {
      imageops::replace(&mut screen, &image, i64::from(x), i64::from(y));
    }
  }
  let t = Instant::now();
  let rgb = rgb_image(screen.as_raw(), 2560, 1440).expect("rgb");
  println!("rgb {:?}", t.elapsed());
  let t = Instant::now();
  let model = Model::load_static_slice(DETECT_MODEL).expect("model");
  println!("load {:?}", t.elapsed());
  let (w, h) = detection_size(2560, 1440);
  let t = Instant::now();
  let resized = imageops::resize(&rgb, w, h, imageops::FilterType::Triangle);
  println!("resize {w}x{h} {:?}", t.elapsed());
  let (wu, hu) = (w as usize, h as usize);
  let t = Instant::now();
  let mut data = vec![0f32; 3 * wu * hu];
  for (x, y, pixel) in resized.enumerate_pixels() {
    let bgr = [pixel[2], pixel[1], pixel[0]];
    for channel in 0..3 {
      data[channel * wu * hu + y as usize * wu + x as usize] =
        (f32::from(bgr[channel]) / 255.0 - DETECT_MEAN[channel]) / DETECT_STD[channel];
    }
  }
  println!("normalize {:?}", t.elapsed());
  let t = Instant::now();
  let (shape, map) = run(&model, [1, 3, hu, wu], data).expect("run");
  println!("model {:?}", t.elapsed());
  let t = Instant::now();
  let found = regions(&map, shape[3], shape[2], 2560, 1440);
  println!("regions {} {:?}", found.len(), t.elapsed());
}
