//! Bounded Windows single-instance activation. Keep the former mutex/window
//! identifiers and COPYDATA tag so an installed older Flinch can be activated.

use std::{
  sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};
use windows::{
  Win32::{
    Foundation::{
      CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
    },
    System::{
      DataExchange::COPYDATASTRUCT,
      LibraryLoader::GetModuleHandleW,
      Threading::{CreateMutexW, GetCurrentThreadId},
    },
    UI::WindowsAndMessaging::{
      AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW,
      GWLP_USERDATA, GetWindowLongPtrW, GetWindowThreadProcessId, RegisterClassW, SMTO_ABORTIFHUNG,
      SMTO_BLOCK, SendMessageTimeoutW, SetWindowLongPtrW, UnregisterClassW, WM_CLOSE, WM_COPYDATA,
      WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
      WS_OVERLAPPED,
    },
  },
  core::HSTRING,
};

const IDENTIFIER: &str = "io.github.vulptik.flinch";
const ACTIVATION_TAG: usize = 1542;
type Shutdown = Box<dyn Fn() -> Result<(), String> + Send + Sync>;
struct Activation {
  activate: Box<dyn Fn() + Send + Sync>,
  shutdown: Option<Shutdown>,
  shutdown_requested: AtomicBool,
}
impl Activation {
  fn request_shutdown(&self) -> bool {
    let Some(shutdown) = &self.shutdown else {
      return false;
    };
    if !self.shutdown_requested.swap(true, Ordering::AcqRel)
      && let Err(error) = shutdown()
    {
      self.shutdown_requested.store(false, Ordering::Release);
      tracing::error!(%error, "Cannot submit Windows maintenance shutdown");
    }
    true
  }
}
struct Owned {
  mutex: usize,
  window: usize,
  data: usize,
  owner: u32,
  class: HSTRING,
}
pub struct Instance(Mutex<Option<Owned>>);

impl Instance {
  /// None means the existing process acknowledged activation; startup must end. `suffix`
  /// separates an isolated probe build's identity; this file is shared with the installer
  /// crate, so it must not reach app-only modules itself.
  pub fn acquire(
    suffix: &str,
    callback: impl Fn() + Send + Sync + 'static,
    shutdown: impl Fn() -> Result<(), String> + Send + Sync + 'static,
  ) -> anyhow::Result<Option<Self>> {
    let identifier = format!("{IDENTIFIER}{suffix}");
    Self::acquire_named(&identifier, callback, Some(Box::new(shutdown)))
  }

