use crate::app::context::AppHandle;
use arboard::Clipboard;
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingControlEvent {
  StopRequested,
  PauseRequested(bool),
}

pub struct RecordingControls {
  stop_flag: AtomicBool,
  stop_claimed: AtomicBool,
  pause_flag: AtomicBool,
  pub(crate) ready: AtomicBool,
  pub(crate) elapsed_ms: AtomicU64,
  pub(crate) pause_pending: AtomicBool,
  pub(crate) pause_failed: AtomicBool,
  /// The microphone the controls show: 0 none, 1 audible, 2 muted, 3 lost.
  microphone: AtomicU8,
  microphone_pending: AtomicBool,
  event_tx: tokio::sync::watch::Sender<Option<RecordingControlEvent>>,
  /// Requested microphone state (audible); separate from `event_tx`, which keeps one event.
  microphone_tx: tokio::sync::watch::Sender<Option<bool>>,
}

impl Default for RecordingControls {
  fn default() -> Self {
    Self::new()
  }
}

impl RecordingControls {
  pub fn new() -> Self {
    let (event_tx, _) = tokio::sync::watch::channel(None);
    let (microphone_tx, _) = tokio::sync::watch::channel(None);
    Self {
      stop_flag: AtomicBool::new(false),
      stop_claimed: AtomicBool::new(false),
      pause_flag: AtomicBool::new(false),
      ready: AtomicBool::new(false),
      elapsed_ms: AtomicU64::new(0),
      pause_pending: AtomicBool::new(false),
      pause_failed: AtomicBool::new(false),
      microphone: AtomicU8::new(0),
      microphone_pending: AtomicBool::new(false),
      event_tx,
      microphone_tx,
    }
  }

  pub fn request_stop(&self) {
    self.stop_flag.store(true, Ordering::SeqCst);
    if self
      .event_tx
      .send(Some(RecordingControlEvent::StopRequested))
      .is_err()
    {
      tracing::debug!(
        operation = "recording_controls",
        "Stop request had no active receiver"
      );
    }
  }

