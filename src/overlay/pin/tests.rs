use super::frame::*;

fn bounds(left: i32, top: i32, right: i32, bottom: i32) -> Bounds {
  Bounds {
    left,
    top,
    right,
    bottom,
  }
}

/// Regression: the window was resized on every zoom frame, and the taskbar kept jumping in front
/// of a pin over it. A zoom now resizes it once, and the image always fits.
#[test]
fn a_zoom_resizes_the_window_at_most_once_and_the_image_always_fits() {
  use super::motion::{View, ZOOM_TIME};
  use std::time::{Duration, Instant};
  let window = |zoom: f32| {
    let (w, h) = scaled_size(1000, 600, zoom);
    (w as i32 + 24, h as i32 + 24)
  };
  for (from, to) in [(1.0, 2.0), (2.0, 1.0), (0.5, 0.25), (0.25, 4.0)] {
    let start = Instant::now();
    let mut view = View::settled(from, (100.0, 100.0), start);
    view.zoom_to(to, (400.0, 300.0), start, ZOOM_TIME);
    let mut placed = Some(window(from));
    let mut resizes = 0;
    for frame in 0..=40 {
      let now = start + Duration::from_millis(frame * 7);
      let (_, zoom) = view.at(now);
      let size = window_size(window(zoom), window(to), placed, view.zooming(now));
      assert!(size.0 >= window(zoom).0 && size.1 >= window(zoom).1);
      if placed != Some(size) {
        resizes += 1;
      }
      placed = Some(size);
    }
    assert_eq!(resizes, 1, "zooming from {from} to {to}");
    assert_eq!(placed, Some(window(to)), "settled from {from} to {to}");
  }
}

#[test]
fn zoom_steps_stop_at_the_ends_and_never_outgrow_the_window_budget() {
  assert_eq!(ZOOMS[ORIGINAL], 1.0);
  assert_eq!(zoom_after(ORIGINAL, 1, 800, 600, 12), ORIGINAL + 1);
  assert_eq!(zoom_after(ORIGINAL, -2, 800, 600, 12), ORIGINAL - 2);
  assert_eq!(zoom_after(0, -5, 800, 600, 12), 0);
  assert_eq!(
    zoom_after(ZOOMS.len() - 1, 3, 800, 600, 12),
    ZOOMS.len() - 1
  );
  // A 4K screenshot can grow to 1.5× (5760×3240 ≈ 18.7 MP) but 2× would pass 64 MP? No: 2× is
  // 7680×4320 ≈ 33 MP and fits; 3× (11520×6480 ≈ 74.6 MP) does not.
  let three = ZOOMS.iter().position(|zoom| *zoom == 3.0).expect("3×");
  assert_eq!(zoom_after(ORIGINAL, 10, 3840, 2160, 12), three - 1);
  // An image that cannot even show 1:1 opens at the largest step that fits.
  assert_eq!(opening_zoom(800, 600, 12), ORIGINAL);
  let huge = opening_zoom(12_000, 8_000, 12);
  assert!(huge < ORIGINAL);
  let (width, height) = scaled_size(12_000, 8_000, ZOOMS[huge]);
  assert!(width <= 16_384 && u64::from(width) * u64::from(height) <= 64_000_000);
  assert_eq!(opacity_after(OPACITIES.len() - 1, 1), OPACITIES.len() - 1);
  assert_eq!(opacity_after(OPACITIES.len() - 1, -2), OPACITIES.len() - 3);
  assert_eq!(opacity_after(0, -1), 0);
  assert_eq!(scaled_size(3, 3, 0.25), (1, 1), "never zero pixels");
}

#[test]
fn the_shadow_grows_with_the_monitor_scaling() {
  assert_eq!(margin(96), 12);
  assert_eq!(margin(144), 18);
  assert_eq!(margin(10_000), 60, "DPI is clamped");
}

#[test]
fn enlarging_repeats_pixels_and_shrinking_averages_blocks() {
  let red = [255, 0, 0, 255];
  let blue = [0, 0, 255, 255];
  let two: Vec<u8> = [red, blue].concat();
  assert_eq!(scale(&two, 2, 1, 2, 1), two, "1:1 is a copy");
  assert_eq!(scale(&two, 2, 1, 4, 1), [red, red, blue, blue].concat());
  assert_eq!(scale(&two, 2, 1, 1, 1), [128, 0, 128, 255]);
  // A 4×2 checkerboard halved: every block holds one black and one white pixel per row.
  let (black, white) = ([0, 0, 0, 255], [255, 255, 255, 255]);
  let board: Vec<u8> = [black, white, black, white, white, black, white, black].concat();
  assert_eq!(
    scale(&board, 4, 2, 2, 1),
    [128, 128, 128, 255, 128, 128, 128, 255]
  );
}

