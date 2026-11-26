//! Thread-owned one-shot animation wakeups. No worker, queued frames or global timer change.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_FAILED, WPARAM};
use windows::Win32::System::Threading::{
  CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateWaitableTimerExW, INFINITE,
  SetWaitableTimerEx, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{
  DispatchMessageW, IsWindow, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
  PeekMessageW, QS_ALLINPUT, SendMessageW, TranslateMessage, WM_QUIT, WM_TIMER,
};

pub(crate) struct AnimationClock(HANDLE);

impl AnimationClock {
  pub fn new() -> windows::core::Result<Self> {
    // SAFETY: unnamed, non-inherited timer with no callback; handle stays on its owning thread.
    unsafe {
      CreateWaitableTimerExW(
        None,
        None,
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
        TIMER_ALL_ACCESS.0,
      )
    }
    .map(Self)
  }

  fn arm(&self, delay: Duration) -> windows::core::Result<()> {
    let ticks = delay.as_nanos().div_ceil(100).clamp(1, i64::MAX as u128) as i64;
    let due = -ticks;
    // SAFETY: owned live timer, relative one-shot deadline, no APC or resume-from-sleep request.
    unsafe { SetWaitableTimerEx(self.0, &due, 0, None, None, None, 0) }
  }

  fn cancel(&self) -> windows::core::Result<()> {
    // SAFETY: owned timer, no APC callback. Cancelling prevents an unused deadline from firing.
    unsafe { CancelWaitableTimer(self.0) }
  }

  pub fn run(
    &self,
    hwnd: HWND,
    timer_id: usize,
    mut deadline: impl FnMut(Instant) -> Option<Instant>,
  ) -> windows::core::Result<()> {
    let mut last_tick = Instant::now();
    let mut armed_deadline = None;
    loop {
      // Bound each batch so sustained input cannot starve an animation deadline.
      for _ in 0..64 {
        let mut message = MSG::default();
        // SAFETY: output is initialized and messages belong to this thread.
        if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
          break;
        }
        if message.message == WM_QUIT {
          return Ok(());
        }
        // SAFETY: standard dispatch; no window-state reference is retained across callbacks.
        unsafe {
          // FALSE means this was not a translatable key message, not an error.
          let _ = TranslateMessage(&message);
          DispatchMessageW(&message);
        }
      }
      // SAFETY: only the owning thread can destroy this HWND while dispatching above.
      if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Ok(());
      }
      let next = deadline(last_tick);
      // SAFETY: callback may cancel its window; never wait for a destroyed overlay.
      if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Ok(());
      }
      if let Some(next) = next {
        let now = Instant::now();
        if next <= now {
          if armed_deadline.take().is_some() {
            self.cancel()?;
          }
          last_tick = now;
          // SAFETY: synchronous dispatch to this thread's live window; no queued frame backlog.
          unsafe { SendMessageW(hwnd, WM_TIMER, Some(WPARAM(timer_id)), Some(LPARAM(0))) };
          continue;
        }
        if armed_deadline != Some(next) {
          self.arm(next.saturating_duration_since(now))?;
          armed_deadline = Some(next);
        }
      }
      if next.is_none() && armed_deadline.take().is_some() {
        self.cancel()?;
      }
      let handles = [self.0];
      // SAFETY: owned timer plus this thread's queue. Messages wake both active and idle waits.
      // Idle has no timer; active waits have a one-shot deadline. WM_CLOSE/WM_QUIT interrupt either.
      let result = unsafe {
        MsgWaitForMultipleObjectsEx(
          next.map(|_| handles.as_slice()),
          INFINITE,
          QS_ALLINPUT,
          MWMO_INPUTAVAILABLE,
        )
      };
      if result == WAIT_FAILED {
        return Err(windows::core::Error::from_thread());
      }
    }
  }
}

impl Drop for AnimationClock {
  fn drop(&mut self) {
    // SAFETY: this thread exclusively owns the handle; no other thread waits on it.
    if let Err(error) = unsafe { CloseHandle(self.0) } {
      tracing::warn!(%error, "Animation timer handle cleanup failed");
    }
  }
}

const MAX_ANIMATION_HZ: u32 = 360;

