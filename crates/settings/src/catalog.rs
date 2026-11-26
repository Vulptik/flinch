use std::collections::BTreeMap;

use crate::model::{LOCALES, Section};

pub struct Catalog {
  strings: BTreeMap<String, String>,
}

pub struct SearchEntry {
  pub id: &'static str,
  pub section: Section,
  pub label: String,
  pub section_label: String,
}

// Stable identifiers match existing tray/navigation and native setting IDs.
const SETTINGS: &[(Section, &str, &str, &str)] = &[
  (
    Section::General,
    "system-language",
    "system_language",
    "language locale",
  ),
  (
    Section::General,
    "system-start-at-boot",
    "system_start_at_boot",
    "startup boot autostart",
  ),
  (
    Section::General,
    "system-import-config",
    "system_import",
    "import json config",
  ),
  (
    Section::General,
    "system-check-for-updates",
    "system_check_for_updates",
    "update version release",
  ),
  (
    Section::General,
    "system-export-config",
    "system_export",
    "export json config",
  ),
  (
    Section::General,
    "system-reset-config",
    "system_reset",
    "reset defaults",
  ),
  (
    Section::Notifications,
    "notifications-enabled",
    "notifications_enabled",
    "notification toast",
  ),
  (
    Section::Notifications,
    "notifications-sound",
    "notifications_sound",
    "sound audio",
  ),
  (
    Section::Notifications,
    "notifications-duration",
    "notifications_duration",
    "duration seconds",
  ),
  (
    Section::Notifications,
    "notifications-position",
    "notifications_position",
    "position corner",
  ),
  (
    Section::Notifications,
    "notifications-test",
    "notifications_test",
    "test preview",
  ),
  (
    Section::Hotkeys,
    "hotkey-screenshot-area",
    "hotkeys_area",
    "screenshot shortcut keybind area",
  ),
  (
    Section::Hotkeys,
    "hotkey-screenshot-fullscreen",
    "hotkeys_fullscreen",
    "screenshot shortcut keybind fullscreen",
  ),
  (
    Section::Hotkeys,
    "hotkey-screenshot-active-window",
    "hotkeys_active_window",
    "screenshot shortcut keybind window",
  ),
  (
    Section::Hotkeys,
    "hotkey-screenshot-edit",
    "hotkeys_capture_edit",
    "screenshot shortcut keybind edit annotate",
  ),
  (
    Section::Hotkeys,
    "hotkey-copy-text",
    "hotkeys_copy_text",
    "text ocr recognize read shortcut keybind",
  ),
  (
    Section::Hotkeys,
    "hotkey-last-region",
    "hotkeys_last_region",
    "last previous same region area repeat again screenshot shortcut keybind",
  ),
  (
    Section::Hotkeys,
    "hotkey-pick-color",
    "hotkeys_color_picker",
    "color colour picker eyedropper hex pixel shortcut keybind",
  ),
  (
    Section::Hotkeys,
    "hotkey-gif-record",
    "hotkeys_record_gif",
    "gif shortcut keybind",
  ),
  (
    Section::Hotkeys,
    "hotkey-video-area",
    "hotkeys_area",
    "video shortcut keybind area",
  ),
  (
    Section::Hotkeys,
    "hotkey-video-fullscreen",
    "hotkeys_fullscreen",
    "video shortcut keybind fullscreen",
  ),
  (
    Section::Hotkeys,
    "hotkey-video-active-window",
    "hotkeys_active_window",
    "video shortcut keybind window",
  ),
  (
    Section::Screenshots,
    "screenshot-format",
    "screenshots_format",
    "png jpg webp format",
  ),
  (
    Section::Screenshots,
    "screenshot-quality",
    "screenshots_quality",
    "quality compression",
  ),
  (
    Section::Screenshots,
    "large-png-as-jpg",
    "screenshots_large_png_as_jpg",
    "png jpg jpeg large size small file compress auto convert",
  ),
  (
    Section::Screenshots,
    "gif-max-duration",
    "screenshots_gif_max_duration",
    "gif duration",
  ),
  (
    Section::Tools,
    "selection-magnifier",
    "selection_magnifier",
    "magnifier zoom lens pixel precise crop selection gif video",
  ),
  (
    Section::Tools,
    "copy-text-direct",
    "tools_copy_text_direct",
    "text ocr recognize read clipboard direct instant panel",
  ),
  (
    Section::Tools,
    "capture-delay",
    "tools_capture_delay",
    "delay timer countdown wait seconds menu tooltip self-timer screenshot gif video",
  ),
  (
    Section::Screenshots,
    "gif-frame-rate",
    "screenshots_frame_rate",
    "gif fps frame rate",
  ),
  (
    Section::Screenshots,
    "gif-show-cursor",
    "gif_show_cursor",
    "gif mouse pointer cursor",
  ),
  (
    Section::Screenshots,
    "gif-show-clicks",
    "gif_show_clicks",
    "gif mouse click clicks highlight",
  ),
  (
    Section::Videos,
    "video-quality",
    "videos_quality",
    "video quality bitrate",
  ),
  (
    Section::Videos,
    "video-frame-rate",
    "videos_frame_rate",
    "video fps frame rate",
  ),
  (
    Section::Videos,
    "video-hardware-encoding",
    "videos_hardware_encoding",
    "gpu nvenc amd amf quicksync acceleration",
  ),
  (
    Section::Videos,
    "video-record-audio",
    "videos_record_audio",
    "microphone audio wasapi",
  ),
  (
    Section::Videos,
    "video-show-cursor",
    "videos_show_cursor",
    "mouse pointer cursor",
  ),
  (
    Section::Videos,
    "video-show-clicks",
    "videos_show_clicks",
    "mouse click clicks highlight tutorial",
  ),
  (
    Section::Videos,
    "video-record-microphone",
    "videos_record_microphone",
    "microphone mic voice narration commentary",
  ),
  (
    Section::Videos,
    "video-microphone",
    "videos_microphone",
    "microphone mic input device headset",
  ),
  (
    Section::Files,
    "files-filename-template",
    "system_filename_template",
    "filename name template tokens",
  ),
  (
    Section::Files,
    "files-save-location",
    "system_save_location",
    "path directory folder documents custom change drive",
  ),
  (
    Section::Files,
    "files-single-folder-subfolder-template",
    "system_single_folder_subfolder_template",
    "subfolder year month day template",
  ),
  (
    Section::About,
    "about-support",
    "about_support",
    "feedback bug help report privacy",
  ),
  (
    Section::About,
    "about-diagnostics",
    "about_diagnostics",
    "version windows diagnostics copy logs",
  ),
  (
    Section::About,
    "about-rate",
    "about_rate",
    "review rating store",
  ),
];

