//! Explicit offscreen export of the Copy Text panel: the production recognizer reads a capture
//! and the production panel shows its reading state, then the recognized lines, on the headless
//! platform. No native window, clipboard, foreground change or preference write is involved.
use super::*;
use crate::app::headless::{advance, drain};
use flinch_settings::slint::LogicalSize;

/// Reading-state frames at 30 per second, enough for one pass of the shimmer.
const READING_FRAMES: usize = 15;

fn save(window: &TextPanelWindow, path: &std::path::Path) {
  let buffer = window.window().take_snapshot().expect("panel snapshot");
  image::save_buffer(
    path,
    buffer.as_bytes(),
    buffer.width(),
    buffer.height(),
    image::ColorType::Rgba8,
  )
  .expect("write panel frame");
}

#[test]
#[ignore = "explicit Copy Text panel frame export"]
fn render_copy_text_panel() {
  crate::app::headless::run(|| {
    let input =
      std::path::PathBuf::from(std::env::var("FLINCH_EXPORT_INPUT").expect("capture PNG"));
    let output =
      std::path::PathBuf::from(std::env::var("FLINCH_EXPORT_OUTPUT").expect("output folder"));
    std::fs::create_dir_all(&output).expect("output folder");
    let capture = image::open(input).expect("capture pixels").into_rgba8();
    let (width, height) = capture.dimensions();
    let limits = crate::ocr::OcrLimits {
      cancel: Arc::new(AtomicBool::new(false)),
      deadline: std::time::Instant::now() + Duration::from_secs(60),
    };
    let text = crate::ocr::recognize(capture.as_raw(), width, height, &limits).expect("recognize");
    assert!(!text.trim().is_empty(), "the capture must contain text");

    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let job = open(&app, Arc::new(AtomicBool::new(false))).expect("open panel");
    drain();
    let window = PANEL.with(|panel| {
      panel
        .borrow()
        .as_ref()
        .expect("text panel exists")
        .window
        .clone_strong()
    });
    // As on a machine with Windows animations on, so the reading shimmer moves.
    window
      .global::<flinch_settings::ui::Interaction>()
      .set_motion_enabled(true);
    window.window().set_size(LogicalSize::new(
      window.get_initial_width(),
      window.get_initial_height(),
    ));
    assert!(window.get_reading());
    for frame in 0..READING_FRAMES {
      save(&window, &output.join(format!("reading-{frame:02}.png")));
      advance(33);
    }
    finish(job, TextOutcome::Text(text.clone())).expect("finish panel");
    drain();
    assert!(!window.get_reading());
    assert_eq!(window.get_text().as_str(), text);
    // Let the text area lay out the new lines before the snapshot.
    advance(100);
    save(&window, &output.join("text.png"));
    std::fs::write(output.join("text.txt"), &text).expect("write recognized text");
    shutdown();
    drain();
  });
}
