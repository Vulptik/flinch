use super::*;

fn plane(data: &[u8], width: usize, height: usize) -> Plane<'_> {
  Plane::new(data, width, width, height).expect("plane")
}

#[test]
fn cadence_accepts_only_supported_rates_within_frame_time_rounding() {
  assert_eq!(cadence(333_333).expect("30"), 30);
  assert_eq!(cadence(333_334).expect("30 rounded up"), 30);
  assert_eq!(cadence(166_667).expect("60"), 60);
  assert_eq!(cadence(83_333).expect("120"), 120);
  assert_eq!(cadence(666_667).expect("15"), 15);
  let error = cadence(400_000).expect_err("25 FPS");
  assert_eq!(error.kind, ErrorKind::Capability);
  assert!(
    cadence(333_340).is_err(),
    "a longer first frame is not guessed"
  );
}

#[test]
fn keyframes_fall_on_the_recorders_slot_boundaries() {
  for fps in SUPPORTED_FPS {
    let gop = u64::from(keyframe_interval(fps));
    let mut group = None;
    let keys: Vec<u64> = (0..gop * 2 + gop / 2)
      .filter(|index| {
        keyframe(
          time::frame_time(*index, fps).expect("time"),
          fps,
          &mut group,
        )
        .expect("key")
      })
      .collect();
    assert_eq!(keys, vec![0, gop, gop * 2], "{fps} FPS");
  }
  // A lost slot that skips a boundary still starts the next group.
  let gop = u64::from(keyframe_interval(30));
  let mut group = None;
  assert!(keyframe(0, 30, &mut group).expect("first"));
  assert!(!keyframe(time::frame_time(gop - 2, 30).expect("t"), 30, &mut group).expect("same"));
  assert!(keyframe(time::frame_time(gop + 1, 30).expect("t"), 30, &mut group).expect("next"));
}

#[test]
fn psnr_is_100_for_identical_pictures_and_follows_the_squared_error() {
  let a = vec![100u8; 16 * 16];
  assert_eq!(
    psnr(&plane(&a, 16, 16), &plane(&a, 16, 16)).expect("same"),
    100.0
  );
  let b = vec![102u8; 16 * 16];
  let expected = 10.0 * (255.0f64 * 255.0 / 4.0).log10();
  let measured = psnr(&plane(&a, 16, 16), &plane(&b, 16, 16)).expect("offset");
  assert!((measured - expected).abs() < 1e-9);
}

#[test]
fn ssim_is_one_for_identical_pictures_and_drops_with_lost_detail() {
  // A sharp checkerboard against a flat grey of the same mean: detail gone, brightness kept.
  let checker: Vec<u8> = (0..32 * 32)
    .map(|i| if (i % 32 + i / 32) % 2 == 0 { 16 } else { 235 })
    .collect();
  let flat = vec![125u8; 32 * 32];
  let same = ssim(&plane(&checker, 32, 32), &plane(&checker, 32, 32)).expect("same");
  assert!((same - 1.0).abs() < 1e-12);
  let lost = ssim(&plane(&checker, 32, 32), &plane(&flat, 32, 32)).expect("blurred");
  assert!(lost < 0.05, "{lost}");
  // Edge remainders beyond whole blocks are left out.
  let wide: Vec<u8> = (0..36 * 32)
    .map(|i| {
      if i % 36 >= 32 {
        0
      } else {
        checker[(i / 36) * 32 + i % 36]
      }
    })
    .collect();
  let mut other = wide.clone();
  for y in 0..32 {
    for x in 32..36 {
      other[y * 36 + x] = 255;
    }
  }
  let ignored = ssim(&plane(&wide, 36, 32), &plane(&other, 36, 32)).expect("edges");
  assert!((ignored - 1.0).abs() < 1e-12);
}

#[test]
fn planes_and_comparisons_refuse_short_mismatched_or_tiny_pictures() {
  let data = vec![0u8; 64];
  assert!(Plane::new(&data, 8, 8, 9).is_err(), "too short");
  assert!(Plane::new(&data, 4, 8, 2).is_err(), "width beyond stride");
  assert!(Plane::new(&data, 8, 0, 1).is_err(), "empty");
  let strided = vec![7u8; 10 * 7 + 8];
  let view = Plane::new(&strided, 10, 8, 8).expect("last row needs only its width");
  assert_eq!(view.row(7).len(), 8);
  let small = vec![0u8; 16];
  assert!(psnr(&plane(&data, 8, 8), &plane(&small, 4, 4)).is_err());
  assert!(ssim(&plane(&data, 8, 8), &plane(&small, 4, 4)).is_err());
  assert!(ssim(&plane(&small, 4, 4), &plane(&small, 4, 4)).is_err());
}

