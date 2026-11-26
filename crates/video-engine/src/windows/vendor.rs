//! What the vendor encoders (NVENC, QSV) share: their settings and bridge results.
use crate::{EngineError, Result, types::*};

/// Bridge results outside each vendor's own status range (`nvenc_bridge.c`, `qsv_bridge.cpp`).
pub(super) const REPEAT: i32 = 1000;
pub(super) const EOF: i32 = 1001;
pub(super) const INPUT_FULL: i32 = 1002;
pub(super) const UNSUPPORTED: i32 = 1003;
pub(super) const INVALID: i32 = 1004;

/// Mirrors `FlinchNvencConfig` and `FlinchQsvConfig` in the bridges.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VendorConfig {
  pub width: u32,
  pub height: u32,
  pub fps: u32,
  pub constant_qp: u32,
  pub qp_i: u32,
  pub qp_p: u32,
  pub bitrate: u32,
  pub gop: u32,
}
impl VendorConfig {
  /// The same choices as AMF: the shared IDR period (`KEYFRAME_SECONDS`), no B-frames, the
  /// app's QPs or VBR.
  pub(crate) fn new(width: u32, height: u32, spec: &SessionSpec) -> Result<Self> {
    if width < 16
      || height < 16
      || !width.is_multiple_of(2)
      || !height.is_multiple_of(2)
      || spec.fps == 0
    {
      return Err(EngineError::protocol(
        "Invalid hardware encoder picture or frame rate",
      ));
    }
    let mut config = Self {
      width,
      height,
      fps: spec.fps,
      gop: keyframe_interval(spec.fps),
      ..Default::default()
    };
    match spec.quality {
      Quality::Cqp { i, p } => {
        if i > 51 || p > 51 {
          return Err(EngineError::protocol("H.264 QP must be at most 51"));
        }
        config.constant_qp = 1;
        config.qp_i = u32::from(i);
        config.qp_p = u32::from(p);
      },
      Quality::Vbr { bitrate } => {
        if bitrate == 0 {
          return Err(EngineError::protocol("VBR bitrate must be positive"));
        }
        config.bitrate = bitrate;
      },
    }
    Ok(config)
  }
}
/// A bridge's NUL-terminated note, trimmed.
pub(super) fn note(text: &[u8]) -> String {
  let end = text
    .iter()
    .position(|byte| *byte == 0)
    .unwrap_or(text.len());
  String::from_utf8_lossy(&text[..end]).trim().to_owned()
}
