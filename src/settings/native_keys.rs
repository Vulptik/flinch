//! Key capture on Slint's window owner; platform shortcut operations stay on workers.
use flinch_settings::{
  slint::winit_030::{
    CustomApplicationHandler, EventResult,
    winit::{
      event::{ElementState, WindowEvent},
      event_loop::ActiveEventLoop,
      keyboard::{ModifiersState, PhysicalKey},
      window::{Window, WindowId},
    },
  },
  windows::ChromeHandler,
};
pub(super) struct Handler {
  chrome: ChromeHandler,
  modifiers: ModifiersState,
  pending: Option<(PhysicalKey, String)>,
}
impl Handler {
  pub(super) fn new(chrome: ChromeHandler) -> Self {
    Self {
      chrome,
      modifiers: ModifiersState::empty(),
      pending: None,
    }
  }
}
impl CustomApplicationHandler for Handler {
  fn window_event(
    &mut self,
    event_loop: &ActiveEventLoop,
    id: WindowId,
    window: Option<&Window>,
    slint_window: Option<&flinch_settings::slint::Window>,
    event: &WindowEvent,
  ) -> EventResult {
    if matches!(event, WindowEvent::Focused(_)) {
      crate::app::tray_menu::focus_changed();
    }
    match event {
      WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
      WindowEvent::Focused(false) | WindowEvent::Destroyed => {
        self.pending = None;
        self.modifiers = ModifiersState::empty();
        super::native_host::cancel_current_capture();
      },
      WindowEvent::MouseInput {
        state: ElementState::Released,
        button: flinch_settings::slint::winit_030::winit::event::MouseButton::Right,
        ..
      } if super::native_host::capture_busy() => {
        self.pending = None;
        super::native_host::capture_key(String::new());
        return EventResult::PreventDefault;
      },
      WindowEvent::KeyboardInput { event, .. } if super::native_host::capture_busy() => {
        if !event.repeat
          && let PhysicalKey::Code(code) = event.physical_key
        {
          use flinch_settings::slint::winit_030::winit::{
            keyboard::Key, platform::modifier_supplement::KeyEventExtModifierSupplement,
          };
          let physical = format!("{code:?}");
          let key = match event.key_without_modifiers() {
            _ if physical.starts_with("Numpad") => physical,
            Key::Character(text) if text.len() == 1 && text.as_bytes()[0].is_ascii_alphabetic() => {
              format!("Key{}", text.to_ascii_uppercase())
            },
            Key::Character(text) if text.len() == 1 && text.as_bytes()[0].is_ascii_digit() => {
              format!("Digit{text}")
            },
            _ => format!("{code:?}"),
          };
          if event.state == ElementState::Pressed {
            if key == "Escape" {
              self.pending = None;
              super::native_host::cancel_current_capture();
            } else if let Some(key) = shortcut(
              &key,
              self.modifiers.control_key(),
              self.modifiers.alt_key(),
              self.modifiers.shift_key(),
              self.modifiers.super_key(),
            ) {
              self.pending = Some((event.physical_key, key));
            }
          } else if self
            .pending
            .as_ref()
            .is_some_and(|(key, _)| *key == event.physical_key)
            && let Some((_, key)) = self.pending.take()
          {
            super::native_host::capture_key(key);
          }
        }
        return EventResult::PreventDefault;
      },
      _ => {},
    }
    self
      .chrome
      .window_event(event_loop, id, window, slint_window, event)
  }
}
fn shortcut(code: &str, ctrl: bool, alt: bool, shift: bool, super_key: bool) -> Option<String> {
  let key = if let Some(letter) = code.strip_prefix("Key") {
    letter
  } else if let Some(digit) = code.strip_prefix("Digit") {
    digit
  } else {
    match code {
      "Space" | "Enter" | "Tab" | "Backspace" | "Delete" | "Home" | "End" | "PageUp"
      | "PageDown" | "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" | "PrintScreen"
      | "Insert" | "Backquote" | "Backslash" | "BracketLeft" | "BracketRight" | "Comma"
      | "Equal" | "Minus" | "Period" | "Quote" | "Semicolon" | "Slash" | "Numpad0" | "Numpad1"
      | "Numpad2" | "Numpad3" | "Numpad4" | "Numpad5" | "Numpad6" | "Numpad7" | "Numpad8"
      | "Numpad9" | "NumpadAdd" | "NumpadDecimal" | "NumpadDivide" | "NumpadEnter"
      | "NumpadEqual" | "NumpadMultiply" | "NumpadSubtract" => code,
      value
        if value
          .strip_prefix('F')
          .and_then(|number| number.parse::<u8>().ok())
          .is_some_and(|number| (1..=24).contains(&number)) =>
      {
        value
      },
      _ => return None,
    }
  };
  let mut parts = Vec::with_capacity(5);
  if ctrl {
    parts.push("Ctrl");
  }
  if alt {
    parts.push("Alt");
  }
  if shift {
    parts.push("Shift");
  }
  if super_key {
    parts.push("Command");
  }
  parts.push(key);
  Some(parts.join("+"))
}
#[cfg(test)]
mod tests {
  use super::shortcut;
  #[test]
  fn mapping_preserves_modifiers_and_keeps_delete_as_a_bindable_key() {
    assert_eq!(
      shortcut("KeyG", true, false, true, false).as_deref(),
      Some("Ctrl+Shift+G")
    );
    assert_eq!(
      shortcut("Delete", false, false, false, false).as_deref(),
      Some("Delete")
    );
    assert_eq!(
      shortcut("Delete", true, false, false, false).as_deref(),
      Some("Ctrl+Delete")
    );
    assert_eq!(shortcut("ShiftLeft", false, false, true, false), None);
    assert_eq!(
      shortcut("F24", false, true, false, false).as_deref(),
      Some("Alt+F24")
    );
    assert_eq!(shortcut("F25", false, false, false, false), None);
    for key in [
      "Semicolon",
      "Slash",
      "BracketLeft",
      "Numpad1",
      "NumpadAdd",
      "NumpadEnter",
    ] {
      let binding = shortcut(key, true, false, false, false).expect("mapped key");
      assert!(
        binding.parse::<crate::app::hotkeys::Shortcut>().is_ok(),
        "{binding}"
      );
    }
  }
}
