#![allow(clippy::expect_used)] // Test fixtures only; production remains deny.
use super::*;
use crate::gif::Dither;

fn recording(directory: &Path, seconds: u32) -> PathBuf {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let path = directory.join("intermediate.mp4");
  super::super::mux::write_fixture(
    &path,
    super::super::fixtures::media_sized(seconds, 512, 64),
    512,
    64,
  );
  path
}

fn leftovers(directory: &Path) -> Vec<String> {
  fs::read_dir(directory)
    .expect("directory")
    .map(|entry| {
      entry
        .expect("entry")
        .file_name()
        .to_string_lossy()
        .into_owned()
    })
    .filter(|name| name.ends_with(".partial"))
    .collect()
}

#[test]
fn a_recording_becomes_a_looping_gif_with_its_timing_size_and_picture() {
  let directory = tempfile::tempdir().expect("directory");
  let input = recording(directory.path(), 1);
  let output = directory.path().join("capture.gif");
  let report = encode(
    &input,
    &output,
    GifOptions {
      dither: Dither::Sierra24a,
    },
  )
  .expect("native GIF");
  assert_eq!((report.width, report.height), (512, 64));
  assert_eq!(report.decoded_frames, 60);
  // 60 FPS input: frames closer than the 2 cs minimum delay are coalesced, never shown late.
  assert_eq!(
    report.written_frames + report.unchanged_frames + report.coalesced_frames,
    60
  );
  assert!(report.coalesced_frames > 0);
  assert_eq!(report.duration_cs, 100);
  assert_eq!(report.bytes, fs::metadata(&output).expect("gif").len());
  assert!(leftovers(directory.path()).is_empty());

  let mut options = ::gif::DecodeOptions::new();
  options.set_color_output(::gif::ColorOutput::RGBA);
  let mut decoder = options
    .read_info(fs::File::open(&output).expect("open gif"))
    .expect("gif header");
  assert_eq!((decoder.width(), decoder.height()), (512, 64));
  let mut delays = 0u64;
  let mut frames = 0u64;
  let mut bright = 0u64;
  while let Some(frame) = decoder.read_next_frame().expect("frame") {
    delays += u64::from(frame.delay);
    assert!(
      u64::from(frame.delay) >= crate::gif::MIN_DELAY_CS,
      "browsers slow delays under 2 cs"
    );
    frames += 1;
    // The fixture alternates a 200/42 luma field; the first frame covers the whole picture.
    if frames == 1 {
      let centre = &frame.buffer[(32 * 512 + 256) * 4..(32 * 512 + 256) * 4 + 3];
      assert!(centre.iter().all(|c| !(50..=180).contains(c)), "{centre:?}");
    }
    if frame.width == 512 && frame.height >= 48 {
      bright += 1;
    }
  }
  assert_eq!(frames, report.written_frames);
  assert_eq!(delays, 100, "delays sum to the recording's length");
  assert!(
    bright > 1,
    "full-field changes are written as large rectangles"
  );
}

#[test]
fn every_dither_encodes_and_existing_or_broken_files_fail_without_leftovers() {
  let directory = tempfile::tempdir().expect("directory");
  let input = recording(directory.path(), 1);
  for (index, dither) in [
    Dither::None,
    Dither::Bayer { scale: 2 },
    Dither::FloydSteinberg,
  ]
  .into_iter()
  .enumerate()
  {
    let output = directory.path().join(format!("{index}.gif"));
    let report = encode(&input, &output, GifOptions { dither }).expect("dither");
    assert!(report.bytes > 0, "{dither:?}");
  }
  let existing = directory.path().join("0.gif");
  let before = fs::read(&existing).expect("existing");
  let error = encode(
    &input,
    &existing,
    GifOptions {
      dither: Dither::None,
    },
  )
  .expect_err("exists");
  assert_eq!(error.stage, "gif output");
  assert_eq!(fs::read(&existing).expect("kept"), before);

  let broken = directory.path().join("broken.mp4");
  let bytes = fs::read(&input).expect("input");
  fs::write(&broken, &bytes[..bytes.len() / 2]).expect("truncated copy");
  let output = directory.path().join("broken.gif");
  assert!(
    encode(
      &broken,
      &output,
      GifOptions {
        dither: Dither::None
      }
    )
    .is_err()
  );
  assert!(!output.exists());
  assert!(
    encode(
      &directory.path().join("absent.mp4"),
      &output,
      GifOptions {
        dither: Dither::None
      }
    )
    .is_err()
  );
  assert!(leftovers(directory.path()).is_empty());
}

