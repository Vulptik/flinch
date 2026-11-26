//! Worker-side owner loop. The parent process enforces deadlines around blocking native calls.
use super::{
  audio_service::AudioService,
  capture::{Acquisition, Capture, Composite},
  encoding::EncoderService,
  gpu::{self, Gpu},
  mux::Writer,
};
use crate::{
  EngineError, ErrorKind, Result,
  artifact::{self, SessionArtifact},
  control::{Inbox, Interrupts},
  limits,
  protocol::{self, Command, Event, Request, Response},
  session::StateMachine,
  time::{self, Scheduler, Timeline},
  types::*,
  watchdog::{Stage, Watchdog},
};
use std::{
  io::BufReader,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
  },
  time::{Duration, Instant},
};

trait DisplaySource {
  fn check(&self, gpu: &Gpu) -> Result<()>;
  fn recreate(&self, gpu: &Gpu, spec: &SessionSpec) -> Result<Composite>;
  fn memory(&self) -> Result<Option<crate::metrics::resources::GpuMemory>> {
    Ok(None)
  }
}
/// A captured output and its offset in the desktop cache.
type Placed = (gpu::Output, (u32, u32));
struct NativeDisplay {
  outputs: Vec<Placed>,
  synthetic: bool,
}
/// Windows withdrew desktop duplication (mode change, secure desktop, another fullscreen owner).
fn access_lost(error: &EngineError) -> bool {
  error.native_code == Some(windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST.0)
}
/// One capture part per output; a part that cannot be created fails the whole composite.
fn composite_capture(gpu: &Gpu, outputs: &[Placed], spec: &SessionSpec) -> Result<Composite> {
  outputs
    .iter()
    .map(|(output, origin)| Capture::new_at(gpu, output, spec.capture, spec.cursor, *origin))
    .collect::<Result<Vec<_>>>()
    .map(Composite::new)
}
impl DisplaySource for NativeDisplay {
  fn memory(&self) -> Result<Option<crate::metrics::resources::GpuMemory>> {
    use windows::{Win32::Graphics::Dxgi::*, core::Interface};
    let Some((output, _)) = self.outputs.first() else {
      return Ok(None);
    };
    let adapter: IDXGIAdapter3 = match output.adapter.cast() {
      Ok(adapter) => adapter,
      Err(error) if error.code() == windows::Win32::Foundation::E_NOINTERFACE => return Ok(None),
      Err(error) => return Err(EngineError::native("GPU memory observer", error)),
    };
    let mut memory = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    // SAFETY: read-only process-local accounting on this session's retained adapter, with a live output.
    unsafe { adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut memory) }
      .map_err(|error| EngineError::native("GPU memory query", error))?;
    Ok(Some(crate::metrics::resources::GpuMemory {
      local_usage_bytes: memory.CurrentUsage,
      local_budget_bytes: memory.Budget,
      local_reservation_bytes: memory.CurrentReservation,
      local_available_reservation_bytes: memory.AvailableForReservation,
    }))
  }
  fn check(&self, _: &Gpu) -> Result<()> {
    if self.synthetic {
      return Ok(());
    }
    super::session_status::check()?;
    for (output, _) in &self.outputs {
      // SAFETY: read-only descriptor query on the retained output; it never captures the desktop.
      let desc = unsafe { output.output.GetDesc1() }
        .map_err(|e| EngineError::native("display topology", e))?;
      let rect = desc.DesktopCoordinates;
      let info = &output.info;
      if !desc.AttachedToDesktop.as_bool()
        || (rect.left, rect.top) != (info.x, info.y)
        || rect.right - rect.left != info.width as i32
        || rect.bottom - rect.top != info.height as i32
        || desc.ColorSpace.0 as u32 != info.color_space
        || desc.Rotation.0 as u32 != info.rotation
      {
        return Err(EngineError::new(
          ErrorKind::DeviceLost,
          "display topology",
          format!(
            "Display {} detached or changed position, dimensions, rotation or color space",
            info.id
          ),
        ));
      }
    }
    Ok(())
  }
  fn recreate(&self, gpu: &Gpu, spec: &SessionSpec) -> Result<Composite> {
    composite_capture(gpu, &self.outputs, spec)
  }
}
/// Native owners acquired during Prepare. Quiet tests substitute COM doubles and run the same stages.
trait Platform {
  fn display(&mut self, spec: &SessionSpec) -> Result<MonitorInfo>;
  fn gpu(&mut self, info: &MonitorInfo, region: Option<Region>, fps: u32) -> Result<Gpu>;
  fn capture(&mut self, gpu: &Gpu, spec: &SessionSpec) -> Result<Composite>;
  fn compositor(
    &mut self,
    gpu: &Gpu,
    shader: super::cursor::ShaderCompile,
  ) -> Result<super::cursor::CursorCompositor>;
  /// Click highlights for a picture whose top-left lies at `origin` on the virtual screen.
  fn clicks(
    &mut self,
    gpu: &Gpu,
    origin: (i32, i32),
    shaders: super::cursor::ShaderCompile<super::clicks::Shaders>,
  ) -> Result<super::clicks::ClickLayer>;
  fn encoder(
    &mut self,
    gpu: &Gpu,
    info: &MonitorInfo,
    spec: &SessionSpec,
    watchdog: Arc<Watchdog>,
  ) -> Result<EncoderService>;
  fn audio(&mut self, spec: &SessionSpec, watchdog: Arc<Watchdog>) -> Result<AudioService>;
  /// DWM's composition period (the clock that stamps presents), for the hybrid sync policy.
  fn composition_period(&mut self) -> Result<Option<u64>>;
  fn source(self, spec: &SessionSpec) -> Result<Box<dyn DisplaySource>>;
}
struct NativePlatform {
  outputs: Vec<Placed>,
}
impl NativePlatform {
  fn adapter(&self) -> Result<&windows::Win32::Graphics::Dxgi::IDXGIAdapter1> {
    self
      .outputs
      .first()
      .map(|(output, _)| &output.adapter)
      .ok_or_else(|| EngineError::protocol("Prepare used the display before selecting it"))
  }
}
impl Platform for NativePlatform {
  fn display(&mut self, spec: &SessionSpec) -> Result<MonitorInfo> {
    if gpu::os_build()? < 19041 {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "OS",
        "Windows build 19041 is required",
      ));
    }
    let mut outputs: Vec<Option<gpu::Output>> = gpu::outputs()?.into_iter().map(Some).collect();
    let infos: Vec<MonitorInfo> = outputs.iter().flatten().map(|o| o.info.clone()).collect();
    let indices = select_outputs(&infos, spec)?;
    let (canvas, origins) = canvas(&indices.iter().map(|i| &infos[*i]).collect::<Vec<_>>())?;
    let placed: Vec<(i64, i64, i64, i64)> = indices
      .iter()
      .map(|index| {
        let info = &infos[*index];
        let (x, y) = (i64::from(info.x), i64::from(info.y));
        (x, y, x + i64::from(info.width), y + i64::from(info.height))
      })
      .collect();
    if let Some(uncovered) = uncovered_pixels(&canvas, spec.region, &placed) {
      eprintln!("Spanned area: {uncovered} pixels lie on no display and record black");
    }
    self.outputs = indices
      .iter()
      .zip(origins)
      .map(|(index, origin)| {
        outputs
          .get_mut(*index)
          .and_then(Option::take)
          .map(|output| (output, origin))
          .ok_or_else(|| EngineError::protocol("Selected display left the inventory"))
      })
      .collect::<Result<_>>()?;
    Ok(canvas)
  }
  fn gpu(&mut self, info: &MonitorInfo, region: Option<Region>, fps: u32) -> Result<Gpu> {
    Gpu::new(self.adapter()?, info.width, info.height, region, fps)
  }
  fn capture(&mut self, gpu: &Gpu, spec: &SessionSpec) -> Result<Composite> {
    composite_capture(gpu, &self.outputs, spec)
  }
  fn compositor(
    &mut self,
    gpu: &Gpu,
    shader: super::cursor::ShaderCompile,
  ) -> Result<super::cursor::CursorCompositor> {
    super::cursor::CursorCompositor::with_bytecode(gpu, &shader.wait(limits::PREPARE_TIMEOUT)?)
  }
  fn clicks(
    &mut self,
    gpu: &Gpu,
    origin: (i32, i32),
    shaders: super::cursor::ShaderCompile<super::clicks::Shaders>,
  ) -> Result<super::clicks::ClickLayer> {
    // Marks are sized for the display under the picture's center.
    let center = |start: i32, length: u32| start.saturating_add((length / 2) as i32);
    let scale = super::clicks::scale_at(
      center(origin.0, gpu.source_width),
      center(origin.1, gpu.source_height),
    )?;
    let painter = super::clicks::ClickPainter::new(gpu, &shaders.wait(limits::PREPARE_TIMEOUT)?)?;
    Ok(super::clicks::ClickLayer::new(painter, scale, origin))
  }
  fn encoder(
    &mut self,
    gpu: &Gpu,
    info: &MonitorInfo,
    spec: &SessionSpec,
    watchdog: Arc<Watchdog>,
  ) -> Result<EncoderService> {
    EncoderService::new_with_watchdog(
      &gpu.device,
      info.adapter_luid,
      (gpu.width, gpu.height),
      spec,
      gpu.conversion(),
      watchdog,
    )
  }
  fn audio(&mut self, spec: &SessionSpec, watchdog: Arc<Watchdog>) -> Result<AudioService> {
    AudioService::new_with_watchdog(
      spec.capture == CaptureKind::Synthetic,
      super::audio_service::Inputs::of(spec),
      watchdog,
    )
  }
  fn composition_period(&mut self) -> Result<Option<u64>> {
    gpu::composition_period()
  }
  fn source(self, spec: &SessionSpec) -> Result<Box<dyn DisplaySource>> {
    if self.outputs.is_empty() {
      return Err(EngineError::protocol(
        "Prepare finished without a selected display",
      ));
    }
    Ok(Box::new(NativeDisplay {
      outputs: self.outputs,
      synthetic: spec.capture == CaptureKind::Synthetic,
    }))
  }
}
/// Capture backends tried in order. Duplication is the fastest path and feeds the GPU cursor
/// compositor; WGC covers displays that duplication refuses (for example cross-adapter outputs).
fn capture_candidates(requested: CaptureKind) -> Vec<CaptureKind> {
  match requested {
    CaptureKind::Auto => vec![CaptureKind::Dxgi, CaptureKind::Wgc],
    explicit => vec![explicit],
  }
}
/// Selected once during Prepare: a later failure of the chosen backend recreates that backend only.
/// A duplication backend with another backend after it must also deliver a first frame, so one that
/// opens but stays silent falls back instead of failing at Start.
fn select_capture(
  platform: &mut impl Platform,
  gpu: &Gpu,
  spec: &SessionSpec,
  interrupts: &Interrupts,
) -> Result<(Composite, CaptureTierReport)> {
  let mut tier = CaptureTierReport {
    requested: spec.capture,
    ..Default::default()
  };
  let candidates = capture_candidates(spec.capture);
  let last = candidates.len().saturating_sub(1);
  for (index, candidate) in candidates.into_iter().enumerate() {
    let attempt = SessionSpec {
      capture: candidate,
      ..spec.clone()
    };
    let opened = platform.capture(gpu, &attempt).and_then(|mut capture| {
      // WGC delivers only after StartCapture, so only duplication is probed here.
      if index < last && candidate.is_duplication() {
        first_frame(&mut capture, gpu, interrupts, limits::CAPTURE_PROBE_TIMEOUT)?;
      }
      Ok(capture)
    });
    match opened {
      Ok(capture) => {
        tier.selected = Some(candidate);
        return Ok((capture, tier));
      },
      Err(error) if error.kind == ErrorKind::Cancelled => return Err(error),
      Err(error) => tier.rejected.push(CaptureRejection {
        capture: candidate,
        error,
      }),
    }
  }
  Err(capture_rejection(spec.capture, tier.rejected))
}
/// Acquire until the capture holds a frame, within `timeout`. The frame stays in the desktop cache,
/// so Start begins from it; its presents are not counted toward the recording's frame trace.
fn first_frame(
  capture: &mut Composite,
  gpu: &Gpu,
  interrupts: &Interrupts,
  timeout: Duration,
) -> Result<()> {
  let start = Instant::now();
  loop {
    interrupts.checkpoint("capture probe", false)?;
    capture.acquire(gpu)?;
    if capture.valid {
      capture.take_presents();
      return Ok(());
    }
    if start.elapsed() > timeout {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "capture probe",
        format!("No capture frame arrived within {} ms", timeout.as_millis()),
      ));
    }
    std::thread::sleep(Duration::from_millis(1));
  }
}
/// Every automatic backend unsupported here is one typed unavailability; otherwise the first real
/// failure is primary, since an unsupported tier only says it does not apply (each cause is kept).
fn capture_rejection(requested: CaptureKind, mut rejected: Vec<CaptureRejection>) -> EngineError {
  let unavailable = requested == CaptureKind::Auto
    && !rejected.is_empty()
    && rejected
      .iter()
      .all(|r| r.error.kind == ErrorKind::Capability);
  rejected.sort_by_key(|r| r.error.kind == ErrorKind::Capability);
  let error = first_rejection(
    rejected
      .into_iter()
      .map(|rejection| (format!("{:?}", rejection.capture), rejection.error)),
    "No capture backend was attempted",
  );
  if unavailable {
    EngineError::new(
      ErrorKind::Capability,
      crate::types::CAPTURE_UNAVAILABLE_STAGE,
      format!("{}: {}", error.stage, error.reason),
    )
  } else {
    error
  }
}
/// The highest tier's original cause stays primary; lower tiers are appended.
fn first_rejection(
  rejected: impl IntoIterator<Item = (String, EngineError)>,
  empty: &str,
) -> EngineError {
  let mut rejected = rejected.into_iter();
  let Some((_, mut error)) = rejected.next() else {
    return EngineError::protocol(empty);
  };
  for (tier, rejection) in rejected {
    error
      .reason
      .push_str(&format!("; {tier} also rejected: {rejection}"));
  }
  error
}
/// Tiers tried in order. Each vendor encoder needs the display's own adapter; the hardware MF
/// encoder joins automatic selection only after acceptance (it stalls on the tested AMD driver).
fn encoder_candidates(requested: EncoderKind, vendor_id: u32) -> Vec<EncoderKind> {
  match requested {
    EncoderKind::Auto => match vendor_encoder(vendor_id) {
      Some(vendor) => vec![vendor, EncoderKind::Software],
      None => vec![EncoderKind::Software],
    },
    explicit => vec![explicit],
  }
}
/// Selected once during Prepare: a later failure of the chosen encoder is a session failure.
fn select_encoder(
  platform: &mut impl Platform,
  gpu: &Gpu,
  info: &MonitorInfo,
  spec: &SessionSpec,
  watchdog: &Arc<Watchdog>,
) -> Result<(EncoderService, TierReport)> {
  let mut tier = TierReport {
    requested: spec.encoder,
    ..Default::default()
  };
  for candidate in encoder_candidates(spec.encoder, info.vendor_id) {
    let attempt = SessionSpec {
      encoder: candidate,
      ..spec.clone()
    };
    match platform.encoder(gpu, info, &attempt, watchdog.clone()) {
      Ok(encoder) => {
        tier.selected = Some(candidate);
        return Ok((encoder, tier));
      },
      Err(error) if error.kind == ErrorKind::Cancelled => return Err(error),
      Err(error) => tier.rejected.push(TierRejection {
        encoder: candidate,
        error,
      }),
    }
  }
  Err(first_rejection(
    tier
      .rejected
      .into_iter()
      .map(|rejection| (format!("{:?}", rejection.encoder), rejection.error)),
    "No encoder tier was attempted",
  ))
}
/// Pick the requested attached display. Only the AMF encoder requires the display's AMD adapter.
fn select_output<'a>(
  outputs: impl IntoIterator<Item = &'a MonitorInfo>,
  spec: &SessionSpec,
) -> Result<usize> {
  let (index, info) = outputs
    .into_iter()
    .enumerate()
    .find(|(_, info)| {
      if spec.monitor == "primary" {
        info.x == 0 && info.y == 0
      } else if let Some((x, y, width, height)) = monitor_geometry(&spec.monitor) {
        (info.x, info.y, info.width, info.height) == (x, y, width, height)
      } else {
        info.id.eq_ignore_ascii_case(&spec.monitor)
      }
    })
    .ok_or_else(|| {
      EngineError::new(
        ErrorKind::Capability,
        "monitor",
        "Requested attached monitor was not found",
      )
    })?;
  if let Some(vendor) = encoder_vendor(spec.encoder)
    && vendor.id != info.vendor_id
  {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "adapter",
      format!(
        "{:?} requires the display's {} adapter",
        spec.encoder, vendor.name
      ),
    ));
  }
  Ok(index)
}
pub(super) struct Vendor {
  pub id: u32,
  pub name: &'static str,
}
/// The adapter vendor a hardware encoder runs on (PCI vendor IDs).
pub(super) fn encoder_vendor(encoder: EncoderKind) -> Option<Vendor> {
  match encoder {
    EncoderKind::Amf => Some(Vendor {
      id: 0x1002,
      name: "AMD",
    }),
    EncoderKind::Nvenc => Some(Vendor {
      id: 0x10de,
      name: "NVIDIA",
    }),
    EncoderKind::Qsv => Some(Vendor {
      id: 0x8086,
      name: "Intel",
    }),
    EncoderKind::MediaFoundation | EncoderKind::Software | EncoderKind::Auto => None,
  }
}
/// The vendor encoder automatic selection tries first on an adapter.
fn vendor_encoder(vendor_id: u32) -> Option<EncoderKind> {
  [EncoderKind::Amf, EncoderKind::Nvenc, EncoderKind::Qsv]
    .into_iter()
    .find(|encoder| encoder_vendor(*encoder).is_some_and(|vendor| vendor.id == vendor_id))
}
/// The requested display, then each spanned one. Every display must be attached and listed once,
/// and share the first display's adapter: cross-adapter composition has no accepted path yet.
fn select_outputs(infos: &[MonitorInfo], spec: &SessionSpec) -> Result<Vec<usize>> {
  let first = select_output(infos, spec)?;
  let mut indices = vec![first];
  for id in &spec.span {
    let index = select_output(
      infos,
      &SessionSpec {
        monitor: id.clone(),
        ..spec.clone()
      },
    )?;
    if indices.contains(&index) {
      return Err(EngineError::protocol("A display is listed twice"));
    }
    if infos[index].adapter_luid != infos[first].adapter_luid {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "span",
        "Recording across displays on different graphics adapters is not supported yet",
      ));
    }
    indices.push(index);
  }
  Ok(indices)
}
/// The outputs' bounding box as one canvas, and each output's offset in it. The canvas keeps the
/// first output's adapter and properties; a single output is its own canvas.
fn canvas(infos: &[&MonitorInfo]) -> Result<(MonitorInfo, Vec<(u32, u32)>)> {
  let first = *infos
    .first()
    .ok_or_else(|| EngineError::protocol("No display selected"))?;
  if infos.len() == 1 {
    return Ok((first.clone(), vec![(0, 0)]));
  }
  let edges = |info: &MonitorInfo| {
    let (x, y) = (i64::from(info.x), i64::from(info.y));
    (x, y, x + i64::from(info.width), y + i64::from(info.height))
  };
  let (mut left, mut top, mut right, mut bottom) = edges(first);
  for info in infos {
    let (l, t, r, b) = edges(info);
    (left, top, right, bottom) = (left.min(l), top.min(t), right.max(r), bottom.max(b));
  }
  let size = |value: i64| u32::try_from(value).map_err(|e| EngineError::protocol(e.to_string()));
  let origins = infos
    .iter()
    .map(|info| {
      Ok((
        size(i64::from(info.x) - left)?,
        size(i64::from(info.y) - top)?,
      ))
    })
    .collect::<Result<Vec<_>>>()?;
  let position =
    |value: i64| i32::try_from(value).map_err(|e| EngineError::protocol(e.to_string()));
  Ok((
    MonitorInfo {
      id: infos
        .iter()
        .map(|info| info.id.as_str())
        .collect::<Vec<_>>()
        .join("+"),
      x: position(left)?,
      y: position(top)?,
      width: size(right - left)?,
      height: size(bottom - top)?,
      ..first.clone()
    },
    origins,
  ))
}
/// Pixels of the recorded area (the region, or the whole canvas) that no display covers, when any.
/// Displays never overlap on a Windows desktop, so covered areas add up.
fn uncovered_pixels(
  canvas: &MonitorInfo,
  region: Option<Region>,
  displays: &[(i64, i64, i64, i64)],
) -> Option<u64> {
  let (x, y) = (i64::from(canvas.x), i64::from(canvas.y));
  let (left, top, right, bottom) = match region {
    Some(r) => {
      let (l, t) = (x + i64::from(r.x), y + i64::from(r.y));
      (l, t, l + i64::from(r.width), t + i64::from(r.height))
    },
    None => (
      x,
      y,
      x + i64::from(canvas.width),
      y + i64::from(canvas.height),
    ),
  };
  let covered: i64 = displays
    .iter()
    .map(|(l, t, r, b)| {
      (right.min(*r) - left.max(*l)).max(0) * (bottom.min(*b) - top.max(*t)).max(0)
    })
    .sum();
  u64::try_from((right - left) * (bottom - top) - covered)
    .ok()
    .filter(|uncovered| *uncovered > 0)
}
/// An application monitor identity `x,y,WxH` in virtual-desktop pixels: an exact output rectangle.
pub(crate) fn monitor_geometry(monitor: &str) -> Option<(i32, i32, u32, u32)> {
  let mut parts = monitor.splitn(3, ',');
  let x = parts.next()?.trim().parse().ok()?;
  let y = parts.next()?.trim().parse().ok()?;
  let (width, height) = parts.next()?.trim().split_once('x')?;
  Some((x, y, width.parse().ok()?, height.parse().ok()?))
}
/// Consecutive stage durations against one monotonic origin, so stages sum to at most the total.
struct StageClock {
  origin: Instant,
  last: Instant,
}
impl StageClock {
  fn new() -> Self {
    let origin = Instant::now();
    Self {
      origin,
      last: origin,
    }
  }
  fn lap(&mut self) -> u64 {
    let now = Instant::now();
    let elapsed = now.duration_since(self.last).as_micros() as u64;
    self.last = now;
    elapsed
  }
  fn total(&self) -> u64 {
    self.last.duration_since(self.origin).as_micros() as u64
  }
}

