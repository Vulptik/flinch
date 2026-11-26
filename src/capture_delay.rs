//! Capture Delay: an optional countdown before every capture.
//!
//! Freeze captures (screenshots, Capture and Edit, Copy Text, Color Picker) wait before the
//! desktop is frozen, so a menu or tooltip opened during the wait is in the frame. GIFs and
//! videos wait after their area is picked, so recording starts when the user is ready.
//!
//! One countdown runs at a time. While it runs, the same capture's shortcut or tray row skips the
//! wait, Esc cancels it, and any other capture request is ignored. The countdown thread owns its
//! pill window and its Esc registration and ends within the delay, or sooner when signalled.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use crate::app::context::AppHandle;

/// The delays offered in Settings and the tray, in seconds; 0 turns the delay off.
pub const CHOICES: [u32; 4] = [0, 3, 5, 10];

/// How often the countdown thread wakes to pump its window's messages and read signals.
const SLICE: Duration = Duration::from_millis(50);

/// Which capture a countdown belongs to, so pressing its own shortcut again can skip the wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
  Area,
  CaptureEdit,
  CopyText,
  PickColor,
  LastRegion,
  Fullscreen,
  ActiveWindow,
  RecordArea,
  RecordFullscreen,
  RecordActiveWindow,
  Gif,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// The delay is off; the capture starts at once.
  Immediate,
  /// The countdown ran to the end.
  Elapsed,
  /// The capture's own shortcut was pressed again during the countdown.
  Skipped,
  /// Esc cancelled the countdown; the capture must not start.
  Cancelled,
}

impl Outcome {
  pub fn proceeds(self) -> bool {
    self != Self::Cancelled
  }

