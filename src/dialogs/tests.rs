use super::*;
use windows::Win32::{Foundation::E_ACCESSDENIED, UI::Controls::*};

#[test]
fn dialog_marshalling_keeps_unicode_buttons_defaults_and_checkbox_alive_through_native_call() {
  for custom in [false, true] {
    let buttons = [
      TaskDialogButton {
        id: 41,
        label: "Garder 日本語",
      },
      TaskDialogButton {
        id: 42,
        label: "Annuler 🙂",
      },
    ];
    let mut options = TaskDialogOptions::new("Flinch — Café", "Conserver ?", "Détail 日本語");
    if custom {
      options = options
        .with_icon(TaskDialogIcon::Warning)
        .with_common_buttons(TaskDialogCommonButtons::OkCancel)
        .with_custom_buttons(&buttons)
        .with_default_button(42)
        .with_verification_text("Ne plus demander");
    }
    let result = show_task_dialog_with(&options, |config, pressed, checked| {
      // SAFETY: the synchronous adapter owns every marshalled string and button array.
      unsafe {
        assert_eq!(
          { config.pszWindowTitle }.to_string().unwrap(),
          options.window_title
        );
        assert_eq!(
          { config.pszMainInstruction }.to_string().unwrap(),
          options.main_instruction
        );
        assert_eq!({ config.pszContent }.to_string().unwrap(), options.content);
        assert_eq!({ config.cButtons }, if custom { 2 } else { 0 });
        assert!({ config.dwFlags }.contains(TDF_ALLOW_DIALOG_CANCELLATION));
        if custom {
          let count = { config.cButtons };
          let pointer = config.pButtons;
          let marshalled = std::slice::from_raw_parts(pointer, count as usize);
          for (actual, expected) in marshalled.iter().zip(&buttons) {
            let id = actual.nButtonID;
            assert_eq!(id, expected.id);
            assert_eq!(
              { actual.pszButtonText }.to_string().unwrap(),
              expected.label
            );
          }
          assert_eq!(
            { config.pszVerificationText }.to_string().unwrap(),
            "Ne plus demander"
          );
          let default = config.nDefaultButton;
          assert_eq!(default, 42);
          assert!({ config.dwCommonButtons }.contains(TDCBF_CANCEL_BUTTON | TDCBF_OK_BUTTON));
          assert_eq!({ config.Anonymous1.pszMainIcon }, TD_WARNING_ICON);
        } else {
          assert!({ config.pszVerificationText }.is_null());
          assert!({ config.Anonymous1.pszMainIcon }.is_null());
        }
      }
      *pressed = if custom { 42 } else { TASK_DIALOG_OK_ID };
      *checked = custom.into();
      Ok(())
    })
    .unwrap();
    assert_eq!(result.button_id, if custom { 42 } else { 1 });
    assert_eq!(result.verification_checked, custom);
    let failure = show_task_dialog_with(&options, |_, _, _| {
      Err(windows::core::Error::from_hresult(E_ACCESSDENIED))
    })
    .unwrap_err();
    assert_eq!(failure.code(), E_ACCESSDENIED);
  }
}