impl Catalog {
  pub fn load(locale: &str) -> Result<Self, String> {
    let source = match locale {
      "en" => include_str!("../../../messages/en.json"),
      "fr" => include_str!("../../../messages/fr.json"),
      "de" => include_str!("../../../messages/de.json"),
      "es" => include_str!("../../../messages/es.json"),
      "it" => include_str!("../../../messages/it.json"),
      _ => return Err(format!("Unsupported settings locale: {locale}")),
    };
    let values: BTreeMap<String, serde_json::Value> =
      serde_json::from_str(source).map_err(|error| error.to_string())?;
    let strings = values
      .into_iter()
      .map(|(key, value)| {
        value
          .as_str()
          .map(|value| (key.clone(), value.to_owned()))
          .ok_or_else(|| format!("Non-string translation: {key}"))
      })
      .collect::<Result<_, _>>()?;
    Ok(Self { strings })
  }

  pub fn text(&self, key: &str) -> Result<&str, String> {
    self
      .strings
      .get(key)
      .map(String::as_str)
      .ok_or_else(|| format!("Missing settings translation: {key}"))
  }

  pub fn search(&self, term: &str) -> Result<Vec<SearchEntry>, String> {
    let query = term.trim().to_lowercase();
    if query.is_empty() {
      return Ok(Vec::new());
    }
    let mut results = Vec::new();
    for &(section, id, label_key, keywords) in SETTINGS {
      let label = self.text(label_key)?;
      let section_key = match section {
        Section::General => "sidebar_general",
        Section::Notifications => "sidebar_notifications",
        Section::Hotkeys => "sidebar_hotkeys",
        Section::Screenshots => "sidebar_screenshots",
        Section::Videos => "sidebar_videos",
        Section::Files => "sidebar_files",
        Section::About => "about_title",
        Section::Tools => "sidebar_tools",
      };
      let section_label = self.text(section_key)?;
      let haystack = format!("{label} {section_label} {keywords}").to_lowercase();
      if query
        .split_whitespace()
        .all(|token| haystack.contains(token))
      {
        results.push(SearchEntry {
          id,
          section,
          label: label.to_owned(),
          section_label: section_label.to_owned(),
        });
      }
    }
    Ok(results)
  }
}

pub fn locale_index(locale: &str) -> Result<i32, String> {
  LOCALES
    .iter()
    .position(|candidate| *candidate == locale)
    .map(|index| index as i32)
    .ok_or_else(|| format!("Unsupported settings locale: {locale}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn all_locales_resolve_every_registered_setting() {
    for locale in LOCALES {
      let catalog = Catalog::load(locale).expect("catalog");
      for &(_, _, key, _) in SETTINGS {
        assert!(!catalog.text(key).expect("setting label").is_empty());
      }
      assert!(catalog.search(" ").expect("search").is_empty());
    }
  }

  #[test]
  fn search_distinguishes_duplicate_hotkey_labels_and_matches_keywords() {
    let catalog = Catalog::load("fr").expect("catalog");
    let results = catalog.search("video area").expect("search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "hotkey-video-area");
    assert_eq!(
      catalog.search("NVENC").expect("search")[0].id,
      "video-hardware-encoding"
    );
    assert_eq!(catalog.search("Fréquence").expect("search").len(), 2);
    assert!(locale_index("unknown").is_err());
  }
}
