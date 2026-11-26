use super::types::{NOTIFICATION_THUMB_SIZE, ThumbnailImage};

/// Preserve the original screenshot pixels for one-shot notification rendering.
/// The notification renderer performs the final cover-scale and centered crop
/// based on the thumbnail slot size.
pub fn build_thumbnail_image(rgba: &[u8], width: u32, height: u32) -> ThumbnailImage {
  if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
    return ThumbnailImage {
      rgba: vec![0u8; (NOTIFICATION_THUMB_SIZE * NOTIFICATION_THUMB_SIZE * 4) as usize],
      width: NOTIFICATION_THUMB_SIZE,
      height: NOTIFICATION_THUMB_SIZE,
    };
  }

  ThumbnailImage {
    rgba: rgba.to_vec(),
    width,
    height,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn solid_pixel(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    [r, g, b, a]
  }

  #[test]
  fn invalid_input_returns_blank_thumbnail() {
    let thumb = build_thumbnail_image(&[], 0, 0);

    assert_eq!(thumb.width, NOTIFICATION_THUMB_SIZE);
    assert_eq!(thumb.height, NOTIFICATION_THUMB_SIZE);
    assert!(thumb.rgba.iter().all(|value| *value == 0));
  }

  #[test]
  fn valid_input_preserves_original_pixels_and_dimensions() {
    let rgba = [
      solid_pixel(255, 0, 0, 255),
      solid_pixel(0, 255, 0, 255),
      solid_pixel(0, 0, 255, 255),
      solid_pixel(255, 255, 0, 255),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    let thumb = build_thumbnail_image(&rgba, 2, 2);

    assert_eq!(thumb.width, 2);
    assert_eq!(thumb.height, 2);
    assert_eq!(thumb.rgba, rgba);
  }
}
