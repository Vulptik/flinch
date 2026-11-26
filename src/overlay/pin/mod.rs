//! Pin to Screen: screenshots shown in borderless, always-on-top windows.
//!
//! Every pin belongs to one long-lived host thread, started by the first pin, that owns the pin
//! windows and a hidden host window. Other threads only queue a request and post `REQUEST`, so
//! no pin state is ever shared. A pin shows its image at 1:1 physical pixels, so Windows display
//! scaling never blurs it; the wheel zooms it, Ctrl+wheel changes its opacity, and a
//! click-through pin lets clicks reach the app below until Ctrl is held over it. Pins are kept
//! in memory only, while Flinch runs, and at most `MAX_PINS` at once.
//!
//! Pins are drawn on the GPU through DirectComposition (`gpu`): zooming, fading and moving are
//! compositor work. The host thread animates them itself, frame by frame (`motion`): each frame
//! sizes every moving window and lays out its visuals, which keep their pixels anchored to the
//! window's top-left, then commits them all at once.

mod frame;
mod gpu;
#[cfg(test)]
mod host_tests;
mod motion;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
  COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
  GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
  GetAsyncKeyState, GetCapture, GetKeyState, ReleaseCapture, SetCapture,
};
use windows::Win32::UI::WindowsAndMessaging::{
  AppendMenuW, CS_DBLCLKS, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
  DestroyWindow, GWL_EXSTYLE, GWLP_USERDATA, GetCursorPos, GetWindowLongPtrW, GetWindowRect,
  IDC_SIZEALL, KillTimer, LWA_ALPHA, LoadCursorW, MF_CHECKED, MF_SEPARATOR, MF_STRING,
  PostMessageW, RegisterClassExW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOOWNERZORDER,
  SWP_NOZORDER, SetForegroundWindow, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW,
  SetWindowPos, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP,
  WM_CAPTURECHANGED, WM_CONTEXTMENU, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONDBLCLK,
  WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY, WM_NULL, WM_TIMER,
  WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
  WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::app::context::AppHandle;
use crate::overlay::animation_clock::{AnimationClock, MonitorCadence};
use crate::overlay::chrome::{Renderer, Scene, TextStyle};
use crate::overlay::gdi::layered::LayeredBitmap;
use frame::{Bounds, OPACITIES, ZOOMS};
use gpu::{Gpu, Layout, Visuals};
use motion::{Presence, Ramp, Slide, View};

/// More would only stack screenshots nobody can see; each also holds its image in memory.
pub(crate) const MAX_PINS: usize = 20;
/// Requests waiting for the host thread; it drains them as soon as it runs.
const MAX_QUEUED: usize = 64;
const REQUEST: u32 = WM_APP + 40;
/// Rebuilds every pin on a new device. Posted, never run inside a pin's own message handling,
/// which could otherwise lose its pin midway.
const RECOVER: u32 = WM_APP + 41;
const GRAB_TIMER: usize = 1;
const GRAB_TICK_MS: u32 = 50;
/// Sent by the animation clock each frame while anything moves.
const FRAME_TIMER: usize = 2;
/// A lost GPU shows nothing new; while pins exist the host checks for one this often.
const DEVICE_TIMER: usize = 3;
const DEVICE_CHECK_MS: u32 = 2000;
const START_TIMEOUT: Duration = Duration::from_secs(2);
const HOST_CLASS: &str = "FlinchPinHost";
const PIN_CLASS: &str = "FlinchPin";
/// The zoom or opacity label, like the selection's size label: inset from the image's corner.
const LABEL_INSET: f32 = 8.0;
const LABEL_HEIGHT: f32 = 36.0;
const LABEL_PADDING: f32 = 14.0;

const COPY: usize = 1;
const SAVE: usize = 2;
const ORIGINAL_SIZE: usize = 3;
const CLICK_THROUGH: usize = 4;
const CLOSE: usize = 5;
const CLOSE_ALL: usize = 6;

/// A screenshot to pin, straight RGBA.
pub(crate) struct PinImage {
  pub(crate) rgba: Arc<Vec<u8>>,
  pub(crate) width: u32,
  pub(crate) height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
  /// The image's top-left in virtual-screen pixels, where it was selected.
  At(i32, i32),
  /// Centered on the pointer's monitor, for a pin opened from a file.
  Centered,
}

enum Request {
  Open {
    app: AppHandle,
    image: PinImage,
    placement: Placement,
  },
}

struct Host {
  window: isize,
  queue: Arc<Mutex<VecDeque<Request>>>,
}

static HOST: Mutex<Option<Host>> = Mutex::new(None);

/// Reads the pointer's screen position, if Windows reports one.
type PointerReader = fn() -> Option<(i32, i32)>;

thread_local! {
  /// The host thread's pins, oldest first.
  static PINS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
  static HOST_WINDOW: RefCell<isize> = const { RefCell::new(0) };
  static QUEUE: RefCell<Option<Arc<Mutex<VecDeque<Request>>>>> = const { RefCell::new(None) };
  /// The device every pin draws with, created with the first pin and rebuilt when lost.
  static GPU: RefCell<Option<Gpu>> = const { RefCell::new(None) };
  static LABELS: RefCell<Renderer> = RefCell::new(Renderer::default());
  /// Every monitor's bounds, for clipping pin windows; listed again on display changes.
  static SCREENS: RefCell<Vec<Bounds>> = const { RefCell::new(Vec::new()) };
  /// Makes the pins' device. Quiet tests swap in WARP, which draws the same on every machine.
  static DEVICE: Cell<fn() -> Result<Gpu, String>> = const { Cell::new(Gpu::new) };
  /// Whether new pins are shown; quiet tests keep their windows hidden.
  static SHOWN: Cell<bool> = const { Cell::new(true) };
  /// Reads the pointer; quiet tests script it.
  static POINTER: Cell<PointerReader> = const { Cell::new(system_cursor) };
  /// Windows' animation setting, unless quiet tests fix it.
  static ANIMATES: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Whether pins animate: Windows' setting, read at each change.
fn animates() -> bool {
  match ANIMATES.with(Cell::get) {
    Some(fixed) => fixed,
    None => crate::overlay::motion::windows_animates(),
  }
}

/// Pins an image. A failure is reported to the user; the pin simply does not appear.
pub(crate) fn open(app: &AppHandle, image: PinImage, placement: Placement) {
  open_with(app, image, placement, || {});
}

/// `open`, with `setup` run first on a host thread it starts.
fn open_with(app: &AppHandle, image: PinImage, placement: Placement, setup: fn()) {
  let request = Request::Open {
    app: app.clone(),
    image,
    placement,
  };
  if let Err(error) = submit(request, setup) {
    tracing::error!(%error, "Cannot pin the screenshot");
    report(app, "notification_error_pin");
  }
}

fn report(app: &AppHandle, body_key: &str) {
  let title = crate::i18n::tray(&crate::i18n::get_lang(app)).pin;
  crate::notifications::show_error(app, title, body_key);
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
  match mutex.lock() {
    Ok(guard) => guard,
    Err(poisoned) => poisoned.into_inner(),
  }
}

fn submit(request: Request, setup: fn()) -> Result<(), String> {
  let mut host = lock(&HOST);
  let mut request = Some(request);
  // A host thread that ended (it never does on its own) is replaced once.
  for _ in 0..2 {
    if host.is_none() {
      *host = Some(start(setup)?);
    }
    let Some(running) = host.as_ref() else {
      return Err("Pin host did not start".into());
    };
    {
      let mut queue = lock(&running.queue);
      if queue.len() >= MAX_QUEUED {
        return Err("Too many pin requests are waiting".into());
      }
      if let Some(request) = request.take() {
        queue.push_back(request);
      }
    }
    // SAFETY: posting to the host window; a window that no longer exists fails harmlessly.
    if unsafe {
      PostMessageW(
        Some(HWND(running.window as *mut _)),
        REQUEST,
        WPARAM(0),
        LPARAM(0),
      )
    }
    .is_ok()
    {
      return Ok(());
    }
    tracing::warn!("Pin host window is gone; starting a new one");
    request = lock(&running.queue).pop_back();
    *host = None;
  }
  Err("Pin host is not responding".into())
}

fn start(setup: fn()) -> Result<Host, String> {
  let queue = Arc::new(Mutex::new(VecDeque::new()));
  let thread_queue = Arc::clone(&queue);
  let (ready, started) = std::sync::mpsc::sync_channel(1);
  std::thread::Builder::new()
    .name("pin-host".into())
    .spawn(move || {
      setup();
      let started = AnimationClock::new()
        .map_err(|error| format!("Cannot create the pin animation clock: {error}"))
        .and_then(|clock| create_host(thread_queue).map(|window| (clock, window)));
      let (clock, window) = match started {
        Ok(started) => started,
        Err(error) => {
          let _ = ready.send(Err(error));
          return;
        },
      };
      if ready.send(Ok(window.0 as isize)).is_err() {
        // SAFETY: the requester gave up; this thread still owns and releases its window.
        let _ = unsafe { DestroyWindow(window) };
        return;
      }
      // The host's message loop for the app's lifetime: idle without a deadline, and one frame
      // per refresh of the pointer's monitor while anything animates.
      let mut cadence = MonitorCadence::default();
      if let Err(error) = clock.run(window, FRAME_TIMER, |last| {
        animating(Instant::now()).then(|| last + cadence.at_cursor())
      }) {
        tracing::error!(%error, "Pin host message loop failed; open pins stop responding");
      }
    })
    .map_err(|error| format!("Cannot start the pin thread: {error}"))?;
  let window = started
    .recv_timeout(START_TIMEOUT)
    .map_err(|error| format!("Pin thread did not start: {error}"))??;
  Ok(Host { window, queue })
}

fn wide(text: &str) -> Vec<u16> {
  text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn register(
  class: &str,
  procedure: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
  double_clicks: bool,
) -> Result<(), String> {
  let name = wide(class);
  // SAFETY: a stock system cursor; no ownership is transferred.
  let cursor = unsafe { LoadCursorW(None, IDC_SIZEALL) }.unwrap_or_default();
  let instance = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
    .map_err(|error| format!("GetModuleHandleW failed: {error}"))?;
  let class = WNDCLASSEXW {
    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
    style: if double_clicks {
      CS_DBLCLKS
    } else {
      Default::default()
    },
    lpfnWndProc: Some(procedure),
    hInstance: instance.into(),
    hCursor: cursor,
    lpszClassName: PCWSTR(name.as_ptr()),
    ..Default::default()
  };
  // SAFETY: the class structure and its name outlive the call.
  if unsafe { RegisterClassExW(&class) } == 0 {
    // SAFETY: reads this thread's last error, set by the call above.
    let error = unsafe { GetLastError() };
    // Classes belong to the process: a host started again, after its thread ended, finds them
    // registered by the first one.
    if error == ERROR_CLASS_ALREADY_EXISTS {
      return Ok(());
    }
    return Err(format!(
      "RegisterClassExW failed for {}: {error:?}",
      String::from_utf16_lossy(&name)
    ));
  }
  Ok(())
}

fn create_host(queue: Arc<Mutex<VecDeque<Request>>>) -> Result<HWND, String> {
  register(HOST_CLASS, host_proc, false)?;
  register(PIN_CLASS, pin_proc, true)?;
  let class = wide(HOST_CLASS);
  // SAFETY: a hidden top-level window owned by this thread; it receives display broadcasts.
  let window = unsafe {
    CreateWindowExW(
      WS_EX_TOOLWINDOW,
      PCWSTR(class.as_ptr()),
      PCWSTR::null(),
      WS_POPUP,
      0,
      0,
      0,
      0,
      None,
      None,
      None,
      None,
    )
  }
  .map_err(|error| format!("Cannot create the pin host window: {error}"))?;
  HOST_WINDOW.with(|host| *host.borrow_mut() = window.0 as isize);
  QUEUE.with(|slot| *slot.borrow_mut() = Some(queue));
  Ok(window)
}

fn host_window() -> HWND {
  HWND(HOST_WINDOW.with(|host| *host.borrow()) as *mut _)
}

unsafe extern "system" fn host_proc(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  match message {
    REQUEST => {
      let requests: Vec<Request> = QUEUE.with(|slot| {
        slot
          .borrow()
          .as_ref()
          .map(|queue| lock(queue).drain(..).collect())
          .unwrap_or_default()
      });
      for request in requests {
        match request {
          Request::Open {
            app,
            image,
            placement,
          } => {
            if PINS.with(|pins| pins.borrow().len()) >= MAX_PINS {
              tracing::warn!("Pin limit reached");
              report(&app, "notification_error_pin_limit");
            } else if let Err(error) = create_pin(app.clone(), image, placement) {
              tracing::error!(%error, "Cannot create a pin window");
              report(&app, "notification_error_pin");
            }
          },
        }
      }
      LRESULT(0)
    },
    RECOVER => {
      if with_gpu(|gpu| Ok(gpu.lost())).unwrap_or(true) {
        recover();
      }
      LRESULT(0)
    },
    WM_TIMER if wparam.0 == GRAB_TIMER => {
      grab_tick();
      LRESULT(0)
    },
    WM_TIMER if wparam.0 == FRAME_TIMER => {
      tick(Instant::now());
      LRESULT(0)
    },
    WM_TIMER if wparam.0 == DEVICE_TIMER => {
      if with_gpu(|gpu| Ok(gpu.lost())).unwrap_or(true) {
        recover();
      }
      LRESULT(0)
    },
    // SAFETY: default handling for this thread's own hidden window.
    _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
  }
}

/// Runs `use_gpu` with the pins' device, creating it first if there is none yet.
fn with_gpu<T>(use_gpu: impl FnOnce(&Gpu) -> Result<T, String>) -> Result<T, String> {
  GPU.with(|slot| {
    let mut slot = slot.borrow_mut();
    if slot.is_none() {
      *slot = Some(DEVICE.with(Cell::get)()?);
    }
    match slot.as_ref() {
      Some(gpu) => use_gpu(gpu),
      None => Err("Pin GPU is missing".into()),
    }
  })
}

fn commit() {
  if let Err(error) = with_gpu(Gpu::commit) {
    tracing::error!(%error, "Cannot show the pins' latest frame");
    request_recovery();
  }
}

/// Asks the host to check the device and rebuild every pin if it was lost.
fn request_recovery() {
  // SAFETY: posting to this thread's own host window.
  if let Err(error) = unsafe { PostMessageW(Some(host_window()), RECOVER, WPARAM(0), LPARAM(0)) } {
    tracing::error!(%error, "Cannot schedule the pins' device check");
  }
}

/// The GPU stopped working: every pin is rebuilt on a new device from its kept pixels. Pins that
/// cannot be rebuilt close, with the usual message.
fn recover() {
  tracing::warn!("The pins' GPU device was lost; rebuilding every pin");
  // SAFETY: pins of this thread.
  each_pin(|hwnd| {
    if let Some(pin) = unsafe { pin_of(hwnd) } {
      pin.visuals = None;
    }
  });
  GPU.with(|slot| *slot.borrow_mut() = None);
  let now = Instant::now();
  let mut failed = Vec::new();
  each_pin(|hwnd| {
    // SAFETY: pins of this thread.
    let Some(pin) = (unsafe { pin_of(hwnd) }) else {
      return;
    };
    let rebuilt = pin.build(hwnd).and_then(|()| pin.present(hwnd, now));
    if let Err(error) = rebuilt {
      tracing::error!(%error, "Cannot rebuild a pin on the new GPU device");
      failed.push((hwnd, pin.app.clone()));
    }
  });
  commit();
  // One message however many pins it closes.
  if let Some((_, app)) = failed.first() {
    report(app, "notification_error_pin");
  }
  for (hwnd, _) in failed {
    // SAFETY: this thread's own pin window; WM_NCDESTROY releases its state.
    let _ = unsafe { DestroyWindow(hwnd) };
  }
}

struct Pin {
  app: AppHandle,
  lang: String,
  image: PinImage,
  margin: i32,
  /// The monitor's UI scale when the pin opened, for its label.
  scale: f32,
  /// The zoom and opacity steps the pin is heading to.
  zoom: usize,
  opacity: usize,
  view: View,
  fade: Ramp,
  presence: Presence,
  slide: Option<Slide>,
  /// When the zoom or opacity label appeared.
  label: Option<Instant>,
  /// The pin's rectangle as last placed, margin included: left, top, width, height.
  placed: Option<(i32, i32, i32, i32)>,
  /// The part of it the window shows, clipped to the screens (`frame::shown`).
  window: Option<Bounds>,
  click_through: bool,
  /// A click-through pin taken back while Ctrl is held over it.
  grabbed: bool,
  /// The pointer's last position during a drag.
  drag: Option<(i32, i32)>,
  visuals: Option<Visuals>,
  /// The sharp redraw after a zoom failed: the pin keeps the pixels it shows rather than retrying
  /// every frame, until the next zoom or a rebuilt device.
  redraw_failed: bool,
}

impl Pin {
  /// The image surface's size for a zoom: below 100% the downscaled pixels, from 100% up the
  /// original ones, which the compositor enlarges.
  fn rendered_size(&self, zoom: f32) -> (u32, u32) {
    frame::scaled_size(self.image.width, self.image.height, zoom.min(1.0))
  }

  /// Builds the pin's visuals on the current device.
  fn build(&mut self, hwnd: HWND) -> Result<(), String> {
    let rendered = self.rendered_size(self.view.zoom_target());
    let inset = (LABEL_INSET * self.scale).round() as i32;
    let visuals = with_gpu(|gpu| {
      Visuals::new(
        gpu,
        hwnd,
        &self.image.rgba,
        (self.image.width, self.image.height),
        self.margin,
        inset,
        rendered,
      )
    })?;
    self.visuals = Some(visuals);
    self.redraw_failed = false;
    if self.label.is_some() {
      self.show_label()?;
    }
    Ok(())
  }

  /// The window size the image needs at `zoom`: the image and its margin on every side.
  fn window_for(&self, zoom: f32) -> (i32, i32) {
    let (width, height) = frame::scaled_size(self.image.width, self.image.height, zoom);
    (
      width as i32 + 2 * self.margin,
      height as i32 + 2 * self.margin,
    )
  }

  /// Whether anything still moves, or the image waits for its sharp redraw or the window for
  /// its settled size.
  fn animating(&self, now: Instant) -> bool {
    // A closing pin needs one more frame after its fade, the one that destroys its window.
    self.presence.closing()
      || self.view.zooming(now)
      || self
        .placed
        .is_some_and(|placed| (placed.2, placed.3) != self.window_for(self.view.zoom_target()))
      || !self.fade.done(now)
      || !self.presence.done(now)
      || self.slide.is_some()
      || self.label.is_some()
      || (!self.redraw_failed
        && self
          .visuals
          .as_ref()
          .is_some_and(|visuals| visuals.rendered() != self.rendered_size(self.view.zoom_target())))
  }

  /// Places the window and lays out the visuals for `now`; the caller commits.
  fn present(&mut self, hwnd: HWND, now: Instant) -> Result<(), String> {
    let ((x, y), zoom) = self.view.at(now);
    let (dx, dy) = self.slide.map_or((0.0, 0.0), |slide| slide.offset(now));
    let (width, height) = frame::scaled_size(self.image.width, self.image.height, zoom);
    let m = self.margin;
    let size = frame::window_size(
      self.window_for(zoom),
      self.window_for(self.view.zoom_target()),
      self.placed.map(|placed| (placed.2, placed.3)),
      self.view.zooming(now),
    );
    let rect = (
      (x + dx).round() as i32 - m,
      (y + dy).round() as i32 - m,
      size.0,
      size.1,
    );
    let label = self
      .label
      .and_then(|shown| motion::label_alpha(now.saturating_duration_since(shown)))
      .unwrap_or(0.0);
    let Some(visuals) = &self.visuals else {
      return Err("Pin visuals are missing".into());
    };
    let window = frame::shown(
      Bounds {
        left: rect.0,
        top: rect.1,
        right: rect.0 + rect.2,
        bottom: rect.1 + rect.3,
      },
      &screens(),
    );
    visuals.layout(Layout {
      image: (width as i32, height as i32),
      offset: (rect.0 - window.left, rect.1 - window.top),
      opacity: self.fade.value(now),
      presence: self.presence.at(now),
      label,
    })?;
    self.placed = Some(rect);
    if self.window != Some(window) {
      self.window = Some(window);
      // SAFETY: this thread's live pin window; its pixels follow its top-left corner, so a move
      // and a resize stay in step with the visuals committed right after.
      unsafe {
        SetWindowPos(
          hwnd,
          None,
          window.left,
          window.top,
          window.right - window.left,
          window.bottom - window.top,
          SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
      }
      .map_err(|error| format!("Cannot place the pin: {error}"))?;
    }
    Ok(())
  }

  /// One animation frame: advances, redraws the image sharply once its zoom settles, and ends
  /// what finished. Returns false once the pin has closed.
  fn advance(&mut self, hwnd: HWND, now: Instant) -> Result<bool, String> {
    if self.presence.closing() && self.presence.done(now) {
      return Ok(false);
    }
    if self.slide.is_some_and(|slide| slide.done(now)) {
      self.slide = None;
    }
    if self
      .label
      .is_some_and(|shown| motion::label_alpha(now.saturating_duration_since(shown)).is_none())
    {
      self.label = None;
    }
    if !self.view.zooming(now) && !self.redraw_failed {
      let wanted = self.rendered_size(self.view.zoom_target());
      if let Some(visuals) = self.visuals.as_mut()
        && let Err(error) = visuals.render(wanted)
      {
        self.redraw_failed = true;
        return Err(error);
      }
    }
    self.present(hwnd, now)?;
    Ok(true)
  }

  fn moved(&mut self, hwnd: HWND, dx: i32, dy: i32) {
    self.view.translate(dx as f32, dy as f32);
    self.show(hwnd, Instant::now());
  }

  /// Shows a change at once rather than at the next frame.
  fn show(&mut self, hwnd: HWND, now: Instant) {
    match self.present(hwnd, now) {
      Ok(()) => commit(),
      Err(error) => {
        tracing::error!(%error, "Cannot show the pin");
        request_recovery();
      },
    }
  }

  fn zoom_to(&mut self, hwnd: HWND, zoom: usize, anchor: (i32, i32)) {
    if zoom == self.zoom {
      return;
    }
    let now = Instant::now();
    self.zoom = zoom;
    self.redraw_failed = false;
    let target = ZOOMS[zoom];
    // Zooming in, the image is first redrawn at least as large as the target, so the animation
    // shrinks or enlarges sharp pixels; zooming out, the sharper redraw waits for the end.
    let wanted = self.rendered_size(target);
    if let Some(visuals) = self.visuals.as_mut()
      && wanted.0 > visuals.rendered().0
      && let Err(error) = visuals.render(wanted)
    {
      tracing::error!(%error, "Cannot redraw the pin for zooming");
    }
    self.view.zoom_to(
      target,
      (anchor.0 as f32, anchor.1 as f32),
      now,
      motion::duration(motion::ZOOM_TIME, animates()),
    );
    self.label = Some(now);
    if let Err(error) = self.show_label() {
      tracing::warn!(%error, "Cannot show the pin's zoom label");
    }
    self.show(hwnd, now);
  }

  fn fade_to(&mut self, hwnd: HWND, opacity: usize, label: bool) {
    let now = Instant::now();
    self.opacity = opacity;
    self.fade.retarget(
      f32::from(OPACITIES[opacity]) / 100.0,
      now,
      motion::duration(motion::OPACITY_TIME, animates()),
    );
    if label {
      self.label = Some(now);
      if let Err(error) = self.show_label_for(OPACITIES[opacity].into()) {
        tracing::warn!(%error, "Cannot show the pin's opacity label");
      }
    }
    self.show(hwnd, now);
  }

  /// The label for the zoom being shown.
  fn show_label(&mut self) -> Result<(), String> {
    let percent = (self.view.zoom_target() * 100.0).round() as u32;
    self.show_label_for(percent)
  }

  fn show_label_for(&mut self, percent: u32) -> Result<(), String> {
    let text = crate::i18n::native_settings(&self.lang, "pin_percent")?
      .replace("{value}", &percent.to_string());
    let (pixels, width, height) = render_label(&text, self.scale)?;
    let Some(visuals) = self.visuals.as_mut() else {
      return Err("Pin visuals are missing".into());
    };
    with_gpu(|gpu| visuals.set_label(gpu, &pixels, width, height))
  }

  /// Fades the pin out after `delay`; it no longer takes clicks and closes when gone.
  fn close(&mut self, hwnd: HWND, delay: Duration) {
    if self.presence.closing() {
      return;
    }
    let now = Instant::now();
    self
      .presence
      .close(now, delay, motion::duration(motion::CLOSE_TIME, animates()));
    self.drag = None;
    // SAFETY: reads and releases this thread's own capture.
    if unsafe { GetCapture() } == hwnd {
      let _ = unsafe { ReleaseCapture() };
    }
    set_transparent(hwnd, true);
  }
}

/// The label's pixels, premultiplied BGRA: the selection's size-label style.
fn render_label(text: &str, scale: f32) -> Result<(Vec<u8>, u32, u32), String> {
  use crate::error::ErrorExt;
  LABELS.with(|renderer| {
    let mut renderer = renderer.borrow_mut();
    (|| -> anyhow::Result<(Vec<u8>, u32, u32)> {
      let text_width = renderer.measure_text(text, TextStyle::DIMENSIONS.scaled(scale))?;
      let width = (text_width + 2.0 * LABEL_PADDING * scale).ceil() as i32;
      let height = (LABEL_HEIGHT * scale).round() as i32;
      let mut bitmap = LayeredBitmap::new(width, height).map_err(anyhow::Error::msg)?;
      bitmap.clear();
      let bounds = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
      };
      let mut scene = Scene::scaled(scale)?;
      scene.panel(bounds)?;
      let pad = (LABEL_PADDING * scale).round() as i32;
      let inset = (7.0 * scale).round() as i32;
      scene.styled_text(
        RECT {
          left: pad,
          top: inset,
          right: width - pad,
          bottom: height - inset,
        },
        text,
        TextStyle::DIMENSIONS,
        crate::overlay::theme::TEXT,
      )?;
      renderer.render(bitmap.dc, bounds, &scene)?;
      Ok((bitmap.pixels_mut().to_vec(), width as u32, height as u32))
    })()
    .map_err(|error| error.to_rich_string())
  })
}

/// Whether any pin still animates; the clock runs frames only then.
fn animating(now: Instant) -> bool {
  let mut any = false;
  // SAFETY: pins of this thread.
  each_pin(|hwnd| any |= unsafe { pin_of(hwnd) }.is_some_and(|pin| pin.animating(now)));
  any
}

/// One frame for every animating pin, committed together.
fn tick(now: Instant) {
  let mut closed = Vec::new();
  let mut lost = false;
  each_pin(|hwnd| {
    // SAFETY: pins of this thread.
    let Some(pin) = (unsafe { pin_of(hwnd) }) else {
      return;
    };
    if !pin.animating(now) {
      return;
    }
    match pin.advance(hwnd, now) {
      Ok(true) => {},
      Ok(false) => closed.push(hwnd),
      Err(error) => {
        tracing::error!(%error, "Cannot animate the pin");
        lost = true;
      },
    }
  });
  commit();
  for hwnd in closed {
    // SAFETY: this thread's own pin window, faded out; WM_NCDESTROY releases its state.
    let _ = unsafe { DestroyWindow(hwnd) };
  }
  if lost {
    request_recovery();
  }
}

fn create_pin(app: AppHandle, image: PinImage, placement: Placement) -> Result<(), String> {
  let point = match placement {
    Placement::At(x, y) => (x, y),
    Placement::Centered => cursor().unwrap_or((0, 0)),
  };
  let (monitor, dpi) = monitor_at(point);
  let margin = frame::margin(dpi);
  let zoom = frame::opening_zoom(image.width, image.height, margin);
  let (width, height) = frame::scaled_size(image.width, image.height, ZOOMS[zoom]);
  let (window_width, window_height) = (width as i32 + 2 * margin, height as i32 + 2 * margin);
  let origin = match placement {
    Placement::At(x, y) => (x - margin, y - margin),
    Placement::Centered => frame::centered(window_width, window_height, monitor),
  };
  let class = wide(PIN_CLASS);
  // SAFETY: a popup owned by this host thread; its state is attached right after creation.
  // Its pixels come from DirectComposition (no redirection bitmap); layered so click-through
  // lets clicks reach the app below.
  let hwnd = unsafe {
    CreateWindowExW(
      WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOREDIRECTIONBITMAP,
      PCWSTR(class.as_ptr()),
      PCWSTR::null(),
      WS_POPUP,
      origin.0,
      origin.1,
      window_width,
      window_height,
      None,
      None,
      None,
      None,
    )
  }
  .map_err(|error| format!("CreateWindowExW (pin) failed: {error}"))?;
  // SAFETY: this thread's new layered window, fully opaque as a whole; the visuals fade it.
  if let Err(error) = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) } {
    // SAFETY: the window was created above by this thread and has no state attached yet.
    let _ = unsafe { DestroyWindow(hwnd) };
    return Err(format!("SetLayeredWindowAttributes (pin) failed: {error}"));
  }
  let lang = crate::i18n::get_lang(&app);
  let now = Instant::now();
  let animates = animates();
  let corner = ((origin.0 + margin) as f32, (origin.1 + margin) as f32);
  let mut pin = Box::new(Pin {
    app,
    lang,
    image,
    margin,
    scale: dpi.clamp(96, 480) as f32 / 96.0,
    zoom,
    opacity: OPACITIES.len() - 1,
    view: View::settled(ZOOMS[zoom], corner, now),
    fade: Ramp::still(1.0, now),
    presence: Presence::opening(now, motion::duration(motion::OPEN_TIME, animates)),
    slide: None,
    label: None,
    placed: Some((origin.0, origin.1, window_width, window_height)),
    // Unknown until the first frame places it, which the window was not created clipped for.
    window: None,
    click_through: false,
    grabbed: false,
    drag: None,
    visuals: None,
    redraw_failed: false,
  });
  // The first frame (fully transparent while opening) is committed before the window shows.
  let built = pin.build(hwnd).and_then(|()| pin.present(hwnd, now));
  let built = built.and_then(|()| with_gpu(Gpu::commit));
  if let Err(error) = built {
    // SAFETY: the window was created above by this thread and has no state attached yet.
    let _ = unsafe { DestroyWindow(hwnd) };
    return Err(error);
  }
  // SAFETY: ownership of the state moves to the window until WM_NCDESTROY takes it back.
  unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(pin) as isize) };
  let first = PINS.with(|pins| {
    let mut pins = pins.borrow_mut();
    pins.push(hwnd.0 as isize);
    pins.len() == 1
  });
  if first {
    // SAFETY: the timer belongs to this thread's host window.
    if unsafe { SetTimer(Some(host_window()), DEVICE_TIMER, DEVICE_CHECK_MS, None) } == 0 {
      tracing::warn!("Cannot start the pin device check; a lost GPU is noticed on the next draw");
    }
  }
  if SHOWN.with(Cell::get) {
    // SAFETY: shows the new pin without taking focus from the app the user is in.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
  }
  tracing::debug!(width, height, zoom = ZOOMS[zoom], "Pinned a screenshot");
  Ok(())
}