#[test]
fn fidelity_follows_the_gif_timeline_and_detects_a_frozen_or_mismatched_gif() {
  let directory = tempfile::tempdir().expect("directory");
  let input = recording(directory.path(), 1);
  let output = directory.path().join("capture.gif");
  let report = encode(
    &input,
    &output,
    GifOptions {
      dither: Dither::Sierra24a,
    },
  )
  .expect("encode");
  let good = fidelity(&input, &output).expect("fidelity");
  assert_eq!(good.source_frames, 60);
  assert_eq!(good.gif_frames, report.written_frames);
  assert!(good.mean_psnr_db > 25.0, "{good:?}");

  // Negative control: the first picture held for the whole second.
  let palette = crate::gif::Palette::new(vec![[0, 0, 0], [255, 255, 255]]).expect("palette");
  let frozen = directory.path().join("frozen.gif");
  let mut writer = GifWriter::new(
    fs::File::create(&frozen).expect("frozen"),
    512,
    64,
    &palette,
  )
  .expect("writer");
  writer.push(&vec![0u8; 512 * 64], 0).expect("frame");
  writer.finish(10_000_000).expect("finish");
  let bad = fidelity(&input, &frozen).expect("frozen fidelity");
  assert!(
    bad.mean_psnr_db + 10.0 < good.mean_psnr_db,
    "{bad:?} vs {good:?}"
  );

  let small = directory.path().join("small.gif");
  let mut writer =
    GifWriter::new(fs::File::create(&small).expect("small"), 4, 4, &palette).expect("writer");
  writer.push(&[0u8; 16], 0).expect("frame");
  writer.finish(100_000).expect("finish");
  assert!(fidelity(&input, &small).is_err());
  assert!(fidelity(&input, &directory.path().join("absent.gif")).is_err());
}

#[test]
fn cancellation_in_either_pass_and_a_racing_destination_leave_no_partial_and_no_overwrite() {
  let directory = tempfile::tempdir().expect("directory");
  let input = recording(directory.path(), 1);
  let cancelled = || EngineError::new(ErrorKind::Cancelled, "test", "cancelled");
  // Frame 10 is in the palette pass; frame 70 is in the writing pass (60 frames per pass).
  for stop_at in [10, 70] {
    let output = directory.path().join(format!("cancel-{stop_at}.gif"));
    let mut frames = 0;
    let error = encode_with(
      &input,
      &output,
      GifOptions {
        dither: Dither::Sierra24a,
      },
      || {
        frames += 1;
        if frames == stop_at {
          return Err(cancelled());
        }
        Ok(())
      },
    )
    .expect_err("cancelled");
    assert_eq!(error.kind, ErrorKind::Cancelled, "{stop_at}");
    assert_eq!(frames, stop_at, "no frame decoded after cancellation");
    assert!(!output.exists());
    assert!(leftovers(directory.path()).is_empty(), "{stop_at}");
  }
  // Another writer creates the destination while encoding: it is kept, never replaced.
  let output = directory.path().join("raced.gif");
  let mut frames = 0;
  let error = encode_with(
    &input,
    &output,
    GifOptions {
      dither: Dither::None,
    },
    || {
      frames += 1;
      if frames == 100 {
        fs::write(&output, b"someone else's").expect("racing writer");
      }
      Ok(())
    },
  )
  .expect_err("destination appeared");
  assert_eq!(error.stage, "gif publish");
  assert_eq!(fs::read(&output).expect("kept"), b"someone else's");
  assert!(leftovers(directory.path()).is_empty());
}

