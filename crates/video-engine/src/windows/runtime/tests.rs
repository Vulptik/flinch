//! Exercise the actual session and coordinator with COM driver doubles, not desktop capture.
#![allow(clippy::expect_used)]
use super::*;
use std::{collections::VecDeque, sync::atomic::AtomicUsize};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

struct FixtureCodec {
  data: Vec<u8>,
  extra: Vec<u8>,
  pending: VecDeque<EncodedPacket>,
  drained: bool,
  fail: Arc<AtomicBool>,
  retain: Arc<AtomicBool>,
  // 0: immediate output, 1: output appears only once draining starts, 2: drain never completes.
  drain_mode: Arc<std::sync::atomic::AtomicU8>,
  held: VecDeque<EncodedPacket>,
}
impl super::super::encoding::Codec for FixtureCodec {
  fn extra(&self) -> &[u8] {
    &self.extra
  }
  fn busy(&self, _: u32) -> bool {
    self.retain.load(Ordering::Acquire)
  }
  fn submit(
    &mut self,
    _: &ID3D11Texture2D,
    _: u32,
    pts: u64,
    duration: u64,
    _: bool,
  ) -> Result<bool> {
    if self.fail.load(Ordering::Acquire) {
      return Err(EngineError {
        native_code: Some(79),
        ..EngineError::new(
          ErrorKind::Native,
          "fixture encoder",
          "original input failure",
        )
      });
    }
    // A self-contained IDR fixture is repeated with the real scheduler's timestamps.
    // It never reads a texture and is never compiled into a production recording path.
    let packet = EncodedPacket {
      track: Track::Video,
      pts: pts as i64,
      dts: pts as i64,
      duration,
      keyframe: true,
      data: self.data.clone(),
    };
    if self.drain_mode.load(Ordering::Acquire) == 1 {
      self.held.push_back(packet);
    } else {
      self.pending.push_back(packet);
    }
    Ok(true)
  }
  fn poll(&mut self) -> Result<Option<EncodedPacket>> {
    Ok(self.pending.pop_front())
  }
  fn drain(&mut self) -> Result<bool> {
    self.pending.extend(self.held.drain(..));
    self.drained = true;
    Ok(true)
  }
  fn drained(&self) -> bool {
    self.drained && self.pending.is_empty() && self.drain_mode.load(Ordering::Acquire) != 2
  }
}
#[derive(Default)]
struct DisplayState {
  lost: AtomicBool,
  retries: AtomicUsize,
  reject_recreate: AtomicBool,
  /// Recreations still refused with E_ACCESSDENIED, as while a UAC prompt shows.
  secure_desktop: AtomicUsize,
  recreate_timeout: AtomicBool,
  latest_source: std::sync::Mutex<Option<Arc<super::super::capture::mock::Shared>>>,
  /// Outputs this process duplicates; recreation is refused while any is still held, as
  /// Windows refuses a second duplication of the same output (E_INVALIDARG).
  exclusive: std::sync::Mutex<Vec<Arc<super::super::capture::mock::Shared>>>,
}
struct FixtureDisplay(Arc<DisplayState>);
impl DisplaySource for FixtureDisplay {
  fn memory(&self) -> Result<Option<crate::metrics::resources::GpuMemory>> {
    Ok(Some(crate::metrics::resources::GpuMemory {
      local_usage_bytes: 32 * 1024 * 1024,
      local_budget_bytes: 256 * 1024 * 1024,
      ..Default::default()
    }))
  }
  fn check(&self, _: &Gpu) -> Result<()> {
    if self.0.lost.load(Ordering::Acquire) {
      Err(EngineError::new(
        ErrorKind::DeviceLost,
        "fixture display",
        "display detached",
      ))
    } else {
      Ok(())
    }
  }
  fn recreate(&self, gpu: &Gpu, _: &SessionSpec) -> Result<Composite> {
    self.0.retries.fetch_add(1, Ordering::AcqRel);
    if self
      .0
      .exclusive
      .lock()
      .expect("duplicated outputs")
      .iter()
      .any(|output| output.duplicated())
    {
      return Err(EngineError {
        native_code: Some(windows::Win32::Foundation::E_INVALIDARG.0),
        ..EngineError::new(
          ErrorKind::DeviceLost,
          "fixture duplication",
          "output already duplicated by this process",
        )
      });
    }
    // While the secure desktop shows (a UAC prompt), Windows refuses duplication.
    if self
      .0
      .secure_desktop
      .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
        left.checked_sub(1)
      })
      .is_ok()
    {
      return Err(EngineError {
        native_code: Some(windows::Win32::Foundation::E_ACCESSDENIED.0),
        ..EngineError::new(
          ErrorKind::DeviceLost,
          "fixture duplication",
          "Access is denied",
        )
      });
    }
    if self.0.reject_recreate.load(Ordering::Acquire) {
      Err(EngineError {
        native_code: Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_UNSUPPORTED.0),
        ..EngineError::new(
          ErrorKind::DeviceLost,
          "fixture duplication",
          "recreation refused",
        )
      })
    } else {
      let (capture, source) = super::super::capture::mock::fixture(gpu);
      source.timeout.store(
        self.0.recreate_timeout.load(Ordering::Acquire),
        Ordering::Release,
      );
      let mut exclusive = self.0.exclusive.lock().expect("duplicated outputs");
      if !exclusive.is_empty() {
        exclusive.push(source.clone());
      }
      drop(exclusive);
      *self.0.latest_source.lock().expect("replacement source") = Some(source);
      Ok(Composite::single(capture))
    }
  }
}
fn spec(path: &std::path::Path) -> SessionSpec {
  SessionSpec {
    monitor: "quiet-fixture".into(),
    fps: 60,
    capture: CaptureKind::Dxgi,
    encoder: EncoderKind::Amf,
    quality: Quality::Cqp { i: 21, p: 23 },
    cursor: false,
    clicks: false,
    audio: false,
    audio_device: None,
    microphone: false,
    microphone_device: None,
    output: path.to_owned(),
    trace_frames: true,
    region: None,
    span: Vec::new(),
  }
}
struct Fixture {
  session: PreparedSession,
  driver: Arc<super::super::gpu::mock::State>,
  source: Arc<super::super::capture::mock::Shared>,
  display: Arc<DisplayState>,
  encoder_failure: Arc<AtomicBool>,
  retain_textures: Arc<AtomicBool>,
  drain_mode: Arc<std::sync::atomic::AtomicU8>,
}
type StageLog = Arc<std::sync::Mutex<Vec<&'static str>>>;
/// COM GPU/capture/encoder doubles behind the production Prepare sequence.
struct QuietPlatform {
  devices: Option<(
    windows::Win32::Graphics::Direct3D11::ID3D11Device,
    windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
  )>,
  info: MonitorInfo,
  display: Arc<DisplayState>,
  source: Arc<std::sync::Mutex<Option<Arc<super::super::capture::mock::Shared>>>>,
  encoder_failure: Arc<AtomicBool>,
  retain_textures: Arc<AtomicBool>,
  drain_mode: Arc<std::sync::atomic::AtomicU8>,
  fail_at: Option<&'static str>,
  stages: StageLog,
  /// Encoder tiers that refuse to initialize, with the error kind they report.
  reject: Vec<(EncoderKind, ErrorKind)>,
  /// Capture backends that refuse to initialize, with the error kind they report.
  reject_capture: Vec<(CaptureKind, ErrorKind)>,
  /// Capture backends that open but never deliver a frame.
  silent_capture: Vec<CaptureKind>,
  /// DWM composition timing the quiet platform reports.
  composition: Result<Option<u64>>,
}
impl QuietPlatform {
  fn step(&mut self, stage: &'static str) -> Result<()> {
    self.stages.lock().expect("stage log").push(stage);
    if self.fail_at == Some(stage) {
      return Err(EngineError {
        native_code: Some(83),
        ..EngineError::new(ErrorKind::Native, stage, "scripted preparation failure")
      });
    }
    Ok(())
  }
}
impl Platform for QuietPlatform {
  fn display(&mut self, spec: &SessionSpec) -> Result<MonitorInfo> {
    self.step("display")?;
    select_output([&self.info], spec)?;
    Ok(self.info.clone())
  }
  fn gpu(&mut self, info: &MonitorInfo, region: Option<Region>, fps: u32) -> Result<Gpu> {
    self.step("gpu")?;
    let (device, context) = self
      .devices
      .take()
      .ok_or_else(|| EngineError::protocol("GPU prepared twice"))?;
    Gpu::from_device(device, context, info.width, info.height, region, fps)
  }
  fn capture(&mut self, gpu: &Gpu, spec: &SessionSpec) -> Result<Composite> {
    self.step("capture")?;
    if let Some((_, kind)) = self
      .reject_capture
      .iter()
      .find(|(capture, _)| *capture == spec.capture)
    {
      return Err(EngineError {
        native_code: Some(85),
        ..EngineError::new(
          *kind,
          "quiet capture",
          format!("{:?} refused", spec.capture),
        )
      });
    }
    let (capture, source) = super::super::capture::mock::fixture(gpu);
    if self.silent_capture.contains(&spec.capture) {
      source.timeout.store(true, Ordering::Release);
    }
    *self.source.lock().expect("source slot") = Some(source);
    Ok(Composite::single(capture))
  }
  fn compositor(
    &mut self,
    gpu: &Gpu,
    shader: super::super::cursor::ShaderCompile,
  ) -> Result<super::super::cursor::CursorCompositor> {
    self.step("cursor")?;
    super::super::cursor::CursorCompositor::with_bytecode(
      gpu,
      &shader.wait(limits::PREPARE_TIMEOUT)?,
    )
  }
  fn clicks(
    &mut self,
    _: &Gpu,
    _: (i32, i32),
    _: super::super::cursor::ShaderCompile<super::super::clicks::Shaders>,
  ) -> Result<super::super::clicks::ClickLayer> {
    self.step("clicks")?;
    Err(EngineError::protocol(
      "The quiet platform has no click painter",
    ))
  }
  fn encoder(
    &mut self,
    _: &Gpu,
    _: &MonitorInfo,
    spec: &SessionSpec,
    _: Arc<Watchdog>,
  ) -> Result<EncoderService> {
    self.step("encoder")?;
    if let Some((_, kind)) = self
      .reject
      .iter()
      .find(|(encoder, _)| *encoder == spec.encoder)
    {
      return Err(EngineError {
        native_code: Some(84),
        ..EngineError::new(
          *kind,
          "quiet encoder",
          format!("{:?} refused", spec.encoder),
        )
      });
    }
    let media = super::super::fixtures::media(1);
    let fail = self.encoder_failure.clone();
    let retain = self.retain_textures.clone();
    let mode = self.drain_mode.clone();
    EncoderService::from_factory(move || {
      Ok(FixtureCodec {
        data: media.video[0].data.clone(),
        extra: media.video_extra,
        pending: VecDeque::new(),
        drained: false,
        fail,
        retain,
        drain_mode: mode,
        held: VecDeque::new(),
      })
    })
  }
  fn audio(&mut self, spec: &SessionSpec, watchdog: Arc<Watchdog>) -> Result<AudioService> {
    self.step("audio")?;
    let inputs = super::super::audio_service::Inputs {
      system_device: None,
      microphone_device: None,
      ..super::super::audio_service::Inputs::of(spec)
    };
    AudioService::new_with_watchdog(true, inputs, watchdog)
  }
  fn composition_period(&mut self) -> Result<Option<u64>> {
    self.composition.clone()
  }
  fn source(mut self, _: &SessionSpec) -> Result<Box<dyn DisplaySource>> {
    self.step("source")?;
    Ok(Box::new(FixtureDisplay(self.display)))
  }
}
fn quiet_monitor() -> MonitorInfo {
  MonitorInfo {
    id: "quiet-fixture".into(),
    adapter_luid: 7,
    adapter: "COM double".into(),
    vendor_id: 0x1002,
    driver: "none".into(),
    width: 512,
    height: 64,
    x: 0,
    y: 0,
    rotation: 1,
    color_space: 0,
    hdr: false,
    refresh_hz: Some(60),
  }
}
fn quiet_platform(
  fail_at: Option<&'static str>,
) -> (QuietPlatform, Arc<super::super::gpu::mock::State>) {
  let (device, context, driver) = super::super::gpu::mock::devices();
  (
    QuietPlatform {
      devices: Some((device, context)),
      info: quiet_monitor(),
      display: Arc::new(DisplayState::default()),
      source: Default::default(),
      encoder_failure: Default::default(),
      retain_textures: Default::default(),
      drain_mode: Default::default(),
      fail_at,
      stages: Default::default(),
      reject: Vec::new(),
      reject_capture: Vec::new(),
      silent_capture: Vec::new(),
      composition: Ok(Some(166_666)),
    },
    driver,
  )
}
fn fixture(spec: SessionSpec, interrupts: Arc<Interrupts>, watchdog: Arc<Watchdog>) -> Fixture {
  let (platform, driver) = quiet_platform(None);
  let display = platform.display.clone();
  let source = platform.source.clone();
  let encoder_failure = platform.encoder_failure.clone();
  let retain_textures = platform.retain_textures.clone();
  let drain_mode = platform.drain_mode.clone();
  let session = PreparedSession::prepare(spec, interrupts, watchdog, platform)
    .expect("production Prepare sequence with COM doubles");
  let source = source
    .lock()
    .expect("source slot")
    .take()
    .expect("prepared capture source");
  Fixture {
    session,
    driver,
    source,
    display,
    encoder_failure,
    retain_textures,
    drain_mode,
  }
}
fn tick_until(session: &mut PreparedSession, frames: u64) {
  let deadline = Instant::now() + Duration::from_secs(2);
  while session.snapshot.frames_encoded < frames {
    session.tick().expect("real session tick with COM doubles");
    assert!(Instant::now() < deadline, "session made no progress");
    std::thread::sleep(Duration::from_millis(1));
  }
}
fn conversions(driver: &super::super::gpu::mock::State) -> usize {
  driver
    .calls
    .lock()
    .expect("GPU calls")
    .iter()
    .filter(|call| **call == "VideoProcessorBlt")
    .count()
}
#[test]
fn abandon_cancels_both_encoders_even_when_native_source_shutdown_fails() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  for close_fails in [false, true] {
    let mut request = spec(&root.path().join(format!("abandoned-{close_fails}.mp4")));
    request.audio = true;
    let mut f = fixture(
      request,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
    );
    if close_fails {
      f.session.capture = Composite::single(super::super::capture::shutdown_failure_fixture(
        &f.session.gpu,
      ));
    }
    let result = f.session.abandon();
    if close_fails {
      assert_eq!(result.expect_err("native close failure").stage, "WGC close");
    } else {
      result.expect("abandoned prepared source");
    }
    assert!(!f.session.capture.valid);
    assert!(f.session.writer.is_none());
    assert_eq!(
      f.session
        .encoder
        .submit(&f.session.gpu.surfaces[0], 0, 0, 166666, true)
        .expect_err("video producer withdrawn")
        .kind,
      ErrorKind::Cancelled
    );
    assert_eq!(
      f.session
        .aac
        .as_ref()
        .expect("audio service")
        .start()
        .expect_err("audio producer withdrawn")
        .kind,
      ErrorKind::Cancelled
    );
    assert!(!f.session.artifact.output.exists());
  }
}
#[test]
fn source_shutdown_failure_prevents_publication_and_preserves_the_recorded_partial() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let output = root.path().join("source-close-failed.mp4");
  let mut f = fixture(
    spec(&output),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 3);
  f.session.pause().expect("committed cut");
  f.session.capture = Composite::single(super::super::capture::shutdown_failure_fixture(
    &f.session.gpu,
  ));
  let error = f
    .session
    .finish()
    .expect_err("source shutdown is required before publication");
  assert_eq!(error.stage, "WGC close");
  assert_eq!(
    error.native_code,
    Some(windows::Win32::Foundation::E_FAIL.0)
  );
  assert!(!f.session.capture.valid);
  assert!(f.session.artifact.partial.exists());
  assert!(!output.exists());
}
#[test]
fn diagnostic_failure_cannot_replace_the_native_capture_loss_that_triggered_recovery() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  for closed_journal in [true, false] {
    let mut f = fixture(
      spec(
        &root
          .path()
          .join(format!("recovery-diagnostic-{closed_journal}.mp4")),
      ),
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
    );
    if closed_journal {
      f.session
        .journal
        .finish()
        .expect("finalized diagnostic fixture");
    } else {
      f.session.capture.pointer.generation = u64::MAX;
    }
    *f.source.acquire_failure.lock().expect("source loss") =
      Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
    let error = f.session.acquire().err().expect("secondary failure");
    assert_eq!(error.stage, "DXGI acquire");
    assert_eq!(
      error.native_code,
      Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST.0)
    );
    assert!(error.reason.contains(if closed_journal {
      "Journal is finalized"
    } else {
      "Pointer revision overflow"
    }));
    assert_eq!(
      f.display.retries.load(Ordering::Acquire),
      usize::from(!closed_journal)
    );
  }
}
#[test]
fn failure_manifest_errors_keep_the_original_recording_cause_and_partial_path() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let session = fixture(
    spec(&root.path().join("failed-output.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  )
  .session;
  let partial = session.artifact.partial.clone();
  std::fs::write(&partial, b"retained partial bytes").expect("partial fixture");
  let manifest = session.artifact.directory.join("result.json.tmp");
  std::fs::write(&manifest, b"existing diagnostic bytes").expect("occupied manifest");
  let original = EngineError {
    native_code: Some(73),
    ..EngineError::new(
      ErrorKind::Native,
      "original recording failure",
      "original detail",
    )
  };
  let outcome = failure(&mut Some(session), original);
  assert_eq!(outcome.kind, OutcomeKind::Failed);
  assert_eq!(outcome.output, None);
  let error = outcome.error.expect("original failure");
  assert_eq!(error.stage, "original recording failure");
  assert_eq!(error.native_code, Some(73));
  assert!(error.reason.starts_with("original detail"));
  assert!(error.reason.contains("failure manifest: manifest create"));
  assert_eq!(error.recoverable_path, Some(partial.clone()));
  assert_eq!(
    std::fs::read(partial).expect("retained partial"),
    b"retained partial bytes"
  );
  assert_eq!(
    std::fs::read(manifest).expect("retained diagnostic"),
    b"existing diagnostic bytes"
  );
}
#[test]
fn finalization_rejects_a_missing_requested_audio_track_and_retains_the_partial() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let output = root.path().join("missing-audio.mp4");
  let mut f = fixture(
    spec(&output),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 3);
  // Model an audio pipeline that vanished while the compressed writer stayed video-only.
  f.session.spec.audio = true;
  assert!(
    f.session.finish().is_err(),
    "structural MP4 success cannot replace the requested track contract"
  );
  assert!(!output.exists());
  assert!(f.session.artifact.partial.exists());
}
#[test]
fn terminal_manifest_failure_preserves_the_already_published_recording() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let output = root.path().join("published-before-report-error.mp4");
  let inbox = Arc::new(Inbox::default());
  submit(&inbox, 1, Command::Prepare(spec(&output)));
  submit(&inbox, 2, Command::Start);
  let control = inbox.clone();
  let (send, events) = mpsc::sync_channel(128);
  let driver = std::thread::spawn(move || {
    loop {
      match events
        .recv_timeout(Duration::from_secs(3))
        .expect("bounded coordinator event")
      {
        Event::State(SessionState::Recording) => submit(&control, 3, Command::Stop),
        Event::Outcome(outcome) => return outcome,
        _ => {},
      }
    }
  });
  run_session(
    &inbox,
    send,
    &AtomicBool::new(false),
    &Arc::new(Watchdog::default()),
    |spec, interrupts, watchdog| {
      let session = fixture(spec, interrupts, watchdog).session;
      std::fs::write(
        session.artifact.directory.join("result.json.tmp"),
        b"existing diagnostic file",
      )
      .expect("manifest publication fault");
      Ok(session)
    },
  )
  .expect("recording outcome survives metadata error");
  let outcome = driver.join().expect("coordinator driver");
  assert_eq!(outcome.kind, OutcomeKind::Degraded);
  assert_eq!(outcome.output, Some(output.clone()));
  let error = outcome.error.expect("secondary manifest failure");
  assert_eq!(error.stage, "manifest create");
  assert_eq!(error.recoverable_path, Some(output.clone()));
  assert!(
    super::super::verify::verify(&output, false)
      .expect("independent decode")
      .video_frames
      > 0
  );
}
/// Regression for the coverage gate: a Stop that publishes a degraded session was only reached
/// when a slow machine happened to drop frames, so the measured lines differed per machine.
#[test]
fn stopping_a_session_that_lost_frames_publishes_it_as_degraded() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let output = root.path().join("degraded.mp4");
  let inbox = Arc::new(Inbox::default());
  submit(&inbox, 1, Command::Prepare(spec(&output)));
  submit(&inbox, 2, Command::Start);
  let control = inbox.clone();
  let (send, events) = mpsc::sync_channel(128);
  let driver = std::thread::spawn(move || {
    loop {
      match events
        .recv_timeout(Duration::from_secs(3))
        .expect("bounded coordinator event")
      {
        Event::State(SessionState::Recording) => submit(&control, 3, Command::Stop),
        Event::Outcome(outcome) => return outcome,
        _ => {},
      }
    }
  });
  run_session(
    &inbox,
    send,
    &AtomicBool::new(false),
    &Arc::new(Watchdog::default()),
    |spec, interrupts, watchdog| {
      let mut session = fixture(spec, interrupts, watchdog).session;
      // Scripted loss: the session already counted as degraded before Stop.
      session.snapshot.degraded = true;
      Ok(session)
    },
  )
  .expect("degraded recording is still published");
  let outcome = driver.join().expect("coordinator driver");
  assert_eq!(outcome.kind, OutcomeKind::Degraded);
  assert!(outcome.snapshot.degraded);
  assert_eq!(outcome.snapshot.state, SessionState::Completed);
  assert_eq!(outcome.output, Some(output.clone()));
  assert!(outcome.error.is_none());
  assert!(
    super::super::verify::verify(&output, false)
      .expect("independent decode")
      .video_frames
      > 0
  );
}
#[test]
fn source_time_reaches_the_native_encoder_and_static_reuse_keeps_its_original_date() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("source-age.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  let mut presentation = 0;
  // SAFETY: read the clock into a live test output; no capture or global timer changes.
  unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut presentation) }
    .expect("native presentation coordinate");
  f.source
    .info
    .lock()
    .expect("source metadata")
    .LastPresentTime = presentation;
  f.session.start().expect("first frame");
  f.source.info.lock().expect("static source").LastPresentTime = 0;
  let first = f
    .session
    .last_source_qpc_100ns
    .expect("capture origin retained during Start");
  tick_until(&mut f.session, 3);
  assert_eq!(f.session.last_source_qpc_100ns, Some(first));
  f.session.finish().expect("finalize and drain metrics");
  let report = f
    .session
    .snapshot
    .encoder_metrics
    .expect("native encoder metrics");
  assert_eq!(
    report.source_age_at_accept.count,
    f.session.snapshot.frames_encoded
  );
  assert_eq!(report.source_time_unavailable, 0);
  assert!(report.source_age_at_accept.maximum_us > 0);
  let trace = std::fs::read_to_string(f.session.artifact.directory.join("measurements.jsonl"))
    .expect("trace");
  let sources: Vec<_> = trace
    .lines()
    .filter_map(|line| {
      let value: serde_json::Value = serde_json::from_str(line).expect("trace JSON");
      (value["kind"] == "video_input_queued")
        .then(|| value["source_qpc_100ns"].as_u64().expect("source QPC"))
    })
    .collect();
  assert!(!sources.is_empty());
  assert!(sources.iter().all(|source| *source == first));
}
#[test]
fn source_without_a_presentation_date_stays_unmeasured_instead_of_using_arrival_time() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("unknown-source-time.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("undated initial static frame");
  assert!(f.session.last_source_qpc_100ns.is_none());
  tick_until(&mut f.session, 3);
  f.session.finish().expect("valid static output");
  let report = f
    .session
    .snapshot
    .encoder_metrics
    .expect("source measurements");
  assert_eq!(report.source_age_at_accept.count, 0);
  assert_eq!(
    report.source_time_unavailable,
    f.session.snapshot.frames_encoded
  );
}
#[test]
fn a_tick_delayed_beyond_the_pause_cutoff_cannot_submit_post_cut_media() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("late-pause-tick.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  // The first output slot is due, but this tick was preempted past its committed cutoff.
  f.session
    .tick_before(Some(0))
    .expect("observe clock and drain without submitting");
  assert_eq!(
    f.session.snapshot.frames_new + f.session.snapshot.frames_reused,
    0
  );
  assert_eq!(f.session.snapshot.frames_encoded, 0);
  assert_eq!(f.session.snapshot.media_100ns, 0);
  assert!(!f.session.encoder.busy(0));
  let cut = f.session.epoch;
  f.session
    .timeline
    .pause_at(cut, super::super::qpc().expect("actual later observation"))
    .expect("commit original cut without lowering clock guard");
}
#[test]
fn desktop_changed_during_pause_is_converted_once_even_if_static_after_resume() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("pause-cache.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 2);
  f.session.pause().expect("pause");
  let before = conversions(&f.driver);
  let fresh = f.session.snapshot.frames_new;
  f.source
    .info
    .lock()
    .expect("source revision")
    .LastPresentTime = 1;
  f.session.tick().expect("update desktop cache while paused");
  assert_eq!(
    conversions(&f.driver),
    before,
    "paused capture must not run NV12 conversion"
  );
  f.source
    .info
    .lock()
    .expect("source now static")
    .LastPresentTime = 0;
  f.session.resume().expect("resume latest cached content");
  let frames = f.session.snapshot.frames_encoded;
  tick_until(&mut f.session, frames + 2);
  assert_eq!(
    conversions(&f.driver),
    before + 1,
    "the newly cached desktop must replace pre-pause NV12 exactly once"
  );
  assert_eq!(f.session.snapshot.frames_new, fresh + 1);
}
#[test]
fn desktop_change_survives_pool_saturation_until_a_texture_is_returned() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("busy-cache.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.retain_textures.store(true, Ordering::Release);
  f.session.start().expect("start");
  tick_until(&mut f.session, 8);
  assert!((0..f.session.gpu.surfaces.len() as u32).all(|slot| f.session.encoder.busy(slot)));
  let before = conversions(&f.driver);
  let fresh = f.session.snapshot.frames_new;
  let dropped = f.session.snapshot.slots_dropped;
  f.source
    .info
    .lock()
    .expect("source revision")
    .LastPresentTime = 1;
  let deadline = Instant::now() + Duration::from_secs(2);
  while f.session.snapshot.slots_dropped == dropped {
    f.session.tick().expect("bounded overload");
    assert!(Instant::now() < deadline);
    std::thread::sleep(Duration::from_millis(1));
  }
  assert_eq!(
    conversions(&f.driver),
    before,
    "leased textures must not be modified"
  );
  f.source
    .info
    .lock()
    .expect("source now static")
    .LastPresentTime = 0;
  f.retain_textures.store(false, Ordering::Release);
  tick_until(&mut f.session, 10);
  assert!(
    f.session.snapshot.degraded,
    "known frame loss cannot end as clean success"
  );
  assert_eq!(
    conversions(&f.driver),
    before + 1,
    "the lost slot must not erase the pending desktop revision"
  );
  assert_eq!(f.session.snapshot.frames_new, fresh + 1);
}
#[test]
fn a_high_frame_rate_session_leases_its_whole_surface_pool() {
  // Regression: the runtime searched only the first eight surfaces. At 120 FPS an encoder slowed
  // by another app's GPU work held all eight and lost about 6% of slots.
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut spec = spec(&root.path().join("deep-pool.mp4"));
  spec.fps = 120;
  let mut f = fixture(
    spec,
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  let pool = f.session.gpu.surfaces.len() as u32;
  assert_eq!(pool, 16);
  assert_eq!(f.session.snapshot.surfaces, pool);
  f.retain_textures.store(true, Ordering::Release);
  f.session.start().expect("start");
  // Each new desktop revision needs a free surface; a held one is never converted into.
  let deadline = Instant::now() + Duration::from_secs(2);
  while !(0..pool).all(|slot| f.session.encoder.busy(slot)) {
    f.source
      .info
      .lock()
      .expect("source revision")
      .LastPresentTime += 1;
    f.session.tick().expect("tick");
    assert!(
      Instant::now() < deadline,
      "only {} of {pool} surfaces were ever leased",
      (0..pool)
        .filter(|slot| f.session.encoder.busy(*slot))
        .count()
    );
    std::thread::sleep(Duration::from_millis(1));
  }
  f.retain_textures.store(false, Ordering::Release);
  let frames = f.session.snapshot.frames_encoded;
  tick_until(&mut f.session, frames + 2);
}
#[test]
fn replacement_without_a_valid_frame_holds_the_last_picture_and_expires_with_its_original_cause() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("missing-replacement.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 2);
  f.display.recreate_timeout.store(true, Ordering::Release);
  *f.source.acquire_failure.lock().expect("source loss") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  assert!(matches!(
    f.session.acquire().expect("pending replacement"),
    Acquisition::Unchanged
  ));
  assert!(!f.session.capture.valid);
  let reused = f.session.snapshot.frames_reused;
  let dropped = f.session.snapshot.slots_dropped;
  let deadline = Instant::now() + Duration::from_secs(1);
  // The recording repeats its last picture while it waits for a valid source.
  while f.session.snapshot.frames_reused == reused {
    f.session
      .tick()
      .expect("hold the last picture while the replacement has no frame");
    assert!(Instant::now() < deadline);
    std::thread::sleep(Duration::from_millis(1));
  }
  assert_eq!(f.session.snapshot.slots_dropped, dropped);
  assert!(f.session.snapshot.degraded);
  f.session
    .capture_recovery
    .as_mut()
    .expect("recovery budget")
    .since = Instant::now() - Duration::from_secs(3);
  let error = f.session.acquire().err().expect("expired replacement");
  assert_eq!(
    error.native_code,
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST.0)
  );
  assert!(error.reason.contains("no valid replacement capture frame"));
  assert_eq!(f.display.retries.load(Ordering::Acquire), 1);
}
#[test]
fn a_secure_desktop_refusal_holds_the_last_picture_until_windows_gives_access_back() {
  // Regression: a real recording ended at a UAC prompt. Both recreation attempts
  // ran in the same instant and Windows refused them while the secure desktop showed.
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("secure-desktop.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 2);
  f.display
    .secure_desktop
    .store(usize::MAX, Ordering::Release);
  *f.source.acquire_failure.lock().expect("source loss") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  assert!(matches!(
    f.session.acquire().expect("refusal is held, not failed"),
    Acquisition::Unchanged
  ));
  assert!(!f.session.capture.valid);
  assert!(!f.source.duplicated(), "the lost source was released");
  let recovery = f.session.capture_recovery.as_mut().expect("recovery");
  assert!(recovery.denied_since.is_some());
  assert_eq!(recovery.attempts, 0, "refusals are not counted attempts");
  // A prompt can stay up far longer than the two-second replacement window.
  recovery.since = Instant::now() - Duration::from_secs(30);
  let reused = f.session.snapshot.frames_reused;
  let dropped = f.session.snapshot.slots_dropped;
  let deadline = Instant::now() + Duration::from_secs(2);
  let mut refusals = f.display.retries.load(Ordering::Acquire);
  while f.session.snapshot.frames_reused < reused + 3 || refusals < 3 {
    f.session
      .capture_recovery
      .as_mut()
      .expect("still recovering")
      .next_attempt = Instant::now();
    f.session
      .tick()
      .expect("the recording continues while the prompt shows");
    refusals = f.display.retries.load(Ordering::Acquire);
    assert!(Instant::now() < deadline);
    std::thread::sleep(Duration::from_millis(2));
  }
  assert_eq!(f.session.snapshot.slots_dropped, dropped);
  assert!(f.session.snapshot.degraded);
  // The prompt closes: the next attempt recreates the source and fresh pictures follow.
  f.display.secure_desktop.store(0, Ordering::Release);
  f.session
    .capture_recovery
    .as_mut()
    .expect("still recovering")
    .next_attempt = Instant::now();
  assert!(matches!(
    f.session.acquire().expect("access restored"),
    Acquisition::New { .. }
  ));
  assert!(f.session.capture.valid);
  assert!(f.session.capture_recovery.is_none());
  let frames = f.session.snapshot.frames_encoded;
  tick_until(&mut f.session, frames + 2);
}
#[test]
fn source_recreation_attempts_are_not_reset_by_a_subsequent_acquire_call() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("repeated-loss.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.display.recreate_timeout.store(true, Ordering::Release);
  *f.source.acquire_failure.lock().expect("source loss") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  f.session.acquire().expect("first attempt pending");
  for attempt in 1..=2 {
    assert_eq!(f.display.retries.load(Ordering::Acquire), attempt);
    let source = f
      .display
      .latest_source
      .lock()
      .expect("latest source")
      .as_ref()
      .expect("replacement")
      .clone();
    source.timeout.store(false, Ordering::Release);
    *source
      .acquire_failure
      .lock()
      .expect("replacement also lost") =
      Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
    if attempt == 1 {
      f.session.acquire().expect("second and last attempt");
    } else {
      assert!(f.session.acquire().is_err());
    }
  }
  assert_eq!(f.display.retries.load(Ordering::Acquire), 2);
}
#[test]
fn resume_waits_for_valid_replacement_content_before_advancing_media_time() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("resume-replacement.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 2);
  f.session.pause().expect("pause");
  f.display.recreate_timeout.store(true, Ordering::Release);
  *f.source.acquire_failure.lock().expect("source loss") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  f.session.acquire().expect("new source waiting");
  let source = f
    .display
    .latest_source
    .lock()
    .expect("latest source")
    .as_ref()
    .expect("replacement")
    .clone();
  let deliver = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(20));
    source.timeout.store(false, Ordering::Release);
  });
  f.session
    .resume()
    .expect("resume only after replacement first frame");
  deliver.join().expect("bounded source fixture");
  assert!(f.session.capture.valid && !f.session.timeline.paused());
  assert!(f.session.capture_recovery.is_none());
}
#[test]
fn session_static_frames_audio_pauses_and_native_writer_form_one_decodable_file() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("session temp");
  let mut specification = spec(&root.path().join("finished.mp4"));
  specification.audio = true;
  let mut f = fixture(
    specification,
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 3);
  f.session.snapshot.state = SessionState::Recording;
  f.session.pause().expect("common pause cutoff");
  let cut = f.session.snapshot.media_100ns;
  std::thread::sleep(Duration::from_millis(3));
  f.session.tick().expect("paused acquire and drain");
  assert_eq!(f.session.snapshot.media_100ns, cut);
  f.session.resume().expect("resume current source");
  let next = f.session.snapshot.frames_encoded + 2;
  tick_until(&mut f.session, next);
  f.session
    .check_topology()
    .expect("substituted display boundary");
  f.session
    .finish()
    .expect("drain and publish single native container");
  assert!(f.session.artifact.output.is_file());
  assert!(!f.session.capture.valid);
  assert!(f.session.capture.acquire(&f.session.gpu).is_err());
  assert!(!f.session.artifact.partial.exists());
  assert!(f.session.snapshot.frames_reused > 0);
  assert_eq!(f.session.timings.first_frame.summary().count, 1);
  assert!(
    f.session
      .snapshot
      .writer_metrics
      .expect("writer metrics")
      .finalized
  );
  assert!(
    f.session
      .snapshot
      .encoder_metrics
      .expect("encoder metrics")
      .final_report
  );
  assert!(f.session.snapshot.audio_frames > 0);
  let first = f.session.snapshot.first_media;
  first.validate().expect("ordered owner milestones");
  assert!(
    first.start_qpc_100ns.is_some()
      && first.capture_qpc_100ns.is_some()
      && first.submit_qpc_100ns.is_some()
      && first.encoded_qpc_100ns.is_some()
      && first.sink_accept_qpc_100ns.is_some()
  );
  assert_eq!(
    f.session
      .snapshot
      .gpu_memory
      .expect("substituted GPU observer")
      .local_usage_bytes,
    32 * 1024 * 1024
  );
  let decoded =
    super::super::verify::verify(&f.session.artifact.output, false).expect("decode complete file");
  assert_eq!(decoded.video_frames, f.session.snapshot.frames_encoded);
  assert!(decoded.audio_frames > 0);
  assert!(f.driver.unexpected.lock().expect("driver calls").is_empty());
}
#[test]
fn replacement_acquisition_failure_preserves_original_loss_and_retry_count() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut request = spec(&root.path().join("replacement-denied.mp4"));
  request.capture = CaptureKind::DxgiLegacy;
  let mut f = fixture(
    request,
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.display.recreate_timeout.store(true, Ordering::Release);
  *f.source.acquire_failure.lock().expect("initial loss") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  f.session.acquire().expect("replacement pending");
  let replacement = f
    .display
    .latest_source
    .lock()
    .expect("replacement")
    .as_ref()
    .expect("source")
    .clone();
  *replacement
    .acquire_failure
    .lock()
    .expect("replacement denied") = Some(windows::Win32::Foundation::E_ACCESSDENIED);
  let error = f.session.acquire().err().expect("secondary failure");
  assert_eq!(
    error.native_code,
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST.0)
  );
  assert_eq!(error.stage, "DXGI acquire");
  assert!(
    error
      .reason
      .contains("capture recreation after 1 attempt(s)")
  );
  assert!(error.reason.contains("0x80070005"));
  assert_eq!(f.display.retries.load(Ordering::Acquire), 1);
}
#[test]
fn transient_duplication_loss_has_two_attempts_and_topology_failure_never_recreates() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("recover.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  *f.source.acquire_failure.lock().expect("failure") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  f.display.reject_recreate.store(true, Ordering::Release);
  // The first attempt runs at once; the second waits its spacing instead of the same instant.
  assert!(matches!(
    f.session
      .acquire()
      .expect("first attempt refused, second scheduled"),
    Acquisition::Unchanged
  ));
  assert_eq!(f.display.retries.load(Ordering::Acquire), 1);
  assert!(matches!(
    f.session.acquire().expect("spacing not yet elapsed"),
    Acquisition::Unchanged
  ));
  assert_eq!(f.display.retries.load(Ordering::Acquire), 1);
  f.session
    .capture_recovery
    .as_mut()
    .expect("recovery")
    .next_attempt = Instant::now();
  let error = f.session.acquire().err().expect("both attempts refused");
  assert_eq!(error.stage, "DXGI acquire");
  assert_eq!(
    error.native_code,
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST.0)
  );
  assert!(
    error.reason.contains("fixture duplication") && error.reason.contains("recreation refused")
  );
  assert_eq!(f.display.retries.load(Ordering::Acquire), 2);
  let mut g = fixture(
    spec(&root.path().join("removed.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  *g.source.acquire_failure.lock().expect("failure") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  g.display.lost.store(true, Ordering::Release);
  let error = g.session.acquire().err().expect("display loss");
  assert_eq!(error.stage, "DXGI acquire");
  assert_eq!(
    error.native_code,
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST.0)
  );
  assert!(error.reason.contains("fixture display") && error.reason.contains("display detached"));
  assert_eq!(g.display.retries.load(Ordering::Acquire), 0);
  let mut h = fixture(
    spec(&root.path().join("recreated.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  h.session
    .start()
    .expect("start source before transient loss");
  h.session.last_progress = Instant::now() - limits::ENCODER_STALL_TIMEOUT - Duration::from_secs(1);
  *h.source.acquire_failure.lock().expect("failure") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  h.session.capture.pointer.generation = 17;
  assert!(matches!(
    h.session.acquire().expect("recreated capture"),
    Acquisition::New { .. }
  ));
  assert!(h.session.snapshot.degraded);
  h.session
    .tick()
    .expect("recovered source gets a fresh encoder-production budget");
  assert_eq!(h.display.retries.load(Ordering::Acquire), 1);
  assert!(
    h.session.capture.pointer.generation > 17,
    "a new source must invalidate cached cursor shapes"
  );
}
#[test]
fn duplication_loss_releases_the_lost_source_before_recreating_it() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("exclusive.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start source before loss");
  f.display
    .exclusive
    .lock()
    .expect("duplicated outputs")
    .push(f.source.clone());
  assert!(f.source.duplicated());
  *f.source.acquire_failure.lock().expect("failure") =
    Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST);
  assert!(matches!(
    f.session
      .acquire()
      .expect("recreated once the lost duplication was released"),
    Acquisition::New { .. }
  ));
  assert!(!f.source.duplicated(), "the lost duplication was released");
  assert_eq!(f.display.retries.load(Ordering::Acquire), 1);
  assert!(f.session.snapshot.degraded);
}
#[test]
fn session_detects_native_conversion_failure_encoder_failure_and_missing_progress() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("failed.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  *f.driver.failure.lock().expect("failure") = Some("VideoProcessorBlt");
  assert_eq!(
    f.session.tick().expect_err("conversion error").stage,
    "GPU color conversion"
  );
  *f.driver.failure.lock().expect("failure") = None;
  f.encoder_failure.store(true, Ordering::Release);
  let deadline = Instant::now() + Duration::from_secs(2);
  let error = loop {
    if let Err(error) = f.session.tick() {
      break error;
    }
    assert!(Instant::now() < deadline);
    std::thread::sleep(Duration::from_millis(1));
  };
  assert_eq!(error.native_code, Some(79));
  assert_eq!(error.reason, "original input failure");
  assert!(!f.session.artifact.output.exists());
}

fn submit(inbox: &Inbox, id: u64, command: Command) {
  assert!(
    inbox
      .submit(Request {
        version: protocol::VERSION,
        session: "quiet-worker".into(),
        id,
        command
      })
      .expect("wire admission")
      .is_none()
  );
}
#[test]
fn actual_worker_coordinator_confirms_controls_and_emits_exactly_one_terminal_result() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let inbox = Arc::new(Inbox::default());
  let (send, events) = mpsc::sync_channel(128);
  submit(
    &inbox,
    1,
    Command::Prepare(spec(&root.path().join("coordinated.mp4"))),
  );
  let control = inbox.clone();
  let driver = std::thread::spawn(move || {
    let mut stage = 0;
    let mut rejected = false;
    let mut confirmed = Vec::new();
    loop {
      match events
        .recv_timeout(Duration::from_secs(3))
        .expect("coordinator event")
      {
        Event::State(SessionState::Ready) if stage == 0 => {
          submit(&control, 2, Command::Resume);
          submit(&control, 3, Command::Start);
          stage = 1;
        },
        Event::Rejected { id: 2, error } => {
          assert_eq!(error.kind, ErrorKind::InvalidState);
          rejected = true;
        },
        Event::State(SessionState::Recording) if stage == 1 => {
          confirmed.push(SessionState::Recording);
          submit(&control, 4, Command::Pause);
          stage = 2;
        },
        Event::State(SessionState::Paused) if stage == 2 => {
          confirmed.push(SessionState::Paused);
          submit(&control, 5, Command::Resume);
          stage = 3;
        },
        Event::State(SessionState::Recording) if stage == 3 => {
          confirmed.push(SessionState::Recording);
          submit(&control, 6, Command::Stop);
          stage = 4;
        },
        Event::Outcome(outcome) => {
          assert!(events.try_iter().all(|e| !matches!(e, Event::Outcome(_))));
          return (outcome, confirmed, rejected);
        },
        _ => {},
      }
    }
  });
  run_session(
    &inbox,
    send,
    &AtomicBool::new(false),
    &Arc::new(Watchdog::default()),
    |s, i, w| Ok(fixture(s, i, w).session),
  )
  .expect("real coordinator loop");
  let (outcome, confirmed, rejected) = driver.join().expect("control fixture");
  assert!(rejected);
  assert_eq!(
    confirmed,
    [
      SessionState::Recording,
      SessionState::Paused,
      SessionState::Recording
    ]
  );
  let lost = outcome.snapshot.slots_dropped != 0;
  assert_eq!(outcome.snapshot.degraded, lost);
  assert_eq!(
    outcome.kind,
    if lost {
      OutcomeKind::Degraded
    } else {
      OutcomeKind::Completed
    }
  );
  assert_eq!(outcome.snapshot.state, SessionState::Completed);
  let output = outcome.output.expect("published result");
  let decoded = super::super::verify::verify(&output, false).expect("decode coordinated result");
  assert_eq!(decoded.video_frames, outcome.snapshot.frames_encoded);
}
#[test]
fn coordinator_confirms_abort_only_after_source_cleanup_and_reports_cleanup_failure() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  for close_fails in [false, true] {
    let inbox = Arc::new(Inbox::default());
    submit(
      &inbox,
      1,
      Command::Prepare(spec(
        &root.path().join(format!("abort-ready-{close_fails}.mp4")),
      )),
    );
    let control = inbox.clone();
    let (send, events) = mpsc::sync_channel(128);
    let driver = std::thread::spawn(move || {
      let mut acknowledged = false;
      loop {
        match events
          .recv_timeout(Duration::from_secs(3))
          .expect("bounded abort event")
        {
          Event::State(SessionState::Ready) => submit(&control, 2, Command::Abort),
          Event::Ack {
            id: 2,
            state: SessionState::Aborted,
          } => acknowledged = true,
          Event::Outcome(outcome) => return (outcome, acknowledged),
          _ => {},
        }
      }
    });
    run_session(
      &inbox,
      send,
      &AtomicBool::new(false),
      &Arc::new(Watchdog::default()),
      |request, interrupts, watchdog| {
        let mut session = fixture(request, interrupts, watchdog).session;
        if close_fails {
          session.capture = Composite::single(super::super::capture::shutdown_failure_fixture(
            &session.gpu,
          ));
        }
        Ok(session)
      },
    )
    .expect("abort coordinator");
    let (outcome, acknowledged) = driver.join().expect("bounded driver");
    assert_eq!(acknowledged, !close_fails);
    assert_eq!(
      outcome.kind,
      if close_fails {
        OutcomeKind::Failed
      } else {
        OutcomeKind::Aborted
      }
    );
    assert_eq!(outcome.output, None);
    if close_fails {
      assert_eq!(outcome.error.expect("close failure").stage, "WGC close");
    }
  }
}
#[test]
fn coordinator_can_abort_before_prepare_and_keeps_an_original_prepare_failure() {
  for abort in [true, false] {
    let inbox = Arc::new(Inbox::default());
    let (send, events) = mpsc::sync_channel(128);
    submit(
      &inbox,
      1,
      if abort {
        Command::Abort
      } else {
        Command::Prepare(spec(std::path::Path::new("unused.mp4")))
      },
    );
    let mut preparations = 0;
    run_session(
      &inbox,
      send,
      &AtomicBool::new(false),
      &Arc::new(Watchdog::default()),
      |_, _, _| {
        preparations += 1;
        Err(EngineError {
          native_code: Some(81),
          ..EngineError::new(
            ErrorKind::Native,
            "fixture preparation",
            "original preparation failure",
          )
        })
      },
    )
    .expect("bounded coordinator");
    let outcomes: Vec<_> = events
      .try_iter()
      .filter_map(|e| {
        if let Event::Outcome(o) = e {
          Some(o)
        } else {
          None
        }
      })
      .collect();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(preparations, usize::from(!abort));
    assert_eq!(
      outcomes[0].kind,
      if abort {
        OutcomeKind::Aborted
      } else {
        OutcomeKind::Failed
      }
    );
    if !abort {
      assert_eq!(
        outcomes[0].error.as_ref().expect("cause").native_code,
        Some(81)
      );
    }
  }
}
#[test]
fn native_preparation_rejects_invalid_specs_and_missing_monitors_before_opening_capture() {
  let mut request = spec(std::path::Path::new("unused.mp4"));
  request.fps = 30;
  assert_eq!(
    PreparedSession::new(request.clone())
      .err()
      .expect("unsupported cadence")
      .kind,
    ErrorKind::Capability
  );
  request.fps = 60;
  request.monitor = "definitely-not-an-attached-flinch-monitor".into();
  let error = PreparedSession::new(request)
    .err()
    .expect("missing display");
  assert_eq!(error.stage, "monitor");
  assert_eq!(error.kind, ErrorKind::Capability);
}
#[test]
fn synthetic_display_checks_and_memory_inventory_do_not_open_a_capture_source() {
  let _com = super::super::Apartment::new().expect("COM");
  let Some(output) = gpu::outputs()
    .expect("read-only adapter inventory")
    .into_iter()
    .next()
  else {
    return;
  };
  let display = NativeDisplay {
    outputs: vec![(output, (0, 0))],
    synthetic: true,
  };
  let (gpu, driver) = super::super::gpu::mock::fixture();
  display
    .check(&gpu)
    .expect("synthetic source does not depend on Windows session or monitor topology");
  match display.memory() {
    Ok(Some(memory)) => {
      serde_json::to_vec(&memory).expect("raw budget observation");
    },
    Ok(None) => {}, // Interface absence is explicitly unmeasured, without a rendering fallback.
    Err(error) => {
      assert!(error.stage == "GPU memory observer" || error.stage == "GPU memory query")
    },
  }
  assert!(driver.unexpected.lock().expect("no driver work").is_empty());
}
#[test]
fn missing_encode_progress_fails_even_if_the_display_is_static_and_does_not_publish() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut f = fixture(
    spec(&root.path().join("stalled.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  f.session
    .scheduler
    .due(3 * time::SECOND)
    .expect("no further frame due during this clock fixture");
  // Three seconds without a packet is a slow encoder (a game starving AMF), not a hung one.
  f.session.last_progress = Instant::now() - Duration::from_secs(3);
  f.session.tick().expect("a slow encoder keeps recording");
  f.session.last_progress = Instant::now() - limits::ENCODER_STALL_TIMEOUT - Duration::from_secs(1);
  let error = f.session.tick().expect_err("encoder made no progress");
  assert_eq!(error.kind, ErrorKind::Timeout);
  assert_eq!(error.stage, "encoder");
  assert!(!f.session.artifact.output.exists());
}
#[test]
fn pending_stop_interrupts_prepare_without_a_lock_or_a_second_terminal_result() {
  let inbox = Arc::new(Inbox::default());
  let (send, events) = mpsc::sync_channel(128);
  submit(
    &inbox,
    1,
    Command::Prepare(spec(std::path::Path::new("unused.mp4"))),
  );
  run_session(
    &inbox,
    send,
    &AtomicBool::new(false),
    &Arc::new(Watchdog::default()),
    |_, interrupts, _| {
      submit(&inbox, 2, Command::Stop);
      interrupts.checkpoint("interrupted prepare", false)?;
      Err(EngineError::protocol(
        "unreachable preparation continuation",
      ))
    },
  )
  .expect("priority Stop preempts preparation");
  let events: Vec<_> = events.try_iter().collect();
  assert!(
    events
      .iter()
      .any(|e| matches!(e, Event::Rejected { id: 1, error } if error.kind==ErrorKind::Cancelled))
  );
  let outcomes: Vec<_> = events
    .iter()
    .filter_map(|e| {
      if let Event::Outcome(o) = e {
        Some(o)
      } else {
        None
      }
    })
    .collect();
  assert_eq!(outcomes.len(), 1);
  assert_eq!(outcomes[0].kind, OutcomeKind::Aborted);
}
#[test]
fn periodic_worker_maintenance_keeps_useful_partial_and_original_display_failure() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let inbox = Arc::new(Inbox::default());
  let (send, events) = mpsc::sync_channel(128);
  submit(
    &inbox,
    1,
    Command::Prepare(spec(&root.path().join("must-not-publish.mp4"))),
  );
  let display = Arc::new(DisplayState::default());
  let owned_display = display.clone();
  let commands = inbox.clone();
  let driver = std::thread::spawn(move || {
    let mut started = false;
    let mut observed_stats = false;
    loop {
      match events
        .recv_timeout(Duration::from_secs(4))
        .expect("worker events")
      {
        Event::State(SessionState::Ready) if !started => {
          started = true;
          submit(&commands, 2, Command::Start);
        },
        Event::Stats(stats) => {
          assert!(stats.resource_metrics_available);
          assert!(stats.frames_encoded > 0);
          observed_stats = true;
          owned_display.lost.store(true, Ordering::Release);
        },
        Event::Outcome(outcome) => {
          assert!(observed_stats);
          return outcome;
        },
        _ => {},
      }
    }
  });
  run_session(
    &inbox,
    send,
    &AtomicBool::new(false),
    &Arc::new(Watchdog::default()),
    |s, i, w| {
      let mut f = fixture(s, i, w);
      f.session.display = Box::new(FixtureDisplay(display.clone()));
      Ok(f.session)
    },
  )
  .expect("coordinator reports failure");
  let outcome = driver.join().expect("control fixture");
  assert_eq!(outcome.kind, OutcomeKind::Failed);
  assert_eq!(outcome.snapshot.state, SessionState::Failed);
  assert_eq!(
    outcome.error.as_ref().expect("original failure").stage,
    "fixture display"
  );
  let partial = outcome.partial.expect("retained media");
  assert!(partial.is_file());
  assert!(
    partial
      .parent()
      .expect("session directory")
      .join("result.json")
      .is_file()
  );
  assert!(!root.path().join("must-not-publish.mp4").exists());
}
#[test]
fn stop_writes_output_that_appears_only_while_the_encoder_drains() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let output = root.path().join("drained.mp4");
  let mut f = fixture(
    spec(&output),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 3);
  f.drain_mode.store(1, Ordering::Release);
  let before = f.session.snapshot.frames_encoded;
  let held_until = Instant::now() + Duration::from_millis(60);
  while Instant::now() < held_until {
    f.session.tick().expect("accepted input without output yet");
    std::thread::sleep(Duration::from_millis(1));
  }
  assert_eq!(
    f.session.snapshot.frames_encoded, before,
    "the codec retained its accepted input until drain"
  );
  f.session.finish().expect("drained output is published");
  assert!(
    f.session.snapshot.frames_encoded > before,
    "packets emitted during drain must reach the container"
  );
  assert!(output.exists());
  let inspection = crate::recovery::inspect(&output).expect("published structure");
  assert_eq!(
    inspection.track_media[0].samples,
    f.session.snapshot.frames_encoded
  );
}
#[test]
fn encoder_that_never_finishes_draining_fails_within_its_bound_and_publishes_nothing() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let output = root.path().join("undrained.mp4");
  let mut f = fixture(
    spec(&output),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  tick_until(&mut f.session, 3);
  f.drain_mode.store(2, Ordering::Release);
  let started = Instant::now();
  let error = f.session.finish().expect_err("drain never completes");
  let elapsed = started.elapsed();
  assert_eq!(
    (error.kind, error.stage.as_str()),
    (ErrorKind::Timeout, "AMF drain")
  );
  assert!(
    elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(8),
    "drain bound took {elapsed:?}"
  );
  assert!(f.session.artifact.partial.exists());
  assert!(!output.exists());
}
#[test]
fn prepare_runs_each_owned_stage_once_and_keeps_lab_provenance_off_the_start_path() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  for (audio, cursor) in [(true, true), (false, false)] {
    let root = tempfile::tempdir().expect("temp");
    let mut request = spec(&root.path().join("staged.mp4"));
    request.audio = audio;
    request.cursor = cursor;
    let (platform, driver) = quiet_platform(None);
    let stages = platform.stages.clone();
    let session = PreparedSession::prepare(
      request,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
      platform,
    )
    .expect("production Prepare sequence with COM doubles");
    let mut expected = vec!["display", "gpu", "capture"];
    expected.extend(cursor.then_some("cursor"));
    expected.push("encoder");
    expected.extend(audio.then_some("audio"));
    expected.push("source");
    assert_eq!(*stages.lock().expect("stage log"), expected);
    let costs = session.snapshot.prepare_stages.expect("stage costs");
    assert_eq!(costs.cursor_us.is_some(), cursor);
    assert_eq!(costs.audio_us.is_some(), audio);
    let sum = costs.stage_sum_us().expect("bounded stage sum");
    // Laps share one origin and truncate independently: never above the total, under 1 us apart each.
    assert!(
      sum <= costs.total_us && costs.total_us - sum < 9,
      "{costs:?}"
    );
    assert_eq!(session.timings.report().prepare.count, 1);
    let directory = &session.artifact.directory;
    for written in ["session.json", "capabilities.json", "source-formats.json"] {
      assert!(directory.join(written).exists(), "{written}");
    }
    for deferred in ["build.json", "aac-calibration.json"] {
      assert!(
        !directory.join(deferred).exists(),
        "{deferred} is supervisor provenance"
      );
    }
    assert_eq!(session.snapshot.state, SessionState::Ready);
    drop(session);
    assert_eq!(driver.live.load(Ordering::Acquire), 0);
  }
}
#[test]
fn every_prepare_stage_failure_keeps_its_cause_releases_owners_and_never_reports_ready() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  for stage in [
    "display", "gpu", "capture", "cursor", "clicks", "encoder", "audio", "source",
  ] {
    let root = tempfile::tempdir().expect("temp");
    let mut request = spec(&root.path().join("failed.mp4"));
    request.audio = true;
    request.cursor = true;
    // Click highlights prepare after the pointer and before the encoder; the quiet platform has
    // no painter, so only the run that fails there asks for them.
    request.clicks = stage == "clicks";
    let (platform, driver) = quiet_platform(Some(stage));
    let stages = platform.stages.clone();
    let error = PreparedSession::prepare(
      request,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
      platform,
    )
    .err()
    .expect("scripted stage failure");
    assert_eq!((error.stage.as_str(), error.native_code), (stage, Some(83)));
    assert_eq!(stages.lock().expect("stage log").last(), Some(&stage));
    let sessions: Vec<_> = std::fs::read_dir(root.path())
      .expect("output parent")
      .collect();
    if stage == "display" {
      // Nothing is created before the requested display is accepted.
      assert!(sessions.is_empty());
      assert!(error.recoverable_path.is_none());
    } else {
      assert_eq!(sessions.len(), 1);
      let directory = error
        .recoverable_path
        .as_ref()
        .expect("diagnostic directory");
      assert!(directory.join("capabilities.json").exists());
      assert!(!directory.join("capture.partial.mp4").exists());
    }
    assert!(!root.path().join("failed.mp4").exists());
    assert_eq!(
      driver.live.load(Ordering::Acquire),
      0,
      "{stage} leaked GPU owners"
    );
  }
}
#[test]
fn display_selection_matches_the_request_before_the_adapter_requirement() {
  let amd = quiet_monitor();
  let other = MonitorInfo {
    id: r"\.\DISPLAY2".into(),
    vendor_id: 0x10de,
    x: 512,
    ..quiet_monitor()
  };
  let mut request = spec(std::path::Path::new("unused.mp4"));
  request.monitor = "primary".into();
  assert_eq!(select_output([&other, &amd], &request).expect("primary"), 1);
  request.monitor = "QUIET-FIXTURE".into();
  assert_eq!(
    select_output([&other, &amd], &request).expect("case-insensitive id"),
    1
  );
  request.monitor = r"\.\display2".into();
  assert_eq!(
    select_output([&other, &amd], &request)
      .expect_err("non-AMD")
      .stage,
    "adapter"
  );
  // Vendor-independent tiers record from any adapter.
  for encoder in [EncoderKind::Software, EncoderKind::MediaFoundation] {
    request.encoder = encoder;
    assert_eq!(
      select_output([&other, &amd], &request).expect("any adapter"),
      0
    );
  }
  request.encoder = EncoderKind::Amf;
  request.monitor = "absent".into();
  let missing = select_output([&other, &amd], &request).expect_err("missing");
  assert_eq!(
    (missing.stage.as_str(), missing.kind),
    ("monitor", ErrorKind::Capability)
  );
}
#[test]
fn application_geometry_identities_select_the_exact_output_rectangle() {
  let left = MonitorInfo {
    x: -1920,
    width: 1920,
    height: 1080,
    ..quiet_monitor()
  };
  let primary = quiet_monitor();
  let mut request = spec(std::path::Path::new("unused.mp4"));
  request.monitor = "-1920,0,1920x1080".into();
  assert_eq!(select_output([&primary, &left], &request).expect("left"), 1);
  request.monitor = "0,0,512x64".into();
  assert_eq!(
    select_output([&left, &primary], &request).expect("primary"),
    1
  );
  // A stale rectangle (resolution or arrangement changed) is a missing monitor, never a neighbour.
  for stale in ["0,0,1920x1080", "-1920,0,1920x1081", "1,0,512x64"] {
    request.monitor = stale.into();
    assert_eq!(
      select_output([&left, &primary], &request)
        .expect_err(stale)
        .stage,
      "monitor"
    );
  }
  assert_eq!(monitor_geometry(" 10, -20 ,30x40"), Some((10, -20, 30, 40)));
  for invalid in [
    "primary",
    "1,2",
    "1,2,3",
    "1,2,3x",
    "a,2,3x4",
    "1,2,-3x4",
    "1,2,3x4,5",
  ] {
    assert_eq!(monitor_geometry(invalid), None, "{invalid}");
  }
}
fn presentation_in(milliseconds: i64) -> i64 {
  let (mut now, mut frequency) = (0, 0);
  // SAFETY: read the clock into live test outputs; no capture or global timer changes.
  unsafe {
    windows::Win32::System::Performance::QueryPerformanceCounter(&mut now).expect("QPC");
    windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency)
      .expect("QPC frequency");
  }
  now + frequency * milliseconds / 1000
}
#[test]
fn source_dated_slightly_ahead_records_its_lead_and_a_large_lead_still_fails_the_session() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let output = root.path().join("lead.mp4");
  let mut f = fixture(
    spec(&output),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  // WGC-style composition target: the first frames are accepted before their date.
  f.source
    .info
    .lock()
    .expect("source metadata")
    .LastPresentTime = presentation_in(30);
  f.session.start().expect("first frame");
  f.source.info.lock().expect("static source").LastPresentTime = 0;
  tick_until(&mut f.session, 3);
  f.session
    .finish()
    .expect("bounded lead is not a recording failure");
  let report = f.session.snapshot.encoder_metrics.expect("encoder metrics");
  assert!(report.source_lead_at_accept.count >= 1, "{report:?}");
  assert!(report.source_lead_at_accept.maximum_us <= 30_000);
  assert_eq!(
    report.source_lead_at_accept.count + report.source_age_at_accept.count,
    f.session.snapshot.frames_encoded
  );
  assert!(output.exists());

  let output = root.path().join("clock-domain.mp4");
  let mut f = fixture(
    spec(&output),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.source
    .info
    .lock()
    .expect("source metadata")
    .LastPresentTime = presentation_in(500);
  let deadline = Instant::now() + Duration::from_secs(2);
  let error = loop {
    let step = if f.session.snapshot.state == SessionState::Ready {
      f.session.start()
    } else {
      f.session.tick().map(|_| ())
    };
    if let Err(error) = step {
      break error;
    }
    assert!(Instant::now() < deadline, "large lead was accepted");
    std::thread::sleep(Duration::from_millis(1));
  };
  assert!(error.reason.contains("beyond the 50 ms"), "{error}");
  assert!(!output.exists());
}
#[test]
fn low_cadence_sessions_publish_frames_one_slot_apart_through_the_native_writer() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  for fps in [15, 30] {
    let root = tempfile::tempdir().expect("temp");
    let mut request = spec(&root.path().join(format!("cadence-{fps}.mp4")));
    request.fps = fps;
    let mut f = fixture(
      request,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
    );
    f.session.start().expect("first frame");
    tick_until(&mut f.session, 6);
    f.session.finish().expect("finalize");
    let decoded = super::super::verify::verify(&f.session.artifact.output, false)
      .expect("decode complete file");
    assert_eq!(decoded.video_frames, f.session.snapshot.frames_encoded);
    let slot_ms = 1000.0 / f64::from(fps);
    assert!(
      (decoded.maximum_video_timestamp_gap_ms - slot_ms).abs() < 0.01,
      "{fps} FPS gap {}",
      decoded.maximum_video_timestamp_gap_ms
    );
  }
}
#[test]
fn region_sessions_encode_the_crop_and_unfit_regions_fail_before_any_session_file() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let mut request = spec(&root.path().join("area.mp4"));
  request.region = Some(Region {
    x: 64,
    y: 16,
    width: 128,
    height: 34,
  });
  let (platform, _driver) = quiet_platform(None);
  let session = PreparedSession::prepare(
    request.clone(),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
    platform,
  )
  .expect("region inside the 512x64 display");
  // A rotated display records areas too: its cache is upright, at the desktop extent.
  for rotation in [2, 3, 4] {
    let (mut platform, _driver) = quiet_platform(None);
    platform.info.rotation = rotation;
    let mut rotated = request.clone();
    rotated.output = root.path().join(format!("rotated-{rotation}.mp4"));
    let rotated = PreparedSession::prepare(
      rotated,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
      platform,
    )
    .expect("area on a rotated display");
    assert_eq!((rotated.gpu.width, rotated.gpu.height), (128, 34));
  }
  assert_eq!(
    (
      session.gpu.source_width,
      session.gpu.source_height,
      session.gpu.width,
      session.gpu.height
    ),
    (512, 64, 128, 34)
  );
  drop(session);
  let cases = [(
    Region {
      x: 448,
      y: 0,
      width: 128,
      height: 34,
    },
    1,
  )];
  for (region, rotation) in cases {
    let root = tempfile::tempdir().expect("temp");
    let mut request = spec(&root.path().join("refused.mp4"));
    request.region = Some(region);
    let (mut platform, _driver) = quiet_platform(None);
    platform.info.rotation = rotation;
    let stages = platform.stages.clone();
    let error = PreparedSession::prepare(
      request,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
      platform,
    )
    .err()
    .expect("refused region");
    assert_eq!(
      (error.stage.as_str(), error.kind),
      ("region", ErrorKind::Capability)
    );
    assert_eq!(*stages.lock().expect("stages"), ["display"]);
    assert_eq!(std::fs::read_dir(root.path()).expect("parent").count(), 0);
  }
}
#[test]
fn automatic_encoder_selection_is_ordered_by_adapter_and_explicit_choices_are_exact() {
  use EncoderKind::*;
  assert_eq!(encoder_candidates(Auto, 0x1002), [Amf, Software]);
  assert_eq!(encoder_candidates(Auto, 0x10de), [Nvenc, Software]);
  assert_eq!(encoder_candidates(Auto, 0x8086), [Qsv, Software]);
  assert_eq!(encoder_candidates(Auto, 0x1414), [Software]);
  for explicit in [Amf, MediaFoundation, Software] {
    assert_eq!(encoder_candidates(explicit, 0x10de), [explicit]);
  }
}
#[test]
fn prepare_selects_one_tier_reports_every_rejection_and_never_hides_the_original_cause() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let prepare = |requested, reject: Vec<(EncoderKind, ErrorKind)>| {
    let root = tempfile::tempdir().expect("temp");
    let mut request = spec(&root.path().join("tier.mp4"));
    request.encoder = requested;
    let (mut platform, _driver) = quiet_platform(None);
    platform.reject = reject;
    let stages = platform.stages.clone();
    let result = PreparedSession::prepare(
      request,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
      platform,
    );
    let encoders = stages
      .lock()
      .expect("stages")
      .iter()
      .filter(|stage| **stage == "encoder")
      .count();
    (result, encoders, root)
  };
  // AMF refuses (for example a sub-minimum region): the software tier records instead.
  let (session, attempts, _root) = prepare(
    EncoderKind::Auto,
    vec![(EncoderKind::Amf, ErrorKind::Native)],
  );
  let session = session.expect("software tier selected");
  assert_eq!(attempts, 2);
  assert_eq!(session.spec.encoder, EncoderKind::Software);
  let tier = session.snapshot.tier.clone().expect("tier report");
  assert_eq!(
    (tier.requested, tier.selected),
    (EncoderKind::Auto, Some(EncoderKind::Software))
  );
  assert_eq!(tier.rejected.len(), 1);
  assert_eq!(tier.rejected[0].encoder, EncoderKind::Amf);
  assert_eq!(tier.rejected[0].error.native_code, Some(84));
  let formats: serde_json::Value = serde_json::from_slice(
    &std::fs::read(session.artifact.directory.join("source-formats.json")).expect("source formats"),
  )
  .expect("JSON");
  assert_eq!(formats["encoder"], "software");
  assert_eq!(formats["tones"], serde_json::json!([{ "range": "sdr" }]));
  drop(session);
  // The best tier is used when it works; nothing lower is even created.
  let (session, attempts, _root) = prepare(EncoderKind::Auto, Vec::new());
  let session = session.expect("AMF selected");
  assert_eq!(attempts, 1);
  assert_eq!(session.spec.encoder, EncoderKind::Amf);
  assert!(session.snapshot.tier.expect("tier").rejected.is_empty());
  // Every tier refused: the highest tier's cause stays primary and the rest are appended.
  let (error, attempts, _root) = prepare(
    EncoderKind::Auto,
    vec![
      (EncoderKind::Amf, ErrorKind::Native),
      (EncoderKind::Software, ErrorKind::Capability),
    ],
  );
  let error = error.err().expect("no tier");
  assert_eq!(attempts, 2);
  assert_eq!(
    (error.stage.as_str(), error.kind, error.native_code),
    ("quiet encoder", ErrorKind::Native, Some(84))
  );
  assert!(
    error
      .reason
      .starts_with("Amf refused; Software also rejected"),
    "{}",
    error.reason
  );
  assert!(error.recoverable_path.is_some());
  // Cancellation stops selection at once; an explicit tier is never substituted.
  let (error, attempts, _root) = prepare(
    EncoderKind::Auto,
    vec![(EncoderKind::Amf, ErrorKind::Cancelled)],
  );
  assert_eq!(error.err().expect("cancelled").kind, ErrorKind::Cancelled);
  assert_eq!(attempts, 1);
  let (error, attempts, _root) = prepare(
    EncoderKind::Amf,
    vec![(EncoderKind::Amf, ErrorKind::Native)],
  );
  let error = error.err().expect("explicit AMF failure");
  assert_eq!((attempts, error.reason.as_str()), (1, "Amf refused"));
}
#[test]
fn automatic_capture_tries_duplication_then_wgc_and_explicit_backends_are_exact() {
  use CaptureKind::*;
  assert_eq!(capture_candidates(Auto), [Dxgi, Wgc]);
  for explicit in [Dxgi, DxgiLegacy, Wgc, Synthetic] {
    assert_eq!(capture_candidates(explicit), [explicit]);
  }
}
#[test]
fn prepare_selects_one_capture_backend_and_reports_every_rejection() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let prepare = |requested, cursor, reject: Vec<(CaptureKind, ErrorKind)>| {
    let root = tempfile::tempdir().expect("temp");
    let mut request = spec(&root.path().join("capture-tier.mp4"));
    request.capture = requested;
    request.cursor = cursor;
    let (mut platform, _driver) = quiet_platform(None);
    platform.reject_capture = reject;
    let stages = platform.stages.clone();
    let result = PreparedSession::prepare(
      request,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
      platform,
    );
    let stages = stages.lock().expect("stages").clone();
    (result, stages, root)
  };
  let count = |stages: &[&str], stage| stages.iter().filter(|s| **s == stage).count();
  // Duplication works: it is selected, nothing else is created and the GPU cursor is composed.
  let (session, stages, _root) = prepare(CaptureKind::Auto, true, Vec::new());
  let session = session.expect("duplication selected");
  assert_eq!(session.spec.capture, CaptureKind::Dxgi);
  assert_eq!(
    (count(&stages, "capture"), count(&stages, "cursor")),
    (1, 1)
  );
  let tier = session.snapshot.capture_tier.clone().expect("capture tier");
  assert_eq!(
    (tier.requested, tier.selected),
    (CaptureKind::Auto, Some(CaptureKind::Dxgi))
  );
  assert!(tier.rejected.is_empty());
  drop(session);
  // Duplication refused: WGC records, the rejection keeps its cause, and the speculative cursor
  // shader is dropped because WGC draws the cursor itself.
  let (session, stages, _root) = prepare(
    CaptureKind::Auto,
    true,
    vec![(CaptureKind::Dxgi, ErrorKind::Capability)],
  );
  let session = session.expect("WGC selected");
  assert_eq!(session.spec.capture, CaptureKind::Wgc);
  assert_eq!(
    (count(&stages, "capture"), count(&stages, "cursor")),
    (2, 0)
  );
  assert!(session.compositor.is_none());
  let tier = session.snapshot.capture_tier.clone().expect("capture tier");
  assert_eq!(tier.selected, Some(CaptureKind::Wgc));
  assert_eq!(tier.rejected.len(), 1);
  assert_eq!(tier.rejected[0].capture, CaptureKind::Dxgi);
  assert_eq!(tier.rejected[0].error.native_code, Some(85));
  let formats: serde_json::Value = serde_json::from_slice(
    &std::fs::read(session.artifact.directory.join("source-formats.json")).expect("source formats"),
  )
  .expect("JSON");
  assert_eq!(formats["capture"], "wgc");
  drop(session);
  // Both refused, one for a real failure: that cause is primary and the unsupported one appended.
  // This is not "capture unavailable", which would hide a real failure behind a generic message.
  let (error, stages, _root) = prepare(
    CaptureKind::Auto,
    false,
    vec![
      (CaptureKind::Dxgi, ErrorKind::Capability),
      (CaptureKind::Wgc, ErrorKind::Native),
    ],
  );
  let error = error.err().expect("no backend");
  assert_eq!(count(&stages, "capture"), 2);
  assert_eq!(
    (error.stage.as_str(), error.kind),
    ("quiet capture", ErrorKind::Native)
  );
  assert!(
    error.reason.starts_with("Wgc refused; Dxgi also rejected"),
    "{}",
    error.reason
  );
  assert!(!error.capture_unavailable());
  assert!(error.recoverable_path.is_some());
  // Every automatic backend unsupported here: one typed unavailability, every cause kept.
  let (error, _stages, _root) = prepare(
    CaptureKind::Auto,
    false,
    vec![
      (CaptureKind::Dxgi, ErrorKind::Capability),
      (CaptureKind::Wgc, ErrorKind::Capability),
    ],
  );
  let error = error.err().expect("no backend");
  assert!(error.capture_unavailable(), "{error}");
  assert_eq!(
    error.reason,
    "quiet capture: Dxgi refused; Wgc also rejected: quiet capture: Wgc refused (Capability)"
  );
  // Cancellation stops selection at once; an explicit backend is never substituted.
  let (error, stages, _root) = prepare(
    CaptureKind::Auto,
    false,
    vec![(CaptureKind::Dxgi, ErrorKind::Cancelled)],
  );
  assert_eq!(error.err().expect("cancelled").kind, ErrorKind::Cancelled);
  assert_eq!(count(&stages, "capture"), 1);
  let (error, stages, _root) = prepare(
    CaptureKind::Dxgi,
    false,
    vec![(CaptureKind::Dxgi, ErrorKind::Capability)],
  );
  assert_eq!(error.err().expect("explicit").reason, "Dxgi refused");
  assert_eq!(count(&stages, "capture"), 1);
}
#[test]
fn an_odd_display_records_its_largest_even_area_and_the_crop_is_in_the_recorded_spec() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let root = tempfile::tempdir().expect("temp");
  let (mut platform, _driver) = quiet_platform(None);
  // A real odd-sized display; whole-display Prepare used to refuse it.
  (platform.info.width, platform.info.height) = (2556, 1179);
  let session = PreparedSession::prepare(
    spec(&root.path().join("odd.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
    platform,
  )
  .expect("odd display prepared");
  let even = Region {
    x: 0,
    y: 0,
    width: 2556,
    height: 1178,
  };
  assert_eq!(session.spec.region, Some(even));
  assert_eq!(
    (session.gpu.source_width, session.gpu.source_height),
    (2556, 1179)
  );
  assert_eq!((session.gpu.width, session.gpu.height), (2556, 1178));
}
#[test]
fn spanned_displays_form_one_canvas_and_each_must_be_distinct_and_on_the_same_adapter() {
  let primary = MonitorInfo {
    width: 2560,
    height: 1440,
    ..quiet_monitor()
  };
  let left = MonitorInfo {
    id: "left".into(),
    x: -2556,
    y: 148,
    width: 2556,
    height: 1179,
    ..quiet_monitor()
  };
  let other_adapter = MonitorInfo {
    id: "other".into(),
    x: 2560,
    adapter_luid: 99,
    ..primary.clone()
  };
  let infos = vec![primary.clone(), left.clone(), other_adapter];
  let mut request = spec(std::path::Path::new("unused.mp4"));
  request.monitor = "-2556,148,2556x1179".into();
  request.span = vec!["0,0,2560x1440".into()];
  let indices = select_outputs(&infos, &request).expect("span");
  assert_eq!(indices, [1, 0]);
  let (canvas_info, origins) = canvas(&[&left, &primary]).expect("canvas");
  assert_eq!(
    (
      canvas_info.x,
      canvas_info.y,
      canvas_info.width,
      canvas_info.height
    ),
    (-2556, 0, 5116, 1440)
  );
  assert_eq!(origins, [(0, 148), (2556, 0)]);
  assert_eq!(canvas_info.id, "left+quiet-fixture");
  let (single, origin) = canvas(&[&primary]).expect("single");
  assert_eq!((single.width, origin), (2560, vec![(0, 0)]));
  request.span = vec!["0,0,2560x1440".into(), "-2556,148,2556x1179".into()];
  assert!(select_outputs(&infos, &request).is_err(), "listed twice");
  request.span = vec!["2560,0,2560x1440".into()];
  assert_eq!(
    select_outputs(&infos, &request)
      .expect_err("cross adapter")
      .stage,
    "span"
  );
  request.span = vec!["9,9,9x9".into()];
  assert!(select_outputs(&infos, &request).is_err(), "missing display");
  assert!(canvas(&[]).is_err());
}
#[test]
fn prepare_locks_slot_phase_only_on_displays_whose_rate_is_a_multiple_of_the_frame_rate() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let failed = Err(EngineError::protocol("no DWM timing"));
  for (composition, locked) in [
    (Ok(Some(166_666)), true),
    (Ok(Some(83_333)), true),
    (Ok(Some(69_444)), false),
    (Ok(None), false),
    (failed, false),
  ] {
    let root = tempfile::tempdir().expect("temp");
    let (mut platform, _driver) = quiet_platform(None);
    platform.composition = composition.clone();
    let session = PreparedSession::prepare(
      spec(&root.path().join("phase.mp4")),
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
      platform,
    )
    .expect("prepared");
    assert_eq!(session.phase_lock.is_some(), locked, "{composition:?}");
    assert_eq!(
      session.slot_delay(1_000_000, 0, 0).expect("delay"),
      0,
      "no present observed yet"
    );
  }
}
#[test]
fn uncovered_spanned_pixels_are_counted_and_fully_covered_areas_report_none() {
  let canvas = MonitorInfo {
    x: -2556,
    y: 0,
    width: 5116,
    height: 1440,
    ..quiet_monitor()
  };
  let displays = [(-2556, 148, 0, 1327), (0, 0, 2560, 1440)];
  // The whole canvas: the left display leaves 148 rows above and 113 below uncovered.
  assert_eq!(
    uncovered_pixels(&canvas, None, &displays),
    Some(2556 * (148 + 113))
  );
  // An area across the seam, inside both displays, is fully covered.
  let seam = Region {
    x: 2456,
    y: 300,
    width: 200,
    height: 200,
  };
  assert_eq!(uncovered_pixels(&canvas, Some(seam), &displays), None);
  // Reaching above the left display uncovers its top-left corner only.
  let above = Region { y: 100, ..seam };
  assert_eq!(
    uncovered_pixels(&canvas, Some(above), &displays),
    Some(100 * 48)
  );
}
#[test]
fn automatic_capture_falls_back_to_wgc_when_duplication_delivers_no_first_frame() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let prepare = |requested, silent: Vec<CaptureKind>| {
    let root = tempfile::tempdir().expect("temp");
    let mut request = spec(&root.path().join("capture-probe.mp4"));
    request.capture = requested;
    request.cursor = true;
    let (mut platform, _driver) = quiet_platform(None);
    platform.silent_capture = silent;
    let stages = platform.stages.clone();
    let result = PreparedSession::prepare(
      request,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
      platform,
    );
    let stages = stages.lock().expect("stages").clone();
    (result, stages, root)
  };
  let count = |stages: &[&str], stage| stages.iter().filter(|s| **s == stage).count();
  // A duplication that delivers is selected and already holds its first frame.
  let (session, _stages, _root) = prepare(CaptureKind::Auto, Vec::new());
  let session = session.expect("duplication selected");
  assert_eq!(session.spec.capture, CaptureKind::Dxgi);
  assert!(session.capture.valid);
  drop(session);
  // A duplication that opens but stays silent is rejected with a timeout; WGC records instead,
  // without the GPU cursor compositor.
  let (session, stages, _root) = prepare(CaptureKind::Auto, vec![CaptureKind::Dxgi]);
  let session = session.expect("WGC selected after a silent duplication");
  assert_eq!(session.spec.capture, CaptureKind::Wgc);
  assert_eq!(
    (count(&stages, "capture"), count(&stages, "cursor")),
    (2, 0)
  );
  assert!(session.compositor.is_none());
  let tier = session.snapshot.capture_tier.clone().expect("capture tier");
  assert_eq!(tier.rejected.len(), 1);
  assert_eq!(tier.rejected[0].capture, CaptureKind::Dxgi);
  assert_eq!(
    (
      tier.rejected[0].error.kind,
      tier.rejected[0].error.stage.as_str()
    ),
    (ErrorKind::Timeout, "capture probe")
  );
  drop(session);
  // An explicit backend has nothing to fall back to: it is not probed, and Start's own
  // first-frame check reports a silent source.
  let (session, stages, _root) = prepare(CaptureKind::Dxgi, vec![CaptureKind::Dxgi]);
  let session = session.expect("explicit duplication prepared");
  assert_eq!(session.spec.capture, CaptureKind::Dxgi);
  assert!(!session.capture.valid);
  assert_eq!(count(&stages, "capture"), 1);
}
#[test]
fn capture_probe_is_bounded_and_yields_to_abort() {
  let (gpu, _driver) = super::super::gpu::mock::fixture();
  // Silent: a timeout within its bound, never an unbounded wait.
  let (capture, source) = super::super::capture::mock::fixture(&gpu);
  source.timeout.store(true, Ordering::Release);
  let mut capture = Composite::single(capture);
  let started = Instant::now();
  let error = first_frame(
    &mut capture,
    &gpu,
    &Interrupts::default(),
    Duration::from_millis(20),
  )
  .expect_err("silent capture times out");
  assert_eq!(error.kind, ErrorKind::Timeout);
  assert!(started.elapsed() < Duration::from_secs(1));
  assert!(!capture.valid);
  // Abort ends the probe at its next checkpoint as a cancellation, not a capture rejection.
  let interrupts = Interrupts::default();
  interrupts.abort_for_test();
  let error = first_frame(&mut capture, &gpu, &interrupts, Duration::from_secs(5))
    .expect_err("aborted probe");
  assert_eq!(error.kind, ErrorKind::Cancelled);
  // Delivering: the first frame is kept and its presents are not left for the frame trace.
  let (capture, _source) = super::super::capture::mock::fixture(&gpu);
  let mut capture = Composite::single(capture);
  first_frame(
    &mut capture,
    &gpu,
    &Interrupts::default(),
    Duration::from_millis(20),
  )
  .expect("first frame");
  assert!(capture.valid);
  assert_eq!(capture.take_presents(), 0);
}

