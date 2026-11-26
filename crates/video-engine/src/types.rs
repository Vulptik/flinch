use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
  Capability,
  DeviceLost,
  Timeout,
  Saturation,
  Protocol,
  Io,
  Cancelled,
  InvalidState,
  Native,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineError {
  pub kind: ErrorKind,
  pub stage: String,
  pub reason: String,
  pub native_code: Option<i32>,
  pub recoverable_path: Option<PathBuf>,
}
/// Stage of the error Prepare returns when every capture backend refused as unsupported here.
pub const CAPTURE_UNAVAILABLE_STAGE: &str = "capture selection";
/// Stage of the error Prepare returns when Windows' privacy settings block the microphone.
pub const MICROPHONE_BLOCKED_STAGE: &str = "microphone access";
/// Stage of the error Prepare returns when the chosen microphone (or any, for the default) is
/// not connected.
pub const MICROPHONE_MISSING_STAGE: &str = "microphone endpoint";
impl EngineError {
  /// No capture backend can record in this session: each one refused as unsupported (for
  /// example desktop duplication and WGC in some RDP sessions). Other failures keep their cause.
  pub fn capture_unavailable(&self) -> bool {
    self.kind == ErrorKind::Capability && self.stage == CAPTURE_UNAVAILABLE_STAGE
  }
  pub fn new(kind: ErrorKind, stage: impl Into<String>, reason: impl Into<String>) -> Self {
    Self {
      kind,
      stage: stage.into(),
      reason: reason.into(),
      native_code: None,
      recoverable_path: None,
    }
  }
  pub fn io(stage: &str, error: std::io::Error) -> Self {
    Self::new(ErrorKind::Io, stage, error.to_string())
  }
  pub fn protocol(reason: impl Into<String>) -> Self {
    Self::new(ErrorKind::Protocol, "protocol", reason)
  }
  pub fn native(stage: &str, error: windows::core::Error) -> Self {
    use windows::Win32::{
      Graphics::Dxgi::{
        DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_RESET,
      },
      Media::Audio::{AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED},
    };
    let kind = if [
      AUDCLNT_E_DEVICE_INVALIDATED,
      AUDCLNT_E_RESOURCES_INVALIDATED,
      DXGI_ERROR_DEVICE_REMOVED,
      DXGI_ERROR_DEVICE_RESET,
      DXGI_ERROR_DEVICE_HUNG,
      DXGI_ERROR_ACCESS_LOST,
    ]
    .contains(&error.code())
    {
      ErrorKind::DeviceLost
    } else {
      ErrorKind::Native
    };
    Self {
      native_code: Some(error.code().0),
      ..Self::new(kind, stage, error.to_string())
    }
  }
}
impl fmt::Display for EngineError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}: {} ({:?})", self.stage, self.reason, self.kind)
  }
}
impl std::error::Error for EngineError {}

/// Where captured BGRA becomes NV12: the D3D11 video processor, or the CPU on adapters that
/// have none (the basic display adapter of machines and VMs without a GPU; measured).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversionKind {
  #[default]
  Gpu,
  Cpu,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
  #[default]
  Dxgi,
  DxgiLegacy,
  Wgc,
  Synthetic,
  /// Prepare tries DuplicateOutput1 and then WGC, once, and reports every rejection. The chosen
  /// backend never changes during the session; recovery recreates the same backend.
  Auto,
}
impl CaptureKind {
  pub fn is_duplication(self) -> bool {
    matches!(self, Self::Dxgi | Self::DxgiLegacy)
  }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EncoderKind {
  #[default]
  Amf,
  MediaFoundation,
  /// Windows' software H.264 encoder: no vendor runtime required.
  Software,
  /// NVIDIA NVENC on the display's NVIDIA adapter.
  Nvenc,
  /// Intel Quick Sync on the display's Intel adapter (not yet run on Intel hardware).
  Qsv,
  /// Prepare selects the highest tier that initializes, once, and reports every rejection.
  Auto,
}
/// One encoder tier that Prepare tried and rejected, with its original cause.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TierRejection {
  pub encoder: EncoderKind,
  pub error: EngineError,
}
/// One capture backend that Prepare tried and rejected, with its original cause.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureRejection {
  pub capture: CaptureKind,
  pub error: EngineError,
}
/// The capture backend chosen during Prepare. It never changes during the session.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureTierReport {
  pub requested: CaptureKind,
  pub selected: Option<CaptureKind>,
  pub rejected: Vec<CaptureRejection>,
}
/// The encoder chosen during Prepare. It never changes during the session.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TierReport {
  pub requested: EncoderKind,
  pub selected: Option<EncoderKind>,
  pub rejected: Vec<TierRejection>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Quality {
  Cqp { i: u8, p: u8 },
  Vbr { bitrate: u32 },
}
impl Default for Quality {
  fn default() -> Self {
    Self::Cqp { i: 21, p: 23 }
  }
}

