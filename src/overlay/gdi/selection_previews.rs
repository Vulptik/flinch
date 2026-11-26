//! Explicit offscreen design probe. No live overlay, input, timers or capture.
use super::{bitmap::ScreenshotBitmap, drawing::*};
use windows::Win32::{
  Foundation::{COLORREF, RECT},
  Graphics::Gdi::*,
};

fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
  RECT {
    left: x,
    top: y,
    right: x + w,
    bottom: y + h,
  }
}

fn text(dc: HDC, bounds: RECT, value: &str, scale: i32) {
  // SAFETY: thread-owned DC, finite UTF-16 buffer, font restored before deletion.
  unsafe {
    let font = CreateFontW(
      -14 * scale,
      0,
      0,
      0,
      500,
      0,
      0,
      0,
      DEFAULT_CHARSET,
      OUT_DEFAULT_PRECIS,
      CLIP_DEFAULT_PRECIS,
      CLEARTYPE_QUALITY,
      DEFAULT_PITCH.0 as u32,
      windows::core::w!("Segoe UI"),
    );
    assert!(!font.is_invalid());
    let old = SelectObject(dc, font.into());
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(0x00F5F5F5));
    let mut wide: Vec<u16> = value.encode_utf16().collect();
    let result = DrawTextW(
      dc,
      &mut wide,
      &mut { bounds },
      DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    SelectObject(dc, old);
    assert!(DeleteObject(font.into()).as_bool());
    assert_ne!(result, 0);
  }
}

#[test]
#[ignore = "explicit native selection design previews"]
fn render_selection_previews() {
  let output = std::path::PathBuf::from(
    std::env::var("FLINCH_SELECTION_PREVIEW_OUTPUT").expect("output directory"),
  );
  std::fs::create_dir_all(&output).expect("create output");
  for scale in [1, 2] {
    for background in ["light", "dark", "busy"] {
      for name in ["D-implemented"] {
        let (width, height) = (720 * scale, 480 * scale);
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
          for x in 0..width {
            let color = match background {
              "light" => {
                if y / scale < 60 {
                  [222, 227, 235]
                } else {
                  [246, 247, 250]
                }
              },
              "dark" => {
                if y / scale < 60 {
                  [32, 35, 43]
                } else {
                  [19, 22, 29]
                }
              },
              _ => {
                let tile = ((x / (36 * scale)) + (y / (28 * scale))) % 4;
                [
                  [57, 77, 105],
                  [204, 183, 128],
                  [101, 142, 129],
                  [161, 115, 132],
                ][tile as usize]
              },
            };
            rgba.extend_from_slice(&[color[0], color[1], color[2], 255]);
          }
        }
        let source =
          ScreenshotBitmap::from_rgba(&rgba, width as u32, height as u32).expect("source DIB");
        let mut target =
          ScreenshotBitmap::from_rgba(&rgba, width as u32, height as u32).expect("target DIB");
        let dc = target.dc;
        let selection = rect(100 * scale, 112 * scale, 520 * scale, 280 * scale);
        // SAFETY: both DIBs remain owned on this thread throughout rendering.
        unsafe {
          draw_dim_layer(dc, width, height, 160).expect("dim layer");
          draw_cutout(dc, source.dc, selection).expect("cutout");
        }
        let red = COLORREF(0x004444EF);
        for bounds in [
          selection,
          rect(290 * scale, 240 * scale, 100 * scale, 62 * scale),
        ] {
          // SAFETY: this test owns the destination DIB on the current thread.
          unsafe {
            draw_selection_chrome(dc, bounds, red, true).expect("production chrome");
          }
        }
        let pill = rect(300 * scale, 66 * scale, 120 * scale, 32 * scale);
        // SAFETY: valid owned DC; shared production pill primitive has bounded local resources.
        unsafe {
          draw_label_pill(dc, pill, 6 * scale).expect("dimensions pill");
        }
        text(dc, pill, "520 × 280", scale);
        text(
          dc,
          rect(0, 424 * scale, width, 36 * scale),
          &format!("{name} · {background} · {}%", scale * 100),
          scale,
        );
        if name == "D-implemented" {
          use crate::overlay::selection::{Rect, SelectionInteraction};
          use crate::overlay::selection_core::{
            SelectionDrawColors, SharedSelectionOverlayState, draw_selection_scene,
          };
          let mut shared = SharedSelectionOverlayState::new(
            width as u32,
            height as u32,
            std::sync::Arc::new(rgba.clone()),
            0,
            0,
            std::time::Instant::now(),
          );
          shared.resources.screenshot = Some(source);
          shared.resources.offscreen_dc = dc;
          shared.controller.interaction = SelectionInteraction::Confirmed {
            rect: Rect {
              x: 100.0,
              y: 112.0,
              w: 520.0,
              h: 280.0,
            },
            from_snap: false,
            moving: None,
            resizing: None,
          };
          // SAFETY: borrowed stock font and target DC remain valid on this test thread.
          let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
          draw_selection_scene(&mut shared, font, SelectionDrawColors::DEFAULT);
        }
        // SAFETY: finish GDI writes before reading this thread's DIB.
        assert!(unsafe { GdiFlush() }.as_bool());
        image::save_buffer(
          output.join(format!("{name}-{background}-{}pct.png", scale * 100)),
          &target.to_rgba().expect("readback"),
          width as u32,
          height as u32,
          image::ColorType::Rgba8,
        )
        .expect("save preview");
      }
    }
  }
}

