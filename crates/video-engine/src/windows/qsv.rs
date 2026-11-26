//! Intel Quick Sync Video through Flinch's C ABI over the pinned oneVPL dispatcher (`native/vpl`).
//!
//! Everything above the native calls is exercised here with a scripted table; the driver itself is
//! covered by `cargo xtask test:video:vendor` on Intel hardware.
use super::vendor::{EOF, INPUT_FULL, INVALID, REPEAT, UNSUPPORTED, VendorConfig, note};
use crate::{EngineError, ErrorKind, Result, types::*};
use std::{ffi::c_void, ptr::NonNull};
use windows::{
  Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
  core::Interface,
};

unsafe extern "C" {
  fn flinch_qsv_open(
    device: *mut c_void,
    luid: i64,
    config: *const VendorConfig,
    output: *mut *mut c_void,
    stage: *mut i32,
    text: *mut u8,
    capacity: usize,
  ) -> i32;
  fn flinch_qsv_describe(value: *mut c_void, text: *mut u8, capacity: usize) -> i32;
  fn flinch_qsv_inventory(text: *mut u8, capacity: usize, count: *mut u32) -> i32;
  fn flinch_qsv_extra(
    value: *mut c_void,
    output: *mut u8,
    capacity: usize,
    size: *mut usize,
  ) -> i32;
  fn flinch_qsv_busy(value: *mut c_void, slot: u32) -> i32;
  fn flinch_qsv_submit(
    value: *mut c_void,
    texture: *mut c_void,
    slot: u32,
    pts: i64,
    duration: i64,
    keyframe: i32,
  ) -> i32;
  fn flinch_qsv_poll(
    value: *mut c_void,
    data: *mut *const u8,
    size: *mut usize,
    pts: *mut i64,
    duration: *mut i64,
    keyframe: *mut i32,
  ) -> i32;
  fn flinch_qsv_drain(value: *mut c_void) -> i32;
  fn flinch_qsv_close(value: *mut c_void);
}
type Open = unsafe extern "C" fn(
  *mut c_void,
  i64,
  *const VendorConfig,
  *mut *mut c_void,
  *mut i32,
  *mut u8,
  usize,
) -> i32;
type Poll = unsafe extern "C" fn(
  *mut c_void,
  *mut *const u8,
  *mut usize,
  *mut i64,
  *mut i64,
  *mut i32,
) -> i32;
pub(crate) struct QsvApi {
  open: Open,
  describe: unsafe extern "C" fn(*mut c_void, *mut u8, usize) -> i32,
  extra: unsafe extern "C" fn(*mut c_void, *mut u8, usize, *mut usize) -> i32,
  busy: unsafe extern "C" fn(*mut c_void, u32) -> i32,
  submit: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, i64, i64, i32) -> i32,
  poll: Poll,
  drain: unsafe extern "C" fn(*mut c_void) -> i32,
  close: unsafe extern "C" fn(*mut c_void),
}
static NATIVE: QsvApi = QsvApi {
  open: flinch_qsv_open,
  describe: flinch_qsv_describe,
  extra: flinch_qsv_extra,
  busy: flinch_qsv_busy,
  submit: flinch_qsv_submit,
  poll: flinch_qsv_poll,
  drain: flinch_qsv_drain,
  close: flinch_qsv_close,
};

