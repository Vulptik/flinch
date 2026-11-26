//! Offline size/quality measurement for choosing recording QPs (`flinch-video-lab reencode`).
//!
//! A reference recording is decoded frame by frame and sent through the recording path without
//! capture: the BGRA desktop cache, the same GPU (or CPU) conversion, an explicit encoder at an
//! explicit quality, and the MP4 writer. The new file is then decoded beside the reference and
//! scored on luma (PSNR and 8×8-block SSIM). Every run of one reference sees identical pictures,
//! so sizes and scores are comparable across QPs and encoders, which live recordings are not.
//!
//! Scores include one decode/convert round trip (NV12 to RGB and back) that every run shares;
//! they rank settings against each other, not against the screen. No desktop, audio, window or
//! input is touched.
use super::{encoding::EncoderService, gpu, mux, verify};
use crate::{EngineError, ErrorKind, Result, recovery, time, types::*, watchdog::Watchdog};
use serde::Serialize;
use std::{
  collections::VecDeque,
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};
use windows::Win32::{
  Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4},
  Media::MediaFoundation::IMFSourceReader,
};

/// Hard ceiling on frames per reference (one hour at 120 FPS).
const FRAME_LIMIT: u64 = 432_000;
/// Reads that return neither a picture nor the end before a reference is called malformed.
const EMPTY_READ_LIMIT: u32 = 1_000;
/// Side of the square blocks SSIM is averaged over.
const BLOCK: usize = 8;

/// Where the measured encoder runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterChoice {
  /// The adapter of the first display whose vendor owns the encoder (any display for software).
  Display,
  /// The software rasterizer, as on GPU-less machines and VMs: CPU conversion, software encoder.
  Warp,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Similarity {
  pub mean_psnr_y_db: f64,
  pub min_psnr_y_db: f64,
  pub mean_ssim_y: f64,
  /// Fifth-percentile frame: brief smearing shows here before it moves the mean.
  pub p5_ssim_y: f64,
  pub min_ssim_y: f64,
  /// Reference time of the lowest-SSIM frame, to find it by eye.
  pub min_ssim_at_seconds: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReencodeReport {
  pub input: PathBuf,
  pub output: PathBuf,
  pub adapter: String,
  pub encoder: EncoderKind,
  pub quality: Quality,
  pub conversion: ConversionKind,
  pub width: u32,
  pub height: u32,
  pub fps: u32,
  pub frames: u64,
  pub duration_seconds: f64,
  pub reference_bytes: u64,
  /// Finished file, including the MP4 index.
  pub bytes: u64,
  pub average_mbps: f64,
  pub megabytes_per_minute: f64,
  pub keyframes: u64,
  /// Share of the video bytes spent on keyframes (forced every `KEYFRAME_SECONDS`, as in recordings).
  pub keyframe_bytes: u64,
  pub video_bytes: u64,
  /// Decode, conversion and encode together; a lower bound on the encoder's own rate.
  pub pipeline_fps: f64,
  pub similarity: Similarity,
}

/// Re-encode `input` (an engine MP4) to the new file `output` and score it against `input`.
pub fn reencode(
  input: &Path,
  output: &Path,
  encoder: EncoderKind,
  quality: Quality,
  adapter: AdapterChoice,
) -> Result<ReencodeReport> {
  let _com = super::Apartment::new()?;
  let _mf = super::MediaRuntime::new()?;
  if !matches!(
    encoder,
    EncoderKind::Amf | EncoderKind::Nvenc | EncoderKind::Qsv | EncoderKind::Software
  ) {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "reencode encoder",
      "Re-encoding needs an explicit amf, nvenc, qsv or software encoder",
    ));
  }
  if adapter == AdapterChoice::Warp && encoder != EncoderKind::Software {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "reencode adapter",
      "WARP has no hardware encoder; use the software encoder",
    ));
  }
  probe(&quality)?;
  if output.exists() {
    return Err(EngineError::new(
      ErrorKind::Io,
      "reencode output",
      "Destination already exists",
    ));
  }
  let structure = recovery::inspect(input)?;
  if structure.truncated {
    return Err(EngineError::new(
      ErrorKind::Io,
      "reencode input",
      "Reference has an incomplete fragment",
    ));
  }
  // Each pass may run well below real time in debug builds or on WARP.
  let budget = Duration::from_secs((structure.duration_100ns / time::SECOND * 6 + 120).min(21_660));
  let (adapter, name) = match adapter {
    AdapterChoice::Display => display_adapter(encoder)?,
    AdapterChoice::Warp => warp_adapter()?,
  };
  let partial = partial_path(output)?;
  let encoded = encode(input, &partial, &adapter, encoder, quality, budget);
  let encoded = match encoded.and_then(|encoded| {
    crate::artifact::move_no_replace(&partial, output, "reencode publish").map(|()| encoded)
  }) {
    Ok(encoded) => encoded,
    Err(mut error) => {
      // Only the partial this call named is removed; a failed removal travels with the cause.
      if partial.exists()
        && let Err(cleanup) = std::fs::remove_file(&partial)
      {
        error.reason.push_str(&format!(
          "; incomplete file {} was not removed: {cleanup}",
          partial.display()
        ));
      }
      return Err(error);
    },
  };
  let similarity = compare(input, output, Instant::now() + budget)?;
  let bytes = std::fs::metadata(output)
    .map_err(|e| EngineError::io("reencode size", e))?
    .len();
  let seconds = encoded.duration_100ns as f64 / time::SECOND as f64;
  Ok(ReencodeReport {
    input: input.to_path_buf(),
    output: output.to_path_buf(),
    adapter: name,
    encoder,
    quality,
    conversion: encoded.conversion,
    width: encoded.width,
    height: encoded.height,
    fps: encoded.fps,
    frames: encoded.frames,
    duration_seconds: seconds,
    reference_bytes: structure.bytes,
    bytes,
    average_mbps: bytes as f64 * 8.0 / seconds.max(f64::EPSILON) / 1_000_000.0,
    megabytes_per_minute: bytes as f64 / 1_000_000.0 / (seconds.max(f64::EPSILON) / 60.0),
    keyframes: encoded.keyframes,
    keyframe_bytes: encoded.keyframe_bytes,
    video_bytes: encoded.video_bytes,
    pipeline_fps: encoded.frames as f64 / encoded.elapsed.as_secs_f64().max(f64::EPSILON),
    similarity,
  })
}

