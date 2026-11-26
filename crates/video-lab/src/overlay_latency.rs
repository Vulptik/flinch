//! Explicit desktop benchmark: a capture shortcut to the tool's overlay window becoming
//! visible. It injects global input, so it never runs in quiet validation. The end
//! boundary is window visibility because Flinch's overlay is excluded from desktop
//! capture; a tool may show its window before drawing into it.
use flinch_video_engine::{EngineError, Result};
use serde::Serialize;
use std::{
  collections::{HashMap, HashSet},
  ffi::{OsString, c_void},
  path::PathBuf,
  time::{Duration, Instant},
};
use windows::Win32::{
  Foundation::{CloseHandle, HWND, LPARAM, RECT},
  Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
  System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
  },
  UI::{
    Input::KeyboardAndMouse::{
      INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
      KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
    },
    WindowsAndMessaging::{
      EnumWindows, GetClassNameW, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId,
      IsWindowVisible, SM_CXSCREEN, SM_CYSCREEN,
    },
  },
};
use windows_core::{BOOL, PWSTR};

const OVERLAY_DEADLINE: Duration = Duration::from_secs(5);
const CLOSE_DEADLINE: Duration = Duration::from_secs(3);
const HOLD: Duration = Duration::from_millis(400);
const MAX_RUNS: u32 = 200;
const MAX_TIMEOUTS: u32 = 3;
const ESCAPE: Key = Key {
  code: 0x1B,
  extended: false,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
  code: u16,
  extended: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
  tool: String,
  processes: Vec<String>,
  keys: String,
  runs: u32,
  gap: Duration,
  output: PathBuf,
}

#[derive(Debug, Serialize)]
struct Report {
  tool: String,
  processes: Vec<String>,
  keys: String,
  gap_ms: u128,
  end_boundary: &'static str,
  screen: (i32, i32),
  warmup_ms: Option<f64>,
  samples_ms: Vec<f64>,
  median_ms: Option<f64>,
  min_ms: Option<f64>,
  max_ms: Option<f64>,
  timeouts: u32,
  window_classes: Vec<String>,
  stopped_early: Option<String>,
}

pub(crate) fn parse_keys(text: &str) -> Result<Vec<Key>> {
  let keys = text
    .split('+')
    .map(|name| parse_key(name.trim()))
    .collect::<Result<Vec<_>>>()?;
  if keys.is_empty() {
    return Err(EngineError::protocol("Shortcut has no keys"));
  }
  Ok(keys)
}

fn parse_key(name: &str) -> Result<Key> {
  let plain = |code| Key {
    code,
    extended: false,
  };
  let extended = |code| Key {
    code,
    extended: true,
  };
  let upper = name.to_ascii_uppercase();
  Ok(match upper.as_str() {
    "CTRL" => plain(0x11),
    "SHIFT" => plain(0x10),
    "ALT" => plain(0x12),
    "WIN" => extended(0x5B),
    "HOME" => extended(0x24),
    "PRINTSCREEN" => extended(0x2C),
    other => match other.as_bytes() {
      [letter] if letter.is_ascii_alphanumeric() => plain(u16::from(*letter)),
      [b'F', ..] => match other[1..].parse::<u16>() {
        Ok(number @ 1..=12) => plain(0x6F + number),
        _ => return Err(EngineError::protocol(format!("Unknown key {name}"))),
      },
      _ => return Err(EngineError::protocol(format!("Unknown key {name}"))),
    },
  })
}

fn parse_options(args: &[OsString]) -> Result<Options> {
  let mut tool = None;
  let mut processes = None;
  let mut keys = None;
  let mut runs = 20;
  let mut gap = Duration::from_millis(1500);
  let mut output = None;
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
    return Err(EngineError::protocol("--runs must be from 1 to 200"));
  }
  let missing = |name: &str| EngineError::protocol(format!("overlay-latency requires {name}"));
  Ok(Options {
    tool: tool.ok_or_else(|| missing("--tool"))?,
    processes: processes.ok_or_else(|| missing("--process"))?,
    keys: keys.ok_or_else(|| missing("--keys"))?,
    runs,
    gap,
    output: output.ok_or_else(|| missing("--output"))?,
  })
}

pub(crate) fn median(samples: &[f64]) -> Option<f64> {
  let mut sorted = samples.to_vec();
  sorted.sort_by(f64::total_cmp);
  let middle = sorted.len() / 2;
  match sorted.len() {
    0 => None,
    length if length % 2 == 1 => Some(sorted[middle]),
    _ => Some((sorted[middle - 1] + sorted[middle]) / 2.0),
  }
}

/// Overlays cover the desktop; this ignores a tool's tray, toast and settings windows.
fn covers_screen(width: i32, height: i32, screen: (i32, i32)) -> bool {
  i64::from(width) * i64::from(height) * 2 >= i64::from(screen.0) * i64::from(screen.1)
}