#[test]
fn the_frame_holds_the_image_an_outline_and_a_shadow_that_fades_out() {
  let (width, height, margin) = (4u32, 3u32, 6i32);
  let image: Vec<u8> = (0..width * height)
    .flat_map(|index| [index as u8 * 10, 200, 50, 255])
    .collect();
  let stride = width as i32 + 2 * margin;
  let mut pixels = vec![0u8; (stride * (height as i32 + 2 * margin) * 4) as usize];
  compose(&mut pixels, &image, width, height, margin);
  let at = |x: i32, y: i32| {
    let offset = ((y * stride + x) * 4) as usize;
    [
      pixels[offset],
      pixels[offset + 1],
      pixels[offset + 2],
      pixels[offset + 3],
    ]
  };
  // The image keeps its exact pixels, as BGRA.
  assert_eq!(at(margin, margin), [50, 200, 0, 255]);
  assert_eq!(at(margin + 3, margin + 2), [50, 200, 110, 255]);
  // A gray ring right outside it, a shadow beyond, and nothing at the window's corner.
  let ring = at(margin - 1, margin + 1);
  assert_eq!(
    (ring[0], ring[0] == ring[1] && ring[1] == ring[2]),
    (ring[0], true)
  );
  assert!(ring[3] >= 80);
  let near = at(margin - 2, margin + 1)[3];
  let far = at(margin - 5, margin + 1)[3];
  assert!(near > far, "{near} > {far}");
  assert_eq!(at(0, 0)[3], 0);
  // The shadow sits slightly lower: the bottom edge's is darker than the top edge's.
  let top = at(margin + 1, margin - 3)[3];
  let bottom = at(margin + 1, margin + height as i32 + 2)[3];
  assert!(bottom > top, "{bottom} > {top}");
  // Premultiplied: a shadow pixel's color never exceeds its alpha.
  for y in 0..height as i32 + 2 * margin {
    for x in 0..stride {
      let [b, g, r, a] = at(x, y);
      assert!(b <= a && g <= a && r <= a, "({x},{y})");
    }
  }
}

#[test]
fn transparent_image_pixels_are_premultiplied() {
  let image = [200, 100, 50, 128];
  let mut pixels = vec![0u8; 3 * 3 * 4];
  compose(&mut pixels, &image, 1, 1, 1);
  assert_eq!(&pixels[16..20], [25, 50, 100, 128]);
}

#[test]
fn a_pin_left_without_a_monitor_moves_fully_onto_the_nearest_one() {
  let primary = bounds(0, 0, 1920, 1080);
  let left = bounds(-2560, 0, 0, 1440);
  // Still partly on a monitor: it stays.
  assert_eq!(rehome(bounds(-100, 100, 300, 400), &[primary]), None);
  // Its monitor (left) was unplugged: it lands inside the primary, at the edge nearest it.
  assert_eq!(
    rehome(bounds(-900, 200, -500, 500), &[primary]),
    Some((0, 200))
  );
  // Below everything: it rises into the nearest monitor, measured from its center. The taller
  // left monitor (bottom 1440) is nearer than the primary (bottom 1080).
  assert_eq!(
    rehome(bounds(100, 2000, 500, 2300), &[primary]),
    Some((100, 780))
  );
  assert_eq!(
    rehome(bounds(100, 2000, 500, 2300), &[primary, left]),
    Some((-400, 1140))
  );
  // Larger than the monitor: its top-left corner is kept on screen.
  assert_eq!(
    rehome(bounds(3000, 0, 6000, 2000), &[primary]),
    Some((0, 0))
  );
  assert_eq!(
    rehome(bounds(3000, 0, 3100, 100), &[]),
    None,
    "no monitors known: leave it"
  );
}

#[test]
fn a_pin_from_a_file_opens_centered_and_inside_its_monitor() {
  let monitor = bounds(1920, 0, 3840, 1040);
  assert_eq!(centered(400, 300, monitor), (2680, 370));
  assert_eq!(
    centered(4000, 3000, monitor),
    (1920, 0),
    "too large: its corner stays visible"
  );
}