/// The pin attached to a window of this thread.
///
/// # Safety
/// `hwnd` must be a pin window of the calling (host) thread.
unsafe fn pin_of<'a>(hwnd: HWND) -> Option<&'a mut Pin> {
  // SAFETY: the pointer was set by `create_pin` and is cleared on WM_NCDESTROY.
  let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Pin;
  // SAFETY: non-null pointers come from Box::into_raw and are only used on this thread.
  unsafe { pointer.as_mut() }
}

/// The pointer in virtual-screen pixels.
fn cursor() -> Option<(i32, i32)> {
  POINTER.with(Cell::get)()
}

fn system_cursor() -> Option<(i32, i32)> {
  let mut point = POINT::default();
  // SAFETY: writes the cursor position into a local.
  unsafe { GetCursorPos(&mut point) }.ok()?;
  Some((point.x, point.y))
}

/// Every monitor's bounds, listed once and kept until a display change.
fn screens() -> Vec<Bounds> {
  let listed = SCREENS.with(|screens| screens.borrow().clone());
  if listed.is_empty() {
    refresh_screens()
  } else {
    listed
  }
}

/// Lists the monitors again, after a display change.
fn refresh_screens() -> Vec<Bounds> {
  let monitors: Vec<Bounds> = crate::monitors::list_monitors()
    .into_iter()
    .map(|monitor| Bounds {
      left: monitor.x,
      top: monitor.y,
      right: monitor.x.saturating_add(monitor.width as i32),
      bottom: monitor.y.saturating_add(monitor.height as i32),
    })
    .collect();
  if monitors.is_empty() {
    tracing::warn!("No monitors listed; pin windows are left unclipped");
  }
  SCREENS.with(|screens| *screens.borrow_mut() = monitors.clone());
  monitors
}