fn interval_from_hz(hz: u32) -> Option<Duration> {
  // Windows uses 0/1 for an unspecified hardware default, not actual refresh rates.
  (hz > 1).then(|| Duration::from_secs_f64(1.0 / f64::from(hz.min(MAX_ANIMATION_HZ))))
}

pub(crate) struct MonitorCadence {
  cursor_error_reported: bool,
  monitor: Option<isize>,
  interval: Duration,
}

impl Default for MonitorCadence {
  fn default() -> Self {
    Self {
      cursor_error_reported: false,
      monitor: None,
      interval: Duration::from_secs_f64(1.0 / 60.0),
    }
  }
}

impl MonitorCadence {
  pub fn at_cursor(&mut self) -> Duration {
    let mut point = windows::Win32::Foundation::POINT::default();
    // SAFETY: initialized physical-screen point; no dependence on overlay DPI coordinate mapping.
    match unsafe { windows::Win32::UI::WindowsAndMessaging::GetPhysicalCursorPos(&mut point) } {
      Ok(()) => {
        self.cursor_error_reported = false;
        self.at_point(point.x, point.y)
      },
      Err(error) => {
        if !self.cursor_error_reported {
          tracing::warn!(%error, "Physical cursor position unavailable; retaining last animation cadence");
          self.cursor_error_reported = true;
        }
        self.interval
      },
    }
  }

  pub fn at_point(&mut self, x: i32, y: i32) -> Duration {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
    // SAFETY: physical desktop coordinates; returns a borrowed system monitor handle.
    let monitor = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
    self.for_monitor(monitor.0 as isize, || monitor_frame_interval(monitor))
  }

  fn for_monitor(&mut self, identity: isize, query: impl FnOnce() -> Duration) -> Duration {
    if self.monitor != Some(identity) {
      self.monitor = Some(identity);
      self.interval = query();
    }
    self.interval
  }
}

fn monitor_frame_interval(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> Duration {
  use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, MONITORINFOEXW,
  };
  let default = Duration::from_secs_f64(1.0 / 60.0);
  let mut info = MONITORINFOEXW::default();
  info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
  // SAFETY: initialized extended monitor info; handle is borrowed from MonitorFromPoint.
  if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
    tracing::warn!("Monitor information unavailable; animation clock targets 60 Hz");
    return default;
  }
  let mut mode = DEVMODEW {
    dmSize: std::mem::size_of::<DEVMODEW>() as u16,
    ..Default::default()
  };
  // SAFETY: API-provided null-terminated device name and initialized mode output.
  if !unsafe {
    EnumDisplaySettingsW(
      windows::core::PCWSTR(info.szDevice.as_ptr()),
      ENUM_CURRENT_SETTINGS,
      &mut mode,
    )
  }
  .as_bool()
  {
    tracing::warn!("Monitor refresh query failed; animation clock targets 60 Hz");
    return default;
  }
  match interval_from_hz(mode.dmDisplayFrequency) {
    Some(interval) => {
      if mode.dmDisplayFrequency > MAX_ANIMATION_HZ {
        tracing::warn!(
          reported_hz = mode.dmDisplayFrequency,
          cap_hz = MAX_ANIMATION_HZ,
          "Animation cadence capped to bound redraw cost"
        );
      }
      interval
    },
    None => {
      tracing::warn!(
        reported_hz = mode.dmDisplayFrequency,
        "Monitor refresh rate unspecified; animation clock targets 60 Hz"
      );
      default
    },
  }
}

