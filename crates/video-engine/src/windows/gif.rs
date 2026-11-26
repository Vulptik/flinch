//! Native GIF from a finished recording: two bounded offline decode passes (palette training,
//! then dithering and writing on every core, see `pipeline`). Replaces FFmpeg's
//! `palettegen`/`paletteuse` path.
use super::verify::{self, Geometry};
use crate::gif::{
  GifEmitter, GifOptions, GifPlanner, GifReport, Nv12, PaletteSampler, nv12_to_rgb,
};
#[cfg(test)]
use crate::gif::{GifWriter, quantize};
use crate::{EngineError, ErrorKind, Result, recovery, time};
use std::fs::{self, OpenOptions};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::Media::MediaFoundation::IMFSourceReader;

/// Hard ceiling on decoded frames per pass (ten hours at 30 FPS).
const FRAME_LIMIT: u64 = 1_080_000;

/// Encode `input` (an engine MP4) to `output`. The GIF is written beside the destination and
/// renamed into place only when complete; an existing destination is refused, never replaced.
pub fn encode(input: &Path, output: &Path, options: GifOptions) -> Result<GifReport> {
  encode_with(input, output, options, || Ok(()))
}
/// [`encode`] with `checkpoint` run before every decoded frame of both passes and while the
/// writing pass drains, so a caller can cancel; its error ends encoding, removes the partial GIF
/// and is returned unchanged.
pub fn encode_with(
  input: &Path,
  output: &Path,
  options: GifOptions,
  checkpoint: impl FnMut() -> Result<()>,
) -> Result<GifReport> {
  encode_tuned(
    input,
    output,
    options,
    checkpoint,
    (workers(), pipeline::PIPELINE_BYTES),
  )
}
/// [`encode_with`] on a chosen number of threads and memory budget for frames in flight.
fn encode_tuned(
  input: &Path,
  output: &Path,
  options: GifOptions,
  mut checkpoint: impl FnMut() -> Result<()>,
  (workers, memory): (usize, usize),
) -> Result<GifReport> {
  let _com = super::Apartment::new()?;
  let _mf = super::MediaRuntime::new()?;
  if output.exists() {
    return Err(EngineError::new(
      ErrorKind::Io,
      "gif output",
      "Destination already exists",
    ));
  }
  let structure = recovery::inspect(input)?;
  if structure.truncated {
    return Err(EngineError::new(
      ErrorKind::Io,
      "gif input",
      "Recording has an incomplete fragment",
    ));
  }
  // Each pass may take up to twice real time on a slow CPU, plus a fixed allowance.
  let budget = Duration::from_secs((structure.duration_100ns / time::SECOND * 2 + 60).min(21_660));

  let mut sampler = PaletteSampler::default();
  let mut size = None;
  decode(input, Instant::now() + budget, |frame| {
    checkpoint()?;
    size.get_or_insert(frame.size());
    sampler.add_nv12(&frame.nv12()?);
    Ok(())
  })?;
  let palette = sampler.palette()?;
  let (width, height) =
    size.ok_or_else(|| EngineError::protocol("Recording has no video frames"))?;

  let partial = partial_path(output)?;
  // Only a partial this call created is ever removed.
  let file = OpenOptions::new()
    .write(true)
    .create_new(true)
    .open(&partial)
    .map_err(|e| EngineError::io("gif partial", e))?;
  let result = (|| {
    let planner = GifPlanner::new(width, height, &palette)?;
    let emitter = GifEmitter::new(BufWriter::new(file), &planner, &palette)?;
    let deadline = Instant::now() + budget;
    // Decoding and draining both run here, never at once, so they share the caller's checkpoint.
    let checkpoint = std::cell::RefCell::new(&mut checkpoint);
    let (buffered, mut report) = pipeline::run(
      emitter,
      planner,
      pipeline::Pass {
        palette: &palette,
        dither: options.dither,
        workers,
        deadline,
        budget: memory,
      },
      |feed| {
        decode(input, deadline, |frame| {
          (checkpoint.borrow_mut())()?;
          let mut rgb = Vec::new();
          frame.rgb(&mut rgb)?;
          feed.push(rgb, frame.pts)
        })
      },
      || (checkpoint.borrow_mut())(),
    )?;
    let file = buffered
      .into_inner()
      .map_err(|e| EngineError::io("gif flush", e.into_error()))?;
    file
      .sync_all()
      .map_err(|e| EngineError::io("gif sync", e))?;
    report.bytes = file
      .metadata()
      .map_err(|e| EngineError::io("gif size", e))?
      .len();
    drop(file);
    // Atomic no-replace: a destination created during encoding is never overwritten.
    crate::artifact::move_no_replace(&partial, output, "gif publish")?;
    Ok(report)
  })();
  if result.is_err()
    && let Err(error) = fs::remove_file(&partial)
  {
    eprintln!("Incomplete GIF cleanup failed: {error}");
  }
  result
}