  pub fn acquire_named(
    identifier: &str,
    callback: impl Fn() + Send + Sync + 'static,
    shutdown: Option<Shutdown>,
  ) -> anyhow::Result<Option<Self>> {
    let class = HSTRING::from(format!("{identifier}-sic"));
    let title = HSTRING::from(format!("{identifier}-siw"));
    let name = HSTRING::from(format!("{identifier}-sim"));
    // SAFETY: strings remain alive throughout the calls; default DACL limits the
    // named object to the current Windows session. No mutex ownership is taken.
    let mutex = unsafe { CreateMutexW(None, false, &name)? };
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if exists {
      let activated = activate(&class, &title);
      // SAFETY: close only this process's newly opened kernel handle.
      if let Err(error) = unsafe { CloseHandle(mutex) } {
        tracing::error!(%error, "Cannot close secondary instance mutex");
      }
      activated?;
      return Ok(None);
    }
    let created = (|| -> anyhow::Result<Owned> {
      // SAFETY: module handle is borrowed from this running executable.
      let module = unsafe { GetModuleHandleW(None)? };
      let descriptor = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: module.into(),
        lpszClassName: windows::core::PCWSTR(class.as_ptr()),
        ..Default::default()
      };
      // SAFETY: descriptor and class name remain valid during registration.
      anyhow::ensure!(
        unsafe { RegisterClassW(&descriptor) } != 0,
        "Cannot register single-instance window: {}",
        windows::core::Error::from_thread()
      );
      // SAFETY: create an invisible top-level tool window on this platform owner
      // thread, with no pointer payload and no settings or capture UI resources.
      let window = match unsafe {
        CreateWindowExW(
          WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
          &class,
          &title,
          WS_OVERLAPPED,
          0,
          0,
          0,
          0,
          None,
          None,
          Some(module.into()),
          None,
        )
      } {
        Ok(window) => window,
        Err(error) => {
          if let Err(cleanup) = unsafe { UnregisterClassW(&class, Some(module.into())) } {
            tracing::error!(%cleanup, "Cannot unregister failed activation window class");
          }
          return Err(error.into());
        },
      };
      let data = Box::into_raw(Box::new(Activation {
        activate: Box::new(callback),
        shutdown,
        shutdown_requested: AtomicBool::new(false),
      }));
      // SAFETY: this pointer is owned by the Instance until its owner destroys
      // the window; the window callback only borrows it synchronously.
      unsafe {
        SetWindowLongPtrW(window, GWLP_USERDATA, data as isize);
      }
      // SAFETY: verify our owner window retained the exact callback allocation.
      if unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } != data as isize {
        // SAFETY: the allocation was not installed, so no window can borrow it.
        unsafe {
          drop(Box::from_raw(data));
        }
        // SAFETY: destroy the just-created window on this same owner thread.
        if let Err(error) = unsafe { DestroyWindow(window) } {
          tracing::error!(%error, "Cannot destroy failed activation window");
        }
        // SAFETY: this process owns the registered class and no callback uses it.
        if let Err(error) = unsafe { UnregisterClassW(&class, Some(module.into())) } {
          tracing::error!(%error, "Cannot unregister failed activation class");
        }
        anyhow::bail!("Windows did not retain the single-instance activation callback");
      }
      // SAFETY: thread identity is read only and recorded for owner-thread cleanup.
      let owner = unsafe { GetCurrentThreadId() };
      Ok(Owned {
        mutex: mutex.0 as usize,
        window: window.0 as usize,
        data: data as usize,
        owner,
        class,
      })
    })();
    match created {
      Ok(owned) => Ok(Some(Self(Mutex::new(Some(owned))))),
      Err(error) => {
        // SAFETY: no Instance owns the handle after failed construction.
        if let Err(cleanup) = unsafe { CloseHandle(mutex) } {
          tracing::error!(%cleanup, "Cannot release failed instance mutex");
        }
        Err(error)
      },
    }
  }

  pub fn shutdown(&self) {
    let owned = match self.0.lock() {
      Ok(mut slot) => slot.take(),
      Err(error) => {
        tracing::error!(%error, "Cannot take single-instance resources for shutdown");
        return;
      },
    };
    if let Some(owned) = owned {
      owned.close();
    }
  }
}
impl Drop for Instance {
  fn drop(&mut self) {
    self.shutdown();
  }
}
impl Owned {
  fn close(self) {
    // SAFETY: query this calling thread before touching an owner-bound window.
    let same_thread = unsafe { GetCurrentThreadId() } == self.owner;
    if same_thread {
      // SAFETY: window is exclusively owned by this guard on its creation thread.
      match unsafe { DestroyWindow(HWND(self.window as *mut _)) } {
        Ok(()) => {
          // SAFETY: destruction finished all callbacks, so this sole allocation
          // can be reclaimed without leaving a window holding a dangling pointer.
          unsafe {
            drop(Box::from_raw(self.data as *mut Activation));
          }
          // SAFETY: the borrowed executable module outlives this class cleanup.
          match unsafe { GetModuleHandleW(None) } {
            Ok(module) => {
              // SAFETY: our window is destroyed and this owner registered the class.
              if let Err(error) = unsafe { UnregisterClassW(&self.class, Some(module.into())) } {
                tracing::error!(%error, "Cannot unregister activation window class");
              }
            },
            Err(error) => {
              tracing::error!(%error, "Cannot resolve module for activation class cleanup")
            },
          }
        },
        Err(error) => {
          tracing::error!(%error, "Cannot destroy activation window; retain its callback until process exit")
        },
      }
    } else {
      tracing::error!(
        "Single-instance shutdown reached the wrong thread; retain its window callback until process exit"
      );
    }
    // SAFETY: no mutex ownership was taken; closing its kernel handle is thread-independent.
    if let Err(error) = unsafe { CloseHandle(HANDLE(self.mutex as *mut _)) } {
      tracing::error!(%error, "Cannot close instance mutex");
    }
  }
}

