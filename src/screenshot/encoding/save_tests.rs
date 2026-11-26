use super::*;

#[test]
fn rejected_screenshot_save_preserves_existing_destination() {
  let dir = tempfile::tempdir().unwrap();
  for (format, width, height, pixels) in [
    ("unsupported", 1, 1, vec![1, 2, 3, 4]),
    ("png", 2, 2, vec![1, 2, 3, 4]),
    ("jpeg", 2, 2, vec![1, 2, 3, 4]),
    ("webp", 2, 2, vec![1, 2, 3, 4]),
  ] {
    let path = dir.path().join("existing.png");
    std::fs::write(&path, b"existing capture").unwrap();
    assert!(save_rgba_image_optimized(&path, width, height, &pixels, format, 90).is_err());
    assert_eq!(
      std::fs::read(&path).unwrap(),
      b"existing capture",
      "failed {format} save changed an existing file"
    );
    assert_eq!(
      std::fs::read_dir(dir.path()).unwrap().count(),
      1,
      "failed save leaked temporary artifacts"
    );
  }
}

#[test]
fn invalid_capture_never_publishes_an_empty_or_partial_image() {
  let dir = tempfile::tempdir().unwrap();
  for format in ["png", "jpeg", "webp", "unknown"] {
    let path = dir.path().join(format!("bad.{format}"));
    assert!(save_rgba_image_optimized(&path, 2, 2, &[0; 3], format, 90).is_err());
    assert!(
      !path.exists(),
      "failed {format} save published a corrupt image"
    );
  }
}

#[test]
fn successful_encoding_is_decodable_and_cannot_overwrite_a_destination_that_already_exists() {
  let dir = tempfile::tempdir().unwrap();
  let rgba = [10u8, 20, 30, 255].repeat(12);
  for (format, quality) in [
    ("png", 1),
    ("PNG", 100),
    ("jpg", 0),
    ("jpeg", 100),
    ("webp", 75),
    ("webp", 100),
  ] {
    let path = dir.path().join(format!("{format}-{quality}.img"));
    save_rgba_image_optimized(&path, 4, 3, &rgba, format, quality).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (4, 3));
    if format.eq_ignore_ascii_case("png") || (format == "webp" && quality == 100) {
      assert_eq!(decoded.to_rgba8().as_raw(), &rgba);
    }
    assert!(save_rgba_image_optimized(&path, 4, 3, &[90; 48], format, quality).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
  }
  assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 6);
}

#[test]
fn crop_rejects_truncated_rows_zero_sizes_out_of_bounds_and_integer_overflows() {
  for (source_width, x, y, w, h, data) in [
    (0, 0, 0, 1, 1, vec![]),
    (2, 0, 0, 1, 1, vec![0; 15]),
    (2, 0, 0, 0, 1, vec![0; 16]),
    (2, 0, 0, 1, 0, vec![0; 16]),
    (2, 2, 0, 1, 1, vec![0; 16]),
    (2, 0, 2, 1, 1, vec![0; 16]),
    (2, u32::MAX, 0, 2, 1, vec![0; 16]),
    (2, 0, u32::MAX, 1, 2, vec![0; 16]),
  ] {
    assert!(crop_rgba_direct(&data, source_width, x, y, w, h).is_err());
  }
  for format in ["png", "jpeg", "webp"] {
    for (w, h) in [(0, 0), (u32::MAX, u32::MAX)] {
      let dir = tempfile::tempdir().unwrap();
      let path = dir.path().join(format!("invalid.{format}"));
      assert!(save_rgba_image_optimized(&path, w, h, &[], format, 90).is_err());
      assert!(!path.exists());
    }
  }
}

