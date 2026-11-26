//! The pin host driven through its own window messages, on the test thread: hidden windows,
//! drawn with WARP and with animations fixed on, so every run takes the same paths.

use super::*;
use crate::overlay::notification::NotificationManager;
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SendMessageW, WM_CLOSE, WM_LBUTTONDOWN};

const WHEEL: usize = 120;
const MK_CONTROL: usize = 0x0008;

struct Fixture {
  app: AppHandle,
  host: HWND,
}

impl Fixture {
  fn new() -> Self {
    DEVICE.with(|device| device.set(Gpu::warp));
    SHOWN.with(|shown| shown.set(false));
    ANIMATES.with(|animates| animates.set(Some(true)));
    let host = create_host(Arc::new(Mutex::new(VecDeque::new()))).expect("hidden pin host");
    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        notifications_enabled: false,
        notification_sound: false,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    Self { app, host }
  }

  /// Queues pins and lets the host open them, as another thread's request would.
  fn open(&self, count: usize, placement: Placement) -> Vec<HWND> {
    let before = pins();
    QUEUE.with(|slot| {
      let slot = slot.borrow();
      let queue = slot.as_ref().expect("host queue");
      for _ in 0..count {
        lock(queue).push_back(Request::Open {
          app: self.app.clone(),
          image: image(64, 48),
          placement,
        });
      }
    });
    self.send_host(REQUEST, 0);
    pins()
      .into_iter()
      .filter(|pin| !before.contains(pin))
      .collect()
  }

  fn send_host(&self, message: u32, wparam: usize) {
    // SAFETY: this thread's own host window, handled synchronously.
    unsafe { SendMessageW(self.host, message, Some(WPARAM(wparam)), Some(LPARAM(0))) };
  }

  fn notices(&self) -> usize {
    self
      .app
      .try_state::<NotificationManager>()
      .expect("notifications")
      .take_test_requests()
      .len()
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    for hwnd in pins() {
      // SAFETY: this thread's own pin windows.
      let _ = unsafe { DestroyWindow(hwnd) };
    }
    // SAFETY: this thread's own host window.
    let _ = unsafe { DestroyWindow(self.host) };
  }
}

fn image(width: u32, height: u32) -> PinImage {
  let rgba = (0..width * height)
    .flat_map(|index| {
      let (x, y) = (index % width, index / width);
      [(x * 4) as u8, (y * 5) as u8, 128, 255]
    })
    .collect();
  PinImage {
    rgba: Arc::new(rgba),
    width,
    height,
  }
}

fn pins() -> Vec<HWND> {
  PINS.with(|pins| {
    pins
      .borrow()
      .iter()
      .map(|pin| HWND(*pin as *mut _))
      .collect()
  })
}

fn state<'a>(hwnd: HWND) -> &'a mut Pin {
  // SAFETY: a pin window of this test thread, alive while the test uses it.
  unsafe { pin_of(hwnd) }.expect("pin state")
}

fn send(hwnd: HWND, message: u32, wparam: usize, lparam: isize) {
  // SAFETY: this thread's own pin window, handled synchronously.
  unsafe { SendMessageW(hwnd, message, Some(WPARAM(wparam)), Some(LPARAM(lparam))) };
}

fn wheel(hwnd: HWND, notches: i16, flags: usize, at: (i32, i32)) {
  let delta = (notches * WHEEL as i16) as u16 as usize;
  let point = (at.0 as u16 as isize) | ((at.1 as u16 as isize) << 16);
  send(hwnd, WM_MOUSEWHEEL, (delta << 16) | flags, point);
}

/// Runs frames until nothing animates, as the clock would.
fn settle() {
  tick(Instant::now());
  for step in 1..=4 {
    tick(Instant::now() + Duration::from_secs(step));
  }
}

/// A moment after every animation started in a test has ended.
fn later() -> Instant {
  Instant::now() + Duration::from_secs(5)
}

fn transparent(hwnd: HWND) -> bool {
  // SAFETY: reads a style of this thread's own window.
  let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
  style & WS_EX_TRANSPARENT.0 as isize != 0
}

