//! The Videos page's Microphone picker and level meter. Microphones are listed off the UI
//! thread every few seconds, and the meter opens the chosen microphone only while the page shows
//! with Record Microphone on, so nothing listens while Settings is elsewhere or closed.
use std::{
  cell::{Cell, RefCell},
  time::{Duration, Instant},
};

use flinch_settings::{
  microphones,
  slint::{self, ComponentHandle, Model},
  ui::{self, SettingsState},
};
use flinch_video_engine::{
  EngineError,
  types::{MICROPHONE_BLOCKED_STAGE, MICROPHONE_MISSING_STAGE},
  windows::audio::devices::{self, LevelMeter},
};

const VIDEOS_PAGE: i32 = 4;
const TICK: Duration = Duration::from_millis(50);
/// How often the list is read again, so a microphone plugged in shows up on its own.
const LIST_EVERY: Duration = Duration::from_secs(3);
/// How long a microphone that could not be opened waits before the meter tries again.
const RETRY_AFTER: Duration = Duration::from_secs(3);

/// An open level meter: the loudest sample since the last read, and why it stopped, once.
trait Meter: Send {
  fn take_peak(&self) -> f32;
  fn failure(&self) -> Option<EngineError>;
}

impl Meter for LevelMeter {
  fn take_peak(&self) -> f32 {
    LevelMeter::take_peak(self)
  }
  fn failure(&self) -> Option<EngineError> {
    LevelMeter::failure(self)
  }
}

type Listing = Result<Vec<devices::Microphone>, EngineError>;
type Opening = Result<Box<dyn Meter>, EngineError>;

/// Where microphones come from: Windows, or scripted ones in quiet tests.
#[derive(Clone, Copy)]
struct Devices {
  list: fn() -> Listing,
  open: fn(Option<String>) -> Opening,
}

fn open_level_meter(device: Option<String>) -> Opening {
  LevelMeter::open(device).map(|meter| Box::new(meter) as Box<dyn Meter>)
}

const WINDOWS: Devices = Devices {
  list: devices::microphones,
  open: open_level_meter,
};

#[derive(Default)]
struct State {
  generation: u64,
  timer: Option<slint::Timer>,
  meter: Option<Box<dyn Meter>>,
  /// The microphone the meter is for (opening or open); `Some(None)` is the Windows default.
  meter_for: Option<Option<String>>,
  opening: bool,
  level: f32,
  problem: String,
  retry_at: Option<Instant>,
  listing: bool,
  listed_at: Option<Instant>,
  available: Vec<(String, String)>,
}

thread_local! {
  static STATE: RefCell<State> = RefCell::new(State::default());
  static DEVICES: Cell<Devices> = const { Cell::new(WINDOWS) };
}

/// Starts watching for the presenter of `generation`; a later generation replaces it.
pub(super) fn attach(generation: u64) {
  STATE.with(|state| {
    let mut state = state.borrow_mut();
    release(&mut state);
    *state = State {
      generation,
      ..State::default()
    };
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, TICK, move || tick(generation));
    state.timer = Some(timer);
  });
}

/// Closes the meter off the UI thread: its capture shutdown is bounded but not instant.
fn release(state: &mut State) {
  state.meter_for = None;
  state.opening = false;
  state.level = 0.0;
  state.problem.clear();
  state.retry_at = None;
  if let Some(meter) = state.meter.take()
    && let Err(error) = std::thread::Builder::new()
      .name("microphone level close".into())
      .spawn(move || drop(meter))
  {
    tracing::error!(%error, "Cannot close the microphone level meter off the UI thread");
  }
}

fn selected(state: &SettingsState) -> Option<String> {
  let index = usize::try_from(state.get_microphone_index()).ok()?;
  let id = state.get_microphone_ids().row_data(index)?;
  (!id.is_empty()).then(|| id.to_string())
}