/// Dithering and compression threads: every core, within a bound.
fn workers() -> usize {
  std::thread::available_parallelism().map_or(1, |cores| cores.get().min(16))
}

/// The one-thread writing pass the parallel pipeline replaced: the byte-for-byte reference
/// its tests compare against.
#[cfg(test)]
fn encode_sequential(input: &Path, output: &Path, options: GifOptions) -> Result<GifReport> {
  let _com = super::Apartment::new()?;
  let _mf = super::MediaRuntime::new()?;
  let deadline = Instant::now() + Duration::from_secs(600);
  let mut sampler = PaletteSampler::default();
  let mut rgb = Vec::new();
  decode(input, deadline, |frame| {
    frame.rgb(&mut rgb)?;
    sampler.add(&rgb);
    Ok(())
  })?;
  let palette = sampler.palette()?;
  let file = fs::File::create(output).map_err(|e| EngineError::io("gif reference", e))?;
  let mut writer: Option<GifWriter<BufWriter<fs::File>>> = None;
  let mut file = Some(file);
  let (mut indices, mut prior_rgb, mut prior_indices) = (Vec::new(), Vec::new(), Vec::new());
  let end = decode(input, deadline, |frame| {
    frame.rgb(&mut rgb)?;
    let (width, height) = frame.size();
    quantize(
      &rgb,
      width,
      height,
      &palette,
      options.dither,
      (!prior_indices.is_empty()).then_some((prior_rgb.as_slice(), prior_indices.as_slice())),
      &mut indices,
    )?;
    if writer.is_none() {
      let file = file
        .take()
        .ok_or_else(|| EngineError::protocol("GIF output already opened"))?;
      writer = Some(GifWriter::new(
        BufWriter::new(file),
        width,
        height,
        &palette,
      )?);
    }
    writer
      .as_mut()
      .ok_or_else(|| EngineError::protocol("GIF writer missing"))?
      .push(&indices, frame.pts)?;
    std::mem::swap(&mut rgb, &mut prior_rgb);
    std::mem::swap(&mut indices, &mut prior_indices);
    Ok(())
  })?;
  let writer = writer.ok_or_else(|| EngineError::protocol("Recording has no video frames"))?;
  let (buffered, report) = writer.finish(end)?;
  buffered
    .into_inner()
    .map_err(|e| EngineError::io("gif flush", e.into_error()))?;
  Ok(report)
}

fn partial_path(output: &Path) -> Result<PathBuf> {
  let name = output
    .file_name()
    .ok_or_else(|| EngineError::protocol("GIF output has no file name"))?
    .to_string_lossy();
  let nonce = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_err(|e| EngineError::protocol(e.to_string()))?
    .as_nanos();
  Ok(output.with_file_name(format!(".{name}.{}-{nonce}.partial", std::process::id())))
}

/// One decoded picture in display order.
struct Frame<'a> {
  bytes: &'a [u8],
  geometry: &'a Geometry,
  pts: u64,
  /// Presentation time of the recording's first frame.
  first: u64,
}
impl Frame<'_> {
  fn size(&self) -> (usize, usize) {
    (
      self.geometry.display.0 as usize,
      self.geometry.display.1 as usize,
    )
  }
  fn rgb(&self, rgb: &mut Vec<u8>) -> Result<()> {
    let (width, height) = self.size();
    nv12_to_rgb(
      self.bytes,
      self.geometry.stride,
      self.geometry.height,
      width,
      height,
      rgb,
    )
  }
  fn nv12(&self) -> Result<Nv12<'_>> {
    let (width, height) = self.size();
    Nv12::new(
      self.bytes,
      self.geometry.stride,
      self.geometry.height,
      width,
      height,
    )
  }
}