fn alive(hwnd: HWND) -> bool {
  // SAFETY: a plain query; a destroyed handle answers false.
  unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

#[test]
fn a_pin_opens_zooms_fades_moves_and_closes_through_its_messages() {
  let fixture = Fixture::new();
  let [hwnd] = fixture.open(1, Placement::At(200, 200))[..] else {
    panic!("one pin");
  };
  assert!(!state(hwnd).presence.done(Instant::now()));
  assert!(animating(Instant::now()));
  settle();
  assert!(!animating(later()));
  let pin = state(hwnd);
  assert_eq!(pin.zoom, frame::ORIGINAL);
  assert_eq!(pin.placed.map(|p| (p.2, p.3)), Some(pin.window_for(1.0)));

  // The wheel zooms around the pointer and shows a label until it fades.
  wheel(hwnd, 1, 0, (232, 224));
  assert_eq!(state(hwnd).zoom, frame::ORIGINAL + 1);
  assert!(state(hwnd).label.is_some());
  fixture.send_host(WM_TIMER, FRAME_TIMER);
  settle();
  let pin = state(hwnd);
  assert!(pin.label.is_none());
  assert_eq!(pin.placed.map(|p| (p.2, p.3)), Some(pin.window_for(1.25)));
  assert_eq!(pin.visuals.as_ref().map(Visuals::rendered), Some((64, 48)));

  // Zooming out redraws the image smaller once it settles; zooming back in redraws it first.
  wheel(hwnd, -2, 0, (232, 224));
  assert_eq!(state(hwnd).zoom, frame::ORIGINAL - 1);
  settle();
  assert_eq!(
    state(hwnd).visuals.as_ref().map(Visuals::rendered),
    Some(frame::scaled_size(64, 48, 0.9))
  );
  wheel(hwnd, 1, 0, (232, 224));
  assert_eq!(
    state(hwnd).visuals.as_ref().map(Visuals::rendered),
    Some((64, 48))
  );
  settle();

  // Ctrl+wheel fades in steps; at the end it stops.
  wheel(hwnd, -1, MK_CONTROL, (232, 224));
  assert_eq!(state(hwnd).opacity, OPACITIES.len() - 2);
  settle();
  assert!((state(hwnd).fade.value(later()) - 0.9).abs() < 1e-6);
  wheel(hwnd, 5, MK_CONTROL, (232, 224));
  settle();
  assert_eq!(state(hwnd).opacity, OPACITIES.len() - 1);
  wheel(hwnd, 1, MK_CONTROL, (232, 224));
  assert_eq!(state(hwnd).opacity, OPACITIES.len() - 1);

  // Arrow keys move it a pixel; other keys fall through.
  let ((x, y), _) = state(hwnd).view.at(later());
  for key in [0x25, 0x26, 0x27, 0x27, 0x28, 0x28] {
    send(hwnd, WM_KEYDOWN, key, 0);
  }
  let ((moved_x, moved_y), _) = state(hwnd).view.at(later());
  assert_eq!((moved_x - x, moved_y - y), (1.0, 1.0));
  send(hwnd, WM_KEYDOWN, 0x41, 0);

  // Original Size goes back to 100% and full opacity.
  wheel(hwnd, -1, MK_CONTROL, (232, 224));
  wheel(hwnd, 2, 0, (232, 224));
  settle();
  command(hwnd, ORIGINAL_SIZE);
  settle();
  let pin = state(hwnd);
  assert_eq!(
    (pin.zoom, pin.opacity),
    (frame::ORIGINAL, OPACITIES.len() - 1)
  );

  // Messages that only end a drag or keep the size change nothing.
  pin.drag = Some((0, 0));
  send(hwnd, WM_CAPTURECHANGED, 0, 0);
  assert!(state(hwnd).drag.is_none());
  send(hwnd, WM_LBUTTONUP, 0, 0);
  send(hwnd, WM_MOUSEMOVE, 0, 0);
  send(hwnd, WM_DPICHANGED, 0, 0);
  send(hwnd, WM_DISPLAYCHANGE, 0, 0);
  settle();
  assert!(alive(hwnd));

  // Esc fades it out; meanwhile it ignores input, then the window goes.
  send(hwnd, WM_KEYDOWN, 0x1B, 0);
  assert!(state(hwnd).presence.closing());
  wheel(hwnd, 1, 0, (232, 224));
  assert_eq!(state(hwnd).zoom, frame::ORIGINAL);
  command(hwnd, ORIGINAL_SIZE);
  settle();
  assert!(!alive(hwnd));
  assert!(pins().is_empty());
  assert_eq!(fixture.notices(), 0);
}

/// Regression: a pin whose fade had ended no longer counted as animating, so the frame that
/// destroys its window never came; closed pins stayed as invisible windows and kept their slot.
#[test]
fn click_through_lets_clicks_pass_until_the_pin_is_grabbed() {
  let fixture = Fixture::new();
  let [hwnd] = fixture.open(1, Placement::Centered)[..] else {
    panic!("one pin");
  };
  settle();
  let entries = menu_entries(state(hwnd)).expect("menu");
  assert_eq!(entries.len(), 7);
  assert!(entries.iter().all(|(_, _, checked)| !checked));
  assert!(
    entries
      .iter()
      .any(|(id, label, _)| *id == COPY && label.contains(&u16::from(b'\t')))
  );

  command(hwnd, CLICK_THROUGH);
  assert!(state(hwnd).click_through && transparent(hwnd));
  let entries = menu_entries(state(hwnd)).expect("menu");
  assert!(
    entries
      .iter()
      .any(|(id, _, checked)| *id == CLICK_THROUGH && *checked)
  );

  // A drag in progress keeps it grabbed, whatever the keys.
  state(hwnd).drag = Some((0, 0));
  fixture.send_host(WM_TIMER, GRAB_TIMER);
  assert!(state(hwnd).grabbed && !transparent(hwnd));
  state(hwnd).drag = None;

  command(hwnd, CLICK_THROUGH);
  assert!(!state(hwnd).click_through && !transparent(hwnd));
  command(hwnd, 0);
  command(hwnd, CLOSE);
  assert!(state(hwnd).presence.closing() && transparent(hwnd));
  // A closing pin takes no more commands.
  command(hwnd, CLICK_THROUGH);
  assert!(!state(hwnd).click_through);
  settle();
  assert!(pins().is_empty());
}

#[test]
fn close_all_closes_every_pin_newest_first_and_a_double_click_closes_one() {
  let fixture = Fixture::new();
  let opened = fixture.open(3, Placement::At(300, 300));
  assert_eq!(opened.len(), 3);
  settle();
  send(opened[0], WM_LBUTTONDBLCLK, 0, 0);
  assert!(state(opened[0]).presence.closing());
  assert!(!state(opened[1]).presence.closing());
  command(opened[1], CLOSE_ALL);
  assert!(opened.iter().all(|hwnd| state(*hwnd).presence.closing()));
  // The newest starts fading first.
  let soon = Instant::now() + Duration::from_millis(60);
  assert!(state(opened[2]).presence.at(soon).0 < state(opened[1]).presence.at(soon).0);
  settle();
  assert!(opened.iter().all(|hwnd| !alive(*hwnd)));
}

#[test]
fn without_windows_animations_every_change_is_immediate() {
  let fixture = Fixture::new();
  ANIMATES.with(|animates| animates.set(Some(false)));
  let [hwnd] = fixture.open(1, Placement::At(200, 200))[..] else {
    panic!("one pin");
  };
  tick(Instant::now());
  wheel(hwnd, 1, 0, (210, 210));
  assert!(!state(hwnd).view.zooming(Instant::now()));
  settle();
  send(hwnd, WM_KEYDOWN, 0x1B, 0);
  tick(Instant::now());
  assert!(pins().is_empty());
}

#[test]
fn a_lost_device_rebuilds_every_pin_on_a_new_one() {
  let fixture = Fixture::new();
  let opened = fixture.open(2, Placement::At(200, 200));
  settle();
  // A working device is left alone by the periodic check.
  fixture.send_host(WM_TIMER, DEVICE_TIMER);
  fixture.send_host(RECOVER, 0);
  with_gpu(|gpu| {
    gpu.simulate_loss();
    Ok(())
  })
  .expect("device");
  fixture.send_host(WM_TIMER, DEVICE_TIMER);
  assert!(!with_gpu(|gpu| Ok(gpu.lost())).expect("new device"));
  assert!(opened.iter().all(|hwnd| state(*hwnd).visuals.is_some()));
  with_gpu(|gpu| {
    gpu.simulate_loss();
    Ok(())
  })
  .expect("device");
  fixture.send_host(RECOVER, 0);
  assert!(!with_gpu(|gpu| Ok(gpu.lost())).expect("new device"));
  wheel(opened[0], 1, 0, (210, 210));
  settle();
  assert!(opened.iter().all(|hwnd| alive(*hwnd)));
  assert_eq!(fixture.notices(), 0);
}

#[test]
fn pins_that_cannot_be_rebuilt_close_with_one_message() {
  let fixture = Fixture::new();
  let opened = fixture.open(2, Placement::At(200, 200));
  settle();
  DEVICE.with(|device| device.set(|| Err("no device".into())));
  with_gpu(|gpu| {
    gpu.simulate_loss();
    Ok(())
  })
  .expect("device");
  fixture.send_host(RECOVER, 0);
  assert!(opened.iter().all(|hwnd| !alive(*hwnd)));
  assert_eq!(fixture.notices(), 1);
  // With no device, a new pin does not open and says so.
  assert!(fixture.open(1, Placement::At(200, 200)).is_empty());
  assert_eq!(fixture.notices(), 1);
  // A commit or a frame without a device asks for a recovery instead of failing silently.
  commit();
  tick(Instant::now());
}

#[test]
fn at_most_max_pins_open_and_the_rest_are_reported() {
  let fixture = Fixture::new();
  let opened = fixture.open(MAX_PINS + 1, Placement::At(200, 200));
  assert_eq!(opened.len(), MAX_PINS);
  assert_eq!(fixture.notices(), 1);
}

/// Regression: window classes belong to the process, so a host started again after its thread
/// ended could not register them and every later pin failed.
#[test]
fn a_restarted_host_registers_its_classes_again() {
  let first = Fixture::new();
  drop(first);
  let handle = std::thread::spawn(|| {
    let second = Fixture::new();
    second.open(1, Placement::At(200, 200)).len()
  });
  assert_eq!(handle.join().expect("second host"), 1);
  register(PIN_CLASS, pin_proc, true).expect("registered again");
}

#[test]
fn a_pin_left_off_every_screen_slides_back_after_a_display_change() {
  let fixture = Fixture::new();
  let [hwnd] = fixture.open(1, Placement::At(200, 200))[..] else {
    panic!("one pin");
  };
  settle();
  let screens = screens();
  // Moved far off every screen, then the screens change: it slides back onto one.
  state(hwnd).moved(hwnd, -100_000, -100_000);
  send(hwnd, WM_DISPLAYCHANGE, 0, 0);
  assert!(state(hwnd).slide.is_some());
  settle();
  let (left, top, width, height) = state(hwnd).placed.expect("placed");
  let on_screen =
    |s: &Bounds| left < s.right && s.left < left + width && top < s.bottom && s.top < top + height;
  assert!(screens.is_empty() || screens.iter().any(on_screen));
  assert!(state(hwnd).slide.is_none());
}

/// A real host thread's setup. Its clock runs on its own, so without animations it never has a
/// frame to time: whether one finished before the test process ended would differ between runs.
fn quiet_host() {
  DEVICE.with(|device| device.set(Gpu::warp));
  SHOWN.with(|shown| shown.set(false));
  ANIMATES.with(|animates| animates.set(Some(false)));
}

fn running_host() -> Option<HWND> {
  lock(&HOST).as_ref().map(|host| HWND(host.window as *mut _))
}

/// Regression: a host whose window was gone was replaced by a new thread that could not register
/// the window classes again, so every later pin failed. Its replacement now opens pins.
#[test]
fn pins_open_on_the_host_thread_and_a_lost_host_is_replaced() {
  let app = Fixture::new().app.clone();
  open_with(&app, image(32, 32), Placement::At(220, 220), quiet_host);
  let first = running_host().expect("host started");
  // The host's window goes away; the next pin starts a new host instead of failing.
  // SAFETY: posting to the host window; its own thread destroys it.
  unsafe { PostMessageW(Some(first), WM_CLOSE, WPARAM(0), LPARAM(0)) }.expect("close host");
  let deadline = Instant::now() + Duration::from_secs(5);
  while alive(first) && Instant::now() < deadline {
    std::thread::sleep(Duration::from_millis(10));
  }
  assert!(!alive(first), "the host window closed");
  open_with(&app, image(32, 32), Placement::Centered, quiet_host);
  let second = running_host().expect("host restarted");
  assert_ne!(first, second);
  assert!(alive(second));
  // SAFETY: the host thread handles the request; this only waits for it to run.
  unsafe { SendMessageW(second, WM_NULL, Some(WPARAM(0)), Some(LPARAM(0))) };
  assert_eq!(
    app
      .try_state::<NotificationManager>()
      .expect("notifications")
      .take_test_requests()
      .len(),
    0
  );
}

#[test]
fn copying_without_a_clipboard_is_reported() {
  crate::app::runtime::test_runtime();
  let fixture = Fixture::new();
  let [hwnd] = fixture.open(1, Placement::At(200, 200))[..] else {
    panic!("one pin");
  };
  let task = command(hwnd, COPY).expect("copy task");
  crate::app::runtime::test_runtime()
    .block_on(task)
    .expect("copied");
  assert_eq!(fixture.notices(), 1);
}

thread_local! {
  static SCRIPTED_POINTER: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
}

fn scripted_pointer() -> Option<(i32, i32)> {
  SCRIPTED_POINTER.with(Cell::get)
}

fn point_at(at: Option<(i32, i32)>) {
  SCRIPTED_POINTER.with(|pointer| pointer.set(at));
}

#[test]
fn dragging_moves_the_pin_with_the_pointer_until_the_button_is_released() {
  let fixture = Fixture::new();
  POINTER.with(|pointer| pointer.set(scripted_pointer));
  let [hwnd] = fixture.open(1, Placement::At(200, 200))[..] else {
    panic!("one pin");
  };
  settle();
  let corner = |hwnd| state(hwnd).view.at(later()).0;
  let start = corner(hwnd);
  // Without a pointer reading there is no drag.
  point_at(None);
  send(hwnd, WM_LBUTTONDOWN, 0, 0);
  assert!(state(hwnd).drag.is_none());
  point_at(Some((300, 300)));
  send(hwnd, WM_LBUTTONDOWN, 0, 0);
  assert_eq!(state(hwnd).drag, Some((300, 300)));
  for at in [(310, 305), (310, 305), (290, 320)] {
    point_at(Some(at));
    send(hwnd, WM_MOUSEMOVE, 0, 0);
  }
  assert_eq!(corner(hwnd), (start.0 - 10.0, start.1 + 20.0));
  send(hwnd, WM_LBUTTONUP, 0, 0);
  point_at(Some((500, 500)));
  send(hwnd, WM_MOUSEMOVE, 0, 0);
  assert_eq!(corner(hwnd), (start.0 - 10.0, start.1 + 20.0), "released");
  // A Centered pin opens around the pointer's monitor.
  let [centered] = fixture.open(1, Placement::Centered)[..] else {
    panic!("one pin");
  };
  assert!(alive(centered));
}

#[test]
fn saving_writes_the_pin_like_a_screenshot_in_each_format() {
  let runtime = crate::app::runtime::test_runtime();
  let folder = tempfile::tempdir().expect("folder");
  let fixture = Fixture::new();
  let [hwnd] = fixture.open(1, Placement::At(200, 200))[..] else {
    panic!("one pin");
  };
  for (format, extension) in [
    (crate::config::ScreenshotFormat::Png, "png"),
    (crate::config::ScreenshotFormat::Jpg, "jpg"),
    (crate::config::ScreenshotFormat::Webp, "webp"),
  ] {
    fixture
      .app
      .try_state::<crate::state::ConfigState>()
      .expect("config")
      .replace(crate::config::Config {
        language: "en".into(),
        notifications_enabled: false,
        notification_sound: false,
        screenshot_format: format,
        save_location_mode: crate::config::SaveLocationMode::SystemFolders,
        custom_screenshots_folder: Some(folder.path().to_string_lossy().into_owned()),
        ..Default::default()
      });
    let task = command(hwnd, SAVE).expect("save task");
    runtime.block_on(task).expect("saved");
    let saved: Vec<_> = std::fs::read_dir(folder.path())
      .expect("folder")
      .flatten()
      .map(|entry| entry.path())
      .filter(|path| path.extension().is_some_and(|ext| ext == extension))
      .collect();
    assert_eq!(saved.len(), 1, "{extension}");
    assert_eq!(
      image::image_dimensions(&saved[0]).expect("readable"),
      (64, 48)
    );
  }
}