#[test]
fn heartbeats_stop_with_the_worker_its_closed_event_channel_or_a_stalled_stage() {
  let watchdog = Arc::new(Watchdog::default());
  let period = Duration::from_millis(1);
  // A stopped worker sends nothing.
  let (events, received) = mpsc::sync_channel(4);
  beat(&AtomicBool::new(true), &watchdog, &events, period);
  assert!(received.try_recv().is_err());
  // A full channel ends the loop as stopped after the beats it could deliver.
  let stopped = AtomicBool::new(false);
  beat(&stopped, &watchdog, &events, period);
  assert!(stopped.load(Ordering::Acquire));
  assert_eq!(received.try_iter().count(), 4);
  // So does a closed one, once the event writer has gone.
  drop(received);
  let stopped = AtomicBool::new(false);
  beat(&stopped, &watchdog, &events, period);
  assert!(stopped.load(Ordering::Acquire));
  // A stalled native stage is reported once and ends the heartbeat without marking a stop.
  let (events, received) = mpsc::sync_channel(4);
  let _stalled = watchdog
    .begin(Stage::AudioSource, Duration::from_millis(1))
    .expect("native call");
  std::thread::sleep(Duration::from_millis(5));
  let stopped = AtomicBool::new(false);
  beat(&stopped, &watchdog, &events, period);
  assert!(!stopped.load(Ordering::Acquire));
  assert!(matches!(received.try_recv(), Ok(Event::StageStalled(_))));
  assert!(received.try_recv().is_err());
  // The stall's report cannot be delivered either: the worker is then stopped too.
  drop(received);
  beat(&stopped, &watchdog, &events, period);
  assert!(stopped.load(Ordering::Acquire));
}

