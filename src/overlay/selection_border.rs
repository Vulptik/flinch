//! Finite border feedback, owned by the selection thread; never changes capture geometry.
use std::time::{Duration, Instant};

use super::motion::{Motion, Tween, blend_rgb};
use super::selection::{Handle, HitResult, SelectionInteraction, hit_test};
use super::selection_core::SelectionControllerState;

pub const HANDLES: [Handle; 8] = [
  Handle::NW,
  Handle::N,
  Handle::NE,
  Handle::E,
  Handle::SE,
  Handle::S,
  Handle::SW,
  Handle::W,
];
const PULSE: Duration = Duration::from_millis(280);
const SNAP_FLASH: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Visual {
  pub accent: [u8; 3],
  pub edge_strength: u8,
  pub hover: [u8; 8],
}

pub struct Feedback {
  enabled: bool,
  hover: [Tween; 8],
  confirmed: bool,
  confirmation: Option<Instant>,
  snap: Option<Instant>,
  snap_key: Option<[u64; 4]>,
  pending: bool,
  pub visual: Visual,
}

impl Feedback {
  pub fn new() -> Self {
    Self::with_enabled(Motion::new().enabled(), Instant::now())
  }

  fn with_enabled(enabled: bool, now: Instant) -> Self {
    let tween = Tween::new(0.0, Duration::from_millis(100), now);
    Self {
      enabled,
      hover: [tween; 8],
      confirmed: false,
      confirmation: None,
      snap: None,
      snap_key: None,
      pending: false,
      visual: Visual {
        accent: super::theme::ACCENT,
        edge_strength: 64,
        hover: [0; 8],
      },
    }
  }

  pub fn active(&self) -> bool {
    self.pending
  }

  pub fn update(
    &mut self,
    controller: &SelectionControllerState,
    handles_enabled: bool,
    now: Instant,
  ) -> bool {
    let before = self.visual;
    let was_pending = self.pending;
    let confirmed = matches!(
      controller.interaction,
      SelectionInteraction::Confirmed { .. }
    );
    if confirmed && !self.confirmed && self.enabled {
      self.confirmation = Some(now);
    }
    if !confirmed {
      self.confirmation = None;
    }
    self.confirmed = confirmed;
    let hover = if handles_enabled && controller.interaction.is_confirmed_idle() {
      match &controller.interaction {
        SelectionInteraction::Confirmed { rect, .. } => match hit_test(rect, controller.cursor_pos)
        {
          HitResult::Handle(handle) => Some(handle),
          _ => None,
        },
        _ => None,
      }
    } else {
      None
    };
    for (index, tween) in self.hover.iter_mut().enumerate() {
      tween.target(
        u8::from(hover == Some(HANDLES[index])) as f32,
        now,
        self.enabled,
      );
    }
    let preview = matches!(
      controller.interaction,
      SelectionInteraction::Idle { .. } | SelectionInteraction::PendingDecision { .. }
    );
    let key = controller
      .snap_anim
      .to_rect
      .filter(|_| preview)
      .map(|rect| {
        [
          rect.x.to_bits(),
          rect.y.to_bits(),
          rect.w.to_bits(),
          rect.h.to_bits(),
        ]
      });
    let changed = key != self.snap_key;
    if changed {
      // Start on the first paint of each target, even if a transition is interrupted.
      self.snap = (key.is_some() && self.enabled).then_some(now);
    }
    if !preview {
      self.snap = None;
    }
    self.snap_key = key;
    let snap_flash = self.snap.map_or(0, |start| {
      let progress = now.saturating_duration_since(start).as_secs_f32() / SNAP_FLASH.as_secs_f32();
      ((1.0 - progress).clamp(0.0, 1.0) * 255.0).round() as u8
    });
    let pulse = pulse(self.confirmation, now).max(snap_flash);
    self.visual = Visual {
      accent: blend_rgb(super::theme::ACCENT, [255, 154, 154], pulse),
      edge_strength: 64 + ((u16::from(pulse) / 2 * 96) / 255) as u8,
      hover: std::array::from_fn(|index| (self.hover[index].value(now) * 255.0).round() as u8),
    };
    self.pending = self.hover.iter().any(|tween| tween.active(now))
      || self
        .confirmation
        .is_some_and(|start| now.saturating_duration_since(start) < PULSE)
      || self
        .snap
        .is_some_and(|start| now.saturating_duration_since(start) < SNAP_FLASH);
    before != self.visual || was_pending || self.pending
  }

