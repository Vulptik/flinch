//! Offscreen acceptance for the vendor encoders, run on NVIDIA and
//! Intel machines with `cargo xtask test:video:vendor`. It drives the recording pipeline without
//! capture: numbered BGRA frames enter the desktop cache, the GPU video processor converts them,
//! the vendor encoder (after its self-test) encodes them, and the independent decoder checks every
//! frame number. No desktop, audio, window or input is touched.
//!
//! The same harness finds the smallest areas every encoder on a machine can record
//! (`cargo xtask test:video:smallest`), which bound `Region::MIN_SIZE`.
use super::{encoding::EncoderService, gpu, mux, verify};
use crate::{EngineError, Result, time, types::*, watchdog::Watchdog};
use serde::Serialize;
use std::{
  collections::{HashSet, VecDeque},
  sync::Arc,
  time::{Duration, Instant},
};
use windows::Win32::Graphics::{
  Direct3D11::ID3D11Texture2D,
  Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4},
};

#[derive(Clone, Copy, Debug, Serialize)]
struct Case {
  width: u32,
  height: u32,
  fps: u32,
  quality: Quality,
}
/// Sizes include a non-16-aligned picture (1366×768) and 4K; rates cover the app's 30/60/120.
const CASES: [Case; 6] = [
  Case {
    width: 1920,
    height: 1080,
    fps: 60,
    quality: Quality::Cqp { i: 20, p: 23 },
  },
  Case {
    width: 1920,
    height: 1080,
    fps: 60,
    quality: Quality::Vbr {
      bitrate: 24_000_000,
    },
  },
  Case {
    width: 1366,
    height: 768,
    fps: 30,
    quality: Quality::Cqp { i: 20, p: 23 },
  },
  Case {
    width: 2560,
    height: 1440,
    fps: 120,
    quality: Quality::Cqp { i: 22, p: 25 },
  },
  Case {
    width: 3840,
    height: 2160,
    fps: 60,
    quality: Quality::Cqp { i: 22, p: 25 },
  },
  Case {
    width: 640,
    height: 360,
    fps: 30,
    quality: Quality::Vbr { bitrate: 4_000_000 },
  },
];
const SECONDS: u32 = 3;
/// Even sizes the smallest-area probe scans, from the overlay's 10 px minimum upward. Encoders may
/// limit each dimension separately, so widths and heights are also scanned as strips.
const SCAN: std::ops::RangeInclusive<u32> = 10..=256;
const STRIP: u32 = 256;

/// An adapter the harness encodes on: a display's adapter, or WARP for the CPU conversion tier.
struct Source {
  adapter: IDXGIAdapter1,
  name: String,
  luid: i64,
}
impl Source {
  fn of(output: &gpu::Output) -> Self {
    Self {
      adapter: output.adapter.clone(),
      name: output.info.adapter.clone(),
      luid: output.info.adapter_luid,
    }
  }
  /// The software adapter of GPU-less machines and VMs: no video processor, so CPU conversion.
  fn warp() -> Result<Self> {
    // SAFETY: read-only factory for the software adapter; no output, capture or display.
    let factory: IDXGIFactory4 =
      unsafe { CreateDXGIFactory1() }.map_err(|e| EngineError::native("DXGI factory", e))?;
    // SAFETY: as above.
    let adapter: IDXGIAdapter1 =
      unsafe { factory.EnumWarpAdapter() }.map_err(|e| EngineError::native("WARP adapter", e))?;
    // SAFETY: live adapter with a writable native descriptor.
    let desc =
      unsafe { adapter.GetDesc1() }.map_err(|e| EngineError::native("adapter description", e))?;
    Ok(Self {
      adapter,
      name: "WARP".into(),
      luid: (i64::from(desc.AdapterLuid.HighPart) << 32) | i64::from(desc.AdapterLuid.LowPart),
    })
  }
}

