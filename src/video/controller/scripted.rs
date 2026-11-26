//! A scripted worker for command and overlay tests, which exercise orchestration (HUD, controls,
//! notifications, publication), not media. It answers the recorder's controls in-process, claims
//! exactly the time it was recording, and writes a stand-in file that its inspector reads back,
//! so Stop runs the recorder's real output check and publication. Real capture, encoding and GIF
//! writing are covered by the engine's tests and `native_pipeline_tests` against a real worker.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use flinch_video_engine::gif::GifOptions;
use flinch_video_engine::recovery::{Inspection, TrackMedia};
use flinch_video_engine::types::{
  OutcomeKind, RecordingOutcome, SessionSnapshot, SessionSpec, SessionState,
};
use flinch_video_engine::{EngineError, ErrorKind};

use super::VideoRecorder;
use super::native::Host;

/// The stand-in file's first line; the rest is `frames media_100ns audio width height`.
const MAGIC: &str = "flinch-scripted-recording";

/// Holds a recorder wired to the scripted worker.
pub(crate) struct ScriptedSession;

impl ScriptedSession {
  pub(crate) fn new() -> (VideoRecorder, Self) {
    let mut recorder = VideoRecorder::new(crate::video::config::VideoConfig::default());
    recorder.scripted = Some(Hooks { inspect, write_gif });
    (recorder, Self)
  }

  /// Record two seconds before the next control, as the tests' timelines expect.
  pub(crate) fn finish_segment(&self, _recorder: &mut VideoRecorder) {
    std::thread::sleep(Duration::from_secs(2));
  }

  /// Seconds of video the worker claimed for a published recording.
  pub(crate) fn recorded_seconds(path: &Path) -> Result<f64, String> {
    read(path).map(|written| written.media_100ns as f64 / 10_000_000.0)
  }
}

pub(super) struct Hooks {
  pub(super) inspect: fn(&Path) -> Result<Inspection, EngineError>,
  pub(super) write_gif: fn(&Path, &Path, GifOptions) -> Result<(), String>,
}

impl Hooks {
  pub(super) fn launcher(&self) -> fn() -> Result<Box<dyn Host>, EngineError> {
    || Ok(Box::new(Worker::default()) as Box<dyn Host>)
  }
}

/// One display large enough for every test area.
pub(super) fn displays() -> Vec<crate::monitors::MonitorInfo> {
  vec![crate::monitors::MonitorInfo {
    id: "0,0,1920x1080".into(),
    name: "scripted".into(),
    x: 0,
    y: 0,
    width: 1920,
    height: 1080,
    is_primary: true,
    dpi_scale: 1.0,
    logical_x: 0,
    logical_y: 0,
    logical_width: 1920,
    logical_height: 1080,
  }]
}

#[derive(Default)]
struct Worker(Mutex<State>);
#[derive(Default)]
struct State {
  spec: Option<SessionSpec>,
  recorded: Duration,
  since: Option<Instant>,
}

impl Worker {
  fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, EngineError> {
    self
      .0
      .lock()
      .map_err(|_| EngineError::protocol("Scripted worker poisoned"))
  }
}

