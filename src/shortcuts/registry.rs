use crate::app::context::AppHandle;

use super::handlers::{
  register_active_window_shortcut, register_area_shortcut, register_capture_edit_shortcut,
  register_color_picker_shortcut, register_copy_text_shortcut, register_fullscreen_shortcut,
  register_gif_shortcut, register_last_region_shortcut, register_record_active_window_shortcut,
  register_record_area_shortcut, register_record_fullscreen_shortcut,
};
use super::normalize::normalize_shortcut;
use super::state::ShortcutType;
use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmptyLogLevel {
  Warn,
  Debug,
}

#[derive(Clone, Copy)]
pub(crate) struct ShortcutDefinition {
  pub shortcut_type: ShortcutType,
  pub config_key: &'static str,
  pub human_label: &'static str,
  pub empty_log_level: EmptyLogLevel,
  pub register_fn: fn(&AppHandle, String) -> Result<(), String>,
}

const SHORTCUT_DEFINITIONS: [ShortcutDefinition; 11] = [
  ShortcutDefinition {
    shortcut_type: ShortcutType::ScreenshotArea,
    config_key: "screenshot_area_shortcut",
    human_label: "Screenshot area",
    empty_log_level: EmptyLogLevel::Warn,
    register_fn: register_area_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::ScreenshotFullscreen,
    config_key: "screenshot_fullscreen_shortcut",
    human_label: "Screenshot fullscreen",
    empty_log_level: EmptyLogLevel::Warn,
    register_fn: register_fullscreen_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::ScreenshotActiveWindow,
    config_key: "screenshot_active_window_shortcut",
    human_label: "Screenshot active window",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_active_window_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::RecordArea,
    config_key: "record_area_shortcut",
    human_label: "Record area",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_record_area_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::RecordFullscreen,
    config_key: "record_fullscreen_shortcut",
    human_label: "Record fullscreen",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_record_fullscreen_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::RecordActiveWindow,
    config_key: "record_active_window_shortcut",
    human_label: "Record active window",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_record_active_window_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::Gif,
    config_key: "gif_shortcut",
    human_label: "Record GIF",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_gif_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::ScreenshotEdit,
    config_key: "screenshot_edit_shortcut",
    human_label: "Capture and edit",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_capture_edit_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::CopyText,
    config_key: "copy_text_shortcut",
    human_label: "Copy text",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_copy_text_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::PickColor,
    config_key: "color_picker_shortcut",
    human_label: "Pick color",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_color_picker_shortcut,
  },
  ShortcutDefinition {
    shortcut_type: ShortcutType::LastRegion,
    config_key: "last_region_shortcut",
    human_label: "Capture last region",
    empty_log_level: EmptyLogLevel::Debug,
    register_fn: register_last_region_shortcut,
  },
];

pub(crate) fn shortcut_definitions() -> &'static [ShortcutDefinition] {
  &SHORTCUT_DEFINITIONS
}

pub(crate) fn definition_for(shortcut_type: ShortcutType) -> &'static ShortcutDefinition {
  match shortcut_type {
    ShortcutType::ScreenshotArea => &SHORTCUT_DEFINITIONS[0],
    ShortcutType::ScreenshotFullscreen => &SHORTCUT_DEFINITIONS[1],
    ShortcutType::ScreenshotActiveWindow => &SHORTCUT_DEFINITIONS[2],
    ShortcutType::RecordArea => &SHORTCUT_DEFINITIONS[3],
    ShortcutType::RecordFullscreen => &SHORTCUT_DEFINITIONS[4],
    ShortcutType::RecordActiveWindow => &SHORTCUT_DEFINITIONS[5],
    ShortcutType::Gif => &SHORTCUT_DEFINITIONS[6],
    ShortcutType::ScreenshotEdit => &SHORTCUT_DEFINITIONS[7],
    ShortcutType::CopyText => &SHORTCUT_DEFINITIONS[8],
    ShortcutType::PickColor => &SHORTCUT_DEFINITIONS[9],
    ShortcutType::LastRegion => &SHORTCUT_DEFINITIONS[10],
  }
}

