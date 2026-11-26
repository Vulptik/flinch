#![allow(clippy::expect_used)]
use super::*;
use crate::audio::PcmBlock;
const EPOCH: u64 = 100 * SECOND;
fn packet(first: u64, frames: usize, value: f32) -> AudioPacket {
  AudioPacket {
    data: PcmBlock::owned(vec![value; frames * 2]),
    raw_device_position: 600000 + first,
    device_rate: 48000,
    position_rate: 48000,
    qpc_100ns: EPOCH + time::scale(first, SECOND, 48000).expect("dated PCM"),
    read_qpc_100ns: None,
    client_rate: 48000,
    silent: false,
    discontinuity: false,
    wasapi_flags: 0,
    drift_ppm: 0.0,
  }
}
fn collect(normalizer: &mut Normalizer, until: u64) -> Vec<i16> {
  let mut out = Vec::new();
  let mut block = [0; 960];
  while normalizer.next() < time::sample_boundary(until, 48000).expect("sample boundary") {
    let count = normalizer
      .read(until, &mut block)
      .expect("dated PCM output");
    if count == 0 {
      break;
    }
    out.extend_from_slice(&block[..count * 2]);
  }
  out
}
#[test]
fn positive_and_negative_clock_drift_change_pcm_sample_positions_in_the_correct_direction() {
  for ppm in [-1000.0, 1000.0] {
    let mut normalizer = Normalizer::default();
    normalizer.map(EPOCH, 0).expect("map");
    let actual_rate = 48000.0 * (1.0 + ppm / 1_000_000.0);
    for index in 0..30 {
      let mut input = packet(index * 480, 480, 0.0);
      let mut pcm = vec![0.0; 960];
      if index == 20 {
        pcm[0] = 0.8;
        pcm[1] = 0.8;
      }
      input.data = PcmBlock::owned(pcm);
      input.qpc_100ns = EPOCH + (index as f64 * 480.0 * SECOND as f64 / actual_rate).round() as u64;
      input.device_rate = 44100;
      input.raw_device_position = 900000 + index * 441;
      input.drift_ppm = ppm;
      normalizer
        .ingest(input)
        .expect("independent physical and client rates");
    }
    let cut = (14400.0 * SECOND as f64 / actual_rate).round() as u64;
    normalizer.cut(cut).expect("cut");
    let out = collect(&mut normalizer, cut);
    let peak = out
      .as_chunks::<2>()
      .0
      .iter()
      .enumerate()
      .max_by_key(|(_, frame)| frame[0].unsigned_abs())
      .map(|(index, _)| index)
      .expect("decoded PCM marker");
    if ppm > 0.0 {
      assert!(peak < 9600);
    } else {
      assert!(peak > 9600);
    }
    let expected = 9600.0 / (1.0 + ppm / 1_000_000.0);
    assert!((peak as f64 - expected).abs() <= 16.0);
    assert_eq!(normalizer.diagnostics().drift_ppm, ppm);
  }
}
#[test]
fn video_aligned_cuts_remain_sample_exact_through_repeated_resume_mappings() {
  let mut normalizer = Normalizer::default();
  let mut media = 0;
  for index in 1..=30 {
    let qpc = EPOCH + index * SECOND;
    normalizer
      .map(qpc, media)
      .expect("continuous AAC sample anchor");
    let mut input = packet(index * 1000, 960, 0.25);
    input.qpc_100ns = qpc;
    normalizer
      .ingest(input)
      .expect("packet across the next video boundary");
    let cut = time::frame_time(index, 60).expect("video boundary");
    normalizer.cut(cut).expect("pause");
    let out = collect(&mut normalizer, cut);
    assert_eq!(out.len(), 1600);
    assert_eq!(normalizer.next(), index * 800);
    assert_eq!(normalizer.diagnostics().inserted_silence_frames, 0);
    media = cut;
  }
}
#[test]
fn reordered_packets_match_ordered_pcm_and_keep_all_fir_tail_samples_at_the_cut() {
  let mut results = Vec::new();
  for order in [[0, 1, 2], [2, 0, 1]] {
    let mut normalizer = Normalizer::default();
    normalizer.map(EPOCH, 0).expect("map");
    for index in order {
      normalizer
        .ingest(packet(index * 480, 480, (index + 1) as f32 / 8.0))
        .expect("packet");
    }
    normalizer
      .process_ready(EPOCH + REORDER_100NS - 1)
      .expect("reserve");
    assert_eq!(
      normalizer
        .read(SECOND / 100, &mut [0; 960])
        .expect("not settled"),
      0
    );
    normalizer
      .process_ready(EPOCH + SECOND / 10)
      .expect("settled packets");
    normalizer
      .cut(SECOND * 3 / 100)
      .expect("exclusive common cut");
    let out = collect(&mut normalizer, SECOND * 3 / 100);
    assert_eq!(out.len(), 2880);
    assert_eq!(normalizer.diagnostics().inserted_silence_frames, 0);
    assert_eq!(normalizer.diagnostics().late_frames, 0);
    assert!(
      out[2800..].iter().any(|value| *value > 10000),
      "FIR tail was retained"
    );
    results.push(out);
  }
  assert_eq!(results[0], results[1]);
}
#[test]
fn real_holes_silent_packets_and_discontinuity_are_separate_diagnostics() {
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  normalizer
    .ingest(packet(0, 480, 0.25))
    .expect("first packet");
  let mut silent = packet(960, 480, 0.0);
  silent.silent = true;
  silent.discontinuity = true;
  silent.wasapi_flags = 4;
  normalizer
    .ingest(silent)
    .expect("silent packet after a hole");
  normalizer.cut(SECOND * 3 / 100).expect("cut");
  let out = collect(&mut normalizer, SECOND * 3 / 100);
  assert!(out[960..1920].iter().all(|value| *value == 0));
  let d = normalizer.diagnostics();
  assert_eq!(d.inserted_silence_frames, 480);
  assert_eq!(d.source_silent_frames, 480);
  assert_eq!(
    (d.gaps, d.discontinuities, d.invalid_timestamp_packets),
    (1, 1, 1)
  );
}
#[test]
fn pause_discards_queued_future_and_resume_uses_a_new_qpc_mapping() {
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  normalizer
    .ingest(packet(0, 480, 0.8))
    .expect("before pause");
  normalizer
    .ingest(packet(960, 480, 0.9))
    .expect("must not survive pause");
  normalizer.cut(SECOND / 100).expect("pause");
  assert_eq!(collect(&mut normalizer, SECOND / 100).len(), 960);
  normalizer.discard_future();
  assert_eq!(normalizer.diagnostics().queued_frames, 0);
  let new_epoch = EPOCH + 300 * SECOND;
  normalizer
    .map(new_epoch, SECOND / 100)
    .expect("long pause excluded");
  let mut resumed = packet(48000, 480, 0.2);
  resumed.qpc_100ns = new_epoch;
  normalizer.ingest(resumed).expect("current PCM");
  normalizer.cut(SECOND / 50).expect("stop");
  let out = collect(&mut normalizer, SECOND / 50);
  assert_eq!(out.len(), 960);
  assert!(
    out[100..800]
      .iter()
      .all(|value| *value > 6000 && *value < 7000)
  );
  assert_eq!(normalizer.diagnostics().inserted_silence_frames, 0);
}
#[test]
fn one_sample_packets_wait_for_filter_lookahead_instead_of_inserting_silence() {
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  normalizer.ingest(packet(0, 1, 0.5)).expect("one sample");
  normalizer
    .process_ready(EPOCH + REORDER_100NS)
    .expect("settled start");
  assert_eq!(
    normalizer
      .read(SECOND / 48000, &mut [0; 2])
      .expect("tail not ready"),
    0
  );
  let cut = time::scale(1, SECOND, 48000).expect("one sample duration") + 1;
  normalizer.cut(cut).expect("flush pending sample");
  assert_eq!(collect(&mut normalizer, cut).len(), 2);
  assert_eq!(normalizer.diagnostics().inserted_silence_frames, 0);
}
#[test]
fn a_contiguous_packet_dated_before_the_cut_ends_cleanly_after_the_cut_shortened_its_neighbor() {
  // Real Stop: a packet's noisy date fell 98 us before the end of the packet the
  // cut had just shortened. Those samples follow the cut, so they cannot fail the recording.
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  let cut = SECOND * 9 / 1000;
  normalizer.ingest(packet(0, 480, 0.2)).expect("first");
  let mut noisy = packet(480, 480, 0.3);
  noisy.qpc_100ns = EPOCH + cut - SECOND / 2000;
  normalizer.ingest(noisy).expect("position-contiguous");
  normalizer.cut(cut).expect("cut completes");
  assert_eq!(collect(&mut normalizer, cut).len(), 432 * 2);
  let diagnostics = normalizer.diagnostics();
  assert_eq!(diagnostics.overlaps, 0);
  assert_eq!(diagnostics.inserted_silence_frames, 0);
}
#[test]
fn overlapping_source_and_invalid_formats_are_explicit_errors() {
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  normalizer.ingest(packet(0, 480, 0.2)).expect("packet");
  assert!(normalizer.ingest(packet(0, 480, 0.2)).is_err());
  normalizer
    .ingest(packet(240, 480, 0.3))
    .expect("overlapping metadata queued");
  assert_eq!(
    normalizer.cut(SECOND).expect_err("overlap").kind,
    ErrorKind::Protocol
  );
  assert_eq!(normalizer.diagnostics().overlaps, 1);
  let mut invalid = packet(0, 10, 0.2);
  invalid.client_rate = 44100;
  assert!(Normalizer::default().ingest(invalid).is_err());
  assert!(
    Normalizer::default()
      .ingest(packet(0, 1, f32::NAN))
      .is_err()
  );
}
#[test]
fn reserve_is_preallocated_bounded_and_late_packets_never_replace_committed_silence() {
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  let storage = normalizer.queue.as_ptr();
  normalizer
    .ingest(packet(0, 48000, 0.2))
    .expect("one second capacity");
  assert_eq!(
    normalizer
      .ingest(packet(48000, 1, 0.2))
      .expect_err("capacity")
      .kind,
    ErrorKind::Saturation
  );
  assert_eq!(normalizer.queue.as_ptr(), storage);
  let mut late = Normalizer::default();
  late.map(EPOCH, 0).expect("map");
  late
    .process_ready(EPOCH + REORDER_100NS + SECOND / 100)
    .expect("expired reserve");
  assert_eq!(collect(&mut late, SECOND / 100).len(), 960);
  late.ingest(packet(0, 480, 0.9)).expect("late arrival");
  late
    .process_ready(EPOCH + REORDER_100NS + SECOND / 50)
    .expect("count late arrival");
  assert_eq!(late.diagnostics().late_frames, 480);
  assert_eq!(late.diagnostics().late_packets, 1);
  assert_eq!(late.diagnostics().inserted_silence_frames, 480);
}
#[test]
fn discontinuity_starting_inside_the_previous_packet_trims_its_overlap_instead_of_failing() {
  // Real WGC+audio run: a mid-stream discontinuity dated within tolerance before
  // the previous packet's end re-anchored onto frames the flushed tail had already placed.
  for early in [1u64, 2] {
    let mut normalizer = Normalizer::default();
    normalizer.map(EPOCH, 0).expect("map");
    normalizer.ingest(packet(0, 480, 0.25)).expect("first");
    normalizer.ingest(packet(480, 480, 0.25)).expect("second");
    let mut restart = packet(960 - early, 480, 0.5);
    restart.discontinuity = true;
    normalizer.ingest(restart).expect("dated within tolerance");
    let cut = time::scale(1440, SECOND, 48000).expect("cut");
    normalizer
      .cut(cut)
      .expect("overlap is trimmed, not an error");
    let out = collect(&mut normalizer, cut);
    assert_eq!(out.len(), 1440 * 2);
    let diagnostics = normalizer.diagnostics();
    assert_eq!(diagnostics.discontinuities, 1);
    assert_eq!(diagnostics.overlap_trimmed_frames, early);
    // The restarted packet keeps its own date: its trimmed frames are dropped, not shifted.
    assert!(out[(960 + 40) * 2] > out[400 * 2]);
  }
}
#[test]
fn position_contiguous_packets_with_jittered_qpc_stay_one_stream() {
  // Regression: VirtualBox HDA loopback. Consecutive 487-frame packets carried QPCs
  // up to ~2 ms off their sample count; a 2.09 ms early date failed the recording as an overlap.
  // That endpoint's positions advanced at its 44.1 kHz mix rate (447 per 487 client frames).
  for position_rate in [48000u32, 44100] {
    let mut normalizer = Normalizer::default();
    normalizer.map(EPOCH, 0).expect("map");
    let jitter = [0i64, -20_900, 15_000, -5_000, 0];
    let mut first = 0u64;
    for offset in jitter {
      let mut next = packet(first, 487, 0.25);
      next.position_rate = position_rate;
      next.raw_device_position =
        600000 + time::scale(first, u64::from(position_rate), 48000).expect("mix position");
      next.qpc_100ns = next.qpc_100ns.saturating_add_signed(offset);
      normalizer.ingest(next).expect("queued");
      first += 487;
    }
    let cut = time::scale(first, SECOND, 48000).expect("cut");
    normalizer
      .cut(cut)
      .expect("jitter is absorbed, not an overlap");
    let out = collect(&mut normalizer, cut);
    assert_eq!(out.len(), first as usize * 2);
    let diagnostics = normalizer.diagnostics();
    assert_eq!(diagnostics.jitter_redated_packets, 3);
    assert_eq!((diagnostics.overlaps, diagnostics.gaps), (0, 0));
    assert_eq!(diagnostics.inserted_silence_frames, 0);
    // Interior samples stay continuous: no silence where a re-anchor would have flushed.
    assert!(out[200..out.len() - 200].iter().all(|sample| *sample > 0));
  }
}
#[test]
fn a_late_dated_contiguous_packet_is_not_preceded_by_committed_silence() {
  // Two contiguous 10 ms packets; the second is dated 4 ms late, inside the jitter allowance.
  // A processing frontier between its true and raw dates must not commit the gap as silence.
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  normalizer.ingest(packet(0, 480, 0.25)).expect("first");
  let mut late = packet(480, 480, 0.25);
  late.qpc_100ns += 40_000;
  normalizer.ingest(late).expect("second");
  let ms = SECOND / 1000;
  normalizer
    .process_ready(EPOCH + 12 * ms + REORDER_100NS)
    .expect("frontier between the dates");
  let early = collect(&mut normalizer, 12 * ms);
  normalizer
    .process_ready(EPOCH + 30 * ms + REORDER_100NS)
    .expect("frontier past both");
  let cut = time::scale(960, SECOND, 48000).expect("cut");
  normalizer.cut(cut).expect("cut");
  let out = [early, collect(&mut normalizer, cut)].concat();
  assert_eq!(out.len(), 960 * 2);
  let diagnostics = normalizer.diagnostics();
  assert_eq!(diagnostics.inserted_silence_frames, 0);
  assert_eq!(diagnostics.jitter_redated_packets, 1);
  assert!(out[200..out.len() - 200].iter().all(|sample| *sample > 0));
}
/// Contiguous 480-frame packets whose QPCs are offset (100 ns) from their sample-count dates.
fn offsets(normalizer: &mut Normalizer, offsets: &[i64]) -> u64 {
  normalizer.map(EPOCH, 0).expect("map");
  let mut first = 0u64;
  for offset in offsets {
    let mut next = packet(first, 480, 0.25);
    next.qpc_100ns = next.qpc_100ns.saturating_add_signed(*offset);
    normalizer.ingest(next).expect("queued");
    first += 480;
  }
  first
}
#[test]
fn isolated_outliers_are_absorbed_but_persistent_offsets_re_anchor() {
  // A single 10.7 ms early outlier (the VM run that still failed after the jitter fix) stays on
  // the continuous stream.
  let mut isolated = Normalizer::default();
  let frames = offsets(&mut isolated, &[0, -107_000, 0, 0]);
  let cut = time::scale(frames, SECOND, 48000).expect("cut");
  isolated.cut(cut).expect("isolated outlier is absorbed");
  assert_eq!(collect(&mut isolated, cut).len(), frames as usize * 2);
  let diagnostics = isolated.diagnostics();
  assert_eq!(
    (
      diagnostics.jitter_redated_packets,
      diagnostics.clock_realignments
    ),
    (1, 0)
  );
  assert_eq!(diagnostics.overlap_trimmed_frames, 0);
  // A persistent 8 ms early offset is absorbed twice, then the third packet re-anchors on its own
  // date and trims the samples it would overlap, instead of failing or drifting forever; the
  // fourth then lies on the new line.
  let mut early = Normalizer::default();
  let frames = offsets(&mut early, &[0, -80_000, -80_000, -80_000, -80_000]);
  let cut = time::scale(frames, SECOND, 48000).expect("cut");
  early.cut(cut).expect("persistent offset re-anchors");
  collect(&mut early, cut);
  let diagnostics = early.diagnostics();
  assert_eq!(
    (
      diagnostics.jitter_redated_packets,
      diagnostics.clock_realignments
    ),
    (2, 1)
  );
  assert!(diagnostics.overlap_trimmed_frames > 0);
  assert_eq!(diagnostics.overlaps, 0);
  // A later date beyond the outlier bound is a real stall: re-anchor with a counted gap.
  let mut late = Normalizer::default();
  let frames = offsets(&mut late, &[0, 300_000]);
  let cut = time::scale(frames, SECOND, 48000).expect("cut") + 300_000;
  late.cut(cut).expect("stall re-anchors");
  let diagnostics = late.diagnostics();
  assert_eq!((diagnostics.clock_realignments, diagnostics.gaps), (1, 1));
  assert_eq!(diagnostics.jitter_redated_packets, 0);
}
#[test]
fn a_position_jump_is_never_absorbed_as_jitter() {
  // Non-contiguous overlap remains a protocol error, whatever its size.
  let mut overlap = Normalizer::default();
  overlap.map(EPOCH, 0).expect("map");
  overlap.ingest(packet(0, 480, 0.2)).expect("first");
  let mut jumped = packet(400, 480, 0.2);
  jumped.raw_device_position += 200;
  overlap.ingest(jumped).expect("queued");
  assert_eq!(
    overlap.cut(SECOND).expect_err("overlap").kind,
    ErrorKind::Protocol
  );
  assert_eq!(overlap.diagnostics().jitter_redated_packets, 0);
  // A position jump is a real hole even when its QPC is within the jitter bound.
  let mut jump = Normalizer::default();
  jump.map(EPOCH, 0).expect("map");
  jump.ingest(packet(0, 480, 0.2)).expect("first");
  let mut skipped = packet(480, 480, 0.2);
  skipped.raw_device_position += 96;
  skipped.qpc_100ns += 20_000;
  jump.ingest(skipped).expect("queued");
  jump
    .cut(time::scale(1440, SECOND, 48000).expect("cut"))
    .expect("gap");
  let diagnostics = jump.diagnostics();
  assert_eq!(
    (diagnostics.jitter_redated_packets, diagnostics.gaps),
    (0, 1)
  );
}
#[test]
fn packets_dated_after_their_read_are_moved_back_by_the_largest_lead() {
  // Regression: VirtualBox HDA dated loopback packets up to 155 ms after they were read, which
  // shifted every recording's audio by that run-dependent amount.
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  let lead = 1_550_000;
  for (index, read_delay) in [(0u64, 20_000u64), (1, 0), (2, 40_000)] {
    let mut next = packet(index * 480, 480, 0.25);
    let captured = next.qpc_100ns;
    next.qpc_100ns = captured + lead;
    next.read_qpc_100ns = Some(captured + read_delay);
    normalizer.ingest(next).expect("queued");
  }
  // The first packet only bounds the lead by 155 - 2 ms; the promptly read second fixes it.
  assert_eq!(normalizer.diagnostics().causal_lead_100ns, lead);
  let dates: Vec<u64> = normalizer
    .queue
    .iter()
    .map(|packet| packet.qpc_100ns)
    .collect();
  assert_eq!(
    dates,
    vec![
      EPOCH + 20_000,
      EPOCH + time::scale(480, SECOND, 48000).expect("second"),
      EPOCH + time::scale(960, SECOND, 48000).expect("third"),
    ]
  );
  // Causal endpoints (dated before their read, as on physical hardware) are untouched.
  let mut causal = Normalizer::default();
  causal.map(EPOCH, 0).expect("map");
  let mut next = packet(0, 480, 0.25);
  next.read_qpc_100ns = Some(next.qpc_100ns + 30_000);
  causal.ingest(next).expect("queued");
  assert_eq!(causal.diagnostics().causal_lead_100ns, 0);
  assert_eq!(causal.queue[0].qpc_100ns, EPOCH);
}
#[test]
fn a_flagged_discontinuity_dated_inside_placed_audio_re_anchors_instead_of_failing() {
  // Regression: a 99-frame VirtualBox packet continuing the previous position but dated 12.9 ms
  // early failed the recording as an overlap. A flagged glitch keeps its own date and trims.
  let mut normalizer = Normalizer::default();
  normalizer.map(EPOCH, 0).expect("map");
  normalizer.ingest(packet(0, 480, 0.25)).expect("first");
  normalizer.ingest(packet(480, 480, 0.25)).expect("second");
  let mut glitch = packet(960, 99, 0.5);
  glitch.qpc_100ns -= 129_000;
  glitch.discontinuity = true;
  normalizer.ingest(glitch).expect("queued");
  let cut = time::scale(1059, SECOND, 48000).expect("cut");
  normalizer
    .cut(cut)
    .expect("re-anchored, not an overlap error");
  collect(&mut normalizer, cut);
  let diagnostics = normalizer.diagnostics();
  assert_eq!((diagnostics.discontinuities, diagnostics.overlaps), (1, 0));
  assert!(diagnostics.overlap_trimmed_frames > 0);
}
#[test]
fn out_of_order_calls_are_explicit_protocol_errors() {
  let mut normalizer = Normalizer::default();
  // Unmapped audio is counted and dropped, and an unmapped cut must match committed samples.
  normalizer
    .ingest(packet(0, 480, 0.1))
    .expect("unmapped packet");
  assert_eq!(normalizer.diagnostics().discarded_unmapped_frames, 480);
  normalizer
    .process_ready(EPOCH)
    .expect("unmapped processing");
  normalizer
    .cut(0)
    .expect("unmapped cut at the committed position");
  assert!(normalizer.cut(SECOND).is_err());
  assert!(normalizer.synthetic_frontier(0).is_err());
  // A second mapping while one is active, or one off the committed cutoff, is refused.
  normalizer.map(EPOCH, 0).expect("map");
  assert!(normalizer.map(EPOCH, 0).is_err());
  normalizer
    .synthetic_frontier(SECOND / 100)
    .expect("packet-free frontier");
  // Processing time never runs backwards, and packets are never dated far in the future.
  normalizer.process_ready(EPOCH + SECOND).expect("process");
  assert!(normalizer.process_ready(EPOCH).is_err());
  let mut future = packet(0, 480, 0.1);
  future.qpc_100ns = EPOCH + 10 * SECOND;
  normalizer.ingest(future).expect("queued future packet");
  assert!(
    normalizer.synthetic_frontier(0).is_err(),
    "a queued packet blocks the frontier"
  );
  assert!(normalizer.process_ready(EPOCH + 2 * SECOND).is_err());
}
