use crate::watchdog::{Stage, Watchdog, Watched};
use crate::{
  EngineError, ErrorKind, Result,
  metrics::{ReportSlot, WriterMeasurements, WriterReport},
  types::*,
};
use std::{
  path::Path,
  sync::mpsc::{self, Receiver, SyncSender, TrySendError},
  thread,
};
use windows::{
  Win32::Media::MediaFoundation::*,
  core::{GUID, PCWSTR},
};

pub fn media_type(major: &GUID, subtype: &GUID) -> Result<IMFMediaType> {
  // SAFETY: MF is initialized by the owner; returned media type is locally owned.
  unsafe {
    let t = MFCreateMediaType().map_err(|e| EngineError::native("media type", e))?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, major)
      .map_err(|e| EngineError::native("major type", e))?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype)
      .map_err(|e| EngineError::native("subtype", e))?;
    Ok(t)
  }
}
pub fn video_type(width: u32, height: u32, fps: u32, extra: &[u8]) -> Result<IMFMediaType> {
  let t = media_type(&MFMediaType_Video, &MFVideoFormat_H264)?;
  // SAFETY: checked physical dimensions and rational cadence; SPS/PPS are owned encoded parameter sets.
  unsafe {
    t.SetUINT64(
      &MF_MT_FRAME_SIZE,
      (u64::from(width) << 32) | u64::from(height),
    )
    .map_err(|e| EngineError::native("video size", e))?;
    t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)
      .map_err(|e| EngineError::native("video rate", e))?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
      .map_err(|e| EngineError::native("video aspect", e))?;
    for (key, value) in [
      (&MF_MT_INTERLACE_MODE, 2),
      (&MF_MT_VIDEO_PRIMARIES, 2),
      (&MF_MT_TRANSFER_FUNCTION, 5),
      (&MF_MT_YUV_MATRIX, 1),
      (&MF_MT_VIDEO_NOMINAL_RANGE, 2),
    ] {
      t.SetUINT32(key, value)
        .map_err(|e| EngineError::native("video color metadata", e))?;
    }
    if !extra.is_empty() {
      t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, extra)
        .map_err(|e| EngineError::native("H264 parameter sets", e))?;
    }
  }
  Ok(t)
}
pub fn audio_type(aac: bool) -> Result<IMFMediaType> {
  let t = media_type(
    &MFMediaType_Audio,
    if aac {
      &MFAudioFormat_AAC
    } else {
      &MFAudioFormat_PCM
    },
  )?;
  // SAFETY: Microsoft AAC-LC supports 48 kHz stereo with 16-bit PCM input.
  unsafe {
    for (key, value) in [
      (&MF_MT_AUDIO_NUM_CHANNELS, 2),
      (&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000),
      (&MF_MT_AUDIO_BITS_PER_SAMPLE, 16),
      (
        &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
        if aac { 24000 } else { 192000 },
      ),
    ] {
      t.SetUINT32(key, value)
        .map_err(|e| EngineError::native("audio type", e))?;
    }
    if aac {
      t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
        .map_err(|e| EngineError::native("AAC payload", e))?;
    } else {
      t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)
        .map_err(|e| EngineError::native("PCM alignment", e))?;
    }
  }
  Ok(t)
}
pub fn sample(bytes: &[u8], pts: i64, duration: u64) -> Result<IMFSample> {
  if bytes.is_empty()
    || bytes.len() > 16 * 1024 * 1024
    || duration == 0
    || duration > i64::MAX as u64
  {
    return Err(EngineError::protocol("Invalid encoded/PCM sample extent"));
  }
  // SAFETY: all buffers are owned on this COM owner thread and unlocked before submission.
  unsafe {
    let buffer = MFCreateMemoryBuffer(bytes.len() as u32)
      .map_err(|e| EngineError::native("sample buffer", e))?;
    let mut pointer = std::ptr::null_mut();
    buffer
      .Lock(&mut pointer, None, None)
      .map_err(|e| EngineError::native("sample lock", e))?;
    if pointer.is_null() {
      let unlock = buffer.Unlock();
      return Err(EngineError::protocol(format!(
        "Null MF buffer; unlock={unlock:?}"
      )));
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
    buffer
      .Unlock()
      .map_err(|e| EngineError::native("sample unlock", e))?;
    buffer
      .SetCurrentLength(bytes.len() as u32)
      .map_err(|e| EngineError::native("sample length", e))?;
    let sample = MFCreateSample().map_err(|e| EngineError::native("sample create", e))?;
    sample
      .AddBuffer(&buffer)
      .map_err(|e| EngineError::native("sample attach", e))?;
    sample
      .SetSampleTime(pts)
      .map_err(|e| EngineError::native("sample PTS", e))?;
    sample
      .SetSampleDuration(duration as i64)
      .map_err(|e| EngineError::native("sample duration", e))?;
    Ok(sample)
  }
}
pub fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
  // SAFETY: contiguous owned buffer stays locked only during the bounded byte copy.
  unsafe {
    let buffer = sample
      .ConvertToContiguousBuffer()
      .map_err(|e| EngineError::native("output buffer", e))?;
    // Decoded pictures may be 2D buffers whose current length counts their padded pitch,
    // while a contiguous lock holds only the default-stride copy; copy by the 2D length.
    if let Ok(plane) = windows::core::Interface::cast::<IMF2DBuffer>(&buffer) {
      let len = plane
        .GetContiguousLength()
        .map_err(|e| EngineError::native("picture size", e))? as usize;
      if len == 0 || len > 256 * 1024 * 1024 {
        return Err(EngineError::protocol("Invalid decoded picture size"));
      }
      let mut bytes = vec![0u8; len];
      plane
        .ContiguousCopyTo(&mut bytes)
        .map_err(|e| EngineError::native("picture copy", e))?;
      return Ok(bytes);
    }
    let len = buffer
      .GetCurrentLength()
      .map_err(|e| EngineError::native("output size", e))? as usize;
    if len == 0 || len > 16 * 1024 * 1024 {
      return Err(EngineError::protocol("Invalid MFT output size"));
    }
    let mut pointer = std::ptr::null_mut();
    buffer
      .Lock(&mut pointer, None, None)
      .map_err(|e| EngineError::native("output lock", e))?;
    let bytes = if pointer.is_null() {
      Err(EngineError::protocol("Null MFT output"))
    } else {
      Ok(std::slice::from_raw_parts(pointer, len).to_vec())
    };
    let unlock = buffer
      .Unlock()
      .map_err(|e| EngineError::native("output unlock", e));
    let bytes = bytes?;
    unlock?;
    Ok(bytes)
  }
}

