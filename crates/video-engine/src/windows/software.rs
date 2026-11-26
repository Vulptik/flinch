//! Windows' software H.264 encoder (tier T3): no vendor runtime. Each GPU frame is read back once.
//!
//! The device is shared with the capture loop and multithread-protected, so every context call
//! takes one device-wide lock. A blocking `Map` would hold that lock until the GPU finished all
//! work queued ahead of the copy (another app's frames included) and stall capture. Copies are
//! therefore queued into a small ring and mapped only once the GPU reports them done.
use super::{mft, mux};
use crate::{EngineError, ErrorKind, Result, types::*};
use std::{
  collections::VecDeque,
  mem::ManuallyDrop,
  time::{Duration, Instant},
};
use windows::{
  Win32::{
    Graphics::{Direct3D11::*, Dxgi::Common::*},
    Media::MediaFoundation::*,
    System::{Com::*, Variant::VARIANT},
  },
  core::Interface,
};

/// Bounded output pulls per call; the synchronous encoder yields at most a few packets per frame.
const PULLS: usize = 64;
/// Frame copies the GPU may still be working on. Three keep a busy GPU's completions overlapping
/// without adding more than a few frames of latency.
const READBACKS: usize = 3;
/// A copy the GPU has not finished in this long is a fault, not a busy GPU.
const READBACK_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll interval while waiting on a copy; the device lock is free between polls.
const READBACK_POLL_US: u32 = 500;

/// A frame whose copy is queued on the GPU, waiting to be mapped and encoded.
struct Queued {
  staging: usize,
  pts: u64,
  duration: u64,
  key: bool,
}

