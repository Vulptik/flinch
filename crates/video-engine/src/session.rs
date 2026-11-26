use crate::{EngineError, ErrorKind, Result, protocol::Command, types::SessionState};

#[derive(Default)]
pub struct StateMachine {
  state: SessionState,
}
impl StateMachine {
  pub fn state(&self) -> SessionState {
    self.state
  }
  pub fn request(&mut self, command: &Command) -> Result<SessionState> {
    use SessionState::*;
    let next = match (self.state, command) {
      (Created, Command::Prepare(_)) => Preparing,
      (Ready, Command::Start) => Starting,
      (Recording, Command::Pause) => Pausing,
      (Paused, Command::Resume) => Resuming,
      (Recording, Command::Microphone(_)) => Recording,
      (Paused, Command::Microphone(_)) => Paused,
      (
        Created | Preparing | Ready | Starting | Recording | Pausing | Paused | Resuming,
        Command::Stop,
      ) => Stopping,
      (
        Created | Preparing | Ready | Starting | Recording | Pausing | Paused | Resuming | Stopping
        | Finalizing,
        Command::Abort,
      ) => Aborting,
      _ => {
        return Err(EngineError::new(
          ErrorKind::InvalidState,
          "control",
          format!("Cannot {command:?} in {:?}", self.state),
        ));
      },
    };
    self.state = next;
    Ok(next)
  }
  pub fn confirm(&mut self, next: SessionState) -> Result<()> {
    use SessionState::*;
    let valid = matches!(
      (self.state, next),
      (Preparing, Ready)
        | (Starting, Recording)
        | (Pausing, Paused)
        | (Resuming, Recording)
        | (Stopping, Finalizing)
        | (Finalizing, Completed)
        | (Aborting, Aborted)
    );
    if !valid {
      return Err(EngineError::new(
        ErrorKind::InvalidState,
        "control",
        format!("Invalid confirmation {:?} -> {next:?}", self.state),
      ));
    }
    self.state = next;
    Ok(())
  }
  pub fn fail(&mut self) -> bool {
    if self.state.terminal() {
      false
    } else {
      self.state = SessionState::Failed;
      true
    }
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  use crate::types::*;
  #[test]
  fn microphone_switches_keep_the_state_and_need_a_running_recording() {
    let mut s = StateMachine::default();
    assert!(s.request(&Command::Microphone(false)).is_err());
    s.state = SessionState::Recording;
    assert_eq!(
      s.request(&Command::Microphone(false)).expect("recording"),
      SessionState::Recording
    );
    s.state = SessionState::Paused;
    assert_eq!(
      s.request(&Command::Microphone(true)).expect("paused"),
      SessionState::Paused
    );
    s.state = SessionState::Stopping;
    assert!(s.request(&Command::Microphone(true)).is_err());
  }
  fn spec() -> SessionSpec {
    SessionSpec {
      monitor: "primary".into(),
      fps: 60,
      capture: CaptureKind::Dxgi,
      encoder: EncoderKind::Amf,
      quality: Quality::default(),
      cursor: false,
      clicks: false,
      audio: false,
      audio_device: None,
      microphone: false,
      microphone_device: None,
      output: "test.mp4".into(),
      trace_frames: false,
      region: None,
      span: Vec::new(),
    }
  }
  #[test]
  fn controls_can_cancel_before_preparation_without_opening_resources() {
    for command in [Command::Stop, Command::Abort] {
      let mut state = StateMachine::default();
      state.request(&command).expect("early cancellation");
      if matches!(command, Command::Stop) {
        state
          .confirm(SessionState::Finalizing)
          .expect("nothing to drain");
        state.request(&Command::Abort).expect("nothing to publish");
      }
      state
        .confirm(SessionState::Aborted)
        .expect("confirmed cancellation");
      assert!(!state.fail());
      assert!(state.request(&Command::Start).is_err());
    }
  }
  #[test]
  fn acknowledges_only_completed_transitions() {
    let mut s = StateMachine::default();
    assert!(s.request(&Command::Start).is_err());
    s.request(&Command::Prepare(spec()))
      .expect("valid test fixture");
    assert_eq!(s.state(), SessionState::Preparing);
    s.confirm(SessionState::Ready).expect("valid test fixture");
    s.request(&Command::Start).expect("valid test fixture");
    s.confirm(SessionState::Recording)
      .expect("valid test fixture");
    s.request(&Command::Pause).expect("valid test fixture");
    assert!(s.request(&Command::Resume).is_err());
    s.confirm(SessionState::Paused).expect("valid test fixture");
    s.request(&Command::Resume).expect("valid test fixture");
    s.confirm(SessionState::Recording)
      .expect("valid test fixture");
    s.request(&Command::Stop).expect("valid test fixture");
    s.confirm(SessionState::Finalizing)
      .expect("valid test fixture");
    s.confirm(SessionState::Completed)
      .expect("valid test fixture");
    assert!(!s.fail());
    assert!(s.request(&Command::Start).is_err());
  }
  #[test]
  fn stop_preempts_start_and_abort_preempts_finalization() {
    let mut s = StateMachine::default();
    s.request(&Command::Prepare(spec()))
      .expect("valid test fixture");
    s.request(&Command::Stop).expect("valid test fixture");
    assert!(s.confirm(SessionState::Ready).is_err());
    s.confirm(SessionState::Finalizing)
      .expect("valid test fixture");
    s.request(&Command::Abort).expect("valid test fixture");
    s.confirm(SessionState::Aborted)
      .expect("valid test fixture");
    assert!(!s.fail());
    let mut f = StateMachine::default();
    assert!(f.fail());
    assert!(!f.fail());
  }
}