fn activate(class: &HSTRING, title: &HSTRING) -> anyhow::Result<()> {
  let deadline = Instant::now() + Duration::from_secs(5);
  let window = loop {
    // SAFETY: lookup uses owned, null-terminated strings and does not borrow window data.
    if let Ok(window) = unsafe { FindWindowW(class, title) } {
      break window;
    }
    anyhow::ensure!(
      Instant::now() < deadline,
      "Existing Flinch instance did not publish its activation window within five seconds"
    );
    std::thread::sleep(Duration::from_millis(10));
  };
  let mut process = 0;
  // SAFETY: Windows verifies the discovered window and writes only the supplied process ID.
  unsafe {
    GetWindowThreadProcessId(window, Some(&mut process));
  }
  // SAFETY: AllowSetForegroundWindow only records a process ID Windows may later refuse.
  grant_foreground(process, |process| unsafe {
    AllowSetForegroundWindow(process)
  });
  let bytes = b"|\0";
  let data = COPYDATASTRUCT {
    dwData: ACTIVATION_TAG,
    cbData: bytes.len() as u32,
    lpData: bytes.as_ptr().cast_mut().cast(),
  };
  let mut acknowledgment = 0;
  // SAFETY: payload lives through the synchronous call. Windows copies COPYDATA
  // into the receiving process; a hung owner cannot block us past two seconds.
  let sent = unsafe {
    SendMessageTimeoutW(
      window,
      WM_COPYDATA,
      WPARAM(0),
      LPARAM((&data as *const COPYDATASTRUCT) as isize),
      SMTO_ABORTIFHUNG | SMTO_BLOCK,
      2000,
      Some(&mut acknowledgment),
    )
  };
  anyhow::ensure!(
    sent.0 != 0 && acknowledgment == 1,
    "Existing Flinch instance did not acknowledge settings activation"
  );
  Ok(())
}

/// Lets the owner `process` take the foreground for settings. Windows grants this only while the
/// caller may set the foreground itself; a refusal is logged and activation continues.
fn grant_foreground(process: u32, allow: impl FnOnce(u32) -> windows::core::Result<()>) {
  if process != 0
    && let Err(error) = allow(process)
  {
    tracing::warn!(%error, "Cannot grant settings foreground activation");
  }
}

