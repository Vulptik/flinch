//! Bounded worker controls. Admission stays in wire order; execution may prioritize cancellation.
use crate::{
  EngineError, ErrorKind, Result,
  protocol::{Command, Event, Request, RequestLedger},
};
use std::{
  collections::{BTreeMap, VecDeque},
  sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Interrupts {
  stop: AtomicBool,
  abort: AtomicBool,
}
impl Interrupts {
  /// Cleanup may ignore Stop, but must still yield to Abort at its next safe point.
  pub fn checkpoint(&self, stage: &str, stopping: bool) -> Result<()> {
    if self.abort.load(Ordering::Acquire) || (!stopping && self.stop.load(Ordering::Acquire)) {
      return Err(EngineError::new(
        ErrorKind::Cancelled,
        stage,
        "A priority control interrupted this operation",
      ));
    }
    Ok(())
  }
  pub fn pending(&self) -> bool {
    self.stop.load(Ordering::Acquire) || self.abort.load(Ordering::Acquire)
  }
  #[cfg(test)]
  pub(crate) fn abort_for_test(&self) {
    self.abort.store(true, Ordering::Release);
  }
}
#[derive(Default)]
struct Queue {
  ledger: RequestLedger,
  session: String,
  ordinary: VecDeque<Request>,
  stop: Option<Request>,
  abort: Option<Request>,
  replies: BTreeMap<u64, Event>,
  closed: bool,
}
impl Queue {
  fn complete(&mut self, id: u64, reply: Event) {
    self.replies.insert(id, reply);
    while self.replies.len() > 32 {
      self.replies.pop_first();
    }
  }
  fn take(&mut self) -> Option<Request> {
    self
      .abort
      .take()
      .or_else(|| self.stop.take())
      .or_else(|| self.ordinary.pop_front())
  }
}
#[derive(Default)]
pub(crate) struct Inbox {
  queue: Mutex<Queue>,
  available: Condvar,
  pub interrupts: Arc<Interrupts>,
}
impl Inbox {
  /// Returns an immediate rejection/replayed reply, or None when a command is pending.
  /// A pending duplicate waits for its original reply and never re-executes its operation.
  pub fn submit(&self, request: Request) -> Result<Option<Event>> {
    let mut queue = self
      .queue
      .lock()
      .map_err(|_| EngineError::protocol("Worker inbox poisoned"))?;
    if queue.closed {
      return Err(EngineError::new(
        ErrorKind::Cancelled,
        "worker inbox",
        "Control pipe is closed",
      ));
    }
    if !queue.ledger.accept(&request)? {
      return Ok(queue.replies.get(&request.id).cloned());
    }
    if queue.session.is_empty() {
      queue.session = request.session.clone();
    }
    let rejection = match &request.command {
      Command::Abort if self.interrupts.abort.load(Ordering::Acquire) => {
        Some((ErrorKind::InvalidState, "Abort is already pending"))
      },
      Command::Stop if self.interrupts.pending() => {
        Some((ErrorKind::InvalidState, "Stop is already pending"))
      },
      Command::Abort | Command::Stop => None,
      _ if self.interrupts.pending() => {
        Some((ErrorKind::Cancelled, "A terminal control is pending"))
      },
      _ if queue.ordinary.len() >= 8 => Some((
        ErrorKind::Saturation,
        "Eight worker controls are already pending",
      )),
      _ => None,
    };
    if let Some((kind, reason)) = rejection {
      let reply = Event::Rejected {
        id: request.id,
        error: EngineError::new(kind, "worker inbox", reason),
      };
      queue.complete(request.id, reply.clone());
      return Ok(Some(reply));
    }
    match &request.command {
      Command::Abort => {
        self.interrupts.abort.store(true, Ordering::Release);
        queue.abort = Some(request);
      },
      Command::Stop => {
        self.interrupts.stop.store(true, Ordering::Release);
        queue.stop = Some(request);
      },
      _ => queue.ordinary.push_back(request),
    }
    // No native call, event emission or pipe write occurs while holding this lock.
    drop(queue);
    self.available.notify_one();
    Ok(None)
  }
  pub fn receive(&self, timeout: Duration) -> Result<Option<Request>> {
    let deadline = Instant::now()
      .checked_add(timeout)
      .ok_or_else(|| EngineError::protocol("Control deadline overflow"))?;
    let mut queue = self
      .queue
      .lock()
      .map_err(|_| EngineError::protocol("Worker inbox poisoned"))?;
    loop {
      if let Some(request) = queue.take() {
        return Ok(Some(request));
      }
      if queue.closed {
        return Err(EngineError::new(
          ErrorKind::Cancelled,
          "worker inbox",
          "Control pipe disconnected",
        ));
      }
      let remaining = deadline.saturating_duration_since(Instant::now());
      if remaining.is_zero() {
        return Ok(None);
      }
      queue = self
        .available
        .wait_timeout(queue, remaining)
        .map_err(|_| EngineError::protocol("Worker inbox wait poisoned"))?
        .0;
    }
  }
  pub fn complete(&self, id: u64, reply: Event) -> Result<()> {
    self
      .queue
      .lock()
      .map_err(|_| EngineError::protocol("Worker inbox poisoned"))?
      .complete(id, reply);
    Ok(())
  }
  pub fn session(&self) -> Result<String> {
    Ok(
      self
        .queue
        .lock()
        .map_err(|_| EngineError::protocol("Worker inbox poisoned"))?
        .session
        .clone(),
    )
  }
  pub fn close(&self) -> Result<()> {
    self
      .queue
      .lock()
      .map_err(|_| EngineError::protocol("Worker inbox poisoned"))?
      .closed = true;
    self.available.notify_all();
    Ok(())
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  use crate::{protocol::VERSION, types::SessionState};
  fn request(id: u64, command: Command) -> Request {
    Request {
      version: VERSION,
      session: "session".into(),
      id,
      command,
    }
  }
  #[test]
  fn full_ordinary_queue_cannot_block_stop_or_abort_and_ids_keep_wire_order() {
    let inbox = Inbox::default();
    for id in 1..=8 {
      assert!(
        inbox
          .submit(request(id, Command::Pause))
          .expect("queued")
          .is_none()
      );
    }
    let rejected = inbox
      .submit(request(9, Command::Resume))
      .expect("explicit saturation");
    assert!(matches!(
      rejected,
      Some(Event::Rejected {
        error: EngineError {
          kind: ErrorKind::Saturation,
          ..
        },
        ..
      })
    ));
    inbox
      .submit(request(10, Command::Stop))
      .expect("reserved Stop slot");
    inbox
      .submit(request(11, Command::Abort))
      .expect("reserved Abort slot");
    assert_eq!(
      inbox
        .receive(Duration::ZERO)
        .expect("Abort")
        .expect("queued")
        .id,
      11
    );
    assert_eq!(
      inbox
        .receive(Duration::ZERO)
        .expect("Stop")
        .expect("queued")
        .id,
      10
    );
    assert_eq!(
      inbox
        .receive(Duration::ZERO)
        .expect("earlier control")
        .expect("queued")
        .id,
      1
    );
    assert!(inbox.interrupts.checkpoint("prepare", false).is_err());
    assert!(inbox.interrupts.checkpoint("finalize", true).is_err());
  }
  #[test]
  fn duplicate_pending_and_completed_controls_never_execute_twice() {
    let inbox = Inbox::default();
    let stop = request(1, Command::Stop);
    inbox.submit(stop.clone()).expect("Stop");
    assert!(
      inbox
        .submit(stop.clone())
        .expect("pending duplicate")
        .is_none()
    );
    assert_eq!(
      inbox
        .receive(Duration::ZERO)
        .expect("request")
        .expect("one request")
        .id,
      1
    );
    assert!(
      inbox
        .receive(Duration::ZERO)
        .expect("no duplicate")
        .is_none()
    );
    inbox
      .complete(
        1,
        Event::Ack {
          id: 1,
          state: SessionState::Completed,
        },
      )
      .expect("reply");
    assert!(matches!(
      inbox.submit(stop).expect("replay"),
      Some(Event::Ack { id: 1, .. })
    ));
    assert!(inbox.submit(request(1, Command::Abort)).is_err());
    assert!(inbox.interrupts.checkpoint("prepare", false).is_err());
    assert!(inbox.interrupts.checkpoint("finalize", true).is_ok());
  }
  #[test]
  fn invalid_requests_do_not_interrupt_and_terminal_controls_reject_new_ordinary_work() {
    let inbox = Inbox::default();
    let mut invalid = request(1, Command::Abort);
    invalid.version = 2;
    assert!(inbox.submit(invalid).is_err());
    assert!(!inbox.interrupts.pending());
    inbox.submit(request(1, Command::Stop)).expect("stop");
    let rejected = inbox
      .submit(request(2, Command::Start))
      .expect("cancel pending work");
    assert!(matches!(
      rejected,
      Some(Event::Rejected {
        error: EngineError {
          kind: ErrorKind::Cancelled,
          ..
        },
        ..
      })
    ));
    assert_eq!(inbox.session().expect("session"), "session");
    inbox.close().expect("close");
    assert!(inbox.submit(request(3, Command::Abort)).is_err());
    inbox
      .receive(Duration::ZERO)
      .expect("remaining stop")
      .expect("stop");
    assert!(inbox.receive(Duration::ZERO).is_err());
  }
  #[test]
  fn waiting_on_an_empty_inbox_is_finite_and_close_wakes_its_owner() {
    let inbox = Arc::new(Inbox::default());
    assert!(
      inbox
        .receive(Duration::from_millis(1))
        .expect("finite timeout")
        .is_none()
    );
    let waiting = inbox.clone();
    let (finished, result) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
      finished
        .send(waiting.receive(Duration::from_secs(1)))
        .expect("answer");
    });
    inbox.close().expect("close");
    assert_eq!(
      result
        .recv_timeout(Duration::from_secs(2))
        .expect("wake")
        .expect_err("closed")
        .kind,
      ErrorKind::Cancelled
    );
  }
}
