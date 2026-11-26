//! Standalone developer lab. No installation, app configuration or global hooks.
mod comparison;
mod console;
mod freeze_latency;
pub mod i18n;
mod overlay_latency;
mod provenance;
mod reencode;
mod resources;
use flinch_video_engine::client::control;
use flinch_video_engine::{
  EngineError, Result,
  protocol::{self, Command, Event, Request},
  types::*,
};
use std::{
  ffi::OsString,
  io::{BufRead, Write},
  path::PathBuf,
  time::{Duration, Instant},
};

fn print_json(value: &impl serde::Serialize) -> Result<()> {
  print_json_before(
    value,
    Instant::now() + flinch_video_engine::limits::CONTROL_TIMEOUT,
  )
}
fn print_json_before(value: &impl serde::Serialize, deadline: Instant) -> Result<()> {
  let bytes = serde_json::to_vec_pretty(value).map_err(|e| EngineError::protocol(e.to_string()))?;
  resources::bounded_io(
    "report output",
    resources::remaining(deadline, "report output")?
      .min(flinch_video_engine::limits::CONTROL_TIMEOUT),
    move || {
      let mut output = std::io::stdout().lock();
      output
        .write_all(&bytes)
        .and_then(|_| output.write_all(b"\n"))
        .and_then(|_| output.flush())
        .map_err(|e| EngineError::io("report output", e))
    },
  )
}
pub fn report_error(error: &EngineError) {
  let message = format!("{}: {error}\n", i18n::text("video_lab_error"));
  // No fallback synchronous write: a blocked consumer must not prevent process exit.
  let _ = resources::bounded_io(
    "terminal error output",
    flinch_video_engine::limits::CONTROL_TIMEOUT,
    move || {
      std::io::stderr()
        .write_all(message.as_bytes())
        .map_err(|e| EngineError::io("terminal error output", e))
    },
  );
}
fn verification_worker(args: &[OsString]) -> Result<()> {
  verification_worker_output(args, &mut std::io::stdout().lock())
}
fn verification_worker_output(args: &[OsString], output: &mut impl Write) -> Result<()> {
  if args.len() != 3 {
    return Err(EngineError::protocol(
      "Invalid private verification request",
    ));
  }
  let emit = |out: &mut _, event| {
    protocol::write(
      out,
      &protocol::Response {
        version: protocol::VERSION,
        session: "verification".into(),
        event,
      },
    )
  };
  emit(output, Event::Hello)?;
  let report = match flinch_video_engine::windows::verify::verify(
    &PathBuf::from(&args[0]),
    args[2] == "--markers",
  ) {
    Ok(report) => report,
    Err(error) => {
      emit(output, Event::Diagnostic(error.clone()))?;
      return Err(error);
    },
  };
  let path = PathBuf::from(&args[1]);
  if let Err(error) = flinch_video_engine::artifact::write_json(&path, &report) {
    emit(output, Event::Diagnostic(error.clone()))?;
    return Err(error);
  }
  emit(output, Event::Verification(path))
}
fn supervised_verification(
  path: &std::path::Path,
  markers: bool,
) -> Result<flinch_video_engine::windows::verify::Verification> {
  let structure = flinch_video_engine::recovery::inspect(path)?;
  let nonce = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_err(|e| EngineError::protocol(e.to_string()))?
    .as_nanos();
  let parent = std::env::current_dir()
    .map_err(|e| EngineError::io("verification workspace", e))?
    .join("target/video-lab-verification");
  std::fs::create_dir_all(&parent).map_err(|e| EngineError::io("verification directory", e))?;
  let directory = parent.join(format!("{}-{nonce}", std::process::id()));
  std::fs::create_dir(&directory).map_err(|e| EngineError::io("verification session", e))?;
  let report_path = directory.join("verification.json");
  let executable =
    std::env::current_exe().map_err(|e| EngineError::io("verification executable", e))?;
  let mut worker = flinch_video_engine::windows::supervisor::WorkerProcess::spawn_verifier(
    &executable,
    path,
    &report_path,
    markers,
  )?;
  let deadline = Instant::now()
    + Duration::from_secs((structure.duration_100ns / 10_000_000 * 2 + 60).min(21660));
  loop {
    if Instant::now() >= deadline {
      return Err(EngineError::new(
        flinch_video_engine::ErrorKind::Timeout,
        "verification supervisor",
        "Offline native decoder exceeded its process deadline",
      ));
    }
    if let Some(response) = worker.receive(Duration::from_millis(100))? {
      if response.version != protocol::VERSION || response.session != "verification" {
        return Err(EngineError::protocol("Invalid verifier response"));
      }
      match response.event {
        Event::Hello => {},
        Event::Diagnostic(error) => return Err(error),
        Event::Verification(returned) => {
          if returned != report_path {
            return Err(EngineError::protocol("Verifier report destination changed"));
          }
          worker.wait(Duration::from_secs(3))?;
          let file = std::fs::File::open(&report_path)
            .map_err(|e| EngineError::io("verification report", e))?;
          if file
            .metadata()
            .map_err(|e| EngineError::io("verification report size", e))?
            .len()
            > 4 * 1024 * 1024
          {
            return Err(EngineError::protocol("Verification report exceeds 4 MiB"));
          }
          return serde_json::from_reader(file).map_err(|e| EngineError::protocol(e.to_string()));
        },
        _ => return Err(EngineError::protocol("Unexpected verifier event")),
      }
    }
  }
}
pub fn run(mut args: Vec<OsString>) -> Result<()> {
  let locale = if args.first().is_some_and(|a| a == "--lang") {
    if args.len() < 2 {
      return Err(EngineError::protocol("Missing --lang value"));
    }
    let locale = args[1]
      .to_str()
      .ok_or_else(|| EngineError::protocol("Invalid language"))?
      .to_string();
    args.drain(..2);
    locale
  } else {
    "en".into()
  };
  i18n::initialize(&locale)?;
  dispatch(&args)
}
fn dispatch(args: &[OsString]) -> Result<()> {
  let command = args.first().and_then(|s| s.to_str()).unwrap_or("help");
  match command {
    "--worker" => flinch_video_engine::windows::runtime::worker(),
    "--verify-worker" => verification_worker(&args[1..]),
    "--fixture-worker" => fixture_worker(&args[1..]),
    "fixture" => fixture_command(&args[1..]),
    "doctor" => print_json(&flinch_video_engine::windows::gpu::doctor()?),
    "calibrate-aac" => print_json(&flinch_video_engine::windows::calibration::calibrate()?),
    "verify" => {
      let path = args
        .get(1)
        .ok_or_else(|| EngineError::protocol("verify requires an input file"))?;
      let markers = args.get(2).is_some_and(|s| s == "--markers");
      let fixture = match args.get(3..) {
        None | Some([]) => None,
        Some([flag, report]) if markers && flag == "--fixture" => {
          Some(read_fixture_report(&PathBuf::from(report))?)
        },
        Some(_) => {
          return Err(EngineError::protocol(
            "verify accepts INPUT [--markers [--fixture REPORT.json]]",
          ));
        },
      };
      let mut report = supervised_verification(&PathBuf::from(path), markers)?;
      if let Some(fixture) = &fixture
        && report.structure.tracks == 2
      {
        let sync = flinch_video_engine::windows::verify::evaluate_independent(&report, fixture)?;
        flinch_video_engine::windows::verify::apply_independent(&mut report, sync);
      }
      print_json(&report)?;
      if markers && report.structure.tracks == 2 && !report.accepted_sync {
        Err(EngineError::protocol(
          report
            .sync_failure
            .as_deref()
            .unwrap_or("Decoded synchronization markers exceed acceptance limits"),
        ))
      } else {
        Ok(())
      }
    },
    "recover" => {
      let input = args
        .get(1)
        .ok_or_else(|| EngineError::protocol("recover requires input and output files"))?;
      let output = args
        .get(2)
        .ok_or_else(|| EngineError::protocol("recover requires a new output file"))?;
      print_json(&flinch_video_engine::recovery::recover(
        &PathBuf::from(input),
        &PathBuf::from(output),
      )?)
    },
    "record" | "scenario" => record(&args[1..], command == "scenario"),
    "compare" => compare(&args[1..]),
    "overlay-latency" => overlay_latency::command(&args[1..]),
    "freeze-latency" => freeze_latency::command(&args[1..]),
    "host-smoke" => host_smoke(&args[1..]),
    "gif" => gif_command(&args[1..]),
    "reencode" => reencode::command(&args[1..]),
    "gif-fidelity" => match &args[1..] {
      [source, gif] => print_json(&flinch_video_engine::windows::gif::fidelity(
        std::path::Path::new(source),
        std::path::Path::new(gif),
      )?),
      _ => Err(EngineError::protocol(
        "gif-fidelity requires SOURCE.mp4 and GIF",
      )),
    },
    "help" | "--help" | "-h" => {
      println!("{}", i18n::text("video_lab_help"));
      Ok(())
    },
    _ => Err(EngineError::protocol("Unknown lab command; use --help")),
  }
}