fn monitor_at(point: (i32, i32)) -> (Bounds, u32) {
  // SAFETY: plain queries about the monitor nearest a point.
  unsafe {
    let monitor = MonitorFromPoint(
      POINT {
        x: point.0,
        y: point.1,
      },
      MONITOR_DEFAULTTONEAREST,
    );
    let mut info = MONITORINFO {
      cbSize: std::mem::size_of::<MONITORINFO>() as u32,
      ..Default::default()
    };
    let area = if GetMonitorInfoW(monitor, &mut info).as_bool() {
      info.rcWork
    } else {
      RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
      }
    };
    let (mut dpi, mut unused) = (96, 96);
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused).is_err() {
      dpi = 96;
    }
    (
      Bounds {
        left: area.left,
        top: area.top,
        right: area.right,
        bottom: area.bottom,
      },
      dpi,
    )
  }
}

fn window_bounds(hwnd: HWND) -> Option<Bounds> {
  let mut rect = RECT::default();
  // SAFETY: reads this thread's own window rectangle.
  unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
  Some(Bounds {
    left: rect.left,
    top: rect.top,
    right: rect.right,
    bottom: rect.bottom,
  })
}

fn set_transparent(hwnd: HWND, transparent: bool) {
  // SAFETY: toggles one extended style bit on this thread's own window.
  unsafe {
    let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    let bit = WS_EX_TRANSPARENT.0 as isize;
    let next = if transparent {
      style | bit
    } else {
      style & !bit
    };
    if next != style {
      SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
    }
  }
}

