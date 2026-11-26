//! CPU color conversion for the GPU-less tier (T3).
//!
//! The D3D11 video processor that converts captured BGRA to NV12 on hardware adapters does not
//! exist on WARP (`ID3D11VideoDevice` is not supported; measured), so machines and
//! VMs without a GPU need this conversion. It matches the hardware path's output: BT.709,
//! limited range, chroma averaged over each 2×2 block, with the same display-relative crop.

use crate::types::{EngineError, Region, Result};

// BT.709 limited-range coefficients in 1/65536 units (Y: 16..235, Cb/Cr: 16..240).
const YR: i32 = 11_966; // 0.2126 * 219/255
const YG: i32 = 40_254; // 0.7152 * 219/255
const YB: i32 = 4_064; // 0.0722 * 219/255
const UR: i32 = -6_596; // -0.2126/1.8556 * 224/255
const UG: i32 = -22_189; // -0.7152/1.8556 * 224/255
const UB: i32 = 28_785; // 0.5 * 224/255
const VR: i32 = 28_785; // 0.5 * 224/255
const VG: i32 = -26_145; // -0.7152/1.5748 * 224/255
const VB: i32 = -2_640; // -0.0722/1.5748 * 224/255

/// Convert a BGRA picture (`stride` bytes per row, `width`×`height`) to tightly packed NV12,
/// cropped to `region` when given. The output extent must be even, as for the encoders.
pub fn bgra_to_nv12(
  bgra: &[u8],
  stride: usize,
  width: u32,
  height: u32,
  region: Option<Region>,
  nv12: &mut Vec<u8>,
) -> Result<(u32, u32)> {
  let area = match region {
    Some(region) => {
      region.within(width, height)?;
      region
    },
    None => Region {
      x: 0,
      y: 0,
      width,
      height,
    },
  };
  if !area.width.is_multiple_of(2) || !area.height.is_multiple_of(2) || area.width == 0 {
    return Err(EngineError::protocol(
      "NV12 needs an even, non-empty picture",
    ));
  }
  let rows = height as usize;
  if stride < width as usize * 4
    || bgra.len() < stride * rows.saturating_sub(1) + width as usize * 4
  {
    return Err(EngineError::protocol(
      "BGRA picture is smaller than its extent",
    ));
  }
  let (w, h) = (area.width as usize, area.height as usize);
  nv12.clear();
  nv12.resize(w * h * 3 / 2, 0);
  let (luma, chroma) = nv12.split_at_mut(w * h);
  let pixel = |x: usize, y: usize| -> (i32, i32, i32) {
    let at = (area.y as usize + y) * stride + (area.x as usize + x) * 4;
    (
      i32::from(bgra[at + 2]),
      i32::from(bgra[at + 1]),
      i32::from(bgra[at]),
    )
  };
  for y in 0..h {
    for x in 0..w {
      let (r, g, b) = pixel(x, y);
      luma[y * w + x] = ((YR * r + YG * g + YB * b + 32_768) / 65_536 + 16).clamp(16, 235) as u8;
    }
  }
  for y in (0..h).step_by(2) {
    for x in (0..w).step_by(2) {
      let mut sum = (0, 0, 0);
      for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        let (r, g, b) = pixel(x + dx, y + dy);
        sum = (sum.0 + r, sum.1 + g, sum.2 + b);
      }
      let (r, g, b) = (sum.0, sum.1, sum.2);
      // Sums of four pixels: divide by 4 × 65536 with rounding.
      let u = (UR * r + UG * g + UB * b + 131_072).div_euclid(262_144) + 128;
      let v = (VR * r + VG * g + VB * b + 131_072).div_euclid(262_144) + 128;
      let at = (y / 2) * w + x;
      chroma[at] = u.clamp(16, 240) as u8;
      chroma[at + 1] = v.clamp(16, 240) as u8;
    }
  }
  Ok((area.width, area.height))
}

#[cfg(test)]
mod tests {
  #![allow(clippy::expect_used)]
  use super::*;

