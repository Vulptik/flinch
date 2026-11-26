#![allow(clippy::expect_used)]
//! Contract doubles for activated native transforms; no GPU or capture device.
use super::*;
use std::{
  collections::{HashMap, VecDeque},
  sync::{Mutex, atomic::AtomicU32},
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HRESULT};
#[implement(IMFTransform, ICodecAPI, IMFMediaEventGenerator, IMFShutdown)]
struct Transform {
  attrs: IMFAttributes,
  queue: IMFMediaEventQueue,
  properties: Mutex<HashMap<u128, u32>>,
  denied: Option<GUID>,
  wrong_readback: bool,
  input: Mutex<Option<IMFMediaType>>,
  output: Mutex<Option<IMFMediaType>>,
  shutdown: Arc<AtomicU32>,
  packets: Arc<Mutex<VecDeque<EncodedPacket>>>,
}
impl IMFTransform_Impl for Transform_Impl {
  fn GetStreamLimits(
    &self,
    _pdwinputminimum: *mut u32,
    _pdwinputmaximum: *mut u32,
    _pdwoutputminimum: *mut u32,
    _pdwoutputmaximum: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetStreamCount(
    &self,
    _pcinputstreams: *mut u32,
    _pcoutputstreams: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetStreamIDs(
    &self,
    _dwinputidarraysize: u32,
    _pdwinputids: *mut u32,
    _dwoutputidarraysize: u32,
    _pdwoutputids: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetInputStreamInfo(
    &self,
    _dwinputstreamid: u32,
    _pstreaminfo: *mut MFT_INPUT_STREAM_INFO,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetOutputStreamInfo(
    &self,
    _dwoutputstreamid: u32,
  ) -> windows_core::Result<MFT_OUTPUT_STREAM_INFO> {
    Ok(MFT_OUTPUT_STREAM_INFO {
      dwFlags: MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32,
      ..Default::default()
    })
  }
  fn GetAttributes(&self) -> windows_core::Result<IMFAttributes> {
    Ok(self.attrs.clone())
  }
  fn GetInputStreamAttributes(&self, _dwinputstreamid: u32) -> windows_core::Result<IMFAttributes> {
    Err(E_NOTIMPL.into())
  }
  fn GetOutputStreamAttributes(
    &self,
    _dwoutputstreamid: u32,
  ) -> windows_core::Result<IMFAttributes> {
    Err(E_NOTIMPL.into())
  }
  fn DeleteInputStream(&self, _dwstreamid: u32) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn AddInputStreams(&self, _cstreams: u32, _adwstreamids: *const u32) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetInputAvailableType(
    &self,
    _dwinputstreamid: u32,
    _dwtypeindex: u32,
  ) -> windows_core::Result<IMFMediaType> {
    Err(E_NOTIMPL.into())
  }
  fn GetOutputAvailableType(
    &self,
    _dwoutputstreamid: u32,
    _dwtypeindex: u32,
  ) -> windows_core::Result<IMFMediaType> {
    Err(E_NOTIMPL.into())
  }
  fn SetInputType(
    &self,
    _dwinputstreamid: u32,
    _ptype: windows_core::Ref<IMFMediaType>,
    _dwflags: u32,
  ) -> windows_core::Result<()> {
    *self.input.lock().expect("input type lock") = _ptype.cloned();
    Ok(())
  }
  fn SetOutputType(
    &self,
    _dwoutputstreamid: u32,
    _ptype: windows_core::Ref<IMFMediaType>,
    _dwflags: u32,
  ) -> windows_core::Result<()> {
    *self.output.lock().expect("output type lock") = _ptype.cloned();
    Ok(())
  }
  fn GetInputCurrentType(&self, _dwinputstreamid: u32) -> windows_core::Result<IMFMediaType> {
    self
      .input
      .lock()
      .expect("input lock")
      .clone()
      .ok_or_else(|| E_NOTIMPL.into())
  }
  fn GetOutputCurrentType(&self, _dwoutputstreamid: u32) -> windows_core::Result<IMFMediaType> {
    self
      .output
      .lock()
      .expect("output lock")
      .clone()
      .ok_or_else(|| E_NOTIMPL.into())
  }
  fn GetInputStatus(&self, _dwinputstreamid: u32) -> windows_core::Result<u32> {
    Err(E_NOTIMPL.into())
  }
  fn GetOutputStatus(&self) -> windows_core::Result<u32> {
    Err(E_NOTIMPL.into())
  }
  fn SetOutputBounds(&self, _hnslowerbound: i64, _hnsupperbound: i64) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn ProcessEvent(
    &self,
    _dwinputstreamid: u32,
    _pevent: windows_core::Ref<IMFMediaEvent>,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn ProcessMessage(
    &self,
    _emessage: MFT_MESSAGE_TYPE,
    _ulparam: usize,
  ) -> windows_core::Result<()> {
    let kind = if _emessage == MFT_MESSAGE_NOTIFY_START_OF_STREAM {
      Some(METransformNeedInput)
    } else if _emessage == MFT_MESSAGE_COMMAND_DRAIN {
      Some(METransformDrainComplete)
    } else {
      None
    };
    if let Some(kind) = kind {
      unsafe {
        self
          .queue
          .QueueEventParamVar(kind.0 as u32, &GUID::zeroed(), HRESULT(0), std::ptr::null())
      }?;
    }
    Ok(())
  }
  fn ProcessInput(
    &self,
    _dwinputstreamid: u32,
    _psample: windows_core::Ref<IMFSample>,
    _dwflags: u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn ProcessOutput(
    &self,
    _dwflags: u32,
    _coutputbuffercount: u32,
    _poutputsamples: *mut MFT_OUTPUT_DATA_BUFFER,
    _pdwstatus: *mut u32,
  ) -> windows_core::Result<()> {
    assert_eq!((_dwflags, _coutputbuffercount), (0, 1));
    let packet = self
      .packets
      .lock()
      .expect("output fixtures")
      .pop_front()
      .ok_or(E_NOTIMPL)?;
    let sample = mux::sample(&packet.data, packet.pts, packet.duration.max(1))
      .map_err(|_| windows_core::Error::from(E_NOTIMPL))?;
    // SAFETY: the transform owns one output slot; the caller takes its sample reference exactly once.
    unsafe {
      sample.SetSampleDuration(packet.duration as i64)?;
      (*_poutputsamples).pSample = ManuallyDrop::new(Some(sample));
      (*_poutputsamples).dwStatus = 0;
      *_pdwstatus = 0;
    }
    Ok(())
  }
}
impl ICodecAPI_Impl for Transform_Impl {
  fn IsSupported(&self, _api: *const windows_core::GUID) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn IsModifiable(&self, _api: *const windows_core::GUID) -> windows_core::HRESULT {
    E_NOTIMPL
  }
  fn GetParameterRange(
    &self,
    _api: *const windows_core::GUID,
    _valuemin: *mut VARIANT,
    _valuemax: *mut VARIANT,
    _steppingdelta: *mut VARIANT,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetParameterValues(
    &self,
    _api: *const windows_core::GUID,
    _values: *mut *mut VARIANT,
    _valuescount: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetDefaultValue(&self, _api: *const windows_core::GUID) -> windows_core::Result<VARIANT> {
    Err(E_NOTIMPL.into())
  }
  fn GetValue(&self, _api: *const windows_core::GUID) -> windows_core::Result<VARIANT> {
    let key = unsafe { *_api };
    let value = if self.wrong_readback {
      0
    } else {
      *self
        .properties
        .lock()
        .expect("property lock")
        .get(&key.to_u128())
        .ok_or_else(|| {
          windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG)
        })?
    };
    Ok(VARIANT::from(value))
  }
  fn SetValue(
    &self,
    _api: *const windows_core::GUID,
    _value: *const VARIANT,
  ) -> windows_core::Result<()> {
    let key = unsafe { *_api };
    if self.denied == Some(key) {
      return Err(windows::Win32::Foundation::E_INVALIDARG.into());
    }
    let value = unsafe { &*_value };
    let value = if key == CODECAPI_AVLowLatencyMode {
      u32::from(bool::try_from(value)?)
    } else {
      u32::try_from(value)?
    };
    self
      .properties
      .lock()
      .expect("property lock")
      .insert(key.to_u128(), value);
    Ok(())
  }
  fn RegisterForEvent(
    &self,
    _api: *const windows_core::GUID,
    _userdata: isize,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn UnregisterForEvent(&self, _api: *const windows_core::GUID) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn SetAllDefaults(&self) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn SetValueWithNotify(
    &self,
    _api: *const windows_core::GUID,
    _value: *const VARIANT,
    _changedparam: *mut *mut windows_core::GUID,
    _changedparamcount: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn SetAllDefaultsWithNotify(
    &self,
    _changedparam: *mut *mut windows_core::GUID,
    _changedparamcount: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn GetAllSettings(
    &self,
    _settings_stream_0000: windows_core::Ref<windows::Win32::System::Com::IStream>,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn SetAllSettings(
    &self,
    _settings_stream_0001: windows_core::Ref<windows::Win32::System::Com::IStream>,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn SetAllSettingsWithNotify(
    &self,
    _settings_stream_0002: windows_core::Ref<windows::Win32::System::Com::IStream>,
    _changedparam: *mut *mut windows_core::GUID,
    _changedparamcount: *mut u32,
  ) -> windows_core::Result<()> {
    Err(E_NOTIMPL.into())
  }
}
impl IMFMediaEventGenerator_Impl for Transform_Impl {
  fn GetEvent(
    &self,
    _dwflags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
  ) -> windows_core::Result<IMFMediaEvent> {
    Err(E_NOTIMPL.into())
  }
  fn BeginGetEvent(
    &self,
    _pcallback: windows_core::Ref<IMFAsyncCallback>,
    _punkstate: windows_core::Ref<windows_core::IUnknown>,
  ) -> windows_core::Result<()> {
    unsafe {
      self
        .queue
        .BeginGetEvent(_pcallback.ok()?, _punkstate.as_ref())
    }
  }
  fn EndGetEvent(
    &self,
    _presult: windows_core::Ref<IMFAsyncResult>,
  ) -> windows_core::Result<IMFMediaEvent> {
    unsafe { self.queue.EndGetEvent(_presult.ok()?) }
  }
  fn QueueEvent(
    &self,
    _met: u32,
    _guidextendedtype: *const windows_core::GUID,
    _hrstatus: windows_core::HRESULT,
    _pvvalue: *const PROPVARIANT,
  ) -> windows_core::Result<()> {
    unsafe {
      self
        .queue
        .QueueEventParamVar(_met, _guidextendedtype, _hrstatus, _pvvalue)
    }
  }
}
impl IMFShutdown_Impl for Transform_Impl {
  fn Shutdown(&self) -> windows_core::Result<()> {
    self.shutdown.fetch_add(1, Ordering::AcqRel);
    unsafe { self.queue.Shutdown() }
  }
  fn GetShutdownStatus(&self) -> windows_core::Result<MFSHUTDOWN_STATUS> {
    Ok(MFSHUTDOWN_COMPLETED)
  }
}

fn fixture(
  denied: Option<GUID>,
  wrong_readback: bool,
  d3d_aware: bool,
) -> (IMFTransform, Arc<AtomicU32>) {
  let (transform, closed, _) = fixture_with_packets(denied, wrong_readback, d3d_aware);
  (transform, closed)
}
type PacketFixtures = Arc<Mutex<VecDeque<EncodedPacket>>>;
fn fixture_with_packets(
  denied: Option<GUID>,
  wrong_readback: bool,
  d3d_aware: bool,
) -> (IMFTransform, Arc<AtomicU32>, PacketFixtures) {
  let mut attrs = None;
  unsafe { MFCreateAttributes(&mut attrs, 4) }.expect("mock attributes");
  let attrs = attrs.expect("attrs");
  unsafe {
    attrs.SetUINT32(&MF_TRANSFORM_ASYNC, 1).expect("async");
    attrs
      .SetUINT32(&MF_SA_D3D11_AWARE, u32::from(d3d_aware))
      .expect("D3D awareness");
  }
  let shutdown = Arc::new(AtomicU32::new(0));
  let packets = Arc::new(Mutex::new(VecDeque::new()));
  let value = Transform {
    attrs,
    queue: unsafe { MFCreateEventQueue() }.expect("event queue"),
    properties: Mutex::new(HashMap::new()),
    denied,
    wrong_readback,
    input: Mutex::new(None),
    output: Mutex::new(None),
    shutdown: shutdown.clone(),
    packets: packets.clone(),
  };
  (value.into(), shutdown, packets)
}

#[test]
fn asynchronous_packets_retain_pts_duration_parameter_sets_and_reject_invalid_native_extents() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  for duration in [166666, 0] {
    let (transform, closed, packets) = fixture_with_packets(None, false, true);
    let events: IMFMediaEventGenerator = transform.cast().expect("native event interface");
    let mut encoder =
      MftEncoder::configure(transform, None, 1280, 720, &spec()).expect("reference negotiation");
    let data = vec![
      0, 0, 0, 1, 0x67, 10, 20, 0, 0, 1, 0x68, 30, 0, 0, 1, 0x65, 40,
    ];
    packets
      .lock()
      .expect("output fixtures")
      .push_back(EncodedPacket {
        track: Track::Video,
        pts: 3000000,
        dts: 3000000,
        duration,
        keyframe: true,
        data: data.clone(),
      });
    // SAFETY: this is a private fake transform's native MF event queue; no GPU or capture is opened.
    unsafe {
      events.QueueEvent(
        METransformHaveOutput.0 as u32,
        &GUID::zeroed(),
        HRESULT(0),
        std::ptr::null(),
      )
    }
    .expect("output notification");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
      match encoder.poll() {
        Ok(Some(packet)) => {
          assert_ne!(duration, 0);
          assert_eq!(
            (packet.pts, packet.dts, packet.duration),
            (3000000, 3000000, duration)
          );
          assert!(packet.keyframe);
          assert_eq!(packet.data, data);
          assert_eq!(encoder.extra, parameter_sets(&data).expect("SPS/PPS"));
          break;
        },
        Err(error) => {
          assert_eq!(duration, 0);
          assert_eq!(error.kind, ErrorKind::Protocol);
          break;
        },
        Ok(None) => {
          assert!(std::time::Instant::now() < deadline);
          std::thread::sleep(std::time::Duration::from_millis(1));
        },
      }
    }
    drop(encoder);
    assert_eq!(closed.load(Ordering::Acquire), 1);
  }
}
fn spec() -> SessionSpec {
  SessionSpec {
    monitor: "primary".into(),
    fps: 60,
    capture: CaptureKind::Synthetic,
    encoder: EncoderKind::MediaFoundation,
    quality: Quality::Vbr {
      bitrate: 24_000_000,
    },
    cursor: false,
    clicks: false,
    audio: false,
    audio_device: None,
    microphone: false,
    microphone_device: None,
    output: "fixture.mp4".into(),
    trace_frames: false,
    region: None,
    span: Vec::new(),
  }
}
#[test]
fn activated_transform_negotiates_without_pixels_and_shuts_down_after_drain() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let (transform, closed) = fixture(None, false, true);
  let mut encoder =
    MftEncoder::configure(transform, None, 1280, 720, &spec()).expect("negotiate fake transform");
  assert!(encoder.poll().expect("no encoded packet").is_none());
  assert!(encoder.drain().expect("drain"));
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
  while !encoder.drained {
    encoder.poll().expect("drain event");
    assert!(std::time::Instant::now() < deadline);
    std::thread::sleep(std::time::Duration::from_millis(1));
  }
  drop(encoder);
  assert_eq!(closed.load(Ordering::Acquire), 1);
}
#[test]
fn tracked_gpu_sample_returns_its_lease_after_a_native_input_refusal() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let (transform, closed) = fixture(None, false, true);
  let mut encoder =
    MftEncoder::configure(transform, None, 1280, 720, &spec()).expect("reference negotiation");
  let (gpu, state) = super::super::gpu::mock::fixture();
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
  while encoder.credits == 0 {
    encoder.events().expect("input credit");
    assert!(std::time::Instant::now() < deadline);
    std::thread::sleep(std::time::Duration::from_millis(1));
  }
  let error = encoder
    .submit(&gpu.surfaces[0], 0, 0, 166666)
    .expect_err("fake native transform refuses input");
  assert_eq!(error.stage, "MFT submit");
  assert_eq!(error.native_code, Some(E_NOTIMPL.0));
  while encoder.slots[0].load(Ordering::Acquire) {
    assert!(
      std::time::Instant::now() < deadline,
      "late callback never returned its lease"
    );
    std::thread::sleep(std::time::Duration::from_millis(1));
  }
  assert!(state.unexpected.lock().expect("driver calls").is_empty());
  drop(encoder);
  assert_eq!(closed.load(Ordering::Acquire), 1);
}
#[test]
fn rejected_ignored_or_non_gpu_profiles_preserve_error_and_release_activation() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  for (denied, wrong, aware, kind) in [
    (
      Some(CODECAPI_AVEncMPVDefaultBPictureCount),
      false,
      true,
      ErrorKind::Native,
    ),
    (None, true, true, ErrorKind::Capability),
    (None, false, false, ErrorKind::Capability),
  ] {
    let (transform, closed) = fixture(denied, wrong, aware);
    let error = MftEncoder::configure(transform, None, 1280, 720, &spec())
      .err()
      .expect("configuration must fail");
    assert_eq!(error.kind, kind);
    assert_eq!(closed.load(Ordering::Acquire), 1);
  }
  let mut request = spec();
  request.quality = Quality::default();
  assert_eq!(
    reference_bitrate(&request).err().map(|e| e.kind),
    Some(ErrorKind::Capability)
  );
}
