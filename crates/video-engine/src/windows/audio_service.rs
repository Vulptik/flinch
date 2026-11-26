//! AAC, drift correction and PCM placement have an owner independent of the GPU thread.
use super::audio::{AacEncoder, AudioCapture, Endpoint};
use crate::watchdog::{Stage, Watchdog, Watched};
use crate::{
  EngineError, ErrorKind, Result,
  audio::{
    mix::{MuteTimeline, limit, to_i16},
    normalize::{AudioDiagnostics, Normalizer, REORDER_100NS},
  },
  time,
  types::EncodedPacket,
};
use std::{
  cell::Cell,
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError},
  },
  time::{Duration, Instant},
};

enum Control {
  Start,
  Map(u64, u64),
  Pause(u64),
  Finish(u64),
  /// The microphone is audible (or muted) from this media time.
  Microphone(bool, u64),
}
/// What a recording's one AAC track carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inputs {
  pub system: bool,
  /// A render endpoint id for system audio; `None` is the Windows default output.
  pub system_device: Option<String>,
  pub microphone: bool,
  /// A capture endpoint id; `None` is the Windows default microphone.
  pub microphone_device: Option<String>,
}
impl Inputs {
  pub fn of(spec: &crate::types::SessionSpec) -> Self {
    Self {
      system: spec.audio,
      system_device: spec.audio_device.clone(),
      microphone: spec.microphone,
      microphone_device: spec.microphone_device.clone(),
    }
  }
}
struct Request {
  control: Control,
  reply: SyncSender<Result<AudioDiagnostics>>,
}
/// One captured endpoint and its placement on the media timeline.
struct Source {
  capture: Option<AudioCapture>,
  normalizer: Normalizer,
}
impl Source {
  fn new(capture: Option<AudioCapture>) -> Self {
    Self {
      capture,
      normalizer: Normalizer::default(),
    }
  }
}
/// The microphone: a source plus when the user muted it, and whether it disappeared.
struct Microphone {
  source: Source,
  mutes: MuteTimeline,
  /// Set once the endpoint disappeared; the rest of the recording has no microphone.
  lost: Arc<AtomicBool>,
}
/// A microphone capture error that means the device went away (unplugged, a Bluetooth headset
/// switched off or changed mode), which the recording survives with silence.
fn microphone_lost(error: &EngineError) -> bool {
  error.kind == ErrorKind::DeviceLost
    || error.native_code == Some(windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED.0)
}
struct Pipeline {
  encoder: Watched<AacEncoder>,
  /// System audio; `None` when only the microphone is recorded.
  system: Option<Source>,
  microphone: Option<Microphone>,
  pcm: Vec<i16>,
  /// Float frames of each source for one chunk, mixed into `pcm`.
  system_pcm: Vec<f32>,
  microphone_pcm: Vec<f32>,
  synthetic: bool,
  output: SyncSender<EncodedPacket>,
  watchdog: Arc<Watchdog>,
}
impl Pipeline {
  fn sources(&mut self) -> impl Iterator<Item = &mut Source> {
    self
      .system
      .iter_mut()
      .chain(self.microphone.iter_mut().map(|mic| &mut mic.source))
  }
  /// The source whose diagnostics the session reports: system audio when recorded.
  fn primary(&self) -> Result<&Normalizer> {
    self
      .system
      .as_ref()
      .map(|source| &source.normalizer)
      .or_else(|| self.microphone.as_ref().map(|mic| &mic.source.normalizer))
      .ok_or_else(|| EngineError::protocol("Audio pipeline without a source"))
  }
  fn mapped(&self) -> Result<bool> {
    Ok(self.primary()?.mapped())
  }
  fn next(&self) -> Result<u64> {
    Ok(self.primary()?.next())
  }
  fn silence(&self) -> Result<u64> {
    Ok(if self.synthetic {
      0
    } else {
      self.primary()?.diagnostics().inserted_silence_frames
    })
  }
  fn diagnostics(&self, revision: u64, coalesced: u64) -> Result<AudioDiagnostics> {
    let mut diagnostics = self.primary()?.diagnostics();
    diagnostics.revision = revision;
    diagnostics.reports_coalesced = coalesced;
    if self.synthetic {
      diagnostics.inserted_silence_frames = 0;
    }
    Ok(diagnostics)
  }
  fn capture_packets(&mut self, process: bool) -> Result<()> {
    if let Some(source) = &mut self.system
      && let Some(capture) = &source.capture
    {
      for _ in 0..100 {
        let Some(packet) = capture.try_packet()? else {
          break;
        };
        source.normalizer.ingest(packet)?;
      }
    }
    if let Some(mic) = &mut self.microphone
      && let Some(capture) = &mic.source.capture
    {
      for _ in 0..100 {
        match capture.try_packet() {
          Ok(Some(packet)) => mic.source.normalizer.ingest(packet)?,
          Ok(None) => break,
          Err(error) if microphone_lost(&error) => {
            // The video and system audio go on; the microphone records silence from here.
            eprintln!("Microphone lost: {error}");
            mic.source.capture = None;
            mic.lost.store(true, Ordering::Release);
            break;
          },
          Err(error) => return Err(error),
        }
      }
    }
    if process && !self.synthetic {
      let now = super::qpc()?;
      for source in self.sources() {
        source.normalizer.process_ready(now)?;
      }
    }
    Ok(())
  }
  fn output(&mut self) -> Result<()> {
    while let Some(packet) = self
      .encoder
      .output()
      .inspect_err(|error| self.watchdog.remember(error))?
    {
      self.output.try_send(packet).map_err(|e| {
        let error = EngineError::new(ErrorKind::Saturation, "AAC output", e.to_string());
        self.watchdog.remember(&error);
        error
      })?;
    }
    Ok(())
  }
  fn advance(&mut self, until: u64) -> Result<()> {
    let target = time::sample_boundary(until, 48000)?;
    if self.synthetic && self.mapped()? {
      for source in self.sources() {
        source.normalizer.synthetic_frontier(until)?;
      }
    }
    let chunk = (self.pcm.len() / 2) as u64;
    loop {
      let first = self.next()?;
      // Every source places the same frames; mix only what all of them have settled.
      let mut end = target;
      for source in self.sources() {
        if source.normalizer.next() != first {
          return Err(EngineError::protocol(
            "Audio sources lost their common frame",
          ));
        }
        end = end.min(source.normalizer.available(until)?);
      }
      if end <= first {
        break;
      }
      let count = (end - first).min(chunk) as usize;
      let frames = first + count as u64;
      let samples = count * 2;
      if let Some(source) = &mut self.system {
        source
          .normalizer
          .read_float(frames, &mut self.system_pcm[..samples])?;
      }
      if let Some(mic) = &mut self.microphone {
        mic
          .source
          .normalizer
          .read_float(frames, &mut self.microphone_pcm[..samples])?;
        mic.mutes.apply(first, &mut self.microphone_pcm[..samples]);
      }
      let (system, microphone) = (self.system.is_some(), self.microphone.is_some());
      for index in 0..samples {
        let value = match (system, microphone) {
          (true, true) => limit(self.system_pcm[index] + self.microphone_pcm[index]),
          (true, false) => self.system_pcm[index],
          _ => self.microphone_pcm[index],
        };
        self.pcm[index] = to_i16(value);
      }
      if self.synthetic {
        for index in 0..count {
          let frame = first + index as u64;
          let phase = frame % 48000;
          let value = if (24000..24960).contains(&phase) {
            ((frame as f64 * std::f64::consts::TAU * 1000.0 / 48000.0).sin() * 16000.0) as i16
          } else {
            0
          };
          self.pcm[index * 2] = value;
          self.pcm[index * 2 + 1] = value;
        }
      }
      self
        .encoder
        .input(&self.pcm[..samples], first)
        .inspect_err(|error| self.watchdog.remember(error))?;
      self.output()?;
    }
    Ok(())
  }
  fn map(&mut self, qpc: u64, media: u64) -> Result<()> {
    for source in self.sources() {
      source.normalizer.map(qpc, media)?;
      if let Some(capture) = &source.capture {
        capture.set_paused(false);
      }
    }
    Ok(())
  }
  /// The common cutoff for every source, then everything before it is encoded.
  fn cut(&mut self, cut: u64) -> Result<()> {
    for source in self.sources() {
      source.normalizer.cut(cut)?;
    }
    self.advance(cut)
  }
  fn pause(&mut self, cut: u64) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_nanos(REORDER_100NS * 100) {
      // Collect the reserve before forcing the common cutoff; do not normalize new post-cut packets.
      self.capture_packets(false)?;
      std::thread::sleep(Duration::from_millis(1));
    }
    self.cut(cut)?;
    for source in self.sources() {
      source.normalizer.discard_future();
      if let Some(capture) = &source.capture {
        capture.set_paused(true);
      }
    }
    Ok(())
  }
  fn begin(&mut self) -> Result<()> {
    for source in self.sources() {
      if let Some(capture) = &mut source.capture {
        capture.begin()?;
      }
    }
    Ok(())
  }
  /// Stops every endpoint; the first failure is returned after all of them were asked to stop.
  fn finish_captures(&mut self) -> Result<()> {
    let mut first = None;
    for source in self.sources() {
      if let Some(capture) = source.capture.take()
        && let Err(error) = capture.finish()
      {
        first.get_or_insert(error);
      }
    }
    first.map_or(Ok(()), Err)
  }
  fn microphone(&mut self, audible: bool, media: u64) -> Result<()> {
    let mic = self.microphone.as_mut().ok_or_else(|| {
      EngineError::new(
        ErrorKind::InvalidState,
        "microphone",
        "This recording has no microphone",
      )
    })?;
    let frame = time::sample_boundary(media, 48000)?;
    mic
      .mutes
      .switch(frame.max(mic.source.normalizer.next()), audible)
  }
}
pub struct AudioService {
  pub source: Option<super::audio::AudioSourceInfo>,
  pub microphone_source: Option<super::audio::AudioSourceInfo>,
  microphone_lost: Arc<AtomicBool>,
  control: SyncSender<Request>,
  packets: Receiver<EncodedPacket>,
  fatal: Receiver<EngineError>,
  target: Arc<AtomicU64>,
  stop: Arc<AtomicBool>,
  frames: Arc<AtomicU64>,
  silence: Arc<AtomicU64>,
  completed: Arc<AtomicBool>,
  diagnostics: Receiver<AudioDiagnostics>,
  latest_diagnostics: Cell<AudioDiagnostics>,
  pub extra: Vec<u8>,
}
impl AudioService {
  pub fn new(synthetic: bool, inputs: Inputs) -> Result<Self> {
    Self::new_with_watchdog(synthetic, inputs, Arc::new(Watchdog::default()))
  }
  pub(crate) fn new_with_watchdog(
    synthetic: bool,
    inputs: Inputs,
    watchdog: Arc<Watchdog>,
  ) -> Result<Self> {
    if !inputs.system && !inputs.microphone {
      return Err(EngineError::protocol("An audio track needs a source"));
    }
    let microphone_lost = Arc::new(AtomicBool::new(false));
    let lost = microphone_lost.clone();
    let (control, requests) = mpsc::sync_channel::<Request>(4);
    let (output, packets) = mpsc::sync_channel(100);
    let (failed, fatal) = mpsc::sync_channel(1);
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let (metrics_tx, diagnostics) = mpsc::sync_channel(1);
    let target = Arc::new(AtomicU64::new(0));
    let watermark = target.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let frames = Arc::new(AtomicU64::new(0));
    let count = frames.clone();
    let silence = Arc::new(AtomicU64::new(0));
    let holes = silence.clone();
    let completed = Arc::new(AtomicBool::new(false));
    let finalized = completed.clone();
    std::thread::Builder::new()
      .name("video audio encoder".into())
      .spawn(move || {
        let result = (|| {
          let startup = watchdog.begin(Stage::AudioEncode, crate::limits::PREPARE_TIMEOUT)?;
          let _com = Watched::new(
            super::Apartment::new()?,
            watchdog.clone(),
            Stage::AudioEncode,
          );
          let _mf = Watched::new(
            super::MediaRuntime::new()?,
            watchdog.clone(),
            Stage::AudioEncode,
          );
          let encoder = Watched::new(AacEncoder::new()?, watchdog.clone(), Stage::AudioEncode);
          let open = |endpoint, device: Option<String>| -> Result<Option<AudioCapture>> {
            if synthetic {
              return Ok(None);
            }
            AudioCapture::prepare_with_watchdog(endpoint, device, watchdog.clone()).map(Some)
          };
          let system = if inputs.system {
            Some(Source::new(open(Endpoint::System, inputs.system_device)?))
          } else {
            None
          };
          let microphone = if inputs.microphone {
            Some(Microphone {
              source: Source::new(open(Endpoint::Microphone, inputs.microphone_device)?),
              mutes: MuteTimeline::default(),
              lost,
            })
          } else {
            None
          };
          let mut p = Pipeline {
            encoder,
            system,
            microphone,
            pcm: vec![0; 960],
            system_pcm: vec![0.0; 960],
            microphone_pcm: vec![0.0; 960],
            synthetic,
            // Keep the outer sender alive until the terminal cause has been published.
            output: output.clone(),
            watchdog: watchdog.clone(),
          };
          let info = |source: Option<&Source>| {
            source.and_then(|source| source.capture.as_ref().map(|c| c.source.clone()))
          };
          ready_tx
            .send(Ok((
              p.encoder.extra.clone(),
              info(p.system.as_ref()),
              info(p.microphone.as_ref().map(|mic| &mic.source)),
            )))
            .map_err(|_| EngineError::protocol("AAC readiness receiver closed"))?;
          drop(startup);
          let mut last_metrics = Instant::now();
          let mut revision = 0u64;
          let mut coalesced = 0u64;
          while !stopped.load(Ordering::Acquire) {
            match requests.recv_timeout(Duration::from_millis(2)) {
              Ok(request) => {
                let limit = if matches!(request.control, Control::Start) {
                  crate::limits::START_TIMEOUT
                } else {
                  crate::limits::CONTROL_TIMEOUT
                };
                let _native = watchdog.begin(Stage::AudioEncode, limit)?;
                let finish = matches!(request.control, Control::Finish(_));
                let operation = (|| {
                  match request.control {
                    Control::Start => p.begin()?,
                    Control::Map(qpc, media) => p.map(qpc, media)?,
                    Control::Pause(cut) => p.pause(cut)?,
                    Control::Microphone(audible, media) => p.microphone(audible, media)?,
                    Control::Finish(cut) => {
                      if p.mapped()? {
                        p.pause(cut)?;
                      } else {
                        p.cut(cut)?;
                      }
                      // Do not acknowledge completed audio while a native Stop can still fail.
                      p.finish_captures()
                        .inspect_err(|error| watchdog.remember(error))?;
                      p.encoder
                        .drain()
                        .inspect_err(|error| watchdog.remember(error))?;
                      p.output()?;
                    },
                  }
                  Ok(())
                })();
                if let Err(error) = &operation {
                  watchdog.remember(error);
                }
                if finish && operation.is_ok() {
                  count.store(p.next()?, Ordering::Release);
                  holes.store(p.silence()?, Ordering::Release);
                  finalized.store(true, Ordering::Release);
                }
                revision = revision.saturating_add(1);
                let reply = operation
                  .clone()
                  .and_then(|_| p.diagnostics(revision, coalesced));
                if request.reply.try_send(reply).is_err() {
                  return Err(EngineError::protocol("Audio control receiver closed"));
                }
                operation?;
                if finish {
                  break;
                }
              },
              Err(mpsc::RecvTimeoutError::Timeout) => {},
              Err(e) => {
                return Err(EngineError::protocol(format!(
                  "Audio control disconnected: {e}"
                )));
              },
            }
            let _native = watchdog.begin(Stage::AudioEncode, crate::limits::CONTROL_TIMEOUT)?;
            p.capture_packets(true)
              .inspect_err(|error| watchdog.remember(error))?;
            if p.mapped()? {
              p.advance(watermark.load(Ordering::Acquire))
                .inspect_err(|error| watchdog.remember(error))?;
            }
            count.store(p.next()?, Ordering::Release);
            holes.store(p.silence()?, Ordering::Release);
            if last_metrics.elapsed() >= Duration::from_secs(1) {
              revision = revision.saturating_add(1);
              match metrics_tx.try_send(p.diagnostics(revision, coalesced)?) {
                Ok(()) => {},
                Err(mpsc::TrySendError::Full(_)) => {
                  coalesced = coalesced.saturating_add(1);
                },
                Err(mpsc::TrySendError::Disconnected(_)) => {
                  return Err(EngineError::protocol(
                    "Audio diagnostics owner disconnected",
                  ));
                },
              }
              last_metrics = Instant::now();
            }
          }
          Ok(())
        })();
        if let Err(error) = result {
          if ready_tx.try_send(Err(error.clone())).is_err() {
            eprintln!("Audio processing: {error}");
          }
          if failed.try_send(error).is_err() {
            eprintln!("Audio terminal receiver closed");
          }
        }
      })
      .map_err(|e| EngineError::io("audio processing thread", e))?;
    let prepared = ready
      .recv_timeout(crate::limits::PREPARE_TIMEOUT)
      .map_err(|e| EngineError::new(ErrorKind::Timeout, "AAC preparation", e.to_string()))
      .and_then(|result| result);
    let (extra, source, microphone_source) = match prepared {
      Ok(prepared) => prepared,
      Err(error) => {
        stop.store(true, Ordering::Release);
        return Err(error);
      },
    };
    Ok(Self {
      source,
      microphone_source,
      microphone_lost,
      control,
      packets,
      fatal,
      target,
      stop,
      frames,
      silence,
      completed,
      diagnostics,
      latest_diagnostics: Cell::new(AudioDiagnostics::default()),
      extra,
    })
  }
  fn request(&self, control: Control) -> Result<()> {
    let timeout = if matches!(control, Control::Start) {
      crate::limits::START_TIMEOUT
    } else {
      crate::limits::CONTROL_TIMEOUT
    };
    if let Ok(error) = self.fatal.try_recv() {
      return Err(error);
    }
    self.ensure_active()?;
    let (reply, answer) = mpsc::sync_channel(1);
    self
      .control
      .try_send(Request { control, reply })
      .map_err(|error| {
        self.fatal.try_recv().unwrap_or_else(|_| {
          EngineError::new(
            if matches!(error, mpsc::TrySendError::Full(_)) {
              ErrorKind::Saturation
            } else {
              ErrorKind::Io
            },
            "audio controls",
            error.to_string(),
          )
        })
      })?;
    let diagnostics = answer.recv_timeout(timeout).map_err(|error| {
      self.fatal.try_recv().unwrap_or_else(|_| {
        EngineError::new(
          if matches!(error, mpsc::RecvTimeoutError::Timeout) {
            ErrorKind::Timeout
          } else {
            ErrorKind::Io
          },
          "audio operation",
          error.to_string(),
        )
      })
    })??;
    self.latest_diagnostics.set(diagnostics);
    Ok(())
  }
  pub fn start(&self) -> Result<()> {
    self.request(Control::Start)
  }
  pub fn map(&self, qpc: u64, media: u64) -> Result<()> {
    self.target.store(media, Ordering::Release);
    self.request(Control::Map(qpc, media))
  }
  pub fn pause(&self, cut: u64) -> Result<()> {
    self.request(Control::Pause(cut))
  }
  pub fn finish(&self, cut: u64) -> Result<()> {
    self.request(Control::Finish(cut))
  }
  /// The microphone is audible (or muted) from media time `media`.
  pub fn microphone(&self, audible: bool, media: u64) -> Result<()> {
    self.request(Control::Microphone(audible, media))
  }
  /// The microphone disappeared during the recording; it recorded silence since.
  pub fn microphone_lost(&self) -> bool {
    self.microphone_lost.load(Ordering::Acquire)
  }
  pub fn advance(&self, until: u64) {
    self.target.store(until, Ordering::Release);
  }
  pub fn poll(&self) -> Result<Option<EncodedPacket>> {
    if let Ok(error) = self.fatal.try_recv() {
      return Err(error);
    }
    self.ensure_active()?;
    match self.packets.try_recv() {
      Ok(packet) => Ok(Some(packet)),
      Err(TryRecvError::Empty) => Ok(None),
      Err(TryRecvError::Disconnected) => {
        if let Ok(error) = self.fatal.try_recv() {
          Err(error)
        } else if self.completed.load(Ordering::Acquire) {
          Ok(None)
        } else {
          Err(EngineError::new(
            ErrorKind::DeviceLost,
            "audio encoder",
            "AAC owner disconnected before finalization",
          ))
        }
      },
    }
  }
  pub fn counts(&self) -> (u64, u64) {
    (
      self.frames.load(Ordering::Acquire),
      self.silence.load(Ordering::Acquire),
    )
  }
  pub fn diagnostics(&self) -> AudioDiagnostics {
    if let Ok(diagnostics) = self.diagnostics.try_recv()
      && diagnostics.revision > self.latest_diagnostics.get().revision
    {
      self.latest_diagnostics.set(diagnostics);
    }
    self.latest_diagnostics.get()
  }
  pub(crate) fn cancel(&self) {
    self.stop.store(true, Ordering::Release);
  }
  fn ensure_active(&self) -> Result<()> {
    if self.stop.load(Ordering::Acquire) {
      return Err(EngineError::new(
        ErrorKind::Cancelled,
        "audio encoder",
        "Audio encoder was cancelled",
      ));
    }
    Ok(())
  }
}
impl Drop for AudioService {
  fn drop(&mut self) {
    self.cancel();
  }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)] // Deliberate failing assertions in test-only fixtures.