unsafe extern "system" fn window_proc(
  window: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  if message == WM_QUERYENDSESSION {
    // A query is not permission to exit: another app can cancel the session.
    return LRESULT(1);
  }
  if message == WM_ENDSESSION || message == WM_CLOSE {
    // SAFETY: the owner retains this allocation through DestroyWindow, including
    // all synchronous callbacks. The callback queues shutdown, never destroys us.
    let data = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const Activation;
    if message == WM_ENDSESSION && wparam.0 == 0 {
      return LRESULT(0);
    }
    if !data.is_null() && unsafe { (*data).request_shutdown() } {
      return LRESULT(0);
    }
  }
  if message == WM_COPYDATA && lparam.0 != 0 {
    // SAFETY: WM_COPYDATA supplies a Windows-owned COPYDATASTRUCT for this call.
    // Do not read caller payload strings or arbitrary pointers.
    let request = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
    if request.dwData == ACTIVATION_TAG && request.cbData <= 16384 {
      // SAFETY: userdata is installed before the owner loop runs and retained
      // until DestroyWindow has finished every synchronous callback.
      let data = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const Activation;
      if !data.is_null() {
        unsafe {
          ((*data).activate)();
        }
        return LRESULT(1);
      }
    }
    return LRESULT(0);
  }
  // SAFETY: forward unhandled messages with their original Windows parameters.
  unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  };
  use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
  };

  #[test]
  fn foreground_grants_skip_unknown_owners_and_tolerate_refusal() {
    let requested = std::cell::Cell::new(None);
    grant_foreground(0, |process| {
      requested.set(Some(process));
      Ok(())
    });
    assert_eq!(requested.get(), None, "no owner process to grant");
    grant_foreground(42, |process| {
      requested.set(Some(process));
      Ok(())
    });
    assert_eq!(requested.get(), Some(42));
    grant_foreground(7, |process| {
      requested.set(Some(process));
      Err(windows::core::Error::from_hresult(
        windows::Win32::Foundation::E_ACCESSDENIED,
      ))
    });
    assert_eq!(requested.get(), Some(7), "a refusal is logged, not fatal");
  }

  #[test]
  fn invalid_or_unpublished_activation_cannot_activate_or_shutdown_the_owner() {
    let activations = Arc::new(AtomicUsize::new(0));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let activated = activations.clone();
    let stopped = shutdowns.clone();
    let instance = Instance::acquire_named(
      &format!("flinch-activation-validation-test-{}", std::process::id()),
      move || {
        activated.fetch_add(1, Ordering::SeqCst);
      },
      Some(Box::new(move || {
        stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
      })),
    )
    .unwrap()
    .unwrap();
    let window = HWND(instance.0.lock().unwrap().as_ref().unwrap().window as *mut _);
    let bytes = vec![0u8; 16_385];
    let send = |tag, size| {
      let request = COPYDATASTRUCT {
        dwData: tag,
        cbData: size,
        lpData: bytes.as_ptr().cast_mut().cast(),
      };
      let mut acknowledgment = 99;
      // SAFETY: the fixture window and complete payload are owned by this test;
      // both buffers remain live until the bounded synchronous send completes.
      let result = unsafe {
        SendMessageTimeoutW(
          window,
          WM_COPYDATA,
          WPARAM(0),
          LPARAM((&request as *const COPYDATASTRUCT) as isize),
          SMTO_ABORTIFHUNG | SMTO_BLOCK,
          1000,
          Some(&mut acknowledgment),
        )
      };
      assert_ne!(result.0, 0);
      acknowledgment
    };
    assert_eq!(send(ACTIVATION_TAG + 1, 1), 0);
    assert_eq!(send(ACTIVATION_TAG, 16_385), 0);
    // SAFETY: temporarily unpublish only this test's callback on its owner
    // thread, then restore it before releasing the Instance's allocation.
    let data = unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
    let missing = send(ACTIVATION_TAG, 1);
    // SAFETY: the original allocation remains owned by instance throughout.
    unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, data) };
    assert_eq!(missing, 0);
    assert_eq!(activations.load(Ordering::SeqCst), 0);
    assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
    assert_eq!(send(ACTIVATION_TAG, 1), 1);
    assert_eq!(activations.load(Ordering::SeqCst), 1);
    assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
    instance.shutdown();
  }

  #[test]
  fn maintenance_shutdown_waits_for_confirmation_coalesces_and_retries_submission_errors() {
    for fail_first in [false, true] {
      let count = Arc::new(AtomicUsize::new(0));
      let calls = count.clone();
      let identity = format!("flinch-shutdown-test-{}-{fail_first}", std::process::id());
      let instance = Instance::acquire_named(
        &identity,
        || {},
        Some(Box::new(move || {
          let previous = calls.fetch_add(1, Ordering::SeqCst);
          if fail_first && previous == 0 {
            Err("injected event-loop rejection".into())
          } else {
            Ok(())
          }
        })),
      )
      .unwrap()
      .unwrap();
      let window = HWND(instance.0.lock().unwrap().as_ref().unwrap().window as *mut _);
      let send = |message, confirmed| {
        let mut acknowledgment = 0;
        // SAFETY: only this test's isolated hidden window receives these messages.
        // Its callback increments a counter; it cannot exit the test/app process.
        let result = unsafe {
          SendMessageTimeoutW(
            window,
            message,
            WPARAM(confirmed),
            LPARAM(1),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            1000,
            Some(&mut acknowledgment),
          )
        };
        assert_ne!(result.0, 0);
        acknowledgment
      };
      assert_eq!(send(WM_QUERYENDSESSION, 0), 1);
      send(WM_ENDSESSION, 0);
      assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "a cancelled query cannot close the app"
      );
      send(WM_ENDSESSION, 1);
      assert_eq!(count.load(Ordering::SeqCst), 1);
      send(WM_CLOSE, 0);
      send(WM_ENDSESSION, 1);
      assert_eq!(count.load(Ordering::SeqCst), if fail_first { 2 } else { 1 });
      instance.shutdown();
    }
    let callbacks = Activation {
      activate: Box::new(|| {}),
      shutdown: None,
      shutdown_requested: AtomicBool::new(false),
    };
    assert!(
      !callbacks.request_shutdown(),
      "installer activation windows have no app quit callback"
    );
  }

  #[test]
  fn secondary_instance_activates_owner_and_shutdown_releases_identity() {
    static SERIAL: AtomicUsize = AtomicUsize::new(0);
    let identity = format!(
      "flinch-instance-test-{}-{}",
      std::process::id(),
      SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    let owner_identity = identity.clone();
    let activated = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let owner_activated = activated.clone();
    let owner_stopped = stopped.clone();
    let worker = std::thread::spawn(move || {
      let instance = Instance::acquire_named(
        &owner_identity,
        move || {
          owner_activated.store(true, Ordering::Release);
        },
        None,
      )
      .expect("create isolated owner")
      .expect("first owner");
      ready_tx.send(()).expect("test receiver");
      let deadline = Instant::now() + Duration::from_secs(5);
      while !owner_stopped.load(Ordering::Acquire) && Instant::now() < deadline {
        let mut message = MSG::default();
        for _ in 0..128 {
          // SAFETY: this test thread owns the hidden window and its message loop.
          unsafe {
            if !PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
              break;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
          }
        }
        std::thread::sleep(Duration::from_millis(1));
      }
      instance.shutdown();
    });
    ready_rx
      .recv_timeout(Duration::from_secs(2))
      .expect("owner readiness");
    let secondary = Instance::acquire_named(&identity, || {}, None).expect("activate owner");
    assert!(secondary.is_none());
    assert!(activated.load(Ordering::Acquire));
    stopped.store(true, Ordering::Release);
    worker.join().expect("bounded owner loop");
    let reopened =
      Instance::acquire_named(&identity, || {}, None).expect("reacquire released identity");
    assert!(reopened.is_some());
    if let Some(instance) = reopened {
      instance.shutdown();
    }
  }
}
