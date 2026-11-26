//! Host-side session client for applications. One pump thread owns the worker and drains its
//! events continuously, so a quiet caller can never back the worker's event pipe up.
use super::supervisor::WorkerProcess;
use crate::{
  EngineError, ErrorKind, Result,
  client::control::{ControlReport, Watch},
  limits,
  protocol::{self, Command, Event, Request, Response},
  types::*,
};
use std::{
  collections::VecDeque,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError},
  },
  thread::JoinHandle,
  time::{Duration, Instant},
};

/// Worker pipes as the pump sees them; tests substitute a scripted worker.
pub trait Transport: Send + 'static {
  fn send(&mut self, request: &Request) -> Result<()>;
  fn receive(&self, timeout: Duration) -> Result<Option<Response>>;
  fn wait(&mut self, timeout: Duration) -> Result<()>;
  fn terminate(&mut self) -> Result<()>;
  /// The next buffered stderr line, if any; every transport states its own.
  fn diagnostic(&self) -> Option<String>;
}
impl Transport for WorkerProcess {
  fn send(&mut self, request: &Request) -> Result<()> {
    Self::send(self, request)
  }
  fn receive(&self, timeout: Duration) -> Result<Option<Response>> {
    Self::receive(self, timeout)
  }
  fn wait(&mut self, timeout: Duration) -> Result<()> {
    Self::wait(self, timeout)
  }
  fn terminate(&mut self) -> Result<()> {
    Self::terminate(self)
  }
  fn diagnostic(&self) -> Option<String> {
    Self::diagnostic(self)
  }
}

