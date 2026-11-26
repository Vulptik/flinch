//! Reads a selected area off the UI thread, then opens the review panel or, when
//! the user chose it, copies straight to the clipboard with a notification.

use crate::app::context::AppHandle;
use crate::ocr::{OcrError, OcrLimits};
use crate::screenshot::CaptureResult;
use crate::settings::text_panel::{self, TextOutcome};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TIME_LIMIT: Duration = Duration::from_secs(30);
const PREVIEW_CHARACTERS: usize = 80;

/// The newest direct job wins, so an older read cannot overwrite the clipboard later.
static DIRECT_JOB: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

pub(super) async fn deliver(app: AppHandle, capture: CaptureResult) -> Result<(), String> {
  if crate::config::Config::current(&app).copy_text_direct {
    read_to_clipboard(app, capture).await
  } else {
    read_into_panel(app, capture).await
  }
}

async fn read_into_panel(app: AppHandle, capture: CaptureResult) -> Result<(), String> {
  let cancel = Arc::new(AtomicBool::new(false));
  let job = text_panel::open(&app, cancel.clone())?;
  match read_capture(capture, cancel).await {
    Some(outcome) => text_panel::finish(job, outcome),
    None => Ok(()),
  }
}

async fn read_to_clipboard(app: AppHandle, capture: CaptureResult) -> Result<(), String> {
  let cancel = replace_direct_job()?;
  let Some(outcome) = read_capture(capture, cancel.clone()).await else {
    return Ok(());
  };
  let TextOutcome::Text(text) = outcome else {
    notify_problem(&app, problem_key(&outcome));
    return Ok(());
  };
  if cancel.load(Ordering::Acquire) {
    return Ok(());
  }
  let notice = preview(&text);
  match crate::commands::copy_text_to_clipboard_with_retry(&app, text).await {
    Ok(()) => {
      match crate::i18n::native_settings(&crate::i18n::get_lang(&app), "ocr_text_copied") {
        Ok(title) => crate::notifications::show(&app, &title, &notice),
        Err(error) => tracing::error!(%error, "Text copied notice localization failed"),
      }
      Ok(())
    },
    Err(error) => {
      notify_problem(&app, "ocr_copy_failed");
      Err(error)
    },
  }
}

fn replace_direct_job() -> Result<Arc<AtomicBool>, String> {
  let mut current = DIRECT_JOB
    .lock()
    .map_err(|error| crate::commands::ipc_display_context("Text job state poisoned", error))?;
  if let Some(previous) = current.as_ref() {
    previous.store(true, Ordering::Release);
  }
  let cancel = Arc::new(AtomicBool::new(false));
  *current = Some(cancel.clone());
  Ok(cancel)
}

/// With no panel on screen, every outcome that copied nothing must still be
/// visible, so these bypass the routine notification preference.
fn notify_problem(app: &AppHandle, body_key: &str) {
  let title = crate::i18n::tray(&crate::i18n::get_lang(app)).copy_text;
  crate::notifications::show_error(app, title, body_key);
}

fn problem_key(outcome: &TextOutcome) -> &'static str {
  match outcome {
    TextOutcome::Empty => "ocr_empty",
    TextOutcome::TooLarge => "ocr_too_large",
    TextOutcome::TimedOut => "ocr_timed_out",
    TextOutcome::Text(_) | TextOutcome::Failed => "ocr_failed",
  }
}

/// The first line, shortened, lets the user see what was read.
fn preview(text: &str) -> String {
  let line = text.lines().next().unwrap_or_default();
  let mut characters = line.chars();
  let short: String = characters.by_ref().take(PREVIEW_CHARACTERS).collect();
  if characters.next().is_some() || text.lines().nth(1).is_some() {
    format!("{short}…")
  } else {
    short
  }
}

async fn read_capture(capture: CaptureResult, cancel: Arc<AtomicBool>) -> Option<TextOutcome> {
  let limits = OcrLimits {
    cancel,
    deadline: Instant::now() + TIME_LIMIT,
  };
  read(capture.rgba, capture.width, capture.height, limits).await
}

/// A model run cannot be interrupted, so the deadline also abandons the task;
/// its late result is dropped with the handle. None means the job was cancelled.
async fn read(rgba: Vec<u8>, width: u32, height: u32, limits: OcrLimits) -> Option<TextOutcome> {
  let cancel = limits.cancel.clone();
  let deadline = tokio::time::Instant::from_std(limits.deadline);
  let task =
    tokio::task::spawn_blocking(move || crate::ocr::recognize(&rgba, width, height, &limits));
  match tokio::time::timeout_at(deadline, task).await {
    Ok(Ok(result)) => outcome(result),
    Ok(Err(error)) => {
      tracing::error!(%error, "Text recognition task failed");
      Some(TextOutcome::Failed)
    },
    Err(_) => {
      cancel.store(true, Ordering::Release);
      outcome(Err(OcrError::TimedOut))
    },
  }
}