/// Holds each track's newest packet until the next one arrives, then sets its duration to reach it.
/// The fragmented sink dates a fragment's last sample by its own duration and accumulates the next
/// fragment's decode time from it, so a duration that disagrees with the next PTS (a phase-locked
/// slot dated when sampled, or a dropped slot) moved the rest of the timeline (measured:
/// 8.3 ms for the rest of a run). The final packet of each track keeps its encoder duration, except
/// that the final video packet reaches the stop cut when one is given.
#[derive(Default)]
struct Lookahead([Option<EncodedPacket>; 2]);
impl Lookahead {
  /// Hold `next` and release the previous packet of its track with an exact duration.
  fn exchange(&mut self, index: usize, next: EncodedPacket) -> Result<Option<EncodedPacket>> {
    let pts = next.pts;
    let held = self
      .0
      .get_mut(index)
      .ok_or_else(|| EngineError::protocol("Unknown mux track"))?
      .replace(next);
    held
      .map(|mut packet| {
        packet.duration = pts
          .checked_sub(packet.pts)
          .and_then(|gap| u64::try_from(gap).ok())
          .filter(|gap| *gap > 0)
          .ok_or_else(|| EngineError::protocol("Non-monotonic or reordered mux packet"))?;
        Ok(packet)
      })
      .transpose()
  }
  /// The held packets, in track order, with their own durations. The video packet is lengthened
  /// (never shortened) to `video_end`: slots lost before the stop cut keep showing the last frame,
  /// as a lost slot mid-run does. Ending at its own duration failed a Degraded recording's
  /// boundary check (measured: 0.88 s short after an encoder starved by a game).
  fn drain(&mut self, video_end: Option<u64>) -> Result<Vec<(usize, EncodedPacket)>> {
    self
      .0
      .iter_mut()
      .enumerate()
      .filter_map(|(index, held)| held.take().map(|packet| (index, packet)))
      .map(|(index, mut packet)| {
        if let (0, Some(end)) = (index, video_end) {
          let start = u64::try_from(packet.pts)
            .map_err(|_| EngineError::protocol("Negative held video PTS"))?;
          let reach = end.saturating_sub(start);
          if reach > packet.duration {
            packet.duration = reach;
          }
        }
        Ok((index, packet))
      })
      .collect()
  }
}

