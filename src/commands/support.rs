use windows::{
  Win32::Graphics::{
    Direct3D10::ID3D10Device,
    Dxgi::{
      Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND,
      IDXGIFactory1, IDXGIOutput6,
    },
  },
  core::Interface,
};

fn driver_version(version: i64) -> String {
  let version = version as u64;
  format!(
    "{}.{}.{}.{}",
    version >> 48,
    (version >> 32) & 0xffff,
    (version >> 16) & 0xffff,
    version & 0xffff
  )
}

pub(super) fn graphics() -> anyhow::Result<serde_json::Value> {
  // SAFETY: the requested COM interface is a DXGI factory; no borrowed pointers escape.
  let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
  let mut gpus = Vec::new();
  let mut outputs = Vec::new();
  // Bound enumeration even if a faulty driver never reports the end.
  for index in 0..32 {
    // SAFETY: factory is valid and the index is bounded.
    let adapter = match unsafe { factory.EnumAdapters1(index) } {
      Ok(adapter) => adapter,
      Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => {
        return Ok(serde_json::json!({ "gpus": gpus, "outputs": outputs }));
      },
      Err(error) => return Err(error.into()),
    };
    // SAFETY: adapter is a live DXGI interface.
    let desc = unsafe { adapter.GetDesc1()? };
    // Software adapters do not have a hardware driver to diagnose.
    if desc.Flags & 2 != 0 {
      continue;
    }
    // SAFETY: ID3D10Device is the documented interface for querying the UMD version.
    let driver = unsafe { adapter.CheckInterfaceSupport(&ID3D10Device::IID) };
    gpus.push(serde_json::json!({
      "name": String::from_utf16_lossy(&desc.Description).trim_end_matches('\0'),
      "driver": driver.as_ref().ok().map(|version| driver_version(*version)),
      "driverError": driver.err().map(|error| error.to_string()),
    }));
    for output_index in 0..32 {
      // SAFETY: adapter is valid and the index is bounded.
      let output = match unsafe { adapter.EnumOutputs(output_index) } {
        Ok(output) => output,
        Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
        Err(error) => return Err(error.into()),
      };
      // SAFETY: output is a live DXGI interface.
      let basic = unsafe { output.GetDesc()? };
      if !basic.AttachedToDesktop.as_bool() {
        continue;
      }
      let advanced = output.cast::<IDXGIOutput6>().and_then(|output| {
        // SAFETY: the cast succeeded and output owns a valid IDXGIOutput6 interface.
        unsafe { output.GetDesc1() }
      });
      outputs.push(serde_json::json!({
        "x": basic.DesktopCoordinates.left, "y": basic.DesktopCoordinates.top,
        "hdr": advanced.as_ref().ok().map(|desc| desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020),
        "error": advanced.err().map(|error| error.to_string()),
      }));
      if output_index == 31 {
        anyhow::bail!("Display enumeration limit reached");
      }
    }
  }
  anyhow::bail!("GPU enumeration limit reached")
}

/// Open the active log directory itself in Explorer.
pub(super) fn open_logs() -> anyhow::Result<()> {
  use std::os::windows::ffi::OsStrExt;
  use windows::{
    Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    core::{PCWSTR, w},
  };
  let directory = crate::logging::support_log_directory()?;
  let path: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
  // SAFETY: the verified directory is null terminated and lives through this call.
  // The explore verb opens its contents rather than selecting it in its parent.
  let status = unsafe {
    ShellExecuteW(
      None,
      w!("explore"),
      PCWSTR(path.as_ptr()),
      None,
      None,
      SW_SHOWNORMAL,
    )
  };
  let code = status.0 as isize;
  anyhow::ensure!(
    code > 32,
    "Windows could not open the log directory (ShellExecute status {code})"
  );
  Ok(())
}
#[cfg(test)]
mod tests {
  #[test]
  fn decodes_all_four_driver_version_components() {
    assert_eq!(super::driver_version(0x0020_0000_000f_1e61), "32.0.15.7777");
  }
}
