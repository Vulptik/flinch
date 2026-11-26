//! The host pump against a scripted worker: no process, pipe, capture or encoder.
#![allow(clippy::expect_used)]
use super::*;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Default)]
struct Script {
  events: Mutex<VecDeque<Response>>,
  commands: Mutex<Vec<Command>>,
  silent: AtomicBool,
  reject_pause: AtomicBool,
  fail_prepare: AtomicBool,
  foreign_session: AtomicBool,
  terminated: AtomicU32,
  waited: AtomicU32,
  diagnostics: Mutex<VecDeque<String>>,
}
struct Worker(Arc<Script>);
impl Worker {
  fn push(&self, session: &str, event: Event) {
    self.0.events.lock().expect("events").push_back(Response {
      version: protocol::VERSION,
      session: session.into(),
      event,
    });
  }
}
impl Transport for Worker {
  fn send(&mut self, request: &Request) -> Result<()> {
    self
      .0
      .commands
      .lock()
      .expect("commands")
      .push(request.command.clone());
    if self.0.silent.load(Ordering::Acquire) {
      return Ok(());
    }
    let session = if self.0.foreign_session.load(Ordering::Acquire) {
      "someone-else"
    } else {
      request.session.as_str()
    };
    let id = request.id;
    match &request.command {
      // The worker ends a failed Prepare with its terminal outcome only, never a rejection.
      Command::Prepare(_) if self.0.fail_prepare.load(Ordering::Acquire) => {
        let mut error = EngineError::new(
          ErrorKind::Capability,
          crate::types::CAPTURE_UNAVAILABLE_STAGE,
          "every capture backend refused",
        );
        error.recoverable_path = Some(PathBuf::from("session-dir/capture.partial.mp4"));
        self.push(
          session,
          Event::Outcome(RecordingOutcome {
            thumbnail: None,
            kind: OutcomeKind::Failed,
            output: None,
            partial: None,
            error: Some(error),
            snapshot: SessionSnapshot::default(),
          }),
        );
      },
      Command::Prepare(_) => {
        self.push(session, Event::Prepared(PathBuf::from("session-dir")));
        self.push(session, Event::State(SessionState::Ready));
        self.push(
          session,
          Event::Ack {
            id,
            state: SessionState::Ready,
          },
        );
      },
      Command::Start => {
        self.push(
          session,
          Event::Ack {
            id,
            state: SessionState::Starting,
          },
        );
        self.push(
          session,
          Event::Stats(SessionSnapshot {
            state: SessionState::Recording,
            frames_encoded: 7,
            ..Default::default()
          }),
        );
        self.push(session, Event::State(SessionState::Recording));
      },
      Command::Pause if self.0.reject_pause.load(Ordering::Acquire) => self.push(
        session,
        Event::Rejected {
          id,
          error: EngineError::new(ErrorKind::InvalidState, "worker", "scripted refusal"),
        },
      ),
      Command::Pause => self.push(
        session,
        Event::Ack {
          id,
          state: SessionState::Paused,
        },
      ),
      Command::Resume => self.push(
        session,
        Event::Ack {
          id,
          state: SessionState::Recording,
        },
      ),
      // The scripted session records; a microphone switch keeps its state.
      Command::Microphone(_) => self.push(
        session,
        Event::Ack {
          id,
          state: SessionState::Recording,
        },
      ),
      Command::Stop | Command::Abort => {
        self.push(session, Event::Diagnostic(EngineError::protocol("closing")));
        self.push(
          session,
          Event::Outcome(RecordingOutcome {
            thumbnail: None,
            kind: if matches!(request.command, Command::Stop) {
              OutcomeKind::Completed
            } else {
              OutcomeKind::Aborted
            },
            output: Some(PathBuf::from("final.mp4")),
            partial: None,
            error: None,
            snapshot: SessionSnapshot {
              frames_encoded: 120,
              ..Default::default()
            },
          }),
        );
      },
    }
    Ok(())
  }
  fn receive(&self, timeout: Duration) -> Result<Option<Response>> {
    let next = self.0.events.lock().expect("events").pop_front();
    if next.is_none() {
      std::thread::sleep(timeout.min(Duration::from_millis(2)));
    }
    Ok(next)
  }
  fn wait(&mut self, _: Duration) -> Result<()> {
    self.0.waited.fetch_add(1, Ordering::AcqRel);
    Ok(())
  }
  fn terminate(&mut self) -> Result<()> {
    self.0.terminated.fetch_add(1, Ordering::AcqRel);
    Ok(())
  }
  fn diagnostic(&self) -> Option<String> {
    self.0.diagnostics.lock().expect("diagnostics").pop_front()
  }
}
fn launch() -> (HostSession, Arc<Script>) {
  let script = Arc::new(Script::default());
  let worker = Worker(script.clone());
  worker.push("", Event::Hello);
  (HostSession::with_transport(worker).expect("hello"), script)
}
fn spec() -> SessionSpec {
  SessionSpec {
    monitor: "primary".into(),
    fps: 30,
    capture: CaptureKind::Wgc,
    encoder: EncoderKind::Auto,
    quality: Quality::Cqp { i: 23, p: 23 },
    cursor: true,
    clicks: false,
    audio: false,
    audio_device: None,
    microphone: false,
    microphone_device: None,
    output: "final.mp4".into(),
    trace_frames: false,
    region: None,
    span: Vec::new(),
  }
}

