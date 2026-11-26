//! The recognized-text panel on the headless platform: job ownership, every outcome, copy
//! completion and close paths. The isolated app has no clipboard, so Copy fails without
//! touching the system clipboard; no native window or foreground change is involved.
use super::*;
use crate::app::headless::{advance, drain, drain_until};
use flinch_settings::slint::platform::WindowEvent;

fn window() -> TextPanelWindow {
  PANEL.with(|panel| {
    panel
      .borrow()
      .as_ref()
      .expect("text panel exists")
      .window
      .clone_strong()
  })
}

fn job() -> u64 {
  PANEL.with(|panel| panel.borrow().as_ref().expect("text panel exists").job)
}

fn isolated(language: &str) -> AppHandle {
  AppHandle::isolated(
    crate::config::Config {
      language: language.into(),
      ..Default::default()
    },
    std::path::PathBuf::new(),
  )
}

#[test]
fn headless_text_panel_keeps_only_the_newest_job_and_reports_copy_and_close() {
  crate::app::headless::run(text_panel_scenario);
}

fn text_panel_scenario() {
  let app = isolated("en");

  // Nothing on screen: results, copies and closes are discarded.
  finish(u64::MAX, TextOutcome::Text("late".into())).unwrap();
  drain();
  copy(&app, "unused".into());
  close();
  shutdown();
  copied(u64::MAX, Ok(()));
  assert!(PANEL.with(|panel| panel.borrow().is_none()));

  let first_cancel = Arc::new(AtomicBool::new(false));
  let first = open(&app, first_cancel.clone()).unwrap();
  drain();
  let window = window();
  assert_eq!(job(), first);
  assert!(window.window().is_visible());
  assert!(window.get_reading());
  assert_eq!(window.get_notice(), TextPanelNotice::None);
  assert_eq!(window.get_language_index(), app_language_index("en"));

  finish(first, TextOutcome::Text("Recognized".into())).unwrap();
  drain();
  assert!(!window.get_reading());
  assert_eq!(window.get_text().as_str(), "Recognized");
  for (outcome, notice) in [
    (TextOutcome::Empty, TextPanelNotice::Empty),
    (TextOutcome::TooLarge, TextPanelNotice::TooLarge),
    (TextOutcome::TimedOut, TextPanelNotice::TimedOut),
    (TextOutcome::Failed, TextPanelNotice::Failed),
  ] {
    finish(first, outcome).unwrap();
    drain();
    assert_eq!(window.get_notice(), notice);
  }

  // A newer job takes over the same window and cancels the one it replaces.
  let second_cancel = Arc::new(AtomicBool::new(false));
  let second = open(&app, second_cancel.clone()).unwrap();
  assert!(second > first);
  drain();
  assert!(first_cancel.load(Ordering::Acquire));
  assert!(!second_cancel.load(Ordering::Acquire));
  assert_eq!(job(), second);
  assert!(window.get_reading());
  assert_eq!(window.get_text().as_str(), "");
  assert_eq!(window.get_notice(), TextPanelNotice::None);
  finish(first, TextOutcome::Text("stale".into())).unwrap();
  drain();
  assert!(window.get_reading(), "a replaced job's result is discarded");
  finish(second, TextOutcome::Text("Fresh".into())).unwrap();
  drain();
  assert_eq!(window.get_text().as_str(), "Fresh");

  // Copy runs on the worker runtime; without a clipboard it reports the failure in the panel.
  crate::app::runtime::test_runtime();
  window.set_notice(TextPanelNotice::Failed);
  window.invoke_copy_request("Fresh".into());
  assert!(window.get_copying());
  assert_eq!(window.get_notice(), TextPanelNotice::None);
  drain_until(|| !window.get_copying());
  assert_eq!(window.get_notice(), TextPanelNotice::CopyFailed);
  assert!(!window.get_copied());
  // A successful copy shows Copied for a moment, unless a newer job replaced it.
  copied(second, Ok(()));
  assert!(window.get_copied());
  advance(COPIED_HOLD.as_millis() as u64 - 10);
  assert!(window.get_copied());
  advance(20);
  assert!(!window.get_copied());
  copied(first, Ok(()));
  assert!(!window.get_copied());

  // Close and the window's own close both hide and cancel without forgetting the window.
  window.invoke_close_request();
  assert!(!window.window().is_visible());
  assert!(second_cancel.load(Ordering::Acquire));
  let third_cancel = Arc::new(AtomicBool::new(false));
  open(&app, third_cancel.clone()).unwrap();
  drain();
  assert!(window.window().is_visible());
  copied(job(), Ok(()));
  window.window().dispatch_event(WindowEvent::CloseRequested);
  assert!(!window.window().is_visible());
  assert!(third_cancel.load(Ordering::Acquire));
  advance(COPIED_HOLD.as_millis() as u64);
  assert!(window.get_copied(), "closing stops the Copied reset");

  // Another language selects its own copy.
  let german = app_language_index("de");
  open(&isolated("de"), Arc::new(AtomicBool::new(false))).unwrap();
  drain();
  assert_eq!(window.get_language_index(), german);

  shutdown();
  assert!(PANEL.with(|panel| panel.borrow().is_none()));
}

fn app_language_index(language: &str) -> i32 {
  flinch_settings::model::LOCALES
    .iter()
    .position(|locale| *locale == language)
    .unwrap() as i32
}
