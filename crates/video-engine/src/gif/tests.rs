#![allow(clippy::expect_used)] // Test fixtures only; production remains deny.
use super::*;

fn nv12(width: usize, height: usize, y: u8, cb: u8, cr: u8) -> Vec<u8> {
  let mut plane = vec![y; width * height];
  for _ in 0..width * height / 4 {
    plane.extend_from_slice(&[cb, cr]);
  }
  plane
}

#[test]
fn bt709_limited_range_nv12_decodes_to_the_expected_rgb() {
  let mut rgb = Vec::new();
  for (y, cb, cr, expected) in [
    (16, 128, 128, [0, 0, 0]),
    (235, 128, 128, [255, 255, 255]),
    (126, 128, 128, [128, 128, 128]),
    // BT.709 limited-range primaries.
    (63, 102, 240, [255, 0, 0]),
    (173, 42, 26, [0, 255, 0]),
    (32, 240, 118, [0, 0, 255]),
  ] {
    nv12_to_rgb(&nv12(4, 2, y, cb, cr), 4, 2, 4, 2, &mut rgb).expect("decode");
    for (got, want) in rgb[..3].iter().zip(expected) {
      assert!(
        (i32::from(*got) - want).abs() <= 2,
        "{y},{cb},{cr}: {:?}",
        &rgb[..3]
      );
    }
    assert_eq!(rgb.len(), 4 * 2 * 3);
  }
  // Coded padding (stride and rows) is skipped; only the displayed picture is converted.
  let padded = nv12(8, 4, 235, 128, 128);
  nv12_to_rgb(&padded, 8, 4, 6, 2, &mut rgb).expect("padded");
  assert_eq!(rgb.len(), 6 * 2 * 3);
  assert!(nv12_to_rgb(&padded[..40], 8, 4, 6, 2, &mut rgb).is_err());
  assert!(nv12_to_rgb(&padded, 8, 4, 9, 2, &mut rgb).is_err());
  assert!(nv12_to_rgb(&padded, 8, 4, 5, 2, &mut rgb).is_err());
}

#[test]
fn few_colors_are_kept_exactly_and_many_are_trained_into_255() {
  let mut sampler = PaletteSampler::default();
  assert!(sampler.palette().is_err());
  let flat: Vec<u8> = [[10, 20, 30], [200, 100, 0], [10, 20, 30]]
    .into_iter()
    .flatten()
    .collect();
  sampler.add(&flat);
  let palette = sampler.palette().expect("palette");
  assert_eq!(palette.len(), 2);
  let (orange, navy) = (palette.nearest(200, 100, 0), palette.nearest(10, 20, 30));
  assert_eq!(
    (palette.color(orange), palette.color(navy)),
    ([200, 100, 0], [10, 20, 30])
  );

  let mut sampler = PaletteSampler::default();
  let gradient: Vec<u8> = (0..256 * 256)
    .flat_map(|i: u32| [(i % 256) as u8, (i / 256) as u8, ((i * 7) % 256) as u8])
    .collect();
  sampler.add(&gradient);
  let palette = sampler.palette().expect("trained");
  assert_eq!(palette.len(), COLORS);
  assert_eq!(palette.table().len(), 768);
}

#[test]
fn palette_sampling_stays_within_its_budget_across_long_recordings() {
  let frame: Vec<u8> = (0..1920 * 1080 * 3).map(|i| (i % 251) as u8).collect();
  let mut sampler = PaletteSampler::default();
  for _ in 0..80 {
    sampler.add(&frame);
    assert!(sampler.samples() <= SAMPLE_BUDGET);
  }
  // Every frame still contributes after the budget halves the per-frame share.
  assert!(
    sampler.samples() > SAMPLE_BUDGET / 4,
    "{}",
    sampler.samples()
  );
  assert!(Palette::new(Vec::new()).is_err());
  assert!(Palette::new(vec![[0, 0, 0]; 256]).is_err());
}

fn black_and_white() -> Palette {
  Palette::new(vec![[0, 0, 0], [255, 255, 255]]).expect("palette")
}
fn mean_white(indices: &[u8]) -> f64 {
  indices.iter().map(|&i| f64::from(i)).sum::<f64>() / indices.len() as f64
}

