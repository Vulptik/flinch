use super::*;

pub(super) fn render_store_listing_images() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use crate::overlay::selection_core::{
    SelectionDrawColors, SharedSelectionOverlayState, draw_selection_scene,
  };
  use std::{path::PathBuf, sync::Arc, time::Instant};
  use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GdiFlush, GetStockObject};
  let input = PathBuf::from(std::env::var("FLINCH_STORE_RENDER_INPUT").expect("input PNG"));
  let output =
    PathBuf::from(std::env::var("FLINCH_STORE_RENDER_OUTPUT").expect("output directory"));
  let pixels = image::open(input).expect("read demo image").into_rgba8();
  let (width, height) = pixels.dimensions();
  assert_eq!((width, height), (1366, 768));
  std::fs::create_dir_all(&output).expect("output directory");
  let source = Arc::new(pixels.into_raw());
  let mut shared =
    SharedSelectionOverlayState::new(width, height, Arc::clone(&source), 0, 0, Instant::now());
  shared.resources.screenshot =
    Some(ScreenshotBitmap::from_rgba(&source, width, height).expect("demo source"));
  let mut offscreen = ScreenshotBitmap::from_rgba(&source, width, height).expect("render target");
  shared.resources.offscreen_dc = offscreen.dc; // Borrowed; the bitmap owns cleanup.
  // SAFETY: stock font is borrowed and both DCs remain owned by this test thread.
  let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
  shared.controller.interaction = SelectionInteraction::Confirmed {
    rect: Rect {
      x: 128.0,
      y: 114.0,
      w: 1110.0,
      h: 498.0,
    },
    from_snap: false,
    moving: None,
    resizing: None,
  };
  shared.controller.cursor_pos = (-100.0, -100.0);
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  let movie = std::env::var("FLINCH_STORE_RENDER_MOVIE").ok();
  if movie.is_some() {
    shared.controller.interaction = SelectionInteraction::Confirmed {
      rect: Rect {
        x: 107.0,
        y: 40.0,
        w: 1152.0,
        h: 648.0,
      },
      from_snap: false,
      moving: None,
      resizing: None,
    };
  }
  for annotated in [false, true] {
    if movie.is_some() && annotated {
      continue;
    }
    draw_selection_scene(&mut shared, font, SelectionDrawColors::DEFAULT);
    if annotated {
      for (index, (tool, points, label)) in [
        (Tool::Rectangle, vec![(896, 266), (1156, 419)], None),
        (Tool::Arrow, vec![(825, 491), (996, 426)], None),
        (
          Tool::Text,
          vec![(690, 512)],
          Some(Label::Text(
            text_data("Check retry handling".into(), 24, 400).expect("label"),
          )),
        ),
      ]
      .into_iter()
      .enumerate()
      {
        assert!(
          editor.apply_change(crate::overlay::screenshot::history::Change {
            counter: None,
            index: editor.annotations.len(),
            before: None,
            after: Some(Annotation {
              id: index as u64 + 1,
              tool,
              points,
              label,
              color: [239, 68, 68],
              width: 3,
              corner_radius: 0,
              rotation_millidegrees: 0,
              path_transform: crate::screenshot::path::IDENTITY,
              curve_controls: Vec::new(),
              pen_nodes: Vec::new(),
            }),
          })
        );
      }
      editor.paint_cached(
        &shared.controller,
        offscreen.dc,
        font,
        RECT {
          left: 0,
          top: 0,
          right: width as i32,
          bottom: height as i32,
        },
        false,
      );
      assert!(!editor.render_failed);
    }
    // SAFETY: complete this thread's GDI operations before reading the owned DIB.
    assert!(unsafe { GdiFlush() }.as_bool());
    let rgba = offscreen.to_rgba().expect("read rendered pixels");
    image::save_buffer(
      output.join(if annotated {
        "02-annotation-editor.png"
      } else {
        "01-area-selection.png"
      }),
      &rgba,
      width,
      height,
      image::ColorType::Rgba8,
    )
    .expect("save PNG");
  }
  if movie.is_some() {
    use crate::overlay::gdi::layered::LayeredBitmap;
    use crate::overlay::recording_shell::{
      RecordingOverlayShell, draw_recording_toolbar, draw_selection_border,
    };
    use crate::overlay::recording_toolbar::{TimerTextStyle, fitted_metrics, toolbar_layout};
    let mut hud = LayeredBitmap::new(width as i32, height as i32).expect("HUD surface");
    let now = Instant::now();
    let mut shell =
      RecordingOverlayShell::new(now - std::time::Duration::from_secs(12), "en".into());
    shell.phase = crate::overlay::recording_toolbar::RecordingPhase::Recording;
    let metrics = fitted_metrics(
      &mut shell.chrome,
      &shell.lang,
      shell.phase,
      std::time::Duration::from_secs(12),
      TimerTextStyle::Hms,
    )
    .expect("recording layout");
    draw_selection_border(&mut hud, 107, 40, 1259, 688, 2);
    // Initialize the same visibility state as the live host before a static export.
    assert!(shell.needs_paint(((width as i32 - metrics.width) / 2, 700)));
    draw_recording_toolbar(
      &mut hud,
      &mut shell,
      toolbar_layout(metrics, (width as i32 - metrics.width) / 2, 700),
      metrics,
      TimerTextStyle::Hms,
      now + std::time::Duration::from_millis(250),
    )
    .expect("recording toolbar");
    // SAFETY: synchronize writes before reading this thread's owned DIB pixels.
    assert!(unsafe { GdiFlush() }.as_bool());
    let mut rgba = source.as_ref().clone();
    for (dst, bgra) in rgba
      .as_chunks_mut::<4>()
      .0
      .iter_mut()
      .zip(hud.pixels_mut().as_chunks::<4>().0)
    {
      let inverse = 255 - u16::from(bgra[3]);
      for (channel, source_channel) in [(0, 2), (1, 1), (2, 0)] {
        dst[channel] = (u16::from(bgra[source_channel]) + u16::from(dst[channel]) * inverse / 255)
          .min(255) as u8;
      }
      dst[3] = 255;
    }
    image::save_buffer(
      output.join("03-video-recording.png"),
      &rgba,
      width,
      height,
      image::ColorType::Rgba8,
    )
    .expect("save HUD PNG");
  }
}