/// Validates the quality the way a session would, before any file or device is opened.
fn probe(quality: &Quality) -> Result<()> {
  let spec = SessionSpec {
    monitor: "reencode".into(),
    fps: 30,
    capture: CaptureKind::Synthetic,
    encoder: EncoderKind::Software,
    quality: *quality,
    cursor: false,
    clicks: false,
    audio: false,
    audio_device: None,
    microphone: false,
    microphone_device: None,
    output: "reencode.mp4".into(),
    trace_frames: false,
    region: None,
    span: Vec::new(),
  };
  spec.validate()
}

fn display_adapter(encoder: EncoderKind) -> Result<(IDXGIAdapter1, String)> {
  let vendor = match encoder {
    EncoderKind::Amf => Some(0x1002),
    EncoderKind::Nvenc => Some(0x10de),
    EncoderKind::Qsv => Some(0x8086),
    _ => None,
  };
  gpu::outputs()?
    .into_iter()
    .find(|output| vendor.is_none_or(|id| output.info.vendor_id == id))
    .map(|output| (output.adapter, output.info.adapter))
    .ok_or_else(|| {
      EngineError::new(
        ErrorKind::Capability,
        "reencode adapter",
        format!("No display is attached to an adapter for {encoder:?}"),
      )
    })
}

fn warp_adapter() -> Result<(IDXGIAdapter1, String)> {
  // SAFETY: read-only factory for the software adapter; no output, capture or display.
  let factory: IDXGIFactory4 =
    unsafe { CreateDXGIFactory1() }.map_err(|e| EngineError::native("DXGI factory", e))?;
  // SAFETY: as above.
  let adapter: IDXGIAdapter1 =
    unsafe { factory.EnumWarpAdapter() }.map_err(|e| EngineError::native("WARP adapter", e))?;
  Ok((adapter, "WARP".into()))
}

fn adapter_luid(adapter: &IDXGIAdapter1) -> Result<i64> {
  // SAFETY: live adapter with a writable native descriptor.
  let desc =
    unsafe { adapter.GetDesc1() }.map_err(|e| EngineError::native("adapter description", e))?;
  Ok((i64::from(desc.AdapterLuid.HighPart) << 32) | i64::from(desc.AdapterLuid.LowPart))
}