#[test]
fn host_session_runs_the_whole_lifecycle_and_returns_the_terminal_outcome() {
  let (host, script) = launch();
  assert_eq!(
    host.prepare(spec()).expect("ready"),
    PathBuf::from("session-dir")
  );
  host.start().expect("recording at first written frame");
  assert_eq!(host.snapshot().frames_encoded, 7);
  host.microphone(false).expect("microphone muted");
  host.pause().expect("paused");
  host.resume().expect("resumed");
  host.microphone(true).expect("microphone audible");
  assert_eq!(host.snapshot().frames_encoded, 7);
  let outcome = host.stop().expect("terminal outcome");
  assert_eq!(outcome.kind, OutcomeKind::Completed);
  assert_eq!(outcome.output, Some(PathBuf::from("final.mp4")));
  assert_eq!(host.snapshot().frames_encoded, 120);
  assert!(host.terminal_outcome().is_some());
  assert!(host.failure().is_none());
  let controls = host.controls();
  assert_eq!(
    (
      controls.prepare.count,
      controls.start.count,
      controls.pause.count,
      controls.resume.count
    ),
    (1, 1, 1, 1)
  );
  assert_eq!(
    host.take_diagnostics().0,
    ["protocol: closing (Protocol)".to_string()]
  );
  // Controls after the terminal result fail explicitly instead of hanging.
  assert!(host.pause().is_err());
  drop(host);
  assert_eq!(script.waited.load(Ordering::Acquire), 1);
  assert_eq!(script.terminated.load(Ordering::Acquire), 0);
  let commands = script.commands.lock().expect("commands");
  assert!(!commands.iter().any(|c| matches!(c, Command::Abort)));
}

#[test]
fn a_control_ended_by_a_failed_outcome_reports_the_outcome_cause() {
  let (host, script) = launch();
  script.fail_prepare.store(true, Ordering::Release);
  let error = host.prepare(spec()).expect_err("failed prepare");
  assert!(error.capture_unavailable(), "{error:?}");
  assert_eq!(error.reason, "every capture backend refused");
  assert_eq!(
    error.recoverable_path,
    Some(PathBuf::from("session-dir/capture.partial.mp4"))
  );
}

#[test]
fn a_refused_control_is_answered_and_the_session_continues() {
  let (host, script) = launch();
  host.prepare(spec()).expect("ready");
  host.start().expect("recording");
  script.reject_pause.store(true, Ordering::Release);
  let error = host.pause().expect_err("worker refusal");
  assert_eq!(
    (error.kind, error.reason.as_str()),
    (ErrorKind::InvalidState, "scripted refusal")
  );
  script.reject_pause.store(false, Ordering::Release);
  host.pause().expect("later pause");
  assert_eq!(host.stop().expect("stop").kind, OutcomeKind::Completed);
}

#[test]
fn a_request_the_pump_abandons_reports_the_recorded_failure_or_a_stopped_pump() {
  // A pump that ends after accepting a request drops its reply unanswered.
  let (requests, commands) = mpsc::sync_channel::<(Command, SyncSender<Result<Reply>>)>(4);
  let pump = std::thread::spawn(move || {
    while let Ok((_command, reply)) = commands.recv() {
      drop(reply);
    }
  });
  let host = HostSession {
    requests: Some(requests),
    shared: Arc::new(Mutex::new(Shared::default())),
    pump: Some(pump),
  };
  let error = host.pause().expect_err("abandoned request");
  assert_eq!(
    (error.kind, error.reason.as_str()),
    (ErrorKind::Protocol, "Host session pump has stopped")
  );
  host.shared.lock().expect("shared").failure = Some(EngineError::new(
    ErrorKind::DeviceLost,
    "scripted worker",
    "worker exited",
  ));
  let error = host.resume().expect_err("abandoned request");
  assert_eq!(
    (error.kind, error.reason.as_str()),
    (ErrorKind::DeviceLost, "worker exited")
  );
}

