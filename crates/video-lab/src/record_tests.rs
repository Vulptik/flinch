//! Supervision fixtures substitute the private transport and never launch a recorder.
#![allow(clippy::expect_used)]
use super::*;
use std::{
  cell::RefCell,
  collections::VecDeque,
  sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
  },
};

#[derive(Clone, Copy, Default)]
enum Fault {
  #[default]
  None,
  WrongSession,
  WrongVersion,
  UnissuedAck,
  Stall,
  Cleanup,
  MissingHello,
  RejectedStop,
  BrokenPipe,
  DuplicatePrepared,
}
struct MockWorker {
  events: RefCell<VecDeque<protocol::Response>>,
  commands: Arc<Mutex<Vec<Command>>>,
  terminated: Arc<AtomicU32>,
  directory: PathBuf,
  output: PathBuf,
  session: String,
  fault: Fault,
}
impl MockWorker {
  fn new(path: &std::path::Path, fault: Fault) -> Self {
    let hello = protocol::Response {
      version: protocol::VERSION,
      session: String::new(),
      event: Event::Hello,
    };
    Self {
      events: RefCell::new(if matches!(fault, Fault::MissingHello) {
        VecDeque::new()
      } else {
        VecDeque::from([hello])
      }),
      commands: Arc::new(Mutex::new(Vec::new())),
      terminated: Arc::new(AtomicU32::new(0)),
      directory: path.to_owned(),
      output: path.join("fixture-output.mp4"),
      session: String::new(),
      fault,
    }
  }
  fn push(&self, event: Event) {
    self.events.borrow_mut().push_back(protocol::Response {
      version: protocol::VERSION,
      session: self.session.clone(),
      event,
    });
  }
}
impl WorkerTransport for MockWorker {
  fn send(&mut self, request: &Request) -> Result<()> {
    self
      .commands
      .lock()
      .expect("commands")
      .push(request.command.clone());
    self.session = request.session.clone();
    match &request.command {
      Command::Prepare(_) => {
        if matches!(
          self.fault,
          Fault::WrongSession | Fault::WrongVersion | Fault::UnissuedAck | Fault::Stall
        ) {
          let mut response = protocol::Response {
            version: protocol::VERSION,
            session: self.session.clone(),
            event: Event::Heartbeat,
          };
          match self.fault {
            Fault::WrongSession => response.session = "unrelated-session".into(),
            Fault::WrongVersion => response.version += 1,
            Fault::UnissuedAck => {
              response.event = Event::Ack {
                id: request.id + 1,
                state: SessionState::Ready,
              }
            },
            Fault::Stall => {
              response.event = Event::StageStalled(EngineError {
                native_code: Some(42),
                ..EngineError::new(
                  flinch_video_engine::ErrorKind::Timeout,
                  "scripted writer",
                  "native call stalled",
                )
              })
            },
            _ => {},
          }
          self.events.borrow_mut().push_back(response);
        } else {
          self.push(Event::Prepared(self.directory.clone()));
          if matches!(self.fault, Fault::DuplicatePrepared) {
            self.push(Event::Prepared(self.directory.join("second")));
          }
          self.push(Event::State(SessionState::Ready));
          self.push(Event::Ack {
            id: request.id,
            state: SessionState::Ready,
          });
        }
      },
      Command::Start => {
        self.push(Event::State(SessionState::Recording));
        self.push(Event::Stats(SessionSnapshot {
          state: SessionState::Recording,
          ..Default::default()
        }));
      },
      Command::Pause => self.push(Event::Ack {
        id: request.id,
        state: SessionState::Paused,
      }),
      Command::Resume | Command::Microphone(_) => self.push(Event::Ack {
        id: request.id,
        state: SessionState::Recording,
      }),
      Command::Stop if matches!(self.fault, Fault::RejectedStop) => self.push(Event::Rejected {
        id: request.id,
        error: EngineError {
          native_code: Some(44),
          ..EngineError::new(
            flinch_video_engine::ErrorKind::Native,
            "scripted Stop",
            "original Stop refusal",
          )
        },
      }),
      Command::Stop | Command::Abort => self.push(Event::Outcome(RecordingOutcome {
        thumbnail: None,
        kind: if matches!(request.command, Command::Abort) {
          OutcomeKind::Aborted
        } else {
          OutcomeKind::Completed
        },
        output: if matches!(request.command, Command::Abort) {
          None
        } else {
          Some(self.output.clone())
        },
        partial: None,
        error: None,
        snapshot: SessionSnapshot {
          state: if matches!(request.command, Command::Abort) {
            SessionState::Aborted
          } else {
            SessionState::Completed
          },
          ..Default::default()
        },
      })),
    }
    Ok(())
  }
  fn receive(&self, _: Duration) -> Result<Option<protocol::Response>> {
    if matches!(self.fault, Fault::BrokenPipe)
      && self
        .commands
        .lock()
        .expect("commands")
        .iter()
        .any(|c| matches!(c, Command::Start))
    {
      return Err(EngineError {
        native_code: Some(109),
        ..EngineError::new(
          flinch_video_engine::ErrorKind::Io,
          "worker events",
          "scripted broken pipe",
        )
      });
    }
    let response = self.events.borrow_mut().pop_front();
    if response.is_none() {
      std::thread::sleep(Duration::from_millis(1));
    }
    Ok(response)
  }
  fn wait(&mut self, _: Duration) -> Result<()> {
    if matches!(self.fault, Fault::Cleanup) {
      Err(EngineError::protocol("scripted exit failure"))
    } else {
      Ok(())
    }
  }
  fn terminate(&mut self) -> Result<()> {
    self.terminated.fetch_add(1, Ordering::AcqRel);
    Ok(())
  }
  fn pid(&self) -> u32 {
    42
  }
}
fn configuration(root: &std::path::Path, scenario: bool) -> Options {
  options(
    &["--output".into(), root.join("unused.mp4").into_os_string()],
    scenario,
  )
  .expect("explicit configuration")
}
#[test]
fn supervisor_exercises_timed_controls_and_preserves_its_terminal_report() {
  let root = tempfile::tempdir().expect("temp");
  let worker = MockWorker::new(root.path(), Fault::None);
  let commands = worker.commands.clone();
  let mut options = configuration(root.path(), true);
  options.duration = Duration::from_millis(80);
  let (_input, receiver) = std::sync::mpsc::sync_channel(1);
  let spawned = Instant::now();
  let before = spawned - Duration::from_millis(5);
  let mut directory = None;
  record_session_in(
    options,
    worker,
    receiver,
    || 0,
    Some((before, spawned)),
    &mut directory,
  )
  .expect("supervised session");
  assert_eq!(directory.as_deref(), Some(root.path()));
  let commands = commands.lock().expect("controls");
  assert!(matches!(commands[0], Command::Prepare(_)));
  assert!(matches!(commands[1], Command::Start));
  assert_eq!(
    commands
      .iter()
      .filter(|c| matches!(c, Command::Pause))
      .count(),
    2
  );
  assert_eq!(
    commands
      .iter()
      .filter(|c| matches!(c, Command::Resume))
      .count(),
    2
  );
  assert!(matches!(commands.last(), Some(Command::Stop)));
  let outcome: RecordingOutcome = serde_json::from_slice(
    &std::fs::read(root.path().join("supervisor-result.json")).expect("terminal report"),
  )
  .expect("outcome");
  assert_eq!(outcome.kind, OutcomeKind::Completed);
  assert!(outcome.error.is_none());
  let controls: control::ControlReport = serde_json::from_slice(
    &std::fs::read(root.path().join("supervisor-controls.json")).expect("control report"),
  )
  .expect("latency distributions");
  assert_eq!(
    (
      controls.prepare.count,
      controls.start.count,
      controls.pause.count,
      controls.resume.count,
      controls.stop.count
    ),
    (1, 1, 2, 2, 1)
  );
  let launch = controls.launch.expect("launch timing");
  let recording = launch.recording_us.expect("first Recording confirmation");
  assert!(
    launch.spawn_us >= 5_000 && launch.hello_us >= launch.spawn_us && recording >= launch.hello_us,
    "{launch:?}"
  );
}
#[test]
fn protocol_faults_and_stage_stalls_terminate_only_the_owned_transport() {
  for fault in [
    Fault::WrongSession,
    Fault::WrongVersion,
    Fault::UnissuedAck,
    Fault::Stall,
    Fault::MissingHello,
  ] {
    let root = tempfile::tempdir().expect("temp");
    let worker = MockWorker::new(root.path(), fault);
    let terminated = worker.terminated.clone();
    let (_input, receiver) = std::sync::mpsc::sync_channel(1);
    let error = record_session(configuration(root.path(), false), worker, receiver, || 0)
      .expect_err("fault must fail");
    if matches!(fault, Fault::MissingHello) {
      assert_eq!(error.kind, flinch_video_engine::ErrorKind::Timeout);
      assert_eq!(error.stage, "worker connection");
    } else {
      assert_eq!(terminated.load(Ordering::Acquire), 1);
      if matches!(fault, Fault::Stall) {
        assert_eq!(error.native_code, Some(42));
      } else {
        assert_eq!(error.kind, flinch_video_engine::ErrorKind::Protocol);
      }
    }
  }
}
#[test]
fn failed_worker_cleanup_keeps_the_completed_file_in_a_degraded_outcome() {
  let root = tempfile::tempdir().expect("temp");
  let worker = MockWorker::new(root.path(), Fault::Cleanup);
  let output = worker.output.clone();
  let terminated = worker.terminated.clone();
  let mut options = configuration(root.path(), false);
  options.duration = Duration::ZERO;
  let (_input, receiver) = std::sync::mpsc::sync_channel(1);
  let error = record_session(options, worker, receiver, || 0).expect_err("cleanup failure");
  assert!(error.reason.contains("scripted exit failure"));
  assert_eq!(error.recoverable_path, Some(output.clone()));
  assert_eq!(terminated.load(Ordering::Acquire), 1);
  let outcome: RecordingOutcome = serde_json::from_slice(
    &std::fs::read(root.path().join("supervisor-result.json")).expect("report"),
  )
  .expect("outcome");
  assert_eq!(outcome.kind, OutcomeKind::Degraded);
  assert_eq!(outcome.output, Some(output));
}
#[test]
fn terminal_refusal_keeps_its_native_cause_and_last_control_counters_in_the_report() {
  let root = tempfile::tempdir().expect("temp");
  let worker = MockWorker::new(root.path(), Fault::RejectedStop);
  let terminated = worker.terminated.clone();
  let mut options = configuration(root.path(), false);
  options.duration = Duration::from_millis(10);
  let (_input, receiver) = std::sync::mpsc::sync_channel(1);
  let error = record_session(options, worker, receiver, || 0).expect_err("terminal refusal");
  assert_eq!(
    (error.native_code, error.stage.as_str()),
    (Some(44), "scripted Stop")
  );
  assert_eq!(terminated.load(Ordering::Acquire), 1);
  let controls: control::ControlReport = serde_json::from_slice(
    &std::fs::read(root.path().join("supervisor-controls.json")).expect("final controls"),
  )
  .expect("report");
  assert_eq!(controls.rejected_requests, 1);
  assert_eq!(
    controls.stop.count, 0,
    "a refusal is not a successful Stop confirmation"
  );
}
#[test]
fn repeated_console_interrupts_escalate_to_abort_without_waiting_for_game_input() {
  let root = tempfile::tempdir().expect("temp");
  let worker = MockWorker::new(root.path(), Fault::None);
  let commands = worker.commands.clone();
  let (_input, receiver) = std::sync::mpsc::sync_channel(1);
  let mut signal = 2;
  record_session(configuration(root.path(), false), worker, receiver, || {
    std::mem::take(&mut signal)
  })
  .expect("explicit abandon");
  assert!(
    commands
      .lock()
      .expect("commands")
      .iter()
      .any(|c| matches!(c, Command::Abort))
  );
}
#[test]
fn offline_cli_routes_inventory_calibration_and_reports_without_opening_a_capture_source() {
  assert!(run(vec!["--lang".into()]).is_err());
  assert!(run(vec!["--lang".into(), "invalid-locale".into()]).is_err());
  dispatch(&["help".into()]).expect("CLI help");
  dispatch(&["doctor".into()]).expect("read-only capability inventory");
  dispatch(&["calibrate-aac".into()]).expect("software-only calibration");
  for args in [
    vec!["unknown".into()],
    vec!["verify".into()],
    vec!["recover".into()],
    vec!["recover".into(), "input.mp4".into()],
    vec!["record".into()],
    vec!["compare".into()],
  ] {
    assert!(dispatch(&args).is_err());
  }
  let root = tempfile::tempdir().expect("temp");
  let input = root.path().join("invalid.mp4");
  std::fs::write(&input, b"invalid container").expect("negative fixture");
  assert!(dispatch(&["verify".into(), input.clone().into_os_string()]).is_err());
  assert!(
    dispatch(&[
      "recover".into(),
      input.into_os_string(),
      root.path().join("new.mp4").into_os_string()
    ])
    .is_err()
  );
  let outcome = RecordingOutcome {
    thumbnail: None,
    kind: OutcomeKind::Aborted,
    output: None,
    partial: None,
    error: None,
    snapshot: SessionSnapshot {
      state: SessionState::Aborted,
      ..Default::default()
    },
  };
  let path = root.path().join("outcome.json");
  std::fs::write(
    &path,
    serde_json::to_vec(&outcome).expect("serialize fixture"),
  )
  .expect("report fixture");
  dispatch(&[
    "compare".into(),
    path.clone().into_os_string(),
    path.into_os_string(),
  ])
  .expect("comparison must keep game impact unmeasured");
}
#[test]
fn broken_transport_and_duplicate_session_confirmation_fail_with_one_owned_termination() {
  for fault in [Fault::BrokenPipe, Fault::DuplicatePrepared] {
    let root = tempfile::tempdir().expect("temp");
    let worker = MockWorker::new(root.path(), fault);
    let terminated = worker.terminated.clone();
    let (_input, receiver) = std::sync::mpsc::sync_channel(1);
    let error = record_session(configuration(root.path(), false), worker, receiver, || 0)
      .expect_err("transport fault must fail");
    assert_eq!(terminated.load(Ordering::Acquire), 1);
    match fault {
      Fault::BrokenPipe => {
        assert_eq!(
          (error.native_code, error.stage.as_str()),
          (Some(109), "worker events")
        );
      },
      _ => assert!(error.reason.contains("already confirmed"), "{error}"),
    }
    let outcome: RecordingOutcome = serde_json::from_slice(
      &std::fs::read(root.path().join("supervisor-result.json")).expect("failure report"),
    )
    .expect("outcome");
    assert_eq!(outcome.kind, OutcomeKind::Failed);
    assert_eq!(outcome.snapshot.state, SessionState::Failed);
  }
}
#[test]
fn fault_injection_kills_only_the_owned_worker_and_points_at_its_partial() {
  let root = tempfile::tempdir().expect("temp");
  let worker = MockWorker::new(root.path(), Fault::None);
  let terminated = worker.terminated.clone();
  let commands = worker.commands.clone();
  let mut options = configuration(root.path(), true);
  options.duration = Duration::from_secs(30);
  options.interrupt_after = Some(Duration::from_millis(5));
  let (_input, receiver) = std::sync::mpsc::sync_channel(1);
  let error = record_session(options, worker, receiver, || 0).expect_err("deliberate fault");
  assert_eq!(error.kind, flinch_video_engine::ErrorKind::Cancelled);
  assert_eq!(error.stage, "scenario fault injection");
  assert_eq!(
    error.recoverable_path,
    Some(root.path().join("capture.partial.mp4"))
  );
  assert_eq!(terminated.load(Ordering::Acquire), 1);
  assert!(
    !commands
      .lock()
      .expect("commands")
      .iter()
      .any(|c| matches!(c, Command::Stop | Command::Abort)),
    "fault injection must not be disguised as a normal Stop"
  );
  let outcome: RecordingOutcome = serde_json::from_slice(
    &std::fs::read(root.path().join("supervisor-result.json")).expect("aborted report"),
  )
  .expect("outcome");
  assert_eq!(outcome.kind, OutcomeKind::Aborted);
}
#[test]
fn console_controls_during_a_pending_stop_are_refused_locally_without_reaching_the_worker() {
  let root = tempfile::tempdir().expect("temp");
  let worker = MockWorker::new(root.path(), Fault::None);
  let commands = worker.commands.clone();
  let observed = commands.clone();
  let mut options = configuration(root.path(), false);
  options.duration = Duration::ZERO;
  let (input, receiver) = std::sync::mpsc::sync_channel(1);
  let mut queued = false;
  record_session(options, worker, receiver, || {
    let stopping = observed
      .lock()
      .expect("commands")
      .iter()
      .any(|c| matches!(c, Command::Stop));
    if stopping && !queued {
      input.try_send(Command::Pause).expect("console command");
      queued = true;
    }
    0
  })
  .expect("Stop still completes");
  assert!(
    queued,
    "the console command was offered while Stop was pending"
  );
  assert!(
    !commands
      .lock()
      .expect("commands")
      .iter()
      .any(|c| matches!(c, Command::Pause)),
    "a refused control must not reach the worker"
  );
}
#[test]
fn fixture_requests_are_validated_before_any_window_or_worker_and_never_replace_reports() {
  let root = tempfile::tempdir().expect("temp");
  let report = root.path().join("fixture.json");
  for args in [
    vec![],
    vec!["5".into()],
    vec!["NaN".into(), report.clone().into_os_string()],
    vec!["abc".into(), report.clone().into_os_string()],
    vec!["0.01".into(), report.clone().into_os_string()],
    vec!["10831".into(), report.clone().into_os_string()],
  ] {
    let args: Vec<OsString> = [vec!["fixture".into()], args].concat();
    let error = dispatch(&args).expect_err("invalid fixture request");
    assert_eq!(error.kind, flinch_video_engine::ErrorKind::Protocol);
    assert!(!report.exists());
  }
  std::fs::write(&report, b"earlier report").expect("existing report");
  let error = dispatch(&[
    "fixture".into(),
    "5".into(),
    report.clone().into_os_string(),
  ])
  .expect_err("existing destination");
  assert!(error.reason.contains("already exists"));
  assert_eq!(
    std::fs::read(&report).expect("unchanged"),
    b"earlier report"
  );
  for args in [
    vec![],
    vec!["5".into(), report.clone().into_os_string()],
    vec![
      "inf".into(),
      report.clone().into_os_string(),
      "title".into(),
    ],
    vec!["x".into(), report.clone().into_os_string(), "title".into()],
    vec!["0".into(), report.clone().into_os_string(), "title".into()],
  ] {
    let args: Vec<OsString> = [vec!["--fixture-worker".into()], args].concat();
    let error = dispatch(&args).expect_err("invalid private fixture request");
    assert_eq!(error.kind, flinch_video_engine::ErrorKind::Protocol);
  }
  assert!(
    verification_worker_output(&["only-one".into()], &mut Vec::new())
      .expect_err("private verifier arity")
      .reason
      .contains("Invalid private verification request")
  );
}
#[test]
fn independent_fixture_evaluation_requires_markers_and_a_readable_bounded_report() {
  let root = tempfile::tempdir().expect("temp");
  let media = root.path().join("absent.mp4").into_os_string();
  let report = root.path().join("absent.json").into_os_string();
  for args in [
    vec![
      "verify".into(),
      media.clone(),
      "--decode".into(),
      "--fixture".into(),
      report.clone(),
    ],
    vec![
      "verify".into(),
      media.clone(),
      "--markers".into(),
      "--report".into(),
      report.clone(),
    ],
    vec![
      "verify".into(),
      media.clone(),
      "--markers".into(),
      "--fixture".into(),
    ],
  ] {
    assert!(
      dispatch(&args)
        .expect_err("invalid independent request")
        .reason
        .contains("--fixture REPORT.json")
    );
  }
  let error = dispatch(&[
    "verify".into(),
    media,
    "--markers".into(),
    "--fixture".into(),
    report,
  ])
  .expect_err("missing fixture report");
  assert_eq!(error.stage, "fixture report");
}
#[test]
fn prewarmed_session_holds_ready_for_its_idle_window_and_sends_start_once() {
  let root = tempfile::tempdir().expect("temp");
  let worker = MockWorker::new(root.path(), Fault::None);
  let commands = worker.commands.clone();
  let mut options = configuration(root.path(), false);
  options.duration = Duration::from_millis(20);
  options.prewarm_idle = Duration::from_millis(80);
  let (_input, receiver) = std::sync::mpsc::sync_channel(1);
  let started = Instant::now();
  record_session(options, worker, receiver, || 0).expect("pre-warmed session");
  assert!(started.elapsed() >= Duration::from_millis(100));
  let commands = commands.lock().expect("controls");
  // Ready arrives as a state event and an acknowledgement; neither may send a second Start.
  assert_eq!(
    commands
      .iter()
      .filter(|command| matches!(command, Command::Start))
      .count(),
    1
  );
  assert!(matches!(commands[0], Command::Prepare(_)));
  assert!(matches!(commands[1], Command::Start));
  for invalid in ["-1", "601", "NaN"] {
    assert!(
      options_from(&["--output", "x.mp4", "--prewarm-idle", invalid]).is_err(),
      "{invalid}"
    );
  }
  assert_eq!(
    options_from(&["--output", "x.mp4", "--prewarm-idle", "2.5"])
      .expect("bounded idle")
      .prewarm_idle,
    Duration::from_millis(2500)
  );
}
fn options_from(args: &[&str]) -> Result<Options> {
  options(&args.iter().map(OsString::from).collect::<Vec<_>>(), false)
}
#[test]
fn region_option_parses_a_display_relative_even_crop_and_refuses_malformed_text() {
  assert_eq!(
    options_from(&["--output", "x.mp4", "--region", "100, 50,640,360"])
      .expect("region")
      .spec
      .region,
    Some(Region {
      x: 100,
      y: 50,
      width: 640,
      height: 360
    })
  );
  for bad in [
    "1,2,3",
    "1,2,640,360,9",
    "a,0,640,360",
    "0,0,641,360",
    "0,0,8,8",
    "",
  ] {
    assert!(
      options_from(&["--output", "x.mp4", "--region", bad]).is_err(),
      "{bad}"
    );
  }
  assert_eq!(
    options_from(&["--output", "x.mp4"])
      .expect("default")
      .spec
      .region,
    None
  );
}

