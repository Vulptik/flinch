//! The Capture Delay countdown pill, top center of the pointer's monitor.
//!
//! It never takes focus or input (a tool window that does not activate), so a menu or tooltip
//! the user opens during the wait stays open, and it is excluded from every capture. The window
//! belongs to the countdown thread, which pumps its messages and destroys it on drop.

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
  DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, SW_SHOWNOACTIVATE,
  ShowWindow, TranslateMessage, WM_DPICHANGED,
};

use crate::error::ErrorExt;
use crate::overlay::chrome::{Renderer, Scene, TextStyle};
use crate::overlay::gdi::layered::LayeredBitmap;
use crate::overlay::gdi::window::{
  create_hud_window, disable_window_animations, exclude_from_capture, register_overlay_class,
  unregister_overlay_class,
};
use crate::overlay::theme;

/// Matches the "Hold Alt to edit" pill: height, padding and distance from the monitor's top.
const HEIGHT: f32 = 38.0;
const PADDING_X: f32 = 16.0;
const EDGE_GAP: f32 = 24.0;

unsafe extern "system" fn pill_proc(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  // Each paint places and sizes the pill itself. The default handling of a scaling change moves
  // and resizes the window to Windows' suggested rectangle, which left it off center on a
  // monitor whose scaling differs from the one it was created on.
  if message == WM_DPICHANGED {
    return LRESULT(0);
  }
  // SAFETY: forwards this thread's own window messages to the default handler unchanged.
  unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// The pill's top-left: centered on `monitor`, `EDGE_GAP` below its top.
fn place(monitor: RECT, width: i32, scale: f32) -> (i32, i32) {
  let span = monitor.right - monitor.left;
  (
    monitor.left + (span - width) / 2,
    monitor.top + (EDGE_GAP * scale).round() as i32,
  )
}

pub(crate) struct Pill {
  hwnd: HWND,
  class: String,
  template: String,
  monitor: RECT,
  scale: f32,
  renderer: Renderer,
  shown: bool,
}

impl Pill {
  pub(crate) fn new(lang: &str) -> Result<Self, String> {
    let template = crate::i18n::native_settings(lang, "capture_delay_countdown")?;
    let monitor = crate::monitors::monitor_under_cursor()
      .or_else(crate::monitors::get_primary_monitor)
      .ok_or("No monitor to show the countdown on")?;
    let class = format!("FlinchCountdown_{:?}", std::thread::current().id());
    register_overlay_class(&class, Some(pill_proc))?;
    // Created on its own monitor, so showing it there is no scaling change.
    let (x, y) = (
      monitor.x.saturating_add((monitor.width / 2) as i32),
      monitor.y,
    );
    let hwnd = match create_hud_window(&class, "FlinchCountdown", x, y, 1, 1, std::ptr::null_mut())
    {
      Ok(hwnd) => hwnd,
      Err(error) => {
        unregister_overlay_class(&class);
        return Err(error);
      },
    };
    disable_window_animations(hwnd);
    if !exclude_from_capture(hwnd) {
      tracing::warn!(
        operation = "capture_delay",
        "Countdown pill may appear in captures"
      );
    }
    Ok(Self {
      hwnd,
      class,
      template,
      monitor: RECT {
        left: monitor.x,
        top: monitor.y,
        right: monitor.x.saturating_add(monitor.width as i32),
        bottom: monitor.y.saturating_add(monitor.height as i32),
      },
      scale: (monitor.dpi_scale as f32).clamp(1.0, 4.0),
      renderer: Renderer::default(),
      shown: false,
    })
  }

  fn paint(&mut self, text: &str) -> anyhow::Result<()> {
    let bitmap = render_pill(&mut self.renderer, text, self.scale)?;
    let width = bitmap.width;
    let (x, y) = place(self.monitor, width, self.scale);
    bitmap
      .apply_to(self.hwnd, x, y)
      .map_err(anyhow::Error::msg)?;
    if !self.shown {
      // SAFETY: this thread owns the live window; SW_SHOWNOACTIVATE leaves focus where it is.
      let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
      self.shown = true;
    }
    Ok(())
  }
}

/// The pill's pixels for `text` at a monitor's UI scale.
fn render_pill(renderer: &mut Renderer, text: &str, scale: f32) -> anyhow::Result<LayeredBitmap> {
  let style = TextStyle::LABEL.scaled(scale);
  let text_width = renderer.measure_text(text, style)?;
  let width = (text_width + PADDING_X * 2.0 * scale).ceil() as i32;
  let height = (HEIGHT * scale).round() as i32;
  let mut bitmap = LayeredBitmap::new(width, height).map_err(anyhow::Error::msg)?;
  bitmap.clear();
  let bounds = RECT {
    left: 0,
    top: 0,
    right: width,
    bottom: height,
  };
  let mut scene = Scene::scaled(scale)?;
  scene.panel(bounds)?;
  let pad = (PADDING_X * scale).round() as i32;
  let inset = (9.0 * scale).round() as i32;
  scene.styled_text(
    RECT {
      left: pad,
      top: inset,
      right: width - pad,
      bottom: height - inset,
    },
    text,
    TextStyle::LABEL,
    theme::TEXT,
  )?;
  renderer.render(bitmap.dc, bounds, &scene)?;
  Ok(bitmap)
}

impl crate::capture_delay::Presenter for Pill {
  fn show(&mut self, remaining: u32) -> Result<(), String> {
    let text = self.template.replace("{value}", &remaining.to_string());
    self.paint(&text).map_err(|error| error.to_rich_string())
  }

  fn pump(&mut self) {
    let mut message = MSG::default();
    // Bounded: only messages already queued for this thread's windows are handled.
    for _ in 0..64 {
      // SAFETY: reads and dispatches this thread's own queued messages.
      let more = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool();
      if !more {
        break;
      }
      // SAFETY: the message was just received by this thread.
      unsafe {
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
      }
    }
  }
}

impl Drop for Pill {
  fn drop(&mut self) {
    // SAFETY: this thread created the window and is its only user.
    if let Err(error) = unsafe { DestroyWindow(self.hwnd) } {
      tracing::warn!(operation = "capture_delay", %error, "Countdown pill window was not destroyed");
    }
    unregister_overlay_class(&self.class);
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;

  /// Columns and rows holding the text's bright ink: left, top, right, bottom (inclusive).
  fn ink(bitmap: &mut LayeredBitmap) -> (i32, i32, i32, i32) {
    let (width, height) = (bitmap.width, bitmap.height);
    let pixels = bitmap.pixels_mut().to_vec();
    let mut bounds = (width, height, -1, -1);
    for y in 0..height {
      for x in 0..width {
        let offset = ((y * width + x) * 4) as usize;
        // The panel is dark; text is near white.
        if pixels[offset + 1] > 160 {
          bounds = (
            bounds.0.min(x),
            bounds.1.min(y),
            bounds.2.max(x),
            bounds.3.max(y),
          );
        }
      }
    }
    bounds
  }

  #[test]
  fn the_pill_is_centered_on_its_monitor_and_ignores_scaling_changes() {
    let left = RECT {
      left: -1920,
      top: 0,
      right: 0,
      bottom: 1080,
    };
    assert_eq!(place(left, 300, 1.0), (-1110, 24));
    let scaled = RECT {
      left: 2560,
      top: -200,
      right: 2560 + 2880,
      bottom: 1600,
    };
    assert_eq!(place(scaled, 451, 1.5), (2560 + 1214, -164));
    // A scaling change is handled without the default resize-and-move, and without a window.
    // SAFETY: the WM_DPICHANGED branch returns before any call that would use the window.
    let result = unsafe { pill_proc(HWND::default(), WM_DPICHANGED, WPARAM(0), LPARAM(0)) };
    assert_eq!(result, LRESULT(0));
  }

  /// The pill's window lives and dies with it, drawn but never shown here.
  #[test]
  fn the_pill_draws_each_second_into_its_own_window_and_releases_it() {
    use crate::capture_delay::Presenter;
    let mut pill = Pill::new("en").expect("pill");
    // Marked as shown already, so drawing never puts it on screen.
    pill.shown = true;
    let hwnd = pill.hwnd;
    for remaining in [3, 2, 1] {
      pill.show(remaining).expect("drawn");
      pill.pump();
    }
    // SAFETY: a plain query on the pill's own window.
    let mut rect = RECT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect) }
      .expect("pill rect");
    assert_eq!(
      (rect.bottom - rect.top) as f32,
      (HEIGHT * pill.scale).round()
    );
    assert!(rect.left >= pill.monitor.left && rect.right <= pill.monitor.right);
    // SAFETY: a plain query; the window is visible only if something showed it.
    assert!(!unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd) }.as_bool());
    drop(pill);
    // SAFETY: a plain query; a destroyed handle answers false.
    assert!(!unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool());
    // A second pill on the same thread registers its class again.
    drop(Pill::new("fr").expect("second pill"));
  }

  #[test]
  fn the_countdown_text_is_centered_across_its_pill_at_every_scale() {
    let mut renderer = Renderer::default();
    for scale in [1.0f32, 1.25, 1.5, 2.0] {
      for lang in ["en", "fr", "de", "es", "it"] {
        let text = crate::i18n::native_settings(lang, "capture_delay_countdown")
          .expect("copy")
          .replace("{value}", "3");
        let mut bitmap = render_pill(&mut renderer, &text, scale).expect("pill");
        let (left, top, right, bottom) = ink(&mut bitmap);
        assert!(
          right > left && bottom > top,
          "{lang} {scale}: no text drawn"
        );
        let width = bitmap.width;
        let horizontal = (left - (width - 1 - right)).abs();
        let tolerance = (2.0 * scale).ceil() as i32;
        assert!(
          horizontal <= tolerance,
          "{lang} {scale}: text {left}..{right} in {width} px is off center by {horizontal}"
        );
      }
    }
  }
}