pub struct PreparedSession {
  capture: Composite,
  encoder: EncoderService,
  writer: Option<Writer>,
  aac: Option<AudioService>,
  compositor: Option<super::cursor::CursorCompositor>,
  clicks: Option<super::clicks::ClickLayer>,
  gpu: Gpu,
  pub artifact: SessionArtifact,
  pub snapshot: SessionSnapshot,
  spec: SessionSpec,
  timeline: Timeline,
  scheduler: Scheduler,
  epoch: u64,
  active_qpc: u64,
  active_media: u64,
  first_encoded: bool,
  last_progress: Instant,
  last_slot: Option<u32>,
  /// The last frame's picture, written at Stop for the saved recording's notification.
  thumbnail: Option<crate::thumbnail::Thumbnail>,
  last_source_qpc_100ns: Option<u64>,
  /// PTS of the latest dated slot; an earlier redate must stay after it.
  last_dated: Option<u64>,
  /// Present-phase tracking under the hybrid sync policy; `None` when the display's rate is not a
  /// whole multiple of the frame rate, or for synthetic sources.
  phase_lock: Option<time::PhaseLock>,
  audio_packets_queued: u64,
  conversion_dirty: bool,
  content_dirty: bool,
  capture_recovery: Option<CaptureRecovery>,
  last_cursor: Option<(i32, i32, bool, u64)>,
  display: Box<dyn DisplaySource>,
  timings: super::telemetry::Timings,
  gpu_timers: super::telemetry::GpuTimers,
  resources: super::telemetry::ResourceSampler,
  start_requested: Option<Instant>,
  journal: artifact::Journal,
  interrupts: Arc<Interrupts>,
  watchdog: Arc<Watchdog>,
  writer_metrics: Option<Arc<crate::metrics::ReportSlot<crate::metrics::WriterReport>>>,
}
struct CaptureRecovery {
  /// Start of the window in which a valid replacement frame must arrive; it restarts after each
  /// refusal while the secure desktop shows.
  since: Instant,
  /// Ordinary recreation attempts made (refusals while the secure desktop shows are not counted).
  attempts: u8,
  original: EngineError,
  /// The latest failure, reported if recovery gives up.
  last: EngineError,
  /// The lost source is released and no replacement is running: hold the last picture.
  released: bool,
  next_attempt: Instant,
  /// Windows has refused duplication since then (secure desktop): no deadline applies, as the
  /// prompt can stay up for as long as the user leaves it.
  denied_since: Option<Instant>,
}
impl PreparedSession {
  pub fn new(spec: SessionSpec) -> Result<Self> {
    Self::with_interrupts(
      spec,
      Arc::new(Interrupts::default()),
      Arc::new(Watchdog::default()),
    )
  }
  fn with_interrupts(
    spec: SessionSpec,
    interrupts: Arc<Interrupts>,
    watchdog: Arc<Watchdog>,
  ) -> Result<Self> {
    Self::prepare(
      spec,
      interrupts,
      watchdog,
      NativePlatform {
        outputs: Vec::new(),
      },
    )
  }
  /// The one Prepare sequence. Lab provenance (binary hash, AAC calibration) is not part of it;
  /// the supervisor records that after the terminal result.
  fn prepare(
    mut spec: SessionSpec,
    interrupts: Arc<Interrupts>,
    watchdog: Arc<Watchdog>,
    mut platform: impl Platform,
  ) -> Result<Self> {
    interrupts.checkpoint("prepare", false)?;
    let mut clock = StageClock::new();
    let mut stages = crate::metrics::PrepareStages::default();
    spec.validate()?;
    // Device-independent; it overlaps display, GPU and capture creation instead of following them.
    // Automatic capture starts it speculatively and drops it when WGC (which draws the cursor) wins.
    let mut cursor_shader =
      if spec.cursor && (spec.capture.is_duplication() || spec.capture == CaptureKind::Auto) {
        Some(super::cursor::ShaderCompile::start()?)
      } else {
        None
      };
    let click_shaders = if spec.clicks {
      Some(super::clicks::start_compile()?)
    } else {
      None
    };
    stages.validate_us = clock.lap();
    let info = platform.display(&spec)?;
    if spec.region.is_none() && (info.width % 2 == 1 || info.height % 2 == 1) {
      // NV12 needs an even picture: an odd display records its largest even area, as the
      // app's area rounding does, and the crop is explicit in the session's recorded spec.
      spec.region = Some(Region {
        x: 0,
        y: 0,
        width: info.width & !1,
        height: info.height & !1,
      });
    }
    if let Some(region) = spec.region {
      // Fail before any session file exists; the crop is display-relative and never clipped.
      // A rotated display's desktop cache is upright, so its regions crop like any other.
      region.within(info.width, info.height)?;
    }
    stages.display_us = clock.lap();
    interrupts.checkpoint("prepare display", false)?;
    let artifact = SessionArtifact::create(&spec)?;
    let recovery_directory = artifact.directory.clone();
    (|| {
      let journal = artifact::Journal::open(&artifact.directory.join("measurements.jsonl"))?;
      artifact::write_json(&artifact.directory.join("capabilities.json"), &info)?;
      stages.artifact_us = clock.lap();
      let gpu = platform.gpu(&info, spec.region, spec.fps)?;
      if !spec.span.is_empty() {
        gpu.clear_desktop()?;
      }
      stages.gpu_us = clock.lap();
      interrupts.checkpoint("prepare GPU", false)?;
      let (capture, capture_tier) = select_capture(&mut platform, &gpu, &spec, &interrupts)?;
      let spec = SessionSpec {
        capture: capture_tier
          .selected
          .ok_or_else(|| EngineError::protocol("Capture selected without a backend"))?,
        ..spec
      };
      stages.capture_us = clock.lap();
      let shader = cursor_shader
        .take()
        .filter(|_| spec.capture.is_duplication());
      let compositor = if let Some(shader) = shader {
        let compositor = platform.compositor(&gpu, shader)?;
        stages.cursor_us = Some(clock.lap());
        Some(compositor)
      } else {
        None
      };
      let clicks = match click_shaders {
        Some(shaders) => Some(platform.clicks(&gpu, (info.x, info.y), shaders)?),
        None => None,
      };
      interrupts.checkpoint("prepare capture", false)?;
      let (encoder, tier) = select_encoder(&mut platform, &gpu, &info, &spec, &watchdog)?;
      let spec = SessionSpec {
        encoder: tier
          .selected
          .ok_or_else(|| EngineError::protocol("Encoder selected without a tier"))?,
        ..spec
      };
      stages.encoder_us = clock.lap();
      interrupts.checkpoint("prepare encoder", false)?;
      let aac = if spec.audio_track() {
        let audio = platform.audio(&spec, watchdog.clone())?;
        stages.audio_us = Some(clock.lap());
        Some(audio)
      } else {
        None
      };
      artifact::write_json(&artifact.directory.join("source-formats.json"),&serde_json::json!({
        "capture":spec.capture,"encoder":spec.encoder,"monitor":info.id,"tones":capture.tones(),
        "audio_mode":if !spec.audio_track() {"silent"} else if spec.capture==CaptureKind::Synthetic {"synthetic"} else if !spec.audio {"microphone"} else if spec.microphone {"system+microphone"} else {"system"},
        "audio":aac.as_ref().and_then(|audio|audio.source.as_ref()),
        "microphone":aac.as_ref().and_then(|audio|audio.microphone_source.as_ref()),
      }))?;
      interrupts.checkpoint("prepare audio", false)?;
      let gpu_timers = super::telemetry::GpuTimers::new(&gpu.device)?;
      let resources = super::telemetry::ResourceSampler::new()?;
      let composition = if spec.capture == CaptureKind::Synthetic {
        Ok(None)
      } else {
        platform.composition_period()
      };
      let display = platform.source(&spec)?;
      stages.reports_us = clock.lap();
      stages.total_us = clock.total();
      let mut timings = super::telemetry::Timings::default();
      timings.prepare.record(stages.total_us);
      let snapshot = SessionSnapshot {
        state: SessionState::Ready,
        texture_bytes: gpu.bytes,
        surfaces: gpu.surfaces.len() as u32,
        prepare_stages: Some(stages),
        tier: Some(tier),
        capture_tier: Some(capture_tier),
        conversion: Some(gpu.conversion()),
        ..Default::default()
      };
      // Without composition timing the session keeps the fixed grid, and says why.
      let phase_lock = match composition {
        Ok(period) => time::PhaseLock::for_composition(spec.fps, period),
        Err(error) => {
          eprintln!("Fixed slot grid: {error}");
          None
        },
      };
      Ok(Self {
        capture,
        encoder,
        writer: None,
        aac,
        compositor,
        clicks,
        gpu,
        artifact,
        snapshot,
        scheduler: Scheduler::new(spec.fps)?,
        spec,
        timeline: Timeline::default(),
        epoch: 0,
        active_qpc: 0,
        active_media: 0,
        first_encoded: false,
        last_progress: Instant::now(),
        last_slot: None,
        thumbnail: None,
        last_source_qpc_100ns: None,
        last_dated: None,
        phase_lock,
        audio_packets_queued: 0,
        conversion_dirty: true,
        content_dirty: true,
        capture_recovery: None,
        last_cursor: None,
        display,
        timings,
        gpu_timers,
        resources,
        start_requested: None,
        journal,
        interrupts,
        watchdog,
        writer_metrics: None,
      })
    })()
    .map_err(|mut error: EngineError| {
      error.recoverable_path = Some(recovery_directory);
      error
    })
  }
  fn start(&mut self) -> Result<()> {
    let _native = self.watchdog.begin(Stage::Capture, limits::START_TIMEOUT)?;
    self.interrupts.checkpoint("start", false)?;
    self.start_requested = Some(Instant::now());
    self.snapshot.first_media.start_qpc_100ns = Some(super::qpc()?);
    self.capture.start()?;
    if let Some(audio) = &self.aac {
      audio.start()?;
    }
    let start = Instant::now();
    while !self.capture.valid {
      self.interrupts.checkpoint("first frame", false)?;
      self.acquire()?;
      if start.elapsed() > limits::START_TIMEOUT {
        return Err(EngineError::new(
          ErrorKind::Timeout,
          "first frame",
          "No valid capture frame arrived",
        ));
      }
      std::thread::sleep(Duration::from_millis(1));
    }
    self.interrupts.checkpoint("start sources", false)?;
    self.snapshot.first_media.capture_qpc_100ns = Some(super::qpc()?);
    self.epoch = super::qpc()?;
    self.last_progress = Instant::now();
    self.active_qpc = self.epoch;
    if let Some(audio) = &self.aac {
      audio.map(self.epoch, 0)?;
    }
    self.timeline.start(self.epoch)?;
    self.snapshot.state = SessionState::Starting;
    Ok(())
  }
  fn audio_packets(&mut self) -> Result<()> {
    if self.writer.is_none() {
      return Ok(());
    }
    if let Some(audio) = &self.aac {
      for _ in 0..100 {
        let Some(packet) = audio.poll()? else {
          break;
        };
        self
          .writer
          .as_ref()
          .ok_or_else(|| EngineError::protocol("Writer unavailable"))?
          .push(packet)?;
        self.audio_packets_queued = self
          .audio_packets_queued
          .checked_add(1)
          .ok_or_else(|| EngineError::protocol("Audio packet counter overflow"))?;
      }
      (self.snapshot.audio_frames, self.snapshot.audio_silence) = audio.counts();
      self.snapshot.audio_diagnostics = audio.diagnostics();
      self.snapshot.microphone_lost = audio.microphone_lost();
    }
    Ok(())
  }
  fn write_audio(&mut self, until: u64) -> Result<()> {
    if let Some(audio) = &self.aac {
      audio.advance(until);
    }
    Ok(())
  }
  fn tick(&mut self) -> Result<bool> {
    self.tick_before(None)
  }
  /// Phase-lock sampling delay for the next slot (zero without a lock). Its nominal media time is
  /// mapped to QPC through this tick's own (now, elapsed) observation of the active span. The
  /// delay is below one frame period, so a pause or stop cut never loses the slot before it.
  fn slot_delay(&self, now: u64, elapsed: u64, nominal: u64) -> Result<u64> {
    let Some(lock) = &self.phase_lock else {
      return Ok(0);
    };
    let qpc = now
      .checked_add(nominal)
      .and_then(|value| value.checked_sub(elapsed))
      .ok_or_else(|| EngineError::protocol("Slot time precedes the clock origin"))?;
    Ok(lock.delay(qpc))
  }
  /// A locked slot that captured the present after or before the one it aimed at is dated on the
  /// present it shows. An earlier present counts only when it arrived for this slot.
  fn dated_by_capture(
    &mut self,
    now: u64,
    elapsed: u64,
    delay: u64,
    slot: time::Slot,
    fresh: bool,
  ) -> Result<time::Slot> {
    let Some(lock) = &mut self.phase_lock else {
      return Ok(slot);
    };
    // A due slot's dated PTS is never after the current media time.
    let sample = elapsed
      .checked_sub(slot.pts)
      .and_then(|behind| now.checked_sub(behind))
      .ok_or_else(|| EngineError::protocol("Sampled slot precedes the clock origin"))?;
    if let Some(shift) = lock.late_capture(delay, sample, self.last_source_qpc_100ns) {
      self.snapshot.phase_lock = Some(lock.report());
      return slot.sampled(shift);
    }
    let back = if fresh {
      lock.early_capture(sample, self.last_source_qpc_100ns)
    } else {
      None
    };
    let Some(back) = back.filter(|back| {
      slot
        .pts
        .checked_sub(*back)
        .is_some_and(|pts| self.last_dated.is_none_or(|last| pts > last))
    }) else {
      return Ok(slot);
    };
    lock.record_early();
    self.snapshot.phase_lock = Some(lock.report());
    slot.earlier(back)
  }
  fn tick_before(&mut self, cutoff: Option<u64>) -> Result<bool> {
    let limit = if self.snapshot.state == SessionState::Starting {
      limits::START_TIMEOUT
    } else {
      limits::CONTROL_TIMEOUT
    };
    let _native = self.watchdog.begin(Stage::Capture, limit)?;
    self
      .gpu_timers
      .collect(&self.gpu.context, &mut self.timings)?;
    let now = super::qpc()?;
    let elapsed = self.timeline.at(now)?;
    self.audio_packets()?;
    if self.timeline.paused() {
      if let Some(clicks) = &mut self.clicks {
        clicks.pause();
      }
      self.acquire()?;
    }
    if !self.timeline.paused() && cutoff.is_none_or(|cut| elapsed < cut) {
      let delay = self.slot_delay(now, elapsed, self.scheduler.next_time()?)?;
      // A slot is due only once its sampling instant has passed; saturating here made the
      // first slot due at once and dated it after the current media time.
      if let Some(due_at) = elapsed.checked_sub(delay)
        && let Some(slot) = self.scheduler.due(due_at)?
      {
        // A locked slot is dated when it was sampled (zero delay without a lock), with the delay
        // that made it due: after skipped slots the due slot's own delay can differ, and would
        // date it after the current media time.
        if let Some(lock) = &mut self.phase_lock {
          lock.commit(delay);
        }
        let slot = slot.sampled(delay)?;
        if self.spec.trace_frames {
          self.journal.push(&serde_json::json!({"kind":"frame_slot","qpc_100ns":now,"index":slot.index,"pts":slot.pts,"skipped":slot.skipped}))?;
        }
        self.capture.synthetic_index(slot.index);
        let fresh = matches!(self.acquire()?, Acquisition::New { .. });
        let slot = self.dated_by_capture(now, elapsed, delay, slot, fresh)?;
        self.last_dated = Some(slot.pts);
        self.record_lost_slots(slot.skipped)?;
        let pointer = &self.capture.pointer;
        let cursor_key = (pointer.x, pointer.y, pointer.visible, pointer.generation);
        if self.last_cursor != Some(cursor_key) {
          self.snapshot.cursor_updates += 1;
        }
        self.conversion_dirty |= self.compositor.is_some() && self.last_cursor != Some(cursor_key);
        if let Some(clicks) = &mut self.clicks {
          self.conversion_dirty |= clicks.tick(now);
        }
        let dirty = self.conversion_dirty;
        let pool = 0..self.gpu.surfaces.len() as u32;
        // While a lost source is recovering, the last converted picture is repeated, for
        // example while Windows withholds the desktop for a UAC prompt.
        let holding =
          !self.capture.valid && self.capture_recovery.is_some() && self.last_slot.is_some();
        let index = if !self.capture.valid && !holding {
          None
        } else if !dirty || holding {
          self
            .last_slot
            .filter(|i| !self.encoder.busy(*i))
            .or_else(|| pool.clone().find(|i| !self.encoder.busy(*i)))
        } else {
          pool.clone().find(|i| !self.encoder.busy(*i))
        };
        if let Some(index) = index {
          if (!dirty || holding) && self.last_slot.is_some() {
            let previous = self
              .last_slot
              .ok_or_else(|| EngineError::protocol("Missing NV12 cache"))?;
            if previous != index {
              // SAFETY: both textures remain immutable to the CPU; target was returned by its native sample owner.
              unsafe {
                self.gpu.context.CopyResource(
                  &self.gpu.surfaces[index as usize],
                  &self.gpu.surfaces[previous as usize],
                );
              }
            }
          } else {
            let source = if let Some(compositor) = &mut self.compositor {
              compositor.compose(&self.gpu, &self.capture.pointer)?
            } else {
              &self.gpu.desktop
            };
            let source = match &mut self.clicks {
              Some(clicks) => clicks.paint(&self.gpu, source, now)?,
              None => source,
            };
            let conversion = Instant::now();
            let query = self.gpu_timers.begin(&self.gpu.context);
            self.gpu.convert(source, index as usize)?;
            if let Some(query) = query {
              self.gpu_timers.end(&self.gpu.context, query);
            } else {
              self.timings.gpu_samples_skipped += 1;
            }
            self
              .timings
              .conversion_submit
              .record(conversion.elapsed().as_micros() as u64);
          }
          self.last_slot = Some(index);
          self.conversion_dirty = false;
          self.last_cursor = Some(cursor_key);
          // SAFETY: publish recorded GPU commands before the encoder's native owner consumes
          // the texture. Flush submits work; it does not wait for GPU completion or read pixels.
          unsafe {
            self.gpu.context.Flush();
          }
          if self.encoder.submit_with_source(
            &self.gpu.surfaces[index as usize],
            index,
            slot.pts,
            slot.duration,
            slot.index % u64::from(crate::types::keyframe_interval(self.spec.fps)) == 0,
            self.last_source_qpc_100ns,
          )? {
            if self.spec.trace_frames {
              self.journal.push(&serde_json::json!({"kind":"video_input_queued","index":slot.index,"source_qpc_100ns":self.last_source_qpc_100ns,"presents":self.capture.take_presents(),"queued_qpc_100ns":super::qpc()?}))?;
            }
            if self.content_dirty {
              self.snapshot.frames_new += 1;
            } else {
              self.snapshot.frames_reused += 1;
            }
            self.content_dirty = false;
          } else {
            self.record_lost_slots(1)?;
          }
        } else {
          self.record_lost_slots(1)?;
        }
      }
      self.write_audio(elapsed.saturating_sub(time::SECOND / 20))?;
    }
    let mut first = false;
    for _ in 0..16 {
      let Some(packet) = self.encoder.poll()? else {
        break;
      };
      self.snapshot.frames_encoded += 1;
      self.last_progress = Instant::now();
      if self.writer.is_none() {
        self.writer = Some(Writer::start_with_watchdog(
          &self.artifact.partial,
          (self.gpu.width, self.gpu.height, self.spec.fps),
          self.encoder.extra().to_vec(),
          self.aac.as_ref().map(|a| a.extra.clone()),
          self.watchdog.clone(),
        )?);
        self.writer_metrics = self.writer.as_ref().map(Writer::metrics_handle);
      }
      self
        .writer
        .as_ref()
        .ok_or_else(|| EngineError::protocol("Writer unavailable"))?
        .push(packet)?;
      if !self.first_encoded {
        if let Some(start) = self.start_requested.take() {
          self
            .timings
            .first_frame
            .record(start.elapsed().as_micros() as u64);
        }
        self.first_encoded = true;
        first = true;
      }
    }
    if let Some(writer) = &self.writer {
      writer.check()?;
    }
    if !self.timeline.paused()
      && self.capture.valid
      && self.last_progress.elapsed() > limits::ENCODER_STALL_TIMEOUT
    {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "encoder",
        format!(
          "No compressed frame for {} seconds",
          limits::ENCODER_STALL_TIMEOUT.as_secs()
        ),
      ));
    }
    self.snapshot.media_100ns = cutoff.map_or(elapsed, |cut| elapsed.min(cut));
    Ok(first)
  }
  fn check_topology(&self) -> Result<()> {
    self.display.check(&self.gpu)
  }
  fn record_lost_slots(&mut self, count: u64) -> Result<()> {
    if count != 0 {
      self.snapshot.slots_dropped = self
        .snapshot
        .slots_dropped
        .checked_add(count)
        .ok_or_else(|| EngineError::protocol("Lost-slot counter overflow"))?;
      self.snapshot.degraded = true;
    }
    Ok(())
  }
  fn acquire(&mut self) -> Result<Acquisition> {
    self.check_capture_recovery()?;
    let result = self.acquire_frame()?;
    self.check_capture_recovery()?;
    for error in self.capture.take_pointer_failures() {
      self
        .journal
        .push(&serde_json::json!({"kind":"pointer_seed_failed","error":&error}))?;
    }
    if self.capture.valid && self.capture_recovery.take().is_some() {
      // Source downtime is measured as lost slots; give resumed production its own output-progress budget.
      self.last_progress = Instant::now();
    }
    if let Acquisition::New { source_qpc_100ns } = &result {
      self.last_source_qpc_100ns = *source_qpc_100ns;
      if let (Some(lock), Some(present)) = (&mut self.phase_lock, source_qpc_100ns) {
        lock.observe(*present);
        self.snapshot.phase_lock = Some(lock.report());
      }
      // A full pool or a pause must not forget a newer desktop already copied to the GPU cache.
      self.conversion_dirty = true;
      self.content_dirty = true;
    }
    Ok(result)
  }
  fn check_capture_recovery(&self) -> Result<()> {
    if let Some(recovery) = &self.capture_recovery
      && recovery.denied_since.is_none()
      && recovery.since.elapsed() >= limits::CONTROL_TIMEOUT
    {
      let mut error = recovery.original.clone();
      error
        .reason
        .push_str("; no valid replacement capture frame within two seconds");
      return Err(error);
    }
    Ok(())
  }
  fn acquire_frame(&mut self) -> Result<Acquisition> {
    // Between recreation attempts the lost source is already released: hold the last picture.
    if let Some(recovery) = &self.capture_recovery
      && recovery.released
    {
      if Instant::now() < recovery.next_attempt {
        return Ok(Acquisition::Unchanged);
      }
      return self.recreate_capture();
    }
    match self.capture.acquire(&self.gpu) {
      Ok(value) => Ok(value),
      Err(error) => {
        if !self.spec.capture.is_duplication() || !access_lost(&error) {
          return Err(self.capture_recovery_error(error));
        }
        if self.capture_recovery.is_none() {
          let now = Instant::now();
          self.capture_recovery = Some(CaptureRecovery {
            since: now,
            attempts: 0,
            original: error.clone(),
            last: error.clone(),
            released: false,
            next_attempt: now,
            denied_since: None,
          });
          self
            .journal
            .push(&serde_json::json!({"kind":"capture_recovery","error":&error}))
            .map_err(|diagnostic| self.capture_recovery_error(diagnostic))?;
        }
        self.release_lost_capture(error)?;
        self.recreate_capture()
      },
    }
  }
  /// Windows refuses a second duplication of an output this process still duplicates, so every
  /// part of the lost composite is released before any part is recreated. The pointer generation
  /// survives the stop and still seeds the replacement's.
  fn release_lost_capture(&mut self, cause: EngineError) -> Result<()> {
    self
      .capture
      .stop()
      .map_err(|error| self.capture_recovery_error(error))?;
    let recovery = self.recovery_mut()?;
    recovery.released = true;
    recovery.last = cause;
    Ok(())
  }
  fn recovery_mut(&mut self) -> Result<&mut CaptureRecovery> {
    self
      .capture_recovery
      .as_mut()
      .ok_or_else(|| EngineError::protocol("Capture recovery context missing"))
  }
  /// One recreation attempt. A refusal while the secure desktop shows keeps holding the last
  /// picture and retries on a steady timer; other failures have a bounded number of spaced attempts.
  fn recreate_capture(&mut self) -> Result<Acquisition> {
    let recovery = self.recovery_mut()?;
    if recovery.attempts >= limits::CAPTURE_RECREATION_ATTEMPTS {
      let last = recovery.last.clone();
      return Err(self.capture_recovery_error(last));
    }
    self
      .check_topology()
      .map_err(|error| self.capture_recovery_error(error))?;
    let now = Instant::now();
    match self.display.recreate(&self.gpu, &self.spec) {
      Err(error) if error.native_code == Some(windows::Win32::Foundation::E_ACCESSDENIED.0) => {
        self.snapshot.degraded = true;
        let recovery = self.recovery_mut()?;
        let first = recovery.denied_since.is_none();
        recovery.denied_since.get_or_insert(now);
        recovery.since = now;
        recovery.next_attempt = now + limits::CAPTURE_DENIED_RETRY;
        recovery.last = error.clone();
        if first {
          self
            .journal
            .push(&serde_json::json!({"kind":"capture_access_denied","error":&error}))
            .map_err(|diagnostic| self.capture_recovery_error(diagnostic))?;
        }
        Ok(Acquisition::Unchanged)
      },
      Err(error) => self.failed_attempt(error, now),
      Ok(mut capture) => {
        let recovery = self.recovery_mut()?;
        recovery.attempts += 1;
        recovery.released = false;
        recovery.since = now;
        let held = recovery.denied_since.take();
        if let Some(denied) = held {
          self
            .journal
            .push(&serde_json::json!({"kind":"capture_access_restored","held_ms":denied.elapsed().as_millis() as u64}))
            .map_err(|diagnostic| self.capture_recovery_error(diagnostic))?;
        }
        capture.pointer.generation = self
          .capture
          .pointer
          .generation
          .checked_add(1)
          .ok_or_else(|| {
            EngineError::protocol("Pointer revision overflow during source recreation")
          })
          .map_err(|error| self.capture_recovery_error(error))?;
        self.capture = capture;
        self.last_source_qpc_100ns = None;
        self.conversion_dirty = true;
        self
          .capture
          .start()
          .map_err(|error| self.capture_recovery_error(error))?;
        self.snapshot.degraded = true;
        match self.capture.acquire(&self.gpu) {
          Ok(value) => Ok(value),
          // Access can be lost again during the same transition: release and try once more.
          Err(error) if access_lost(&error) => {
            self.release_lost_capture(error.clone())?;
            self.spaced_retry(error, now)
          },
          Err(error) => Err(self.capture_recovery_error(error)),
        }
      },
    }
  }
  /// An ordinary failed recreation: counted, then retried after a spacing or reported.
  fn failed_attempt(&mut self, error: EngineError, now: Instant) -> Result<Acquisition> {
    let recovery = self.recovery_mut()?;
    recovery.attempts += 1;
    // A refusal before this one no longer explains the outage; the two-second window applies.
    recovery.denied_since = None;
    self.spaced_retry(error, now)
  }
  fn spaced_retry(&mut self, error: EngineError, now: Instant) -> Result<Acquisition> {
    let recovery = self.recovery_mut()?;
    recovery.last = error.clone();
    if recovery.attempts >= limits::CAPTURE_RECREATION_ATTEMPTS {
      return Err(self.capture_recovery_error(error));
    }
    recovery.next_attempt = now + limits::CAPTURE_RETRY_SPACING;
    Ok(Acquisition::Unchanged)
  }
  fn capture_recovery_error(&self, secondary: EngineError) -> EngineError {
    let Some(recovery) = &self.capture_recovery else {
      return secondary;
    };
    let mut original = recovery.original.clone();
    original.reason.push_str(&format!(
      "; capture recreation after {} attempt(s): {secondary}",
      recovery.attempts
    ));
    original
  }
  fn pause(&mut self) -> Result<()> {
    self.interrupts.checkpoint("pause", true)?;
    let requested = Instant::now();
    let now = super::qpc()?;
    let elapsed = self.timeline.at(now)?;
    let boundary = time::frame_time(time::frame_ceil(elapsed, self.spec.fps)?, self.spec.fps)?;
    let cut = now + (boundary - elapsed);
    while super::qpc()? < cut {
      self.interrupts.checkpoint("pause cutoff", true)?;
      self.tick_before(Some(boundary))?;
      std::thread::sleep(Duration::from_millis(1));
    }
    if let Some(audio) = &self.aac {
      audio.pause(boundary)?;
    }
    self.interrupts.checkpoint("pause audio", true)?;
    self.audio_packets()?;
    self.timeline.pause_at(cut, super::qpc()?)?;
    self.snapshot.media_100ns = boundary;
    self.snapshot.state = SessionState::Paused;
    self
      .timings
      .pause
      .record(requested.elapsed().as_micros() as u64);
    Ok(())
  }
  /// Mutes or unmutes the microphone from now: while recording at the current media time, while
  /// paused from where the recording resumes.
  fn microphone(&mut self, audible: bool) -> Result<()> {
    let audio = self
      .aac
      .as_ref()
      .filter(|_| self.spec.microphone)
      .ok_or_else(|| {
        EngineError::new(
          ErrorKind::InvalidState,
          "microphone",
          "This recording has no microphone",
        )
      })?;
    let media = if self.timeline.paused() {
      self.snapshot.media_100ns
    } else {
      self.timeline.at(super::qpc()?)?
    };
    audio.microphone(audible, media)?;
    self.snapshot.microphone_muted = !audible;
    Ok(())
  }
  fn resume(&mut self) -> Result<()> {
    let _native = self
      .watchdog
      .begin(Stage::Capture, limits::CONTROL_TIMEOUT)?;
    self.interrupts.checkpoint("resume", false)?;
    let requested = Instant::now();
    self.acquire()?;
    while !self.capture.valid {
      self.check_capture_recovery()?;
      if requested.elapsed() >= limits::CONTROL_TIMEOUT {
        return Err(EngineError::new(
          ErrorKind::Timeout,
          "resume current frame",
          "No valid current source before the Resume deadline",
        ));
      }
      self
        .interrupts
        .checkpoint("resume source recovery", false)?;
      self.acquire()?;
      std::thread::sleep(Duration::from_millis(1));
    }
    self.interrupts.checkpoint("resume capture", false)?;
    let now = super::qpc()?;
    self.active_media = self.timeline.at(now)?;
    self.active_qpc = now;
    self.timeline.resume(now)?;
    if let Some(audio) = &self.aac {
      audio.map(now, self.active_media)?;
    }
    self.last_progress = Instant::now();
    self.snapshot.state = SessionState::Recording;
    self
      .timings
      .resume
      .record(requested.elapsed().as_micros() as u64);
    Ok(())
  }
  fn finish(&mut self) -> Result<()> {
    self.interrupts.checkpoint("finalize", true)?;
    let requested = Instant::now();
    if !self.timeline.paused() {
      self.pause()?;
    }
    let now = super::qpc()?;
    let cut = self.timeline.stop(now)?;
    {
      let _native = self
        .watchdog
        .begin(Stage::Capture, limits::CONTROL_TIMEOUT)?;
      self
        .capture
        .stop()
        .inspect_err(|error| self.watchdog.remember(error))?;
    }
    if let Some(audio) = &self.aac {
      audio.finish(cut)?;
    }
    self.audio_packets()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut draining = false;
    while !self.encoder.drained() {
      self.interrupts.checkpoint("encoder drain", true)?;
      if !draining {
        draining = self.encoder.drain()?;
      }
      if let Some(packet) = self.encoder.poll()? {
        self
          .writer
          .as_ref()
          .ok_or_else(|| EngineError::protocol("Writer unavailable"))?
          .push(packet)?;
        self.snapshot.frames_encoded += 1;
      }
      if Instant::now() > deadline {
        return Err(EngineError::new(
          ErrorKind::Timeout,
          "AMF drain",
          "Encoder drain exceeded five seconds",
        ));
      }
      std::thread::sleep(Duration::from_millis(1));
    }
    // The encoder has drained, so the last surface holds the file's final frame. A thumbnail is
    // a convenience: without one the app asks the Shell, so a failure is only reported.
    let picturing = Instant::now();
    self.thumbnail = self.last_slot.and_then(|slot| {
      match super::thumbnail::last_frame(&self.gpu, slot, &self.artifact.directory) {
        Ok(thumbnail) => Some(thumbnail),
        Err(error) => {
          eprintln!("Recording thumbnail unavailable: {error}");
          None
        },
      }
    });
    self
      .timings
      .thumbnail
      .record(picturing.elapsed().as_micros() as u64);
    let interrupts = self.interrupts.clone();
    self
      .writer
      .take()
      .ok_or_else(|| EngineError::protocol("Writer already finalized"))?
      .finish_cancellable(requested + limits::STOP_TIMEOUT, Some(cut), || {
        interrupts.checkpoint("writer finalization", true)
      })?;
    self.interrupts.checkpoint("file publication", true)?;
    self.snapshot.media_100ns = cut;
    // Windows players stop at a fragment split across their 256 KiB reads; move such fragments.
    let interrupts = self.interrupts.clone();
    let realigning = Instant::now();
    let realigned = crate::interop::realign(&self.artifact.partial, || {
      interrupts.checkpoint("file realignment", true)
    })?;
    // The elapsed time is Stop-path evidence: a rewrite is proportional to the file size.
    artifact::write_json(
      &self.artifact.directory.join("interop.json"),
      &serde_json::json!({
        "realignment": realigned,
        "elapsed_us": realigning.elapsed().as_micros() as u64,
      }),
    )?;
    let check = crate::recovery::inspect(&self.artifact.partial)?;
    if check.chunk_straddles != 0 {
      return Err(EngineError::new(
        ErrorKind::Io,
        "file realignment",
        format!(
          "{} fragments still straddle a 256 KiB read",
          check.chunk_straddles
        ),
      ));
    }
    check.validate_recording(self.spec.audio_track(), self.spec.fps, cut)?;
    check.validate_packet_counts(self.snapshot.frames_encoded, self.audio_packets_queued)?;
    artifact::write_json(&self.artifact.directory.join("container.json"), &check)?;
    self.snapshot.bytes_written = std::fs::metadata(&self.artifact.partial)
      .map_err(|e| EngineError::io("final file size", e))?
      .len();
    self
      .timings
      .finalize
      .record(requested.elapsed().as_micros() as u64);
    self.report_metrics()?;
    self.journal.push(&self.snapshot)?;
    self.journal.finish()?;
    self.watchdog.check()?;
    self.interrupts.checkpoint("publish", true)?;
    self.artifact.publish()?;
    Ok(())
  }
  fn abandon(&mut self) -> Result<()> {
    // Withdraw producers before a potentially blocking native close. Their owners
    // observe cancellation; the parent remains the bound on native teardown.
    if let Some(audio) = &self.aac {
      audio.cancel();
    }
    self.encoder.cancel();
    drop(self.writer.take());
    let _native = self.watchdog.begin(Stage::Capture, limits::ABORT_TIMEOUT)?;
    self
      .capture
      .stop()
      .inspect_err(|error| self.watchdog.remember(error))
  }
  fn sample_resources(&mut self) -> Result<()> {
    if let Some(report) = self.encoder.report()? {
      self.timings.encoder = Some(report);
      self.snapshot.encoder_metrics = Some(report);
      self.snapshot.first_media.submit_qpc_100ns = report.first_input_qpc_100ns;
      self.snapshot.first_media.encoded_qpc_100ns = report.first_output_qpc_100ns;
    }
    if let Some(metrics) = &self.writer_metrics
      && let Some(mut report) = metrics.read()?
    {
      (report.reports_coalesced, report.report_reads_contended) = metrics.coalesced();
      self.timings.writer = Some(report);
      self.snapshot.writer_metrics = Some(report);
      self.snapshot.first_media.sink_accept_qpc_100ns = report.first_packet_qpc_100ns;
    }
    let (cpu, sample) = self.resources.sample_detailed()?;
    self.snapshot.gpu_memory = self.display.memory()?;
    self.snapshot.first_media.validate()?;
    self.snapshot.cpu_core_equivalents = cpu;
    self.snapshot.private_bytes = sample.private_bytes;
    self.snapshot.process_resources = Some(sample);
    self.snapshot.resource_metrics_available = true;
    self.snapshot.diagnostics_lost = self.journal.lost;
    self.journal.push(&self.snapshot)?;
    Ok(())
  }
  fn report_metrics(&mut self) -> Result<()> {
    self.sample_resources()?;
    artifact::write_json(
      &self.artifact.directory.join("timings.json"),
      &self.timings.report(),
    )
  }
}

