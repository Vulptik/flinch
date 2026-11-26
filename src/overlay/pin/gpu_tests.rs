#![allow(clippy::expect_used)]

use super::*;

/// A deterministic image with detail in every channel.
fn pattern(width: u32, height: u32) -> Vec<u8> {
  (0..width * height)
    .flat_map(|index| {
      let (x, y) = (index % width, index / width);
      [
        (x * 7 + y * 3) as u8,
        ((x * 13) ^ (y * 5)) as u8,
        (y * 11) as u8,
        if (x + y) % 9 == 0 { 128 } else { 255 },
      ]
    })
    .collect()
}

fn draw(gpu: &Gpu, source: &Source, size: (u32, u32)) -> Vec<u8> {
  render_offscreen(gpu, size.0, size.1, |context| {
    draw_image(context, source, size, (0.0, 0.0));
  })
  .expect("render")
}

#[test]
fn one_to_one_is_the_exact_image_and_enlarging_repeats_pixels() {
  let gpu = Gpu::warp().expect("WARP");
  let (width, height) = (37, 23);
  let rgba = pattern(width, height);
  let source = Source::new(&gpu, &rgba, width, height).expect("source");
  assert_eq!(
    draw(&gpu, &source, (width, height)),
    premultiplied_bgra(&rgba)
  );
  let doubled = draw(&gpu, &source, (width * 2, height * 2));
  let expected = premultiplied_bgra(&rgba);
  for y in 0..height * 2 {
    for x in 0..width * 2 {
      let at = ((y * width * 2 + x) * 4) as usize;
      let from = (((y / 2) * width + x / 2) * 4) as usize;
      assert_eq!(doubled[at..at + 4], expected[from..from + 4], "({x},{y})");
    }
  }
}

#[test]
fn shrinking_averages_fine_detail_instead_of_dropping_it() {
  let gpu = Gpu::warp().expect("WARP");
  // One-pixel black and white stripes: point sampling would keep only one color.
  let (width, height) = (256u32, 64u32);
  let rgba: Vec<u8> = (0..width * height)
    .flat_map(|index| {
      let value = if index % 2 == 0 { 0 } else { 255 };
      [value, value, value, 255]
    })
    .collect();
  let source = Source::new(&gpu, &rgba, width, height).expect("source");
  for size in [(128, 32), (64, 16)] {
    let shrunk = draw(&gpu, &source, size);
    // Away from the edges every pixel is close to mid-gray.
    for y in 2..size.1 - 2 {
      for x in 2..size.0 - 2 {
        let value = shrunk[((y * size.0 + x) * 4) as usize];
        assert!((100..=156).contains(&value), "{size:?} ({x},{y}) = {value}");
      }
    }
  }
}

#[test]
fn a_tiled_image_draws_like_a_whole_one_with_no_seams() {
  let gpu = Gpu::warp().expect("WARP");
  let (width, height) = (200, 150);
  let rgba = pattern(width, height);
  let whole = Source::tiled(&gpu, &rgba, width, height, 4096).expect("whole");
  // 40-pixel bitmaps, each drawing 24 pixels: 9 × 7 of them.
  let tiled = Source::tiled(&gpu, &rgba, width, height, 40).expect("tiled");
  assert_eq!(whole.tile_count(), 1);
  assert_eq!(tiled.tile_count(), 9 * 7);
  for size in [(200, 150), (300, 225)] {
    assert_eq!(
      draw(&gpu, &whole, size),
      draw(&gpu, &tiled, size),
      "{size:?}"
    );
  }
  // Shrunk, each tile resamples on its own: on a screenshot-like smooth image the result matches
  // and no seam is left uncovered.
  let smooth: Vec<u8> = (0..width * height)
    .flat_map(|index| {
      let (x, y) = (index % width, index / width);
      [
        (x * 255 / width) as u8,
        (y * 255 / height) as u8,
        200 - (x / 2) as u8,
        255,
      ]
    })
    .collect();
  let whole = Source::tiled(&gpu, &smooth, width, height, 4096).expect("whole");
  let tiled = Source::tiled(&gpu, &smooth, width, height, 40).expect("tiled");
  for size in [(100, 75), (67, 50), (151, 113)] {
    let (a, b) = (draw(&gpu, &whole, size), draw(&gpu, &tiled, size));
    assert!(
      b.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255),
      "{size:?}: a seam shows through"
    );
    let worst = a
      .iter()
      .zip(&b)
      .map(|(x, y)| x.abs_diff(*y))
      .max()
      .unwrap_or(0);
    assert!(worst <= 4, "{size:?}: tiles differ by up to {worst}");
  }
  assert!(
    Source::new(&gpu, &rgba[..100], width, height).is_err(),
    "short pixels"
  );
}