  pub fn request_pause(&self, paused: bool) {
    if !self.ready.load(Ordering::SeqCst)
      || self.stop_requested()
      || self
        .pause_pending
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
      return;
    }
    self.pause_failed.store(false, Ordering::SeqCst);
    if self
      .event_tx
      .send(Some(RecordingControlEvent::PauseRequested(paused)))
      .is_err()
    {
      self.fail_pause();
      tracing::error!(
        operation = "recording_controls",
        paused,
        "Pause request had no active receiver"
      );
    }
  }

  /// Asks for the microphone to become audible (or muted). Ignored before the recording runs,
  /// while stopping, while a switch is in flight, or when there is no usable microphone.
  pub fn request_microphone(&self, audible: bool) {
    if !self.ready.load(Ordering::SeqCst)
      || self.stop_requested()
      || !matches!(
        self.microphone(),
        Some(
          crate::video::controller::MicrophoneState::Audible
            | crate::video::controller::MicrophoneState::Muted
        )
      )
      || self
        .microphone_pending
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
      return;
    }
    if self.microphone_tx.send(Some(audible)).is_err() {
      self.microphone_pending.store(false, Ordering::SeqCst);
      tracing::error!(audible, "Microphone request had no active receiver");
    }
  }

  /// The recorder's microphone state; a switch in flight keeps the controls as they are.
  pub fn sync_microphone(&self, state: Option<crate::video::controller::MicrophoneState>) {
    use crate::video::controller::MicrophoneState::*;
    self.microphone.store(
      match state {
        None => 0,
        Some(Audible) => 1,
        Some(Muted) => 2,
        Some(Lost) => 3,
      },
      Ordering::SeqCst,
    );
  }

  /// A microphone switch finished (or failed): the next sync shows the recorder's state.
  pub fn finish_microphone(&self, state: Option<crate::video::controller::MicrophoneState>) {
    self.sync_microphone(state);
    self.microphone_pending.store(false, Ordering::SeqCst);
  }

  pub fn microphone(&self) -> Option<crate::video::controller::MicrophoneState> {
    use crate::video::controller::MicrophoneState::*;
    match self.microphone.load(Ordering::SeqCst) {
      1 => Some(Audible),
      2 => Some(Muted),
      3 => Some(Lost),
      _ => None,
    }
  }

  pub fn microphone_pending(&self) -> bool {
    self.microphone_pending.load(Ordering::SeqCst)
  }

  pub fn subscribe_microphone(&self) -> tokio::sync::watch::Receiver<Option<bool>> {
    self.microphone_tx.subscribe()
  }

  pub fn sync_paused(&self, paused: bool) {
    self.pause_flag.store(paused, Ordering::SeqCst);
    self.pause_pending.store(false, Ordering::SeqCst);
    self.pause_failed.store(false, Ordering::SeqCst);
  }

  pub fn fail_pause(&self) {
    self.pause_failed.store(true, Ordering::SeqCst);
    self.pause_pending.store(false, Ordering::SeqCst);
    self.stop_flag.store(true, Ordering::SeqCst);
  }

  pub fn sync_recording(&self, elapsed_ms: u64, paused: bool) {
    self.elapsed_ms.store(elapsed_ms, Ordering::SeqCst);
    self.pause_flag.store(paused, Ordering::SeqCst);
    self.ready.store(true, Ordering::SeqCst);
  }

  pub fn sync_stopped(&self) {
    self.stop_flag.store(true, Ordering::SeqCst);
  }

  pub fn stop_requested(&self) -> bool {
    self.stop_flag.load(Ordering::SeqCst)
  }

  pub fn claim_stop(&self) -> bool {
    self.stop_requested()
      && self
        .stop_claimed
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
  }

  pub fn paused(&self) -> bool {
    self.pause_flag.load(Ordering::SeqCst)
  }

  pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<RecordingControlEvent>> {
    self.event_tx.subscribe()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveRecordingKind {
  Video,
  Gif,
}

#[cfg(test)]
mod recording_control_tests {
  use super::*;

  #[test]
  fn microphone_switches_need_a_usable_microphone_and_one_at_a_time() {
    use crate::video::controller::MicrophoneState::*;
    let controls = RecordingControls::new();
    let mut requests = controls.subscribe_microphone();
    controls.request_microphone(false);
    assert!(!controls.microphone_pending(), "not ready yet");
    controls.sync_recording(0, false);
    controls.request_microphone(false);
    assert!(!controls.microphone_pending(), "no microphone");
    controls.sync_microphone(Some(Audible));
    controls.request_microphone(false);
    assert!(controls.microphone_pending());
    assert_eq!(*requests.borrow_and_update(), Some(false));
    controls.request_microphone(true);
    assert!(
      !requests.has_changed().unwrap_or(true),
      "one switch in flight"
    );
    controls.finish_microphone(Some(Muted));
    assert!(!controls.microphone_pending());
    assert_eq!(controls.microphone(), Some(Muted));
    controls.sync_microphone(Some(Lost));
    controls.request_microphone(true);
    assert!(
      !controls.microphone_pending(),
      "a lost microphone cannot switch"
    );
    // A pause request is never replaced by a microphone switch.
    let mut events = controls.subscribe();
    controls.sync_microphone(Some(Audible));
    controls.request_pause(true);
    controls.request_microphone(false);
    assert_eq!(
      *events.borrow_and_update(),
      Some(RecordingControlEvent::PauseRequested(true))
    );
  }

  #[test]
  fn pause_is_acknowledged_only_after_recorder_completion() {
    let controls = RecordingControls::new();
    let mut receiver = controls.subscribe();
    controls.request_pause(true);
    assert!(!controls.pause_pending.load(Ordering::SeqCst));
    controls.sync_recording(1200, false);
    controls.request_pause(true);
    assert!(!controls.paused());
    assert!(controls.pause_pending.load(Ordering::SeqCst));
    assert_eq!(
      *receiver.borrow_and_update(),
      Some(RecordingControlEvent::PauseRequested(true))
    );
    controls.request_pause(false);
    assert!(!receiver.has_changed().expect("live receiver"));
    controls.sync_paused(true);
    assert!(controls.paused());
    assert!(!controls.pause_pending.load(Ordering::SeqCst));
    controls.request_pause(false);
    controls.fail_pause();
    assert!(controls.paused());
    assert!(controls.pause_failed.load(Ordering::SeqCst));
    assert!(!controls.pause_pending.load(Ordering::SeqCst));
    controls.request_pause(false);
    assert!(controls.pause_failed.load(Ordering::SeqCst));
    assert!(controls.stop_requested());
    assert!(!controls.pause_pending.load(Ordering::SeqCst));
  }

  #[test]
  fn disconnected_control_receiver_releases_pending_request() {
    let controls = RecordingControls::new();
    controls.sync_recording(0, false);
    controls.request_pause(true);
    assert!(!controls.paused());
    assert!(!controls.pause_pending.load(Ordering::SeqCst));
    assert!(controls.pause_failed.load(Ordering::SeqCst));
    assert!(controls.stop_requested());
    controls.request_stop();
    controls.request_pause(true);
    assert!(!controls.pause_pending.load(Ordering::SeqCst));
  }

  #[test]
  fn startup_and_supervisor_claim_a_stop_only_once() {
    let controls = RecordingControls::new();
    assert!(!controls.claim_stop());
    controls.request_stop();
    assert!(controls.claim_stop());
    assert!(!controls.claim_stop());
    controls.request_stop();
    assert!(!controls.claim_stop());
  }
}

#[derive(Debug, Clone, Copy)]
pub struct ActiveRecordingSession {
  pub kind: ActiveRecordingKind,
  pub paused: bool,
  pub presentation_mode: crate::recording_visibility::RecordingPresentationMode,
}

#[derive(Default)]
pub struct RecordingSessionState(pub Mutex<Option<ActiveRecordingSession>>);

impl RecordingSessionState {
  pub fn set(
    &self,
    kind: ActiveRecordingKind,
    presentation_mode: crate::recording_visibility::RecordingPresentationMode,
  ) {
    if let Ok(mut guard) = self.0.lock() {
      *guard = Some(ActiveRecordingSession {
        kind,
        paused: false,
        presentation_mode,
      });
    }
  }

  pub fn set_paused(&self, paused: bool) {
    if let Ok(mut guard) = self.0.lock()
      && let Some(session) = guard.as_mut()
    {
      session.paused = paused;
    }
  }

  pub fn clear(&self) {
    if let Ok(mut guard) = self.0.lock() {
      *guard = None;
    }
  }

  pub fn snapshot(&self) -> Option<ActiveRecordingSession> {
    self.0.lock().ok().and_then(|guard| *guard)
  }
}

pub fn current_recording_session(app: &AppHandle) -> Option<ActiveRecordingSession> {
  let session = app
    .try_state::<RecordingSessionState>()
    .and_then(|state| state.snapshot())?;

  let recorder_status = app
    .try_state::<VideoRecorderState>()
    .and_then(|recorder_state| match recorder_state.0.try_lock() {
      Ok(mut recorder) => Some(recorder.status()),
      Err(_) => None,
    });

  match (session.kind, recorder_status) {
    (
      ActiveRecordingKind::Video | ActiveRecordingKind::Gif,
      Some(
        crate::video::controller::RecordingStatus::Starting
        | crate::video::controller::RecordingStatus::Recording
        | crate::video::controller::RecordingStatus::Paused
        | crate::video::controller::RecordingStatus::Stopping,
      ),
    ) => Some(session),
    (
      ActiveRecordingKind::Video | ActiveRecordingKind::Gif,
      Some(
        crate::video::controller::RecordingStatus::Idle
        | crate::video::controller::RecordingStatus::Error,
      ),
    ) => {
      if let Some(state) = app.try_state::<RecordingSessionState>() {
        state.clear();
      }
      None
    },
    _ => Some(session),
  }
}

pub struct AppExitState {
  allow_exit: AtomicBool,
}

impl Default for AppExitState {
  fn default() -> Self {
    Self::new()
  }
}

impl AppExitState {
  pub fn new() -> Self {
    Self {
      allow_exit: AtomicBool::new(false),
    }
  }

  pub fn allow_exit(&self) {
    self.allow_exit.store(true, Ordering::SeqCst);
    tracing::info!(
      operation = "app_exit",
      step = "allow_exit",
      "Exit has been allowed (explicit quit requested)"
    );
  }

  pub fn should_exit(&self) -> bool {
    self.allow_exit.load(Ordering::SeqCst)
  }
}

pub struct OverlayActivityState {
  active_mask: AtomicU8,
}

impl Default for OverlayActivityState {
  fn default() -> Self {
    Self::new()
  }
}

impl OverlayActivityState {
  const SCREENSHOT_BIT: u8 = 0b0000_0001;
  const VIDEO_BIT: u8 = 0b0000_0010;
  const GIF_BIT: u8 = 0b0000_0100;

  pub fn new() -> Self {
    Self {
      active_mask: AtomicU8::new(0),
    }
  }

  pub fn try_claim_screenshot(&self) -> bool {
    self.try_claim(Self::SCREENSHOT_BIT)
  }

  pub fn release_screenshot(&self) {
    self.release(Self::SCREENSHOT_BIT);
  }

  pub fn try_claim_video_area_selection(&self) -> bool {
    self.try_claim(Self::VIDEO_BIT)
  }

  pub fn release_video_area_selection(&self) {
    self.release(Self::VIDEO_BIT);
  }

  pub fn try_claim_gif(&self) -> bool {
    self.try_claim(Self::GIF_BIT)
  }

  pub fn release_gif(&self) {
    self.release(Self::GIF_BIT);
  }

  pub fn is_screenshot_active(&self) -> bool {
    self.active_mask.load(Ordering::SeqCst) & Self::SCREENSHOT_BIT != 0
  }

  pub fn is_video_area_selection_active(&self) -> bool {
    self.active_mask.load(Ordering::SeqCst) & Self::VIDEO_BIT != 0
  }

  pub fn is_gif_active(&self) -> bool {
    self.active_mask.load(Ordering::SeqCst) & Self::GIF_BIT != 0
  }

  pub fn is_any_active(&self) -> bool {
    self.is_screenshot_active() || self.is_video_area_selection_active() || self.is_gif_active()
  }

  fn try_claim(&self, bit: u8) -> bool {
    let mut current = self.active_mask.load(Ordering::SeqCst);
    loop {
      if current != 0 {
        return false;
      }
      match self.active_mask.compare_exchange(
        current,
        current | bit,
        Ordering::SeqCst,
        Ordering::SeqCst,
      ) {
        Ok(_) => return true,
        Err(next) => current = next,
      }
    }
  }

  fn release(&self, bit: u8) {
    self.active_mask.fetch_and(!bit, Ordering::SeqCst);
  }
}

pub struct ClipboardState(pub Mutex<Clipboard>);

pub struct VideoRecorderState(
  pub Arc<Mutex<crate::video::controller::VideoRecorder>>,
  Arc<AtomicBool>,
);

pub(crate) struct RecordingOperation(Arc<AtomicBool>);
impl Drop for RecordingOperation {
  fn drop(&mut self) {
    self.0.store(false, Ordering::Release);
  }
}
impl VideoRecorderState {
  pub fn new(recorder: crate::video::controller::VideoRecorder) -> Self {
    Self(
      Arc::new(Mutex::new(recorder)),
      Arc::new(AtomicBool::new(false)),
    )
  }
  pub(crate) fn claim_operation(&self) -> Result<RecordingOperation, String> {
    self
      .1
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
      .map(|_| RecordingOperation(Arc::clone(&self.1)))
      .map_err(|_| "A recording operation is already in progress".into())
  }
}

/// Only blocking recording workers use this bounded acquisition. UI/status
/// queries use try_lock directly; transient monitor reads must not lose Stop.
pub(crate) fn lock_recorder(
  recorder: &Mutex<crate::video::controller::VideoRecorder>,
) -> Result<std::sync::MutexGuard<'_, crate::video::controller::VideoRecorder>, String> {
  let started = std::time::Instant::now();
  loop {
    match recorder.try_lock() {
      Ok(guard) => return Ok(guard),
      Err(std::sync::TryLockError::Poisoned(error)) => {
        return Err(crate::commands::ipc_display_context(
          "Recorder lock poisoned",
          error,
        ));
      },
      Err(std::sync::TryLockError::WouldBlock) => {
        if started.elapsed() >= std::time::Duration::from_secs(5) {
          return Err("Recorder operation exceeded its five-second lock deadline".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
      },
    }
  }
}

pub struct ConfigState(pub Mutex<crate::config::Config>);

impl ConfigState {
  pub fn new(config: crate::config::Config) -> Self {
    Self(Mutex::new(config))
  }

  pub fn snapshot(&self) -> crate::config::Config {
    self
      .0
      .lock()
      .map(|guard| guard.clone())
      .unwrap_or_else(|_| crate::config::Config::default())
  }

  pub fn replace(&self, config: crate::config::Config) {
    if let Ok(mut guard) = self.0.lock() {
      *guard = config;
    }
  }
}

#[derive(Default)]
pub struct RecordingHudState {
  pub controls: Mutex<Option<Arc<RecordingControls>>>,
}

impl RecordingHudState {
  pub fn new() -> Self {
    Self {
      controls: Mutex::new(None),
    }
  }

  pub fn set(&self, controls: Arc<RecordingControls>) {
    if let Ok(mut guard) = self.controls.lock() {
      *guard = Some(controls);
    }
  }

  pub fn signal_stop(&self) {
    if let Ok(guard) = self.controls.lock()
      && let Some(controls) = guard.as_ref()
    {
      controls.request_stop();
    }
  }

  pub fn sync_paused(&self, paused: bool) {
    if let Ok(guard) = self.controls.lock()
      && let Some(controls) = guard.as_ref()
    {
      controls.sync_paused(paused);
    }
  }

  pub fn sync_stopped(&self) {
    if let Ok(guard) = self.controls.lock()
      && let Some(controls) = guard.as_ref()
    {
      controls.sync_stopped();
    }
  }

  pub fn clear(&self) {
    if let Ok(mut guard) = self.controls.lock() {
      *guard = None;
    }
  }
}

pub struct LanguageState(pub Mutex<String>);