#[test]
fn host_smoke_drives_the_lab_worker_or_an_explicit_app_binary() {
  let args: Vec<OsString> = ["--app-worker", r"C:\apps\flinch.exe", "--seconds", "2"]
    .into_iter()
    .map(OsString::from)
    .collect();
  let (executable, switch, rest) = host_worker(&args).expect("app worker");
  assert_eq!(
    (executable, switch, rest.len()),
    (
      std::path::PathBuf::from(r"C:\apps\flinch.exe"),
      APP_WORKER_SWITCH,
      2
    )
  );
  let own = [OsString::from("--seconds")];
  let (executable, switch, rest) = host_worker(&own).expect("lab worker");
  assert_eq!((switch, rest.len()), ("--worker", 1));
  assert_eq!(
    executable,
    std::env::current_exe().expect("test executable")
  );
  assert!(host_worker(&[OsString::from("--app-worker")]).is_err());
}

#[test]
fn gif_command_options_name_each_native_dither_and_refuse_unknown_ones() {
  use flinch_video_engine::gif::Dither;
  for (value, dither) in [
    (None, Dither::Sierra24a),
    (Some("sierra"), Dither::Sierra24a),
    (Some("none"), Dither::None),
    (Some("floyd"), Dither::FloydSteinberg),
    (Some("bayer:3"), Dither::Bayer { scale: 3 }),
  ] {
    assert_eq!(gif_options(value).expect("dither").dither, dither);
  }
  for bad in ["bayer", "bayer:x", "atkinson"] {
    assert!(gif_options(Some(bad)).is_err(), "{bad}");
  }
  assert!(gif_command(&[OsString::from("only-input.mp4")]).is_err());
}
