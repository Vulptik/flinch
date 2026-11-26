//! Explicit, bounded offscreen replay of the production input handlers and renderer.
//! No desktop capture, injected input, visible window, recorder worker or preference writes.
use super::layout::*;
use super::*;
use crate::overlay::{
  gdi::{bitmap::ScreenshotBitmap, layered::LayeredBitmap},
  recording_shell::RecordingOverlayShell,
  recording_toolbar::{self, HoverTarget, RecordingPhase, TimerTextStyle},
  selection_core::{self, SelectionDrawColors, SharedSelectionOverlayState},
};
use crate::screenshot::annotations::Label;
use image::ImageEncoder;
use std::{
  fs,
  path::PathBuf,
  sync::Arc,
  time::{Duration, Instant},
};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{HDC, HFONT};
use windows::{
  Win32::{
    Graphics::Gdi::{DEFAULT_GUI_FONT, GdiFlush, GetStockObject},
    UI::WindowsAndMessaging::{
      CreateWindowExW, DestroyWindow, SetWindowTextW, WINDOW_EX_STYLE, WS_POPUP,
    },
  },
  core::{PCWSTR, w},
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
  kind: String,
  frames: usize,
  #[serde(default)]
  target: Option<String>,
  #[serde(default)]
  point: Option<[f64; 2]>,
  #[serde(default)]
  text: Option<String>,
  #[serde(default)]
  tag: String,
  #[serde(default)]
  bounds: Option<[f64; 4]>,
  /// Lateral offsets for the two Bezier controls, in capture pixels.
  #[serde(default)]
  curve: Option<[f64; 2]>,
  /// CSS-style cubic timing curve; absent retains the original replay easing.
  #[serde(default)]
  timing: Option<[f64; 4]>,
  /// Required on export steps: the drawings and cursors the replay must have produced.
  /// A tool name repeated N times requires N drawings with that tool.
  #[serde(default)]
  expect: Option<Vec<String>>,
}

/// Checks an export against its step's expectations, so a broken plan cannot pass quietly.
fn check_export(editor: &AnnotationEditor, steps: &[Step], expect: &[String]) {
  let count = |tool: Tool| editor.annotations.iter().filter(|a| a.tool == tool).count();
  for check in expect {
    let wanted = expect.iter().filter(|c| *c == check).count();
    match check.as_str() {
      "bent-arrow" => assert!(
        editor
          .annotations
          .iter()
          .any(|a| a.tool == Tool::Arrow && a.points.len() == 3),
        "the replay must actually bend an arrow"
      ),
      "text" => {
        let typed = steps
          .iter()
          .rev()
          .find_map(|s| (s.kind == "type").then_some(s.text.as_deref()).flatten())
          .expect("a text expectation needs typed text");
        assert!(
          editor
            .annotations
            .iter()
            .any(|a| matches!(&a.label, Some(Label::Text(t)) if t.content == typed)),
          "native text must be committed before export"
        );
      },
      // Checked once the whole replay has run; see `check_cursors`.
      "cursors" => {},
      name => {
        let tool = match name {
          "pen" => Tool::Pen,
          "highlighter" => Tool::Highlighter,
          "arrow" => Tool::Arrow,
          "rectangle" => Tool::Rectangle,
          "ellipse" => Tool::Ellipse,
          "marker" => Tool::Marker,
          "blur" => Tool::Blur,
          _ => panic!("unknown replay expectation {name}"),
        };
        assert!(
          count(tool) >= wanted,
          "the replay needs {wanted} {name} drawing(s)"
        );
      },
    }
  }
}

/// The production toast for a finished capture, saved as a premultiplied layer an editor can
/// slide in over footage, exactly as the notification window presents it.
fn write_toast(
  output: &std::path::Path,
  title: &str,
  body: &str,
  thumbnail: crate::overlay::notification::ThumbnailImage,
) {
  let request = crate::overlay::notification::NotificationRequest {
    title: title.into(),
    body: body.into(),
    thumbnail: Some(thumbnail),
    click_action: None,
    play_sound: false,
    critical: false,
    completed: None,
    processing: None,
  };
  let mut renderer = crate::overlay::chrome::Renderer::default();
  let (mut bitmap, height) =
    crate::overlay::notification::render_toast(&mut renderer, &request).expect("production toast");
  // SAFETY: flush this thread's owned DIB operations before reading pixels.
  assert!(unsafe { GdiFlush() }.as_bool());
  let width = bitmap.width as u32;
  let layer: Vec<u8> = bitmap
    .pixels_mut()
    .as_chunks::<4>()
    .0
    .iter()
    .flat_map(|p| [p[2], p[1], p[0], p[3]])
    .collect();
  image::save_buffer(
    output.join("toast.png"),
    &layer,
    width,
    height as u32,
    image::ColorType::Rgba8,
  )
  .expect("write toast layer");
}

