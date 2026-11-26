//! Bounded owner-loop submission. Keep this value behind one lock so taking the
//! payload and clearing its posted flag cannot lose a concurrent wakeup.
pub struct CoalescedEvent<T> {
  posted: bool,
  pending: Option<T>,
}

impl<T> Default for CoalescedEvent<T> {
  fn default() -> Self {
    Self::new()
  }
}

impl<T> CoalescedEvent<T> {
  pub const fn new() -> Self {
    Self {
      posted: false,
      pending: None,
    }
  }

  /// True means the caller must submit exactly one event after releasing its lock.
  pub fn submit(&mut self, value: T) -> bool {
    self.pending = Some(value);
    if self.posted {
      false
    } else {
      self.posted = true;
      true
    }
  }

  pub fn take(&mut self) -> Option<T> {
    self.posted = false;
    self.pending.take()
  }

  pub fn submission_failed(&mut self) {
    self.posted = false;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn repeated_opens_coalesce_and_the_next_open_gets_a_new_wakeup() {
    let mut queue = CoalescedEvent::new();
    assert!(queue.submit("general"));
    for _ in 0..1000 {
      assert!(!queue.submit("about"));
    }
    assert_eq!(queue.take(), Some("about"));
    assert!(queue.submit("video"));
    assert_eq!(queue.take(), Some("video"));
  }

  #[test]
  fn failed_dispatch_can_be_retried_without_stranding_the_pending_request() {
    let mut queue = CoalescedEvent::new();
    assert!(queue.submit("general"));
    queue.submission_failed();
    assert!(queue.submit("files"));
    assert_eq!(queue.take(), Some("files"));
  }
}
