// src/screenshot/platform/windows/utils.rs
// Shared utility functions for Windows capture methods

use anyhow::{Context, Result};
use windows::{
  Win32::{
    Foundation::HMODULE,
    Graphics::{
      Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
      Direct3D11::{D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext},
      Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput},
    },
  },
  core::Interface,
};

/// Helper to format HRESULT errors with common error names
pub fn format_hresult(hresult: windows::core::HRESULT) -> String {
  let code = hresult.0 as u32;
  use windows::Win32::Graphics::Dxgi as dxgi;
  let name = match hresult {
    dxgi::DXGI_ERROR_INVALID_CALL => "DXGI_ERROR_INVALID_CALL",
    dxgi::DXGI_ERROR_NOT_FOUND => "DXGI_ERROR_NOT_FOUND",
    dxgi::DXGI_ERROR_MORE_DATA => "DXGI_ERROR_MORE_DATA",
    dxgi::DXGI_ERROR_UNSUPPORTED => "DXGI_ERROR_UNSUPPORTED",
    dxgi::DXGI_ERROR_DEVICE_REMOVED => "DXGI_ERROR_DEVICE_REMOVED",
    dxgi::DXGI_ERROR_DEVICE_HUNG => "DXGI_ERROR_DEVICE_HUNG",
    dxgi::DXGI_ERROR_DEVICE_RESET => "DXGI_ERROR_DEVICE_RESET",
    dxgi::DXGI_ERROR_WAS_STILL_DRAWING => "DXGI_ERROR_WAS_STILL_DRAWING",
    dxgi::DXGI_ERROR_FRAME_STATISTICS_DISJOINT => "DXGI_ERROR_FRAME_STATISTICS_DISJOINT",
    dxgi::DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => "DXGI_ERROR_NOT_CURRENTLY_AVAILABLE",
    dxgi::DXGI_ERROR_REMOTE_CLIENT_DISCONNECTED => "DXGI_ERROR_REMOTE_CLIENT_DISCONNECTED",
    dxgi::DXGI_ERROR_ACCESS_LOST => "DXGI_ERROR_ACCESS_LOST",
    dxgi::DXGI_ERROR_WAIT_TIMEOUT => "DXGI_ERROR_WAIT_TIMEOUT",
    dxgi::DXGI_ERROR_SESSION_DISCONNECTED => "DXGI_ERROR_SESSION_DISCONNECTED",
    dxgi::DXGI_ERROR_RESTRICT_TO_OUTPUT_STALE => "DXGI_ERROR_RESTRICT_TO_OUTPUT_STALE",
    dxgi::DXGI_ERROR_CANNOT_PROTECT_CONTENT => "DXGI_ERROR_CANNOT_PROTECT_CONTENT",
    dxgi::DXGI_ERROR_ACCESS_DENIED => "DXGI_ERROR_ACCESS_DENIED",
    windows::Win32::Foundation::E_ACCESSDENIED => "E_ACCESSDENIED",
    windows::Win32::Foundation::E_OUTOFMEMORY => "E_OUTOFMEMORY",
    _ => "UNKNOWN",
  };
  format!("0x{:08X} ({})", code, name)
}

/// Find the first hardware adapter that has at least one output, skipping software renderers
/// Returns (adapter, output) tuple
pub unsafe fn find_hardware_adapter_with_output(
  factory: &IDXGIFactory1,
) -> Result<(IDXGIAdapter1, IDXGIOutput)> {
  find_hardware_adapter_with_output_with(
    |index| unsafe { factory.EnumAdapters1(index) },
    |adapter| unsafe { adapter.GetDesc1() }.map(|desc| String::from_utf16_lossy(&desc.Description)),
    |adapter| unsafe { adapter.EnumOutputs(0) },
  )
}