fn each_pin(mut visit: impl FnMut(HWND)) {
  let pins = PINS.with(|pins| pins.borrow().clone());
  for pin in pins {
    visit(HWND(pin as *mut _));
  }
}

/// Closes every pin, the newest first, each a moment after the one before.
fn close_every_pin() {
  let mut open = Vec::new();
  each_pin(|hwnd| {
    // SAFETY: pins of this thread.
    if unsafe { pin_of(hwnd) }.is_some_and(|pin| !pin.presence.closing()) {
      open.push(hwnd);
    }
  });
  let count = open.len();
  let animates = animates();
  for (index, hwnd) in open.into_iter().rev().enumerate() {
    // SAFETY: pins of this thread.
    if let Some(pin) = unsafe { pin_of(hwnd) } {
      let delay = motion::duration(motion::close_all_delay(index, count), animates);
      pin.close(hwnd, delay);
    }
  }
}

/// The grab timer runs only while some pin is click-through.
fn update_grab_timer() {
  let mut any = false;
  // SAFETY: pins of this thread.
  each_pin(|hwnd| any |= unsafe { pin_of(hwnd) }.is_some_and(|pin| pin.click_through));
  let host = host_window();
  // SAFETY: the timer belongs to this thread's host window.
  unsafe {
    if any {
      if SetTimer(Some(host), GRAB_TIMER, GRAB_TICK_MS, None) == 0 {
        tracing::warn!("Cannot start the pin grab timer; Ctrl will not move click-through pins");
      }
    } else {
      let _ = KillTimer(Some(host), GRAB_TIMER);
    }
  }
}

