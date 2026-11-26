//! Asynchronous MFT event subscription. Callbacks only publish completed results.
use crate::{EngineError, ErrorKind, Result};
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
  mpsc::{self, Receiver, SyncSender, TryRecvError},
};
use windows::{
  Win32::{Foundation::E_NOTIMPL, Media::MediaFoundation::*},
  core::Ref,
};
use windows_core::implement;
#[implement(IMFAsyncCallback)]
struct Callback {
  completed: SyncSender<IMFAsyncResult>,
  failed: Arc<AtomicBool>,
  closed: Arc<AtomicBool>,
}
impl IMFAsyncCallback_Impl for Callback_Impl {
  fn GetParameters(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
    Err(E_NOTIMPL.into())
  }
  fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
    if self.closed.load(Ordering::Acquire) {
      return Ok(());
    }
    match result.ok() {
      Ok(result) => {
        if self.completed.try_send(result.clone()).is_err() {
          self.failed.store(true, Ordering::Release);
        }
      },
      Err(_) => self.failed.store(true, Ordering::Release),
    }
    Ok(())
  }
}
pub struct AsyncEvents {
  source: IMFMediaEventGenerator,
  callback: IMFAsyncCallback,
  completed: Receiver<IMFAsyncResult>,
  failed: Arc<AtomicBool>,
  closed: Arc<AtomicBool>,
}
impl AsyncEvents {
  pub fn new(source: IMFMediaEventGenerator) -> Result<Self> {
    let (sender, completed) = mpsc::sync_channel(4);
    let failed = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let callback: IMFAsyncCallback = Callback {
      completed: sender,
      failed: failed.clone(),
      closed: closed.clone(),
    }
    .into();
    // SAFETY: subscription precedes StartOfStream, callback is bounded and retained until shutdown.
    unsafe { source.BeginGetEvent(&callback, None) }
      .map_err(|e| EngineError::native("MFT event subscription", e))?;
    Ok(Self {
      source,
      callback,
      completed,
      failed,
      closed,
    })
  }
  pub fn poll(&self) -> Result<Option<IMFMediaEvent>> {
    if self.failed.load(Ordering::Acquire) {
      return Err(EngineError::new(
        ErrorKind::Saturation,
        "MFT event callback",
        "Completed event could not enter its bounded owner queue",
      ));
    }
    let result = match self.completed.try_recv() {
      Ok(result) => result,
      Err(TryRecvError::Empty) => return Ok(None),
      Err(e) => {
        return Err(EngineError::protocol(format!(
          "MFT callback disconnected: {e}"
        )));
      },
    };
    // SAFETY: callback delivered a completed result; the owner finishes and renews the one subscription.
    unsafe {
      let event = self
        .source
        .EndGetEvent(&result)
        .map_err(|e| EngineError::native("MFT completed event", e))?;
      self
        .source
        .BeginGetEvent(&self.callback, None)
        .map_err(|e| EngineError::native("MFT event renewal", e))?;
      Ok(Some(event))
    }
  }
  pub fn close(&self) {
    self.closed.store(true, Ordering::Release);
  }
}
impl Drop for AsyncEvents {
  fn drop(&mut self) {
    self.close();
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  use windows::{
    Win32::System::Com::StructuredStorage::PROPVARIANT,
    core::{GUID, HRESULT, IUnknown},
  };
  #[implement(IMFMediaEventGenerator)]
  struct Source(IMFMediaEventQueue);
  impl IMFMediaEventGenerator_Impl for Source_Impl {
    fn GetEvent(
      &self,
      _: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    ) -> windows::core::Result<IMFMediaEvent> {
      Err(E_NOTIMPL.into())
    }
    fn BeginGetEvent(
      &self,
      callback: Ref<IMFAsyncCallback>,
      state: Ref<IUnknown>,
    ) -> windows::core::Result<()> {
      unsafe { self.0.BeginGetEvent(callback.ok()?, state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
      unsafe { self.0.EndGetEvent(result.ok()?) }
    }
    fn QueueEvent(
      &self,
      kind: u32,
      guid: *const GUID,
      status: HRESULT,
      value: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
      unsafe { self.0.QueueEventParamVar(kind, guid, status, value) }
    }
  }
  #[test]
  fn subscription_delivers_ordered_events_without_synchronous_get_or_inline_encoding() {
    let _com = super::super::Apartment::new().expect("COM");
    let _mf = super::super::MediaRuntime::new().expect("MF");
    let queue = unsafe { MFCreateEventQueue() }.expect("native event queue");
    let source: IMFMediaEventGenerator = Source(queue.clone()).into();
    assert!(unsafe { source.GetEvent(MF_EVENT_FLAG_NO_WAIT) }.is_err());
    let events = AsyncEvents::new(source).expect("asynchronous subscription");
    assert!(events.poll().expect("no event").is_none());
    let expected = [
      METransformNeedInput,
      METransformHaveOutput,
      METransformDrainComplete,
    ];
    for kind in expected {
      unsafe {
        queue.QueueEventParamVar(kind.0 as u32, &GUID::zeroed(), HRESULT(0), std::ptr::null())
      }
      .expect("queue event");
    }
    for kind in expected {
      let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
      loop {
        if let Some(event) = events.poll().expect("event owner") {
          assert_eq!(
            unsafe { event.GetType() }.expect("event type"),
            kind.0 as u32
          );
          break;
        }
        assert!(
          std::time::Instant::now() < deadline,
          "callback did not complete"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
      }
    }
    events.close();
    unsafe { queue.Shutdown() }.expect("bounded queue shutdown");
  }
  #[test]
  fn malformed_or_saturated_callback_cannot_disappear_as_a_successful_drain() {
    let (tx, rx) = mpsc::sync_channel(1);
    let failed = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let callback: IMFAsyncCallback = Callback {
      completed: tx,
      failed: failed.clone(),
      closed: closed.clone(),
    }
    .into();
    unsafe { callback.Invoke(None) }.expect("callback reports failure through flag");
    assert!(failed.load(Ordering::Acquire));
    assert!(rx.try_recv().is_err());
    failed.store(false, Ordering::Release);
    closed.store(true, Ordering::Release);
    unsafe { callback.Invoke(None) }.expect("late callback");
    assert!(!failed.load(Ordering::Acquire));
  }
}
