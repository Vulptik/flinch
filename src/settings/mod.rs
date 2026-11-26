//! Native settings projection. The Config remains authoritative; this adapter
//! does not persist, register shortcuts, start UI loops or own capture state.

use crate::config::{Config, NotificationPosition, SaveLocationMode};
use crate::video::config::VideoQuality;
use flinch_settings::edit::PreferenceEdit;
pub use flinch_settings::model::Section;
use flinch_settings::model::SettingsSnapshot;

pub(crate) mod config_actions;
pub(crate) mod config_service;
#[cfg(feature = "native-settings")]
mod microphone;
#[cfg(feature = "native-settings")]
pub mod native_host;
#[cfg(feature = "native-settings")]
pub(crate) mod native_keys;
#[cfg(all(feature = "native-settings", debug_assertions))]
pub(crate) mod native_probe;
#[cfg(feature = "native-settings")]
mod placement;
#[cfg(feature = "native-settings")]
pub(crate) mod shortcut_capture;
#[cfg(feature = "native-settings")]
pub(crate) mod text_panel;
#[cfg(feature = "native-settings")]
pub(crate) mod updates;

/// Render the current drafts without consulting saved settings or suppressing invalid input.
/// Validation and persistence remain separate; both previews share one timestamp.
pub(crate) fn template_draft_previews(filename: &str, subfolder: &str) -> (String, String) {
  template_draft_previews_at(filename, subfolder, chrono::Local::now())
}

pub(crate) fn template_draft_previews_at(
  filename: &str,
  subfolder: &str,
  now: chrono::DateTime<chrono::Local>,
) -> (String, String) {
  (
    crate::filename::render_template(
      filename,
      &crate::filename::FilenameContext {
        capture_type: "screenshot",
        app_name: Some(crate::filename::PREVIEW_APP),
        now,
      },
    ),
    crate::capture_subfolder::render_template(
      subfolder,
      &crate::capture_subfolder::SubfolderContext { now },
    ),
  )
}

/// Apply one edit to the latest authoritative configuration. Never reconstruct
/// Config from the UI projection: hidden capture preferences must survive.
impl TryFrom<(Config, PreferenceEdit)> for Config {
  type Error = anyhow::Error;