fn tick(generation: u64) {
  let Some((window, controller)) = super::native_host::matching(generation) else {
    STATE.with(|state| {
      let mut state = state.borrow_mut();
      if state.generation == generation {
        release(&mut state);
        state.timer = None;
      }
    });
    return;
  };
  let settings = window.global::<SettingsState>();
  let wanted =
    (window.get_page() == VIDEOS_PAGE && settings.get_microphone() && window.window().is_visible())
      .then(|| selected(&settings));
  let lang = crate::i18n::get_lang(&controller.borrow().app);
  let mut list = false;
  let mut open = None;
  let (level, problem) = STATE.with(|state| {
    let mut state = state.borrow_mut();
    if wanted.is_none() {
      if state.meter_for.is_some() || state.meter.is_some() {
        release(&mut state);
      }
      return (0.0, String::new());
    }
    if !state.listing
      && state
        .listed_at
        .is_none_or(|listed| listed.elapsed() >= LIST_EVERY)
    {
      state.listing = true;
      list = true;
    }
    if state.meter_for != wanted && !state.opening {
      release(&mut state);
      state.meter_for = wanted.clone();
      state.opening = true;
      open = wanted.clone();
    }
    let reading = state
      .meter
      .as_ref()
      .map(|meter| (meter.take_peak(), meter.failure()));
    if let Some((peak, failure)) = reading {
      state.level = microphones::bar(state.level, peak);
      if let Some(error) = failure {
        tracing::warn!(error = %error, "Microphone level meter stopped");
        let problem = problem_text(&lang, &error);
        release(&mut state);
        state.meter_for = wanted.clone();
        state.problem = problem;
        state.retry_at = Some(Instant::now() + RETRY_AFTER);
      }
    } else if state.retry_at.is_some_and(|retry| Instant::now() >= retry) && !state.opening {
      // Try again: the microphone may have been plugged in or allowed meanwhile. Its reason
      // stays shown until it opens, rather than blinking out at every attempt.
      state.retry_at = None;
      state.opening = true;
      open = wanted.clone();
    }
    (state.level, state.problem.clone())
  });
  ui::set_microphone_level(&window, level, &problem);
  let devices = DEVICES.with(Cell::get);
  if list {
    spawn("microphone list", move || {
      let result = (devices.list)();
      deliver(move || listed(generation, result));
    });
  }
  if let Some(device) = open {
    spawn("microphone level open", move || {
      let result = (devices.open)(device.clone());
      deliver(move || opened(generation, device, result));
    });
  }
}

fn spawn(name: &str, work: impl FnOnce() + Send + 'static) {
  if let Err(error) = std::thread::Builder::new().name(name.into()).spawn(work) {
    tracing::error!(%error, name, "Cannot start a microphone settings thread");
  }
}

fn deliver(result: impl FnOnce() + Send + 'static) {
  if let Err(error) = slint::invoke_from_event_loop(result) {
    tracing::error!(%error, "Cannot deliver a microphone settings result");
  }
}

fn listed(generation: u64, result: Listing) {
  let Some((window, _)) = super::native_host::matching(generation) else {
    return;
  };
  let available = STATE.with(|state| {
    let mut state = state.borrow_mut();
    state.listing = false;
    state.listed_at = Some(Instant::now());
    match result {
      Ok(list) => {
        let available: Vec<_> = list.into_iter().map(|mic| (mic.id, mic.name)).collect();
        let changed = state.available != available;
        state.available = available;
        changed.then(|| state.available.clone())
      },
      Err(error) => {
        tracing::warn!(error = %error, "Cannot list microphones");
        None
      },
    }
  });
  if let Some(available) = available
    && let Err(error) = ui::refresh_microphones(&window, &available)
  {
    tracing::error!(%error, "Cannot show the microphone list");
  }
}

fn opened(generation: u64, device: Option<String>, result: Opening) {
  let lang = super::native_host::matching(generation)
    .map(|(_, controller)| crate::i18n::get_lang(&controller.borrow().app));
  STATE.with(|state| {
    let mut state = state.borrow_mut();
    let current = state.generation == generation && state.meter_for.as_ref() == Some(&device);
    if !current || lang.is_none() {
      if let Ok(meter) = result {
        // Superseded by another choice or a closed window: never keep it open.
        let mut stale = State {
          meter: Some(meter),
          ..State::default()
        };
        release(&mut stale);
      }
      if state.generation == generation {
        state.opening = false;
      }
      return;
    }
    state.opening = false;
    match result {
      Ok(meter) => {
        state.meter = Some(meter);
        state.problem.clear();
      },
      Err(error) => {
        tracing::warn!(error = %error, "Cannot open the microphone level meter");
        state.problem = problem_text(lang.as_deref().unwrap_or("en"), &error);
        state.retry_at = Some(Instant::now() + RETRY_AFTER);
      },
    }
  });
}