#[test]
fn a_worker_that_stops_answering_is_terminated_within_its_deadline() {
  let (host, script) = launch();
  host.prepare(spec()).expect("ready");
  script.silent.store(true, Ordering::Release);
  let started = Instant::now();
  let error = host.pause().expect_err("silent worker");
  // Bounded by the heartbeat (3 s) or the Pause deadline (2 s), not by the caller.
  assert!(
    started.elapsed() < limits::HELLO_TIMEOUT + REPLY_MARGIN,
    "{:?}",
    started.elapsed()
  );
  assert_eq!(error.kind, ErrorKind::Timeout);
  assert_eq!(
    error.recoverable_path,
    Some(PathBuf::from("session-dir").join("capture.partial.mp4"))
  );
  assert_eq!(script.terminated.load(Ordering::Acquire), 1);
  assert_eq!(
    host.failure().map(|failure| failure.kind),
    Some(ErrorKind::Timeout)
  );
  // The recorded failure answers every later call.
  assert_eq!(
    host.start().expect_err("stopped pump").kind,
    ErrorKind::Timeout
  );
}

#[test]
fn dropping_an_active_session_aborts_the_worker_before_returning() {
  let (host, script) = launch();
  host.prepare(spec()).expect("ready");
  host.start().expect("recording");
  drop(host);
  let commands = script.commands.lock().expect("commands");
  assert!(matches!(commands.last(), Some(Command::Abort)));
  assert_eq!(script.waited.load(Ordering::Acquire), 1);
}

#[test]
fn protocol_violations_and_bad_hellos_fail_explicitly() {
  let (host, script) = launch();
  script.foreign_session.store(true, Ordering::Release);
  let error = host.prepare(spec()).expect_err("foreign session");
  assert!(error.reason.contains("session identity"), "{error}");
  assert_eq!(script.terminated.load(Ordering::Acquire), 1);

  for hello in [
    Response {
      version: protocol::VERSION + 1,
      session: String::new(),
      event: Event::Hello,
    },
    Response {
      version: protocol::VERSION,
      session: "early".into(),
      event: Event::Hello,
    },
    Response {
      version: protocol::VERSION,
      session: String::new(),
      event: Event::Heartbeat,
    },
  ] {
    let script = Arc::new(Script::default());
    script.events.lock().expect("events").push_back(hello);
    assert!(HostSession::with_transport(Worker(script)).is_err());
  }
  let started = Instant::now();
  let error = HostSession::with_transport(Worker(Arc::new(Script::default())))
    .err()
    .expect("no hello");
  assert_eq!(error.kind, ErrorKind::Timeout);
  assert!(started.elapsed() < limits::HELLO_TIMEOUT + Duration::from_secs(1));
}

/// Regression for the coverage gate: the pump only forwarded worker stderr when a real worker
/// happened to write some, so the measured lines differed per machine.
#[test]
fn the_pump_forwards_worker_diagnostics_in_order() {
  let (host, script) = launch();
  script
    .diagnostics
    .lock()
    .expect("diagnostics")
    .extend(["first stderr line".to_string(), "second".to_string()]);
  // The pump drains diagnostics in the same pass that sends Prepare, before the reply returns.
  host.prepare(spec()).expect("ready");
  assert_eq!(
    host.take_diagnostics(),
    (
      vec!["first stderr line".to_string(), "second".to_string()],
      0
    )
  );
  assert!(script.diagnostics.lock().expect("diagnostics").is_empty());
}
#[test]
fn diagnostics_are_bounded_and_count_what_they_drop() {
  let shared = Arc::new(Mutex::new(Shared::default()));
  for index in 0..DIAGNOSTICS + 5 {
    diagnostic(&shared, format!("message {index}"));
  }
  let host = HostSession {
    requests: None,
    shared,
    pump: None,
  };
  let (messages, lost) = host.take_diagnostics();
  assert_eq!((messages.len(), lost), (DIAGNOSTICS, 5));
  assert_eq!(messages[0], "message 5");
  assert_eq!(host.take_diagnostics(), (Vec::new(), 0));
  assert!(host.start().is_err());
}
