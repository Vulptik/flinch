use std::time::{Duration, Instant};

use crate::overlay::{
  chrome::{Icon, Renderer, Scene, TextStyle},
  gdi::layered::LayeredBitmap,
  theme,
};
use windows::Win32::Foundation::RECT;

const TIMER_RIGHT_PADDING: i32 = 16;

const TIMER_TEXT_STYLE: TextStyle = TextStyle {
  size: 14.0,
  semibold: false,
  leading: false,
};

pub fn recording_pulse_alpha(phase: RecordingPhase, age: Duration) -> u8 {
  if phase != RecordingPhase::Recording {
    return 32;
  }
  let cycle = (age.as_millis() % 2800) as f64 / 2800.0;
  (38.0 - 16.0 * (cycle * std::f64::consts::TAU).cos()).round() as u8
}

fn status_text_style(phase: RecordingPhase) -> TextStyle {
  if matches!(phase, RecordingPhase::Recording | RecordingPhase::Paused) {
    TIMER_TEXT_STYLE
  } else {
    TextStyle::LABEL
  }
}

pub const TOOLBAR_W: i32 = 428;
pub const TOOLBAR_H: i32 = 38;
pub const TIMER_SECTION_W: i32 = 238;
pub const PAUSE_SECTION_W: i32 = 112;
pub const STOP_SECTION_W: i32 = 78;
pub const BTN_TOP_OFFSET: i32 = 4;
pub const BTN_H: i32 = 30;
/// Concentric with the panel's corner, since the buttons sit BTN_TOP_OFFSET inside it.
pub const BTN_RADIUS: i32 = 10;
pub const TOOLBAR_GAP: i32 = 6;
pub const TOOLBAR_MARGIN: i32 = 4;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum HoverTarget {
  None,
  Drag,
  Microphone,
  Pause,
  Stop,
}

/// The microphone button's section: an icon-only square beside Pause.
pub const MICROPHONE_SECTION_W: i32 = 40;
pub use crate::video::controller::MicrophoneState;

#[derive(Clone, Copy)]
pub struct PauseState {
  pub is_paused: bool,
  pub total_paused: Duration,
  pub pause_started_at: Option<Instant>,
}

impl Default for PauseState {
  fn default() -> Self {
    Self {
      is_paused: false,
      total_paused: Duration::ZERO,
      pause_started_at: None,
    }
  }
}

impl PauseState {
  pub fn set_paused(&mut self, paused: bool, now: Instant) -> bool {
    if self.is_paused == paused {
      return false;
    }

    self.is_paused = paused;
    if paused {
      self.pause_started_at = Some(now);
    } else if let Some(started) = self.pause_started_at.take() {
      self.total_paused += now.saturating_duration_since(started);
    }

    true
  }

  pub fn sync_from_flag(&mut self, paused: bool, now: Instant) -> bool {
    self.set_paused(paused, now)
  }

