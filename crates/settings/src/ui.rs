use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::{
  catalog::Catalog,
  edit::{PreferenceEdit, selection},
  model::{LOCALES, SettingsSnapshot},
};

slint::include_modules!();

/// Embedded confirmations share the settings event loop and never block it.
pub fn connect_reset_confirmation(window: &SettingsWindow) {
  let weak = window.as_weak();
  window.global::<SettingsState>().on_reset_request(move || {
    if let Some(window) = weak.upgrade()
      && window.global::<Interaction>().get_editable()
      && !window.global::<SettingsState>().get_confirm_invalid_close()
      && !window.get_confirming_reset()
    {
      window.set_confirming_reset(true);
    }
  });
}

pub struct Presentation {
  pub locale: String,
  pub filename_preview: String,
  pub subfolder_preview: String,
  pub screenshots_path: String,
  pub videos_path: String,
  pub channel: String,
}

/// Closing is a presenter decision. Dispatch must return promptly; pending
/// writes may finish asynchronously before the presenter calls Window::hide.
pub fn connect_close_request(
  window: &SettingsWindow,
  dispatch: impl Fn(&SettingsWindow) + 'static,
) {
  let weak = window.as_weak();
  window.on_close_request(move || {
    if let Some(window) = weak.upgrade() {
      if window.get_confirming_reset() {
        window.invoke_cancel_reset();
      } else {
        dispatch(&window);
      }
    }
  });
  let weak = window.as_weak();
  window.window().on_close_requested(move || {
    if let Some(window) = weak.upgrade() {
      window.invoke_close_request();
    }
    slint::CloseRequestResponse::KeepWindowShown
  });
}

/// Register once on the UI owner thread. The callback receives an owned edit;
/// service adapters must dispatch it asynchronously, never wait on this loop.
pub fn connect_edits(
  window: &SettingsWindow,
  dispatch: impl Fn(Result<PreferenceEdit, String>) + 'static,
) {
  let weak = window.as_weak();
  window
    .global::<SettingsState>()
    .on_edit_request(move |field| {
      if let Some(window) = weak.upgrade() {
        dispatch(read_edit(&window, field.as_str()));
      }
    });
}

