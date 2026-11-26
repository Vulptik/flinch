//! Explicit desktop benchmark: which moment a capture tool actually froze. A small window
//! shows a running millisecond clock; the clock value inside the frozen image, minus the
//! value when the shortcut was sent, is the delay. It needs no cooperation from the tool.
//! `screen` reads the frozen image from the tool's overlay; `clipboard` drags a selection
//! and reads the result, for overlays excluded from desktop capture. Resolution is one
//! display refresh. It injects input and may replace the clipboard: never quiet validation.
use crate::overlay_latency::{
  Key, Scanner, close_overlay, median, parse_keys, screen_size, wait_for_overlay,
};
use flinch_video_engine::{EngineError, Result};
use serde::Serialize;
use std::{
  ffi::OsString,
  path::PathBuf,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread::JoinHandle,
  time::{Duration, Instant},
};
use windows::Win32::{
  Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM},
  Graphics::Gdi::{
    BI_BITFIELDS, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, BitBlt, CAPTUREBLT,
    CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, FillRect,
    GdiFlush, GetDC, GetDIBits, GetDeviceCaps, GetStockObject, HBRUSH, ReleaseDC, SRCCOPY,
    SelectObject, VREFRESH, WHITE_BRUSH,
  },
  System::{
    DataExchange::{CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard},
    LibraryLoader::GetModuleHandleW,
    Memory::{GlobalLock, GlobalSize, GlobalUnlock},
  },
  UI::{
    HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
    Input::KeyboardAndMouse::{
      INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
      MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
    },
    WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
      PeekMessageW, RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WNDCLASSW,
      WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
  },
};
use windows_core::{PCWSTR, w};

const CELL: i32 = 12;
const VALUE_BITS: usize = 24;
const CELLS: usize = 2 + VALUE_BITS * 2;
const WIDTH: i32 = CELL * CELLS as i32;
const HEIGHT: i32 = 40;
const MINIMUM_CONTRAST: i32 = 24;
const MAX_RUNS: u32 = 100;
const READ_GAP: Duration = Duration::from_millis(120);
const CLIPBOARD_DEADLINE: Duration = Duration::from_secs(3);
const SELECTION_MARGIN: i32 = 24;

/// Guard cells give the local white and black, so a dimmed overlay still reads. The
/// inverted copy rejects anything but a complete clock value.
fn encode(value: u32) -> [bool; CELLS] {
  let mut cells = [false; CELLS];
  cells[0] = true;
  for bit in 0..VALUE_BITS {
    let set = value >> bit & 1 == 1;
    cells[2 + bit] = set;
    cells[2 + VALUE_BITS + bit] = !set;
  }
  cells
}

/// Green channel at a pixel of a top-down 32-bit image.
fn level(pixels: &[u8], stride: usize, x: usize, y: usize) -> i32 {
  pixels
    .get(y * stride + x * 4 + 1)
    .copied()
    .map_or(0, i32::from)
}

fn decode_at(pixels: &[u8], stride: usize, left: usize, y: usize) -> Option<u32> {
  let cell = CELL as usize;
  let sample = |index: usize| level(pixels, stride, left + index * cell + cell / 2, y);
  let (white, black) = (sample(0), sample(1));
  if white - black < MINIMUM_CONTRAST {
    return None;
  }
  let threshold = (white + black) / 2;
  let mut value = 0_u32;
  for bit in 0..VALUE_BITS {
    let set = sample(2 + bit) > threshold;
    if set == (sample(2 + VALUE_BITS + bit) > threshold) {
      return None;
    }
    value |= u32::from(set) << bit;
  }
  Some(value)
}

/// Find the clock anywhere in a top-down 32-bit image. The tool's crop may be off by a
/// few pixels; the inverted copy keeps a blind scan from reading noise.
fn find_clock(pixels: &[u8], width: usize, height: usize) -> Option<u32> {
  let stride = width * 4;
  let span = WIDTH as usize;
  if width < span {
    return None;
  }
  (0..height).step_by(4).find_map(|y| {
    (0..=width - span)
      .step_by(2)
      .find_map(|left| decode_at(pixels, stride, left, y))
  })
}

