//! Review panel for recognized text. Workers submit one bounded event per job;
//! the Slint window and its job state stay on the UI owner thread.

use super::placement;
use crate::app::context::AppHandle;
use flinch_settings::slint::{
  self, ComponentHandle,
  winit_030::{
    WinitWindowAccessor,
    winit::raw_window_handle::{HasWindowHandle, RawWindowHandle},
  },
};
use flinch_settings::ui::{TextPanelNotice, TextPanelWindow};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use windows::Win32::{
  Foundation::HWND,
  System::Threading::{AttachThreadInput, GetCurrentThreadId},
  UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE,
    SWP_NOSIZE, SetForegroundWindow, SetWindowPos,
  },
};

const COPIED_HOLD: Duration = Duration::from_millis(1500);

#[cfg(test)]
mod frame_export;
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextOutcome {
  Text(String),
  Empty,
  TooLarge,
  TimedOut,
  Failed,
}

struct Panel {
  window: TextPanelWindow,
  job: u64,
  cancel: Arc<AtomicBool>,
  copied_timer: slint::Timer,
}

thread_local! { static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) }; }
static NEXT_JOB: AtomicU64 = AtomicU64::new(0);

/// Shows the reading state. A newer job replaces the one on screen.
pub(crate) fn open(app: &AppHandle, cancel: Arc<AtomicBool>) -> Result<u64, String> {
  let job = NEXT_JOB.fetch_add(1, Ordering::AcqRel) + 1;
  let app = app.clone();
  slint::invoke_from_event_loop(move || {
    if let Err(error) = show(&app, job, cancel.clone()) {
      tracing::error!(%error, "Cannot open the text panel");
      cancel.store(true, Ordering::Release);
      let strings = crate::i18n::get(&crate::i18n::get_lang(&app));
      crate::notifications::show_error(&app, strings.capture_failed, "notification_error_capture");
    }
  })
  .map_err(|error| error.to_string())?;
  Ok(job)
}

/// Results for a closed or replaced job are discarded on the owner thread.
pub(crate) fn finish(job: u64, outcome: TextOutcome) -> Result<(), String> {
  slint::invoke_from_event_loop(move || {
    with_job(job, |panel| {
      let window = &panel.window;
      window.set_reading(false);
      bring_to_front(window);
      match outcome {
        TextOutcome::Text(text) => {
          window.set_text(text.into());
          window.invoke_focus_text();
        },
        TextOutcome::Empty => window.set_notice(TextPanelNotice::Empty),
        TextOutcome::TooLarge => window.set_notice(TextPanelNotice::TooLarge),
        TextOutcome::TimedOut => window.set_notice(TextPanelNotice::TimedOut),
        TextOutcome::Failed => window.set_notice(TextPanelNotice::Failed),
      }
    });
  })
  .map_err(|error| error.to_string())
}

pub(super) fn shutdown() {
  PANEL.with(|panel| {
    if let Some(panel) = panel.borrow_mut().take() {
      panel.cancel.store(true, Ordering::Release);
    }
  });
}

fn with_job(job: u64, apply: impl FnOnce(&Panel)) {
  PANEL.with(|panel| {
    if let Some(panel) = panel.borrow().as_ref().filter(|panel| panel.job == job) {
      apply(panel);
    }
  });
}

fn show(app: &AppHandle, job: u64, cancel: Arc<AtomicBool>) -> Result<(), String> {
  let window = PANEL.with(|panel| {
    let mut panel = panel.borrow_mut();
    if let Some(current) = panel.as_mut() {
      current.cancel.store(true, Ordering::Release);
      current.copied_timer.stop();
      current.job = job;
      current.cancel = cancel;
      return Ok(current.window.clone_strong());
    }
    let window = create(app)?;
    *panel = Some(Panel {
      window: window.clone_strong(),
      job,
      cancel,
      copied_timer: slint::Timer::default(),
    });
    Ok::<_, String>(window)
  })?;
  let language = crate::i18n::get_lang(app);
  let index = flinch_settings::model::LOCALES
    .iter()
    .position(|locale| *locale == language)
    .unwrap_or(0);
  window.set_language_index(index as i32);
  window.set_text("".into());
  window.set_notice(TextPanelNotice::None);
  window.set_reading(true);
  window.set_copying(false);
  window.set_copied(false);
  if !window.window().is_visible() {
    placement::center_on_pointer(
      window.window(),
      window.get_initial_width(),
      window.get_initial_height(),
    )?;
  }
  window.window().set_minimized(false);
  window.show().map_err(|error| error.to_string())?;
  bring_to_front(&window);
  Ok(())
}