/// The plain-language reason beside the meter; the full cause stays in the log.
fn problem_text(lang: &str, error: &EngineError) -> String {
  let key = problem_key(error);
  crate::i18n::native_settings(lang, key).unwrap_or_else(|failure| {
    tracing::error!(%failure, key, "Microphone problem localization failed");
    String::new()
  })
}

fn problem_key(error: &EngineError) -> &'static str {
  if error.stage == MICROPHONE_BLOCKED_STAGE {
    "videos_microphone_blocked"
  } else if error.stage == MICROPHONE_MISSING_STAGE {
    "videos_microphone_missing"
  } else {
    "videos_microphone_failed"
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::app::headless::{advance, drain_until};
  use flinch_settings::ui::{Interaction, SettingsWindow};
  use flinch_video_engine::ErrorKind;
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

  /// The scripted microphones: what Windows lists, what opening one does and what it hears.
  static LISTED: Mutex<Option<Listing>> = Mutex::new(None);
  static OPENED: Mutex<Vec<Option<String>>> = Mutex::new(Vec::new());
  static OPEN_ERROR: Mutex<Option<EngineError>> = Mutex::new(None);
  static PEAK: AtomicU32 = AtomicU32::new(0);
  static FAILURE: Mutex<Option<EngineError>> = Mutex::new(None);
  static CLOSED: AtomicUsize = AtomicUsize::new(0);

  fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }

  struct Scripted;
  impl Meter for Scripted {
    fn take_peak(&self) -> f32 {
      f32::from_bits(PEAK.swap(0, Ordering::AcqRel))
    }
    fn failure(&self) -> Option<EngineError> {
      lock(&FAILURE).take()
    }
  }
  impl Drop for Scripted {
    fn drop(&mut self) {
      CLOSED.fetch_add(1, Ordering::AcqRel);
    }
  }

  fn scripted_list() -> Listing {
    lock(&LISTED).take().unwrap_or_else(|| Ok(Vec::new()))
  }

  fn scripted_open(device: Option<String>) -> Opening {
    lock(&OPENED).push(device);
    match lock(&OPEN_ERROR).take() {
      Some(error) => Err(error),
      None => Ok(Box::new(Scripted)),
    }
  }

  fn microphones(names: &[(&str, &str)]) -> Listing {
    Ok(
      names
        .iter()
        .map(|(id, name)| devices::Microphone {
          id: (*id).into(),
          name: (*name).into(),
        })
        .collect(),
    )
  }

  fn failed(stage: &str) -> EngineError {
    EngineError::new(ErrorKind::Capability, stage, "cause")
  }

  fn opens() -> usize {
    lock(&OPENED).len()
  }

  fn metering() -> bool {
    STATE.with(|state| state.borrow().meter.is_some())
  }

  /// One meter tick, then every worker result it started.
  fn frame() {
    advance(TICK.as_millis() as u64);
  }

  fn text(key: &str) -> String {
    crate::i18n::native_settings("en", key).expect("copy")
  }

  /// The meter opens only on a visible Videos page with Record Microphone on, follows the
  /// chosen microphone, shows a plain reason when one fails and tries again, and closes when the
  /// page or window goes.
  #[test]
  fn the_level_meter_listens_only_while_the_videos_page_shows_and_recovers_from_failures() {
    crate::app::headless::run(meter_scenario);
  }

  fn meter_scenario() {
    DEVICES.with(|devices| {
      devices.set(Devices {
        list: scripted_list,
        open: scripted_open,
      })
    });
    let app = crate::app::context::AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        check_for_updates: false,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let window = SettingsWindow::new().expect("headless settings");
    let generation = 4_242;
    super::super::native_host::connect_presenter(&window, app, generation).expect("presenter");
    window.global::<Interaction>().set_editable(true);
    window.show().expect("offscreen window");
    let settings = window.global::<SettingsState>();

    // Elsewhere in Settings nothing listens.
    frame();
    assert_eq!(opens(), 0);

    *lock(&LISTED) = Some(microphones(&[("{a}", "Headset"), ("{b}", "Desk")]));
    window.set_page(VIDEOS_PAGE);
    settings.set_microphone(true);
    frame();
    drain_until(|| metering() && STATE.with(|state| state.borrow().listed_at.is_some()));
    assert_eq!(*lock(&OPENED), [None], "the Windows default first");
    assert_eq!(settings.get_microphone_options().row_count(), 3);

    // It shows what it hears.
    PEAK.store(0.8f32.to_bits(), Ordering::Release);
    frame();
    assert!(settings.get_microphone_level() > 0.0);

    // Another choice closes the first meter and opens that microphone.
    settings.set_microphone_index(2);
    frame();
    drain_until(|| opens() == 2 && metering());
    assert_eq!(lock(&OPENED).last(), Some(&Some("{b}".to_string())));
    drain_until(|| CLOSED.load(Ordering::Acquire) >= 1);

    // A failure closes it with a plain reason, then it tries again.
    *lock(&FAILURE) = Some(failed(MICROPHONE_BLOCKED_STAGE));
    frame();
    assert_eq!(
      settings.get_microphone_problem(),
      text("videos_microphone_blocked")
    );
    assert!(!metering());
    // Regression: each retry cleared the reason until the attempt ended, so it blinked out every
    // three seconds while the microphone stayed unavailable.
    STATE.with(|state| state.borrow_mut().retry_at = Some(Instant::now()));
    *lock(&OPEN_ERROR) = Some(failed(MICROPHONE_MISSING_STAGE));
    frame();
    assert_eq!(
      settings.get_microphone_problem(),
      text("videos_microphone_blocked"),
      "the reason stays while it tries again"
    );
    drain_until(|| opens() == 3 && !STATE.with(|state| state.borrow().opening));
    frame();
    assert_eq!(
      settings.get_microphone_problem(),
      text("videos_microphone_missing")
    );
    STATE.with(|state| state.borrow_mut().retry_at = Some(Instant::now()));
    frame();
    assert_eq!(
      settings.get_microphone_problem(),
      text("videos_microphone_missing")
    );
    drain_until(|| opens() == 4 && metering());
    frame();
    assert_eq!(
      settings.get_microphone_problem(),
      "",
      "cleared once it opens"
    );

    // A failed listing keeps the picker as it was.
    *lock(&LISTED) = Some(Err(failed("enumerate")));
    STATE.with(|state| state.borrow_mut().listed_at = Some(Instant::now() - LIST_EVERY));
    frame();
    drain_until(|| !STATE.with(|state| state.borrow().listing));
    assert_eq!(settings.get_microphone_options().row_count(), 3);

    // Results for another window, or a meter nobody wants any more, are closed at once.
    let closed = CLOSED.load(Ordering::Acquire);
    opened(generation + 1, None, Ok(Box::new(Scripted)));
    opened(generation, Some("{gone}".into()), Ok(Box::new(Scripted)));
    opened(generation, Some("{gone}".into()), Err(failed("start")));
    listed(generation + 1, microphones(&[]));
    drain_until(|| CLOSED.load(Ordering::Acquire) >= closed + 2);

    // Record Microphone off closes the meter; so does leaving the page.
    settings.set_microphone(false);
    frame();
    assert!(!metering());
    settings.set_microphone(true);
    frame();
    drain_until(|| opens() == 5 && metering());
    window.set_page(0);
    frame();
    assert!(!metering());

    // A replaced or closed window stops the meter's timer.
    attach(generation + 1);
    frame();
    assert!(STATE.with(|state| state.borrow().timer.is_none()));
    drop(window);
  }

  #[test]
  fn each_meter_problem_has_its_own_translated_explanation() {
    let failed = |stage: &str| EngineError::new(ErrorKind::Capability, stage, "cause");
    let keys = [
      problem_key(&failed(MICROPHONE_BLOCKED_STAGE)),
      problem_key(&failed(MICROPHONE_MISSING_STAGE)),
      problem_key(&failed("WASAPI start")),
    ];
    assert_eq!(
      keys,
      [
        "videos_microphone_blocked",
        "videos_microphone_missing",
        "videos_microphone_failed"
      ]
    );
    for lang in ["en", "fr", "de", "es", "it"] {
      let texts: std::collections::HashSet<_> = keys
        .iter()
        .map(|key| crate::i18n::native_settings(lang, key).expect("copy"))
        .collect();
      assert_eq!(texts.len(), 3, "{lang}");
    }
  }
}
