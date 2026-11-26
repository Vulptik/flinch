//! Bounded presenter scheduling; persistence and startup side effects belong to
//! the application service. No channels, locks or runtime are owned here.

use crate::edit::{EditDraft, PreferenceEdit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
  generation: u64,
  sequence: u64,
}

#[derive(Debug, Clone)]
pub struct SaveRequest<T> {
  pub ticket: Ticket,
  pub draft: T,
}

/// At most one active transaction and one newest pending draft. Completion
/// returns failed drafts for retry and rejects callbacks from retired windows.
pub struct SaveQueue<T> {
  generation: u64,
  sequence: u64,
  active: Option<Ticket>,
  pending: Option<T>,
}

impl<T> SaveQueue<T> {
  pub fn new(generation: u64) -> Self {
    Self {
      generation,
      sequence: 0,
      active: None,
      pending: None,
    }
  }

  pub fn edit(&mut self, draft: T) {
    self.pending = Some(draft);
  }

  pub fn start(&mut self) -> Result<Option<SaveRequest<T>>, &'static str> {
    if self.active.is_some() || self.pending.is_none() {
      return Ok(None);
    }
    let sequence = self
      .sequence
      .checked_add(1)
      .ok_or("Settings save sequence exhausted")?;
    self.sequence = sequence;
    let ticket = Ticket {
      generation: self.generation,
      sequence,
    };
    self.active = Some(ticket);
    Ok(
      self
        .pending
        .take()
        .map(|draft| SaveRequest { ticket, draft }),
    )
  }

  pub fn complete(&mut self, ticket: Ticket) -> bool {
    if self.active != Some(ticket) {
      return false;
    }
    self.active = None;
    true
  }

  pub fn fail(&mut self, request: SaveRequest<T>) -> bool {
    if !self.complete(request.ticket) {
      return false;
    }
    if self.pending.is_none() {
      self.pending = Some(request.draft);
    }
    true
  }

  pub fn is_idle(&self) -> bool {
    self.active.is_none() && self.pending.is_none()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
  KeepOpen,
  Close,
  WaitForSave,
  ResolveFailure,
}

/// UI-thread state only. A failed write stays paused until another user edit or close;
/// there is no automatic retry loop, channel backlog or wait on the UI thread.
pub struct EditSession {
  validation_blocked: bool,
  unsaved: EditDraft,
  queue: SaveQueue<EditDraft>,
  failed: bool,
  closing: bool,
}

impl EditSession {
  pub fn is_idle(&self) -> bool {
    self.queue.is_idle()
  }
  pub fn new(generation: u64) -> Self {
    Self {
      validation_blocked: false,
      unsaved: EditDraft::default(),
      queue: SaveQueue::new(generation),
      failed: false,
      closing: false,
    }
  }

