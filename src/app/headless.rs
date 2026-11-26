//! Headless Slint platform for app-side window tests: offscreen software adapters, a manual
//! clock for timers, and a bounded event queue the owner thread drains.
//!
//! Slint keeps its platform per thread but its event-loop proxy once per process, so every
//! suite runs through [`run`]: on its own owner thread, one suite at a time, sharing the proxy.
use flinch_settings::slint::{
  self,
  platform::{
    EventLoopProxy, Platform, PlatformError, WindowAdapter,
    software_renderer::MinimalSoftwareWindow,
  },
};
use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

type Event = Box<dyn FnOnce() + Send>;
static EVENTS: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());
/// Slint accepts one proxy per process; the first installed platform hands it over.
static PROXY_CLAIMED: AtomicBool = AtomicBool::new(false);
static SUITE: Mutex<()> = Mutex::new(());
thread_local! {
  static CLOCK: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

struct Proxy;
impl EventLoopProxy for Proxy {
  fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
    Ok(())
  }
  fn invoke_from_event_loop(&self, event: Event) -> Result<(), slint::EventLoopError> {
    let mut queue = EVENTS
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
      queue.len() < 64,
      "headless callback queue must stay bounded"
    );
    queue.push_back(event);
    Ok(())
  }
}

struct Headless;
impl Platform for Headless {
  fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
    Ok(MinimalSoftwareWindow::new(Default::default()))
  }
  fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
    (!PROXY_CLAIMED.swap(true, Ordering::AcqRel)).then(|| Box::new(Proxy) as Box<_>)
  }
  fn duration_since_start(&self) -> Duration {
    CLOCK.with(Cell::get)
  }
}

/// Runs a scenario on a fresh owner thread with the platform installed, after any other suite.
pub(crate) fn run(scenario: impl FnOnce() + Send + 'static) {
  let _suite = SUITE
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let owner = std::thread::Builder::new()
    .name("headless-slint-owner".into())
    .spawn(move || {
      slint::platform::set_platform(Box::new(Headless)).expect("headless platform");
      // Late deliveries for earlier suites run first and find their windows gone; dropping
      // them instead would leave their senders' pending flags set.
      drain();
      scenario();
    })
    .expect("headless owner thread");
  if let Err(panic) = owner.join() {
    std::panic::resume_unwind(panic);
  }
}

/// Runs queued callbacks, including ones they queue in turn.
pub(crate) fn drain() {
  loop {
    let event = EVENTS
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .pop_front();
    match event {
      Some(event) => event(),
      None => break,
    }
  }
}

/// Drains until `done` holds, waiting briefly for worker threads; panics after five seconds.
pub(crate) fn drain_until(mut done: impl FnMut() -> bool) {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    drain();
    if done() {
      return;
    }
    assert!(Instant::now() < deadline, "headless callback deadline");
    std::thread::sleep(Duration::from_millis(2));
  }
}

/// Moves this thread's clock and fires every timer that is due, once.
pub(crate) fn advance(milliseconds: u64) {
  CLOCK.with(|clock| clock.set(clock.get() + Duration::from_millis(milliseconds)));
  slint::platform::update_timers_and_animations();
}