/// One read from a decoded video source.
struct Step {
  finished: bool,
  /// The decoder renegotiated its output (for example padded rows); geometry must be re-read.
  changed: bool,
  /// Presentation time and duration (100 ns) and the NV12 bytes.
  sample: Option<(i64, i64, Vec<u8>)>,
}
/// Decoded frames in display order: the Media Foundation reader, or a scripted test source.
trait Source {
  fn geometry(&mut self) -> Result<Geometry>;
  fn next(&mut self) -> Result<Step>;
}
/// Dates decoded frames by container position, as the verifier does: Media Foundation has
/// reported backwards video times for a recording whose container order was exact.
/// The native pipeline has no B-frames, so the k-th decoded frame is the k-th container sample;
/// it lasts until the next sample starts, the last one until the track ends.
struct ContainerClock {
  starts: Vec<i64>,
  end: i64,
  next: usize,
}
impl ContainerClock {
  fn date(&mut self) -> Result<(i64, i64)> {
    let pts = *self.starts.get(self.next).ok_or_else(|| {
      EngineError::protocol(format!(
        "Decoder produced video frame {} but the container holds {}",
        self.next,
        self.starts.len()
      ))
    })?;
    let following = self.starts.get(self.next + 1).copied().unwrap_or(self.end);
    self.next += 1;
    Ok((pts, following - pts))
  }
  /// Every container sample must have been decoded; a missing frame would shift the rest.
  fn finish(&self) -> Result<()> {
    if self.next != self.starts.len() {
      return Err(EngineError::protocol(format!(
        "Decoder produced {} video frames but the container holds {}",
        self.next,
        self.starts.len()
      )));
    }
    Ok(())
  }
}
struct MediaSource {
  reader: IMFSourceReader,
  stream: u32,
  clock: ContainerClock,
}
impl Source for MediaSource {
  fn geometry(&mut self) -> Result<Geometry> {
    verify::geometry(&self.reader, self.stream, false)
  }
  fn next(&mut self) -> Result<Step> {
    let (finished, changed, sample) = verify::next(&self.reader, self.stream, true)?;
    let sample = match sample {
      Some(sample) => {
        let (pts, duration) = self.clock.date()?;
        Some((pts, duration, super::mux::sample_bytes(&sample)?))
      },
      None => None,
    };
    if finished {
      self.clock.finish()?;
    }
    Ok(Step {
      finished,
      changed,
      sample,
    })
  }
}

/// Decode every video frame in order; returns the end of the last frame (100 ns, from zero).
fn decode(
  input: &Path,
  deadline: Instant,
  each: impl FnMut(Frame<'_>) -> Result<()>,
) -> Result<u64> {
  let (starts, end) = verify::video_timeline(input)?;
  let (reader, stream) = verify::video_reader(input)?;
  decode_from(
    &mut MediaSource {
      reader,
      stream,
      clock: ContainerClock {
        starts,
        end,
        next: 0,
      },
    },
    deadline,
    FRAME_LIMIT,
    each,
  )
}

/// The decode loop and its checks, independent of where frames come from.
fn decode_from(
  source: &mut impl Source,
  deadline: Instant,
  limit: u64,
  mut each: impl FnMut(Frame<'_>) -> Result<()>,
) -> Result<u64> {
  let mut geometry = source.geometry()?;
  let display = geometry.display;
  let mut previous: Option<i64> = None;
  let mut first: Option<u64> = None;
  let mut end = 0u64;
  for _ in 0..=limit {
    if Instant::now() > deadline {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "gif decode",
        "Offline decode exceeded its duration budget",
      ));
    }
    let step = source.next()?;
    if step.changed {
      geometry = source.geometry()?;
      if geometry.display != display {
        return Err(EngineError::protocol(
          "Decoded picture size changed mid-stream",
        ));
      }
    }
    if let Some((pts, duration, bytes)) = step.sample {
      if pts < 0 || duration <= 0 || previous.is_some_and(|p| pts <= p) {
        return Err(EngineError::protocol(
          "Decoded video timestamps are not increasing",
        ));
      }
      previous = Some(pts);
      end = (pts + duration) as u64;
      each(Frame {
        bytes: &bytes,
        geometry: &geometry,
        pts: pts as u64,
        first: *first.get_or_insert(pts as u64),
      })?;
    }
    if step.finished {
      return Ok(end);
    }
  }
  Err(EngineError::new(
    ErrorKind::Capability,
    "gif decode",
    "Recording exceeds the GIF frame limit",
  ))
}

/// How closely a GIF reproduces its source recording, frame by frame in source time.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Fidelity {
  pub source_frames: u64,
  pub gif_frames: u64,
  pub mean_psnr_db: f64,
  pub min_psnr_db: f64,
}

