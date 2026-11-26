//! Installer presentation state. Burn/MSI own installation transactions.
pub mod motion;
pub mod options;
pub mod placement;
// Reuse the app's bounded native activation endpoint with an isolated installer identity.
#[cfg(feature = "native-instance")]
#[path = "../../../src/app/instance.rs"]
pub mod instance;
/// `HRESULT_FROM_WIN32(ERROR_PRODUCT_VERSION)`: a newer Flinch is installed (downgrade refused).
pub const NEWER_INSTALLED: i32 = 0x8007_0666_u32 as i32;
/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`: Flinch is installed both per user and per machine.
pub const SCOPE_CONFLICT: i32 = 0x8007_00B7_u32 as i32;

/// Catalog key for a failure the user can act on; anything else points to the log with its code.
pub fn failure_copy(status: i32) -> Option<&'static str> {
  match status {
    NEWER_INSTALLED => Some("newer_installed"),
    SCOPE_CONFLICT => Some("scope_conflict"),
    _ => None,
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
  #[default]
  Detecting,
  Ready,
  Planning,
  Applying,
  Complete,
  Failed,
  Cancelled,
  Closed,
}

#[derive(Clone, Debug, Default)]
pub struct State {
  pub phase: Phase,
  pub installed: bool,
  pub upgrade: bool,
  pub progress: i32,
  pub status: i32,
  pub restart: bool,
  pub display: bool,
  pub operation: i32,
  pub stage: i32,
  pub canceling: bool,
  pub removal_confirmation: bool,
  pub existing_scope: i32,
  pub folder_step: bool,
  pub optional_step: bool,
  pub activation_requested: bool,
  pub close_after_cancel: bool,
  pub finish_requested: bool,
  pub launch_requested: bool,
  pub launch_error: Option<String>,
}

impl State {
  /// Finish is a user action; callback progress must never launch the app twice.
  pub fn finish(&mut self, shortcuts: bool, launch: bool) -> Result<Option<i32>, String> {
    if !self.optional_step || self.busy() {
      return Err("Finish requires a completed installation".into());
    }
    let command = if shortcuts {
      self.command(7)?
    } else {
      self.optional_step = false;
      self.phase = Phase::Complete;
      None
    };
    self.finish_requested = true;
    self.launch_requested = launch;
    self.launch_error = None;
    Ok(command)
  }

  pub fn take_finish_action(&mut self) -> Option<bool> {
    if self.finish_requested && self.phase == Phase::Complete && !self.optional_step {
      self.finish_requested = false;
      Some(std::mem::take(&mut self.launch_requested) && !self.restart)
    } else {
      None
    }
  }

  pub fn launch_failed(&mut self, error: String) {
    self.optional_step = true;
    self.launch_error = Some(error);
  }

  /// Reserve presentation state before submitting an asynchronous Burn command.
  /// Removing files requires a separate confirmation; Back submits nothing.
  pub fn command(&mut self, operation: i32) -> Result<Option<i32>, String> {
    if self.phase == Phase::Ready && !self.installed && !self.upgrade {
      if operation == 11 {
        self.folder_step = true;
        return Ok(None);
      }
      if operation == 0 && self.folder_step {
        self.folder_step = false;
        return Ok(None);
      }
      if operation == 6 && !self.folder_step {
        return Err("Select installation scope before continuing".into());
      }
    }
    if operation == 0 && self.removal_confirmation {
      self.removal_confirmation = false;
      return Ok(None);
    }
    match operation {
      10 if !self.busy() => Ok(Some(0)),
      0 if !self.busy() => Ok(Some(0)),
      9 => {
        self.canceling = true;
        self.close_after_cancel = true;
        Ok(Some(9))
      },
      4 | 6 | 7 | 8 if !self.busy() => {
        if operation == 7 && !self.optional_step {
          return Err("Shortcut choices require a completed fresh installation".into());
        }
        if operation != 6 && operation != 7 && !self.installed {
          return Err("Maintenance requires an installed package".into());
        }
        if operation == 4 && !self.removal_confirmation {
          self.removal_confirmation = true;
          return Ok(None);
        }
        self.removal_confirmation = false;
        self.canceling = false;
        self.close_after_cancel = false;
        self.phase = Phase::Planning;
        self.operation = operation;
        self.progress = 0;
        self.stage = 0;
        Ok(Some(operation))
      },
      _ => Err("Installer operation is invalid or another operation is active".into()),
    }
  }
  pub fn busy(&self) -> bool {
    matches!(
      self.phase,
      Phase::Detecting | Phase::Planning | Phase::Applying
    )
  }

  pub fn take_activation_request(&mut self) -> bool {
    std::mem::take(&mut self.activation_requested)
  }

  pub fn apply_event(&mut self, kind: i32, value: i32, detail: i32) -> Result<(), String> {
    match kind {
      1 => {
        self.installed = value == 1;
        self.upgrade = value == 2;
        self.phase = Phase::Ready;
      },
      2 => {
        self.phase = Phase::Planning;
        self.operation = value;
      },
      3 => self.phase = Phase::Applying,
      4 => self.progress = value.clamp(0, 100),
      5 => {
        self.status = value;
        self.restart |= detail != 0;
        self.canceling = false;
        self.phase = if matches!(value, -2147023294 | -2147023673) {
          Phase::Cancelled
        } else if value < 0 {
          Phase::Failed
        } else {
          Phase::Complete
        };
        if value >= 0 {
          self.progress = 100;
          if self.operation == 6 && !self.installed && !self.upgrade {
            self.optional_step = true;
          }
          if self.operation == 7 {
            self.optional_step = false;
          }
        }
      },
      6 => {
        self.status = value;
        self.canceling = false;
        self.phase = if matches!(value, -2147023294 | -2147023673) {
          Phase::Cancelled
        } else {
          Phase::Failed
        };
      },
      7 => self.phase = Phase::Closed,
      8 => self.display = value >= 4,
      9 => self.stage = value,
      10 => self.existing_scope = value,
      11 => self.activation_requested |= self.display,
      _ => return Err(format!("Unknown Burn presentation event {kind}")),
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn finish_waits_for_shortcuts_and_consumes_launch_once() {
    for shortcuts in [false, true] {
      for launch in [false, true] {
        let mut state = State {
          phase: Phase::Complete,
          optional_step: true,
          ..Default::default()
        };
        assert_eq!(
          state.finish(shortcuts, launch).expect("finish"),
          shortcuts.then_some(7)
        );
        if shortcuts {
          assert_eq!(state.take_finish_action(), None);
          state.apply_event(5, -1, 0).expect("shortcut failure");
          assert_eq!(state.take_finish_action(), None);
          state.finish(true, launch).expect("retry shortcuts");
          state.apply_event(5, 0, 0).expect("shortcuts complete");
        }
        assert_eq!(state.take_finish_action(), Some(launch));
        assert_eq!(
          state.take_finish_action(),
          None,
          "coalesced callbacks must not launch twice"
        );
        if launch {
          state.launch_failed("access denied".into());
          assert!(state.optional_step);
          assert_eq!(state.launch_error.as_deref(), Some("access denied"));
          state.finish(false, false).expect("close without launching");
          assert!(state.launch_error.is_none());
          assert_eq!(state.take_finish_action(), Some(false));
        }
      }
    }
    let mut restart = State {
      phase: Phase::Complete,
      optional_step: true,
      restart: true,
      ..Default::default()
    };
    restart.finish(false, true).expect("acknowledge restart");
    assert_eq!(restart.take_finish_action(), Some(false));
    assert!(State::default().finish(false, true).is_err());
  }

  #[test]
  fn cancellation_waits_for_terminal_callback_and_never_masks_committed_success() {
    for phase in [Phase::Detecting, Phase::Planning, Phase::Applying] {
      for event in [5, 6] {
        let mut state = State {
          phase,
          operation: 6,
          ..Default::default()
        };
        assert_eq!(state.command(9).expect("cancel"), Some(9));
        assert!(state.busy(), "wait for Burn's terminal callback");
        assert!(state.close_after_cancel);
        state
          .apply_event(4, 40, 0)
          .expect("progress while stopping");
        assert!(state.canceling);
        state
          .apply_event(event, -2147023294, 0)
          .expect("rollback completed");
        assert_eq!(state.phase, Phase::Cancelled);
        assert!(!state.busy());
        assert!(!state.canceling);
        assert!(!state.optional_step);
        assert_eq!(state.command(0).expect("close after rollback"), Some(0));
        state.folder_step = true;
        assert_eq!(state.command(6).expect("retry install"), Some(6));
        assert!(!state.close_after_cancel);
        assert!(!state.canceling);
      }
    }
    let mut committed = State {
      phase: Phase::Applying,
      operation: 6,
      ..Default::default()
    };
    committed.command(9).expect("late cancel request");
    committed.apply_event(5, 0, 0).expect("already committed");
    assert_eq!(
      committed.phase,
      Phase::Complete,
      "report the actual transaction result"
    );
    assert!(committed.optional_step);
    let mut declined = State::default();
    declined
      .apply_event(6, -2147023673, 0)
      .expect("UAC declined");
    assert_eq!(declined.phase, Phase::Cancelled);
    assert!(
      !declined.close_after_cancel,
      "declining UAC alone does not request close"
    );
  }

  #[test]
  fn elevation_completion_survives_coalesced_progress_and_activates_once() {
    for status in [0, -2147023673, -2147467259] {
      let mut state = State {
        phase: Phase::Applying,
        display: true,
        ..Default::default()
      };
      state
        .apply_event(11, status, 0)
        .expect("elevation completed");
      state.apply_event(4, 25, 0).expect("coalesced progress");
      assert_eq!(state.phase, Phase::Applying);
      assert_eq!(state.status, 0, "Apply owns the transaction result");
      assert!(state.take_activation_request());
      assert!(!state.take_activation_request());
      state.apply_event(4, 50, 0).expect("later progress");
      assert!(!state.take_activation_request());
      state.apply_event(11, 0, 0).expect("another elevation");
      assert!(state.take_activation_request());
    }
    let mut hidden = State::default();
    hidden.apply_event(11, 0, 0).expect("silent elevation");
    assert!(!hidden.take_activation_request());
  }

  #[test]
  fn fresh_steps_are_ordered_and_shortcuts_follow_success_only() {
    let mut state = State {
      phase: Phase::Ready,
      ..Default::default()
    };
    assert!(state.command(7).is_err());
    assert!(state.command(6).is_err());
    assert_eq!(state.command(11).expect("next"), None);
    assert!(state.folder_step);
    assert_eq!(state.command(0).expect("back"), None);
    assert!(!state.folder_step);
    state.command(11).expect("next again");
    assert_eq!(state.command(6).expect("install"), Some(6));
    state.apply_event(5, -1, 0).expect("failed install");
    assert!(!state.optional_step);
    state.apply_event(5, 0, 1).expect("successful install");
    assert!(state.optional_step);
    assert_eq!(state.command(7).expect("add shortcuts"), Some(7));
    state
      .apply_event(5, -2147023294, 0)
      .expect("decline shortcut elevation");
    assert!(state.optional_step);
    assert!(state.restart);
    assert_eq!(state.command(7).expect("retry shortcuts"), Some(7));
    state.apply_event(5, 0, 0).expect("shortcuts added");
    assert!(!state.optional_step);
    assert!(state.restart);
    let mut maintenance = State {
      phase: Phase::Ready,
      installed: true,
      ..Default::default()
    };
    maintenance.command(8).expect("repair");
    maintenance.apply_event(5, 0, 0).expect("repair complete");
    assert!(!maintenance.optional_step);
  }

  #[test]
  fn known_detection_failures_have_their_own_copy() {
    assert_eq!(failure_copy(-2147023258), Some("newer_installed"));
    assert_eq!(failure_copy(-2147024713), Some("scope_conflict"));
    assert_eq!(failure_copy(-2147023293), None);
    assert_eq!(failure_copy(-1), None);
    let catalogs: serde_json::Value =
      serde_json::from_str(include_str!("../../../messages/installer-ui.json")).expect("catalog");
    for status in [NEWER_INSTALLED, SCOPE_CONFLICT] {
      let key = failure_copy(status).expect("known failure");
      for locale in ["en", "fr", "de", "es", "it"] {
        assert!(
          catalogs[locale][key]
            .as_str()
            .is_some_and(|copy| !copy.is_empty())
        );
      }
    }
  }

  #[test]
  fn repair_failure_and_reboot_remain_explicit() {
    let mut state = State::default();
    assert!(state.busy());
    state.apply_event(1, 1, 0).expect("detect");
    assert!(state.installed);
    assert!(!state.busy());
    state.apply_event(2, 8, 0).expect("repair plan");
    state.apply_event(3, 0, 0).expect("apply");
    state.apply_event(4, 200, 0).expect("progress");
    assert_eq!(state.progress, 100);
    state.apply_event(5, -2147023293, 0).expect("failure");
    assert_eq!(state.phase, Phase::Failed);
    assert!(!state.busy());
    state.apply_event(5, 0, 1).expect("restart");
    assert!(state.restart);
    assert_eq!(state.phase, Phase::Complete);
    assert!(state.apply_event(99, 0, 0).is_err());
  }

  #[test]
  fn removal_requires_confirmation_and_commands_reserve_before_callbacks() {
    let mut state = State {
      phase: Phase::Ready,
      installed: true,
      ..Default::default()
    };
    assert_eq!(state.command(4).expect("request removal"), None);
    assert!(state.removal_confirmation);
    assert_eq!(state.command(0).expect("back"), None);
    assert!(!state.removal_confirmation);
    assert_eq!(state.command(4).expect("request again"), None);
    assert_eq!(state.command(4).expect("confirm"), Some(4));
    assert_eq!(state.phase, Phase::Planning);
    assert!(state.command(8).is_err());
    assert_eq!(state.command(9).expect("cancel"), Some(9));
    state
      .apply_event(5, -2147023294, 0)
      .expect("cancel complete");
    assert_eq!(state.phase, Phase::Cancelled);
    assert!(!state.canceling);
    let mut fresh = State {
      phase: Phase::Ready,
      ..Default::default()
    };
    assert!(fresh.command(4).is_err());
    assert!(fresh.command(8).is_err());
  }
}
