use std::sync::mpsc::{Receiver, Sender};

pub(crate) fn send_overlay_result<T>(
  operation: &'static str,
  tx: Option<Sender<Option<T>>>,
  result: Option<T>,
) {
  let Some(tx) = tx else {
    return;
  };

  if tx.send(result).is_err() {
    tracing::warn!(
      operation,
      "Overlay result receiver was dropped before the overlay finished"
    );
  }
}

pub(crate) fn recv_overlay_result<T>(
  operation: &'static str,
  rx: Receiver<Option<T>>,
) -> anyhow::Result<Option<T>> {
  rx.recv()
    .map_err(|error| anyhow::anyhow!("{operation} channel disconnected: {error}"))
}