#[test]
fn small_png_screenshots_are_saved_with_stronger_compression() {
  // A window-sized, text-like capture well under one megapixel, at the default quality 90.
  let (width, height) = (600u32, 400u32);
  let mut rgba = Vec::with_capacity((width * height * 4) as usize);
  for y in 0..height {
    for x in 0..width {
      let ink = (x * 7 + y * 13) % 11 < 3 && y % 18 < 12;
      let shade = if ink {
        25
      } else {
        240 - ((x / 40) % 3) as u8 * 6
      };
      rgba.extend_from_slice(&[shade, shade, shade.saturating_add(8), 255]);
    }
  }
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("small.png");
  save_rgba_image_optimized(&path, width, height, &rgba, "png", 90).unwrap();
  let saved = std::fs::read(&path).unwrap();
  let encoded = |compression| {
    let mut out = Vec::new();
    encode_png(&rgba, width, height, &mut out, compression).unwrap();
    out
  };
  assert_eq!(saved, encoded(PngCompression::Default));
  let fast = encoded(PngCompression::Fast);
  assert!(
    saved.len() < fast.len(),
    "{} vs {}",
    saved.len(),
    fast.len()
  );
  assert_eq!(
    image::load_from_memory(&saved).unwrap().to_rgba8().as_raw(),
    &rgba
  );
}

