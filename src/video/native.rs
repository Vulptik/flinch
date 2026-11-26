//! Bridge from the app's recording plans to the native video engine, which records every video and
//! GIF.

use std::ffi::OsStr;
use std::path::Path;

use flinch_video_engine::gif::{Dither, GifOptions};
use flinch_video_engine::types::{CaptureKind, EncoderKind, Quality, Region, SessionSpec};

use super::capture::CaptureConfig;
use super::config::VideoConfig;
use crate::monitors::MonitorInfo;
use crate::screenshot::CaptureBackendKind;

/// First argument that turns this executable into a native engine worker instead of the app.
pub const WORKER_SWITCH: &str = "--video-engine-worker";
/// Native capture backend: `auto` (the default: DuplicateOutput1, then WGC), or an exact `dxgi`,
/// `wgc` or `dxgi-legacy` backend for acceptance comparisons.
pub const CAPTURE_VARIABLE: &str = "FLINCH_VIDEO_CAPTURE";

/// Automatic selection is the default: the engine tries DuplicateOutput1 (fastest, GPU cursor)
/// and then WGC once during Prepare and reports every rejection. An explicit backend is exact.
fn capture_from(
  value: Option<&OsStr>,
  forced: Option<CaptureBackendKind>,
) -> Result<CaptureKind, String> {
  match forced {
    Some(CaptureBackendKind::Dxgi) => return Ok(CaptureKind::Dxgi),
    Some(CaptureBackendKind::Wgc) => return Ok(CaptureKind::Wgc),
    Some(CaptureBackendKind::Gdi) => {
      return Err("The native video engine has no GDI capture backend".into());
    },
    None => {},
  }
  match value.map(OsStr::to_str) {
    None => Ok(CaptureKind::Auto),
    Some(Some(name)) if name.is_empty() || name.eq_ignore_ascii_case("auto") => {
      Ok(CaptureKind::Auto)
    },
    Some(Some(name)) if name.eq_ignore_ascii_case("wgc") => Ok(CaptureKind::Wgc),
    Some(Some(name)) if name.eq_ignore_ascii_case("dxgi") => Ok(CaptureKind::Dxgi),
    Some(Some(name)) if name.eq_ignore_ascii_case("dxgi-legacy") => Ok(CaptureKind::DxgiLegacy),
    _ => Err(format!(
      "{CAPTURE_VARIABLE} must be \"auto\", \"dxgi\", \"wgc\" or \"dxgi-legacy\", not {value:?}"
    )),
  }
}

/// Map a planned recording onto one display of the native engine.
///
/// The capture rectangle is in physical virtual-desktop pixels. Sizes round down to even, as
/// H.264's 4:2:0 chroma requires. On one display, a rectangle covering it records it uncropped. A
/// rectangle crossing displays records every display it touches as one picture: the display
/// holding its top-left corner first, the others spanned, and the region relative to their
/// bounding box (anything no display covers is black).
pub fn session_spec(
  video: &VideoConfig,
  capture: &CaptureConfig,
  monitors: &[MonitorInfo],
  output: &Path,
) -> Result<SessionSpec, String> {
  session_spec_with(
    video,
    capture,
    monitors,
    output,
    std::env::var_os(CAPTURE_VARIABLE).as_deref(),
  )
}