fn partial_path(output: &Path) -> Result<PathBuf> {
  let name = output
    .file_name()
    .ok_or_else(|| EngineError::protocol("Re-encode output has no file name"))?
    .to_string_lossy();
  let nonce = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_err(|e| EngineError::protocol(e.to_string()))?
    .as_nanos();
  // The writer requires an .mp4 extension.
  Ok(output.with_file_name(format!(
    ".{name}.{}-{nonce}.partial.mp4",
    std::process::id()
  )))
}

/// One decoded picture in display order.
struct Picture {
  pts: i64,
  duration: i64,
  bytes: Vec<u8>,
}

/// Pull-based reader over a recording's decoded NV12 pictures.
struct Pictures {
  reader: IMFSourceReader,
  stream: u32,
  geometry: verify::Geometry,
  previous: Option<i64>,
  finished: bool,
  read: u64,
}
impl Pictures {
  fn open(path: &Path) -> Result<Self> {
    let (reader, stream) = verify::video_reader(path)?;
    let geometry = verify::geometry(&reader, stream, false)?;
    Ok(Self {
      reader,
      stream,
      geometry,
      previous: None,
      finished: false,
      read: 0,
    })
  }
  fn size(&self) -> (u32, u32) {
    self.geometry.display
  }
  fn luma<'a>(&self, picture: &'a Picture) -> Result<Plane<'a>> {
    let (width, height) = self.size();
    Plane::new(
      &picture.bytes,
      self.geometry.stride,
      width as usize,
      height as usize,
    )
  }
  fn rgb(&self, picture: &Picture, rgb: &mut Vec<u8>) -> Result<()> {
    let (width, height) = self.size();
    crate::gif::nv12_to_rgb(
      &picture.bytes,
      self.geometry.stride,
      self.geometry.height,
      width as usize,
      height as usize,
      rgb,
    )
  }
  fn next(&mut self) -> Result<Option<Picture>> {
    if self.finished {
      return Ok(None);
    }
    for _ in 0..EMPTY_READ_LIMIT {
      let (finished, changed, sample) = verify::next(&self.reader, self.stream, true)?;
      if changed {
        let display = self.geometry.display;
        self.geometry = verify::geometry(&self.reader, self.stream, false)?;
        if self.geometry.display != display {
          return Err(EngineError::protocol(
            "Decoded picture size changed mid-stream",
          ));
        }
      }
      self.finished = finished;
      if let Some(sample) = sample {
        // SAFETY: decoded samples on the selected stream carry time and duration.
        let pts = unsafe { sample.GetSampleTime() }
          .map_err(|e| EngineError::native("decoded video time", e))?;
        // SAFETY: as above; a missing duration is a malformed recording.
        let duration = unsafe { sample.GetSampleDuration() }
          .map_err(|e| EngineError::native("decoded video duration", e))?;
        if pts < 0 || duration <= 0 || self.previous.is_some_and(|p| pts <= p) {
          return Err(EngineError::protocol(
            "Decoded video timestamps are not increasing",
          ));
        }
        self.read += 1;
        if self.read > FRAME_LIMIT {
          return Err(EngineError::new(
            ErrorKind::Capability,
            "reencode decode",
            "Reference exceeds the re-encode frame limit",
          ));
        }
        self.previous = Some(pts);
        return Ok(Some(Picture {
          pts,
          duration,
          bytes: mux::sample_bytes(&sample)?,
        }));
      }
      if finished {
        return Ok(None);
      }
    }
    Err(EngineError::protocol(
      "Decoder returned no picture and no end of stream",
    ))
  }
}

/// The supported cadence a frame duration (100 ns) belongs to, within frame-time rounding.
fn cadence(duration: i64) -> Result<u32> {
  SUPPORTED_FPS
    .iter()
    .copied()
    .find(|fps| (time::SECOND as i64 / i64::from(*fps) - duration).abs() <= 1)
    .ok_or_else(|| {
      EngineError::new(
        ErrorKind::Capability,
        "reencode cadence",
        format!(
          "The first frame lasts {duration} (100 ns), not one frame at {SUPPORTED_FPS:?} FPS"
        ),
      )
    })
}

