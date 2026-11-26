use super::super::rotate::fixtures::*;
use super::*;

/// The sRGB decode Windows applies to SDR content before scaling it into an HDR desktop.
fn decode(level: u8) -> f32 {
  let c = f32::from(level) / 255.0;
  if c <= 0.040_45 {
    c / 12.92
  } else {
    ((c + 0.055) / 1.055).powf(2.4)
  }
}
/// scRGB value Windows composes for an 8-bit SDR level at `nits` SDR white.
fn composed(level: u8, nits: f32) -> f32 {
  decode(level) * nits / 80.0
}
fn level(encoded: f32) -> u8 {
  (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[test]
fn the_shader_turns_each_rotation_upright_like_the_reference_mapping() {
  let _com = super::super::Apartment::new().expect("COM");
  // Odd extents that are not multiples of the 8x8 thread group catch edge mistakes.
  let (width, height) = (37u32, 71u32);
  let gpu = warp_gpu(width, height);
  for orientation in [
    Orientation::Rotate90,
    Orientation::Rotate180,
    Orientation::Rotate270,
  ] {
    let pass =
      FramePass::new(&gpu.device, width, height, orientation, Tone::Sdr).expect("frame pass");
    assert_eq!(pass.input_format(), DXGI_FORMAT_B8G8R8A8_UNORM);
    let (panel_width, panel_height) = pass.panel_extent();
    let frame = numbered(&gpu, panel_width, panel_height);
    // SAFETY: the frame is a same-device BGRA texture at the panel extent.
    let upright = unsafe { pass.apply(&gpu.context, &frame) };
    let image = pixels(&gpu, upright, width, height);
    for y in 0..height {
      for x in 0..width {
        let (px, py) = orientation.panel_texel(width, height, x, y);
        assert_eq!(
          image[(y * width + x) as usize],
          coordinate(px, py),
          "{orientation:?} desktop ({x}, {y})"
        );
      }
    }
  }
}

#[test]
fn sdr_content_on_an_hdr_desktop_records_as_the_pixels_the_application_drew() {
  let _com = super::super::Apartment::new().expect("COM");
  let (width, height) = (256u32, 2u32);
  let gpu = warp_gpu(width, height);
  // Windows' minimum, a common setting and its maximum SDR content brightness.
  for nits in [80.0f32, 240.0, 480.0] {
    let colour = |v: u32| [v as u8, (255 - v) as u8, ((v * 37) % 256) as u8];
    let mut texels = Vec::new();
    for v in 0..width {
      let n = composed(v as u8, nits);
      texels.push([n, n, n]);
    }
    for v in 0..width {
      let [r, g, b] = colour(v);
      texels.push([composed(r, nits), composed(g, nits), composed(b, nits)]);
    }
    let frame = scrgb(&gpu, width, height, &texels);
    let pass = FramePass::new(
      &gpu.device,
      width,
      height,
      Orientation::Upright,
      Tone::Hdr {
        sdr_white_nits: nits,
      },
    )
    .expect("HDR pass");
    assert_eq!(pass.input_format(), DXGI_FORMAT_R16G16B16A16_FLOAT);
    // SAFETY: the frame is a same-device FP16 texture at the desktop extent.
    let image = pixels(
      &gpu,
      unsafe { pass.apply(&gpu.context, &frame) },
      width,
      height,
    );
    for v in 0..width {
      let grey = v as u8;
      assert_eq!(
        image[v as usize],
        [grey, grey, grey, 255],
        "{nits} nits, grey {v}"
      );
      let [r, g, b] = colour(v);
      assert_eq!(
        image[(width + v) as usize],
        [b, g, r, 255],
        "{nits} nits, colour {v}"
      );
    }
  }
}

#[test]
fn hdr_highlights_keep_their_hue_and_out_of_gamut_colours_clip_like_the_reference() {
  let _com = super::super::Apartment::new().expect("COM");
  let nits = 240.0;
  let texels: Vec<[f32; 3]> = vec![
    [0.0, 0.0, 0.0],
    // Three times SDR white, neutral: plain white.
    [9.0, 9.0, 9.0],
    // A bright saturated highlight keeps its 5:1 red-to-green ratio in linear light.
    [25.0, 5.0, 0.0],
    // Peak brightness of a 1000 nit display.
    [12.5, 11.0, 2.0],
    // Wide-gamut colours have negative BT.709 components.
    [-0.2, 2.0, 1.0],
    [0.5, -0.05, 4.0],
    [1000.0, 0.0, 0.0],
    [0.000_1, 0.000_1, 0.000_1],
  ];
  let (width, height) = (texels.len() as u32, 1u32);
  let gpu = warp_gpu(width.max(2), 2);
  let frame = scrgb(&gpu, width, height, &texels);
  let pass = FramePass::new(
    &gpu.device,
    width,
    height,
    Orientation::Upright,
    Tone::Hdr {
      sdr_white_nits: nits,
    },
  )
  .expect("HDR pass");
  // SAFETY: the frame is a same-device FP16 texture at the pass extent.
  let image = pixels(
    &gpu,
    unsafe { pass.apply(&gpu.context, &frame) },
    width,
    height,
  );
  for (index, texel) in texels.iter().enumerate() {
    // The reference sees what the texture stores.
    let stored = texel.map(|value| from_f16(f16(value)));
    let [r, g, b] = sdr_from_scrgb(stored, nits).map(level);
    let [actual_b, actual_g, actual_r, alpha] = image[index];
    for (actual, expected) in [(actual_r, r), (actual_g, g), (actual_b, b)] {
      assert!(
        actual.abs_diff(expected) <= 1,
        "{texel:?}: {:?} against reference {:?}",
        [actual_r, actual_g, actual_b],
        [r, g, b]
      );
    }
    assert_eq!(alpha, 255);
  }
  assert_eq!(image[1], [255, 255, 255, 255]);
  // Hue: green stays a fifth of red in linear light, blue stays zero.
  let [b, g, r, _] = image[2];
  assert_eq!((r, b), (255, 0));
  assert_eq!(g, level(sdr_from_scrgb([1.0, 0.2, 0.0], 80.0)[1]));
}

#[test]
fn a_rotated_hdr_display_is_turned_upright_and_mapped_in_one_pass() {
  let _com = super::super::Apartment::new().expect("COM");
  let nits = 240.0;
  let (width, height) = (37u32, 71u32);
  let gpu = warp_gpu(width, height);
  for orientation in [
    Orientation::Rotate90,
    Orientation::Rotate180,
    Orientation::Rotate270,
  ] {
    let (panel_width, panel_height) = orientation.panel(width, height);
    // Each panel texel carries its own coordinates as SDR levels: red = x, green = y.
    let texels: Vec<[f32; 3]> = (0..panel_height)
      .flat_map(|y| {
        (0..panel_width).map(move |x| [composed(x as u8, nits), composed(y as u8, nits), 0.0])
      })
      .collect();
    let frame = scrgb(&gpu, panel_width, panel_height, &texels);
    let pass = FramePass::new(
      &gpu.device,
      width,
      height,
      orientation,
      Tone::Hdr {
        sdr_white_nits: nits,
      },
    )
    .expect("rotated HDR pass");
    assert_eq!(pass.panel_extent(), (panel_width, panel_height));
    // SAFETY: the frame is a same-device FP16 texture at the panel extent.
    let image = pixels(
      &gpu,
      unsafe { pass.apply(&gpu.context, &frame) },
      width,
      height,
    );
    for y in 0..height {
      for x in 0..width {
        let (px, py) = orientation.panel_texel(width, height, x, y);
        assert_eq!(
          image[(y * width + x) as usize],
          [0, py as u8, px as u8, 255],
          "{orientation:?} desktop ({x}, {y})"
        );
      }
    }
  }
}

#[test]
fn passes_that_do_nothing_or_cannot_be_right_are_refused() {
  let _com = super::super::Apartment::new().expect("COM");
  let gpu = warp_gpu(16, 16);
  let hdr = |sdr_white_nits| Tone::Hdr { sdr_white_nits };
  assert!(FramePass::new(&gpu.device, 16, 16, Orientation::Upright, Tone::Sdr).is_err());
  assert!(FramePass::new(&gpu.device, 0, 16, Orientation::Rotate90, Tone::Sdr).is_err());
  assert!(FramePass::new(&gpu.device, 16, 0, Orientation::Upright, hdr(240.0)).is_err());
  for nits in [0.0, 79.0, f32::NAN, f32::INFINITY] {
    assert!(
      FramePass::new(&gpu.device, 16, 16, Orientation::Upright, hdr(nits)).is_err(),
      "{nits} nits"
    );
  }
}

#[test]
fn half_precision_fixtures_round_like_the_texture_does() {
  // Exact values, the largest half, rounding to nearest even and subnormals.
  for (value, half) in [
    (0.0f32, 0x0000u16),
    (1.0, 0x3c00),
    (-2.0, 0xc000),
    (65504.0, 0x7bff),
    (1.0 + 1.0 / 2048.0, 0x3c00),
    (1.0 + 3.0 / 2048.0, 0x3c02),
    (2f32.powi(-24), 0x0001),
    (2f32.powi(-14), 0x0400),
    (1e9, 0x7c00),
  ] {
    assert_eq!(f16(value), half, "{value}");
  }
  for half in [0x0001u16, 0x03ff, 0x0400, 0x3c00, 0x3c01, 0x7bff, 0xc000] {
    assert_eq!(f16(from_f16(half)), half, "{half:#06x}");
  }
}
