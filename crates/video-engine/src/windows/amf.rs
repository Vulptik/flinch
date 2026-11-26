//! Small hand-authored binding to Flinch's C ABI, not a copy of AMD C++ vtables.
use crate::{EngineError, ErrorKind, Result, types::*};
use std::{ffi::c_void, ptr::NonNull};
use windows::{
  Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
  core::Interface,
};

unsafe extern "C" {
  fn flinch_amf_version(version: *mut u64) -> i32;
  fn flinch_amf_open(device: *mut c_void, output: *mut *mut c_void) -> i32;
  fn flinch_amf_int(value: *mut c_void, key: *const u16, wanted: i64, boolean: i32) -> i32;
  fn flinch_amf_init(value: *mut c_void, width: i32, height: i32, fps: i32) -> i32;
  fn flinch_amf_query_wait(value: *mut c_void, supported: *mut i32) -> i32;
  fn flinch_amf_extra(
    value: *mut c_void,
    output: *mut u8,
    capacity: usize,
    size: *mut usize,
  ) -> i32;
  fn flinch_amf_busy(value: *mut c_void, slot: u32) -> i32;
  fn flinch_amf_submit(
    value: *mut c_void,
    texture: *mut c_void,
    slot: u32,
    pts: i64,
    duration: i64,
    keyframe: i32,
  ) -> i32;
  fn flinch_amf_poll(
    value: *mut c_void,
    data: *mut *const u8,
    size: *mut usize,
    pts: *mut i64,
    duration: *mut i64,
    keyframe: *mut i32,
  ) -> i32;
  fn flinch_amf_drain(value: *mut c_void) -> i32;
  fn flinch_amf_close(value: *mut c_void);
}
struct AmfApi {
  version: unsafe extern "C" fn(*mut u64) -> i32,
  open: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> i32,
  int: unsafe extern "C" fn(*mut c_void, *const u16, i64, i32) -> i32,
  init: unsafe extern "C" fn(*mut c_void, i32, i32, i32) -> i32,
  query_wait: unsafe extern "C" fn(*mut c_void, *mut i32) -> i32,
  extra: unsafe extern "C" fn(*mut c_void, *mut u8, usize, *mut usize) -> i32,
  busy: unsafe extern "C" fn(*mut c_void, u32) -> i32,
  submit: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, i64, i64, i32) -> i32,
  poll: unsafe extern "C" fn(
    *mut c_void,
    *mut *const u8,
    *mut usize,
    *mut i64,
    *mut i64,
    *mut i32,
  ) -> i32,
  drain: unsafe extern "C" fn(*mut c_void) -> i32,
  close: unsafe extern "C" fn(*mut c_void),
}
static NATIVE: AmfApi = AmfApi {
  version: flinch_amf_version,
  open: flinch_amf_open,
  int: flinch_amf_int,
  init: flinch_amf_init,
  query_wait: flinch_amf_query_wait,
  extra: flinch_amf_extra,
  busy: flinch_amf_busy,
  submit: flinch_amf_submit,
  poll: flinch_amf_poll,
  drain: flinch_amf_drain,
  close: flinch_amf_close,
};