  fn try_from((mut config, edit): (Config, PreferenceEdit)) -> Result<Self, Self::Error> {
    use PreferenceEdit::*;
    match edit {
      Language(value) => {
        anyhow::ensure!(
          flinch_settings::model::LOCALES.contains(&value.as_str()),
          "Unsupported settings language"
        );
        config.language = value;
      },
      AutoStart(value) => config.auto_start = value,
      CheckForUpdates(value) => config.check_for_updates = value,
      NotificationsEnabled(value) => config.notifications_enabled = value,
      NotificationSound(value) => config.notification_sound = value,
      NotificationDuration(value) => config.notification_duration_ms = value,
      NotificationPosition(value) => {
        config.notification_position = serde_json::from_value(value.into())?
      },
      ScreenshotFormat(value) => config.screenshot_format = serde_json::from_value(value.into())?,
      ScreenshotQuality(value) => config.screenshot_quality = value,
      GifFps(value) => config.gif_fps = value,
      GifDuration(value) => config.gif_max_duration = value,
      VideoFps(value) => config.video_fps = value,
      VideoQuality(value) => config.video_quality = serde_json::from_value(value.into())?,
      HardwareEncoding(value) => config.video_hardware_encoding = value,
      RecordAudio(value) => config.record_audio = value,
      ShowCursor(value) => config.show_cursor = value,
      GifShowCursor(value) => config.gif_show_cursor = value,
      ShowClicks(value) => config.show_clicks = value,
      GifShowClicks(value) => config.gif_show_clicks = value,
      RecordMicrophone(value) => config.record_microphone = value,
      Microphone(choice) => {
        (config.microphone_device, config.microphone_name) = match choice {
          Some(choice) => (Some(choice.id), Some(choice.name)),
          None => (None, None),
        };
      },
      SelectionMagnifier(value) => config.selection_magnifier = value,
      CopyTextDirect(value) => config.copy_text_direct = value,
      CaptureDelay(value) => config.capture_delay = value,
      LargePngAsJpg(value) => config.screenshot_large_png_as_jpg = value,
      SaveLocation(value) => config.save_location_mode = serde_json::from_value(value.into())?,
      SaveFolder(target, value) => {
        if let Some(path) = &value {
          crate::config::validate_custom_folder(path).map_err(anyhow::Error::msg)?;
        }
        *config.custom_folder_mut(target) = value;
      },
      Filename(value) => {
        crate::filename::validate_template(&value).map_err(anyhow::Error::msg)?;
        config.filename_template = Some(value);
      },
      Subfolder(value) => {
        crate::capture_subfolder::validate_template(&value).map_err(anyhow::Error::msg)?;
        config.single_folder_subfolder_template = Some(value);
      },
      ResetFilename => config.filename_template = None,
      ResetSubfolder => config.single_folder_subfolder_template = None,
      Shortcut(action, value) => {
        use flinch_settings::edit::ShortcutAction;
        let value = crate::shortcuts::registry::canonicalize_shortcut(&value);
        if !value.is_empty() {
          let parsed: crate::app::hotkeys::Shortcut = value
            .parse()
            .map_err(|error| anyhow::anyhow!("Invalid shortcut: {error}"))?;
          use crate::shortcuts::state::ShortcutType;
          let edited_type = match action {
            ShortcutAction::ScreenshotArea => ShortcutType::ScreenshotArea,
            ShortcutAction::ScreenshotFullscreen => ShortcutType::ScreenshotFullscreen,
            ShortcutAction::ScreenshotWindow => ShortcutType::ScreenshotActiveWindow,
            ShortcutAction::ScreenshotEdit => ShortcutType::ScreenshotEdit,
            ShortcutAction::CopyText => ShortcutType::CopyText,
            ShortcutAction::PickColor => ShortcutType::PickColor,
            ShortcutAction::LastRegion => ShortcutType::LastRegion,
            ShortcutAction::Gif => ShortcutType::Gif,
            ShortcutAction::VideoArea => ShortcutType::RecordArea,
            ShortcutAction::VideoFullscreen => ShortcutType::RecordFullscreen,
            ShortcutAction::VideoWindow => ShortcutType::RecordActiveWindow,
          };
          for definition in crate::shortcuts::registry::shortcut_definitions() {
            if definition.shortcut_type != edited_type {
              let other = crate::shortcuts::registry::canonicalize_shortcut(
                crate::shortcuts::registry::shortcut_value(&config, definition.shortcut_type),
              );
              if let Ok(other) = other.parse::<crate::app::hotkeys::Shortcut>()
                && parsed == other
              {
                // Preserve the existing UI's move-binding behavior atomically
                // with the new assignment, inside the configuration save lock.
                let previous = match definition.shortcut_type {
                  ShortcutType::ScreenshotArea => &mut config.screenshot_area_shortcut,
                  ShortcutType::ScreenshotFullscreen => &mut config.screenshot_fullscreen_shortcut,
                  ShortcutType::ScreenshotActiveWindow => {
                    &mut config.screenshot_active_window_shortcut
                  },
                  ShortcutType::ScreenshotEdit => &mut config.screenshot_edit_shortcut,
                  ShortcutType::CopyText => &mut config.copy_text_shortcut,
                  ShortcutType::PickColor => &mut config.color_picker_shortcut,
                  ShortcutType::LastRegion => &mut config.last_region_shortcut,
                  ShortcutType::Gif => &mut config.gif_shortcut,
                  ShortcutType::RecordArea => &mut config.record_area_shortcut,
                  ShortcutType::RecordFullscreen => &mut config.record_fullscreen_shortcut,
                  ShortcutType::RecordActiveWindow => &mut config.record_active_window_shortcut,
                };
                previous.clear();
              }
            }
          }
        }
        let target = match action {
          ShortcutAction::ScreenshotArea => &mut config.screenshot_area_shortcut,
          ShortcutAction::ScreenshotFullscreen => &mut config.screenshot_fullscreen_shortcut,
          ShortcutAction::ScreenshotWindow => &mut config.screenshot_active_window_shortcut,
          ShortcutAction::ScreenshotEdit => &mut config.screenshot_edit_shortcut,
          ShortcutAction::CopyText => &mut config.copy_text_shortcut,
          ShortcutAction::PickColor => &mut config.color_picker_shortcut,
          ShortcutAction::LastRegion => &mut config.last_region_shortcut,
          ShortcutAction::Gif => &mut config.gif_shortcut,
          ShortcutAction::VideoArea => &mut config.record_area_shortcut,
          ShortcutAction::VideoFullscreen => &mut config.record_fullscreen_shortcut,
          ShortcutAction::VideoWindow => &mut config.record_active_window_shortcut,
        };
        *target = value;
      },
    }
    SettingsSnapshot::from(&config)
      .validate_projection()
      .map_err(anyhow::Error::msg)?;
    Ok(config)
  }
}

