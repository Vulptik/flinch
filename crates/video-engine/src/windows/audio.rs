use super::mux;
pub mod devices;
mod source;
use crate::watchdog::{Stage, Watchdog, Watched};
use crate::{
  EngineError, ErrorKind, Result,
  audio::{AudioPacket, EndpointClock, PcmBlock},
  time,
  types::*,
};
pub use source::{AudioSourceInfo, Encoding as AudioEncoding};
use std::{
  mem::ManuallyDrop,
  os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TrySendError},
  },
  thread,
};
use windows::{
  Win32::{
    Foundation::HANDLE,
    Media::{Audio::*, MediaFoundation::*},
    System::{Com::*, Threading::*},
  },
  core::{Interface, PCWSTR},
};

/// Which endpoint a capture records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
  /// What the speakers play (WASAPI loopback of a render endpoint).
  System,
  /// A microphone (a capture endpoint).
  Microphone,
}
pub struct AudioCapture {
  pub source: AudioSourceInfo,
  packets: Receiver<AudioPacket>,
  fatal: Receiver<EngineError>,
  completion: Receiver<Result<()>>,
  stop: Arc<AtomicBool>,
  paused: Arc<AtomicBool>,
  start: Option<mpsc::SyncSender<mpsc::SyncSender<Result<()>>>>,
}
impl AudioCapture {
  pub fn start(endpoint: Endpoint, device_id: Option<String>) -> Result<Self> {
    let mut capture = Self::prepare(endpoint, device_id)?;
    capture.begin()?;
    Ok(capture)
  }
  pub fn prepare(endpoint: Endpoint, device_id: Option<String>) -> Result<Self> {
    Self::prepare_with_watchdog(endpoint, device_id, Arc::new(Watchdog::default()))
  }
  pub(crate) fn prepare_with_watchdog(
    endpoint: Endpoint,
    device_id: Option<String>,
    watchdog: Arc<Watchdog>,
  ) -> Result<Self> {
    Self::prepare_factory(
      move || Wasapi::new(endpoint, device_id.as_deref()),
      endpoint,
      watchdog,
    )
  }
  #[cfg(test)]
  fn from_factory<F>(create: F) -> Result<Self>
  where
    F: FnOnce() -> Result<Wasapi> + Send + 'static,
  {
    let mut capture =
      Self::prepare_factory(create, Endpoint::System, Arc::new(Watchdog::default()))?;
    capture.begin()?;
    Ok(capture)
  }
  fn prepare_factory<F>(create: F, endpoint: Endpoint, watchdog: Arc<Watchdog>) -> Result<Self>
  where
    F: FnOnce() -> Result<Wasapi> + Send + 'static,
  {
    // Each endpoint has its own owner thread, so each has its own watchdog stage: one shared
    // stage refused the second owner's Start, and with it every recording with a microphone.
    let stage = endpoint.stage();
    let (sender, packets) = mpsc::sync_channel(100);
    // A terminal error must remain deliverable even when every PCM queue slot is occupied.
    let (failed, fatal) = mpsc::sync_channel(1);
    let (finished, completion) = mpsc::sync_channel(1);
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let (start, start_rx) = mpsc::sync_channel::<mpsc::SyncSender<Result<()>>>(1);
    let stop = Arc::new(AtomicBool::new(false));
    let request = stop.clone();
    let paused = Arc::new(AtomicBool::new(false));
    let pause = paused.clone();
    thread::Builder::new()
      .name("video WASAPI owner".into())
      .spawn(move || {
        let result = (|| {
          let startup = watchdog.begin(stage, crate::limits::PREPARE_TIMEOUT)?;
          let _com = Watched::new(super::Apartment::new()?, watchdog.clone(), stage);
          let mut capture = Watched::new(create()?, watchdog.clone(), stage);
          if request.load(Ordering::Acquire) {
            return Err(EngineError::new(
              ErrorKind::Cancelled,
              "WASAPI startup",
              "Audio preparation was cancelled",
            ));
          }
          drop(startup);
          ready_tx
            .send(Ok(capture.source.clone()))
            .map_err(|_| EngineError::protocol("Audio startup receiver closed"))?;
          // Initialization negotiates the fixed endpoint on this owner, without acquiring audio.
          // Cancellation remains finite while Ready; only an explicit Start calls IAudioClient::Start.
          loop {
            if request.load(Ordering::Acquire) {
              return Ok(());
            }
            match start_rx.recv_timeout(std::time::Duration::from_millis(20)) {
              Ok(reply) => {
                // Every failure here answers Start, so the caller reports its real cause instead
                // of a closed reply channel.
                let result = watchdog
                  .begin(stage, crate::limits::START_TIMEOUT)
                  .and_then(|native| {
                    let started = capture.start();
                    drop(native);
                    started
                  })
                  .inspect_err(|error| watchdog.remember(error));
                reply.try_send(result.clone()).map_err(|_| {
                  EngineError::protocol("Audio Start acknowledgement receiver closed")
                })?;
                result?;
                break;
              },
              Err(mpsc::RecvTimeoutError::Timeout) => {},
              Err(error) => {
                return Err(EngineError::protocol(format!(
                  "Audio Start disconnected: {error}"
                )));
              },
            }
          }
          while !request.load(Ordering::Acquire) {
            let _native = watchdog.begin(stage, crate::limits::CONTROL_TIMEOUT)?;
            // SAFETY: event handle belongs to capture and remains live; finite wait bounds cancellation.
            let wait = unsafe { WaitForSingleObject(HANDLE(capture.event.as_raw_handle()), 20) };
            if wait == windows::Win32::Foundation::WAIT_FAILED {
              return Err(EngineError::native(
                "WASAPI wait",
                windows::core::Error::from_thread(),
              ));
            }
            for _ in 0..100 {
              if request.load(Ordering::Acquire) {
                break;
              }
              let Some(packet) = capture
                .next()
                .inspect_err(|error| watchdog.remember(error))?
              else {
                break;
              };
              if pause.load(Ordering::Acquire) {
                continue;
              }
              match sender.try_send(packet) {
                Ok(()) => {},
                Err(TrySendError::Disconnected(_)) => {
                  return Err(EngineError::protocol("Audio producer disconnected"));
                },
                Err(TrySendError::Full(_)) => {
                  return Err(EngineError::new(
                    ErrorKind::Saturation,
                    "WASAPI",
                    "One-second audio queue exhausted",
                  ));
                },
              }
            }
          }
          let _native = watchdog.begin(stage, crate::limits::ABORT_TIMEOUT)?;
          capture
            .stop()
            .inspect_err(|error| watchdog.remember(error))?;
          Ok(())
        })();
        if let Err(e) = &result {
          if ready_tx.try_send(Err(e.clone())).is_err() {
            eprintln!("Audio owner error: {e}");
          }
          if failed.try_send(e.clone()).is_err() {
            eprintln!("Audio terminal receiver closed");
          }
        }
        // Completion is independent of packet/fatal consumption and follows native teardown.
        // Disconnection is normal when Drop cancels an abandoned owner; failures were reported above.
        if let Err(mpsc::TrySendError::Full(_)) = finished.try_send(result) {
          eprintln!("WASAPI completion channel unexpectedly occupied");
        }
      })
      .map_err(|e| EngineError::io("audio thread", e))?;
    let prepared = ready
      .recv_timeout(crate::limits::PREPARE_TIMEOUT)
      .map_err(|e| EngineError::new(ErrorKind::Timeout, "WASAPI ready", e.to_string()))
      .and_then(|result| result);
    let source = match prepared {
      Ok(source) => source,
      Err(error) => {
        stop.store(true, Ordering::Release);
        return Err(error);
      },
    };
    Ok(Self {
      source,
      packets,
      fatal,
      completion,
      stop,
      paused,
      start: Some(start),
    })
  }
  pub fn begin(&mut self) -> Result<()> {
    if let Ok(error) = self.fatal.try_recv() {
      return Err(error);
    }
    if self.stop.load(Ordering::Acquire) {
      return Err(EngineError::new(
        ErrorKind::Cancelled,
        "WASAPI Start",
        "Audio owner was cancelled",
      ));
    }
    let start = self
      .start
      .take()
      .ok_or_else(|| EngineError::protocol("Audio source Start already requested"))?;
    let (reply, answer) = mpsc::sync_channel(1);
    let result = start
      .try_send(reply)
      .map_err(|error| EngineError::protocol(format!("Audio Start delivery: {error}")))
      .and_then(|_| {
        answer
          .recv_timeout(crate::limits::START_TIMEOUT)
          .map_err(|error| {
            EngineError::new(
              if matches!(error, mpsc::RecvTimeoutError::Timeout) {
                ErrorKind::Timeout
              } else {
                ErrorKind::Io
              },
              "WASAPI Start",
              error.to_string(),
            )
          })
          .and_then(|result| result)
      });
    if result.is_err() {
      self.stop.store(true, Ordering::Release);
    }
    result.map_err(|error| self.fatal.try_recv().unwrap_or(error))
  }
  pub fn set_paused(&self, paused: bool) {
    self.paused.store(paused, Ordering::Release);
  }
  pub fn stop(&self) {
    self.stop.store(true, Ordering::Release);
  }
  pub fn finish(self) -> Result<()> {
    self.finish_with_timeout(crate::limits::CONTROL_TIMEOUT)
  }
  fn finish_with_timeout(self, timeout: std::time::Duration) -> Result<()> {
    self.stop();
    self.completion.recv_timeout(timeout).map_err(|error| {
      self.fatal.try_recv().unwrap_or_else(|_| {
        EngineError::new(
          if matches!(error, mpsc::RecvTimeoutError::Timeout) {
            ErrorKind::Timeout
          } else {
            ErrorKind::Io
          },
          "WASAPI shutdown",
          error.to_string(),
        )
      })
    })?
  }
  pub fn try_packet(&self) -> Result<Option<AudioPacket>> {
    if let Ok(error) = self.fatal.try_recv() {
      return Err(error);
    }
    match self.packets.try_recv() {
      Ok(packet) => Ok(Some(packet)),
      Err(mpsc::TryRecvError::Empty) => Ok(None),
      Err(mpsc::TryRecvError::Disconnected) => {
        // The producer publishes its cause before dropping its packet sender.
        Err(self.fatal.try_recv().unwrap_or_else(|_| {
          EngineError::new(
            ErrorKind::DeviceLost,
            "audio producer",
            "PCM producer disconnected",
          )
        }))
      },
    }
  }
}
impl Drop for AudioCapture {
  fn drop(&mut self) {
    self.stop();
  }
}
struct Wasapi {
  source: AudioSourceInfo,
  client: IAudioClient,
  capture: IAudioCaptureClient,
  clock: IAudioClock2,
  event: OwnedHandle,
  dating: EndpointClock,
  device_rate: u32,
  position_rate: u32,
  pcm: Receiver<Vec<f32>>,
  recycle: std::sync::mpsc::SyncSender<Vec<f32>>,
  packet_capacity: u32,
  running: bool,
  hardware_clock_reported: bool,
}
impl Wasapi {
  fn new(endpoint: Endpoint, id: Option<&str>) -> Result<Self> {
    // SAFETY: caller owns an initialized MTA; all interfaces remain on this thread.
    unsafe {
      let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .map_err(|e| EngineError::native("audio devices", e))?;
      let wide = id.map(|s| s.encode_utf16().chain([0]).collect::<Vec<_>>());
      let device = if let Some(wide) = &wide {
        enumerator.GetDevice(PCWSTR(wide.as_ptr()))
      } else {
        let flow = match endpoint {
          Endpoint::System => eRender,
          Endpoint::Microphone => eCapture,
        };
        enumerator.GetDefaultAudioEndpoint(flow, eConsole)
      }
      .map_err(|e| endpoint_error(endpoint, "audio endpoint", e))?;
      if endpoint == Endpoint::Microphone {
        let state = device
          .GetState()
          .map_err(|e| endpoint_error(endpoint, "microphone state", e))?;
        if state != DEVICE_STATE_ACTIVE {
          return Err(EngineError::new(
            ErrorKind::DeviceLost,
            MICROPHONE_MISSING_STAGE,
            "The microphone is not connected",
          ));
        }
      }
      Self::from_device(device, endpoint)
    }
  }
  fn from_device(device: IMMDevice, endpoint: Endpoint) -> Result<Self> {
    // SAFETY: the supplied endpoint belongs to this initialized owner apartment.
    unsafe {
      let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| endpoint_error(endpoint, "audio client", e))?;
      let device_rate = endpoint_rate(&device)?;
      let mut source = AudioSourceInfo::query(&device, &client, device_rate)?;
      let format = WAVEFORMATEX {
        wFormatTag: 1,
        nChannels: 2,
        nSamplesPerSec: 48000,
        nAvgBytesPerSec: 192000,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
      };
      // A microphone is captured directly; system audio is the loopback of a render endpoint.
      // Both convert to the same 48 kHz stereo client format (a mono microphone is duplicated).
      let loopback = match endpoint {
        Endpoint::System => AUDCLNT_STREAMFLAGS_LOOPBACK,
        Endpoint::Microphone => 0,
      };
      client
        .Initialize(
          AUDCLNT_SHAREMODE_SHARED,
          loopback
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
          0, // Native shared-mode minimum; a 100 ms buffer wastes pooled capacity on 10 ms packets.
          0,
          &format,
          None,
        )
        .map_err(|e| endpoint_error(endpoint, "WASAPI initialize", e))?;
      let event = CreateEventW(None, false, false, None)
        .map_err(|e| EngineError::native("audio event", e))?;
      // SAFETY: successful CreateEventW transfers one unique owning handle, including all error paths.
      let event = OwnedHandle::from_raw_handle(event.0);
      client
        .SetEventHandle(HANDLE(event.as_raw_handle()))
        .map_err(|e| EngineError::native("audio event handle", e))?;
      let capture: IAudioCaptureClient = client
        .GetService()
        .map_err(|e| EngineError::native("audio capture service", e))?;
      let clock: IAudioClock = client
        .GetService()
        .map_err(|e| EngineError::native("audio clock service", e))?;
      let clock: IAudioClock2 = clock
        .cast()
        .map_err(|e| EngineError::native("raw audio clock", e))?;
      let packet_capacity = client
        .GetBufferSize()
        .map_err(|e| EngineError::native("WASAPI buffer capacity", e))?;
      if packet_capacity == 0 || packet_capacity > 48000 {
        return Err(EngineError::new(
          ErrorKind::Capability,
          "WASAPI buffer capacity",
          "Endpoint buffer exceeds the one-second PCM budget",
        ));
      }
      let count = (48000 / packet_capacity).min(100) as usize;
      source.packet_capacity_frames = packet_capacity;
      let (recycle, pcm) = mpsc::sync_channel(count);
      for _ in 0..count {
        recycle
          .try_send(Vec::with_capacity(packet_capacity as usize * 2))
          .map_err(|e| EngineError::protocol(format!("PCM pool preparation: {e}")))?;
      }
      Ok(Self {
        position_rate: source.packet_position_rate,
        source,
        client,
        capture,
        clock,
        event,
        dating: EndpointClock::default(),
        device_rate,
        pcm,
        recycle,
        packet_capacity,
        running: false,
        hardware_clock_reported: false,
      })
    }
  }
  fn start(&mut self) -> Result<()> {
    // SAFETY: this is the prepared client's sole owner; Start is paired with Stop even on later failures.
    unsafe { self.client.Start() }.map_err(|e| EngineError::native("WASAPI start", e))?;
    self.running = true;
    Ok(())
  }
  fn stop(&mut self) -> Result<()> {
    if !self.running {
      return Ok(());
    }
    self.running = false;
    // SAFETY: finite cancellation is process-supervised; no other thread calls this client's Stop.
    unsafe { self.client.Stop() }.map_err(|e| EngineError::native("WASAPI stop", e))
  }
  fn next(&mut self) -> Result<Option<AudioPacket>> {
    // SAFETY: endpoint APIs are serialized on their creating thread; every acquired packet is released.
    unsafe {
      if self
        .capture
        .GetNextPacketSize()
        .map_err(|e| EngineError::native("audio packet size", e))?
        == 0
      {
        return Ok(None);
      }
      let mut reported = 0;
      let mut data = std::ptr::null_mut();
      let mut frames = 0;
      let mut flags = 0;
      let mut position = 0;
      self
        .capture
        .GetBuffer(
          &mut data,
          &mut frames,
          &mut flags,
          Some(&mut position),
          Some(&mut reported),
        )
        .map_err(|e| EngineError::native("WASAPI packet", e))?;

      let work = (|| {
        // GetBuffer owns the native packet/QPC pair. The separate physical clock
        // can remain unavailable for loopback and must never redate that packet.
        // Clock-query errors still release the acquired native buffer below.
        let before = super::qpc()?;
        let mut raw = 0;
        let mut clock_reported = 0;
        let status = (Interface::vtable(&self.clock).GetDevicePosition)(
          self.clock.as_raw(),
          &mut raw,
          &mut clock_reported,
        );
        status
          .ok()
          .map_err(|e| EngineError::native("endpoint position", e))?;
        let after = super::qpc()?;
        if !self.hardware_clock_reported {
          self.hardware_clock_reported = true;
          if status == AUDCLNT_S_POSITION_STALLED || (raw == 0 && clock_reported == 0) {
            eprintln!(
              "WASAPI timing: native packet QPC; raw hardware clock unavailable (status={})",
              status.0
            );
          }
        }
        if frames > self.packet_capacity {
          return Err(EngineError::new(
            ErrorKind::Saturation,
            "audio packet",
            "Packet exceeds its negotiated preallocated capacity",
          ));
        }
        let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
        let mut samples = self
          .pcm
          .try_recv()
          .map_err(|e| EngineError::new(ErrorKind::Saturation, "WASAPI PCM pool", e.to_string()))?;
        samples.resize(frames as usize * 2, 0.0);
        // Lease immediately: every later error returns this allocation to the pool.
        let mut samples = PcmBlock::pooled(samples, self.recycle.clone());
        if silent {
          samples.fill(0.0);
        }
        if !silent {
          if data.is_null() {
            return Err(EngineError::protocol("Null non-silent audio packet"));
          }
          let bytes = std::slice::from_raw_parts(data, frames as usize * 4);
          for (sample, pair) in samples.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *sample = f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32768.0;
          }
        }
        let packet_qpc = reported;
        if packet_qpc > after.saturating_add(time::SECOND) {
          return Err(EngineError::protocol(format!(
            "WASAPI packet exceeds the one-second future bound: packet_position={position}, clock_position={raw}, rate={}, before_qpc={before}, after_qpc={after}, clock_reported_qpc={clock_reported}, packet_reported_qpc={reported}, dated_qpc={packet_qpc}, flags={flags}",
            self.device_rate
          )));
        }
        if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 || packet_qpc == 0 {
          return Err(EngineError::new(
            ErrorKind::DeviceLost,
            "WASAPI packet timestamp",
            format!(
              "Invalid native packet timestamp: position={position}, qpc={reported}, flags={flags}"
            ),
          ));
        }
        // GetBuffer positions count the shared engine's mix frames (see AudioSourceInfo),
        // independently of the 48 kHz client format and the IAudioClock2 origin.
        // Native packet QPC dates the media; local read time is only a safety bound.
        self
          .dating
          .observe(position, self.position_rate, packet_qpc, packet_qpc)
          .map_err(|mut error| {
            error.reason.push_str(&format!(
              "; packet position={position}, frames={frames}, flags={flags}"
            ));
            error
          })?;
        Ok(Some(AudioPacket {
          data: samples,
          raw_device_position: position,
          device_rate: self.device_rate,
          position_rate: self.position_rate,
          qpc_100ns: packet_qpc,
          read_qpc_100ns: Some(before),
          client_rate: 48000,
          silent,
          discontinuity: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
          wasapi_flags: flags,
          drift_ppm: self.dating.drift_ppm,
        }))
      })();
      let release = self
        .capture
        .ReleaseBuffer(frames)
        .map_err(|e| EngineError::native("audio packet release", e));
      match (work, release) {
        (Err(mut original), Err(cleanup)) => {
          original.reason.push_str(&format!("; cleanup: {cleanup}"));
          Err(original)
        },
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Ok(packet), Ok(())) => Ok(packet),
      }
    }
  }
}

