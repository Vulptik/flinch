//! The menu windows fade in whole, surface, border and compositor shadow included, as Windows 11
//! menus do; the content only slides. Fading the content alone left the dark surface and its
//! shadow popping in at full strength, which read as a flash.
//!
//! The fade is the window's layered opacity, stepped by a bounded owner-thread timer. A window
//! whose opacity cannot be set is left opaque and the reason is logged, so a menu is never stuck
//! invisible.

use std::rc::Rc;
use std::time::{Duration, Instant};

use flinch_settings::slint;
use windows::Win32::Foundation::{COLORREF, HWND};

/// How long a window takes to become opaque.
pub(super) const DURATION: Duration = Duration::from_millis(120);
/// Opacity steps; the fade ends after `DURATION` whatever the tick rate.
const TICK: Duration = Duration::from_millis(8);

/// The window's opacity `elapsed` into its fade, decelerating like the content's slide.
pub(super) fn opacity(elapsed: Duration) -> u8 {
  let t = (elapsed.as_secs_f32() / DURATION.as_secs_f32()).clamp(0.0, 1.0);
  let eased = 1.0 - (1.0 - t).powi(3);
  (eased * 255.0).round() as u8
}

/// One window's fade at a time; starting another cancels the last.
pub(super) struct Fade {
  timer: Rc<slint::Timer>,
}

impl Fade {
  pub(super) fn new() -> Self {
    Self {
      timer: Rc::new(slint::Timer::default()),
    }
  }

  /// Make a created window invisible before it is shown, so its first frame never pops in.
  pub(super) fn conceal(&self, hwnd: HWND) {
    self.timer.stop();
    if let Err(error) = apply(hwnd, 0) {
      tracing::warn!(%error, "Cannot prepare the tray menu fade; it appears without one");
    }
  }

  /// Fade the window from invisible to opaque, or make it opaque at once when Windows animations
  /// are off.
  pub(super) fn reveal(&self, hwnd: HWND, animate: bool) {
    self.timer.stop();
    let first = if animate { 0 } else { 255 };
    if let Err(error) = apply(hwnd, first) {
      tracing::warn!(%error, "Cannot fade in the tray menu; it appears without a fade");
      return;
    }
    if !animate {
      return;
    }
    let started = Instant::now();
    let timer = Rc::downgrade(&self.timer);
    self.timer.start(slint::TimerMode::Repeated, TICK, move || {
      let alpha = opacity(started.elapsed());
      let failed = match apply(hwnd, alpha) {
        Ok(()) => false,
        Err(error) => {
          tracing::warn!(%error, "Tray menu fade stopped; the menu is opaque");
          true
        },
      };
      if (alpha == u8::MAX || failed)
        && let Some(timer) = timer.upgrade()
      {
        timer.stop();
      }
    });
  }

  pub(super) fn stop(&self) {
    self.timer.stop();
  }
}

/// Set the window's whole opacity. If Windows refuses it, the window is made an ordinary opaque
/// window again rather than staying layered with no opacity, which would hide it.
fn apply(hwnd: HWND, alpha: u8) -> Result<(), String> {
  use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, LWA_ALPHA, SetLayeredWindowAttributes, SetWindowLongPtrW,
    WS_EX_LAYERED,
  };
  let layered = WS_EX_LAYERED.0 as isize;
  // SAFETY: the HWND belongs to a menu window on this owner thread; existing bits are kept, and a
  // destroyed window only makes these calls fail.
  unsafe {
    use windows::Win32::Foundation::{GetLastError, SetLastError, WIN32_ERROR};
    // No extended style at all is valid, so only a recorded error means the read failed.
    SetLastError(WIN32_ERROR(0));
    let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if style == 0 && GetLastError() != WIN32_ERROR(0) {
      return Err(format!(
        "Cannot read the window style: {}",
        windows::core::Error::from_thread()
      ));
    }
    if style & layered == 0 {
      SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | layered);
    }
    if let Err(error) = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) {
      SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !layered);
      return Err(format!("Cannot set the window opacity: {error}"));
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GWL_EXSTYLE, GetLayeredWindowAttributes, GetWindowLongPtrW,
    IsWindowVisible, LAYERED_WINDOW_ATTRIBUTES_FLAGS, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_POPUP,
  };
  use windows::core::w;

  #[test]
  fn opacity_rises_quickly_then_settles_and_ends_opaque() {
    assert_eq!(opacity(Duration::ZERO), 0);
    let mut last = 0;
    for ms in (8..=120).step_by(8) {
      let alpha = opacity(Duration::from_millis(ms));
      assert!(alpha >= last, "{ms} ms");
      last = alpha;
    }
    assert_eq!(opacity(DURATION), 255);
    assert_eq!(opacity(DURATION * 3), 255);
    // Decelerating: most of the way there by half time, as the slide is.
    assert!(opacity(DURATION / 2) > 200);
  }

  fn alpha_of(hwnd: HWND) -> u8 {
    let mut alpha = 0u8;
    let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
    // SAFETY: a live fixture window and writable locals.
    unsafe { GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)) }
      .expect("layered attributes");
    alpha
  }

  #[test]
  fn a_concealed_window_is_layered_and_invisible_and_reveals_opaque_without_animation() {
    // Regression: the menu surface and shadow popped in at full opacity while only the content
    // faded. A concealed window must start fully transparent.
    // SAFETY: a thread-owned hidden STATIC fixture, never shown or activated.
    let hwnd = unsafe {
      CreateWindowExW(
        WS_EX_NOACTIVATE,
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        0,
        0,
        64,
        64,
        None,
        None,
        None,
        None,
      )
    }
    .expect("fixture window");
    let fade = Fade::new();
    fade.conceal(hwnd);
    // SAFETY: style query of the live fixture.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    assert_ne!(style & WS_EX_LAYERED.0 as isize, 0);
    assert_ne!(
      style & WS_EX_NOACTIVATE.0 as isize,
      0,
      "existing bits are kept"
    );
    assert_eq!(alpha_of(hwnd), 0);
    fade.reveal(hwnd, false);
    assert_eq!(alpha_of(hwnd), 255);
    fade.stop();
    // SAFETY: the fixture is still hidden and owned here.
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
    // SAFETY: destroyed once.
    unsafe { DestroyWindow(hwnd) }.expect("destroy fixture");
    assert!(
      apply(hwnd, 255).is_err(),
      "a destroyed window is reported, not styled"
    );
  }
}
