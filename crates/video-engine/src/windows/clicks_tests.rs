#![allow(clippy::expect_used)] // Test fixtures only; production remains deny.
use super::super::rotate::fixtures::{pixels, warp_gpu};
use super::*;
use windows::core::Interface;

const GRAY: [u8; 4] = [90, 90, 90, 255];

fn filled(gpu: &Gpu, texture: &ID3D11Texture2D, color: [u8; 4]) {
  let data: Vec<u8> = (0..gpu.source_width * gpu.source_height)
    .flat_map(|_| color)
    .collect();
  // SAFETY: the data spans the whole BGRA cache at its packed pitch.
  unsafe {
    gpu.context.UpdateSubresource(
      texture,
      0,
      None,
      data.as_ptr().cast(),
      gpu.source_width * 4,
      0,
    );
  }
}

fn ring(x: f32, y: f32, radius: f32, thickness: f32, alpha: f32) -> Mark {
  Mark {
    x,
    y,
    radius,
    thickness,
    color: [1.0, 0.0, 0.0],
    alpha,
  }
}

fn painter(gpu: &Gpu) -> ClickPainter {
  ClickPainter::new(gpu, &compile().expect("click shaders")).expect("painter")
}

#[test]
fn rings_and_dots_blend_over_the_frame_and_leave_the_rest_alone() {
  let _com = super::super::Apartment::new().expect("COM");
  let (width, height) = (64u32, 48u32);
  let gpu = warp_gpu(width, height);
  filled(&gpu, &gpu.desktop, GRAY);
  let painter = painter(&gpu);
  let marks = [
    ring(20.0, 20.0, 10.0, 3.0, 1.0),
    Mark {
      thickness: 0.0,
      radius: 4.0,
      color: [0.0, 0.0, 1.0],
      alpha: 0.5,
      ..ring(50.0, 30.0, 0.0, 0.0, 0.0)
    },
  ];
  let painted = painter
    .paint(&gpu, &gpu.desktop, &marks)
    .expect("paint from the desktop cache");
  assert_eq!(painted.as_raw(), gpu.scratch.as_raw());
  let image = pixels(&gpu, painted, width, height);
  let at = |x: u32, y: u32| image[(y * width + x) as usize];
  // On the ring (10 px right of its center): opaque red, BGRA.
  assert_eq!(at(30, 20), [0, 0, 255, 255]);
  // Inside and outside the ring, and far away: the frame as it was.
  assert_eq!(at(20, 20), GRAY);
  assert_eq!(at(20, 36), GRAY);
  assert_eq!(at(2, 46), GRAY);
  // The dot's center: half blue over gray; alpha stays opaque.
  let [b, g, r, a] = at(50, 30);
  assert!((170..=175).contains(&b), "{b}");
  assert!((43..=47).contains(&g) && (43..=47).contains(&r), "{g} {r}");
  assert_eq!(a, 255);
  // The ring's edge is antialiased, between the frame and full red.
  let edge = at(28, 28)[2];
  assert!(edge > 90 && edge < 255, "{edge}");
  // The desktop cache itself is never drawn on: the next frame may reuse it.
  assert_eq!(
    pixels(&gpu, &gpu.desktop, width, height)[20 * 64 + 30],
    GRAY
  );
}

#[test]
fn marks_draw_over_a_composed_pointer_and_clip_at_the_display_edge() {
  let _com = super::super::Apartment::new().expect("COM");
  let (width, height) = (32u32, 32u32);
  let gpu = warp_gpu(width, height);
  filled(&gpu, &gpu.desktop, GRAY);
  // Scratch already holds the frame with the pointer: it is drawn on in place, not replaced.
  let composed = [10, 200, 10, 255];
  filled(&gpu, &gpu.scratch, composed);
  let painter = painter(&gpu);
  let painted = painter
    .paint(&gpu, &gpu.scratch, &[ring(0.0, 0.0, 6.0, 2.0, 1.0)])
    .expect("paint over the pointer");
  let image = pixels(&gpu, painted, width, height);
  assert_eq!(image[6], [0, 0, 255, 255], "the quarter on the display");
  assert_eq!(image[20 * 32 + 20], composed);
  // Nothing to draw: the source comes back untouched, with no copy.
  filled(&gpu, &gpu.scratch, GRAY);
  let same = painter
    .paint(&gpu, &gpu.desktop, &[])
    .expect("nothing to draw");
  assert_eq!(same.as_raw(), gpu.desktop.as_raw());
  // Only the session's own caches are accepted.
  let foreign = Gpu::texture(
    &gpu.device,
    width,
    height,
    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
    D3D11_BIND_SHADER_RESOURCE.0 as u32,
  )
  .expect("foreign texture");
  assert!(
    painter
      .paint(&gpu, &foreign, &[ring(1.0, 1.0, 2.0, 1.0, 1.0)])
      .is_err()
  );
}

