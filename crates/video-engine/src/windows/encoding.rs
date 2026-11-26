//! Encoder ownership is separate from the immediate-context/capture owner.
use super::mft::NativeEncoder;
use crate::watchdog::{Stage, Watchdog, Watched};
use crate::{
  EngineError, ErrorKind, Result, limits,
  metrics::{EncoderMeasurements, EncoderReport, ReportSlot},
  types::*,
};
use std::{
  collections::VecDeque,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
  },
  thread,
  time::Duration,
  time::Instant,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

struct Input {
  texture: ID3D11Texture2D,
  slot: u32,
  pts: u64,
  duration: u64,
  key: bool,
  queued_at: Instant,
  source_qpc_100ns: Option<u64>,
}
pub(super) trait Codec {
  fn extra(&self) -> &[u8];
  fn busy(&self, slot: u32) -> bool;
  fn submit(
    &mut self,
    texture: &ID3D11Texture2D,
    slot: u32,
    pts: u64,
    duration: u64,
    key: bool,
  ) -> Result<bool>;
  fn poll(&mut self) -> Result<Option<EncodedPacket>>;
  fn drain(&mut self) -> Result<bool>;
  fn drained(&self) -> bool;
}
impl Codec for NativeEncoder {
  fn extra(&self) -> &[u8] {
    NativeEncoder::extra(self)
  }
  fn busy(&self, slot: u32) -> bool {
    NativeEncoder::busy(self, slot)
  }
  fn submit(&mut self, t: &ID3D11Texture2D, s: u32, p: u64, d: u64, k: bool) -> Result<bool> {
    NativeEncoder::submit(self, t, s, p, d, k)
  }
  fn poll(&mut self) -> Result<Option<EncodedPacket>> {
    NativeEncoder::poll(self)
  }
  fn drain(&mut self) -> Result<bool> {
    NativeEncoder::drain(self)
  }
  fn drained(&self) -> bool {
    NativeEncoder::drained(self)
  }
}
pub struct EncoderService {
  metrics: Arc<ReportSlot<EncoderReport>>,
  input: SyncSender<Input>,
  packets: Receiver<Result<EncodedPacket>>,
  extra: Receiver<Vec<u8>>,
  slots: Arc<Vec<AtomicBool>>,
  stop: Arc<AtomicBool>,
  drain: Arc<AtomicBool>,
  done: Arc<AtomicBool>,
  terminal: Receiver<Result<()>>,
  pub parameter_sets: Vec<u8>,
  pub drained: bool,
}
impl EncoderService {
  pub fn new(
    device: &ID3D11Device,
    luid: i64,
    width: u32,
    height: u32,
    spec: &SessionSpec,
  ) -> Result<Self> {
    Self::new_with_watchdog(
      device,
      luid,
      (width, height),
      spec,
      ConversionKind::Gpu,
      Arc::new(Watchdog::default()),
    )
  }
  pub(crate) fn new_with_watchdog(
    device: &ID3D11Device,
    luid: i64,
    (width, height): (u32, u32),
    spec: &SessionSpec,
    conversion: ConversionKind,
    watchdog: Arc<Watchdog>,
  ) -> Result<Self> {
    let device = device.clone();
    let spec = spec.clone();
    Self::from_factory_context(
      move || NativeEncoder::new(&device, luid, width, height, &spec, conversion),
      limits::PREPARE_TIMEOUT,
      watchdog,
    )
  }
  #[cfg(test)]
  pub(super) fn from_factory<F, C>(create: F) -> Result<Self>
  where
    F: FnOnce() -> Result<C> + Send + 'static,
    C: Codec + 'static,
  {
    Self::from_factory_with_timeout(create, limits::PREPARE_TIMEOUT)
  }
  #[cfg(test)]
  fn from_factory_with_timeout<F, C>(create: F, timeout: Duration) -> Result<Self>
  where
    F: FnOnce() -> Result<C> + Send + 'static,
    C: Codec + 'static,
  {
    Self::from_factory_context(create, timeout, Arc::new(Watchdog::default()))
  }
  fn from_factory_context<F, C>(
    create: F,
    timeout: Duration,
    watchdog: Arc<Watchdog>,
  ) -> Result<Self>
  where
    F: FnOnce() -> Result<C> + Send + 'static,
    C: Codec + 'static,
  {
    let metrics = Arc::new(ReportSlot::default());
    let reports = metrics.clone();
    let (input, inputs) = mpsc::sync_channel::<Input>(2);
    let (output, packets) = mpsc::sync_channel(16);
    let (extra_tx, extra) = mpsc::sync_channel(1);
    let (ready_tx, ready) = mpsc::sync_channel(1);
    let (terminal_tx, terminal) = mpsc::sync_channel(1);
    let slots = Arc::new(
      (0..limits::MAX_SURFACES)
        .map(|_| AtomicBool::new(false))
        .collect::<Vec<_>>(),
    );
    let flags = slots.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let drain = Arc::new(AtomicBool::new(false));
    let draining = drain.clone();
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    thread::Builder::new()
      .name("video hardware encoder".into())
      .spawn(move || {
        let mut measurements = EncoderMeasurements::default();
        let result = (|| {
          let startup = watchdog.begin(Stage::EncoderCall, limits::PREPARE_TIMEOUT)?;
          let _com = Watched::new(
            super::Apartment::new()?,
            watchdog.clone(),
            Stage::EncoderCall,
          );
          let _mf = Watched::new(
            super::MediaRuntime::new()?,
            watchdog.clone(),
            Stage::EncoderCall,
          );
          let mut encoder = Watched::new(create()?, watchdog.clone(), Stage::EncoderCall);
          drop(startup);
          if stopped.load(Ordering::Acquire) {
            return Err(EngineError::new(
              ErrorKind::Cancelled,
              "encoder preparation",
              "Encoder preparation was cancelled",
            ));
          }
          let initial = encoder.extra().to_vec();
          ready_tx
            .send(Ok(initial))
            .map_err(|_| EngineError::protocol("Encoder readiness receiver closed"))?;
          let mut pending = None;
          let mut submitted = [false; limits::MAX_SURFACES];
          let mut requested_drain = false;
          let mut sent_extra = !encoder.extra().is_empty();
          let mut lease_started = [None::<Instant>; limits::MAX_SURFACES];
          let mut output_pending = VecDeque::with_capacity(256);
          let mut last_report = Instant::now();
          while !stopped.load(Ordering::Acquire) {
            for _ in 0..16 {
              let native = watchdog.begin(Stage::EncoderCall, limits::CONTROL_TIMEOUT)?;
              let queried = Instant::now();
              let packet = encoder.poll().inspect_err(|error| watchdog.remember(error));
              drop(native);
              measurements
                .native_query
                .record(queried.elapsed().as_micros() as u64);
              let Some(packet) = packet? else {
                break;
              };
              if measurements.first_output_qpc_100ns.is_none() {
                measurements.first_output_qpc_100ns = Some(super::qpc()?);
              }
              let (pts, duration, accepted): (u64, u64, Instant) = output_pending
                .pop_front()
                .ok_or_else(|| EngineError::protocol("Encoder output has no accepted input"))?;
              packet.validate()?;
              if packet.track != Track::Video || packet.pts as u64 != pts || packet.duration.abs_diff(duration) > 1 {
                return Err(EngineError::protocol(
                  "Encoder output track/time differs from its accepted B-frame-free input (one 100 ns duration tick allowed)",
                ));
              }
              watchdog.output_progress(!output_pending.is_empty())?;
              measurements
                .encoded_output_observed
                .record(accepted.elapsed().as_micros() as u64);
              if !sent_extra {
                extra_tx
                  .try_send(encoder.extra().to_vec())
                  .map_err(|e| EngineError::protocol(e.to_string()))?;
                sent_extra = true;
              }
              output.try_send(Ok(packet)).map_err(|e| {
                EngineError::new(ErrorKind::Saturation, "encoded output", e.to_string())
              })?;
            }
            for slot in 0..limits::MAX_SURFACES {
              if submitted[slot] && !encoder.busy(slot as u32) {
                submitted[slot] = false;
                if let Some(started) = lease_started[slot].take() {
                  measurements
                    .texture_lease_observed
                    .record(started.elapsed().as_micros() as u64);
                }
                measurements.texture_leases_outstanding =
                  submitted.iter().filter(|busy| **busy).count();
                flags[slot].store(false, Ordering::Release);
              }
            }
            if pending.is_none() {
              match inputs.recv_timeout(Duration::from_millis(if submitted.iter().any(|s| *s) {
                2
              } else {
                10
              })) {
                Ok(value) => pending = Some(value),
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                  return Err(EngineError::new(
                    ErrorKind::Cancelled,
                    "encoder",
                    "GPU producer disconnected",
                  ));
                },
              }
            }
            if let Some(frame) = &pending {
              if output_pending.len() >= 256 {
                return Err(EngineError::new(
                  ErrorKind::Saturation,
                  "encoder timing",
                  "Accepted-input accounting exceeded 256 entries",
                ));
              }
              watchdog.pending_output()?;
              let native = watchdog.begin(Stage::EncoderCall, limits::CONTROL_TIMEOUT)?;
              let submitted_at = Instant::now();
              let accepted = encoder
                .submit(
                  &frame.texture,
                  frame.slot,
                  frame.pts,
                  frame.duration,
                  frame.key,
                )
                .inspect_err(|error| watchdog.remember(error));
              drop(native);
              measurements
                .native_submit
                .record(submitted_at.elapsed().as_micros() as u64);
              if accepted? {
                let accepted_qpc = super::qpc()?;
                measurements.accepted_source(frame.source_qpc_100ns, accepted_qpc)?;
                if measurements.first_input_qpc_100ns.is_none() {
                  measurements.first_input_qpc_100ns = Some(accepted_qpc);
                }
                let accepted_at = Instant::now();
                measurements
                  .input_wait
                  .record(frame.queued_at.elapsed().as_micros() as u64);
                output_pending.push_back((frame.pts, frame.duration, accepted_at));
                measurements.pending_outputs_peak =
                  measurements.pending_outputs_peak.max(output_pending.len());
                submitted[frame.slot as usize] = true;
                lease_started[frame.slot as usize] = Some(frame.queued_at);
                measurements.texture_leases_outstanding =
                  submitted.iter().filter(|busy| **busy).count();
                pending = None;
              } else {
                measurements.backpressure = measurements.backpressure.saturating_add(1);
              }
            }
            if pending.is_some() {
              thread::sleep(Duration::from_millis(2));
            }
            if draining.load(Ordering::Acquire)
              && pending.is_none()
              && inputs
                .try_recv()
                .map(|frame| {
                  pending = Some(frame);
                })
                .is_err()
            {
              if !requested_drain {
                let _native = watchdog.begin(Stage::EncoderCall, limits::CONTROL_TIMEOUT)?;
                requested_drain = encoder
                  .drain()
                  .inspect_err(|error| watchdog.remember(error))?;
              }
              if encoder.drained() {
                if !output_pending.is_empty() {
                  return Err(EngineError::protocol(
                    "Encoder drained without returning every accepted input",
                  ));
                }
                break;
              }
            }
            if last_report.elapsed() >= Duration::from_secs(1) {
              reports.publish(measurements.report(false))?;
              last_report = Instant::now();
            }
          }
          // Terminate releases outstanding surface observers before pool availability is withdrawn.
          drop(encoder);
          watchdog.output_progress(false)?;
          for (index, started) in lease_started.into_iter().enumerate() {
            if let Some(started) = started {
              measurements
                .texture_lease_observed
                .record(started.elapsed().as_micros() as u64);
            }
            flags[index].store(false, Ordering::Release);
          }
          measurements.texture_leases_outstanding = 0;
          Ok(())
        })();
        let report = reports.publish(measurements.report(result.is_ok()));
        let result = match (result, report) {
          (Err(mut original), Err(diagnostic)) => {
            original
              .reason
              .push_str(&format!("; metrics: {diagnostic}"));
            Err(original)
          },
          (Err(original), _) => Err(original),
          (Ok(()), Err(error)) => Err(error),
          (Ok(()), Ok(_)) => Ok(()),
        };
        if let Err(error) = &result
          && ready_tx.try_send(Err(error.clone())).is_err()
        {
          eprintln!("Encoder owner: {error}");
        }
        // A separate single-result channel cannot be starved by a full packet queue.
        if terminal_tx.try_send(result).is_err() {
          eprintln!("Encoder terminal receiver unavailable");
        }
        finished.store(true, Ordering::Release);
      })
      .map_err(|e| EngineError::io("encoder thread", e))?;
    let prepared = ready
      .recv_timeout(timeout)
      .map_err(|e| EngineError::new(ErrorKind::Timeout, "encoder preparation", e.to_string()))
      .and_then(|result| result);
    let parameter_sets = match prepared {
      Ok(parameter_sets) => parameter_sets,
      Err(error) => {
        stop.store(true, Ordering::Release);
        return Err(error);
      },
    };
    Ok(Self {
      metrics,
      input,
      packets,
      extra,
      slots,
      stop,
      drain,
      done,
      terminal,
      parameter_sets,
      drained: false,
    })
  }
  pub fn busy(&self, slot: u32) -> bool {
    self
      .slots
      .get(slot as usize)
      .is_none_or(|s| s.load(Ordering::Acquire))
  }
  pub fn submit(
    &self,
    texture: &ID3D11Texture2D,
    slot: u32,
    pts: u64,
    duration: u64,
    key: bool,
  ) -> Result<bool> {
    self.submit_with_source(texture, slot, pts, duration, key, None)
  }
  pub fn submit_with_source(
    &self,
    texture: &ID3D11Texture2D,
    slot: u32,
    pts: u64,
    duration: u64,
    key: bool,
    source_qpc_100ns: Option<u64>,
  ) -> Result<bool> {
    self.ensure_active()?;
    if duration == 0
      || pts
        .checked_add(duration)
        .is_none_or(|end| end > i64::MAX as u64)
    {
      return Err(EngineError::protocol(
        "Video input exceeds the native signed timestamp extent",
      ));
    }
    let flag = self
      .slots
      .get(slot as usize)
      .ok_or_else(|| EngineError::protocol("Encoder pool slot"))?;
    if flag.swap(true, Ordering::AcqRel) {
      return Ok(false);
    }
    match self.input.try_send(Input {
      texture: texture.clone(),
      slot,
      pts,
      duration,
      key,
      queued_at: Instant::now(),
      source_qpc_100ns,
    }) {
      Ok(()) => Ok(true),
      Err(TrySendError::Full(_)) => {
        flag.store(false, Ordering::Release);
        Ok(false)
      },
      Err(e) => {
        flag.store(false, Ordering::Release);
        Err(EngineError::new(
          ErrorKind::Native,
          "encoder submission",
          e.to_string(),
        ))
      },
    }
  }
  pub fn report(&self) -> Result<Option<EncoderReport>> {
    let mut report = self.metrics.read()?;
    if let Some(report) = &mut report {
      (report.reports_coalesced, report.report_reads_contended) = self.metrics.coalesced();
    }
    Ok(report)
  }
  pub fn extra(&self) -> &[u8] {
    &self.parameter_sets
  }
  pub fn poll(&mut self) -> Result<Option<EncodedPacket>> {
    self.ensure_active()?;
    if let Ok(value) = self.extra.try_recv() {
      self.parameter_sets = value;
    }
    let first = self.packets.try_recv();
    self.take_or_complete(first)
  }
  /// Act on one queue read. An owner sends its last packet before it marks itself done, so a
  /// read that found the queue empty is repeated after `done` is observed: the last packet may
  /// have arrived between the two, and completing without it would silently drop a picture.
  fn take_or_complete(
    &mut self,
    first: std::result::Result<Result<EncodedPacket>, TryRecvError>,
  ) -> Result<Option<EncodedPacket>> {
    match first {
      Ok(packet) => packet.map(Some),
      Err(TryRecvError::Empty) => {
        if self.done.load(Ordering::Acquire) {
          if let Ok(packet) = self.packets.try_recv() {
            return packet.map(Some);
          }
          self.complete()?;
        }
        Ok(None)
      },
      Err(TryRecvError::Disconnected) => {
        if self.done.load(Ordering::Acquire) {
          self.complete()?;
          Ok(None)
        } else {
          Err(EngineError::protocol("Encoder output disconnected"))
        }
      },
    }
  }
  fn complete(&mut self) -> Result<()> {
    if self.drained {
      return Ok(());
    }
    self
      .terminal
      .try_recv()
      .map_err(|e| EngineError::protocol(format!("Encoder terminal result missing: {e}")))??;
    if !self.drain.load(Ordering::Acquire) {
      return Err(EngineError::protocol(
        "Encoder exited without a drain request",
      ));
    }
    self.drained = true;
    Ok(())
  }
  pub fn drain(&self) -> Result<bool> {
    self.ensure_active()?;
    self.drain.store(true, Ordering::Release);
    Ok(true)
  }
  pub fn drained(&self) -> bool {
    self.drained
  }
  pub(crate) fn cancel(&self) {
    self.stop.store(true, Ordering::Release);
  }
  fn ensure_active(&self) -> Result<()> {
    if self.stop.load(Ordering::Acquire) {
      return Err(EngineError::new(
        ErrorKind::Cancelled,
        "encoder",
        "Encoder was cancelled",
      ));
    }
    Ok(())
  }
}
impl Drop for EncoderService {
  fn drop(&mut self) {
    self.cancel();
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  use std::{collections::VecDeque, sync::atomic::AtomicUsize, time::Instant};
  struct Fake {
    output: VecDeque<(u32, ID3D11Texture2D, EncodedPacket)>,
    held: [bool; limits::MAX_SURFACES],
    draining: bool,
    attempts: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    fail_output: bool,
    /// Keeps every accepted frame, so its surface stays leased until the owner stops.
    hold_output: bool,
    corrupt_output: Option<fn(&mut EncodedPacket)>,
    /// Holds the first poll until the test has queued its inputs, so races become scripted.
    gate: Option<mpsc::Receiver<()>>,
  }
  impl Codec for Fake {
    fn extra(&self) -> &[u8] {
      &[1, 2, 3]
    }
    fn busy(&self, slot: u32) -> bool {
      self.held.get(slot as usize).copied().unwrap_or(true)
    }
    fn submit(
      &mut self,
      texture: &ID3D11Texture2D,
      slot: u32,
      pts: u64,
      duration: u64,
      _: bool,
    ) -> Result<bool> {
      if self.attempts.fetch_add(1, Ordering::AcqRel) == 0 {
        return Ok(false);
      }
      self.held[slot as usize] = true;
      self.output.push_back((
        slot,
        texture.clone(),
        EncodedPacket {
          track: Track::Video,
          pts: pts as i64,
          dts: pts as i64,
          duration,
          keyframe: true,
          data: vec![slot as u8],
        },
      ));
      Ok(true)
    }
    fn poll(&mut self) -> Result<Option<EncodedPacket>> {
      if let Some(gate) = self.gate.take() {
        gate
          .recv_timeout(Duration::from_secs(5))
          .expect("test released the gated poll");
      }
      if self.hold_output {
        return Ok(None);
      }
      if let Some((slot, _texture, mut packet)) = self.output.pop_front() {
        self.held[slot as usize] = false;
        if self.fail_output {
          return Err(EngineError::new(
            ErrorKind::Native,
            "scripted encoder output",
            "retained native failure",
          ));
        }
        if let Some(corrupt) = self.corrupt_output {
          corrupt(&mut packet);
        }
        Ok(Some(packet))
      } else {
        Ok(None)
      }
    }
    fn drain(&mut self) -> Result<bool> {
      self.draining = true;
      Ok(true)
    }
    fn drained(&self) -> bool {
      self.draining && self.output.is_empty()
    }
  }
  impl Drop for Fake {
    fn drop(&mut self) {
      self.closed.store(true, Ordering::Release);
    }
  }
  fn factory(fail_output: bool, hold_output: bool) -> (Fake, Arc<AtomicUsize>, Arc<AtomicBool>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    (
      Fake {
        output: VecDeque::new(),
        held: [false; limits::MAX_SURFACES],
        draining: false,
        attempts: attempts.clone(),
        closed: closed.clone(),
        fail_output,
        hold_output,
        corrupt_output: None,
        gate: None,
      },
      attempts,
      closed,
    )
  }
  #[test]
  fn explicit_cancellation_rejects_new_input_and_drain_without_leasing_a_surface() {
    let (gpu, _) = super::super::gpu::mock::fixture();
    let (fake, attempts, _) = factory(false, false);
    let mut service = EncoderService::from_factory(move || Ok(fake)).expect("codec double");
    service.cancel();
    service.cancel();
    assert_eq!(
      service
        .submit(&gpu.surfaces[0], 0, 0, 166666, true)
        .expect_err("cancelled admission")
        .kind,
      ErrorKind::Cancelled
    );
    assert!(!service.busy(0));
    assert_eq!(
      service.poll().expect_err("cancelled output").kind,
      ErrorKind::Cancelled
    );
    assert_eq!(
      service.drain().expect_err("cancelled drain").kind,
      ErrorKind::Cancelled
    );
    assert_eq!(attempts.load(Ordering::Acquire), 0);
  }
  #[test]
  fn owner_retries_backpressure_once_keeps_texture_immutable_and_drains_accepted_input() {
    let (gpu, state) = super::super::gpu::mock::fixture();
    let (fake, attempts, closed) = factory(false, false);
    let mut service = EncoderService::from_factory(move || Ok(fake)).expect("codec owner");
    assert_eq!(service.extra(), [1, 2, 3]);
    assert!(
      service
        .submit(&gpu.surfaces[0], 0, 0, 166666, true)
        .expect("queued")
    );
    assert!(
      !service
        .submit(&gpu.surfaces[0], 0, 166666, 166667, false)
        .expect("lease still borrowed")
    );
    assert!(service.busy(limits::MAX_SURFACES as u32));
    assert!(
      service
        .submit(&gpu.surfaces[0], limits::MAX_SURFACES as u32, 0, 1, false)
        .is_err()
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let packet = loop {
      if let Some(packet) = service.poll().expect("encoded output") {
        break packet;
      }
      assert!(Instant::now() < deadline);
      thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(packet.pts, 0);
    assert_eq!(packet.data, [0]);
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    service.drain().expect("request drain");
    while !service.drained() {
      service.poll().expect("drain");
      assert!(Instant::now() < deadline);
      thread::sleep(Duration::from_millis(1));
    }
    assert!(closed.load(Ordering::Acquire));
    let report = service
      .report()
      .expect("encoder report")
      .expect("final report");
    assert!(report.final_report);
    assert_eq!(report.input_wait.count, 1);
    assert_eq!(report.native_submit.count, 2);
    assert_eq!(report.backpressure, 1);
    assert_eq!(report.encoded_output_observed.count, 1);
    assert_eq!(report.texture_lease_observed.count, 1);
    assert_eq!(report.texture_leases_outstanding, 0);
    drop(service);
    drop(gpu);
    assert_eq!(state.live.load(Ordering::Acquire), 0);
  }
  /// Regression for the coverage gate: a frame still queued when the drain starts was only
  /// reached when thread timing happened to line up, so the measured lines differed per machine.
  #[test]
  fn a_frame_queued_when_draining_starts_is_still_encoded_before_the_drain_completes() {
    let (gpu, _) = super::super::gpu::mock::fixture();
    let (mut fake, attempts, closed) = factory(false, false);
    let (release, gate) = mpsc::channel();
    fake.gate = Some(gate);
    let mut service = EncoderService::from_factory(move || Ok(fake)).expect("codec owner");
    // The owner is held in its first poll: both frames and the drain request are queued first.
    assert!(
      service
        .submit(&gpu.surfaces[0], 0, 0, 166666, true)
        .expect("first queued")
    );
    assert!(
      service
        .submit(&gpu.surfaces[1], 1, 166666, 166667, false)
        .expect("second queued")
    );
    // The two-frame input queue is full: refused without keeping the surface leased.
    assert!(
      !service
        .submit(&gpu.surfaces[2], 2, 333333, 166667, false)
        .expect("queue full")
    );
    assert!(!service.busy(2));
    service.drain().expect("request drain");
    release.send(()).expect("release the owner");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut packets = Vec::new();
    while !service.drained() {
      if let Some(packet) = service.poll().expect("drain") {
        packets.push(packet.pts);
      }
      assert!(Instant::now() < deadline, "drain did not complete");
      thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(packets, [0, 166666]);
    // One scripted backpressure retry, then both frames.
    assert_eq!(attempts.load(Ordering::Acquire), 3);
    assert!(closed.load(Ordering::Acquire));
    let report = service.report().expect("report").expect("final report");
    assert_eq!(report.encoded_output_observed.count, 2);
    assert_eq!(report.texture_leases_outstanding, 0);
  }
  /// Regression for the coverage gate: a consumer too slow for the 16-packet output queue was only
  /// reached on a slow runner, so the measured lines differed per machine.
  #[test]
  fn a_full_output_queue_fails_as_saturation_after_delivering_what_fit() {
    let (gpu, _) = super::super::gpu::mock::fixture();
    let (fake, _, closed) = factory(false, false);
    let mut service = EncoderService::from_factory(move || Ok(fake)).expect("codec owner");
    let deadline = Instant::now() + Duration::from_secs(5);
    // Never poll while feeding: the 17th encoded packet has nowhere to go.
    let mut sent = 0u64;
    while sent < 17 {
      let slot = (sent % 8) as u32;
      match service.submit(
        &gpu.surfaces[slot as usize],
        slot,
        sent * 166666,
        166666,
        sent == 0,
      ) {
        Ok(true) => sent += 1,
        Ok(false) => {},
        // The owner may already have failed once the queue overflowed.
        Err(_) => break,
      }
      assert!(Instant::now() < deadline, "frames were never accepted");
      thread::sleep(Duration::from_millis(1));
    }
    let mut delivered = 0;
    let error = loop {
      match service.poll() {
        Ok(Some(_)) => delivered += 1,
        Ok(None) => {},
        Err(error) => break error,
      }
      assert!(Instant::now() < deadline, "saturation was never reported");
      thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(
      (error.kind, error.stage.as_str()),
      (ErrorKind::Saturation, "encoded output")
    );
    assert_eq!(delivered, 16, "every packet that fit is still delivered");
    assert!(closed.load(Ordering::Acquire));
  }
  /// Regression for the coverage gate: stopping with a surface still leased to the codec was only
  /// reached when output happened to lag, so the measured lines differed per machine.
  #[test]
  fn stopping_with_a_surface_still_leased_releases_it_and_records_the_lease() {
    let (gpu, _) = super::super::gpu::mock::fixture();
    let (fake, attempts, closed) = factory(false, true);
    let service = EncoderService::from_factory(move || Ok(fake)).expect("codec owner");
    assert!(
      service
        .submit(&gpu.surfaces[0], 0, 0, 166666, true)
        .expect("queued")
    );
    // One scripted backpressure refusal, then the codec accepts and keeps the frame.
    let deadline = Instant::now() + Duration::from_secs(5);
    while attempts.load(Ordering::Acquire) < 2 {
      assert!(Instant::now() < deadline, "codec never accepted the frame");
      thread::sleep(Duration::from_millis(1));
    }
    assert!(service.busy(0), "the codec still holds the surface");
    service.cancel();
    let report = loop {
      if let Some(report) = service.report().expect("report").filter(|r| r.final_report) {
        break report;
      }
      assert!(Instant::now() < deadline, "owner never stopped");
      thread::sleep(Duration::from_millis(1));
    };
    assert!(
      closed.load(Ordering::Acquire),
      "codec released before the final report"
    );
    assert!(
      !service.busy(0),
      "stopping returns the leased surface to the pool"
    );
    assert_eq!(report.texture_lease_observed.count, 1);
    assert_eq!(report.texture_leases_outstanding, 0);
  }
  #[test]
  fn malformed_native_packets_fail_before_writer_admission_and_invalid_input_never_leases_a_surface()
   {
    let mutations: [fn(&mut EncodedPacket); 5] = [
      |packet| packet.track = Track::Audio,
      |packet| packet.pts += 1,
      |packet| packet.dts += 1,
      |packet| packet.duration += 1000,
      |packet| packet.data.clear(),
    ];
    for corrupt in mutations {
      let (gpu, _) = super::super::gpu::mock::fixture();
      let (mut fake, _, _) = factory(false, false);
      fake.corrupt_output = Some(corrupt);
      let mut service = EncoderService::from_factory(move || Ok(fake)).expect("codec owner");
      assert!(service.submit(&gpu.surfaces[0], 0, 0, 0, false).is_err());
      assert!(
        service
          .submit(&gpu.surfaces[0], 0, i64::MAX as u64, 1, false)
          .is_err()
      );
      assert!(!service.busy(0));
      service
        .submit(&gpu.surfaces[0], 0, 0, 166666, true)
        .expect("valid input");
      let deadline = Instant::now() + Duration::from_secs(2);
      loop {
        match service.poll() {
          Err(error) => {
            assert_eq!(error.kind, ErrorKind::Protocol);
            break;
          },
          Ok(packet) => {
            assert!(
              packet.is_none(),
              "malformed output cannot escape into the mux queue"
            );
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
          },
        }
      }
    }
  }
  #[test]
  fn owner_failure_is_not_mistaken_for_successful_eof_and_failed_prepare_keeps_its_cause() {
    let error = EncoderService::from_factory(|| {
      Err::<Fake, _>(EngineError::new(
        ErrorKind::Capability,
        "scripted prepare",
        "missing codec",
      ))
    })
    .err()
    .expect("prepare error");
    assert_eq!(error.kind, ErrorKind::Capability);
    assert_eq!(error.stage, "scripted prepare");
    let (gpu, _) = super::super::gpu::mock::fixture();
    let (fake, _, closed) = factory(true, false);
    let mut service = EncoderService::from_factory(move || Ok(fake)).expect("owner");
    service
      .submit(&gpu.surfaces[0], 0, 0, 166666, true)
      .expect("input");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
      match service.poll() {
        Err(error) => {
          assert_eq!(error.stage, "scripted encoder output");
          assert_eq!(error.reason, "retained native failure");
          assert!(!service.drained());
          break;
        },
        Ok(_) => {
          assert!(Instant::now() < deadline);
          thread::sleep(Duration::from_millis(1));
        },
      }
    }
    assert!(closed.load(Ordering::Acquire));
  }
  #[test]
  fn failed_preparation_cancels_a_late_owner_without_joining_its_native_call() {
    let (fake, _, closed) = factory(false, false);
    let (release, blocked) = mpsc::sync_channel(1);
    let error = EncoderService::from_factory_with_timeout(
      move || {
        blocked
          .recv_timeout(Duration::from_secs(2))
          .expect("release simulated native call");
        Ok(fake)
      },
      Duration::from_millis(1),
    )
    .err()
    .expect("preparation timeout");
    assert_eq!(error.kind, ErrorKind::Timeout);
    release
      .send(())
      .expect("owner was not joined by the failed constructor");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !closed.load(Ordering::Acquire) {
      assert!(Instant::now() < deadline, "late owner was not released");
      thread::sleep(Duration::from_millis(1));
    }
  }
  #[test]
  fn a_finished_owner_completes_even_before_its_output_channel_closes() {
    // The owner marks itself done before it drops its packet sender; a poll between the two
    // sees an empty, still-connected queue and must still take the terminal result.
    let (input, _inputs) = mpsc::sync_channel(1);
    let (_packet_tx, packets) = mpsc::sync_channel(1);
    let (_extra_tx, extra) = mpsc::sync_channel(1);
    let (terminal_tx, terminal) = mpsc::sync_channel(1);
    terminal_tx.send(Ok(())).expect("terminal result");
    let mut service = EncoderService {
      metrics: Arc::new(ReportSlot::default()),
      input,
      packets,
      extra,
      slots: Arc::new(Vec::new()),
      stop: Arc::new(AtomicBool::new(false)),
      drain: Arc::new(AtomicBool::new(true)),
      done: Arc::new(AtomicBool::new(true)),
      terminal,
      parameter_sets: Vec::new(),
      drained: false,
    };
    assert!(service.poll().expect("completed").is_none());
    assert!(service.drained());
    assert!(service.poll().expect("still completed").is_none());
  }
  #[test]
  fn a_last_packet_sent_between_the_empty_read_and_done_is_not_dropped() {
    // Interleaving: the consumer reads an empty queue, then the owner sends its last packet,
    // its terminal result and marks itself done before the consumer checks `done`.
    let (input, _inputs) = mpsc::sync_channel(1);
    let (packet_tx, packets) = mpsc::sync_channel(1);
    let (_extra_tx, extra) = mpsc::sync_channel(1);
    let (terminal_tx, terminal) = mpsc::sync_channel(1);
    let mut service = EncoderService {
      metrics: Arc::new(ReportSlot::default()),
      input,
      packets,
      extra,
      slots: Arc::new(Vec::new()),
      stop: Arc::new(AtomicBool::new(false)),
      drain: Arc::new(AtomicBool::new(true)),
      done: Arc::new(AtomicBool::new(false)),
      terminal,
      parameter_sets: Vec::new(),
      drained: false,
    };
    let first = service.packets.try_recv();
    assert!(matches!(first, Err(TryRecvError::Empty)));
    packet_tx
      .send(Ok(EncodedPacket {
        track: Track::Video,
        pts: 0,
        dts: 0,
        duration: 1,
        keyframe: true,
        data: vec![1],
      }))
      .expect("last packet");
    terminal_tx.send(Ok(())).expect("terminal result");
    service.done.store(true, Ordering::Release);
    let packet = service
      .take_or_complete(first)
      .expect("packet")
      .expect("the last packet is delivered");
    assert_eq!(packet.data, [1]);
    assert!(!service.drained());
    assert!(service.poll().expect("completed").is_none());
    assert!(service.drained());
  }
  #[test]
  fn dropping_the_owner_never_joins_a_native_call() {
    let (fake, _, closed) = factory(false, true);
    let service = EncoderService::from_factory(move || Ok(fake)).expect("owner");
    let start = Instant::now();
    drop(service);
    assert!(start.elapsed() < Duration::from_millis(50));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !closed.load(Ordering::Acquire) {
      assert!(Instant::now() < deadline);
      thread::sleep(Duration::from_millis(1));
    }
  }
}