impl From<&Config> for SettingsSnapshot {
  fn from(config: &Config) -> Self {
    SettingsSnapshot {
      language: config.language.clone(),
      auto_start: config.auto_start,
      check_for_updates: config.check_for_updates,
      notifications_enabled: config.notifications_enabled,
      notification_sound: config.notification_sound,
      notification_duration_ms: config.notification_duration_ms,
      notification_position: match config.notification_position {
        NotificationPosition::TopRight => "top-right",
        NotificationPosition::TopLeft => "top-left",
        NotificationPosition::BottomRight => "bottom-right",
        NotificationPosition::BottomLeft => "bottom-left",
      }
      .to_owned(),
      screenshot_format: config.screenshot_format.as_str().to_owned(),
      screenshot_quality: config.screenshot_quality,
      gif_fps: config.gif_fps,
      gif_max_duration: config.gif_max_duration,
      video_fps: config.video_fps,
      video_quality: match config.video_quality {
        VideoQuality::Low => "low",
        VideoQuality::Medium => "medium",
        VideoQuality::High => "high",
      }
      .to_owned(),
      video_hardware_encoding: config.video_hardware_encoding,
      record_audio: config.record_audio,
      show_cursor: config.show_cursor,
      gif_show_cursor: config.gif_show_cursor,
      show_clicks: config.show_clicks,
      gif_show_clicks: config.gif_show_clicks,
      record_microphone: config.record_microphone,
      microphone_device: config.microphone_device.clone(),
      microphone_name: config.microphone_name.clone(),
      selection_magnifier: config.selection_magnifier,
      copy_text_direct: config.copy_text_direct,
      capture_delay: config.capture_delay,
      screenshot_large_png_as_jpg: config.screenshot_large_png_as_jpg,
      save_location_mode: match config.save_location_mode {
        SaveLocationMode::SystemFolders => "system_folders",
        SaveLocationMode::SingleFolder => "single_folder",
      }
      .to_owned(),
      custom_single_folder: config.custom_single_folder.clone(),
      custom_screenshots_folder: config.custom_screenshots_folder.clone(),
      custom_videos_folder: config.custom_videos_folder.clone(),
      filename_template: config.filename_template.clone(),
      single_folder_subfolder_template: config.single_folder_subfolder_template.clone(),
      screenshot_area_shortcut: config.screenshot_area_shortcut.clone(),
      screenshot_fullscreen_shortcut: config.screenshot_fullscreen_shortcut.clone(),
      screenshot_active_window_shortcut: config.screenshot_active_window_shortcut.clone(),
      screenshot_edit_shortcut: config.screenshot_edit_shortcut.clone(),
      copy_text_shortcut: config.copy_text_shortcut.clone(),
      color_picker_shortcut: config.color_picker_shortcut.clone(),
      last_region_shortcut: config.last_region_shortcut.clone(),
      gif_shortcut: config.gif_shortcut.clone(),
      record_area_shortcut: config.record_area_shortcut.clone(),
      record_fullscreen_shortcut: config.record_fullscreen_shortcut.clone(),
      record_active_window_shortcut: config.record_active_window_shortcut.clone(),
    }
  }
}