fn find_hardware_adapter_with_output_with<A, O>(
  enumerate: impl Fn(u32) -> windows::core::Result<A>,
  description: impl Fn(&A) -> windows::core::Result<String>,
  output: impl Fn(&A) -> windows::core::Result<O>,
) -> Result<(A, O)> {
  let mut adapter_idx = 0;
  loop {
    let adapter = match enumerate(adapter_idx) {
      Ok(adapter) => adapter,
      Err(_) => {
        return Err(anyhow::anyhow!("No hardware adapter with outputs found"));
      },
    };

    // Get adapter description to check if it's a software renderer
    if let Ok(adapter_desc) = description(&adapter) {
      let desc_str = adapter_desc;
      let desc_trimmed = desc_str.trim_end_matches('\0');

      // Skip Microsoft Basic Render Driver (software renderer)
      if desc_trimmed.contains("Microsoft Basic Render Driver") {
        tracing::debug!(
            operation = "find_hardware_adapter",
            adapter_index = adapter_idx,
            description = %desc_trimmed,
            "Skipping software renderer"
        );
        adapter_idx += 1;
        continue;
      }

      // Check if this adapter has any outputs
      if let Ok(output) = output(&adapter) {
        tracing::debug!(
            operation = "find_hardware_adapter",
            adapter_index = adapter_idx,
            output_index = 0,
            description = %desc_trimmed,
            "Found hardware adapter with output"
        );
        return Ok((adapter, output));
      }

      tracing::debug!(
          operation = "find_hardware_adapter",
          adapter_index = adapter_idx,
          description = %desc_trimmed,
          "Adapter has no outputs, trying next"
      );
    }

    adapter_idx += 1;
  }
}

/// Find the adapter and output that matches the given monitor bounds.
///
/// Matches by comparing the output's DesktopCoordinates with the monitor's position and size.
/// If no monitor is specified, returns the first available output (current behavior).
pub unsafe fn find_adapter_with_output_for_monitor(
  factory: &IDXGIFactory1,
  target_x: i32,
  target_y: i32,
  target_width: u32,
  target_height: u32,
) -> Result<(IDXGIAdapter1, IDXGIOutput)> {
  use windows::Win32::Graphics::Dxgi::IDXGIOutput1;
  use windows::core::Interface;

  find_adapter_with_output_for_monitor_with(
    (target_x, target_y, target_width, target_height),
    |index| unsafe { factory.EnumAdapters1(index) },
    |adapter| unsafe { adapter.GetDesc1() }.map(|desc| String::from_utf16_lossy(&desc.Description)),
    |adapter, index| unsafe { adapter.EnumOutputs(index) },
    |output| {
      let output: IDXGIOutput1 = output.cast()?;
      unsafe { output.GetDesc() }.map(|desc| desc.DesktopCoordinates)
    },
  )
}

fn find_adapter_with_output_for_monitor_with<A, O>(
  target: (i32, i32, u32, u32),
  enumerate: impl Fn(u32) -> windows::core::Result<A>,
  description: impl Fn(&A) -> windows::core::Result<String>,
  output: impl Fn(&A, u32) -> windows::core::Result<O>,
  bounds: impl Fn(&O) -> windows::core::Result<windows::Win32::Foundation::RECT>,
) -> Result<(A, O)> {
  let (target_x, target_y, target_width, target_height) = target;
  let mut adapter_idx = 0;
  loop {
    let adapter = match enumerate(adapter_idx) {
      Ok(adapter) => adapter,
      Err(_) => {
        return Err(anyhow::anyhow!(
          "No adapter with output matching bounds ({}, {}, {}x{}) found",
          target_x,
          target_y,
          target_width,
          target_height
        ));
      },
    };

    // Get adapter description to check if it's a software renderer
    if let Ok(adapter_desc) = description(&adapter) {
      let desc_str = adapter_desc;
      let desc_trimmed = desc_str.trim_end_matches('\0');

      // Skip Microsoft Basic Render Driver (software renderer)
      if desc_trimmed.contains("Microsoft Basic Render Driver") {
        adapter_idx += 1;
        continue;
      }

      // Check each output on this adapter
      let mut output_idx = 0;
      while let Ok(output) = output(&adapter, output_idx) {
        // Cast to IDXGIOutput1 to get the output description
        if let Ok(coords) = bounds(&output) {
          let out_x = coords.left;
          let out_y = coords.top;
          let out_width = (coords.right - coords.left) as u32;
          let out_height = (coords.bottom - coords.top) as u32;

          // Check if this output matches the target monitor
          if out_x == target_x
            && out_y == target_y
            && out_width == target_width
            && out_height == target_height
          {
            tracing::debug!(
              operation = "find_adapter_for_monitor",
              adapter_index = adapter_idx,
              output_index = output_idx,
              x = out_x,
              y = out_y,
              width = out_width,
              height = out_height,
              "Found matching output for monitor"
            );
            return Ok((adapter, output));
          }
        }
        output_idx += 1;
      }
    }

    adapter_idx += 1;
  }
}

