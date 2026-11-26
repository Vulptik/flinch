use super::types::NotificationRequest;
use crate::error::ErrorExt;
use crate::overlay::{
  chrome::{Renderer, Scene, TextStyle},
  gdi::layered::LayeredBitmap,
  theme,
};
use windows::Win32::Foundation::RECT;

const TOAST_WIDTH: i32 = 340;
const TOAST_HEIGHT: i32 = 76;

pub(super) fn render_hover(
  renderer: &mut Renderer,
  bitmap: &LayeredBitmap,
  alpha: u8,
) -> anyhow::Result<()> {
  let bounds = RECT {
    left: 0,
    top: 0,
    right: bitmap.width,
    bottom: bitmap.height,
  };
  let mut scene = Scene::default();
  scene.rounded(
    bounds,
    crate::overlay::chrome::PANEL_RADIUS,
    [160; 3],
    alpha / 20,
  )?;
  scene.outline(
    bounds,
    crate::overlay::chrome::PANEL_RADIUS,
    [105, 105, 116],
    alpha,
  );
  renderer.render(bitmap.dc, bounds, &scene)
}

/// Content-sized status layout using the same Direct2D/DirectWrite chrome as capture controls.
pub fn render_toast(
  renderer: &mut Renderer,
  request: &NotificationRequest,
) -> Result<(LayeredBitmap, i32), String> {
  let title = single_line(&request.title);
  let processing_body = request.processing.as_ref().map(|feedback| feedback.body());
  let body = single_line(processing_body.as_deref().unwrap_or(&request.body));
  let has_details = !body.trim().is_empty();
  let height = if has_details { TOAST_HEIGHT } else { 46 };
  let mut bitmap = LayeredBitmap::new(TOAST_WIDTH, height)?;
  bitmap.clear();
  let bounds = RECT {
    left: 0,
    top: 0,
    right: TOAST_WIDTH,
    bottom: height,
  };
  let result = (|| -> anyhow::Result<()> {
    let mut scene = Scene::default();
    scene.panel(bounds)?;
    scene.styled_text(
      RECT {
        left: 76,
        top: if has_details { 14 } else { 12 },
        right: TOAST_WIDTH - 18,
        bottom: if has_details { 36 } else { 34 },
      },
      &title,
      TextStyle::TITLE,
      theme::TEXT,
    )?;
    if has_details {
      scene.styled_text(
        RECT {
          left: 76,
          top: 40,
          right: TOAST_WIDTH - 18,
          bottom: 62,
        },
        &body,
        TextStyle::DETAILS,
        theme::SECONDARY_TEXT,
      )?;
    }
    renderer.render(bitmap.dc, bounds, &scene)
  })();
  result.map_err(|error| error.to_rich_string())?;
  render_preview(&mut bitmap, request, height)?;
  Ok((bitmap, height))
}

fn render_preview(
  bitmap: &mut LayeredBitmap,
  request: &NotificationRequest,
  height: i32,
) -> Result<(), String> {
  let size = if height == TOAST_HEIGHT { 48 } else { 30 };
  let source = if let Some(thumbnail) = &request.thumbnail {
    image::RgbaImage::from_raw(thumbnail.width, thumbnail.height, thumbnail.rgba.clone())
      .ok_or_else(|| "Invalid notification thumbnail dimensions or pixels".to_string())?
  } else {
    image::load_from_memory(include_bytes!(
      "../../../icons/Square44x44Logo.targetsize-48_altform-unplated.png"
    ))
    .map_err(|error| {
      anyhow::Error::new(error)
        .context("Decode notification logo")
        .to_rich_string()
    })?
    .into_rgba8()
  };
  if source.width() == 0 || source.height() == 0 {
    return Err("Notification preview dimensions must be nonzero".into());
  }
  let preview = image::imageops::resize(
    &image::imageops::crop_imm(
      &source,
      (source.width() - source.width().min(source.height())) / 2,
      (source.height() - source.width().min(source.height())) / 2,
      source.width().min(source.height()),
      source.width().min(source.height()),
    )
    .to_image(),
    size,
    size,
    image::imageops::FilterType::Triangle,
  );
  let left = 16 + (48 - size as i32) / 2;
  let top = (height - size as i32) / 2;
  let stride = bitmap.width as usize;
  let pixels = bitmap.pixels_mut();
  for (x, y, pixel) in preview.enumerate_pixels() {
    let offset = (((top as u32 + y) as usize * stride) + left as usize + x as usize) * 4;
    let alpha = u16::from(pixel[3]);
    for (dst, src) in [(0, 2), (1, 1), (2, 0)] {
      pixels[offset + dst] = ((u16::from(pixel[src]) * alpha
        + u16::from(pixels[offset + dst]) * (255 - alpha))
        / 255) as u8;
    }
    pixels[offset + 3] = (alpha + u16::from(pixels[offset + 3]) * (255 - alpha) / 255) as u8;
  }
  Ok(())
}