  /// Whether time passed before the capture, which speed timings must leave out.
  pub fn waited(self) -> bool {
    matches!(self, Self::Elapsed | Self::Skipped)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signal {
  Skip,
  Cancel,
}

/// The visible countdown. Tests use a recording fake; production draws the pill.
pub(crate) trait Presenter {
  /// Show the seconds left. A failure is logged once and the countdown keeps its timing.
  fn show(&mut self, remaining: u32) -> Result<(), String>;
  /// Handle pending window messages so Windows never treats the pill as unresponsive.
  fn pump(&mut self) {}
}

struct Active {
  trigger: Trigger,
  signals: SyncSender<Signal>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

fn active() -> std::sync::MutexGuard<'static, Option<Active>> {
  ACTIVE.lock().unwrap_or_else(|poisoned| {
    tracing::error!(
      operation = "capture_delay",
      "Countdown state poisoned; recovering"
    );
    poisoned.into_inner()
  })
}

fn send(signals: &SyncSender<Signal>, signal: Signal) {
  match signals.try_send(signal) {
    Ok(()) => {},
    // A full queue already holds a signal that ends the countdown.
    Err(TrySendError::Full(_)) => {},
    Err(TrySendError::Disconnected(_)) => {
      tracing::debug!(
        operation = "capture_delay",
        ?signal,
        "Countdown already ended"
      );
    },
  }
}

/// Called first by every capture shortcut and tray row. While a countdown runs it absorbs the
/// request: its own capture skips the wait, anything else is ignored. Returns whether it did.
pub fn intercept(trigger: Trigger) -> bool {
  let active = active();
  let Some(running) = active.as_ref() else {
    return false;
  };
  if running.trigger == trigger {
    tracing::info!(operation = "capture_delay", ?trigger, "Countdown skipped");
    send(&running.signals, Signal::Skip);
  } else {
    tracing::debug!(
      operation = "capture_delay",
      ?trigger,
      running = ?running.trigger,
      "Capture ignored during another capture's countdown"
    );
  }
  true
}

/// Waits the configured delay before a capture. Returns at once when the delay is off.
pub async fn wait(app: &AppHandle, trigger: Trigger) -> Outcome {
  let seconds = crate::config::Config::current(app).capture_delay;
  if seconds == 0 {
    return Outcome::Immediate;
  }
  let app = app.clone();
  match tokio::task::spawn_blocking(move || wait_blocking_for(&app, trigger, seconds, pill)).await {
    Ok(outcome) => outcome,
    Err(error) => {
      tracing::error!(operation = "capture_delay", %error, "Countdown task failed; cancelling the capture");
      Outcome::Cancelled
    },
  }
}

/// `wait` for callers already on a blocking thread, such as the GIF overlay worker.
pub fn wait_blocking(app: &AppHandle, trigger: Trigger) -> Outcome {
  let seconds = crate::config::Config::current(app).capture_delay;
  if seconds == 0 {
    return Outcome::Immediate;
  }
  wait_blocking_for(app, trigger, seconds, pill)
}

/// The countdown pill on the pointer's monitor.
fn pill(lang: &str) -> Result<Box<dyn Presenter>, String> {
  crate::overlay::countdown::Pill::new(lang).map(|pill| Box::new(pill) as Box<dyn Presenter>)
}

fn wait_blocking_for(
  app: &AppHandle,
  trigger: Trigger,
  seconds: u32,
  presenter: fn(&str) -> Result<Box<dyn Presenter>, String>,
) -> Outcome {
  let seconds = seconds.min(*CHOICES.last().unwrap_or(&10));
  let (signals, receiver) = std::sync::mpsc::sync_channel(4);
  {
    let mut active = active();
    if let Some(running) = active.as_ref() {
      tracing::warn!(
        operation = "capture_delay",
        ?trigger,
        running = ?running.trigger,
        "A countdown is already running; this capture is cancelled"
      );
      return Outcome::Cancelled;
    }
    *active = Some(Active {
      trigger,
      signals: signals.clone(),
    });
  }
  let _clear = ClearActive;
  let _escape = EscapeGuard::register(app, signals);
  let lang = crate::i18n::get_lang(app);
  let outcome = match presenter(&lang) {
    Ok(mut pill) => run(seconds, Duration::from_secs(1), &receiver, pill.as_mut()),
    Err(error) => {
      tracing::error!(operation = "capture_delay", %error, "Countdown pill unavailable; waiting without it");
      run(seconds, Duration::from_secs(1), &receiver, &mut Hidden)
    },
  };
  tracing::info!(
    operation = "capture_delay",
    ?trigger,
    seconds,
    ?outcome,
    "Countdown ended"
  );
  outcome
}

struct ClearActive;
impl Drop for ClearActive {
  fn drop(&mut self) {
    *active() = None;
  }
}

struct Hidden;
impl Presenter for Hidden {
  fn show(&mut self, _remaining: u32) -> Result<(), String> {
    Ok(())
  }
}

/// Counts `seconds` ticks down, showing whole seconds left, until the end or a signal.
pub(crate) fn run(
  seconds: u32,
  tick: Duration,
  signals: &Receiver<Signal>,
  presenter: &mut dyn Presenter,
) -> Outcome {
  let started = Instant::now();
  let deadline = started + tick * seconds;
  let mut shown = None;
  let mut failed = false;
  loop {
    let now = Instant::now();
    if now >= deadline {
      return Outcome::Elapsed;
    }
    let left = deadline - now;
    let remaining = left.as_nanos().div_ceil(tick.as_nanos().max(1)) as u32;
    if shown != Some(remaining) {
      shown = Some(remaining);
      if let Err(error) = presenter.show(remaining)
        && !failed
      {
        failed = true;
        tracing::error!(operation = "capture_delay", %error, "Countdown pill could not be drawn");
      }
    }
    presenter.pump();
    match signals.recv_timeout(SLICE.min(left)) {
      Ok(Signal::Skip) => return Outcome::Skipped,
      Ok(Signal::Cancel) => return Outcome::Cancelled,
      Err(RecvTimeoutError::Timeout) => {},
      // Every sender is gone, so nothing can skip or cancel: finish the wait as set.
      Err(RecvTimeoutError::Disconnected) => {
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        return Outcome::Elapsed;
      },
    }
  }
}

/// Esc cancels the countdown. It is a global shortcut only while the countdown runs, since the
/// pill never takes focus: focusing it would close the very menu the delay is for.
struct EscapeGuard {
  app: AppHandle,
  registered: bool,
}

impl EscapeGuard {
  fn register(app: &AppHandle, signals: SyncSender<Signal>) -> Self {
    use crate::app::hotkeys::GlobalShortcutExt;
    let registered = app
      .global_shortcut()
      .on_shortcut("Escape", move |_app, _shortcut, event| {
        if event.state == global_hotkey::HotKeyState::Pressed {
          send(&signals, Signal::Cancel);
        }
      });
    if let Err(error) = &registered {
      tracing::error!(
        operation = "capture_delay",
        %error,
        "Esc could not be registered; the countdown can still be skipped or waited out"
      );
    }
    Self {
      app: app.clone(),
      registered: registered.is_ok(),
    }
  }
}

impl Drop for EscapeGuard {
  fn drop(&mut self) {
    use crate::app::hotkeys::GlobalShortcutExt;
    if self.registered
      && let Err(error) = self.app.global_shortcut().unregister("Escape")
    {
      tracing::error!(operation = "capture_delay", %error, "Esc stayed registered after the countdown");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Default)]
  struct Recorder {
    shown: Vec<u32>,
    pumps: usize,
    fail: bool,
  }

  impl Presenter for Recorder {
    fn show(&mut self, remaining: u32) -> Result<(), String> {
      self.shown.push(remaining);
      if self.fail {
        Err("pill window gone".into())
      } else {
        Ok(())
      }
    }

    fn pump(&mut self) {
      self.pumps += 1;
    }
  }

  /// Tests that start a countdown share the one-countdown-at-a-time slot, so they take turns.
  static TURN: Mutex<()> = Mutex::new(());

  fn turn() -> std::sync::MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
  }

  fn app(delay: u32) -> AppHandle {
    AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        capture_delay: delay,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    )
  }