/// The overlay that had the foreground is gone by now, so Windows may refuse a
/// plain activation. The native window can also still be pending on first show;
/// `finish` raises the panel again once the result is ready.
fn bring_to_front(window: &TextPanelWindow) {
  let handle = window
    .window()
    .with_winit_window(|window| window.window_handle().map(|handle| handle.as_raw()));
  let Some(Ok(RawWindowHandle::Win32(handle))) = handle else {
    tracing::debug!("Text panel window is not ready to raise");
    return;
  };
  if let Err(error) = raise(HWND(handle.hwnd.get() as *mut std::ffi::c_void)) {
    tracing::warn!(%error, "Cannot raise the text panel");
  }
}

/// A brief topmost pass settles the z-order even if activation is refused, and
/// leaves the panel free to go behind the app the user pastes into.
fn raise(hwnd: HWND) -> windows::core::Result<()> {
  let flags = SWP_NOMOVE | SWP_NOSIZE;
  // SAFETY: the HWND belongs to the live panel on this owner thread. Input
  // queues stay attached only across one non-blocking activation call.
  unsafe {
    SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags)?;
    SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags)?;
    let owner = GetWindowThreadProcessId(GetForegroundWindow(), None);
    let current = GetCurrentThreadId();
    let attached =
      owner != 0 && owner != current && AttachThreadInput(current, owner, true).as_bool();
    let activated = SetForegroundWindow(hwnd).as_bool();
    if attached {
      let _ = AttachThreadInput(current, owner, false);
    }
    if !activated {
      tracing::debug!("Windows refused to activate the text panel");
    }
  }
  Ok(())
}

fn create(app: &AppHandle) -> Result<TextPanelWindow, String> {
  let window = TextPanelWindow::new().map_err(|error| error.to_string())?;
  flinch_settings::windows::configure_text_panel(&window).map_err(|error| error.to_string())?;
  window.on_close_request(close);
  window.window().on_close_requested(|| {
    close();
    slint::CloseRequestResponse::KeepWindowShown
  });
  let app = app.clone();
  window.on_copy_request(move |text| copy(&app, text.to_string()));
  Ok(window)
}

fn close() {
  PANEL.with(|panel| {
    if let Some(panel) = panel.borrow().as_ref() {
      panel.cancel.store(true, Ordering::Release);
      panel.copied_timer.stop();
      if let Err(error) = panel.window.hide() {
        tracing::error!(%error, "Cannot hide the text panel");
      }
    }
  });
}

fn copy(app: &AppHandle, text: String) {
  let Some(job) = PANEL.with(|panel| {
    panel.borrow().as_ref().map(|panel| {
      panel.window.set_copying(true);
      panel.window.set_notice(TextPanelNotice::None);
      panel.job
    })
  }) else {
    return;
  };
  let app = app.clone();
  crate::app::runtime::spawn(async move {
    let result = crate::commands::copy_text_to_clipboard_with_retry(&app, text).await;
    if let Err(error) = slint::invoke_from_event_loop(move || copied(job, result)) {
      tracing::error!(%error, "Cannot deliver the text copy result");
    }
  });
}

fn copied(job: u64, result: Result<(), String>) {
  with_job(job, |panel| {
    panel.window.set_copying(false);
    match result {
      Ok(()) => {
        panel.window.set_copied(true);
        let window = panel.window.as_weak();
        panel
          .copied_timer
          .start(slint::TimerMode::SingleShot, COPIED_HOLD, move || {
            if let Some(window) = window.upgrade() {
              window.set_copied(false);
            }
          });
      },
      Err(error) => {
        tracing::error!(%error, "Cannot copy recognized text");
        panel.window.set_notice(TextPanelNotice::CopyFailed);
      },
    }
  });
}