pub(crate) fn shortcut_value(config: &Config, shortcut_type: ShortcutType) -> &str {
  match shortcut_type {
    ShortcutType::ScreenshotArea => &config.screenshot_area_shortcut,
    ShortcutType::ScreenshotFullscreen => &config.screenshot_fullscreen_shortcut,
    ShortcutType::ScreenshotActiveWindow => &config.screenshot_active_window_shortcut,
    ShortcutType::RecordArea => &config.record_area_shortcut,
    ShortcutType::RecordFullscreen => &config.record_fullscreen_shortcut,
    ShortcutType::RecordActiveWindow => &config.record_active_window_shortcut,
    ShortcutType::Gif => &config.gif_shortcut,
    ShortcutType::ScreenshotEdit => &config.screenshot_edit_shortcut,
    ShortcutType::CopyText => &config.copy_text_shortcut,
    ShortcutType::PickColor => &config.color_picker_shortcut,
    ShortcutType::LastRegion => &config.last_region_shortcut,
  }
}

pub(crate) fn shortcuts_changed(previous: &Config, current: &Config) -> bool {
  shortcut_definitions().iter().any(|definition| {
    normalize_shortcut(shortcut_value(previous, definition.shortcut_type))
      != normalize_shortcut(shortcut_value(current, definition.shortcut_type))
  })
}

pub(crate) fn canonicalize_shortcut(shortcut: &str) -> String {
  let normalized = normalize_shortcut(shortcut);
  if normalized.is_empty() {
    return normalized;
  }

  normalized
    .split('+')
    .map(|part| {
      let trimmed = part.trim();
      if trimmed.eq_ignore_ascii_case("ctrl") {
        "Ctrl".to_string()
      } else if trimmed.eq_ignore_ascii_case("alt") {
        "Alt".to_string()
      } else if trimmed.eq_ignore_ascii_case("shift") {
        "Shift".to_string()
      } else if trimmed.eq_ignore_ascii_case("command") {
        "Command".to_string()
      } else if trimmed.eq_ignore_ascii_case("print") || trimmed.eq_ignore_ascii_case("printscreen")
      {
        "PrintScreen".to_string()
      } else if trimmed.len() == 1
        && trimmed
          .chars()
          .next()
          .is_some_and(|ch| ch.is_ascii_alphabetic())
      {
        trimmed.to_ascii_uppercase()
      } else {
        trimmed.to_string()
      }
    })
    .collect::<Vec<_>>()
    .join("+")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn definitions_cover_all_shortcut_types_once() {
    assert_eq!(shortcut_definitions().len(), 11);
    assert_eq!(
      definition_for(ShortcutType::LastRegion).config_key,
      "last_region_shortcut"
    );
    assert_eq!(
      definition_for(ShortcutType::PickColor).config_key,
      "color_picker_shortcut"
    );
    assert_eq!(
      definition_for(ShortcutType::ScreenshotArea).config_key,
      "screenshot_area_shortcut"
    );
    assert_eq!(definition_for(ShortcutType::Gif).human_label, "Record GIF");
  }

  #[test]
  fn canonicalize_shortcut_normalizes_expected_tokens() {
    assert_eq!(canonicalize_shortcut(" ctrl + shift + 3 "), "Ctrl+Shift+3");
    assert_eq!(
      canonicalize_shortcut("alt + printscreen"),
      "Alt+PrintScreen"
    );
    assert_eq!(canonicalize_shortcut(" ctrl + alt + g "), "Ctrl+Alt+G");
  }

  #[test]
  fn shortcuts_changed_iterates_over_registry_entries() {
    let previous = Config::default();
    let current = Config {
      gif_shortcut: "Ctrl+Shift+G".to_string(),
      ..Config::default()
    };

    assert!(shortcuts_changed(&previous, &current));
    assert!(!shortcuts_changed(&current, &current));
  }
}
