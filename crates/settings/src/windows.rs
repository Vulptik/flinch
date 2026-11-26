use crate::ui::{
  SettingsWindow, TextPanelWindow, TrayChoiceWindow, TrayMenuWindow, TrayRecentWindow,
};
use slint::ComponentHandle;

#[path = "../../../windows/slint_chrome.rs"]
mod chrome;
pub use chrome::ChromeHandler;

#[cfg(windows)]
pub fn configure_fonts(window: &SettingsWindow) -> Result<(), Box<dyn std::error::Error>> {
  window
    .global::<crate::ui::Interaction>()
    .set_motion_enabled(motion_enabled());
  let (text, small, display) = font_families()?;
  let families = window.global::<crate::ui::Typography>();
  families.set_text_family(text.into());
  families.set_small_family(small.into());
  families.set_display_family(display.into());
  Ok(())
}

/// Slint globals are per window, so each window receives the resolved families.
#[cfg(windows)]
pub fn configure_text_panel(window: &TextPanelWindow) -> Result<(), Box<dyn std::error::Error>> {
  window
    .global::<crate::ui::Interaction>()
    .set_motion_enabled(motion_enabled());
  let (text, small, display) = font_families()?;
  let families = window.global::<crate::ui::Typography>();
  families.set_text_family(text.into());
  families.set_small_family(small.into());
  families.set_display_family(display.into());
  Ok(())
}

/// Both tray menu windows use the settings typography; motion stays off in menus.
#[cfg(windows)]
pub fn configure_tray_menu(
  menu: &TrayMenuWindow,
  recent: &TrayRecentWindow,
  choice: &TrayChoiceWindow,
) -> Result<(), Box<dyn std::error::Error>> {
  let (text, small, display) = font_families()?;
  let families = menu.global::<crate::ui::Typography>();
  families.set_text_family(text.into());
  families.set_small_family(small.into());
  families.set_display_family(display.into());
  let families = recent.global::<crate::ui::Typography>();
  families.set_text_family(text.into());
  families.set_small_family(small.into());
  families.set_display_family(display.into());
  let families = choice.global::<crate::ui::Typography>();
  families.set_text_family(text.into());
  families.set_small_family(small.into());
  families.set_display_family(display.into());
  Ok(())
}

#[cfg(windows)]
fn motion_enabled() -> bool {
  use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
  };
  let mut animations = windows::core::BOOL(0);
  // SAFETY: Windows writes one BOOL on the UI owner thread; no preferences are modified.
  match unsafe {
    SystemParametersInfoW(
      SPI_GETCLIENTAREAANIMATION,
      0,
      Some((&mut animations as *mut windows::core::BOOL).cast()),
      SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
  } {
    Ok(()) => animations.as_bool(),
    Err(error) => {
      eprintln!("Settings animation preference unavailable; transitions disabled: {error}");
      false
    },
  }
}

#[cfg(windows)]
fn font_families() -> Result<(&'static str, &'static str, &'static str), Box<dyn std::error::Error>>
{
  use std::path::PathBuf;
  use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
  let mut buffer = [0u16; 32768];
  // SAFETY: Windows writes at most the slice capacity; the UTF-16 result is
  // converted only after its returned length has been checked.
  let length = unsafe { GetWindowsDirectoryW(Some(&mut buffer)) } as usize;
  if length == 0 || length >= buffer.len() {
    return Err("Cannot resolve the Windows font directory".into());
  }
  let fonts = PathBuf::from(String::from_utf16(&buffer[..length])?).join("Fonts");
  if fonts.join("SegUIVar.ttf").is_file() {
    Ok((
      "Segoe UI Variable Text",
      "Segoe UI Variable Small",
      "Segoe UI Variable Display",
    ))
  } else if fonts.join("segoeui.ttf").is_file() {
    // Windows 10 is an explicitly supported typography variant, not an
    // implicit renderer fallback. No Microsoft fonts are bundled.
    eprintln!("Segoe UI Variable unavailable; using the supported Windows 10 Segoe UI typography.");
    Ok(("Segoe UI", "Segoe UI", "Segoe UI"))
  } else {
    Err("Required Windows Segoe UI font is unavailable".into())
  }
}

#[cfg(not(windows))]
pub fn configure_fonts(_: &SettingsWindow) -> Result<(), Box<dyn std::error::Error>> {
  Err("Flinch settings requires Windows".into())
}

#[cfg(not(windows))]
pub fn configure_tray_menu(
  _: &TrayMenuWindow,
  _: &TrayRecentWindow,
  _: &TrayChoiceWindow,
) -> Result<(), Box<dyn std::error::Error>> {
  Err("Flinch settings requires Windows".into())
}

#[cfg(not(windows))]
pub fn configure_text_panel(_: &TextPanelWindow) -> Result<(), Box<dyn std::error::Error>> {
  Err("Flinch settings requires Windows".into())
}