#[test]
fn scores_report_mean_fifth_percentile_and_the_time_of_the_worst_frame() {
  let mut scores = Scores::default();
  for index in 0..100 {
    let ssim = if index == 42 {
      0.5
    } else {
      0.9 + f64::from(index) / 1000.0
    };
    scores.add(f64::from(index) / 10.0, 40.0 + f64::from(index % 2), ssim);
  }
  let result = scores.finish().expect("scores");
  assert!((result.mean_psnr_y_db - 40.5).abs() < 1e-9);
  assert_eq!(result.min_psnr_y_db, 40.0);
  assert_eq!(result.min_ssim_y, 0.5);
  assert!((result.min_ssim_at_seconds - 4.2).abs() < 1e-9);
  // Sorted, index 4 of 100 is the fifth-lowest frame: 0.5 then 0.900..0.903.
  assert!((result.p5_ssim_y - 0.903).abs() < 1e-9);
  assert!(Scores::default().finish().is_err());
}

#[test]
fn bgra_swaps_channels_and_adds_opaque_alpha() {
  let mut out = vec![9u8; 3];
  bgra(&[1, 2, 3, 4, 5, 6], &mut out);
  assert_eq!(out, [3, 2, 1, 255, 6, 5, 4, 255]);
}

fn fixture(path: &Path, seconds: u32) {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let media = super::super::fixtures::media_sized(seconds, 512, 64);
  let writer =
    mux::Writer::start(path, 512, 64, 60, media.video_extra.clone(), None).expect("fixture writer");
  let deadline = Instant::now() + Duration::from_secs(30);
  for packet in media.video {
    // The writer bounds its backlog to two seconds of media; wait for it to drain.
    let mut pushed = writer.push(packet.clone());
    while pushed
      .as_ref()
      .is_err_and(|error| error.kind == ErrorKind::Saturation)
      && Instant::now() < deadline
    {
      std::thread::sleep(Duration::from_millis(2));
      pushed = writer.push(packet.clone());
    }
    pushed.expect("fixture packet");
  }
  writer.finish().expect("fixture finished");
}

#[test]
fn requests_are_refused_before_any_device_or_file_is_touched() {
  let root = tempfile::tempdir().expect("temp");
  let input = root.path().join("absent.mp4");
  let output = root.path().join("out.mp4");
  let refuse = |encoder, quality, adapter| {
    reencode(&input, &output, encoder, quality, adapter).expect_err("refused")
  };
  let cqp = Quality::Cqp { i: 23, p: 23 };
  for encoder in [EncoderKind::Auto, EncoderKind::MediaFoundation] {
    assert_eq!(
      refuse(encoder, cqp, AdapterChoice::Display).stage,
      "reencode encoder"
    );
  }
  assert_eq!(
    refuse(EncoderKind::Amf, cqp, AdapterChoice::Warp).stage,
    "reencode adapter"
  );
  let invalid = refuse(
    EncoderKind::Software,
    Quality::Cqp { i: 52, p: 52 },
    AdapterChoice::Warp,
  );
  assert_eq!(invalid.kind, ErrorKind::Protocol);
  std::fs::write(&output, b"keep").expect("existing destination");
  let existing = refuse(EncoderKind::Software, cqp, AdapterChoice::Warp);
  assert_eq!(existing.stage, "reencode output");
  assert_eq!(std::fs::read(&output).expect("kept"), b"keep");
  assert!(!input.exists());
}