#[test]
fn the_parallel_writing_pass_matches_the_one_thread_reference_byte_for_byte() {
  let directory = tempfile::tempdir().expect("directory");
  // Two seconds at 60 FPS: coalesced, unchanged and full-field frames all occur.
  let input = recording(directory.path(), 2);
  for (index, dither) in [
    Dither::None,
    Dither::Bayer { scale: 2 },
    Dither::FloydSteinberg,
    Dither::Sierra24a,
  ]
  .into_iter()
  .enumerate()
  {
    let reference = directory.path().join(format!("{index}-reference.gif"));
    let expected = encode_sequential(&input, &reference, GifOptions { dither }).expect("reference");
    let bytes = fs::read(&reference).expect("reference bytes");
    // One worker; a few; more than frames in flight; and the three-frame memory floor, where
    // every stage waits on the next (a deadlock would hang this test, not pass it).
    for (workers, memory) in [
      (1, pipeline::PIPELINE_BYTES),
      (3, pipeline::PIPELINE_BYTES),
      (16, pipeline::PIPELINE_BYTES),
      (4, 0),
    ] {
      let output = directory
        .path()
        .join(format!("{index}-{workers}-{memory}.gif"));
      let report = encode_tuned(
        &input,
        &output,
        GifOptions { dither },
        || Ok(()),
        (workers, memory),
      )
      .expect("parallel");
      assert!(
        fs::read(&output).expect("parallel bytes") == bytes,
        "{dither:?} on {workers} workers and {memory} bytes differs from the reference"
      );
      assert_eq!(GifReport { bytes: 0, ..report }, expected, "{dither:?}");
    }
  }
  assert!(leftovers(directory.path()).is_empty());
}

#[test]
fn cancelling_while_the_writing_pass_drains_leaves_no_partial() {
  let directory = tempfile::tempdir().expect("directory");
  let input = recording(directory.path(), 1);
  let output = directory.path().join("drain.gif");
  let mut calls = 0;
  // 60 checkpoints per decode pass; the next is the first check of the drain.
  let error = encode_with(
    &input,
    &output,
    GifOptions {
      dither: Dither::Sierra24a,
    },
    || {
      calls += 1;
      if calls == 121 {
        return Err(EngineError::new(ErrorKind::Cancelled, "test", "drain"));
      }
      Ok(())
    },
  )
  .expect_err("cancelled while draining");
  assert_eq!(
    (error.kind, error.reason.as_str()),
    (ErrorKind::Cancelled, "drain")
  );
  assert_eq!(calls, 121);
  assert!(!output.exists());
  assert!(leftovers(directory.path()).is_empty());
}

/// One scripted read: (changed, Option<(pts, duration)>, display size after).
type ScriptStep = (bool, Option<(i64, i64)>, (u32, u32));
/// A decoded-frame script.
struct Scripted {
  steps: std::collections::VecDeque<ScriptStep>,
  display: (u32, u32),
}
impl Source for Scripted {
  fn geometry(&mut self) -> Result<Geometry> {
    Ok(Geometry {
      width: 16,
      height: 16,
      stride: 16,
      display: self.display,
    })
  }
  fn next(&mut self) -> Result<Step> {
    let (changed, sample, display) = self
      .steps
      .pop_front()
      .ok_or_else(|| EngineError::protocol("script exhausted"))?;
    self.display = display;
    Ok(Step {
      finished: self.steps.is_empty(),
      changed,
      sample: sample.map(|(pts, duration)| (pts, duration, vec![16u8; 16 * 24])),
    })
  }
}
fn scripted(steps: &[ScriptStep]) -> Scripted {
  Scripted {
    steps: steps.iter().copied().collect(),
    display: (16, 16),
  }
}
fn run(source: &mut Scripted, limit: u64) -> (Result<u64>, Vec<(u64, u64)>) {
  let mut frames = Vec::new();
  let result = decode_from(
    source,
    Instant::now() + Duration::from_secs(10),
    limit,
    |frame| {
      frames.push((frame.pts, frame.first));
      Ok(())
    },
  );
  (result, frames)
}

