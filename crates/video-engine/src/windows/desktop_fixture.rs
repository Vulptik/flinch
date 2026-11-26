//! Explicit supervised desktop workload. Its WinMM output clock is independent of the recorder's WASAPI clock.
use crate::{
  EngineError, ErrorKind, Result,
  metrics::{Distribution, Histogram},
};
use serde::{Deserialize, Serialize};
use std::{
  mem::ManuallyDrop,
  sync::atomic::{AtomicBool, AtomicU64, Ordering},
  time::{Duration, Instant},
};
use windows::{
  Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    Media::{Audio::*, MMTIME, TIME_SAMPLES},
    System::LibraryLoader::GetModuleHandleW,
    UI::{HiDpi::*, WindowsAndMessaging::*},
  },
  core::{PCWSTR, w},
};

static POSITION: AtomicU64 = AtomicU64::new(0);
static CLOSED: AtomicBool = AtomicBool::new(false);
static PAINT_FAILED: AtomicBool = AtomicBool::new(false);
/// Render-clock coordinates shared with offline verification; changing them changes the marker contract.
pub const RATE: u64 = 48000;
pub const FRAMES_PER_SECOND: u64 = 60;
/// Tone and green-pulse window within each render second, in render samples.
pub const PULSE: std::ops::Range<u64> = 24000..28800;
/// 32 cells of 16 px encode the frame index in rows 0..16 and the render-sample clock in rows
/// 16..32. The clock strip is repainted between full frames so each captured image dates its
/// content to the paint loop's granularity rather than to the 60 FPS frame.
pub const CELL: i32 = 16;
pub const CLOCK_TOP: i32 = 16;
pub const CODE_WIDTH: i32 = 32 * CELL;
/// Leave preparation/teardown headroom around a three-hour recorder run.
pub const MAX_SECONDS: f64 = 10830.0;

#[derive(Serialize, Deserialize)]
pub struct MarkerObservation {
  pub second: u64,
  pub render_sample_position: u64,
  pub paint_submission_qpc_100ns: u64,
}
#[derive(Serialize, Deserialize)]
pub struct FixtureReport {
  pub clock: String,
  pub presentation_limit: String,
  pub elapsed_seconds: f64,
  pub rendered_samples: u64,
  pub frames_painted: u64,
  pub marker_submission_lateness: Distribution,
  pub markers: Vec<MarkerObservation>,
  /// Absent in reports from fixtures without the render-clock strip.
  #[serde(default)]
  pub clock_strip: Option<ClockStrip>,
  /// The fixture display's refresh rate; the verifier's variation limit depends on whether it
  /// is a multiple of the recording's frame rate. Absent in older reports.
  #[serde(default)]
  pub display_refresh_hz: Option<u32>,
  /// DWM's composition period, which stamps presents and can differ from the display's mode
  /// (an indirect display composed at another display's cadence). Preferred by the verifier.
  #[serde(default)]
  pub composition_period_100ns: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClockStrip {
  pub unit: ClockUnit,
  pub paints: u64,
  /// Largest render-clock advance between consecutive strip paints; a captured strip value
  /// may be older than the composed image by up to this amount.
  pub maximum_gap_samples: u64,
  /// The strip shows the render position interpolated with QPC between WinMM updates. These
  /// bound how far the shown clock ran ahead of, or behind, the next real position; the
  /// verifier widens every interval by them. Zero in reports from uninterpolated fixtures.
  #[serde(default)]
  pub maximum_lead_samples: u64,
  #[serde(default)]
  pub maximum_lag_samples: u64,
  /// The same bounds around each second's pulse. A marker uses its own window, so a paint-loop
  /// stall elsewhere in the run does not widen it; reports without windows use the maxima.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub pulse_windows: Vec<PulseWindow>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PulseWindow {
  pub second: u64,
  pub maximum_gap_samples: u64,
  pub maximum_lead_samples: u64,
  pub maximum_lag_samples: u64,
}
/// Render samples per fixture frame.
const FRAME_SAMPLES: u64 = RATE / FRAMES_PER_SECOND;
/// Strip values a pulse frame's capture can show: from one frame before the pulse (margin) to
/// three frames after it ends (the pulse frame plus a torn one-slot lead, with margin).
pub fn pulse_window_second(samples: u64) -> Option<u64> {
  let offset = samples % RATE;
  (offset + FRAME_SAMPLES >= PULSE.start && offset < PULSE.end + 3 * FRAME_SAMPLES)
    .then_some(samples / RATE)
}
/// Per-pulse strip bounds, built in render order. A window opens at the paint that first shows
/// its pulse: a pulse frame's strip value is never older than that paint (the verifier refuses one
/// that is), so the slower paints just before it no longer widen the marker.
#[derive(Default)]
struct PulseWindows {
  windows: Vec<PulseWindow>,
  /// Render position of the newest window's pulse paint.
  from: Option<u64>,
}
impl PulseWindows {
  fn open(&mut self, second: u64, position: u64) {
    self.windows.push(PulseWindow {
      second,
      ..PulseWindow::default()
    });
    self.from = Some(position);
  }
  fn at(&mut self, samples: u64) -> Option<&mut PulseWindow> {
    let from = self.from?;
    let window = self.windows.last_mut()?;
    (samples >= from && pulse_window_second(samples) == Some(window.second)).then_some(window)
  }
}
/// The moving band sits right below the two code rows, away from the centre pulse sensor, so a
/// frame update repaints one compact strip of rows (measured: whole-window GDI paints
/// held the clock strip for 3 ms around every pulse).
const BAND_TOP: i32 = CLOCK_TOP + CELL;
/// A quarter of the height, ending above the centre row (empty on buffers under 128 rows).
fn band_bottom(height: i32) -> i32 {
  BAND_TOP + (height / 4).min(height / 2 - BAND_TOP).max(0)
}
/// What a paint must refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
  /// The clock strip only, between frames.
  Clock,
  /// A new frame: code rows and moving band.
  Frame,
  /// A pulse edge: the centre block only, painted after the frame's codes.
  Pulse,
  /// Everything: the first paint, or a system repaint after the window was covered.
  Full,
}
/// The green pulse covers a centre block around the verifier's centre sensor rather than the
/// whole window: a full-window flash made DWM skip vblanks composing it (traced: the
/// skipped present never arrived), so the pulse reached the screen a refresh after its paint and
/// the fixture misdated it. It stays below the band, so the regions never overlap.
fn pulse_block(width: i32, height: i32) -> RECT {
  let (half_width, half_height) = ((width / 16).max(1), (height / 16).max(1));
  RECT {
    left: width / 2 - half_width,
    top: (height / 2 - half_height).max(band_bottom(height)),
    right: width / 2 + half_width,
    bottom: height / 2 + half_height,
  }
}
/// Classify a paint by its update rectangle. Only the loop's own invalidations fit the small
/// regions, so a system repaint always refreshes the whole window.
fn region(update: &RECT, width: i32, height: i32) -> Region {
  let within = |bounds: RECT| {
    update.left >= bounds.left
      && update.top >= bounds.top
      && update.right <= bounds.right
      && update.bottom <= bounds.bottom
  };
  let rect = |left, top, right, bottom| RECT {
    left,
    top,
    right,
    bottom,
  };
  if within(rect(0, CLOCK_TOP, CODE_WIDTH, CLOCK_TOP + CELL)) {
    Region::Clock
  } else if within(rect(0, 0, width, band_bottom(height))) {
    Region::Frame
  } else if within(pulse_block(width, height)) {
    Region::Pulse
  } else {
    Region::Full
  }
}