/// Holding Ctrl over a click-through pin takes it back for moving, zooming or its menu.
fn grab_tick() {
  // SAFETY: reads global key and cursor state.
  let ctrl = unsafe { GetAsyncKeyState(0x11) } < 0;
  let pointer = cursor();
  each_pin(|hwnd| {
    // SAFETY: pins of this thread.
    let Some(pin) = (unsafe { pin_of(hwnd) }) else {
      return;
    };
    if !pin.click_through || pin.presence.closing() {
      return;
    }
    let over = pointer
      .zip(window_bounds(hwnd))
      .is_some_and(|((x, y), bounds)| {
        x >= bounds.left && x < bounds.right && y >= bounds.top && y < bounds.bottom
      });
    let grab = (ctrl && over) || pin.drag.is_some();
    if grab != pin.grabbed {
      pin.grabbed = grab;
      set_transparent(hwnd, !grab);
    }
  });
}

fn text(lang: &str, key: &str) -> Result<Vec<u16>, String> {
  Ok(wide(&crate::i18n::native_settings(lang, key)?))
}

/// The menu's entries: command, label and whether it is checked; command 0 is a separator.
fn menu_entries(pin: &Pin) -> Result<Vec<(usize, Vec<u16>, bool)>, String> {
  let items = [
    (COPY, "pin_copy"),
    (SAVE, "pin_save"),
    (ORIGINAL_SIZE, "pin_original_size"),
    (CLICK_THROUGH, "pin_click_through"),
    (0, ""),
    (CLOSE, "pin_close"),
    (CLOSE_ALL, "pin_close_all"),
  ];
  items
    .iter()
    .map(|(id, key)| {
      // Windows menus right-align what follows a tab: the shortcut.
      let shortcut = match *id {
        COPY => Some("pin_copy_shortcut"),
        CLOSE => Some("pin_close_shortcut"),
        _ => None,
      };
      let label = match (*id, shortcut) {
        (0, _) => Vec::new(),
        (_, Some(shortcut)) => wide(&format!(
          "{}\t{}",
          crate::i18n::native_settings(&pin.lang, key)?,
          crate::i18n::native_settings(&pin.lang, shortcut)?
        )),
        (_, None) => text(&pin.lang, key)?,
      };
      Ok((*id, label, *id == CLICK_THROUGH && pin.click_through))
    })
    .collect()
}