  fn hidden(_lang: &str) -> Result<Box<dyn Presenter>, String> {
    Ok(Box::new(Hidden))
  }

  fn broken(_lang: &str) -> Result<Box<dyn Presenter>, String> {
    Err("no monitor".into())
  }

  fn escape() -> crate::app::hotkeys::Shortcut {
    "Escape".parse().expect("Esc")
  }

  /// Waits until a countdown holds the slot.
  fn running() {
    let deadline = Instant::now() + Duration::from_secs(5);
    while active().is_none() && Instant::now() < deadline {
      std::thread::sleep(Duration::from_millis(5));
    }
  }

  /// Runs a countdown on its own thread and skips it once it is running.
  fn skipped(
    app: &AppHandle,
    presenter: fn(&str) -> Result<Box<dyn Presenter>, String>,
  ) -> Outcome {
    let thread_app = app.clone();
    let countdown =
      std::thread::spawn(move || wait_blocking_for(&thread_app, Trigger::Gif, 10, presenter));
    running();
    assert!(intercept(Trigger::Gif));
    countdown.join().expect("countdown thread")
  }

  #[test]
  fn an_off_delay_starts_at_once() {
    let app = app(0);
    assert_eq!(wait_blocking(&app, Trigger::Area), Outcome::Immediate);
    let runtime = tokio::runtime::Builder::new_current_thread()
      .build()
      .expect("runtime");
    assert_eq!(
      runtime.block_on(wait(&app, Trigger::Area)),
      Outcome::Immediate
    );
  }

  #[test]
  fn a_countdown_holds_esc_only_while_it_runs_and_one_runs_at_a_time() {
    let _turn = turn();
    let app = app(10);
    let thread_app = app.clone();
    let countdown =
      std::thread::spawn(move || wait_blocking_for(&thread_app, Trigger::Gif, 30, hidden));
    running();
    // Esc is registered for the countdown and released after it.
    let shortcuts = app.test_hotkeys().expect("test hotkeys");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !shortcuts.calls().contains(&escape()) && Instant::now() < deadline {
      std::thread::sleep(Duration::from_millis(5));
    }
    assert!(shortcuts.calls().contains(&escape()));
    // A second countdown meanwhile is refused rather than queued.
    assert_eq!(
      wait_blocking_for(&app, Trigger::Area, 3, hidden),
      Outcome::Cancelled
    );
    assert!(intercept(Trigger::Gif));
    assert_eq!(countdown.join().expect("countdown"), Outcome::Skipped);
    use crate::app::hotkeys::GlobalShortcutExt;
    assert_eq!(app.global_shortcut().is_registered("Escape"), Ok(false));
    assert!(!intercept(Trigger::Gif), "the slot is free again");
  }