/// Forced keyframes on the recorder's two-second slot boundaries: the slot index of a picture
/// (from its time since the first) starts a new group, as `runtime` forces them.
fn keyframe(at_100ns: u64, fps: u32, group: &mut Option<u64>) -> Result<bool> {
  // Nearest slot: frame times are rounded to 100 ns, so neither floor nor ceiling is safe.
  let slot = u64::try_from(
    (u128::from(at_100ns) * u128::from(fps) + u128::from(time::SECOND / 2))
      / u128::from(time::SECOND),
  )
  .map_err(|_| EngineError::protocol("Frame index overflow"))?;
  let current = slot / u64::from(keyframe_interval(fps));
  let key = *group != Some(current);
  *group = Some(current);
  Ok(key)
}

fn bgra(rgb: &[u8], out: &mut Vec<u8>) {
  out.clear();
  out.reserve(rgb.len() / 3 * 4);
  for pixel in rgb.as_chunks::<3>().0 {
    out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
  }
}

struct Encoded {
  conversion: ConversionKind,
  width: u32,
  height: u32,
  fps: u32,
  frames: u64,
  duration_100ns: u64,
  keyframes: u64,
  keyframe_bytes: u64,
  video_bytes: u64,
  elapsed: Duration,
}

/// Feeds the MP4 writer as packets arrive; a saturated writer is retried after it drains.
struct Sink {
  writer: mux::Writer,
  queue: VecDeque<EncodedPacket>,
  keyframes: u64,
  keyframe_bytes: u64,
  video_bytes: u64,
}
impl Sink {
  fn feed(&mut self, service: &mut EncoderService) -> Result<()> {
    while let Some(packet) = service.poll()? {
      let bytes = packet.data.len() as u64;
      self.video_bytes += bytes;
      if packet.keyframe {
        self.keyframes += 1;
        self.keyframe_bytes += bytes;
      }
      self.queue.push_back(packet);
    }
    while let Some(packet) = self.queue.front() {
      match self.writer.push(packet.clone()) {
        Ok(()) => {
          self.queue.pop_front();
        },
        Err(error) if error.kind == ErrorKind::Saturation => break,
        Err(error) => return Err(error),
      }
    }
    Ok(())
  }
}

fn expired(deadline: Instant, stage: &str) -> Result<()> {
  if Instant::now() > deadline {
    return Err(EngineError::new(
      ErrorKind::Timeout,
      "reencode",
      format!("Re-encoding exceeded its duration budget while {stage}"),
    ));
  }
  Ok(())
}

/// Moves finished packets to the writer. Named, not an inline closure, and also used for the
/// per-frame feed, so it runs on every pass whether or not a retry happened.
fn feed_writer((service, sink): &mut (&mut EncoderService, &mut Sink)) -> Result<()> {
  sink.feed(service)
}

/// Retries `ready` until it succeeds, feeding the writer between attempts so the encoder can free
/// a surface or queue slot. Bounded by the pass deadline; `stage` names what was being waited on.
fn retry_until<S>(
  state: &mut S,
  deadline: Instant,
  stage: &str,
  mut ready: impl FnMut(&mut S) -> Result<bool>,
  mut feed: impl FnMut(&mut S) -> Result<()>,
) -> Result<()> {
  while !ready(state)? {
    expired(deadline, stage)?;
    feed(state)?;
    std::thread::sleep(Duration::from_micros(200));
  }
  Ok(())
}

