use super::*;
use std::cell::RefCell;

#[derive(Default)]
struct Fake {
  opened: u32,
  closed: u32,
  open_code: i32,
  open_stage: i32,
  note: &'static str,
  luid: i64,
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
      extra_size: 32,
      ..Default::default()
    }
  });
}
fn write_text(text: *mut u8, capacity: usize, value: &str) {
  let count = value.len().min(capacity - 1);
  // SAFETY: the caller's buffer holds capacity bytes.
  unsafe {
    std::ptr::copy_nonoverlapping(value.as_ptr(), text, count);
    *text.add(count) = 0;
  }
}
unsafe extern "C" fn open(
  _: *mut c_void,
  luid: i64,
  config: *const VendorConfig,
  out: *mut *mut c_void,
  stage: *mut i32,
  text: *mut u8,
  capacity: usize,
) -> i32 {
  STATE.with(|s| {
    let mut s = s.borrow_mut();
    s.luid = luid;
    s.config = Some(unsafe { *config });
    unsafe { *stage = s.open_stage };
    write_text(text, capacity, s.note);
    if s.open_code != 0 {
      return s.open_code;
    }
    s.opened += 1;
    unsafe { *out = std::ptr::dangling_mut::<u8>().cast() };
    0
  })
}
unsafe extern "C" fn describe_runtime(_: *mut c_void, text: *mut u8, capacity: usize) -> i32 {
  write_text(
    text,
    capacity,
    "Intel(R) Arc(TM) A380 Graphics, runtime API 2.17",
  );
  0
}
unsafe extern "C" fn extra(_: *mut c_void, out: *mut u8, cap: usize, size: *mut usize) -> i32 {
  STATE.with(|s| {
    let wanted = s.borrow().extra_size;
    unsafe { *size = wanted };
    if cap < wanted {
      return INPUT_FULL;
    }
    unsafe { std::ptr::write_bytes(out, 3, wanted) };
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
    static PACKET: [u8; 5] = [0, 0, 0, 1, 0x41];
    unsafe {
      *data = PACKET.as_ptr();
      *size = PACKET.len();
      *pts = if s.bad_packet { -1 } else { 500_000 };
      *duration = 333_333;
      *key = 0;
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
static MOCK: QsvApi = QsvApi {
  open,
  describe: describe_runtime,
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
    fps: 30,
    capture: CaptureKind::Synthetic,
    encoder: EncoderKind::Qsv,
    quality: Quality::Vbr {
      bitrate: 80_000_000,
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
fn encoder() -> Result<QsvEncoder> {
  // SAFETY: the scripted table never dereferences the device.
  unsafe { QsvEncoder::open_raw(std::ptr::null_mut(), 0x1234_5678, 1366, 768, &spec(), &MOCK) }
}

#[test]
fn open_passes_the_adapter_identity_and_classifies_failures() {
  reset();
  let opened = encoder().expect("opened");
  assert_eq!(opened.extra, vec![3; 32]);
  assert_eq!(
    opened.description,
    "Intel(R) Arc(TM) A380 Graphics, runtime API 2.17"
  );
  drop(opened);
  STATE.with(|s| {
    let s = s.borrow();
    // The display's adapter LUID chooses between an iGPU and a discrete Arc.
    assert_eq!(s.luid, 0x1234_5678);
    let config = s.config.expect("configuration passed");
    // 80 Mbit/s crosses as bits per second; the bridge applies BRCParamMultiplier for kbps.
    assert_eq!(
      (config.bitrate, config.gop, config.constant_qp),
      (80_000_000, keyframe_interval(30), 0)
    );
    assert_eq!((s.opened, s.closed), (1, 1));
  });
  // No Intel runtime on this adapter: a capability result.
  for (code, stage) in [(UNSUPPORTED, 3), (-9, 3), (-3, 2)] {
    reset();
    STATE.with(|s| {
      let mut s = s.borrow_mut();
      s.open_code = code;
      s.open_stage = stage;
      s.note = "no Intel runtime on this adapter";
    });
    let error = encoder().err().expect("no runtime");
    assert_eq!(error.kind, ErrorKind::Capability, "{error}");
  }
  // Software-only (partial acceleration) is refused as a capability, with the bridge's note.
  reset();
  STATE.with(|s| {
    let mut s = s.borrow_mut();
    s.open_code = UNSUPPORTED;
    s.open_stage = 7;
    s.note = "partial acceleration (software)";
  });
  let error = encoder().err().expect("software runtime");
  assert_eq!(error.kind, ErrorKind::Capability);
  assert_eq!(
    error.reason,
    "unsupported by this GPU at query (partial acceleration (software))"
  );
  // A refused initialization is a native failure that names the step and status.
  reset();
  STATE.with(|s| {
    let mut s = s.borrow_mut();
    s.open_code = -15;
    s.open_stage = 8;
  });
  let error = encoder().err().expect("refused settings");
  assert_eq!(error.kind, ErrorKind::Native);
  assert_eq!(error.reason, "MFX_ERR_INVALID_VIDEO_PARAM at initialize");
  STATE.with(|s| assert_eq!((s.borrow().opened, s.borrow().closed), (0, 0)));
  // Parameter sets are bounded; a bad size closes the session.
  for size in [0, 2048] {
    reset();
    STATE.with(|s| s.borrow_mut().extra_size = size);
    assert!(encoder().is_err());
    STATE.with(|s| assert_eq!((s.borrow().opened, s.borrow().closed), (1, 1)));
  }
}

#[test]
fn backpressure_packets_drain_pressure_and_eof_are_observable() {
  reset();
  let mut e = encoder().expect("opened");
  assert!(!e.busy(0));
  assert!(!e.busy(crate::limits::MAX_SURFACES as u32 - 1));
  assert!(e.busy(crate::limits::MAX_SURFACES as u32));
  STATE.with(|s| s.borrow_mut().submit_code = INPUT_FULL);
  // SAFETY: the scripted table never dereferences the texture.
  assert!(!unsafe { e.submit_raw(std::ptr::null_mut(), 0, 0, 333_333, false) }.expect("full"));
  STATE.with(|s| s.borrow_mut().submit_code = 0);
  // SAFETY: as above.
  assert!(unsafe { e.submit_raw(std::ptr::null_mut(), 0, 0, 333_333, true) }.expect("accepted"));
  STATE.with(|s| s.borrow_mut().submit_code = -17);
  let error =
    unsafe { e.submit_raw(std::ptr::null_mut(), 1, 0, 1, false) }.expect_err("device failed");
  assert_eq!(error.reason, "MFX_ERR_DEVICE_FAILED");
  let packet = e.poll().expect("encoded").expect("packet");
  assert_eq!(
    (packet.pts, packet.dts, packet.duration),
    (500_000, 500_000, 333_333)
  );
  assert!(!packet.keyframe);
  STATE.with(|s| s.borrow_mut().bad_packet = true);
  assert!(e.poll().is_err());
  STATE.with(|s| s.borrow_mut().poll_code = REPEAT);
  assert!(e.poll().expect("in execution").is_none());
  // Drain waits for a free output buffer instead of failing.
  STATE.with(|s| s.borrow_mut().drain_code = INPUT_FULL);
  assert!(!e.drain().expect("drain pressure"));
  STATE.with(|s| s.borrow_mut().drain_code = 0);
  assert!(e.drain().expect("drained"));
  STATE.with(|s| s.borrow_mut().poll_code = EOF);
  assert!(e.poll().expect("end").is_none());
  assert!(e.drained);
  STATE.with(|s| s.borrow_mut().drain_code = -21);
  assert_eq!(e.drain().expect_err("GPU hang").reason, "MFX_ERR_GPU_HANG");
}

#[test]
fn status_names_cover_errors_and_warnings() {
  assert_eq!(describe(-14), "MFX_ERR_INCOMPATIBLE_VIDEO_PARAM");
  assert_eq!(describe(4), "MFX_WRN_PARTIAL_ACCELERATION");
  assert_eq!(describe(-10000), "MFX_ERR_MORE_DATA_SUBMIT_TASK");
  assert_eq!(describe(-99), "mfxStatus -99");
  assert_eq!(describe(INVALID), "invalid input");
  assert!(open_error(-1, 42, "").reason.ends_with("at unknown step"));
}

#[test]
fn native_codec_dispatch_retains_qsv_packets_and_drain_pressure() {
  reset();
  let mut codec = super::super::mft::NativeEncoder::Qsv(encoder().expect("mock"));
  let (gpu, driver) = super::super::gpu::mock::fixture();
  assert_eq!(codec.extra(), &[3; 32]);
  assert!(!codec.busy(0));
  assert!(
    codec
      .submit(&gpu.surfaces[0], 0, 0, 333_333, true)
      .expect("accepted")
  );
  assert_eq!(
    codec.poll().expect("packet").expect("data").data,
    [0, 0, 0, 1, 0x41]
  );
  STATE.with(|s| s.borrow_mut().drain_code = INPUT_FULL);
  assert!(!codec.drain().expect("pressure survives dispatch"));
  STATE.with(|s| s.borrow_mut().poll_code = EOF);
  assert!(codec.poll().expect("EOF").is_none());
  assert!(codec.drained());
  assert!(driver.unexpected.lock().expect("driver calls").is_empty());
  drop(codec);
  STATE.with(|s| assert_eq!(s.borrow().opened, s.borrow().closed));
}

/// The real bridges on a CPU-only device (WARP), which neither vendor runtime accepts.
fn warp() -> (windows::Win32::Graphics::Direct3D11::ID3D11Device, i64) {
  use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
  let _com = super::super::Apartment::new().expect("COM");
  // SAFETY: read-only factory for the software adapter; no output, capture or display.
  let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
  // SAFETY: as above.
  let adapter: IDXGIAdapter1 = unsafe { factory.EnumWarpAdapter() }.expect("WARP adapter");
  // SAFETY: plain adapter description read.
  let luid = unsafe { adapter.GetDesc1() }
    .expect("adapter description")
    .AdapterLuid;
  let gpu = super::super::gpu::Gpu::new(&adapter, 256, 64, None, 30).expect("WARP device");
  (
    gpu.device.clone(),
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart),
  )
}

#[test]
fn real_vendor_bridges_refuse_a_foreign_adapter_cleanly() {
  // Without NVIDIA or Intel hardware these run the real loaders and must refuse explicitly:
  // no runtime, or a runtime that rejects the software adapter. Never a crash or success.
  let (device, luid) = warp();
  let mut settings = spec();
  settings.quality = Quality::default();
  let nvenc = super::super::nvenc::NvencEncoder::new(&device, 256, 64, &settings)
    .err()
    .expect("NVENC on WARP");
  assert!(nvenc.reason.contains(" at "), "names its step: {nvenc}");
  if nvenc.reason.contains("load nvEncodeAPI64.dll") {
    assert_eq!(nvenc.kind, ErrorKind::Capability, "{nvenc}");
  }
  let qsv = QsvEncoder::new(&device, luid, 256, 64, &settings)
    .err()
    .expect("QSV on WARP");
  assert!(qsv.reason.contains(" at "), "names its step: {qsv}");
  eprintln!("NVENC: {nvenc}\nQSV: {qsv}");
}