/// Independent quality oracle: composite the GIF by its own delays and compare the picture
/// shown at each source frame's time with that frame (RGB PSNR, 100 dB when identical).
/// Works for any GIF writer, so native and FFmpeg output are judged the same way.
pub fn fidelity(source: &Path, gif_path: &Path) -> Result<Fidelity> {
  let _com = super::Apartment::new()?;
  let _mf = super::MediaRuntime::new()?;
  let mut options = ::gif::DecodeOptions::new();
  options.set_color_output(::gif::ColorOutput::RGBA);
  let file = fs::File::open(gif_path).map_err(|e| EngineError::io("gif open", e))?;
  let mut decoder = options
    .read_info(std::io::BufReader::new(file))
    .map_err(|e| EngineError::new(ErrorKind::Io, "gif read", e.to_string()))?;
  let (width, height) = (usize::from(decoder.width()), usize::from(decoder.height()));
  let mut canvas = vec![0u8; width * height * 4];
  // Shown pictures with the time (100 ns) each stops being shown.
  let mut shown: std::collections::VecDeque<(u64, Vec<u8>)> = std::collections::VecDeque::new();
  let mut gif_end = 0u64;
  let mut gif_done = false;
  let mut report = Fidelity {
    min_psnr_db: f64::INFINITY,
    ..Default::default()
  };
  let mut total = 0.0;
  let mut rgb = Vec::new();
  let structure = recovery::inspect(source)?;
  let budget = Duration::from_secs((structure.duration_100ns / time::SECOND * 2 + 60).min(21_660));
  decode(source, Instant::now() + budget, |frame| {
    let (w, h) = frame.size();
    if (w, h) != (width, height) {
      return Err(EngineError::protocol("GIF and recording sizes differ"));
    }
    frame.rgb(&mut rgb)?;
    // GIF time starts at the first source frame.
    let at = frame.pts - frame.first;
    while !gif_done && shown.back().is_none_or(|(end, _)| *end <= at) {
      let Some(next) = decoder
        .read_next_frame()
        .map_err(|e| EngineError::new(ErrorKind::Io, "gif read", e.to_string()))?
      else {
        gif_done = true;
        break;
      };
      composite(&mut canvas, width, next)?;
      gif_end += u64::from(next.delay) * 100_000;
      report.gif_frames += 1;
      shown.push_back((gif_end, canvas.clone()));
      if shown.len() > 2 {
        shown.pop_front();
      }
    }
    let picture = shown
      .iter()
      .find(|(end, _)| *end > at)
      .or(shown.back())
      .map(|(_, picture)| picture)
      .ok_or_else(|| EngineError::protocol("GIF has no frames"))?;
    let mut squared = 0u64;
    for (source, shown) in rgb
      .as_chunks::<3>()
      .0
      .iter()
      .zip(picture.as_chunks::<4>().0)
    {
      for channel in 0..3 {
        let d = i64::from(source[channel]) - i64::from(shown[channel]);
        squared += (d * d) as u64;
      }
    }
    let mse = squared as f64 / (width * height * 3) as f64;
    let psnr = if mse == 0.0 {
      100.0
    } else {
      10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    report.source_frames += 1;
    report.min_psnr_db = report.min_psnr_db.min(psnr);
    total += psnr;
    Ok(())
  })?;
  if report.source_frames == 0 {
    return Err(EngineError::protocol("Recording has no video frames"));
  }
  report.mean_psnr_db = total / report.source_frames as f64;
  Ok(report)
}

fn composite(canvas: &mut [u8], width: usize, frame: &::gif::Frame<'_>) -> Result<()> {
  let (left, top) = (usize::from(frame.left), usize::from(frame.top));
  let (w, h) = (usize::from(frame.width), usize::from(frame.height));
  if left + w > width || (top + h) * width * 4 > canvas.len() || frame.buffer.len() < w * h * 4 {
    return Err(EngineError::protocol("GIF frame lies outside its picture"));
  }
  for y in 0..h {
    for x in 0..w {
      let pixel = &frame.buffer[(y * w + x) * 4..(y * w + x) * 4 + 4];
      if pixel[3] != 0 {
        let at = ((top + y) * width + left + x) * 4;
        canvas[at..at + 4].copy_from_slice(pixel);
      }
    }
  }
  Ok(())
}

#[path = "gif/pipeline.rs"]
mod pipeline;
#[cfg(test)]
#[path = "gif/tests.rs"]
mod tests;
