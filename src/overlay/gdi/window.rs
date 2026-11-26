//! Win32 window creation helpers for overlay types.
//!
//! Each overlay thread calls `register_overlay_class`, creates its window,
//! and then calls `run_message_loop`. On shutdown:
//!   1. DestroyWindow   (triggers WM_DESTROY → GDI cleanup)
//!   2. run_message_loop returns
//!   3. unregister_overlay_class

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::UI::WindowsAndMessaging::{
  CS_HREDRAW, CS_VREDRAW, ClipCursor, CreateWindowExW, DestroyWindow, DispatchMessageW,
  GetMessageW, GetWindowRect, HWND_TOPMOST, IDC_CROSS, LoadCursorW, MSG, PostQuitMessage,
  RegisterClassExW, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowDisplayAffinity, SetWindowPos,
  TranslateMessage, UnregisterClassW, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR, WINDOW_DISPLAY_AFFINITY,
  WNDCLASSEXW, WNDPROC, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::Win32::UI::WindowsAndMessaging::{HCURSOR, HICON};
use windows::core::{PCWSTR, s};

const WINDOWS_10_2004_BUILD: u32 = 19041;
static OBSERVED_TRUE_CAPTURE_EXCLUSION_FAILURE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowsVersion {
  major: u32,
  minor: u32,
  build: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayAffinityMode {
  ExcludeFromCapture,
  MonitorOnly,
}

/// Encode a `&str` as a null-terminated UTF-16 `Vec<u16>`.
fn wide(s: &str) -> Vec<u16> {
  OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn query_windows_version() -> Option<WindowsVersion> {
  type RtlGetVersionFn = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;

  let ntdll = match unsafe { GetModuleHandleW(windows::core::w!("ntdll.dll")) } {
    Ok(handle) => handle,
    Err(error) => {
      tracing::warn!(
        "GetModuleHandleW(ntdll.dll) failed while probing display affinity support: {error}"
      );
      return None;
    },
  };

  let rtl_get_version = match unsafe { GetProcAddress(ntdll, s!("RtlGetVersion")) } {
    Some(proc) => {
      // SAFETY: `proc` points to ntdll!RtlGetVersion with the declared ABI/signature.
      unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, RtlGetVersionFn>(proc) }
    },
    None => {
      tracing::warn!("RtlGetVersion export was not found while probing display affinity support");
      return None;
    },
  };

  let mut info = OSVERSIONINFOW {
    dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
    ..Default::default()
  };

  let status = unsafe { rtl_get_version(&mut info) };
  if status < 0 {
    tracing::warn!(
      status,
      "RtlGetVersion returned failure while probing display affinity support"
    );
    return None;
  }

  Some(WindowsVersion {
    major: info.dwMajorVersion,
    minor: info.dwMinorVersion,
    build: info.dwBuildNumber,
  })
}

fn preferred_display_affinity_mode() -> DisplayAffinityMode {
  static MODE: OnceLock<DisplayAffinityMode> = OnceLock::new();
  *MODE.get_or_init(|| match query_windows_version() {
    Some(version) if supports_exclude_from_capture(version) => {
      tracing::info!(
        major = version.major,
        minor = version.minor,
        build = version.build,
        "Using WDA_EXCLUDEFROMCAPTURE for overlay capture exclusion"
      );
      DisplayAffinityMode::ExcludeFromCapture
    },
    Some(version) => {
      tracing::info!(
        major = version.major,
        minor = version.minor,
        build = version.build,
        "True capture exclusion is unavailable on this Windows build; falling back to WDA_MONITOR"
      );
      DisplayAffinityMode::MonitorOnly
    },
    None => {
      tracing::warn!(
        "Unable to determine Windows build for capture exclusion; trying WDA_EXCLUDEFROMCAPTURE first"
      );
      DisplayAffinityMode::ExcludeFromCapture
    },
  })
}

fn probe_true_capture_exclusion_support() -> bool {
  if !matches!(
    preferred_display_affinity_mode(),
    DisplayAffinityMode::ExcludeFromCapture
  ) {
    return false;
  }

  let class_name = wide("STATIC");
  let title = wide("FlinchCaptureExclusionProbe");
  let hinstance = match unsafe { GetModuleHandleW(None) } {
    Ok(handle) => handle,
    Err(error) => {
      tracing::warn!(
        "GetModuleHandleW failed while probing true capture exclusion support: {error}"
      );
      return false;
    },
  };

  let hwnd = match unsafe {
    CreateWindowExW(
      WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
      PCWSTR(class_name.as_ptr()),
      PCWSTR(title.as_ptr()),
      WS_POPUP,
      0,
      0,
      1,
      1,
      None,
      None,
      Some(hinstance.into()),
      None,
    )
  } {
    Ok(hwnd) => hwnd,
    Err(error) => {
      tracing::warn!(
        "CreateWindowExW failed while probing true capture exclusion support: {error}"
      );
      return false;
    },
  };

  let supports = apply_display_affinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok();
  let _ = unsafe { DestroyWindow(hwnd) };

  if !supports {
    tracing::warn!(
      "SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE) probe failed; treating true capture exclusion as unavailable"
    );
  }

  supports
}

pub fn supports_true_capture_exclusion() -> bool {
  static SUPPORTS_TRUE_CAPTURE_EXCLUSION: OnceLock<bool> = OnceLock::new();

  if OBSERVED_TRUE_CAPTURE_EXCLUSION_FAILURE.load(Ordering::SeqCst) {
    return false;
  }

  *SUPPORTS_TRUE_CAPTURE_EXCLUSION.get_or_init(probe_true_capture_exclusion_support)
}

fn supports_exclude_from_capture(version: WindowsVersion) -> bool {
  version.major > 10
    || (version.major == 10 && (version.minor > 0 || version.build >= WINDOWS_10_2004_BUILD))
}

fn display_affinity_name(affinity: WINDOW_DISPLAY_AFFINITY) -> &'static str {
  if affinity == WDA_EXCLUDEFROMCAPTURE {
    "WDA_EXCLUDEFROMCAPTURE"
  } else if affinity == WDA_MONITOR {
    "WDA_MONITOR"
  } else {
    "UNKNOWN"
  }
}

fn apply_display_affinity(
  hwnd: HWND,
  affinity: WINDOW_DISPLAY_AFFINITY,
) -> windows::core::Result<()> {
  unsafe { SetWindowDisplayAffinity(hwnd, affinity) }
}

/// Returns a class name unique to the calling thread.
/// Uses `{:?}` debug format — `as_u64()` is unstable.
pub fn class_name_for_thread() -> String {
  format!("FlinchOverlay_{:?}", std::thread::current().id())
}

/// Register a `WNDCLASSEXW` with the given class name and `WndProc`.
///
/// Style is `CS_HREDRAW | CS_VREDRAW`, default cursor `IDC_CROSS`.
/// Returns the atom on success.
pub fn register_overlay_class(class_name: &str, wnd_proc: WNDPROC) -> Result<u16, String> {
  let wide_name = wide(class_name);

  let hinstance = unsafe {
    GetModuleHandleW(None)
      .map_err(|e| crate::error::rich_display_context("GetModuleHandleW failed", e))?
  };

  let cursor = unsafe {
    LoadCursorW(None, IDC_CROSS).unwrap_or_else(|e| {
      tracing::warn!("LoadCursorW(IDC_CROSS) failed: {}", e);
      HCURSOR::default()
    })
  };

  let wc = WNDCLASSEXW {
    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
    style: CS_HREDRAW | CS_VREDRAW,
    lpfnWndProc: wnd_proc,
    cbClsExtra: 0,
    cbWndExtra: 0,
    hInstance: hinstance.into(),
    hIcon: HICON::default(),
    hCursor: cursor,
    hbrBackground: HBRUSH::default(),
    lpszMenuName: PCWSTR::null(),
    lpszClassName: PCWSTR(wide_name.as_ptr()),
    hIconSm: HICON::default(),
  };

  let atom = unsafe { RegisterClassExW(&wc) };
  if atom == 0 {
    return Err(crate::error::rich_display_context(
      &format!("RegisterClassExW failed for class '{class_name}'"),
      "registration failed",
    ));
  }

  Ok(atom)
}

/// Unregister a previously registered overlay class.
/// Called AFTER the message loop exits — silently ignores failure.
pub fn unregister_overlay_class(class_name: &str) {
  let wide_name = wide(class_name);

  let hinstance = match unsafe { GetModuleHandleW(None) } {
    Ok(h) => h,
    Err(_) => return,
  };

  // Ignore result — window may already be gone.
  let _ = unsafe { UnregisterClassW(PCWSTR(wide_name.as_ptr()), Some(hinstance.into())) };
}

/// Create a fullscreen overlay window for selection UIs.
///
/// Extended style: `WS_EX_TOPMOST | WS_EX_TOOLWINDOW`
/// Style: `WS_POPUP` (no `WS_EX_LAYERED`, no `WS_EX_NOACTIVATE` — window can receive focus)
pub fn create_selection_window(
  class_name: &str,
  title: &str,
  x: i32,
  y: i32,
  width: i32,
  height: i32,
  create_params: *mut std::ffi::c_void,
) -> Result<HWND, String> {
  let wide_class = wide(class_name);
  let wide_title = wide(title);

  let hinstance = unsafe {
    GetModuleHandleW(None)
      .map_err(|e| crate::error::rich_display_context("GetModuleHandleW failed", e))?
  };

  let hwnd = unsafe {
    CreateWindowExW(
      WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
      PCWSTR(wide_class.as_ptr()),
      PCWSTR(wide_title.as_ptr()),
      WS_POPUP,
      x,
      y,
      width,
      height,
      None,
      None,
      Some(hinstance.into()),
      Some(create_params),
    )
    .map_err(|e| crate::error::rich_display_context("CreateWindowExW (selection) failed", e))?
  };

  Ok(hwnd)
}

/// Create a small layered window for HUD overlays.
///
/// Extended style: `WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED`
/// Style: `WS_POPUP`
pub fn create_hud_window(
  class_name: &str,
  title: &str,
  x: i32,
  y: i32,
  width: i32,
  height: i32,
  create_params: *mut std::ffi::c_void,
) -> Result<HWND, String> {
  let wide_class = wide(class_name);
  let wide_title = wide(title);

  let hinstance = unsafe {
    GetModuleHandleW(None)
      .map_err(|e| crate::error::rich_display_context("GetModuleHandleW failed", e))?
  };

  let hwnd = unsafe {
    CreateWindowExW(
      WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
      PCWSTR(wide_class.as_ptr()),
      PCWSTR(wide_title.as_ptr()),
      WS_POPUP,
      x,
      y,
      width,
      height,
      None,
      None,
      Some(hinstance.into()),
      Some(create_params),
    )
    .map_err(|e| crate::error::rich_display_context("CreateWindowExW (hud) failed", e))?
  };

  Ok(hwnd)
}

/// Standard Win32 message loop. Blocks until `WM_QUIT` is received.
pub fn run_message_loop() {
  let mut msg = MSG::default();
  loop {
    let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
    if ret.0 == -1 {
      tracing::warn!("GetMessageW returned -1 (Win32 error) in overlay message loop");
      break;
    } else if ret.0 == 0 {
      // WM_QUIT
      break;
    }
    unsafe {
      let _ = TranslateMessage(&msg);
      DispatchMessageW(&msg);
    }
  }
}

/// Post `WM_QUIT` to end the calling thread's message loop.
pub fn quit_message_loop() {
  unsafe { PostQuitMessage(0) };
}

/// Exclude `hwnd` from screen capture via `SetWindowDisplayAffinity`.
/// Logs a warning on failure — does not return an error.
pub fn exclude_from_capture(hwnd: HWND) -> bool {
  let preferred = match preferred_display_affinity_mode() {
    DisplayAffinityMode::ExcludeFromCapture => WDA_EXCLUDEFROMCAPTURE,
    DisplayAffinityMode::MonitorOnly => WDA_MONITOR,
  };

  exclude_from_capture_with(
    preferred,
    |affinity| apply_display_affinity(hwnd, affinity),
    || {
      OBSERVED_TRUE_CAPTURE_EXCLUSION_FAILURE.store(true, Ordering::SeqCst);
    },
  )
}

fn exclude_from_capture_with(
  preferred: WINDOW_DISPLAY_AFFINITY,
  apply: impl Fn(WINDOW_DISPLAY_AFFINITY) -> windows::core::Result<()>,
  mark_failure: impl FnOnce(),
) -> bool {
  if let Err(error) = apply(preferred) {
    if preferred == WDA_EXCLUDEFROMCAPTURE {
      mark_failure();
      tracing::warn!(
        "SetWindowDisplayAffinity({}) failed: {}. Retrying with WDA_MONITOR.",
        display_affinity_name(preferred),
        error
      );
      if let Err(fallback_error) = apply(WDA_MONITOR) {
        tracing::warn!(
          "SetWindowDisplayAffinity({}) failed after fallback: {}",
          display_affinity_name(WDA_MONITOR),
          fallback_error
        );
      }
      false
    } else {
      tracing::warn!(
        "SetWindowDisplayAffinity({}) failed: {}",
        display_affinity_name(preferred),
        error
      );
      false
    }
  } else {
    preferred == WDA_EXCLUDEFROMCAPTURE
  }
}

/// Disable window show/hide animations via DWM.
/// Logs a warning on failure — does not return an error.
pub fn disable_window_animations(hwnd: HWND) {
  let value: u32 = 1;
  let result = unsafe {
    DwmSetWindowAttribute(
      hwnd,
      DWMWA_TRANSITIONS_FORCEDISABLED,
      &value as *const u32 as *const std::ffi::c_void,
      std::mem::size_of::<u32>() as u32,
    )
  };
  if let Err(e) = result {
    tracing::warn!("DwmSetWindowAttribute(DWMWA_TRANSITIONS_FORCEDISABLED) failed: {e}");
  }
}

/// Re-assert topmost Z-order for a layered overlay while preserving its bounds.
///
/// This keeps no-activate HUD-style overlays above other app windows that may
/// be raised after the overlay was first created.
pub fn show_window_topmost(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) {
  if let Err(error) = unsafe {
    SetWindowPos(
      hwnd,
      Some(HWND_TOPMOST),
      x,
      y,
      width,
      height,
      SWP_NOACTIVATE | SWP_SHOWWINDOW,
    )
  } {
    tracing::warn!(
      error = %error,
      "SetWindowPos(HWND_TOPMOST) failed while presenting overlay"
    );
  }
}

/// Clip the cursor to the bounding rect of `hwnd`.
/// Logs a warning on failure.
pub fn clip_cursor_to(hwnd: HWND) -> bool {
  let mut rect = windows::Win32::Foundation::RECT::default();
  let ok = unsafe { GetWindowRect(hwnd, &mut rect) };
  if let Err(e) = ok {
    tracing::warn!("GetWindowRect failed in clip_cursor_to: {e}");
    return false;
  }
  if let Err(e) = unsafe { ClipCursor(Some(&rect)) } {
    tracing::warn!("ClipCursor failed: {e}");
    return false;
  }
  true
}

/// Release any active cursor clip.
pub fn release_cursor_clip() {
  if let Err(e) = unsafe { ClipCursor(None) } {
    tracing::warn!("ClipCursor(None) failed: {e}");
  }
}

#[cfg(test)]
mod tests {
  #[test]
  fn recording_visibility_respects_failed_exclusion_and_a_previously_dismissed_warning_without_dialogs()
   {
    use super::*;
    let app = crate::app::context::AppHandle::isolated(
      crate::config::Config {
        recording_visibility_warning_dismissed: true,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let normal = crate::recording_visibility::resolve_presentation_mode();
    assert_eq!(
      crate::recording_visibility::ensure_recording_visibility_policy(&app).unwrap(),
      Some(normal)
    );
    struct Restore(bool);
    impl Drop for Restore {
      fn drop(&mut self) {
        OBSERVED_TRUE_CAPTURE_EXCLUSION_FAILURE.store(self.0, Ordering::SeqCst);
      }
    }
    let _restore = Restore(OBSERVED_TRUE_CAPTURE_EXCLUSION_FAILURE.swap(true, Ordering::SeqCst));
    assert_eq!(
      crate::recording_visibility::resolve_presentation_mode(),
      crate::recording_visibility::RecordingPresentationMode::Silent
    );
    assert_eq!(
      crate::recording_visibility::ensure_recording_visibility_policy(&app).unwrap(),
      Some(crate::recording_visibility::RecordingPresentationMode::Silent)
    );
  }
  #[test]
  fn exclusion_fallback_is_bounded_and_never_reports_monitor_only_as_true_exclusion() {
    use super::*;
    use std::cell::{Cell, RefCell};
    for preferred in [WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR] {
      for failed_calls in 0..=2 {
        let calls = RefCell::new(Vec::new());
        let failed = Cell::new(false);
        let result = exclude_from_capture_with(
          preferred,
          |affinity| {
            calls.borrow_mut().push(affinity);
            if calls.borrow().len() <= failed_calls {
              Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_ACCESSDENIED,
              ))
            } else {
              Ok(())
            }
          },
          || failed.set(true),
        );
        assert_eq!(
          result,
          preferred == WDA_EXCLUDEFROMCAPTURE && failed_calls == 0
        );
        assert_eq!(
          failed.get(),
          preferred == WDA_EXCLUDEFROMCAPTURE && failed_calls > 0
        );
        assert_eq!(
          calls.borrow().len(),
          if preferred == WDA_EXCLUDEFROMCAPTURE && failed_calls > 0 {
            2
          } else {
            1
          }
        );
        if calls.borrow().len() == 2 {
          assert_eq!(calls.borrow()[1], WDA_MONITOR);
        }
      }
    }
  }

  #[test]
  fn capture_exclusion_probe_and_animation_policy_never_expose_the_owned_window() {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowDisplayAffinity, IsWindowVisible};
    let version = query_windows_version().expect("Windows version metadata");
    assert!(version.major >= 10);
    // This production support probe creates and destroys only its own hidden window.
    let _ = probe_true_capture_exclusion_support();
    let class = wide("STATIC");
    // SAFETY: own a hidden fixture; neither this test nor these policy helpers show it.
    let hwnd = unsafe {
      CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        PCWSTR(class.as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        8,
        8,
        None,
        None,
        None,
        None,
      )
    }
    .unwrap();
    disable_window_animations(hwnd);
    let supported = exclude_from_capture(hwnd);
    let mut affinity = 0;
    // SAFETY: valid owned HWND and writable output; metadata only.
    unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }.unwrap();
    assert_eq!(supported, affinity == WDA_EXCLUDEFROMCAPTURE.0);
    assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
    unsafe { DestroyWindow(hwnd) }.unwrap();
  }
  use super::{
    DisplayAffinityMode, WindowsVersion, preferred_display_affinity_mode,
    supports_exclude_from_capture,
  };

  #[test]
  fn windows_10_2004_supports_true_exclusion() {
    assert!(supports_exclude_from_capture(WindowsVersion {
      major: 10,
      minor: 0,
      build: 19041,
    }));
  }

  #[test]
  fn older_windows_10_falls_back_to_monitor_only() {
    assert!(!supports_exclude_from_capture(WindowsVersion {
      major: 10,
      minor: 0,
      build: 19040,
    }));
  }

  #[test]
  fn windows_11_supports_true_exclusion() {
    assert!(supports_exclude_from_capture(WindowsVersion {
      major: 10,
      minor: 0,
      build: 22000,
    }));
  }

  #[test]
  fn cached_affinity_mode_is_valid() {
    let mode = preferred_display_affinity_mode();
    assert!(matches!(
      mode,
      DisplayAffinityMode::ExcludeFromCapture | DisplayAffinityMode::MonitorOnly
    ));
  }
}