#[test]
fn every_dither_reproduces_mid_grey_from_black_and_white_and_none_does_not() {
  let (width, height) = (64, 32);
  let grey = vec![128u8; width * height * 3];
  let mut indices = Vec::new();
  for dither in [
    Dither::FloydSteinberg,
    Dither::Sierra24a,
    Dither::Bayer { scale: 0 },
  ] {
    let palette = black_and_white();
    quantize(&grey, width, height, &palette, dither, None, &mut indices).expect("dither");
    let mean = mean_white(&indices);
    assert!((0.4..0.6).contains(&mean), "{dither:?}: {mean}");
  }
  let palette = black_and_white();
  quantize(
    &grey,
    width,
    height,
    &palette,
    Dither::None,
    None,
    &mut indices,
  )
  .expect("none");
  assert!(mean_white(&indices) == 0.0 || mean_white(&indices) == 1.0);
  // Weaker Bayer patterns move less of the picture: dark grey only dithers at full strength.
  let dark = vec![100u8; width * height * 3];
  quantize(
    &dark,
    width,
    height,
    &palette,
    Dither::Bayer { scale: 0 },
    None,
    &mut indices,
  )
  .expect("strong");
  // Only the top four thresholds (offset >= 28) lift 100 past the black/white midpoint.
  assert!(
    (0.05..0.08).contains(&mean_white(&indices)),
    "{}",
    mean_white(&indices)
  );
  quantize(
    &dark,
    width,
    height,
    &palette,
    Dither::Bayer { scale: 5 },
    None,
    &mut indices,
  )
  .expect("weak");
  assert_eq!(mean_white(&indices), 0.0);
  assert!(
    quantize(
      &grey,
      width,
      height,
      &palette,
      Dither::Bayer { scale: 6 },
      None,
      &mut indices
    )
    .is_err()
  );
  assert!(
    quantize(
      &grey[3..],
      width,
      height,
      &palette,
      Dither::None,
      None,
      &mut indices
    )
    .is_err()
  );
}

#[test]
fn exact_palette_colors_survive_error_diffusion_unchanged() {
  let palette = Palette::new(vec![[0, 0, 0], [255, 0, 0], [0, 0, 255]]).expect("palette");
  let picture: Vec<u8> = (0..16 * 8)
    .flat_map(|i| if i % 3 == 0 { [255, 0, 0] } else { [0, 0, 255] })
    .collect();
  let mut indices = Vec::new();
  quantize(
    &picture,
    16,
    8,
    &palette,
    Dither::Sierra24a,
    None,
    &mut indices,
  )
  .expect("quantize");
  for (i, index) in indices.iter().enumerate() {
    assert_eq!(*index, if i % 3 == 0 { 1 } else { 2 });
  }
}

struct Decoded {
  frames: Vec<(u16, u16, u16, u16, u16, Option<u8>)>,
  canvas: Vec<u8>,
}
fn decode(bytes: &[u8], width: usize) -> Decoded {
  let mut options = gif::DecodeOptions::new();
  options.set_color_output(gif::ColorOutput::Indexed);
  let mut decoder = options.read_info(bytes).expect("gif header");
  assert_eq!(decoder.width() as usize, width);
  let mut canvas = vec![0u8; width * decoder.height() as usize];
  let mut frames = Vec::new();
  while let Some(frame) = decoder.read_next_frame().expect("gif frame") {
    frames.push((
      frame.delay,
      frame.left,
      frame.top,
      frame.width,
      frame.height,
      frame.transparent,
    ));
    for y in 0..frame.height as usize {
      for x in 0..frame.width as usize {
        let value = frame.buffer[y * frame.width as usize + x];
        if Some(value) != frame.transparent {
          canvas[(frame.top as usize + y) * width + frame.left as usize + x] = value;
        }
      }
    }
  }
  Decoded { frames, canvas }
}

#[test]
fn frames_are_written_as_changed_rectangles_timed_from_their_presentation() {
  let (width, height) = (8, 6);
  let palette = Palette::new(vec![[0, 0, 0], [255, 255, 255], [255, 0, 0]]).expect("palette");
  let mut writer = GifWriter::new(Vec::new(), width, height, &palette).expect("writer");
  let first = vec![0u8; width * height];
  let mut second = first.clone();
  second[2 * width + 3] = 1;
  second[3 * width + 5] = 2;
  let frame = 333_333; // 30 FPS in 100 ns units
  writer.push(&first, 0).expect("first");
  writer.push(&second, frame).expect("second");
  // A different picture within the minimum delay of the shown frame is not displayed.
  writer
    .push(&first, frame + 50_000)
    .expect("same centisecond");
  writer.push(&second, 2 * frame).expect("unchanged");
  let mut third = second.clone();
  third[0] = 1;
  writer.push(&third, 3 * frame).expect("third");
  let (bytes, report) = writer.finish(4 * frame).expect("finish");
  assert_eq!(
    (
      report.decoded_frames,
      report.written_frames,
      report.unchanged_frames,
      report.coalesced_frames
    ),
    (5, 3, 1, 1)
  );
  assert_eq!(report.palette_colors, 3);
  let decoded = decode(&bytes, width);
  // Delays: 0→3 cs, 3→9 cs (absorbing the unchanged frame), 9→14 cs; no drift from rounding.
  assert_eq!(
    decoded.frames,
    vec![
      (3, 0, 0, 8, 6, None),
      (6, 3, 2, 3, 2, Some(TRANSPARENT)),
      (5, 0, 0, 1, 1, Some(TRANSPARENT)),
    ]
  );
  assert_eq!(report.duration_cs, 14);
  assert_eq!(decoded.canvas, third);
}