fn show_menu(hwnd: HWND, at: (i32, i32)) -> Result<(), String> {
  // SAFETY: pins of this thread.
  let Some(pin) = (unsafe { pin_of(hwnd) }) else {
    return Ok(());
  };
  let entries = menu_entries(pin)?;
  // SAFETY: the menu is created, shown and destroyed on this thread; labels outlive it.
  let chosen = unsafe {
    let menu = CreatePopupMenu().map_err(|error| format!("CreatePopupMenu failed: {error}"))?;
    for (id, label, checked) in &entries {
      let appended = if *id == 0 {
        AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())
      } else {
        let flags = if *checked {
          MF_STRING | MF_CHECKED
        } else {
          MF_STRING
        };
        AppendMenuW(menu, flags, *id, PCWSTR(label.as_ptr()))
      };
      if let Err(error) = appended {
        let _ = DestroyMenu(menu);
        return Err(format!("AppendMenuW failed: {error}"));
      }
    }
    // A popup menu closes properly only when its owner is the foreground window.
    let _ = SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenu(
      menu,
      TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
      at.0,
      at.1,
      None,
      hwnd,
      None,
    );
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    chosen.0 as usize
  };
  // Copy and Save report their own failures.
  drop(command(hwnd, chosen));
  Ok(())
}