fn read_fixture_report(
  path: &std::path::Path,
) -> Result<flinch_video_engine::windows::desktop_fixture::FixtureReport> {
  let file = std::fs::File::open(path).map_err(|error| EngineError::io("fixture report", error))?;
  if file
    .metadata()
    .map_err(|error| EngineError::io("fixture report size", error))?
    .len()
    > 4 * 1024 * 1024
  {
    return Err(EngineError::protocol("Fixture report exceeds 4 MiB"));
  }
  serde_json::from_reader(file).map_err(|error| EngineError::protocol(error.to_string()))
}
fn fixture_worker(args: &[OsString]) -> Result<()> {
  if args.len() != 4 {
    return Err(EngineError::protocol("Invalid private fixture request"));
  }
  let duration: f64 = args[0]
    .to_str()
    .ok_or_else(|| EngineError::protocol("Invalid fixture duration"))?
    .parse()
    .map_err(|_| EngineError::protocol("Invalid fixture duration"))?;
  if !duration.is_finite()
    || !(0.1..=flinch_video_engine::windows::desktop_fixture::MAX_SECONDS).contains(&duration)
  {
    return Err(EngineError::protocol(
      "Fixture duration must be 0.1..=10830 seconds",
    ));
  }
  let report_path = PathBuf::from(&args[1]);
  let title = args[2]
    .to_str()
    .ok_or_else(|| EngineError::protocol("Invalid fixture title"))?;
  let monitor = args[3]
    .to_str()
    .ok_or_else(|| EngineError::protocol("Invalid fixture monitor"))?;
  let mut output = std::io::stdout().lock();
  let mut emit = |event| {
    protocol::write(
      &mut output,
      &protocol::Response {
        version: protocol::VERSION,
        session: "fixture".into(),
        event,
      },
    )
  };
  emit(Event::Hello)?;
  let result = flinch_video_engine::windows::desktop_fixture::run(
    Duration::from_secs_f64(duration),
    title,
    monitor,
    || emit(Event::Heartbeat),
  )
  .and_then(|report| flinch_video_engine::artifact::write_json(&report_path, &report));
  match result {
    Ok(()) => emit(Event::FixtureComplete(report_path)),
    Err(error) => {
      emit(Event::Diagnostic(error.clone()))?;
      Err(error)
    },
  }
}
fn fixture_command(args: &[OsString]) -> Result<()> {
  let monitor = match args {
    [_, _] => "primary",
    [_, _, flag, monitor] if flag == "--monitor" => monitor
      .to_str()
      .ok_or_else(|| EngineError::protocol("Invalid fixture monitor"))?,
    _ => {
      return Err(EngineError::protocol(
        "fixture requires SECONDS and a new REPORT.json path, then optionally --monitor X,Y,WxH",
      ));
    },
  };
  let seconds = args[0]
    .to_str()
    .ok_or_else(|| EngineError::protocol("Invalid fixture duration"))?;
  let duration: f64 = seconds
    .parse()
    .map_err(|_| EngineError::protocol("Invalid fixture duration"))?;
  if !duration.is_finite()
    || !(0.1..=flinch_video_engine::windows::desktop_fixture::MAX_SECONDS).contains(&duration)
  {
    return Err(EngineError::protocol(
      "Fixture duration must be 0.1..=10830 seconds",
    ));
  }
  let report = PathBuf::from(&args[1]);
  let report = if report.is_absolute() {
    report
  } else {
    std::env::current_dir()
      .map_err(|error| EngineError::io("fixture workspace", error))?
      .join(report)
  };
  if report.exists() {
    return Err(EngineError::protocol(
      "Fixture report destination already exists",
    ));
  }
  std::fs::create_dir_all(
    report
      .parent()
      .ok_or_else(|| EngineError::protocol("Fixture report has no parent"))?,
  )
  .map_err(|error| EngineError::io("fixture directory", error))?;
  let executable =
    std::env::current_exe().map_err(|error| EngineError::io("fixture executable", error))?;
  let signals = flinch_video_engine::windows::supervisor::ConsoleSignals::install()?;
  let mut worker = flinch_video_engine::windows::supervisor::WorkerProcess::spawn_fixture(
    &executable,
    seconds,
    &report,
    i18n::text("video_lab_fixture_title"),
    monitor,
  )?;
  let deadline = Instant::now() + Duration::from_secs_f64(duration) + Duration::from_secs(15);
  let mut last_event = Instant::now();
  loop {
    if signals.take() != 0 {
      return Err(EngineError::new(
        flinch_video_engine::ErrorKind::Cancelled,
        "fixture",
        "Fixture cancelled",
      ));
    }
    if Instant::now() >= deadline || last_event.elapsed() > Duration::from_secs(3) {
      return Err(EngineError::new(
        flinch_video_engine::ErrorKind::Timeout,
        "fixture supervisor",
        "Fixture owner stopped responding or exceeded its duration budget",
      ));
    }
    let Some(response) = worker.receive(Duration::from_millis(100))? else {
      continue;
    };
    if response.version != protocol::VERSION || response.session != "fixture" {
      return Err(EngineError::protocol("Invalid fixture response"));
    }
    last_event = Instant::now();
    match response.event {
      Event::Hello | Event::Heartbeat => {},
      Event::Diagnostic(error) => return Err(error),
      Event::FixtureComplete(path) if path == report => {
        worker.wait(Duration::from_secs(3))?;
        let report_data = read_fixture_report(&report)?;
        return print_json(
          &serde_json::json!({"report":report,"markers":report_data.markers.len(),"elapsed_seconds":report_data.elapsed_seconds,"marker_submission_lateness":report_data.marker_submission_lateness,"presentation_limit":report_data.presentation_limit}),
        );
      },
      _ => return Err(EngineError::protocol("Unexpected fixture event")),
    }
  }
}