fn pointer_progress(t: f64, timing: Option<[f64; 4]>) -> f64 {
  if t <= 0.0 {
    return 0.0;
  }
  let Some([x1, y1, x2, y2]) = timing else {
    return t * t * (3.0 - 2.0 * t);
  };
  assert!(
    [x1, y1, x2, y2]
      .iter()
      .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
  );
  let cubic = |u: f64, a: f64, b: f64| {
    3.0 * (1.0 - u).powi(2) * u * a + 3.0 * (1.0 - u) * u * u * b + u.powi(3)
  };
  let (mut lo, mut hi) = (0.0, 1.0);
  for _ in 0..20 {
    let mid = (lo + hi) / 2.0;
    if cubic(mid, x1, x2) < t {
      lo = mid;
    } else {
      hi = mid;
    }
  }
  if t >= 1.0 {
    1.0
  } else {
    cubic((lo + hi) / 2.0, y1, y2)
  }
}

#[test]
fn replay_pointer_routes_keep_endpoints_and_monotonic_timing() {
  let timing = Some([0.16, 0.04, 0.28, 1.0]);
  assert_eq!(pointer_progress(0.0, timing), 0.0);
  assert_eq!(pointer_progress(1.0, timing), 1.0);
  let mut previous = 0.0;
  for i in 0..=100 {
    let progress = pointer_progress(i as f64 / 100.0, timing);
    assert!(progress >= previous && progress <= 1.0);
    previous = progress;
  }
  let (from, to) = ((55.0, 400.0), (640.0, 300.0));
  let curve = Some([-20.0, 9.0]);
  assert_eq!(pointer_path(from, to, 0.0, curve), from);
  assert_eq!(pointer_path(from, to, 1.0, curve), to);
  assert_ne!(
    pointer_path(from, to, 0.5, curve),
    pointer_path(from, to, 0.5, None)
  );
}

fn pointer_path(from: (f64, f64), to: (f64, f64), t: f64, curve: Option<[f64; 2]>) -> (f64, f64) {
  let [a, b] = curve.unwrap_or([0.0, 0.0]);
  assert!([a, b].iter().all(|v| v.is_finite() && v.abs() <= 120.0));
  let dx = to.0 - from.0;
  let dy = to.1 - from.1;
  let length = dx.hypot(dy).max(1.0);
  let bend = 3.0 * (1.0 - t).powi(2) * t * a + 3.0 * (1.0 - t) * t * t * b;
  (
    from.0 + dx * t - dy / length * bend,
    from.1 + dy * t + dx / length * bend,
  )
}

/// Resolve without SetCursor/GetCursorPos: match selection_host's WM_SETCURSOR
/// precedence using only this replay's owner-thread state.
fn replay_cursor(
  editor: &mut AnnotationEditor,
  controller: &selection_core::SelectionControllerState,
  stopped: bool,
  record: bool,
) -> windows::Win32::UI::WindowsAndMessaging::HCURSOR {
  use windows::Win32::UI::WindowsAndMessaging::{IDC_ARROW, IDC_HAND, LoadCursorW};
  if stopped || record || editor.toolbar_cursor(controller) || editor.grip_cursor(controller) {
    // SAFETY: borrowed Windows resource; no global cursor or input state is changed.
    return unsafe {
      LoadCursorW(
        None,
        if !stopped && !record && editor.grip_cursor(controller) {
          IDC_HAND
        } else {
          IDC_ARROW
        },
      )
    }
    .expect("native pointer resource");
  }
  if let Some(cursor) = editor
    .tool_cursor(controller)
    .expect("production tool cursor")
  {
    return cursor;
  }
  let cursor = crate::overlay::selection::cursor_for_hit(
    selection_core::selection_hit_for_cursor(controller),
    matches!(
      controller.overlay_state,
      crate::overlay::selection::OverlayState::Idle
    ),
  );
  assert!(!cursor.is_invalid(), "selection cursor must load");
  cursor
}