pub(super) fn render_toolbar_previews() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use crate::overlay::selection_core::{
    SelectionDrawColors, SharedSelectionOverlayState, draw_selection_scene_with_indicator,
  };
  use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GdiFlush, GetStockObject};
  let output = std::path::PathBuf::from(
    std::env::var("FLINCH_TOOLBAR_PREVIEW_OUTPUT").expect("output directory"),
  );
  std::fs::create_dir_all(&output).expect("create output");
  for (case, lang) in [
    ("area", "en"),
    ("pen", "en"),
    ("colors", "en"),
    ("width", "en"),
    ("text", "en"),
    ("wrapped", "en"),
    ("top-edge", "en"),
    ("full-height", "en"),
    ("full-height-area", "en"),
    ("drag-over-indicator", "en"),
    ("bottom-edge", "en"),
    ("negative-origin", "en"),
    ("pen", "fr"),
    ("pen", "de"),
    ("pen", "es"),
    ("pen", "it"),
    ("styles", "en"),
    ("styles", "fr"),
    ("styles", "de"),
    ("styles", "es"),
    ("styles", "it"),
    ("limit", "en"),
    ("limit", "fr"),
    ("limit", "de"),
    ("limit", "es"),
    ("limit", "it"),
    ("error-tooltip", "en"),
    ("error-tooltip", "fr"),
    ("error-tooltip", "de"),
    ("error-tooltip", "es"),
    ("error-tooltip", "it"),
    ("error-wrapped", "de"),
    ("error-bottom", "en"),
    ("tooltip", "en"),
    ("tooltip", "de"),
    ("scaled-150", "en"),
    ("scaled-200", "en"),
    ("grip", "en"),
    ("grip-scaled-200", "en"),
    ("redact", "en"),
    ("redact", "de"),
    ("redact-none", "en"),
    ("redact-none", "fr"),
  ] {
    let (width, height) = (
      if matches!(case, "wrapped" | "error-wrapped") {
        420
      } else if matches!(case, "scaled-200" | "grip-scaled-200") {
        1400
      } else {
        960
      },
      640,
    );
    let mut pixels = image::RgbaImage::from_pixel(width, height, image::Rgba([28, 30, 37, 255]));
    for y in 100..430 {
      for x in 60..width - 60 {
        let shade = if y % 44 < 2 { 63 } else { 39 };
        pixels.put_pixel(x, y, image::Rgba([shade, shade + 2, shade + 7, 255]));
      }
    }
    let source = std::sync::Arc::new(pixels.into_raw());
    let mut shared = SharedSelectionOverlayState::new(
      width,
      height,
      source.clone(),
      0,
      0,
      std::time::Instant::now(),
    );
    shared.resources.screenshot =
      Some(ScreenshotBitmap::from_rgba(&source, width, height).expect("source DIB"));
    let mut target = ScreenshotBitmap::from_rgba(&source, width, height).expect("target DIB");
    shared.resources.offscreen_dc = target.dc;
    let region = Rect {
      x: 60.0,
      y: if matches!(case, "top-edge" | "full-height" | "full-height-area") {
        0.0
      } else if case == "bottom-edge" {
        300.0
      } else {
        100.0
      },
      w: (width - 120) as f64,
      h: if matches!(case, "full-height" | "full-height-area") {
        640.0
      } else {
        330.0
      },
    };
    shared.controller.interaction = SelectionInteraction::Confirmed {
      rect: region,
      from_snap: false,
      moving: None,
      resizing: None,
    };
    shared.controller.cursor_pos = (-100.0, -100.0);
    if case == "negative-origin" {
      shared.controller.vd_offset_x = -1920;
      shared.controller.vd_offset_y = -200;
    }
    // SAFETY: stock font is borrowed; both DIBs stay on this rendering thread.
    let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
    let mut editor = AnnotationEditor::new(lang.into());
    editor.monitors.clear();
    // Scaled cases put the crop on one monitor at that DPI; others keep 100% fallback bounds.
    if let Some(percent) = case.trim_start_matches("grip-").strip_prefix("scaled-") {
      let dpi_scale = percent.parse::<f64>().expect("scale percent") / 100.0;
      editor.monitors.push(crate::monitors::MonitorInfo {
        id: case.into(),
        name: case.into(),
        x: 0,
        y: 0,
        width,
        height,
        is_primary: true,
        dpi_scale,
        logical_x: 0,
        logical_y: 0,
        logical_width: (f64::from(width) / dpi_scale) as u32,
        logical_height: (f64::from(height) / dpi_scale) as u32,
      });
    }
    if case == "drag-over-indicator" {
      editor.toolbar_position = Some((199, 34));
    }
    if !matches!(case, "area" | "full-height-area") {
      editor.tool = Tool::Pen;
    }
    editor.color_open = matches!(case, "colors" | "scaled-200");
    editor.width_open = matches!(case, "width" | "text" | "styles" | "scaled-150");
    editor.text_style_open = case == "styles";
    editor.text_error = case == "limit" || case.starts_with("error-");
    if case == "limit" || case.starts_with("error-") {
      editor.tool = Tool::Text;
    }
    if case == "error-bottom" {
      editor.toolbar_position = Some((200, 590));
    }
    editor.error_details_pinned = case.starts_with("error-");
    if matches!(case, "text" | "styles" | "scaled-150") {
      editor.tool = Tool::Text;
    }
    if case.starts_with("redact") {
      editor.tool = Tool::Blur;
    }
    if case == "redact-none" {
      editor.redact_notice = Some("annotation_error_redact_none");
      editor.error_details_pinned = true;
    }
    let show_indicator = editor.shows_selection_indicator(&shared.controller);
    draw_selection_scene_with_indicator(
      &mut shared,
      font,
      SelectionDrawColors::DEFAULT,
      show_indicator,
    );
    let toolbar = editor.toolbar_rect(&shared.controller, region);
    let hovered = toolbar_button(
      toolbar,
      if case == "text" {
        13
      } else if case.starts_with("redact") {
        11
      } else if case == "area" {
        0
      } else {
        1
      },
      editor.metrics(&shared.controller, region),
    );
    let hovered = if case.starts_with("grip") {
      toolbar_grip(toolbar, editor.metrics(&shared.controller, region))
    } else {
      hovered
    };
    shared.controller.cursor_pos = (
      (hovered.left + hovered.right) as f64 / 2.0,
      (hovered.top + hovered.bottom) as f64 / 2.0,
    );
    if case == "tooltip" {
      editor.button_tip.show_now(1);
    }
    if case == "redact" {
      editor.button_tip.show_now(11);
    }
    editor.paint_cached(
      &shared.controller,
      target.dc,
      font,
      RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
      },
      false,
    );
    assert!(!editor.render_failed);
    // SAFETY: flush this thread's GDI writes before reading the owned target.
    assert!(unsafe { GdiFlush() }.as_bool());
    image::save_buffer(
      output.join(format!("toolbar-{case}-{lang}.png")),
      &target.to_rgba().expect("readback"),
      width,
      height,
      image::ColorType::Rgba8,
    )
    .expect("save preview");
  }
}