#[derive(Debug, Serialize)]
struct Outcome {
  adapter: String,
  encoder: EncoderKind,
  case: Case,
  /// The tier the adapter gave: GPU video processor, or CPU conversion (WARP, basic display).
  conversion: Option<ConversionKind>,
  passed: bool,
  error: Option<String>,
  /// Encoder service creation, including the self-test.
  open_ms: f64,
  /// Wall-clock encode rate; must exceed the case's frame rate to keep up.
  frames_per_second: f64,
  bytes: u64,
  decoded_frames: u64,
  numbered_frames: u64,
  maximum_gap_ms: f64,
  /// Decoded display extent; must equal the case's size exactly.
  video_width: u32,
  video_height: u32,
}

/// Whether a case is large enough for the verifier's 512 px-wide code row.
fn marked(case: Case) -> bool {
  case.width >= 512 && case.height >= 16
}

/// A flat picture whose level changes every frame, for areas too small for the code row.
fn plain(width: u32, height: u32, index: u32) -> Vec<u8> {
  let level = 32 + (index * 7 % 192) as u8;
  [level, 255 - level, level / 2, 255].repeat(width as usize * height as usize)
}

/// Numbered code blocks (white/black 16×16 per bit) over grey, as the verifier reads them.
fn numbered(width: u32, height: u32, index: u32) -> Vec<u8> {
  let (width, height) = (width as usize, height as usize);
  let mut image = vec![96u8; width * height * 4];
  for bit in 0..32usize {
    let value = if index & (1 << bit) != 0 { 255 } else { 0 };
    for y in 0..16 {
      let start = (y * width + bit * 16) * 4;
      image[start..start + 64].fill(value);
    }
  }
  // A moving bar so rate control has real work beyond the code rows.
  let bar = (index as usize * 8) % width.saturating_sub(32).max(1);
  for y in 32..height {
    let start = (y * width + bar) * 4;
    image[start..start + 128].fill(200);
  }
  image
}

/// Feeds the MP4 writer as packets arrive, like a recording: the writer bounds its backlog to
/// two seconds of media, so a saturated push is retried after it drains.
struct Sink {
  path: std::path::PathBuf,
  size: (u32, u32, u32),
  writer: Option<mux::Writer>,
  queue: VecDeque<EncodedPacket>,
  bytes: u64,
}
impl Sink {
  fn feed(&mut self, service: &mut EncoderService) -> Result<()> {
    while let Some(packet) = service.poll()? {
      self.bytes += packet.data.len() as u64;
      self.queue.push_back(packet);
    }
    if self.writer.is_none() && !self.queue.is_empty() {
      let (width, height, fps) = self.size;
      self.writer = Some(mux::Writer::start(
        &self.path,
        width,
        height,
        fps,
        service.extra().to_vec(),
        None,
      )?);
    }
    if let Some(writer) = &self.writer {
      while let Some(packet) = self.queue.front() {
        match writer.push(packet.clone()) {
          Ok(()) => {
            self.queue.pop_front();
          },
          Err(error) if error.kind == crate::ErrorKind::Saturation => break,
          Err(error) => return Err(error),
        }
      }
    }
    Ok(())
  }
}

fn upload(gpu: &gpu::Gpu, texture: &ID3D11Texture2D, image: &[u8]) {
  // SAFETY: the image spans the whole BGRA cache at its packed pitch.
  unsafe {
    gpu
      .context
      .UpdateSubresource(texture, 0, None, image.as_ptr().cast(), gpu.width * 4, 0);
  }
}

