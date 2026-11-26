use super::*;
use crate::compositing::backend::MockCaptureBackend;

fn monitor(w: u32, h: u32) -> MonitorInfo {
  MonitorInfo {
    id: "synthetic".into(),
    name: "Synthetic".into(),
    x: 0,
    y: 0,
    width: w,
    height: h,
    is_primary: true,
    dpi_scale: 1.,
    logical_x: 0,
    logical_y: 0,
    logical_width: w,
    logical_height: h,
  }
}
fn geometry(w: u32, h: u32) -> SliceGeometry {
  SliceGeometry {
    monitor: monitor(w, h),
    src_rect: CropRect {
      x: 0,
      y: 0,
      width: w,
      height: h,
    },
    dst_offset: (0, 0),
    scale_factor: None,
    monitor_width: w,
    monitor_height: h,
  }
}

#[test]
fn cropped_blit_and_failure_fill_cannot_spill_into_the_next_output_row() {
  let mut geo = geometry(3, 2);
  geo.dst_offset = (3, 0);
  let slice = MonitorSlice::new(
    geo,
    Box::new(MockCaptureBackend::new_solid(3, 2, [1, 2, 3, 255])),
    true,
  );
  let mut compositor = MultiMonitorCompositor::new(vec![slice], 4, 2);
  compositor.output.fill(77);
  compositor.capture_frame().unwrap();
  assert_eq!(&compositor.output()[16..28], &[77; 12]);
  compositor.output.fill(77);
  compositor.fill_black_region(0);
  assert_eq!(&compositor.output()[16..28], &[77; 12]);
  assert_eq!(&compositor.output()[12..16], &[0; 4]);
  assert_eq!(&compositor.output()[28..32], &[0; 4]);
}

#[test]
fn scaled_blits_match_pixel_values_with_padding_and_fractional_interpolation() {
  let mut source = vec![0u8; 24];
  source[..8].copy_from_slice(&[0, 0, 0, 255, 100, 100, 100, 255]);
  source[12..20].copy_from_slice(&[200, 200, 200, 255, 240, 240, 240, 255]);
  let mut output = vec![99; 4 * 4 * 4];
  blit_slice(
    &mut output,
    4,
    4,
    &source,
    2,
    2,
    12,
    0,
    0,
    2,
    2,
    0,
    0,
    Some(2.),
  );
  assert_eq!(&output[..8], &[0, 0, 0, 255, 0, 0, 0, 255]);
  assert_eq!(&output[8..16], &[100, 100, 100, 255, 100, 100, 100, 255]);
  assert_eq!(&output[48..52], &[200, 200, 200, 255]);
  let mut output = vec![0; 3 * 3 * 4];
  blit_slice(
    &mut output,
    3,
    3,
    &source,
    2,
    2,
    12,
    0,
    0,
    2,
    2,
    0,
    0,
    Some(1.5),
  );
  assert_eq!(&output[..4], &[0, 0, 0, 255]);
  assert_eq!(&output[4..8], &[67, 67, 67, 255]);
  assert_eq!(&output[16..20], &[173, 173, 173, 255]);
  assert_eq!(&output[32..36], &[240, 240, 240, 255]);
}

struct Scripted {
  frame: Vec<u8>,
  step: usize,
  calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl CaptureBackend for Scripted {
  fn acquire_frame(&mut self) -> Result<FrameAcquireResult<'_>> {
    self
      .calls
      .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    self.step += 1;
    match self.step {
      1 | 3 => Ok(FrameAcquireResult::NewFrame {
        data: &self.frame,
        width: 2,
        height: 2,
        row_pitch: 8,
      }),
      4 => Ok(FrameAcquireResult::NoChange),
      _ => bail!("synthetic disconnected monitor"),
    }
  }
  fn release(&mut self) {}
}

#[test]
fn monitor_resize_recovery_and_terminal_failure_are_bounded_per_slice() {
  let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let backend = Scripted {
    frame: [12, 34, 56, 255].repeat(4),
    step: 0,
    calls: calls.clone(),
  };
  let mut compositor = MultiMonitorCompositor::new(
    vec![MonitorSlice::new(geometry(4, 4), Box::new(backend), true)],
    4,
    4,
  );
  let result = compositor.capture_frame().unwrap();
  assert!(result.changed && !result.had_error);
  assert_eq!(&result.data[..8], &[12, 34, 56, 255, 12, 34, 56, 255]);
  assert_eq!(compositor.slices[0].geometry.monitor_width, 2);
  assert_eq!(compositor.slices[0].geometry.src_rect.width, 2);
  let result = compositor.capture_frame().unwrap();
  assert!(result.changed && result.had_error);
  let result = compositor.capture_frame().unwrap();
  assert!(result.changed && !result.had_error);
  assert_eq!(compositor.slices[0].consecutive_errors, 0);
  let result = compositor.capture_frame().unwrap();
  assert!(!result.changed && !result.had_error);
  for _ in 0..60 {
    compositor.capture_frame().unwrap();
  }
  assert!(compositor.slices[0].hold_last_frame);
  let count = calls.load(std::sync::atomic::Ordering::Relaxed);
  let frozen = compositor.output().to_vec();
  for _ in 0..10 {
    let result = compositor.capture_frame().unwrap();
    assert!(!result.changed && !result.had_error);
    assert_eq!(result.data, frozen);
  }
  assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), count);
  compositor.fill_black_region(usize::MAX);
  assert_eq!(compositor.output(), frozen);
}