impl Host for Worker {
  fn prepare(&self, spec: SessionSpec) -> Result<PathBuf, EngineError> {
    let directory = spec
      .output
      .parent()
      .ok_or_else(|| EngineError::protocol("Scripted output has no folder"))?
      .to_path_buf();
    self.state()?.spec = Some(spec);
    Ok(directory)
  }
  fn start(&self) -> Result<(), EngineError> {
    self.state()?.since = Some(Instant::now());
    Ok(())
  }
  fn pause(&self) -> Result<(), EngineError> {
    let mut state = self.state()?;
    if let Some(since) = state.since.take() {
      state.recorded += since.elapsed();
    }
    Ok(())
  }
  fn resume(&self) -> Result<(), EngineError> {
    self.state()?.since = Some(Instant::now());
    Ok(())
  }
  fn stop(&self) -> Result<RecordingOutcome, EngineError> {
    let mut state = self.state()?;
    if let Some(since) = state.since.take() {
      state.recorded += since.elapsed();
    }
    let spec = state
      .spec
      .clone()
      .ok_or_else(|| EngineError::new(ErrorKind::InvalidState, "stop", "Not prepared"))?;
    let media_100ns = (state.recorded.as_nanos() / 100) as u64;
    let frames = (media_100ns * u64::from(spec.fps) / 10_000_000).max(1);
    let (width, height) = spec
      .region
      .map_or((1920, 1080), |region| (region.width, region.height));
    std::fs::write(
      &spec.output,
      format!(
        "{MAGIC}\n{frames} {media_100ns} {} {width} {height}",
        u8::from(spec.audio)
      ),
    )
    .map_err(|error| EngineError::io("scripted output", error))?;
    Ok(RecordingOutcome {
      thumbnail: None,
      kind: OutcomeKind::Completed,
      output: Some(spec.output),
      partial: None,
      error: None,
      snapshot: SessionSnapshot {
        state: SessionState::Completed,
        frames_new: frames,
        frames_encoded: frames,
        media_100ns,
        ..Default::default()
      },
    })
  }
  fn snapshot(&self) -> SessionSnapshot {
    SessionSnapshot {
      state: SessionState::Recording,
      ..Default::default()
    }
  }
  fn failure(&self) -> Option<EngineError> {
    None
  }
  fn terminal_outcome(&self) -> Option<RecordingOutcome> {
    None
  }
  fn take_diagnostics(&self) -> (Vec<String>, u64) {
    (Vec::new(), 0)
  }
}

struct Written {
  frames: u64,
  media_100ns: u64,
  audio: bool,
  width: u32,
  height: u32,
}

fn read(path: &Path) -> Result<Written, String> {
  let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
  let mut lines = text.lines();
  if lines.next() != Some(MAGIC) {
    return Err(format!("{} is not a scripted recording", path.display()));
  }
  let fields = lines
    .next()
    .unwrap_or_default()
    .split(' ')
    .map(str::parse::<u64>)
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| error.to_string())?;
  match fields[..] {
    [frames, media_100ns, audio, width, height] => Ok(Written {
      frames,
      media_100ns,
      audio: audio == 1,
      width: width as u32,
      height: height as u32,
    }),
    _ => Err("Malformed scripted recording".into()),
  }
}

/// The structure an engine MP4 with the written claims would have.
fn inspect(path: &Path) -> Result<Inspection, EngineError> {
  let written =
    read(path).map_err(|reason| EngineError::new(ErrorKind::Protocol, "inspect", reason))?;
  let mut tracks = vec![TrackMedia {
    id: 1,
    handler: Some(*b"vide"),
    codec: Some(*b"avc1"),
    samples: written.frames,
    start_100ns: Some(0),
    end_100ns: written.media_100ns,
    ..Default::default()
  }];
  if written.audio {
    tracks.push(TrackMedia {
      id: 2,
      handler: Some(*b"soun"),
      codec: Some(*b"mp4a"),
      samples: 1,
      start_100ns: Some(0),
      end_100ns: written.media_100ns,
      ..Default::default()
    });
  }
  Ok(Inspection {
    tracks: tracks.len(),
    track_media: tracks,
    ..Default::default()
  })
}

/// A real GIF with one plain frame per claimed video frame, at the recorded size.
fn write_gif(intermediate: &Path, destination: &Path, _: GifOptions) -> Result<(), String> {
  use image::codecs::gif::GifEncoder;
  let written = read(intermediate)?;
  let file = std::fs::File::create(destination).map_err(|error| error.to_string())?;
  let mut encoder = GifEncoder::new(std::io::BufWriter::new(file));
  let picture = image::RgbaImage::from_pixel(
    written.width,
    written.height,
    image::Rgba([32, 96, 160, 255]),
  );
  let frames =
    (0..written.frames).map(|_| Ok(image::Frame::new(picture.clone())) as image::ImageResult<_>);
  encoder
    .try_encode_frames(frames)
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_inspector_refuses_files_the_worker_did_not_write() {
    let folder = tempfile::tempdir().unwrap();
    let foreign = folder.path().join("foreign.mp4");
    std::fs::write(&foreign, b"not a recording").unwrap();
    assert!(inspect(&foreign).is_err());
    std::fs::write(&foreign, format!("{MAGIC}\n1 2")).unwrap();
    assert!(inspect(&foreign).is_err());
    assert!(
      write_gif(
        &foreign,
        &folder.path().join("out.gif"),
        crate::video::native::gif_options()
      )
      .is_err()
    );
  }
}
