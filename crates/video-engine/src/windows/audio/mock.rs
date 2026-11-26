//! WASAPI endpoint doubles. No enumerator, playback or physical endpoint is opened.
#![allow(clippy::expect_used)]
use super::*;
use std::sync::{
  Mutex,
  atomic::{AtomicIsize, AtomicU32},
};
use windows::Win32::{
  Foundation::{E_FAIL, E_NOTIMPL, E_OUTOFMEMORY, PROPERTYKEY},
  System::{Com::StructuredStorage::PROPVARIANT, Variant::*},
  UI::Shell::PropertiesSystem::*,
};

use windows_core::implement;
struct Shared {
  /// Whether the client was initialized for loopback (system audio) or direct capture.
  loopback: std::sync::atomic::AtomicBool,
  event: AtomicIsize,
  pending: AtomicU32,
  frames: u32,
  releases: AtomicU32,
  acquires: AtomicU32,
  cancel_on_acquire: Mutex<Option<Arc<AtomicBool>>>,
  stops: AtomicU32,
  starts: AtomicU32,
  start_failure: AtomicBool,
  stop_failure: AtomicBool,
  flags: AtomicU32,
  null_data: AtomicBool,
  device_lost: AtomicBool,
  release_failure: AtomicBool,
  service_failure: AtomicBool,
  rate: u32,
  channels: u16,
  mix_mask: AtomicU32,
  mix_float: AtomicBool,
  future_packet: AtomicBool,
  clock_waits_for_buffer: AtomicBool,
  stalled_clock: AtomicBool,
  clock_failure: AtomicBool,
  mix_failure: AtomicU32,
  identity_mode: AtomicU32,
  data: Vec<u8>,
  unexpected: Mutex<Vec<&'static str>>,
}
#[implement(IMMDevice)]
struct Device(Arc<Shared>);
impl IMMDevice_Impl for Device_Impl {
  fn Activate(
    &self,
    _iid: *const windows_core::GUID,
    _dwclsctx: windows::Win32::System::Com::CLSCTX,
    _pactivationparams: *const windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
    _ppinterface: *mut *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    assert_eq!(_dwclsctx, CLSCTX_ALL);
    assert!(_pactivationparams.is_null());
    let client: IAudioClient = Client(self.0.clone()).into();
    // SAFETY: QI forwards the caller's output slot and transfers its own COM reference.
    unsafe { client.query(_iid, _ppinterface).ok() }
  }
  fn OpenPropertyStore(
    &self,
    _stgmaccess: windows::Win32::System::Com::STGM,
  ) -> windows_core::Result<IPropertyStore> {
    assert_eq!(_stgmaccess, STGM_READ);
    Ok(Properties(self.0.clone()).into())
  }
  fn GetId(&self) -> windows_core::Result<windows_core::PWSTR> {
    let id: Vec<u16> = match self.0.identity_mode.load(Ordering::Acquire) {
      1 => return Ok(windows_core::PWSTR::null()),
      2 => vec![0],
      3 => vec![0xd800, 0],
      4 => return Err(E_FAIL.into()),
      5 => std::iter::repeat_n(65, 4096).chain([0]).collect(),
      _ => "mock-render-endpoint".encode_utf16().chain([0]).collect(),
    };
    // SAFETY: ownership of the COM-task allocation transfers to the caller, matching IMMDevice::GetId.
    unsafe {
      let memory = CoTaskMemAlloc(id.len() * 2).cast::<u16>();
      if memory.is_null() {
        return Err(E_OUTOFMEMORY.into());
      }
      std::ptr::copy_nonoverlapping(id.as_ptr(), memory, id.len());
      Ok(windows_core::PWSTR(memory))
    }
  }
  fn GetState(&self) -> windows_core::Result<DEVICE_STATE> {
    Err(E_NOTIMPL.into())
  }
}
#[implement(IAudioClient)]
struct Client(Arc<Shared>);
impl IAudioClient_Impl for Client_Impl {
  fn Initialize(
    &self,
    _sharemode: AUDCLNT_SHAREMODE,
    _streamflags: u32,
    _hnsbufferduration: i64,
    _hnsperiodicity: i64,
    _pformat: *const WAVEFORMATEX,
    _audiosessionguid: *const windows_core::GUID,
  ) -> windows_core::Result<()> {
    assert_eq!(_sharemode, AUDCLNT_SHAREMODE_SHARED);
    assert_eq!(
      _streamflags & !AUDCLNT_STREAMFLAGS_LOOPBACK,
      AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
    );
    self.0.loopback.store(
      _streamflags & AUDCLNT_STREAMFLAGS_LOOPBACK != 0,
      std::sync::atomic::Ordering::Release,
    );
    assert_eq!((_hnsbufferduration, _hnsperiodicity), (0, 0));
    assert!(_audiosessionguid.is_null());
    // SAFETY: the caller supplies a live WAVEFORMATEX for this call.
    let format = unsafe { _pformat.read_unaligned() };
    let actual = (
      format.wFormatTag,
      format.nChannels,
      format.nSamplesPerSec,
      format.nBlockAlign,
      format.wBitsPerSample,
    );
    assert_eq!(actual, (1, 2, 48000, 4, 16));
    Ok(())
  }
  fn GetBufferSize(&self) -> windows_core::Result<u32> {
    Ok(4800)
  }
  fn GetStreamLatency(&self) -> windows_core::Result<i64> {
    Err(E_NOTIMPL.into())
  }
  fn GetCurrentPadding(&self) -> windows_core::Result<u32> {
    Err(E_NOTIMPL.into())
  }
  fn IsFormatSupported(
    &self,
    _sharemode: AUDCLNT_SHAREMODE,
    _pformat: *const WAVEFORMATEX,
    _ppclosestmatch: *mut *mut WAVEFORMATEX,
  ) -> windows_core::HRESULT {
    E_NOTIMPL
  }
  fn GetMixFormat(&self) -> windows_core::Result<*mut WAVEFORMATEX> {
    match self.0.mix_failure.load(Ordering::Acquire) {
      1 => return Ok(std::ptr::null_mut()),
      2 => return Err(E_FAIL.into()),
      _ => {},
    }
    // SAFETY: the COM-owned result is transferred to the caller, which frees it once.
    unsafe {
      let mask = self.0.mix_mask.load(Ordering::Acquire);
      let floating = self.0.mix_float.load(Ordering::Acquire);
      let bits = if floating { 32 } else { 16 };
      let size = if mask != 0 {
        std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
      } else {
        std::mem::size_of::<WAVEFORMATEX>()
      };
      let memory = CoTaskMemAlloc(size).cast::<WAVEFORMATEX>();
      if memory.is_null() {
        return Err(E_OUTOFMEMORY.into());
      }
      let format = WAVEFORMATEX {
        wFormatTag: if mask != 0 {
          0xfffe
        } else if floating {
          3
        } else {
          1
        },
        nChannels: self.0.channels,
        nSamplesPerSec: 48000,
        nAvgBytesPerSec: 48000 * u32::from(self.0.channels) * u32::from(bits / 8),
        nBlockAlign: self.0.channels * (bits / 8),
        wBitsPerSample: bits,
        cbSize: if mask != 0 { 22 } else { 0 },
      };
      if mask != 0 {
        memory
          .cast::<WAVEFORMATEXTENSIBLE>()
          .write_unaligned(WAVEFORMATEXTENSIBLE {
            Format: format,
            Samples: WAVEFORMATEXTENSIBLE_0 {
              wValidBitsPerSample: bits,
            },
            dwChannelMask: mask,
            SubFormat: if floating {
              MFAudioFormat_Float
            } else {
              MFAudioFormat_PCM
            },
          });
      } else {
        memory.write_unaligned(format);
      }
      Ok(memory)
    }
  }
  fn GetDevicePeriod(
    &self,
    _phnsdefaultdeviceperiod: *mut i64,
    _phnsminimumdeviceperiod: *mut i64,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn Start(&self) -> windows_core::Result<()> {
    self.0.starts.fetch_add(1, Ordering::AcqRel);
    if self.0.start_failure.load(Ordering::Acquire) {
      return Err(E_FAIL.into());
    }
    // SAFETY: the owner keeps this exact event alive until after Stop.
    unsafe { SetEvent(HANDLE(self.0.event.load(Ordering::Acquire) as *mut _)) }
  }
  fn Stop(&self) -> windows_core::Result<()> {
    self.0.stops.fetch_add(1, Ordering::AcqRel);
    if self.0.stop_failure.load(Ordering::Acquire) {
      return Err(E_FAIL.into());
    }
    Ok(())
  }
  fn Reset(&self) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn SetEventHandle(
    &self,
    _eventhandle: windows::Win32::Foundation::HANDLE,
  ) -> windows_core::Result<()> {
    self
      .0
      .event
      .store(_eventhandle.0 as isize, Ordering::Release);
    Ok(())
  }
  fn GetService(
    &self,
    _riid: *const windows_core::GUID,
    _ppv: *mut *mut core::ffi::c_void,
  ) -> windows_core::Result<()> {
    if self.0.service_failure.load(Ordering::Acquire) {
      return Err(E_FAIL.into());
    }
    let source: IAudioCaptureClient = Source(self.0.clone()).into();
    // SAFETY: QI forwards the requested interface and transfers its own reference.
    unsafe { source.query(_riid, _ppv).ok() }
  }
}
#[implement(IAudioCaptureClient, IAudioClock, IAudioClock2)]
struct Source(Arc<Shared>);
impl IAudioCaptureClient_Impl for Source_Impl {
  fn GetBuffer(
    &self,
    _ppdata: *mut *mut u8,
    _pnumframestoread: *mut u32,
    _pdwflags: *mut u32,
    _pu64deviceposition: *mut u64,
    _pu64qpcposition: *mut u64,
  ) -> windows_core::Result<()> {
    self.0.acquires.fetch_add(1, Ordering::AcqRel);
    if let Some(cancel) = &*self
      .0
      .cancel_on_acquire
      .lock()
      .expect("scripted cancellation")
    {
      cancel.store(true, Ordering::Release);
    }
    // SAFETY: required output slots are provided; storage lives in the shared fixture.
    unsafe {
      *_ppdata = if self.0.null_data.load(Ordering::Acquire) {
        std::ptr::null_mut()
      } else {
        self.0.data.as_ptr().cast_mut()
      };
      *_pnumframestoread = self.0.frames;
      *_pdwflags = self.0.flags.load(Ordering::Acquire);
      if !_pu64deviceposition.is_null() {
        *_pu64deviceposition = 600000
          + if self.0.future_packet.load(Ordering::Acquire) {
            u64::from(self.0.rate) * 2
          } else {
            0
          };
      }
      // Native packet QPC has its documented epoch. The diagnostic hardware
      // clock below deliberately has a different, unusable QPC origin.
      if !_pu64qpcposition.is_null() {
        let now = super::super::qpc().expect("native packet QPC");
        *_pu64qpcposition = if self.0.future_packet.load(Ordering::Acquire) {
          now + time::SECOND * 2
        } else {
          now - 100000
        };
      }
    }
    Ok(())
  }
  fn ReleaseBuffer(&self, _numframesread: u32) -> windows_core::Result<()> {
    assert!(_numframesread == 0 || _numframesread == self.0.frames);
    self.0.releases.fetch_add(1, Ordering::AcqRel);
    if _numframesread == 0 {
      return Ok(());
    }
    assert!(self.0.pending.fetch_sub(1, Ordering::AcqRel) > 0);
    if self.0.release_failure.load(Ordering::Acquire) {
      Err(E_FAIL.into())
    } else {
      Ok(())
    }
  }
  fn GetNextPacketSize(&self) -> windows_core::Result<u32> {
    if self.0.device_lost.load(Ordering::Acquire) {
      return Err(AUDCLNT_E_DEVICE_INVALIDATED.into());
    }
    Ok(if self.0.pending.load(Ordering::Acquire) == 0 {
      0
    } else {
      self.0.frames
    })
  }
}
impl IAudioClock_Impl for Source_Impl {
  fn GetFrequency(&self) -> windows_core::Result<u64> {
    self
      .0
      .unexpected
      .lock()
      .expect("calls")
      .push("client GetFrequency");
    Err(E_NOTIMPL.into())
  }
  fn GetPosition(
    &self,
    _pu64position: *mut u64,
    _pu64qpcposition: *mut u64,
  ) -> windows_core::Result<()> {
    self
      .0
      .unexpected
      .lock()
      .expect("calls")
      .push("client GetPosition");
    Err(E_NOTIMPL.into())
  }
  fn GetCharacteristics(&self) -> windows_core::Result<u32> {
    Err(E_NOTIMPL.into())
  }
}
impl IAudioClock2_Impl for Source_Impl {
  fn GetDevicePosition(
    &self,
    _deviceposition: *mut u64,
    _qpcposition: *mut u64,
  ) -> windows_core::Result<()> {
    if self.0.clock_failure.load(Ordering::Acquire) {
      return Err(AUDCLNT_E_DEVICE_INVALIDATED.into());
    }
    // SAFETY: the native contract provides a position and an optional timestamp slot.
    unsafe {
      if self.0.stalled_clock.load(Ordering::Acquire)
        || (self.0.clock_waits_for_buffer.load(Ordering::Acquire)
          && self.0.acquires.load(Ordering::Acquire) == 0)
      {
        *_deviceposition = 0;
        if !_qpcposition.is_null() {
          *_qpcposition = 0;
        }
        return Ok(());
      }
      *_deviceposition = 600000 + u64::from(self.0.rate / 100);
      if !_qpcposition.is_null() {
        *_qpcposition = 9;
      }
    }
    Ok(())
  }
}
#[implement(IPropertyStore)]
struct Properties(Arc<Shared>);
impl IPropertyStore_Impl for Properties_Impl {
  fn GetCount(&self) -> windows_core::Result<u32> {
    Err(E_NOTIMPL.into())
  }
  fn GetAt(&self, _iprop: u32, _pkey: *mut PROPERTYKEY) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetValue(
    &self,
    _key: *const PROPERTYKEY,
  ) -> windows_core::Result<windows::Win32::System::Com::StructuredStorage::PROPVARIANT> {
    // SAFETY: the key remains valid throughout this native call.
    assert_eq!(unsafe { *_key }, PKEY_AudioEngine_DeviceFormat);
    let mut format = [0u8; 18];
    format[4..8].copy_from_slice(&self.0.rate.to_le_bytes());
    // SAFETY: VT_BLOB transfers this COM allocation to the caller's PropVariantClear.
    unsafe {
      let memory = CoTaskMemAlloc(format.len()).cast::<u8>();
      if memory.is_null() {
        return Err(E_OUTOFMEMORY.into());
      }
      std::ptr::copy_nonoverlapping(format.as_ptr(), memory, format.len());
      let mut value = PROPVARIANT::default();
      let fields = &mut value.Anonymous.Anonymous;
      fields.vt = VT_BLOB;
      fields.Anonymous.blob = BLOB {
        cbSize: format.len() as u32,
        pBlobData: memory,
      };
      Ok(value)
    }
  }
  fn SetValue(
    &self,
    _key: *const PROPERTYKEY,
    _propvar: *const windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn Commit(&self) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
}
impl Shared {
  fn new(rate: u32, channels: u16) -> Arc<Self> {
    Arc::new(Self {
      loopback: std::sync::atomic::AtomicBool::new(false),
      event: AtomicIsize::new(0),
      pending: AtomicU32::new(1),
      frames: 480,
      releases: AtomicU32::new(0),
      acquires: AtomicU32::new(0),
      cancel_on_acquire: Mutex::new(None),
      stops: AtomicU32::new(0),
      starts: AtomicU32::new(0),
      start_failure: AtomicBool::new(false),
      stop_failure: AtomicBool::new(false),
      flags: AtomicU32::new(0),
      null_data: AtomicBool::new(false),
      device_lost: AtomicBool::new(false),
      release_failure: AtomicBool::new(false),
      service_failure: AtomicBool::new(false),
      rate,
      channels,
      mix_mask: AtomicU32::new(0),
      mix_float: AtomicBool::new(false),
      future_packet: AtomicBool::new(false),
      clock_waits_for_buffer: AtomicBool::new(false),
      stalled_clock: AtomicBool::new(false),
      clock_failure: AtomicBool::new(false),
      mix_failure: AtomicU32::new(0),
      identity_mode: AtomicU32::new(0),
      data: (0..960).flat_map(|_| 16384i16.to_le_bytes()).collect(),
      unexpected: Mutex::new(Vec::new()),
    })
  }
}
fn endpoint(state: &Arc<Shared>) -> IMMDevice {
  Device(state.clone()).into()
}
fn wait_stopped(state: &Shared) {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
  while state.stops.load(Ordering::Acquire) == 0 {
    assert!(
      std::time::Instant::now() < deadline,
      "audio owner failed to stop"
    );
    thread::sleep(std::time::Duration::from_millis(1));
  }
}
fn wait_released(state: &Arc<Shared>) {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
  while Arc::strong_count(state) != 1 {
    assert!(
      std::time::Instant::now() < deadline,
      "prepared audio interfaces were not released"
    );
    thread::sleep(std::time::Duration::from_millis(1));
  }
}
#[test]
fn cancellation_interrupts_a_native_packet_batch_before_acquiring_more_audio() {
  let state = Shared::new(48000, 2);
  state.pending.store(100, Ordering::Release);
  let owned = state.clone();
  let mut capture = AudioCapture::prepare_factory(
    move || Wasapi::from_device(endpoint(&owned), Endpoint::System),
    Endpoint::System,
    Arc::new(Watchdog::default()),
  )
  .expect("prepared source double");
  *state
    .cancel_on_acquire
    .lock()
    .expect("cancel at first acquisition") = Some(capture.stop.clone());
  capture.begin().expect("begin source");
  capture
    .completion
    .recv_timeout(std::time::Duration::from_secs(2))
    .expect("bounded completion")
    .expect("native stop succeeds");
  assert_eq!(state.acquires.load(Ordering::Acquire), 1);
  assert_eq!(state.releases.load(Ordering::Acquire), 1);
  assert_eq!(state.pending.load(Ordering::Acquire), 99);
  assert_eq!(state.stops.load(Ordering::Acquire), 1);
}
#[test]
fn normal_finish_waits_for_native_shutdown_and_preserves_its_late_failure() {
  for fails in [false, true] {
    let state = Shared::new(48000, 2);
    state.pending.store(0, Ordering::Release);
    let owned = state.clone();
    let capture =
      AudioCapture::from_factory(move || Wasapi::from_device(endpoint(&owned), Endpoint::System))
        .expect("prepared endpoint double");
    state.stop_failure.store(fails, Ordering::Release);
    let result = capture.finish();
    if fails {
      let error = result.expect_err("late Stop HRESULT must reach finalization");
      assert_eq!(error.stage, "WASAPI stop");
      assert_eq!(error.native_code, Some(E_FAIL.0));
    } else {
      result.expect("confirmed native Stop");
    }
    assert_eq!(state.stops.load(Ordering::Acquire), 1);
    assert_eq!(
      Arc::strong_count(&state),
      1,
      "completion follows COM teardown"
    );
  }
}
#[test]
fn finish_retains_a_consumed_producer_error_and_a_stalled_owner_is_bounded() {
  let state = Shared::new(48000, 2);
  state.pending.store(11, Ordering::Release);
  let owned = state.clone();
  let capture =
    AudioCapture::from_factory(move || Wasapi::from_device(endpoint(&owned), Endpoint::System))
      .expect("producer double");
  let first = capture
    .fatal
    .recv_timeout(std::time::Duration::from_secs(2))
    .expect("pool failure");
  assert_eq!(
    capture
      .finish()
      .expect_err("independent terminal result")
      .stage,
    first.stage
  );
  let (_packets, packets) = mpsc::sync_channel(1);
  let (_fatal, fatal) = mpsc::sync_channel(1);
  let (_held, completion) = mpsc::sync_channel(1);
  let capture = AudioCapture {
    source: Default::default(),
    packets,
    fatal,
    completion,
    stop: Arc::new(AtomicBool::new(false)),
    paused: Arc::new(AtomicBool::new(false)),
    start: None,
  };
  assert_eq!(
    capture
      .finish_with_timeout(std::time::Duration::from_millis(1))
      .expect_err("blocked native owner")
      .kind,
    ErrorKind::Timeout
  );
}
#[test]
fn prepared_endpoint_never_starts_or_reads_before_the_explicit_start_and_rejects_duplicate_start() {
  let state = Shared::new(44100, 2);
  let owned = state.clone();
  let mut capture = AudioCapture::prepare_factory(
    move || Wasapi::from_device(endpoint(&owned), Endpoint::System),
    Endpoint::System,
    Arc::new(Watchdog::default()),
  )
  .expect("prepare fixed endpoint");
  assert_ne!(
    state.event.load(Ordering::Acquire),
    0,
    "the endpoint is negotiated before Ready"
  );
  assert_eq!(state.starts.load(Ordering::Acquire), 0);
  assert_eq!(state.acquires.load(Ordering::Acquire), 0);
  assert_eq!(capture.source.device_id, "mock-render-endpoint");
  assert_eq!(
    (
      capture.source.device_rate,
      capture.source.mix_rate,
      capture.source.client_rate
    ),
    (44100, 48000, 48000)
  );
  assert!(capture.try_packet().expect("prepared source").is_none());
  // Stay Ready across several 20 ms owner polls, so the idle wait runs on every machine rather
  // than only when Start happens to arrive late (it decided the coverage count per machine).
  std::thread::sleep(std::time::Duration::from_millis(100));
  assert_eq!(
    state.starts.load(Ordering::Acquire),
    0,
    "idle Ready never starts"
  );
  capture.begin().expect("Start prepared endpoint");
  assert_eq!(state.starts.load(Ordering::Acquire), 1);
  assert_eq!(
    capture.begin().expect_err("duplicate Start").kind,
    ErrorKind::Protocol
  );
  capture.stop();
  drop(capture);
  wait_released(&state);
  assert_eq!(state.stops.load(Ordering::Acquire), 1);
}
#[test]
fn abandoning_ready_audio_releases_interfaces_without_starting_capture() {
  let state = Shared::new(48000, 2);
  let owned = state.clone();
  let capture = AudioCapture::prepare_factory(
    move || Wasapi::from_device(endpoint(&owned), Endpoint::System),
    Endpoint::System,
    Arc::new(Watchdog::default()),
  )
  .expect("prepared endpoint");
  drop(capture);
  wait_released(&state);
  assert_eq!(state.starts.load(Ordering::Acquire), 0);
  assert_eq!(state.acquires.load(Ordering::Acquire), 0);
  assert_eq!(state.stops.load(Ordering::Acquire), 0);
}
#[test]
fn ready_owner_finishes_cleanly_when_cancelled_while_start_remains_deliverable() {
  let state = Shared::new(48000, 2);
  let owned = state.clone();
  let capture = AudioCapture::prepare_factory(
    move || Wasapi::from_device(endpoint(&owned), Endpoint::System),
    Endpoint::System,
    Arc::new(Watchdog::default()),
  )
  .expect("prepared endpoint");
  // Cancellation alone, with the Start sender still held, must end the Ready wait.
  capture.stop();
  capture
    .completion
    .recv_timeout(std::time::Duration::from_secs(2))
    .expect("bounded completion")
    .expect("cancelled Ready owner is not a failure");
  assert!(capture.start.is_some());
  assert_eq!(state.starts.load(Ordering::Acquire), 0);
  drop(capture);
  wait_released(&state);
}
#[test]
fn ready_owner_reports_a_closed_start_channel_as_a_protocol_failure() {
  let state = Shared::new(48000, 2);
  let owned = state.clone();
  let mut capture = AudioCapture::prepare_factory(
    move || Wasapi::from_device(endpoint(&owned), Endpoint::System),
    Endpoint::System,
    Arc::new(Watchdog::default()),
  )
  .expect("prepared endpoint");
  // Dropping the Start sender without cancelling is a broken controller, not a cancellation.
  drop(capture.start.take());
  let error = capture
    .completion
    .recv_timeout(std::time::Duration::from_secs(2))
    .expect("bounded completion")
    .expect_err("closed Start channel");
  assert_eq!(error.kind, ErrorKind::Protocol);
  assert_eq!(
    capture
      .fatal
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("terminal error published")
      .kind,
    ErrorKind::Protocol
  );
  assert_eq!(state.starts.load(Ordering::Acquire), 0);
  drop(capture);
  wait_released(&state);
}
fn detached_capture(stop: bool) -> (AudioCapture, mpsc::SyncSender<EngineError>) {
  let (_packets, packets) = mpsc::sync_channel(1);
  let (failed, fatal) = mpsc::sync_channel(1);
  let (_completed, completion) = mpsc::sync_channel(1);
  let (start, _start_rx) = mpsc::sync_channel(1);
  let capture = AudioCapture {
    source: Default::default(),
    packets,
    fatal,
    completion,
    stop: Arc::new(AtomicBool::new(stop)),
    paused: Arc::new(AtomicBool::new(false)),
    start: Some(start),
  };
  (capture, failed)
}
#[test]
fn begin_reports_a_pending_owner_failure_before_requesting_start() {
  let (mut capture, failed) = detached_capture(false);
  failed
    .try_send(EngineError::new(
      ErrorKind::DeviceLost,
      "WASAPI",
      "endpoint removed",
    ))
    .expect("queue owner failure");
  let error = capture.begin().expect_err("pending failure");
  assert_eq!(error.kind, ErrorKind::DeviceLost);
  assert!(capture.start.is_some(), "Start is not consumed");
}
#[test]
fn begin_after_cancellation_never_requests_start() {
  let (mut capture, _failed) = detached_capture(true);
  assert_eq!(
    capture.begin().expect_err("cancelled owner").kind,
    ErrorKind::Cancelled
  );
  assert!(capture.start.is_some(), "Start is not consumed");
}
#[test]
fn prepared_source_reports_the_original_start_failure_and_does_not_retry_it() {
  let state = Shared::new(48000, 2);
  state.start_failure.store(true, Ordering::Release);
  let owned = state.clone();
  let mut capture = AudioCapture::prepare_factory(
    move || Wasapi::from_device(endpoint(&owned), Endpoint::System),
    Endpoint::System,
    Arc::new(Watchdog::default()),
  )
  .expect("prepare without Start");
  let error = capture.begin().expect_err("native Start refusal");
  assert_eq!(error.stage, "WASAPI start");
  assert_eq!(error.native_code, Some(E_FAIL.0));
  assert!(capture.begin().is_err());
  drop(capture);
  wait_released(&state);
  assert_eq!(state.starts.load(Ordering::Acquire), 1);
  assert_eq!(state.stops.load(Ordering::Acquire), 0);
}
#[test]
fn packets_use_raw_endpoint_coordinates_and_recycle_preallocated_storage() {
  let _com = super::super::Apartment::new().expect("COM");
  let state = Shared::new(44100, 2);
  state.flags.store(
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32,
    Ordering::Release,
  );
  let mut source = Wasapi::from_device(endpoint(&state), Endpoint::System).expect("fake endpoint");
  source.start().expect("start fake client");
  let before = super::super::qpc().expect("before");
  let packet = source.next().expect("first packet").expect("PCM");
  let after = super::super::qpc().expect("after");
  assert_eq!(packet.raw_device_position, 600000);
  assert_eq!((packet.device_rate, packet.client_rate), (44100, 48000));
  assert_eq!(
    packet.wasapi_flags,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32
  );
  assert!(packet.qpc_100ns >= before - 100000 && packet.qpc_100ns <= after - 100000);
  assert_eq!(packet.data.len(), 960);
  assert!(packet.data.iter().all(|sample| *sample == 0.5));
  let first = packet.data.as_ptr();
  drop(packet);
  // Ten distinct FIFO allocations: the eleventh lease reuses the first.
  for index in 0..10 {
    state.pending.store(1, Ordering::Release);
    let packet = source.next().expect("packet").expect("PCM");
    if index == 9 {
      assert_eq!(packet.data.as_ptr(), first);
    }
  }
  assert!(source.next().expect("unchanged endpoint").is_none());
  source.stop().expect("stop");
  drop(source);
  assert_eq!(state.stops.load(Ordering::Acquire), 1);
  assert_eq!(
    state.acquires.load(Ordering::Acquire),
    state.releases.load(Ordering::Acquire)
  );
  assert!(state.unexpected.lock().expect("calls").is_empty());
}
#[test]
fn silence_errors_and_cleanup_preserve_buffers_and_the_original_failure() {
  let _com = super::super::Apartment::new().expect("COM");
  let state = Shared::new(48000, 1);
  state.null_data.store(true, Ordering::Release);
  state
    .flags
    .store(AUDCLNT_BUFFERFLAGS_SILENT.0 as u32, Ordering::Release);
  let mut source = Wasapi::from_device(endpoint(&state), Endpoint::System).expect("fake endpoint");
  let packet = source.next().expect("null silent buffer").expect("silence");
  assert!(packet.silent && packet.data.iter().all(|sample| *sample == 0.0));
  drop(packet);
  state.flags.store(0, Ordering::Release);
  // More failures than pool entries: failed packets must return their allocations.
  for _ in 0..20 {
    state.pending.store(1, Ordering::Release);
    let error = source.next().expect_err("invalid non-silent buffer");
    assert_eq!(error.kind, ErrorKind::Protocol);
    assert!(error.reason.contains("Null non-silent"));
  }
  state.pending.store(1, Ordering::Release);
  state.release_failure.store(true, Ordering::Release);
  let error = source.next().expect_err("packet and release failure");
  assert_eq!(error.kind, ErrorKind::Protocol);
  assert!(error.reason.contains("Null non-silent") && error.reason.contains("cleanup:"));
  state.device_lost.store(true, Ordering::Release);
  assert_eq!(
    source.next().expect_err("endpoint lost").kind,
    ErrorKind::DeviceLost
  );
  assert_eq!(
    state.acquires.load(Ordering::Acquire),
    state.releases.load(Ordering::Acquire)
  );
}
/// Set in the child process that runs the handle-count test alone.
const ISOLATED_HANDLE_COUNT: &str = "FLINCH_ISOLATED_HANDLE_COUNT";
#[test]
fn failed_preparation_releases_the_exact_event_and_rejects_unsupported_endpoints() {
  if std::env::var_os(ISOLATED_HANDLE_COUNT).is_none() {
    // The handle count is process-wide and parallel tests move it (measured: 512 to
    // 485 while the suite wound down), so the exact count is taken in a process running only this.
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
      .args([
        "--exact",
        "windows::audio::mock::failed_preparation_releases_the_exact_event_and_rejects_unsupported_endpoints",
        "--test-threads=1",
      ])
      .env(ISOLATED_HANDLE_COUNT, "1")
      .stdout(std::process::Stdio::piped())
      .spawn()
      .expect("isolated test process");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut finished = false;
    while !finished && std::time::Instant::now() < deadline {
      finished = child.try_wait().expect("isolated test status").is_some();
      if !finished {
        std::thread::sleep(std::time::Duration::from_millis(20));
      }
    }
    if !finished {
      let _ = child.kill();
    }
    // Returns at once: the child exited or was just killed.
    let status = child.wait().expect("isolated test exit");
    assert!(finished, "isolated handle-count test exceeded 60 s");
    // The harness's summary is a few hundred bytes, far below the pipe buffer, so the child never
    // blocks on it before exiting.
    let mut output = String::new();
    std::io::Read::read_to_string(
      &mut child.stdout.take().expect("isolated test output"),
      &mut output,
    )
    .expect("isolated test output text");
    assert!(
      status.success(),
      "isolated handle-count test failed: {status}"
    );
    // A renamed test would match nothing and pass silently.
    assert!(output.contains("1 passed"), "isolated run: {output}");
    return;
  }
  let _com = super::super::Apartment::new().expect("COM");
  // COM may create a process-wide marshaler event on its first interface query.
  // Warm that path, then require exact handle stability across repeated failures.
  let warmup = Shared::new(48000, 2);
  warmup.service_failure.store(true, Ordering::Release);
  assert_eq!(
    Wasapi::from_device(endpoint(&warmup), Endpoint::System)
      .err()
      .expect("warmup service failure")
      .stage,
    "audio capture service"
  );
  let state = Shared::new(48000, 2);
  state.service_failure.store(true, Ordering::Release);
  let mut before = 0;
  // SAFETY: the pseudo-handle names only this process; the output is a live local integer.
  unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut before) }.expect("handle count before");
  for _ in 0..16 {
    let error = Wasapi::from_device(endpoint(&state), Endpoint::System)
      .err()
      .expect("service failure");
    assert_eq!(error.stage, "audio capture service");
  }
  let handle = HANDLE(state.event.load(Ordering::Acquire) as *mut _);
  assert!(!handle.is_invalid());
  let mut after = 0;
  // SAFETY: measure this process without querying a closed handle (strict handle checking can raise).
  unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut after) }.expect("handle count after");
  assert_eq!(
    before, after,
    "the event created before GetService must be released"
  );
  for (rate, channels, kind) in [
    (48000, 6, ErrorKind::Capability),
    (0, 2, ErrorKind::Protocol),
  ] {
    let state = Shared::new(rate, channels);
    let error = Wasapi::from_device(endpoint(&state), Endpoint::System)
      .err()
      .expect("invalid endpoint");
    assert_eq!(error.kind, kind);
    assert_eq!(state.event.load(Ordering::Acquire), 0);
  }
}
#[test]
fn impossible_future_packet_preserves_raw_diagnostics_and_returns_the_buffer() {
  let _com = super::super::Apartment::new().expect("COM");
  let state = Shared::new(48000, 2);
  state.future_packet.store(true, Ordering::Release);
  let mut source = Wasapi::from_device(endpoint(&state), Endpoint::System).expect("source");
  source.start().expect("start");
  let error = source.next().expect_err("future packet");
  assert_eq!(error.kind, ErrorKind::Protocol);
  assert!(error.reason.contains("packet_position=696000"));
  assert!(error.reason.contains("clock_position=600480"));
  assert!(error.reason.contains("rate=48000"));
  assert_eq!(state.releases.load(Ordering::Acquire), 1);
  assert_eq!(
    source.pcm.try_iter().count(),
    10,
    "failed dating returned its PCM lease"
  );
}
#[test]
fn first_packet_acquisition_precedes_clock_correlation_and_still_releases_on_clock_failure() {
  let _com = super::super::Apartment::new().expect("COM");
  let state = Shared::new(48000, 2);
  state.clock_waits_for_buffer.store(true, Ordering::Release);
  let mut source = Wasapi::from_device(endpoint(&state), Endpoint::System).expect("source");
  source.start().expect("start");
  let packet = source
    .next()
    .expect("correlated after first acquisition")
    .expect("packet");
  assert_eq!(packet.raw_device_position, 600000);
  assert_eq!(state.releases.load(Ordering::Acquire), 1);
  drop(packet);
  state.pending.store(1, Ordering::Release);
  state.clock_failure.store(true, Ordering::Release);
  let error = source.next().expect_err("native clock query failed");
  assert_eq!(error.kind, ErrorKind::DeviceLost);
  assert_eq!(error.native_code, Some(AUDCLNT_E_DEVICE_INVALIDATED.0));
  assert_eq!(state.releases.load(Ordering::Acquire), 2);
  assert_eq!(source.pcm.try_iter().count(), 10);
}
#[test]
fn native_packet_dates_remain_usable_when_the_raw_hardware_clock_never_advances() {
  let _com = super::super::Apartment::new().expect("COM");
  let state = Shared::new(48000, 2);
  state.stalled_clock.store(true, Ordering::Release);
  let mut source = Wasapi::from_device(endpoint(&state), Endpoint::System).expect("source");
  source.start().expect("start");
  let before = super::super::qpc().expect("before");
  let packet = source.next().expect("native packet time").expect("PCM");
  let after = super::super::qpc().expect("after");
  assert!(packet.qpc_100ns >= before - 100000 && packet.qpc_100ns <= after - 100000);
  assert_eq!(packet.raw_device_position, 600000);
  assert_eq!(state.releases.load(Ordering::Acquire), 1);
  assert_eq!(state.pending.load(Ordering::Acquire), 0);
}
#[test]
fn invalid_native_packet_timestamp_is_fatal_and_releases_its_buffer() {
  let _com = super::super::Apartment::new().expect("COM");
  let state = Shared::new(96000, 2);
  state.flags.store(
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32,
    Ordering::Release,
  );
  let mut source = Wasapi::from_device(endpoint(&state), Endpoint::System).expect("source");
  source.start().expect("start");
  let error = source
    .next()
    .expect_err("invalid timestamp cannot date media");
  assert_eq!(error.kind, ErrorKind::DeviceLost);
  assert_eq!(error.stage, "WASAPI packet timestamp");
  assert_eq!(state.releases.load(Ordering::Acquire), 1);
  assert_eq!(source.pcm.try_iter().count(), 10);
}
#[test]
fn missing_native_mix_format_never_starts_or_leaks_a_partially_prepared_client() {
  let _com = super::super::Apartment::new().expect("COM");
  for mode in [1, 2] {
    let state = Shared::new(48000, 2);
    state.mix_failure.store(mode, Ordering::Release);
    let error = Wasapi::from_device(endpoint(&state), Endpoint::System)
      .err()
      .expect("mix query failed");
    if mode == 1 {
      assert_eq!(error.kind, ErrorKind::Protocol);
      assert!(error.reason.contains("Null audio mix format"));
      assert_eq!(error.native_code, None);
    } else {
      assert_eq!(error.kind, ErrorKind::Native);
      assert_eq!(error.stage, "audio mix format");
      assert_eq!(error.native_code, Some(E_FAIL.0));
    }
    assert_eq!(state.starts.load(Ordering::Acquire), 0);
    assert_eq!(state.acquires.load(Ordering::Acquire), 0);
    assert_eq!(state.event.load(Ordering::Acquire), 0);
    assert_eq!(
      Arc::strong_count(&state),
      1,
      "all partial COM owners released"
    );
  }
}
#[test]
fn producer_reports_pool_saturation_through_an_independent_terminal_channel() {
  let state = Shared::new(48000, 2);
  state.pending.store(11, Ordering::Release);
  let owned = state.clone();
  let capture =
    AudioCapture::from_factory(move || Wasapi::from_device(endpoint(&owned), Endpoint::System))
      .expect("fake producer");
  // Wait on the terminal channel itself; Stop occurs just before error publication.
  let error = capture
    .fatal
    .recv_timeout(std::time::Duration::from_secs(2))
    .expect("fatal");
  assert_eq!(error.kind, ErrorKind::Saturation);
  assert_eq!(error.stage, "WASAPI PCM pool");
  wait_stopped(&state);
  assert_eq!(state.releases.load(Ordering::Acquire), 11);
  assert_eq!(state.stops.load(Ordering::Acquire), 1);
}
#[test]
fn producer_reports_a_departed_packet_consumer_as_its_terminal_failure() {
  let state = Shared::new(48000, 2);
  state.pending.store(0, Ordering::Release);
  let owned = state.clone();
  let mut capture =
    AudioCapture::from_factory(move || Wasapi::from_device(endpoint(&owned), Endpoint::System))
      .expect("fake producer");
  // The PCM consumer goes away while the owner keeps capturing, as at a racing shutdown.
  let (_, detached) = mpsc::sync_channel(1);
  drop(std::mem::replace(&mut capture.packets, detached));
  state.pending.store(1, Ordering::Release);
  // SAFETY: the producer owns this event until after Stop; the fixture remains alive.
  unsafe { SetEvent(HANDLE(state.event.load(Ordering::Acquire) as *mut _)) }.expect("signal");
  let error = capture
    .fatal
    .recv_timeout(std::time::Duration::from_secs(2))
    .expect("fatal");
  assert_eq!(
    (error.kind, error.reason.as_str()),
    (ErrorKind::Protocol, "Audio producer disconnected")
  );
  let completed = capture
    .completion
    .recv_timeout(std::time::Duration::from_secs(2))
    .expect("bounded completion");
  assert_eq!(
    completed.expect_err("terminal failure").reason,
    "Audio producer disconnected"
  );
  assert_eq!(state.acquires.load(Ordering::Acquire), 1);
  assert_eq!(state.releases.load(Ordering::Acquire), 1);
}
#[test]
fn paused_producer_keeps_draining_and_returns_every_borrowed_buffer() {
  let state = Shared::new(48000, 2);
  state.pending.store(0, Ordering::Release);
  let owned = state.clone();
  let capture =
    AudioCapture::from_factory(move || Wasapi::from_device(endpoint(&owned), Endpoint::System))
      .expect("fake producer");
  capture.set_paused(true);
  state.pending.store(30, Ordering::Release);
  // SAFETY: the producer owns this event until after Stop; the fixture remains alive.
  unsafe { SetEvent(HANDLE(state.event.load(Ordering::Acquire) as *mut _)) }.expect("signal");
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
  while state.pending.load(Ordering::Acquire) != 0 {
    assert!(std::time::Instant::now() < deadline);
    thread::sleep(std::time::Duration::from_millis(1));
  }
  assert!(capture.try_packet().expect("paused producer").is_none());
  capture.stop();
  wait_stopped(&state);
  assert_eq!(state.releases.load(Ordering::Acquire), 30);
}
#[test]
fn fatal_delivery_does_not_depend_on_an_available_pcm_queue_slot() {
  let (sender, packets) = mpsc::sync_channel(1);
  let (failed, fatal) = mpsc::sync_channel(1);
  let (_finished, completion) = mpsc::sync_channel(1);
  sender
    .send(AudioPacket {
      data: PcmBlock::owned(vec![0.0; 2]),
      raw_device_position: 600000,
      device_rate: 44100,
      position_rate: 48000,
      qpc_100ns: 100000,
      read_qpc_100ns: None,
      client_rate: 48000,
      silent: false,
      discontinuity: false,
      wasapi_flags: 0,
      drift_ppm: 0.0,
    })
    .expect("full PCM queue");
  failed
    .send(EngineError::new(
      ErrorKind::Native,
      "scripted endpoint failure",
      "original cause",
    ))
    .expect("independent cause");
  let capture = AudioCapture {
    source: Default::default(),
    packets,
    fatal,
    completion,
    stop: Arc::new(AtomicBool::new(false)),
    paused: Arc::new(AtomicBool::new(false)),
    start: None,
  };
  let error = capture
    .try_packet()
    .expect_err("failure has priority over buffered PCM");
  assert_eq!(error.stage, "scripted endpoint failure");
  assert_eq!(error.reason, "original cause");
  assert!(capture.packets.try_recv().is_ok());
}
#[test]
fn aac_input_rejects_unrepresentable_sample_indices_without_wrapping_native_pts() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let mut encoder = AacEncoder::new().expect("software AAC without endpoint");
  assert_eq!(
    encoder.input(&[0, 0], u64::MAX).expect_err("overflow").kind,
    ErrorKind::Protocol
  );
  assert!(encoder.input(&[0], 0).is_err());
  encoder.input(&[], 0).expect("empty PCM is harmless");
}
#[test]
fn source_formats_keep_endpoint_identity_and_reject_non_stereo_channel_masks_before_start() {
  let _com = super::super::Apartment::new().expect("COM");
  for (rate, channels, floating, mask) in [
    (44100, 2, false, 0),
    (48000, 2, true, 3),
    (96000, 1, true, 4),
  ] {
    let state = Shared::new(rate, channels);
    state.mix_float.store(floating, Ordering::Release);
    state.mix_mask.store(mask, Ordering::Release);
    let source =
      Wasapi::from_device(endpoint(&state), Endpoint::System).expect("prepared source format");
    assert_eq!(source.source.device_id, "mock-render-endpoint");
    assert_eq!(
      (source.source.device_rate, source.source.mix_rate),
      (rate, 48000)
    );
    assert_eq!(
      (
        source.source.client_rate,
        source.source.client_channels,
        source.source.client_bits
      ),
      (48000, 2, 16)
    );
    assert_eq!(source.source.mix_channel_mask, mask);
    assert_eq!(source.source.mix_bits, if floating { 32 } else { 16 });
    assert_eq!(source.source.packet_capacity_frames, 4800);
    assert_eq!(state.starts.load(Ordering::Acquire), 0);
  }
  let state = Shared::new(48000, 2);
  state.mix_mask.store(5, Ordering::Release);
  let error = Wasapi::from_device(endpoint(&state), Endpoint::System)
    .err()
    .expect("unsupported two-channel layout");
  assert_eq!(error.kind, ErrorKind::Capability);
  assert_eq!(error.stage, "audio layout");
  assert_eq!(state.starts.load(Ordering::Acquire), 0);
  assert_eq!(state.event.load(Ordering::Acquire), 0);
}
#[test]
fn malformed_or_failed_endpoint_identity_cannot_be_reported_as_prepared_audio() {
  let _com = super::super::Apartment::new().expect("COM");
  for mode in 1..=5 {
    let state = Shared::new(48000, 2);
    state.identity_mode.store(mode, Ordering::Release);
    let error = Wasapi::from_device(endpoint(&state), Endpoint::System)
      .err()
      .expect("invalid endpoint identity");
    if mode == 4 {
      assert_eq!(error.stage, "audio endpoint identity");
      assert_eq!(error.native_code, Some(E_FAIL.0));
    } else {
      assert_eq!(error.kind, ErrorKind::Protocol);
    }
    assert_eq!(state.starts.load(Ordering::Acquire), 0);
    assert_eq!(state.event.load(Ordering::Acquire), 0);
  }
}