fn emit(sender: &SyncSender<Event>, event: Event) -> Result<()> {
  sender
    .try_send(event)
    .map_err(|e| EngineError::new(ErrorKind::Saturation, "worker events", e.to_string()))
}
/// Sends a heartbeat every `period` until `stopped` is set, reporting a stalled native stage once
/// instead. An event channel that is full or closed ends the worker as stopped.
fn beat(stopped: &AtomicBool, watchdog: &Watchdog, events: &SyncSender<Event>, period: Duration) {
  while !stopped.load(Ordering::Acquire) {
    std::thread::sleep(period);
    if let Err(error) = watchdog.check() {
      if events.try_send(Event::StageStalled(error)).is_err() {
        stopped.store(true, Ordering::Release);
      }
      break;
    }
    if events.try_send(Event::Heartbeat).is_err() {
      stopped.store(true, Ordering::Release);
      break;
    }
  }
}
/// Worker argument for a standby worker launched before the user asks to record.
pub const WARM_ARGUMENT: &str = "--warm";
/// One-time process costs that every first Prepare otherwise pays: DXGI enumeration and the
/// D3D11 device/video-processor driver initialization (measured: first Prepare
/// 84–98 ms, later ones in the same process 55–63 ms). Nothing created here is kept.
pub fn warm_up() -> Result<()> {
  let outputs = gpu::outputs()?;
  let output = outputs
    .first()
    .ok_or_else(|| EngineError::new(ErrorKind::Capability, "warm-up", "No attached display"))?;
  drop(Gpu::new(&output.adapter, 64, 64, None, 60)?);
  Ok(())
}
pub fn worker() -> Result<()> {
  worker_with(false)
}
/// `warm` runs [`warm_up`] beside the request loop after Hello; a request never waits for it,
/// and a warm-up failure is a diagnostic, because Prepare reports the same cause if it recurs.
pub fn worker_with(warm: bool) -> Result<()> {
  // DuplicateOutput1 needs per-monitor DPI awareness; failing to get it is reported, not fatal,
  // because WGC sessions do not need it and a duplication session then fails with its own cause.
  if let Err(error) = super::dpi::ensure_per_monitor_v2() {
    eprintln!("{error}; DuplicateOutput1 will be refused by Windows");
  }
  // Before anything slow, so starting a worker never shows the busy cursor.
  // Not fatal: the cursor then times out on its own, as it did before.
  if let Err(error) = super::launch_feedback::end() {
    eprintln!("{error}; Windows keeps the launch cursor until it times out");
  }
  let watchdog = Arc::new(Watchdog::default());
  let requests = Arc::new(Inbox::default());
  let outgoing_identity = requests.clone();
  let (send, events) = mpsc::sync_channel::<Event>(128);
  let stopped = Arc::new(AtomicBool::new(false));
  let done = stopped.clone();
  let (flushed_tx, flushed) = mpsc::sync_channel(1);
  std::thread::Builder::new()
    .name("video event writer".into())
    .spawn(move || {
      let mut output = std::io::stdout().lock();
      while let Ok(event) = events.recv() {
        let terminal = matches!(event, Event::Outcome(_));
        let session = match outgoing_identity.session() {
          Ok(session) => session,
          Err(error) => {
            eprintln!("{error}");
            done.store(true, Ordering::Release);
            break;
          },
        };
        if let Err(e) = protocol::write(
          &mut output,
          &Response {
            version: protocol::VERSION,
            session: session.clone(),
            event,
          },
        ) {
          eprintln!("{e}");
          done.store(true, Ordering::Release);
          break;
        }
        if terminal {
          if flushed_tx.try_send(()).is_err() {
            eprintln!("Terminal flush receiver closed");
          }
          break;
        }
      }
    })
    .map_err(|e| EngineError::io("event thread", e))?;
  emit(&send, Event::Hello)?;
  if warm {
    std::thread::Builder::new()
      .name("video warm-up".into())
      .spawn(|| {
        if let Err(error) = warm_up() {
          eprintln!("Standby warm-up: {error}");
        }
      })
      .map_err(|e| EngineError::io("warm-up thread", e))?;
  }
  let heartbeat = send.clone();
  let alive = stopped.clone();
  let stage_watchdog = watchdog.clone();
  std::thread::Builder::new()
    .name("video heartbeat".into())
    .spawn(move || {
      beat(
        &alive,
        &stage_watchdog,
        &heartbeat,
        Duration::from_millis(500),
      )
    })
    .map_err(|e| EngineError::io("heartbeat thread", e))?;
  let incoming = requests.clone();
  let rejections = send.clone();
  let cancelled = stopped.clone();
  std::thread::Builder::new()
    .name("video requests".into())
    .spawn(move || {
      let mut input = BufReader::new(std::io::stdin().lock());
      loop {
        match protocol::read::<Request>(&mut input) {
          Ok(Some(request)) => {
            let id = request.id;
            let reply = match incoming.submit(request) {
              Ok(reply) => reply,
              Err(error) => Some(Event::Rejected { id, error }),
            };
            if let Some(reply) = reply
              && let Err(error) = emit(&rejections, reply)
            {
              eprintln!("{error}");
              cancelled.store(true, Ordering::Release);
              break;
            }
          },
          Ok(None) => {
            cancelled.store(true, Ordering::Release);
            break;
          },
          Err(e) => {
            eprintln!("{e}");
            cancelled.store(true, Ordering::Release);
            break;
          },
        }
      }
      if let Err(error) = incoming.close() {
        eprintln!("{error}");
        cancelled.store(true, Ordering::Release);
      }
    })
    .map_err(|e| EngineError::io("request thread", e))?;
  let _com = super::Apartment::new()?;
  let _mf = super::MediaRuntime::new()?;
  run_session(
    &requests,
    send.clone(),
    &stopped,
    &watchdog,
    PreparedSession::with_interrupts,
  )?;
  stopped.store(true, Ordering::Release);
  // A confirmed flush replaces timing assumptions; a wedged pipe remains parent-bounded.
  drop(send);
  flushed
    .recv_timeout(limits::ABORT_TIMEOUT)
    .map_err(|e| EngineError::new(ErrorKind::Timeout, "terminal event flush", e.to_string()))?;
  Ok(())
}
fn run_session(
  requests: &Arc<Inbox>,
  send: SyncSender<Event>,
  stopped: &AtomicBool,
  watchdog: &Arc<Watchdog>,
  mut prepare: impl FnMut(SessionSpec, Arc<Interrupts>, Arc<Watchdog>) -> Result<PreparedSession>,
) -> Result<()> {
  let mut state = StateMachine::default();
  let mut prepared: Option<PreparedSession> = None;
  let mut last_stats = Instant::now();
  let mut terminal = None;
  let poll_timer = super::timer::Waiter::new()?;
  while !stopped.load(Ordering::Acquire) {
    let wait = if state.state() == SessionState::Paused {
      Duration::from_millis(100)
    } else if matches!(
      state.state(),
      SessionState::Starting | SessionState::Recording
    ) {
      Duration::from_millis(2)
    } else {
      Duration::from_millis(100)
    };
    let received = if wait == Duration::from_millis(2) {
      poll_timer.wait(1000)?;
      requests.receive(Duration::ZERO)
    } else {
      requests.receive(wait)
    };
    match received {
      Ok(Some(request)) => {
        if let Err(e) = state.request(&request.command) {
          let rejected = Event::Rejected {
            id: request.id,
            error: e.clone(),
          };
          requests.complete(request.id, rejected)?;
          emit(
            &send,
            Event::Rejected {
              id: request.id,
              error: e,
            },
          )?;
          continue;
        }
        emit(&send, Event::State(state.state()))?;
        let stopping = matches!(request.command, Command::Stop);
        let result: Result<()> = (|| {
          match request.command {
            Command::Prepare(spec) => {
              prepared = Some(prepare(
                spec,
                requests.interrupts.clone(),
                watchdog.clone(),
              )?);
              emit(
                &send,
                Event::Prepared(
                  prepared
                    .as_ref()
                    .ok_or_else(|| EngineError::protocol("Prepared session missing"))?
                    .artifact
                    .directory
                    .clone(),
                ),
              )?;
              state.confirm(SessionState::Ready)?;
            },
            Command::Start => {
              prepared
                .as_mut()
                .ok_or_else(|| EngineError::protocol("No prepared session"))?
                .start()?;
            },
            Command::Pause => {
              prepared
                .as_mut()
                .ok_or_else(|| EngineError::protocol("No session"))?
                .pause()?;
              state.confirm(SessionState::Paused)?;
            },
            Command::Resume => {
              prepared
                .as_mut()
                .ok_or_else(|| EngineError::protocol("No session"))?
                .resume()?;
              state.confirm(SessionState::Recording)?;
            },
            Command::Microphone(audible) => {
              prepared
                .as_mut()
                .ok_or_else(|| EngineError::protocol("No session"))?
                .microphone(audible)?;
            },
            Command::Stop => {
              state.confirm(SessionState::Finalizing)?;
              emit(&send, Event::State(state.state()))?;
              if let Some(p) = prepared.as_mut().filter(|p| p.first_encoded) {
                p.finish()?;
                state.confirm(SessionState::Completed)?;
                terminal = Some(if p.snapshot.degraded {
                  OutcomeKind::Degraded
                } else {
                  OutcomeKind::Completed
                });
              } else {
                if let Some(p) = &mut prepared {
                  p.abandon()?;
                }
                state.request(&Command::Abort)?;
                state.confirm(SessionState::Aborted)?;
                terminal = Some(OutcomeKind::Aborted);
              }
            },
            Command::Abort => {
              if let Some(p) = &mut prepared {
                p.abandon()?;
              }
              state.confirm(SessionState::Aborted)?;
              terminal = Some(OutcomeKind::Aborted);
            },
          }
          Ok(())
        })();
        match result {
          Ok(()) => {
            requests.complete(
              request.id,
              Event::Ack {
                id: request.id,
                state: state.state(),
              },
            )?;
            emit(
              &send,
              Event::Ack {
                id: request.id,
                state: state.state(),
              },
            )?;
            emit(&send, Event::State(state.state()))?;
          },
          Err(error)
            if error.kind == ErrorKind::Cancelled
              && requests.interrupts.checkpoint("control", stopping).is_err() =>
          {
            let rejected = Event::Rejected {
              id: request.id,
              error,
            };
            requests.complete(request.id, rejected.clone())?;
            emit(&send, rejected)?;
            // The interrupting command is already admitted and has a reserved priority slot.
            continue;
          },
          Err(e) => {
            watchdog.remember(&e);
            state.fail();
            let outcome = failure(&mut prepared, e);
            emit(&send, Event::Outcome(outcome))?;
            break;
          },
        }
      },
      Ok(None) => {},
      Err(error) if error.kind == ErrorKind::Cancelled => break,
      Err(error) => return Err(error),
    }
    if let Some(kind) = terminal {
      let outcome = if let Some(p) = prepared.as_mut() {
        p.snapshot.state = state.state();
        let published = matches!(kind, OutcomeKind::Completed | OutcomeKind::Degraded);
        let mut outcome = RecordingOutcome {
          thumbnail: p.thumbnail.clone().filter(|_| published),
          kind,
          output: if published {
            Some(p.artifact.output.clone())
          } else {
            None
          },
          partial: if p.artifact.partial.exists() {
            Some(p.artifact.partial.clone())
          } else {
            None
          },
          error: None,
          snapshot: p.snapshot.clone(),
        };
        if let Err(mut error) =
          artifact::write_json(&p.artifact.directory.join("result.json"), &outcome)
        {
          // If publication succeeded, a reporting error must retain the real output path.
          error.recoverable_path = outcome.output.clone().or_else(|| outcome.partial.clone());
          if matches!(outcome.kind, OutcomeKind::Completed | OutcomeKind::Degraded) {
            outcome.kind = OutcomeKind::Degraded;
            outcome.snapshot.degraded = true;
          }
          outcome.error = Some(error);
        }
        outcome
      } else if kind == OutcomeKind::Aborted {
        RecordingOutcome {
          thumbnail: None,
          kind,
          output: None,
          partial: None,
          error: None,
          snapshot: SessionSnapshot {
            state: SessionState::Aborted,
            ..Default::default()
          },
        }
      } else {
        return Err(EngineError::protocol("Terminal session missing"));
      };
      emit(&send, Event::Outcome(outcome))?;
      break;
    }
    if matches!(
      state.state(),
      SessionState::Starting | SessionState::Recording | SessionState::Paused
    ) && let Some(p) = &mut prepared
    {
      match p.tick() {
        Ok(first) => {
          if first && state.state() == SessionState::Starting {
            state.confirm(SessionState::Recording)?;
            p.snapshot.state = SessionState::Recording;
            emit(&send, Event::State(state.state()))?;
          }
        },
        Err(e) => {
          watchdog.remember(&e);
          let _native_stage = watchdog.begin(Stage::Capture, limits::CONTROL_TIMEOUT)?;
          state.fail();
          let outcome = failure(&mut prepared, e);
          emit(&send, Event::Outcome(outcome))?;
          break;
        },
      }
    }
    if last_stats.elapsed() >= Duration::from_secs(1) {
      if let Some(p) = &mut prepared {
        let _native_stage = watchdog.begin(Stage::Capture, limits::CONTROL_TIMEOUT)?;
        if let Err(error) = p
          .check_topology()
          .and_then(|_| p.sample_resources())
          .map_err(|error| p.capture_recovery_error(error))
        {
          watchdog.remember(&error);
          state.fail();
          let outcome = failure(&mut prepared, error);
          emit(&send, Event::Outcome(outcome))?;
          break;
        }
        if p.artifact.partial.exists() {
          match std::fs::metadata(&p.artifact.partial) {
            Ok(metadata) => p.snapshot.bytes_written = metadata.len(),
            Err(error) => {
              state.fail();
              let outcome = failure(&mut prepared, EngineError::io("output size", error));
              emit(&send, Event::Outcome(outcome))?;
              break;
            },
          }
        }
        emit(&send, Event::Stats(p.snapshot.clone()))?;
      }
      last_stats = Instant::now();
    }
  }
  Ok(())
}
fn failure(prepared: &mut Option<PreparedSession>, mut error: EngineError) -> RecordingOutcome {
  let (partial, snapshot) = if let Some(p) = prepared {
    p.snapshot.state = SessionState::Failed;
    error.recoverable_path = Some(p.artifact.partial.clone());
    (Some(p.artifact.partial.clone()), p.snapshot.clone())
  } else {
    (
      None,
      SessionSnapshot {
        state: SessionState::Failed,
        ..Default::default()
      },
    )
  };
  let mut outcome = RecordingOutcome {
    thumbnail: None,
    kind: OutcomeKind::Failed,
    output: None,
    partial,
    error: Some(error),
    snapshot,
  };
  if let Some(p) = prepared
    && let Err(e) = artifact::write_json(&p.artifact.directory.join("result.json"), &outcome)
  {
    if let Some(original) = &mut outcome.error {
      original
        .reason
        .push_str(&format!("; failure manifest: {e}"));
    }
    eprintln!("Failure manifest: {e}");
  }
  outcome
}

#[cfg(test)]
#[path = "runtime/tests.rs"]
mod tests;
