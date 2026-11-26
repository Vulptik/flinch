//! NVIDIA NVENC through Flinch's C ABI over the pinned public header (`native/nvenc`).
//!
//! Everything above the native calls is exercised here with a scripted table; the driver itself is
//! covered by `cargo xtask test:video:vendor` on NVIDIA hardware.
use super::vendor::{EOF, INPUT_FULL, INVALID, REPEAT, UNSUPPORTED, VendorConfig, note};
use crate::{EngineError, ErrorKind, Result, types::*};
use std::{ffi::c_void, ptr::NonNull};
use windows::{
  Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
  core::Interface,
};

unsafe extern "C" {
  fn flinch_nvenc_version(version: *mut u32) -> i32;
  fn flinch_nvenc_open(
    device: *mut c_void,
    config: *const VendorConfig,
    output: *mut *mut c_void,
    stage: *mut i32,
    text: *mut u8,
    capacity: usize,
  ) -> i32;
  fn flinch_nvenc_extra(
    value: *mut c_void,
    output: *mut u8,
    capacity: usize,
    size: *mut usize,
  ) -> i32;
  fn flinch_nvenc_busy(value: *mut c_void, slot: u32) -> i32;
  fn flinch_nvenc_submit(
    value: *mut c_void,
    texture: *mut c_void,
    slot: u32,
    pts: i64,
    duration: i64,
    keyframe: i32,
  ) -> i32;
  fn flinch_nvenc_poll(
    value: *mut c_void,
    data: *mut *const u8,
    size: *mut usize,
    pts: *mut i64,
    duration: *mut i64,
    keyframe: *mut i32,
  ) -> i32;
  fn flinch_nvenc_drain(value: *mut c_void) -> i32;
  fn flinch_nvenc_close(value: *mut c_void);
}
type Open = unsafe extern "C" fn(
  *mut c_void,
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
pub(crate) struct NvencApi {
  version: unsafe extern "C" fn(*mut u32) -> i32,
  open: Open,
  extra: unsafe extern "C" fn(*mut c_void, *mut u8, usize, *mut usize) -> i32,
  busy: unsafe extern "C" fn(*mut c_void, u32) -> i32,
  submit: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, i64, i64, i32) -> i32,
  poll: Poll,
  drain: unsafe extern "C" fn(*mut c_void) -> i32,
  close: unsafe extern "C" fn(*mut c_void),
}
static NATIVE: NvencApi = NvencApi {
  version: flinch_nvenc_version,
  open: flinch_nvenc_open,
  extra: flinch_nvenc_extra,
  busy: flinch_nvenc_busy,
  submit: flinch_nvenc_submit,
  poll: flinch_nvenc_poll,
  drain: flinch_nvenc_drain,
  close: flinch_nvenc_close,
};