  pub fn elapsed_since(&self, start_time: Instant, now: Instant) -> Duration {
    let current_pause = if self.is_paused {
      self
        .pause_started_at
        .map(|started| now.saturating_duration_since(started))
        .unwrap_or_default()
    } else {
      Duration::ZERO
    };

    now
      .saturating_duration_since(start_time)
      .saturating_sub(self.total_paused + current_pause)
  }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TimerTextStyle {
  Hms,
  Gif { limit_secs: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingPhase {
  Starting,
  Recording,
  Paused,
  Pausing,
  Resuming,
}

impl RecordingPhase {
  pub fn can_pause(self) -> bool {
    matches!(self, Self::Recording | Self::Paused)
  }
}

#[derive(Clone, Copy)]
pub struct ToolbarMetrics {
  pub width: i32,
  pub height: i32,
  pub timer_width: i32,
  pub pause_width: i32,
  pub stop_width: i32,
  /// The microphone button's section; 0 when the recording has no microphone.
  pub microphone_width: i32,
  pub button_top: i32,
  pub button_height: i32,
  pub button_radius: i32,
  pub divider_top_inset: i32,
  pub divider_bottom_inset: i32,
  pub dot_offset_x: i32,
  pub timer_text_offset_x: i32,
}

pub const TOOLBAR_METRICS: ToolbarMetrics = ToolbarMetrics {
  width: TOOLBAR_W,
  height: TOOLBAR_H,
  timer_width: TIMER_SECTION_W,
  pause_width: PAUSE_SECTION_W,
  stop_width: STOP_SECTION_W,
  microphone_width: 0,
  button_top: BTN_TOP_OFFSET,
  button_height: BTN_H,
  button_radius: BTN_RADIUS,
  divider_top_inset: 7,
  divider_bottom_inset: 7,
  dot_offset_x: 32,
  timer_text_offset_x: 48,
};

/// Measure the active copy, keeping action widths stable across Pause/Resume.
/// Changes to status width extend to the left so action targets stay in place.
pub fn fitted_metrics(
  renderer: &mut Renderer,
  lang: &str,
  phase: RecordingPhase,
  elapsed: Duration,
  style: TimerTextStyle,
) -> anyhow::Result<ToolbarMetrics> {
  let measure = |renderer: &mut Renderer, text: &str| -> anyhow::Result<i32> {
    Ok(renderer.measure_text(text, TextStyle::LABEL)?.ceil() as i32)
  };
  let labels = crate::i18n::recording_controls(lang);
  let pause_width = measure(renderer, labels.pause)?.max(measure(renderer, labels.resume)?) + 48;
  let stop_width = measure(renderer, labels.stop)? + 38;
  let sample_secs = if elapsed.as_secs() < 3600 {
    3599
  } else {
    elapsed.as_secs().max(359999)
  };
  let sample = Duration::from_secs(sample_secs);
  let mut text_width = renderer
    .measure_text(
      &recording_status_text(lang, phase, elapsed, style)?,
      status_text_style(phase),
    )?
    .ceil() as i32;
  for state in [RecordingPhase::Recording, RecordingPhase::Paused] {
    text_width = text_width.max(
      renderer
        .measure_text(
          &recording_status_text(lang, state, sample, style)?,
          status_text_style(state),
        )?
        .ceil() as i32,
    );
  }
  let timer_width = 56 + text_width + TIMER_RIGHT_PADDING;
  Ok(ToolbarMetrics {
    width: timer_width + pause_width + stop_width,
    timer_width,
    pause_width,
    stop_width,
    dot_offset_x: 40,
    timer_text_offset_x: 56,
    ..TOOLBAR_METRICS
  })
}

/// The same metrics with the microphone button's section added (or removed).
pub fn with_microphone(metrics: ToolbarMetrics, microphone: bool) -> ToolbarMetrics {
  let width = if microphone { MICROPHONE_SECTION_W } else { 0 };
  ToolbarMetrics {
    width: metrics.width - metrics.microphone_width + width,
    microphone_width: width,
    ..metrics
  }
}

#[derive(Clone, Copy)]
pub struct ToolbarLayout {
  pub background_rect: RECT,
  pub drag_rect: RECT,
  /// The microphone button, when the recording has one.
  pub microphone_rect: Option<RECT>,
  pub pause_rect: RECT,
  pub stop_rect: RECT,
  pub timer_rect: RECT,
  pub divider_x: i32,
  pub dot_x: i32,
  pub dot_y: i32,
}

pub fn toolbar_layout(metrics: ToolbarMetrics, origin_x: i32, origin_y: i32) -> ToolbarLayout {
  let background_rect = RECT {
    left: origin_x,
    top: origin_y,
    right: origin_x + metrics.width,
    bottom: origin_y + metrics.height,
  };
  let actions = origin_x + metrics.timer_width + metrics.microphone_width;
  let microphone_rect = (metrics.microphone_width > 0).then(|| RECT {
    left: origin_x + metrics.timer_width + 4,
    top: origin_y + metrics.button_top,
    right: actions - 2,
    bottom: origin_y + metrics.button_top + metrics.button_height,
  });
  let pause_rect = RECT {
    left: actions + 4,
    top: origin_y + metrics.button_top,
    right: actions + metrics.pause_width - 4,
    bottom: origin_y + metrics.button_top + metrics.button_height,
  };
  let stop_rect = RECT {
    left: actions + metrics.pause_width + 4,
    top: origin_y + metrics.button_top,
    right: actions + metrics.pause_width + metrics.stop_width - 4,
    bottom: origin_y + metrics.button_top + metrics.button_height,
  };
  let timer_rect = RECT {
    left: origin_x + metrics.timer_text_offset_x,
    top: origin_y,
    right: origin_x + metrics.timer_width - TIMER_RIGHT_PADDING,
    bottom: origin_y + metrics.height,
  };

  ToolbarLayout {
    background_rect,
    drag_rect: RECT {
      right: origin_x + 32,
      ..background_rect
    },
    microphone_rect,
    pause_rect,
    stop_rect,
    timer_rect,
    divider_x: origin_x + metrics.timer_width,
    dot_x: origin_x + metrics.dot_offset_x,
    dot_y: origin_y + (metrics.height - 8) / 2,
  }
}

pub fn point_in_rect(x: i32, y: i32, rect: RECT) -> bool {
  x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

pub fn hover_from_point(x: i32, y: i32, pause_rect: RECT, stop_rect: RECT) -> HoverTarget {
  if point_in_rect(x, y, pause_rect) {
    HoverTarget::Pause
  } else if point_in_rect(x, y, stop_rect) {
    HoverTarget::Stop
  } else {
    HoverTarget::None
  }
}

pub fn toolbar_hover(x: i32, y: i32, layout: ToolbarLayout) -> HoverTarget {
  if point_in_rect(x, y, layout.drag_rect) {
    HoverTarget::Drag
  } else if layout
    .microphone_rect
    .is_some_and(|rect| point_in_rect(x, y, rect))
  {
    HoverTarget::Microphone
  } else {
    hover_from_point(x, y, layout.pause_rect, layout.stop_rect)
  }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn draw_toolbar(
  bmp: &mut LayeredBitmap,
  renderer: &mut Renderer,
  layout: ToolbarLayout,
  metrics: ToolbarMetrics,
  hover: HoverTarget,
  is_paused: bool,
  elapsed: Duration,
  timer_style: TimerTextStyle,
  lang: &str,
) -> anyhow::Result<()> {
  draw_toolbar_state(
    bmp,
    renderer,
    layout,
    metrics,
    hover,
    is_paused,
    elapsed,
    timer_style,
    lang,
    if is_paused {
      RecordingPhase::Paused
    } else {
      RecordingPhase::Recording
    },
    32,
    HoverTarget::None,
  )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub fn draw_toolbar_state(
  bmp: &mut LayeredBitmap,
  renderer: &mut Renderer,
  layout: ToolbarLayout,
  metrics: ToolbarMetrics,
  hover: HoverTarget,
  is_paused: bool,
  elapsed: Duration,
  timer_style: TimerTextStyle,
  lang: &str,
  phase: RecordingPhase,
  pulse_alpha: u8,
  pressed: HoverTarget,
) -> anyhow::Result<()> {
  draw_toolbar_animated(
    bmp,
    renderer,
    layout,
    metrics,
    hover,
    is_paused,
    elapsed,
    timer_style,
    lang,
    phase,
    pulse_alpha,
    pressed,
    ToolbarAnimation {
      opacity: 255,
      hover: [
        u8::from(hover == HoverTarget::Drag) * 255,
        u8::from(hover == HoverTarget::Pause) * 255,
        u8::from(hover == HoverTarget::Stop) * 255,
        u8::from(hover == HoverTarget::Microphone) * 255,
      ],
      dot_color: status_dot_color(phase),
      microphone: None,
    },
  )
}

pub struct ToolbarAnimation {
  pub opacity: u8,
  /// Drag, Pause, Stop and Microphone hover alphas.
  pub hover: [u8; 4],
  pub dot_color: [u8; 3],
  /// The microphone button's state; `None` draws no button.
  pub microphone: Option<MicrophoneState>,
}

pub fn status_dot_color(phase: RecordingPhase) -> [u8; 3] {
  match phase {
    RecordingPhase::Recording => theme::ACTIVE_ICON,
    RecordingPhase::Paused | RecordingPhase::Pausing => [245, 184, 76],
    RecordingPhase::Starting | RecordingPhase::Resuming => theme::SECONDARY_TEXT,
  }
}

#[allow(clippy::too_many_arguments)]
pub fn draw_toolbar_animated(
  bmp: &mut LayeredBitmap,
  renderer: &mut Renderer,
  layout: ToolbarLayout,
  metrics: ToolbarMetrics,
  hover: HoverTarget,
  is_paused: bool,
  elapsed: Duration,
  timer_style: TimerTextStyle,
  lang: &str,
  phase: RecordingPhase,
  pulse_alpha: u8,
  pressed: HoverTarget,
  animation: ToolbarAnimation,
) -> anyhow::Result<()> {
  let mut scene = Scene::default();
  scene.panel(layout.background_rect)?;
  if animation.hover[0] > 0 {
    scene.rounded(
      RECT {
        left: layout.drag_rect.left + 4,
        right: layout.drag_rect.right - 2,
        top: layout.drag_rect.top + metrics.button_top,
        bottom: layout.drag_rect.bottom - metrics.button_top,
      },
      metrics.button_radius as f32,
      theme::HOVER,
      animation.hover[0],
    )?;
  }
  // The drag handle shares the screenshot toolbar's icon, centered where its dots sat.
  scene.icon(
    RECT {
      left: layout.background_rect.left + 7,
      right: layout.background_rect.left + 27,
      ..layout.background_rect
    },
    Icon::Grip,
    theme::MUTED,
  )?;
  // Only halo opacity breathes; the core stays visible and paused states stay still.
  let dot_color = animation.dot_color;
  scene.rounded(
    RECT {
      left: layout.dot_x - 3,
      top: layout.dot_y - 3,
      right: layout.dot_x + 11,
      bottom: layout.dot_y + 11,
    },
    7.0,
    dot_color,
    pulse_alpha,
  )?;
  scene.rounded(
    RECT {
      left: layout.dot_x,
      top: layout.dot_y,
      right: layout.dot_x + 8,
      bottom: layout.dot_y + 8,
    },
    4.0,
    dot_color,
    255,
  )?;
  let timer_text = recording_status_text(lang, phase, elapsed, timer_style)?;
  scene.styled_text(
    layout.timer_rect,
    &timer_text,
    status_text_style(phase),
    theme::TEXT,
  )?;
  if let TimerTextStyle::Gif { limit_secs } = timer_style {
    let width = (layout.timer_rect.right - layout.timer_rect.left).max(0);
    let filled = (f64::from(width)
      * (elapsed.as_secs_f64() / f64::from(limit_secs.max(1))).clamp(0.0, 1.0))
      as i32;
    let track = RECT {
      left: layout.timer_rect.left,
      right: layout.timer_rect.left + width,
      top: layout.background_rect.bottom - 5,
      bottom: layout.background_rect.bottom - 3,
    };
    scene.fill(track, theme::SEPARATOR)?;
    if filled > 0 {
      scene.fill(
        RECT {
          right: track.left + filled,
          ..track
        },
        if is_paused {
          theme::MUTED
        } else {
          theme::ACCENT
        },
      )?;
    }
  }
  scene.fill(
    RECT {
      left: layout.divider_x,
      right: layout.divider_x + 1,
      top: layout.background_rect.top + metrics.divider_top_inset,
      bottom: layout.background_rect.bottom - metrics.divider_bottom_inset,
    },
    theme::SEPARATOR,
  )?;
  if let (Some(rect), Some(state)) = (layout.microphone_rect, animation.microphone) {
    // A lost microphone cannot switch; a muted one shows its crossed icon in the accent color.
    let enabled = state != MicrophoneState::Lost && phase.can_pause();
    let pushed = pressed == HoverTarget::Microphone && enabled;
    let hover_alpha = animation.hover[3];
    if (hover_alpha > 0 && enabled) || pushed {
      scene.rounded(
        rect,
        metrics.button_radius as f32,
        if pushed {
          theme::SEPARATOR
        } else {
          theme::HOVER
        },
        if pushed { 255 } else { hover_alpha },
      )?;
    }
    let (icon, color) = match state {
      MicrophoneState::Audible => (Icon::Microphone, theme::TEXT),
      MicrophoneState::Muted => (Icon::MicrophoneMuted, theme::ACTIVE_ICON),
      MicrophoneState::Lost => (Icon::MicrophoneMuted, theme::MUTED),
    };
    let left = rect.left + (rect.right - rect.left - 20) / 2;
    scene.icon(
      RECT {
        left,
        right: left + 20,
        ..rect
      },
      icon,
      if enabled { color } else { theme::MUTED },
    )?;
  }
  let labels = crate::i18n::recording_controls(lang);
  for (rect, icon, label, _hovered, target) in [
    (
      layout.pause_rect,
      if is_paused { Icon::Resume } else { Icon::Pause },
      if is_paused {
        labels.resume
      } else {
        labels.pause
      },
      hover == HoverTarget::Pause,
      HoverTarget::Pause,
    ),
    (
      layout.stop_rect,
      Icon::Stop,
      labels.stop,
      hover == HoverTarget::Stop,
      HoverTarget::Stop,
    ),
  ] {
    let enabled = icon == Icon::Stop || phase.can_pause();
    let pushed = pressed == target && enabled;
    let hover_alpha = animation.hover[if icon == Icon::Stop { 2 } else { 1 }];
    if (hover_alpha > 0 && enabled) || pushed {
      scene.rounded(
        rect,
        metrics.button_radius as f32,
        if icon == Icon::Stop {
          if pushed { [96, 44, 50] } else { theme::ACTIVE }
        } else if pushed {
          theme::SEPARATOR
        } else {
          theme::HOVER
        },
        if pushed { 255 } else { hover_alpha },
      )?;
    }
    let color = if icon != Icon::Stop && !phase.can_pause() {
      theme::MUTED
    } else if icon == Icon::Stop {
      crate::overlay::motion::blend_rgb(
        theme::TEXT,
        theme::ACTIVE_ICON,
        if pushed { 255 } else { hover_alpha },
      )
    } else {
      theme::TEXT
    };
    // Center icon + label as one group; measure through the same DirectWrite format.
    let text_width = renderer.measure_text(label, TextStyle::LABEL)?.ceil() as i32;
    let available = rect.right - rect.left - 26;
    let text_width = text_width.min(available.max(1));
    let left = rect.left + (rect.right - rect.left - text_width - 24) / 2;
    scene.icon(
      RECT {
        left,
        right: left + 20,
        ..rect
      },
      icon,
      color,
    )?;
    scene.text(
      RECT {
        left: left + 24,
        right: left + 24 + text_width,
        ..rect
      },
      label,
      color,
    )?;
  }
  renderer.render_with_opacity(bmp.dc, layout.background_rect, &scene, animation.opacity)
}
pub fn recording_status_text(
  lang: &str,
  phase: RecordingPhase,
  elapsed: Duration,
  style: TimerTextStyle,
) -> anyhow::Result<String> {
  let time = format_elapsed_hms(elapsed);
  let timer = match style {
    TimerTextStyle::Gif { limit_secs } => format!(
      "{time} / {}",
      format_elapsed_hms(Duration::from_secs(u64::from(limit_secs)))
    ),
    TimerTextStyle::Hms => time,
  };
  let key = match phase {
    RecordingPhase::Starting => "recording_starting",
    RecordingPhase::Pausing => "recording_pausing",
    RecordingPhase::Resuming => "recording_resuming",
    RecordingPhase::Paused | RecordingPhase::Recording => return Ok(timer),
  };
  let label = crate::i18n::native_settings(lang, key).map_err(anyhow::Error::msg)?;
  Ok(label)
}

pub fn format_elapsed_hms(elapsed: Duration) -> String {
  let secs = elapsed.as_secs();
  if secs < 3600 {
    return format!("{:02}:{:02}", secs / 60, secs % 60);
  }
  format!(
    "{:02}:{:02}:{:02}",
    secs / 3600,
    (secs % 3600) / 60,
    secs % 60
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[ignore = "explicit offscreen animated recording controls preview"]
  fn render_recording_motion_preview() {
    use crate::overlay::motion::{Motion, blend_rgb};
    use image::codecs::gif::{GifEncoder, Repeat};
    let output = std::path::PathBuf::from(
      std::env::var("FLINCH_RECORDING_PREVIEW_OUTPUT").expect("output directory"),
    );
    std::fs::create_dir_all(&output).expect("output");
    let mut encoder = GifEncoder::new_with_speed(
      std::fs::File::create(output.join("recording-motion.gif")).expect("GIF"),
      30,
    );
    encoder.set_repeat(Repeat::Infinite).expect("repeat");
    let mut renderers = [Renderer::default(), Renderer::default()];
    for frame in 0..120 {
      let phase = frame / 30;
      let age = Duration::from_millis((frame % 30) * 16);
      let hold = Duration::from_secs(1);
      let hover = if phase == 1 {
        HoverTarget::Pause
      } else if phase == 3 {
        HoverTarget::Stop
      } else {
        HoverTarget::None
      };
      let paused = phase >= 2;
      let state = if paused {
        RecordingPhase::Paused
      } else {
        RecordingPhase::Recording
      };
      let motion = Motion::preview(
        if phase == 0 { age } else { hold },
        if phase == 2 { age } else { hold },
        age,
        state as u8 + 1,
        match hover {
          HoverTarget::Pause => Some(1),
          HoverTarget::Stop => Some(2),
          _ => None,
        },
      );
      let mut bitmap = LayeredBitmap::new(520, 170).expect("canvas");
      bitmap.fill_rect(0, 0, 520, 170, 28, 28, 28, 255);
      for (index, style) in [TimerTextStyle::Hms, TimerTextStyle::Gif { limit_secs: 15 }]
        .into_iter()
        .enumerate()
      {
        let renderer = &mut renderers[index];
        let metrics =
          fitted_metrics(renderer, "en", state, Duration::from_secs(12), style).expect("measure");
        draw_toolbar_animated(
          &mut bitmap,
          renderer,
          toolbar_layout(metrics, 20, 20 + index as i32 * 74),
          metrics,
          hover,
          paused,
          Duration::from_secs(12),
          style,
          "en",
          state,
          32,
          HoverTarget::None,
          ToolbarAnimation {
            opacity: motion.toolbar_alpha(Instant::now()),
            hover: [
              0,
              motion.hover_alpha(1, Instant::now()),
              motion.hover_alpha(2, Instant::now()),
              0,
            ],
            microphone: None,
            dot_color: if phase == 2 {
              blend_rgb(
                status_dot_color(RecordingPhase::Recording),
                status_dot_color(state),
                motion.popup_alpha(Instant::now()),
              )
            } else {
              status_dot_color(state)
            },
          },
        )
        .expect("render animated HUD");
      }
      // SAFETY: flush owner-thread GDI writes before reading the owned bitmap.
      assert!(unsafe { windows::Win32::Graphics::Gdi::GdiFlush() }.as_bool());
      let mut pixels = bitmap.pixels_mut().to_vec();
      for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
      }
      if frame == 119 {
        image::save_buffer(
          output.join("recording-motion-settled.png"),
          &pixels,
          520,
          170,
          image::ColorType::Rgba8,
        )
        .expect("PNG");
      }
      encoder
        .encode_frame(image::Frame::from_parts(
          image::RgbaImage::from_raw(520, 170, pixels).expect("RGBA"),
          0,
          0,
          image::Delay::from_numer_denom_ms(16, 1),
        ))
        .expect("GIF frame");
    }
    for renderer in renderers {
      assert!(
        renderer.surface_misses <= 2,
        "transitions reuse HUD surfaces"
      );
    }
  }

  #[test]
  fn recording_pulse_stays_subtle_and_paused_timer_stays_identical() {
    let mut lowest = u8::MAX;
    let mut highest = 0;
    for millis in (0..5600).step_by(200) {
      let age = Duration::from_millis(millis);
      let alpha = recording_pulse_alpha(RecordingPhase::Recording, age);
      lowest = lowest.min(alpha);
      highest = highest.max(alpha);
      assert_eq!(recording_pulse_alpha(RecordingPhase::Paused, age), 32);
      assert!((22..=54).contains(&alpha));
    }
    assert!(highest > lowest);
    for lang in ["en", "fr", "de", "es", "it"] {
      for style in [TimerTextStyle::Hms, TimerTextStyle::Gif { limit_secs: 15 }] {
        let active = recording_status_text(
          lang,
          RecordingPhase::Recording,
          Duration::from_secs(12),
          style,
        )
        .expect("active timer");
        let paused =
          recording_status_text(lang, RecordingPhase::Paused, Duration::from_secs(12), style)
            .expect("paused timer");
        assert_eq!(active, paused);
        assert!(active.starts_with("00:12"));
      }
    }
  }

  #[test]
  fn hour_timers_fit_with_explicit_right_padding() {
    let mut renderer = Renderer::default();
    for lang in ["en", "fr", "de", "es", "it"] {
      for seconds in [3599, 3600, 359999, 360000] {
        for style in [
          TimerTextStyle::Hms,
          TimerTextStyle::Gif { limit_secs: 7200 },
        ] {
          let elapsed = Duration::from_secs(seconds);
          let metrics = fitted_metrics(
            &mut renderer,
            lang,
            RecordingPhase::Recording,
            elapsed,
            style,
          )
          .expect("hour layout");
          let layout = toolbar_layout(metrics, 0, 0);
          let text = recording_status_text(lang, RecordingPhase::Recording, elapsed, style)
            .expect("hour timer");
          let width = renderer
            .measure_text(&text, TIMER_TEXT_STYLE)
            .expect("hour text width");
          assert!(width <= (layout.timer_rect.right - layout.timer_rect.left) as f32);
          assert_eq!(
            layout.divider_x - layout.timer_rect.right,
            TIMER_RIGHT_PADDING
          );
        }
      }
    }
  }

  #[test]
  fn fitted_controls_are_compact_and_pause_targets_stay_stable() {
    let mut renderer = Renderer::default();
    for lang in ["en", "fr", "de", "es", "it"] {
      let active = fitted_metrics(
        &mut renderer,
        lang,
        RecordingPhase::Recording,
        Duration::from_secs(12),
        TimerTextStyle::Hms,
      )
      .expect("active layout");
      let paused = fitted_metrics(
        &mut renderer,
        lang,
        RecordingPhase::Paused,
        Duration::from_secs(12),
        TimerTextStyle::Hms,
      )
      .expect("paused layout");
      assert_eq!(active.width, paused.width);
      let labels = crate::i18n::recording_controls(lang);
      for (label, width) in [
        (labels.pause, active.pause_width),
        (labels.resume, active.pause_width),
        (labels.stop, active.stop_width),
      ] {
        assert!(
          renderer
            .measure_text(label, TextStyle::LABEL)
            .expect("label width")
            <= (width - 28) as f32
        );
      }
      if lang == "en" {
        assert!(active.width < 310, "English video bar: {}", active.width);
        assert!(active.pause_width < 95);
        assert!(active.stop_width < 75);
        let gif = fitted_metrics(
          &mut renderer,
          lang,
          RecordingPhase::Recording,
          Duration::from_secs(12),
          TimerTextStyle::Gif { limit_secs: 15 },
        )
        .expect("GIF layout");
        assert!(gif.width < 360, "English GIF bar: {}", gif.width);
      }
    }
  }

  #[test]
  fn status_labels_fit_in_every_locale_and_phase() {
    let mut renderer = Renderer::default();
    for lang in ["en", "fr", "de", "es", "it"] {
      for phase in [
        RecordingPhase::Starting,
        RecordingPhase::Recording,
        RecordingPhase::Paused,
        RecordingPhase::Pausing,
        RecordingPhase::Resuming,
      ] {
        for style in [TimerTextStyle::Hms, TimerTextStyle::Gif { limit_secs: 15 }] {
          let metrics = fitted_metrics(&mut renderer, lang, phase, Duration::from_secs(12), style)
            .expect("measured layout");
          let layout = toolbar_layout(metrics, 0, 0);
          let text = recording_status_text(lang, phase, Duration::from_secs(12), style)
            .expect("status copy");
          let width = renderer
            .measure_text(&text, status_text_style(phase))
            .expect("status measurement");
          assert!(
            width <= (layout.timer_rect.right - layout.timer_rect.left) as f32,
            "{lang}: {text} does not fit ({width})"
          );
        }
      }
    }
    assert_eq!(format_elapsed_hms(Duration::from_secs(12)), "00:12");
    assert_eq!(format_elapsed_hms(Duration::from_secs(3723)), "01:02:03");
    assert!(!RecordingPhase::Starting.can_pause());
    assert!(!RecordingPhase::Pausing.can_pause());
    assert!(!RecordingPhase::Resuming.can_pause());
  }

  #[test]
  fn buttons_and_grip_highlight_sit_evenly_inside_the_panel_so_their_corners_follow_it() {
    let layout = toolbar_layout(TOOLBAR_METRICS, 100, 200);
    let panel = layout.background_rect;
    let gap = BTN_TOP_OFFSET;
    assert_eq!(layout.stop_rect.top - panel.top, gap);
    assert_eq!(panel.bottom - layout.stop_rect.bottom, gap);
    assert_eq!(
      panel.right - layout.stop_rect.right,
      gap,
      "Stop sits at the edge"
    );
    assert_eq!(
      BTN_RADIUS as f32,
      crate::overlay::chrome::inner_radius(gap as f32)
    );
  }

  #[test]
  fn grip_and_buttons_have_separate_hit_regions() {
    let layout = toolbar_layout(TOOLBAR_METRICS, 100, 200);
    assert_eq!(toolbar_hover(110, 220, layout), HoverTarget::Drag);
    assert_eq!(toolbar_hover(160, 220, layout), HoverTarget::None);
    assert_eq!(
      toolbar_hover(layout.pause_rect.left, 220, layout),
      HoverTarget::Pause
    );
    assert_eq!(
      toolbar_hover(layout.stop_rect.left, 220, layout),
      HoverTarget::Stop
    );
    assert_eq!(toolbar_hover(99, 220, layout), HoverTarget::None);
  }

  #[test]
  fn pause_state_excludes_completed_and_current_pauses() {
    let start = Instant::now();
    let mut state = PauseState::default();

    state.set_paused(true, start + Duration::from_secs(4));
    state.set_paused(false, start + Duration::from_secs(7));
    state.set_paused(true, start + Duration::from_secs(9));

    let elapsed = state.elapsed_since(start, start + Duration::from_secs(12));
    assert_eq!(elapsed, Duration::from_secs(6));
  }

  #[test]
  fn recording_chrome_is_transparent_and_localized_labels_fit() {
    let layout = toolbar_layout(TOOLBAR_METRICS, 0, 0);
    let mut renderer = Renderer::default();
    let mut bitmap = LayeredBitmap::new(TOOLBAR_W, TOOLBAR_H).expect("toolbar");
    for lang in ["en", "fr", "de", "es", "it"] {
      let metrics = fitted_metrics(
        &mut renderer,
        lang,
        RecordingPhase::Recording,
        Duration::from_secs(3723),
        TimerTextStyle::Hms,
      )
      .expect("localized layout");
      let measured_layout = toolbar_layout(metrics, 0, 0);
      let labels = crate::i18n::recording_controls(lang);
      for (label, rect) in [
        (labels.pause, measured_layout.pause_rect),
        (labels.resume, measured_layout.pause_rect),
        (labels.stop, measured_layout.stop_rect),
      ] {
        let width = renderer
          .measure_text(label, TextStyle::LABEL)
          .expect("measure label");
        assert!(
          width <= (rect.right - rect.left - 28) as f32,
          "{lang}: {label} must fit beside its icon"
        );
      }
      for paused in [false, true] {
        bitmap.clear();
        draw_toolbar(
          &mut bitmap,
          &mut renderer,
          layout,
          TOOLBAR_METRICS,
          HoverTarget::Pause,
          paused,
          Duration::from_secs(3723),
          TimerTextStyle::Hms,
          lang,
        )
        .expect("render controls");
        let pixels = bitmap.pixels_mut();
        assert_eq!(&pixels[..4], &[0, 0, 0, 0]);
        assert!(
          pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[..3].iter().all(|channel| *channel <= p[3]))
        );
        assert!(
          pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[3] > 0 && p[3] < 255)
        );
      }
    }
    assert_eq!(renderer.surface_misses, 1);
    // Grip, pause, resume and stop.
    assert_eq!(renderer.icon_misses, 4);
    assert_eq!(layout.pause_rect.top, 4);
    assert_eq!(TOOLBAR_H - layout.pause_rect.bottom, 4);
  }

  #[test]
  #[ignore = "explicit native recording control previews"]
  fn render_recording_previews() {
    let output = std::path::PathBuf::from(
      std::env::var("FLINCH_RECORDING_PREVIEW_OUTPUT").expect("output directory"),
    );
    std::fs::create_dir_all(&output).expect("create output");
    let mut renderer = Renderer::default();
    for lang in ["en", "fr", "de", "es", "it"] {
      for (case, paused, hover, timer) in [
        ("starting", false, HoverTarget::None, TimerTextStyle::Hms),
        ("pausing", false, HoverTarget::None, TimerTextStyle::Hms),
        ("resuming", true, HoverTarget::None, TimerTextStyle::Hms),
        ("drag-hover", false, HoverTarget::Drag, TimerTextStyle::Hms),
        (
          "gif-paused",
          true,
          HoverTarget::None,
          TimerTextStyle::Gif { limit_secs: 15 },
        ),
        ("recording", false, HoverTarget::None, TimerTextStyle::Hms),
        ("hours", false, HoverTarget::None, TimerTextStyle::Hms),
        (
          "gif-hours",
          false,
          HoverTarget::None,
          TimerTextStyle::Gif { limit_secs: 7200 },
        ),
        (
          "pause-hover",
          false,
          HoverTarget::Pause,
          TimerTextStyle::Hms,
        ),
        ("paused", true, HoverTarget::None, TimerTextStyle::Hms),
        (
          "resume-hover",
          true,
          HoverTarget::Pause,
          TimerTextStyle::Hms,
        ),
        ("stop-hover", false, HoverTarget::Stop, TimerTextStyle::Hms),
        (
          "gif",
          false,
          HoverTarget::None,
          TimerTextStyle::Gif { limit_secs: 15 },
        ),
      ] {
        let phase = match case {
          "starting" => RecordingPhase::Starting,
          "pausing" => RecordingPhase::Pausing,
          "resuming" => RecordingPhase::Resuming,
          _ if paused => RecordingPhase::Paused,
          _ => RecordingPhase::Recording,
        };
        let elapsed = Duration::from_secs(if case.ends_with("hours") { 3723 } else { 12 });
        let metrics =
          fitted_metrics(&mut renderer, lang, phase, elapsed, timer).expect("measure controls");
        let mut bitmap = LayeredBitmap::new(metrics.width, metrics.height).expect("toolbar");
        draw_toolbar_state(
          &mut bitmap,
          &mut renderer,
          toolbar_layout(metrics, 0, 0),
          metrics,
          hover,
          paused,
          elapsed,
          timer,
          lang,
          phase,
          recording_pulse_alpha(phase, Duration::from_millis(1400)),
          HoverTarget::None,
        )
        .expect("render toolbar");
        let mut rgba = vec![0u8; (metrics.width * metrics.height * 4) as usize];
        for (dst, src) in rgba
          .as_chunks_mut::<4>()
          .0
          .iter_mut()
          .zip(bitmap.pixels_mut().as_chunks::<4>().0)
        {
          let inverse = 255 - u16::from(src[3]);
          for (d, s) in [(0, 2), (1, 1), (2, 0)] {
            dst[d] = (u16::from(src[s]) + 28 * inverse / 255).min(255) as u8;
          }
          dst[3] = 255;
        }
        image::save_buffer(
          output.join(format!("recording-{lang}-{case}.png")),
          &rgba,
          metrics.width as u32,
          metrics.height as u32,
          image::ColorType::Rgba8,
        )
        .expect("save preview");
      }
    }
  }

  #[test]
  fn toolbar_layout_produces_expected_button_rects() {
    let layout = toolbar_layout(TOOLBAR_METRICS, 100, 200);
    assert_eq!(layout.pause_rect.left, 342);
    assert_eq!(layout.pause_rect.right, 446);
    assert_eq!(layout.stop_rect.left, 454);
    assert_eq!(layout.stop_rect.right, 524);
  }

  #[test]
  fn hover_resolution_prefers_pause_then_stop_then_none() {
    let layout = toolbar_layout(TOOLBAR_METRICS, 0, 0);
    assert_eq!(
      hover_from_point(
        layout.pause_rect.left + 1,
        layout.pause_rect.top + 1,
        layout.pause_rect,
        layout.stop_rect
      ),
      HoverTarget::Pause
    );
    assert_eq!(
      hover_from_point(
        layout.stop_rect.left + 1,
        layout.stop_rect.top + 1,
        layout.pause_rect,
        layout.stop_rect
      ),
      HoverTarget::Stop
    );
    assert_eq!(
      hover_from_point(1, 1, layout.pause_rect, layout.stop_rect),
      HoverTarget::None
    );
  }
}

#[cfg(test)]
mod microphone_tests {
  use super::*;

  /// Every microphone state draws its button: hovered, pressed, or disabled while lost.
  #[test]
  fn the_microphone_button_draws_each_state_hovered_or_pressed() {
    let metrics = with_microphone(TOOLBAR_METRICS, true);
    let layout = toolbar_layout(metrics, 0, 0);
    let mic = layout.microphone_rect.expect("microphone button");
    let mut renderer = Renderer::default();
    let mut bitmap = LayeredBitmap::new(metrics.width, TOOLBAR_H).expect("toolbar");
    let mut drawn = Vec::new();
    for state in [
      MicrophoneState::Audible,
      MicrophoneState::Muted,
      MicrophoneState::Lost,
    ] {
      for (pressed, hover) in [
        (HoverTarget::None, 0),
        (HoverTarget::None, 200),
        (HoverTarget::Microphone, 0),
      ] {
        bitmap.clear();
        draw_toolbar_animated(
          &mut bitmap,
          &mut renderer,
          layout,
          metrics,
          HoverTarget::None,
          false,
          Duration::from_secs(5),
          TimerTextStyle::Hms,
          "en",
          RecordingPhase::Recording,
          120,
          pressed,
          ToolbarAnimation {
            opacity: 255,
            hover: [0, 0, 0, hover],
            dot_color: status_dot_color(RecordingPhase::Recording),
            microphone: Some(state),
          },
        )
        .expect("render controls");
        let width = metrics.width as usize;
        let pixels = bitmap.pixels_mut();
        let button: Vec<u8> = (mic.top..mic.bottom)
          .flat_map(|y| {
            let row = y as usize * width * 4;
            pixels[row + mic.left as usize * 4..row + mic.right as usize * 4].to_vec()
          })
          .collect();
        drawn.push(button);
      }
    }
    // Muted and audible differ; a lost microphone ignores hover and presses.
    assert_ne!(drawn[0], drawn[3]);
    assert_ne!(drawn[0], drawn[1]);
    assert_ne!(drawn[0], drawn[2]);
    assert_eq!(drawn[6], drawn[7]);
    assert_eq!(drawn[6], drawn[8]);
  }

  #[test]
  fn the_microphone_button_sits_before_pause_and_moves_the_actions_right() {
    let plain = toolbar_layout(TOOLBAR_METRICS, 0, 0);
    assert!(plain.microphone_rect.is_none());
    let metrics = with_microphone(TOOLBAR_METRICS, true);
    assert_eq!(metrics.width, TOOLBAR_METRICS.width + MICROPHONE_SECTION_W);
    assert_eq!(
      with_microphone(metrics, true).width,
      metrics.width,
      "idempotent"
    );
    assert_eq!(with_microphone(metrics, false).width, TOOLBAR_METRICS.width);
    let layout = toolbar_layout(metrics, 0, 0);
    let mic = layout.microphone_rect.expect("microphone button");
    assert!(mic.left >= metrics.timer_width && mic.right <= layout.pause_rect.left);
    assert_eq!(
      layout.pause_rect.left - plain.pause_rect.left,
      MICROPHONE_SECTION_W
    );
    assert_eq!(
      layout.stop_rect.right - plain.stop_rect.right,
      MICROPHONE_SECTION_W
    );
    let center = ((mic.left + mic.right) / 2, (mic.top + mic.bottom) / 2);
    assert_eq!(
      toolbar_hover(center.0, center.1, layout),
      HoverTarget::Microphone
    );
    assert_eq!(
      toolbar_hover(center.0, center.1, plain),
      HoverTarget::Pause,
      "without a microphone that spot is Pause"
    );
  }
}
