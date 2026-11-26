use crate::app::context::AppHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingPresentationMode {
  FullUi,
  Silent,
}

impl RecordingPresentationMode {
  pub fn is_silent(self) -> bool {
    matches!(self, Self::Silent)
  }
}

pub fn can_guarantee_capture_exclusion() -> bool {
  crate::overlay::gdi::window::supports_true_capture_exclusion()
}

pub fn resolve_presentation_mode() -> RecordingPresentationMode {
  if can_guarantee_capture_exclusion() {
    RecordingPresentationMode::FullUi
  } else {
    RecordingPresentationMode::Silent
  }
}

pub fn ensure_recording_visibility_policy(
  app: &AppHandle,
) -> Result<Option<RecordingPresentationMode>, String> {
  let mode = resolve_presentation_mode();
  if mode == RecordingPresentationMode::FullUi {
    return Ok(Some(mode));
  }

  let config = crate::config::Config::current(app);
  if config.recording_visibility_warning_dismissed {
    return Ok(Some(mode));
  }

  let result = show_warning_dialog().map_err(|error| {
    format!(
      "Failed to show recording visibility warning dialog: {}",
      error
    )
  });

  apply_warning_decision(mode, result, || {
    let mut updated = config;
    updated.recording_visibility_warning_dismissed = true;
    updated.validate();
    updated.save().map_err(|error| {
      format!(
        "Failed to save recording visibility warning preference: {}",
        error
      )
    })
  })
}

fn apply_warning_decision(
  mode: RecordingPresentationMode,
  result: Result<WarningDialogResult, String>,
  save_dismissal: impl FnOnce() -> Result<(), String>,
) -> Result<Option<RecordingPresentationMode>, String> {
  let result = result?;
  if result.dismiss_forever {
    save_dismissal()?;
  }
  Ok(result.continue_recording.then_some(mode))
}

struct WarningDialogResult {
  continue_recording: bool,
  dismiss_forever: bool,
}

#[cfg(target_os = "windows")]
fn show_warning_dialog() -> windows::core::Result<WarningDialogResult> {
  use crate::dialogs::{
    TASK_DIALOG_OK_ID, TaskDialogCommonButtons, TaskDialogOptions, show_task_dialog,
  };

  let result = show_task_dialog(
    &TaskDialogOptions::new(
      "Flinch",
      "Flinch may appear inside this recording",
      "Flinch cannot guarantee that its recording HUD and notifications will be hidden on this Windows setup.\n\nArea selection still works normally. Once recording starts, Flinch will hide its recording UI and suppress Flinch notifications until the capture finishes.",
    )
    .with_common_buttons(TaskDialogCommonButtons::OkCancel)
    .with_default_button(TASK_DIALOG_OK_ID)
    .with_verification_text("Don't display again"),
  )?;

  Ok(WarningDialogResult {
    continue_recording: result.button_id == TASK_DIALOG_OK_ID,
    dismiss_forever: result.verification_checked,
  })
}

#[cfg(not(target_os = "windows"))]
fn show_warning_dialog() -> Result<WarningDialogResult, String> {
  Ok(WarningDialogResult {
    continue_recording: true,
    dismiss_forever: false,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn declined_warning_is_cancellation_but_dialog_and_save_failures_remain_errors() {
    let mode = RecordingPresentationMode::Silent;
    for proceed in [false, true] {
      let result = apply_warning_decision(
        mode,
        Ok(WarningDialogResult {
          continue_recording: proceed,
          dismiss_forever: false,
        }),
        || panic!("No persistence requested"),
      );
      assert_eq!(result.expect("explicit decision"), proceed.then_some(mode));
    }
    let failure = "cancel button could not be created";
    assert_eq!(
      apply_warning_decision(mode, Err(failure.into()), || panic!("Dialog failed"))
        .expect_err("real dialog failure"),
      failure
    );
    assert_eq!(
      apply_warning_decision(
        mode,
        Ok(WarningDialogResult {
          continue_recording: false,
          dismiss_forever: true,
        }),
        || Err("access denied".into())
      )
      .expect_err("real save failure"),
      "access denied"
    );
  }
}