pub struct SoftwareEncoder {
  transform: IMFTransform,
  codec: ICodecAPI,
  context: ID3D11DeviceContext,
  staging: Vec<ID3D11Texture2D>,
  /// Copies in submission order; the front is the next to finish.
  queued: VecDeque<Queued>,
  next_staging: usize,
  waiter: super::timer::Waiter,
  /// GPU conversion hands NV12 surfaces; the CPU conversion hands BGRA, converted here.
  conversion: ConversionKind,
  frame: Vec<u8>,
  width: usize,
  height: usize,
  pending: VecDeque<EncodedPacket>,
  pub extra: Vec<u8>,
  pub drained: bool,
  draining: bool,
}
impl SoftwareEncoder {
  pub fn new(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    spec: &SessionSpec,
    conversion: ConversionKind,
  ) -> Result<Self> {
    // Constant QP matches the app's Low/Medium/High mapping; VBR stays available for the lab.
    let (mode, bitrate, qp) = match spec.quality {
      Quality::Vbr { bitrate } => (
        eAVEncCommonRateControlMode_PeakConstrainedVBR,
        bitrate,
        None,
      ),
      Quality::Cqp { i, p } => {
        // Only the output type's nominal rate; quality mode ignores it for rate control.
        let nominal = (u64::from(width) * u64::from(height) * u64::from(spec.fps) / 10)
          .clamp(100_000, 100_000_000) as u32;
        (eAVEncCommonRateControlMode_Quality, nominal, Some((i, p)))
      },
    };
    // SAFETY: in-process Windows component on the encoder owner's COM apartment.
    let transform: IMFTransform =
      unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| EngineError::native("software H264 encoder", e))?;
    let codec: ICodecAPI = transform
      .cast()
      .map_err(|e| EngineError::native("software H264 codec controls", e))?;
    // SAFETY: property writes on the live encoder before any stream type is set.
    unsafe {
      codec
        .SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true))
        .map_err(|e| EngineError::native("software H264 low latency", e))?;
      let mut settings = vec![
        (&CODECAPI_AVEncMPVDefaultBPictureCount, 0u32),
        (&CODECAPI_AVEncMPVGOPSize, keyframe_interval(spec.fps)),
        (&CODECAPI_AVEncCommonRateControlMode, mode.0 as u32),
        // Speed is Flinch's headline: favor encode speed over compression efficiency.
        (&CODECAPI_AVEncCommonQualityVsSpeed, 0u32),
      ];
      if qp.is_none() {
        settings.push((&CODECAPI_AVEncCommonMeanBitRate, bitrate));
        settings.push((&CODECAPI_AVEncCommonMaxBitRate, bitrate));
      }
      for (key, value) in settings {
        codec
          .SetValue(key, &VARIANT::from(value))
          .map_err(|e| EngineError::native("software H264 required setting", e))?;
        let actual = codec
          .GetValue(key)
          .map_err(|e| EngineError::native("software H264 setting readback", e))?;
        if u32::try_from(&actual)
          .map_err(|e| EngineError::native("software H264 readback type", e))?
          != value
        {
          return Err(EngineError::new(
            ErrorKind::Capability,
            "software H264 profile",
            "Required codec property was not retained",
          ));
        }
      }
      if let Some((i, p)) = qp {
        // Packed I, P and B QPs (16 bits each); B frames are disabled above.
        let packed = u64::from(i) | (u64::from(p) << 16) | (u64::from(p) << 32);
        codec
          .SetValue(
            &CODECAPI_AVEncVideoEncodeFrameTypeQP,
            &VARIANT::from(packed),
          )
          .map_err(|e| EngineError::native("software H264 constant QP", e))?;
        let actual = codec
          .GetValue(&CODECAPI_AVEncVideoEncodeFrameTypeQP)
          .map_err(|e| EngineError::native("software H264 QP readback", e))?;
        if u64::try_from(&actual).map_err(|e| EngineError::native("software H264 QP type", e))?
          != packed
        {
          return Err(EngineError::new(
            ErrorKind::Capability,
            "software H264 profile",
            "Constant QP was not retained",
          ));
        }
      }
      let output = mux::video_type(width, height, spec.fps, &[])?;
      output
        .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
        .map_err(|e| EngineError::native("software H264 bitrate", e))?;
      output
        .SetUINT32(&MF_MT_MPEG2_PROFILE, 100)
        .map_err(|e| EngineError::native("software H264 profile", e))?;
      transform
        .SetOutputType(0, &output, 0)
        .map_err(|e| EngineError::native("software H264 output", e))?;
      let input = mux::media_type(&MFMediaType_Video, &MFVideoFormat_NV12)?;
      input
        .SetUINT64(
          &MF_MT_FRAME_SIZE,
          (u64::from(width) << 32) | u64::from(height),
        )
        .map_err(|e| EngineError::native("software H264 input size", e))?;
      input
        .SetUINT64(&MF_MT_FRAME_RATE, (u64::from(spec.fps) << 32) | 1)
        .map_err(|e| EngineError::native("software H264 input rate", e))?;
      input
        .SetUINT32(&MF_MT_INTERLACE_MODE, 2)
        .map_err(|e| EngineError::native("software H264 progressive", e))?;
      transform
        .SetInputType(0, &input, 0)
        .map_err(|e| EngineError::native("software H264 NV12 input", e))?;
      transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        .map_err(|e| EngineError::native("software H264 begin", e))?;
      transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        .map_err(|e| EngineError::native("software H264 start", e))?;
    }
    let desc = D3D11_TEXTURE2D_DESC {
      Width: width,
      Height: height,
      MipLevels: 1,
      ArraySize: 1,
      Format: match conversion {
        ConversionKind::Gpu => DXGI_FORMAT_NV12,
        ConversionKind::Cpu => DXGI_FORMAT_B8G8R8A8_UNORM,
      },
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Usage: D3D11_USAGE_STAGING,
      CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
      ..Default::default()
    };
    let mut staging = Vec::with_capacity(READBACKS);
    for _ in 0..READBACKS {
      let mut texture = None;
      // SAFETY: fixed CPU-readable copy targets, owned for the encoder's lifetime.
      unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| EngineError::native("software H264 readback texture", e))?;
      staging.push(texture.ok_or_else(|| EngineError::protocol("No readback texture"))?);
    }
    // SAFETY: the shared immediate context is multithread-protected by the GPU owner.
    let context = unsafe { device.GetImmediateContext() }
      .map_err(|e| EngineError::native("software H264 device context", e))?;
    let (width, height) = (width as usize, height as usize);
    Ok(Self {
      transform,
      codec,
      context,
      staging,
      queued: VecDeque::with_capacity(READBACKS),
      next_staging: 0,
      waiter: super::timer::Waiter::new()?,
      conversion,
      frame: vec![0; width * height * 3 / 2],
      width,
      height,
      pending: VecDeque::new(),
      extra: Vec::new(),
      drained: false,
      draining: false,
    })
  }
  /// The frame's copy is queued before this returns, and the immediate context runs it before
  /// any later write to the pool texture, so the texture is never held.
  pub fn submit(
    &mut self,
    texture: &ID3D11Texture2D,
    pts: u64,
    duration: u64,
    key: bool,
  ) -> Result<bool> {
    if self.draining {
      return Err(EngineError::protocol("Software encoder input after drain"));
    }
    self.service()?;
    if self.queued.len() == READBACKS {
      self.finish_oldest()?;
    }
    let staging = self.next_staging;
    self.next_staging = (self.next_staging + 1) % READBACKS;
    // SAFETY: same-size resources on one device; Flush submits the copy without waiting for it.
    unsafe {
      self.context.CopyResource(&self.staging[staging], texture);
      self.context.Flush();
    }
    self.queued.push_back(Queued {
      staging,
      pts,
      duration,
      key,
    });
    Ok(true)
  }
  /// Encodes every queued frame the GPU has finished copying, in order, without waiting.
  fn service(&mut self) -> Result<()> {
    while self.encode_front_if_ready()? {}
    Ok(())
  }
  /// Waits, bounded, for the oldest copy and encodes it. Only a full ring or a drain waits.
  fn finish_oldest(&mut self) -> Result<()> {
    bounded(Instant::now() + READBACK_TIMEOUT, || {
      if self.encode_front_if_ready()? {
        return Ok(true);
      }
      self.waiter.wait(READBACK_POLL_US)?;
      Ok(false)
    })
  }
  fn encode_front_if_ready(&mut self) -> Result<bool> {
    let Some(front) = self.queued.front() else {
      return Ok(false);
    };
    if !self.read_back(front.staging)? {
      return Ok(false);
    }
    let Queued {
      pts, duration, key, ..
    } = self
      .queued
      .pop_front()
      .ok_or_else(|| EngineError::protocol("Software H264 readback queue"))?;
    self.encode(pts, duration, key)?;
    Ok(true)
  }
  fn encode(&mut self, pts: u64, duration: u64, key: bool) -> Result<()> {
    if key {
      // SAFETY: documented per-frame control on the live encoder, before this frame's input.
      unsafe {
        self
          .codec
          .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32))
      }
      .map_err(|e| EngineError::native("software H264 keyframe", e))?;
    }
    let sample = mux::sample(
      &self.frame,
      i64::try_from(pts).map_err(|_| EngineError::protocol("Software H264 PTS overflow"))?,
      duration,
    )?;
    // SAFETY: the sample owns its system-memory copy; the synchronous MFT holds it as needed.
    match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
      Ok(()) => {},
      Err(error) if error.code() == MF_E_NOTACCEPTING => {
        self.pull()?;
        // SAFETY: as above, after draining the encoder's ready output.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
          .map_err(|e| EngineError::native("software H264 submit", e))?;
      },
      Err(error) => return Err(EngineError::native("software H264 submit", error)),
    }
    self.pull()
  }
  /// Copies a finished readback into the frame buffer; false while the GPU is still copying.
  fn read_back(&mut self, staging: usize) -> Result<bool> {
    let texture = self
      .staging
      .get(staging)
      .ok_or_else(|| EngineError::protocol("Software H264 readback slot"))?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: DO_NOT_WAIT returns at once instead of holding the device lock until the GPU is done.
    let result = unsafe {
      self.context.Map(
        texture,
        0,
        D3D11_MAP_READ,
        D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
        Some(&mut mapped),
      )
    };
    match result {
      Ok(()) => {},
      Err(error)
        if error.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING =>
      {
        return Ok(false);
      },
      Err(error) => return Err(EngineError::native("software H264 readback", error)),
    }
    let data = mapped.pData.cast::<u8>().cast_const();
    let pitch = mapped.RowPitch as usize;
    let copied = match self.conversion {
      ConversionKind::Gpu => copy_nv12(data, pitch, self.width, self.height, &mut self.frame),
      ConversionKind::Cpu => bgra_frame(data, pitch, self.width, self.height, &mut self.frame),
    };
    // SAFETY: balanced with the successful Map above on every path.
    unsafe { self.context.Unmap(texture, 0) };
    copied.map(|()| true)
  }
  fn pull(&mut self) -> Result<()> {
    for _ in 0..PULLS {
      // SAFETY: client-allocated output sample sized from the encoder's stream contract.
      let (result, sample) = unsafe {
        let info = self
          .transform
          .GetOutputStreamInfo(0)
          .map_err(|e| EngineError::native("software H264 output info", e))?;
        if info.cbSize > 16 * 1024 * 1024 {
          return Err(EngineError::protocol("Software H264 output exceeds 16 MiB"));
        }
        let sample =
          MFCreateSample().map_err(|e| EngineError::native("software H264 output sample", e))?;
        let buffer = MFCreateMemoryBuffer(info.cbSize.max(4096))
          .map_err(|e| EngineError::native("software H264 output buffer", e))?;
        sample
          .AddBuffer(&buffer)
          .map_err(|e| EngineError::native("software H264 output attach", e))?;
        let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
          dwStreamID: 0,
          pSample: ManuallyDrop::new(Some(sample)),
          dwStatus: 0,
          pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = self.transform.ProcessOutput(0, &mut outputs, &mut status);
        let sample = ManuallyDrop::take(&mut outputs[0].pSample);
        drop(ManuallyDrop::take(&mut outputs[0].pEvents));
        (result, sample)
      };
      match result {
        Ok(()) => {},
        Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
        Err(error) => return Err(EngineError::native("software H264 output", error)),
      }
      let sample =
        sample.ok_or_else(|| EngineError::protocol("Software H264 produced no packet"))?;
      let packet = packet(&sample)?;
      if self.extra.is_empty() && packet.keyframe {
        self.extra = mft::parameter_sets(&packet.data)?;
      }
      self.pending.push_back(packet);
    }
    Err(EngineError::new(
      ErrorKind::Saturation,
      "software H264 output",
      "Encoder produced more packets than one bounded pull accepts",
    ))
  }
  pub fn finished(&self) -> bool {
    self.drained && self.pending.is_empty()
  }
  pub fn poll(&mut self) -> Result<Option<EncodedPacket>> {
    // Called every few milliseconds by the encoder owner, so frames still copying during a pause
    // are encoded without waiting for the next input.
    if !self.draining {
      self.service()?;
    }
    Ok(self.pending.pop_front())
  }
  /// Frames whose copy is still queued on the GPU.
  pub fn copies_in_flight(&self) -> usize {
    self.queued.len()
  }
  pub fn drain(&mut self) -> Result<bool> {
    if !self.draining {
      while !self.queued.is_empty() {
        self.finish_oldest()?;
      }
      // SAFETY: no input follows; the synchronous drain completes within the pulls below.
      unsafe {
        self
          .transform
          .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
          .map_err(|e| EngineError::native("software H264 end", e))?;
        self
          .transform
          .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
          .map_err(|e| EngineError::native("software H264 drain", e))?;
      }
      self.draining = true;
      self.pull()?;
      self.drained = true;
    }
    Ok(true)
  }
}
/// Repeats `step` (which sleeps when not done) until it reports done or `deadline` passes.
fn bounded(deadline: Instant, mut step: impl FnMut() -> Result<bool>) -> Result<()> {
  loop {
    if step()? {
      return Ok(());
    }
    if Instant::now() >= deadline {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "software H264 readback",
        "The GPU did not finish copying a frame within two seconds",
      ));
    }
  }
}
fn packet(sample: &IMFSample) -> Result<EncodedPacket> {
  // SAFETY: read-only time queries on an owned output sample.
  let (pts, duration) = unsafe {
    (
      sample
        .GetSampleTime()
        .map_err(|e| EngineError::native("software H264 output PTS", e))?,
      sample
        .GetSampleDuration()
        .map_err(|e| EngineError::native("software H264 output duration", e))?,
    )
  };
  if pts < 0 || duration <= 0 {
    return Err(EngineError::protocol("Invalid software H264 packet timing"));
  }
  let data = mux::sample_bytes(sample)?;
  let keyframe = mft::nal_units(&data)
    .iter()
    .any(|unit| unit.first().is_some_and(|n| n & 31 == 5));
  Ok(EncodedPacket {
    track: Track::Video,
    pts,
    dts: pts,
    duration: duration as u64,
    keyframe,
    data,
  })
}
/// Convert a mapped BGRA surface (the GPU-less tier) to the encoder's tightly packed NV12.
fn bgra_frame(
  data: *const u8,
  pitch: usize,
  width: usize,
  height: usize,
  frame: &mut Vec<u8>,
) -> Result<()> {
  if data.is_null() || pitch < width * 4 || height == 0 {
    return Err(EngineError::protocol("Invalid BGRA readback mapping"));
  }
  // SAFETY: a mapped BGRA staging subresource spans `pitch` bytes for each of its rows.
  let mapped = unsafe { std::slice::from_raw_parts(data, pitch * height) };
  let extent = |value: usize| {
    u32::try_from(value).map_err(|_| EngineError::protocol("BGRA readback extent overflow"))
  };
  crate::convert::bgra_to_nv12(mapped, pitch, extent(width)?, extent(height)?, None, frame)?;
  Ok(())
}
/// Pack a mapped NV12 surface (luma rows, then interleaved chroma at `pitch * height`) tightly.
fn copy_nv12(
  data: *const u8,
  pitch: usize,
  width: usize,
  height: usize,
  frame: &mut [u8],
) -> Result<()> {
  if data.is_null() || pitch < width || frame.len() != width * height * 3 / 2 {
    return Err(EngineError::protocol("Invalid NV12 readback mapping"));
  }
  let rows = height + height / 2;
  // SAFETY: a mapped NV12 staging subresource spans `pitch` bytes for each of its `rows` rows.
  let mapped = unsafe { std::slice::from_raw_parts(data, pitch * rows) };
  for (row, target) in frame.chunks_exact_mut(width).enumerate() {
    target.copy_from_slice(&mapped[row * pitch..row * pitch + width]);
  }
  Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn nv12_readback_drops_row_padding_and_refuses_unusable_mappings() {
    let (width, height, pitch) = (4, 4, 8);
    let mapped: Vec<u8> = (0..(pitch * 6) as u8).collect();
    let mut frame = vec![0; width * height * 3 / 2];
    copy_nv12(mapped.as_ptr(), pitch, width, height, &mut frame).expect("padded rows");
    assert_eq!(&frame[..4], &[0, 1, 2, 3]);
    assert_eq!(&frame[4..8], &[8, 9, 10, 11]);
    // Chroma begins at pitch * height in the mapping and right after luma in the frame.
    assert_eq!(&frame[16..20], &[32, 33, 34, 35]);
    assert_eq!(&frame[20..24], &[40, 41, 42, 43]);
    assert!(copy_nv12(std::ptr::null(), pitch, width, height, &mut frame).is_err());
    assert!(copy_nv12(mapped.as_ptr(), 3, width, height, &mut frame).is_err());
    assert!(copy_nv12(mapped.as_ptr(), pitch, width, height, &mut frame[1..]).is_err());
  }
  #[test]
  fn bgra_readback_drops_row_padding_and_refuses_unusable_mappings() {
    let (width, height, pitch) = (4usize, 2usize, 24usize);
    let packed: Vec<u8> = (0..width * height * 4)
      .map(|v| (v * 7 % 256) as u8)
      .collect();
    let mut mapped = vec![0xEEu8; pitch * height];
    for row in 0..height {
      mapped[row * pitch..row * pitch + width * 4]
        .copy_from_slice(&packed[row * width * 4..(row + 1) * width * 4]);
    }
    let mut expected = Vec::new();
    crate::convert::bgra_to_nv12(&packed, width * 4, 4, 2, None, &mut expected).expect("packed");
    let mut frame = Vec::new();
    bgra_frame(mapped.as_ptr(), pitch, width, height, &mut frame).expect("padded rows");
    assert_eq!(frame, expected);
    assert!(bgra_frame(std::ptr::null(), pitch, width, height, &mut frame).is_err());
    assert!(bgra_frame(mapped.as_ptr(), width * 4 - 1, width, height, &mut frame).is_err());
    assert!(bgra_frame(mapped.as_ptr(), pitch, width, 0, &mut frame).is_err());
  }
  /// CPU-only Direct3D (WARP): the device class available on machines and VMs without a GPU.
  fn warp() -> ID3D11Device {
    let mut device = None;
    // SAFETY: software rasterizer device; no adapter, display or capture is opened.
    unsafe {
      windows::Win32::Graphics::Direct3D11::D3D11CreateDevice(
        None,
        windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP,
        windows::Win32::Foundation::HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&[windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_1]),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
      )
    }
    .expect("WARP device");
    let device = device.expect("WARP device");
    let multithread: windows::Win32::Graphics::Direct3D10::ID3D10Multithread =
      device.cast().expect("multithread interface");
    // SAFETY: before any other thread uses this device.
    unsafe {
      let _ = multithread.SetMultithreadProtected(true);
    }
    device
  }
  fn numbered(device: &ID3D11Device, width: usize, height: usize, index: u32) -> ID3D11Texture2D {
    frame(device, width, height, index, false)
  }
  /// `textured` adds per-frame detail below the code rows so rate control has something to spend.
  fn frame(
    device: &ID3D11Device,
    width: usize,
    height: usize,
    index: u32,
    textured: bool,
  ) -> ID3D11Texture2D {
    let mut image = vec![128u8; width * height * 3 / 2];
    image[..width * height].fill(42);
    if textured {
      for (offset, value) in image[16 * width..width * height].iter_mut().enumerate() {
        let mixed = (offset as u32 ^ index.wrapping_mul(2_654_435_761)).wrapping_mul(2_246_822_519);
        *value = 16 + (mixed >> 24) as u8 % 220;
      }
    }
    for bit in 0..32 {
      for y in 0..16 {
        let x = bit * 16;
        image[y * width + x..y * width + x + 16].fill(if index & (1 << bit) != 0 {
          235
        } else {
          16
        });
      }
    }
    let desc = D3D11_TEXTURE2D_DESC {
      Width: width as u32,
      Height: height as u32,
      MipLevels: 1,
      ArraySize: 1,
      Format: DXGI_FORMAT_NV12,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Usage: D3D11_USAGE_DEFAULT,
      BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
      ..Default::default()
    };
    let initial = D3D11_SUBRESOURCE_DATA {
      pSysMem: image.as_ptr().cast(),
      SysMemPitch: width as u32,
      SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: initial data spans luma plus interleaved chroma at the declared pitch.
    unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }
      .expect("NV12 source frame");
    texture.expect("NV12 source frame")
  }
  fn spec(fps: u32) -> SessionSpec {
    SessionSpec {
      monitor: "warp".into(),
      fps,
      capture: CaptureKind::Synthetic,
      encoder: EncoderKind::Software,
      quality: Quality::Cqp { i: 23, p: 23 },
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
    }
  }
  /// Whether every command queued before `query` has finished on the GPU; never waits.
  fn finished(context: &ID3D11DeviceContext, query: &ID3D11Query) -> bool {
    let mut done = windows::core::BOOL(0);
    // SAFETY: an event query's data is one BOOL, written only once the event has completed.
    let _ = unsafe {
      context.GetData(
        query,
        Some((&raw mut done).cast()),
        std::mem::size_of_val(&done) as u32,
        D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
      )
    };
    done.as_bool()
  }
  #[test]
  fn a_busy_gpu_never_holds_submission_and_queued_frames_finish_in_order() {
    // Regression: the readback waited for the GPU inside Map, holding the device lock the capture
    // loop shares. Under another app's GPU load the loop stalled for 30-60 ms and lost slots.
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let device = warp();
    // SAFETY: test-owned device; this thread is its only user.
    let context = unsafe { device.GetImmediateContext() }.expect("context");
    let (width, height, fps) = (512usize, 72usize, 30u32);
    let mut encoder = SoftwareEncoder::new(
      &device,
      width as u32,
      height as u32,
      &spec(fps),
      ConversionKind::Gpu,
    )
    .expect("software tier");
    let frames: Vec<_> = (0..=READBACKS as u32 + 1)
      .map(|i| numbered(&device, width, height, i))
      .collect();
    // Heavy GPU work ahead of the frame, as another app's rendering would be, then a marker.
    let big = D3D11_TEXTURE2D_DESC {
      Width: 8192,
      Height: 8192,
      MipLevels: 1,
      ArraySize: 1,
      Format: DXGI_FORMAT_B8G8R8A8_UNORM,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Usage: D3D11_USAGE_DEFAULT,
      ..Default::default()
    };
    let (mut a, mut b) = (None, None);
    let query_desc = D3D11_QUERY_DESC {
      Query: D3D11_QUERY_EVENT,
      MiscFlags: 0,
    };
    let mut query = None;
    // SAFETY: test-owned resources on the WARP device.
    unsafe {
      device
        .CreateTexture2D(&big, None, Some(&mut a))
        .expect("load source");
      device
        .CreateTexture2D(&big, None, Some(&mut b))
        .expect("load target");
      device
        .CreateQuery(&query_desc, Some(&mut query))
        .expect("event query");
    }
    let (a, b, query) = (a.expect("a"), b.expect("b"), query.expect("query"));
    // SAFETY: as above.
    unsafe {
      for _ in 0..24 {
        context.CopyResource(&b, &a);
        context.CopyResource(&a, &b);
      }
      context.End(&query);
      context.Flush();
    }
    assert!(
      !finished(&context, &query),
      "the load must outlast the submission"
    );
    let started = Instant::now();
    assert!(
      encoder
        .submit(&frames[0], 0, 333_333, true)
        .expect("submit")
    );
    let submitted = started.elapsed();
    // A blocking readback could only have returned after the load (and its marker) finished.
    assert!(
      !finished(&context, &query),
      "submission waited for the GPU ({submitted:?})"
    );
    assert_eq!(encoder.copies_in_flight(), 1);
    assert!(encoder.poll().expect("poll while copying").is_none());
    // Frames that are still copying finish through polling alone, as during a pause.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut packets = Vec::new();
    while packets.is_empty() {
      assert!(Instant::now() < deadline, "the queued frame never finished");
      if let Some(packet) = encoder.poll().expect("poll") {
        packets.push(packet);
      }
      std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(encoder.copies_in_flight(), 0);
    // Under a second, shorter load the ring fills; the next frame waits (bounded, two seconds)
    // for its oldest copy.
    // SAFETY: as above.
    unsafe {
      for _ in 0..4 {
        context.CopyResource(&b, &a);
        context.CopyResource(&a, &b);
      }
      context.End(&query);
      context.Flush();
    }
    for (index, frame) in frames.iter().enumerate().skip(1) {
      let pts = index as u64 * 333_333;
      if index == READBACKS + 1 {
        assert!(
          !finished(&context, &query),
          "the load must outlast filling the ring"
        );
        assert_eq!(encoder.copies_in_flight(), READBACKS);
      }
      assert!(encoder.submit(frame, pts, 333_333, false).expect("submit"));
      assert!(encoder.copies_in_flight() <= READBACKS);
    }
    encoder.drain().expect("drain");
    while let Some(packet) = encoder.poll().expect("tail") {
      packets.push(packet);
    }
    assert_eq!(encoder.copies_in_flight(), 0);
    // Order and timing survive the pipeline.
    let times: Vec<_> = packets.iter().map(|p| p.pts).collect();
    assert_eq!(times, [0, 333_333, 666_666, 999_999, 1_333_332]);
  }
  #[test]
  fn a_readback_that_never_finishes_fails_with_a_timeout_instead_of_waiting_forever() {
    let mut steps = 0;
    let error = bounded(Instant::now(), || {
      steps += 1;
      Ok(false)
    })
    .expect_err("never finished");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(steps, 1, "a passed deadline stops after one attempt");
    let mut steps = 0;
    bounded(Instant::now() + Duration::from_secs(5), || {
      steps += 1;
      Ok(steps == 3)
    })
    .expect("finished on the third attempt");
    assert_eq!(steps, 3);
    // A failing step surfaces its own error, not a timeout.
    let error = bounded(Instant::now() + Duration::from_secs(5), || {
      Err(EngineError::protocol("map failed"))
    })
    .expect_err("step error");
    assert_eq!(error.kind, ErrorKind::Protocol);
  }
  #[test]
  fn software_tier_encodes_gpu_frames_into_a_decodable_numbered_file_without_a_gpu() {
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let device = warp();
    let (width, height, fps) = (512usize, 72usize, 30u32);
    let spec = SessionSpec {
      monitor: "warp".into(),
      fps,
      capture: CaptureKind::Synthetic,
      encoder: EncoderKind::Software,
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
    let mut encoder = SoftwareEncoder::new(
      &device,
      width as u32,
      height as u32,
      &spec,
      ConversionKind::Gpu,
    )
    .expect("software tier");
    let mut packets = Vec::new();
    for index in 0..fps * 2 {
      let frame = numbered(&device, width, height, index);
      let pts = crate::time::frame_time(u64::from(index), fps).expect("pts");
      let end = crate::time::frame_time(u64::from(index + 1), fps).expect("end");
      assert!(
        encoder
          .submit(&frame, pts, end - pts, index % fps == 0)
          .expect("submit")
      );
      while let Some(packet) = encoder.poll().expect("poll") {
        packets.push(packet);
      }
    }
    encoder.drain().expect("drain");
    while let Some(packet) = encoder.poll().expect("tail") {
      packets.push(packet);
    }
    assert!(encoder.finished());
    assert!(encoder.drain().expect("idempotent drain"));
    assert!(
      encoder
        .submit(&numbered(&device, width, height, 0), 0, 1, false)
        .is_err()
    );
    assert_eq!(packets.len(), (fps * 2) as usize);
    assert!(packets[0].keyframe && packets[fps as usize].keyframe);
    assert!(!encoder.extra.is_empty());
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("software.mp4");
    let writer = mux::Writer::start(
      &path,
      width as u32,
      height as u32,
      fps,
      encoder.extra.clone(),
      None,
    )
    .expect("native writer");
    for packet in packets {
      writer.push(packet).expect("packet");
    }
    writer.finish().expect("finalize");
    let report = super::super::verify::verify(&path, true).expect("independent decode");
    assert_eq!((report.video_width, report.video_height), (512, 72));
    assert_eq!(report.video_frames, u64::from(fps * 2));
    assert_eq!(report.numbered_frames_checked, u64::from(fps * 2));
    assert!((report.maximum_video_timestamp_gap_ms - 1000.0 / f64::from(fps)).abs() < 0.01);
    // The app's Low/Medium/High constant QPs drive this tier too; finer QP means more bytes.
    let size = |qp| {
      let mut cqp = spec.clone();
      cqp.quality = Quality::Cqp { i: qp, p: qp };
      let mut encoder = SoftwareEncoder::new(
        &device,
        width as u32,
        height as u32,
        &cqp,
        ConversionKind::Gpu,
      )
      .expect("constant QP retained");
      let mut bytes = 0;
      for index in 0..10 {
        encoder
          .submit(
            &frame(&device, width, height, index, true),
            u64::from(index) * 333_333,
            333_333,
            index == 0,
          )
          .expect("submit");
        while let Some(packet) = encoder.poll().expect("poll") {
          bytes += packet.data.len();
        }
      }
      encoder.drain().expect("drain");
      while let Some(packet) = encoder.poll().expect("tail") {
        bytes += packet.data.len();
      }
      bytes
    };
    // Observed on WARP: 356,652 / 264,186 / 172,931 / 50,469 bytes for ten frames.
    let sizes = [10, 18, 28, 45].map(size);
    assert!(sizes.windows(2).all(|pair| pair[0] > pair[1]), "{sizes:?}");
  }
  /// A BGRA surface as the GPU-less tier hands it over: numbered code blocks over grey.
  fn numbered_bgra(
    device: &ID3D11Device,
    width: usize,
    height: usize,
    index: u32,
  ) -> ID3D11Texture2D {
    let mut image = vec![40u8; width * height * 4];
    for bit in 0..32 {
      let value = if index & (1 << bit) != 0 { 255 } else { 0 };
      for y in 0..16 {
        let start = (y * width + bit * 16) * 4;
        image[start..start + 16 * 4].fill(value);
      }
    }
    let desc = D3D11_TEXTURE2D_DESC {
      Width: width as u32,
      Height: height as u32,
      MipLevels: 1,
      ArraySize: 1,
      Format: DXGI_FORMAT_B8G8R8A8_UNORM,
      SampleDesc: DXGI_SAMPLE_DESC {
        Count: 1,
        Quality: 0,
      },
      Usage: D3D11_USAGE_DEFAULT,
      BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
      ..Default::default()
    };
    let initial = D3D11_SUBRESOURCE_DATA {
      pSysMem: image.as_ptr().cast(),
      SysMemPitch: (width * 4) as u32,
      SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: initial data spans every BGRA row at the declared pitch.
    unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }
      .expect("BGRA source frame");
    texture.expect("BGRA source frame")
  }
  #[test]
  fn cpu_conversion_encodes_bgra_surfaces_into_a_decodable_numbered_file() {
    // The GPU-less tier: BGRA surfaces read back and converted to NV12 on the CPU (VM).
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let device = warp();
    let (width, height, fps) = (512usize, 72usize, 30u32);
    let spec = SessionSpec {
      monitor: "warp".into(),
      fps,
      capture: CaptureKind::Synthetic,
      encoder: EncoderKind::Software,
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
    let mut encoder = SoftwareEncoder::new(
      &device,
      width as u32,
      height as u32,
      &spec,
      ConversionKind::Cpu,
    )
    .expect("software tier over BGRA");
    let mut packets = Vec::new();
    for index in 0..fps {
      let pts = crate::time::frame_time(u64::from(index), fps).expect("pts");
      let end = crate::time::frame_time(u64::from(index + 1), fps).expect("end");
      encoder
        .submit(
          &numbered_bgra(&device, width, height, index),
          pts,
          end - pts,
          index == 0,
        )
        .expect("submit");
      while let Some(packet) = encoder.poll().expect("poll") {
        packets.push(packet);
      }
    }
    encoder.drain().expect("drain");
    while let Some(packet) = encoder.poll().expect("tail") {
      packets.push(packet);
    }
    assert_eq!(packets.len(), fps as usize);
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("cpu-conversion.mp4");
    let writer = mux::Writer::start(
      &path,
      width as u32,
      height as u32,
      fps,
      encoder.extra.clone(),
      None,
    )
    .expect("native writer");
    for packet in packets {
      writer.push(packet).expect("packet");
    }
    writer.finish().expect("finalize");
    let report = super::super::verify::verify(&path, true).expect("independent decode");
    assert_eq!(report.video_frames, u64::from(fps));
    assert_eq!(report.numbered_frames_checked, u64::from(fps));
  }
}