/// Two backgrounds retain monochrome AND/XOR behavior, including Windows'
/// inverting crosshair. An RGBA approximation would lose that behavior.
fn export_cursor(
  cursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
  output: &std::path::Path,
  id: usize,
) -> [u32; 2] {
  use windows::Win32::Graphics::Gdi::DeleteObject;
  use windows::Win32::UI::WindowsAndMessaging::{
    DI_NORMAL, DrawIconEx, GetIconInfo, HICON, ICONINFO,
  };
  let mut info = ICONINFO::default();
  // SAFETY: cursor is live on this thread; GetIconInfo returns owned bitmap copies.
  unsafe { GetIconInfo(HICON(cursor.0), &mut info) }.expect("native cursor hotspot");
  // SAFETY: release only bitmap copies returned by GetIconInfo, never the cursor.
  unsafe {
    if !info.hbmMask.is_invalid() {
      assert!(DeleteObject(info.hbmMask.into()).as_bool());
    }
    if !info.hbmColor.is_invalid() {
      assert!(DeleteObject(info.hbmColor.into()).as_bool());
    }
  }
  assert!(info.xHotspot < 96 && info.yHotspot < 96);
  for (name, background) in [("black", 0), ("white", 255)] {
    let mut pixels = vec![background; 96 * 96 * 4];
    for p in pixels.as_chunks_mut::<4>().0 {
      p[3] = 255;
    }
    let mut bitmap = ScreenshotBitmap::from_rgba(&pixels, 96, 96).expect("cursor DIB");
    // SAFETY: draw borrowed cursor into this owned DIB; zero size keeps native resource size.
    unsafe { DrawIconEx(bitmap.dc, 0, 0, HICON(cursor.0), 0, 0, 0, None, DI_NORMAL) }
      .expect("native cursor pixels");
    // SAFETY: finish owner-thread GDI writes before DIB readback.
    assert!(unsafe { GdiFlush() }.as_bool());
    image::save_buffer(
      output.join(format!("cursor-{id}-{name}.png")),
      &bitmap.to_rgba().expect("cursor pixels"),
      96,
      96,
      image::ColorType::Rgba8,
    )
    .expect("cursor export");
  }
  [info.xHotspot, info.yHotspot]
}

struct Parent(HWND);

unsafe extern "system" fn parent_colors(
  hwnd: HWND,
  msg: u32,
  wparam: windows::Win32::Foundation::WPARAM,
  lparam: LPARAM,
  id: usize,
  _data: usize,
) -> windows::Win32::Foundation::LRESULT {
  use windows::Win32::UI::{
    Shell::{DefSubclassProc, RemoveWindowSubclass},
    WindowsAndMessaging::{WM_CTLCOLOREDIT, WM_NCDESTROY},
  };
  if msg == WM_CTLCOLOREDIT
    && let Some(result) =
      super::super::text_field::background(HDC(wparam.0 as *mut _), HWND(lparam.0 as *mut _))
  {
    return result;
  }
  // SAFETY: subclass serves only this hidden parent; its children use production color properties.
  unsafe {
    if msg == WM_NCDESTROY && !RemoveWindowSubclass(hwnd, Some(parent_colors), id).as_bool() {
      eprintln!("Replay parent color cleanup failed");
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
  }
}

fn paint_native_field(editor: &AnnotationEditor, parent: HWND, target: HDC) {
  use windows::Win32::Graphics::Gdi::{BitBlt, ClientToScreen, SRCCOPY, ScreenToClient};
  use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, PRF_CLIENT, PRF_ERASEBKGND, SendMessageW, WM_PRINTCLIENT,
  };
  let Some(edit) = &editor.text_edit else {
    return;
  };
  let mut rect = RECT::default();
  // SAFETY: query only the live, hidden editor child owned on this test thread.
  unsafe { GetClientRect(edit.field.hwnd, &mut rect) }.expect("native EDIT geometry");
  assert!((1..=1366).contains(&rect.right) && (1..=768).contains(&rect.bottom));
  let native = ScreenshotBitmap::from_rgba(
    &vec![255; (rect.right * rect.bottom * 4) as usize],
    rect.right as u32,
    rect.bottom as u32,
  )
  .expect("native EDIT print DIB");
  let mut point = windows::Win32::Foundation::POINT::default();
  // SAFETY: native EDIT prints its own glyphs/backdrop into the owned DC; no injected input or editor callback is involved.
  unsafe {
    SendMessageW(
      edit.field.hwnd,
      WM_PRINTCLIENT,
      Some(windows::Win32::Foundation::WPARAM(native.dc.0 as usize)),
      Some(LPARAM((PRF_CLIENT | PRF_ERASEBKGND) as isize)),
    );
    assert!(ClientToScreen(edit.field.hwnd, &mut point).as_bool());
    assert!(ScreenToClient(parent, &mut point).as_bool());
    BitBlt(
      target,
      point.x,
      point.y,
      rect.right,
      rect.bottom,
      Some(native.dc),
      0,
      0,
      SRCCOPY,
    )
    .expect("compose native EDIT pixels");
  }
}
impl Drop for Parent {
  fn drop(&mut self) {
    // SAFETY: this hidden window and any remaining children belong to this test thread.
    if let Err(error) = unsafe { DestroyWindow(self.0) } {
      eprintln!("Replay window cleanup failed: {error}");
    }
  }
}

