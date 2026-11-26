use crate::{EngineError, Result, types::*};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
  collections::VecDeque,
  io::{BufRead, Write},
};

pub const VERSION: u32 = 1;
pub const MAX_MESSAGE: usize = 64 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", content = "spec", rename_all = "snake_case")]
pub enum Command {
  Prepare(SessionSpec),
  Start,
  Pause,
  Resume,
  /// The microphone is audible (`true`) or muted, from now; the state stays as it is.
  Microphone(bool),
  Stop,
  Abort,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
  pub version: u32,
  pub session: String,
  pub id: u64,
  pub command: Command,
}
// Each event is serialized and dropped right away, never stored in bulk, so the larger snapshot
// variants cost nothing worth a box.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
  Hello,
  Prepared(std::path::PathBuf),
  Ack { id: u64, state: SessionState },
  Rejected { id: u64, error: EngineError },
  State(SessionState),
  Stats(SessionSnapshot),
  Diagnostic(EngineError),
  StageStalled(EngineError),
  Heartbeat,
  Outcome(RecordingOutcome),
  Verification(std::path::PathBuf),
  FixtureComplete(std::path::PathBuf),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
  pub version: u32,
  pub session: String,
  pub event: Event,
}

/// Read no more than a bounded line, even if a peer never sends a newline.
pub fn read<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<Option<T>> {
  let mut bytes = Vec::with_capacity(512);
  loop {
    let chunk = reader
      .fill_buf()
      .map_err(|e| EngineError::io("protocol read", e))?;
    if chunk.is_empty() {
      return if bytes.is_empty() {
        Ok(None)
      } else {
        Err(EngineError::protocol("EOF in a protocol message"))
      };
    }
    let count = chunk
      .iter()
      .position(|b| *b == b'\n')
      .map_or(chunk.len(), |n| n + 1);
    if bytes.len() + count > MAX_MESSAGE {
      return Err(EngineError::protocol("Message exceeds 64 KiB"));
    }
    let done = chunk[count - 1] == b'\n';
    bytes.extend_from_slice(&chunk[..count]);
    reader.consume(count);
    if done {
      return serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| EngineError::protocol(e.to_string()));
    }
  }
}
pub fn write(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
  let mut bytes = crate::limits::json_record(value, MAX_MESSAGE - 1)?;
  bytes.reserve_exact(1);
  bytes.push(b'\n');
  writer
    .write_all(&bytes)
    .and_then(|_| writer.flush())
    .map_err(|e| EngineError::io("protocol write", e))
}

#[derive(Default)]
pub struct RequestLedger {
  session: Option<String>,
  recent: VecDeque<Request>,
  highest: u64,
}
impl RequestLedger {
  /// Returns false only for a byte-equivalent repeated request.
  pub fn accept(&mut self, request: &Request) -> Result<bool> {
    if request.version != VERSION
      || request.id == 0
      || request.session.is_empty()
      || request.session.len() > 128
    {
      return Err(EngineError::protocol(
        "Invalid protocol version, request id or session",
      ));
    }
    if self.session.as_ref().is_some_and(|s| *s != request.session) {
      return Err(EngineError::protocol("Worker owns a different session"));
    }
    if let Some(previous) = self.recent.iter().find(|r| r.id == request.id) {
      return if previous == request {
        Ok(false)
      } else {
        Err(EngineError::protocol(
          "Request id reused with different content",
        ))
      };
    }
    if request.id <= self.highest {
      return Err(EngineError::protocol("Request id is stale"));
    }
    self.session = Some(request.session.clone());
    self.highest = request.id;
    if self.recent.len() == 32 {
      self.recent.pop_front();
    }
    self.recent.push_back(request.clone());
    Ok(true)
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  #[test]
  fn outgoing_size_limits_include_framing_and_never_publish_a_partial_oversized_message() {
    let mut output = Vec::new();
    write(&mut output, &"x".repeat(MAX_MESSAGE - 3))
      .expect("payload plus quotes and newline fits exactly");
    assert_eq!(output.len(), MAX_MESSAGE);
    assert_eq!(output.last(), Some(&b'\n'));
    let previous = output.clone();
    assert!(write(&mut output, &"x".repeat(MAX_MESSAGE - 2)).is_err());
    assert_eq!(output, previous, "no partial message reaches the pipe");
  }
  #[test]
  fn framing_bounds_allocation_and_rejects_partial_and_malformed_input() {
    let mut bytes = Vec::new();
    write(
      &mut bytes,
      &Request {
        version: VERSION,
        session: "s".into(),
        id: 1,
        command: Command::Stop,
      },
    )
    .expect("valid test fixture");
    assert_eq!(
      read::<Request>(&mut &bytes[..])
        .expect("valid test fixture")
        .expect("valid test fixture")
        .command,
      Command::Stop
    );
    assert!(read::<Request>(&mut &bytes[..bytes.len() - 1]).is_err());
    assert!(read::<Request>(&mut &vec![b'x'; MAX_MESSAGE + 1][..]).is_err());
    assert!(read::<Request>(&mut &b"{}\n"[..]).is_err());
    assert!(
      read::<Request>(&mut &b""[..])
        .expect("valid test fixture")
        .is_none()
    );
  }
  #[test]
  fn duplicates_do_not_reexecute_and_other_sessions_are_rejected() {
    let mut ledger = RequestLedger::default();
    let mut r = Request {
      version: VERSION,
      session: "s".into(),
      id: 1,
      command: Command::Stop,
    };
    assert!(ledger.accept(&r).expect("valid test fixture"));
    assert!(!ledger.accept(&r).expect("valid test fixture"));
    r.command = Command::Start;
    assert!(ledger.accept(&r).is_err());
    r.id = 2;
    r.session = "other".into();
    assert!(ledger.accept(&r).is_err());
    r.session = "s".into();
    r.version = 2;
    assert!(ledger.accept(&r).is_err());
  }
}
