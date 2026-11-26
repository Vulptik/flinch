//! Worker-wide deadlines. Heartbeats inspect atomics without borrowing a native owner.
use crate::{EngineError, ErrorKind, Result};
use std::{
  mem::ManuallyDrop,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  time::{Duration, Instant},
};

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum Stage {
  Capture,
  EncoderCall,
  EncoderOutput,
  AudioSource,
  /// The microphone has its own WASAPI owner, running beside the system-audio one.
  MicrophoneSource,
  AudioEncode,
  Write,
  Finalize,
}
impl Stage {
  fn name(self) -> &'static str {
    match self {
      Self::Capture => "capture/GPU owner",
      Self::EncoderCall => "native encoder call",
      Self::EncoderOutput => "pending encoded output",
      Self::AudioSource => "WASAPI owner",
      Self::MicrophoneSource => "microphone WASAPI owner",
      Self::AudioEncode => "audio normalization/AAC owner",
      Self::Write => "native MP4 write",
      Self::Finalize => "native MP4 finalization",
    }
  }
}
const STAGES: [Stage; 8] = [
  Stage::Capture,
  Stage::EncoderCall,
  Stage::EncoderOutput,
  Stage::AudioSource,
  Stage::MicrophoneSource,
  Stage::AudioEncode,
  Stage::Write,
  Stage::Finalize,
];
pub(crate) struct Watchdog {
  epoch: Instant,
  deadlines: [AtomicU64; STAGES.len()],
  original: Mutex<Option<EngineError>>,
  failed: AtomicBool,
  breached: AtomicU64,
  #[cfg(test)]
  test_clock: Option<Arc<AtomicU64>>,
}
impl Default for Watchdog {
  fn default() -> Self {
    Self {
      epoch: Instant::now(),
      deadlines: std::array::from_fn(|_| AtomicU64::new(0)),
      original: Mutex::new(None),
      failed: AtomicBool::new(false),
      breached: AtomicU64::new(0),
      #[cfg(test)]
      test_clock: None,
    }
  }
}
/// Give native destructors the same process-level deadline boundary as native methods.
pub(crate) struct Watched<T> {
  value: ManuallyDrop<T>,
  watchdog: Arc<Watchdog>,
  stage: Stage,
  taken: bool,
}
impl<T> Watched<T> {
  pub fn new(value: T, watchdog: Arc<Watchdog>, stage: Stage) -> Self {
    Self {
      value: ManuallyDrop::new(value),
      watchdog,
      stage,
      taken: false,
    }
  }
  pub fn into_inner(mut self) -> T {
    self.taken = true;
    // SAFETY: consuming this wrapper transfers its only T; Drop skips T after this move.
    unsafe { ManuallyDrop::take(&mut self.value) }
  }
}
impl<T> std::ops::Deref for Watched<T> {
  type Target = T;
  fn deref(&self) -> &T {
    &self.value
  }
}
impl<T> std::ops::DerefMut for Watched<T> {
  fn deref_mut(&mut self) -> &mut T {
    &mut self.value
  }
}
impl<T> Drop for Watched<T> {
  fn drop(&mut self) {
    if self.taken {
      return;
    }
    let _guard = match self.watchdog.cleanup_scope(self.stage) {
      Ok(guard) => guard,
      Err(_) => {
        self.watchdog.failed.store(true, Ordering::Release);
        None
      },
    };
    // SAFETY: this wrapper exclusively owns T and drops it exactly once, under its owner's watchdog.
    unsafe {
      ManuallyDrop::drop(&mut self.value);
    }
  }
}
pub(crate) struct NativeCall {
  watchdog: Arc<Watchdog>,
  stage: Stage,
  deadline: u64,
}
impl Drop for NativeCall {
  fn drop(&mut self) {
    // No lock, FFI, join or callback on this native-owner completion path.
    match self.watchdog.now() {
      Ok(now) => self.watchdog.returned_at(self.stage, self.deadline, now),
      Err(_) => {
        self.watchdog.failed.store(true, Ordering::Release);
      },
    }
  }
}
impl Watchdog {
  fn latch(&self, stage: Stage) {
    let _ =
      self
        .breached
        .compare_exchange(0, stage as u64 + 1, Ordering::AcqRel, Ordering::Acquire);
  }
  fn returned_at(&self, stage: Stage, deadline: u64, now: u64) {
    if now > deadline {
      self.latch(stage);
    }
    let _ = self.deadlines[stage as usize].compare_exchange(
      deadline,
      0,
      Ordering::AcqRel,
      Ordering::Acquire,
    );
  }
  pub fn remember(&self, error: &EngineError) {
    if let Ok(mut original) = self.original.try_lock() {
      if original.is_none() {
        *original = Some(error.clone());
      }
    } else {
      self.failed.store(true, Ordering::Release);
    }
  }
  fn cleanup_scope(self: &Arc<Self>, stage: Stage) -> Result<Option<NativeCall>> {
    // During unwinding an earlier scope can still be active; retain that existing bound.
    if self.deadlines[stage as usize].load(Ordering::Acquire) != 0 {
      return Ok(None);
    }
    self.begin(stage, crate::limits::ABORT_TIMEOUT).map(Some)
  }
  fn now(&self) -> Result<u64> {
    #[cfg(test)]
    if let Some(clock) = &self.test_clock {
      return Ok(clock.load(Ordering::Acquire));
    }
    u64::try_from(self.epoch.elapsed().as_millis())
      .map_err(|_| EngineError::protocol("Watchdog clock overflow"))
  }
  fn deadline(now: u64, limit: Duration) -> Result<u64> {
    let limit = u64::try_from(limit.as_millis())
      .map_err(|_| EngineError::protocol("Watchdog limit overflow"))?;
    if limit == 0 {
      return Err(EngineError::protocol("Watchdog limit must be positive"));
    }
    now
      .checked_add(limit)
      .ok_or_else(|| EngineError::protocol("Watchdog deadline overflow"))
  }
  pub fn begin(self: &Arc<Self>, stage: Stage, limit: Duration) -> Result<NativeCall> {
    self.begin_at(stage, self.now()?, limit)
  }
  fn begin_at(self: &Arc<Self>, stage: Stage, now: u64, limit: Duration) -> Result<NativeCall> {
    let deadline = Self::deadline(now, limit)?;
    self.deadlines[stage as usize]
      .compare_exchange(0, deadline, Ordering::AcqRel, Ordering::Acquire)
      .map_err(|_| {
        EngineError::protocol(format!("Overlapping watchdog scopes for {}", stage.name()))
      })?;
    Ok(NativeCall {
      watchdog: self.clone(),
      stage,
      deadline,
    })
  }
  pub fn pending_output(&self) -> Result<()> {
    self.pending_at(self.now()?)
  }
  fn pending_at(&self, now: u64) -> Result<()> {
    let deadline = Self::deadline(now, crate::limits::ENCODER_STALL_TIMEOUT)?;
    // Accepting another input must not extend the deadline of the existing backlog.
    let _ = self.deadlines[Stage::EncoderOutput as usize].compare_exchange(
      0,
      deadline,
      Ordering::AcqRel,
      Ordering::Acquire,
    );
    Ok(())
  }
  pub fn output_progress(&self, pending: bool) -> Result<()> {
    let now = self.now()?;
    let previous = self.deadlines[Stage::EncoderOutput as usize].load(Ordering::Acquire);
    if previous != 0 && now > previous {
      self.latch(Stage::EncoderOutput);
    }
    let deadline = if pending {
      Self::deadline(now, crate::limits::ENCODER_STALL_TIMEOUT)?
    } else {
      0
    };
    self.deadlines[Stage::EncoderOutput as usize].store(deadline, Ordering::Release);
    Ok(())
  }
  pub fn check(&self) -> Result<()> {
    self.check_at(self.now()?)
  }
  fn check_at(&self, now: u64) -> Result<()> {
    if self.failed.load(Ordering::Acquire) {
      return Err(EngineError::protocol(
        "Native watchdog could not retain its cleanup deadline or error context",
      ));
    }
    for stage in STAGES {
      let deadline = self.deadlines[stage as usize].load(Ordering::Acquire);
      if (deadline != 0 && now > deadline)
        || self.breached.load(Ordering::Acquire) == stage as u64 + 1
      {
        if let Ok(original) = self.original.try_lock()
          && let Some(mut error) = original.clone()
        {
          error
            .reason
            .push_str(&format!("; native cleanup stalled in {}", stage.name()));
          return Err(error);
        }
        return Err(EngineError::new(
          ErrorKind::Timeout,
          stage.name(),
          "Native stage exceeded its deadline; an active heartbeat is not media progress",
        ));
      }
    }
    Ok(())
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  fn frozen_clock() -> Arc<Watchdog> {
    Arc::new(Watchdog {
      test_clock: Some(Arc::new(AtomicU64::new(0))),
      ..Default::default()
    })
  }
  #[test]
  fn a_late_return_between_heartbeat_samples_remains_a_reported_breach() {
    let watchdog = frozen_clock();
    let native = watchdog
      .begin_at(Stage::Capture, 0, Duration::from_secs(2))
      .expect("native call");
    watchdog.returned_at(Stage::Capture, native.deadline, 2200);
    assert!(watchdog.check_at(2500).is_err());
    drop(native);
    assert!(watchdog.check_at(2500).is_err());
  }
  #[test]
  fn watched_values_drop_exactly_once_and_a_known_native_cause_survives_cleanup_timeout() {
    struct Native(std::sync::Arc<std::sync::atomic::AtomicU64>);
    impl Drop for Native {
      fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
      }
    }
    let watchdog = frozen_clock();
    let drops = Arc::new(AtomicU64::new(0));
    drop(Watched::new(
      Native(drops.clone()),
      watchdog.clone(),
      Stage::EncoderCall,
    ));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    let value = Watched::new(Native(drops.clone()), watchdog.clone(), Stage::Write).into_inner();
    assert_eq!(drops.load(Ordering::Acquire), 1);
    drop(value);
    assert_eq!(drops.load(Ordering::Acquire), 2);
    watchdog.remember(&EngineError {
      native_code: Some(99),
      ..EngineError::new(
        ErrorKind::Native,
        "original native call",
        "original failure",
      )
    });
    let _blocked = watchdog
      .begin_at(Stage::EncoderCall, 0, Duration::from_secs(2))
      .expect("cleanup");
    let error = watchdog
      .check_at(2001)
      .expect_err("deadline preserves native cause");
    assert_eq!(error.native_code, Some(99));
    assert_eq!(error.stage, "original native call");
    assert!(error.reason.starts_with("original failure"));
  }
  #[test]
  fn an_active_heartbeat_or_other_owner_cannot_hide_a_blocked_native_call() {
    let watchdog = frozen_clock();
    let blocked = watchdog
      .begin_at(Stage::Capture, 0, Duration::from_secs(2))
      .expect("capture call");
    let other = watchdog
      .begin_at(Stage::Write, 1900, Duration::from_secs(2))
      .expect("writer progresses");
    assert!(watchdog.check_at(2000).is_ok());
    let error = watchdog.check_at(2001).expect_err("capture deadline");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.stage, "capture/GPU owner");
    drop(blocked);
    assert!(watchdog.check_at(2001).is_ok());
    drop(other);
    assert!(watchdog.check_at(u64::MAX).is_ok());
  }
  #[test]
  fn additional_accepted_input_does_not_extend_pending_output_and_native_return_clears_its_scope() {
    let watchdog = frozen_clock();
    let stall = crate::limits::ENCODER_STALL_TIMEOUT.as_millis() as u64;
    watchdog.pending_at(0).expect("first accepted input");
    watchdog
      .pending_at(stall - 100)
      .expect("more input, no progress");
    // A slow encoder (three seconds between packets under a game) is not a hung one.
    assert!(watchdog.check_at(3000).is_ok());
    assert!(watchdog.check_at(stall + 1).is_err());
    watchdog.output_progress(false).expect("output drained");
    assert!(watchdog.check_at(stall + 1).is_ok());
    let call = watchdog
      .begin_at(Stage::EncoderCall, 0, Duration::from_secs(2))
      .expect("native call");
    assert!(
      watchdog
        .begin_at(Stage::EncoderCall, 1, Duration::from_secs(2))
        .is_err()
    );
    drop(call);
    assert!(watchdog.check_at(2001).is_ok());
  }
}