#[test]
#[ignore = "explicit animated selection feedback preview"]
fn render_selection_feedback_preview() {
  let output = std::path::PathBuf::from(
    std::env::var("FLINCH_SELECTION_PREVIEW_OUTPUT").expect("output directory"),
  );
  std::fs::create_dir_all(&output).expect("create output");
  let (width, height) = (960, 360);
  let mut rgba = Vec::with_capacity((width * height * 4) as usize);
  for y in 0..height {
    for x in 0..width {
      let rgb = match x / 320 {
        0 => [246, 247, 250],
        1 => [24, 27, 34],
        _ => [
          [57, 77, 105],
          [223, 204, 151],
          [101, 142, 129],
          [161, 115, 132],
        ][((x / 28 + y / 24) % 4) as usize],
      };
      rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
    }
  }
  let source = ScreenshotBitmap::from_rgba(&rgba, width as u32, height as u32).expect("source DIB");
  let file = std::fs::File::create(output.join("selection-feedback.gif")).expect("create GIF");
  let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(file, 10);
  encoder
    .set_repeat(image::codecs::gif::Repeat::Infinite)
    .expect("repeat");
  // Exactly 48 frames, no timers or live windows. Production message loops are untouched.
  for frame in 0i32..48 {
    let phase = frame / 16;
    let mut target =
      ScreenshotBitmap::from_rgba(&rgba, width as u32, height as u32).expect("target DIB");
    let dc = target.dc;
    // SAFETY: both native DIBs belong to this test thread.
    unsafe {
      draw_dim_layer(dc, width, height, 145).expect("dim");
    }
    for (column, name) in ["Light", "Dark", "Busy"].iter().enumerate() {
      let left = column as i32 * 320;
      let bounds = rect(left + 28, 94, 264, 174);
      // SAFETY: source and destination DCs remain valid for the bounded drawing pass.
      unsafe {
        draw_cutout(dc, source.dc, bounds).expect("cutout");
      }
      // SAFETY: native DIB belongs to this test thread; preview uses production feedback.
      unsafe {
        if phase == 2 {
          draw_selection_chrome(dc, bounds, COLORREF(0x004444EF), true).expect("confirmed chrome");
        } else {
          draw_selection_feedback(
            dc,
            bounds,
            COLORREF(0x004444EF),
            Some(frame * 3),
            phase == 1,
          )
          .expect("active feedback");
        }
      }
      text(dc, rect(left, 24, 320, 28), name, 1);
      text(
        dc,
        rect(left, 302, 320, 28),
        [
          "Dragging · moving segments",
          "Snapping · stronger corners",
          "Confirmed · quiet frame",
        ][phase as usize],
        1,
      );
    }
    // SAFETY: finish native text writes before DIB readback on this thread.
    assert!(unsafe { GdiFlush() }.as_bool());
    let pixels = target.to_rgba().expect("readback");
    if frame % 16 == 8 {
      image::save_buffer(
        output.join(format!("feedback-phase-{phase}.png")),
        &pixels,
        width as u32,
        height as u32,
        image::ColorType::Rgba8,
      )
      .expect("save still");
    }
    let image =
      image::RgbaImage::from_raw(width as u32, height as u32, pixels).expect("frame image");
    encoder
      .encode_frame(image::Frame::from_parts(
        image,
        0,
        0,
        image::Delay::from_numer_denom_ms(100, 1),
      ))
      .expect("encode GIF");
  }
}