fn session_spec_with(
  video: &VideoConfig,
  capture: &CaptureConfig,
  monitors: &[MonitorInfo],
  output: &Path,
  capture_variable: Option<&OsStr>,
) -> Result<SessionSpec, String> {
  if !flinch_video_engine::types::SUPPORTED_FPS.contains(&capture.fps) {
    return Err(format!(
      "The native video engine records at {:?} FPS, not {}",
      flinch_video_engine::types::SUPPORTED_FPS,
      capture.fps
    ));
  }
  let width = capture.width & !1;
  let height = capture.height & !1;
  if width < Region::MIN_SIZE || height < Region::MIN_SIZE {
    return Err(format!(
      "Native recording needs an area of at least {0}x{0} pixels, not {1}x{2}",
      Region::MIN_SIZE,
      capture.width,
      capture.height
    ));
  }
  let (left, top) = (i64::from(capture.x), i64::from(capture.y));
  let (right, bottom) = (left + i64::from(width), top + i64::from(height));
  let edges = |m: &MonitorInfo| {
    let (x, y) = (i64::from(m.x), i64::from(m.y));
    (x, y, x + i64::from(m.width), y + i64::from(m.height))
  };
  let mut touched: Vec<&MonitorInfo> = monitors
    .iter()
    .filter(|m| {
      let (x, y, r, b) = edges(m);
      left < r && x < right && top < b && y < bottom
    })
    .collect();
  // The display holding the top-left corner leads; the rest keep a stable left-to-right order.
  touched.sort_by_key(|m| {
    let (x, y, r, b) = edges(m);
    (!(left >= x && left < r && top >= y && top < b), m.x, m.y)
  });
  let Some(monitor) = touched.first().copied() else {
    return Err(format!(
      "Recording area {width}x{height} at ({}, {}) does not lie on any display",
      capture.x, capture.y
    ));
  };
  let (canvas_x, canvas_y, canvas_right, canvas_bottom) = touched
    .iter()
    .map(|m| edges(m))
    .fold(edges(monitor), |(l, t, r, b), (x, y, mr, mb)| {
      (l.min(x), t.min(y), r.max(mr), b.max(mb))
    });
  if left < canvas_x || top < canvas_y || right > canvas_right || bottom > canvas_bottom {
    return Err(format!(
      "Recording area {width}x{height} at ({}, {}) leaves the displays it touches",
      capture.x, capture.y
    ));
  }
  let offset_x = u32::try_from(left - canvas_x).map_err(|e| e.to_string())?;
  let offset_y = u32::try_from(top - canvas_y).map_err(|e| e.to_string())?;
  let canvas = (canvas_right - canvas_x, canvas_bottom - canvas_y);
  let region = touched.len() > 1
    || (offset_x, offset_y, i64::from(width), i64::from(height)) != (0, 0, canvas.0, canvas.1);
  let identity = |m: &MonitorInfo| format!("{},{},{}x{}", m.x, m.y, m.width, m.height);
  let qp = video.qp();
  Ok(SessionSpec {
    monitor: identity(monitor),
    fps: capture.fps,
    capture: capture_from(capture_variable, capture.forced_backend)?,
    encoder: if video.hardware_encoding {
      EncoderKind::Auto
    } else {
      EncoderKind::Software
    },
    quality: Quality::Cqp { i: qp, p: qp },
    cursor: capture.show_cursor,
    clicks: capture.show_clicks,
    audio: video.audio.enabled,
    audio_device: None,
    microphone: video.microphone.is_some(),
    microphone_device: video
      .microphone
      .as_ref()
      .and_then(|microphone| microphone.device.clone()),
    output: output.to_path_buf(),
    trace_frames: false,
    region: region.then_some(Region {
      x: offset_x,
      y: offset_y,
      width,
      height,
    }),
    span: touched[1..].iter().map(|m| identity(m)).collect(),
  })
}