/// Cadences the app records: video 30/60, GIF 15/30, plus the 120 FPS high-refresh path.
pub const SUPPORTED_FPS: [u32; 4] = [15, 30, 60, 120];

/// Seconds between forced keyframes (IDR pictures). The recorder forces one on each boundary and
/// every encoder gets the same period, so no encoder inserts keyframes of its own. Ten seconds
/// (x264's usual 250 frames at 30 FPS is about eight) cut text and UI recordings by about 60%
/// against two at equal quality, and moving footage by about 1% (measured with
/// `flinch-video-lab reencode`). Players still seek to any frame.
pub const KEYFRAME_SECONDS: u32 = 10;

/// Frames from one forced keyframe to the next at `fps`.
pub const fn keyframe_interval(fps: u32) -> u32 {
  fps.saturating_mul(KEYFRAME_SECONDS)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionSpec {
  pub monitor: String,
  pub fps: u32,
  pub capture: CaptureKind,
  pub encoder: EncoderKind,
  pub quality: Quality,
  pub cursor: bool,
  /// Draw click highlights into the picture (`crate::clicks`).
  #[serde(default)]
  pub clicks: bool,
  pub audio: bool,
  pub audio_device: Option<String>,
  /// Also record a microphone, mixed with system audio (`audio`) into the one AAC track.
  #[serde(default)]
  pub microphone: bool,
  /// The microphone endpoint id; `None` follows the Windows default microphone.
  #[serde(default)]
  pub microphone_device: Option<String>,
  pub output: PathBuf,
  pub trace_frames: bool,
  /// Area to record, in the selected display's pixels; `None` records the whole display. With
  /// `span`, it is relative to the bounding box of all the displays.
  #[serde(default)]
  pub region: Option<Region>,
  /// Further displays composed with `monitor` into one picture (same identities as `monitor`).
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub span: Vec<String>,
}
/// Even-sized, display-relative crop. Containment in the display is checked during Prepare.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Region {
  pub x: u32,
  pub y: u32,
  pub width: u32,
  pub height: u32,
}
impl Region {
  /// The smallest width and height Windows' software H.264 encoder accepts, measured
  /// with `cargo xtask test:video:smallest` on both conversion tiers. Software is the fallback
  /// on every machine; a vendor encoder that needs more (AMF: 96×32) falls back to it.
  pub const MIN_SIZE: u32 = 34;
  pub fn validate(&self) -> Result<()> {
    if self.width < Self::MIN_SIZE
      || self.height < Self::MIN_SIZE
      || !self.width.is_multiple_of(2)
      || !self.height.is_multiple_of(2)
    {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "region",
        format!(
          "Region {}x{} must be even and at least {}x{}",
          self.width,
          self.height,
          Self::MIN_SIZE,
          Self::MIN_SIZE
        ),
      ));
    }
    Ok(())
  }
  /// Refuses a region that leaves the display rather than clipping it silently.
  pub fn within(&self, width: u32, height: u32) -> Result<()> {
    self.validate()?;
    let right = self.x.checked_add(self.width);
    let bottom = self.y.checked_add(self.height);
    if right.is_none_or(|right| right > width) || bottom.is_none_or(|bottom| bottom > height) {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "region",
        format!(
          "Region {}x{} at {},{} leaves the {}x{} display",
          self.width, self.height, self.x, self.y, width, height
        ),
      ));
    }
    Ok(())
  }
}
impl SessionSpec {
  /// Whether the recording has an AAC track: system audio, a microphone or both.
  pub fn audio_track(&self) -> bool {
    self.audio || self.microphone
  }
  pub fn validate(&self) -> Result<()> {
    if self
      .microphone_device
      .as_ref()
      .is_some_and(|id| !self.microphone || id.is_empty() || id.len() > 512)
    {
      return Err(EngineError::protocol("Invalid microphone endpoint"));
    }
    if !SUPPORTED_FPS.contains(&self.fps) {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "spec",
        format!("{} FPS is not supported; use 15, 30, 60 or 120", self.fps),
      ));
    }
    if let Some(region) = &self.region {
      region.validate()?;
    }
    if self.monitor.is_empty() || self.monitor.len() > 512 {
      return Err(EngineError::protocol("Invalid monitor identity"));
    }
    if self.span.len() > 7 || self.span.iter().any(|id| id.is_empty() || id.len() > 512) {
      return Err(EngineError::protocol("Invalid spanned displays"));
    }
    if self
      .output
      .extension()
      .is_none_or(|x| !x.eq_ignore_ascii_case("mp4"))
    {
      return Err(EngineError::protocol("Output must have .mp4 extension"));
    }
    match self.quality {
      Quality::Cqp { i, p } if i > 51 || p > 51 => {
        return Err(EngineError::protocol("QP must be in 0..=51"));
      },
      Quality::Vbr { bitrate } if !(100_000..=100_000_000).contains(&bitrate) => {
        return Err(EngineError::protocol(
          "Bitrate must be in 100000..=100000000",
        ));
      },
      _ => {},
    }
    Ok(())
  }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitorInfo {
  pub id: String,
  pub adapter_luid: i64,
  pub adapter: String,
  pub vendor_id: u32,
  pub driver: String,
  pub width: u32,
  pub height: u32,
  pub x: i32,
  pub y: i32,
  pub rotation: u32,
  pub color_space: u32,
  pub hdr: bool,
  /// Current mode's refresh rate in whole hertz; `None` when Windows reports a hardware default.
  #[serde(default)]
  pub refresh_hz: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityReport {
  pub os_build: u32,
  pub architecture: String,
  pub monitors: Vec<MonitorInfo>,
  pub amf_runtime: Option<String>,
  /// Highest NVENC API the NVIDIA driver supports, as `major.minor`.
  #[serde(default)]
  pub nvenc_api: Option<String>,
  /// Intel runtimes the oneVPL dispatcher finds.
  #[serde(default)]
  pub qsv_runtimes: Option<String>,
  pub diagnostics: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
  #[default]
  Created,
  Preparing,
  Ready,
  Starting,
  Recording,
  Pausing,
  Paused,
  Resuming,
  Stopping,
  Finalizing,
  Completed,
  Failed,
  Aborting,
  Aborted,
}
impl SessionState {
  pub fn terminal(self) -> bool {
    matches!(self, Self::Completed | Self::Failed | Self::Aborted)
  }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
  pub state: SessionState,
  pub media_100ns: u64,
  pub frames_new: u64,
  pub frames_reused: u64,
  pub slots_dropped: u64,
  pub frames_encoded: u64,
  pub bytes_written: u64,
  pub texture_bytes: u64,
  /// Encode surfaces in the pool: sized to the frame rate, trimmed to the texture budget.
  #[serde(default)]
  pub surfaces: u32,
  pub audio_frames: u64,
  pub audio_silence: u64,
  /// The microphone disappeared mid-recording and recorded silence since.
  #[serde(default)]
  pub microphone_lost: bool,
  /// The microphone is muted from the recording controls.
  #[serde(default)]
  pub microphone_muted: bool,
  #[serde(default)]
  pub audio_diagnostics: crate::audio::normalize::AudioDiagnostics,
  pub diagnostics_lost: u64,
  pub degraded: bool,
  #[serde(default)]
  pub private_bytes: u64,
  #[serde(default)]
  pub cpu_core_equivalents: f64,
  #[serde(default)]
  pub cursor_updates: u64,
  #[serde(default)]
  pub resource_metrics_available: bool,
  #[serde(default)]
  pub encoder_metrics: Option<crate::metrics::EncoderReport>,
  #[serde(default)]
  pub writer_metrics: Option<crate::metrics::WriterReport>,
  #[serde(default)]
  pub process_resources: Option<crate::metrics::resources::ProcessResources>,
  #[serde(default)]
  pub combined_resources: Option<crate::metrics::resources::CombinedResources>,
  #[serde(default)]
  pub first_media: crate::metrics::MediaMilestones,
  #[serde(default)]
  pub prepare_stages: Option<crate::metrics::PrepareStages>,
  #[serde(default)]
  pub tier: Option<TierReport>,
  #[serde(default)]
  pub capture_tier: Option<CaptureTierReport>,
  /// Set in Prepare: GPU conversion, or the CPU conversion of the GPU-less tier.
  #[serde(default)]
  pub conversion: Option<ConversionKind>,
  #[serde(default)]
  pub phase_lock: Option<crate::time::PhaseLockReport>,
  #[serde(default)]
  pub gpu_memory: Option<crate::metrics::resources::GpuMemory>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
  Completed,
  Degraded,
  Failed,
  Aborted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingOutcome {
  pub kind: OutcomeKind,
  pub output: Option<PathBuf>,
  pub partial: Option<PathBuf>,
  pub error: Option<EngineError>,
  pub snapshot: SessionSnapshot,
  /// The published recording's last frame, in the session directory; only with an output.
  #[serde(default)]
  pub thumbnail: Option<crate::thumbnail::Thumbnail>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Track {
  Video,
  Audio,
}
#[derive(Clone, Debug)]
pub struct EncodedPacket {
  pub track: Track,
  pub pts: i64,
  pub dts: i64,
  pub duration: u64,
  pub keyframe: bool,
  pub data: Vec<u8>,
}
impl EncodedPacket {
  /// The current native pipeline deliberately excludes B-frame reordering.
  pub fn validate(&self) -> Result<()> {
    if self.data.is_empty()
      || self.data.len() > 16 * 1024 * 1024
      || self.pts < 0
      || self.dts != self.pts
      || self.duration == 0
      || i64::try_from(self.duration)
        .ok()
        .and_then(|duration| self.pts.checked_add(duration))
        .is_none()
    {
      return Err(EngineError::protocol(format!(
        "Invalid compressed {:?} packet: pts={}, dts={}, duration={}, bytes={}",
        self.track,
        self.pts,
        self.dts,
        self.duration,
        self.data.len()
      )));
    }
    Ok(())
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn keyframe_interval_spans_whole_seconds_and_fits_every_encoders_gop_field() {
    for fps in SUPPORTED_FPS {
      let interval = keyframe_interval(fps);
      assert_eq!(interval, fps * KEYFRAME_SECONDS);
      // Quick Sync's GopPicSize is 16-bit; NVENC, AMF and Media Foundation take 32 bits.
      assert!(u16::try_from(interval).is_ok(), "{fps} FPS");
    }
  }
  #[test]
  fn compressed_packet_extents_cannot_wrap_native_end_times() {
    let mut packet = EncodedPacket {
      track: Track::Video,
      pts: i64::MAX,
      dts: i64::MAX,
      duration: 1,
      keyframe: true,
      data: vec![1],
    };
    assert!(packet.validate().is_err());
    packet.pts = 0;
    packet.dts = 0;
    packet.duration = u64::MAX;
    assert!(packet.validate().is_err());
    packet.duration = 1;
    packet.validate().expect("representable packet");
  }
  #[test]
  fn validates_settings_without_mutating_them() {
    let mut spec = SessionSpec {
      monitor: "primary".into(),
      fps: 60,
      capture: CaptureKind::Dxgi,
      encoder: EncoderKind::Amf,
      quality: Quality::default(),
      cursor: true,
      clicks: false,
      audio: false,
      audio_device: None,
      microphone: false,
      microphone_device: None,
      output: "test.mp4".into(),
      trace_frames: false,
      region: None,
      span: Vec::new(),
    };
    assert!(spec.validate().is_ok());
    for fps in SUPPORTED_FPS {
      spec.fps = fps;
      spec.validate().expect("supported cadence");
    }
    for fps in [0, 24, 25, 45, 144, 240] {
      spec.fps = fps;
      let error = spec.validate().expect_err("unsupported cadence");
      assert_eq!(error.kind, ErrorKind::Capability);
    }
    spec.fps = 120;
    spec.output = "test.avi".into();
    assert!(spec.validate().is_err());
    spec.output = "test.mp4".into();
    spec.quality = Quality::Cqp { i: 52, p: 23 };
    assert!(spec.validate().is_err());
    spec.quality = Quality::Vbr { bitrate: 0 };
    assert!(spec.validate().is_err());
  }
  #[test]
  fn regions_are_even_bounded_and_must_fit_their_display() {
    let region = Region {
      x: 10,
      y: 20,
      width: 100,
      height: 50,
    };
    region.within(110, 70).expect("exact fit");
    assert!(region.within(109, 70).is_err());
    assert!(region.within(110, 69).is_err());
    for bad in [
      Region {
        width: 101,
        ..region
      },
      Region {
        height: 51,
        ..region
      },
      Region {
        width: 14,
        ..region
      },
      // Even and above the former 16 px floor, but below what the software encoder accepts.
      Region {
        width: 32,
        ..region
      },
      Region {
        height: 32,
        ..region
      },
      Region {
        height: 0,
        ..region
      },
    ] {
      assert_eq!(bad.validate().expect_err("invalid size").stage, "region");
    }
    let overflow = Region {
      x: u32::MAX - 1,
      ..region
    };
    assert!(overflow.within(u32::MAX, u32::MAX).is_err());
    Region {
      width: Region::MIN_SIZE,
      height: Region::MIN_SIZE,
      ..region
    }
    .validate()
    .expect("smallest accepted area");
    let mut spec = SessionSpec {
      monitor: "primary".into(),
      fps: 30,
      capture: CaptureKind::Wgc,
      encoder: EncoderKind::Amf,
      quality: Quality::default(),
      cursor: true,
      clicks: false,
      audio: false,
      audio_device: None,
      microphone: false,
      microphone_device: None,
      output: "area.mp4".into(),
      trace_frames: false,
      region: Some(Region {
        width: 15,
        ..region
      }),
      span: Vec::new(),
    };
    assert_eq!(spec.validate().expect_err("odd region").stage, "region");
    spec.region = Some(region);
    spec.validate().expect("valid region");
    let text = serde_json::to_string(&spec).expect("JSON");
    assert_eq!(
      serde_json::from_str::<SessionSpec>(&text).expect("round trip"),
      spec
    );
    // Older lab specs without a region still deserialize as whole-display recordings.
    let legacy = text.replace(
      ",\"region\":{\"x\":10,\"y\":20,\"width\":100,\"height\":50}",
      "",
    );
    assert_eq!(
      serde_json::from_str::<SessionSpec>(&legacy)
        .expect("legacy spec")
        .region,
      None
    );
  }
}
