// Cursor icon helpers for the shared selection overlay — GDI/Win32 path

use super::state::{Handle as ResizeHandle, HitResult};

use windows::Win32::UI::WindowsAndMessaging::{
  HCURSOR, IDC_CROSS, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, LoadCursorW,
};
use windows::core::PCWSTR;

/// Returns the Win32 cursor ID constant for the given hit/idle state.
/// Extracted for testability (no Win32 call).
fn cursor_id_for_hit(hit: HitResult, is_idle: bool) -> PCWSTR {
  if is_idle {
    IDC_CROSS
  } else {
    match hit {
      // A press outside a confirmed selection starts a new one.
      HitResult::Outside => IDC_CROSS,
      HitResult::Inside => IDC_SIZEALL,
      HitResult::Handle(h) => match h {
        ResizeHandle::NW | ResizeHandle::SE => IDC_SIZENWSE,
        ResizeHandle::NE | ResizeHandle::SW => IDC_SIZENESW,
        ResizeHandle::N | ResizeHandle::S => IDC_SIZENS,
        ResizeHandle::W | ResizeHandle::E => IDC_SIZEWE,
      },
    }
  }
}

/// Returns the appropriate Win32 HCURSOR for the current overlay state.
///
/// Call `SetCursor(cursor_for_hit(hit, is_idle))` from a `WM_SETCURSOR` handler.
/// `is_idle` should be `true` while no selection has been confirmed yet.
pub fn cursor_for_hit(hit: HitResult, is_idle: bool) -> HCURSOR {
  unsafe { LoadCursorW(None, cursor_id_for_hit(hit, is_idle)).unwrap_or_default() }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn idle_always_crosshair() {
    assert_eq!(cursor_id_for_hit(HitResult::Outside, true), IDC_CROSS);
    assert_eq!(cursor_id_for_hit(HitResult::Inside, true), IDC_CROSS);
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::NW), true),
      IDC_CROSS
    );
  }

  #[test]
  fn outside_confirmed_uses_crosshair_because_a_press_there_reselects() {
    assert_eq!(cursor_id_for_hit(HitResult::Outside, false), IDC_CROSS);
  }

  #[test]
  fn inside_confirmed_uses_move() {
    assert_eq!(cursor_id_for_hit(HitResult::Inside, false), IDC_SIZEALL);
  }

  #[test]
  fn handles_use_resize_cursors() {
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::NW), false),
      IDC_SIZENWSE
    );
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::SE), false),
      IDC_SIZENWSE
    );
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::NE), false),
      IDC_SIZENESW
    );
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::SW), false),
      IDC_SIZENESW
    );
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::N), false),
      IDC_SIZENS
    );
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::S), false),
      IDC_SIZENS
    );
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::W), false),
      IDC_SIZEWE
    );
    assert_eq!(
      cursor_id_for_hit(HitResult::Handle(ResizeHandle::E), false),
      IDC_SIZEWE
    );
  }
}