#[derive(Debug)]
pub struct Options {
  pub spec: SessionSpec,
  pub duration: Duration,
  pub scenario: bool,
  pub pauses: u32,
  pub interrupt_after: Option<Duration>,
  /// Hold the prepared worker idle before Start: a pre-warm prototype measured as Start alone.
  pub prewarm_idle: Duration,
}
pub fn options(args: &[OsString], scenario: bool) -> Result<Options> {
  let mut spec = SessionSpec {
    monitor: "primary".into(),
    fps: 60,
    capture: CaptureKind::Dxgi,
    encoder: EncoderKind::Auto,
    quality: Quality::default(),
    cursor: true,
    clicks: false,
    audio: false,
    audio_device: None,
    microphone: false,
    microphone_device: None,
    output: PathBuf::new(),
    trace_frames: false,
    region: None,
    span: Vec::new(),
  };
  let mut duration = 60.0f64;
  let mut index = 0;
  let mut pauses = 2;
  let mut interrupt_after = None;
  let mut prewarm_idle = Duration::ZERO;
  let mut quality_set = false;
  while index < args.len() {
    let name = args[index]
      .to_str()
      .ok_or_else(|| EngineError::protocol("Option is not UTF-8"))?;
    let takes = matches!(
      name,
      "--output"
        | "--duration"
        | "--fps"
        | "--capture"
        | "--encoder"
        | "--monitor"
        | "--span"
        | "--audio-device"
        | "--bitrate"
        | "--qp"
        | "--pauses"
        | "--interrupt-after"
        | "--prewarm-idle"
        | "--region"
    );
    let value = if takes {
      index += 1;
      Some(
        args
          .get(index)
          .ok_or_else(|| EngineError::protocol(format!("Missing value for {name}")))?,
      )
    } else {
      None
    };
    let text = || {
      value
        .and_then(|v| v.to_str())
        .ok_or_else(|| EngineError::protocol("Option requires UTF-8 value"))
    };
    match name {
      "--output" => {
        spec.output = PathBuf::from(value.ok_or_else(|| EngineError::protocol("Missing output"))?)
      },
      "--duration" => {
        duration = text()?
          .parse()
          .map_err(|_| EngineError::protocol("Invalid duration"))?
      },
      "--fps" => {
        spec.fps = text()?
          .parse()
          .map_err(|_| EngineError::protocol("Invalid FPS"))?
      },
      "--span" => spec.span.push(text()?.to_owned()),
      "--capture" => {
        spec.capture = match text()? {
          "auto" => CaptureKind::Auto,
          "dxgi" => CaptureKind::Dxgi,
          "dxgi-legacy" => CaptureKind::DxgiLegacy,
          "wgc" => CaptureKind::Wgc,
          "synthetic" => CaptureKind::Synthetic,
          _ => return Err(EngineError::protocol("Unknown capture kind")),
        }
      },
      "--encoder" => {
        spec.encoder = match text()? {
          "amf" => EncoderKind::Amf,
          "nvenc" => EncoderKind::Nvenc,
          "qsv" => EncoderKind::Qsv,
          "mf" => EncoderKind::MediaFoundation,
          "software" => EncoderKind::Software,
          "auto" => EncoderKind::Auto,
          _ => return Err(EngineError::protocol("Unknown encoder")),
        }
      },
      "--monitor" => spec.monitor = text()?.into(),
      "--audio-device" => {
        spec.audio_device = Some(text()?.into());
        spec.audio = true;
      },
      "--bitrate" | "--qp" if quality_set => {
        return Err(EngineError::protocol("Use either --qp or --bitrate, once"));
      },
      "--bitrate" => {
        quality_set = true;
        spec.quality = Quality::Vbr {
          bitrate: text()?
            .parse()
            .map_err(|_| EngineError::protocol("Invalid bitrate"))?,
        }
      },
      // Constant QP for I and P frames, as the app's Low/Medium/High; a low QP records a
      // near-lossless reference for `reencode`.
      "--qp" => {
        quality_set = true;
        let qp = text()?
          .parse()
          .map_err(|_| EngineError::protocol("Invalid QP"))?;
        spec.quality = Quality::Cqp { i: qp, p: qp };
      },
      "--audio" => spec.audio = true,
      "--no-cursor" => spec.cursor = false,
      "--trace-frames" => spec.trace_frames = true,
      "--pauses" => {
        pauses = text()?
          .parse()
          .map_err(|_| EngineError::protocol("Invalid pause count"))?
      },
      "--interrupt-after" => {
        let seconds: f64 = text()?
          .parse()
          .map_err(|_| EngineError::protocol("Invalid interruption time"))?;
        if !seconds.is_finite() || !(0.1..=10800.0).contains(&seconds) {
          return Err(EngineError::protocol("Invalid interruption time"));
        }
        interrupt_after = Some(Duration::from_secs_f64(seconds));
      },
      "--region" => {
        let parts = text()?
          .split(",")
          .map(|part| part.trim().parse::<u32>())
          .collect::<std::result::Result<Vec<_>, _>>()
          .map_err(|_| EngineError::protocol("Region must be X,Y,WIDTH,HEIGHT"))?;
        let [x, y, width, height] = parts[..] else {
          return Err(EngineError::protocol("Region must be X,Y,WIDTH,HEIGHT"));
        };
        let region = Region {
          x,
          y,
          width,
          height,
        };
        region.validate()?;
        spec.region = Some(region);
      },
      "--prewarm-idle" => {
        let seconds: f64 = text()?
          .parse()
          .map_err(|_| EngineError::protocol("Invalid pre-warm idle time"))?;
        if !seconds.is_finite() || !(0.0..=600.0).contains(&seconds) {
          return Err(EngineError::protocol("Invalid pre-warm idle time"));
        }
        prewarm_idle = Duration::from_secs_f64(seconds);
      },
      _ => return Err(EngineError::protocol(format!("Unknown option {name}"))),
    }
    index += 1;
  }
  if !duration.is_finite() || !(0.1..=10800.0).contains(&duration) {
    return Err(EngineError::protocol(
      "Duration must be 0.1..=10800 seconds",
    ));
  }
  spec.validate()?;
  if pauses == 0
    || pauses > 1000
    || (pauses != 2 && duration < f64::from(pauses))
    || interrupt_after.is_some_and(|at| at.as_secs_f64() >= duration)
  {
    return Err(EngineError::protocol(
      "Scenario pauses or interruption do not fit the duration",
    ));
  }
  if !scenario && (pauses != 2 || interrupt_after.is_some()) {
    return Err(EngineError::protocol(
      "Pause count and fault injection require scenario",
    ));
  }
  Ok(Options {
    spec,
    duration: Duration::from_secs_f64(duration),
    scenario,
    pauses,
    interrupt_after,
    prewarm_idle,
  })
}
trait WorkerTransport {
  fn send(&mut self, request: &Request) -> Result<()>;
  fn receive(&self, timeout: Duration) -> Result<Option<protocol::Response>>;
  fn wait(&mut self, timeout: Duration) -> Result<()>;
  fn terminate(&mut self) -> Result<()>;
  fn pid(&self) -> u32;
  fn diagnostic(&self) -> Option<String> {
    None
  }
  fn diagnostics_lost(&self) -> u64 {
    0
  }
}
impl WorkerTransport for flinch_video_engine::windows::supervisor::WorkerProcess {
  fn send(&mut self, request: &Request) -> Result<()> {
    Self::send(self, request)
  }
  fn receive(&self, timeout: Duration) -> Result<Option<protocol::Response>> {
    Self::receive(self, timeout)
  }
  fn wait(&mut self, timeout: Duration) -> Result<()> {
    Self::wait(self, timeout)
  }
  fn terminate(&mut self) -> Result<()> {
    Self::terminate(self)
  }
  fn pid(&self) -> u32 {
    Self::pid(self)
  }
  fn diagnostic(&self) -> Option<String> {
    Self::diagnostic(self)
  }
  fn diagnostics_lost(&self) -> u64 {
    Self::diagnostics_lost(self)
  }
}
struct RecordDriver<W: WorkerTransport> {
  worker: W,
  session: String,
  next: u64,
  watch: control::Watch,
  bound: bool,
}
impl<W: WorkerTransport> RecordDriver<W> {
  fn send(&mut self, command: Command) -> Result<()> {
    self.watch.submitted(self.next, &command, Instant::now())?;
    let request = Request {
      version: protocol::VERSION,
      session: self.session.clone(),
      id: self.next,
      command,
    };
    self.next = self
      .next
      .checked_add(1)
      .ok_or_else(|| EngineError::protocol("Request id overflow"))?;
    self.worker.send(&request)
  }
  fn receive(&mut self) -> Result<Option<protocol::Response>> {
    let Some(response) = self.worker.receive(Duration::from_millis(20))? else {
      return Ok(None);
    };
    if response.version != protocol::VERSION {
      return Err(EngineError::protocol("Worker protocol version changed"));
    }
    if matches!(&response.event,Event::Ack{id,..}|Event::Rejected{id,..} if *id==0||*id>=self.next)
    {
      return Err(EngineError::protocol(
        "Worker acknowledged an unissued request",
      ));
    }
    if response.session == self.session {
      self.bound = true;
    } else if !(response.session.is_empty()
      && !self.bound
      && matches!(response.event, Event::Heartbeat))
    {
      return Err(EngineError::protocol("Worker session identity changed"));
    }
    self.watch.observe(&response.event, Instant::now())?;
    Ok(Some(response))
  }
}
fn append_outcome_failure(outcome: &mut RecordingOutcome, mut error: EngineError, stage: &str) {
  if let Some(original) = &mut outcome.error {
    original.reason.push_str(&format!("; {stage}: {error}"));
  } else {
    if error.recoverable_path.is_none() {
      error.recoverable_path = outcome.output.clone().or_else(|| outcome.partial.clone());
    }
    if matches!(outcome.kind, OutcomeKind::Completed | OutcomeKind::Degraded) {
      outcome.kind = OutcomeKind::Degraded;
      outcome.snapshot.degraded = true;
    }
    outcome.error = Some(error);
  }
}
fn recording_failure(
  resources: &mut resources::ResourceReports,
  error: EngineError,
  snapshot: SessionSnapshot,
  kind: OutcomeKind,
) -> Result<()> {
  let outcome = resources.failure(error, snapshot, kind);
  let mut original = outcome
    .error
    .clone()
    .ok_or_else(|| EngineError::protocol("Missing supervisor failure cause"))?;
  if let Err(report) = print_json(&outcome) {
    original
      .reason
      .push_str(&format!("; terminal output: {report}"));
  }
  Err(original)
}
fn record(args: &[OsString], scenario: bool) -> Result<()> {
  let options = options(args, scenario)?;
  let signals = flinch_video_engine::windows::supervisor::ConsoleSignals::install()?;
  let executable = std::env::current_exe().map_err(|e| EngineError::io("lab executable", e))?;
  // Held from before launch so the post-session hash covers the bytes the worker loaded.
  let held = flinch_video_engine::windows::build_identity::HeldExecutable::open(&executable)?;
  let before_spawn = Instant::now();
  let worker = flinch_video_engine::windows::supervisor::WorkerProcess::spawn(&executable)?;
  let launched = (before_spawn, Instant::now());
  let audio = options.spec.audio;
  let (input_tx, input) = std::sync::mpsc::sync_channel(8);
  std::thread::Builder::new()
    .name("video lab console".into())
    .spawn(move || {
      for line in std::io::stdin().lock().lines() {
        match line {
          Ok(line) => {
            let command = match line.trim() {
              "pause" => Some(Command::Pause),
              "resume" => Some(Command::Resume),
              "stop" => Some(Command::Stop),
              "abort" => Some(Command::Abort),
              _ => None,
            };
            if let Some(c) = command
              && input_tx.send(c).is_err()
            {
              break;
            }
          },
          Err(e) => {
            console::diagnostic(format!("{}: {e}", i18n::text("video_lab_console_error")));
            break;
          },
        }
      }
    })
    .map_err(|e| EngineError::io("console reader", e))?;
  let mut directory = None;
  let recording = record_session_in(
    options,
    worker,
    input,
    || signals.take(),
    Some(launched),
    &mut directory,
  );
  // Provenance follows the terminal result; without a session directory there is no run to describe.
  match directory {
    Some(directory) => provenance::merge(
      recording,
      provenance::record(
        &directory,
        held,
        audio,
        flinch_video_engine::windows::calibration::calibrate,
      ),
    ),
    None => recording,
  }
}
#[cfg(test)]
fn record_session(
  options: Options,
  worker: impl WorkerTransport,
  input: std::sync::mpsc::Receiver<Command>,
  signals: impl FnMut() -> u8,
) -> Result<()> {
  record_session_in(options, worker, input, signals, None, &mut None)
}
fn record_session_in(
  options: Options,
  worker: impl WorkerTransport,
  input: std::sync::mpsc::Receiver<Command>,
  mut signals: impl FnMut() -> u8,
  launched: Option<(Instant, Instant)>,
  directory: &mut Option<PathBuf>,
) -> Result<()> {
  let hello = worker.receive(Duration::from_secs(3))?.ok_or_else(|| {
    EngineError::new(
      flinch_video_engine::ErrorKind::Timeout,
      "worker connection",
      "Worker hello timeout",
    )
  })?;
  let hello_at = Instant::now();
  if !matches!(hello.event, Event::Hello)
    || hello.version != protocol::VERSION
    || !hello.session.is_empty()
  {
    return Err(EngineError::protocol("Invalid worker hello"));
  }
  let mut driver = RecordDriver {
    session: format!("lab-{}", worker.pid()),
    worker,
    next: 1,
    watch: control::Watch::new(Instant::now()),
    bound: false,
  };
  if let Some((before, spawned)) = launched {
    driver.watch.launched(before, spawned, hello_at)?;
  }
  let mut resources = resources::ResourceReports::new()?;
  driver.send(Command::Prepare(options.spec.clone()))?;
  let mut interrupted = false;
  let mut running = None;
  let mut started = false;
  let mut start_due = None;
  let mut scenario_stage = 0;
  let points = scenario_points(options.pauses);
  let mut last_snapshot = SessionSnapshot::default();
  let previous_losses = console::lost();
  loop {
    let now = Instant::now();
    for _ in 0..8 {
      let Some(message) = driver.worker.diagnostic() else {
        break;
      };
      console::diagnostic(message);
    }
    let interrupts = signals();
    if interrupts > 0 {
      let abort = interrupted || interrupts > 1 || driver.watch.stopping();
      interrupted = true;
      driver.send(if abort { Command::Abort } else { Command::Stop })?;
    }
    if let Err(mut error) = driver.watch.check(now) {
      if let Err(cleanup) = driver.worker.terminate() {
        error
          .reason
          .push_str(&format!("; owned worker termination: {cleanup}"));
      }
      if let Some(directory) = directory.as_ref() {
        error.recoverable_path = Some(std::path::Path::new(directory).join("capture.partial.mp4"));
      }
      last_snapshot.state = SessionState::Failed;
      return recording_failure(&mut resources, error, last_snapshot, OutcomeKind::Failed);
    }
    if !started && !driver.watch.stopping() && start_due.is_some_and(|due| now >= due) {
      driver.send(Command::Start)?;
      started = true;
    }
    if let Some(start) = running {
      let elapsed = now.duration_since(start);
      if options.interrupt_after.is_some_and(|at| elapsed >= at) {
        let mut error = EngineError::new(
          flinch_video_engine::ErrorKind::Cancelled,
          "scenario fault injection",
          "Deliberate termination of the exclusively owned worker",
        );
        if let Err(cleanup) = driver.worker.terminate() {
          error
            .reason
            .push_str(&format!("; owned worker termination: {cleanup}"));
        }
        if let Some(directory) = directory.as_ref() {
          error.recoverable_path =
            Some(std::path::Path::new(directory).join("capture.partial.mp4"));
        }
        last_snapshot.state = SessionState::Aborted;
        return recording_failure(&mut resources, error, last_snapshot, OutcomeKind::Aborted);
      }
      if elapsed >= options.duration && !driver.watch.stopping() {
        driver.send(Command::Stop)?;
      }
      if options.scenario && !driver.watch.stopping() {
        let duration = options.duration.as_secs_f64();
        if let Some((at, command)) = points.get(scenario_stage)
          && elapsed.as_secs_f64() >= duration * at
        {
          driver.send(command.clone())?;
          scenario_stage += 1;
        }
      }
    }
    if let Ok(command) = input.try_recv()
      && let Err(error) = driver.send(command)
    {
      if error.kind == flinch_video_engine::ErrorKind::InvalidState {
        console::diagnostic(format!("{}: {error}", i18n::text("video_lab_error")));
      } else {
        return Err(error);
      }
    }
    let response = match driver.receive() {
      Ok(Some(response)) => response,
      Ok(None) => continue,
      Err(mut error) => {
        resources.controls(driver.watch.report());
        if let Err(cleanup) = driver.worker.terminate() {
          error
            .reason
            .push_str(&format!("; owned worker termination: {cleanup}"));
        }
        last_snapshot.state = SessionState::Failed;
        return recording_failure(&mut resources, error, last_snapshot, OutcomeKind::Failed);
      },
    };
    resources.controls(driver.watch.report());
    match response.event {
      Event::Prepared(path) => {
        if let Err(mut error) = resources.prepared(&path) {
          if let Err(cleanup) = driver.worker.terminate() {
            error
              .reason
              .push_str(&format!("; owned worker termination: {cleanup}"));
          }
          last_snapshot.state = SessionState::Failed;
          return recording_failure(&mut resources, error, last_snapshot, OutcomeKind::Failed);
        }
        *directory = Some(path);
      },
      Event::State(SessionState::Ready)
      | Event::Ack {
        state: SessionState::Ready,
        ..
      } if !started && !driver.watch.stopping() && start_due.is_none() => {
        if options.prewarm_idle.is_zero() {
          driver.send(Command::Start)?;
          started = true;
        } else {
          start_due = Some(Instant::now() + options.prewarm_idle);
        }
      },
      Event::State(SessionState::Recording) if !driver.watch.stopping() => {
        if running.is_none() {
          running = Some(Instant::now());
        }
      },
      Event::Outcome(mut outcome) => {
        let deadline = driver
          .watch
          .terminal_deadline()
          .unwrap_or_else(|| Instant::now() + flinch_video_engine::limits::ABORT_TIMEOUT);
        outcome.snapshot.diagnostics_lost = outcome
          .snapshot
          .diagnostics_lost
          .saturating_add(driver.worker.diagnostics_lost())
          .saturating_add(console::lost().saturating_sub(previous_losses));
        if let Err(error) = resources.observe(&mut outcome.snapshot) {
          append_outcome_failure(&mut outcome, error, "resource observation");
        }
        if let Err(cleanup) = resources::remaining(deadline, "worker cleanup")
          .and_then(|remaining| driver.worker.wait(remaining.min(Duration::from_secs(3))))
        {
          append_outcome_failure(&mut outcome, cleanup, "worker cleanup");
          if let Err(error) = driver.worker.terminate() {
            append_outcome_failure(&mut outcome, error, "owned worker termination");
          }
        }
        if let Err(error) = resources.finish_before(&outcome, deadline) {
          append_outcome_failure(&mut outcome, error, "supervisor report");
        }
        if let Err(error) = print_json_before(&outcome, deadline) {
          append_outcome_failure(&mut outcome, error, "terminal output");
        }
        return if outcome.error.is_none() {
          Ok(())
        } else {
          Err(
            outcome
              .error
              .ok_or_else(|| EngineError::protocol("Missing failure cause"))?,
          )
        };
      },
      Event::Rejected { error, .. } => {
        console::diagnostic(format!("{}: {error}", i18n::text("video_lab_error")));
      },
      Event::Diagnostic(error) => console::diagnostic(error.to_string()),
      Event::Stats(mut stats) => {
        if let Err(mut error) = resources.observe(&mut stats) {
          if let Err(cleanup) = driver.worker.terminate() {
            error
              .reason
              .push_str(&format!("; owned worker termination: {cleanup}"));
          }
          stats.state = SessionState::Failed;
          return recording_failure(&mut resources, error, stats, OutcomeKind::Failed);
        }
        console::diagnostic(format!(
          "{}: {:?} {:.3}s encoded={} dropped={}",
          i18n::text("video_lab_progress"),
          stats.state,
          stats.media_100ns as f64 / 10_000_000.0,
          stats.frames_encoded,
          stats.slots_dropped
        ));
        last_snapshot = stats;
      },
      _ => {},
    }
  }
}
/// Drive one real session through the application-facing host client, with the same options as record.
/// Worker executable and switch: the lab itself, or `--app-worker PATH` for the Flinch app binary.
fn host_worker(args: &[OsString]) -> Result<(std::path::PathBuf, &'static str, &[OsString])> {
  match args {
    [flag, path, rest @ ..] if flag == "--app-worker" => {
      Ok((std::path::PathBuf::from(path), APP_WORKER_SWITCH, rest))
    },
    [flag] if flag == "--app-worker" => Err(EngineError::protocol("--app-worker requires a path")),
    rest => Ok((
      std::env::current_exe().map_err(|e| EngineError::io("lab executable", e))?,
      "--worker",
      rest,
    )),
  }
}
/// Must match `flinch::video::native::WORKER_SWITCH`; the lab does not link the app.
const APP_WORKER_SWITCH: &str = "--video-engine-worker";
/// `gif INPUT.mp4 OUTPUT.gif [none|bayer:N|floyd|sierra]`: native GIF from a recording.
fn gif_command(args: &[OsString]) -> Result<()> {
  let [input, output, rest @ ..] = args else {
    return Err(EngineError::protocol("gif requires INPUT and OUTPUT"));
  };
  let options = gif_options(rest.first().and_then(|value| value.to_str()))?;
  let started = Instant::now();
  let report = flinch_video_engine::windows::gif::encode(
    std::path::Path::new(input),
    std::path::Path::new(output),
    options,
  )?;
  print_json(&serde_json::json!({
    "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
    "options": options,
    "report": report,
  }))
}
fn gif_options(value: Option<&str>) -> Result<flinch_video_engine::gif::GifOptions> {
  use flinch_video_engine::gif::Dither;
  let dither = match value {
    None | Some("sierra") => Dither::Sierra24a,
    Some("none") => Dither::None,
    Some("floyd") => Dither::FloydSteinberg,
    Some(other) => match other.strip_prefix("bayer:").map(str::parse::<u8>) {
      Some(Ok(scale)) => Dither::Bayer { scale },
      _ => return Err(EngineError::protocol(format!("Unknown GIF dither {other}"))),
    },
  };
  Ok(flinch_video_engine::gif::GifOptions { dither })
}
fn host_smoke(args: &[OsString]) -> Result<()> {
  let (executable, switch, args) = host_worker(args)?;
  let options = options(args, false)?;
  let launched = Instant::now();
  let host = flinch_video_engine::windows::host::HostSession::launch(&executable, &[switch])?;
  let directory = host.prepare(options.spec.clone())?;
  let prepared = launched.elapsed();
  host.start()?;
  let recording = launched.elapsed();
  std::thread::sleep(options.duration / 2);
  host.pause()?;
  std::thread::sleep(Duration::from_millis(300));
  host.resume()?;
  std::thread::sleep(options.duration / 2);
  let stopping = Instant::now();
  let outcome = host.stop()?;
  print_json(&serde_json::json!({
    "directory": directory,
    "prepared_ms": prepared.as_secs_f64() * 1000.0,
    "recording_ms": recording.as_secs_f64() * 1000.0,
    "stop_ms": stopping.elapsed().as_secs_f64() * 1000.0,
    "controls": host.controls(),
    "diagnostics": host.take_diagnostics(),
    "outcome": outcome,
  }))
}
fn scenario_points(pauses: u32) -> Vec<(f64, Command)> {
  if pauses == 2 {
    return vec![
      (0.2, Command::Pause),
      (0.25, Command::Resume),
      (0.6, Command::Pause),
      (0.7, Command::Resume),
    ];
  }
  (0..pauses)
    .flat_map(|i| {
      let start = 0.1 + f64::from(i) * 0.8 / f64::from(pauses);
      [
        (start, Command::Pause),
        (start + 0.15 / f64::from(pauses), Command::Resume),
      ]
    })
    .collect()
}
fn compare(args: &[OsString]) -> Result<()> {
  if args.first().is_some_and(|s| s == "--frames") {
    return print_json(&comparison::compare(&args[1..])?);
  }
  if args.len() != 2 {
    return Err(EngineError::protocol(
      "compare requires two result JSON files",
    ));
  }
  let mut reports = Vec::new();
  for path in args {
    let file = std::fs::File::open(path).map_err(|e| EngineError::io("comparison input", e))?;
    let report: RecordingOutcome =
      serde_json::from_reader(file).map_err(|e| EngineError::protocol(e.to_string()))?;
    reports.push(report);
  }
  print_json(
    &serde_json::json!({"recordings":reports,"game_impact":"not_measured","synchronization":"not_measured","superiority_established":false}),
  )
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn verification_worker_emits_the_original_decode_failure_before_closing_its_pipe() -> Result<()> {
    let directory = tempfile::tempdir().expect("fixture");
    let mut output = Vec::new();
    let error = verification_worker_output(
      &[
        directory.path().join("absent.mp4").into_os_string(),
        directory.path().join("report.json").into_os_string(),
        "--markers".into(),
      ],
      &mut output,
    )
    .expect_err("missing media");
    let mut reader = std::io::BufReader::new(output.as_slice());
    let hello: protocol::Response = protocol::read(&mut reader)
      .expect("hello packet")
      .expect("hello");
    assert!(matches!(hello.event, Event::Hello));
    let failure: protocol::Response = protocol::read(&mut reader)
      .expect("error packet")
      .expect("error");
    let Event::Diagnostic(retained) = failure.event else {
      return Err(EngineError::protocol("Expected original decoder failure"));
    };
    assert_eq!(retained.stage, error.stage);
    assert_eq!(retained.reason, error.reason);
    assert_eq!(retained.native_code, error.native_code);
    Ok(())
  }
  #[test]
  fn secondary_failures_preserve_native_causes_and_keep_completed_media_accessible() {
    let mut outcome = RecordingOutcome {
      thumbnail: None,
      kind: OutcomeKind::Failed,
      output: None,
      partial: Some("partial.mp4".into()),
      error: Some(EngineError {
        native_code: Some(77),
        ..EngineError::new(
          flinch_video_engine::ErrorKind::Native,
          "original encoder error",
          "original cause",
        )
      }),
      snapshot: SessionSnapshot::default(),
    };
    append_outcome_failure(
      &mut outcome,
      EngineError::protocol("secondary report failure"),
      "report",
    );
    let error = outcome.error.expect("original failure");
    assert_eq!(error.native_code, Some(77));
    assert_eq!(error.stage, "original encoder error");
    assert!(error.reason.starts_with("original cause"));
    let mut completed = RecordingOutcome {
      thumbnail: None,
      kind: OutcomeKind::Completed,
      output: Some("finished.mp4".into()),
      partial: None,
      error: None,
      snapshot: SessionSnapshot::default(),
    };
    append_outcome_failure(
      &mut completed,
      EngineError::protocol("cleanup timeout"),
      "worker cleanup",
    );
    assert_eq!(completed.kind, OutcomeKind::Degraded);
    assert_eq!(completed.output, Some("finished.mp4".into()));
    assert_eq!(
      completed.error.expect("cleanup cause").recoverable_path,
      completed.output
    );
  }
  #[test]
  fn thirty_pause_schedule_is_ordered_bounded_and_faults_require_explicit_scenarios() {
    let points = scenario_points(30);
    assert_eq!(points.len(), 60);
    assert!(points.windows(2).all(|w| w[0].0 < w[1].0));
    assert!(points.iter().all(|p| p.0 > 0.0 && p.0 < 1.0));
    let args = [
      "--output".into(),
      "test.mp4".into(),
      "--duration".into(),
      "35".into(),
      "--pauses".into(),
      "30".into(),
      "--interrupt-after".into(),
      "3".into(),
    ];
    assert!(options(&args, false).is_err());
    let parsed = options(&args, true).expect("explicit synthetic fault scenario");
    assert_eq!(parsed.pauses, 30);
    assert_eq!(parsed.interrupt_after, Some(Duration::from_secs(3)));
  }
  #[test]
  fn record_takes_one_constant_qp_or_bitrate_and_validates_the_qp() {
    let with = |extra: &[&str]| {
      let mut args: Vec<OsString> = vec!["--output".into(), "v.mp4".into()];
      args.extend(extra.iter().map(OsString::from));
      options(&args, false)
    };
    assert_eq!(
      with(&["--qp", "10"]).expect("reference QP").spec.quality,
      Quality::Cqp { i: 10, p: 10 }
    );
    assert!(with(&["--qp", "52"]).is_err(), "out of range");
    assert!(with(&["--qp", "x"]).is_err());
    assert!(with(&["--qp", "10", "--bitrate", "8000000"]).is_err());
    assert!(with(&["--bitrate", "8000000", "--qp", "10"]).is_err());
    assert!(with(&["--qp", "10", "--qp", "12"]).is_err());
  }
  #[test]
  fn parser_requires_explicit_destination_and_rejects_implied_fallbacks() {
    assert!(options(&[], false).is_err());
    let o = options(
      &[
        "--output".into(),
        "video.mp4".into(),
        "--fps".into(),
        "120".into(),
        "--audio".into(),
      ],
      true,
    )
    .expect("valid test fixture");
    assert_eq!(o.spec.fps, 120);
    assert!(o.spec.audio);
    assert!(o.scenario);
    assert!(
      options(
        &[
          "--output".into(),
          "v.mp4".into(),
          "--duration".into(),
          "NaN".into()
        ],
        false
      )
      .is_err()
    );
    assert!(
      options(
        &[
          "--output".into(),
          "v.mp4".into(),
          "--fps".into(),
          "24".into()
        ],
        false
      )
      .is_err()
    );
  }
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod record_tests;