/// `mfxStatus` names (mfxdefs.h, oneVPL 2.17).
const STATUS: [(i32, &str); 37] = [
  (0, "MFX_ERR_NONE"),
  (-1, "MFX_ERR_UNKNOWN"),
  (-2, "MFX_ERR_NULL_PTR"),
  (-3, "MFX_ERR_UNSUPPORTED"),
  (-4, "MFX_ERR_MEMORY_ALLOC"),
  (-5, "MFX_ERR_NOT_ENOUGH_BUFFER"),
  (-6, "MFX_ERR_INVALID_HANDLE"),
  (-7, "MFX_ERR_LOCK_MEMORY"),
  (-8, "MFX_ERR_NOT_INITIALIZED"),
  (-9, "MFX_ERR_NOT_FOUND"),
  (-10, "MFX_ERR_MORE_DATA"),
  (-11, "MFX_ERR_MORE_SURFACE"),
  (-12, "MFX_ERR_ABORTED"),
  (-13, "MFX_ERR_DEVICE_LOST"),
  (-14, "MFX_ERR_INCOMPATIBLE_VIDEO_PARAM"),
  (-15, "MFX_ERR_INVALID_VIDEO_PARAM"),
  (-16, "MFX_ERR_UNDEFINED_BEHAVIOR"),
  (-17, "MFX_ERR_DEVICE_FAILED"),
  (-18, "MFX_ERR_MORE_BITSTREAM"),
  (-21, "MFX_ERR_GPU_HANG"),
  (-22, "MFX_ERR_REALLOC_SURFACE"),
  (-23, "MFX_ERR_RESOURCE_MAPPED"),
  (-24, "MFX_ERR_NOT_IMPLEMENTED"),
  (-25, "MFX_ERR_MORE_EXTBUFFER"),
  (1, "MFX_WRN_IN_EXECUTION"),
  (2, "MFX_WRN_DEVICE_BUSY"),
  (3, "MFX_WRN_VIDEO_PARAM_CHANGED"),
  (4, "MFX_WRN_PARTIAL_ACCELERATION"),
  (5, "MFX_WRN_INCOMPATIBLE_VIDEO_PARAM"),
  (6, "MFX_WRN_VALUE_NOT_CHANGED"),
  (7, "MFX_WRN_OUT_OF_RANGE"),
  (8, "MFX_TASK_WORKING"),
  (9, "MFX_TASK_BUSY"),
  (10, "MFX_WRN_FILTER_SKIPPED"),
  (12, "MFX_ERR_NONE_PARTIAL_OUTPUT"),
  (13, "MFX_WRN_ALLOC_TIMEOUT_EXPIRED"),
  (-10000, "MFX_ERR_MORE_DATA_SUBMIT_TASK"),
];
/// Open stages in `qsv_bridge.cpp`, so a failure names the step that refused.
const STAGES: [&str; 11] = [
  "ready",
  "load dispatcher",
  "implementation filters",
  "adapter match",
  "session",
  "D3D11 device",
  "frame allocator",
  "query",
  "initialize",
  "parameter sets",
  "bitstream buffers",
];
fn describe(code: i32) -> String {
  match code {
    REPEAT => "no output yet".into(),
    EOF => "end of stream".into(),
    INPUT_FULL => "input full".into(),
    UNSUPPORTED => "unsupported by this GPU".into(),
    INVALID => "invalid input".into(),
    known => STATUS
      .iter()
      .find(|(value, _)| *value == known)
      .map_or_else(|| format!("mfxStatus {known}"), |(_, name)| (*name).into()),
  }
}
fn check(code: i32, stage: &str) -> Result<()> {
  if code == 0 {
    return Ok(());
  }
  Err(EngineError {
    native_code: Some(code),
    ..EngineError::new(ErrorKind::Native, stage, describe(code))
  })
}
/// No runtime, no matching Intel adapter, software-only or changed settings are capability
/// results, so Prepare moves on to the next tier.
fn open_error(code: i32, stage: i32, detail: &str) -> EngineError {
  let step = STAGES
    .get(stage as usize)
    .copied()
    .unwrap_or("unknown step");
  let mut reason = format!("{} at {step}", describe(code));
  if !detail.is_empty() {
    reason.push_str(&format!(" ({detail})"));
  }
  // MFX_ERR_NOT_FOUND / UNSUPPORTED while choosing an implementation mean "no Intel runtime".
  let absent = (2..=3).contains(&stage) && matches!(code, -9 | -3);
  let kind = if code == UNSUPPORTED || absent {
    ErrorKind::Capability
  } else {
    ErrorKind::Native
  };
  EngineError {
    native_code: Some(code),
    ..EngineError::new(kind, "QSV open", reason)
  }
}
/// Hardware runtimes the dispatcher finds (name, API version, vendor), for doctor; read-only.
pub fn inventory() -> Result<Option<String>> {
  let mut text = [0u8; 1024];
  let mut count = 0;
  // SAFETY: writable buffers; the bridge loads the dispatcher and opens no session.
  check(
    unsafe { flinch_qsv_inventory(text.as_mut_ptr(), text.len(), &mut count) },
    "QSV inventory",
  )?;
  Ok((count > 0).then(|| note(&text)))
}
pub struct QsvEncoder {
  handle: NonNull<c_void>,
  api: &'static QsvApi,
  pub extra: Vec<u8>,
  pub drained: bool,
  /// The runtime the dispatcher chose: adapter name and API version, for diagnostics.
  pub description: String,
}
impl QsvEncoder {
  pub fn new(
    device: &ID3D11Device,
    luid: i64,
    width: u32,
    height: u32,
    spec: &SessionSpec,
  ) -> Result<Self> {
    // SAFETY: live D3D11 device stays owned by the caller for encoder creation.
    unsafe { Self::open_raw(device.as_raw(), luid, width, height, spec, &NATIVE) }
  }
  unsafe fn open_raw(
    device: *mut c_void,
    luid: i64,
    width: u32,
    height: u32,
    spec: &SessionSpec,
    api: &'static QsvApi,
  ) -> Result<Self> {
    let config = VendorConfig::new(width, height, spec)?;
    let mut raw = std::ptr::null_mut();
    let mut stage = 0;
    let mut text = [0u8; 256];
    // SAFETY: config and outputs outlive the call; the bridge truncates text to its capacity.
    let code = unsafe {
      (api.open)(
        device,
        luid,
        &config,
        &mut raw,
        &mut stage,
        text.as_mut_ptr(),
        text.len(),
      )
    };
    if code != 0 {
      return Err(open_error(code, stage, &note(&text)));
    }
    let handle =
      NonNull::new(raw).ok_or_else(|| EngineError::protocol("QSV returned null encoder"))?;
    let mut encoder = Self {
      handle,
      api,
      extra: Vec::new(),
      drained: false,
      description: String::new(),
    };
    let mut text = [0u8; 256];
    // SAFETY: live handle; the bridge truncates to the buffer's capacity.
    check(
      unsafe { (api.describe)(handle.as_ptr(), text.as_mut_ptr(), text.len()) },
      "QSV runtime",
    )?;
    encoder.description = note(&text);
    eprintln!("QSV runtime: {}", encoder.description);
    let mut size = 0;
    // SAFETY: zero capacity is a size query; no output is dereferenced by the bridge.
    let code = unsafe { (api.extra)(handle.as_ptr(), std::ptr::null_mut(), 0, &mut size) };
    if code != INPUT_FULL {
      check(code, "QSV parameter-set size")?;
    }
    if size == 0 || size > 1024 {
      return Err(EngineError::protocol("Invalid QSV parameter-set size"));
    }
    encoder.extra.resize(size, 0);
    // SAFETY: exactly sized owned byte slice remains writable throughout the call.
    check(
      unsafe { (api.extra)(handle.as_ptr(), encoder.extra.as_mut_ptr(), size, &mut size) },
      "QSV parameter sets",
    )?;
    Ok(encoder)
  }
  pub fn busy(&self, slot: u32) -> bool {
    // SAFETY: the bridge validates slot and reads only the surface's lock count.
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
    // SAFETY: the pool keeps this texture alive and unwritten until busy(slot) becomes false.
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
    let duration =
      i64::try_from(duration).map_err(|_| EngineError::protocol("Video duration overflow"))?;
    // SAFETY: as above; the runtime reads the texture through the bridge's allocator.
    let code = unsafe {
      (self.api.submit)(
        self.handle.as_ptr(),
        texture,
        slot,
        pts,
        duration,
        i32::from(keyframe),
      )
    };
    if code == INPUT_FULL {
      return Ok(false);
    }
    check(code, "QSV submit")?;
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
    if code == EOF {
      self.drained = true;
      return Ok(None);
    }
    if code == REPEAT {
      return Ok(None);
    }
    check(code, "QSV output")?;
    if data.is_null() || size == 0 || size > 16 * 1024 * 1024 || pts < 0 || duration <= 0 {
      return Err(EngineError::protocol("Invalid compressed QSV packet"));
    }
    // SAFETY: the bridge's bitstream holds at least size bytes until the next poll or close.
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
  /// False when every output buffer is still pending: poll, then drain again.
  pub fn drain(&mut self) -> Result<bool> {
    // SAFETY: live encoder, called on its owner only.
    let code = unsafe { (self.api.drain)(self.handle.as_ptr()) };
    if code == INPUT_FULL {
      return Ok(false);
    }
    check(code, "QSV drain")?;
    Ok(true)
  }
}
impl Drop for QsvEncoder {
  fn drop(&mut self) {
    // SAFETY: exclusive allocation from flinch_qsv_open; the supervisor bounds driver teardown.
    unsafe { (self.api.close)(self.handle.as_ptr()) }
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
