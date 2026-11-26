//! One owned worker runtime; native window threads submit through its handle.
use std::{future::Future, sync::OnceLock};
static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

#[cfg(test)]
pub(crate) fn test_runtime() -> &'static tokio::runtime::Runtime {
  static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
  RUNTIME.get_or_init(|| initialize().expect("one shared test worker runtime"))
}
pub fn initialize() -> anyhow::Result<tokio::runtime::Runtime> {
  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .thread_name("flinch-worker")
    .build()?;
  HANDLE
    .set(runtime.handle().clone())
    .map_err(|_| anyhow::anyhow!("Worker runtime already initialized"))?;
  Ok(runtime)
}
fn handle() -> &'static tokio::runtime::Handle {
  match HANDLE.get() {
    Some(handle) => handle,
    None => {
      tracing::error!("Worker task submitted before runtime initialization");
      std::process::exit(1);
    },
  }
}
pub fn spawn<F: Future + Send + 'static>(future: F) -> tokio::task::JoinHandle<F::Output>
where
  F::Output: Send + 'static,
{
  handle().spawn(future)
}
pub fn spawn_blocking<F, R>(task: F) -> tokio::task::JoinHandle<R>
where
  F: FnOnce() -> R + Send + 'static,
  R: Send + 'static,
{
  handle().spawn_blocking(task)
}