#[cfg(test)]
mod tests {
  #[test]
  fn template_edits_preserve_the_frozen_preview_timestamp() {
    let frozen = chrono::Local::now();
    let template = "%DATETIME%_%TIMESTAMP%";
    let folder = "%YEAR%-%MONTH%-%DAY%";
    let original = super::template_draft_previews_at(template, folder, frozen);
    let edited = super::template_draft_previews_at(&format!("{template}_edited"), folder, frozen);
    assert_eq!(edited.0, format!("{}_edited", original.0));
    assert_eq!(edited.1, original.1);
    let later =
      super::template_draft_previews_at(template, folder, frozen + chrono::Duration::days(2));
    assert_ne!(later.0, original.0);
    assert_ne!(later.1, original.1);
  }

  #[test]
  fn previews_follow_both_drafts_even_while_validation_fails() {
    for (filename, folder, expected_filename, expected_folder) in [
      ("%APP%_new", "new folder", "chrome_new", "new folder"),
      ("%APP%_new:", "folder:", "chrome_new:", "folder:"),
      ("", "", "", ""),
      ("%whatever%", "%whatever%", "%whatever%", "%whatever%"),
    ] {
      assert_eq!(
        super::template_draft_previews(filename, folder),
        (expected_filename.into(), expected_folder.into())
      );
    }
    assert!(crate::filename::validate_template("%APP%_new:").is_err());
    assert!(crate::capture_subfolder::validate_template("folder:").is_err());
  }
  use super::*;