fn encode(
  input: &Path,
  output: &Path,
  adapter: &IDXGIAdapter1,
  encoder: EncoderKind,
  quality: Quality,
  budget: Duration,
) -> Result<Encoded> {
  let started = Instant::now();
  let deadline = started + budget;
  let luid = adapter_luid(adapter)?;
  let mut pictures = Pictures::open(input)?;
  let first = pictures
    .next()?
    .ok_or_else(|| EngineError::protocol("Reference has no video frames"))?;
  let fps = cadence(first.duration)?;
  let (width, height) = pictures.size();
  let gpu = gpu::Gpu::new(adapter, width, height, None, fps)?;
  let spec = SessionSpec {
    monitor: "reencode".into(),
    fps,
    capture: CaptureKind::Synthetic,
    encoder,
    quality,
    cursor: false,
    clicks: false,
    audio: false,
    audio_device: None,
    microphone: false,
    microphone_device: None,
    output: output.to_path_buf(),
    trace_frames: false,
    region: None,
    span: Vec::new(),
  };
  spec.validate()?;
  let mut service = EncoderService::new_with_watchdog(
    &gpu.device,
    luid,
    (gpu.width, gpu.height),
    &spec,
    gpu.conversion(),
    Arc::new(Watchdog::default()),
  )?;
  let writer = mux::Writer::start(
    output,
    gpu.width,
    gpu.height,
    fps,
    service.extra().to_vec(),
    None,
  )?;
  let mut sink = Sink {
    writer,
    queue: VecDeque::new(),
    keyframes: 0,
    keyframe_bytes: 0,
    video_bytes: 0,
  };
  let origin = first.pts;
  let (mut rgb, mut image) = (Vec::new(), Vec::new());
  let mut group = None;
  let mut frames = 0u64;
  let mut end = 0u64;
  let mut next = Some(first);
  while let Some(picture) = next {
    let slot = (frames % gpu.surfaces.len() as u64) as u32;
    retry_until(
      &mut (&mut service, &mut sink),
      deadline,
      "waiting for a surface",
      |(service, _)| Ok(!service.busy(slot)),
      feed_writer,
    )?;
    pictures.rgb(&picture, &mut rgb)?;
    bgra(&rgb, &mut image);
    // SAFETY: the image spans the whole BGRA desktop cache at its packed pitch.
    unsafe {
      gpu.context.UpdateSubresource(
        &gpu.desktop,
        0,
        None,
        image.as_ptr().cast(),
        gpu.source_width * 4,
        0,
      );
    }
    gpu.convert(&gpu.desktop, slot as usize)?;
    let at = u64::try_from(picture.pts - origin)
      .map_err(|_| EngineError::protocol("Reference time precedes its first frame"))?;
    let duration = picture.duration as u64;
    let key = keyframe(at, fps, &mut group)?;
    retry_until(
      &mut (&mut service, &mut sink),
      deadline,
      "submitting",
      |(service, _)| service.submit(&gpu.surfaces[slot as usize], slot, at, duration, key),
      feed_writer,
    )?;
    feed_writer(&mut (&mut service, &mut sink))?;
    frames += 1;
    end = at + duration;
    next = pictures.next()?;
  }
  service.drain()?;
  while !service.drained() || !sink.queue.is_empty() {
    expired(deadline, "draining")?;
    sink.feed(&mut service)?;
    std::thread::sleep(Duration::from_millis(1));
  }
  let elapsed = started.elapsed();
  let conversion = gpu.conversion();
  let (width, height) = (gpu.width, gpu.height);
  sink.writer.finish_by(deadline)?;
  Ok(Encoded {
    conversion,
    width,
    height,
    fps,
    frames,
    duration_100ns: end,
    keyframes: sink.keyframes,
    keyframe_bytes: sink.keyframe_bytes,
    video_bytes: sink.video_bytes,
    elapsed,
  })
}

/// Decode reference and re-encode side by side and score every frame pair.
fn compare(reference: &Path, encoded: &Path, deadline: Instant) -> Result<Similarity> {
  let mut left = Pictures::open(reference)?;
  let mut right = Pictures::open(encoded)?;
  if left.size() != right.size() {
    return Err(EngineError::protocol(format!(
      "Re-encode is {:?}, reference {:?}",
      right.size(),
      left.size()
    )));
  }
  let mut scores = Scores::default();
  let mut origin = None;
  loop {
    expired(deadline, "comparing")?;
    match (left.next()?, right.next()?) {
      (None, None) => break,
      (Some(a), Some(b)) => {
        let start = *origin.get_or_insert(a.pts);
        if a.pts - start != b.pts {
          return Err(EngineError::protocol(format!(
            "Frame {} is at {} in the re-encode, {} in the reference",
            scores.ssim.len(),
            b.pts,
            a.pts - start
          )));
        }
        let (a_plane, b_plane) = (left.luma(&a)?, right.luma(&b)?);
        scores.add(
          (a.pts - start) as f64 / time::SECOND as f64,
          psnr(&a_plane, &b_plane)?,
          ssim(&a_plane, &b_plane)?,
        );
      },
      _ => {
        return Err(EngineError::protocol(
          "Re-encode and reference have different frame counts",
        ));
      },
    }
  }
  scores.finish()
}

