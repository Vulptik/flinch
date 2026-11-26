#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskDialogIcon {
  None,
  Warning,
}

pub const TASK_DIALOG_OK_ID: i32 = 1;
#[cfg(all(test, windows))]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskDialogCommonButtons {
  None,
  OkCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskDialogButton<'a> {
  pub id: i32,
  pub label: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskDialogOptions<'a> {
  pub window_title: &'a str,
  pub main_instruction: &'a str,
  pub content: &'a str,
  pub icon: TaskDialogIcon,
  pub common_buttons: TaskDialogCommonButtons,
  pub custom_buttons: &'a [TaskDialogButton<'a>],
  pub default_button: i32,
  pub verification_text: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskDialogResult {
  pub button_id: i32,
  pub verification_checked: bool,
}

impl<'a> TaskDialogOptions<'a> {
  pub fn new(window_title: &'a str, main_instruction: &'a str, content: &'a str) -> Self {
    Self {
      window_title,
      main_instruction,
      content,
      icon: TaskDialogIcon::None,
      common_buttons: TaskDialogCommonButtons::None,
      custom_buttons: &[],
      default_button: TASK_DIALOG_OK_ID,
      verification_text: None,
    }
  }

  pub fn with_icon(mut self, icon: TaskDialogIcon) -> Self {
    self.icon = icon;
    self
  }

  pub fn with_common_buttons(mut self, common_buttons: TaskDialogCommonButtons) -> Self {
    self.common_buttons = common_buttons;
    self
  }

  pub fn with_custom_buttons(mut self, custom_buttons: &'a [TaskDialogButton<'a>]) -> Self {
    self.custom_buttons = custom_buttons;
    self
  }

  pub fn with_default_button(mut self, default_button: i32) -> Self {
    self.default_button = default_button;
    self
  }

  pub fn with_verification_text(mut self, verification_text: &'a str) -> Self {
    self.verification_text = Some(verification_text);
    self
  }
}

#[cfg(target_os = "windows")]
pub fn show_task_dialog(
  options: &TaskDialogOptions<'_>,
) -> windows::core::Result<TaskDialogResult> {
  show_task_dialog_with(options, |config, button, checked| {
    // SAFETY: the constructor retains all referenced strings until this synchronous call returns.
    unsafe {
      windows::Win32::UI::Controls::TaskDialogIndirect(config, Some(button), None, Some(checked))
    }
  })
}

#[cfg(target_os = "windows")]
fn show_task_dialog_with(
  options: &TaskDialogOptions<'_>,
  show: impl FnOnce(
    &windows::Win32::UI::Controls::TASKDIALOGCONFIG,
    &mut i32,
    &mut windows::core::BOOL,
  ) -> windows::core::Result<()>,
) -> windows::core::Result<TaskDialogResult> {
  use windows::Win32::Foundation::{HINSTANCE, HWND};
  use windows::Win32::System::LibraryLoader::GetModuleHandleW;
  use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TD_WARNING_ICON, TDCBF_CANCEL_BUTTON,
    TDCBF_OK_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION, TDF_POSITION_RELATIVE_TO_WINDOW,
    TDF_SIZE_TO_CONTENT,
  };
  use windows::core::{BOOL, HSTRING, PCWSTR};

  let window_title = HSTRING::from(options.window_title);
  let main_instruction = HSTRING::from(options.main_instruction);
  let content = HSTRING::from(options.content);
  let verification_text = options.verification_text.map(HSTRING::from);
  let custom_button_labels: Vec<HSTRING> = options
    .custom_buttons
    .iter()
    .map(|button| HSTRING::from(button.label))
    .collect();
  let custom_buttons: Vec<TASKDIALOG_BUTTON> = options
    .custom_buttons
    .iter()
    .zip(custom_button_labels.iter())
    .map(|(button, label)| TASKDIALOG_BUTTON {
      nButtonID: button.id,
      pszButtonText: PCWSTR(label.as_ptr()),
    })
    .collect();

  let common_buttons = match options.common_buttons {
    TaskDialogCommonButtons::None => Default::default(),
    TaskDialogCommonButtons::OkCancel => TDCBF_OK_BUTTON | TDCBF_CANCEL_BUTTON,
  };
  let main_icon = match options.icon {
    TaskDialogIcon::None => Default::default(),
    TaskDialogIcon::Warning => TD_WARNING_ICON,
  };
  // SAFETY: Passing None asks Windows for the current process module handle and
  // does not transfer ownership.
  let hinstance: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
    Ok(handle) => handle.into(),
    Err(_) => HINSTANCE::default(),
  };
  let mut pressed_button = 0i32;
  let mut verification_checked = BOOL(0);

  let config = TASKDIALOGCONFIG {
    cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
    hwndParent: HWND::default(),
    hInstance: hinstance,
    dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW | TDF_SIZE_TO_CONTENT,
    dwCommonButtons: common_buttons,
    pszWindowTitle: PCWSTR(window_title.as_ptr()),
    Anonymous1: TASKDIALOGCONFIG_0 {
      pszMainIcon: main_icon,
    },
    pszMainInstruction: PCWSTR(main_instruction.as_ptr()),
    pszContent: PCWSTR(content.as_ptr()),
    cButtons: custom_buttons.len() as u32,
    pButtons: custom_buttons.as_ptr(),
    nDefaultButton: options.default_button,
    pszVerificationText: verification_text
      .as_ref()
      .map(|text| PCWSTR(text.as_ptr()))
      .unwrap_or_default(),
    ..Default::default()
  };

  show(&config, &mut pressed_button, &mut verification_checked)?;

  Ok(TaskDialogResult {
    button_id: pressed_button,
    verification_checked: verification_checked.as_bool(),
  })
}

#[cfg(not(target_os = "windows"))]
pub fn show_task_dialog(_options: &TaskDialogOptions<'_>) -> Result<TaskDialogResult, String> {
  Ok(TaskDialogResult {
    button_id: 1,
    verification_checked: false,
  })
}
