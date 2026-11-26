//! Explicit hardware-MFT reference path, never an automatic AMF fallback.
use super::{
  amf::AmfEncoder, mux, nvenc::NvencEncoder, qsv::QsvEncoder, software::SoftwareEncoder,
};
use crate::{EngineError, ErrorKind, Result, types::*};
use std::{
  mem::ManuallyDrop,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};
use windows::{
  Win32::{
    Foundation::E_NOTIMPL,
    Graphics::Direct3D11::*,
    Media::MediaFoundation::*,
    System::{Com::CoTaskMemFree, Variant::VARIANT},
  },
  core::Interface,
};
use windows_core::implement;

#[implement(IMFAsyncCallback)]
struct ReturnSlot(Arc<AtomicBool>);
impl IMFAsyncCallback_Impl for ReturnSlot_Impl {
  fn GetParameters(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn Invoke(&self, _: windows::core::Ref<IMFAsyncResult>) -> windows::core::Result<()> {
    self.0.store(false, Ordering::Release);
    Ok(())
  }
}

pub enum NativeEncoder {
  Amf(AmfEncoder),
  Nvenc(NvencEncoder),
  Qsv(QsvEncoder),
  Mft(MftEncoder),
  Software(SoftwareEncoder),
}
impl NativeEncoder {
  /// Vendor encoders pass a self-test first (see `self_test`).
  pub fn new(
    device: &ID3D11Device,
    luid: i64,
    width: u32,
    height: u32,
    spec: &SessionSpec,
    conversion: ConversionKind,
  ) -> Result<Self> {
    if conversion == ConversionKind::Gpu && super::self_test::required(spec.encoder) {
      super::self_test::run(device, luid, width, height, spec)?;
    }
    Self::open(device, luid, width, height, spec, conversion)
  }
  pub(super) fn open(
    device: &ID3D11Device,
    luid: i64,
    width: u32,
    height: u32,
    spec: &SessionSpec,
    conversion: ConversionKind,
  ) -> Result<Self> {
    // Hardware encoders take GPU NV12 surfaces; only the software tier reads back BGRA.
    if conversion == ConversionKind::Cpu
      && matches!(
        spec.encoder,
        EncoderKind::Amf | EncoderKind::Nvenc | EncoderKind::Qsv | EncoderKind::MediaFoundation
      )
    {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "encoder",
        "Hardware encoders need GPU colour conversion, which this adapter lacks",
      ));
    }
    match spec.encoder {
      EncoderKind::Amf => AmfEncoder::new(device, width, height, spec).map(Self::Amf),
      EncoderKind::Nvenc => NvencEncoder::new(device, width, height, spec).map(Self::Nvenc),
      EncoderKind::Qsv => QsvEncoder::new(device, luid, width, height, spec).map(Self::Qsv),
      EncoderKind::MediaFoundation => {
        MftEncoder::new(device, luid, width, height, spec).map(Self::Mft)
      },
      EncoderKind::Software => {
        SoftwareEncoder::new(device, width, height, spec, conversion).map(Self::Software)
      },
      EncoderKind::Auto => Err(EngineError::protocol(
        "Automatic encoder selection must be resolved during Prepare",
      )),
    }
  }
  pub fn extra(&self) -> &[u8] {
    match self {
      Self::Amf(e) => &e.extra,
      Self::Nvenc(e) => &e.extra,
      Self::Qsv(e) => &e.extra,
      Self::Mft(e) => &e.extra,
      Self::Software(e) => &e.extra,
    }
  }
  pub fn busy(&self, slot: u32) -> bool {
    match self {
      Self::Amf(e) => e.busy(slot),
      Self::Nvenc(e) => e.busy(slot),
      Self::Qsv(e) => e.busy(slot),
      Self::Mft(e) => e
        .slots
        .get(slot as usize)
        .is_none_or(|s| s.load(Ordering::Acquire)),
      Self::Software(_) => false,
    }
  }
  pub fn submit(
    &mut self,
    t: &ID3D11Texture2D,
    slot: u32,
    pts: u64,
    duration: u64,
    key: bool,
  ) -> Result<bool> {
    match self {
      Self::Amf(e) => e.submit(t, slot, pts, duration, key),
      Self::Nvenc(e) => e.submit(t, slot, pts, duration, key),
      Self::Qsv(e) => e.submit(t, slot, pts, duration, key),
      Self::Mft(e) => e.submit(t, slot, pts, duration),
      Self::Software(e) => e.submit(t, pts, duration, key),
    }
  }
  pub fn poll(&mut self) -> Result<Option<EncodedPacket>> {
    match self {
      Self::Amf(e) => e.poll(),
      Self::Nvenc(e) => e.poll(),
      Self::Qsv(e) => e.poll(),
      Self::Mft(e) => e.poll(),
      Self::Software(e) => e.poll(),
    }
  }
  pub fn drain(&mut self) -> Result<bool> {
    match self {
      Self::Amf(e) => e.drain(),
      Self::Nvenc(e) => e.drain(),
      Self::Qsv(e) => e.drain(),
      Self::Mft(e) => e.drain(),
      Self::Software(e) => e.drain(),
    }
  }
  pub fn drained(&self) -> bool {
    match self {
      Self::Amf(e) => e.drained,
      Self::Nvenc(e) => e.drained,
      Self::Qsv(e) => e.drained,
      Self::Mft(e) => e.drained && e.outputs == 0,
      Self::Software(e) => e.finished(),
    }
  }
}
pub(super) fn reference_bitrate(spec: &SessionSpec) -> Result<u32> {
  Ok(match spec.quality {
    Quality::Vbr { bitrate } => bitrate,
    _ => {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "MFT profile",
        "Reference encoder requires explicit --bitrate VBR; CQP is not silently translated",
      ));
    },
  })
}
struct OwnedTransform(IMFTransform);
impl std::ops::Deref for OwnedTransform {
  type Target = IMFTransform;
  fn deref(&self) -> &IMFTransform {
    &self.0
  }
}
impl Drop for OwnedTransform {
  fn drop(&mut self) {
    // SAFETY: the activated asynchronous transform is shut down once, including negotiation failures.
    if let Err(error) = unsafe { MFShutdownObject(&self.0) } {
      eprintln!("MFT shutdown: {error}");
    }
  }
}
pub struct MftEncoder {
  transform: OwnedTransform,
  events: super::mft_events::AsyncEvents,
  _manager: Option<IMFDXGIDeviceManager>,
  pub extra: Vec<u8>,
  slots: Vec<Arc<AtomicBool>>,
  credits: u32,
  outputs: u32,
  pub drained: bool,
  draining: bool,
}
impl MftEncoder {
  fn new(
    device: &ID3D11Device,
    luid: i64,
    width: u32,
    height: u32,
    spec: &SessionSpec,
  ) -> Result<Self> {
    Self::with_transport(device, luid, width, height, spec, true)
  }
  fn with_transport(
    device: &ID3D11Device,
    luid: i64,
    width: u32,
    height: u32,
    spec: &SessionSpec,
    gpu_transport: bool,
  ) -> Result<Self> {
    reference_bitrate(spec)?;
    // SAFETY: native enumeration allocations and activation references are released on every branch.
    unsafe {
      let mut attributes = None;
      MFCreateAttributes(&mut attributes, 1)
        .map_err(|e| EngineError::native("MFT adapter attributes", e))?;
      let attributes =
        attributes.ok_or_else(|| EngineError::protocol("No MFT adapter attributes"))?;
      attributes
        .SetUINT64(&MFT_ENUM_ADAPTER_LUID, luid as u64)
        .map_err(|e| EngineError::native("MFT adapter LUID", e))?;
      let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
      };
      let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
      };
      let mut pointers = std::ptr::null_mut();
      let mut count = 0;
      MFTEnum2(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &attributes,
        &mut pointers,
        &mut count,
      )
      .map_err(|e| EngineError::native("hardware MFT enumerate", e))?;
      if count == 0 || pointers.is_null() {
        return Err(EngineError::new(
          ErrorKind::Capability,
          "MFT enumerate",
          "No same-adapter hardware H264 encoder",
        ));
      }
      let entries = std::slice::from_raw_parts_mut(pointers, count as usize);
      let first = entries[0].take();
      for entry in entries.iter_mut() {
        drop(entry.take());
      }
      CoTaskMemFree(Some(pointers.cast()));
      let first = first.ok_or_else(|| EngineError::protocol("Null MFT activation"))?;
      let mut name = [0u16; 256];
      match first.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut name, None) {
        Ok(()) => eprintln!(
          "MFT selected: {}",
          String::from_utf16_lossy(
            &name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())]
          )
        ),
        Err(error) => eprintln!("MFT friendly name unavailable: {error}"),
      }
      let transform: IMFTransform = first
        .ActivateObject()
        .map_err(|e| EngineError::native("hardware MFT activate", e))?;
      let mut token = 0;
      let mut manager = None;
      MFCreateDXGIDeviceManager(&mut token, &mut manager)
        .map_err(|e| EngineError::native("MFT device manager", e))?;
      let manager = manager.ok_or_else(|| EngineError::protocol("No DXGI manager"))?;
      manager
        .ResetDevice(device, token)
        .map_err(|e| EngineError::native("MFT device assignment", e))?;
      Self::configure(
        transform,
        if gpu_transport { Some(manager) } else { None },
        width,
        height,
        spec,
      )
    }
  }
  fn configure(
    transform: IMFTransform,
    manager: Option<IMFDXGIDeviceManager>,
    width: u32,
    height: u32,
    spec: &SessionSpec,
  ) -> Result<Self> {
    let transform = OwnedTransform(transform);
    let bitrate = reference_bitrate(spec)?;
    unsafe {
      let attrs = transform
        .GetAttributes()
        .map_err(|e| EngineError::native("MFT attributes", e))?;
      if attrs
        .GetUINT32(&MF_TRANSFORM_ASYNC)
        .map_err(|e| EngineError::native("MFT async capability", e))?
        != 1
      {
        return Err(EngineError::new(
          ErrorKind::Capability,
          "MFT reference",
          "Hardware reference must implement asynchronous processing",
        ));
      }
      if attrs
        .GetUINT32(&MF_SA_D3D11_AWARE)
        .map_err(|e| EngineError::native("MFT D3D11 capability", e))?
        == 0
      {
        return Err(EngineError::new(
          ErrorKind::Capability,
          "MFT",
          "Encoder does not accept D3D11 textures",
        ));
      }
      attrs
        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
        .map_err(|e| EngineError::native("MFT async unlock", e))?;
      attrs
        .SetUINT32(&MF_LOW_LATENCY, 1)
        .map_err(|e| EngineError::native("MFT low latency attribute", e))?;
      if let Some(manager) = &manager {
        transform
          .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
          .map_err(|e| EngineError::native("MFT D3D manager", e))?;
      }
      let codec: ICodecAPI = transform
        .cast()
        .map_err(|e| EngineError::native("MFT codec controls", e))?;
      codec
        .SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true))
        .map_err(|e| EngineError::native("MFT low latency codec mode", e))?;
      for (key, value) in [
        (&CODECAPI_AVEncCommonMeanBitRate, bitrate),
        (&CODECAPI_AVEncCommonMaxBitRate, bitrate),
        (&CODECAPI_AVEncMPVDefaultBPictureCount, 0u32),
        (&CODECAPI_AVEncMPVGOPSize, keyframe_interval(spec.fps)),
        (
          &CODECAPI_AVEncCommonRateControlMode,
          eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
        ),
      ] {
        codec
          .SetValue(key, &VARIANT::from(value))
          .map_err(|e| EngineError::native("MFT required codec setting", e))?;
        let actual = codec
          .GetValue(key)
          .map_err(|e| EngineError::native("MFT codec setting readback", e))?;
        if u32::try_from(&actual).map_err(|e| EngineError::native("MFT codec readback type", e))?
          != value
        {
          return Err(EngineError::new(
            ErrorKind::Capability,
            "MFT profile",
            "Required codec property was not retained",
          ));
        }
      }
      let output = mux::video_type(width, height, spec.fps, &[])?;
      output
        .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
        .map_err(|e| EngineError::native("MFT bitrate", e))?;
      output
        .SetUINT32(&MF_MT_MPEG2_PROFILE, 100)
        .map_err(|e| EngineError::native("MFT H264 profile", e))?;
      transform
        .SetOutputType(0, &output, 0)
        .map_err(|e| EngineError::native("MFT H264 output", e))?;
      let input = mux::media_type(&MFMediaType_Video, &MFVideoFormat_NV12)?;
      input
        .SetUINT64(
          &MF_MT_FRAME_SIZE,
          (u64::from(width) << 32) | u64::from(height),
        )
        .map_err(|e| EngineError::native("MFT input size", e))?;
      input
        .SetUINT64(&MF_MT_FRAME_RATE, (u64::from(spec.fps) << 32) | 1)
        .map_err(|e| EngineError::native("MFT input rate", e))?;
      input
        .SetUINT32(&MF_MT_INTERLACE_MODE, 2)
        .map_err(|e| EngineError::native("MFT progressive", e))?;
      transform
        .SetInputType(0, &input, 0)
        .map_err(|e| EngineError::native("MFT NV12 input", e))?;
      match transform.GetInputStreamAttributes(0) {
        Ok(input_attributes) => eprintln!(
          "MFT input bind flags: {:?}",
          input_attributes.GetUINT32(&MF_SA_D3D11_BINDFLAGS)
        ),
        Err(error) if error.code() == E_NOTIMPL => {
          eprintln!("MFT input attributes not implemented by this transform")
        },
        Err(error) => return Err(EngineError::native("MFT input attributes", error)),
      }
      let output_info = transform
        .GetOutputStreamInfo(0)
        .map_err(|e| EngineError::native("MFT output stream information", e))?;
      eprintln!(
        "MFT output flags={} size={}",
        output_info.dwFlags, output_info.cbSize
      );
      let events: IMFMediaEventGenerator = transform
        .cast()
        .map_err(|e| EngineError::native("MFT asynchronous events", e))?;
      transform
        .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
        .map_err(|e| EngineError::native("MFT initial stream reset", e))?;
      let events = super::mft_events::AsyncEvents::new(events)?;
      transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        .map_err(|e| EngineError::native("MFT begin", e))?;
      transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        .map_err(|e| EngineError::native("MFT start", e))?;
      Ok(Self {
        transform,
        events,
        _manager: manager,
        extra: Vec::new(),
        slots: (0..crate::limits::MAX_SURFACES)
          .map(|_| Arc::new(AtomicBool::new(false)))
          .collect(),
        credits: 0,
        outputs: 0,
        drained: false,
        draining: false,
      })
    }
  }
  fn events(&mut self) -> Result<()> {
    for _ in 0..64 {
      // SAFETY: nonblocking asynchronous MFT event queue, owned by this encoder only.
      let Some(event) = self.events.poll()? else {
        break;
      };
      // SAFETY: event is a live COM event returned by the queue.
      unsafe {
        event
          .GetStatus()
          .map_err(|e| EngineError::native("MFT event status", e))?
          .ok()
          .map_err(|e| EngineError::native("MFT event failure", e))?;
        let event_kind = event
          .GetType()
          .map_err(|e| EngineError::native("MFT event type", e))? as i32;
        match event_kind {
          t if t == METransformNeedInput.0 => grant_credit(&mut self.credits)?,
          t if t == METransformHaveOutput.0 => {
            grant_credit(&mut self.outputs)?;
          },
          t if t == METransformDrainComplete.0 => self.drained = true,
          _ => {},
        }
      }
    }
    Ok(())
  }
  fn submit(
    &mut self,
    texture: &ID3D11Texture2D,
    slot: u32,
    pts: u64,
    duration: u64,
  ) -> Result<bool> {
    self.events()?;
    if self.credits == 0 {
      return Ok(false);
    }
    let busy = self
      .slots
      .get(slot as usize)
      .ok_or_else(|| EngineError::protocol("MFT pool index"))?
      .clone();
    if busy.swap(true, Ordering::AcqRel) {
      return Ok(false);
    }
    // SAFETY: tracked sample callback returns ownership only after the MFT releases its sample.
    unsafe {
      let tracked =
        MFCreateTrackedSample().map_err(|e| EngineError::native("MFT tracked sample", e))?;
      let callback: IMFAsyncCallback = ReturnSlot(busy).into();
      tracked
        .SetAllocator(&callback, None)
        .map_err(|e| EngineError::native("MFT texture allocator", e))?;
      let sample: IMFSample = tracked
        .cast()
        .map_err(|e| EngineError::native("MFT sample cast", e))?;
      let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
        .map_err(|e| EngineError::native("MFT GPU input", e))?;
      let plane: IMF2DBuffer = buffer
        .cast()
        .map_err(|e| EngineError::native("MFT GPU extent interface", e))?;
      let length = plane
        .GetContiguousLength()
        .map_err(|e| EngineError::native("MFT GPU extent", e))?;
      buffer
        .SetCurrentLength(length)
        .map_err(|e| EngineError::native("MFT GPU sample length", e))?;
      sample
        .AddBuffer(&buffer)
        .map_err(|e| EngineError::native("MFT GPU attach", e))?;
      sample
        .SetSampleTime(pts as i64)
        .map_err(|e| EngineError::native("MFT input PTS", e))?;
      sample
        .SetSampleDuration(duration as i64)
        .map_err(|e| EngineError::native("MFT input duration", e))?;
      self
        .transform
        .ProcessInput(0, &sample, 0)
        .map_err(|e| EngineError::native("MFT submit", e))?;
    }
    self.credits -= 1;
    Ok(true)
  }
  fn poll(&mut self) -> Result<Option<EncodedPacket>> {
    self.events()?;
    if self.outputs == 0 {
      return Ok(None);
    }
    // SAFETY: output buffer fields are explicitly taken after ProcessOutput on every branch.
    unsafe {
      let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(None),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
      }];
      let mut flags = 0;
      let result = self.transform.ProcessOutput(0, &mut buffers, &mut flags);
      let sample = ManuallyDrop::take(&mut buffers[0].pSample);
      drop(ManuallyDrop::take(&mut buffers[0].pEvents));
      result.map_err(|e| EngineError::native("MFT compressed output", e))?;
      self.outputs -= 1;
      let sample = sample.ok_or_else(|| EngineError::protocol("MFT produced no packet"))?;
      let pts = sample
        .GetSampleTime()
        .map_err(|e| EngineError::native("MFT output PTS", e))?;
      let duration = sample
        .GetSampleDuration()
        .map_err(|e| EngineError::native("MFT output duration", e))?;
      if duration <= 0 {
        return Err(EngineError::protocol("Invalid MFT packet duration"));
      }
      let data = mux::sample_bytes(&sample)?;
      if self.extra.is_empty() {
        self.extra = parameter_sets(&data)?;
      }
      let keyframe = nal_units(&data)
        .iter()
        .any(|unit| unit.first().is_some_and(|n| n & 31 == 5));
      Ok(Some(EncodedPacket {
        track: Track::Video,
        pts,
        dts: pts,
        duration: duration as u64,
        keyframe,
        data,
      }))
    }
  }
  fn drain(&mut self) -> Result<bool> {
    if !self.draining {
      // SAFETY: no further input is submitted once draining begins.
      unsafe {
        self
          .transform
          .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
          .map_err(|e| EngineError::native("MFT end", e))?;
        self
          .transform
          .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
          .map_err(|e| EngineError::native("MFT drain", e))?;
      }
      self.draining = true;
    }
    Ok(true)
  }
}
pub(super) fn nal_units(bytes: &[u8]) -> Vec<&[u8]> {
  let mut starts = Vec::new();
  let mut index = 0;
  while index + 3 <= bytes.len() {
    let n = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
      4
    } else if bytes[index..].starts_with(&[0, 0, 1]) {
      3
    } else {
      index += 1;
      continue;
    };
    starts.push((index, index + n));
    index += n;
  }
  starts
    .iter()
    .enumerate()
    .map(|(i, (_, begin))| &bytes[*begin..starts.get(i + 1).map_or(bytes.len(), |p| p.0)])
    .collect()
}
fn grant_credit(counter: &mut u32) -> Result<()> {
  if *counter >= 1024 {
    return Err(EngineError::new(
      ErrorKind::Saturation,
      "MFT events",
      "Native event credits exceed the bounded protocol budget",
    ));
  }
  *counter += 1;
  Ok(())
}
impl Drop for MftEncoder {
  fn drop(&mut self) {
    self.events.close();
  }
}