/// Runs a menu command. Copy and Save finish in the background; their task is returned so a
/// caller can wait for it, and is otherwise left to run.
fn command(hwnd: HWND, chosen: usize) -> Option<tokio::task::JoinHandle<()>> {
  // SAFETY: pins of this thread.
  let pin = unsafe { pin_of(hwnd) }?;
  if pin.presence.closing() {
    return None;
  }
  match chosen {
    COPY => return Some(copy(pin)),
    SAVE => return Some(save(pin)),
    ORIGINAL_SIZE => {
      let ((x, y), _) = pin.view.at(Instant::now());
      // 1:1, or the largest step below it that fits for an enormous image.
      let zoom = frame::opening_zoom(pin.image.width, pin.image.height, pin.margin);
      pin.zoom_to(hwnd, zoom, (x.round() as i32, y.round() as i32));
      if pin.opacity != OPACITIES.len() - 1 {
        pin.fade_to(hwnd, OPACITIES.len() - 1, false);
      }
    },
    CLICK_THROUGH => {
      pin.click_through = !pin.click_through;
      pin.grabbed = false;
      set_transparent(hwnd, pin.click_through);
      update_grab_timer();
    },
    CLOSE => pin.close(hwnd, Duration::ZERO),
    CLOSE_ALL => close_every_pin(),
    _ => {},
  }
  None
}

fn copy(pin: &Pin) -> tokio::task::JoinHandle<()> {
  let (app, rgba, width, height) = (
    pin.app.clone(),
    Arc::clone(&pin.image.rgba),
    pin.image.width,
    pin.image.height,
  );
  crate::app::runtime::spawn(async move {
    if let Err(error) =
      crate::commands::clipboard::copy_rgba_to_clipboard_with_retry(&app, width, height, rgba).await
    {
      tracing::error!(%error, "Cannot copy the pinned screenshot");
      report(&app, "notification_error_pin_copy");
    }
  })
}

/// Saves the pin like a screenshot: into the screenshots folder, copied, notified and listed.
fn save(pin: &Pin) -> tokio::task::JoinHandle<()> {
  let (app, rgba, width, height) = (
    pin.app.clone(),
    Arc::clone(&pin.image.rgba),
    pin.image.width,
    pin.image.height,
  );
  crate::app::runtime::spawn(async move {
    let config = crate::config::Config::current(&app);
    let folder = config.resolve_capture_path(crate::config::CaptureType::Screenshot);
    let format = config.screenshot_format.as_str().to_string();
    let extension = match format.as_str() {
      "jpg" | "jpeg" => "jpg",
      "webp" => "webp",
      _ => "png",
    };
    let name = crate::filename::render_template(
      config.effective_filename_template(),
      &crate::filename::FilenameContext {
        capture_type: "screenshot",
        app_name: None,
        now: chrono::Local::now(),
      },
    );
    let result = crate::screenshot::CaptureResult {
      path: folder.join(format!("{name}.{extension}")),
      width,
      height,
      rgba: rgba.as_ref().clone(),
      format,
      quality: config.screenshot_quality,
      source_app: None,
      pin: None,
    };
    if let Err(error) = crate::commands::screenshot::post_capture::handle_capture_result(
      &app, result, &config, "pin_save",
    )
    .await
    {
      tracing::error!(%error, "Cannot save the pinned screenshot");
    }
  })
}