pub(crate) fn press(keys: &[Key]) -> Result<()> {
  let event = |key: &Key, up: bool| {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if key.extended {
      flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
      flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
      r#type: INPUT_KEYBOARD,
      Anonymous: INPUT_0 {
        ki: KEYBDINPUT {
          wVk: VIRTUAL_KEY(key.code),
          dwFlags: flags,
          ..Default::default()
        },
      },
    }
  };
  let inputs = keys
    .iter()
    .map(|key| event(key, false))
    .chain(keys.iter().rev().map(|key| event(key, true)))
    .collect::<Vec<_>>();
  // SAFETY: `inputs` is a live slice of fully initialized keyboard events.
  let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
  if sent as usize == inputs.len() {
    Ok(())
  } else {
    Err(EngineError::protocol("Windows rejected injected input"))
  }
}

unsafe extern "system" fn collect_window(hwnd: HWND, list: LPARAM) -> BOOL {
  // SAFETY: `top_level_windows` passes a live Vec that outlives this synchronous call.
  let list = unsafe { &mut *(list.0 as *mut Vec<isize>) };
  list.push(hwnd.0 as isize);
  true.into()
}

fn top_level_windows() -> Result<Vec<isize>> {
  let mut list = Vec::<isize>::with_capacity(256);
  // SAFETY: the callback only runs during this call, while `list` is alive.
  unsafe {
    EnumWindows(
      Some(collect_window),
      LPARAM(std::ptr::from_mut(&mut list) as isize),
    )
  }
  .map_err(|error| EngineError::protocol(format!("Cannot list windows: {error}")))?;
  Ok(list)
}

fn shown(hwnd: HWND) -> bool {
  // SAFETY: these calls tolerate stale handles and only read window state.
  unsafe {
    if !IsWindowVisible(hwnd).as_bool() {
      return false;
    }
    let mut cloaked = 0_u32;
    let read = DwmGetWindowAttribute(
      hwnd,
      DWMWA_CLOAKED,
      std::ptr::from_mut(&mut cloaked).cast::<c_void>(),
      std::mem::size_of::<u32>() as u32,
    );
    read.is_err() || cloaked == 0
  }
}

fn process_name(pid: u32) -> Option<String> {
  // SAFETY: the handle is checked, read with a bounded buffer and closed here.
  unsafe {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buffer = [0_u16; 1024];
    let mut length = buffer.len() as u32;
    let read = QueryFullProcessImageNameW(
      process,
      PROCESS_NAME_WIN32,
      PWSTR(buffer.as_mut_ptr()),
      &mut length,
    );
    let _ = CloseHandle(process);
    read.ok()?;
    let path = String::from_utf16_lossy(&buffer[..length as usize]);
    path
      .rsplit('\\')
      .next()
      .map(|name| name.to_ascii_lowercase())
  }
}