  #[cfg(test)]
  pub fn preview(now: Instant) -> Self {
    Self::with_enabled(true, now)
  }
}

fn pulse(start: Option<Instant>, now: Instant) -> u8 {
  start.map_or(0, |start| {
    let t = now.saturating_duration_since(start).as_secs_f32() / PULSE.as_secs_f32();
    if t >= 1.0 {
      0
    } else {
      ((t * std::f32::consts::PI).sin() * 255.0).round() as u8
    }
  })
}

pub fn draw_hover(
  renderer: &mut super::chrome::Renderer,
  dc: windows::Win32::Graphics::Gdi::HDC,
  sel: windows::Win32::Foundation::RECT,
  visual: Visual,
) -> anyhow::Result<()> {
  use windows::Win32::Foundation::RECT;
  let cx = (sel.left + sel.right) / 2;
  let cy = (sel.top + sel.bottom) / 2;
  let marks = super::gdi::drawing::selection_mark_rects(sel, true);
  for (index, handle) in HANDLES.into_iter().enumerate() {
    let alpha = visual.hover[index];
    if alpha == 0 {
      continue;
    }
    let (x, y) = match handle {
      Handle::NW => (sel.left, sel.top),
      Handle::N => (cx, sel.top),
      Handle::NE => (sel.right - 1, sel.top),
      Handle::E => (sel.right - 1, cy),
      Handle::SE => (sel.right - 1, sel.bottom - 1),
      Handle::S => (cx, sel.bottom - 1),
      Handle::SW => (sel.left, sel.bottom - 1),
      Handle::W => (sel.left, cy),
    };
    let bounds = RECT {
      left: x - 16,
      top: y - 16,
      right: x + 16,
      bottom: y + 16,
    };
    let mut scene = super::chrome::Scene::default();
    scene.rounded(
      RECT {
        left: x - 9,
        top: y - 9,
        right: x + 10,
        bottom: y + 10,
      },
      8.0,
      super::theme::ACTIVE_ICON,
      35,
    )?;
    for (mark_index, mark) in marks.iter().enumerate() {
      let selected = match handle {
        Handle::NW => matches!(mark_index, 0 | 4),
        Handle::NE => matches!(mark_index, 2 | 6),
        Handle::SW => matches!(mark_index, 1 | 5),
        Handle::SE => matches!(mark_index, 3 | 7),
        Handle::N => mark_index >= 8 && mark.top == sel.top,
        Handle::S => mark_index >= 8 && mark.bottom == sel.bottom,
        Handle::W => mark_index >= 8 && mark.left == sel.left,
        Handle::E => mark_index >= 8 && mark.right == sel.right,
      };
      if selected {
        scene.fill(*mark, [255, 175, 175])?;
      }
    }
    renderer.render_with_opacity(dc, bounds, &scene, alpha)?;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::super::selection::Rect;
  use super::*;
  #[test]
  #[ignore = "explicit offscreen selection border motion preview"]
  fn render_border_motion_preview() {
    use super::super::gdi::bitmap::ScreenshotBitmap;
    use super::super::selection_core::{
      SelectionDrawColors, SharedSelectionOverlayState, draw_selection_scene_with_indicator,
    };
    use image::codecs::gif::{GifEncoder, Repeat};
    use windows::Win32::Graphics::Gdi::{GdiFlush, HDC, HFONT};
    let output =
      std::path::PathBuf::from(std::env::var("FLINCH_TOOLBAR_PREVIEW_OUTPUT").expect("output"));
    std::fs::create_dir_all(&output).expect("directory");
    let (width, height) = (640, 360);
    let source = std::sync::Arc::new(
      image::RgbaImage::from_pixel(width, height, image::Rgba([39, 41, 46, 255])).into_raw(),
    );
    let mut shared =
      SharedSelectionOverlayState::new(width, height, source.clone(), 0, 0, Instant::now());
    shared.resources.screenshot =
      Some(ScreenshotBitmap::from_rgba(&source, width, height).expect("source"));
    let rect = Rect {
      x: 70.0,
      y: 60.0,
      w: 480.0,
      h: 230.0,
    };
    let mut encoder = GifEncoder::new_with_speed(
      std::fs::File::create(output.join("selection-border-motion.gif")).expect("GIF"),
      30,
    );
    encoder.set_repeat(Repeat::Infinite).expect("repeat");
    for frame in 0..160 {
      let phase = frame / 40;
      let now = Instant::now();
      let start = now - Duration::from_millis((frame % 40) * 16);
      shared.resources.border_feedback = Feedback::preview(start - Duration::from_secs(1));
      shared.controller.cursor_pos = (-100.0, -100.0);
      shared.controller.interaction = SelectionInteraction::Confirmed {
        rect,
        from_snap: false,
        moving: None,
        resizing: None,
      };
      shared.resources.border_feedback.update(
        &shared.controller,
        true,
        if phase == 0 {
          start
        } else {
          start - Duration::from_secs(1)
        },
      );
      match phase {
        1 => shared.controller.cursor_pos = (rect.x, rect.y),
        2 => {
          shared.controller.interaction = SelectionInteraction::FreeDraw {
            start: (rect.x, rect.y),
            current: (rect.x + rect.w, rect.y + rect.h),
          }
        },
        3 => {
          shared.controller.interaction = SelectionInteraction::Idle {
            snapped_window: Some(rect),
          };
          shared.controller.snap_anim.to_rect = Some(rect);
          shared.controller.snap_anim.from_rect = Some(rect);
          shared.controller.snap_anim.start_time = Some(start - Duration::from_secs(1));
        },
        _ => {},
      }
      shared
        .resources
        .border_feedback
        .update(&shared.controller, true, start);
      let mut target = ScreenshotBitmap::from_rgba(&source, width, height).expect("target");
      shared.resources.indicator_motion = super::super::motion::Motion::preview(
        if phase == 0 {
          now.saturating_duration_since(start)
        } else {
          Duration::from_secs(1)
        },
        Duration::from_secs(1),
        Duration::from_secs(1),
        0,
        None,
      );
      shared.resources.offscreen_dc = target.dc;
      draw_selection_scene_with_indicator(
        &mut shared,
        HFONT::default(),
        SelectionDrawColors::DEFAULT,
        true,
      );
      assert!(unsafe { GdiFlush() }.as_bool());
      let pixels = target.to_rgba().expect("pixels");
      if frame == 119 {
        image::save_buffer(
          output.join("selection-border-drawing.png"),
          &pixels,
          width,
          height,
          image::ColorType::Rgba8,
        )
        .expect("drawing PNG");
      }
      if frame == 79 {
        image::save_buffer(
          output.join("selection-border-hover.png"),
          &pixels,
          width,
          height,
          image::ColorType::Rgba8,
        )
        .expect("PNG");
      }
      encoder
        .encode_frame(image::Frame::from_parts(
          image::RgbaImage::from_raw(width, height, pixels).expect("RGBA"),
          0,
          0,
          image::Delay::from_numer_denom_ms(16, 1),
        ))
        .expect("frame");
      shared.resources.offscreen_dc = HDC::default();
    }
  }
  #[test]
  fn confirmation_pulses_once_and_hover_retargets_without_changing_geometry() {
    let now = Instant::now();
    let mut state = SelectionControllerState::new(800, 600, 0, 0);
    let rect = Rect {
      x: 50.0,
      y: 50.0,
      w: 500.0,
      h: 300.0,
    };
    state.interaction = SelectionInteraction::Confirmed {
      rect,
      from_snap: false,
      moving: None,
      resizing: None,
    };
    let mut feedback = Feedback::preview(now);
    feedback.update(&state, true, now);
    feedback.update(&state, true, now + Duration::from_millis(140));
    assert!(feedback.visual.accent[1] > crate::overlay::theme::ACCENT[1]);
    feedback.update(&state, true, now + Duration::from_secs(1));
    assert!(!feedback.active());
    assert_eq!(feedback.visual.accent, crate::overlay::theme::ACCENT);
    state.cursor_pos = (50.0, 50.0);
    feedback.update(&state, true, now + Duration::from_secs(1));
    feedback.update(&state, true, now + Duration::from_millis(1100));
    assert_eq!(feedback.visual.hover[0], 255);
    assert_eq!(
      state.interaction.get_current_rect(800.0, 600.0),
      Some(rect.to_x1y1x2y2())
    );
    feedback.update(&state, false, now + Duration::from_millis(1200));
    feedback.update(&state, false, now + Duration::from_millis(1300));
    assert_eq!(feedback.visual.hover, [0; 8]);
    assert!(!feedback.active());
  }
  #[test]
  fn drawing_and_moving_keep_resting_edit_chrome_with_reduced_motion() {
    let now = Instant::now();
    let mut state = SelectionControllerState::new(800, 600, 0, 0);
    state.interaction = SelectionInteraction::FreeDraw {
      start: (10.0, 10.0),
      current: (400.0, 300.0),
    };
    let mut feedback = Feedback::with_enabled(false, now);
    feedback.update(&state, true, now);
    assert_eq!(feedback.visual.edge_strength, 64);
    assert_eq!(feedback.visual.accent, crate::overlay::theme::ACCENT);
    assert!(!feedback.active());
    state.interaction = SelectionInteraction::Confirmed {
      rect: Rect {
        x: 10.0,
        y: 10.0,
        w: 390.0,
        h: 290.0,
      },
      from_snap: false,
      moving: Some((10.0, 10.0)),
      resizing: None,
    };
    feedback.update(&state, true, now);
    assert_eq!(feedback.visual.edge_strength, 64);
    assert_eq!(feedback.visual.accent, crate::overlay::theme::ACCENT);
    assert!(!feedback.active());
    state.interaction = SelectionInteraction::default();
    feedback.update(&state, true, now);
    assert_eq!(feedback.visual.edge_strength, 64);
    assert!(!feedback.active());
  }

  #[test]
  fn snap_flashes_immediately_once_then_stops_requesting_frames() {
    let now = Instant::now();
    let mut state = SelectionControllerState::new(800, 600, 0, 0);
    let rect = Rect {
      x: 50.0,
      y: 50.0,
      w: 500.0,
      h: 300.0,
    };
    state.snap_anim.to_rect = Some(rect);
    state.snap_anim.from_rect = Some(rect);
    state.snap_anim.start_time = Some(now - Duration::from_secs(1));
    let mut feedback = Feedback::preview(now);
    feedback.update(&state, true, now);
    assert!(feedback.visual.accent[1] > crate::overlay::theme::ACCENT[1]);
    feedback.update(&state, true, now + Duration::from_millis(60));
    assert!(feedback.active());
    assert!(
      feedback.update(&state, true, now + SNAP_FLASH),
      "final frame must clear cached pulse"
    );
    assert!(!feedback.active());
    assert_eq!(feedback.visual.accent, crate::overlay::theme::ACCENT);
    assert!(!feedback.update(&state, true, now + Duration::from_secs(2)));
  }

  #[test]
  fn interrupted_snap_restarts_flash_without_waiting_for_arrival() {
    let now = Instant::now();
    let mut state = SelectionControllerState::new(800, 600, 0, 0);
    let first = Rect {
      x: 50.0,
      y: 50.0,
      w: 300.0,
      h: 200.0,
    };
    state.snap_anim.update_target(Some(first));
    let mut feedback = Feedback::preview(now);
    feedback.update(&state, true, now);
    assert_eq!(feedback.visual.accent, [255, 154, 154]);
    feedback.update(&state, true, now + Duration::from_millis(40));
    assert!(feedback.visual.accent[1] < 154);
    state
      .snap_anim
      .update_target(Some(Rect { x: 400.0, ..first }));
    feedback.update(&state, true, now + Duration::from_millis(50));
    assert_eq!(feedback.visual.accent, [255, 154, 154]);
    feedback.update(&state, true, now + Duration::from_millis(170));
    assert!(!feedback.active());
    assert_eq!(feedback.visual.accent, crate::overlay::theme::ACCENT);
    // Finishing geometry motion must not start a second flash.
    state.snap_anim.start_time = Some(now - Duration::from_secs(1));
    assert!(!feedback.update(&state, true, now + Duration::from_millis(180)));
    let mut reduced = Feedback::with_enabled(false, now);
    reduced.update(&state, true, now);
    assert!(!reduced.active());
    assert_eq!(reduced.visual.accent, crate::overlay::theme::ACCENT);
  }

  #[test]
  fn idle_draw_move_and_resize_render_identical_eight_handle_frames() {
    use super::super::gdi::bitmap::ScreenshotBitmap;
    use super::super::selection_core::{
      SelectionDrawColors, SharedSelectionOverlayState, draw_selection_scene_with_indicator,
    };
    use windows::Win32::Graphics::Gdi::{GdiFlush, HDC, HFONT};
    let (width, height) = (640, 360);
    let source = std::sync::Arc::new(vec![40; (width * height * 4) as usize]);
    let rect = Rect {
      x: 70.0,
      y: 60.0,
      w: 480.0,
      h: 230.0,
    };
    let now = Instant::now();
    let mut shared = SharedSelectionOverlayState::new(width, height, source.clone(), 0, 0, now);
    shared.resources.screenshot =
      Some(ScreenshotBitmap::from_rgba(&source, width, height).expect("source"));
    let mut reference = None;
    for interaction in [
      SelectionInteraction::Confirmed {
        rect,
        from_snap: false,
        moving: None,
        resizing: None,
      },
      SelectionInteraction::FreeDraw {
        start: (rect.x, rect.y),
        current: (rect.x + rect.w, rect.y + rect.h),
      },
      SelectionInteraction::Confirmed {
        rect,
        from_snap: false,
        moving: Some((10.0, 10.0)),
        resizing: None,
      },
      SelectionInteraction::Confirmed {
        rect,
        from_snap: false,
        moving: None,
        resizing: Some((Handle::SE, rect)),
      },
    ] {
      shared.controller.interaction = interaction;
      shared.resources.border_feedback = Feedback::preview(now);
      // Compare settled chrome with animation enabled, excluding the entry pulse.
      shared.resources.border_feedback.confirmed = true;
      let mut target = ScreenshotBitmap::from_rgba(&source, width, height).expect("target");
      shared.resources.offscreen_dc = target.dc;
      draw_selection_scene_with_indicator(
        &mut shared,
        HFONT::default(),
        SelectionDrawColors::DEFAULT,
        false,
      );
      // SAFETY: flushes this thread's GDI writes before reading its DIB pixels.
      assert!(unsafe { GdiFlush() }.as_bool());
      let pixels = target.to_rgba().expect("pixels");
      if let Some(reference) = &reference {
        assert_eq!(
          &pixels, reference,
          "moving/resizing must preserve the resting frame"
        );
      } else {
        reference = Some(pixels);
      }
      shared.resources.offscreen_dc = HDC::default();
    }
  }
}
