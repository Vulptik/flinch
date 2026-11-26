pub struct ChromeHandler {
  initialized: std::collections::HashSet<slint::winit_030::winit::window::WindowId>,
  failure: std::rc::Rc<std::cell::RefCell<Option<String>>>,
}

impl ChromeHandler {
  pub fn new(failure: std::rc::Rc<std::cell::RefCell<Option<String>>>) -> Self {
    Self {
      initialized: std::collections::HashSet::new(),
      failure,
    }
  }
}

impl slint::winit_030::CustomApplicationHandler for ChromeHandler {
  fn window_event(
    &mut self,
    event_loop: &slint::winit_030::winit::event_loop::ActiveEventLoop,
    window_id: slint::winit_030::winit::window::WindowId,
    winit_window: Option<&slint::winit_030::winit::window::Window>,
    _slint_window: Option<&slint::Window>,
    event: &slint::winit_030::winit::event::WindowEvent,
  ) -> slint::winit_030::EventResult {
    use slint::winit_030::{EventResult, winit::event::WindowEvent};
    if matches!(
      event,
      WindowEvent::ScaleFactorChanged { .. } | WindowEvent::Destroyed
    ) {
      self.initialized.remove(&window_id);
    }
    if matches!(event, WindowEvent::RedrawRequested)
      && !self.initialized.contains(&window_id)
      && let Some(window) = winit_window
      && !window.is_decorated()
    {
      // Slint's startup sizing has now created the native window. Apply frame
      // attributes once before paint, and again after DPI changes. No sleeps,
      // polling loops or cross-thread HWND access are involved.
      match configure_chrome(window) {
        Ok(()) => {
          self.initialized.insert(window_id);
        },
        Err(error) => {
          *self.failure.borrow_mut() = Some(error.to_string());
          event_loop.exit();
        },
      }
    }
    EventResult::Propagate
  }
}

#[cfg(windows)]
fn configure_chrome(
  window: &slint::winit_030::winit::window::Window,
) -> Result<(), Box<dyn std::error::Error>> {
  use raw_window_handle::{HasWindowHandle, RawWindowHandle};
  let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
    return Err("Slint did not create a Windows HWND".into());
  };
  configure_hwnd(windows::Win32::Foundation::HWND(
    handle.hwnd.get() as *mut std::ffi::c_void
  ))
}

#[cfg(windows)]
fn configure_hwnd(
  hwnd: windows::Win32::Foundation::HWND,
) -> Result<(), Box<dyn std::error::Error>> {
  use windows::Win32::{
    Foundation::{SetLastError, WIN32_ERROR},
    Graphics::Dwm::{
      DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY, DWMWA_WINDOW_CORNER_PREFERENCE,
      DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
    },
    UI::WindowsAndMessaging::{
      GWL_STYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
      SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos,
    },
  };
  // SAFETY: The component is alive and its HWND is only accessed on the Slint
  // owning thread. Attribute pointers refer to initialized stack values for
  // the duration of each synchronous call. Styles preserve existing bits.
  unsafe {
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
    if style == 0 {
      return Err(windows::core::Error::from_thread().into());
    }
    SetLastError(WIN32_ERROR(0));
    if SetWindowLongPtrW(hwnd, GWL_STYLE, style | 0x00C40000) == 0 {
      return Err(windows::core::Error::from_thread().into());
    }
    SetWindowPos(
      hwnd,
      None,
      0,
      0,
      0,
      0,
      SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
    )?;
    let enabled = 2i32;
    DwmSetWindowAttribute(hwnd, DWMWA_NCRENDERING_POLICY, &enabled as *const _ as _, 4)?;
    let rounded = 2i32;
    if let Err(error) = DwmSetWindowAttribute(
      hwnd,
      DWMWA_WINDOW_CORNER_PREFERENCE,
      &rounded as *const _ as _,
      4,
    ) {
      // These two attributes were introduced in Windows 11. Windows 10
      // intentionally retains square corners; other failures remain fatal.
      if error.code() != windows::core::HRESULT(0x80070057u32 as i32) {
        return Err(error.into());
      }
      eprintln!(
        "Windows compositor does not support rounded window corners; using square corners."
      );
    }
    let no_border = 0xFFFFFFFEu32;
    if let Err(error) =
      DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &no_border as *const _ as _, 4)
    {
      if error.code() != windows::core::HRESULT(0x80070057u32 as i32) {
        return Err(error.into());
      }
      eprintln!("Windows compositor does not support window border-color control.");
    }
    let margins = windows::Win32::UI::Controls::MARGINS {
      cxLeftWidth: 1,
      cxRightWidth: 1,
      cyTopHeight: 1,
      cyBottomHeight: 1,
    };
    DwmExtendFrameIntoClientArea(hwnd, &margins)?;
  }
  Ok(())
}

#[cfg(all(test, windows))]
mod quiet_tests {
  use super::*;
  use windows::{Win32::UI::WindowsAndMessaging::*, core::w};
  #[test]
  fn native_chrome_preserves_window_styles_and_never_shows_its_hidden_owner() {
    // SAFETY: thread-owned hidden STATIC fixture, without activation or an event loop.
    let hwnd = unsafe {
      CreateWindowExW(
        WS_EX_NOACTIVATE,
        w!("STATIC"),
        w!(""),
        WS_POPUP,
        0,
        0,
        320,
        240,
        None,
        None,
        None,
        None,
      )
    }
    .unwrap();
    let result = configure_hwnd(hwnd);
    // SAFETY: query and destroy only the fixture's owned hidden HWND.
    unsafe {
      let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
      assert_eq!(style & 0x00C40000, 0x00C40000);
      assert_ne!(style & WS_POPUP.0 as isize, 0);
      assert_ne!(
        GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize,
        0
      );
      assert!(!IsWindowVisible(hwnd).as_bool());
      DestroyWindow(hwnd).unwrap();
    }
    result.unwrap();
    assert!(
      configure_hwnd(hwnd).is_err(),
      "destroyed windows must not be accepted"
    );
  }
}

#[cfg(not(windows))]
pub fn configure_chrome(
  _: &slint::winit_030::winit::window::Window,
) -> Result<(), Box<dyn std::error::Error>> {
  Err("Flinch window chrome requires Windows".into())
}