const AMF_EOF: i32 = 23;
const AMF_REPEAT: i32 = 24;
const AMF_INPUT_FULL: i32 = 25;
fn check(code: i32, stage: &str) -> Result<()> {
  if code == 0 {
    Ok(())
  } else {
    Err(EngineError {
      native_code: Some(code),
      ..EngineError::new(ErrorKind::Native, stage, format!("AMF result {code}"))
    })
  }
}
pub fn version() -> Result<String> {
  runtime_version(&NATIVE)
}
fn runtime_version(api: &AmfApi) -> Result<String> {
  let mut value = 0;
  // SAFETY: bridge writes one u64 and loads only the system driver's runtime.
  check(unsafe { (api.version)(&mut value) }, "AMF runtime")?;
  Ok(format!(
    "{}.{}.{}.{}",
    value >> 48,
    (value >> 32) & 65535,
    (value >> 16) & 65535,
    value & 65535
  ))
}
pub struct AmfEncoder {
  handle: NonNull<c_void>,
  api: &'static AmfApi,
  pub extra: Vec<u8>,
  pub drained: bool,
}
impl AmfEncoder {
  pub fn new(device: &ID3D11Device, width: u32, height: u32, spec: &SessionSpec) -> Result<Self> {
    // SAFETY: live D3D11 device stays owned by the caller for encoder creation.
    unsafe { Self::open_raw(device.as_raw(), width, height, spec, &NATIVE) }
  }
  unsafe fn open_raw(
    device: *mut c_void,
    width: u32,
    height: u32,
    spec: &SessionSpec,
    api: &'static AmfApi,
  ) -> Result<Self> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: device remains alive for this encoder, bridge returns an exclusively owned handle.
    check(unsafe { (api.open)(device, &mut raw) }, "AMF open")?;
    let handle =
      NonNull::new(raw).ok_or_else(|| EngineError::protocol("AMF returned null encoder"))?;
    let mut encoder = Self {
      handle,
      api,
      extra: Vec::new(),
      drained: false,
    };
    for (key, value, boolean) in [
      ("Usage", 2, false),
      ("Profile", 100, false),
      ("QualityPreset", 0, false),
      ("BPicturesPattern", 0, false),
      ("EnablePreAnalysis", 0, true),
      ("InputQueueSize", 4, false),
      ("RateControlSkipFrameEnable", 0, true),
      ("IDRPeriod", i64::from(keyframe_interval(spec.fps)), false),
    ] {
      encoder.set(key, value, boolean)?;
    }
    match spec.quality {
      Quality::Cqp { i, p } => {
        encoder.set("RateControlMethod", 0, false)?;
        encoder.set("QPI", i64::from(i), false)?;
        encoder.set("QPP", i64::from(p), false)?;
      },
      Quality::Vbr { bitrate } => {
        encoder.set("RateControlMethod", 3, false)?;
        encoder.set("TargetBitrate", i64::from(bitrate), false)?;
        encoder.set("PeakBitrate", i64::from(bitrate), false)?;
      },
    }
    // SAFETY: dimensions were checked against the D3D budget and fit i32; live exclusive handle.
    check(
      unsafe {
        (api.init)(
          handle.as_ptr(),
          width as i32,
          height as i32,
          spec.fps as i32,
        )
      },
      "AMF initialize",
    )?;
    let mut query_wait = 0;
    // SAFETY: exclusively owned encoder and writable capability output.
    check(
      unsafe { (api.query_wait)(handle.as_ptr(), &mut query_wait) },
      "AMF query wait negotiation",
    )?;
    eprintln!(
      "AMF QueryOutput: {}",
      if query_wait != 0 {
        "native 2 ms wait"
      } else {
        "bounded timer polling; native wait unavailable"
      }
    );
    let mut size = 0;
    // SAFETY: zero capacity is a size query; no output is dereferenced by the bridge.
    let code = unsafe { (api.extra)(handle.as_ptr(), std::ptr::null_mut(), 0, &mut size) };
    if code != AMF_INPUT_FULL {
      check(code, "AMF extradata size")?;
    }
    if size == 0 || size > 1024 * 1024 {
      return Err(EngineError::protocol("Invalid AMF parameter-set size"));
    }
    encoder.extra.resize(size, 0);
    // SAFETY: exactly sized owned byte slice remains writable throughout the call.
    check(
      unsafe { (api.extra)(handle.as_ptr(), encoder.extra.as_mut_ptr(), size, &mut size) },
      "AMF extradata",
    )?;
    Ok(encoder)
  }
  fn set(&self, key: &str, wanted: i64, boolean: bool) -> Result<()> {
    let key_wide: Vec<u16> = key.encode_utf16().chain([0]).collect();
    // SAFETY: terminated key lives for this synchronous checked property operation.
    check(
      unsafe {
        (self.api.int)(
          self.handle.as_ptr(),
          key_wide.as_ptr(),
          wanted,
          i32::from(boolean),
        )
      },
      &format!("AMF property {key}"),
    )
  }
  pub fn busy(&self, slot: u32) -> bool {
    // SAFETY: bridge validates slot and reads only its atomic ownership flag.
    unsafe { (self.api.busy)(self.handle.as_ptr(), slot) != 0 }
  }
  pub fn submit(
    &mut self,
    texture: &ID3D11Texture2D,
    slot: u32,
    pts: u64,
    duration: u64,
    keyframe: bool,
  ) -> Result<bool> {
    // SAFETY: caller's borrowed live texture is immutable until the native release notification.
    unsafe { self.submit_raw(texture.as_raw(), slot, pts, duration, keyframe) }
  }
  unsafe fn submit_raw(
    &mut self,
    texture: *mut c_void,
    slot: u32,
    pts: u64,
    duration: u64,
    keyframe: bool,
  ) -> Result<bool> {
    let pts = i64::try_from(pts).map_err(|_| EngineError::protocol("Video PTS overflow"))?;
    // SAFETY: texture pool keeps this resource alive and immutable until busy(slot) becomes false.
    let code = unsafe {
      (self.api.submit)(
        self.handle.as_ptr(),
        texture,
        slot,
        pts,
        duration as i64,
        i32::from(keyframe),
      )
    };
    if code == AMF_INPUT_FULL {
      return Ok(false);
    }
    check(code, "AMF submit")?;
    Ok(true)
  }
  pub fn poll(&mut self) -> Result<Option<EncodedPacket>> {
    let mut data = std::ptr::null();
    let mut size = 0;
    let mut pts = 0;
    let mut duration = 0;
    let mut keyframe = 0;
    // SAFETY: output parameters are writable; returned data remains valid until next poll/close.
    let code = unsafe {
      (self.api.poll)(
        self.handle.as_ptr(),
        &mut data,
        &mut size,
        &mut pts,
        &mut duration,
        &mut keyframe,
      )
    };
    if code == AMF_EOF {
      self.drained = true;
      return Ok(None);
    }
    if code == AMF_REPEAT {
      return Ok(None);
    }
    check(code, "AMF output")?;
    if data.is_null() || size == 0 || size > 16 * 1024 * 1024 || pts < 0 || duration <= 0 {
      return Err(EngineError::protocol("Invalid compressed AMF packet"));
    }
    // SAFETY: bridge owns at least size bytes and does not release them during this copy.
    let bytes = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
    Ok(Some(EncodedPacket {
      track: Track::Video,
      pts,
      dts: pts,
      duration: duration as u64,
      keyframe: keyframe != 0,
      data: bytes,
    }))
  }
  pub fn drain(&mut self) -> Result<bool> {
    // SAFETY: live encoder, called on its owner only.
    let code = unsafe { (self.api.drain)(self.handle.as_ptr()) };
    if code == AMF_INPUT_FULL {
      return Ok(false);
    }
    check(code, "AMF drain")?;
    Ok(true)
  }
}