#[cfg(test)]
#[path = "mft/mock.rs"]
mod mock;
pub fn parameter_sets(bytes: &[u8]) -> Result<Vec<u8>> {
  let units = nal_units(bytes);
  let mut out = Vec::new();
  let mut flags = 0;
  for unit in units {
    if let Some(tag) = unit.first().map(|b| b & 31)
      && (tag == 7 || tag == 8)
    {
      flags |= if tag == 7 { 1 } else { 2 };
      out.extend([0, 0, 0, 1]);
      out.extend(unit);
    }
  }
  if flags != 3 {
    return Err(EngineError::protocol(
      "First reference packet lacks Annex-B SPS/PPS",
    ));
  }
  Ok(out)
}
#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn native_event_credits_are_retained_instead_of_silently_capped() {
    let mut value = 0;
    for _ in 0..1024 {
      grant_credit(&mut value).expect("bounded credit");
    }
    assert_eq!(value, 1024);
    assert!(grant_credit(&mut value).is_err());
    assert_eq!(value, 1024);
  }
  #[test]
  #[ignore = "Explicit offscreen hardware diagnostic; no desktop capture or audio endpoint"]
  fn hardware_reference_cpu_input_diagnostic() {
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let output = super::super::gpu::outputs()
      .expect("display inventory")
      .into_iter()
      .find(|o| o.info.vendor_id == 0x1002)
      .expect("AMD adapter");
    let gpu =
      super::super::gpu::Gpu::new(&output.adapter, 1280, 720, None, 60).expect("offscreen GPU");
    let spec = SessionSpec {
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
      output: "diagnostic.mp4".into(),
      trace_frames: false,
      region: None,
      span: Vec::new(),
    };
    let mut encoder = MftEncoder::with_transport(
      &gpu.device,
      output.info.adapter_luid,
      1280,
      720,
      &spec,
      false,
    )
    .expect("reference MFT");
    let bytes = vec![128; 1280 * 720 * 3 / 2];
    let start = std::time::Instant::now();
    let mut inputs = 0;
    let mut outputs = 0;
    while start.elapsed() < std::time::Duration::from_secs(3) {
      encoder.events().expect("events");
      if encoder.credits > 0 && inputs < 8 {
        let sample = mux::sample(&bytes, inputs * 166666, 166666).expect("diagnostic sample");
        // SAFETY: isolated diagnostic input has the configured NV12 extent; no captured pixels or fallback are involved.
        unsafe { encoder.transform.ProcessInput(0, &sample, 0) }.expect("diagnostic input");
        encoder.credits -= 1;
        inputs += 1;
      }
      if encoder.poll().expect("diagnostic output").is_some() {
        outputs += 1;
      }
      std::thread::sleep(std::time::Duration::from_millis(2));
    }
    eprintln!("Reference diagnostic CPU fixtures: inputs={inputs}, outputs={outputs}");
    assert!(
      outputs > 0,
      "reference transform did not produce compressed output from CPU fixture either"
    );
  }
  #[test]
  fn hardware_encoders_refuse_cpu_converted_bgra_surfaces() {
    // AMF and the hardware MFT take NV12 GPU surfaces; the GPU-less tier's BGRA surfaces are only
    // read back by the software encoder. Refused before any encoder or adapter is touched.
    let _com = super::super::Apartment::new().expect("COM");
    // SAFETY: read-only factory for the software adapter; no output, capture or display.
    let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory4 =
      unsafe { windows::Win32::Graphics::Dxgi::CreateDXGIFactory1() }.expect("DXGI factory");
    // SAFETY: as above.
    let adapter: windows::Win32::Graphics::Dxgi::IDXGIAdapter1 =
      unsafe { factory.EnumWarpAdapter() }.expect("WARP adapter");
    let gpu = super::super::gpu::Gpu::new(&adapter, 256, 64, None, 30).expect("GPU-less tier");
    assert_eq!(gpu.conversion(), ConversionKind::Cpu);
    for encoder in [
      EncoderKind::Amf,
      EncoderKind::Nvenc,
      EncoderKind::Qsv,
      EncoderKind::MediaFoundation,
    ] {
      let spec = SessionSpec {
        monitor: "warp".into(),
        fps: 30,
        capture: CaptureKind::Synthetic,
        encoder,
        quality: Quality::Vbr { bitrate: 2_000_000 },
        cursor: false,
        clicks: false,
        audio: false,
        audio_device: None,
        microphone: false,
        microphone_device: None,
        output: "unused.mp4".into(),
        trace_frames: false,
        region: None,
        span: Vec::new(),
      };
      let error = NativeEncoder::new(&gpu.device, 0, 256, 64, &spec, ConversionKind::Cpu)
        .err()
        .expect("hardware encoder refused");
      assert_eq!(error.kind, ErrorKind::Capability, "{encoder:?}");
      assert!(error.reason.contains("GPU colour conversion"), "{error}");
    }
  }
  #[test]
  fn extracts_parameter_sets_without_including_frame_data() {
    let bytes = [
      0, 0, 0, 1, 0x67, 10, 20, 0, 0, 1, 0x68, 30, 0, 0, 1, 0x65, 40,
    ];
    assert_eq!(
      parameter_sets(&bytes).expect("valid test fixture"),
      vec![0, 0, 0, 1, 0x67, 10, 20, 0, 0, 0, 1, 0x68, 30]
    );
    assert!(parameter_sets(&[0, 0, 1, 0x65]).is_err());
  }
}
