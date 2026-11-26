//! Offline native decoding. This never displays frames or plays audio.
use super::mux;
use crate::{
  EngineError, ErrorKind, Result,
  recovery::{self, Inspection},
  time,
};
use serde::{Deserialize, Serialize};
use std::{
  path::Path,
  time::{Duration, Instant},
};
use windows::{Win32::Media::MediaFoundation::*, core::PCWSTR};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verification {
  pub structure: Inspection,
  pub video_frames: u64,
  /// Displayed extent: the decoder's minimum display aperture when present, else its frame size.
  #[serde(default)]
  pub video_width: u32,
  #[serde(default)]
  pub video_height: u32,
  pub audio_frames: u64,
  pub numbered_frames_checked: u64,
  #[serde(default)]
  pub repeated_source_frames: u64,
  #[serde(default)]
  pub longest_source_hold_ms: f64,
  #[serde(default)]
  pub maximum_video_timestamp_gap_ms: f64,
  pub marker_pairs: usize,
  pub maximum_sync_ms: Option<f64>,
  pub drift_ms: Option<f64>,
  pub markers_checked: bool,
  pub accepted_sync: bool,
  #[serde(default)]
  pub sync_failure: Option<String>,
  pub sync_offsets_ms: Vec<f64>,
  pub video_marker_times_ms: Vec<f64>,
  pub audio_marker_times_ms: Vec<f64>,
  /// PTS of the last non-pulse frame before each visual onset; the onset lies after it.
  #[serde(default)]
  pub video_marker_previous_ms: Vec<Option<f64>>,
  /// Source frame number decoded from the first pulse frame of each visual onset.
  #[serde(default)]
  pub video_marker_source_index: Vec<u32>,
  /// Render-sample clock strip decoded from the first pulse frame (fixtures that paint it).
  #[serde(default)]
  pub video_marker_clock_samples: Vec<Option<u32>>,
  #[serde(default)]
  pub independent_sync: Option<IndependentSync>,
  /// Decoder timestamps are diagnostics only: decoded output is dated by its container position
  /// ([`Timeline`]). Windows Server's decoders reported timestamps that went backwards or jumped
  /// 100 ms on recordings whose container and decoded content were exact (CI).
  #[serde(default)]
  pub decoder_clock: DecoderClocks,
}
/// How far the decoder's own timestamps strayed from the container's, per stream.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DecoderClocks {
  pub video_reversals: u64,
  pub video_maximum_offset_ms: f64,
  pub audio_reversals: u64,
  pub audio_maximum_offset_ms: f64,
  /// Decoded PCM frames past the container's last AAC packet (1024 frames each).
  pub audio_frames_beyond_container: u64,
}
/// One decoder clock compared with the container time of the same output.
#[derive(Default)]
struct DecoderClock {
  previous: Option<i64>,
  reversals: u64,
  maximum_offset_100ns: u64,
}
impl DecoderClock {
  fn observe(&mut self, decoder: i64, container: i64) {
    if self.previous.is_some_and(|previous| decoder <= previous) {
      self.reversals += 1;
    }
    self.previous = Some(decoder);
    self.maximum_offset_100ns = self.maximum_offset_100ns.max(decoder.abs_diff(container));
  }
  fn maximum_offset_ms(&self) -> f64 {
    self.maximum_offset_100ns as f64 / 10_000.0
  }
}
/// PCM frames one AAC packet decodes to.
const AAC_FRAMES: u64 = 1024;
/// Rate of the PCM the verifier requests from the AAC decoder (`mux::audio_type`).
const PCM_RATE: u64 = 48_000;
/// Container presentation times for decoded output. The native pipeline has no B-frames, so the
/// k-th decoded video frame is the k-th container sample; each AAC packet decodes to 1024 PCM
/// frames, so a PCM position maps to its packet's container start plus its offset inside it.
struct Timeline {
  /// Video sample starts in 100 ns.
  video: Vec<i64>,
  /// Audio packet starts in the audio track's ticks, and those ticks per second.
  audio: Vec<u64>,
  audio_scale: u64,
}
impl Timeline {
  fn new(structure: &Inspection) -> Result<Self> {
    let track = |handler: &[u8; 4]| {
      structure
        .track_media
        .iter()
        .find(|track| track.handler.as_ref() == Some(handler))
    };
    let video = track(b"vide").ok_or_else(|| EngineError::protocol("No video track"))?;
    if video.timescale == 0 || video.sample_starts.len() as u64 != video.samples {
      return Err(EngineError::protocol(
        "Container video timeline is incomplete",
      ));
    }
    let video = video
      .sample_starts
      .iter()
      .map(|start| {
        time::scale(*start, time::SECOND, u64::from(video.timescale)).and_then(|value| {
          i64::try_from(value).map_err(|_| EngineError::protocol("Video time overflow"))
        })
      })
      .collect::<Result<Vec<_>>>()?;
    let (audio, audio_scale) = match track(b"soun") {
      Some(audio) if audio.timescale == 0 => {
        return Err(EngineError::protocol(
          "Container audio timeline is incomplete",
        ));
      },
      Some(audio) => (audio.sample_starts.clone(), u64::from(audio.timescale)),
      None => (Vec::new(), 1),
    };
    Ok(Self {
      video,
      audio,
      audio_scale,
    })
  }
  /// Container time of the `index`-th decoded video frame.
  fn video(&self, index: u64) -> Result<i64> {
    usize::try_from(index)
      .ok()
      .and_then(|index| self.video.get(index))
      .copied()
      .ok_or_else(|| {
        EngineError::protocol(format!(
          "Decoder produced video frame {index} but the container holds {}",
          self.video.len()
        ))
      })
  }
  /// PCM frames the container's AAC packets decode to.
  fn audio_frames(&self) -> u64 {
    self.audio.len() as u64 * AAC_FRAMES
  }
  /// Container time in ms of decoded PCM frame `position`, and whether it lies past the last
  /// packet (then it continues that packet's spacing, and the caller counts it).
  fn audio_ms(&self, position: u64) -> Result<(f64, bool)> {
    let last = self
      .audio
      .len()
      .checked_sub(1)
      .ok_or_else(|| EngineError::protocol("Container has no audio packets"))?;
    let packet = usize::try_from(position / AAC_FRAMES).unwrap_or(usize::MAX);
    let (start, extra) = if packet <= last {
      (self.audio[packet], 0)
    } else {
      (self.audio[last], (packet - last) as u64)
    };
    let inside = (extra * AAC_FRAMES + position % AAC_FRAMES) as f64 / PCM_RATE as f64;
    Ok((
      (start as f64 / self.audio_scale as f64 + inside) * 1000.0,
      packet > last,
    ))
  }
}
/// Container video sample starts and the video track's end (100 ns), for consumers that date
/// decoded frames by container position instead of the decoder's clock (GIF conversion).
pub(super) fn video_timeline(path: &Path) -> Result<(Vec<i64>, i64)> {
  let structure = recovery::inspect_timeline(path)?;
  if structure.truncated {
    return Err(EngineError::new(
      ErrorKind::Io,
      "decode",
      "File has an incomplete fragment",
    ));
  }
  let end = structure
    .track_media
    .iter()
    .find(|track| track.handler.as_ref() == Some(b"vide"))
    .map(|track| track.end_100ns)
    .ok_or_else(|| EngineError::protocol("No video track"))?;
  let end = i64::try_from(end).map_err(|_| EngineError::protocol("Video time overflow"))?;
  Ok((Timeline::new(&structure)?.video, end))
}
fn reader(path: &Path) -> Result<IMFSourceReader> {
  let path: Vec<u16> = path
    .as_os_str()
    .to_string_lossy()
    .encode_utf16()
    .chain([0])
    .collect();
  // SAFETY: reader owns its file and decoder, has no rendering sink and forbids DXVA.
  unsafe {
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 2).map_err(|e| EngineError::native("decoder attributes", e))?;
    let attrs = attrs.ok_or_else(|| EngineError::protocol("No decoder attributes"))?;
    attrs
      .SetUINT32(&MF_SOURCE_READER_DISABLE_DXVA, 1)
      .map_err(|e| EngineError::native("disable DXVA", e))?;
    attrs
      .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)
      .map_err(|e| EngineError::native("software verification", e))?;
    MFCreateSourceReaderFromURL(PCWSTR(path.as_ptr()), &attrs)
      .map_err(|e| EngineError::native("decoder open", e))
  }
}
/// Software decoder for the video stream alone, producing CPU-readable NV12.
pub(super) fn video_reader(path: &Path) -> Result<(IMFSourceReader, u32)> {
  let video = reader(path)?;
  let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
  // SAFETY: explicitly select only the video stream and request CPU-readable NV12.
  unsafe {
    video
      .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
      .map_err(|e| EngineError::native("decoder stream selection", e))?;
    video
      .SetStreamSelection(stream, true)
      .map_err(|e| EngineError::native("decoder video selection", e))?;
    video
      .SetCurrentMediaType(
        stream,
        None,
        &mux::media_type(&MFMediaType_Video, &MFVideoFormat_NV12)?,
      )
      .map_err(|e| EngineError::native("decoder NV12 output", e))?;
  }
  Ok((video, stream))
}
/// Decoded NV12 layout: coded width, height and stride, plus the displayed extent.
pub(super) struct Geometry {
  pub(super) width: usize,
  pub(super) height: usize,
  pub(super) stride: usize,
  pub(super) display: (u32, u32),
}
pub(super) fn geometry(reader: &IMFSourceReader, stream: u32, markers: bool) -> Result<Geometry> {
  // SAFETY: immutable negotiated output type and size.
  let media = unsafe { reader.GetCurrentMediaType(stream) }
    .map_err(|e| EngineError::native("decoded media type", e))?;
  // SAFETY: output has a negotiated frame size.
  let packed = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE) }
    .map_err(|e| EngineError::native("decoded extent", e))?;
  let width = (packed >> 32) as usize;
  let height = packed as u32 as usize;
  // A positive stride may exceed width. If absent, tightly packed NV12 is the documented default here.
  // SAFETY: optional attribute query on a live immutable media type.
  let stride = match unsafe { media.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
    Ok(v) => v as i32,
    Err(e) if e.code() == MF_E_ATTRIBUTENOTFOUND => width as i32,
    Err(e) => return Err(EngineError::native("decoded stride", e)),
  };
  if stride <= 0
    || (stride as usize) < width
    || width < 2
    || height < 2
    || (markers && (width < 512 || height < 16))
  {
    return Err(EngineError::protocol("Unsupported decoded NV12 extent"));
  }
  Ok(Geometry {
    width,
    height,
    stride: stride as usize,
    display: display_extent(&media, width, height)?,
  })
}
/// Returns (end of stream, current output type changed, sample). Only the video decoder may
/// renegotiate its output, which it does to pad odd macroblock rows; errors and native changes fail.
pub(super) fn next(
  reader: &IMFSourceReader,
  index: u32,
  allow_type_change: bool,
) -> Result<(bool, bool, Option<IMFSample>)> {
  let mut sample = None;
  let mut flags = 0;
  // SAFETY: synchronous offline reader; outputs belong to this call and are checked before use.
  unsafe { reader.ReadSample(index, 0, None, Some(&mut flags), None, Some(&mut sample)) }
    .map_err(|e| EngineError::native("decode sample", e))?;
  let changed = flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0;
  if flags & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0) as u32 != 0
    || (changed && !allow_type_change)
  {
    return Err(EngineError::new(
      ErrorKind::Io,
      "decode",
      "Decoder reported an error or changed media type",
    ));
  }
  Ok((
    flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0,
    changed,
    sample,
  ))
}
pub fn marker_offsets(video: &[f64], audio: &[f64]) -> Result<Vec<f64>> {
  if video.len() < 2 || video.len() != audio.len() {
    return Err(EngineError::protocol(format!(
      "Marker counts differ: video={}, audio={}",
      video.len(),
      audio.len()
    )));
  }
  if video
    .windows(2)
    .chain(audio.windows(2))
    .any(|p| !p[0].is_finite() || !p[1].is_finite() || p[1] <= p[0])
  {
    return Err(EngineError::protocol(
      "Marker times must be finite and ordered",
    ));
  }
  let differences: Vec<_> = video.iter().zip(audio).map(|(v, a)| a - v).collect();
  if let Some(i) = differences.iter().position(|d| d.abs() >= 250.0) {
    return Err(EngineError::protocol(format!(
      "Marker {i} pairing exceeds 250 ms: video {:.1} ms, audio {:.1} ms (video {:.1?}, audio {:.1?})",
      video[i], audio[i], video, audio
    )));
  }
  Ok(differences)
}
/// Initial acceptance limits (RECORDING_ENGINE.md section 4.1); both marker methods share them.
pub const MAX_OFFSET_MS: f64 = 25.0;
pub const MAX_VARIATION_MS: f64 = 5.0;
/// Sync limit: 5 ms, or one composition refresh when
/// the frame period is not a whole number of refreshes. A fixed frame grid then samples the
/// refresh at a cycling phase (60 FPS on 144 Hz: five phases over 5.56 ms) that no alignment can
/// hold. An unknown period or frame rate keeps the strict limit.
pub fn variation_limit_ms(fps: Option<u32>, period_100ns: Option<u64>) -> f64 {
  match (fps, period_100ns) {
    (Some(fps), Some(period))
      if period > 0 && crate::time::refreshes_per_frame(fps, period).is_none() =>
    {
      MAX_VARIATION_MS.max(period as f64 / 10_000.0)
    },
    _ => MAX_VARIATION_MS,
  }
}
/// The fixture display's composition period, or its mode's period for older reports.
fn fixture_period(fixture: &super::desktop_fixture::FixtureReport) -> Option<u64> {
  fixture.composition_period_100ns.or_else(|| {
    fixture
      .display_refresh_hz
      .filter(|hz| *hz > 0)
      .map(|hz| crate::time::SECOND / u64::from(hz))
  })
}
/// The recording's cadence from decoded frames over its duration, when it is a supported one.
/// Marker onsets in a stream of on/off observations. An onset counts only after the signal was
/// observed off for `quiet` consecutive observations, so a pulse or tone already on when the
/// recording starts is never mistaken for one (measured: a recording that began
/// mid-pulse paired its first frame with the first sound after the start-up silence, 40 ms off).
#[derive(Default)]
struct Onsets {
  /// Off observations required before an onset; at least one.
  quiet: u64,
  off_run: u64,
  on: bool,
}
impl Onsets {
  /// Audio: a 1 kHz tone dips under the threshold every half cycle for a few samples, far
  /// shorter than the silence between markers.
  fn after_quiet(quiet: u64) -> Self {
    Self {
      quiet,
      ..Self::default()
    }
  }
  fn onset(&mut self, on: bool) -> bool {
    let onset = on && !self.on && self.off_run >= self.quiet.max(1);
    if on {
      self.off_run = 0;
    } else {
      self.off_run = self.off_run.saturating_add(1);
    }
    self.on = on;
    onset
  }
}
fn recorded_fps(report: &Verification) -> Option<u32> {
  let duration = report.structure.duration_100ns;
  if duration == 0 {
    return None;
  }
  let fps = (report.video_frames as f64 * 1e7 / duration as f64).round();
  crate::types::SUPPORTED_FPS
    .iter()
    .copied()
    .find(|supported| f64::from(*supported) == fps)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncVerdict {
  /// Every offset consistent with the observations meets both limits.
  Accepted,
  /// No offset consistent with the observations meets the limits.
  Failed,
  /// The observations are too coarse to decide; this is not a pass.
  Unresolved,
}
/// Offset (audio minus video, ms) bounded by what was actually observed. A missing upper
/// bound means the visual onset may precede the first decoded frame.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncInterval {
  pub lower_ms: f64,
  pub upper_ms: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndependentSync {
  pub method: String,
  pub verdict: SyncVerdict,
  /// Widest per-marker interval; None when one is unbounded.
  pub resolution_ms: Option<f64>,
  pub intervals_ms: Vec<SyncInterval>,
  pub fixture_seconds: Vec<u64>,
  pub render_lateness_ms: Vec<f64>,
  /// Smallest maximum absolute offset consistent with the observations.
  pub maximum_offset_best_ms: f64,
  pub maximum_offset_worst_ms: Option<f64>,
  pub variation_lower_ms: f64,
  pub variation_upper_ms: Option<f64>,
  /// The variation limit applied: 5 ms, or one refresh of a non-multiple display.
  #[serde(default)]
  pub variation_limit_ms: f64,
  /// With a clock strip, the offset splits into the audio track's placement against the render
  /// clock and the video frames' content age. These are the spreads (max - min) of each part
  /// across markers, as point estimates, so a variation can be traced to one side.
  #[serde(default)]
  pub audio_placement_spread_ms: Option<f64>,
  #[serde(default)]
  pub video_age_spread_ms: Option<f64>,
  pub limitation: String,
}
/// Evaluate real recordings of the independently clocked desktop fixture.
///
/// A decoded frame only shows that the visual onset happened after the previous decoded
/// frame and no later than the first pulse frame, so each marker yields an offset interval
/// as wide as the frame spacing. The fixture's logged paint position removes its own render
/// lateness (pulse painted after the tone's render onset). Limits are unchanged: acceptance
/// requires the worst consistent case to pass, and failure requires the best case to fail.
pub fn evaluate_independent(
  report: &Verification,
  fixture: &super::desktop_fixture::FixtureReport,
) -> Result<IndependentSync> {
  use super::desktop_fixture::{FRAMES_PER_SECOND, PULSE, RATE};
  let video = &report.video_marker_times_ms;
  let audio = &report.audio_marker_times_ms;
  if report.video_marker_previous_ms.len() != video.len()
    || report.video_marker_source_index.len() != video.len()
  {
    return Err(EngineError::protocol(
      "Verification lacks per-marker onset bounds; decode again with this verifier",
    ));
  }
  marker_offsets(video, audio)?;
  let pulse_frames = PULSE.start * FRAMES_PER_SECOND / RATE..PULSE.end * FRAMES_PER_SECOND / RATE;
  let mut intervals = Vec::with_capacity(video.len());
  let mut seconds = Vec::with_capacity(video.len());
  let mut lateness = Vec::with_capacity(video.len());
  let mut placement = Vec::with_capacity(video.len());
  let mut age = Vec::with_capacity(video.len());
  for (i, (&onset, &tone)) in video.iter().zip(audio).enumerate() {
    let index = u64::from(report.video_marker_source_index[i]);
    if !pulse_frames.contains(&(index % FRAMES_PER_SECOND)) {
      return Err(EngineError::protocol(format!(
        "Visual marker {i} shows source frame {index}, outside the fixture pulse"
      )));
    }
    let second = index / FRAMES_PER_SECOND;
    let marker = fixture
      .markers
      .iter()
      .find(|marker| marker.second == second)
      .ok_or_else(|| {
        EngineError::protocol(format!(
          "Recorded marker at fixture second {second} has no fixture paint observation"
        ))
      })?;
    let nominal = second * RATE + PULSE.start;
    let late = marker
      .render_sample_position
      .checked_sub(nominal)
      .filter(|late| *late < PULSE.end - PULSE.start)
      .ok_or_else(|| {
        EngineError::protocol(format!(
          "Fixture paint position for second {second} is outside its pulse"
        ))
      })? as f64
      * 1000.0
      / RATE as f64;
    if let Some(previous) = report.video_marker_previous_ms[i]
      && !(previous.is_finite() && previous < onset)
    {
      return Err(EngineError::protocol(format!(
        "Visual marker {i} has a non-increasing preceding frame"
      )));
    }
    intervals.push(if let Some(strip) = fixture.clock_strip.as_ref() {
      // The pulse frame's strip dates its content on the render clock (within one strip
      // paint gap), so the video mapping is evaluated at the tone's own render sample.
      let clock = report
        .video_marker_clock_samples
        .get(i)
        .copied()
        .flatten()
        .map(u64::from)
        .ok_or_else(|| {
          EngineError::protocol(
            "Fixture painted a render clock but the recording has no decoded strip values",
          )
        })?;
      // The strip dates the capture instant; the index names the paint whose pulse is shown.
      // DWM can compose a full-frame blit part-way (measured: clock 552804, index
      // 690), showing the next paint's clock beside the previous index, so the clock may lead
      // by one slot. It never trails, and the pulse-paint check below still applies.
      let slot = clock * FRAMES_PER_SECOND / RATE;
      if slot != index && slot != index + 1 {
        return Err(EngineError::protocol(format!(
          "Visual marker {i}: render clock {clock} disagrees with source frame {index}"
        )));
      }
      if clock < marker.render_sample_position {
        return Err(EngineError::protocol(format!(
          "Visual marker {i}: pulse content is dated before its own paint"
        )));
      }
      let ms = |samples: u64| samples as f64 * 1000.0 / RATE as f64;
      let lower = tone - onset + ms(clock - nominal);
      placement.push(tone - ms(nominal));
      age.push(onset - ms(clock));
      // An interpolated strip may show the render clock up to its measured lead ahead of, or lag
      // behind, the real position; the interval covers both. The bounds measured around this
      // pulse apply when the fixture recorded them, otherwise the run's maxima.
      let (gap, lead, lag) = super::desktop_fixture::pulse_window_second(clock)
        .and_then(|second| strip.pulse_windows.iter().find(|w| w.second == second))
        .map_or(
          (
            strip.maximum_gap_samples,
            strip.maximum_lead_samples,
            strip.maximum_lag_samples,
          ),
          |w| {
            (
              w.maximum_gap_samples,
              w.maximum_lead_samples,
              w.maximum_lag_samples,
            )
          },
        );
      SyncInterval {
        lower_ms: lower - ms(lead),
        upper_ms: Some(lower + ms(gap) + ms(lag)),
      }
    } else {
      // Perfect capture reproduces the paint lateness as audio leading video; remove it.
      SyncInterval {
        lower_ms: tone - onset + late,
        upper_ms: report.video_marker_previous_ms[i].map(|previous| tone - previous + late),
      }
    });
    seconds.push(second);
    lateness.push(late);
  }
  let upper = |interval: &SyncInterval| interval.upper_ms.unwrap_or(f64::INFINITY);
  let best = intervals
    .iter()
    .map(|interval| {
      if interval.lower_ms <= 0.0 && upper(interval) >= 0.0 {
        0.0
      } else {
        interval.lower_ms.abs().min(upper(interval).abs())
      }
    })
    .fold(0.0, f64::max);
  let worst = intervals
    .iter()
    .map(|interval| interval.lower_ms.abs().max(upper(interval).abs()))
    .fold(0.0, f64::max);
  let highest_lower = intervals
    .iter()
    .map(|interval| interval.lower_ms)
    .fold(f64::NEG_INFINITY, f64::max);
  let lowest_lower = intervals
    .iter()
    .map(|interval| interval.lower_ms)
    .fold(f64::INFINITY, f64::min);
  let lowest_upper = intervals.iter().map(upper).fold(f64::INFINITY, f64::min);
  let highest_upper = intervals
    .iter()
    .map(upper)
    .fold(f64::NEG_INFINITY, f64::max);
  let variation_lower = (highest_lower - lowest_upper).max(0.0);
  let variation_upper = highest_upper - lowest_lower;
  let resolution = intervals
    .iter()
    .map(|interval| upper(interval) - interval.lower_ms)
    .fold(0.0, f64::max);
  let variation_limit = variation_limit_ms(recorded_fps(report), fixture_period(fixture));
  let verdict = if best > MAX_OFFSET_MS || variation_lower > variation_limit {
    SyncVerdict::Failed
  } else if worst <= MAX_OFFSET_MS && variation_upper <= variation_limit {
    SyncVerdict::Accepted
  } else {
    SyncVerdict::Unresolved
  };
  let finite = |value: f64| value.is_finite().then_some(value);
  let spread = |values: &[f64]| {
    (!values.is_empty()).then(|| {
      values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        - values.iter().copied().fold(f64::INFINITY, f64::min)
    })
  };
  Ok(IndependentSync {
    audio_placement_spread_ms: spread(&placement),
    video_age_spread_ms: spread(&age),
    method: if fixture.clock_strip.is_some() {
      "fixture_render_clock_strip"
    } else {
      "decoded_frame_bracket_with_fixture_paint_lateness"
    }
    .into(),
    verdict,
    resolution_ms: finite(resolution),
    intervals_ms: intervals,
    fixture_seconds: seconds,
    render_lateness_ms: lateness,
    maximum_offset_best_ms: best,
    maximum_offset_worst_ms: finite(worst),
    variation_lower_ms: variation_lower,
    variation_upper_ms: finite(variation_upper),
    variation_limit_ms: variation_limit,
    limitation: if fixture.clock_strip.is_some() {
      "Offsets include compositor, scan-out and capture-phase lag a viewer of the recording also sees, minus audio-render latency; interval width is the largest strip repaint gap."
    } else {
      "Bounds include constant compositor, scan-out and audio-render latency, which affect absolute offset but not variation. Interval width equals decoded frame spacing; a fixture timing signal finer than the capture rate is needed to resolve below it."
    }
    .into(),
  })
}
/// Apply an independent evaluation as the authoritative synchronization result.
pub fn apply_independent(report: &mut Verification, sync: IndependentSync) {
  report.accepted_sync = sync.verdict == SyncVerdict::Accepted;
  report.sync_failure = match sync.verdict {
    SyncVerdict::Accepted => None,
    SyncVerdict::Failed => Some(format!(
      "Independent markers fail the limits: best-case maximum offset {:.3} ms, minimum variation {:.3} ms (limits {MAX_OFFSET_MS} ms / {:.3} ms)",
      sync.maximum_offset_best_ms, sync.variation_lower_ms, sync.variation_limit_ms
    )),
    SyncVerdict::Unresolved => Some(format!(
      "Independent markers are unresolved at {} ms resolution: maximum offset {:.3}..{} ms, variation {:.3}..{} ms; this is not acceptance",
      sync
        .resolution_ms
        .map_or("unbounded".into(), |value| format!("{value:.3}")),
      sync.maximum_offset_best_ms,
      sync
        .maximum_offset_worst_ms
        .map_or("unbounded".into(), |value| format!("{value:.3}")),
      sync.variation_lower_ms,
      sync
        .variation_upper_ms
        .map_or("unbounded".into(), |value| format!("{value:.3}")),
    )),
  };
  report.independent_sync = Some(sync);
}
/// Coded frames may be padded to whole macroblocks; the aperture is the picture a viewer sees.
fn display_extent(media: &IMFMediaType, width: usize, height: usize) -> Result<(u32, u32)> {
  let mut area = MFVideoArea::default();
  let mut size = 0;
  // SAFETY: the blob is copied into a correctly sized, stack-owned MFVideoArea.
  let read = unsafe {
    media.GetBlob(
      &MF_MT_MINIMUM_DISPLAY_APERTURE,
      std::slice::from_raw_parts_mut(
        (&mut area as *mut MFVideoArea).cast::<u8>(),
        std::mem::size_of::<MFVideoArea>(),
      ),
      Some(&mut size),
    )
  };
  let coded = (
    u32::try_from(width).map_err(|_| EngineError::protocol("Decoded width overflow"))?,
    u32::try_from(height).map_err(|_| EngineError::protocol("Decoded height overflow"))?,
  );
  match read {
    Ok(()) if size as usize == std::mem::size_of::<MFVideoArea>() => {
      let (w, h) = (area.Area.cx, area.Area.cy);
      if w <= 0 || h <= 0 || w as u32 > coded.0 || h as u32 > coded.1 {
        return Err(EngineError::protocol(
          "Decoded display aperture exceeds its frame",
        ));
      }
      Ok((w as u32, h as u32))
    },
    Ok(()) => Err(EngineError::protocol(
      "Decoded display aperture has the wrong size",
    )),
    Err(e) if e.code() == MF_E_ATTRIBUTENOTFOUND => Ok(coded),
    Err(e) => Err(EngineError::native("decoded display aperture", e)),
  }
}
pub fn verify(path: &Path, markers: bool) -> Result<Verification> {
  let _com = super::Apartment::new()?;
  let _mf = super::MediaRuntime::new()?;
  let mut structure = recovery::inspect_timeline(path)?;
  if structure.truncated {
    return Err(EngineError::new(
      ErrorKind::Io,
      "decode",
      "File has an incomplete fragment",
    ));
  }
  let timeline = Timeline::new(&structure)?;
  // The report keeps the summary; the per-sample starts live in `timeline`.
  for track in &mut structure.track_media {
    track.sample_starts = Vec::new();
  }
  let (video, stream) = video_reader(path)?;
  let Geometry {
    mut width,
    mut height,
    mut stride,
    display: (video_width, video_height),
  } = geometry(&video, stream, markers)?;
  let deadline = Instant::now()
    + Duration::from_secs((structure.duration_100ns / time::SECOND * 2 + 60).min(21660));
  let mut report = Verification {
    video_width,
    video_height,
    structure: structure.clone(),
    video_frames: 0,
    audio_frames: 0,
    numbered_frames_checked: 0,
    repeated_source_frames: 0,
    longest_source_hold_ms: 0.0,
    maximum_video_timestamp_gap_ms: 0.0,
    marker_pairs: 0,
    maximum_sync_ms: None,
    drift_ms: None,
    markers_checked: markers,
    accepted_sync: false,
    sync_failure: None,
    sync_offsets_ms: Vec::new(),
    video_marker_times_ms: Vec::new(),
    audio_marker_times_ms: Vec::new(),
    video_marker_previous_ms: Vec::new(),
    video_marker_source_index: Vec::new(),
    video_marker_clock_samples: Vec::new(),
    independent_sync: None,
    decoder_clock: DecoderClocks::default(),
  };
  let mut video_markers = Vec::new();
  let mut source_progress = SourceProgress::default();
  let mut previous_pts = None;
  let mut video_clock = DecoderClock::default();
  let mut visual = Onsets::default();
  let mut video_end = false;
  for _ in 0..2_000_000 {
    if Instant::now() > deadline {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "decode",
        "Offline decode exceeded duration budget",
      ));
    }
    let (end, changed, sample) = next(&video, stream, true)?;
    if changed {
      let renegotiated = geometry(&video, stream, markers)?;
      // Coded padding may change; the picture a viewer sees may not.
      if renegotiated.display != (video_width, video_height) {
        return Err(EngineError::protocol(format!(
          "Decoded picture changed from {video_width}x{video_height} to {}x{} mid-stream",
          renegotiated.display.0, renegotiated.display.1
        )));
      }
      (width, height, stride) = (renegotiated.width, renegotiated.height, renegotiated.stride);
    }
    if let Some(sample) = sample {
      // SAFETY: decoder provides sample time on the negotiated video stream.
      let decoder = unsafe { sample.GetSampleTime() }
        .map_err(|e| EngineError::native("decoded video time", e))?;
      let pts = timeline.video(report.video_frames)?;
      video_clock.observe(decoder, pts);
      let prior = previous_pts;
      if previous_pts.is_some_and(|p| pts <= p) {
        return Err(EngineError::protocol(
          "Container video times are not increasing",
        ));
      }
      if let Some(previous) = previous_pts {
        report.maximum_video_timestamp_gap_ms = report
          .maximum_video_timestamp_gap_ms
          .max((pts - previous) as f64 / 10000.0);
      }
      previous_pts = Some(pts);
      let bytes = mux::sample_bytes(&sample)?;
      if bytes.len()
        < stride
          .checked_mul(height)
          .ok_or_else(|| EngineError::protocol("Decoded size overflow"))?
      {
        return Err(EngineError::protocol("Truncated decoded NV12 plane"));
      }
      report.video_frames += 1;
      if markers {
        let index = read_code(&bytes, stride, 0)?;
        source_progress.observe(index, pts)?;
        report.numbered_frames_checked += 1;
        let current = bytes[(height / 2) * stride + width / 2] > 130;
        if visual.onset(current) {
          if video_markers.len() >= 16384 {
            return Err(EngineError::protocol("Too many visual markers"));
          }
          video_markers.push(pts as f64 / 10000.0);
          report
            .video_marker_previous_ms
            .push(prior.map(|value| value as f64 / 10000.0));
          report.video_marker_source_index.push(index);
          report.video_marker_clock_samples.push(if height >= 32 {
            Some(read_code(&bytes, stride, 16)?)
          } else {
            None
          });
        }
      }
    }
    if end {
      video_end = true;
      break;
    }
  }
  if report.video_frames == 0 || !video_end {
    return Err(EngineError::protocol("Decoder produced no video"));
  }
  validate_decoded_video_count(&structure, report.video_frames).map_err(|mut error| {
    // Where the decoder stopped separates a truncated stream from a decoder that ended early.
    error.reason.push_str(&format!(
      "; last decoded video time {}",
      previous_pts.map_or("none".to_owned(), |pts| format!(
        "{:.3} ms",
        pts as f64 / 10_000.0
      ))
    ));
    error
  })?;
  report.repeated_source_frames = source_progress.repeated;
  report.longest_source_hold_ms = source_progress.longest_hold as f64 / 10000.0;
  report.decoder_clock.video_reversals = video_clock.reversals;
  report.decoder_clock.video_maximum_offset_ms = video_clock.maximum_offset_ms();
  let mut audio_markers = Vec::new();
  if structure.tracks == 2 {
    let audio = reader(path)?;
    let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    // SAFETY: select only audio and decode to the fixed stereo PCM format, without a playback sink.
    unsafe {
      audio
        .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
        .map_err(|e| EngineError::native("audio decoder selection", e))?;
      audio
        .SetStreamSelection(stream, true)
        .map_err(|e| EngineError::native("audio decoder stream", e))?;
      audio
        .SetCurrentMediaType(stream, None, &mux::audio_type(false)?)
        .map_err(|e| EngineError::native("PCM decoder output", e))?;
    }
    let mut tone = Onsets::after_quiet(2_400);
    let mut audio_clock = DecoderClock::default();
    let mut audio_end = false;
    for _ in 0..2_000_000 {
      if Instant::now() > deadline {
        return Err(EngineError::new(
          ErrorKind::Timeout,
          "audio decode",
          "Offline decode exceeded duration budget",
        ));
      }
      let (end, _, sample) = next(&audio, stream, false)?;
      if let Some(sample) = sample {
        // SAFETY: audio sample is owned by this decode iteration.
        let decoder = unsafe { sample.GetSampleTime() }
          .map_err(|e| EngineError::native("decoded audio time", e))?;
        let (first, _) = timeline.audio_ms(report.audio_frames)?;
        audio_clock.observe(decoder, (first * 10_000.0).round() as i64);
        let bytes = mux::sample_bytes(&sample)?;
        if bytes.len() % 4 != 0 {
          return Err(EngineError::protocol("Decoded PCM is not stereo aligned"));
        }
        let frames = (bytes.len() / 4) as u64;
        let held = timeline.audio_frames();
        report.decoder_clock.audio_frames_beyond_container += (report.audio_frames + frames)
          .saturating_sub(held)
          - report.audio_frames.saturating_sub(held);
        for (i, frame) in bytes.as_chunks::<4>().0.iter().enumerate() {
          let value = i16::from_le_bytes([frame[0], frame[1]]).unsigned_abs();
          if tone.onset(value > 3000) && markers {
            let (at, _) = timeline.audio_ms(report.audio_frames + i as u64)?;
            if audio_markers.len() >= 16384 {
              return Err(EngineError::protocol("Too many audio markers"));
            }
            audio_markers.push(at);
          }
        }
        report.audio_frames += (bytes.len() / 4) as u64;
      }
      if end {
        audio_end = true;
        break;
      }
    }
    if report.audio_frames == 0 || !audio_end {
      return Err(EngineError::protocol("Decoder produced no audio"));
    }
    report.decoder_clock.audio_reversals = audio_clock.reversals;
    report.decoder_clock.audio_maximum_offset_ms = audio_clock.maximum_offset_ms();
  }
  if markers && structure.tracks == 2 {
    report.video_marker_times_ms = video_markers.clone();
    report.audio_marker_times_ms = audio_markers.clone();
    let differences = match marker_offsets(&video_markers, &audio_markers) {
      Ok(differences) => differences,
      Err(error) => {
        report.sync_failure = Some(error.to_string());
        structure.decoded = true;
        report.structure = structure;
        return Ok(report);
      },
    };
    report.marker_pairs = differences.len();
    if differences.len() < 2 || differences.len() != video_markers.len() {
      return Err(EngineError::protocol(format!(
        "Missing synchronization markers: video={}, audio={}, paired={}",
        video_markers.len(),
        audio_markers.len(),
        differences.len()
      )));
    }
    let maximum = differences.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let minimum = differences.iter().fold(f64::INFINITY, |m, v| m.min(*v));
    let highest = differences.iter().fold(f64::NEG_INFINITY, |m, v| m.max(*v));
    report.maximum_sync_ms = Some(maximum);
    report.drift_ms = Some(highest - minimum);
    report.accepted_sync = maximum <= MAX_OFFSET_MS && highest - minimum <= MAX_VARIATION_MS;
    if !report.accepted_sync {
      report.sync_failure = Some(format!(
        "Decoded marker maximum offset {maximum:.3} ms and variation {:.3} ms exceed acceptance limits",
        highest - minimum
      ));
    }
    report.sync_offsets_ms = differences;
    report.video_marker_times_ms = video_markers;
    report.audio_marker_times_ms = audio_markers;
  }
  structure.decoded = true;
  report.structure = structure;
  Ok(report)
}