fn run(source: &Source, encoder: EncoderKind, case: Case, folder: &std::path::Path) -> Outcome {
  let marked = marked(case);
  let mut outcome = Outcome {
    adapter: source.name.clone(),
    encoder,
    case,
    conversion: None,
    passed: false,
    error: None,
    open_ms: 0.0,
    frames_per_second: 0.0,
    bytes: 0,
    decoded_frames: 0,
    numbered_frames: 0,
    maximum_gap_ms: 0.0,
    video_width: 0,
    video_height: 0,
  };
  let result = (|| -> Result<()> {
    let gpu = gpu::Gpu::new(&source.adapter, case.width, case.height, None, case.fps)?;
    outcome.conversion = Some(gpu.conversion());
    let spec = SessionSpec {
      monitor: "offscreen".into(),
      fps: case.fps,
      capture: CaptureKind::Synthetic,
      encoder,
      quality: case.quality,
      cursor: false,
      clicks: false,
      audio: false,
      audio_device: None,
      microphone: false,
      microphone_device: None,
      output: "vendor.mp4".into(),
      trace_frames: false,
      region: None,
      span: Vec::new(),
    };
    let opened = Instant::now();
    let mut service = EncoderService::new_with_watchdog(
      &gpu.device,
      source.luid,
      (gpu.width, gpu.height),
      &spec,
      gpu.conversion(),
      Arc::new(Watchdog::default()),
    )?;
    outcome.open_ms = opened.elapsed().as_secs_f64() * 1000.0;
    let frames = case.fps * SECONDS;
    let duration = time::frame_time(1, case.fps)?;
    let deadline = Duration::from_secs(u64::from(SECONDS) * 10 + 10);
    // The adapter keeps software runs on several adapters (WARP included) apart.
    let path = folder.join(format!(
      "{:x}-{encoder:?}-{}x{}-{}fps-{}.mp4",
      source.luid,
      case.width,
      case.height,
      case.fps,
      match case.quality {
        Quality::Cqp { .. } => "cqp",
        Quality::Vbr { .. } => "vbr",
      }
    ));
    let mut sink = Sink {
      path,
      size: (gpu.width, gpu.height, case.fps),
      writer: None,
      queue: VecDeque::new(),
      bytes: 0,
    };
    let started = Instant::now();
    let pump = |service: &mut EncoderService, sink: &mut Sink| -> Result<()> {
      sink.feed(service)?;
      if started.elapsed() > deadline {
        return Err(EngineError::new(
          crate::ErrorKind::Timeout,
          "vendor acceptance",
          "Encoding did not keep within its bound",
        ));
      }
      Ok(())
    };
    for index in 0..frames {
      let slot = index % gpu.surfaces.len() as u32;
      while service.busy(slot) {
        pump(&mut service, &mut sink)?;
        std::thread::sleep(Duration::from_micros(200));
      }
      let picture = if marked {
        numbered(case.width, case.height, index)
      } else {
        plain(case.width, case.height, index)
      };
      upload(&gpu, &gpu.desktop, &picture);
      gpu.convert(&gpu.desktop, slot as usize)?;
      let pts = time::frame_time(u64::from(index), case.fps)?;
      while !service.submit(
        &gpu.surfaces[slot as usize],
        slot,
        pts,
        duration,
        index % crate::types::keyframe_interval(case.fps) == 0,
      )? {
        pump(&mut service, &mut sink)?;
        std::thread::sleep(Duration::from_micros(200));
      }
      pump(&mut service, &mut sink)?;
    }
    service.drain()?;
    while !service.drained() || !sink.queue.is_empty() {
      pump(&mut service, &mut sink)?;
      std::thread::sleep(Duration::from_millis(1));
    }
    outcome.frames_per_second = f64::from(frames) / started.elapsed().as_secs_f64();
    outcome.bytes = sink.bytes;
    let path = sink.path.clone();
    sink
      .writer
      .ok_or_else(|| EngineError::protocol("No packet reached the writer"))?
      .finish()?;
    let report = verify::verify(&path, marked)?;
    outcome.decoded_frames = report.video_frames;
    outcome.numbered_frames = report.numbered_frames_checked;
    outcome.maximum_gap_ms = report.maximum_video_timestamp_gap_ms;
    outcome.video_width = report.video_width;
    outcome.video_height = report.video_height;
    let expected_numbered = if marked { u64::from(frames) } else { 0 };
    if report.video_frames != u64::from(frames)
      || report.numbered_frames_checked != expected_numbered
    {
      return Err(EngineError::protocol(format!(
        "Decoded {} frames ({} numbered) of {frames}",
        report.video_frames, report.numbered_frames_checked
      )));
    }
    // Encoders pad tiny pictures to whole macroblocks; the file must still crop back exactly.
    if (report.video_width, report.video_height) != (case.width, case.height) {
      return Err(EngineError::protocol(format!(
        "Decoded picture is {}x{}, not {}x{}",
        report.video_width, report.video_height, case.width, case.height
      )));
    }
    Ok(())
  })();
  match result {
    Ok(()) => outcome.passed = true,
    Err(error) => outcome.error = Some(error.to_string()),
  }
  outcome
}