  #[test]
  fn a_missing_pill_or_esc_never_stops_the_countdown() {
    let _turn = turn();
    let app = app(10);
    assert_eq!(skipped(&app, broken), Outcome::Skipped);
    app
      .test_hotkeys()
      .expect("test hotkeys")
      .reject(escape(), "Esc is taken");
    assert_eq!(skipped(&app, hidden), Outcome::Skipped);
    let mut pill = Hidden;
    assert_eq!(pill.show(3), Ok(()));
    pill.pump();
  }

  /// Long enough that a busy test machine never stalls a whole tick between two 50 ms slices.
  const TICK: Duration = Duration::from_millis(400);

  /// Shown seconds count down from the delay to 1 without repeats; a stalled slice may skip one.
  fn counts_down(shown: &[u32], from: u32) -> bool {
    shown.first() == Some(&from)
      && shown.last() == Some(&1)
      && shown.windows(2).all(|pair| pair[0] > pair[1])
  }

  #[test]
  fn a_countdown_shows_every_second_once_and_ends_on_time() {
    let (_signals, receiver) = std::sync::mpsc::sync_channel(4);
    let mut pill = Recorder::default();
    let started = Instant::now();
    assert_eq!(run(3, TICK, &receiver, &mut pill), Outcome::Elapsed);
    let took = started.elapsed();
    assert!(took >= TICK * 3, "ended early after {took:?}");
    assert!(
      took < TICK * 3 + Duration::from_secs(1),
      "ended late after {took:?}"
    );
    assert!(counts_down(&pill.shown, 3), "{:?}", pill.shown);
    assert!(pill.pumps >= 3, "the window is pumped while waiting");
  }

  #[test]
  fn skip_starts_at_once_and_cancel_stops_the_capture() {
    for (signal, expected) in [
      (Signal::Skip, Outcome::Skipped),
      (Signal::Cancel, Outcome::Cancelled),
    ] {
      let (signals, receiver) = std::sync::mpsc::sync_channel(4);
      signals.send(signal).unwrap();
      let started = Instant::now();
      assert_eq!(
        run(
          10,
          Duration::from_secs(1),
          &receiver,
          &mut Recorder::default()
        ),
        expected
      );
      assert!(started.elapsed() < Duration::from_millis(500));
    }
    assert!(Outcome::Skipped.proceeds() && Outcome::Skipped.waited());
    assert!(!Outcome::Cancelled.proceeds());
    assert!(Outcome::Immediate.proceeds() && !Outcome::Immediate.waited());
  }

  #[test]
  fn a_failed_pill_or_lost_signals_still_keep_the_chosen_delay() {
    let (signals, receiver) = std::sync::mpsc::sync_channel(4);
    let mut pill = Recorder {
      fail: true,
      ..Recorder::default()
    };
    assert_eq!(run(2, TICK, &receiver, &mut pill), Outcome::Elapsed);
    assert!(
      counts_down(&pill.shown, 2),
      "it keeps trying each second: {:?}",
      pill.shown
    );
    drop(signals);
    let started = Instant::now();
    assert_eq!(
      run(2, TICK, &receiver, &mut Recorder::default()),
      Outcome::Elapsed
    );
    assert!(started.elapsed() >= TICK * 2);
  }

  #[test]
  fn requests_during_a_countdown_skip_their_own_wait_and_leave_others_alone() {
    let _turn = turn();
    assert!(!intercept(Trigger::Area), "nothing to intercept when idle");
    let (signals, receiver) = std::sync::mpsc::sync_channel(4);
    *active() = Some(Active {
      trigger: Trigger::RecordArea,
      signals,
    });
    assert!(intercept(Trigger::Area), "another capture is absorbed");
    assert!(receiver.try_recv().is_err(), "and does not skip the wait");
    assert!(intercept(Trigger::RecordArea));
    assert_eq!(receiver.try_recv(), Ok(Signal::Skip));
    for _ in 0..8 {
      assert!(
        intercept(Trigger::RecordArea),
        "repeats never block on a full queue"
      );
    }
    drop(ClearActive);
    assert!(!intercept(Trigger::RecordArea));
  }
}