fn single_line(text: &str) -> String {
  text
    .chars()
    .take(512)
    .map(|c| if c.is_control() { ' ' } else { c })
    .collect()
}

pub const fn toast_width() -> i32 {
  TOAST_WIDTH
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[ignore = "explicit offscreen animated notification hover preview"]
  fn render_notification_motion_preview() {
    use image::codecs::gif::{GifEncoder, Repeat};
    use std::time::{Duration, Instant};
    let output = std::path::PathBuf::from(
      std::env::var("FLINCH_NOTIFICATION_PREVIEW_OUTPUT").expect("output directory"),
    );
    std::fs::create_dir_all(&output).expect("output");
    let mut encoder = GifEncoder::new_with_speed(
      std::fs::File::create(output.join("notification-motion.gif")).expect("GIF"),
      30,
    );
    encoder.set_repeat(Repeat::Infinite).expect("repeat");
    let mut renderer = Renderer::default();
    let request = NotificationRequest {
      title: crate::i18n::screenshot_notif_titles("en").saved.into(),
      body: "flinch_capture.png".into(),
      thumbnail: None,
      click_action: None,
      play_sound: false,
      critical: false,
      completed: None,
      processing: None,
    };
    for frame in 0..90 {
      let phase = frame / 30;
      let age = Duration::from_millis((frame % 30) * 16);
      let motion = crate::overlay::motion::Motion::preview(
        Duration::from_secs(1),
        Duration::from_secs(1),
        age,
        0,
        (phase == 1).then_some(1),
      );
      let (mut bitmap, height) = render_toast(&mut renderer, &request).expect("toast");
      let alpha = if phase == 2 {
        let appearing = crate::overlay::motion::Motion::preview(
          Duration::from_secs(1),
          Duration::from_secs(1),
          age,
          0,
          Some(1),
        );
        255 - appearing.hover_alpha(1, Instant::now())
      } else {
        motion.hover_alpha(1, Instant::now())
      };
      render_hover(&mut renderer, &bitmap, alpha).expect("hover");
      // SAFETY: flush owner-thread GDI writes before reading this owned DIB.
      assert!(unsafe { windows::Win32::Graphics::Gdi::GdiFlush() }.as_bool());
      let mut pixels = vec![0; (bitmap.width * height * 4) as usize];
      for (dst, src) in pixels
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(bitmap.pixels_mut().as_chunks::<4>().0)
      {
        let inverse = 255 - u16::from(src[3]);
        for (d, s) in [(0, 2), (1, 1), (2, 0)] {
          dst[d] = (u16::from(src[s]) + 28 * inverse / 255).min(255) as u8;
        }
        dst[3] = 255;
      }
      if frame == 59 {
        image::save_buffer(
          output.join("notification-hover.png"),
          &pixels,
          toast_width() as u32,
          height as u32,
          image::ColorType::Rgba8,
        )
        .expect("PNG");
      }
      encoder
        .encode_frame(image::Frame::from_parts(
          image::RgbaImage::from_raw(toast_width() as u32, height as u32, pixels).expect("RGBA"),
          0,
          0,
          image::Delay::from_numer_denom_ms(16, 1),
        ))
        .expect("frame");
    }
    assert_eq!(
      renderer.surface_misses, 1,
      "hover uses the cached renderer surface"
    );
  }

  #[test]
  fn error_bodies_fit_and_always_render_two_lines() {
    let mut renderer = Renderer::default();
    for locale in ["en", "fr", "de", "es", "it"] {
      for key in [
        "notification_error_capture",
        "notification_error_save",
        "notification_error_recording",
        "notification_error_capture_unavailable",
        "notification_error_microphone_blocked",
        "notification_error_microphone_missing",
        "notification_error_microphone_lost",
        "notification_error_gif_start",
      ] {
        let body = crate::i18n::native_settings(locale, key).expect("error copy");
        assert!(
          renderer
            .measure_text(&body, TextStyle::DETAILS)
            .expect("measure error body")
            <= (TOAST_WIDTH - 94) as f32,
          "{locale}/{key}: error body would be clipped"
        );
        let request = NotificationRequest {
          title: crate::i18n::native_settings(locale, "recording_failed").expect("title"),
          body,
          thumbnail: None,
          click_action: None,
          play_sound: false,
          critical: false,
          completed: None,
          processing: None,
        };
        assert_eq!(
          render_toast(&mut renderer, &request)
            .expect("error toast")
            .1,
          TOAST_HEIGHT
        );
      }
    }
  }

  #[test]
  fn zero_hover_preserves_pixels_and_full_hover_retains_content() {
    let request = NotificationRequest {
      title: "Saved".into(),
      body: "capture.png".into(),
      thumbnail: None,
      click_action: None,
      play_sound: false,
      critical: false,
      completed: None,
      processing: None,
    };
    let mut renderer = Renderer::default();
    let (mut bitmap, _) = render_toast(&mut renderer, &request).expect("toast");
    // SAFETY: finish owner-thread GDI writes before reading the owned DIB.
    assert!(unsafe { windows::Win32::Graphics::Gdi::GdiFlush() }.as_bool());
    let before = bitmap.pixels_mut().to_vec();
    render_hover(&mut renderer, &bitmap, 0).expect("zero hover");
    // SAFETY: finish owner-thread GDI writes before reading the owned DIB.
    assert!(unsafe { windows::Win32::Graphics::Gdi::GdiFlush() }.as_bool());
    assert_eq!(before, bitmap.pixels_mut());
    render_hover(&mut renderer, &bitmap, 255).expect("hover");
    // SAFETY: finish owner-thread GDI writes before reading the owned DIB.
    assert!(unsafe { windows::Win32::Graphics::Gdi::GdiFlush() }.as_bool());
    let after = bitmap.pixels_mut();
    assert_ne!(before, after);
    assert!(
      after
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200),
      "text remains bright"
    );
    assert_eq!(renderer.surface_misses, 1);
  }

  #[test]
  fn thumbnail_uses_center_crop_and_rejects_invalid_pixels() {
    let mut rgba = vec![0; 96 * 48 * 4];
    for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
      let x = index % 96;
      pixel.copy_from_slice(if (24..72).contains(&x) {
        &[0, 255, 0, 255]
      } else {
        &[255, 0, 0, 255]
      });
    }
    let mut request = NotificationRequest {
      title: "Capture saved".into(),
      body: "capture.png".into(),
      thumbnail: Some(super::super::types::ThumbnailImage {
        rgba,
        width: 96,
        height: 48,
      }),
      click_action: None,
      play_sound: false,
      critical: false,
      completed: None,
      processing: None,
    };
    let (mut bitmap, _) = render_toast(&mut Renderer::default(), &request).expect("thumbnail");
    let offset = (38 * toast_width() as usize + 40) * 4;
    assert_eq!(&bitmap.pixels_mut()[offset..offset + 4], &[0, 255, 0, 255]);
    request.thumbnail.as_mut().expect("thumbnail").rgba.clear();
    assert!(render_toast(&mut Renderer::default(), &request).is_err());
  }

  #[test]
  fn status_surface_preserves_premultiplied_alpha_and_fixed_geometry() {
    let request = NotificationRequest {
      title: "Capture saved".into(),
      body: "A long filename with accents éà and details.png".into(),
      thumbnail: None,
      click_action: None,
      play_sound: false,
      critical: false,
      completed: None,
      processing: None,
    };
    let (mut bitmap, height) =
      render_toast(&mut Renderer::default(), &request).expect("render status");
    assert_eq!(height, 76);
    assert_eq!(bitmap.width, toast_width());
    assert!(
      bitmap
        .pixels_mut()
        .as_chunks::<4>()
        .0
        .iter()
        .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3])
    );
    assert!(
      bitmap
        .pixels_mut()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[3] > 0 && p[3] < 255)
    );
  }

  #[test]
  fn notification_text_is_bounded_and_renderer_is_reused() {
    assert_eq!(single_line("one\ntwo\tthree"), "one two three");
    assert_eq!(single_line(&"é".repeat(2048)).chars().count(), 512);
    let request = NotificationRequest {
      title: "Capture saved".into(),
      body: "".into(),
      thumbnail: None,
      click_action: None,
      play_sound: false,
      critical: false,
      completed: None,
      processing: None,
    };
    let mut renderer = Renderer::default();
    let (_, height) = render_toast(&mut renderer, &request).expect("first toast");
    assert_eq!(
      height, 46,
      "title-only notifications use a single compact row"
    );
    render_toast(&mut renderer, &request).expect("second toast");
    assert_eq!(renderer.surface_misses, 1);
  }

  #[test]
  #[ignore = "explicit native notification previews"]
  fn render_notification_previews() {
    let output = std::path::PathBuf::from(
      std::env::var("FLINCH_NOTIFICATION_PREVIEW_OUTPUT").expect("output directory"),
    );
    std::fs::create_dir_all(&output).expect("create output");
    for lang in ["en", "fr", "de", "es", "it"] {
      for (case, body) in [
        ("thumbnail", "flinch_capture_2026-10-06.png"),
        ("saved", "flinch_capture_2026-10-06.png"),
        ("processing", ""),
        ("error", ""),
        (
          "long",
          "Capture — présentation du projet et notes de réunion — version finale.png",
        ),
      ] {
        let request = NotificationRequest {
          title: if case == "processing" {
            crate::i18n::native_settings(lang, "recording_processing_video")
              .expect("processing title")
              .to_string()
          } else if case == "error" {
            crate::i18n::screenshot_notif_titles(lang)
              .save_failed
              .to_string()
          } else {
            crate::i18n::screenshot_notif_titles(lang).saved.to_string()
          },
          body: if case == "error" {
            crate::i18n::native_settings(lang, "notification_error_save")
              .expect("error notification body")
          } else {
            body.into()
          },
          thumbnail: if case == "thumbnail" {
            let mut rgba = vec![0; 96 * 48 * 4];
            for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
              let x = index % 96;
              let y = index / 96;
              pixel.copy_from_slice(&[40 + x as u8, 80 + y as u8 * 2, 170, 255]);
            }
            Some(super::super::types::ThumbnailImage {
              rgba,
              width: 96,
              height: 48,
            })
          } else {
            None
          },
          click_action: None,
          play_sound: false,
          critical: false,
          completed: None,
          processing: (case == "processing").then(|| super::super::types::ProcessingFeedback {
            started: std::time::Instant::now() - std::time::Duration::from_secs(243),
            stage: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            labels: vec![
              crate::i18n::native_settings(lang, "recording_processing_preparing").expect("stage"),
            ],
          }),
        };
        let (mut bitmap, height) =
          render_toast(&mut Renderer::default(), &request).expect("render preview");
        let mut rgba = vec![0; (toast_width() * height * 4) as usize];
        for (dst, src) in rgba
          .as_chunks_mut::<4>()
          .0
          .iter_mut()
          .zip(bitmap.pixels_mut().as_chunks::<4>().0)
        {
          let inverse = 255 - u16::from(src[3]);
          for (d, s) in [(0, 2), (1, 1), (2, 0)] {
            dst[d] = (u16::from(src[s]) + 28 * inverse / 255).min(255) as u8;
          }
          dst[3] = 255;
        }
        image::save_buffer(
          output.join(format!("status-{lang}-{case}.png")),
          &rgba,
          toast_width() as u32,
          height as u32,
          image::ColorType::Rgba8,
        )
        .expect("save preview");
      }
    }
  }
}