/// The composed pin over a light and a dark background, for review: `--ignored`, with
/// `FLINCH_PIN_PREVIEW_OUTPUT` set to a folder.
#[test]
#[ignore = "writes review images"]
fn render_pin_previews() {
  let output =
    std::path::PathBuf::from(std::env::var("FLINCH_PIN_PREVIEW_OUTPUT").expect("output"));
  std::fs::create_dir_all(&output).expect("folder");
  let source =
    image::load_from_memory(include_bytes!("../../../tests/fixtures/ocr/en-hotkeys.png"))
      .expect("fixture")
      .to_rgba8();
  let crop = image::imageops::crop_imm(&source, 0, 60, 420, 260).to_image();
  for (name, background, zoom) in [
    ("dark", [32u8, 33, 38], 1.0f32),
    ("light", [243, 243, 243], 1.0),
    ("zoom-50", [243, 243, 243], 0.5),
    ("zoom-200", [32, 33, 38], 2.0),
  ] {
    let margin = margin(96);
    let (width, height) = scaled_size(crop.width(), crop.height(), zoom);
    let scaled = scale(crop.as_raw(), crop.width(), crop.height(), width, height);
    let (frame_width, frame_height) = (width + 2 * margin as u32, height + 2 * margin as u32);
    let mut pixels = vec![0u8; (frame_width * frame_height * 4) as usize];
    compose(&mut pixels, &scaled, width, height, margin);
    let mut canvas =
      image::RgbImage::from_pixel(frame_width + 80, frame_height + 80, image::Rgb(background));
    for y in 0..frame_height {
      for x in 0..frame_width {
        let offset = ((y * frame_width + x) * 4) as usize;
        let [b, g, r, a] = [
          pixels[offset],
          pixels[offset + 1],
          pixels[offset + 2],
          pixels[offset + 3],
        ];
        let under = canvas.get_pixel(x + 40, y + 40).0;
        let blend = |color: u8, base: u8| {
          (u16::from(color) + u16::from(base) * (255 - u16::from(a)) / 255).min(255) as u8
        };
        canvas.put_pixel(
          x + 40,
          y + 40,
          image::Rgb([blend(r, under[0]), blend(g, under[1]), blend(b, under[2])]),
        );
      }
    }
    canvas
      .save(output.join(format!("pin-{name}.png")))
      .expect("save");
  }
}

/// The CPU pin's costs on a 4K screenshot: `--ignored --nocapture`. Kept for the record of the
/// GPU rework's before/after comparison.
#[test]
#[ignore = "timing"]
fn cpu_pin_timing() {
  use windows::Win32::UI::WindowsAndMessaging::*;
  use windows::core::w;
  let (width, height) = (3840u32, 2160u32);
  let rgba: Vec<u8> = (0..width * height)
    .flat_map(|index| [(index % 251) as u8, (index % 199) as u8, 90, 255])
    .collect();
  let margin = margin(96);
  // SAFETY: a hidden layered window owned by this test thread; never shown.
  let hwnd = unsafe {
    CreateWindowExW(
      WS_EX_LAYERED | WS_EX_TOOLWINDOW,
      w!("STATIC"),
      None,
      WS_POPUP,
      0,
      0,
      1,
      1,
      None,
      None,
      None,
      None,
    )
  }
  .expect("window");
  let render = |zoom: f32| {
    let (w, h) = scaled_size(width, height, zoom);
    let scaled = scale(&rgba, width, height, w, h);
    let mut bitmap = crate::overlay::gdi::layered::LayeredBitmap::new(
      w as i32 + 2 * margin,
      h as i32 + 2 * margin,
    )
    .expect("bitmap");
    compose(bitmap.pixels_mut(), &scaled, w, h, margin);
    bitmap
  };
  let mut best = [f64::MAX; 3];
  for _ in 0..5 {
    let start = std::time::Instant::now();
    let bitmap = render(1.25);
    bitmap.apply_with_opacity(hwnd, 0, 0, 255).expect("apply");
    best[0] = best[0].min(start.elapsed().as_secs_f64() * 1000.0);
    let start = std::time::Instant::now();
    bitmap.apply_with_opacity(hwnd, 0, 0, 200).expect("apply");
    best[1] = best[1].min(start.elapsed().as_secs_f64() * 1000.0);
    let start = std::time::Instant::now();
    // SAFETY: this test's own hidden window.
    unsafe {
      SetWindowPos(
        hwnd,
        None,
        10,
        10,
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
      )
    }
    .expect("move");
    best[2] = best[2].min(start.elapsed().as_secs_f64() * 1000.0);
  }
  println!(
    "4K CPU pin, best of 5: zoom notch {:.1} ms, opacity notch {:.1} ms, drag step {:.2} ms",
    best[0], best[1], best[2]
  );
  // SAFETY: the window created above.
  let _ = unsafe { DestroyWindow(hwnd) };
}

/// Draws the eight slices like the compositor does: each source piece stretched over its target.
fn assemble(margin: i32, width: i32, height: i32) -> Vec<u8> {
  let template = template(margin);
  let (template_width, _) = template_size(margin);
  let stride = width + 2 * margin;
  let mut pixels = vec![0u8; (stride * (height + 2 * margin) * 4) as usize];
  for piece in slices(margin, width, height) {
    let (source, target) = (piece.source, piece.target);
    for y in 0..target.height {
      for x in 0..target.width {
        let sx = source.x + x * source.width / target.width;
        let sy = source.y + y * source.height / target.height;
        let from = ((sy * template_width as i32 + sx) * 4) as usize;
        let to = (((target.y + y) * stride + target.x + x) * 4) as usize;
        pixels[to..to + 4].copy_from_slice(&template[from..from + 4]);
      }
    }
  }
  pixels
}

