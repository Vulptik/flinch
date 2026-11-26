//! Routine diagnostics never write to a potentially blocked console on the control thread.
use std::{
  io::Write,
  sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, SyncSender},
  },
};
static LOGGER: OnceLock<Option<SyncSender<String>>> = OnceLock::new();
static LOST: AtomicU64 = AtomicU64::new(0);
pub fn lost() -> u64 {
  LOST.load(Ordering::Relaxed)
}
pub fn diagnostic(message: String) {
  let logger = LOGGER.get_or_init(|| spawn(std::io::stderr()).ok());
  publish(logger, message);
}
fn spawn(mut writer: impl Write + Send + 'static) -> std::io::Result<SyncSender<String>> {
  let (sender, receiver) = mpsc::sync_channel::<String>(64);
  std::thread::Builder::new()
    .name("video lab diagnostics".into())
    .spawn(move || {
      for message in receiver {
        if writer
          .write_all(message.as_bytes())
          .and_then(|_| writer.write_all(b"\n"))
          .is_err()
        {
          LOST.fetch_add(1, Ordering::Relaxed);
        }
      }
    })?;
  Ok(sender)
}
fn publish(logger: &Option<SyncSender<String>>, message: String) {
  if message.len() > 4096
    || logger
      .as_ref()
      .is_none_or(|sender| sender.try_send(message).is_err())
  {
    LOST.fetch_add(1, Ordering::Relaxed);
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  struct Blocked {
    entered: Option<SyncSender<()>>,
    release: Option<mpsc::Receiver<()>>,
  }
  impl Write for Blocked {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
      if let Some(entered) = self.entered.take() {
        entered.send(()).expect("writer entered");
      }
      if let Some(release) = self.release.take() {
        release
          .recv_timeout(std::time::Duration::from_secs(2))
          .expect("release writer");
      }
      Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }
  #[test]
  fn a_blocked_console_cannot_block_publication_or_hide_queue_losses() {
    let (entered, ready) = mpsc::sync_channel(1);
    let (release, gate) = mpsc::sync_channel(1);
    let sender = Some(
      spawn(Blocked {
        entered: Some(entered),
        release: Some(gate),
      })
      .expect("diagnostics owner"),
    );
    publish(&sender, "first".into());
    ready
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("blocked writer");
    let before = lost();
    for _ in 0..66 {
      publish(&sender, "bounded log".into());
    }
    publish(&sender, "x".repeat(4097));
    publish(&None, "owner unavailable".into());
    assert!(lost() >= before + 4);
    release.send(()).expect("release owned writer");
  }
}