pub(super) fn render_motion_preview() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use crate::overlay::selection_core::{
    SelectionDrawColors, SharedSelectionOverlayState, draw_selection_scene_with_indicator,
  };
  use image::codecs::gif::{GifEncoder, Repeat};
  use std::{
    sync::Arc,
    time::{Duration, Instant},
  };
  use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GdiFlush, GetStockObject};
  let output = std::path::PathBuf::from(
    std::env::var("FLINCH_TOOLBAR_PREVIEW_OUTPUT").expect("output directory"),
  );
  std::fs::create_dir_all(&output).expect("output");
  let (width, height) = (800, 450);
  let source = Arc::new(
    image::RgbaImage::from_pixel(width, height, image::Rgba([39, 41, 46, 255])).into_raw(),
  );
  let mut shared =
    SharedSelectionOverlayState::new(width, height, source.clone(), 0, 0, Instant::now());
  shared.resources.screenshot =
    Some(ScreenshotBitmap::from_rgba(&source, width, height).expect("source"));
  let rect = Rect {
    x: 40.0,
    y: 40.0,
    w: 720.0,
    h: 250.0,
  };
  shared.controller.interaction = SelectionInteraction::Confirmed {
    rect,
    from_snap: false,
    moving: None,
    resizing: None,
  };
  // SAFETY: stock font is borrowed for this thread's offscreen render only.
  let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
  let mut encoder = GifEncoder::new_with_speed(
    std::fs::File::create(output.join("chrome-motion.gif")).expect("GIF"),
    30,
  );
  encoder.set_repeat(Repeat::Infinite).expect("repeat");
  let mut editor = AnnotationEditor::new("en".into());
  editor.monitors.clear();
  editor.tool = Tool::Pen;
  let mut first = None;
  let mut settled = None;
  for frame in 0..90 {
    let phase = frame / 30;
    let age = Duration::from_millis((frame % 30) * 16);
    let hold = Duration::from_secs(1);
    editor.color_open = phase >= 1;
    shared.controller.cursor_pos = if phase >= 2 {
      let cell = toolbar_button(
        editor.toolbar_rect(&shared.controller, rect),
        1,
        editor.metrics(&shared.controller, rect),
      );
      (
        (cell.left + cell.right) as f64 / 2.0,
        (cell.top + cell.bottom) as f64 / 2.0,
      )
    } else {
      (-100.0, -100.0)
    };
    editor.motion = crate::overlay::motion::Motion::preview(
      if phase == 0 { age } else { hold },
      if phase == 1 { age } else { hold },
      age,
      u8::from(editor.color_open),
      if phase >= 2 { Some(1) } else { None },
    );
    let mut target = ScreenshotBitmap::from_rgba(&source, width, height).expect("target");
    shared.resources.handle_motion = crate::overlay::motion::Motion::preview(
      if phase == 0 { age } else { hold },
      hold,
      hold,
      0,
      None,
    );
    shared.resources.offscreen_dc = target.dc;
    draw_selection_scene_with_indicator(&mut shared, font, SelectionDrawColors::DEFAULT, false);
    editor.draw(&shared.controller, target.dc, font);
    assert!(!editor.render_failed);
    // SAFETY: flush this thread's GDI writes before reading its owned DIB.
    assert!(unsafe { GdiFlush() }.as_bool());
    let pixels = target.to_rgba().expect("pixels");
    if frame == 0 {
      first = Some(pixels.clone());
    }
    if frame == 29 {
      settled = Some(pixels.clone());
    }
    if frame == 5 || frame == 89 {
      image::save_buffer(
        output.join(format!("motion-frame-{frame}.png")),
        &pixels,
        width,
        height,
        image::ColorType::Rgba8,
      )
      .expect("PNG");
    }
    encoder
      .encode_frame(image::Frame::from_parts(
        image::RgbaImage::from_raw(width, height, pixels).expect("RGBA"),
        0,
        0,
        image::Delay::from_numer_denom_ms(16, 1),
      ))
      .expect("frame");
    shared.resources.offscreen_dc = HDC::default();
  }
  assert_ne!(
    first, settled,
    "entrance must visibly change the production render"
  );
  assert!(
    editor.chrome.surface_misses <= 2,
    "motion must reuse toolbar surfaces"
  );
  assert!(
    editor.popup_chrome.surface_misses <= 2,
    "motion must reuse picker surfaces"
  );
  assert!(
    !editor.animation_active(),
    "last rendered frame settles the clock"
  );
}
