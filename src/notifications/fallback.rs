//! Finite batches on the owned blocking pool; no lock is held during Windows delivery.
use std::{
  collections::VecDeque,
  sync::{Arc, LazyLock, Mutex, MutexGuard},
};

const CAPACITY: usize = 16;

struct Toast {
  title: String,
  body: String,
}

#[derive(Default)]
struct State {
  pending: VecDeque<Toast>,
  running: bool,
}

#[derive(Default)]
struct Queue(Mutex<State>);

impl Queue {
  fn state(&self) -> MutexGuard<'_, State> {
    self.0.lock().unwrap_or_else(|error| {
      tracing::error!("Recovering poisoned Windows notification queue");
      error.into_inner()
    })
  }

  /// The caller launches a worker only when it claims the idle queue.
  fn push(&self, title: &str, body: &str) -> Result<bool, String> {
    let mut state = self.state();
    if state.pending.len() == CAPACITY {
      return Err(format!(
        "Windows notification fallback queue full ({CAPACITY})"
      ));
    }
    state.pending.push_back(Toast {
      title: title.into(),
      body: body.into(),
    });
    let start = !state.running;
    state.running = true;
    Ok(start)
  }

  /// At most CAPACITY deliveries per task. Idle release and producer handoff are atomic.
  fn drain_batch(&self, mut deliver: impl FnMut(&str, &str) -> Result<(), String>) -> bool {
    struct Worker<'a>(&'a Queue);
    impl Drop for Worker<'_> {
      fn drop(&mut self) {
        if std::thread::panicking() {
          self.0.state().running = false;
          tracing::error!(
            "Windows notification worker panicked; queued messages retained for next submission"
          );
        }
      }
    }
    let _worker = Worker(self);
    for _ in 0..CAPACITY {
      let next = {
        let mut state = self.state();
        match state.pending.pop_front() {
          Some(next) => next,
          None => {
            state.running = false;
            return false;
          },
        }
      };
      if let Err(error) = deliver(&next.title, &next.body) {
        tracing::error!(%error, title = next.title, "Windows notification fallback failed");
      }
    }
    let mut state = self.state();
    let more = !state.pending.is_empty();
    state.running = more;
    more
  }
}

fn start_worker(queue: Arc<Queue>) {
  crate::app::runtime::spawn_blocking(move || {
    if queue.drain_batch(|title, body| {
      crate::app::platform::show_toast(title, body)
        .map_err(|error| crate::error::ErrorExt::to_rich_string(&error))
    }) {
      start_worker(queue);
    }
  });
}

pub(super) fn enqueue(title: &str, body: &str) {
  static QUEUE: LazyLock<Arc<Queue>> = LazyLock::new(|| Arc::new(Queue::default()));
  match QUEUE.push(title, body) {
    Ok(true) => start_worker(Arc::clone(&QUEUE)),
    Ok(false) => {},
    Err(error) => {
      tracing::error!(%error, title, "Notification could not be queued for Windows delivery")
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn concurrent_submission_does_not_block_on_delivery_or_launch_another_worker() {
    let queue = Arc::new(Queue::default());
    assert!(queue.push("first", "one").expect("first queued"));
    let worker_queue = Arc::clone(&queue);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
      let mut delivered = Vec::new();
      assert!(!worker_queue.drain_batch(|title, _| {
        if title == "first" {
          started_tx.send(()).map_err(|error| error.to_string())?;
          release_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|error| error.to_string())?;
        }
        delivered.push(title.to_owned());
        Ok(())
      }));
      delivered
    });
    started_rx
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("delivery started");
    assert!(!queue.push("second", "two").expect("concurrent enqueue"));
    assert!(!queue.push("third", "three").expect("concurrent enqueue"));
    release_tx
      .send(())
      .expect("delivery was not blocked by queue lock");
    assert_eq!(
      worker.join().expect("worker completed"),
      ["first", "second", "third"]
    );
    assert!(!queue.state().running);
  }

  #[test]
  fn busy_delivery_queues_messages_in_order_and_continues_after_failure() {
    let queue = Queue::default();
    assert!(queue.push("first", "one").expect("first queued"));
    let mut delivered = Vec::new();
    assert!(!queue.drain_batch(|title, body| {
      delivered.push((title.to_owned(), body.to_owned()));
      if title == "first" {
        assert!(!queue.push("second", "two").expect("queued while busy"));
        assert!(!queue.push("third", "three").expect("queued while busy"));
        return Err("Injected Windows failure".into());
      }
      Ok(())
    }));
    assert_eq!(
      delivered,
      [
        ("first".into(), "one".into()),
        ("second".into(), "two".into()),
        ("third".into(), "three".into())
      ]
    );
    assert!(queue.push("fourth", "four").expect("idle worker reclaimed"));
  }

  #[test]
  fn overflow_is_explicit_and_batches_yield_without_losing_queued_work() {
    let queue = Queue::default();
    for index in 0..CAPACITY {
      assert_eq!(
        queue.push(&index.to_string(), "body").expect("queued"),
        index == 0
      );
    }
    assert!(queue.push("overflow", "body").is_err());
    let mut count = 0;
    assert!(queue.drain_batch(|_, _| {
      count += 1;
      assert!(!queue.push("replacement", "body").expect("bounded refill"));
      Ok(())
    }));
    assert_eq!(count, CAPACITY);
    assert!(!queue.drain_batch(|_, _| {
      count += 1;
      Ok(())
    }));
    assert_eq!(count, CAPACITY * 2);
    assert!(!queue.state().running);
  }
}