struct Clock {
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
  started: Instant,
  rect: RECT,
}

impl Drop for Clock {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    if let Some(thread) = self.thread.take()
      && thread.join().is_err()
    {
      crate::console::diagnostic("Clock window thread failed".to_owned());
    }
  }
}

unsafe extern "system" fn window_proc(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  // SAFETY: forwards the arguments Windows supplied for this window.
  unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Paint the clock until `stop`. The whole value is composed offscreen, then copied in
/// one blit, so a composed frame never shows half of two values.
fn run_clock(rect: RECT, started: Instant, stop: &AtomicBool) -> Result<()> {
  let failed = |step: &str| EngineError::protocol(format!("Clock window: {step} failed"));
  // SAFETY: every handle is created, used and released on this thread within this call.
  unsafe {
    let instance = GetModuleHandleW(None).map_err(|_| failed("module"))?;
    let class = w!("FlinchFreezeClock");
    let description = WNDCLASSW {
      lpfnWndProc: Some(window_proc),
      hInstance: instance.into(),
      lpszClassName: class,
      ..Default::default()
    };
    RegisterClassW(&description);
    let window = CreateWindowExW(
      WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
      class,
      PCWSTR::null(),
      WS_POPUP,
      rect.left,
      rect.top,
      WIDTH,
      HEIGHT,
      None,
      None,
      Some(instance.into()),
      None,
    )
    .map_err(|_| failed("create"))?;
    let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
    let target = GetDC(Some(window));
    let memory = CreateCompatibleDC(Some(target));
    let bitmap = CreateCompatibleBitmap(target, WIDTH, HEIGHT);
    let previous = SelectObject(memory, bitmap.into());
    let white = HBRUSH(GetStockObject(WHITE_BRUSH).0);
    let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
    let mut shown = u32::MAX;
    while !stop.load(Ordering::Acquire) {
      let mut message = MSG::default();
      while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
      }
      let value = started.elapsed().as_millis() as u32;
      if value == shown {
        std::thread::yield_now();
        continue;
      }
      shown = value;
      for (index, set) in encode(value).iter().enumerate() {
        let left = index as i32 * CELL;
        let cell = RECT {
          left,
          top: 0,
          right: left + CELL,
          bottom: HEIGHT,
        };
        FillRect(memory, &cell, if *set { white } else { black });
      }
      let _ = BitBlt(target, 0, 0, WIDTH, HEIGHT, Some(memory), 0, 0, SRCCOPY);
      let _ = GdiFlush();
    }
    SelectObject(memory, previous);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(memory);
    ReleaseDC(Some(window), target);
    let _ = DestroyWindow(window);
  }
  Ok(())
}

fn start_clock(screen: (i32, i32)) -> Result<Clock> {
  let left = (screen.0 - WIDTH) / 2;
  let top = screen.1 * 3 / 5;
  let rect = RECT {
    left,
    top,
    right: left + WIDTH,
    bottom: top + HEIGHT,
  };
  let stop = Arc::new(AtomicBool::new(false));
  let started = Instant::now();
  let flag = Arc::clone(&stop);
  let thread = std::thread::Builder::new()
    .name("freeze clock".into())
    .spawn(move || {
      if let Err(error) = run_clock(rect, started, &flag) {
        crate::console::diagnostic(error.to_string());
      }
    })
    .map_err(|error| EngineError::io("clock thread", error))?;
  std::thread::sleep(Duration::from_millis(300));
  Ok(Clock {
    stop,
    thread: Some(thread),
    started,
    rect,
  })
}

