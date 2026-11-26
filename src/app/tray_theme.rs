//! Opt native popup menus into the Windows app theme (including high contrast).
//! These UxTheme exports are also used by Microsoft's PowerToys/ZoomIt.
use anyhow::{Context, anyhow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{WM_NCDESTROY, WM_SETTINGCHANGE, WM_THEMECHANGED};
use windows::core::{PCSTR, s, w};

pub(super) fn watch_system_theme(window: *mut std::ffi::c_void) -> anyhow::Result<()> {
  let hwnd = HWND(window);
  // SAFETY: this is the live tray window, called on its owning Slint thread.
  if !unsafe { SetWindowSubclass(hwnd, Some(theme_proc), 1, 0) }.as_bool() {
    return Err(anyhow!("Cannot attach tray theme observer"));
  }
  Ok(())
}

unsafe extern "system" fn theme_proc(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
  id: usize,
  _data: usize,
) -> LRESULT {
  if matches!(msg, WM_SETTINGCHANGE | WM_THEMECHANGED)
    && let Err(error) = enable_system_theme()
  {
    tracing::warn!(%error, "Cannot refresh Windows tray menu theme");
  }
  if msg == WM_NCDESTROY {
    // SAFETY: Windows is destroying this window on its owner thread.
    if !unsafe { RemoveWindowSubclass(hwnd, Some(theme_proc), id) }.as_bool() {
      tracing::warn!("Cannot remove tray theme observer");
    }
  }
  // SAFETY: forward the original message unchanged; this observer owns no window state.
  unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

pub(super) fn enable_system_theme() -> anyhow::Result<()> {
  // Ordinal 135 changed signature in Windows 10 1903; never call it on older builds.
  // SAFETY: ntdll is loaded for the process lifetime; the export name is terminated.
  let ntdll = unsafe { GetModuleHandleW(w!("ntdll.dll")) }?;
  // SAFETY: ntdll is a valid module and the export name is terminated.
  let query = unsafe { GetProcAddress(ntdll, s!("RtlGetVersion")) }
    .context("RtlGetVersion unavailable for tray theme")?;
  type QueryVersion = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;
  // SAFETY: RtlGetVersion has this ABI and signature.
  let query =
    unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, QueryVersion>(query) };
  let mut version = OSVERSIONINFOW {
    dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
    ..Default::default()
  };
  // SAFETY: version is writable and has its required size initialized.
  if unsafe { query(&mut version) } < 0 {
    return Err(anyhow!("Cannot query Windows version for tray theme"));
  }
  if version.dwMajorVersion < 10 || version.dwBuildNumber < 18362 {
    return Err(anyhow!(
      "Windows tray theme requires Windows 10 1903 or newer"
    ));
  }
  // SAFETY: UxTheme is loaded by the native UI; obtaining its handle adds no ownership.
  let theme =
    unsafe { GetModuleHandleW(w!("uxtheme.dll")) }.context("UxTheme unavailable for tray theme")?;
  // SAFETY: the module is valid; low-valued pointers encode export ordinals, not strings.
  let preferred = unsafe { GetProcAddress(theme, PCSTR(135usize as *const u8)) }
    .context("SetPreferredAppMode unavailable")?;
  // SAFETY: the module is valid and 136 is the FlushMenuThemes ordinal.
  let flush = unsafe { GetProcAddress(theme, PCSTR(136usize as *const u8)) }
    .context("FlushMenuThemes unavailable")?;
  // SAFETY: the module is valid and 104 refreshes the cached Windows color policy.
  let refresh = unsafe { GetProcAddress(theme, PCSTR(104usize as *const u8)) }
    .context("RefreshImmersiveColorPolicyState unavailable")?;
  type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
  type FlushMenuThemes = unsafe extern "system" fn();
  // SAFETY: on the checked Windows builds these exports have the specified ABIs.
  let preferred = unsafe {
    std::mem::transmute::<unsafe extern "system" fn() -> isize, SetPreferredAppMode>(preferred)
  };
  // SAFETY: ordinal 136 has the specified no-argument ABI.
  let flush =
    unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, FlushMenuThemes>(flush) };
  // SAFETY: ordinal 104 has the same no-argument, no-return ABI.
  let refresh = unsafe {
    std::mem::transmute::<unsafe extern "system" fn() -> isize, FlushMenuThemes>(refresh)
  };
  // SAFETY: both exports remain loaded, and AllowDark (1) follows the OS preference
  // rather than forcing dark mode. Windows retains high-contrast menu rendering.
  unsafe {
    refresh();
    preferred(1);
    flush();
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::{
    Foundation::FreeLibrary,
    System::LibraryLoader::LoadLibraryW,
    UI::{Shell::GetWindowSubclass, WindowsAndMessaging::*},
  };

  #[test]
  fn process_theme_observer_attaches_refreshes_and_releases_only_its_hidden_window() {
    // The setting is process-local. No system preference, tray icon, visible window or focus changes.
    let module = unsafe { LoadLibraryW(w!("uxtheme.dll")) }.unwrap();
    struct Module(windows::Win32::Foundation::HMODULE);
    impl Drop for Module {
      fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) }.unwrap();
      }
    }
    let _module = Module(module);
    enable_system_theme().unwrap();
    assert!(watch_system_theme(std::ptr::null_mut()).is_err());
    let hwnd = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        0,
        0,
        32,
        24,
        None,
        None,
        None,
        None,
      )
    }
    .unwrap();
    watch_system_theme(hwnd.0).unwrap();
    let mut data = usize::MAX;
    assert!(unsafe { GetWindowSubclass(hwnd, Some(theme_proc), 1, Some(&mut data)) }.as_bool());
    assert_eq!(data, 0);
    for message in [WM_THEMECHANGED, WM_SETTINGCHANGE] {
      unsafe {
        SendMessageW(hwnd, message, None, None);
      }
      assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
      assert!(unsafe { GetWindowSubclass(hwnd, Some(theme_proc), 1, Some(&mut data)) }.as_bool());
    }
    unsafe { DestroyWindow(hwnd) }.unwrap();
    assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
  }
}