/// The palette settings every app GIF uses: one palette from the whole recording (the engine's
/// only mode) with Sierra-2-4A error diffusion.
pub fn gif_options() -> GifOptions {
  GifOptions {
    dither: Dither::Sierra24a,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::video::capture::RecordingMode;
  use crate::video::config::{AudioConfig, VideoQuality};

  fn monitor(x: i32, y: i32, width: u32, height: u32) -> MonitorInfo {
    MonitorInfo {
      id: format!("{x},{y},{width}x{height}"),
      name: "fixture".into(),
      x,
      y,
      width,
      height,
      is_primary: x == 0 && y == 0,
      dpi_scale: 1.0,
      logical_x: x,
      logical_y: y,
      logical_width: width,
      logical_height: height,
    }
  }
  fn displays() -> Vec<MonitorInfo> {
    vec![monitor(0, 0, 2560, 1440), monitor(-1920, 200, 1920, 1080)]
  }
  fn area(x: i32, y: i32, width: u32, height: u32) -> CaptureConfig {
    CaptureConfig {
      x,
      y,
      width,
      height,
      mode: RecordingMode::Area,
      show_cursor: true,
      ..CaptureConfig::fullscreen(width, height, 30)
    }
  }
  fn map(video: &VideoConfig, capture: &CaptureConfig) -> Result<SessionSpec, String> {
    session_spec_with(video, capture, &displays(), Path::new("out.mp4"), None)
  }

  #[test]
  fn app_gifs_use_sierra_error_diffusion() {
    assert_eq!(
      gif_options(),
      GifOptions {
        dither: Dither::Sierra24a
      }
    );
  }

  #[test]
  fn capture_selection_defaults_to_automatic_and_honours_explicit_backends() {
    assert_eq!(capture_from(None, None), Ok(CaptureKind::Auto));
    assert_eq!(
      capture_from(Some(OsStr::new("")), None),
      Ok(CaptureKind::Auto)
    );
    assert_eq!(
      capture_from(Some(OsStr::new("Auto")), None),
      Ok(CaptureKind::Auto)
    );
    assert_eq!(
      capture_from(Some(OsStr::new("wgc")), None),
      Ok(CaptureKind::Wgc)
    );
    assert_eq!(
      capture_from(Some(OsStr::new("DXGI")), None),
      Ok(CaptureKind::Dxgi)
    );
    assert_eq!(
      capture_from(Some(OsStr::new("dxgi-legacy")), None),
      Ok(CaptureKind::DxgiLegacy)
    );
    assert!(capture_from(Some(OsStr::new("gdi")), None).is_err());
    // An explicit plan backend wins over the acceptance variable.
    assert_eq!(
      capture_from(Some(OsStr::new("wgc")), Some(CaptureBackendKind::Dxgi)),
      Ok(CaptureKind::Dxgi)
    );
    assert!(capture_from(None, Some(CaptureBackendKind::Gdi)).is_err());
    assert_eq!(
      capture_from(None, Some(CaptureBackendKind::Wgc)),
      Ok(CaptureKind::Wgc)
    );
  }

  #[test]
  fn whole_displays_record_uncropped_and_areas_become_display_relative_regions() {
    let video = VideoConfig::default();
    let full = map(&video, &CaptureConfig::fullscreen(2560, 1440, 60)).expect("primary");
    assert_eq!(
      (full.monitor.as_str(), full.region, full.fps),
      ("0,0,2560x1440", None, 60)
    );
    let left = map(&video, &area(-1920, 200, 1920, 1080)).expect("left display");
    assert_eq!(
      (left.monitor.as_str(), left.region),
      ("-1920,200,1920x1080", None)
    );

    let cropped = map(&video, &area(-1000, 300, 641, 361)).expect("area");
    assert_eq!(cropped.monitor, "-1920,200,1920x1080");
    assert_eq!(
      cropped.region,
      Some(Region {
        x: 920,
        y: 100,
        width: 640,
        height: 360
      })
    );
    assert!(cropped.cursor);
    assert_eq!(cropped.output, Path::new("out.mp4"));
  }

  #[test]
  fn quality_hardware_and_audio_choices_map_onto_the_engine() {
    for (quality, qp) in [
      (VideoQuality::Low, 30),
      (VideoQuality::Medium, 26),
      (VideoQuality::High, 23),
    ] {
      let video = VideoConfig {
        quality,
        hardware_encoding: false,
        audio: AudioConfig { enabled: true },
        ..VideoConfig::default()
      };
      let spec = map(&video, &area(0, 0, 640, 360)).expect("spec");
      assert_eq!(spec.quality, Quality::Cqp { i: qp, p: qp });
      assert_eq!(spec.encoder, EncoderKind::Software);
      assert!(spec.audio);
    }
    let spec = map(&VideoConfig::default(), &area(0, 0, 640, 360)).expect("spec");
    assert_eq!((spec.encoder, spec.audio), (EncoderKind::Auto, false));
    assert_eq!(spec.capture, CaptureKind::Auto);
    // A GIF's intermediate keeps its own QP, whatever the video quality setting.
    let gif = crate::gif::session::build_gif_video_config(Path::new("."), 15);
    let spec = map(&gif, &area(0, 0, 640, 360)).expect("GIF intermediate");
    let qp = crate::video::config::GIF_INTERMEDIATE_QP;
    assert_eq!(spec.quality, Quality::Cqp { i: qp, p: qp });
    assert_eq!(spec.encoder, EncoderKind::Software);
  }

  #[test]
  fn areas_crossing_displays_record_every_display_they_touch_as_one_picture() {
    let video = VideoConfig::default();
    // Top-left on the left display (-1920,200 1920x1080), extending onto the primary.
    let spec = map(&video, &area(-100, 300, 200, 200)).expect("spanning area");
    assert_eq!(spec.monitor, "-1920,200,1920x1080");
    assert_eq!(spec.span, ["0,0,2560x1440"]);
    // Relative to the bounding box (-1920,0)..(2560,1440).
    assert_eq!(
      spec.region,
      Some(Region {
        x: 1820,
        y: 300,
        width: 200,
        height: 200
      })
    );
    // The primary leads when it holds the top-left corner.
    let spec = map(&video, &area(-50, 100, 64, 400)).expect("from the primary edge");
    assert_eq!(
      spec.monitor, "-1920,200,1920x1080",
      "(-50,100) is above the left display"
    );
    assert_eq!(spec.span, ["0,0,2560x1440"]);
    let spec = map(&video, &area(10, 300, 64, 64)).expect("single display");
    assert_eq!(
      (spec.monitor.as_str(), spec.span.len()),
      ("0,0,2560x1440", 0)
    );
    // The whole desktop is one uncropped-size picture with a region over the bounding box.
    let everything = map(&video, &area(-1920, 0, 4480, 1440)).expect("everything");
    assert_eq!(everything.span.len(), 1);
    assert_eq!(
      everything.region,
      Some(Region {
        x: 0,
        y: 0,
        width: 4480,
        height: 1440
      })
    );
  }

  #[test]
  fn unsupported_plans_fail_explicitly_instead_of_recording_something_else() {
    let video = VideoConfig::default();
    let odd_rate = CaptureConfig::fullscreen(2560, 1440, 24);
    assert!(map(&video, &odd_rate).expect_err("24 FPS").contains("FPS"));
    let floor = format!("at least {0}x{0}", Region::MIN_SIZE);
    assert!(
      map(&video, &area(10, 10, 15, 64))
        .expect_err("narrow")
        .contains(&floor)
    );
    // Regression: 16 to 33 px areas passed here, then failed in every encoder.
    assert!(
      map(&video, &area(10, 10, 32, 64))
        .expect_err("narrow")
        .contains(&floor)
    );
    assert!(
      map(&video, &area(10, 10, 64, 33))
        .expect_err("short")
        .contains(&floor)
    );
    map(&video, &area(10, 10, 34, 34)).expect("smallest recordable area");
    // Leaving the desktop is refused, never clipped.
    for (x, y, w, h) in [
      (2500, 0, 100, 100),
      (0, 1400, 64, 64),
      (-2000, 300, 200, 200),
    ] {
      assert!(
        map(&video, &area(x, y, w, h))
          .expect_err("off desktop")
          .contains("leaves the displays"),
        "{x},{y}"
      );
    }
    assert!(
      map(&video, &area(5000, 5000, 64, 64))
        .expect_err("nowhere")
        .contains("any display")
    );
    let mut gdi = area(0, 0, 640, 360);
    gdi.forced_backend = Some(CaptureBackendKind::Gdi);
    assert!(map(&video, &gdi).expect_err("gdi").contains("GDI"));
  }
}