/// Read the desktop as composed around the clock, for overlays visible to capture.
fn read_screen(rect: RECT) -> Result<Option<u32>> {
  let width = rect.right - rect.left + SELECTION_MARGIN * 2;
  let height = rect.bottom - rect.top + SELECTION_MARGIN * 2;
  let mut pixels = vec![0_u8; width as usize * height as usize * 4];
  // SAFETY: handles live only inside this block; the buffer matches the requested DIB.
  unsafe {
    let screen = GetDC(None);
    let memory = CreateCompatibleDC(Some(screen));
    let bitmap = CreateCompatibleBitmap(screen, width, height);
    let previous = SelectObject(memory, bitmap.into());
    let copied = BitBlt(
      memory,
      0,
      0,
      width,
      height,
      Some(screen),
      rect.left - SELECTION_MARGIN,
      rect.top - SELECTION_MARGIN,
      SRCCOPY | CAPTUREBLT,
    );
    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = -height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    SelectObject(memory, previous);
    let rows = GetDIBits(
      memory,
      bitmap,
      0,
      height as u32,
      Some(pixels.as_mut_ptr().cast()),
      &mut info,
      DIB_RGB_COLORS,
    );
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(memory);
    ReleaseDC(None, screen);
    copied.map_err(|error| EngineError::protocol(format!("Screen read failed: {error}")))?;
    if rows != height {
      return Err(EngineError::protocol("Screen read returned too few rows"));
    }
  }
  Ok(find_clock(&pixels, width as usize, height as usize))
}

/// A 32-bit `CF_DIB` as a top-down image: (pixels, width, height).
fn dib_pixels(dib: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
  let word = |at: usize| dib.get(at..at + 4)?.try_into().ok().map(u32::from_le_bytes);
  let header = word(0)? as usize;
  let width = usize::try_from(word(4)? as i32).ok()?;
  let raw_height = word(8)? as i32;
  let bits = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?);
  let masks = if header == 40 && word(16)? == BI_BITFIELDS.0 {
    12
  } else {
    0
  };
  if bits != 32 || width == 0 || raw_height == 0 {
    return None;
  }
  let height = raw_height.unsigned_abs() as usize;
  let stride = width * 4;
  let body = dib.get(header + masks..header + masks + stride * height)?;
  if raw_height < 0 {
    return Some((body.to_vec(), width, height));
  }
  let flipped = body
    .chunks_exact(stride)
    .rev()
    .flatten()
    .copied()
    .collect::<Vec<_>>();
  Some((flipped, width, height))
}

fn read_clipboard(before: u32) -> Result<Option<u32>> {
  let started = Instant::now();
  // SAFETY: reads a counter only.
  while unsafe { GetClipboardSequenceNumber() } == before {
    if started.elapsed() > CLIPBOARD_DEADLINE {
      return Ok(None);
    }
    std::thread::sleep(Duration::from_millis(10));
  }
  std::thread::sleep(Duration::from_millis(50));
  // SAFETY: the clipboard is opened and closed here, and the block stays locked while read.
  let dib = unsafe {
    OpenClipboard(None).map_err(|error| EngineError::protocol(format!("Clipboard: {error}")))?;
    let copied = GetClipboardData(8).ok().and_then(|handle: HANDLE| {
      let global = windows::Win32::Foundation::HGLOBAL(handle.0);
      let data = GlobalLock(global);
      if data.is_null() {
        return None;
      }
      let bytes = std::slice::from_raw_parts(data.cast::<u8>(), GlobalSize(global)).to_vec();
      let _ = GlobalUnlock(global);
      Some(bytes)
    });
    let _ = CloseClipboard();
    copied
  };
  Ok(
    dib
      .and_then(|dib| dib_pixels(&dib))
      .and_then(|(pixels, width, height)| find_clock(&pixels, width, height)),
  )
}

fn mouse(screen: (i32, i32), x: i32, y: i32, flags: MOUSE_EVENT_FLAGS) -> Result<()> {
  let input = INPUT {
    r#type: INPUT_MOUSE,
    Anonymous: INPUT_0 {
      mi: MOUSEINPUT {
        dx: x * 65535 / (screen.0 - 1),
        dy: y * 65535 / (screen.1 - 1),
        dwFlags: flags | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
        ..Default::default()
      },
    },
  };
  // SAFETY: a fully initialized single event.
  if unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) } == 1 {
    Ok(())
  } else {
    Err(EngineError::protocol(
      "Windows rejected injected mouse input",
    ))
  }
}