/// `NV_ENC_ERR_INVALID_VERSION`: the driver predates this header's API.
const INVALID_VERSION: i32 = 15;
/// NVENC API 12.2 needs Windows driver 551.76 or newer.
pub const MINIMUM_DRIVER: &str = "551.76";
/// `NVENCSTATUS` names in declaration order (nvEncodeAPI.h 12.2).
const STATUS: [&str; 27] = [
  "NV_ENC_SUCCESS",
  "NV_ENC_ERR_NO_ENCODE_DEVICE",
  "NV_ENC_ERR_UNSUPPORTED_DEVICE",
  "NV_ENC_ERR_INVALID_ENCODERDEVICE",
  "NV_ENC_ERR_INVALID_DEVICE",
  "NV_ENC_ERR_DEVICE_NOT_EXIST",
  "NV_ENC_ERR_INVALID_PTR",
  "NV_ENC_ERR_INVALID_EVENT",
  "NV_ENC_ERR_INVALID_PARAM",
  "NV_ENC_ERR_INVALID_CALL",
  "NV_ENC_ERR_OUT_OF_MEMORY",
  "NV_ENC_ERR_ENCODER_NOT_INITIALIZED",
  "NV_ENC_ERR_UNSUPPORTED_PARAM",
  "NV_ENC_ERR_LOCK_BUSY",
  "NV_ENC_ERR_NOT_ENOUGH_BUFFER",
  "NV_ENC_ERR_INVALID_VERSION",
  "NV_ENC_ERR_MAP_FAILED",
  "NV_ENC_ERR_NEED_MORE_INPUT",
  "NV_ENC_ERR_ENCODER_BUSY",
  "NV_ENC_ERR_EVENT_NOT_REGISTERD",
  "NV_ENC_ERR_GENERIC",
  "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
  "NV_ENC_ERR_UNIMPLEMENTED",
  "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
  "NV_ENC_ERR_RESOURCE_NOT_REGISTERED",
  "NV_ENC_ERR_RESOURCE_NOT_MAPPED",
  "NV_ENC_ERR_NEED_MORE_OUTPUT",
];
/// Open stages in `nvenc_bridge.c`, so a failure names the step that refused.
const STAGES: [&str; 12] = [
  "ready",
  "load nvEncodeAPI64.dll",
  "runtime version",
  "API instance",
  "D3D11 session",
  "H.264 support",
  "capabilities",
  "preset P4 low latency",
  "initialize",
  "bitstream buffers",
  "completion events",
  "parameter sets",
];
fn describe(code: i32) -> String {
  match code {
    REPEAT => "no output yet".into(),
    EOF => "end of stream".into(),
    INPUT_FULL => "input full".into(),
    UNSUPPORTED => "unsupported by this GPU".into(),
    INVALID => "invalid input".into(),
    negative if negative < 0 => format!("Windows error {}", -i64::from(negative)),
    known => STATUS
      .get(known as usize)
      .map_or_else(|| format!("NVENC status {known}"), |name| (*name).into()),
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
/// The open failure, classified: an absent runtime, an old driver or an unsupported GPU are
/// capability results that let Prepare move on to the next tier.
fn open_error(code: i32, stage: i32, driver: &str) -> EngineError {
  let step = STAGES
    .get(stage as usize)
    .copied()
    .unwrap_or("unknown step");
  let mut reason = format!("{} at {step}", describe(code));
  if !driver.is_empty() {
    reason.push_str(&format!(" ({driver})"));
  }
  let kind = if code == INVALID_VERSION {
    reason.push_str(&format!("; NVENC needs driver {MINIMUM_DRIVER} or newer"));
    ErrorKind::Capability
  } else if code == UNSUPPORTED || (stage == 1 && code < 0) {
    ErrorKind::Capability
  } else {
    ErrorKind::Native
  };
  EngineError {
    native_code: Some(code),
    ..EngineError::new(kind, "NVENC open", reason)
  }
}
/// The driver's highest NVENC API version, as `major.minor`.
pub fn version() -> Result<String> {
  runtime_version(&NATIVE)
}
fn runtime_version(api: &NvencApi) -> Result<String> {
  let mut value = 0;
  // SAFETY: the bridge writes one u32 and loads only the system driver's runtime.
  check(unsafe { (api.version)(&mut value) }, "NVENC runtime")?;
  Ok(format!("{}.{}", value >> 4, value & 15))
}
pub struct NvencEncoder {
  handle: NonNull<c_void>,
  api: &'static NvencApi,
  pub extra: Vec<u8>,
  pub drained: bool,
}
impl NvencEncoder {
  pub fn new(device: &ID3D11Device, width: u32, height: u32, spec: &SessionSpec) -> Result<Self> {
    // SAFETY: live D3D11 device stays owned by the caller for encoder creation.
    unsafe { Self::open_raw(device.as_raw(), width, height, spec, &NATIVE) }
  }
  unsafe fn open_raw(
    device: *mut c_void,
    width: u32,
    height: u32,
    spec: &SessionSpec,
    api: &'static NvencApi,
  ) -> Result<Self> {
    let config = VendorConfig::new(width, height, spec)?;
    let mut raw = std::ptr::null_mut();
    let mut stage = 0;
    let mut text = [0u8; 256];
    // SAFETY: config and outputs outlive the call; the bridge truncates text to its capacity.
    let code = unsafe {
      (api.open)(
        device,
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
      NonNull::new(raw).ok_or_else(|| EngineError::protocol("NVENC returned null encoder"))?;
    let mut encoder = Self {
      handle,
      api,
      extra: Vec::new(),
      drained: false,
    };
    let mut size = 0;
    // SAFETY: zero capacity is a size query; no output is dereferenced by the bridge.
    let code = unsafe { (api.extra)(handle.as_ptr(), std::ptr::null_mut(), 0, &mut size) };
    if code != INPUT_FULL {
      check(code, "NVENC parameter-set size")?;
    }
    if size == 0 || size > 1024 {
      return Err(EngineError::protocol("Invalid NVENC parameter-set size"));
    }
    encoder.extra.resize(size, 0);
    // SAFETY: exactly sized owned byte slice remains writable throughout the call.
    check(
      unsafe { (api.extra)(handle.as_ptr(), encoder.extra.as_mut_ptr(), size, &mut size) },
      "NVENC parameter sets",
    )?;
    Ok(encoder)
  }
  pub fn busy(&self, slot: u32) -> bool {
    // SAFETY: the bridge validates slot and reads only its mapping state.
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
    // SAFETY: as above; the bridge maps the registered texture only for this slot's frame.
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
    check(code, "NVENC submit")?;
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
    check(code, "NVENC output")?;
    if data.is_null() || size == 0 || size > 16 * 1024 * 1024 || pts < 0 || duration <= 0 {
      return Err(EngineError::protocol("Invalid compressed NVENC packet"));
    }
    // SAFETY: the locked bitstream holds at least size bytes until the next poll or close.
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
    check(
      unsafe { (self.api.drain)(self.handle.as_ptr()) },
      "NVENC drain",
    )?;
    Ok(true)
  }
}
impl Drop for NvencEncoder {
  fn drop(&mut self) {
    // SAFETY: exclusive allocation from flinch_nvenc_open; the supervisor bounds driver teardown.
    unsafe { (self.api.close)(self.handle.as_ptr()) }
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
