//! Deadlines belong to requests, rather than to the latest state event.
use crate::{
  EngineError, ErrorKind, Result, limits,
  metrics::{Distribution, Histogram},
  protocol::{Command, Event},
  types::SessionState,
};
use std::{collections::BTreeMap, time::Instant};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum Operation {
  Prepare,
  Start,
  Pause,
  Resume,
  Stop,
  Abort,
  Microphone,
}
struct Pending {
  operation: Operation,
  deadline: Instant,
  submitted: Instant,
}
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ControlReport {
  pub prepare: Distribution,
  pub start: Distribution,
  pub pause: Distribution,
  pub resume: Distribution,
  pub stop: Distribution,
  pub abort: Distribution,
  #[serde(default)]
  pub microphone: Distribution,
  pub rejected_requests: u64,
  pub superseded_requests: u64,
  #[serde(default)]
  pub launch: Option<LaunchTiming>,
}
/// Lab request-to-recording, measured from just before the worker process is created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaunchTiming {
  pub spawn_us: u64,
  pub hello_us: u64,
  pub recording_us: Option<u64>,
}
fn since(origin: Instant, now: Instant) -> Result<u64> {
  let elapsed = now
    .checked_duration_since(origin)
    .ok_or_else(|| EngineError::protocol("Supervisor launch clock reversed"))?;
  u64::try_from(elapsed.as_micros()).map_err(|_| EngineError::protocol("Launch latency overflow"))
}
pub struct Watch {
  pending: BTreeMap<u64, Pending>,
  last_event: Instant,
  terminal_deadline: Option<Instant>,
  measurements: [Histogram; 7],
  rejected_requests: u64,
  superseded_requests: u64,
  launch: Option<(Instant, LaunchTiming)>,
}
impl Watch {
  pub fn new(now: Instant) -> Self {
    Self {
      pending: BTreeMap::new(),
      last_event: now,
      terminal_deadline: None,
      measurements: std::array::from_fn(|_| Histogram::default()),
      rejected_requests: 0,
      superseded_requests: 0,
      launch: None,
    }
  }
  /// `before` precedes process creation; `spawned` follows it; `hello` is the first worker event.
  pub fn launched(&mut self, before: Instant, spawned: Instant, hello: Instant) -> Result<()> {
    let spawn_us = since(before, spawned)?;
    let hello_us = since(before, hello)?;
    if hello_us < spawn_us {
      return Err(EngineError::protocol(
        "Worker hello preceded its process creation",
      ));
    }
    self.launch = Some((
      before,
      LaunchTiming {
        spawn_us,
        hello_us,
        recording_us: None,
      },
    ));
    Ok(())
  }
  pub fn submitted(&mut self, id: u64, command: &Command, now: Instant) -> Result<()> {
    let (operation, limit) = match command {
      Command::Prepare(_) => (Operation::Prepare, limits::PREPARE_TIMEOUT),
      Command::Start => (Operation::Start, limits::START_TIMEOUT),
      Command::Pause => (Operation::Pause, limits::CONTROL_TIMEOUT),
      Command::Resume => (Operation::Resume, limits::CONTROL_TIMEOUT),
      Command::Microphone(_) => (Operation::Microphone, limits::CONTROL_TIMEOUT),
      Command::Stop => (Operation::Stop, limits::STOP_TIMEOUT),
      Command::Abort => (Operation::Abort, limits::ABORT_TIMEOUT),
    };
    if self.pending.contains_key(&id) || id == 0 {
      return Err(EngineError::protocol("Duplicate supervisor request id"));
    }
    if self.stopping() && !matches!(operation, Operation::Stop | Operation::Abort) {
      return Err(EngineError::new(
        ErrorKind::InvalidState,
        "supervisor controls",
        "A terminal operation is pending",
      ));
    }
    if operation == Operation::Stop
      && self
        .pending
        .values()
        .any(|p| p.operation == Operation::Abort)
    {
      return Err(EngineError::new(
        ErrorKind::InvalidState,
        "supervisor controls",
        "Abort already superseded Stop",
      ));
    }
    if self.pending.len() >= 8 && !matches!(operation, Operation::Stop | Operation::Abort) {
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "supervisor controls",
        "Eight controls are already in flight",
      ));
    }
    // One outstanding Stop and Abort each is sufficient. Repeated human controls
    // cannot extend the deadline of the original request.
    if matches!(operation, Operation::Stop | Operation::Abort)
      && self.pending.values().any(|p| p.operation == operation)
    {
      return Err(EngineError::new(
        ErrorKind::InvalidState,
        "supervisor controls",
        "This terminal control is already pending",
      ));
    }
    let before = self.pending.len();
    match operation {
      Operation::Stop => self
        .pending
        .retain(|_, p| matches!(p.operation, Operation::Stop | Operation::Abort)),
      Operation::Abort => self.pending.retain(|_, p| p.operation == Operation::Abort),
      _ => {},
    }
    self.superseded_requests = self
      .superseded_requests
      .saturating_add((before - self.pending.len()) as u64);
    if matches!(operation, Operation::Stop | Operation::Abort) {
      self.terminal_deadline = Some(now + limit);
    }
    self.pending.insert(
      id,
      Pending {
        operation,
        deadline: now + limit,
        submitted: now,
      },
    );
    Ok(())
  }
  pub fn stopping(&self) -> bool {
    self
      .pending
      .values()
      .any(|p| matches!(p.operation, Operation::Stop | Operation::Abort))
  }
  /// Retained after Outcome so child cleanup, metadata and terminal output share the original budget.
  pub fn terminal_deadline(&self) -> Option<Instant> {
    self.terminal_deadline
  }
  pub fn report(&self) -> ControlReport {
    ControlReport {
      prepare: self.measurements[0].summary(),
      start: self.measurements[1].summary(),
      pause: self.measurements[2].summary(),
      resume: self.measurements[3].summary(),
      stop: self.measurements[4].summary(),
      abort: self.measurements[5].summary(),
      microphone: self.measurements[6].summary(),
      rejected_requests: self.rejected_requests,
      superseded_requests: self.superseded_requests,
      launch: self.launch.map(|(_, timing)| timing),
    }
  }
  fn completed(&mut self, id: u64, now: Instant) -> Result<()> {
    if let Some(pending) = self.pending.remove(&id) {
      let elapsed = now
        .checked_duration_since(pending.submitted)
        .ok_or_else(|| EngineError::protocol("Supervisor control clock reversed"))?;
      self.measurements[pending.operation as usize].record(
        u64::try_from(elapsed.as_micros())
          .map_err(|_| EngineError::protocol("Control latency overflow"))?,
      );
    }
    Ok(())
  }
  pub fn observe(&mut self, event: &Event, now: Instant) -> Result<()> {
    if matches!(event, Event::Ack { .. } | Event::State(_)) {
      self.check_pending_deadlines(now)?;
    }
    self.last_event = now;
    if matches!(
      event,
      Event::State(SessionState::Recording)
        | Event::Ack {
          state: SessionState::Recording,
          ..
        }
    ) && let Some((before, timing)) = &mut self.launch
      && timing.recording_us.is_none()
    {
      // The first confirmation only; a later Resume is not a launch.
      timing.recording_us = Some(since(*before, now)?);
    }
    match event {
      Event::StageStalled(error) => return Err(error.clone()),
      Event::Ack { id, state } => {
        if let Some(p) = self.pending.get(id) {
          let done = match p.operation {
            Operation::Prepare => *state == SessionState::Ready,
            Operation::Start => *state == SessionState::Recording,
            Operation::Pause => *state == SessionState::Paused,
            Operation::Resume => *state == SessionState::Recording,
            Operation::Microphone => {
              matches!(state, SessionState::Recording | SessionState::Paused)
            },
            Operation::Stop | Operation::Abort => false,
          };
          if done {
            self.completed(*id, now)?;
          }
        }
      },
      Event::State(SessionState::Recording) => {
        let starts: Vec<_> = self
          .pending
          .iter()
          .filter(|(_, p)| p.operation == Operation::Start)
          .map(|(id, _)| *id)
          .collect();
        for id in starts {
          self.completed(id, now)?;
        }
      },
      Event::Rejected { id, error } => {
        self.rejected_requests = self.rejected_requests.saturating_add(1);
        if self
          .pending
          .remove(id)
          .is_some_and(|p| matches!(p.operation, Operation::Stop | Operation::Abort))
        {
          return Err(error.clone());
        }
      },
      Event::Outcome(_) => {
        let terminal: Vec<_> = self
          .pending
          .iter()
          .filter(|(_, p)| matches!(p.operation, Operation::Stop | Operation::Abort))
          .map(|(id, _)| *id)
          .collect();
        for id in terminal {
          self.completed(id, now)?;
        }
        self.pending.clear();
      },
      _ => {},
    }
    Ok(())
  }
  pub fn check(&self, now: Instant) -> Result<()> {
    if now.saturating_duration_since(self.last_event) > limits::HELLO_TIMEOUT {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "supervisor heartbeat",
        "No worker event for three seconds",
      ));
    }
    self.check_pending_deadlines(now)
  }
  fn check_pending_deadlines(&self, now: Instant) -> Result<()> {
    if let Some((id, p)) = self
      .pending
      .iter()
      .filter(|(_, p)| now > p.deadline)
      .min_by_key(|(_, p)| p.deadline)
    {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "supervisor operation",
        format!("Request {id} ({:?}) exceeded its deadline", p.operation),
      ));
    }
    Ok(())
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  use std::time::Duration;
  #[test]
  fn a_confirmation_received_after_its_deadline_cannot_erase_the_expired_request() {
    let now = Instant::now();
    let mut watch = Watch::new(now);
    watch.submitted(1, &Command::Pause, now).expect("Pause");
    let late = now + limits::CONTROL_TIMEOUT + Duration::from_millis(1);
    assert_eq!(
      watch
        .observe(
          &Event::Ack {
            id: 1,
            state: SessionState::Paused
          },
          late
        )
        .expect_err("late confirmation")
        .kind,
      ErrorKind::Timeout
    );
    assert_eq!(watch.report().pause.count, 0);
    assert!(watch.check(late).is_err());
  }
  #[test]
  fn command_latencies_wait_for_confirmation_and_duplicates_do_not_double_count() {
    let now = Instant::now();
    let mut watch = Watch::new(now);
    watch.submitted(1, &Command::Start, now).expect("Start");
    watch
      .observe(
        &Event::Ack {
          id: 1,
          state: SessionState::Starting,
        },
        now + Duration::from_millis(20),
      )
      .expect("sources starting");
    assert_eq!(watch.report().start.count, 0);
    watch
      .observe(
        &Event::State(SessionState::Recording),
        now + Duration::from_millis(90),
      )
      .expect("first encoded confirmation");
    watch
      .observe(
        &Event::Ack {
          id: 1,
          state: SessionState::Recording,
        },
        now + Duration::from_millis(95),
      )
      .expect("duplicate confirmation");
    assert_eq!(
      (watch.report().start.count, watch.report().start.maximum_us),
      (1, 90000)
    );
    watch
      .submitted(2, &Command::Pause, now + Duration::from_millis(100))
      .expect("Pause");
    watch
      .observe(
        &Event::Ack {
          id: 2,
          state: SessionState::Paused,
        },
        now + Duration::from_millis(220),
      )
      .expect("confirmed Pause");
    assert_eq!(watch.report().pause.maximum_us, 120000);
    watch
      .submitted(3, &Command::Resume, now + Duration::from_millis(230))
      .expect("Resume");
    watch
      .submitted(4, &Command::Stop, now + Duration::from_millis(240))
      .expect("Stop supersedes Resume");
    assert_eq!(watch.report().superseded_requests, 1);
    assert_eq!(watch.report().resume.count, 0);
    watch
      .observe(
        &Event::Rejected {
          id: 3,
          error: EngineError::new(ErrorKind::Cancelled, "Resume", "superseded"),
        },
        now + Duration::from_millis(245),
      )
      .expect("late rejection");
    assert_eq!(watch.report().rejected_requests, 1);
    watch
      .observe(
        &Event::Ack {
          id: 4,
          state: SessionState::Completed,
        },
        now + Duration::from_millis(250),
      )
      .expect("Ack does not mean terminal flush");
    assert_eq!(watch.report().stop.count, 0);
    watch
      .observe(
        &Event::Outcome(crate::types::RecordingOutcome {
          thumbnail: None,
          kind: crate::types::OutcomeKind::Completed,
          output: Some("fixture.mp4".into()),
          partial: None,
          error: None,
          snapshot: crate::types::SessionSnapshot::default(),
        }),
        now + Duration::from_millis(340),
      )
      .expect("terminal result");
    assert_eq!(
      (watch.report().stop.count, watch.report().stop.maximum_us),
      (1, 100000)
    );
    watch
      .submitted(5, &Command::Pause, now + Duration::from_secs(1))
      .expect("clock fixture");
    assert!(
      watch
        .observe(
          &Event::Ack {
            id: 5,
            state: SessionState::Paused
          },
          now
        )
        .is_err()
    );
  }
  #[test]
  fn outcome_does_not_grant_fresh_cleanup_and_output_budgets() {
    let now = Instant::now();
    let mut watch = Watch::new(now);
    watch.submitted(1, &Command::Stop, now).expect("Stop");
    let deadline = watch.terminal_deadline().expect("shared deadline");
    watch
      .observe(
        &Event::Outcome(crate::types::RecordingOutcome {
          thumbnail: None,
          kind: crate::types::OutcomeKind::Aborted,
          output: None,
          partial: None,
          error: None,
          snapshot: crate::types::SessionSnapshot::default(),
        }),
        now + Duration::from_secs(9),
      )
      .expect("terminal event");
    assert_eq!(watch.terminal_deadline(), Some(deadline));
    assert_eq!(deadline, now + limits::STOP_TIMEOUT);
    watch
      .observe(
        &Event::Ack {
          id: 1,
          state: SessionState::Completed,
        },
        now + Duration::from_secs(10),
      )
      .expect("late Ack");
    assert_eq!(watch.terminal_deadline(), Some(deadline));
  }
  #[test]
  fn a_stage_stall_is_fatal_even_with_recent_heartbeats_and_no_pending_control() {
    let now = Instant::now();
    let mut watch = Watch::new(now);
    watch
      .observe(&Event::Heartbeat, now)
      .expect("coordinator heartbeat alive");
    let error = watch
      .observe(
        &Event::StageStalled(EngineError::new(
          ErrorKind::Timeout,
          "scripted GPU call",
          "native owner did not return",
        )),
        now,
      )
      .expect_err("stage progress is independent of heartbeat");
    assert_eq!(error.stage, "scripted GPU call");
    assert_eq!(error.kind, ErrorKind::Timeout);
  }
  #[test]
  fn terminal_controls_supersede_interrupted_deadlines_without_extending_their_own() {
    let t = Instant::now();
    let mut watch = Watch::new(t);
    watch.submitted(1, &Command::Pause, t).expect("pause");
    watch
      .submitted(2, &Command::Stop, t + Duration::from_millis(1900))
      .expect("Stop preempts pause");
    watch
      .observe(&Event::Heartbeat, t + Duration::from_secs(3))
      .expect("heartbeat");
    assert!(watch.check(t + Duration::from_secs(3)).is_ok());
    assert!(
      watch
        .submitted(3, &Command::Resume, t + Duration::from_secs(3))
        .is_err()
    );
    watch
      .submitted(3, &Command::Abort, t + Duration::from_secs(11))
      .expect("Abort preempts Stop");
    watch
      .observe(
        &Event::Ack {
          id: 2,
          state: SessionState::Completed,
        },
        t + Duration::from_secs(12),
      )
      .expect("late Stop ack");
    assert!(watch.check(t + Duration::from_secs(12)).is_ok());
    assert!(
      watch
        .submitted(4, &Command::Stop, t + Duration::from_secs(12))
        .is_err()
    );
    assert!(
      watch
        .submitted(4, &Command::Abort, t + Duration::from_secs(12))
        .is_err()
    );
    watch
      .observe(&Event::Heartbeat, t + Duration::from_secs(15))
      .expect("heartbeat");
    assert!(watch.check(t + Duration::from_secs(15)).is_err());
  }
  #[test]
  fn refusing_a_terminal_control_propagates_its_cause_instead_of_leaving_a_live_worker() {
    let t = Instant::now();
    for command in [Command::Stop, Command::Abort] {
      let mut watch = Watch::new(t);
      watch.submitted(1, &command, t).expect("terminal control");
      let error = watch
        .observe(
          &Event::Rejected {
            id: 1,
            error: EngineError::new(ErrorKind::Protocol, "scripted rejection", "original cause"),
          },
          t,
        )
        .expect_err("terminal rejection is fatal");
      assert_eq!(error.stage, "scripted rejection");
      assert_eq!(error.reason, "original cause");
    }
  }
  #[test]
  fn a_late_pause_ack_and_heartbeats_cannot_extend_stop_or_abort() {
    let t = Instant::now();
    let mut w = Watch::new(t);
    w.submitted(1, &Command::Pause, t).expect("pause");
    w.submitted(2, &Command::Stop, t).expect("preempting stop");
    w.observe(
      &Event::Ack {
        id: 1,
        state: SessionState::Paused,
      },
      t + Duration::from_millis(100),
    )
    .expect("late pause ack");
    w.observe(
      &Event::State(SessionState::Paused),
      t + Duration::from_secs(9),
    )
    .expect("late state");
    w.observe(&Event::Heartbeat, t + Duration::from_secs(11))
      .expect("heartbeat");
    assert!(w.check(t + Duration::from_secs(11)).is_err());
    w.submitted(3, &Command::Abort, t + Duration::from_secs(1))
      .expect("abort");
    w.observe(&Event::Heartbeat, t + Duration::from_secs(5))
      .expect("heartbeat");
    assert!(w.check(t + Duration::from_secs(5)).is_err());
  }
  #[test]
  fn starting_ack_is_not_a_first_encoded_frame_and_rejections_preserve_other_deadlines() {
    let t = Instant::now();
    let mut w = Watch::new(t);
    w.submitted(1, &Command::Start, t).expect("start");
    w.observe(
      &Event::Ack {
        id: 1,
        state: SessionState::Starting,
      },
      t,
    )
    .expect("starting");
    w.submitted(2, &Command::Pause, t + Duration::from_secs(1))
      .expect("queued invalid pause");
    w.observe(
      &Event::Rejected {
        id: 2,
        error: EngineError::protocol("invalid state"),
      },
      t + Duration::from_secs(2),
    )
    .expect("rejection");
    w.observe(&Event::Heartbeat, t + Duration::from_secs(6))
      .expect("heartbeat");
    assert!(w.check(t + Duration::from_secs(6)).is_err());
    assert!(
      w.observe(
        &Event::State(SessionState::Recording),
        t + Duration::from_secs(6),
      )
      .is_err()
    );
    assert_eq!(w.report().start.count, 0);
    assert!(w.check(t + Duration::from_secs(7)).is_err());

    let mut timely = Watch::new(t);
    timely.submitted(1, &Command::Start, t).expect("start");
    timely
      .observe(
        &Event::State(SessionState::Recording),
        t + Duration::from_secs(4),
      )
      .expect("encoded first frame within deadline");
    assert!(timely.check(t + Duration::from_secs(5)).is_ok());
    assert!(timely.check(t + Duration::from_secs(8)).is_err());
  }
  #[test]
  fn controls_remain_bounded_and_terminal_duplicates_do_not_restart_the_clock() {
    let t = Instant::now();
    let mut w = Watch::new(t);
    for id in 1..=8 {
      w.submitted(id, &Command::Pause, t)
        .expect("bounded queued request");
    }
    assert!(w.submitted(9, &Command::Resume, t).is_err());
    w.submitted(9, &Command::Stop, t).expect("priority stop");
    assert!(w.submitted(10, &Command::Stop, t).is_err());
    w.submitted(10, &Command::Abort, t).expect("priority abort");
    assert!(w.stopping());
    assert!(w.submitted(10, &Command::Abort, t).is_err());
  }
  #[test]
  fn launch_timing_starts_before_spawn_and_ends_at_the_first_recording_confirmation() {
    let before = Instant::now();
    let spawned = before + std::time::Duration::from_millis(30);
    let hello = before + std::time::Duration::from_millis(45);
    let mut watch = Watch::new(before);
    assert_eq!(watch.report().launch, None);
    assert!(watch.launched(spawned, before, hello).is_err());
    assert!(watch.launched(before, hello, spawned).is_err());
    watch
      .launched(before, spawned, hello)
      .expect("ordered launch");
    watch
      .observe(&Event::State(SessionState::Ready), hello)
      .expect("ready is not recording");
    assert_eq!(watch.report().launch.expect("timing").recording_us, None);
    let recording = before + std::time::Duration::from_millis(160);
    watch
      .observe(&Event::State(SessionState::Recording), recording)
      .expect("recording");
    // A later Resume confirmation must not move the launch milestone.
    watch
      .observe(
        &Event::State(SessionState::Recording),
        recording + std::time::Duration::from_secs(1),
      )
      .expect("resume");
    assert_eq!(
      watch.report().launch,
      Some(LaunchTiming {
        spawn_us: 30_000,
        hello_us: 45_000,
        recording_us: Some(160_000),
      })
    );
  }
}
