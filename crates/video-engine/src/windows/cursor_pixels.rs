//! Explicit offscreen GPU acceptance. Readback exists only in this test module.
#![allow(clippy::expect_used, clippy::panic)] // Explicit GPU test fixtures only.
use super::*;
use std::time::{Duration, Instant};

fn upload(gpu: &Gpu, color: [u8; 4]) {
  let pixels = color.repeat((gpu.width * gpu.height) as usize);
  // SAFETY: initialized BGRA fixture covers the entire offscreen resource.
  unsafe {
    gpu.context.UpdateSubresource(
      &gpu.desktop,
      0,
      None,
      pixels.as_ptr().cast(),
      gpu.width * 4,
      0,
    );
  }
}
fn readback(gpu: &Gpu, source: &ID3D11Texture2D) -> (Vec<u8>, usize) {
  let mut desc = D3D11_TEXTURE2D_DESC::default();
  // SAFETY: test-only descriptor output; production capture never maps images.
  unsafe {
    source.GetDesc(&mut desc);
  }
  desc.Usage = D3D11_USAGE_STAGING;
  desc.BindFlags = 0;
  desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
  desc.MiscFlags = 0;
  let mut staging = None;
  // SAFETY: matching resource extent; staging exists only for the explicit test.
  unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
    .expect("test staging texture");
  let staging = staging.expect("staging allocation");
  // SAFETY: single owner submits the copy; Flush submits without waiting for completion.
  unsafe {
    gpu.context.CopyResource(&staging, source);
    gpu.context.Flush();
  }
  let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    // SAFETY: nonblocking map is retried only before the finite test deadline.
    match unsafe {
      gpu.context.Map(
        &staging,
        0,
        D3D11_MAP_READ,
        D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
        Some(&mut mapped),
      )
    } {
      Ok(()) => break,
      Err(e)
        if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING
          && Instant::now() < deadline =>
      {
        std::thread::sleep(Duration::from_millis(1))
      },
      Err(e) => panic!("test GPU readback failed: {e}"),
    }
  }
  let pitch = mapped.RowPitch as usize;
  let rows = if desc.Format == DXGI_FORMAT_NV12 {
    desc.Height * 3 / 2
  } else {
    desc.Height
  };
  // SAFETY: mapped rows include the documented NV12 chroma plane and driver row padding.
  let bytes = unsafe {
    std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * rows as usize).to_vec()
  };
  // SAFETY: exactly one Unmap follows the successful test map, before assertions.
  unsafe {
    gpu.context.Unmap(&staging, 0);
  }
  (bytes, pitch)
}
fn pixel(bytes: &[u8], pitch: usize, x: usize, y: usize) -> [u8; 4] {
  bytes[y * pitch + x * 4..y * pitch + x * 4 + 4]
    .try_into()
    .expect("BGRA pixel")
}
#[test]
#[ignore = "Explicit offscreen GPU pixels; no desktop or audio capture"]
fn offscreen_cursor_and_bt709_pixels() {
  let _com = super::super::Apartment::new().expect("COM");
  let output = super::super::gpu::outputs()
    .expect("adapter inventory")
    .into_iter()
    .find(|o| o.info.vendor_id == 0x1002)
    .expect("AMD test adapter");
  let gpu = Gpu::new(&output.adapter, 64, 32, None, 60).expect("offscreen GPU");
  let mut compositor = CursorCompositor::new(&gpu).expect("real cursor shader");
  let background = [32, 64, 96, 255];
  upload(&gpu, background);
  let mut shape = PointerShape {
    kind: 2,
    width: 1,
    height: 1,
    pitch: 4,
    bytes: vec![20, 40, 60, 128],
    x: 4,
    y: 3,
    visible: true,
    generation: 1,
  };
  let (bytes, pitch) = readback(
    &gpu,
    compositor
      .compose(&gpu, &shape)
      .expect("premultiplied color"),
  );
  assert_eq!(pixel(&bytes, pitch, 4, 3), [36, 72, 108, 255]);
  assert_eq!(pixel(&bytes, pitch, 3, 3), background);
  shape.kind = 1;
  shape.width = 4;
  shape.height = 2;
  shape.pitch = 1;
  shape.bytes = vec![0b11000000, 0b10100000];
  shape.generation += 1;
  let (bytes, pitch) = readback(
    &gpu,
    compositor.compose(&gpu, &shape).expect("monochrome masks"),
  );
  for (x, expected) in [[223, 191, 159, 255], background, [255; 4], [0, 0, 0, 255]]
    .into_iter()
    .enumerate()
  {
    assert_eq!(pixel(&bytes, pitch, x + 4, 3), expected);
  }
  shape.kind = 4;
  shape.width = 2;
  shape.height = 1;
  shape.pitch = 8;
  shape.bytes = vec![5, 10, 15, 0, 1, 2, 4, 255];
  shape.generation += 1;
  let (bytes, pitch) = readback(
    &gpu,
    compositor.compose(&gpu, &shape).expect("masked color"),
  );
  assert_eq!(pixel(&bytes, pitch, 4, 3), [5, 10, 15, 255]);
  assert_eq!(pixel(&bytes, pitch, 5, 3), [33, 66, 100, 255]);
  // Movement reuses the immutable uploaded shape and clips its negative origin.
  shape.x = -1;
  let (bytes, pitch) = readback(
    &gpu,
    compositor
      .compose(&gpu, &shape)
      .expect("clipped cached shape"),
  );
  assert_eq!(pixel(&bytes, pitch, 0, 3), [33, 66, 100, 255]);
  assert_eq!(pixel(&bytes, pitch, 4, 3), background);
  shape.visible = false;
  let (bytes, pitch) = readback(
    &gpu,
    compositor.compose(&gpu, &shape).expect("hidden cursor"),
  );
  assert_eq!(pixel(&bytes, pitch, 0, 3), background);

  for (bgra, expected) in [
    ([0, 0, 0, 255], [16, 128, 128]),
    ([255; 4], [235, 128, 128]),
    ([0, 0, 255, 255], [63, 102, 240]),
    ([0, 255, 0, 255], [173, 42, 26]),
    ([255, 0, 0, 255], [32, 240, 118]),
  ] {
    upload(&gpu, bgra);
    gpu
      .convert(&gpu.desktop, 0)
      .expect("BT709 limited-range GPU conversion");
    let (bytes, pitch) = readback(&gpu, &gpu.surfaces[0]);
    let actual = [
      bytes[8 * pitch + 8],
      bytes[gpu.height as usize * pitch + 4 * pitch + 8],
      bytes[gpu.height as usize * pitch + 4 * pitch + 9],
    ];
    for (actual, expected) in actual.into_iter().zip(expected) {
      assert!(
        actual.abs_diff(expected) <= 2,
        "BT709 color {bgra:?}: {actual} differs from {expected}"
      );
    }
  }
}