fn hidden_window() -> HWND {
  use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, WS_EX_LAYERED, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_POPUP,
  };
  // SAFETY: a never-shown window of this test thread, destroyed by the test.
  unsafe {
    CreateWindowExW(
      WS_EX_NOREDIRECTIONBITMAP | WS_EX_LAYERED | WS_EX_TOOLWINDOW,
      windows::core::w!("STATIC"),
      None,
      WS_POPUP,
      0,
      0,
      100,
      80,
      None,
      None,
      None,
      None,
    )
  }
  .expect("window")
}

#[test]
fn pin_visuals_build_lay_out_and_survive_a_lost_device() {
  let hwnd = hidden_window();
  let (width, height) = (64, 40);
  let rgba = pattern(width, height);
  let layout = Layout {
    image: (96, 60),
    offset: (0, 0),
    opacity: 0.5,
    presence: (0.7, 0.96),
    label: 1.0,
  };
  let gpu = Gpu::warp().expect("WARP");
  let mut visuals =
    Visuals::new(&gpu, hwnd, &rgba, (width, height), 12, 8, (width, height)).expect("visuals");
  assert_eq!(visuals.rendered(), (width, height));
  visuals
    .set_label(&gpu, &[255; 20 * 10 * 4], 20, 10)
    .expect("label");
  visuals.layout(layout).expect("layout");
  gpu.commit().expect("commit");
  visuals.render((32, 20)).expect("downscaled");
  assert_eq!(visuals.rendered(), (32, 20));
  assert!(!gpu.lost());

  // The device goes away: everything is rebuilt on a new one, for the same window.
  gpu.simulate_loss();
  assert!(gpu.lost());
  drop(visuals);
  drop(gpu);
  let gpu = Gpu::warp().expect("new device");
  let visuals = Visuals::new(&gpu, hwnd, &rgba, (width, height), 12, 8, (32, 20)).expect("rebuilt");
  visuals.layout(layout).expect("layout");
  gpu.commit().expect("commit");
  assert!(!gpu.lost());
  drop(visuals);
  // SAFETY: the window created above.
  let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
}

/// The GPU pin's costs on a 4K screenshot: the host thread's work per frame (the compositor shows it
/// at its next refresh), and a redraw until the compositor took it:
/// `--ignored --nocapture`. Compare with `cpu_pin_timing`.
#[test]
#[ignore = "timing"]
fn gpu_pin_timing() {
  use windows::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos};
  let (width, height) = (3840u32, 2160u32);
  let rgba: Vec<u8> = (0..width * height)
    .flat_map(|index| [(index % 251) as u8, (index % 199) as u8, 90, 255])
    .collect();
  for (name, gpu) in [("GPU", Gpu::new()), ("WARP", Gpu::warp())] {
    let gpu = gpu.expect("device");
    let hwnd = hidden_window();
    let start = std::time::Instant::now();
    let mut visuals =
      Visuals::new(&gpu, hwnd, &rgba, (width, height), 12, 8, (width, height)).expect("visuals");
    gpu.commit().expect("commit");
    // SAFETY: this thread's composition device.
    unsafe { gpu.dcomp.WaitForCommitCompletion() }.expect("wait");
    let open = start.elapsed().as_secs_f64() * 1000.0;
    let frame = |visuals: &Visuals, image: (i32, i32), opacity: f32, at: i32| {
      let start = std::time::Instant::now();
      visuals_layout(visuals, image, opacity);
      // SAFETY: this test's own hidden window.
      unsafe {
        SetWindowPos(
          hwnd,
          None,
          at,
          at,
          image.0 + 24,
          image.1 + 24,
          SWP_NOZORDER | SWP_NOACTIVATE,
        )
      }
      .expect("place");
      gpu.commit().expect("commit");
      let work = start.elapsed().as_secs_f64() * 1000.0;
      // SAFETY: this thread's composition device.
      unsafe { gpu.dcomp.WaitForCommitCompletion() }.expect("wait");
      work
    };
    let mut best = [f64::MAX; 4];
    for round in 0..5 {
      best[0] = best[0].min(frame(&visuals, (4800, 2700), 1.0, round));
      best[1] = best[1].min(frame(&visuals, (3840, 2160), 0.8 - round as f32 * 0.01, 0));
      best[2] = best[2].min(frame(&visuals, (3840, 2160), 1.0, round + 10));
      let start = std::time::Instant::now();
      visuals.render((3456 - round as u32, 1944)).expect("render");
      gpu.commit().expect("commit");
      // SAFETY: this thread's composition device.
      unsafe { gpu.dcomp.WaitForCommitCompletion() }.expect("wait");
      best[3] = best[3].min(start.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
      "4K {name} pin, best of 5: open {open:.1} ms, zoom frame {:.2} ms, opacity frame {:.2} ms, \
       drag step {:.2} ms, sharp redraw at 90% {:.1} ms",
      best[0], best[1], best[2], best[3]
    );
    drop(visuals);
    // SAFETY: the window created above.
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
  }
}

fn visuals_layout(visuals: &Visuals, image: (i32, i32), opacity: f32) {
  visuals
    .layout(Layout {
      image,
      offset: (0, 0),
      opacity,
      presence: (1.0, 1.0),
      label: 0.0,
    })
    .expect("layout");
}