pub(crate) fn frame_interval_at_point(x: i32, y: i32) -> Duration {
  MonitorCadence::default().at_point(x, y)
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::Foundation::WAIT_OBJECT_0;
  use windows::Win32::System::Threading::WaitForSingleObject;

  #[test]
  fn cadence_handles_rates_defaults_caps_and_monitor_switches() {
    for hz in [24, 60, 90, 120, 144, 240, 360] {
      assert_eq!(
        interval_from_hz(hz),
        Some(Duration::from_secs_f64(1.0 / f64::from(hz)))
      );
    }
    assert_eq!(interval_from_hz(0), None);
    assert_eq!(interval_from_hz(1), None);
    assert_eq!(interval_from_hz(480), interval_from_hz(360));
    let mut cadence = MonitorCadence::default();
    let first = Duration::from_secs_f64(1.0 / 60.0);
    let second = Duration::from_secs_f64(1.0 / 90.0);
    assert_eq!(cadence.for_monitor(1, || first), first);
    assert_eq!(
      cadence.for_monitor(1, || panic!("unchanged monitor must not re-query")),
      first
    );
    assert_eq!(cadence.for_monitor(2, || second), second);
    assert_eq!(cadence.for_monitor(1, || first), first);
  }

  #[test]
  fn one_shot_clock_signals_once_and_rearms() {
    let clock = AnimationClock::new().expect("high-resolution timer");
    for _ in 0..3 {
      clock.arm(Duration::from_millis(2)).expect("arm timer");
      // SAFETY: live timer owned by this test. Both waits have explicit finite bounds.
      assert_eq!(unsafe { WaitForSingleObject(clock.0, 1000) }, WAIT_OBJECT_0);
      assert_ne!(unsafe { WaitForSingleObject(clock.0, 0) }, WAIT_OBJECT_0);
    }
    clock
      .arm(Duration::from_secs(1))
      .expect("arm idle deadline");
    clock.cancel().expect("cancel unused deadline");
    // SAFETY: owned cancelled timer with a bounded wait; cancellation must prevent signaling.
    assert_eq!(
      unsafe { WaitForSingleObject(clock.0, 5) },
      windows::Win32::Foundation::WAIT_TIMEOUT
    );
  }

  #[test]
  fn message_loop_advances_frames_and_accepts_idle_close() {
    use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, PostMessageW, WM_CLOSE};
    let clock = AnimationClock::new().expect("clock");
    let hwnd = crate::overlay::gdi::window::create_hud_window(
      "STATIC",
      "Animation clock test",
      0,
      0,
      16,
      16,
      std::ptr::null_mut(),
    )
    .expect("hidden window");
    let mut previous = None;
    let mut ticks = 0;
    clock
      .run(hwnd, 101, |last_tick| {
        if previous != Some(last_tick) {
          if previous.is_some() {
            ticks += 1;
          }
          previous = Some(last_tick);
        }
        if ticks == 3 {
          // SAFETY: hidden test window is owned by this test thread.
          unsafe { DestroyWindow(hwnd) }.expect("close animated test window");
          None
        } else {
          Some(last_tick + Duration::from_millis(2))
        }
      })
      .expect("animated message loop");
    assert_eq!(ticks, 3);
    let idle = crate::overlay::gdi::window::create_hud_window(
      "STATIC",
      "Idle clock test",
      0,
      0,
      16,
      16,
      std::ptr::null_mut(),
    )
    .expect("idle window");
    let idle_address = idle.0 as usize;
    let closer = std::thread::spawn(move || {
      std::thread::sleep(Duration::from_millis(20));
      // SAFETY: post to the hidden test HWND; Windows reports failure if it was destroyed.
      unsafe {
        PostMessageW(
          Some(HWND(idle_address as *mut _)),
          WM_CLOSE,
          WPARAM(0),
          LPARAM(0),
        )
      }
    });
    clock
      .run(idle, 101, |_| None)
      .expect("idle close wakes message loop");
    closer.join().expect("close worker").expect("idle close");
  }

  #[test]
  #[ignore = "explicit native animation clock pacing probe"]
  fn high_resolution_pacing_probe() {
    use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
    let clock = AnimationClock::new().expect("clock");
    let hwnd = crate::overlay::gdi::window::create_hud_window(
      "STATIC",
      "Pacing probe",
      0,
      0,
      16,
      16,
      std::ptr::null_mut(),
    )
    .expect("hidden window");
    let mut previous = None;
    let mut intervals = Vec::new();
    let step = Duration::from_secs_f64(1.0 / 144.0);
    clock
      .run(hwnd, 101, |last_tick| {
        if previous != Some(last_tick) {
          if let Some(previous) = previous {
            intervals.push(last_tick.duration_since(previous).as_micros() as u64);
          }
          previous = Some(last_tick);
        }
        if intervals.len() == 64 {
          // SAFETY: hidden window is owned by this serial probe thread.
          unsafe { DestroyWindow(hwnd) }.expect("close probe window");
          None
        } else {
          Some(last_tick + step)
        }
      })
      .expect("message loop");
    intervals.sort_unstable();
    println!(
      "target_hz=144 samples=64 interval_p50_us={} interval_p95_us={}",
      intervals[31], intervals[60]
    );
  }
}