/// Decode 32 luma cells of 16 px starting at `top`, sampled at each cell centre.
fn read_code(luma: &[u8], stride: usize, top: usize) -> Result<u32> {
  let row = (top + 8)
    .checked_mul(stride)
    .ok_or_else(|| EngineError::protocol("Decoded code row overflow"))?;
  let mut value = 0u32;
  for bit in 0..32 {
    let sample = *luma
      .get(row + bit * 16 + 8)
      .ok_or_else(|| EngineError::protocol("Decoded frame lacks its binary code row"))?;
    if sample > 128 {
      value |= 1 << bit;
    }
  }
  Ok(value)
}
fn validate_decoded_video_count(structure: &Inspection, decoded: u64) -> Result<()> {
  let video: Vec<_> = structure
    .track_media
    .iter()
    .filter(|track| {
      track
        .handler
        .as_ref()
        .is_some_and(|handler| handler == b"vide")
    })
    .collect();
  if video.len() != 1 || video[0].samples != decoded {
    return Err(EngineError::protocol(format!(
      "Decoded {decoded} video frames but container has video sample counts {:?}",
      video.iter().map(|track| track.samples).collect::<Vec<_>>()
    )));
  }
  Ok(())
}

#[derive(Default)]
struct SourceProgress {
  previous: Option<(u32, i64)>,
  run_start: i64,
  repeated: u64,
  longest_hold: i64,
}
impl SourceProgress {
  fn observe(&mut self, index: u32, pts: i64) -> Result<()> {
    if let Some((previous, previous_pts)) = self.previous {
      if index < previous || pts <= previous_pts {
        return Err(EngineError::protocol(format!(
          "Numbered video markers reversed: source {previous} at {previous_pts}, source {index} at {pts} (100 ns)"
        )));
      }
      self.longest_hold = self.longest_hold.max(pts - self.run_start);
      if index == previous {
        self.repeated += 1;
      } else {
        self.run_start = pts;
      }
    } else {
      self.run_start = pts;
    }
    self.previous = Some((index, pts));
    Ok(())
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn decode_completion_cannot_hide_a_silently_discarded_video_sample() {
    let structure = Inspection {
      track_media: vec![crate::recovery::TrackMedia {
        handler: Some(*b"vide"),
        samples: 10,
        ..Default::default()
      }],
      ..Default::default()
    };
    assert!(validate_decoded_video_count(&structure, 10).is_ok());
    assert!(validate_decoded_video_count(&structure, 9).is_err());
    assert!(validate_decoded_video_count(&structure, 11).is_err());
    assert!(validate_decoded_video_count(&Inspection::default(), 0).is_err());
  }
  fn timeline_structure() -> Inspection {
    Inspection {
      track_media: vec![
        crate::recovery::TrackMedia {
          handler: Some(*b"vide"),
          samples: 4,
          timescale: 60_000,
          // One lost slot between the third and fourth frames.
          sample_starts: vec![0, 1000, 2000, 4000],
          ..Default::default()
        },
        crate::recovery::TrackMedia {
          handler: Some(*b"soun"),
          samples: 3,
          timescale: 48_000,
          sample_starts: vec![0, 1024, 2048],
          ..Default::default()
        },
      ],
      ..Default::default()
    }
  }
  #[test]
  fn decoded_output_is_dated_by_its_container_position_not_the_decoder_clock() {
    let timeline = Timeline::new(&timeline_structure()).expect("timeline");
    assert_eq!(timeline.video(0).expect("first"), 0);
    // A container gap stays visible to the gap check.
    assert_eq!(
      timeline.video(3).expect("after the gap") - timeline.video(2).expect("before"),
      time::scale(2000, time::SECOND, 60_000).expect("two slots") as i64
    );
    assert!(timeline.video(4).is_err(), "more frames than the container");
    assert_eq!(timeline.audio_frames(), 3 * AAC_FRAMES);
    assert_eq!(timeline.audio_ms(0).expect("start"), (0.0, false));
    assert_eq!(timeline.audio_ms(1536).expect("inside"), (32.0, false));
    // Decoded PCM past the last packet keeps its spacing and is flagged for the report.
    assert_eq!(timeline.audio_ms(3072).expect("beyond"), (64.0, true));
    // Regression (CI, Windows Server 2025): the decoder dated audio 100 ms late and video went
    // backwards on exact files. Its clock is measured, never used to date markers.
    let mut clock = DecoderClock::default();
    for (decoder, container) in [(0, 0), (166_667, 166_667), (166_000, 333_333)] {
      clock.observe(decoder, container);
    }
    assert_eq!(clock.reversals, 1);
    assert_eq!(clock.maximum_offset_ms(), 16.7333);
    let mut late = DecoderClock::default();
    late.observe(1_000_000, 0);
    assert_eq!((late.reversals, late.maximum_offset_ms()), (0, 100.0));
  }
  #[test]
  fn a_verifier_without_a_complete_container_timeline_refuses_to_guess() {
    let mut plain = timeline_structure();
    // `recovery::inspect` keeps no starts; dating by it would silently invent times.
    plain.track_media[0].sample_starts.clear();
    assert!(Timeline::new(&plain).is_err());
    let mut unscaled = timeline_structure();
    unscaled.track_media[1].timescale = 0;
    assert!(Timeline::new(&unscaled).is_err());
    let mut silent = timeline_structure();
    silent.track_media.truncate(1);
    let silent = Timeline::new(&silent).expect("video only");
    assert!(silent.audio_ms(0).is_err());
    assert!(Timeline::new(&Inspection::default()).is_err());
  }
  #[test]
  fn source_progress_measures_stale_output_without_treating_pause_jumps_as_loss() {
    let mut progress = SourceProgress::default();
    progress.observe(10, 0).expect("initial");
    progress.observe(10, 166667).expect("held");
    progress.observe(10, 333333).expect("held");
    progress
      .observe(100, 500000)
      .expect("wall clock jumps across pause");
    assert_eq!(progress.repeated, 2);
    assert_eq!(progress.longest_hold, 500000);
    assert!(progress.observe(99, 666667).is_err());
    assert!(progress.observe(101, 500000).is_err());
  }
  use super::super::desktop_fixture::{FixtureReport, MarkerObservation};
  /// Independent fixture: second `s` paints its pulse `late_ms` after the tone's render onset.
  fn fixture(late_ms: &[f64]) -> FixtureReport {
    FixtureReport {
      clock: "test".into(),
      presentation_limit: "test".into(),
      elapsed_seconds: late_ms.len() as f64,
      rendered_samples: 0,
      frames_painted: 0,
      marker_submission_lateness: Default::default(),
      markers: late_ms
        .iter()
        .enumerate()
        .map(|(second, late)| MarkerObservation {
          second: second as u64,
          render_sample_position: second as u64 * 48000 + 24000 + (late * 48.0).round() as u64,
          paint_submission_qpc_100ns: 0,
        })
        .collect(),
      clock_strip: None,
      display_refresh_hz: None,
      composition_period_100ns: None,
    }
  }
  /// Simulate capture of the fixture: the tone is recorded `error_ms[s]` (audio minus video
  /// error) relative to the visual onset; frames are sampled every `spacing_ms` at `phase_ms[s]`.
  fn observe(error_ms: &[f64], late_ms: &[f64], phase_ms: &[f64], spacing_ms: f64) -> Verification {
    let mut report = Verification {
      video_width: 512,
      video_height: 64,
      structure: Inspection::default(),
      video_frames: 0,
      audio_frames: 0,
      numbered_frames_checked: 0,
      repeated_source_frames: 0,
      longest_source_hold_ms: 0.0,
      maximum_video_timestamp_gap_ms: 0.0,
      marker_pairs: 0,
      maximum_sync_ms: None,
      drift_ms: None,
      markers_checked: true,
      accepted_sync: false,
      sync_failure: None,
      sync_offsets_ms: Vec::new(),
      video_marker_times_ms: Vec::new(),
      audio_marker_times_ms: Vec::new(),
      video_marker_previous_ms: Vec::new(),
      video_marker_source_index: Vec::new(),
      video_marker_clock_samples: Vec::new(),
      independent_sync: None,
      decoder_clock: DecoderClocks::default(),
    };
    for (second, error) in error_ms.iter().enumerate() {
      let tone = second as f64 * 1000.0 + 500.0;
      let visual = tone + late_ms[second];
      // First sample at or after the visual onset on this stretch's sampling grid.
      let steps = ((visual - phase_ms[second]) / spacing_ms).ceil();
      let onset = phase_ms[second] + steps * spacing_ms;
      report.video_marker_times_ms.push(onset);
      report
        .video_marker_previous_ms
        .push(Some(onset - spacing_ms));
      report
        .video_marker_source_index
        .push(second as u32 * 60 + 30);
      // Recorder error shifts the audio media position relative to its correct place.
      report.audio_marker_times_ms.push(tone + error);
    }
    report
  }
  const FRAME: f64 = 1000.0 / 60.0;
  #[test]
  fn frame_quantized_markers_are_unresolved_rather_than_failed_for_a_correct_recorder() {
    // Retained real runs showed constant offsets between pauses and a ~17 ms range overall:
    // a correct recorder sampled at a new phase after each pause looks exactly like that.
    let phases = [0.3, 9.1, 15.9, 4.4, 12.0];
    let late = [1.0, 0.5, 1.3, 0.0, 0.8];
    let report = observe(&[0.0; 5], &late, &phases, FRAME);
    let point: Vec<f64> =
      marker_offsets(&report.video_marker_times_ms, &report.audio_marker_times_ms).expect("paired");
    let range = point.iter().cloned().fold(f64::MIN, f64::max)
      - point.iter().cloned().fold(f64::MAX, f64::min);
    assert!(
      range > MAX_VARIATION_MS,
      "the point estimate falsely fails ({range} ms)"
    );
    let sync = evaluate_independent(&report, &fixture(&late)).expect("evaluation");
    assert_eq!(sync.verdict, SyncVerdict::Unresolved);
    assert!(sync.variation_lower_ms <= MAX_VARIATION_MS);
    assert!(
      sync
        .resolution_ms
        .is_some_and(|value| (value - FRAME).abs() < 1e-6)
    );
    for interval in &sync.intervals_ms {
      let upper = interval.upper_ms.expect("bounded");
      assert!(
        interval.lower_ms <= 1e-6 && upper >= -1e-6,
        "truth is inside every interval"
      );
    }
    let mut verified = report.clone();
    apply_independent(&mut verified, sync);
    assert!(!verified.accepted_sync, "unresolved is never acceptance");
    assert!(
      verified
        .sync_failure
        .expect("explained")
        .contains("unresolved")
    );
    // The same correct recorder bracketed by 1 ms observations is accepted.
    let fine = observe(&[0.0; 5], &late, &phases, 1.0);
    assert_eq!(
      evaluate_independent(&fine, &fixture(&late))
        .expect("fine")
        .verdict,
      SyncVerdict::Accepted
    );
  }
  #[test]
  fn independent_negative_controls_fail_delay_and_drift_even_at_frame_resolution() {
    let phases = [2.0, 7.0, 11.0, 3.0, 14.0];
    let late = [0.5; 5];
    for (name, error) in [
      ("constant 45 ms delay", [45.0; 5]),
      ("constant 45 ms lead", [-45.0; 5]),
      // Frame-wide brackets only guarantee detecting variation above 2 frames + 5 ms.
      ("45 ms drift", [0.0, 11.25, 22.5, 33.75, 45.0]),
    ] {
      let sync =
        evaluate_independent(&observe(&error, &late, &phases, FRAME), &fixture(&late)).expect(name);
      assert_eq!(sync.verdict, SyncVerdict::Failed, "{name}");
    }
    // Small drift is invisible at frame resolution but detected with fine bracketing.
    let drift = [0.0, 2.0, 4.0, 6.0, 8.0];
    assert_eq!(
      evaluate_independent(&observe(&drift, &late, &phases, FRAME), &fixture(&late))
        .expect("coarse")
        .verdict,
      SyncVerdict::Unresolved
    );
    assert_eq!(
      evaluate_independent(&observe(&drift, &late, &phases, 0.5), &fixture(&late))
        .expect("fine")
        .verdict,
      SyncVerdict::Failed
    );
  }
  #[test]
  fn fixture_paint_lateness_is_removed_per_marker_and_unmatched_data_is_refused() {
    let phases = [0.0; 4];
    let late = [0.0, 6.0, 0.0, 6.0];
    let report = observe(&[0.0; 4], &late, &phases, 0.25);
    assert_eq!(
      evaluate_independent(&report, &fixture(&late))
        .expect("corrected")
        .verdict,
      SyncVerdict::Accepted
    );
    // Ignoring the logged lateness would turn the fixture's own paint delay into a failure.
    assert_eq!(
      evaluate_independent(&report, &fixture(&[0.0; 4]))
        .expect("uncorrected")
        .verdict,
      SyncVerdict::Failed
    );
    let mut outside = report.clone();
    outside.video_marker_source_index[1] = 60 + 10;
    assert!(evaluate_independent(&outside, &fixture(&late)).is_err());
    let mut missing = report.clone();
    missing.video_marker_source_index[3] = 60 * 9 + 30;
    assert!(
      evaluate_independent(&missing, &fixture(&late))
        .expect_err("no fixture second 9")
        .reason
        .contains("no fixture paint observation")
    );
    let mut legacy = report.clone();
    legacy.video_marker_previous_ms.clear();
    assert!(evaluate_independent(&legacy, &fixture(&late)).is_err());
    let mut first_frame = report.clone();
    first_frame.video_marker_previous_ms[0] = None;
    let sync = evaluate_independent(&first_frame, &fixture(&late)).expect("unbounded onset");
    assert_eq!(sync.verdict, SyncVerdict::Unresolved);
    assert!(sync.resolution_ms.is_none() && sync.variation_upper_ms.is_none());
    let mut reversed = report;
    reversed.video_marker_previous_ms[2] = Some(reversed.video_marker_times_ms[2]);
    assert!(evaluate_independent(&reversed, &fixture(&late)).is_err());
  }
  /// Clock-strip fixture: a pulse frame shows content painted `stale_ms` before its composition
  /// and appears `lag_ms[s]` later in video media time (compositor, scan-out and capture phase).
  fn observe_clock(
    error_ms: &[f64],
    lag_ms: &[f64],
    late_ms: f64,
    stale_ms: f64,
  ) -> (Verification, FixtureReport) {
    let late = vec![late_ms; error_ms.len()];
    let mut report = observe(error_ms, &late, &vec![0.0; error_ms.len()], FRAME);
    let mut fixture = fixture(&late);
    fixture.clock_strip = Some(super::super::desktop_fixture::ClockStrip {
      unit: super::super::desktop_fixture::ClockUnit::RenderSamples48k,
      paints: 1,
      maximum_gap_samples: 48,
      maximum_lead_samples: 0,
      maximum_lag_samples: 0,
      pulse_windows: Vec::new(),
    });
    report.video_marker_clock_samples.clear();
    for (second, lag) in lag_ms.iter().enumerate().take(error_ms.len()) {
      let tone = second as f64 * 1000.0 + 500.0;
      let snapshot = tone + late_ms + stale_ms;
      report.video_marker_times_ms[second] = snapshot + lag;
      let clock = ((snapshot - stale_ms) * 48.0).round() as u32;
      report.video_marker_clock_samples.push(Some(clock));
      report.video_marker_source_index[second] = clock / 800;
    }
    (report, fixture)
  }
  #[test]
  fn render_clock_strip_resolves_what_frame_brackets_cannot() {
    let errors = [0.0; 4];
    let (constant, fixture) = observe_clock(&errors, &[8.0; 4], 0.7, 0.6);
    let sync = evaluate_independent(&constant, &fixture).expect("constant lag");
    assert_eq!(sync.method, "fixture_render_clock_strip");
    assert_eq!(sync.verdict, SyncVerdict::Accepted, "{sync:?}");
    assert!(
      sync
        .resolution_ms
        .is_some_and(|value| (value - 1.0).abs() < 1e-9)
    );
    // Capture phase re-sampled after each pause is real lag in the recording; the strip now
    // measures it instead of hiding it inside a frame-wide bracket.
    let (phase, fixture) = observe_clock(&errors, &[2.0, 9.0, 15.0, 4.0], 0.7, 0.6);
    let sync = evaluate_independent(&phase, &fixture).expect("phase");
    assert_eq!(sync.verdict, SyncVerdict::Failed);
    assert!(sync.variation_lower_ms > MAX_VARIATION_MS);
    for (name, error) in [
      ("40 ms delay", [40.0; 4]),
      ("8 ms drift", [0.0, 2.5, 5.0, 8.0]),
    ] {
      let (report, fixture) = observe_clock(&error, &[3.0; 4], 0.7, 0.6);
      assert_eq!(
        evaluate_independent(&report, &fixture).expect(name).verdict,
        SyncVerdict::Failed,
        "{name}"
      );
    }
  }
  #[test]
  fn an_interpolated_strip_widens_every_interval_by_its_measured_lead_and_lag() {
    let (report, mut fixture) = observe_clock(&[0.0; 3], &[3.0; 3], 0.7, 0.6);
    let exact = evaluate_independent(&report, &fixture).expect("exact strip");
    if let Some(strip) = fixture.clock_strip.as_mut() {
      (strip.maximum_lead_samples, strip.maximum_lag_samples) = (96, 48);
    }
    let widened = evaluate_independent(&report, &fixture).expect("interpolated strip");
    for (before, after) in exact.intervals_ms.iter().zip(&widened.intervals_ms) {
      assert!(
        (before.lower_ms - 2.0 - after.lower_ms).abs() < 1e-9,
        "lead 2 ms"
      );
      let upper_before = before.upper_ms.expect("strip intervals are bounded");
      let upper_after = after.upper_ms.expect("strip intervals are bounded");
      assert!((upper_before + 1.0 - upper_after).abs() < 1e-9, "lag 1 ms");
    }
    assert!(widened.resolution_ms > exact.resolution_ms);
  }
  #[test]
  fn a_stall_away_from_a_pulse_does_not_widen_that_markers_interval() {
    use super::super::desktop_fixture::{PulseWindow, pulse_window_second};
    let (report, mut fixture) = observe_clock(&[0.0; 3], &[3.0; 3], 0.7, 0.6);
    let exact = evaluate_independent(&report, &fixture).expect("exact strip");
    let seconds: Vec<u64> = report
      .video_marker_clock_samples
      .iter()
      .map(|clock| clock.and_then(|c| pulse_window_second(u64::from(c))))
      .collect::<Option<_>>()
      .expect("pulse clocks lie in their windows");
    if let Some(strip) = fixture.clock_strip.as_mut() {
      // A 15 ms stall somewhere in the run; around each pulse the loop stayed at 1 ms.
      (strip.maximum_gap_samples, strip.maximum_lag_samples) = (728, 632);
      strip.pulse_windows = seconds
        .iter()
        .map(|&second| PulseWindow {
          second,
          maximum_gap_samples: 48,
          maximum_lead_samples: 0,
          maximum_lag_samples: 0,
        })
        .collect();
    }
    let local = evaluate_independent(&report, &fixture).expect("windowed strip");
    assert_eq!(local.intervals_ms, exact.intervals_ms);
    // One window caught the stall: only that marker widens.
    if let Some(window) = fixture
      .clock_strip
      .as_mut()
      .and_then(|strip| strip.pulse_windows.get_mut(1))
    {
      (window.maximum_gap_samples, window.maximum_lag_samples) = (728, 632);
    }
    let stalled = evaluate_independent(&report, &fixture).expect("one stalled window");
    assert_eq!(stalled.intervals_ms[0], exact.intervals_ms[0]);
    assert_eq!(stalled.intervals_ms[2], exact.intervals_ms[2]);
    assert!(stalled.intervals_ms[1].upper_ms > exact.intervals_ms[1].upper_ms);
    // Without windows the run's maxima widen every marker.
    if let Some(strip) = fixture.clock_strip.as_mut() {
      strip.pulse_windows.clear();
    }
    let global = evaluate_independent(&report, &fixture).expect("unwindowed strip");
    assert!(
      global
        .intervals_ms
        .iter()
        .zip(&exact.intervals_ms)
        .all(|(wide, tight)| wide.upper_ms > tight.upper_ms)
    );
  }
  #[test]
  fn the_strip_splits_a_variation_into_audio_placement_and_video_age() {
    let close =
      |value: Option<f64>, expected: f64| value.is_some_and(|v| (v - expected).abs() < 0.05);
    // Capture-phase lag moves only the video frames' content age (measured: a
    // 144 Hz display sampled on a 60 FPS grid, with audio placed within 0.02 ms).
    let (phase, strip) = observe_clock(&[0.0; 4], &[2.0, 9.0, 15.0, 4.0], 0.7, 0.6);
    let sync = evaluate_independent(&phase, &strip).expect("phase");
    assert!(close(sync.audio_placement_spread_ms, 0.0), "{sync:?}");
    assert!(close(sync.video_age_spread_ms, 13.0), "{sync:?}");
    // Audio drift moves only the audio track's placement.
    let (drift, strip) = observe_clock(&[0.0, 2.5, 5.0, 8.0], &[3.0; 4], 0.7, 0.6);
    let sync = evaluate_independent(&drift, &strip).expect("drift");
    assert!(close(sync.audio_placement_spread_ms, 8.0), "{sync:?}");
    assert!(close(sync.video_age_spread_ms, 0.0), "{sync:?}");
    // Frame brackets cannot split the offset.
    let late = [1.0; 3];
    let bracketed = evaluate_independent(
      &observe(&[0.0; 3], &late, &[0.0; 3], FRAME),
      &fixture(&late),
    )
    .expect("brackets");
    assert_eq!(
      (
        bracketed.audio_placement_spread_ms,
        bracketed.video_age_spread_ms
      ),
      (None, None)
    );
  }
  #[test]
  fn a_marker_already_on_when_the_recording_starts_is_not_an_onset() {
    // Visual: a pulse lit in the first frame is not an onset; the next one after a dark frame is.
    let mut visual = Onsets::default();
    let frames = [true, true, false, false, true, true, false, true];
    let onsets: Vec<usize> = frames
      .iter()
      .enumerate()
      .filter_map(|(i, on)| visual.onset(*on).then_some(i))
      .collect();
    assert_eq!(onsets, [4, 7]);
    // Audio: 8.6 ms of start-up silence then a tone already playing is not an onset; a tone after
    // real silence is, once, despite its half-cycle dips below the threshold.
    let mut tone = Onsets::after_quiet(2_400);
    let tone_samples = |count: usize| (0..count).map(|i| i % 48 > 2);
    let stream: Vec<bool> = std::iter::repeat_n(false, 414)
      .chain(tone_samples(4_800))
      .chain(std::iter::repeat_n(false, 43_000))
      .chain(tone_samples(4_800))
      .collect();
    let onsets: Vec<usize> = stream
      .iter()
      .enumerate()
      .filter_map(|(i, on)| tone.onset(*on).then_some(i))
      .collect();
    assert_eq!(onsets, [414 + 4_800 + 43_000 + 3]);
  }
  #[test]
  fn the_variation_limit_is_one_refresh_only_on_displays_the_frame_grid_cannot_divide() {
    let hz = |rate: u64| Some(crate::time::SECOND / rate);
    assert_eq!(variation_limit_ms(Some(60), hz(60)), MAX_VARIATION_MS);
    assert_eq!(variation_limit_ms(Some(60), hz(120)), MAX_VARIATION_MS);
    assert!((variation_limit_ms(Some(60), hz(144)) - 6.9444).abs() < 1e-3);
    assert!((variation_limit_ms(Some(60), hz(165)) - 6.0606).abs() < 1e-3);
    // Never looser than 5 ms, and strict whenever either rate is unknown.
    assert_eq!(variation_limit_ms(Some(60), hz(300)), MAX_VARIATION_MS);
    assert_eq!(variation_limit_ms(None, hz(144)), MAX_VARIATION_MS);
    assert_eq!(variation_limit_ms(Some(60), None), MAX_VARIATION_MS);
    // The composition period wins over the display's mode: a 60 Hz indirect display composed
    // at 144 Hz is judged by 144 Hz.
    let (_, mut composed) = observe_clock(&[0.0; 2], &[1.0; 2], 0.7, 0.6);
    composed.display_refresh_hz = Some(60);
    composed.composition_period_100ns = hz(144);
    assert_eq!(fixture_period(&composed), hz(144));
    composed.composition_period_100ns = None;
    assert_eq!(
      fixture_period(&composed),
      hz(60),
      "older reports use the mode"
    );
    // The same 5.5 ms (±0.5 ms) of variation cannot pass on a 60 Hz fixture display and passes
    // within one 144 Hz refresh.
    let (mut report, mut fixture) = observe_clock(&[0.0; 4], &[1.0, 6.5, 1.0, 6.5], 0.7, 0.6);
    report.video_frames = 600;
    report.structure.duration_100ns = 100_000_000;
    fixture.display_refresh_hz = Some(60);
    let strict = evaluate_independent(&report, &fixture).expect("60 Hz");
    assert_eq!(strict.variation_limit_ms, MAX_VARIATION_MS);
    assert_ne!(strict.verdict, SyncVerdict::Accepted, "{strict:?}");
    fixture.display_refresh_hz = Some(144);
    let hybrid = evaluate_independent(&report, &fixture).expect("144 Hz");
    assert_eq!(hybrid.verdict, SyncVerdict::Accepted, "{hybrid:?}");
    // Without a decodable frame rate the strict limit applies.
    report.structure.duration_100ns = 0;
    let unknown = evaluate_independent(&report, &fixture).expect("unknown fps");
    assert_eq!(unknown.variation_limit_ms, MAX_VARIATION_MS);
    assert_ne!(unknown.verdict, SyncVerdict::Accepted);
  }
  #[test]
  fn a_frame_composed_mid_paint_is_dated_by_its_newer_clock_but_never_further() {
    let (report, fixture) = observe_clock(&[0.0; 3], &[5.0; 3], 0.7, 0.6);
    let baseline = evaluate_independent(&report, &fixture).expect("baseline");
    // Captured one slot after the pulse paint: the strip shows the next paint's clock and the
    // decoded PTS is correspondingly later, so the verdict is unchanged.
    let mut torn = report.clone();
    torn.video_marker_clock_samples[1] = torn.video_marker_clock_samples[1].map(|c| c + 800);
    torn.video_marker_times_ms[1] += 800.0 / 48.0;
    let sync = evaluate_independent(&torn, &fixture).expect("one-slot lead");
    assert_eq!(sync.verdict, baseline.verdict);
    // Two slots is not one torn paint; the decode is inconsistent and refused.
    let mut far = report;
    far.video_marker_clock_samples[1] = far.video_marker_clock_samples[1].map(|c| c + 1600);
    far.video_marker_times_ms[1] += 1600.0 / 48.0;
    assert!(
      evaluate_independent(&far, &fixture)
        .expect_err("two-slot lead")
        .reason
        .contains("disagrees with source frame")
    );
  }
  #[test]
  fn render_clock_strip_inconsistencies_are_refused_instead_of_falling_back() {
    let (report, fixture) = observe_clock(&[0.0; 3], &[5.0; 3], 0.7, 0.6);
    let mut disagreeing = report.clone();
    disagreeing.video_marker_source_index[1] += 1;
    assert!(
      evaluate_independent(&disagreeing, &fixture)
        .expect_err("index mismatch")
        .reason
        .contains("disagrees with source frame")
    );
    let mut early = report.clone();
    early.video_marker_clock_samples[2] = Some(2 * 48000 + 24000);
    assert!(
      evaluate_independent(&early, &fixture)
        .expect_err("dated before its paint")
        .reason
        .contains("before its own paint")
    );
    let mut missing = report.clone();
    missing.video_marker_clock_samples[0] = None;
    assert!(evaluate_independent(&missing, &fixture).is_err());
    let mut legacy = report;
    legacy.video_marker_clock_samples.clear();
    assert!(
      evaluate_independent(&legacy, &fixture).is_err(),
      "a strip fixture never silently degrades to frame brackets"
    );
  }
  #[test]
  fn binary_code_rows_decode_from_cell_centres_and_reject_short_frames() {
    let stride = 520;
    let mut luma = vec![0u8; stride * 32];
    for (top, value) in [(0usize, 0x8000_0001u32), (16, 123_456_789)] {
      for bit in 0..32 {
        if (value >> bit) & 1 != 0 {
          for y in top..top + 16 {
            luma[y * stride + bit * 16..y * stride + bit * 16 + 16].fill(235);
          }
        }
      }
      assert_eq!(read_code(&luma, stride, top).expect("decoded row"), value);
    }
    assert!(read_code(&luma[..stride * 20], stride, 16).is_err());
  }
  #[test]
  fn negative_controls_detect_delay_drift_missing_and_reused_pairs() {
    let video = [500.0, 1500.0, 2500.0];
    assert!(marker_offsets(&video, &[500.0, 1500.0]).is_err());
    assert!(marker_offsets(&video, &[500.0, 500.0, 2500.0]).is_err());
    assert!(marker_offsets(&video, &[f64::NAN, 1500.0, 2500.0]).is_err());
    let delayed = marker_offsets(&video, &[540.0, 1540.0, 2540.0]).expect("paired delayed markers");
    assert!(delayed.iter().any(|v| v.abs() > 25.0));
    let drift = marker_offsets(&video, &[500.0, 1504.0, 2508.0]).expect("paired drifting markers");
    assert!(drift[2] - drift[0] > 5.0);
  }
}

#[cfg(test)]
mod decoded_buffer_tests {
  #![allow(clippy::expect_used)]
  use super::*;

  #[test]
  fn decoded_padded_pictures_copy_exactly_their_contiguous_bytes() {
    // 800-wide pictures decode into 2D buffers with an 832-byte pitch. Copying the buffer's
    // current length (579,072) out of its 556,800-byte contiguous lock read past the copy
    // (debug UB check in the native GIF desktop probe).
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("padded.mp4");
    super::super::mux::write_fixture(
      &path,
      super::super::fixtures::media_sized(1, 800, 450),
      800,
      450,
    );
    let (reader, stream) = video_reader(&path).expect("reader");
    let mut geometry = geometry(&reader, stream, false).expect("geometry");
    let mut frames = 0;
    loop {
      let (end, changed, sample) = next(&reader, stream, true).expect("sample");
      if changed {
        geometry = super::geometry(&reader, stream, false).expect("renegotiated");
      }
      if let Some(sample) = sample {
        let bytes = mux::sample_bytes(&sample).expect("bytes");
        assert_eq!(
          bytes.len(),
          geometry.stride * geometry.height * 3 / 2,
          "frame {frames}: stride {} rows {}",
          geometry.stride,
          geometry.height
        );
        frames += 1;
      }
      if end {
        break;
      }
    }
    assert_eq!(frames, 60);
  }
}