#[test]
fn decoded_timing_and_size_are_checked_before_any_frame_reaches_the_encoder() {
  const S: (u32, u32) = (16, 16);
  // A good stream, including a renegotiated (padded) type with the same displayed size.
  let (end, frames) = run(
    &mut scripted(&[
      (false, Some((1000, 333_333)), S),
      (true, Some((334_333, 333_333)), S),
      (false, None, S),
    ]),
    10,
  );
  assert_eq!(end.expect("good stream"), 667_666);
  assert_eq!(
    frames,
    vec![(1000, 1000), (334_333, 1000)],
    "time counts from the first frame"
  );

  for (name, steps, expected) in [
    (
      "size change",
      vec![(false, Some((0, 10)), S), (true, Some((10, 10)), (32, 16))],
      "size changed",
    ),
    (
      "repeated time",
      vec![(false, Some((0, 10)), S), (false, Some((0, 10)), S)],
      "not increasing",
    ),
    (
      "backwards",
      vec![(false, Some((20, 10)), S), (false, Some((10, 10)), S)],
      "not increasing",
    ),
    (
      "negative time",
      vec![(false, Some((-1, 10)), S)],
      "not increasing",
    ),
    (
      "zero duration",
      vec![(false, Some((0, 0)), S)],
      "not increasing",
    ),
  ] {
    let (result, frames) = run(&mut scripted(&steps), 10);
    let error = result.expect_err(name);
    assert!(error.to_string().contains(expected), "{name}: {error}");
    // The failing frame is never handed on.
    assert_eq!(frames.len(), steps.len() - 1, "{name}");
  }
}

/// A decoder whose own clock runs backwards, dated by container position like `MediaSource`.
struct ReversedDecoder {
  decoder_times: std::collections::VecDeque<i64>,
  clock: ContainerClock,
}
impl Source for ReversedDecoder {
  fn geometry(&mut self) -> Result<Geometry> {
    Ok(Geometry {
      width: 16,
      height: 16,
      stride: 16,
      display: (16, 16),
    })
  }
  fn next(&mut self) -> Result<Step> {
    let decoded = self.decoder_times.pop_front();
    let finished = self.decoder_times.is_empty();
    let sample = match decoded {
      Some(_ignored_decoder_time) => {
        let (pts, duration) = self.clock.date()?;
        Some((pts, duration, vec![16u8; 16 * 24]))
      },
      None => None,
    };
    if finished {
      self.clock.finish()?;
    }
    Ok(Step {
      finished,
      changed: false,
      sample,
    })
  }
}
#[test]
fn decoded_frames_take_container_times_when_the_decoder_clock_reverses() {
  // Regression: Media Foundation reported backwards video times for an exact container;
  // GIF conversion refused such recordings as "not increasing".
  let clock = || ContainerClock {
    starts: vec![0, 166_667, 333_333],
    end: 500_000,
    next: 0,
  };
  let mut source = ReversedDecoder {
    decoder_times: [0, 333_333, 166_667].into(),
    clock: clock(),
  };
  let mut frames = Vec::new();
  let end = decode_from(
    &mut source,
    Instant::now() + Duration::from_secs(10),
    10,
    |frame| {
      frames.push(frame.pts);
      Ok(())
    },
  )
  .expect("container-dated decode");
  assert_eq!(frames, [0, 166_667, 333_333]);
  assert_eq!(end, 500_000);

  // More decoded frames than samples, or fewer, are explicit errors.
  let mut extra = clock();
  for _ in 0..3 {
    extra.date().expect("container sample");
  }
  assert!(extra.date().is_err());
  let mut short = clock();
  short.date().expect("first");
  assert!(short.finish().is_err());
}

#[test]
fn decoding_is_bounded_by_its_frame_limit_and_its_deadline() {
  let steps: Vec<_> = (0..5)
    .map(|i| (false, Some((i * 10, 10)), (16, 16)))
    .collect();
  let (result, frames) = run(&mut scripted(&steps), 2);
  let error = result.expect_err("over the limit");
  assert_eq!(error.kind, ErrorKind::Capability);
  assert_eq!(frames.len(), 3, "limit + 1 reads, then refusal");

  let mut source = scripted(&steps);
  let expired = Instant::now()
    .checked_sub(Duration::from_millis(1))
    .expect("past instant");
  let error = decode_from(&mut source, expired, 10, |_| Ok(())).expect_err("deadline");
  assert_eq!(error.kind, ErrorKind::Timeout);
  assert_eq!(source.steps.len(), 5, "nothing is read after the deadline");

  // A recording that ends without frames reports zero length rather than inventing one.
  let (end, frames) = run(&mut scripted(&[(false, None, (16, 16))]), 10);
  assert_eq!((end.expect("empty"), frames.len()), (0, 0));
  // Source failures surface unchanged.
  let (result, _) = run(&mut scripted(&[]), 10);
  assert!(
    result
      .expect_err("exhausted")
      .to_string()
      .contains("script exhausted")
  );
}