struct Mux {
  writer: IMFSinkWriter,
  _sink: IMFMediaSink,
  _stream: IMFByteStream,
  audio: bool,
  last: [Option<i64>; 2],
  pending: Lookahead,
}
impl Mux {
  fn new(
    path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    extra: &[u8],
    audio_extra: Option<&[u8]>,
  ) -> Result<Self> {
    let video = video_type(width, height, fps, extra)?;
    let audio = if let Some(extra) = audio_extra {
      let a = audio_type(true)?;
      // SAFETY: AAC encoder output type's user data has the documented WAVEFORMAT/ASC representation.
      unsafe { a.SetBlob(&MF_MT_USER_DATA, extra) }
        .map_err(|e| EngineError::native("AAC configuration", e))?;
      Some(a)
    } else {
      None
    };
    let path: Vec<u16> = path
      .as_os_str()
      .to_string_lossy()
      .encode_utf16()
      .chain([0])
      .collect();
    // SAFETY: terminated path, CREATE_NEW protects existing files; the stream stays on this thread.
    let stream = unsafe {
      MFCreateFile(
        MF_ACCESSMODE_WRITE,
        MF_OPENMODE_FAIL_IF_EXIST,
        MF_FILEFLAGS_NONE,
        PCWSTR(path.as_ptr()),
      )
    }
    .map_err(|e| EngineError::native("native MP4 file", e))?;
    // Fragments are kept clear of Windows players' 256 KiB reads as they are written.
    let stream = super::aligned::AlignedStream::wrap(stream, crate::interop::READ_CHUNK);
    Self::over(stream, video, audio)
  }
  /// Build the fragmented-MP4 sink writer over `stream`.
  fn over(stream: IMFByteStream, video: IMFMediaType, audio: Option<IMFMediaType>) -> Result<Self> {
    // SAFETY: all objects remain on the writer thread that owns this mux.
    unsafe {
      let sink = MFCreateFMPEG4MediaSink(&stream, &video, audio.as_ref())
        .map_err(|e| EngineError::native("fragmented MP4 sink", e))?;
      let mut attributes = None;
      MFCreateAttributes(&mut attributes, 4)
        .map_err(|e| EngineError::native("writer attributes", e))?;
      let attributes = attributes.ok_or_else(|| EngineError::protocol("No writer attributes"))?;
      attributes
        .SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)
        .map_err(|e| EngineError::native("disable converters", e))?;
      attributes
        .SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)
        .map_err(|e| EngineError::native("disable writer throttle", e))?;
      let writer = MFCreateSinkWriterFromMediaSink(&sink, &attributes)
        .map_err(|e| EngineError::native("sink writer", e))?;
      writer
        .SetInputMediaType(0, &video, None)
        .map_err(|e| EngineError::native("compressed video input", e))?;
      if let Some(audio) = &audio {
        writer
          .SetInputMediaType(1, audio, None)
          .map_err(|e| EngineError::native("compressed audio input", e))?;
      }
      writer
        .BeginWriting()
        .map_err(|e| EngineError::native("begin MP4 writing", e))?;
      Ok(Self {
        writer,
        _sink: sink,
        _stream: stream,
        audio: audio.is_some(),
        last: [None, None],
        pending: Lookahead::default(),
      })
    }
  }
  fn write(&mut self, packet: EncodedPacket) -> Result<()> {
    packet.validate()?;
    let index = if packet.track == Track::Video { 0 } else { 1 };
    if index == 1 && !self.audio {
      return Err(EngineError::protocol("Audio packet for silent session"));
    }
    if packet.pts < 0
      || packet.dts != packet.pts
      || self.last[index].is_some_and(|p| packet.pts <= p)
    {
      return Err(EngineError::protocol(
        "Non-monotonic or reordered mux packet",
      ));
    }
    let pts = packet.pts;
    if let Some(ready) = self.pending.exchange(index, packet)? {
      self.write_now(index, &ready)?;
    }
    self.last[index] = Some(pts);
    Ok(())
  }
  fn write_now(&self, index: usize, packet: &EncodedPacket) -> Result<()> {
    let sample = sample(&packet.data, packet.pts, packet.duration)?;
    // SAFETY: compressed packet types match configured sink streams; writer is exclusively owned.
    unsafe {
      sample
        .SetUINT32(&MFSampleExtension_CleanPoint, u32::from(packet.keyframe))
        .map_err(|e| EngineError::native("keyframe flag", e))?;
      self
        .writer
        .WriteSample(index as u32, &sample)
        .map_err(|e| EngineError::native("MP4 write", e))?;
    }
    Ok(())
  }
  #[cfg(test)]
  fn finish(self) -> Result<()> {
    self.finish_at(None)
  }
  /// Finalize; `video_end` is the stop cut the last video packet must reach (100 ns).
  fn finish_at(mut self, video_end: Option<u64>) -> Result<()> {
    let held = self.pending.drain(video_end)?;
    for (index, packet) in &held {
      self.write_now(*index, packet)?;
    }
    // SAFETY: no more producers can submit after Finish; no Flush that would drop encoder samples.
    unsafe {
      self
        .writer
        .Finalize()
        .map_err(|e| EngineError::native("MP4 finalize", e))?;
    }
    // Finalize closes the file byte stream on the native fragmented sink. Flushing
    // that closed IMFByteStream returns E_INVALIDARG. Publication reopens the
    // completed file with write access and performs the durable file flush.
    Ok(())
  }
}