#[test]
fn a_microphone_track_records_alone_or_mixed_and_mutes_while_recording_or_paused() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  for system in [false, true] {
    let root = tempfile::tempdir().expect("session temp");
    let mut specification = spec(&root.path().join("voice.mp4"));
    specification.audio = system;
    specification.microphone = true;
    let mut f = fixture(
      specification,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
    );
    f.session.start().expect("start");
    tick_until(&mut f.session, 3);
    f.session.snapshot.state = SessionState::Recording;
    f.session.microphone(false).expect("mute while recording");
    assert!(f.session.snapshot.microphone_muted);
    f.session.pause().expect("pause");
    f.session.microphone(true).expect("unmute while paused");
    assert!(!f.session.snapshot.microphone_muted);
    f.session.resume().expect("resume");
    let next = f.session.snapshot.frames_encoded + 2;
    tick_until(&mut f.session, next);
    f.session.finish().expect("one file with its audio track");
    assert!(f.session.artifact.output.is_file());
    assert!(
      f.session.snapshot.audio_frames > 0,
      "system audio: {system}"
    );
    assert!(!f.session.snapshot.microphone_lost);
  }
  // A recording without a microphone refuses the switch and keeps recording.
  let root = tempfile::tempdir().expect("session temp");
  let mut f = fixture(
    spec(&root.path().join("silent.mp4")),
    Arc::new(Interrupts::default()),
    Arc::new(Watchdog::default()),
  );
  f.session.start().expect("start");
  let error = f.session.microphone(false).expect_err("no microphone");
  assert_eq!(error.kind, ErrorKind::InvalidState);
}
