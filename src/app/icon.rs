// src/app/icon.rs
// Application icon loading utilities

use crate::app::context::Image;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconState {
  Recording,
  Paused,
}

fn recording_icon_rgba(size: u32, state: TrayIconState) -> Vec<u8> {
  let mut rgba = vec![0u8; (size * size * 4) as usize];
  let center = (size as f32 - 1.0) / 2.0;
  let outer_radius = size as f32 * 0.38;
  let inner_radius = size as f32 * 0.28;
  let (inner_red, inner_green, inner_blue) = match state {
    TrayIconState::Recording => (217, 47, 47),
    TrayIconState::Paused => (232, 146, 33),
  };

  for y in 0..size {
    for x in 0..size {
      let dx = x as f32 - center;
      let dy = y as f32 - center;
      let distance = (dx * dx + dy * dy).sqrt();
      let offset = ((y * size + x) * 4) as usize;

      if distance <= outer_radius {
        rgba[offset] = 32;
        rgba[offset + 1] = 32;
        rgba[offset + 2] = 32;
        rgba[offset + 3] = 255;
      }

      if distance <= inner_radius {
        rgba[offset] = inner_red;
        rgba[offset + 1] = inner_green;
        rgba[offset + 2] = inner_blue;
        rgba[offset + 3] = 255;
      }
    }
  }

  rgba
}

pub fn build_recording_tray_icon(size: u32, state: TrayIconState) -> Image<'static> {
  Image::new_owned(recording_icon_rgba(size, state), size, size)
}

/// Dedicated transparent, high-contrast artwork for notification-area sizes.
pub fn load_tray_icon() -> Option<Image<'static>> {
  match image::load_from_memory(include_bytes!("../../icons/tray.png")) {
    Ok(image) => {
      let rgba = image.to_rgba8();
      let (width, height) = rgba.dimensions();
      Some(Image::new_owned(rgba.into_raw(), width, height))
    },
    Err(error) => {
      tracing::error!(operation = "load_tray_icon", %error, "Failed to decode embedded tray icon");
      None
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tray_artwork_is_transparent_and_visible_at_small_sizes() {
    let image = image::load_from_memory(include_bytes!("../../icons/tray.png"))
      .expect("embedded tray artwork should decode")
      .to_rgba8();
    assert_eq!(image.dimensions(), (32, 32));
    assert_eq!(image.get_pixel(0, 0)[3], 0);
    let small = image::imageops::resize(&image, 16, 16, image::imageops::FilterType::Lanczos3);
    // Both parts of the small-size mark must survive downsampling.
    let red = small
      .pixels()
      .filter(|pixel| {
        pixel[3] > 128 && pixel[0] > 140 && u16::from(pixel[0]) > u16::from(pixel[1]) * 2
      })
      .count();
    let grey = small
      .pixels()
      .filter(|pixel| pixel[3] > 128 && pixel[0] > 120 && pixel[1] > 120)
      .count();
    assert!(red >= 30, "the bolt must remain recognizable at 16 px");
    assert!(grey >= 8, "the arrow must remain visible at 16 px");
  }

  #[test]
  fn paused_tray_icon_uses_pause_color() {
    let icon = recording_icon_rgba(32, TrayIconState::Paused);
    let center = ((16 * 32 + 16) * 4) as usize;
    assert_eq!(icon[center], 232);
    assert_eq!(icon[center + 1], 146);
    assert_eq!(icon[center + 2], 33);
    assert_eq!(icon[center + 3], 255);
  }
}
