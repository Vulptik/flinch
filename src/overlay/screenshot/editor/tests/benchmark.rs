use super::*;

pub(super) fn rendering_performance_baseline() {
  use crate::overlay::gdi::bitmap::ScreenshotBitmap;
  use crate::overlay::screenshot::performance::{Diagnostics, elapsed_us};
  use crate::overlay::selection::Selection;
  use crate::overlay::selection_core::{
    SelectionDrawColors, SharedSelectionOverlayState, draw_selection_scene,
  };
  use std::sync::Arc;
  use std::time::Instant;
  use windows::Win32::Graphics::Gdi::{
    BitBlt, DEFAULT_GUI_FONT, GdiFlush, GetStockObject, SRCCOPY,
  };

  fn resources() -> serde_json::Value {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{
      GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources, GetProcessTimes,
    };
    let mut memory = PROCESS_MEMORY_COUNTERS {
      cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
      ..Default::default()
    };
    let (mut created, mut exited, mut kernel, mut user) = (
      FILETIME::default(),
      FILETIME::default(),
      FILETIME::default(),
      FILETIME::default(),
    );
    // SAFETY: pseudo process handle is borrowed; output structures have the required layouts.
    unsafe {
      let process = GetCurrentProcess();
      GetProcessMemoryInfo(process, &mut memory, memory.cb).expect("process memory counters");
      GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user)
        .expect("process CPU times");
      let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
      serde_json::json!({"working_set_bytes": memory.WorkingSetSize,
        "gdi_handles": GetGuiResources(process, GR_GDIOBJECTS),
        "cpu_us": (ticks(kernel) + ticks(user)) / 10})
    }
  }

  let reference = matches!(
    std::env::var("FLINCH_ANNOTATION_BENCHMARK_REFERENCE").as_deref(),
    Ok("1")
  );
  let mut results = Vec::new();
  let process_before = resources();
  for (width, height) in [(1920u32, 1080u32), (3840, 2160)] {
    for workload in [
      "idle",
      "hover",
      "long_pen",
      "mixed_50",
      "maximum_256",
      "undo_redo",
      "crop_move_resize",
      "labels_50",
      "labels_256",
    ] {
      let before = resources();
      let source = Arc::new(vec![120u8; width as usize * height as usize * 4]);
      let mut shared =
        SharedSelectionOverlayState::new(width, height, Arc::clone(&source), 0, 0, Instant::now());
      if reference {
        shared.resources.background.disable();
      }
      shared.resources.screenshot =
        Some(ScreenshotBitmap::from_rgba(&source, width, height).expect("frozen source"));
      let offscreen = ScreenshotBitmap::from_rgba(&source, width, height).expect("offscreen");
      let present = ScreenshotBitmap::from_rgba(&source, width, height).expect("present target");
      shared.resources.offscreen_dc = offscreen.dc; // Borrowed; bitmap owns cleanup on this thread.
      // SAFETY: stock font is borrowed and never deleted by the harness.
      let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
      let base_rect = Rect {
        x: 80.0,
        y: 60.0,
        w: width as f64 - 160.0,
        h: height as f64 - 220.0,
      };
      shared.controller.interaction = SelectionInteraction::Confirmed {
        rect: base_rect,
        from_snap: false,
        moving: None,
        resizing: None,
      };
      let mut editor = AnnotationEditor::new("en".into());
      // Fixed virtual monitor makes workload layout reproducible on any physical desktop.
      editor.monitors.clear();
      editor.diagnostics = Some(Diagnostics::default());
      let count = match workload {
        "mixed_50" | "undo_redo" | "crop_move_resize" | "labels_50" => 50,
        "maximum_256" | "labels_256" => 256,
        _ => 0,
      };
      for index in 0..count {
        let x = 100 + (index * 61 % (width as i32 - 400));
        let y = 100 + (index * 43 % (height as i32 - 420));
        let mut annotation = Annotation {
          corner_radius: 0,
          rotation_millidegrees: 0,
          path_transform: crate::screenshot::path::IDENTITY,
          curve_controls: Vec::new(),
          pen_nodes: Vec::new(),
          label: None,
          id: index as u64 + 1,
          tool: TOOLS[1 + index as usize % 4],
          points: vec![(x, y), (x + 140, y + 90)],
          color: COLORS[index as usize % 4],
          width: 3,
        };
        if workload.starts_with("labels_") {
          annotation.points = vec![(x, y)];
          if index % 2 == 0 {
            annotation.tool = Tool::Text;
            annotation.label = Some(Label::Text(
              text_data(format!("Step {}\nCafé 日本語", index + 1), 24, 160)
                .expect("benchmark text layout"),
            ));
          } else {
            annotation.tool = Tool::Marker;
            annotation.label = Some(Label::Number(index as u16 + 1));
          }
        }
        assert!(
          editor.apply_change(crate::overlay::screenshot::history::Change {
            counter: None,
            index: editor.annotations.len(),
            before: None,
            after: Some(annotation)
          })
        );
      }
      editor.next_id = count as u64 + 1;
      if workload == "long_pen" {
        editor.draft = Some(Annotation {
          corner_radius: 0,
          rotation_millidegrees: 0,
          path_transform: crate::screenshot::path::IDENTITY,
          curve_controls: Vec::new(),
          pen_nodes: Vec::new(),
          label: None,
          id: 0,
          tool: Tool::Pen,
          points: (0..MAX_POINTS)
            .map(|i| {
              (
                100 + i as i32 % (width as i32 - 250),
                100 + (i as i32 / 20) % (height as i32 - 350),
              )
            })
            .collect(),
          color: COLORS[0],
          width: 3,
        });
      }
      let render_before = resources();
      let wall_started = Instant::now();
      for frame in 0..65 {
        if workload == "hover" {
          let panel = toolbar_rect(&shared.controller, base_rect, &editor.monitors);
          let button = toolbar_button(
            panel,
            frame % BUTTONS,
            editor.metrics(&shared.controller, base_rect),
          );
          shared.controller.cursor_pos = ((button.left + 8) as f64, (button.top + 8) as f64);
        }
        if workload == "undo_redo" {
          if frame % 2 == 0 {
            editor.undo();
          } else {
            editor.redo();
          }
        }
        if workload == "crop_move_resize" {
          shared.controller.interaction = SelectionInteraction::Confirmed {
            rect: Rect {
              x: base_rect.x + (frame % 16) as f64,
              w: base_rect.w - (frame % 20) as f64,
              ..base_rect
            },
            from_snap: false,
            moving: None,
            resizing: None,
          };
        }
        let full = RECT {
          left: 0,
          top: 0,
          right: width as i32,
          bottom: height as i32,
        };
        let damage = if reference || frame == 0 {
          full
        } else if workload == "hover" {
          editor.chrome_region(&shared.controller, base_rect)
        } else if workload == "long_pen" {
          editor.active_bounds().expect("draft bounds")
        } else {
          full
        };
        let damage = crate::overlay::screenshot::cache::intersect(damage, full);
        let started = Instant::now();
        let restored = if reference {
          None
        } else {
          editor.restore_scene(&shared.controller, offscreen.dc, damage)
        };
        if restored.is_none() {
          draw_selection_scene(&mut shared, font, SelectionDrawColors::DEFAULT);
        }
        // SAFETY: synchronize GDI so scene timings include completion rather than just submission.
        assert!(unsafe { GdiFlush() }.as_bool());
        let scene_us = elapsed_us(started);
        if reference {
          editor.draw(&shared.controller, offscreen.dc, font);
        } else {
          editor.paint_cached(
            &shared.controller,
            offscreen.dc,
            font,
            damage,
            restored.is_some(),
          );
        }
        assert!(!editor.render_failed);
        let present_started = Instant::now();
        // SAFETY: both full-sized DIB DCs are live and owned by this test thread.
        unsafe {
          BitBlt(
            present.dc,
            damage.left,
            damage.top,
            damage.right - damage.left,
            damage.bottom - damage.top,
            Some(offscreen.dc),
            damage.left,
            damage.top,
            SRCCOPY,
          )
          .expect("present");
          assert!(GdiFlush().as_bool());
        }
        let mut sample = std::mem::take(&mut editor.paint_sample);
        sample.present_us = elapsed_us(present_started);
        sample.total_us = elapsed_us(started);
        sample.scene_us = scene_us;
        sample.dirty_pixels = crate::overlay::screenshot::cache::area(damage);
        sample.composed_pixels = restored.unwrap_or(u64::from(width) * u64::from(height));
        sample.presented_pixels = sample.dirty_pixels;
        editor
          .diagnostics
          .as_mut()
          .expect("enabled diagnostics")
          .record(sample);
      }
      let render_wall_us = elapsed_us(wall_started);
      let render_after = resources();
      editor.finish_gesture();
      let selection = Selection {
        x: 80,
        y: 60,
        width: width - 160,
        height: height - 220,
        source_app: None,
      };
      let crop = crate::screenshot::encoding::crop_rgba_direct(
        &source,
        width,
        selection.x,
        selection.y,
        selection.width,
        selection.height,
      )
      .expect("valid fixture crop");
      let export_started = Instant::now();
      let output = crate::screenshot::annotations::composite(crop, &selection, &editor.annotations)
        .expect("export benchmark");
      let export_us = elapsed_us(export_started);
      let during = resources();
      let paints = editor.diagnostics.as_ref().expect("diagnostics").report();
      drop(output);
      drop(editor);
      drop(present);
      drop(offscreen);
      drop(shared);
      drop(source);
      results.push(
        serde_json::json!({"width":width,"height":height,"workload":workload,
        "paints":paints,"wall_us":elapsed_us(wall_started),"export_us":export_us,
        "render_wall_us":render_wall_us,"render_before":render_before,"render_after":render_after,
        "before":before,"during":during,"after":resources()}),
      );
    }
  }
  let result = serde_json::json!({"schema":2,"variant":if reference {"reference_full_repaint"} else {"cached_damage_repaint"},"harness":"headless GDI render; no live input, desktop presentation, capture, encoding or clipboard",
    "profile": if cfg!(debug_assertions) {"debug"} else {"release"},
    "machine":std::env::var("COMPUTERNAME").unwrap_or_default(),
    "processors":std::env::var("NUMBER_OF_PROCESSORS").unwrap_or_default(),
    "processor_identifier":std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default(),
    "monitors":crate::monitors::list_monitors().iter().map(|m| serde_json::json!({"name":m.name,"x":m.x,"y":m.y,"width":m.width,"height":m.height,"dpi_scale":m.dpi_scale})).collect::<Vec<_>>(), "process_before":process_before,"process_after":resources(),"workloads":results});
  let output =
    std::env::var("FLINCH_ANNOTATION_BENCHMARK_OUTPUT").expect("set benchmark output path");
  std::fs::write(
    output,
    serde_json::to_vec_pretty(&result).expect("serialize measurements"),
  )
  .expect("write measurements");
}