/// Bounded worker diagnostics kept for the host's own logging.
const DIAGNOSTICS: usize = 32;
/// Margin over the pump-enforced deadline before a caller stops waiting for its reply.
const REPLY_MARGIN: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum Reply {
  State(SessionState),
  Outcome(Box<RecordingOutcome>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
  Prepare,
  Start,
  Pause,
  Resume,
  Microphone,
  Terminal,
}
struct Waiter {
  id: u64,
  wait: Wait,
  reply: SyncSender<Result<Reply>>,
}
#[derive(Default)]
struct Shared {
  snapshot: SessionSnapshot,
  directory: Option<PathBuf>,
  terminal: Option<RecordingOutcome>,
  failure: Option<EngineError>,
  controls: ControlReport,
  diagnostics: VecDeque<String>,
  diagnostics_lost: u64,
}

/// One recording session in an owned worker process. Every call is bounded by its control
/// deadline; a worker that stops answering is terminated and reported, never waited on forever.
pub struct HostSession {
  requests: Option<SyncSender<(Command, SyncSender<Result<Reply>>)>>,
  shared: Arc<Mutex<Shared>>,
  pump: Option<JoinHandle<()>>,
}
impl HostSession {
  /// Launch `executable` with `worker_args` (for example the app's own worker switch).
  pub fn launch(executable: &Path, worker_args: &[&str]) -> Result<Self> {
    Self::with_transport(WorkerProcess::spawn_with(executable, worker_args)?)
  }
  pub fn with_transport(worker: impl Transport) -> Result<Self> {
    let hello = worker.receive(limits::HELLO_TIMEOUT)?.ok_or_else(|| {
      EngineError::new(
        ErrorKind::Timeout,
        "worker connection",
        "Worker hello timeout",
      )
    })?;
    if !matches!(hello.event, Event::Hello)
      || hello.version != protocol::VERSION
      || !hello.session.is_empty()
    {
      return Err(EngineError::protocol("Invalid worker hello"));
    }
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (requests, commands) = mpsc::sync_channel(4);
    let state = shared.clone();
    let session = format!("host-{}", std::process::id());
    let pump = std::thread::Builder::new()
      .name("video host pump".into())
      .spawn(move || pump(worker, session, commands, state))
      .map_err(|e| EngineError::io("video host pump", e))?;
    Ok(Self {
      requests: Some(requests),
      shared,
      pump: Some(pump),
    })
  }
  fn request(&self, command: Command, limit: Duration) -> Result<Reply> {
    let requests = self
      .requests
      .as_ref()
      .ok_or_else(|| EngineError::protocol("Host session already closed"))?;
    let (reply, result) = mpsc::sync_channel(1);
    requests
      .send((command, reply))
      .map_err(|_| self.failure_or("Host session pump has stopped"))?;
    match result.recv_timeout(limit + REPLY_MARGIN) {
      Ok(reply) => reply,
      Err(RecvTimeoutError::Timeout) => Err(EngineError::new(
        ErrorKind::Timeout,
        "host control",
        "The worker pump did not answer within its control deadline",
      )),
      Err(RecvTimeoutError::Disconnected) => Err(self.failure_or("Host session pump has stopped")),
    }
  }
  /// The pump's recorded failure, or `fallback` when none was recorded.
  fn failure_or(&self, fallback: &str) -> EngineError {
    self
      .failure()
      .unwrap_or_else(|| EngineError::protocol(fallback))
  }
  /// Returns the session directory, which holds the partial file and diagnostics.
  pub fn prepare(&self, spec: SessionSpec) -> Result<PathBuf> {
    self.request(Command::Prepare(spec), limits::PREPARE_TIMEOUT)?;
    self
      .shared
      .lock()
      .map_err(|_| EngineError::protocol("Host state poisoned"))?
      .directory
      .clone()
      .ok_or_else(|| EngineError::protocol("Worker became Ready without a session directory"))
  }
  /// Returns once the first compressed frame has been accepted by the writer.
  pub fn start(&self) -> Result<()> {
    self
      .request(Command::Start, limits::START_TIMEOUT)
      .map(|_| ())
  }
  pub fn pause(&self) -> Result<()> {
    self
      .request(Command::Pause, limits::CONTROL_TIMEOUT)
      .map(|_| ())
  }
  pub fn resume(&self) -> Result<()> {
    self
      .request(Command::Resume, limits::CONTROL_TIMEOUT)
      .map(|_| ())
  }
  /// Mutes or unmutes the recording's microphone from now.
  pub fn microphone(&self, audible: bool) -> Result<()> {
    self
      .request(Command::Microphone(audible), limits::CONTROL_TIMEOUT)
      .map(|_| ())
  }
  pub fn stop(&self) -> Result<RecordingOutcome> {
    self.terminal(Command::Stop, limits::STOP_TIMEOUT)
  }
  pub fn abort(&self) -> Result<RecordingOutcome> {
    self.terminal(Command::Abort, limits::ABORT_TIMEOUT)
  }
  fn terminal(&self, command: Command, limit: Duration) -> Result<RecordingOutcome> {
    match self.request(command, limit)? {
      Reply::Outcome(outcome) => Ok(*outcome),
      Reply::State(state) => Err(EngineError::protocol(format!(
        "Terminal control confirmed {state:?} instead of an outcome"
      ))),
    }
  }
  pub fn snapshot(&self) -> SessionSnapshot {
    self
      .shared
      .lock()
      .map(|shared| shared.snapshot.clone())
      .unwrap_or_default()
  }
  pub fn controls(&self) -> ControlReport {
    self
      .shared
      .lock()
      .map(|shared| shared.controls)
      .unwrap_or_default()
  }
  /// Worker diagnostics since the last call, oldest first, and how many were dropped.
  pub fn take_diagnostics(&self) -> (Vec<String>, u64) {
    self
      .shared
      .lock()
      .map(|mut shared| {
        let lost = std::mem::take(&mut shared.diagnostics_lost);
        (shared.diagnostics.drain(..).collect(), lost)
      })
      .unwrap_or_default()
  }
  /// The failure that stopped the pump (deadline, protocol, transport), if any.
  pub fn failure(&self) -> Option<EngineError> {
    self
      .shared
      .lock()
      .ok()
      .and_then(|shared| shared.failure.clone())
  }
  pub fn terminal_outcome(&self) -> Option<RecordingOutcome> {
    self
      .shared
      .lock()
      .ok()
      .and_then(|shared| shared.terminal.clone())
  }
}
impl Drop for HostSession {
  fn drop(&mut self) {
    let finished = self
      .shared
      .lock()
      .map(|shared| shared.terminal.is_some() || shared.failure.is_some())
      .unwrap_or(true);
    if !finished && let Err(error) = self.abort() {
      eprintln!("Video host abort on drop: {error}");
    }
    // Closing the request channel ends the pump, which terminates any remaining worker.
    self.requests = None;
    if let Some(pump) = self.pump.take()
      && pump.join().is_err()
    {
      eprintln!("Video host pump panicked");
    }
  }
}

fn pump(
  mut worker: impl Transport,
  session: String,
  commands: Receiver<(Command, SyncSender<Result<Reply>>)>,
  shared: Arc<Mutex<Shared>>,
) {
  let mut watch = Watch::new(Instant::now());
  let mut next = 1u64;
  let mut bound = false;
  let mut waiters: Vec<Waiter> = Vec::new();
  let result = (|| -> Result<()> {
    loop {
      loop {
        match commands.try_recv() {
          Ok((command, reply)) => {
            let wait = match command {
              Command::Prepare(_) => Wait::Prepare,
              Command::Start => Wait::Start,
              Command::Pause => Wait::Pause,
              Command::Resume => Wait::Resume,
              Command::Microphone(_) => Wait::Microphone,
              Command::Stop | Command::Abort => Wait::Terminal,
            };
            let id = next;
            next = next
              .checked_add(1)
              .ok_or_else(|| EngineError::protocol("Request id overflow"))?;
            // A refused control (for example Pause while stopping) is answered, not fatal.
            if let Err(error) = watch.submitted(id, &command, Instant::now()) {
              answer(reply, Err(error));
              continue;
            }
            worker.send(&Request {
              version: protocol::VERSION,
              session: session.clone(),
              id,
              command,
            })?;
            waiters.push(Waiter { id, wait, reply });
          },
          Err(TryRecvError::Empty) => break,
          // The host was dropped without a terminal result: nothing may keep recording.
          Err(TryRecvError::Disconnected) => {
            return Err(EngineError::new(
              ErrorKind::Cancelled,
              "host session",
              "Host session closed before a terminal result",
            ));
          },
        }
      }
      while let Some(message) = worker.diagnostic() {
        diagnostic(&shared, message);
      }
      watch.check(Instant::now())?;
      let Some(response) = worker.receive(Duration::from_millis(5))? else {
        continue;
      };
      if response.version != protocol::VERSION {
        return Err(EngineError::protocol("Worker protocol version changed"));
      }
      if matches!(&response.event, Event::Ack { id, .. } | Event::Rejected { id, .. } if *id == 0 || *id >= next)
      {
        return Err(EngineError::protocol(
          "Worker acknowledged an unissued request",
        ));
      }
      if response.session == session {
        bound = true;
      } else if !(response.session.is_empty()
        && !bound
        && matches!(response.event, Event::Heartbeat))
      {
        return Err(EngineError::protocol("Worker session identity changed"));
      }
      watch.observe(&response.event, Instant::now())?;
      let mut state = shared
        .lock()
        .map_err(|_| EngineError::protocol("Host state poisoned"))?;
      state.controls = watch.report();
      match response.event {
        Event::Prepared(path) => {
          if state.directory.replace(path).is_some() {
            return Err(EngineError::protocol(
              "Worker prepared a second session directory",
            ));
          }
        },
        Event::Ack {
          id,
          state: confirmed,
        } => {
          let expected = waiters.iter().position(|w| {
            w.id == id
              && matches!(
                (w.wait, confirmed),
                (Wait::Prepare, SessionState::Ready)
                  | (Wait::Pause, SessionState::Paused)
                  | (Wait::Resume, SessionState::Recording)
                  | (
                    Wait::Microphone,
                    SessionState::Recording | SessionState::Paused
                  )
              )
          });
          if let Some(index) = expected {
            answer(waiters.remove(index).reply, Ok(Reply::State(confirmed)));
          }
        },
        Event::State(SessionState::Recording) => {
          // Start completes at the first written frame, not at its acknowledgement.
          waiters.retain(|w| {
            if w.wait == Wait::Start {
              answer(w.reply.clone(), Ok(Reply::State(SessionState::Recording)));
              false
            } else {
              true
            }
          });
        },
        Event::Rejected { id, error } => {
          if let Some(index) = waiters.iter().position(|w| w.id == id) {
            answer(waiters.remove(index).reply, Err(error));
          }
        },
        Event::Stats(snapshot) => state.snapshot = snapshot,
        Event::Diagnostic(error) => {
          drop(state);
          diagnostic(&shared, error.to_string());
        },
        Event::Outcome(outcome) => {
          state.snapshot = outcome.snapshot.clone();
          state.terminal = Some(outcome.clone());
          drop(state);
          for waiter in waiters.drain(..) {
            answer(
              waiter.reply,
              if waiter.wait == Wait::Terminal {
                Ok(Reply::Outcome(Box::new(outcome.clone())))
              } else if let Some(error) = &outcome.error {
                // A failed Prepare or Start ends the session with its typed cause (capture
                // unavailable, retained partial); the waiting control must report that cause.
                Err(error.clone())
              } else {
                Err(EngineError::new(
                  ErrorKind::InvalidState,
                  "host control",
                  "The session ended before this control completed",
                ))
              },
            );
          }
          // The worker exits after its terminal result; a lingering process is terminated.
          let deadline = watch
            .terminal_deadline()
            .unwrap_or_else(|| Instant::now() + limits::ABORT_TIMEOUT);
          let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(100));
          if let Err(cleanup) = worker.wait(remaining.min(limits::ABORT_TIMEOUT)) {
            diagnostic(&shared, format!("Worker cleanup: {cleanup}"));
            worker.terminate()?;
          }
          return Ok(());
        },
        _ => {},
      }
    }
  })();
  if let Err(error) = result {
    let mut error = error;
    if let Err(cleanup) = worker.terminate() {
      error
        .reason
        .push_str(&format!("; owned worker termination: {cleanup}"));
    }
    if let Ok(mut state) = shared.lock() {
      if error.recoverable_path.is_none() {
        error.recoverable_path = state
          .directory
          .as_ref()
          .map(|directory| directory.join("capture.partial.mp4"));
      }
      state.failure = Some(error.clone());
    }
    for waiter in waiters.drain(..) {
      answer(waiter.reply, Err(error.clone()));
    }
  }
}
fn answer(reply: SyncSender<Result<Reply>>, value: Result<Reply>) {
  // Capacity one and a single answer per request: never blocks; a departed caller is fine.
  let _ = reply.try_send(value);
}
fn diagnostic(shared: &Arc<Mutex<Shared>>, message: String) {
  if let Ok(mut state) = shared.lock() {
    if state.diagnostics.len() >= DIAGNOSTICS {
      state.diagnostics.pop_front();
      state.diagnostics_lost = state.diagnostics_lost.saturating_add(1);
    }
    state.diagnostics.push_back(message);
  }
}

#[cfg(test)]
mod tests;