unsafe extern "system" fn pin_proc(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  // SAFETY: every message here is for a pin window of this thread.
  let Some(pin) = (unsafe { pin_of(hwnd) }) else {
    // SAFETY: default handling before the state is attached.
    return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
  };
  let input = matches!(
    message,
    WM_LBUTTONDOWN | WM_MOUSEMOVE | WM_LBUTTONDBLCLK | WM_MOUSEWHEEL | WM_KEYDOWN | WM_CONTEXTMENU
  );
  // A closing pin is fading out: it ignores the pointer and keys meanwhile.
  if input && pin.presence.closing() {
    return LRESULT(0);
  }
  match message {
    WM_LBUTTONDOWN => {
      if let Some(pointer) = cursor() {
        pin.drag = Some(pointer);
        // SAFETY: captures the mouse for this thread's own window during the drag.
        unsafe { SetCapture(hwnd) };
      }
      LRESULT(0)
    },
    WM_MOUSEMOVE => {
      if let Some((last_x, last_y)) = pin.drag
        && let Some((x, y)) = cursor()
        && (x, y) != (last_x, last_y)
      {
        pin.drag = Some((x, y));
        pin.moved(hwnd, x - last_x, y - last_y);
      }
      LRESULT(0)
    },
    WM_LBUTTONUP => {
      pin.drag = None;
      // SAFETY: releases this thread's own capture.
      let _ = unsafe { ReleaseCapture() };
      LRESULT(0)
    },
    WM_CAPTURECHANGED => {
      pin.drag = None;
      LRESULT(0)
    },
    WM_LBUTTONDBLCLK => {
      pin.close(hwnd, Duration::ZERO);
      LRESULT(0)
    },
    WM_MOUSEWHEEL => {
      let delta = (wparam.0 >> 16) as u16 as i16 as i32;
      let notches = if delta > 0 {
        (delta / 120).max(1)
      } else {
        (delta / 120).min(-1)
      };
      let ctrl = wparam.0 & 0x0008 != 0;
      if ctrl {
        let opacity = frame::opacity_after(pin.opacity, notches);
        if opacity != pin.opacity {
          pin.fade_to(hwnd, opacity, true);
        }
      } else {
        let anchor = (
          (lparam.0 & 0xffff) as u16 as i16 as i32,
          ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32,
        );
        let zoom = frame::zoom_after(
          pin.zoom,
          notches,
          pin.image.width,
          pin.image.height,
          pin.margin,
        );
        pin.zoom_to(hwnd, zoom, anchor);
      }
      LRESULT(0)
    },
    WM_KEYDOWN => {
      // SAFETY: reads this thread's keyboard state.
      let (ctrl, shift) = unsafe { (GetKeyState(0x11) < 0, GetKeyState(0x10) < 0) };
      let step = if shift { 10 } else { 1 };
      match wparam.0 as u32 {
        0x1B => pin.close(hwnd, Duration::ZERO),
        0x43 if ctrl => drop(copy(pin)),
        0x25 => pin.moved(hwnd, -step, 0),
        0x26 => pin.moved(hwnd, 0, -step),
        0x27 => pin.moved(hwnd, step, 0),
        0x28 => pin.moved(hwnd, 0, step),
        // SAFETY: default handling for other keys.
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
      }
      LRESULT(0)
    },
    WM_CONTEXTMENU => {
      let at = (
        (lparam.0 & 0xffff) as u16 as i16 as i32,
        ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32,
      );
      // A keyboard-opened menu (-1, -1) appears at the image's corner.
      let at = if at == (-1, -1) {
        let ((x, y), _) = pin.view.at(Instant::now());
        (x.round() as i32, y.round() as i32)
      } else {
        at
      };
      if let Err(error) = show_menu(hwnd, at) {
        tracing::error!(%error, "Cannot show the pin menu");
      }
      LRESULT(0)
    },
    WM_DISPLAYCHANGE => {
      let monitors = refresh_screens();
      let bounds = pin.placed.map(|(left, top, width, height)| Bounds {
        left,
        top,
        right: left + width,
        bottom: top + height,
      });
      if let Some(bounds) = bounds
        && let Some((x, y)) = frame::rehome(bounds, &monitors)
      {
        tracing::debug!("A pin's monitor went away; sliding it onto a remaining one");
        let (dx, dy) = ((x - bounds.left) as f32, (y - bounds.top) as f32);
        let now = Instant::now();
        let shown = pin.slide.map_or((0.0, 0.0), |slide| slide.offset(now));
        pin.view.translate(dx, dy);
        pin.slide = Some(Slide::new(
          (shown.0 - dx, shown.1 - dy),
          now,
          motion::duration(motion::SLIDE_TIME, animates()),
        ));
        pin.show(hwnd, now);
      } else {
        // Still on a screen, but the screens changed: clipped again.
        pin.show(hwnd, Instant::now());
      }
      LRESULT(0)
    },
    // Keep the pin's physical size, so a monitor at another scaling never resamples it.
    WM_DPICHANGED => LRESULT(0),
    WM_NCDESTROY => {
      // SAFETY: takes back the state attached by `create_pin`; nothing uses it afterwards.
      unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(pin as *mut Pin));
      }
      let none_left = PINS.with(|pins| {
        let mut pins = pins.borrow_mut();
        pins.retain(|other| *other != hwnd.0 as isize);
        pins.is_empty()
      });
      if none_left {
        // SAFETY: the timer belongs to this thread's host window.
        let _ = unsafe { KillTimer(Some(host_window()), DEVICE_TIMER) };
      }
      update_grab_timer();
      // SAFETY: default destruction handling.
      unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    },
    // SAFETY: default handling for everything else.
    _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
  }
}
