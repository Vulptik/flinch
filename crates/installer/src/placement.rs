//! Center initial placement in monitor work coordinates, including negative origins.
pub fn centered_position(
  work: (i32, i32, i32, i32),
  size: (i32, i32),
) -> Result<(i32, i32), String> {
  let (left, top, right, bottom) = work;
  if right <= left || bottom <= top || size.0 <= 0 || size.1 <= 0 {
    return Err("Invalid installer monitor work area or window size".into());
  }
  let x = i64::from(left) + ((i64::from(right) - i64::from(left) - i64::from(size.0)) / 2).max(0);
  let y = i64::from(top) + ((i64::from(bottom) - i64::from(top) - i64::from(size.1)) / 2).max(0);
  Ok((x as i32, y as i32))
}

#[cfg(feature = "ui")]
pub fn center(window: &slint::Window) -> Result<(), String> {
  use windows::Win32::{
    Foundation::POINT,
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
    UI::{
      HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
      WindowsAndMessaging::GetCursorPos,
    },
  };
  let mut pointer = POINT::default();
  // SAFETY: All queries and Slint placement run on the UI thread with initialized outputs.
  unsafe { GetCursorPos(&mut pointer) }
    .map_err(|error| format!("Read installer monitor: {error}"))?;
  // SAFETY: The point is initialized and the documented nearest-monitor flag is used.
  let monitor = unsafe { MonitorFromPoint(pointer, MONITOR_DEFAULTTONEAREST) };
  let mut info = MONITORINFO {
    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
    ..Default::default()
  };
  // SAFETY: The monitor handle is queried immediately and info has the required size.
  if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
    return Err(format!(
      "Read installer monitor work area: {}",
      windows::core::Error::from_thread()
    ));
  }
  let (mut dpi_x, mut dpi_y) = (0, 0);
  // SAFETY: Both DPI outputs are writable and the monitor was resolved on this thread.
  unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
    .map_err(|error| format!("Read installer monitor DPI: {error}"))?;
  let size = (
    (640.0 * dpi_x as f64 / 96.0).round() as i32,
    (420.0 * dpi_y as f64 / 96.0).round() as i32,
  );
  let work = info.rcWork;
  let (x, y) = centered_position((work.left, work.top, work.right, work.bottom), size)?;
  // Place before show so there is no visible jump, and never recenter after user dragging.
  window.set_position(slint::PhysicalPosition::new(x, y));
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn respects_work_area_negative_monitors_and_scaled_size() {
    assert_eq!(
      centered_position((0, 0, 1920, 1040), (640, 420)),
      Ok((640, 310))
    );
    assert_eq!(
      centered_position((-2560, 40, 0, 1440), (960, 630)),
      Ok((-1760, 425))
    );
    assert_eq!(
      centered_position((1920, 0, 3840, 1040), (1280, 840)),
      Ok((2240, 100))
    );
    assert_eq!(
      centered_position((-800, -600, 0, 0), (1280, 840)),
      Ok((-800, -600))
    );
    assert!(centered_position((0, 0, 0, 100), (640, 420)).is_err());
  }
}