/// Helper function to create D3D11 device (shared between Desktop Duplication and WGC)
pub unsafe fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
  let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
    .inspect_err(|e| {
      let hresult = e.code();
      tracing::error!(
          operation = "create_d3d11_device",
          hresult = %format_hresult(hresult),
          "Failed to create DXGI factory"
      );
    })
    .context("Failed to create DXGI factory")?;

  let (adapter1, _output) = unsafe { find_hardware_adapter_with_output(&factory) }
    .context("Failed to find hardware adapter with outputs")?;

  let adapter: IDXGIAdapter = adapter1
    .cast()
    .inspect_err(|e| {
      let hresult = e.code();
      tracing::error!(
          operation = "create_d3d11_device",
          hresult = %format_hresult(hresult),
          "Failed to cast adapter"
      );
    })
    .context("Failed to cast adapter")?;

  let mut device: Option<ID3D11Device> = None;
  let mut device_context: Option<ID3D11DeviceContext> = None;
  let mut feature_level = D3D_FEATURE_LEVEL_11_0;

  let feature_levels = [
    D3D_FEATURE_LEVEL_11_0,
    windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_1,
    windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_10_0,
  ];

  unsafe {
    D3D11CreateDevice(
      Some(&adapter),
      D3D_DRIVER_TYPE_UNKNOWN,
      HMODULE(std::ptr::null_mut()),
      windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG::default(),
      Some(&feature_levels),
      windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION,
      Some(&mut device),
      Some(&mut feature_level),
      Some(&mut device_context),
    )
  }
  .inspect_err(|e| {
    let hresult = e.code();
    tracing::error!(
        operation = "create_d3d11_device",
        hresult = %format_hresult(hresult),
        "Failed to create D3D11 device"
    );
  })
  .context("Failed to create D3D11 device")?;

  let device = device.ok_or_else(|| anyhow::anyhow!("D3D11 device is None after creation"))?;
  let device_context =
    device_context.ok_or_else(|| anyhow::anyhow!("D3D11 device context is None after creation"))?;

  Ok((device, device_context))
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::Foundation::RECT;

  #[test]
  fn adapter_enumeration_skips_software_broken_descriptions_and_missing_outputs() {
    let end =
      || windows::core::Error::from_hresult(windows::Win32::Graphics::Dxgi::DXGI_ERROR_NOT_FOUND);
    let enumerate = |index| if index < 4 { Ok(index) } else { Err(end()) };
    let description = |index: &u32| match index {
      0 => Ok("Microsoft Basic Render Driver\0".into()),
      1 => Err(end()),
      _ => Ok("Hardware GPU\0".into()),
    };
    let result = find_hardware_adapter_with_output_with(enumerate, description, |index| {
      if *index == 3 { Ok(17) } else { Err(end()) }
    })
    .unwrap();
    assert_eq!(result, (3, 17));
    assert!(
      find_hardware_adapter_with_output_with(enumerate, description, |_| Err::<(), _>(end()))
        .unwrap_err()
        .to_string()
        .contains("No hardware adapter")
    );
    let target = (-1920, -100, 1920, 1080);
    let output = |index: &u32, output: u32| {
      if *index >= 2 && output < 3 {
        Ok((*index, output))
      } else {
        Err(end())
      }
    };
    let bounds = |&(adapter, output): &(u32, u32)| match (adapter, output) {
      (_, 0) => Err(end()),
      (3, 2) => Ok(RECT {
        left: -1920,
        top: -100,
        right: 0,
        bottom: 980,
      }),
      _ => Ok(RECT {
        left: 0,
        top: 0,
        right: 640,
        bottom: 480,
      }),
    };
    assert_eq!(
      find_adapter_with_output_for_monitor_with(target, enumerate, description, output, bounds)
        .unwrap(),
      (3, (3, 2))
    );
    let error = find_adapter_with_output_for_monitor_with(
      (1, 2, 3, 4),
      enumerate,
      description,
      output,
      bounds,
    )
    .unwrap_err();
    assert!(error.to_string().contains("(1, 2, 3x4)"));
  }
  #[test]
  fn dxgi_hresult_names_distinguish_timeout_drawing_and_protection() {
    for (code, name) in [
      (0x887A0027u32, "DXGI_ERROR_WAIT_TIMEOUT"),
      (0x887A000Au32, "DXGI_ERROR_WAS_STILL_DRAWING"),
      (0x887A0022u32, "DXGI_ERROR_NOT_CURRENTLY_AVAILABLE"),
      (0x887A0028u32, "DXGI_ERROR_SESSION_DISCONNECTED"),
      (0x887A002Au32, "DXGI_ERROR_CANNOT_PROTECT_CONTENT"),
      (0x887A002Bu32, "DXGI_ERROR_ACCESS_DENIED"),
    ] {
      assert_eq!(
        super::format_hresult(windows::core::HRESULT(code as i32)),
        format!("0x{code:08X} ({name})")
      );
    }
  }
}
