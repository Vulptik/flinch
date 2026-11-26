use super::*;
use std::cell::RefCell;

#[derive(Default)]
struct Fake {
  opened: u32,
  closed: u32,
  open_code: i32,
  open_stage: i32,
  driver_text: &'static str,
  config: Option<VendorConfig>,
  extra_size: usize,
  submit_code: i32,
  poll_code: i32,
  bad_packet: bool,
  drain_code: i32,
}
thread_local! {static STATE: RefCell<Fake> = RefCell::new(Fake::default());}
fn reset() {
  STATE.with(|s| {
    *s.borrow_mut() = Fake {
      extra_size: 24,
      ..Default::default()
    }
  });
}
unsafe extern "C" fn version(out: *mut u32) -> i32 {
  unsafe { *out = (12 << 4) | 2 };
  0
}
unsafe extern "C" fn open(
  _: *mut c_void,
  config: *const VendorConfig,
  out: *mut *mut c_void,
  stage: *mut i32,
  text: *mut u8,
  capacity: usize,
) -> i32 {
  STATE.with(|s| {
    let mut s = s.borrow_mut();
    s.config = Some(unsafe { *config });
    unsafe { *stage = s.open_stage };
    if !s.driver_text.is_empty() {
      let bytes = s.driver_text.as_bytes();
      let count = bytes.len().min(capacity - 1);
      unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), text, count);
        *text.add(count) = 0;
      }
    }
    if s.open_code != 0 {
      // The bridge destroys its own partial state on failure.
      return s.open_code;
    }
    s.opened += 1;
    unsafe { *out = std::ptr::dangling_mut::<u8>().cast() };
    0
  })
}
unsafe extern "C" fn extra(_: *mut c_void, out: *mut u8, cap: usize, size: *mut usize) -> i32 {
  STATE.with(|s| {
    let wanted = s.borrow().extra_size;
    unsafe { *size = wanted };
    if cap < wanted {
      return INPUT_FULL;
    }
    unsafe { std::ptr::write_bytes(out, 7, wanted) };
    0
  })
}
unsafe extern "C" fn busy(_: *mut c_void, slot: u32) -> i32 {
  i32::from(slot as usize >= crate::limits::MAX_SURFACES)
}
unsafe extern "C" fn submit(_: *mut c_void, _: *mut c_void, _: u32, _: i64, _: i64, _: i32) -> i32 {
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
    static PACKET: [u8; 5] = [0, 0, 0, 1, 0x65];
    unsafe {
      *data = PACKET.as_ptr();
      *size = PACKET.len();
      *pts = if s.bad_packet { -1 } else { 333_333 };
      *duration = 166_667;
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
static MOCK: NvencApi = NvencApi {
  version,
  open,
  extra,
  busy,
  submit,
  poll,
  drain,
  close,
};
fn spec(quality: Quality) -> SessionSpec {
  SessionSpec {
    monitor: "primary".into(),
    fps: 60,
    capture: CaptureKind::Synthetic,
    encoder: EncoderKind::Nvenc,
    quality,
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
fn encoder(quality: Quality) -> Result<NvencEncoder> {
  // SAFETY: the scripted table never dereferences the device.
  unsafe { NvencEncoder::open_raw(std::ptr::null_mut(), 1920, 1080, &spec(quality), &MOCK) }
}

#[test]
fn settings_match_amf_and_refuse_values_the_driver_would_misread() {
  let cqp = VendorConfig::new(1920, 1080, &spec(Quality::Cqp { i: 18, p: 22 })).expect("CQP");
  assert_eq!(
    cqp,
    VendorConfig {
      width: 1920,
      height: 1080,
      fps: 60,
      constant_qp: 1,
      qp_i: 18,
      qp_p: 22,
      bitrate: 0,
      gop: keyframe_interval(60),
    }
  );
  let vbr = VendorConfig::new(
    1280,
    720,
    &spec(Quality::Vbr {
      bitrate: 12_000_000,
    }),
  )
  .expect("VBR");
  assert_eq!(
    (vbr.constant_qp, vbr.bitrate, vbr.gop),
    (0, 12_000_000, keyframe_interval(60))
  );
  for (width, height) in [(1921, 1080), (1920, 1081), (8, 1080), (1920, 0)] {
    assert!(VendorConfig::new(width, height, &spec(Quality::default())).is_err());
  }
  assert!(VendorConfig::new(1920, 1080, &spec(Quality::Cqp { i: 52, p: 20 })).is_err());
  assert!(VendorConfig::new(1920, 1080, &spec(Quality::Vbr { bitrate: 0 })).is_err());
  let mut still = spec(Quality::default());
  still.fps = 0;
  assert!(VendorConfig::new(1920, 1080, &still).is_err());
}

#[test]
fn open_failures_name_the_step_and_classify_tier_fallbacks() {
  // No runtime (no NVIDIA driver): a capability result, so Prepare moves to the next tier.
  reset();
  STATE.with(|s| {
    let mut s = s.borrow_mut();
    s.open_code = -126;
    s.open_stage = 1;
  });
  let error = encoder(Quality::default()).err().expect("missing runtime");
  assert_eq!(error.kind, ErrorKind::Capability);
  assert!(
    error
      .reason
      .contains("Windows error 126 at load nvEncodeAPI64.dll"),
    "{error}"
  );
  // A driver older than the header's API names the minimum driver.
  reset();
  STATE.with(|s| {
    let mut s = s.borrow_mut();
    s.open_code = INVALID_VERSION;
    s.open_stage = 2;
  });
  let error = encoder(Quality::default()).err().expect("old driver");
  assert_eq!(error.kind, ErrorKind::Capability);
  assert!(error.reason.contains(MINIMUM_DRIVER), "{error}");
  // An unsupported size or codec is a capability result too.
  reset();
  STATE.with(|s| {
    let mut s = s.borrow_mut();
    s.open_code = UNSUPPORTED;
    s.open_stage = 6;
  });
  let error = encoder(Quality::default()).err().expect("too large");
  assert_eq!(error.kind, ErrorKind::Capability);
  assert!(
    error
      .reason
      .contains("unsupported by this GPU at capabilities")
  );
  // A refused initialization keeps NVIDIA's own message for the hardware log.
  reset();
  STATE.with(|s| {
    let mut s = s.borrow_mut();
    s.open_code = 8;
    s.open_stage = 8;
    s.driver_text = "Invalid rate control mode";
  });
  let error = encoder(Quality::default()).err().expect("refused settings");
  assert_eq!(error.kind, ErrorKind::Native);
  assert_eq!(error.native_code, Some(8));
  assert_eq!(
    error.reason,
    "NV_ENC_ERR_INVALID_PARAM at initialize (Invalid rate control mode)"
  );
  // The bridge cleans up a failed open itself; Rust never closes a handle it did not get.
  STATE.with(|s| assert_eq!((s.borrow().opened, s.borrow().closed), (0, 0)));
  // The validated configuration crosses the ABI unchanged.
  reset();
  drop(encoder(Quality::Cqp { i: 20, p: 24 }).expect("opened"));
  STATE.with(|s| {
    let s = s.borrow();
    let config = s.config.expect("configuration passed");
    assert_eq!((config.qp_i, config.qp_p, config.constant_qp), (20, 24, 1));
    assert_eq!((s.opened, s.closed), (1, 1));
  });
}

#[test]
fn parameter_sets_are_bounded_and_a_bad_size_closes_the_session() {
  reset();
  let opened = encoder(Quality::default()).expect("opened");
  assert_eq!(opened.extra, vec![7; 24]);
  drop(opened);
  for size in [0, 4096] {
    reset();
    STATE.with(|s| s.borrow_mut().extra_size = size);
    assert!(encoder(Quality::default()).is_err());
    STATE.with(|s| assert_eq!((s.borrow().opened, s.borrow().closed), (1, 1)));
  }
}

#[test]
fn backpressure_packets_eof_and_errors_are_observable() {
  reset();
  assert_eq!(runtime_version(&MOCK).expect("version"), "12.2");
  let mut e = encoder(Quality::default()).expect("opened");
  assert!(!e.busy(0));
  assert!(!e.busy(crate::limits::MAX_SURFACES as u32 - 1));
  assert!(e.busy(crate::limits::MAX_SURFACES as u32));
  STATE.with(|s| s.borrow_mut().submit_code = INPUT_FULL);
  // SAFETY: the scripted table never dereferences the texture.
  assert!(!unsafe { e.submit_raw(std::ptr::null_mut(), 0, 0, 166_667, false) }.expect("full"));
  STATE.with(|s| s.borrow_mut().submit_code = 0);
  // SAFETY: as above.
  assert!(unsafe { e.submit_raw(std::ptr::null_mut(), 0, 0, 166_667, true) }.expect("accepted"));
  assert!(unsafe { e.submit_raw(std::ptr::null_mut(), 0, u64::MAX, 1, false) }.is_err());
  STATE.with(|s| s.borrow_mut().submit_code = 9);
  let error =
    unsafe { e.submit_raw(std::ptr::null_mut(), 1, 0, 1, false) }.expect_err("refused submit");
  assert_eq!(error.reason, "NV_ENC_ERR_INVALID_CALL");
  let packet = e.poll().expect("encoded").expect("packet");
  assert_eq!(
    (packet.pts, packet.dts, packet.duration),
    (333_333, 333_333, 166_667)
  );
  assert!(packet.keyframe);
  assert_eq!(packet.data, [0, 0, 0, 1, 0x65]);
  STATE.with(|s| s.borrow_mut().bad_packet = true);
  assert!(e.poll().is_err());
  STATE.with(|s| s.borrow_mut().poll_code = REPEAT);
  assert!(e.poll().expect("not ready").is_none());
  assert!(!e.drained);
  STATE.with(|s| s.borrow_mut().poll_code = EOF);
  assert!(e.poll().expect("end").is_none());
  assert!(e.drained);
  assert!(e.drain().expect("drain"));
  STATE.with(|s| s.borrow_mut().drain_code = 20);
  assert_eq!(
    e.drain().expect_err("drain refused").reason,
    "NV_ENC_ERR_GENERIC"
  );
}

#[test]
fn status_names_follow_the_header_and_unknown_codes_stay_numeric() {
  assert_eq!(describe(8), "NV_ENC_ERR_INVALID_PARAM");
  assert_eq!(describe(18), "NV_ENC_ERR_ENCODER_BUSY");
  assert_eq!(describe(26), "NV_ENC_ERR_NEED_MORE_OUTPUT");
  assert_eq!(describe(77), "NVENC status 77");
  assert_eq!(describe(-5), "Windows error 5");
  assert_eq!(describe(INVALID), "invalid input");
  assert!(open_error(3, 99, "").reason.ends_with("at unknown step"));
}

#[test]
fn native_codec_dispatch_retains_nvenc_backpressure_packets_and_drain_status() {
  reset();
  let mut codec =
    super::super::mft::NativeEncoder::Nvenc(encoder(Quality::default()).expect("mock"));
  let (gpu, driver) = super::super::gpu::mock::fixture();
  assert_eq!(codec.extra(), &[7; 24]);
  assert!(!codec.busy(0));
  assert!(!codec.drained());
  STATE.with(|state| state.borrow_mut().submit_code = INPUT_FULL);
  assert!(
    !codec
      .submit(&gpu.surfaces[0], 0, 0, 166_667, true)
      .expect("backpressure survives dispatch")
  );
  STATE.with(|state| state.borrow_mut().submit_code = 0);
  assert!(
    codec
      .submit(&gpu.surfaces[0], 0, 0, 166_667, true)
      .expect("accepted")
  );
  assert_eq!(
    codec.poll().expect("packet").expect("compressed data").data,
    [0, 0, 0, 1, 0x65]
  );
  assert!(codec.drain().expect("drain"));
  STATE.with(|state| state.borrow_mut().poll_code = EOF);
  assert!(codec.poll().expect("EOF").is_none());
  assert!(codec.drained());
  assert!(driver.unexpected.lock().expect("driver calls").is_empty());
  drop(codec);
  STATE.with(|state| assert_eq!(state.borrow().opened, state.borrow().closed));
}