/// A new report folder. The vendor kit sets FLINCH_VENDOR_REPORT_DIR; in the repository, reports
/// go under target/.
#[allow(clippy::expect_used)]
fn report_folder(name: &str) -> std::path::PathBuf {
  let nonce = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("clock")
    .as_secs();
  let folder = std::env::var_os("FLINCH_VENDOR_REPORT_DIR")
    .map_or_else(
      || std::path::PathBuf::from("../../target").join(name),
      std::path::PathBuf::from,
    )
    .join(nonce.to_string());
  std::fs::create_dir_all(&folder).expect("report folder");
  folder
}

#[allow(clippy::expect_used)]
fn write_report(folder: &std::path::Path, outcomes: &[Outcome]) -> std::path::PathBuf {
  let path = folder.join("report.json");
  std::fs::write(
    &path,
    serde_json::to_vec_pretty(outcomes).expect("report JSON"),
  )
  .expect("report written");
  eprintln!("Report: {}", path.display());
  path
}

fn log(outcome: &Outcome) {
  let case = outcome.case;
  eprintln!(
    "{} {:?} {}x{}@{} {:?} {:?}: {} ({:.0} fps, open {:.0} ms){}",
    outcome.adapter,
    outcome.encoder,
    case.width,
    case.height,
    case.fps,
    case.quality,
    outcome.conversion,
    if outcome.passed { "PASS" } else { "FAIL" },
    outcome.frames_per_second,
    outcome.open_ms,
    outcome
      .error
      .as_deref()
      .map_or_else(String::new, |error| format!(" — {error}"))
  );
}

#[test]
#[ignore = "Explicit vendor hardware acceptance (NVIDIA/Intel); offscreen, no desktop or audio"]
#[allow(clippy::expect_used)]
fn vendor_encoder_offscreen_acceptance() {
  let _com = super::Apartment::new().expect("COM");
  let _mf = super::MediaRuntime::new().expect("MF");
  let report = gpu::doctor().expect("capabilities");
  eprintln!(
    "Runtimes: NVENC API {:?}; QSV {:?}",
    report.nvenc_api, report.qsv_runtimes
  );
  let folder = report_folder("vendor-acceptance");
  let rehearsal = std::env::var_os("FLINCH_VENDOR_REHEARSAL").is_some();
  let mut outcomes = Vec::new();
  let mut adapters = HashSet::new();
  for output in gpu::outputs().expect("adapters") {
    let encoder = match output.info.vendor_id {
      0x10de => EncoderKind::Nvenc,
      0x8086 => EncoderKind::Qsv,
      // Rehearsal of this harness on AMD hardware, before rented vendor time is spent.
      0x1002 if rehearsal => EncoderKind::Amf,
      _ => continue,
    };
    // One pass per adapter, whatever the number of its displays.
    if !adapters.insert(output.info.adapter_luid) {
      continue;
    }
    let source = Source::of(&output);
    for case in CASES {
      let outcome = run(&source, encoder, case, &folder);
      log(&outcome);
      outcomes.push(outcome);
    }
  }
  let path = write_report(&folder, &outcomes);
  assert!(
    !outcomes.is_empty(),
    "No NVIDIA or Intel adapter has an attached display on this machine"
  );
  assert!(
    outcomes.iter().all(|outcome| outcome.passed),
    "Vendor acceptance failed; see {}",
    path.display()
  );
}