#[test]
fn the_sliced_frame_matches_the_composed_one_pixel_for_pixel() {
  for margin in [12, 18, 24] {
    let drop = shadow_drop(margin);
    for (width, height) in [(1, drop), (1, drop + 1), (5, 7), (37, 20), (300, 2)] {
      if height < drop {
        continue;
      }
      let image = vec![0u8; (width * height * 4) as usize];
      let stride = width + 2 * margin;
      let mut composed = vec![0u8; (stride * (height + 2 * margin) * 4) as usize];
      compose(&mut composed, &image, width as u32, height as u32, margin);
      assert_eq!(
        assemble(margin, width, height),
        composed,
        "margin {margin}, {width}×{height}"
      );
    }
  }
  // Slices never overlap or leave the window, even for an image thinner than the shadow's drop.
  let pieces = slices(12, 1, 1);
  for piece in pieces {
    assert!(piece.target.width >= 0 && piece.target.height >= 0);
    assert!(piece.target.x + piece.target.width <= 25);
    assert!(piece.target.y + piece.target.height <= 25);
  }
}

#[test]
fn the_zoom_label_reads_in_every_language_and_scale() {
  for lang in ["en", "fr", "de", "es", "it"] {
    let copy = crate::i18n::native_settings(lang, "pin_percent").expect("copy");
    assert!(
      copy.contains("{value}") && copy.contains('%'),
      "{lang}: {copy}"
    );
    for scale in [1.0f32, 1.5, 2.0] {
      let short = copy.replace("{value}", "25");
      let long = copy.replace("{value}", "400");
      let (pixels, width, height) = super::render_label(&short, scale).expect("label");
      let (_, long_width, _) = super::render_label(&long, scale).expect("label");
      assert_eq!(pixels.len(), (width * height * 4) as usize);
      assert_eq!(height, (36.0 * scale).round() as u32, "{lang} {scale}");
      assert!(long_width > width, "{lang} {scale}: wider for more digits");
      // Opaque panel with bright text inside it.
      let center = ((height / 2 * width + width / 2) * 4) as usize;
      assert!(
        pixels[center + 3] > 200,
        "{lang} {scale}: the panel is opaque"
      );
      assert!(
        pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[1] > 200),
        "{lang} {scale}: no text drawn"
      );
    }
  }
}

/// Regression: a pin larger than the screen covered the monitor, Windows took it for a
/// full-screen app, and the taskbar kept jumping in front of it while it zoomed. Its window now
/// stops a pixel short of any monitor, and stays put while the pin moves past the screen edges.
#[test]
fn a_huge_pin_never_covers_a_whole_monitor_and_its_window_stays_put() {
  let one = [bounds(0, 0, 1920, 1080)];
  let huge = bounds(-500, -400, 3000, 2000);
  assert_eq!(shown(huge, &one), bounds(0, 0, 1920, 1079));
  assert_eq!(
    shown(bounds(-499, -398, 3001, 2002), &one),
    shown(huge, &one)
  );
  // Inside or partly off the screen it is left alone or only clipped.
  assert_eq!(
    shown(bounds(10, 10, 500, 400), &one),
    bounds(10, 10, 500, 400)
  );
  assert_eq!(
    shown(bounds(1800, 10, 2500, 400), &one),
    bounds(1800, 10, 1920, 400)
  );
  // Exactly the monitor's size is still trimmed.
  assert_eq!(
    shown(bounds(0, 0, 1920, 1080), &one),
    bounds(0, 0, 1920, 1079)
  );
  // Off every screen it stays whole, for `rehome` to bring back.
  assert_eq!(
    shown(bounds(3000, 0, 3500, 400), &one),
    bounds(3000, 0, 3500, 400)
  );

  // Side by side, the trimmed edge never cuts into the neighbor.
  let two = [bounds(0, 0, 1920, 1080), bounds(1920, 0, 3840, 1080)];
  assert_eq!(
    shown(bounds(-50, -50, 2500, 1200), &two),
    bounds(0, 0, 2500, 1079)
  );
  // Stacked, the left monitor's bottom is shared, so its free top is trimmed instead.
  let stacked = [bounds(0, 0, 1920, 1080), bounds(0, 1080, 1920, 2160)];
  assert_eq!(
    shown(bounds(-50, -50, 2000, 1500), &stacked),
    bounds(0, 1, 1920, 1500)
  );
  for screens in [&one[..], &two[..], &stacked[..]] {
    let window = shown(bounds(-9000, -9000, 9000, 9000), screens);
    for monitor in screens {
      let covers = window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom;
      assert!(!covers, "{window:?} covers {monitor:?}");
    }
  }
  assert_eq!(shown(huge, &[]), huge);
}