fn drag_over(rect: RECT, screen: (i32, i32)) -> Result<()> {
  let (left, top) = (rect.left - SELECTION_MARGIN, rect.top - SELECTION_MARGIN);
  let (right, bottom) = (
    rect.right + SELECTION_MARGIN,
    rect.bottom + SELECTION_MARGIN,
  );
  let settle = || std::thread::sleep(Duration::from_millis(40));
  mouse(screen, left, top, MOUSE_EVENT_FLAGS(0))?;
  settle();
  mouse(screen, left, top, MOUSEEVENTF_LEFTDOWN)?;
  settle();
  mouse(
    screen,
    (left + right) / 2,
    (top + bottom) / 2,
    MOUSE_EVENT_FLAGS(0),
  )?;
  settle();
  mouse(screen, right, bottom, MOUSE_EVENT_FLAGS(0))?;
  settle();
  mouse(screen, right, bottom, MOUSEEVENTF_LEFTUP)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadMode {
  Screen,
  Clipboard,
}

#[derive(Debug)]
struct Options {
  tool: String,
  processes: Vec<String>,
  keys: String,
  read: ReadMode,
  runs: u32,
  gap: Duration,
  output: PathBuf,
}

#[derive(Debug, Serialize)]
struct Report {
  tool: String,
  keys: String,
  read: String,
  refresh_hz: i32,
  warmup_ms: Option<f64>,
  samples_ms: Vec<f64>,
  median_ms: Option<f64>,
  min_ms: Option<f64>,
  max_ms: Option<f64>,
  unreadable: u32,
  not_frozen: u32,
  timeouts: u32,
  stopped_early: Option<String>,
}

fn parse_options(args: &[OsString]) -> Result<Options> {
  let (mut tool, mut processes, mut keys, mut output) = (None, None, None, None);
  let mut read = ReadMode::Screen;
  let mut runs = 20;
  let mut gap = Duration::from_millis(1500);
  let mut args = args.iter();
  while let Some(flag) = args.next() {
    let flag = flag
      .to_str()
      .ok_or_else(|| EngineError::protocol("Invalid option"))?;
    let value = args
      .next()
      .and_then(|value| value.to_str())
      .ok_or_else(|| EngineError::protocol(format!("Missing value for {flag}")))?;
    match flag {
      "--tool" => tool = Some(value.to_owned()),
      "--process" => {
        processes = Some(
          value
            .split(',')
            .map(|name| name.trim().to_ascii_lowercase())
            .collect::<Vec<_>>(),
        );
      },
      "--keys" => keys = Some(value.to_owned()),
      "--read" => {
        read = match value {
          "screen" => ReadMode::Screen,
          "clipboard" => ReadMode::Clipboard,
          _ => return Err(EngineError::protocol("--read must be screen or clipboard")),
        };
      },
      "--runs" => {
        runs = value
          .parse()
          .map_err(|_| EngineError::protocol("Invalid --runs"))?;
      },
      "--gap-ms" => {
        gap = Duration::from_millis(
          value
            .parse()
            .map_err(|_| EngineError::protocol("Invalid --gap-ms"))?,
        );
      },
      "--output" => output = Some(PathBuf::from(value)),
      _ => return Err(EngineError::protocol(format!("Unknown option {flag}"))),
    }
  }
  if runs == 0 || runs > MAX_RUNS {
    return Err(EngineError::protocol("--runs must be from 1 to 100"));
  }
  let missing = |name: &str| EngineError::protocol(format!("freeze-latency requires {name}"));
  Ok(Options {
    tool: tool.ok_or_else(|| missing("--tool"))?,
    processes: processes.ok_or_else(|| missing("--process"))?,
    keys: keys.ok_or_else(|| missing("--keys"))?,
    read,
    runs,
    gap,
    output: output.ok_or_else(|| missing("--output"))?,
  })
}

enum Outcome {
  Frozen(u32),
  Unreadable,
  NotFrozen,
}

/// Two reads apart: a value that moves means this is the live clock, not a frozen image.
fn read_frozen(rect: RECT) -> Result<Outcome> {
  let first = read_screen(rect)?;
  std::thread::sleep(READ_GAP);
  let second = read_screen(rect)?;
  Ok(match (first, second) {
    (Some(first), Some(second)) if first == second => Outcome::Frozen(first),
    (Some(_), Some(_)) => Outcome::NotFrozen,
    _ => Outcome::Unreadable,
  })
}

fn run_once(
  options: &Options,
  scanner: &mut Scanner,
  keys: &[Key],
  clock: &Clock,
  screen: (i32, i32),
) -> Result<Option<(Outcome, Instant)>> {
  // SAFETY: reads a counter only.
  let before = unsafe { GetClipboardSequenceNumber() };
  let Some(shown) = wait_for_overlay(scanner, keys)? else {
    return Ok(None);
  };
  std::thread::sleep(Duration::from_millis(250));
  let outcome = match options.read {
    ReadMode::Screen => {
      let outcome = read_frozen(clock.rect)?;
      if !close_overlay(shown.window)? {
        return Err(EngineError::protocol("Overlay did not close after Escape"));
      }
      outcome
    },
    ReadMode::Clipboard => {
      drag_over(clock.rect, screen)?;
      match read_clipboard(before)? {
        Some(value) => Outcome::Frozen(value),
        None => Outcome::Unreadable,
      }
    },
  };
  Ok(Some((outcome, shown.pressed)))
}

fn measure(options: &Options) -> Result<Report> {
  let keys = parse_keys(&options.keys)?;
  let screen = screen_size();
  let clock = start_clock(screen)?;
  let mut scanner = Scanner::new(options.processes.clone(), screen);
  // SAFETY: reads a device capability from a screen DC released immediately.
  let refresh_hz = unsafe {
    let dc = GetDC(None);
    let rate = GetDeviceCaps(Some(dc), VREFRESH);
    ReleaseDC(None, dc);
    rate
  };
  let mut warmup_ms = None;
  let mut samples_ms = Vec::new();
  let (mut unreadable, mut not_frozen, mut timeouts) = (0, 0, 0);
  let mut stopped_early = None;
  for run in 0..=options.runs {
    mouse(screen, screen.0 / 2, 60, MOUSE_EVENT_FLAGS(0))?;
    std::thread::sleep(Duration::from_millis(30));
    match run_once(options, &mut scanner, &keys, &clock, screen) {
      Ok(Some((Outcome::Frozen(value), pressed))) => {
        let at_press = pressed.duration_since(clock.started).as_secs_f64() * 1000.0;
        let delay = f64::from(value) - at_press;
        if run == 0 {
          warmup_ms = Some(delay);
        } else {
          samples_ms.push(delay);
        }
      },
      Ok(Some((Outcome::Unreadable, _))) => unreadable += 1,
      Ok(Some((Outcome::NotFrozen, _))) => not_frozen += 1,
      Ok(None) => timeouts += 1,
      Err(error) => {
        stopped_early = Some(error.to_string());
        break;
      },
    }
    if timeouts + unreadable + not_frozen >= 4 {
      stopped_early = Some("too many failed runs".to_owned());
      break;
    }
    std::thread::sleep(options.gap);
  }
  let min_ms = samples_ms.iter().copied().min_by(f64::total_cmp);
  let max_ms = samples_ms.iter().copied().max_by(f64::total_cmp);
  Ok(Report {
    tool: options.tool.clone(),
    keys: options.keys.clone(),
    read: format!("{:?}", options.read).to_ascii_lowercase(),
    refresh_hz,
    warmup_ms,
    median_ms: median(&samples_ms),
    min_ms,
    max_ms,
    samples_ms,
    unreadable,
    not_frozen,
    timeouts,
    stopped_early,
  })
}

pub fn command(args: &[OsString]) -> Result<()> {
  let options = parse_options(args)?;
  // SAFETY: process-wide setting before any window; a failure only means it was already set.
  let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
  let report = measure(&options)?;
  let bytes =
    serde_json::to_vec_pretty(&report).map_err(|error| EngineError::protocol(error.to_string()))?;
  if let Some(parent) = options.output.parent() {
    std::fs::create_dir_all(parent).map_err(|error| EngineError::io("report folder", error))?;
  }
  std::fs::write(&options.output, bytes).map_err(|error| EngineError::io("report", error))?;
  crate::print_json(&report)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A top-down 32-bit image with the clock at (left, top), scaled toward black by `dim`.
  fn image(value: u32, width: usize, height: usize, left: usize, top: usize, dim: u8) -> Vec<u8> {
    let mut pixels = vec![40_u8; width * height * 4];
    let cells = encode(value);
    for y in top..top + HEIGHT as usize {
      for (index, set) in cells.iter().enumerate() {
        for x in 0..CELL as usize {
          let at = y * width * 4 + (left + index * CELL as usize + x) * 4;
          pixels[at..at + 4].fill(if *set { dim } else { 0 });
        }
      }
    }
    pixels
  }

  #[test]
  fn the_clock_reads_back_when_offset_and_dimmed() {
    for value in [0, 1, 37_123, (1 << VALUE_BITS) - 1] {
      let pixels = image(value, 660, 96, 27, 25, 95);
      assert_eq!(find_clock(&pixels, 660, 96), Some(value));
    }
  }

  #[test]
  fn noise_and_flat_images_read_as_nothing() {
    assert_eq!(find_clock(&vec![40_u8; 660 * 96 * 4], 660, 96), None);
    let noise = (0..660 * 96 * 4)
      .map(|index: u32| (index.wrapping_mul(2_654_435_761) >> 24) as u8)
      .collect::<Vec<_>>();
    assert_eq!(find_clock(&noise, 660, 96), None);
    assert_eq!(find_clock(&[0; 16], 2, 2), None);
  }

  #[test]
  fn a_half_updated_value_is_rejected() {
    let mut pixels = image(5, 660, 96, 27, 25, 255);
    let newer = image(6, 660, 96, 27, 25, 255);
    let middle = (27 + (2 + VALUE_BITS) * CELL as usize) * 4;
    for row in 0..96 {
      let start = row * 660 * 4;
      pixels[start..start + middle].copy_from_slice(&newer[start..start + middle]);
    }
    assert_eq!(find_clock(&pixels, 660, 96), None);
  }

  #[test]
  fn clipboard_images_decode_in_either_row_order() {
    let top_down = image(4242, 660, 96, 27, 25, 255);
    let header = |height: i32| {
      let mut dib = vec![0_u8; 40];
      dib[0..4].copy_from_slice(&40_u32.to_le_bytes());
      dib[4..8].copy_from_slice(&660_i32.to_le_bytes());
      dib[8..12].copy_from_slice(&height.to_le_bytes());
      dib[14..16].copy_from_slice(&32_u16.to_le_bytes());
      dib
    };
    let mut negative = header(-96);
    negative.extend_from_slice(&top_down);
    let mut positive = header(96);
    positive.extend(top_down.as_chunks::<{ 660 * 4 }>().0.iter().rev().flatten());
    for dib in [negative, positive] {
      let decoded =
        dib_pixels(&dib).map(|(pixels, width, height)| find_clock(&pixels, width, height));
      assert_eq!(decoded, Some(Some(4242)));
    }
    assert!(dib_pixels(&[0; 8]).is_none());
  }

  #[test]
  fn options_need_a_known_read_mode() {
    let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
    let complete = [
      "--tool",
      "Flinch",
      "--process",
      "flinch.exe",
      "--keys",
      "Home",
      "--output",
      "out.json",
    ];
    assert!(parse_options(&args(&complete)).is_ok());
    let mut bad = complete.to_vec();
    bad.extend(["--read", "camera"]);
    assert!(parse_options(&args(&bad)).is_err());
    assert!(parse_options(&args(&complete[..6])).is_err());
  }
}