struct QueuedPacket {
  packet: EncodedPacket,
  queued_at: std::time::Instant,
}
enum Message {
  Packet(QueuedPacket),
  /// Finalize, lengthening the last video packet to this stop cut (100 ns) when given.
  Finish(Option<u64>),
}
/// Test helper: write a software fixture's video packets to a fragmented MP4 at 60 FPS.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) fn write_fixture(
  path: &Path,
  fixture: super::fixtures::MediaFixture,
  width: u32,
  height: u32,
) {
  let sink = Mux::new(path, width, height, 60, &fixture.video_extra, None).expect("sink");
  write_packets(sink, fixture);
}
/// Test helper: write a fixture at 60 FPS through `stream` (for byte-stream tests).
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) fn write_fixture_over(
  stream: IMFByteStream,
  fixture: super::fixtures::MediaFixture,
  width: u32,
  height: u32,
) {
  let video = video_type(width, height, 60, &fixture.video_extra).expect("video type");
  write_packets(Mux::over(stream, video, None).expect("sink"), fixture);
}
#[cfg(test)]
#[allow(clippy::expect_used)]
fn write_packets(mut sink: Mux, fixture: super::fixtures::MediaFixture) {
  for packet in fixture.video {
    sink.write(packet).expect("compressed packet");
  }
  sink.finish().expect("finalization");
}
pub struct Writer {
  metrics: std::sync::Arc<ReportSlot<WriterReport>>,
  sender: SyncSender<Message>,
  completion: Receiver<Result<()>>,
  terminal: std::sync::Mutex<Option<Result<()>>>,
  outstanding: std::sync::Arc<std::sync::atomic::AtomicUsize>,
  budget: std::sync::Arc<std::sync::Mutex<crate::limits::PacketBudget>>,
}
impl Writer {
  pub fn start(
    path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    extra: Vec<u8>,
    audio_extra: Option<Vec<u8>>,
  ) -> Result<Self> {
    Self::start_with_watchdog(
      path,
      (width, height, fps),
      extra,
      audio_extra,
      std::sync::Arc::new(Watchdog::default()),
    )
  }
  pub(crate) fn start_with_watchdog(
    path: &Path,
    dimensions: (u32, u32, u32),
    extra: Vec<u8>,
    audio_extra: Option<Vec<u8>>,
    watchdog: std::sync::Arc<Watchdog>,
  ) -> Result<Self> {
    let (width, height, fps) = dimensions;
    let path = path.to_owned();
    let metrics = std::sync::Arc::new(ReportSlot::default());
    let reports = metrics.clone();
    let (sender, receiver) = mpsc::sync_channel(256);
    let (done, completion) = mpsc::sync_channel(1);
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let outstanding = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = outstanding.clone();
    let budget = std::sync::Arc::new(std::sync::Mutex::new(crate::limits::PacketBudget::default()));
    let in_flight = budget.clone();
    thread::Builder::new()
      .name("video MP4 writer".into())
      .spawn(move || {
        let mut announced = false;
        let mut measurements = WriterMeasurements::default();
        let result = (|| {
          let startup = watchdog.begin(Stage::Write, crate::limits::PREPARE_TIMEOUT)?;
          let _com = Watched::new(super::Apartment::new()?, watchdog.clone(), Stage::Write);
          let _mf = Watched::new(super::MediaRuntime::new()?, watchdog.clone(), Stage::Write);
          let mut mux = Watched::new(
            Mux::new(&path, width, height, fps, &extra, audio_extra.as_deref())?,
            watchdog.clone(),
            Stage::Write,
          );
          drop(startup);
          ready_tx
            .send(Ok(()))
            .map_err(|_| EngineError::protocol("Writer owner disconnected"))?;
          announced = true;
          let mut last_report = std::time::Instant::now();
          while let Ok(message) = receiver.recv() {
            match message {
              Message::Packet(queued) => {
                let _native = watchdog.begin(Stage::Write, crate::limits::CONTROL_TIMEOUT)?;
                let bytes = queued.packet.data.len();
                measurements
                  .queue_wait
                  .record(queued.queued_at.elapsed().as_micros() as u64);
                let started = std::time::Instant::now();
                let result = mux
                  .write(queued.packet)
                  .inspect_err(|error| watchdog.remember(error));
                measurements
                  .native_write
                  .record(started.elapsed().as_micros() as u64);
                result?;
                if measurements.first_packet_qpc_100ns.is_none() {
                  measurements.first_packet_qpc_100ns = Some(super::qpc()?);
                }
                measurements.packets_written = measurements.packets_written.saturating_add(1);
                measurements.bytes_written =
                  measurements.bytes_written.saturating_add(bytes as u64);
                let mut budget = in_flight
                  .lock()
                  .map_err(|_| EngineError::protocol("Writer budget poisoned"))?;
                budget.complete()?;
                // Keep the byte mirror and the reservation ledger consistent under the same lock.
                count.fetch_sub(bytes, std::sync::atomic::Ordering::AcqRel);
              },
              Message::Finish(video_end) => {
                let _native = watchdog.begin(Stage::Finalize, crate::limits::STOP_TIMEOUT)?;
                let started = std::time::Instant::now();
                let result = mux
                  .into_inner()
                  .finish_at(video_end)
                  .inspect_err(|error| watchdog.remember(error));
                measurements
                  .native_finalize
                  .record(started.elapsed().as_micros() as u64);
                return result;
              },
            }
            if last_report.elapsed() >= std::time::Duration::from_secs(1) {
              reports.publish(measurements.report(false))?;
              last_report = std::time::Instant::now();
            }
          }
          Err(EngineError::new(
            ErrorKind::Cancelled,
            "writer",
            "Media producer disconnected before finalization",
          ))
        })();
        let report = reports.publish(measurements.report(result.is_ok()));
        let result = match (result, report) {
          (Err(mut original), Err(diagnostic)) => {
            original
              .reason
              .push_str(&format!("; metrics: {diagnostic}"));
            Err(original)
          },
          (Err(original), _) => Err(original),
          (Ok(()), Err(error)) => Err(error),
          (Ok(()), Ok(_)) => Ok(()),
        };
        if !announced
          && let Err(error) = &result
          && ready_tx.try_send(Err(error.clone())).is_err()
        {
          eprintln!("MP4 readiness receiver closed: {error}");
        }
        if done.send(result).is_err() {
          eprintln!("MP4 completion receiver closed");
        }
      })
      .map_err(|e| EngineError::io("writer thread", e))?;
    ready
      .recv_timeout(crate::limits::PREPARE_TIMEOUT)
      .map_err(|e| EngineError::new(ErrorKind::Timeout, "writer prepare", e.to_string()))??;
    Ok(Self {
      metrics,
      sender,
      completion,
      terminal: std::sync::Mutex::new(None),
      outstanding,
      budget,
    })
  }
  pub fn push(&self, packet: EncodedPacket) -> Result<()> {
    self.check()?;
    packet.validate()?;
    let bytes = packet.data.len();
    // This lock covers only bookkeeping and a nonblocking channel submission, never a native call.
    let mut budget = self
      .budget
      .lock()
      .map_err(|_| EngineError::protocol("Writer budget poisoned"))?;
    budget.reserve(
      bytes,
      packet.track,
      u64::try_from(packet.pts).map_err(|_| EngineError::protocol("Negative queued PTS"))?,
      std::time::Instant::now(),
    )?;
    let reservation = self.outstanding.try_update(
      std::sync::atomic::Ordering::AcqRel,
      std::sync::atomic::Ordering::Acquire,
      |n| {
        n.checked_add(bytes)
          .filter(|v| *v <= crate::limits::PACKET_LIMIT)
      },
    );
    if reservation.is_err() {
      budget.cancel_last();
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "writer",
        "Encoded packet budget exceeded",
      ));
    }
    if let Err(error) = self.sender.try_send(Message::Packet(QueuedPacket {
      packet,
      queued_at: std::time::Instant::now(),
    })) {
      budget.cancel_last();
      self
        .outstanding
        .fetch_sub(bytes, std::sync::atomic::Ordering::AcqRel);
      let disconnected = matches!(error, TrySendError::Disconnected(_));
      let submission_error = EngineError::new(
        if matches!(error, TrySendError::Full(_)) {
          ErrorKind::Saturation
        } else {
          ErrorKind::Io
        },
        "writer",
        error.to_string(),
      );
      drop(budget);
      if disconnected && let Some(Err(original)) = self.poll_completion() {
        return Err(original);
      }
      return Err(submission_error);
    }
    Ok(())
  }
  pub fn metrics_handle(&self) -> std::sync::Arc<ReportSlot<WriterReport>> {
    self.metrics.clone()
  }
  pub fn check(&self) -> Result<()> {
    if let Some(result) = self.poll_completion() {
      return result.and_then(|_| Err(EngineError::protocol("Writer completed before requested")));
    }
    self
      .budget
      .lock()
      .map_err(|_| EngineError::protocol("Writer budget poisoned"))?
      .check(std::time::Instant::now())
  }
  fn poll_completion(&self) -> Option<Result<()>> {
    let mut terminal = match self.terminal.lock() {
      Ok(terminal) => terminal,
      Err(_) => {
        return Some(Err(EngineError::protocol(
          "Writer terminal status poisoned",
        )));
      },
    };
    if terminal.is_none() {
      match self.completion.try_recv() {
        Ok(result) => *terminal = Some(result),
        Err(mpsc::TryRecvError::Empty) => {},
        Err(error) => {
          *terminal = Some(Err(EngineError::new(
            ErrorKind::Io,
            "writer",
            error.to_string(),
          )));
        },
      }
    }
    terminal.clone()
  }
  pub fn finish(self) -> Result<()> {
    self.finish_by(std::time::Instant::now() + crate::limits::STOP_TIMEOUT)
  }
  pub(super) fn finish_by(self, deadline: std::time::Instant) -> Result<()> {
    self.finish_cancellable(deadline, None, || Ok(()))
  }
  /// `video_end` is the recording's stop cut (100 ns), which the last video packet must reach.
  pub(super) fn finish_cancellable(
    self,
    deadline: std::time::Instant,
    video_end: Option<u64>,
    checkpoint: impl Fn() -> Result<()>,
  ) -> Result<()> {
    loop {
      if let Some(result) = self.poll_completion() {
        return result;
      }
      checkpoint()?;
      match self.sender.try_send(Message::Finish(video_end)) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => break,
        Err(TrySendError::Full(_)) => {
          if std::time::Instant::now() >= deadline {
            return Err(EngineError::new(
              ErrorKind::Timeout,
              "writer finalization",
              "Timed out queuing finalization",
            ));
          }
          // A full media queue is drained before Finish; never drop an accepted packet.
          thread::sleep(std::time::Duration::from_millis(1));
        },
      }
    }
    loop {
      if let Some(result) = self.poll_completion() {
        return result;
      }
      checkpoint()?;
      let remaining = deadline.saturating_duration_since(std::time::Instant::now());
      if remaining.is_zero() {
        return Err(EngineError::new(
          ErrorKind::Timeout,
          "writer finalization",
          "Native finalization deadline exceeded",
        ));
      }
      self
        .budget
        .lock()
        .map_err(|_| EngineError::protocol("Writer budget poisoned"))?
        .check(std::time::Instant::now())?;
      match self
        .completion
        .recv_timeout(remaining.min(std::time::Duration::from_millis(5)))
      {
        Ok(result) => return result,
        Err(mpsc::RecvTimeoutError::Timeout) => {},
        Err(error) => {
          return Err(EngineError::new(
            ErrorKind::Io,
            "writer finalization",
            error.to_string(),
          ));
        },
      }
    }
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  fn controlled_writer(capacity: usize) -> (Writer, Receiver<Message>, SyncSender<Result<()>>) {
    let (sender, messages) = mpsc::sync_channel(capacity);
    let (done, completion) = mpsc::sync_channel(1);
    (
      Writer {
        metrics: std::sync::Arc::new(ReportSlot::default()),
        sender,
        completion,
        terminal: std::sync::Mutex::new(None),
        outstanding: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        budget: std::sync::Arc::new(std::sync::Mutex::new(crate::limits::PacketBudget::default())),
      },
      messages,
      done,
    )
  }
  fn packet() -> EncodedPacket {
    EncodedPacket {
      track: Track::Video,
      pts: 0,
      dts: 0,
      duration: 166666,
      keyframe: true,
      data: vec![1, 2, 3],
    }
  }
  #[test]
  fn abort_interrupts_both_queuing_finish_and_waiting_for_the_native_owner() {
    for full in [true, false] {
      let (writer, _messages, _done) = controlled_writer(1);
      if full {
        writer.push(packet()).expect("full media queue");
      }
      let attempts = std::sync::atomic::AtomicU32::new(0);
      let error = writer
        .finish_cancellable(
          std::time::Instant::now() + std::time::Duration::from_secs(1),
          None,
          || {
            if attempts.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= 2 {
              Err(EngineError::new(
                ErrorKind::Cancelled,
                "scripted Abort",
                "aborted",
              ))
            } else {
              Ok(())
            }
          },
        )
        .expect_err("priority Abort");
      assert_eq!(error.kind, ErrorKind::Cancelled);
      assert_eq!(error.stage, "scripted Abort");
    }
  }
  #[test]
  fn finish_waits_for_a_full_queue_without_losing_accepted_packets() {
    let (writer, messages, done) = controlled_writer(1);
    writer.push(packet()).expect("accepted packet");
    let consumer = thread::spawn(move || {
      thread::sleep(std::time::Duration::from_millis(10));
      assert!(matches!(
        messages.recv_timeout(std::time::Duration::from_secs(1)),
        Ok(Message::Packet(_))
      ));
      assert!(matches!(
        messages.recv_timeout(std::time::Duration::from_secs(1)),
        Ok(Message::Finish(None))
      ));
      done.send(Ok(())).expect("finalized");
    });
    writer
      .finish_by(std::time::Instant::now() + std::time::Duration::from_secs(1))
      .expect("queue drainage precedes finalization");
    consumer.join().expect("bounded consumer");
  }
  #[test]
  fn original_writer_failure_survives_repeated_observation_and_finish() {
    let (writer, messages, done) = controlled_writer(1);
    let original = EngineError::new(ErrorKind::Io, "scripted disk write", "disk full");
    done.send(Err(original.clone())).expect("original cause");
    drop(messages);
    for _ in 0..2 {
      let error = writer.check().expect_err("same failure");
      assert_eq!(error.stage, original.stage);
      assert_eq!(error.reason, original.reason);
    }
    let error = writer.finish().expect_err("preserved finalization cause");
    assert_eq!(error.stage, original.stage);
    assert_eq!(error.reason, original.reason);
  }
  #[test]
  fn full_queue_deadline_and_byte_reservation_failure_are_explicit() {
    let (writer, _messages, _done) = controlled_writer(1);
    writer.outstanding.store(
      crate::limits::PACKET_LIMIT,
      std::sync::atomic::Ordering::Release,
    );
    assert_eq!(
      writer.push(packet()).expect_err("byte cap").kind,
      ErrorKind::Saturation
    );
    assert!(writer.budget.lock().expect("budget").complete().is_err());
    writer
      .outstanding
      .store(0, std::sync::atomic::Ordering::Release);
    writer.push(packet()).expect("reservation was rolled back");
    let error = writer
      .finish_by(std::time::Instant::now())
      .expect_err("full queue deadline");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.stage, "writer finalization");
  }
  /// Regression for the coverage gate: a backlog over the media-span budget was only reached
  /// when a slow machine fell behind, so the measured lines differed per machine.
  #[test]
  fn a_backlog_over_two_seconds_of_one_track_is_refused_before_queuing() {
    let (writer, messages, _done) = controlled_writer(2);
    writer.push(packet()).expect("first packet queued");
    let late = EncodedPacket {
      pts: 2 * crate::time::SECOND as i64 + 1,
      dts: 2 * crate::time::SECOND as i64 + 1,
      keyframe: false,
      ..packet()
    };
    let error = writer.push(late).expect_err("span budget");
    assert_eq!(
      (error.kind, error.stage.as_str()),
      (ErrorKind::Saturation, "writer budget")
    );
    // Only the first packet was queued and only its bytes stay reserved.
    assert_eq!(messages.try_iter().count(), 1);
    assert_eq!(
      writer
        .outstanding
        .load(std::sync::atomic::Ordering::Acquire),
      3
    );
  }
  #[test]
  fn finish_returns_a_completion_that_lands_before_or_during_its_wait() {
    // The checkpoint runs once before Finish is queued and once per wait; completing inside the
    // first lands before the post-queue check, inside the second before the bounded receive.
    for completes_at in [1, 2] {
      let (writer, messages, done) = controlled_writer(1);
      let calls = std::sync::atomic::AtomicU32::new(0);
      writer
        .finish_cancellable(
          std::time::Instant::now() + std::time::Duration::from_secs(5),
          Some(42),
          || {
            if calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1 == completes_at {
              done.send(Ok(())).expect("completion slot");
            }
            Ok(())
          },
        )
        .expect("completed finalization");
      assert_eq!(
        calls.load(std::sync::atomic::Ordering::Acquire),
        completes_at
      );
      assert!(matches!(messages.try_recv(), Ok(Message::Finish(Some(42)))));
    }
  }
  #[test]
  fn lookahead_gives_each_packet_the_gap_to_its_successor_and_keeps_the_last_duration() {
    let packet = |track, pts| EncodedPacket {
      track,
      pts,
      dts: pts,
      duration: 166_667,
      keyframe: false,
      data: vec![1],
    };
    let mut pending = Lookahead::default();
    assert!(
      pending
        .exchange(0, packet(Track::Video, 0))
        .expect("first")
        .is_none()
    );
    assert!(
      pending
        .exchange(1, packet(Track::Audio, 0))
        .expect("audio")
        .is_none()
    );
    // A wrap shortens one gap to 8.3 ms and lengthens the next: durations follow the PTS.
    let first = pending
      .exchange(0, packet(Track::Video, 83_333))
      .expect("short");
    assert_eq!(first.map(|p| (p.pts, p.duration)), Some((0, 83_333)));
    let second = pending
      .exchange(0, packet(Track::Video, 333_333))
      .expect("long");
    assert_eq!(second.map(|p| (p.pts, p.duration)), Some((83_333, 250_000)));
    assert!(pending.exchange(0, packet(Track::Video, 333_333)).is_err());
    assert!(pending.exchange(2, packet(Track::Video, 999_999)).is_err());
    let rest: Vec<_> = pending
      .drain(None)
      .expect("held packets")
      .into_iter()
      .map(|(i, p)| (i, p.pts, p.duration))
      .collect();
    assert_eq!(rest, [(0, 333_333, 166_667), (1, 0, 166_667)]);
    assert!(pending.drain(None).expect("nothing held").is_empty());
  }
  #[test]
  fn the_last_video_packet_reaches_the_stop_cut_and_is_never_shortened() {
    let packet = |track, pts| EncodedPacket {
      track,
      pts,
      dts: pts,
      duration: 83_333,
      keyframe: false,
      data: vec![1],
    };
    let held = |video_end| {
      let mut pending = Lookahead::default();
      pending
        .exchange(0, packet(Track::Video, 1_000_000))
        .expect("video");
      pending
        .exchange(1, packet(Track::Audio, 1_500_000))
        .expect("audio");
      pending
        .drain(video_end)
        .expect("held packets")
        .into_iter()
        .map(|(i, p)| (i, p.duration))
        .collect::<Vec<_>>()
    };
    // Slots lost before the cut at 2 s: the last frame covers them; audio keeps its own duration.
    assert_eq!(held(Some(2_000_000)), [(0, 1_000_000), (1, 83_333)]);
    // A cut inside the frame's own duration never shortens it.
    assert_eq!(held(Some(1_050_000)), [(0, 83_333), (1, 83_333)]);
    assert_eq!(held(None), [(0, 83_333), (1, 83_333)]);
  }
  #[test]
  fn a_recording_whose_final_slots_were_lost_still_meets_its_stop_boundary() {
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let fixture = super::super::fixtures::media(3);
    let cut = fixture
      .video
      .last()
      .map(|p| p.pts as u64 + p.duration)
      .expect("fixture end");
    let t = tempfile::tempdir().expect("fixture directory");
    for (name, video_end, valid) in [
      ("starved.mp4", Some(cut), true),
      ("unreached.mp4", None, false),
    ] {
      let path = t.path().join(name);
      let mut sink = Mux::new(&path, 512, 64, 60, &fixture.video_extra, None).expect("sink");
      // The encoder refused the last 54 slots (0.9 s at 60 FPS) before Stop.
      let kept = fixture.video.len() - 54;
      for packet in fixture.video.iter().take(kept).cloned() {
        sink.write(packet).expect("compressed packet");
      }
      sink.finish_at(video_end).expect("finalize");
      let inspection = crate::recovery::inspect(&path).expect("inspect");
      assert_eq!(
        inspection.validate_recording(false, 60, cut).is_ok(),
        valid,
        "{name}"
      );
      assert_eq!(
        inspection.track_media.first().map(|v| v.samples),
        Some(kept as u64)
      );
    }
  }
  #[test]
  fn irregular_video_timestamps_keep_the_file_timeline_on_their_pts() {
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let fixture = super::super::fixtures::media(3);
    let t = tempfile::tempdir().expect("fixture directory");
    let path = t.path().join("irregular.mp4");
    let mut sink = Mux::new(&path, 512, 64, 60, &fixture.video_extra, None).expect("sink");
    // A phase lock flapping across a refresh boundary: every other slot sampled 8.3 ms later,
    // with the encoder's nominal durations (what the fragment sink used to trust).
    let mut last = 0;
    for (i, mut packet) in fixture.video.into_iter().enumerate() {
      packet.pts += if i % 2 == 1 { 83_333 } else { 0 };
      packet.dts = packet.pts;
      last = packet.pts + packet.duration as i64;
      sink.write(packet).expect("compressed packet");
    }
    sink.finish().expect("finalize");
    let inspection = crate::recovery::inspect(&path).expect("inspect");
    let video = inspection.track_media.first().expect("video track");
    assert_eq!(video.samples, 180);
    let span = video.end_100ns - video.start_100ns.expect("video start");
    assert!(
      span.abs_diff(last as u64) <= 2,
      "file timeline {span} against PTS span {last}"
    );
  }
  #[test]
  fn windows_compressed_sink_and_software_decoders_preserve_numbered_and_audio_markers() {
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let fixture = super::super::fixtures::media(3);
    let expected_audio_packets = fixture.audio.len() as u64;
    let t = tempfile::tempdir().expect("fixture directory");
    let path = t.path().join("native.mp4");
    let mut sink = Mux::new(
      &path,
      512,
      64,
      60,
      &fixture.video_extra,
      Some(&fixture.audio_extra),
    )
    .expect("native compressed sink");
    let first = fixture.video[0].clone();
    sink.write(first.clone()).expect("first packet");
    assert!(sink.write(first).is_err());
    let mut packets = fixture
      .video
      .into_iter()
      .skip(1)
      .chain(fixture.audio)
      .collect::<Vec<_>>();
    packets.sort_by_key(|p| p.pts);
    for packet in packets {
      sink.write(packet).expect("compressed packet");
    }
    sink.finish().expect("native finalization");
    let report =
      super::super::verify::verify(&path, true).expect("software decode without playback");
    assert_eq!(report.video_frames, 180);
    assert_eq!(report.numbered_frames_checked, 180);
    assert_eq!(report.marker_pairs, 3);
    assert!(report.accepted_sync);
    assert!(report.structure.decoded);
    report
      .structure
      .validate_packet_counts(180, expected_audio_packets)
      .expect("all submitted native packets retained");
    assert!(
      report
        .structure
        .validate_packet_counts(181, expected_audio_packets)
        .is_err(),
      "matching endpoints cannot hide a missing video packet"
    );
    assert!(
      report
        .structure
        .validate_packet_counts(180, expected_audio_packets + 1)
        .is_err(),
      "matching endpoints cannot hide a missing AAC packet"
    );
    report
      .structure
      .validate_recording(true, 60, 3 * crate::time::SECOND)
      .expect("decoded file also matches requested tracks and cutoff");
    assert!(
      report
        .structure
        .validate_recording(false, 60, 3 * crate::time::SECOND)
        .is_err()
    );
    assert!(
      report
        .structure
        .validate_recording(true, 60, 6 * crate::time::SECOND)
        .is_err(),
      "agreeing tracks cannot conceal a shortened recording"
    );
    for (from, to) in [(b"vide", b"soun"), (b"avc1", b"hvc1")] {
      let mut bytes = std::fs::read(&path).expect("actual native container");
      let position = bytes
        .windows(4)
        .position(|bytes| bytes == from)
        .expect("native track metadata");
      bytes[position..position + 4].copy_from_slice(to);
      let changed = t
        .path()
        .join(format!("wrong-{}.mp4", String::from_utf8_lossy(from)));
      std::fs::write(&changed, bytes).expect("changed metadata fixture");
      let inspected = crate::recovery::inspect(&changed).expect("still structurally valid");
      assert!(
        inspected
          .validate_recording(true, 60, 3 * crate::time::SECOND)
          .is_err(),
        "structural validity must not accept wrong stream identities"
      );
    }
    assert!(Mux::new(&path, 512, 64, 60, &fixture.video_extra, None).is_err());
  }
  #[test]
  fn mf_samples_reject_invalid_extents_and_round_trip_owned_buffers() {
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    assert!(sample(&[], 0, 1).is_err());
    assert!(sample(&[1], 0, 0).is_err());
    assert!(sample(&[1], 0, u64::MAX).is_err());
    let value = sample(&[1, 2, 3, 4], 37, 100).expect("owned buffer");
    assert_eq!(sample_bytes(&value).expect("round trip"), [1, 2, 3, 4]);
  }
  #[test]
  fn writer_thread_drains_a_silent_file_and_preserves_native_packet_errors() {
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let fixture = super::super::fixtures::media(1);
    let t = tempfile::tempdir().expect("fixture directory");
    let path = t.path().join("writer.mp4");
    let writer =
      Writer::start(&path, 512, 64, 60, fixture.video_extra.clone(), None).expect("writer owner");
    let metrics = writer.metrics_handle();
    for packet in fixture.video.iter().take(3) {
      writer.push(packet.clone()).expect("packet");
    }
    writer.finish().expect("bounded native finalize");
    let report = metrics
      .read()
      .expect("writer metrics")
      .expect("final report");
    assert!(report.finalized);
    assert_eq!(report.packets_written, 3);
    assert_eq!(report.queue_wait.count, 3);
    assert_eq!(report.native_write.count, 3);
    assert_eq!(report.native_finalize.count, 1);
    let report = super::super::verify::verify(&path, false).expect("decode silent output");
    assert_eq!(report.video_frames, 3);
    assert_eq!(report.structure.tracks, 1);
    assert_eq!(report.audio_frames, 0);
    let path = t.path().join("invalid.mp4");
    let writer = Writer::start(&path, 512, 64, 60, fixture.video_extra, None).expect("writer");
    let mut packet = fixture.video[0].clone();
    packet.dts = 17;
    assert_eq!(
      writer
        .push(packet)
        .expect_err("malformed packet rejected before queue admission")
        .kind,
      ErrorKind::Protocol
    );
    assert_eq!(
      writer
        .outstanding
        .load(std::sync::atomic::Ordering::Acquire),
      0
    );
    let mut packet = fixture.video[0].clone();
    packet.track = Track::Audio;
    writer
      .push(packet)
      .expect("valid packet extent, invalid for this silent container");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
      match writer.check() {
        Err(error) => {
          assert_eq!(error.kind, ErrorKind::Protocol);
          assert!(error.reason.contains("Audio packet for silent session"));
          break;
        },
        Ok(()) => {
          assert!(std::time::Instant::now() < deadline);
          std::thread::sleep(std::time::Duration::from_millis(1));
        },
      }
    }
  }
  #[test]
  fn padded_macroblock_heights_decode_at_their_displayed_size_across_a_type_change() {
    // Real 640x360 and 800x600 regions: the decoder renegotiates to padded rows.
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let fixture = super::super::fixtures::media_sized(1, 512, 72);
    let t = tempfile::tempdir().expect("fixture directory");
    let path = t.path().join("padded.mp4");
    let mut sink = Mux::new(&path, 512, 72, 60, &fixture.video_extra, None).expect("sink");
    for packet in fixture.video {
      sink.write(packet).expect("compressed packet");
    }
    sink.finish().expect("finalization");
    let report = super::super::verify::verify(&path, true).expect("decode across padding change");
    assert_eq!((report.video_width, report.video_height), (512, 72));
    assert_eq!(report.video_frames, 60);
    assert_eq!(report.numbered_frames_checked, 60);
  }
}