#[test]
fn software_reencodes_on_warp_score_finer_qps_higher_and_cost_more_bytes() {
  let root = tempfile::tempdir().expect("temp");
  let input = root.path().join("reference.mp4");
  let seconds = 3;
  fixture(&input, seconds);
  let run = |qp: u8| {
    let output = root.path().join(format!("qp{qp}.mp4"));
    let report = reencode(
      &input,
      &output,
      EncoderKind::Software,
      Quality::Cqp { i: qp, p: qp },
      AdapterChoice::Warp,
    )
    .expect("re-encode");
    assert!(output.exists());
    report
  };
  let fine = run(12);
  let coarse = run(44);
  for report in [&fine, &coarse] {
    assert_eq!((report.width, report.height, report.fps), (512, 64, 60));
    assert_eq!(report.frames, u64::from(seconds * 60));
    assert_eq!(report.conversion, ConversionKind::Cpu);
    assert_eq!(report.adapter, "WARP");
    assert!((report.duration_seconds - f64::from(seconds)).abs() < 0.001);
    // One keyframe per started interval, as a recording forces them.
    assert_eq!(
      report.keyframes,
      u64::from((seconds * 60).div_ceil(keyframe_interval(60)))
    );
    assert!(report.keyframe_bytes > 0 && report.keyframe_bytes < report.video_bytes);
    assert!(report.bytes > report.video_bytes);
    assert!(report.average_mbps > 0.0 && report.megabytes_per_minute > 0.0);
    assert!(report.pipeline_fps > 0.0);
    let s = &report.similarity;
    assert!(s.min_ssim_y <= s.p5_ssim_y && s.p5_ssim_y <= s.mean_ssim_y && s.mean_ssim_y <= 1.0);
    assert!(s.min_psnr_y_db <= s.mean_psnr_y_db);
    assert!((0.0..f64::from(seconds)).contains(&s.min_ssim_at_seconds));
  }
  // Intra pictures show the QP cost directly. Total bytes need not: on this nearly flat clip,
  // coarse predicted frames keep correcting their blurred reference and outgrow fine ones.
  assert!(
    fine.keyframe_bytes > coarse.keyframe_bytes,
    "{} vs {}",
    fine.keyframe_bytes,
    coarse.keyframe_bytes
  );
  assert!(fine.similarity.mean_ssim_y > coarse.similarity.mean_ssim_y);
  assert!(fine.similarity.mean_psnr_y_db > coarse.similarity.mean_psnr_y_db);
  assert!(
    fine.similarity.mean_psnr_y_db > 35.0,
    "{:?}",
    fine.similarity
  );
  // Re-running onto an existing result is refused and leaves it intact.
  let again = reencode(
    &input,
    &fine.output,
    EncoderKind::Software,
    Quality::Cqp { i: 12, p: 12 },
    AdapterChoice::Warp,
  )
  .expect_err("existing");
  assert_eq!(again.stage, "reencode output");
  let leftovers = std::fs::read_dir(root.path())
    .expect("listing")
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.file_name().to_string_lossy().contains("partial"))
    .count();
  assert_eq!(leftovers, 0, "no partial file is left behind");
}

#[test]
fn a_damaged_reference_is_refused_and_leaves_no_output() {
  let root = tempfile::tempdir().expect("temp");
  let input = root.path().join("reference.mp4");
  fixture(&input, 1);
  let bytes = std::fs::read(&input).expect("reference");
  let cut = root.path().join("cut.mp4");
  std::fs::write(&cut, &bytes[..bytes.len() * 2 / 3]).expect("truncated copy");
  let output = root.path().join("out.mp4");
  let error = reencode(
    &cut,
    &output,
    EncoderKind::Software,
    Quality::Cqp { i: 23, p: 23 },
    AdapterChoice::Warp,
  )
  .expect_err("truncated");
  assert_eq!(error.stage, "reencode input");
  assert!(!output.exists());
}

/// Regression for the coverage gate: the full-queue retry only ran when a slow machine's encoder
/// fell behind, so the measured lines differed per machine. Refusals are scripted here.
#[test]
fn a_refused_submission_feeds_the_writer_and_retries_until_its_deadline() {
  // (attempts, feeds): refused twice, then accepted.
  let mut state = (0u32, 0u32);
  let far = Instant::now() + Duration::from_secs(5);
  retry_until(
    &mut state,
    far,
    "submitting",
    |(attempts, _)| {
      *attempts += 1;
      Ok(*attempts > 2)
    },
    |(_, feeds)| {
      *feeds += 1;
      Ok(())
    },
  )
  .expect("accepted on the third attempt");
  assert_eq!(state, (3, 2));
  // A feed failure stops the wait with its own error.
  let error = retry_until(
    &mut (),
    far,
    "submitting",
    |_| Ok(false),
    |_| Err(EngineError::protocol("scripted writer failure")),
  )
  .expect_err("writer failure");
  assert_eq!(error.reason, "scripted writer failure");
  // A queue that never frees fails at the deadline instead of waiting forever.
  let error = retry_until(
    &mut (),
    Instant::now() - Duration::from_millis(1),
    "submitting",
    |_| Ok(false),
    |_| Ok(()),
  )
  .expect_err("deadline");
  assert_eq!(
    (error.kind, error.stage.as_str()),
    (ErrorKind::Timeout, "reencode")
  );
  assert!(error.reason.ends_with("while submitting"));
}