fn outcome(result: Result<String, OcrError>) -> Option<TextOutcome> {
  match result {
    Ok(text) if text.is_empty() => Some(TextOutcome::Empty),
    Ok(text) => Some(TextOutcome::Text(text)),
    Err(OcrError::Cancelled) => None,
    Err(OcrError::TimedOut) => {
      tracing::warn!("Text recognition exceeded its time limit");
      Some(TextOutcome::TimedOut)
    },
    Err(OcrError::TooLarge) => Some(TextOutcome::TooLarge),
    Err(OcrError::Failed(error)) => {
      tracing::error!(
        error = %crate::error::ErrorExt::to_rich_string(&error),
        "Text recognition failed"
      );
      Some(TextOutcome::Failed)
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn limits(cancelled: bool, time: Duration) -> OcrLimits {
    OcrLimits {
      cancel: Arc::new(AtomicBool::new(cancelled)),
      deadline: Instant::now() + time,
    }
  }

  #[test]
  fn every_recognition_result_maps_to_one_panel_state() {
    assert_eq!(
      outcome(Ok("Hello".into())),
      Some(TextOutcome::Text("Hello".into()))
    );
    assert_eq!(outcome(Ok(String::new())), Some(TextOutcome::Empty));
    assert_eq!(outcome(Err(OcrError::Cancelled)), None);
    assert_eq!(
      outcome(Err(OcrError::TimedOut)),
      Some(TextOutcome::TimedOut)
    );
    assert_eq!(
      outcome(Err(OcrError::TooLarge)),
      Some(TextOutcome::TooLarge)
    );
    assert_eq!(
      outcome(Err(anyhow::anyhow!("model load").into())),
      Some(TextOutcome::Failed)
    );
  }

  #[tokio::test]
  async fn cancelled_jobs_stay_quiet_and_expired_jobs_report_the_limit() {
    let pixels = vec![255; 64 * 64 * 4];
    let cancelled = limits(true, TIME_LIMIT);
    assert_eq!(read(pixels.clone(), 64, 64, cancelled).await, None);
    assert_eq!(
      read(pixels, 64, 64, limits(false, Duration::ZERO)).await,
      Some(TextOutcome::TimedOut)
    );
  }

  fn capture(rgba: Vec<u8>, width: u32, height: u32) -> CaptureResult {
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

  fn direct_app() -> AppHandle {
    AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        notifications_enabled: false,
        notification_sound: false,
        copy_text_direct: true,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    )
  }

  fn notices(app: &AppHandle) -> Vec<crate::overlay::notification::NotificationRequest> {
    app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .expect("manager")
      .take_test_requests()
  }

  #[tokio::test]
  #[serial_test::serial]
  async fn direct_mode_says_when_nothing_was_copied_even_with_notifications_off() {
    let app = direct_app();
    deliver(app.clone(), capture(vec![255; 200 * 100 * 4], 200, 100))
      .await
      .expect("handled");
    let notices = notices(&app);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].critical);
  }

  #[tokio::test]
  #[serial_test::serial]
  async fn direct_mode_reports_a_clipboard_failure_after_reading() {
    let image =
      image::load_from_memory(include_bytes!("../../../tests/fixtures/ocr/en-hotkeys.png"))
        .expect("fixture")
        .to_rgba8();
    let (width, height) = image.dimensions();
    let app = direct_app();
    let error = deliver(app.clone(), capture(image.into_raw(), width, height))
      .await
      .unwrap_err();
    assert!(error.contains("Clipboard"), "{error}");
    let notices = notices(&app);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].critical);
  }

  #[test]
  #[serial_test::serial]
  fn a_newer_direct_job_cancels_the_older_one() {
    let older = replace_direct_job().expect("job");
    let newer = replace_direct_job().expect("job");
    assert!(older.load(Ordering::Acquire));
    assert!(!newer.load(Ordering::Acquire));
  }

  #[test]
  fn every_problem_has_its_own_message() {
    assert_eq!(problem_key(&TextOutcome::Empty), "ocr_empty");
    assert_eq!(problem_key(&TextOutcome::TooLarge), "ocr_too_large");
    assert_eq!(problem_key(&TextOutcome::TimedOut), "ocr_timed_out");
    assert_eq!(problem_key(&TextOutcome::Failed), "ocr_failed");
  }

  #[test]
  fn previews_show_the_first_line_and_mark_anything_left_out() {
    assert_eq!(preview("Hello"), "Hello");
    assert_eq!(preview("Hello\nWorld"), "Hello…");
    let long = "é".repeat(PREVIEW_CHARACTERS + 1);
    assert_eq!(preview(&long).chars().count(), PREVIEW_CHARACTERS + 1);
    assert!(preview(&long).ends_with('…'));
    assert_eq!(
      preview(&"é".repeat(PREVIEW_CHARACTERS)).chars().count(),
      PREVIEW_CHARACTERS
    );
  }

  #[tokio::test]
  async fn invalid_buffers_fail_instead_of_reading_out_of_bounds() {
    let result = read(vec![0; 3], 64, 64, limits(false, TIME_LIMIT)).await;
    assert_eq!(result, Some(TextOutcome::Failed));
  }
}