/// Opaque pseudo-random pixels from a fixed seed: incompressible for PNG, so a 1024×1024 image
/// is about 3 MB as PNG, well over `LARGE_PNG_BYTES`.
fn noisy(width: u32, height: u32) -> Vec<u8> {
  let mut state = 0x2545_f491_4f6c_dd1du64;
  let mut rgba = Vec::with_capacity((width * height * 4) as usize);
  for y in 0..height {
    for x in 0..width {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      let noise = (state >> 56) as u8;
      let pixel = [
        ((x / 4) as u8).wrapping_add(noise / 8),
        (y / 4) as u8,
        noise / 2,
      ];
      rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
  }
  rgba
}

fn jpg_beside(png: &Path) -> PathBuf {
  png.with_extension("jpg")
}

#[test]
fn a_large_png_is_saved_as_a_smaller_jpg_beside_it_when_enabled() {
  let dir = tempfile::tempdir().unwrap();
  let png = dir.path().join("large.png");
  let rgba = noisy(1024, 1024);
  let mut encoded = Vec::new();
  encode_png(&rgba, 1024, 1024, &mut encoded, PngCompression::Fast).unwrap();
  assert!(encoded.len() > LARGE_PNG_BYTES, "{}", encoded.len());

  let written = save_screenshot(&png, 1024, 1024, &rgba, "png", 90, true, jpg_beside).unwrap();
  assert_eq!(written, dir.path().join("large.jpg"));
  assert!(!png.exists());
  let bytes = std::fs::read(&written).unwrap();
  assert!(bytes.starts_with(&[0xFF, 0xD8]) && bytes.len() < LARGE_PNG_BYTES);
  let decoded = image::load_from_memory(&bytes).unwrap();
  assert_eq!((decoded.width(), decoded.height()), (1024, 1024));
}

#[test]
fn large_pngs_stay_png_when_disabled_and_small_pngs_always_do() {
  let dir = tempfile::tempdir().unwrap();
  let rgba = noisy(1024, 1024);
  let large = dir.path().join("large.png");
  let written = save_screenshot(&large, 1024, 1024, &rgba, "png", 90, false, jpg_beside).unwrap();
  assert_eq!(written, large);
  assert_eq!(
    image::load_from_memory(&std::fs::read(&large).unwrap())
      .unwrap()
      .to_rgba8()
      .as_raw(),
    &rgba
  );

  let small_rgba = noisy(64, 64);
  let small = dir.path().join("small.png");
  let jpg_chosen = std::cell::Cell::new(false);
  let written = save_screenshot(&small, 64, 64, &small_rgba, "png", 90, true, |png| {
    jpg_chosen.set(true);
    jpg_beside(png)
  })
  .unwrap();
  assert_eq!(written, small);
  assert!(!jpg_chosen.get());
}

#[test]
fn other_formats_ignore_the_large_png_option() {
  let dir = tempfile::tempdir().unwrap();
  let rgba = noisy(1024, 1024);
  for (format, name) in [("webp", "shot.webp"), ("jpg", "shot.jpg")] {
    let path = dir.path().join(name);
    let written = save_screenshot(&path, 1024, 1024, &rgba, format, 90, true, |_| {
      unreachable!("only PNGs switch")
    })
    .unwrap();
    assert_eq!(written, path);
  }
}

#[test]
fn a_switched_jpg_never_replaces_an_existing_file() {
  let dir = tempfile::tempdir().unwrap();
  let png = dir.path().join("large.png");
  let taken = dir.path().join("large.jpg");
  std::fs::write(&taken, b"existing capture").unwrap();
  let rgba = noisy(1024, 1024);
  assert!(save_screenshot(&png, 1024, 1024, &rgba, "png", 90, true, jpg_beside).is_err());
  assert_eq!(std::fs::read(&taken).unwrap(), b"existing capture");
  assert!(!png.exists());
}

#[test]
fn a_large_but_simple_screenshot_stays_png_and_leaves_no_jpg() {
  // 2560×1440 starts the speculative JPG (its RGB bytes exceed the limit), but flat UI
  // compresses far below it, so the JPG is cancelled and only the PNG is written.
  let (width, height) = (2560u32, 1440u32);
  assert!(png_may_exceed_limit(width, height));
  let mut rgba = Vec::with_capacity((width * height * 4) as usize);
  for y in 0..height {
    for x in 0..width {
      let panel: u8 = if (x / 320 + y / 180) % 2 == 0 {
        32
      } else {
        236
      };
      rgba.extend_from_slice(&[panel, panel, panel.saturating_add(10), 255]);
    }
  }
  let dir = tempfile::tempdir().unwrap();
  let png = dir.path().join("desktop.png");
  let jpg_chosen = std::cell::Cell::new(false);
  let written = save_screenshot(&png, width, height, &rgba, "png", 90, true, |png| {
    jpg_chosen.set(true);
    jpg_beside(png)
  })
  .unwrap();
  assert_eq!(written, png);
  assert!(!jpg_chosen.get());
  assert!(std::fs::metadata(&png).unwrap().len() < LARGE_PNG_BYTES as u64);
  assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn only_screenshots_whose_png_could_pass_the_limit_start_a_speculative_jpg() {
  // 2 MiB of RGB is about 0.7 MP: a 900×700 PNG can never reach the limit.
  assert!(!png_may_exceed_limit(900, 700));
  assert!(!png_may_exceed_limit(64, 64));
  assert!(png_may_exceed_limit(1024, 1024));
  assert!(png_may_exceed_limit(2560, 1440));
  assert!(png_may_exceed_limit(u32::MAX, u32::MAX));
}

#[test]
fn the_sampled_estimate_starts_the_jpg_for_detailed_shots_but_not_flat_ui() {
  assert!(png_likely_large(1024, 1024, &noisy(1024, 1024)));
  let (width, height) = (1600u32, 900u32);
  let mut flat = Vec::with_capacity((width * height * 4) as usize);
  for y in 0..height {
    for x in 0..width {
      let shade: u8 = if (x / 200 + y / 100) % 2 == 0 {
        40
      } else {
        220
      };
      flat.extend_from_slice(&[shade, shade, shade, 255]);
    }
  }
  assert!(png_may_exceed_limit(width, height));
  assert!(!png_likely_large(width, height, &flat));
  // A single row still yields an estimate rather than an empty encode.
  assert!(!png_likely_large(4, 1, &[0; 16]));
}

#[test]
fn a_translucent_screenshot_keeps_its_alpha_with_the_large_png_option_on() {
  let (width, height) = (64u32, 48u32);
  let mut rgba = Vec::with_capacity((width * height * 4) as usize);
  for i in 0..width * height {
    rgba.extend_from_slice(&[(i % 256) as u8, 40, 200, if i % 3 == 0 { 128 } else { 255 }]);
  }
  let dir = tempfile::tempdir().unwrap();
  let png = dir.path().join("translucent.png");
  let written = save_screenshot(&png, width, height, &rgba, "png", 90, true, jpg_beside).unwrap();
  assert_eq!(written, png);
  let decoded = image::open(&png).unwrap();
  assert_eq!(decoded.color(), image::ColorType::Rgba8);
  assert_eq!(decoded.to_rgba8().into_raw(), rgba);
}