/// A microphone Windows' privacy settings block, or one that is missing, gets a stage the app
/// recognizes and explains; other failures keep their native cause.
fn endpoint_error(endpoint: Endpoint, stage: &str, error: windows::core::Error) -> EngineError {
  let code = error.code();
  if endpoint == Endpoint::Microphone {
    if code == windows::Win32::Foundation::E_ACCESSDENIED {
      return EngineError {
        native_code: Some(code.0),
        ..EngineError::new(
          ErrorKind::Capability,
          MICROPHONE_BLOCKED_STAGE,
          "Windows privacy settings block microphone access",
        )
      };
    }
    // E_NOTFOUND (no microphone, or an unknown id) and a removed endpoint.
    if code == windows::core::HRESULT(0x8007_0490_u32 as i32)
      || code == AUDCLNT_E_DEVICE_INVALIDATED
    {
      return EngineError {
        native_code: Some(code.0),
        ..EngineError::new(
          ErrorKind::DeviceLost,
          MICROPHONE_MISSING_STAGE,
          "The microphone is not connected",
        )
      };
    }
  }
  EngineError::native(stage, error)
}

impl Drop for Wasapi {
  fn drop(&mut self) {
    if let Err(error) = self.stop() {
      eprintln!("WASAPI cleanup: {error}");
    }
  }
}