mod tests {
  use super::*;
  fn controlled_audio(
    completed: bool,
  ) -> (
    AudioService,
    SyncSender<EncodedPacket>,
    SyncSender<EngineError>,
  ) {
    let (control, _requests) = mpsc::sync_channel(1);
    let (output, packets) = mpsc::sync_channel(1);
    let (failed, fatal) = mpsc::sync_channel(1);
    let (_metrics, diagnostics) = mpsc::sync_channel(1);
    (
      AudioService {
        source: None,
        microphone_source: None,
        microphone_lost: Arc::new(AtomicBool::new(false)),
        control,
        packets,
        fatal,
        extra: Vec::new(),
        target: Arc::new(AtomicU64::new(0)),
        stop: Arc::new(AtomicBool::new(false)),
        frames: Arc::new(AtomicU64::new(0)),
        silence: Arc::new(AtomicU64::new(0)),
        completed: Arc::new(AtomicBool::new(completed)),
        diagnostics,
        latest_diagnostics: Cell::new(AudioDiagnostics::default()),
      },
      output,
      failed,
    )
  }
  #[test]
  fn explicit_audio_cancellation_rejects_controls_and_output_without_waiting_for_the_owner() {
    let (audio, _output, _failed) = controlled_audio(false);
    audio.cancel();
    audio.cancel();
    assert_eq!(
      audio.start().expect_err("cancelled start").kind,
      ErrorKind::Cancelled
    );
    assert_eq!(
      audio.map(100, 0).expect_err("cancelled mapping").kind,
      ErrorKind::Cancelled
    );
    assert_eq!(
      audio.finish(0).expect_err("cancelled finish").kind,
      ErrorKind::Cancelled
    );
    assert_eq!(
      audio.poll().expect_err("cancelled output").kind,
      ErrorKind::Cancelled
    );
  }
  #[test]
  fn final_control_diagnostics_cannot_be_replaced_by_an_older_periodic_snapshot() {
    let (mut audio, _output, _failed) = controlled_audio(true);
    let (metrics, diagnostics) = mpsc::sync_channel(1);
    audio.diagnostics = diagnostics;
    audio.latest_diagnostics.set(AudioDiagnostics {
      revision: 3,
      late_frames: 9,
      ..Default::default()
    });
    metrics
      .send(AudioDiagnostics {
        revision: 2,
        late_frames: 1,
        ..Default::default()
      })
      .expect("old report");
    assert_eq!(audio.diagnostics().late_frames, 9);
  }
  #[test]
  fn audio_control_failure_retains_the_published_native_cause() {
    let (audio, _output, failed) = controlled_audio(false);
    failed
      .send(EngineError::new(
        ErrorKind::Native,
        "scripted audio control failure",
        "original cause",
      ))
      .expect("native failure");
    let error = audio
      .pause(0)
      .expect_err("original error before channel disconnection");
    assert_eq!(error.stage, "scripted audio control failure");
    assert_eq!(error.reason, "original cause");
  }
  #[test]
  fn channel_closure_is_eof_only_after_confirmed_aac_finalization() {
    let (audio, output, _failed) = controlled_audio(true);
    drop(output);
    assert!(audio.poll().expect("confirmed EOF").is_none());
    let (audio, output, _failed) = controlled_audio(false);
    drop(output);
    let error = audio.poll().expect_err("unexpected owner disconnection");
    assert_eq!(error.kind, ErrorKind::DeviceLost);
    let (audio, output, failed) = controlled_audio(false);
    failed
      .send(EngineError::new(
        ErrorKind::Native,
        "scripted AAC failure",
        "original cause",
      ))
      .expect("terminal cause precedes sender destruction");
    drop(output);
    let error = audio.poll().expect_err("original failure");
    assert_eq!(error.stage, "scripted AAC failure");
    assert_eq!(error.reason, "original cause");
  }
  #[test]
  fn synthetic_audio_owner_keeps_one_codec_across_pause_without_opening_an_endpoint() {
    let audio = AudioService::new(
      true,
      Inputs {
        system: true,
        system_device: Some("must-never-open-this-endpoint".into()),
        ..Inputs::default()
      },
    )
    .expect("synthetic AAC owner");
    audio.start().expect("start synthetic sources");
    audio.map(100, 0).expect("first mapping");
    let mut packets = Vec::new();
    for step in 1..=3u64 {
      let target = step * time::SECOND * 3 / 5;
      audio.advance(target);
      let deadline = Instant::now() + Duration::from_secs(2);
      loop {
        while let Some(packet) = audio.poll().expect("AAC output") {
          packets.push(packet);
        }
        if audio.counts().0 >= target * 48000 / time::SECOND {
          break;
        }
        assert!(Instant::now() < deadline, "audio owner stalled");
        std::thread::sleep(Duration::from_millis(1));
      }
      audio.pause(target).expect("common cutoff");
      audio
        .map(step * 100 * time::SECOND, target)
        .expect("new epoch excludes pause");
    }
    audio
      .finish(18 * time::SECOND / 10)
      .expect("drain same AAC encoder");
    while let Some(packet) = audio.poll().expect("AAC tail") {
      packets.push(packet);
    }
    assert_eq!(audio.counts(), (86400, 0));
    assert!(packets.len() > 80);
    assert!(packets.windows(2).all(|p| p[0].pts < p[1].pts));
    assert_eq!(packets[0].pts, 0);
  }
  #[test]
  fn unconsumed_compressed_audio_is_an_explicit_fatal_saturation() {
    let audio = AudioService::new(
      true,
      Inputs {
        system: true,
        ..Inputs::default()
      },
    )
    .expect("AAC owner");
    audio.start().expect("synthetic source");
    audio.map(100, 0).expect("mapping");
    audio.advance(3 * time::SECOND);
    // Do not drain the 100-packet channel until the owner has reached its budget.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
      match audio.fatal.try_recv() {
        Ok(error) => {
          assert_eq!(error.kind, ErrorKind::Saturation);
          assert_eq!(error.stage, "AAC output");
          break;
        },
        Err(TryRecvError::Empty) => {
          assert!(Instant::now() < deadline, "missing saturation error");
          std::thread::sleep(Duration::from_millis(1));
        },
        Err(error) => panic!("missing owner result: {error}"),
      }
    }
  }
}