#[test]
fn malformed_frame_sequences_fail_explicitly() {
  let palette = black_and_white();
  let writer = GifWriter::new(Vec::new(), 4, 4, &palette).expect("writer");
  assert!(writer.finish(0).is_err(), "no frames");
  let mut writer = GifWriter::new(Vec::new(), 4, 4, &palette).expect("writer");
  assert!(writer.push(&[0; 3], 0).is_err(), "wrong size");
  writer.push(&[0; 16], 200_000).expect("first");
  assert!(writer.push(&[1; 16], 100_000).is_err(), "time order");
  assert!(GifWriter::new(Vec::new(), 70_000, 4, &palette).is_err());
  assert!(GifWriter::new(Vec::new(), 0, 4, &palette).is_err());
}

#[test]
fn unchanged_source_pixels_keep_their_index_so_diffusion_noise_stays_where_the_picture_changed() {
  let (width, height) = (64, 32);
  let palette = black_and_white();
  let grey = vec![128u8; width * height * 3];
  let (mut first, mut second, mut unstable) = (Vec::new(), Vec::new(), Vec::new());
  quantize(
    &grey,
    width,
    height,
    &palette,
    Dither::Sierra24a,
    None,
    &mut first,
  )
  .expect("first");
  let mut changed = grey.clone();
  changed[..3].copy_from_slice(&[255, 255, 255]); // one pixel at the top-left corner
  quantize(
    &changed,
    width,
    height,
    &palette,
    Dither::Sierra24a,
    Some((&grey, &first)),
    &mut second,
  )
  .expect("stable");
  let differing = first.iter().zip(&second).filter(|(a, b)| a != b).count();
  assert!(differing <= 1, "{differing} pixels changed");
  // Without the previous frame the same one-pixel change re-dithers far more of the picture.
  quantize(
    &changed,
    width,
    height,
    &palette,
    Dither::Sierra24a,
    None,
    &mut unstable,
  )
  .expect("unstable");
  assert!(first.iter().zip(&unstable).filter(|(a, b)| a != b).count() > 10);
  let short = vec![0u8; 3];
  assert!(
    quantize(
      &grey,
      width,
      height,
      &palette,
      Dither::Sierra24a,
      Some((&short, &first)),
      &mut second
    )
    .is_err()
  );
}

#[test]
fn the_pruned_palette_search_is_the_exact_nearest_with_lowest_index_ties() {
  let mut seed = 0x9e37_79b9u32;
  let mut next = || {
    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    (seed >> 24) as u8
  };
  for size in [1usize, 2, 17, 255] {
    let mut colors: Vec<[u8; 3]> = (0..size).map(|_| [next(), next(), next()]).collect();
    // Duplicates and equal-red neighbours exercise ties and the search's stopping rule.
    if size > 2 {
      colors[size - 1] = colors[0];
      colors[size - 2] = [colors[1][0], colors[1][1].wrapping_add(1), colors[1][2]];
    }
    let palette = Palette::new(colors.clone()).expect("palette");
    for _ in 0..20_000 {
      let (r, g, b) = (next(), next(), next());
      let expected = colors
        .iter()
        .enumerate()
        .map(|(index, c)| {
          let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
          (d(r, c[0]) + d(g, c[1]) + d(b, c[2]), index as u8)
        })
        .min()
        .map(|(_, index)| index);
      // Twice: the computed answer and the cached one.
      for _ in 0..2 {
        assert_eq!(
          Some(palette.nearest(r, g, b)),
          expected,
          "{size} colors at {r},{g},{b}"
        );
      }
    }
  }
}

#[test]
fn sampling_a_decoded_picture_equals_sampling_its_full_rgb_conversion() {
  let (width, height) = (64, 32);
  let mut plane: Vec<u8> = (0..width * height).map(|i| (16 + i % 220) as u8).collect();
  plane.extend((0..width * height / 2).map(|i| (i * 7 % 256) as u8));
  let picture = Nv12::new(&plane, width, height, width, height).expect("picture");
  let mut rgb = Vec::new();
  picture.to_rgb(&mut rgb);
  let (mut from_rgb, mut from_nv12) = (PaletteSampler::default(), PaletteSampler::default());
  for _ in 0..40 {
    from_rgb.add(&rgb);
    from_nv12.add_nv12(&picture);
  }
  assert_eq!(from_rgb.rgba, from_nv12.rgba);
  assert_eq!(from_rgb.stride, from_nv12.stride);
  for index in [0, 1, width - 1, width, width * height - 1] {
    assert_eq!(picture.pixel(index)[..], rgb[index * 3..index * 3 + 3]);
  }
}