fn endpoint_rate(device: &IMMDevice) -> Result<u32> {
  use windows::Win32::System::{Com::StructuredStorage::PropVariantClear, Variant::VT_BLOB};
  // SAFETY: read-only physical endpoint format, independent of client mix format.
  let store = unsafe { device.OpenPropertyStore(STGM_READ) }
    .map_err(|e| EngineError::native("endpoint properties", e))?;
  // SAFETY: returned PROPVARIANT allocation is owned and cleared once below.
  let mut value = unsafe { store.GetValue(&PKEY_AudioEngine_DeviceFormat) }
    .map_err(|e| EngineError::native("endpoint physical format", e))?;
  let parsed = (|| {
    // SAFETY: inspect tag before selecting the blob union member.
    let fields = unsafe { &value.Anonymous.Anonymous };
    if fields.vt != VT_BLOB {
      return Err(EngineError::protocol("Endpoint format is not a blob"));
    }
    // SAFETY: VT_BLOB identifies this bounded allocation.
    let blob = unsafe { fields.Anonymous.blob };
    if blob.pBlobData.is_null() || !(18..=4096).contains(&blob.cbSize) {
      return Err(EngineError::protocol("Invalid endpoint format allocation"));
    }
    // SAFETY: blob pointer and size were checked and remain owned until clear.
    let bytes = unsafe { std::slice::from_raw_parts(blob.pBlobData, blob.cbSize as usize) };
    let rate = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if !(8000..=384000).contains(&rate) {
      return Err(EngineError::protocol("Invalid physical audio sample rate"));
    }
    Ok(rate)
  })();
  // SAFETY: exactly one clear after all references into the variant have expired.
  let clear = unsafe { PropVariantClear(&mut value) }
    .map_err(|e| EngineError::native("endpoint format release", e));
  match (parsed, clear) {
    (Ok(rate), Ok(())) => Ok(rate),
    (Err(original), Ok(())) => Err(original),
    (Ok(_), Err(error)) => Err(error),
    (Err(original), Err(error)) => Err(EngineError::protocol(format!(
      "{original}; cleanup: {error}"
    ))),
  }
}