  fn solid(width: u32, height: u32, stride: usize, rgb: [u8; 3]) -> Vec<u8> {
    let mut bgra = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
      for x in 0..width as usize {
        bgra[y * stride + x * 4..y * stride + x * 4 + 4]
          .copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
      }
    }
    bgra
  }

  #[test]
  fn primaries_land_on_their_bt709_limited_range_codes() {
    let mut nv12 = Vec::new();
    for (rgb, yuv) in [
      ([0, 0, 0], [16, 128, 128]),
      ([255, 255, 255], [235, 128, 128]),
      ([255, 0, 0], [63, 102, 240]),
      ([0, 255, 0], [173, 42, 26]),
      ([0, 0, 255], [32, 240, 118]),
    ] {
      bgra_to_nv12(&solid(4, 2, 16, rgb), 16, 4, 2, None, &mut nv12).expect("convert");
      let got = [nv12[0], nv12[8], nv12[9]];
      for (g, w) in got.iter().zip(yuv) {
        assert!(
          (i32::from(*g) - w).abs() <= 1,
          "{rgb:?}: {got:?} vs {yuv:?}"
        );
      }
    }
  }

  #[test]
  fn a_round_trip_through_the_independent_decoder_conversion_preserves_the_picture() {
    // A gradient with every channel varying, decoded by the GIF path's own NV12 → RGB.
    let (width, height, stride) = (64u32, 32u32, 64 * 4 + 12);
    let mut bgra = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
      for x in 0..width as usize {
        let at = y * stride + x * 4;
        bgra[at..at + 4].copy_from_slice(&[(x * 4) as u8, (y * 8) as u8, (128 + x) as u8, 255]);
      }
    }
    let mut nv12 = Vec::new();
    bgra_to_nv12(&bgra, stride, width, height, None, &mut nv12).expect("convert");
    let mut rgb = Vec::new();
    crate::gif::nv12_to_rgb(&nv12, width as usize, height as usize, 64, 32, &mut rgb)
      .expect("decode");
    let mut worst = 0;
    for y in 0..height as usize {
      for x in 0..width as usize {
        let at = y * stride + x * 4;
        let source = [bgra[at + 2], bgra[at + 1], bgra[at]];
        for channel in 0..3 {
          let back = rgb[(y * width as usize + x) * 3 + channel];
          worst = worst.max((i32::from(back) - i32::from(source[channel])).abs());
        }
      }
    }
    // Limited range and 2×2 chroma cost a few code values on smooth gradients, never more.
    assert!(worst <= 8, "worst channel error {worst}");
  }

  #[test]
  fn crops_take_the_region_and_bad_extents_are_refused() {
    let (width, height, stride) = (8u32, 6u32, 32usize);
    let mut bgra = solid(width, height, stride, [0, 0, 0]);
    // A white 2×2 block at (4, 2).
    for (x, y) in [(4, 2), (5, 2), (4, 3), (5, 3)] {
      bgra[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
    }
    let mut nv12 = Vec::new();
    let region = Region {
      x: 4,
      y: 2,
      width: 2,
      height: 2,
    };
    // Region::within enforces the engine's minimum area; small crops are refused.
    assert!(bgra_to_nv12(&bgra, stride, width, height, Some(region), &mut nv12).is_err());
    let big = solid(96, 48, 384, [255, 255, 255]);
    let crop = Region {
      x: 16,
      y: 8,
      width: 48,
      height: 34,
    };
    assert_eq!(
      bgra_to_nv12(&big, 384, 96, 48, Some(crop), &mut nv12).expect("crop"),
      (48, 34)
    );
    assert_eq!(nv12.len(), 48 * 34 * 3 / 2);
    assert!(nv12[..48 * 34].iter().all(|&y| y == 235));
    let outside = Region { x: 50, ..crop };
    assert!(bgra_to_nv12(&big, 384, 96, 48, Some(outside), &mut nv12).is_err());
    assert!(
      bgra_to_nv12(&big, 256, 63, 32, None, &mut nv12).is_err(),
      "odd width"
    );
    assert!(
      bgra_to_nv12(&big, 200, 64, 32, None, &mut nv12).is_err(),
      "short stride"
    );
    assert!(
      bgra_to_nv12(&big[..1000], 256, 64, 32, None, &mut nv12).is_err(),
      "short buffer"
    );
  }
}
