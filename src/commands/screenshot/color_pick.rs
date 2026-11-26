//! Copies the color of the pixel picked in the frozen screen as HEX, with a swatch notification.

use crate::app::context::AppHandle;
use crate::overlay::notification::{NOTIFICATION_THUMB_SIZE, ThumbnailImage};
use crate::screenshot::CaptureResult;

pub(super) async fn deliver(app: AppHandle, capture: CaptureResult) -> Result<(), String> {
  let color = picked_color(&capture)?;
  let hex = crate::overlay::magnifier::hex(color);
  match crate::commands::copy_text_to_clipboard_with_retry(&app, hex.clone()).await {
    Ok(()) => {
      notify_copied(&app, color, &hex);
      Ok(())
    },
    Err(error) => {
      // Nothing else shows the pick failed, so this bypasses the routine preference.
      let title = crate::i18n::tray(&crate::i18n::get_lang(&app)).color_picker;
      crate::notifications::show_error(&app, title, "color_copy_failed");
      Err(error)
    },
  }
}

/// The picked pixel is the whole 1×1 capture.
fn picked_color(capture: &CaptureResult) -> Result<[u8; 3], String> {
  match capture.rgba.get(..3) {
    Some([red, green, blue]) if capture.width >= 1 && capture.height >= 1 => {
      Ok([*red, *green, *blue])
    },
    _ => Err(format!(
      "The picked pixel is missing from a {}x{} capture",
      capture.width, capture.height
    )),
  }
}

fn notify_copied(app: &AppHandle, color: [u8; 3], hex: &str) {
  match crate::i18n::native_settings(&crate::i18n::get_lang(app), "color_copied") {
    Ok(title) => crate::notifications::show_rich(app, &title, hex, Some(swatch(color)), None),
    Err(error) => tracing::error!(%error, "Color copied notice localization failed"),
  }
}

/// Corner radius and outline of the notification swatch, at its 48px size.
const SWATCH_RADIUS: f32 = 10.0;
const SWATCH_OUTLINE: [u8; 3] = crate::overlay::theme::OUTLINE;

/// A rounded square of the picked color with a 1px outline, so black and very dark colors still
/// stand out on the notification, shown where a capture's thumbnail would be. Corners are
/// antialiased in straight alpha, which the notification blends over its panel.
pub(crate) fn swatch(color: [u8; 3]) -> ThumbnailImage {
  let side = NOTIFICATION_THUMB_SIZE;
  let mut rgba = Vec::with_capacity((side * side * 4) as usize);
  for y in 0..side {
    for x in 0..side {
      // Signed distance from the pixel center to the rounded square's edge; negative inside.
      let half = side as f32 / 2.0;
      let qx = ((x as f32 + 0.5) - half).abs() - (half - SWATCH_RADIUS);
      let qy = ((y as f32 + 0.5) - half).abs() - (half - SWATCH_RADIUS);
      let outside = qx.max(0.0).hypot(qy.max(0.0));
      let distance = outside + qx.max(qy).min(0.0) - SWATCH_RADIUS;
      let coverage = (0.5 - distance).clamp(0.0, 1.0);
      let ring = (distance + 1.5).clamp(0.0, 1.0);
      let mix = |fill: u8, line: u8| {
        (f32::from(fill) * (1.0 - ring) + f32::from(line) * ring).round() as u8
      };
      rgba.extend([
        mix(color[0], SWATCH_OUTLINE[0]),
        mix(color[1], SWATCH_OUTLINE[1]),
        mix(color[2], SWATCH_OUTLINE[2]),
        (coverage * 255.0).round() as u8,
      ]);
    }
  }
  ThumbnailImage {
    rgba,
    width: side,
    height: side,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn capture(width: u32, height: u32, rgba: Vec<u8>) -> CaptureResult {
    CaptureResult {
      path: std::path::PathBuf::new(),
      width,
      height,
      rgba,
      format: "png".into(),
      quality: 100,
      source_app: None,
      pin: None,
    }
  }

  #[test]
  fn the_picked_pixel_becomes_uppercase_hex() {
    let picked = picked_color(&capture(1, 1, vec![217, 47, 10, 255])).unwrap();
    assert_eq!(crate::overlay::magnifier::hex(picked), "#D92F0A");
    assert_eq!(crate::overlay::magnifier::hex([0, 0, 0]), "#000000");
    assert_eq!(crate::overlay::magnifier::hex([255, 255, 255]), "#FFFFFF");
    assert!(picked_color(&capture(0, 0, Vec::new())).is_err());
    assert!(picked_color(&capture(1, 1, vec![1, 2])).is_err());
  }

  #[test]
  fn the_notification_swatch_is_a_rounded_outlined_square_of_the_color() {
    let swatch = swatch([1, 2, 3]);
    assert_eq!((swatch.width, swatch.height), (48, 48));
    assert_eq!(swatch.rgba.len(), 48 * 48 * 4);
    let pixel = |x: usize, y: usize| &swatch.rgba[(y * 48 + x) * 4..(y * 48 + x) * 4 + 4];
    assert_eq!(pixel(24, 24), [1, 2, 3, 255], "the color fills the middle");
    assert_eq!(pixel(0, 0)[3], 0, "corners are rounded away");
    assert_eq!(pixel(47, 47)[3], 0);
    let partial = (0..10)
      .flat_map(|y| (0..10).map(move |x| (x, y)))
      .filter(|(x, y)| (1..255).contains(&pixel(*x, *y)[3]))
      .count();
    assert!(partial >= 4, "antialiased corner: {partial} partial pixels");
    let outline = crate::overlay::theme::OUTLINE;
    assert_eq!(&pixel(24, 0)[..3], &outline, "a 1px outline on the edge");
    assert_eq!(pixel(24, 0)[3], 255);
    assert_eq!(&pixel(24, 2)[..3], &[1, 2, 3], "only 1px wide");
  }

  #[test]
  fn a_copied_color_notifies_with_its_hex_and_swatch() {
    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    notify_copied(&app, [217, 47, 47], "#D92F2F");
    let requests = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .unwrap()
      .take_test_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].title, "Color copied");
    assert_eq!(requests[0].body, "#D92F2F");
    assert!(requests[0].click_action.is_none());
    let thumbnail = requests[0].thumbnail.as_ref().unwrap();
    let middle = (24 * 48 + 24) * 4;
    assert_eq!(&thumbnail.rgba[middle..middle + 4], &[217, 47, 47, 255]);
  }

  #[tokio::test]
  async fn a_failed_copy_is_reported_without_touching_the_system_clipboard() {
    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let result = deliver(app.clone(), capture(1, 1, vec![1, 2, 3, 255])).await;
    assert!(result.is_err());
    let requests = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .unwrap()
      .take_test_requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].critical);
    assert_eq!(requests[0].title, "Color Picker");
  }
}