pub struct AacEncoder {
  transform: IMFTransform,
  pub extra: Vec<u8>,
}
impl AacEncoder {
  pub fn new() -> Result<Self> {
    // SAFETY: caller owns COM/MF guards; synchronous Microsoft AAC transform is private to this owner.
    unsafe {
      let transform: IMFTransform = CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER)
        .map_err(|e| EngineError::native("AAC encoder", e))?;
      transform
        .SetInputType(0, &mux::audio_type(false)?, 0)
        .map_err(|e| EngineError::native("AAC PCM input", e))?;
      transform
        .SetOutputType(0, &mux::audio_type(true)?, 0)
        .map_err(|e| EngineError::native("AAC output", e))?;
      let output = transform
        .GetOutputCurrentType(0)
        .map_err(|e| EngineError::native("AAC negotiated type", e))?;
      let size = output
        .GetBlobSize(&MF_MT_USER_DATA)
        .map_err(|e| EngineError::native("AAC extra size", e))?;
      if size == 0 || size > 4096 {
        return Err(EngineError::protocol("Invalid AAC extra data"));
      }
      let mut extra = vec![0; size as usize];
      output
        .GetBlob(&MF_MT_USER_DATA, &mut extra, None)
        .map_err(|e| EngineError::native("AAC extra", e))?;
      transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        .map_err(|e| EngineError::native("AAC begin", e))?;
      transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        .map_err(|e| EngineError::native("AAC start", e))?;
      Ok(Self { transform, extra })
    }
  }
  pub fn input(&mut self, pcm: &[i16], first_frame: u64) -> Result<()> {
    if !pcm.len().is_multiple_of(2) {
      return Err(EngineError::protocol("AAC input not stereo"));
    }
    let frames = pcm.len() / 2;
    if frames == 0 {
      return Ok(());
    }
    let last = first_frame
      .checked_add(frames as u64)
      .ok_or_else(|| EngineError::protocol("AAC sample index overflow"))?;
    let pts = i64::try_from(u128::from(first_frame) * u128::from(time::SECOND) / 48000)
      .map_err(|_| EngineError::protocol("AAC PTS exceeds native range"))?;
    let end = i64::try_from(u128::from(last) * u128::from(time::SECOND) / 48000)
      .map_err(|_| EngineError::protocol("AAC end exceeds native range"))?;
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for sample in pcm {
      bytes.extend(sample.to_le_bytes());
    }
    let sample = mux::sample(&bytes, pts, (end - pts) as u64)?;
    // SAFETY: nonzero duration and validated PCM type, input is retained by COM if needed.
    unsafe { self.transform.ProcessInput(0, &sample, 0) }
      .map_err(|e| EngineError::native("AAC input", e))
  }
  pub fn output(&mut self) -> Result<Option<EncodedPacket>> {
    // SAFETY: output sample allocation follows the MFT's documented stream flags and size.
    unsafe {
      let info = self
        .transform
        .GetOutputStreamInfo(0)
        .map_err(|e| EngineError::native("AAC output info", e))?;
      if info.cbSize > 16 * 1024 * 1024 {
        return Err(EngineError::protocol("AAC output allocation too large"));
      }
      let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
        let sample = MFCreateSample().map_err(|e| EngineError::native("AAC sample", e))?;
        let buffer = MFCreateMemoryBuffer(info.cbSize.max(4096))
          .map_err(|e| EngineError::native("AAC output buffer", e))?;
        sample
          .AddBuffer(&buffer)
          .map_err(|e| EngineError::native("AAC output attach", e))?;
        Some(sample)
      } else {
        None
      };
      let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(sample),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
      }];
      let mut status = 0;
      let result = self.transform.ProcessOutput(0, &mut outputs, &mut status);
      let sample = ManuallyDrop::take(&mut outputs[0].pSample);
      let events = ManuallyDrop::take(&mut outputs[0].pEvents);
      drop(events);
      match result {
        Ok(()) => {},
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
        Err(e) => return Err(EngineError::native("AAC output", e)),
      }
      let sample = sample.ok_or_else(|| EngineError::protocol("AAC output returned no sample"))?;
      let pts = sample
        .GetSampleTime()
        .map_err(|e| EngineError::native("AAC PTS", e))?;
      let duration = sample
        .GetSampleDuration()
        .map_err(|e| EngineError::native("AAC duration", e))?;
      if duration <= 0 {
        return Err(EngineError::protocol("AAC output duration is invalid"));
      }
      Ok(Some(EncodedPacket {
        track: Track::Audio,
        pts,
        dts: pts,
        duration: duration as u64,
        keyframe: true,
        data: mux::sample_bytes(&sample)?,
      }))
    }
  }
  pub fn drain(&self) -> Result<()> {
    // SAFETY: no further PCM will be supplied after drain.
    unsafe {
      self
        .transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        .map_err(|e| EngineError::native("AAC end", e))?;
      self
        .transform
        .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
        .map_err(|e| EngineError::native("AAC drain", e))?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod mock;
impl Endpoint {
  /// The watchdog stage of this endpoint's owner thread.
  fn stage(self) -> Stage {
    match self {
      Self::System => Stage::AudioSource,
      Self::Microphone => Stage::MicrophoneSource,
    }
  }
}