fn read_edit(window: &SettingsWindow, field: &str) -> Result<PreferenceEdit, String> {
  use PreferenceEdit::*;
  let state = window.global::<SettingsState>();
  Ok(match field {
    "language" => Language(selection(&LOCALES, window.get_language_index())?.to_owned()),
    "boot" => AutoStart(state.get_boot()),
    "updates" => CheckForUpdates(state.get_updates()),
    "notifications" => NotificationsEnabled(state.get_notifications()),
    "sound" => NotificationSound(state.get_sound()),
    "notification-duration-index" => {
      // Read the represented duration, including imported custom durations.
      use slint::Model;
      let text = state
        .get_notification_durations()
        .row_data(
          usize::try_from(state.get_notification_duration_index())
            .map_err(|_| "Invalid notification duration selection")?,
        )
        .ok_or("Missing notification duration selection")?;
      let seconds = text
        .strip_suffix('s')
        .ok_or("Invalid notification duration")?
        .parse::<f64>()
        .map_err(|_| "Invalid notification duration")?;
      let milliseconds = seconds * 1000.0;
      if !milliseconds.is_finite() || !(1000.0..=10000.0).contains(&milliseconds) {
        return Err("Notification duration is out of range".into());
      }
      NotificationDuration(milliseconds.round() as u64)
    },
    "notification-position-index" => NotificationPosition(
      selection(
        &["top-right", "top-left", "bottom-right", "bottom-left"],
        state.get_notification_position_index(),
      )?
      .to_owned(),
    ),
    "format-index" => {
      ScreenshotFormat(selection(&["png", "jpg", "webp"], state.get_format_index())?.to_owned())
    },
    "screenshot-quality-index" => ScreenshotQuality(selection(
      &[50, 75, 90],
      state.get_screenshot_quality_index(),
    )?),
    "gif-fps-index" => GifFps(selection(&[15, 30], state.get_gif_fps_index())?),
    "gif-duration-index" => GifDuration(selection(&[10, 20, 30], state.get_gif_duration_index())?),
    "video-fps-index" => VideoFps(selection(&[30, 60], state.get_video_fps_index())?),
    "video-quality-index" => VideoQuality(
      selection(&["low", "medium", "high"], state.get_video_quality_index())?.to_owned(),
    ),
    "hardware" => HardwareEncoding(state.get_hardware()),
    "audio" => RecordAudio(state.get_audio()),
    "cursor" => ShowCursor(state.get_cursor()),
    "gif-cursor" => GifShowCursor(state.get_gif_cursor()),
    "clicks" => ShowClicks(state.get_clicks()),
    "gif-clicks" => GifShowClicks(state.get_gif_clicks()),
    "microphone" => RecordMicrophone(state.get_microphone()),
    "microphone-index" => {
      let index = usize::try_from(state.get_microphone_index())
        .map_err(|_| "Invalid microphone selection")?;
      let id = slint::Model::row_data(&state.get_microphone_ids(), index)
        .ok_or("Invalid microphone selection")?;
      let name = slint::Model::row_data(&state.get_microphone_names(), index)
        .ok_or("Invalid microphone selection")?;
      Microphone((!id.is_empty()).then(|| crate::edit::MicrophoneChoice {
        id: id.into(),
        name: name.into(),
      }))
    },
    "magnifier" => SelectionMagnifier(state.get_magnifier()),
    "copy-text-direct" => CopyTextDirect(state.get_copy_text_direct()),
    "capture-delay-index" => {
      CaptureDelay(selection(&[0, 3, 5, 10], state.get_capture_delay_index())?)
    },
    "large-png-as-jpg" => LargePngAsJpg(state.get_large_png_as_jpg()),
    "folder-index" => SaveLocation(
      selection(
        &["system_folders", "single_folder"],
        state.get_folder_index(),
      )?
      .to_owned(),
    ),
    "filename" => Filename(state.get_filename().to_string()),
    "subfolder" => Subfolder(state.get_subfolder().to_string()),
    _ => return Err(format!("Unknown native settings edit: {field}")),
  })
}

fn strings(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
  Rc::new(VecModel::from(
    values
      .into_iter()
      .map(SharedString::from)
      .collect::<Vec<_>>(),
  ))
  .into()
}