/// Longest the clock extrapolates past the last real position: 2 ms, twice the measured WinMM
/// update period. This also bounds the lead, so a device stall cannot widen every interval.
const MAX_EXTRAPOLATION: u64 = RATE / 500;

/// WinMM's render position advances about every 1 ms in 48-sample steps (measured:
/// 4,464 updates in 5 s, longest gap 4 ms at start-up). Between real updates this clock
/// extrapolates at the nominal rate from when the update was observed. It never runs backwards,
/// never extrapolates more than `MAX_EXTRAPOLATION`, and records how far it was ahead of or
/// behind each new real position, so the error is measured rather than assumed.
#[derive(Default)]
struct RenderClock {
  /// The last real position and the QPC time (100 ns) it was first observed.
  anchor: Option<(u64, u64)>,
  shown: u64,
  lead: u64,
  lag: u64,
  /// The latest update's (shown value it replaced, lead, lag), taken by the paint loop.
  correction: Option<(u64, u64, u64)>,
}
impl RenderClock {
  fn observe(&mut self, real: u64, now_100ns: u64) -> u64 {
    match self.anchor {
      Some((anchored, _)) if anchored == real => {},
      previous => {
        if previous.is_some() {
          // What the clock showed at this moment, against the position that replaces it. An
          // update observed late makes this conservative: the bound only grows.
          let shown = self.estimate(now_100ns);
          let (lead, lag) = (shown.saturating_sub(real), real.saturating_sub(shown));
          self.lead = self.lead.max(lead);
          self.lag = self.lag.max(lag);
          self.correction = Some((shown, lead, lag));
        }
        self.anchor = Some((real, now_100ns));
      },
    }
    self.shown = self.estimate(now_100ns);
    self.shown
  }
  /// The extrapolated position at `now`, never below what was already shown.
  fn estimate(&self, now_100ns: u64) -> u64 {
    let Some((anchored, since)) = self.anchor else {
      return self.shown;
    };
    let elapsed = now_100ns.saturating_sub(since);
    let extrapolated = u64::try_from(u128::from(elapsed) * u128::from(RATE) / 10_000_000)
      .unwrap_or(u64::MAX)
      .min(MAX_EXTRAPOLATION);
    self.shown.max(anchored.saturating_add(extrapolated))
  }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockUnit {
  RenderSamples48k,
}
fn mm(result: u32, stage: &str) -> Result<()> {
  if result == 0 {
    return Ok(());
  }
  Err(EngineError {
    native_code: Some(result as i32),
    ..EngineError::new(ErrorKind::Native, stage, format!("WinMM result {result}"))
  })
}
struct AudioLoop {
  handle: HWAVEOUT,
  header: ManuallyDrop<Box<WAVEHDR>>,
  samples: ManuallyDrop<Vec<i16>>,
  prepared: bool,
  started: bool,
}
impl AudioLoop {
  fn new() -> Result<Self> {
    let samples = (0..RATE)
      .flat_map(|frame| {
        let value = if PULSE.contains(&frame) {
          ((frame as f64 * std::f64::consts::TAU * 1000.0 / RATE as f64).sin() * 16000.0) as i16
        } else {
          0
        };
        [value, value]
      })
      .collect::<Vec<_>>();
    let mut result = Self {
      handle: HWAVEOUT::default(),
      header: ManuallyDrop::new(Box::new(WAVEHDR::default())),
      samples: ManuallyDrop::new(samples),
      prepared: false,
      started: false,
    };
    let format = WAVEFORMATEX {
      wFormatTag: 1,
      nChannels: 2,
      nSamplesPerSec: RATE as u32,
      nAvgBytesPerSec: RATE as u32 * 4,
      nBlockAlign: 4,
      wBitsPerSample: 16,
      cbSize: 0,
    };
    // SAFETY: fixed PCM input, no callback, one owner and stable pinned header/sample storage.
    unsafe {
      mm(
        waveOutOpen(
          Some(&mut result.handle),
          WAVE_MAPPER,
          &format,
          None,
          None,
          CALLBACK_NULL,
        ),
        "fixture audio open",
      )?;
      result.header.lpData = windows::core::PSTR(result.samples.as_mut_ptr().cast());
      result.header.dwBufferLength = (result.samples.len() * 2) as u32;
      result.header.dwFlags = WHDR_BEGINLOOP | WHDR_ENDLOOP;
      result.header.dwLoops = u32::MAX;
      mm(
        waveOutPrepareHeader(
          result.handle,
          &mut **result.header,
          std::mem::size_of::<WAVEHDR>() as u32,
        ),
        "fixture audio prepare",
      )?;
    }
    result.prepared = true;
    Ok(result)
  }
  fn start(&mut self) -> Result<()> {
    // SAFETY: header/sample storage remains fixed until reset and unprepare have succeeded.
    unsafe {
      mm(
        waveOutWrite(
          self.handle,
          &mut **self.header,
          std::mem::size_of::<WAVEHDR>() as u32,
        ),
        "fixture audio start",
      )?;
    }
    self.started = true;
    Ok(())
  }
  fn position(&self) -> Result<u64> {
    let mut position = MMTIME {
      wType: TIME_SAMPLES,
      ..Default::default()
    };
    // SAFETY: native output clock writes this live structure; no recorder clock or packet data is consulted.
    unsafe {
      mm(
        waveOutGetPosition(
          self.handle,
          &mut position,
          std::mem::size_of::<MMTIME>() as u32,
        ),
        "fixture output clock",
      )?;
    }
    if position.wType != TIME_SAMPLES {
      return Err(EngineError::new(
        ErrorKind::Capability,
        "fixture output clock",
        "The render device did not provide a sample coordinate",
      ));
    }
    // SAFETY: TIME_SAMPLES selects sample in the published MMTIME union.
    Ok(u64::from(unsafe { position.u.sample }))
  }
  fn close(&mut self) -> Result<()> {
    // SAFETY: serialized on this owner; storage is freed only after native ownership is released.
    unsafe {
      if self.started {
        mm(waveOutReset(self.handle), "fixture audio reset")?;
        self.started = false;
      }
      if self.prepared {
        mm(
          waveOutUnprepareHeader(
            self.handle,
            &mut **self.header,
            std::mem::size_of::<WAVEHDR>() as u32,
          ),
          "fixture audio unprepare",
        )?;
        self.prepared = false;
      }
      if !self.handle.0.is_null() {
        mm(waveOutClose(self.handle), "fixture audio close")?;
        self.handle = HWAVEOUT::default();
      }
    }
    Ok(())
  }
}
impl Drop for AudioLoop {
  fn drop(&mut self) {
    if let Err(error) = self.close() {
      // A bounded buffer must outlive a driver that refused release. The supervised fixture process owns it until exit.
      eprintln!("{error}; retaining native fixture buffers until process exit");
      return;
    }
    // SAFETY: successful reset/unprepare/close removed every native reference, and these fields are dropped exactly once.
    unsafe {
      ManuallyDrop::drop(&mut self.header);
      ManuallyDrop::drop(&mut self.samples);
    }
  }
}
trait RenderAudio {
  fn start(&mut self) -> Result<()>;
  fn position(&self) -> Result<u64>;
  fn close(&mut self) -> Result<()>;
}
impl RenderAudio for AudioLoop {
  fn start(&mut self) -> Result<()> {
    AudioLoop::start(self)
  }
  fn position(&self) -> Result<u64> {
    AudioLoop::position(self)
  }
  fn close(&mut self) -> Result<()> {
    AudioLoop::close(self)
  }
}
fn draw(dc: HDC, width: i32, height: i32, samples: u64) -> Result<()> {
  let frame = samples * FRAMES_PER_SECOND / RATE;
  // SAFETY: a live paint DC and stock brush; no owned GDI object is created or destroyed here.
  unsafe {
    let brush = HBRUSH(GetStockObject(DC_BRUSH).0);
    if brush.0.is_null() || SetDCBrushColor(dc, COLORREF(0)).0 == u32::MAX {
      return Err(EngineError::native(
        "fixture brush",
        windows::core::Error::from_thread(),
      ));
    }
    if FillRect(
      dc,
      &RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
      },
      brush,
    ) == 0
    {
      return Err(EngineError::native(
        "fixture background",
        windows::core::Error::from_thread(),
      ));
    }
  }
  draw_pulse(dc, width, height, samples)?;
  draw_band(dc, width, height, frame)?;
  draw_code(dc, 0, frame, "fixture frame index")?;
  draw_clock(dc, samples)
}
/// The centre block: green inside the pulse, black otherwise.
fn draw_pulse(dc: HDC, width: i32, height: i32, samples: u64) -> Result<()> {
  let pulse = PULSE.contains(&(samples % RATE));
  // SAFETY: a live paint DC and stock brush; no owned GDI object is created or destroyed here.
  unsafe {
    let brush = HBRUSH(GetStockObject(DC_BRUSH).0);
    if brush.0.is_null()
      || SetDCBrushColor(dc, COLORREF(if pulse { 0x0000ff00 } else { 0 })).0 == u32::MAX
      || FillRect(dc, &pulse_block(width, height), brush) == 0
    {
      return Err(EngineError::native(
        "fixture pulse",
        windows::core::Error::from_thread(),
      ));
    }
  }
  Ok(())
}
/// Moving edges away from the marker sensor, without turning unrelated motion into a green pulse.
fn draw_band(dc: HDC, width: i32, height: i32, frame: u64) -> Result<()> {
  // SAFETY: a live paint DC and stock brush; no owned GDI object is created or destroyed here.
  unsafe {
    let brush = HBRUSH(GetStockObject(DC_BRUSH).0);
    if brush.0.is_null() {
      return Err(EngineError::native(
        "fixture moving edges",
        windows::core::Error::from_thread(),
      ));
    }
    let tile = 64;
    for column in 0..(width / tile + 2) {
      let left = column * tile - (frame % tile as u64) as i32;
      let color = if column % 2 == 0 {
        0x00404040
      } else {
        0x00101010
      };
      if SetDCBrushColor(dc, COLORREF(color)).0 == u32::MAX
        || FillRect(
          dc,
          &RECT {
            left: left.max(0),
            top: BAND_TOP,
            right: (left + tile).min(width),
            bottom: band_bottom(height),
          },
          brush,
        ) == 0
      {
        return Err(EngineError::native(
          "fixture moving edges",
          windows::core::Error::from_thread(),
        ));
      }
    }
  }
  Ok(())
}
fn draw_clock(dc: HDC, samples: u64) -> Result<()> {
  let clock = u32::try_from(samples)
    .map_err(|_| EngineError::protocol("Fixture render clock exceeds its 32-bit strip"))?;
  draw_code(dc, CLOCK_TOP, u64::from(clock), "fixture render clock")
}
/// Paint 32 black/white cells encoding `value`, least significant bit first.
fn draw_code(dc: HDC, top: i32, value: u64, stage: &str) -> Result<()> {
  // SAFETY: a live paint DC and stock brush; no owned GDI object is created or destroyed here.
  unsafe {
    let brush = HBRUSH(GetStockObject(DC_BRUSH).0);
    if brush.0.is_null() {
      return Err(EngineError::native(
        stage,
        windows::core::Error::from_thread(),
      ));
    }
    for bit in 0..32 {
      let color = if (value >> bit) & 1 != 0 {
        0x00ffffff
      } else {
        0
      };
      if SetDCBrushColor(dc, COLORREF(color)).0 == u32::MAX
        || FillRect(
          dc,
          &RECT {
            left: bit * CELL,
            top,
            right: bit * CELL + CELL,
            bottom: top + CELL,
          },
          brush,
        ) == 0
      {
        return Err(EngineError::native(
          stage,
          windows::core::Error::from_thread(),
        ));
      }
    }
  }
  Ok(())
}
struct PaintBuffer {
  dc: HDC,
  bitmap: HBITMAP,
  previous: HGDIOBJ,
  width: i32,
  height: i32,
}
impl PaintBuffer {
  fn new(width: i32, height: i32) -> Result<Self> {
    // SAFETY: the screen DC supplies only its pixel format; no screen pixels are read.
    unsafe {
      let reference = GetDC(None);
      if reference.0.is_null() {
        return Err(EngineError::native(
          "fixture compatible DC",
          windows::core::Error::from_thread(),
        ));
      }
      let mut result = Self {
        dc: CreateCompatibleDC(Some(reference)),
        bitmap: HBITMAP::default(),
        previous: HGDIOBJ::default(),
        width,
        height,
      };
      if !result.dc.0.is_null() {
        result.bitmap = CreateCompatibleBitmap(reference, width, height);
      }
      if ReleaseDC(None, reference) == 0 {
        return Err(EngineError::native(
          "fixture reference DC release",
          windows::core::Error::from_thread(),
        ));
      }
      if result.dc.0.is_null() || result.bitmap.0.is_null() {
        return Err(EngineError::native(
          "fixture paint buffer",
          windows::core::Error::from_thread(),
        ));
      }
      result.previous = SelectObject(result.dc, HGDIOBJ(result.bitmap.0));
      if result.previous.0.is_null() || result.previous.0 as isize == -1 {
        result.previous = HGDIOBJ::default();
        return Err(EngineError::native(
          "fixture bitmap selection",
          windows::core::Error::from_thread(),
        ));
      }
      Ok(result)
    }
  }
  fn present(&mut self, destination: HDC, samples: u64, region: Region) -> Result<()> {
    let area = match region {
      Region::Clock => {
        draw_clock(self.dc, samples)?;
        RECT {
          left: 0,
          top: CLOCK_TOP,
          right: CODE_WIDTH,
          bottom: CLOCK_TOP + CELL,
        }
      },
      Region::Frame => {
        let frame = samples * FRAMES_PER_SECOND / RATE;
        draw_band(self.dc, self.width, self.height, frame)?;
        draw_code(self.dc, 0, frame, "fixture frame index")?;
        draw_clock(self.dc, samples)?;
        RECT {
          left: 0,
          top: 0,
          right: self.width,
          bottom: band_bottom(self.height),
        }
      },
      Region::Pulse => {
        draw_pulse(self.dc, self.width, self.height, samples)?;
        pulse_block(self.width, self.height)
      },
      Region::Full => {
        draw(self.dc, self.width, self.height, samples)?;
        RECT {
          left: 0,
          top: 0,
          right: self.width,
          bottom: self.height,
        }
      },
    };
    // SAFETY: the updated offscreen region is copied in one operation, keeping binary carry bits
    // together; smaller regions leave the unchanged background untouched.
    unsafe {
      BitBlt(
        destination,
        area.left,
        area.top,
        area.right - area.left,
        area.bottom - area.top,
        Some(self.dc),
        area.left,
        area.top,
        SRCCOPY,
      )
    }
    .map_err(|error| EngineError::native("fixture buffered presentation", error))
  }
}
impl Drop for PaintBuffer {
  fn drop(&mut self) {
    // SAFETY: the owned bitmap is deselected before deletion; all resources remain on this window's thread.
    unsafe {
      if !self.previous.0.is_null() {
        let restored = SelectObject(self.dc, self.previous);
        if restored.0.is_null() || restored.0 as isize == -1 {
          eprintln!("Fixture bitmap restore failed");
        }
      }
      if !self.bitmap.0.is_null() && !DeleteObject(HGDIOBJ(self.bitmap.0)).as_bool() {
        eprintln!("Fixture bitmap cleanup failed");
      }
      if !self.dc.0.is_null() && !DeleteDC(self.dc).as_bool() {
        eprintln!("Fixture paint DC cleanup failed");
      }
    }
  }
}
unsafe extern "system" fn procedure(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  match message {
    WM_NCCREATE => {
      // SAFETY: CreateWindowExW supplies the live CREATESTRUCTW and stable Box<PaintBuffer> pointer.
      unsafe {
        let creation = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, creation.lpCreateParams as isize);
      }
      LRESULT(1)
    },
    WM_NCDESTROY => {
      // SAFETY: clear the borrowed pointer before the owner releases its backing buffer.
      unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
      }
      LRESULT(0)
    },
    WM_PAINT => {
      let mut paint = PAINTSTRUCT::default();
      let mut bounds = RECT::default();
      // SAFETY: WM_PAINT owns this window's paint cycle and all outputs remain local.
      unsafe {
        let dc = BeginPaint(hwnd, &mut paint);
        let result = GetClientRect(hwnd, &mut bounds)
          .map_err(|error| EngineError::native("fixture bounds", error))
          .and_then(|_| {
            let buffer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PaintBuffer;
            if buffer.is_null() || dc.0.is_null() {
              return Err(EngineError::protocol("Fixture paint buffer unavailable"));
            }
            // The window owner holds this box until after DestroyWindow; paint is serialized on that thread.
            let region = region(&paint.rcPaint, bounds.right, bounds.bottom);
            (&mut *buffer).present(dc, POSITION.load(Ordering::Acquire), region)
          });
        if result.is_err() {
          PAINT_FAILED.store(true, Ordering::Release);
        }
        if !EndPaint(hwnd, &paint).as_bool() {
          PAINT_FAILED.store(true, Ordering::Release);
        }
      }
      LRESULT(0)
    },
    WM_ERASEBKGND => LRESULT(1),
    WM_CLOSE => {
      CLOSED.store(true, Ordering::Release);
      // SAFETY: closes this fixture's own window on its owner thread.
      if let Err(error) = unsafe { DestroyWindow(hwnd) } {
        eprintln!("Fixture close: {error}");
      }
      LRESULT(0)
    },
    WM_DESTROY => {
      CLOSED.store(true, Ordering::Release);
      LRESULT(0)
    },
    // SAFETY: forward untouched messages for this owned native window.
    _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
  }
}
struct DisplayWindow {
  hwnd: HWND,
  instance: HINSTANCE,
  dpi: DPI_AWARENESS_CONTEXT,
  buffer: Box<PaintBuffer>,
  registered: bool,
}
impl Drop for DisplayWindow {
  fn drop(&mut self) {
    // SAFETY: owner-thread cleanup of this process's single fixture window/class and prior DPI context.
    unsafe {
      if IsWindow(Some(self.hwnd)).as_bool()
        && let Err(error) = DestroyWindow(self.hwnd)
      {
        eprintln!("Fixture window cleanup: {error}");
      }
      if self.registered
        && let Err(error) =
          UnregisterClassW(w!("FlinchStandaloneVideoFixture"), Some(self.instance))
      {
        eprintln!("Fixture class cleanup: {error}");
      }
      if SetThreadDpiAwarenessContext(self.dpi).0.is_null() {
        eprintln!(
          "Fixture DPI restore: {}",
          windows::core::Error::from_thread()
        );
      }
    }
  }
}
/// Must run only in the lab's supervised, explicitly invoked fixture subprocess.
pub fn run(
  duration: Duration,
  title: &str,
  monitor: &str,
  heartbeat: impl FnMut() -> Result<()>,
) -> Result<FixtureReport> {
  if !(0.1..=MAX_SECONDS).contains(&duration.as_secs_f64()) {
    return Err(EngineError::protocol(
      "Fixture duration must be 0.1..=10830 seconds",
    ));
  }
  let _com = super::Apartment::new()?;
  let output = super::gpu::outputs()?
    .into_iter()
    .find(|output| {
      let info = &output.info;
      match super::runtime::monitor_geometry(monitor) {
        Some(geometry) => (info.x, info.y, info.width, info.height) == geometry,
        None => monitor == "primary" && info.x == 0 && info.y == 0,
      }
    })
    .ok_or_else(|| {
      EngineError::new(
        ErrorKind::Capability,
        "fixture monitor",
        format!("No attached display matches {monitor:?} (use primary or X,Y,WxH)"),
      )
    })?;
  let mut report = run_with_source(
    duration,
    title,
    heartbeat,
    AudioLoop::new()?,
    (output.info.x, output.info.y),
    (output.info.width as i32, output.info.height as i32),
    true,
  )?;
  report.display_refresh_hz = output.info.refresh_hz;
  report.composition_period_100ns = super::gpu::composition_period()?;
  Ok(report)
}
fn run_with_source(
  duration: Duration,
  title: &str,
  mut heartbeat: impl FnMut() -> Result<()>,
  mut audio: impl RenderAudio,
  origin: (i32, i32),
  extent: (i32, i32),
  visible: bool,
) -> Result<FixtureReport> {
  POSITION.store(0, Ordering::Release);
  CLOSED.store(false, Ordering::Release);
  PAINT_FAILED.store(false, Ordering::Release);
  let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
  // SAFETY: class/procedure belong to this process; this command is an explicit visible desktop workload.
  let window = unsafe {
    let module =
      GetModuleHandleW(None).map_err(|error| EngineError::native("fixture module", error))?;
    let instance = HINSTANCE(module.0);
    let buffer = Box::new(PaintBuffer::new(extent.0, extent.1)?);
    let dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    if dpi.0.is_null() {
      return Err(EngineError::native(
        "fixture DPI",
        windows::core::Error::from_thread(),
      ));
    }
    let class = WNDCLASSW {
      lpfnWndProc: Some(procedure),
      hInstance: instance,
      lpszClassName: w!("FlinchStandaloneVideoFixture"),
      ..Default::default()
    };
    let mut owner = DisplayWindow {
      hwnd: HWND::default(),
      instance,
      dpi,
      buffer,
      registered: false,
    };
    if RegisterClassW(&class) == 0 {
      return Err(EngineError::native(
        "fixture class",
        windows::core::Error::from_thread(),
      ));
    }
    owner.registered = true;
    owner.hwnd = CreateWindowExW(
      WS_EX_TOPMOST | WS_EX_NOACTIVATE,
      class.lpszClassName,
      PCWSTR(title.as_ptr()),
      WS_POPUP
        | if visible {
          WS_VISIBLE
        } else {
          WINDOW_STYLE::default()
        },
      origin.0,
      origin.1,
      extent.0,
      extent.1,
      None,
      None,
      Some(instance),
      Some((&mut *owner.buffer as *mut PaintBuffer).cast()),
    )
    .map_err(|error| EngineError::native("fixture window", error))?;
    owner
  };
  audio.start()?;
  let started = Instant::now();
  let mut last_heartbeat = Instant::now();
  let mut previous = 0;
  let mut clock_progress = Instant::now();
  let mut last_frame = None;
  let mut last_marker = None;
  let mut markers = Vec::with_capacity(duration.as_secs() as usize + 1);
  let mut lateness = Histogram::default();
  let mut painted = 0;
  let mut last_strip: Option<u64> = None;
  let mut strip_paints = 0u64;
  let mut strip_gap = 0u64;
  let mut clock = RenderClock::default();
  let mut windows = PulseWindows::default();
  let mut last_pulse = None;
  let timer = super::timer::Waiter::new()?;
  while started.elapsed() < duration {
    let mut message = MSG::default();
    // SAFETY: dispatch only this owner's messages; no global input hook or synthesized input.
    unsafe {
      for _ in 0..64 {
        if !PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
          break;
        }
        let _translated = TranslateMessage(&message);
        DispatchMessageW(&message);
      }
    }
    if CLOSED.load(Ordering::Acquire) {
      return Err(EngineError::new(
        ErrorKind::Cancelled,
        "fixture",
        "Fixture window closed before the requested duration",
      ));
    }
    if PAINT_FAILED.load(Ordering::Acquire) {
      return Err(EngineError::new(
        ErrorKind::Native,
        "fixture paint",
        "A native fixture paint operation failed",
      ));
    }
    let real = audio.position()?;
    if real < previous {
      return Err(EngineError::protocol("Fixture output clock reversed"));
    }
    if real > previous {
      clock_progress = Instant::now();
    }
    if clock_progress.elapsed() > Duration::from_secs(2) {
      return Err(EngineError::new(
        ErrorKind::Timeout,
        "fixture output clock",
        "The render clock stopped advancing",
      ));
    }
    previous = real;
    let samples = clock.observe(real, super::qpc()?);
    if let Some((shown, lead, lag)) = clock.correction.take() {
      for value in [shown, real] {
        if let Some(window) = windows.at(value) {
          window.maximum_lead_samples = window.maximum_lead_samples.max(lead);
          window.maximum_lag_samples = window.maximum_lag_samples.max(lag);
        }
      }
    }
    let frame = samples * FRAMES_PER_SECOND / RATE;
    let full = last_frame != Some(frame);
    if full || last_strip != Some(samples) {
      POSITION.store(samples, Ordering::Release);
      // A new frame repaints the strip too. On a pulse edge the block follows in its own paint,
      // after the frame index, so no capture shows the pulse beside the previous index.
      let pulse = PULSE.contains(&(samples % RATE));
      let mut updates = vec![if full {
        RECT {
          left: 0,
          top: 0,
          right: extent.0,
          bottom: band_bottom(extent.1),
        }
      } else {
        RECT {
          left: 0,
          top: CLOCK_TOP,
          right: CODE_WIDTH,
          bottom: CLOCK_TOP + CELL,
        }
      }];
      if full && last_pulse != Some(pulse) {
        updates.push(pulse_block(extent.0, extent.1));
        last_pulse = Some(pulse);
      }
      for update in &updates {
        // SAFETY: invalidate/update only this explicit workload's window, without moving focus or global input.
        unsafe {
          if !InvalidateRect(Some(window.hwnd), Some(update as *const RECT), false).as_bool()
            || !UpdateWindow(window.hwnd).as_bool()
          {
            return Err(EngineError::native(
              "fixture repaint",
              windows::core::Error::from_thread(),
            ));
          }
        }
      }
      if let Some(last) = last_strip {
        strip_gap = strip_gap.max(samples - last);
        // The gap belongs to the value it follows: that value may be captured until now.
        if let Some(window) = windows.at(last) {
          window.maximum_gap_samples = window.maximum_gap_samples.max(samples - last);
        }
      }
      last_strip = Some(samples);
      strip_paints += 1;
    }
    if full {
      painted += 1;
      last_frame = Some(frame);
      if PULSE.contains(&(samples % RATE)) && last_marker != Some(samples / RATE) {
        lateness.record((samples % RATE - PULSE.start) * 1_000_000 / RATE);
        markers.push(MarkerObservation {
          second: samples / RATE,
          render_sample_position: samples,
          paint_submission_qpc_100ns: super::qpc()?,
        });
        windows.open(samples / RATE, samples);
        last_marker = Some(samples / RATE);
      }
    }
    if last_heartbeat.elapsed() >= Duration::from_millis(500) {
      heartbeat()?;
      last_heartbeat = Instant::now();
    }
    // Around a pulse the strip repaints as fast as the timer allows; elsewhere every 1 ms.
    timer.wait(if pulse_window_second(samples).is_some() {
      250
    } else {
      1000
    })?;
  }
  audio.close()?;
  Ok(FixtureReport { clock: "independent_winmm_render_sample_position".into(), presentation_limit: "Paint submission observed; compositor/scanout latency is not independently measured. No recorder timestamp or guessed A/V offset is used.".into(), elapsed_seconds: started.elapsed().as_secs_f64(), rendered_samples: previous, frames_painted: painted, marker_submission_lateness: lateness.summary(), markers, clock_strip: Some(ClockStrip { unit: ClockUnit::RenderSamples48k, paints: strip_paints, maximum_gap_samples: strip_gap, maximum_lead_samples: clock.lead, maximum_lag_samples: clock.lag, pulse_windows: windows.windows }), display_refresh_hz: None, composition_period_100ns: None })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  /// QPC time (100 ns) of `ms` milliseconds.
  fn at(ms: f64) -> u64 {
    (ms * 10_000.0).round() as u64
  }
  #[test]
  fn the_render_clock_interpolates_between_positions_and_bounds_its_error() {
    let mut clock = RenderClock::default();
    // WinMM reports 48-sample (1 ms) steps; between them the clock advances with QPC.
    assert_eq!(clock.observe(0, at(0.0)), 0);
    assert_eq!(clock.observe(0, at(0.5)), 24);
    // An update exactly where the extrapolation is: nothing ahead or behind.
    assert_eq!(clock.observe(48, at(1.0)), 48);
    assert_eq!(clock.observe(96, at(2.0)), 96);
    assert_eq!((clock.lead, clock.lag), (0, 0));
    // A stalled position stops extrapolating after 2 ms, so the lead stays within it.
    assert_eq!(clock.observe(96, at(10.0)), 96 + MAX_EXTRAPOLATION);
    assert_eq!(clock.observe(110, at(10.5)), 96 + MAX_EXTRAPOLATION);
    assert_eq!((clock.lead, clock.lag), (96 + MAX_EXTRAPOLATION - 110, 0));
    // The clock never goes below what it showed while the real position catches up.
    assert_eq!(clock.observe(150, at(11.0)), 96 + MAX_EXTRAPOLATION);
    assert_eq!(clock.lead, 82);
    // An update observed late arrives above the extrapolation: the lag is recorded and the
    // clock jumps to it.
    assert_eq!(clock.observe(400, at(12.0)), 400);
    assert_eq!(clock.lag, 400 - (150 + 48));
    assert!(clock.lead <= MAX_EXTRAPOLATION);
  }
  #[test]
  fn pulse_windows_cover_the_strip_values_a_pulse_capture_can_show() {
    let frame = RATE / FRAMES_PER_SECOND;
    assert_eq!(pulse_window_second(PULSE.start - frame), Some(0));
    assert_eq!(pulse_window_second(PULSE.start - frame - 1), None);
    assert_eq!(
      pulse_window_second(RATE * 7 + PULSE.end + 3 * frame - 1),
      Some(7)
    );
    assert_eq!(pulse_window_second(RATE * 7 + PULSE.end + 3 * frame), None);
    // A window opens at its pulse paint: earlier values, other seconds and values before any
    // pulse paint are outside every window.
    let mut windows = PulseWindows::default();
    assert!(windows.at(PULSE.start).is_none());
    let paint = PULSE.start + 120;
    windows.open(0, paint);
    for samples in [
      PULSE.start,
      paint - 1,
      paint,
      paint + 10,
      RATE + PULSE.start,
    ] {
      if let Some(window) = windows.at(samples) {
        window.maximum_gap_samples += 1;
      }
    }
    windows.open(1, RATE + paint);
    for samples in [RATE + paint - 1, RATE + paint] {
      if let Some(window) = windows.at(samples) {
        window.maximum_gap_samples += 1;
      }
    }
    assert_eq!(
      windows
        .windows
        .iter()
        .map(|w| (w.second, w.maximum_gap_samples))
        .collect::<Vec<_>>(),
      [(0, 2), (1, 1)]
    );
  }
  #[test]
  fn only_the_loops_own_updates_skip_a_full_repaint() {
    let (width, height) = (2560, 1440);
    let rect = |left, top, right, bottom| RECT {
      left,
      top,
      right,
      bottom,
    };
    let strip = rect(0, CLOCK_TOP, CODE_WIDTH, CLOCK_TOP + CELL);
    assert_eq!(region(&strip, width, height), Region::Clock);
    let frame = rect(0, 0, width, band_bottom(height));
    assert_eq!(region(&frame, width, height), Region::Frame);
    // The band stays clear of the centre pulse sensor, and the block holds it.
    assert!(band_bottom(height) < height / 2);
    for (w, h) in [(width, height), (512, 64), (1179, 2556)] {
      let block = pulse_block(w, h);
      assert!(block.top >= band_bottom(h), "{w}x{h}");
      assert!(block.left <= w / 2 && w / 2 < block.right, "{w}x{h}");
      assert!(block.top <= h / 2 && h / 2 < block.bottom, "{w}x{h}");
      assert_eq!(region(&block, w, h), Region::Pulse, "{w}x{h}");
    }
    // A system repaint (covered or first shown) reaches outside the loop's regions: everything
    // is redrawn.
    for update in [
      rect(0, 0, width, height),
      rect(100, 500, 300, 700),
      rect(1100, 600, 1400, 900),
      rect(0, 0, width, band_bottom(height) + 1),
      rect(-1, CLOCK_TOP, CODE_WIDTH, CLOCK_TOP + CELL),
    ] {
      assert_eq!(region(&update, width, height), Region::Full, "{update:?}");
    }
  }
  #[test]
  fn each_render_clock_update_reports_its_own_correction() {
    let mut clock = RenderClock::default();
    clock.observe(0, at(0.0));
    assert_eq!(clock.correction.take(), None);
    clock.observe(48, at(1.0));
    assert_eq!(clock.correction.take(), Some((48, 0, 0)));
    clock.observe(48, at(10.0));
    assert_eq!(clock.correction.take(), None);
    clock.observe(110, at(10.5));
    assert_eq!(
      clock.correction.take(),
      Some((48 + MAX_EXTRAPOLATION, 34, 0))
    );
  }
  struct SilentClock {
    start: Instant,
    reverse: bool,
  }
  impl RenderAudio for SilentClock {
    fn start(&mut self) -> Result<()> {
      self.start = Instant::now();
      Ok(())
    }
    fn position(&self) -> Result<u64> {
      let samples = (self.start.elapsed().as_secs_f64() * RATE as f64) as u64;
      Ok(if self.reverse && samples > 1000 {
        0
      } else {
        samples
      })
    }
    fn close(&mut self) -> Result<()> {
      Ok(())
    }
  }
  #[test]
  fn hidden_fixture_preserves_marker_progress_and_cleans_up_after_clock_failure() {
    // A hidden HWND with a clock double: no display capture, sound, focus change or input registration.
    let mut heartbeats = 0;
    let report = run_with_source(
      Duration::from_millis(620),
      "",
      || {
        heartbeats += 1;
        Ok(())
      },
      SilentClock {
        start: Instant::now(),
        reverse: false,
      },
      (0, 0),
      (512, 64),
      false,
    )
    .expect("hidden workload");
    assert!(report.frames_painted >= 20);
    assert_eq!(report.markers.len(), 1);
    let strip = report.clock_strip.expect("render-clock strip");
    assert_eq!(strip.unit, ClockUnit::RenderSamples48k);
    assert!(
      strip.paints > report.frames_painted,
      "the strip is refreshed between full frames"
    );
    assert!(strip.maximum_gap_samples > 0 && strip.maximum_gap_samples < RATE / 10);
    assert!(heartbeats > 0);
    let error = run_with_source(
      Duration::from_millis(100),
      "",
      || Ok(()),
      SilentClock {
        start: Instant::now(),
        reverse: true,
      },
      (0, 0),
      (512, 64),
      false,
    )
    .err()
    .expect("reversal rejected");
    assert!(error.reason.contains("reversed"));
    run_with_source(
      Duration::from_millis(100),
      "",
      || Ok(()),
      SilentClock {
        start: Instant::now(),
        reverse: false,
      },
      (0, 0),
      (512, 64),
      false,
    )
    .expect("window and class can reopen after failure");
  }
  fn read_code(dc: HDC, top: i32) -> u64 {
    let mut decoded = 0;
    for bit in 0..32 {
      // SAFETY: reads generated pixels in an owned offscreen bitmap, never the desktop.
      if unsafe { GetPixel(dc, bit * CELL + 8, top + 8) }.0 & 255 > 128 {
        decoded |= 1u64 << bit;
      }
    }
    decoded
  }
  #[test]
  fn buffered_frame_copies_carry_bits_and_marker_edges_as_one_image() {
    let mut source = PaintBuffer::new(512, 64).expect("source buffer");
    let destination = PaintBuffer::new(512, 64).expect("destination buffer");
    for (frame, pulse) in [
      (29u64, false),
      (30, true),
      (31, true),
      (32, true),
      (36, false),
      (63, false),
      (64, false),
    ] {
      source
        .present(destination.dc, frame * 800, Region::Full)
        .expect("buffered paint");
      let mut decoded = 0;
      for bit in 0..32 {
        // SAFETY: reads generated pixels in an owned offscreen bitmap, never the desktop.
        let color = unsafe { GetPixel(destination.dc, bit * 16 + 8, 8) };
        if color.0 & 255 > 128 {
          decoded |= 1u64 << bit;
        }
      }
      assert_eq!(decoded, frame);
      // SAFETY: the generated center sensor has a known pixel position within this bitmap.
      assert_eq!(
        unsafe { GetPixel(destination.dc, 256, 32) }.0,
        if pulse { 0x0000ff00 } else { 0 }
      );
    }
    // A strip-only update between frames advances the clock without repainting the index or
    // pulse: frame 29 stays visible even when the clock has reached the pulse onset sample.
    source
      .present(destination.dc, 29 * 800, Region::Full)
      .expect("full frame 29");
    for samples in [29 * 800 + 48, PULSE.start, PULSE.start + 37] {
      source
        .present(destination.dc, samples, Region::Clock)
        .expect("strip only");
      assert_eq!(read_code(destination.dc, CLOCK_TOP), samples);
      assert_eq!(read_code(destination.dc, 0), 29, "index untouched");
      // SAFETY: reads the generated center sensor in an owned offscreen bitmap.
      assert_eq!(
        unsafe { GetPixel(destination.dc, 256, 40) }.0,
        0,
        "pulse untouched"
      );
    }
    source
      .present(destination.dc, PULSE.start + 40, Region::Full)
      .expect("full pulse frame");
    assert_eq!(read_code(destination.dc, 0), 30);
    assert_eq!(read_code(destination.dc, CLOCK_TOP), PULSE.start + 40);
    // A frame update inside the pulse advances index and clock and keeps the green background.
    source
      .present(destination.dc, 31 * 800 + 5, Region::Frame)
      .expect("frame update");
    assert_eq!(read_code(destination.dc, 0), 31);
    assert_eq!(read_code(destination.dc, CLOCK_TOP), 31 * 800 + 5);
    // SAFETY: reads the generated center sensor in an owned offscreen bitmap.
    assert_eq!(unsafe { GetPixel(destination.dc, 256, 32) }.0, 0x0000ff00);
    // At the pulse end the frame update moves the index first and leaves the block alone; the
    // block's own paint then clears it, so no image pairs the pulse with an earlier index.
    source
      .present(destination.dc, PULSE.end, Region::Frame)
      .expect("frame at pulse end");
    assert_eq!(read_code(destination.dc, 0), 36);
    // SAFETY: reads the generated center sensor in an owned offscreen bitmap.
    assert_eq!(unsafe { GetPixel(destination.dc, 256, 32) }.0, 0x0000ff00);
    source
      .present(destination.dc, PULSE.end, Region::Pulse)
      .expect("pulse end");
    // SAFETY: reads the generated center sensor in an owned offscreen bitmap.
    assert_eq!(unsafe { GetPixel(destination.dc, 256, 32) }.0, 0);
    // The rest of the window is never green: only the centre block flashes.
    source
      .present(destination.dc, PULSE.start, Region::Full)
      .expect("full pulse");
    // SAFETY: reads generated pixels in an owned offscreen bitmap.
    assert_eq!(unsafe { GetPixel(destination.dc, 8, 60) }.0, 0);
    assert!(draw_clock(destination.dc, u64::from(u32::MAX) + 1).is_err());
    assert!(mm(5, "fixture test").is_err());
  }
}