#[test]
fn constants_center_marks_on_pixels_and_refuse_an_overflow() {
  let mark = ring(3.0, 4.0, 5.0, 2.0, 0.5);
  let values = constants(&[mark], 640, 360).expect("one mark");
  assert_eq!(&values[..2], &[640.0, 360.0]);
  assert_eq!(values[2].to_bits(), 1);
  assert_eq!(&values[4..12], &[3.5, 4.5, 5.0, 2.0, 1.0, 0.0, 0.0, 0.5]);
  assert!(constants(&[mark; MAX_MARKS], 1, 1).is_ok());
  assert!(constants(&[mark; MAX_MARKS + 1], 1, 1).is_err());
}

#[test]
fn the_layer_redraws_while_marks_show_and_once_after_then_stops() {
  let _com = super::super::Apartment::new().expect("COM");
  let gpu = warp_gpu(64, 64);
  filled(&gpu, &gpu.desktop, GRAY);
  // The picture starts at (100, 200) on the virtual screen; the pointer is at its (10, 12).
  fn up() -> Result<Sample> {
    Ok(Sample {
      pointer: (110, 212),
      ..Sample::default()
    })
  }
  fn clicked() -> Result<Sample> {
    Ok(Sample {
      pressed: [true, false],
      pointer: (110, 212),
      ..Sample::default()
    })
  }
  fn denied() -> Result<Sample> {
    Err(EngineError::protocol("secure desktop"))
  }
  let mut layer = ClickLayer::with_reader(painter(&gpu), 1.0, (100, 200), up);
  assert!(!layer.tick(0), "idle: the frame is reused");
  layer.read = clicked;
  assert!(layer.tick(1_000));
  layer.read = up;
  assert!(layer.tick(2_000));
  let painted = layer.paint(&gpu, &gpu.desktop, 2_000).expect("ring");
  assert_eq!(painted.as_raw(), gpu.scratch.as_raw());
  let end = 1_000 + crate::clicks::RING_100NS;
  // The ring has ended, but the last frame showed it: convert once more, then reuse frames.
  assert!(layer.tick(end));
  let clean = layer.paint(&gpu, &gpu.desktop, end).expect("no marks");
  assert_eq!(clean.as_raw(), gpu.desktop.as_raw());
  assert!(!layer.tick(end + 1));
  // A press while paused never draws, nor does a failed read.
  layer.read = clicked;
  layer.pause();
  assert!(
    !layer.tick(end + 2),
    "the first read after a pause sets the baseline"
  );
  layer.read = denied;
  assert!(!layer.tick(end + 3));
  assert!(layer.failing);
  layer.read = up;
  assert!(!layer.tick(end + 4));
  assert!(!layer.failing);
  // The pointer is placed relative to the picture.
  layer.read = clicked;
  assert!(layer.tick(end + 5));
  let marks = layer.tracker.marks(end + 5);
  assert_eq!((marks[0].x, marks[0].y), (10.0, 12.0));
}

#[test]
fn reading_the_buttons_and_the_display_scale_never_fails_on_the_desktop() {
  let sample = sample().expect("pointer");
  let scale = scale_at(sample.pointer.0, sample.pointer.1).expect("scale");
  assert!((1.0..=5.0).contains(&scale), "{scale}");
}

/// What a frame with click marks costs on this machine's first GPU, offscreen: `--ignored`.
/// The full-frame copy from the desktop cache dominates; frames without marks cost nothing.
#[test]
#[ignore = "measures the local GPU"]
fn click_pass_timing() {
  use windows::Win32::Graphics::Dxgi::*;
  let _com = super::super::Apartment::new().expect("COM");
  // SAFETY: read-only factory and adapter enumeration; no output, capture or display.
  let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
  // SAFETY: as above.
  let adapter = unsafe { factory.EnumAdapters1(0) }.expect("first adapter");
  for (width, height) in [(1920u32, 1080u32), (2560, 1440), (3840, 2160)] {
    let gpu = Gpu::new(&adapter, width, height, None, 60).expect("GPU");
    filled(&gpu, &gpu.desktop, GRAY);
    let painter = painter(&gpu);
    let marks: Vec<Mark> = (0..9)
      .map(|index| ring(100.0 + 80.0 * index as f32, 200.0, 28.0, 3.0, 0.6))
      .collect();
    let sync = || {
      let _ = pixels(&gpu, &gpu.scratch, 1, 1);
    };
    sync();
    let rounds = 300;
    let started = std::time::Instant::now();
    for _ in 0..rounds {
      painter.paint(&gpu, &gpu.desktop, &marks).expect("paint");
      // SAFETY: submits recorded work; no CPU wait.
      unsafe { gpu.context.Flush() };
    }
    let submit = started.elapsed();
    sync();
    let total = started.elapsed();
    println!(
      "{width}x{height}: submit {:.3} ms, with GPU {:.3} ms per frame",
      submit.as_secs_f64() * 1000.0 / f64::from(rounds),
      total.as_secs_f64() * 1000.0 / f64::from(rounds)
    );
  }
}