fn center(rect: RECT) -> (f64, f64) {
  (
    (rect.left + rect.right) as f64 / 2.0,
    (rect.top + rect.bottom) as f64 / 2.0,
  )
}
fn lparam(point: (f64, f64)) -> LPARAM {
  LPARAM(((point.0.round() as u16 as usize) | ((point.1.round() as u16 as usize) << 16)) as isize)
}

#[test]
#[ignore = "explicit production-input replay frame export"]
fn render_replay_frames() {
  let started = Instant::now();
  let input = PathBuf::from(std::env::var("FLINCH_EXPORT_INPUT").expect("demo PNG path"));
  let plan = PathBuf::from(std::env::var("FLINCH_EXPORT_PLAN").expect("replay JSON path"));
  let output = PathBuf::from(std::env::var("FLINCH_EXPORT_OUTPUT").expect("output path"));
  let steps: Vec<Step> =
    serde_json::from_slice(&fs::read(plan).expect("read plan")).expect("valid plan");
  let frame_count: usize = steps.iter().map(|step| step.frames).sum();
  assert!(!steps.is_empty() && steps.len() <= 200);
  assert!(frame_count > 0 && frame_count <= 1800);
  assert!(steps.iter().all(|step| (1..=300).contains(&step.frames)));
  let pixels = image::open(input).expect("demo pixels").into_rgba8();
  let (width, height) = pixels.dimensions();
  assert_eq!((width, height), (1366, 768));
  fs::create_dir_all(&output).expect("frame directory");
  // SAFETY: hidden STATIC parent owns all editor children; it has no visible style or event loop.
  let parent = Parent(
    unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("STATIC"),
        w!("Flinch replay"),
        WS_POPUP,
        0,
        0,
        width as i32,
        height as i32,
        None,
        None,
        None,
        None,
      )
    }
    .expect("hidden replay parent"),
  );
  // SAFETY: same-thread subclass supplies the production parent color callback for hidden EDIT printing.
  assert!(
    unsafe { windows::Win32::UI::Shell::SetWindowSubclass(parent.0, Some(parent_colors), 41, 0) }
      .as_bool()
  );
  let source = Arc::new(pixels.into_raw());
  let mut shared =
    SharedSelectionOverlayState::new(width, height, Arc::clone(&source), 0, 0, started);
  shared.resources.screenshot =
    Some(ScreenshotBitmap::from_rgba(&source, width, height).expect("source DIB"));
  let mut bitmap = ScreenshotBitmap::from_rgba(&source, width, height).expect("render DIB");
  shared.resources.offscreen_dc = bitmap.dc;
  // SAFETY: borrowed stock font; DIBs stay on their owner thread.
  let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
  // As the selection host builds it: blur regions read the frozen capture.
  let mut editor = AnnotationEditor::new("en".into()).with_frozen_pixels(FrozenPixels {
    rgba: Arc::clone(&source),
    width,
    height,
  });
  editor.monitors.clear();
  shared.controller.cursor_pos = (680.0, 360.0);
  let mut hud = LayeredBitmap::new(width as i32, height as i32).expect("HUD DIB");
  let mut shell = RecordingOverlayShell::new(started, "en".into());
  shell.phase = RecordingPhase::Recording;
  let mut timer_style = TimerTextStyle::Hms;
  let mut metrics = recording_toolbar::fitted_metrics(
    &mut shell.chrome,
    "en",
    shell.phase,
    Duration::ZERO,
    timer_style,
  )
  .expect("HUD metrics");
  let mut recording_origin = ((width as i32 - metrics.width) / 2, 674);
  // Left, top, right, bottom; a record step's bounds replace the original fixture area.
  let mut record_area = (142, 116, 1224, 584);
  let mut record = false;
  let mut pressed = false;
  let mut record_time = Duration::ZERO;
  let mut manifest = Vec::with_capacity(frame_count);
  let mut index = 0;
  let mut export_written = false;
  let mut stopped = false;
  let mut cursors = std::collections::HashMap::new();
  // The selection magnifier, or the Color Picker's lens while `pick` holds.
  let mut magnifier: Option<crate::overlay::magnifier::Magnifier> = None;
  let mut pick = false;
  let mut picked: Option<String> = None;
  // Lens motion follows the replay's frame clock, not the time it takes to export a frame.
  let epoch = Instant::now();
  let lang = "en";
  for (step_index, step) in steps.iter().enumerate() {
    let from = shared.controller.cursor_pos;
    let endpoint = if let Some(point) = step.point {
      assert!(point[0].is_finite() && point[1].is_finite());
      assert!((0.0..width as f64).contains(&point[0]) && (0.0..height as f64).contains(&point[1]));
      (point[0], point[1])
    } else if let Some(target) = &step.target {
      if let Some(action) = target.strip_prefix("toolbar:") {
        let action: i32 = action.parse().expect("toolbar action");
        assert!(ACTIONS.contains(&action));
        let rect =
          AnnotationEditor::confirmed(&shared.controller).expect("confirmed crop for toolbar");
        center(toolbar_button(
          editor.toolbar_rect(&shared.controller, rect),
          action,
          editor.metrics(&shared.controller, rect),
        ))
      } else if let Some(color) = target.strip_prefix("color:") {
        let color: i32 = color.parse().expect("palette index");
        assert!((0..12).contains(&color) && editor.color_open);
        let rect = AnnotationEditor::confirmed(&shared.controller).expect("palette crop");
        center(color_cell(
          editor.picker_rect(&shared.controller, rect),
          color,
          editor.metrics(&shared.controller, rect),
        ))
      } else if let Some(row) = target.strip_prefix("width:") {
        let row: i32 = row.parse().expect("width index");
        assert!((0..3).contains(&row) && editor.width_open);
        let rect = AnnotationEditor::confirmed(&shared.controller).expect("width crop");
        center(width_row(
          editor.picker_rect(&shared.controller, rect),
          row,
          editor.metrics(&shared.controller, rect),
        ))
      } else {
        let layout =
          recording_toolbar::toolbar_layout(metrics, recording_origin.0, recording_origin.1);
        center(match target.as_str() {
          "pause" => layout.pause_rect,
          "stop" => layout.stop_rect,
          "grip" => layout.drag_rect,
          _ => panic!("unknown replay target"),
        })
      }
    } else {
      from
    };
    for frame in 0..step.frames {
      assert!(
        started.elapsed() < Duration::from_secs(600),
        "bounded replay deadline"
      );
      let t = (frame + 1) as f64 / step.frames as f64;
      let eased = pointer_progress(t, step.timing);
      if step.kind == "move" {
        let point = pointer_path(from, endpoint, eased, step.curve);
        assert!((0.0..width as f64).contains(&point.0) && (0.0..height as f64).contains(&point.1));
        if pick {
          // As in the Color Picker: only the lens follows the pointer, with no window snapping.
          shared.controller.cursor_pos = point;
        } else if record {
          let previous = shared.controller.cursor_pos;
          shared.controller.cursor_pos = point;
          if pressed && shell.hover == HoverTarget::Drag {
            recording_origin = (
              (recording_origin.0 + (point.0 - previous.0).round() as i32)
                .clamp(0, width as i32 - metrics.width),
              (recording_origin.1 + (point.1 - previous.1).round() as i32)
                .clamp(0, height as i32 - 60),
            );
          }
        } else if !editor.pointer_move(&mut shared.controller, parent.0, lparam(point)) {
          selection_core::handle_selection_mouse_move(
            &mut shared.controller,
            parent.0,
            lparam(point),
          );
        }
      } else if frame == 0 {
        match step.kind.as_str() {
          "snap-window" => {
            let [x, y, w, h] = step.bounds.expect("fixture window bounds");
            assert!(x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0);
            assert!(x + w <= width as f64 && y + h <= height as f64);
            shared.controller.window_snapshot =
              vec![crate::overlay::core::window_detection::DetectedWindow {
                hwnd: 0,
                bounds: crate::overlay::selection::Rect { x, y, w, h },
                source_app: Some("game".into()),
              }];
            if let Some(point) = step.point {
              selection_core::handle_selection_mouse_move(
                &mut shared.controller,
                parent.0,
                lparam((point[0], point[1])),
              );
            }
          },
          "hold" => {},
          "magnifier" => {
            assert!(magnifier.is_none() && !pick, "one lens per replay");
            magnifier = Some(crate::overlay::magnifier::Magnifier::new(true));
          },
          "pick" => {
            assert!(
              step_index == 0 && magnifier.is_none(),
              "the Color Picker opens as its own overlay"
            );
            pick = true;
            magnifier = Some(crate::overlay::magnifier::Magnifier::picker());
          },
          "down" if pick => {
            // A press picks, as in the Color Picker, and the overlay closes on the frozen frame.
            let pixel = crate::overlay::selection_host::picked_pixel(
              shared.controller.cursor_pos,
              width as i32,
              height as i32,
            );
            let offset = ((pixel.y * width + pixel.x) * 4) as usize;
            let color = [source[offset], source[offset + 1], source[offset + 2]];
            let hex = crate::overlay::magnifier::hex(color);
            let title = crate::i18n::native_settings(lang, "color_copied").expect("toast title");
            write_toast(
              &output,
              &title,
              &hex,
              crate::commands::screenshot::color_swatch(color),
            );
            picked = Some(hex);
            pick = false;
            magnifier = None;
            stopped = true;
          },
          "down" => {
            pressed = true;
            if !record && !editor.pointer_down(&shared.controller, parent.0) {
              selection_core::handle_selection_lbutton_down(&mut shared.controller, parent.0);
            }
          },
          "up" => {
            if record {
              let layout =
                recording_toolbar::toolbar_layout(metrics, recording_origin.0, recording_origin.1);
              let target = recording_toolbar::toolbar_hover(
                shared.controller.cursor_pos.0 as i32,
                shared.controller.cursor_pos.1 as i32,
                layout,
              );
              if pressed && target == HoverTarget::Pause {
                shell.acknowledge_pause(!shell.is_paused(), record_time, started + record_time);
              }
              if pressed && target == HoverTarget::Stop {
                record = false;
                stopped = true;
              }
            } else if !editor.pointer_up(&shared.controller, parent.0) {
              let _ = selection_core::handle_selection_lbutton_up(&mut shared.controller, parent.0);
              editor.sync_crop(&mut shared.controller);
            }
            pressed = false;
          },
          "alt" => shared.controller.modifiers.alt = true,
          "release-alt" => shared.controller.modifiers.alt = false,
          "type" => {
            let field = editor
              .text_edit
              .as_ref()
              .expect("active native EDIT")
              .field
              .hwnd;
            let text = step.text.as_ref().expect("typed text");
            assert!(text.len() <= 160);
            let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            // SAFETY: finite text is assigned only to this hidden test-owned native EDIT control.
            unsafe { SetWindowTextW(field, PCWSTR(wide.as_ptr())) }.expect("type into EDIT");
            editor.text_changed(&shared.controller, parent.0, Some(field));
          },
          "enter" => {
            assert!(editor.keydown(&shared.controller, parent.0, 0x0d));
          },
          "export" | "finish-enter" | "release" => {
            if step.kind == "finish-enter" {
              assert!(!editor.keydown(&shared.controller, parent.0, 0x0d));
              assert!(matches!(
                selection_core::handle_selection_keydown(&mut shared.controller, parent.0, 0x0d),
                selection_core::SelectionKeyOutcome::Confirm(_)
              ));
              let snapped = steps.iter().any(|s| s.kind == "snap-window");
              assert!(matches!(
                shared.controller.interaction,
                crate::overlay::selection::SelectionInteraction::Confirmed { from_snap, .. }
                  if from_snap == snapped
              ));
            }
            assert!(editor.draft.is_none() && editor.text_edit.is_none());
            check_export(
              &editor,
              &steps,
              step
                .expect
                .as_deref()
                .expect("export steps list the drawings they require"),
            );
            let selection = if step.kind == "release" {
              // Without Alt, releasing the drag captures at once, as in the real overlay.
              pressed = false;
              match selection_core::handle_selection_lbutton_up(&mut shared.controller, parent.0) {
                selection_core::SelectionMouseUpOutcome::Confirm(selection) => selection,
                _ => panic!("a plain release must capture"),
              }
            } else {
              let rect = AnnotationEditor::confirmed(&shared.controller).expect("export crop");
              crate::overlay::selection::finalize_rect(rect, width as f64, height as f64)
                .expect("valid export crop")
            };
            let image =
              image::RgbaImage::from_raw(width, height, source.as_ref().clone()).expect("source");
            let crop = image::imageops::crop_imm(
              &image,
              selection.x,
              selection.y,
              selection.width,
              selection.height,
            )
            .to_image()
            .into_raw();
            // The capture path blurs the crop before compositing the other drawings.
            let mut crop = crop;
            crate::screenshot::blur::apply(
              &mut crop,
              &selection,
              &source,
              width,
              height,
              &editor.annotations,
            );
            let output_pixels =
              crate::screenshot::annotations::composite(crop, &selection, &editor.annotations)
                .expect("production annotation export");
            image::save_buffer(
              output.join("captured-result.png"),
              &output_pixels,
              selection.width,
              selection.height,
              image::ColorType::Rgba8,
            )
            .expect("save result");
            let strings = crate::i18n::get(lang);
            write_toast(
              &output,
              crate::i18n::screenshot_notif_titles(lang).copied,
              strings.body_image_copied,
              crate::overlay::notification::build_thumbnail_image(
                &output_pixels,
                selection.width,
                selection.height,
              ),
            );
            export_written = true;
            if step.kind != "export" {
              stopped = true;
            }
          },
          "record" => {
            record = true;
            stopped = false;
            record_time = Duration::from_secs(2);
            // A named mode starts its timer at zero, and "gif" uses the GIF bar's style.
            if let Some(mode) = step.text.as_deref() {
              record_time = Duration::ZERO;
              if mode == "gif" {
                timer_style = TimerTextStyle::Gif { limit_secs: 10 };
              }
              metrics = recording_toolbar::fitted_metrics(
                &mut shell.chrome,
                "en",
                shell.phase,
                Duration::ZERO,
                timer_style,
              )
              .expect("HUD metrics");
            }
            if let Some([x, y, w, h]) = step.bounds {
              assert!(x >= 0.0 && y >= 0.0 && w >= 34.0 && h >= 34.0);
              assert!(x + w <= width as f64 && y + h <= height as f64);
              record_area = (x as i32, y as i32, (x + w) as i32, (y + h) as i32);
              // Same placement as the production area HUD, on this one fixture display.
              recording_origin = crate::overlay::core::place_toolbar_near_selection(
                &[],
                0,
                0,
                record_area,
                (metrics.width, metrics.height),
                recording_toolbar::TOOLBAR_GAP,
                recording_toolbar::TOOLBAR_MARGIN,
                (width as i32, height as i32),
              );
            }
          },
          _ => panic!("unsupported replay operation"),
        }
      }
      let position = shared.controller.cursor_pos;
      let mut hud_file = None;
      let full = RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
      };
      if let Some(lens) = &mut magnifier {
        let toolbar = (!pick)
          .then(|| editor.crop_toolbar_rect(&shared.controller))
          .flatten();
        let clock = epoch + Duration::from_secs_f64(index as f64 / 30.0);
        lens.update(&shared.controller, |_| full, toolbar, clock, true);
      }
      // After the scene and its cache, as in the selection host.
      let paint_lens = |magnifier: &mut Option<crate::overlay::magnifier::Magnifier>,
                        shared: &SharedSelectionOverlayState| {
        if let Some(lens) = magnifier {
          let shot = shared.resources.screenshot.as_ref().expect("frozen frame");
          lens
            .paint(bitmap.dc, shot.dc, shot.width, shot.height, full)
            .expect("production lens");
        }
      };
      let rgba = if stopped {
        source.as_ref().clone()
      } else if pick {
        let shot = shared.resources.screenshot.as_ref().expect("frozen frame");
        // SAFETY: both are live memory DCs owned by this thread; the picker shows true pixels.
        unsafe {
          windows::Win32::Graphics::Gdi::BitBlt(
            bitmap.dc,
            0,
            0,
            width as i32,
            height as i32,
            Some(shot.dc),
            0,
            0,
            windows::Win32::Graphics::Gdi::SRCCOPY,
          )
        }
        .expect("picker frame copy");
        paint_lens(&mut magnifier, &shared);
        // SAFETY: bitmap is owned on this thread and GDI writes finish before readback.
        assert!(unsafe { GdiFlush() }.as_bool());
        bitmap.to_rgba().expect("frame pixels")
      } else if record {
        let layout =
          recording_toolbar::toolbar_layout(metrics, recording_origin.0, recording_origin.1);
        shell.hover =
          recording_toolbar::toolbar_hover(position.0 as i32, position.1 as i32, layout);
        if !shell.is_paused() {
          record_time += Duration::from_secs_f64(1.0 / 30.0);
        }
        hud.pixels_mut().fill(0);
        let (left, top, right, bottom) = record_area;
        crate::overlay::recording_shell::draw_selection_border(
          &mut hud, left, top, right, bottom, 2,
        );
        let paused = shell.is_paused();
        recording_toolbar::draw_toolbar_state(
          &mut hud,
          &mut shell.chrome,
          layout,
          metrics,
          shell.hover,
          paused,
          record_time,
          timer_style,
          "en",
          shell.phase,
          recording_toolbar::recording_pulse_alpha(shell.phase, record_time),
          if pressed {
            shell.hover
          } else {
            HoverTarget::None
          },
        )
        .expect("recording renderer");
        // SAFETY: flush this thread's owned DIB operations before reading pixels.
        assert!(unsafe { GdiFlush() }.as_bool());
        // Premultiplied layer, so an editor can place the HUD over moving footage.
        let name = format!("hud-{index:04}.png");
        let layer: Vec<u8> = hud
          .pixels_mut()
          .as_chunks::<4>()
          .0
          .iter()
          .flat_map(|p| [p[2], p[1], p[0], p[3]])
          .collect();
        image::save_buffer(
          output.join(&name),
          &layer,
          width,
          height,
          image::ColorType::Rgba8,
        )
        .expect("write HUD layer");
        hud_file = Some(name);
        let mut rgba = source.as_ref().clone();
        for (dst, pixel) in rgba
          .as_chunks_mut::<4>()
          .0
          .iter_mut()
          .zip(hud.pixels_mut().as_chunks::<4>().0)
        {
          let inverse = 255 - u16::from(pixel[3]);
          for (channel, src) in [(0, 2), (1, 1), (2, 0)] {
            dst[channel] =
              (u16::from(pixel[src]) + u16::from(dst[channel]) * inverse / 255).min(255) as u8;
          }
          dst[3] = 255;
        }
        rgba
      } else {
        let show_indicator = editor.shows_selection_indicator(&shared.controller);
        selection_core::draw_selection_scene_with_indicator(
          &mut shared,
          font,
          SelectionDrawColors::DEFAULT,
          show_indicator,
        );
        editor.paint_cached(
          &shared.controller,
          bitmap.dc,
          font,
          RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
          },
          false,
        );
        assert!(
          !editor.render_failed,
          "native render failed in step {step_index}"
        );
        paint_native_field(&editor, parent.0, bitmap.dc);
        paint_lens(&mut magnifier, &shared);
        // SAFETY: bitmap is owned on this thread and GDI writes finish before readback.
        assert!(unsafe { GdiFlush() }.as_bool());
        bitmap.to_rgba().expect("frame pixels")
      };
      let name = format!("frame-{index:04}.png");
      let cursor = replay_cursor(&mut editor, &shared.controller, stopped, record);
      let cursor_key = cursor.0 as usize;
      if !cursors.contains_key(&cursor_key) {
        let id = cursors.len();
        let hotspot = export_cursor(cursor, &output, id);
        cursors.insert(cursor_key, (id, hotspot));
      }
      let (cursor_id, hotspot) = cursors[&cursor_key];
      let writer =
        std::io::BufWriter::new(fs::File::create(output.join(&name)).expect("frame file"));
      image::codecs::png::PngEncoder::new_with_quality(
        writer,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Sub,
      )
      .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
      .expect("write native frame");
      manifest.push(serde_json::json!({"file":name,"step":step_index,"kind":step.kind,"tag":step.tag,"cursor":[position.0,position.1],"cursor_id":cursor_id,"cursor_hotspot":hotspot,"pressed":pressed,"recording":record,"hud":hud_file,"paused":record && shell.is_paused(),"elapsed":record_time.as_secs_f64(),"annotations":editor.annotations.len(),"text_active":editor.text_edit.is_some()}));
      index += 1;
    }
    println!("Replay {step_index}: {} / {index} frames", step.kind);
  }
  editor.complete_text(parent.0, false, None);
  editor.cancel_gesture();
  if steps
    .iter()
    .filter_map(|s| s.expect.as_deref())
    .any(|expect| expect.iter().any(|check| check == "cursors"))
  {
    use windows::Win32::UI::WindowsAndMessaging::{IDC_ARROW, IDC_CROSS, IDC_SIZEALL, LoadCursorW};
    for resource in [IDC_CROSS, IDC_ARROW, IDC_SIZEALL] {
      // SAFETY: inspect borrowed Windows resources; no cursor/input state is changed.
      let cursor = unsafe { LoadCursorW(None, resource) }.expect("required system cursor");
      assert!(
        cursors.contains_key(&(cursor.0 as usize)),
        "snap, toolbar and move cursors must occur"
      );
    }
  }
  let exports = steps
    .iter()
    .any(|s| matches!(s.kind.as_str(), "export" | "finish-enter" | "release"));
  assert_eq!(
    export_written, exports,
    "an export step must produce a real annotation export"
  );
  assert_eq!(
    picked.is_some(),
    steps.first().is_some_and(|s| s.kind == "pick"),
    "a Color Picker replay must end with a pick"
  );
  if let Some(hex) = &picked {
    fs::write(output.join("picked.txt"), hex).expect("write picked color");
  }
  fs::write(
    output.join("frames.json"),
    serde_json::to_vec(&manifest).expect("manifest"),
  )
  .expect("write manifest");
}