thread_local! {
  /// The connected microphones last listed, kept so a snapshot refresh never empties the picker.
  static AVAILABLE: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Fills the Microphone picker from the last listed microphones, keeping the saved choice
/// selected, and listed while it is unplugged.
fn apply_microphones(
  window: &SettingsWindow,
  snapshot: &SettingsSnapshot,
  locale: &str,
) -> Result<(), String> {
  let available = AVAILABLE.with(|available| available.borrow().clone());
  let chosen = snapshot
    .microphone_device
    .as_deref()
    .map(|id| (id, snapshot.microphone_name.as_deref()));
  fill_microphones(window, locale, &available, chosen)
}

/// Rebuilds the picker from a new list of connected microphones (id, name), keeping what is
/// selected now (saved or not yet), and listed while it is unplugged.
pub fn refresh_microphones(
  window: &SettingsWindow,
  available: &[(String, String)],
) -> Result<(), String> {
  AVAILABLE.with(|list| *list.borrow_mut() = available.to_vec());
  let locale = selection(&LOCALES, window.get_language_index())?;
  let state = window.global::<SettingsState>();
  let index = usize::try_from(state.get_microphone_index()).unwrap_or(0);
  let id = slint::Model::row_data(&state.get_microphone_ids(), index).unwrap_or_default();
  let name = slint::Model::row_data(&state.get_microphone_names(), index).unwrap_or_default();
  let chosen = (!id.is_empty()).then(|| (id.as_str(), Some(name.as_str())));
  fill_microphones(window, locale, available, chosen)
}

fn fill_microphones(
  window: &SettingsWindow,
  locale: &str,
  available: &[(String, String)],
  chosen: Option<(&str, Option<&str>)>,
) -> Result<(), String> {
  let catalog = Catalog::load(locale)?;
  let (entries, selected) = crate::microphones::picker(
    catalog.text("videos_microphone_default")?,
    catalog.text("videos_microphone_unplugged")?,
    available,
    chosen,
  );
  let column = |pick: fn(&crate::microphones::Entry) -> String| {
    ModelRc::new(VecModel::from(
      entries
        .iter()
        .map(|entry| SharedString::from(pick(entry)))
        .collect::<Vec<_>>(),
    ))
  };
  let state = window.global::<SettingsState>();
  state.set_microphone_options(column(|entry| entry.label.clone()));
  state.set_microphone_ids(column(|entry| entry.id.clone().unwrap_or_default()));
  state.set_microphone_names(column(|entry| entry.name.clone()));
  state.set_microphone_index(selected as i32);
  Ok(())
}

/// The live input level (0 to 1) and, when the meter cannot run, why, in plain language.
pub fn set_microphone_level(window: &SettingsWindow, level: f32, problem: &str) {
  let state = window.global::<SettingsState>();
  state.set_microphone_level(level.clamp(0.0, 1.0));
  state.set_microphone_problem(problem.into());
}

fn index<T: PartialEq>(options: &[T], value: &T) -> Result<i32, String> {
  options
    .iter()
    .position(|option| option == value)
    .map(|index| index as i32)
    .ok_or_else(|| "Settings value has no UI option".to_owned())
}

pub fn apply_snapshot(
  window: &SettingsWindow,
  snapshot: &SettingsSnapshot,
  presentation: &Presentation,
) -> Result<(), String> {
  snapshot.validate_projection()?;
  let locale = crate::catalog::locale_index(&presentation.locale)?;
  window.set_language_index(locale);
  let state = window.global::<SettingsState>();
  state.set_boot(snapshot.auto_start);
  state.set_notifications(snapshot.notifications_enabled);
  state.set_sound(snapshot.notification_sound);
  state.set_cursor(snapshot.show_cursor);
  state.set_gif_cursor(snapshot.gif_show_cursor);
  state.set_clicks(snapshot.show_clicks);
  state.set_gif_clicks(snapshot.gif_show_clicks);
  state.set_microphone(snapshot.record_microphone);
  apply_microphones(window, snapshot, &presentation.locale)?;
  state.set_magnifier(snapshot.selection_magnifier);
  state.set_copy_text_direct(snapshot.copy_text_direct);
  state.set_capture_delay_index(index(&[0, 3, 5, 10], &snapshot.capture_delay)?);
  state.set_large_png_as_jpg(snapshot.screenshot_large_png_as_jpg);
  state.set_audio(snapshot.record_audio);
  state.set_updates(snapshot.check_for_updates);
  state.set_hardware(snapshot.video_hardware_encoding);
  state.set_filename(
    snapshot
      .filename_template
      .clone()
      .unwrap_or_default()
      .into(),
  );
  state.set_subfolder(
    snapshot
      .single_folder_subfolder_template
      .clone()
      .unwrap_or_default()
      .into(),
  );
  state.set_filename_default(crate::edit::DEFAULT_FILENAME_TEMPLATE.into());
  state.set_subfolder_default(crate::edit::DEFAULT_SUBFOLDER_TEMPLATE.into());
  state.set_folder_index(index(
    &["system_folders", "single_folder"],
    &snapshot.save_location_mode.as_str(),
  )?);
  state.set_together_folder_custom(snapshot.custom_single_folder.is_some());
  state.set_screenshots_folder_custom(snapshot.custom_screenshots_folder.is_some());
  state.set_videos_folder_custom(snapshot.custom_videos_folder.is_some());
  state.set_format_index(index(
    &["png", "jpg", "webp"],
    &snapshot.screenshot_format.as_str(),
  )?);
  state.set_screenshot_quality_index(index(&[50, 75, 90], &snapshot.screenshot_quality)?);
  state.set_video_quality_index(index(
    &["low", "medium", "high"],
    &snapshot.video_quality.as_str(),
  )?);
  state.set_video_fps_index(index(&[30, 60], &snapshot.video_fps)?);
  state.set_gif_fps_index(index(&[15, 30], &snapshot.gif_fps)?);
  state.set_gif_duration_index(index(&[10, 20, 30], &snapshot.gif_max_duration)?);
  state.set_notification_position_index(index(
    &["top-right", "top-left", "bottom-right", "bottom-left"],
    &snapshot.notification_position.as_str(),
  )?);
  let mut durations = vec![1000, 3000, 5000, 10000];
  if !durations.contains(&snapshot.notification_duration_ms) {
    durations.push(snapshot.notification_duration_ms);
    durations.sort_unstable();
  }
  state.set_notification_duration_index(index(&durations, &snapshot.notification_duration_ms)?);
  state.set_notification_durations(strings(
    durations
      .into_iter()
      .map(|duration| format!("{}s", duration as f64 / 1000.0)),
  ));
  let keycaps = |shortcut: &str| {
    strings(
      shortcut
        .split('+')
        .filter(|part| !part.is_empty())
        .map(str::to_owned),
    )
  };
  state.set_shortcut_area(keycaps(&snapshot.screenshot_area_shortcut));
  state.set_shortcut_fullscreen(keycaps(&snapshot.screenshot_fullscreen_shortcut));
  state.set_shortcut_window(keycaps(&snapshot.screenshot_active_window_shortcut));
  state.set_shortcut_edit(keycaps(&snapshot.screenshot_edit_shortcut));
  state.set_shortcut_copy_text(keycaps(&snapshot.copy_text_shortcut));
  state.set_shortcut_pick_color(keycaps(&snapshot.color_picker_shortcut));
  state.set_shortcut_last_region(keycaps(&snapshot.last_region_shortcut));
  state.set_shortcut_gif(keycaps(&snapshot.gif_shortcut));
  state.set_shortcut_video_area(keycaps(&snapshot.record_area_shortcut));
  state.set_shortcut_video_fullscreen(keycaps(&snapshot.record_fullscreen_shortcut));
  state.set_shortcut_video_window(keycaps(&snapshot.record_active_window_shortcut));
  state.set_filename_preview(presentation.filename_preview.clone().into());
  state.set_subfolder_preview(presentation.subfolder_preview.clone().into());
  state.set_screenshots_path(presentation.screenshots_path.clone().into());
  state.set_videos_path(presentation.videos_path.clone().into());
  state.set_channel(presentation.channel.clone().into());
  Ok(())
}

/// A chip that no longer fits explains itself under the field. The next edit clears it.
fn show_template_limit(window: &SettingsWindow, subfolder: bool) -> Result<(), String> {
  let locale = selection(&LOCALES, window.get_language_index())?;
  let message = SharedString::from(Catalog::load(locale)?.text("native_template_long")?);
  let state = window.global::<SettingsState>();
  if subfolder {
    state.set_subfolder_error(message);
  } else {
    state.set_filename_error(message);
  }
  Ok(())
}

pub fn connect_search(window: &SettingsWindow) -> Result<(), String> {
  window
    .global::<TemplateEditing>()
    .on_caret_prefix(|text, cursor, subfolder| {
      let cursor = crate::edit::snap_template_cursor(text.as_str(), cursor, 0, subfolder);
      let mut end = (cursor.max(0) as usize).min(text.len());
      while !text.as_str().is_char_boundary(end) {
        end -= 1;
      }
      text.as_str()[..end].into()
    });
  window
    .global::<TemplateEditing>()
    .on_blocks(|text, subfolder| {
      let mut blocks = Vec::new();
      let mut offset = 0;
      for (start, end) in crate::edit::template_tokens(text.as_str(), subfolder) {
        if start > offset {
          blocks.push(TemplateBlock {
            prefix: text.as_str()[..offset].into(),
            token: text.as_str()[offset..start].into(),
            start: offset as i32,
            end: start as i32,
            tag: false,
          });
        }
        blocks.push(TemplateBlock {
          prefix: text.as_str()[..start].into(),
          token: text.as_str()[start..end].into(),
          start: start as i32,
          end: end as i32,
          tag: true,
        });
        offset = end;
      }
      if offset < text.len() {
        blocks.push(TemplateBlock {
          prefix: text.as_str()[..offset].into(),
          token: text.as_str()[offset..].into(),
          start: offset as i32,
          end: text.len() as i32,
          tag: false,
        });
      }
      ModelRc::new(VecModel::from(blocks))
    });
  window
    .global::<TemplateEditing>()
    .on_snap_cursor(|text, cursor, previous, subfolder| {
      crate::edit::snap_template_cursor(text.as_str(), cursor, previous, subfolder)
    });
  window
    .global::<TemplateEditing>()
    .on_limit_edit(|previous, candidate, cursor, subfolder| {
      let (text, cursor) =
        crate::edit::atomic_template_edit(previous.as_str(), candidate.as_str(), cursor, subfolder);
      TokenInsertion {
        text: text.into(),
        cursor,
        error: "".into(),
      }
    });
  let weak = window.as_weak();
  window.global::<TemplateEditing>().on_insert_token(
    move |text, token, anchor, cursor, subfolder| match crate::edit::insert_token(
      text.as_str(),
      token.as_str(),
      anchor,
      cursor,
      subfolder,
    ) {
      Ok((text, cursor)) => TokenInsertion {
        text: text.into(),
        cursor,
        error: "".into(),
      },
      Err(error) => {
        if error != crate::edit::TEMPLATE_LIMIT_ERROR {
          eprintln!("Cannot insert template token: {error}");
        } else if let Some(window) = weak.upgrade()
          && let Err(error) = show_template_limit(&window, subfolder)
        {
          eprintln!("Cannot show template limit: {error}");
        }
        TokenInsertion {
          text,
          cursor,
          error: error.into(),
        }
      },
    },
  );
  // Parse catalogs once, before entering the UI loop. Searches visit a fixed
  // registry, allocate at most its entry count, and hold no service locks.
  let catalogs = LOCALES
    .into_iter()
    .map(Catalog::load)
    .collect::<Result<Vec<_>, _>>()?;
  let weak = window.as_weak();
  window.on_search_request(move |query| {
    let Some(window) = weak.upgrade() else {
      return;
    };
    let Some(catalog) = usize::try_from(window.get_language_index())
      .ok()
      .and_then(|index| catalogs.get(index))
    else {
      eprintln!("Invalid native settings locale index");
      return;
    };
    match catalog.search(query.as_str()) {
      Ok(results) => {
        let rows = results
          .into_iter()
          .filter(|entry| {
            let channel = window.global::<SettingsState>().get_channel();
            (entry.id != "system-check-for-updates" || channel == "installer")
              && (entry.id != "about-rate" || channel == "msix")
          })
          .map(|entry| SearchResult {
            label: entry.label.into(),
            section: entry.section_label.into(),
            page: entry.section.index(),
            target: entry.id.into(),
          })
          .collect::<Vec<_>>();
        window.set_search_results(Rc::new(VecModel::from(rows)).into());
      },
      Err(error) => eprintln!("Native settings search failed: {error}"),
    }
  });
  Ok(())
}

#[cfg(test)]
mod tests;