impl Drop for AmfEncoder {
  fn drop(&mut self) {
    // SAFETY: exclusive allocation from flinch_amf_open; supervisor bounds teardown by process lifetime.
    unsafe {
      (self.api.close)(self.handle.as_ptr());
    }
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  use std::cell::RefCell;
  #[derive(Default)]
  struct Fake {
    opened: u32,
    closed: u32,
    fail_property: bool,
    fail_open: bool,
    extra_size: usize,
    poll_code: i32,
    bad_packet: bool,
    submit_code: i32,
    drain_code: i32,
    properties: Vec<String>,
  }
  thread_local! {static STATE:RefCell<Fake>=RefCell::new(Fake{extra_size:4,..Default::default()});}
  unsafe extern "C" fn version(out: *mut u64) -> i32 {
    unsafe {
      *out = (1u64 << 48) | (5u64 << 32) | (3u64 << 16);
    }
    0
  }
  unsafe extern "C" fn open(_: *mut c_void, out: *mut *mut c_void) -> i32 {
    STATE.with(|s| {
      let mut s = s.borrow_mut();
      if s.fail_open {
        return 19;
      }
      s.opened += 1;
      unsafe {
        *out = std::ptr::dangling_mut::<u8>().cast();
      }
      0
    })
  }
  unsafe extern "C" fn property(_: *mut c_void, key: *const u16, _: i64, _: i32) -> i32 {
    let mut length = 0;
    unsafe {
      while *key.add(length) != 0 && length < 128 {
        length += 1;
      }
    }
    let name = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(key, length)) };
    STATE.with(|s| {
      let mut s = s.borrow_mut();
      s.properties.push(name);
      if s.fail_property { 4 } else { 0 }
    })
  }
  unsafe extern "C" fn init(_: *mut c_void, _: i32, _: i32, _: i32) -> i32 {
    0
  }
  unsafe extern "C" fn query(_: *mut c_void, out: *mut i32) -> i32 {
    unsafe {
      *out = 1;
    }
    0
  }
  unsafe extern "C" fn extra(_: *mut c_void, out: *mut u8, cap: usize, size: *mut usize) -> i32 {
    STATE.with(|s| {
      let size_value = s.borrow().extra_size;
      unsafe {
        *size = size_value;
      }
      if cap < size_value {
        return AMF_INPUT_FULL;
      }
      if !out.is_null() {
        unsafe {
          std::ptr::write_bytes(out, 1, size_value);
        }
      }
      0
    })
  }
  unsafe extern "C" fn busy(_: *mut c_void, slot: u32) -> i32 {
    i32::from(slot as usize >= crate::limits::MAX_SURFACES)
  }
  unsafe extern "C" fn submit(
    _: *mut c_void,
    _: *mut c_void,
    _: u32,
    _: i64,
    _: i64,
    _: i32,
  ) -> i32 {
    STATE.with(|s| s.borrow().submit_code)
  }
  unsafe extern "C" fn poll(
    _: *mut c_void,
    data: *mut *const u8,
    size: *mut usize,
    pts: *mut i64,
    duration: *mut i64,
    key: *mut i32,
  ) -> i32 {
    STATE.with(|s| {
      let s = s.borrow();
      if s.poll_code != 0 {
        return s.poll_code;
      }
      static PACKET: [u8; 4] = [0, 0, 1, 0x65];
      unsafe {
        *data = PACKET.as_ptr();
        *size = 4;
        *pts = if s.bad_packet { -1 } else { 0 };
        *duration = 166666;
        *key = 1;
      }
      0
    })
  }
  unsafe extern "C" fn drain(_: *mut c_void) -> i32 {
    STATE.with(|s| s.borrow().drain_code)
  }
  unsafe extern "C" fn close(_: *mut c_void) {
    STATE.with(|s| s.borrow_mut().closed += 1);
  }
  static MOCK: AmfApi = AmfApi {
    version,
    open,
    int: property,
    init,
    query_wait: query,
    extra,
    busy,
    submit,
    poll,
    drain,
    close,
  };
  fn spec() -> SessionSpec {
    SessionSpec {
      monitor: "primary".into(),
      fps: 60,
      capture: CaptureKind::Synthetic,
      encoder: EncoderKind::Amf,
      quality: Quality::default(),
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
  fn reset() {
    STATE.with(|s| {
      *s.borrow_mut() = Fake {
        extra_size: 4,
        ..Default::default()
      }
    });
  }
  fn encoder() -> Result<AmfEncoder> {
    unsafe { AmfEncoder::open_raw(std::ptr::null_mut(), 1280, 720, &spec(), &MOCK) }
  }
  #[test]
  fn ffi_negotiation_preserves_property_failures_and_releases_partial_context() {
    reset();
    STATE.with(|s| s.borrow_mut().fail_open = true);
    assert!(encoder().is_err());
    STATE.with(|s| assert_eq!(s.borrow().closed, 0));
    reset();
    STATE.with(|s| s.borrow_mut().fail_property = true);
    let error = encoder().err().expect("property rejected");
    assert!(error.stage.contains("Usage"));
    STATE.with(|s| assert_eq!(s.borrow().closed, 1));
    reset();
    STATE.with(|s| s.borrow_mut().extra_size = 2 * 1024 * 1024);
    assert!(encoder().is_err());
    STATE.with(|s| assert_eq!(s.borrow().closed, 1));
    reset();
    let mut settings = spec();
    settings.quality = Quality::Vbr {
      bitrate: 24_000_000,
    };
    let e = unsafe { AmfEncoder::open_raw(std::ptr::null_mut(), 1280, 720, &settings, &MOCK) }
      .expect("VBR mock");
    drop(e);
    STATE.with(|s| {
      let s = s.borrow();
      assert!(s.properties.contains(&"PeakBitrate".into()));
      assert_eq!(s.opened, s.closed);
    });
  }
  #[test]
  fn ffi_backpressure_release_eof_and_invalid_packets_are_observable() {
    reset();
    assert_eq!(runtime_version(&MOCK).expect("mock version"), "1.5.3.0");
    let mut e = encoder().expect("mock encoder");
    assert!(!e.busy(0));
    assert!(!e.busy(crate::limits::MAX_SURFACES as u32 - 1));
    assert!(e.busy(crate::limits::MAX_SURFACES as u32));
    STATE.with(|s| s.borrow_mut().submit_code = AMF_INPUT_FULL);
    assert!(
      !unsafe { e.submit_raw(std::ptr::null_mut(), 0, 0, 166666, false) }.expect("backpressure")
    );
    STATE.with(|s| s.borrow_mut().submit_code = 0);
    assert!(unsafe { e.submit_raw(std::ptr::null_mut(), 0, 0, 166666, true) }.expect("accepted"));
    let packet = e.poll().expect("encoded").expect("packet");
    assert_eq!(packet.data, [0, 0, 1, 0x65]);
    assert!(packet.keyframe);
    STATE.with(|s| s.borrow_mut().bad_packet = true);
    assert!(e.poll().is_err());
    STATE.with(|s| s.borrow_mut().poll_code = AMF_REPEAT);
    assert!(e.poll().expect("wait").is_none());
    STATE.with(|s| s.borrow_mut().poll_code = AMF_EOF);
    assert!(e.poll().expect("EOF").is_none());
    assert!(e.drained);
    STATE.with(|s| s.borrow_mut().drain_code = AMF_INPUT_FULL);
    assert!(!e.drain().expect("drain pressure"));
    STATE.with(|s| s.borrow_mut().drain_code = 0);
    assert!(e.drain().expect("drain accepted"));
  }
  #[test]
  fn native_codec_dispatch_retains_amf_backpressure_packets_and_drain_status() {
    reset();
    let mut codec = super::super::mft::NativeEncoder::Amf(encoder().expect("mock AMF"));
    let (gpu, driver) = super::super::gpu::mock::fixture();
    assert_eq!(codec.extra(), &[1, 1, 1, 1]);
    assert!(!codec.busy(0));
    assert!(!codec.drained());
    STATE.with(|state| state.borrow_mut().submit_code = AMF_INPUT_FULL);
    assert!(
      !codec
        .submit(&gpu.surfaces[0], 0, 0, 166666, true)
        .expect("backpressure survives dispatch")
    );
    STATE.with(|state| state.borrow_mut().submit_code = 0);
    assert!(
      codec
        .submit(&gpu.surfaces[0], 0, 0, 166666, true)
        .expect("accepted")
    );
    assert_eq!(
      codec.poll().expect("packet").expect("compressed data").data,
      [0, 0, 1, 0x65]
    );
    assert!(codec.drain().expect("drain"));
    STATE.with(|state| state.borrow_mut().poll_code = AMF_EOF);
    assert!(codec.poll().expect("EOF").is_none());
    assert!(codec.drained());
    assert!(driver.unexpected.lock().expect("driver calls").is_empty());
    drop(codec);
    STATE.with(|state| assert_eq!(state.borrow().opened, state.borrow().closed));
  }
}