  #[test]
  fn gif_and_video_cursor_preferences_are_independent() {
    for (gif, video) in [(false, true), (true, false)] {
      let config = Config::try_from((Config::default(), PreferenceEdit::GifShowCursor(gif)))
        .expect("GIF cursor edit");
      let config =
        Config::try_from((config, PreferenceEdit::ShowCursor(video))).expect("video cursor edit");
      let reloaded: Config = serde_json::from_value(serde_json::to_value(config).expect("JSON"))
        .expect("reload independent preferences");
      assert_eq!(reloaded.gif_show_cursor, gif);
      assert_eq!(reloaded.show_cursor, video);
    }
    let legacy: Config = serde_json::from_str(r#"{"show_cursor":false}"#)
      .expect("older config without a GIF cursor preference");
    assert!(legacy.gif_show_cursor);
    assert!(!legacy.show_cursor);
  }

  #[test]
  fn update_preference_changes_only_its_own_persisted_field() {
    let config = Config::default();
    let mut expected = serde_json::to_value(&config).expect("config JSON");
    expected["check_for_updates"] = false.into();
    let edited = Config::try_from((config, PreferenceEdit::CheckForUpdates(false)))
      .expect("update preference");
    assert_eq!(serde_json::to_value(edited).expect("config JSON"), expected);
  }

  #[test]
  fn template_resets_restore_default_semantics_without_changing_other_preferences() {
    let config = Config {
      filename_template: Some("custom_%TYPE%".into()),
      single_folder_subfolder_template: Some("custom".into()),
      record_audio: true,
      ..Config::default()
    };
    let reset = Config::try_from((config, PreferenceEdit::ResetFilename)).expect("filename reset");
    assert!(reset.filename_template.is_none());
    assert_eq!(
      reset.effective_filename_template(),
      crate::filename::DEFAULT_TEMPLATE
    );
    assert_eq!(
      reset.single_folder_subfolder_template.as_deref(),
      Some("custom")
    );
    let reset = Config::try_from((reset, PreferenceEdit::ResetSubfolder)).expect("subfolder reset");
    assert!(reset.single_folder_subfolder_template.is_none());
    assert_eq!(
      reset.effective_single_folder_subfolder_template(),
      Some(crate::capture_subfolder::DEFAULT_TEMPLATE)
    );
    assert!(reset.record_audio);
  }

  #[test]
  fn save_folder_edits_apply_to_their_own_target_and_reset_to_the_built_in_folder() {
    use flinch_settings::edit::FolderTarget;
    let config = Config::try_from((
      Config::default(),
      PreferenceEdit::SaveFolder(FolderTarget::Videos, Some(r"D:\Clips".into())),
    ))
    .expect("custom folder");
    assert_eq!(config.custom_videos_folder.as_deref(), Some(r"D:\Clips"));
    assert!(config.custom_screenshots_folder.is_none());
    assert!(config.custom_single_folder.is_none());
    let config = Config::try_from((
      config,
      PreferenceEdit::SaveFolder(FolderTarget::Videos, None),
    ))
    .expect("reset folder");
    assert!(config.custom_videos_folder.is_none());
    let config = Config::try_from((
      config,
      PreferenceEdit::SaveFolder(FolderTarget::Screenshots, Some(r"D:\Shots".into())),
    ))
    .expect("screenshots folder");
    let config = Config::try_from((
      config,
      PreferenceEdit::SaveFolder(FolderTarget::Together, Some(r"D:\All".into())),
    ))
    .expect("together folder");
    assert_eq!(
      config.custom_screenshots_folder.as_deref(),
      Some(r"D:\Shots")
    );
    assert_eq!(config.custom_single_folder.as_deref(), Some(r"D:\All"));
    for path in ["relative", r"\\server\share", ""] {
      assert!(
        Config::try_from((
          Config::default(),
          PreferenceEdit::SaveFolder(FolderTarget::Together, Some(path.into()))
        ))
        .is_err(),
        "accepted {path}"
      );
    }
  }

  #[test]
  fn shortcut_edits_reject_invalid_bindings_and_allow_clear() {
    use flinch_settings::edit::ShortcutAction;
    {
      let value = "NotAKey";
      assert!(
        Config::try_from((
          Config::default(),
          PreferenceEdit::Shortcut(ShortcutAction::Gif, value.into())
        ))
        .is_err(),
        "accepted {value}"
      );
    }
    let cleared = Config::try_from((
      Config::default(),
      PreferenceEdit::Shortcut(ShortcutAction::Gif, String::new()),
    ))
    .expect("clear shortcut");
    assert!(cleared.gif_shortcut.is_empty());
    let config = Config::default();
    let value = config.screenshot_area_shortcut.clone();
    assert!(
      Config::try_from((
        config,
        PreferenceEdit::Shortcut(ShortcutAction::ScreenshotArea, value)
      ))
      .is_ok()
    );
  }

  #[test]
  fn assigning_an_existing_binding_moves_it_to_the_new_action() {
    use flinch_settings::edit::ShortcutAction;
    let original = Config::default();
    let binding = original.screenshot_area_shortcut.clone();
    let config = Config::try_from((
      original.clone(),
      PreferenceEdit::Shortcut(ShortcutAction::Gif, binding.clone()),
    ))
    .expect("move binding");
    assert_eq!(config.gif_shortcut, binding);
    assert!(config.screenshot_area_shortcut.is_empty());
    assert_eq!(
      config.screenshot_fullscreen_shortcut,
      original.screenshot_fullscreen_shortcut
    );
  }

  #[test]
  fn former_quit_chord_is_available_as_an_ordinary_shortcut() {
    use flinch_settings::edit::ShortcutAction;
    let config = Config::try_from((
      Config::default(),
      PreferenceEdit::Shortcut(ShortcutAction::Gif, "Ctrl+Shift+Q".into()),
    ))
    .expect("ordinary binding");
    assert_eq!(config.gif_shortcut, "Ctrl+Shift+Q");
  }

  #[test]
  fn preference_edit_preserves_every_unrelated_persisted_field() {
    let config = Config::default();
    let mut expected = serde_json::to_value(&config).expect("config JSON");
    expected["record_audio"] = true.into();
    let edited = Config::try_from((config, PreferenceEdit::RecordAudio(true))).expect("valid edit");
    assert_eq!(serde_json::to_value(edited).expect("config JSON"), expected);
  }

  #[test]
  fn preference_edit_rejects_unknown_choices_before_persistence() {
    for edit in [
      PreferenceEdit::Language("xx".into()),
      PreferenceEdit::ScreenshotFormat("avif".into()),
      PreferenceEdit::VideoFps(144),
      PreferenceEdit::NotificationDuration(0),
      PreferenceEdit::Filename("%what:ever%".into()),
      PreferenceEdit::Filename("x".repeat(101)),
      PreferenceEdit::Subfolder("%YEAR%/%MONTH%".into()),
    ] {
      assert!(Config::try_from((Config::default(), edit)).is_err());
    }
  }

  #[test]
  fn template_edits_preserve_spaces_between_keystrokes() {
    for value in [" My capture ", "%APP% ", "%APP% %TYPE%"] {
      let edited = Config::try_from((Config::default(), PreferenceEdit::Filename(value.into())))
        .expect("spaces accepted during typing");
      assert_eq!(edited.filename_template.as_deref(), Some(value));
    }
    for value in [" My captures ", "%YEAR% ", "%YEAR% %MONTH%"] {
      let edited = Config::try_from((Config::default(), PreferenceEdit::Subfolder(value.into())))
        .expect("spaces accepted during typing");
      assert_eq!(
        edited.single_folder_subfolder_template.as_deref(),
        Some(value)
      );
    }
  }

  #[test]
  fn unknown_template_names_are_preserved_by_preference_edits() {
    let edited = Config::try_from((
      Config::default(),
      PreferenceEdit::Filename("%whatever%".into()),
    ))
    .expect("literal filename accepted");
    assert_eq!(edited.filename_template.as_deref(), Some("%whatever%"));
    let edited = Config::try_from((edited, PreferenceEdit::Subfolder("%whatever%".into())))
      .expect("literal subfolder accepted");
    assert_eq!(
      edited.single_folder_subfolder_template.as_deref(),
      Some("%whatever%")
    );
  }

  #[test]
  fn empty_template_edit_remains_explicit_instead_of_restoring_a_default() {
    let edited = Config::try_from((Config::default(), PreferenceEdit::Subfolder(String::new())))
      .expect("empty subfolder is valid");
    assert_eq!(edited.single_folder_subfolder_template, Some(String::new()));
  }

  #[test]
  fn validated_configuration_projects_without_mutation() {
    let mut config = Config {
      language: "fr".into(),
      auto_start: true,
      video_fps: 60,
      record_audio: true,
      record_active_window_shortcut: "Ctrl+Shift+F9".into(),
      notification_duration_ms: 2500,
      filename_template: None,
      single_folder_subfolder_template: Some(String::new()),
      ..Config::default()
    };
    config.validate();
    let before = serde_json::to_value(&config).expect("config JSON");
    let snapshot = SettingsSnapshot::from(&config);
    snapshot
      .validate_projection()
      .expect("supported native settings");
    assert_eq!(snapshot.language, "fr");
    assert!(snapshot.auto_start);
    assert!(snapshot.record_audio);
    assert_eq!(snapshot.video_fps, 60);
    assert_eq!(snapshot.record_active_window_shortcut, "Ctrl+Shift+F9");
    assert_eq!(snapshot.notification_duration_ms, 2500);
    assert_eq!(snapshot.filename_template, None);
    assert_eq!(
      snapshot.single_folder_subfolder_template,
      Some(String::new())
    );
    assert_eq!(serde_json::to_value(&config).expect("config JSON"), before);
  }
}
