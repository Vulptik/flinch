use super::*;
use std::collections::VecDeque;

/// A scripted codec: outputs appear `delay` polls after each accepted frame.
struct Scripted {
  extra: Vec<u8>,
  delay: u32,
  pending: VecDeque<(EncodedPacket, u32)>,
  busy_polls: u32,
  full_submits: u32,
  draining: bool,
  stall: bool,
  shift_pts: i64,
  idr_first: bool,
  extra_packet: bool,
  submitted: u32,
}
impl Default for Scripted {
  fn default() -> Self {
    Self {
      extra: vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2],
      delay: 2,
      pending: VecDeque::new(),
      busy_polls: 0,
      full_submits: 1,
      draining: false,
      stall: false,
      shift_pts: 0,
      idr_first: true,
      extra_packet: false,
      submitted: 0,
    }
  }
}
impl Codec for Scripted {
  fn extra(&self) -> &[u8] {
    &self.extra
  }
  fn busy(&self, _: u32) -> bool {
    self.busy_polls > 0
  }
  fn submit(
    &mut self,
    _: &ID3D11Texture2D,
    slot: u32,
    pts: u64,
    duration: u64,
    key: bool,
  ) -> Result<bool> {
    assert_eq!(slot, 0, "the self-test reuses one slot");
    if self.full_submits > 0 {
      self.full_submits -= 1;
      return Ok(false);
    }
    let nal = if key && self.idr_first { 0x65 } else { 0x41 };
    self.pending.push_back((
      EncodedPacket {
        track: Track::Video,
        pts: pts as i64 + self.shift_pts,
        dts: pts as i64 + self.shift_pts,
        duration,
        keyframe: key,
        data: vec![0, 0, 0, 1, nal, 9],
      },
      self.delay,
    ));
    self.busy_polls = 1;
    self.submitted += 1;
    Ok(true)
  }
  fn poll(&mut self) -> Result<Option<EncodedPacket>> {
    self.busy_polls = self.busy_polls.saturating_sub(1);
    if self.stall {
      return Ok(None);
    }
    match self.pending.front_mut() {
      Some((_, 0)) => Ok(self.pending.pop_front().map(|(packet, _)| packet)),
      Some((_, wait)) => {
        *wait -= 1;
        Ok(None)
      },
      None if self.draining && self.extra_packet => {
        self.extra_packet = false;
        Ok(Some(EncodedPacket {
          track: Track::Video,
          pts: 999,
          dts: 999,
          duration: 1,
          keyframe: false,
          data: vec![0, 0, 1, 0x41],
        }))
      },
      None => Ok(None),
    }
  }
  fn drain(&mut self) -> Result<bool> {
    self.draining = true;
    Ok(true)
  }
  fn drained(&self) -> bool {
    self.draining && self.pending.is_empty() && !self.extra_packet
  }
}
fn run(codec: &mut Scripted, deadline: Duration) -> Result<usize> {
  let (gpu, _driver) = super::super::gpu::mock::fixture();
  exercise(codec, &gpu.surfaces[0], 30, deadline)
}

#[test]
fn a_healthy_encoder_passes_with_backpressure_and_delayed_output() {
  let mut codec = Scripted::default();
  assert_eq!(run(&mut codec, DEADLINE).expect("healthy"), 18);
  assert_eq!(codec.submitted, FRAMES);
  assert!(required(EncoderKind::Amf) && required(EncoderKind::Nvenc) && required(EncoderKind::Qsv));
  // Software is the last tier: a refusal there has nothing to fall back to.
  assert!(!required(EncoderKind::Software));
}

#[test]
fn broken_encoders_are_refused_with_their_cause() {
  // No parameter sets: a decoder could not start the file.
  let mut codec = Scripted {
    extra: vec![0, 0, 0, 1, 0x67, 1],
    ..Default::default()
  };
  assert!(
    run(&mut codec, DEADLINE)
      .expect_err("no PPS")
      .reason
      .contains("SPS and a PPS")
  );
  // A stalled encoder times out instead of hanging Prepare.
  let mut codec = Scripted {
    stall: true,
    ..Default::default()
  };
  let error = run(&mut codec, Duration::from_millis(30)).expect_err("stall");
  assert_eq!(error.kind, ErrorKind::Timeout);
  assert!(error.reason.contains("waiting for output"), "{error}");
  // Timestamps the driver rewrote would desynchronize the file.
  let mut codec = Scripted {
    shift_pts: 1,
    ..Default::default()
  };
  assert!(
    run(&mut codec, DEADLINE)
      .expect_err("shifted")
      .reason
      .contains("Packet 0 is timed")
  );
  // A first frame that is not an IDR cannot start a stream.
  let mut codec = Scripted {
    idr_first: false,
    ..Default::default()
  };
  assert!(
    run(&mut codec, DEADLINE)
      .expect_err("no IDR")
      .reason
      .contains("Packet 0")
  );
  // An unexpected extra packet means frames and outputs do not pair up.
  let mut codec = Scripted {
    extra_packet: true,
    ..Default::default()
  };
  assert!(
    run(&mut codec, DEADLINE)
      .expect_err("extra")
      .reason
      .contains("4 packets")
  );
  // Whatever failed, selection sees a capability result that keeps the cause.
  let refused = refuse(
    EncoderKind::Nvenc,
    EngineError {
      native_code: Some(8),
      ..EngineError::new(
        ErrorKind::Native,
        "NVENC submit",
        "NV_ENC_ERR_INVALID_PARAM",
      )
    },
  );
  assert_eq!(refused.kind, ErrorKind::Capability);
  assert_eq!(refused.native_code, Some(8));
  assert_eq!(
    refused.reason,
    "Nvenc failed its self-test: NVENC submit: NV_ENC_ERR_INVALID_PARAM"
  );
}
