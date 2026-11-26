//! Windows' "SDR content brightness" of an HDR display: the brightness, in nits, at which it
//! composes SDR white into the HDR desktop. Read once when a capture opens, from the display
//! configuration. Read-only: no mode or setting is changed.
use crate::{EngineError, ErrorKind, Result};
use windows::{Win32::Devices::Display::*, core::HRESULT};

/// `DISPLAYCONFIG_SDR_WHITE_LEVEL::SDRWhiteLevel` counts thousandths of 80 nits. Windows' slider
/// spans 80 to 480 nits; anything outside a display's possible range is refused.
pub fn nits_from_level(level: u32) -> Result<f32> {
  let nits = level as f32 * 80.0 / 1000.0;
  if !(80.0..=10_000.0).contains(&nits) {
    return Err(EngineError::protocol(format!(
      "SDR white level {level} ({nits} nits) is outside a display's range"
    )));
  }
  Ok(nits)
}

/// The SDR white level of the active display whose GDI name is `device_name` (`\\.\DISPLAY1`,
/// as DXGI reports it).
pub fn sdr_white_nits(device_name: &str) -> Result<f32> {
  // The topology can change between sizing and reading the paths; retry a bounded number of
  // times rather than reading a partial list.
  for _ in 0..3 {
    let (mut path_count, mut mode_count) = (0u32, 0u32);
    // SAFETY: counts are written into stack-owned values.
    let status = unsafe {
      GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    };
    HRESULT::from_win32(status.0)
      .ok()
      .map_err(|e| EngineError::native("display configuration size", e))?;
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    // SAFETY: both arrays hold the counts passed; Windows writes back how many it filled.
    let status = unsafe {
      QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut path_count,
        paths.as_mut_ptr(),
        &mut mode_count,
        modes.as_mut_ptr(),
        None,
      )
    };
    if status == windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER {
      continue;
    }
    HRESULT::from_win32(status.0)
      .ok()
      .map_err(|e| EngineError::native("display configuration", e))?;
    paths.truncate(path_count as usize);
    for path in &paths {
      let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
          r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
          size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
          adapterId: path.sourceInfo.adapterId,
          id: path.sourceInfo.id,
        },
        ..Default::default()
      };
      // SAFETY: the header describes this packet's own type and size.
      let status = unsafe { DisplayConfigGetDeviceInfo(&mut source.header) };
      HRESULT::from_win32(status as u32)
        .ok()
        .map_err(|e| EngineError::native("display source name", e))?;
      if utf16(&source.viewGdiDeviceName) != device_name {
        continue;
      }
      let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
          r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
          size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
          adapterId: path.targetInfo.adapterId,
          id: path.targetInfo.id,
        },
        SDRWhiteLevel: 0,
      };
      // SAFETY: as above; the white level belongs to the path's target.
      let status = unsafe { DisplayConfigGetDeviceInfo(&mut white.header) };
      HRESULT::from_win32(status as u32)
        .ok()
        .map_err(|e| EngineError::native("SDR white level", e))?;
      return nits_from_level(white.SDRWhiteLevel);
    }
    return Err(EngineError::new(
      ErrorKind::Capability,
      "SDR white level",
      format!("No active display path is named {device_name}"),
    ));
  }
  Err(EngineError::new(
    ErrorKind::Capability,
    "SDR white level",
    "The display configuration kept changing while it was read",
  ))
}

fn utf16(name: &[u16]) -> String {
  let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
  String::from_utf16_lossy(&name[..end])
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;

  #[test]
  fn white_levels_are_thousandths_of_80_nits_and_impossible_ones_are_refused() {
    assert_eq!(nits_from_level(1000).expect("minimum"), 80.0);
    assert_eq!(nits_from_level(3000).expect("240 nits"), 240.0);
    assert_eq!(nits_from_level(6000).expect("maximum slider"), 480.0);
    for level in [0, 999, 125_001, u32::MAX] {
      assert!(nits_from_level(level).is_err(), "{level}");
    }
  }

  #[test]
  fn gdi_names_end_at_their_terminator() {
    let mut raw = [0u16; 32];
    for (slot, unit) in raw.iter_mut().zip(r"\\.\DISPLAY12".encode_utf16()) {
      *slot = unit;
    }
    assert_eq!(utf16(&raw), r"\\.\DISPLAY12");
    assert_eq!(utf16(&[u16::from(b'A'); 32]), "A".repeat(32));
  }

  #[test]
  fn an_unknown_display_is_a_capability_error_and_known_ones_read_without_changes() {
    // Read-only queries of the display configuration; nothing is captured or changed.
    let error = sdr_white_nits(r"\\.\NO-SUCH-DISPLAY").expect_err("unknown display");
    assert!(
      error.kind == ErrorKind::Capability || error.stage.starts_with("display configuration"),
      "{error}"
    );
    let _com = super::super::Apartment::new().expect("COM");
    for output in super::super::gpu::outputs().unwrap_or_default() {
      if output.info.hdr {
        let nits = sdr_white_nits(&output.info.id).expect("HDR display white level");
        eprintln!("{} SDR white: {nits} nits", output.info.id);
        assert!(
          (80.0..=480.0).contains(&nits),
          "{} at {nits}",
          output.info.id
        );
      }
    }
  }
}