/// Measures, never lowers: every encoder automatic selection can pick on this machine (the display
/// adapter's vendor encoder, then software) and software on WARP's CPU conversion tier. For each it
/// finds the smallest even width (at the strip height), the smallest even height (at the strip
/// width) and the smallest square. Every area the engine accepts must record: software is the
/// fallback automatic selection ends with, so its smallest square must not exceed
/// `Region::MIN_SIZE` on any adapter. A vendor encoder that needs more is reported, not failed,
/// since selection falls back to software below its limit.
#[test]
#[ignore = "Explicit smallest-area probe of this machine's encoders; offscreen, no desktop or audio"]
#[allow(clippy::expect_used)]
fn smallest_area_probe() {
  let _com = super::Apartment::new().expect("COM");
  let _mf = super::MediaRuntime::new().expect("MF");
  let folder = report_folder("smallest-area");
  let mut runs = Vec::new();
  let mut adapters = HashSet::new();
  for output in gpu::outputs().expect("adapters") {
    if !adapters.insert(output.info.adapter_luid) {
      continue;
    }
    let vendor = match output.info.vendor_id {
      0x1002 => Some(EncoderKind::Amf),
      0x10de => Some(EncoderKind::Nvenc),
      0x8086 => Some(EncoderKind::Qsv),
      _ => None,
    };
    for encoder in vendor.into_iter().chain([EncoderKind::Software]) {
      runs.push((Source::of(&output), encoder));
    }
  }
  runs.push((Source::warp().expect("WARP"), EncoderKind::Software));
  let mut outcomes = Vec::new();
  let mut squares = Vec::new();
  for (source, encoder) in &runs {
    // The first passing size of an ascending scan; the last refusal before it is logged as the reason.
    let mut scan = |shape: &dyn Fn(u32) -> (u32, u32)| -> Option<u32> {
      let mut refusal = None;
      let found = SCAN.step_by(2).find(|&n| {
        let (width, height) = shape(n);
        let case = Case {
          width,
          height,
          fps: 30,
          // The app's Medium quality.
          quality: Quality::Cqp { i: 23, p: 23 },
        };
        let outcome = run(source, *encoder, case, &folder);
        let passed = outcome.passed;
        if passed {
          if let Some(refused) = refusal.take() {
            log(&refused);
            outcomes.push(refused);
          }
          log(&outcome);
          outcomes.push(outcome);
        } else {
          refusal = Some(outcome);
        }
        passed
      });
      if let Some(refused) = refusal {
        log(&refused);
        outcomes.push(refused);
      }
      found
    };
    let width = scan(&|n| (n, STRIP));
    let height = scan(&|n| (STRIP, n));
    let square = scan(&|n| (n, n));
    eprintln!(
      "{} {encoder:?}: smallest width {width:?} (at height {STRIP}), height {height:?} (at width {STRIP}), square {square:?}",
      source.name
    );
    squares.push((source.name.clone(), *encoder, square));
  }
  let path = write_report(&folder, &outcomes);
  let refused: Vec<_> = squares
    .iter()
    .filter(|(_, encoder, square)| {
      *encoder == EncoderKind::Software && square.is_none_or(|n| n > Region::MIN_SIZE)
    })
    .map(|(adapter, encoder, square)| format!("{adapter} {encoder:?} ({square:?})"))
    .collect();
  assert!(
    refused.is_empty(),
    "The engine accepts {0}x{0} areas, but the software fallback refuses them on: {1}; see {2}",
    Region::MIN_SIZE,
    refused.join(", "),
    path.display()
  );
}