#[derive(Default)]
struct Scores {
  psnr_total: f64,
  psnr_min: f64,
  ssim: Vec<f64>,
  ssim_min: (f64, f64),
}
impl Scores {
  fn add(&mut self, at_seconds: f64, psnr: f64, ssim: f64) {
    if self.ssim.is_empty() {
      self.psnr_min = psnr;
      self.ssim_min = (ssim, at_seconds);
    }
    self.psnr_total += psnr;
    self.psnr_min = self.psnr_min.min(psnr);
    if ssim < self.ssim_min.0 {
      self.ssim_min = (ssim, at_seconds);
    }
    self.ssim.push(ssim);
  }
  fn finish(mut self) -> Result<Similarity> {
    if self.ssim.is_empty() {
      return Err(EngineError::protocol("Reference has no video frames"));
    }
    let count = self.ssim.len() as f64;
    let mean_ssim = self.ssim.iter().sum::<f64>() / count;
    self.ssim.sort_by(f64::total_cmp);
    let p5 = self.ssim[(self.ssim.len() - 1) * 5 / 100];
    Ok(Similarity {
      mean_psnr_y_db: self.psnr_total / count,
      min_psnr_y_db: self.psnr_min,
      mean_ssim_y: mean_ssim,
      p5_ssim_y: p5,
      min_ssim_y: self.ssim_min.0,
      min_ssim_at_seconds: self.ssim_min.1,
    })
  }
}

/// A checked view of an 8-bit luma plane.
struct Plane<'a> {
  data: &'a [u8],
  stride: usize,
  width: usize,
  height: usize,
}
impl<'a> Plane<'a> {
  fn new(data: &'a [u8], stride: usize, width: usize, height: usize) -> Result<Self> {
    if width == 0 || height == 0 || width > stride || data.len() < stride * (height - 1) + width {
      return Err(EngineError::protocol(
        "Luma plane is smaller than its extent",
      ));
    }
    Ok(Self {
      data,
      stride,
      width,
      height,
    })
  }
  fn row(&self, y: usize) -> &[u8] {
    &self.data[y * self.stride..y * self.stride + self.width]
  }
}

fn same_extent(a: &Plane<'_>, b: &Plane<'_>) -> Result<()> {
  if (a.width, a.height) != (b.width, b.height) {
    return Err(EngineError::protocol("Compared pictures differ in size"));
  }
  Ok(())
}

/// Luma PSNR in dB; identical pictures score 100, as the GIF fidelity check does.
fn psnr(a: &Plane<'_>, b: &Plane<'_>) -> Result<f64> {
  same_extent(a, b)?;
  let mut squared = 0u64;
  for y in 0..a.height {
    for (left, right) in a.row(y).iter().zip(b.row(y)) {
      let d = u64::from(left.abs_diff(*right));
      squared += d * d;
    }
  }
  let mse = squared as f64 / (a.width * a.height) as f64;
  Ok(if mse == 0.0 {
    100.0
  } else {
    10.0 * (255.0f64 * 255.0 / mse).log10()
  })
}

/// Mean SSIM over non-overlapping 8×8 luma blocks (edge remainders are left out).
fn ssim(a: &Plane<'_>, b: &Plane<'_>) -> Result<f64> {
  same_extent(a, b)?;
  let (columns, rows) = (a.width / BLOCK, a.height / BLOCK);
  if columns == 0 || rows == 0 {
    return Err(EngineError::protocol(
      "Picture is smaller than one SSIM block",
    ));
  }
  const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
  const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
  let n = (BLOCK * BLOCK) as f64;
  let mut total = 0.0;
  for row in 0..rows {
    for column in 0..columns {
      let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0u64, 0u64, 0u64, 0u64, 0u64);
      for y in row * BLOCK..(row + 1) * BLOCK {
        let x = column * BLOCK;
        for (left, right) in a.row(y)[x..x + BLOCK].iter().zip(&b.row(y)[x..x + BLOCK]) {
          let (l, r) = (u64::from(*left), u64::from(*right));
          sa += l;
          sb += r;
          saa += l * l;
          sbb += r * r;
          sab += l * r;
        }
      }
      let (ma, mb) = (sa as f64 / n, sb as f64 / n);
      let va = saa as f64 / n - ma * ma;
      let vb = sbb as f64 / n - mb * mb;
      let cov = sab as f64 / n - ma * mb;
      total +=
        ((2.0 * ma * mb + C1) * (2.0 * cov + C2)) / ((ma * ma + mb * mb + C1) * (va + vb + C2));
    }
  }
  Ok(total / (columns * rows) as f64)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
#[path = "reencode/tests.rs"]
mod tests;