  pub fn edit(&mut self, edit: PreferenceEdit) -> Result<(), &'static str> {
    if self.closing {
      return Err("Settings are closing; wait for the save decision");
    }
    self.failed = false;
    self.unsaved.insert(edit);
    self.queue.edit(self.unsaved.clone());
    Ok(())
  }
  pub fn remove(&mut self, edit: &PreferenceEdit) {
    self.unsaved.remove(edit);
    self.queue.pending = if self.unsaved.is_empty() {
      None
    } else {
      Some(self.unsaved.clone())
    };
  }
  pub fn set_validation_blocked(&mut self, blocked: bool) {
    self.validation_blocked = blocked;
  }

  pub fn start(&mut self) -> Result<Option<SaveRequest<EditDraft>>, &'static str> {
    if self.failed {
      return Ok(None);
    }
    self.queue.start()
  }

  /// Returns false for a completion belonging to a retired window or request.
  pub fn saved(&mut self, request: &SaveRequest<EditDraft>) -> bool {
    if !self.queue.complete(request.ticket) {
      return false;
    }
    self.unsaved.acknowledge(&request.draft);
    self.queue.pending = if self.unsaved.is_empty() {
      None
    } else {
      Some(self.unsaved.clone())
    };
    true
  }

  pub fn failed(&mut self, request: SaveRequest<EditDraft>) -> bool {
    if !self.queue.fail(request) {
      return false;
    }
    self.queue.edit(self.unsaved.clone());
    self.failed = true;
    true
  }

  pub fn retry(&mut self) {
    self.failed = false;
  }

  pub fn request_close(&mut self) -> CloseDecision {
    self.closing = true;
    self.close_decision()
  }

  /// Explicit app quit never asks the user to resolve invalid or failed edits.
  /// Valid writes keep their ownership and drain before exit; failed pending
  /// drafts are dropped only after their transaction has completed.
  pub fn request_quit(&mut self) -> CloseDecision {
    self.closing = true;
    self.validation_blocked = false;
    if self.failed {
      self.unsaved = EditDraft::default();
      self.queue.pending = None;
      self.failed = false;
    }
    self.close_decision()
  }

  pub fn close_decision(&self) -> CloseDecision {
    if !self.closing {
      CloseDecision::KeepOpen
    } else if self.failed || self.validation_blocked {
      CloseDecision::ResolveFailure
    } else if self.queue.is_idle() {
      CloseDecision::Close
    } else {
      CloseDecision::WaitForSave
    }
  }

  pub fn cancel_close(&mut self) {
    self.closing = false;
  }

  /// Invalid text never enters the queue. Preserve valid pending edits.
  pub fn discard_invalid_and_close(&mut self) -> Result<(), &'static str> {
    if !self.closing || !self.validation_blocked {
      return Err("Discard invalid edits requires an invalid close request");
    }
    self.validation_blocked = false;
    Ok(())
  }

  /// Discard is offered only after a failed transaction has finished. An
  /// in-progress worker cannot be cancelled or hidden by discarding its draft.
  pub fn discard_and_close(&mut self) -> Result<(), &'static str> {
    if !self.closing || !(self.failed || self.validation_blocked) || self.queue.active.is_some() {
      return Err("Discard requires a completed failed save and a close request");
    }
    self.unsaved = EditDraft::default();
    self.queue.pending = None;
    self.failed = false;
    self.validation_blocked = false;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn invalid_field_does_not_block_other_saves_and_requires_a_close_decision() {
    let mut session = EditSession::new(1);
    session
      .edit(PreferenceEdit::Filename("valid".into()))
      .expect("edit");
    session.remove(&PreferenceEdit::Filename(String::new()));
    session.set_validation_blocked(true);
    session
      .edit(PreferenceEdit::RecordAudio(true))
      .expect("other edit");
    let request = session.start().expect("sequence").expect("request");
    assert_eq!(request.draft.edits().count(), 1);
    assert!(session.saved(&request));
    assert_eq!(session.request_close(), CloseDecision::ResolveFailure);
    session.discard_and_close().expect("explicit discard");
    assert_eq!(session.close_decision(), CloseDecision::Close);
  }

  #[test]
  fn discarding_invalid_input_preserves_active_and_pending_valid_edits() {
    let mut session = EditSession::new(1);
    session
      .edit(PreferenceEdit::RecordAudio(true))
      .expect("edit");
    let active = session.start().expect("sequence").expect("active");
    session.edit(PreferenceEdit::VideoFps(60)).expect("edit");
    session.set_validation_blocked(true);
    assert_eq!(session.request_close(), CloseDecision::ResolveFailure);
    session
      .discard_invalid_and_close()
      .expect("discard invalid");
    assert_eq!(session.close_decision(), CloseDecision::WaitForSave);
    assert!(session.saved(&active));
    let pending = session.start().expect("sequence").expect("pending");
    assert_eq!(
      pending.draft.edits().next(),
      Some(&PreferenceEdit::VideoFps(60))
    );
    assert!(session.saved(&pending));
    assert_eq!(session.close_decision(), CloseDecision::Close);
  }

  #[test]
  fn user_edit_resumes_failed_save_without_an_automatic_retry_loop() {
    let mut session = EditSession::new(1);
    session
      .edit(PreferenceEdit::RecordAudio(true))
      .expect("edit");
    let request = session.start().expect("sequence").expect("request");
    session.failed(request);
    assert!(session.start().expect("sequence").is_none());
    session.edit(PreferenceEdit::VideoFps(60)).expect("edit");
    let request = session.start().expect("sequence").expect("request");
    assert_eq!(request.draft.edits().count(), 2);
  }

  #[test]
  fn explicit_quit_discards_invalid_input_and_drains_valid_writes() {
    let mut session = EditSession::new(1);
    session
      .edit(PreferenceEdit::RecordAudio(true))
      .expect("edit");
    let active = session.start().expect("sequence").expect("active");
    session.edit(PreferenceEdit::VideoFps(60)).expect("edit");
    session.set_validation_blocked(true);
    assert_eq!(session.request_close(), CloseDecision::ResolveFailure);
    assert_eq!(session.request_quit(), CloseDecision::WaitForSave);
    assert!(session.saved(&active));
    let pending = session.start().expect("sequence").expect("pending");
    assert!(session.saved(&pending));
    assert_eq!(session.close_decision(), CloseDecision::Close);
  }

  #[test]
  fn explicit_quit_discards_failed_draft_without_retrying_or_prompting() {
    let mut session = EditSession::new(1);
    session
      .edit(PreferenceEdit::RecordAudio(true))
      .expect("edit");
    let active = session.start().expect("sequence").expect("active");
    assert_eq!(session.request_quit(), CloseDecision::WaitForSave);
    assert!(session.failed(active));
    assert_eq!(session.request_quit(), CloseDecision::Close);
    assert!(session.start().expect("sequence").is_none());
  }

  #[test]
  fn session_coalesces_typing_without_losing_edits_to_other_controls() {
    let mut session = EditSession::new(1);
    session
      .edit(PreferenceEdit::RecordAudio(true))
      .expect("edit");
    let first = session.start().expect("sequence").expect("request");
    for index in 0..1000 {
      session
        .edit(PreferenceEdit::Filename(index.to_string()))
        .expect("edit");
    }
    session.edit(PreferenceEdit::VideoFps(60)).expect("edit");
    assert!(session.saved(&first));
    let next = session.start().expect("sequence").expect("request");
    assert_eq!(
      next.draft.edits().cloned().collect::<Vec<_>>(),
      [
        PreferenceEdit::VideoFps(60),
        PreferenceEdit::Filename("999".into())
      ]
    );
  }

  #[test]
  fn acknowledging_old_value_does_not_clear_newer_value_of_same_field() {
    let mut session = EditSession::new(1);
    session
      .edit(PreferenceEdit::RecordAudio(true))
      .expect("edit");
    let first = session.start().expect("sequence").expect("request");
    session
      .edit(PreferenceEdit::RecordAudio(false))
      .expect("edit");
    assert!(session.saved(&first));
    let next = session.start().expect("sequence").expect("request");
    assert_eq!(
      next.draft.edits().next(),
      Some(&PreferenceEdit::RecordAudio(false))
    );
  }

  #[test]
  fn close_waits_for_save_and_failures_require_an_explicit_decision() {
    let mut session = EditSession::new(1);
    session.edit(PreferenceEdit::AutoStart(true)).expect("edit");
    let first = session.start().expect("sequence").expect("request");
    assert_eq!(session.request_close(), CloseDecision::WaitForSave);
    assert!(session.discard_and_close().is_err());
    assert!(session.edit(PreferenceEdit::RecordAudio(true)).is_err());
    assert!(session.failed(first));
    assert_eq!(session.close_decision(), CloseDecision::ResolveFailure);
    assert!(session.start().expect("sequence").is_none());
    session.retry();
    let retry = session.start().expect("sequence").expect("request");
    assert!(session.saved(&retry));
    assert_eq!(session.close_decision(), CloseDecision::Close);
  }

  #[test]
  fn failed_close_can_be_cancelled_or_explicitly_discarded() {
    let mut session = EditSession::new(1);
    session
      .edit(PreferenceEdit::Filename("unsaved".into()))
      .expect("edit");
    let first = session.start().expect("sequence").expect("request");
    session.failed(first);
    session.request_close();
    session.cancel_close();
    assert!(session.discard_and_close().is_err());
    session.request_close();
    session
      .discard_and_close()
      .expect("discard finished failed write");
    assert_eq!(session.close_decision(), CloseDecision::Close);
  }

  #[test]
  fn edits_coalesce_while_save_runs() {
    let mut queue = SaveQueue::new(1);
    queue.edit("first");
    let first = queue.start().expect("sequence").expect("request");
    for _ in 0..1000 {
      queue.edit("obsolete");
    }
    queue.edit("latest");
    assert!(queue.start().expect("sequence").is_none());
    assert!(queue.complete(first.ticket));
    let latest = queue.start().expect("sequence").expect("request");
    assert_eq!(latest.draft, "latest");
    assert!(!queue.complete(first.ticket));
    assert!(queue.complete(latest.ticket));
    assert!(queue.is_idle());
  }

  #[test]
  fn failed_save_retains_draft_without_overwriting_newer_edits() {
    let mut queue = SaveQueue::new(1);
    queue.edit("retry");
    let first = queue.start().expect("sequence").expect("request");
    assert!(queue.fail(first));
    let retry = queue.start().expect("sequence").expect("request");
    assert_eq!(retry.draft, "retry");
    queue.edit("newer");
    assert!(queue.fail(retry));
    assert_eq!(
      queue.start().expect("sequence").expect("request").draft,
      "newer"
    );
  }

  #[test]
  fn old_window_completion_cannot_acknowledge_new_window() {
    let mut old = SaveQueue::new(1);
    old.edit(1);
    let old = old.start().expect("sequence").expect("request");
    let mut new = SaveQueue::new(2);
    new.edit(2);
    let current = new.start().expect("sequence").expect("request");
    assert!(!new.complete(old.ticket));
    assert!(new.complete(current.ticket));
  }
}