#[test]
fn a_microphone_is_captured_directly_and_system_audio_through_loopback() {
  let _com = super::super::Apartment::new().expect("COM");
  for (endpoint_kind, loopback) in [(Endpoint::System, true), (Endpoint::Microphone, false)] {
    let state = Shared::new(48000, 1);
    let source = Wasapi::from_device(endpoint(&state), endpoint_kind).expect("fake endpoint");
    assert_eq!(
      state.loopback.load(Ordering::Acquire),
      loopback,
      "{endpoint_kind:?}"
    );
    drop(source);
  }
}
#[test]
fn blocked_or_missing_microphones_get_stages_the_app_explains() {
  use windows::Win32::Foundation::E_ACCESSDENIED;
  let blocked = endpoint_error(
    Endpoint::Microphone,
    "audio client",
    windows::core::Error::from(E_ACCESSDENIED),
  );
  assert_eq!(
    (blocked.stage.as_str(), blocked.kind),
    (MICROPHONE_BLOCKED_STAGE, ErrorKind::Capability)
  );
  for missing in [
    windows::core::HRESULT(0x8007_0490_u32 as i32),
    AUDCLNT_E_DEVICE_INVALIDATED,
  ] {
    let error = endpoint_error(
      Endpoint::Microphone,
      "audio endpoint",
      windows::core::Error::from(missing),
    );
    assert_eq!(
      (error.stage.as_str(), error.kind),
      (MICROPHONE_MISSING_STAGE, ErrorKind::DeviceLost)
    );
  }
  // System audio keeps the native cause.
  let system = endpoint_error(
    Endpoint::System,
    "audio client",
    windows::core::Error::from(E_ACCESSDENIED),
  );
  assert_eq!(system.stage, "audio client");
}
#[test]
fn system_audio_and_a_microphone_record_side_by_side_under_one_watchdog() {
  // Regression: both owners used one watchdog stage, so while the system-audio owner waited for
  // packets the microphone's Start was refused as an overlapping scope, and the recording failed
  // with "channel is empty and sending half is closed" instead of a cause.
  let watchdog = Arc::new(Watchdog::default());
  let system = Shared::new(48000, 2);
  let microphone = Shared::new(48000, 1);
  let (owned_system, owned_microphone) = (system.clone(), microphone.clone());
  let mut system_capture = AudioCapture::prepare_factory(
    move || Wasapi::from_device(endpoint(&owned_system), Endpoint::System),
    Endpoint::System,
    watchdog.clone(),
  )
  .expect("system audio prepared");
  let mut microphone_capture = AudioCapture::prepare_factory(
    move || Wasapi::from_device(endpoint(&owned_microphone), Endpoint::Microphone),
    Endpoint::Microphone,
    watchdog.clone(),
  )
  .expect("microphone prepared beside system audio");
  system_capture.begin().expect("system audio started");
  // The system owner now spends nearly all its time inside its packet-wait scope.
  thread::sleep(std::time::Duration::from_millis(50));
  microphone_capture
    .begin()
    .expect("microphone started while system audio records");
  thread::sleep(std::time::Duration::from_millis(100));
  assert!(system_capture.fatal.try_recv().is_err());
  assert!(microphone_capture.fatal.try_recv().is_err());
  system_capture.finish().expect("system audio finished");
  microphone_capture.finish().expect("microphone finished");
  assert_eq!(system.starts.load(Ordering::Acquire), 1);
  assert_eq!(microphone.starts.load(Ordering::Acquire), 1);
}