fn class_name(hwnd: HWND) -> String {
  let mut buffer = [0_u16; 256];
  // SAFETY: the buffer is bounded and the call tolerates a stale handle.
  let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
  String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

pub(crate) struct Scanner {
  processes: Vec<String>,
  matches: HashMap<u32, bool>,
  screen: (i32, i32),
}

pub(crate) fn screen_size() -> (i32, i32) {
  // SAFETY: reads system metrics only.
  unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

impl Scanner {
  pub(crate) fn new(processes: Vec<String>, screen: (i32, i32)) -> Self {
    Self {
      processes,
      matches: HashMap::new(),
      screen,
    }
  }

  fn owned(&mut self, hwnd: HWND) -> bool {
    let mut pid = 0_u32;
    // SAFETY: `pid` is a live out-parameter and stale handles return zero.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
      return false;
    }
    let processes = &self.processes;
    *self
      .matches
      .entry(pid)
      .or_insert_with(|| process_name(pid).is_some_and(|name| processes.contains(&name)))
  }

  fn overlays(&mut self) -> Result<Vec<isize>> {
    let mut found = Vec::new();
    for handle in top_level_windows()? {
      let hwnd = HWND(handle as *mut c_void);
      let mut rect = RECT::default();
      // SAFETY: `rect` is a live out-parameter; a stale handle simply fails.
      let sized = unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok();
      if sized
        && covers_screen(rect.right - rect.left, rect.bottom - rect.top, self.screen)
        && shown(hwnd)
        && self.owned(hwnd)
      {
        found.push(handle);
      }
    }
    Ok(found)
  }
}

pub(crate) struct Shown {
  pub elapsed_ms: f64,
  pub window: isize,
  pub pressed: Instant,
}

pub(crate) fn wait_for_overlay(scanner: &mut Scanner, keys: &[Key]) -> Result<Option<Shown>> {
  let baseline = scanner.overlays()?.into_iter().collect::<HashSet<_>>();
  let started = Instant::now();
  press(keys)?;
  while started.elapsed() < OVERLAY_DEADLINE {
    if let Some(window) = scanner
      .overlays()?
      .into_iter()
      .find(|window| !baseline.contains(window))
    {
      return Ok(Some(Shown {
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        window,
        pressed: started,
      }));
    }
    std::thread::yield_now();
  }
  Ok(None)
}

pub(crate) fn close_overlay(window: isize) -> Result<bool> {
  std::thread::sleep(HOLD);
  press(&[ESCAPE])?;
  let started = Instant::now();
  while started.elapsed() < CLOSE_DEADLINE {
    if !shown(HWND(window as *mut c_void)) {
      return Ok(true);
    }
    std::thread::sleep(Duration::from_millis(5));
  }
  Ok(false)
}

fn measure(options: &Options) -> Result<Report> {
  let keys = parse_keys(&options.keys)?;
  let screen = screen_size();
  let mut scanner = Scanner::new(options.processes.clone(), screen);
  let mut warmup_ms = None;
  let mut samples_ms = Vec::new();
  let mut timeouts = 0;
  let mut classes = HashSet::new();
  let mut stopped_early = None;
  for run in 0..=options.runs {
    match wait_for_overlay(&mut scanner, &keys)? {
      Some(Shown {
        elapsed_ms: elapsed,
        window,
        ..
      }) => {
        classes.insert(class_name(HWND(window as *mut c_void)));
        if run == 0 {
          warmup_ms = Some(elapsed);
        } else {
          samples_ms.push(elapsed);
        }
        if !close_overlay(window)? {
          stopped_early = Some("overlay did not close after Escape".to_owned());
          break;
        }
      },
      None => {
        timeouts += 1;
        if timeouts >= MAX_TIMEOUTS {
          stopped_early = Some("too many overlay timeouts".to_owned());
          break;
        }
      },
    }
    std::thread::sleep(options.gap);
  }
  let min_ms = samples_ms.iter().copied().min_by(f64::total_cmp);
  let max_ms = samples_ms.iter().copied().max_by(f64::total_cmp);
  let mut window_classes = classes.into_iter().collect::<Vec<_>>();
  window_classes.sort();
  Ok(Report {
    tool: options.tool.clone(),
    processes: options.processes.clone(),
    keys: options.keys.clone(),
    gap_ms: options.gap.as_millis(),
    end_boundary: "new visible uncloaked window covering half the primary screen",
    screen,
    warmup_ms,
    median_ms: median(&samples_ms),
    min_ms,
    max_ms,
    samples_ms,
    timeouts,
    window_classes,
    stopped_early,
  })
}

pub fn command(args: &[OsString]) -> Result<()> {
  let options = parse_options(args)?;
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

  fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
  }

  #[test]
  fn shortcuts_parse_with_extended_keys() -> Result<()> {
    let keys = parse_keys("Ctrl+Shift+Home")?;
    assert_eq!(keys.len(), 3);
    assert_eq!(keys[2], parse_key("home")?);
    assert!(keys[2].extended);
    assert_eq!(parse_key("F12")?.code, 0x7B);
    assert_eq!(parse_key("s")?.code, u16::from(b'S'));
    assert!(parse_keys("Ctrl+Nope").is_err());
    assert!(parse_key("F13").is_err());
    Ok(())
  }

  #[test]
  fn options_require_a_target_and_bound_runs() -> Result<()> {
    let complete = [
      "--tool",
      "Flinch",
      "--process",
      "Flinch.exe, other.exe",
      "--keys",
      "Ctrl+Home",
      "--output",
      "report.json",
    ];
    let options = parse_options(&args(&complete))?;
    assert_eq!(options.processes, ["flinch.exe", "other.exe"]);
    assert_eq!(options.runs, 20);
    assert!(parse_options(&args(&complete[..6])).is_err());
    let mut too_many = complete.to_vec();
    too_many.extend(["--runs", "201"]);
    assert!(parse_options(&args(&too_many)).is_err());
    assert!(parse_options(&args(&["--tool"])).is_err());
    Ok(())
  }

  #[test]
  fn median_handles_odd_even_and_empty() {
    assert_eq!(median(&[]), None);
    assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
    assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
  }

  #[test]
  fn only_large_windows_count_as_overlays() {
    let screen = (2560, 1440);
    assert!(covers_screen(2560, 1440, screen));
    assert!(covers_screen(2560, 720, screen));
    assert!(!covers_screen(1280, 700, screen));
    assert!(!covers_screen(0, 0, screen));
  }
}
