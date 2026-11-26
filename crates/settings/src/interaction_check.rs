//! Read-only checks against the production pages, not duplicate test widgets.
//! Fixed-size window coordinates deliberately fail when a control stops working.
use std::{cell::RefCell, rc::Rc};

use slint::{
  ComponentHandle,
  platform::{Key, PointerEventButton, WindowEvent},
};

use crate::{
  edit::{DEFAULT_FILENAME_TEMPLATE, DEFAULT_SUBFOLDER_TEMPLATE, FolderTarget, PreferenceEdit},
  model::SettingsSnapshot,
  ui::{self, Interaction, Presentation, SettingsState, SettingsWindow},
};

pub const FIELDS: &[&str] = &[
  "language",
  "boot",
  "updates",
  "notifications",
  "sound",
  "notification-duration-index",
  "notification-position-index",
  "format-index",
  "screenshot-quality-index",
  "gif-duration-index",
  "gif-fps-index",
  "video-quality-index",
  "video-fps-index",
  "hardware",
  "audio",
  "microphone",
  "microphone-index",
  "cursor",
  "gif-cursor",
  "clicks",
  "gif-clicks",
  "magnifier",
  "copy-text-direct",
  "capture-delay-index",
  "large-png-as-jpg",
  "filename",
  "folder-index",
  "subfolder",
];

pub const ACTIONS: &[&str] = &[
  "download-update",
  "import",
  "export",
  "notification",
  "open-screenshots",
  "open-videos",
  "pick-folder-together",
  "pick-folder-screenshots",
  "pick-folder-videos",
  "reset-folder-together",
  "reset-folder-screenshots",
  "reset-folder-videos",
  "reset-filename",
  "reset-subfolder",
  "developer-blog",
  "feedback",
  "bug",
  "copy-diagnostics",
  "logs",
  "rate",
  "privacy",
];

pub fn click(window: &slint::Window, x: f32, y: f32) {
  let position = slint::LogicalPosition::new(x, y);
  window.dispatch_event(WindowEvent::PointerPressed {
    position,
    button: PointerEventButton::Left,
  });
  window.dispatch_event(WindowEvent::PointerReleased {
    position,
    button: PointerEventButton::Left,
  });
}

fn wheel(window: &slint::Window, delta_y: f32) {
  let position = slint::LogicalPosition::new(400.0, 400.0);
  window.dispatch_event(WindowEvent::PointerMoved { position });
  window.dispatch_event(WindowEvent::PointerScrolled {
    position,
    delta_x: 0.0,
    delta_y,
  });
}

/// Rows below the fold are scrolled into view; the page returns to the top after.
pub(crate) fn click_shortcut_row(window: &SettingsWindow, y: f32) {
  let below_fold = y > 620.0;
  if below_fold {
    wheel(window.window(), -200.0);
  }
  click(window.window(), 500.0, y + window.get_content_scroll_y());
  if below_fold {
    wheel(window.window(), 200.0);
  }
}

fn key(window: &slint::Window, text: slint::SharedString) {
  window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
  window.dispatch_event(WindowEvent::KeyReleased { text });
}

/// The coordinates below assume the fixed settings window. In-window popups are clamped to the
/// window's real size, so a smaller window would move dropdown options away from their fixtures
/// and fail only mouse picks; name the geometry instead.
fn fixed_geometry(window: &SettingsWindow, locale: &str) -> Result<(), String> {
  let scale = window.window().scale_factor();
  let size = window.window().size().to_logical(scale);
  let (width, height) = (window.get_initial_width(), window.get_initial_height());
  if (size.width - width).abs() > 0.5 || (size.height - height).abs() > 0.5 {
    return Err(format!(
      "{locale}: interaction fixtures need the fixed {width}x{height} logical window, got {}x{} at scale {scale}",
      size.width, size.height
    ));
  }
  Ok(())
}

type Edits = Rc<RefCell<Vec<Result<PreferenceEdit, String>>>>;

fn expect(edits: &Edits, expected: PreferenceEdit, context: &str) -> Result<(), String> {
  let actual = std::mem::take(&mut *edits.borrow_mut());
  if actual != [Ok(expected.clone())] {
    return Err(format!(
      "{context}: expected one {expected:?}, received {actual:?}"
    ));
  }
  Ok(())
}

/// Hotkeys page rows, in visual order: capture target and vertical click position.
pub(crate) const SHORTCUT_ROWS: [(&str, f32); 11] = [
  ("screenshot-area", 110.0),
  ("screenshot-fullscreen", 170.0),
  ("screenshot-window", 229.0),
  ("screenshot-edit", 289.0),
  ("last-region", 349.0),
  ("gif", 453.0),
  ("video-area", 557.0),
  ("video-fullscreen", 616.0),
  ("video-window", 675.0),
  ("copy-text", 779.0),
  ("pick-color", 839.0),
];

pub(crate) const ABOUT_ACTIONS: [(&str, f32, f32); 7] = [
  ("developer-blog", 590.0, 61.0),
  ("feedback", 275.0, 103.0),
  ("bug", 400.0, 143.0),
  ("copy-diagnostics", 570.0, 226.0),
  ("logs", 606.0, 226.0),
  ("rate", 400.0, 492.0),
  ("privacy", 400.0, 183.0),
];

pub fn run(window: &SettingsWindow) -> Result<usize, String> {
  // Coordinate fixtures verify input and persistence with settled layouts; the timed
  // polish probe separately checks production transitions and search focus.
  window.global::<Interaction>().set_motion_enabled(false);
  use PreferenceEdit::*;
  let edits: Edits = Rc::new(RefCell::new(Vec::new()));
  let received = edits.clone();
  ui::connect_edits(window, move |edit| received.borrow_mut().push(edit));
  let captures = Rc::new(RefCell::new(Vec::new()));
  let received = captures.clone();
  window
    .global::<SettingsState>()
    .on_capture_request(move |action| received.borrow_mut().push(action.to_string()));
  let mut count = 0;
  let actions = Rc::new(RefCell::new(Vec::new()));
  let received = actions.clone();
  window
    .global::<SettingsState>()
    .on_action(move |action| received.borrow_mut().push(action.to_string()));
  let mut covered = std::collections::BTreeSet::new();
  let mut covered_actions = std::collections::BTreeSet::new();
  let resets = Rc::new(RefCell::new(0usize));
  let received = resets.clone();
  window
    .global::<SettingsState>()
    .on_reset_request(move || *received.borrow_mut() += 1);
  // Explicit expected values ensure a swapped field or option is caught. Each y is a point on the
  // dropdown's trigger, about 2.5 px below its centre (pages start 8 px below the search field).
  let dropdowns = [
    (
      "language",
      0,
      120.0,
      vec![
        Language("en".into()),
        Language("fr".into()),
        Language("de".into()),
        Language("es".into()),
        Language("it".into()),
      ],
    ),
    (
      "notification-duration-index",
      1,
      230.0,
      vec![
        NotificationDuration(1000),
        NotificationDuration(3000),
        NotificationDuration(5000),
        NotificationDuration(10000),
      ],
    ),
    (
      "notification-position-index",
      1,
      277.0,
      vec![
        NotificationPosition("top-right".into()),
        NotificationPosition("top-left".into()),
        NotificationPosition("bottom-right".into()),
        NotificationPosition("bottom-left".into()),
      ],
    ),
    (
      "format-index",
      3,
      120.0,
      vec![
        ScreenshotFormat("png".into()),
        ScreenshotFormat("jpg".into()),
        ScreenshotFormat("webp".into()),
      ],
    ),
    (
      "screenshot-quality-index",
      3,
      175.0,
      vec![
        ScreenshotQuality(50),
        ScreenshotQuality(75),
        ScreenshotQuality(90),
      ],
    ),
    (
      "gif-duration-index",
      3,
      325.0,
      vec![GifDuration(10), GifDuration(20), GifDuration(30)],
    ),
    ("gif-fps-index", 3, 380.0, vec![GifFps(15), GifFps(30)]),
    (
      "capture-delay-index",
      7,
      212.0,
      vec![
        CaptureDelay(0),
        CaptureDelay(3),
        CaptureDelay(5),
        CaptureDelay(10),
      ],
    ),
    (
      "video-quality-index",
      4,
      120.0,
      vec![
        VideoQuality("low".into()),
        VideoQuality("medium".into()),
        VideoQuality("high".into()),
      ],
    ),
    (
      "video-fps-index",
      4,
      175.0,
      vec![VideoFps(30), VideoFps(60)],
    ),
    (
      "folder-index",
      5,
      142.0,
      vec![
        SaveLocation("system_folders".into()),
        SaveLocation("single_folder".into()),
      ],
    ),
    (
      "microphone-index",
      4,
      520.0,
      vec![
        Microphone(None),
        Microphone(Some(crate::edit::MicrophoneChoice {
          id: "usb".into(),
          name: "USB Microphone".into(),
        })),
        Microphone(Some(crate::edit::MicrophoneChoice {
          id: "headset".into(),
          name: "Headset".into(),
        })),
      ],
    ),
  ];
  // The picker shows with Record Microphone on, listing two connected microphones.
  ui::refresh_microphones(
    window,
    &[
      ("usb".into(), "USB Microphone".into()),
      ("headset".into(), "Headset".into()),
    ],
  )?;
  for locale in crate::model::LOCALES {
    let mut snapshot: SettingsSnapshot =
      serde_json::from_str(include_str!("../../../resources/defaultConfig.json"))
        .map_err(|error| error.to_string())?;
    snapshot.language = locale.into();
    snapshot.notifications_enabled = true;
    snapshot.check_for_updates = true;
    snapshot.save_location_mode = "single_folder".into();
    snapshot.filename_template = Some("fixture".into());
    snapshot.single_folder_subfolder_template = Some("fixture".into());
    snapshot.custom_single_folder = Some("fixture".into());
    snapshot.custom_screenshots_folder = Some("fixture".into());
    snapshot.custom_videos_folder = Some("fixture".into());
    snapshot.record_microphone = true;
    let presentation = Presentation {
      locale: locale.into(),
      subfolder_preview: "fixture".into(),
      filename_preview: "fixture".into(),
      screenshots_path: "fixture".into(),
      videos_path: "fixture".into(),
      channel: "installer".into(),
    };
    ui::apply_snapshot(window, &snapshot, &presentation)?;
    window.show().map_err(|error| error.to_string())?;
    fixed_geometry(window, locale)?;
    let state = window.global::<SettingsState>();
    let mut errors = vec![slint::SharedString::default(); crate::edit::SLOT_COUNT];
    errors[1] = "Startup unavailable".into();
    errors[10] = "Could not save frame rate".into();
    state.set_field_errors(slint::ModelRc::new(slint::VecModel::from(errors)));
    state.set_save_error("Isolated write denied".into());
    for page in 0..8 {
      window.set_page(page);
      window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      if !window.get_save_notice_visible() {
        return Err(format!("{locale}: error notice lost errors on page {page}"));
      }
    }
    state.set_field_errors(slint::ModelRc::new(slint::VecModel::from(
      vec![slint::SharedString::default(); crate::edit::SLOT_COUNT],
    )));
    state.set_save_error("".into());
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    if window.get_save_notice_visible() {
      return Err(format!("{locale}: notice retained resolved errors"));
    }
    count += 10;
    for (field, page, y, choices) in &dropdowns {
      covered.insert(*field);
      ui::apply_snapshot(window, &snapshot, &presentation)?;
      window.global::<Interaction>().set_editable(true);
      window.set_page(*page);
      window.show().map_err(|error| error.to_string())?;
      window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      if !edits.borrow().is_empty() {
        return Err(format!("{locale}/{field}: refresh emitted an edit"));
      }
      for (index, expected) in choices.iter().enumerate() {
        for keyboard in [false, true] {
          let position_picker = *field == "notification-position-index";
          let folder_picker = *field == "folder-index";
          if position_picker {
            let x = [370.0, 270.0, 570.0, 470.0][index];
            click(window.window(), x, 310.0);
            if keyboard {
              expect(&edits, expected.clone(), "Position picker focus click")?;
            }
          } else if folder_picker {
            click(window.window(), 500.0, 107.0 + index as f32 * 66.0);
            if keyboard {
              expect(&edits, expected.clone(), "Folder picker focus click")?;
            }
          } else {
            click(window.window(), 540.0, *y);
          }
          window
            .window()
            .take_snapshot()
            .map_err(|error| error.to_string())?;
          if (position_picker || folder_picker) && keyboard {
            key(window.window(), "\n".into());
          } else if position_picker || folder_picker {
            // The visual option was clicked above.
          } else if keyboard {
            for _ in 0..choices.len() {
              key(window.window(), Key::UpArrow.into());
            }
            for _ in 0..index {
              key(window.window(), Key::DownArrow.into());
            }
            key(window.window(), "\n".into());
          } else {
            // The option's centre: half the 34 px trigger, the popup's 6 px gap and 4 px
            // padding, then half a 34 px option, from the trigger's centre. Aiming at the
            // centre, not an edge, keeps a small layout shift from missing the option.
            click(
              window.window(),
              540.0,
              *y - 2.5 + 44.0 + index as f32 * 34.0,
            );
          }
          window
            .window()
            .take_snapshot()
            .map_err(|error| error.to_string())?;
          expect(
            &edits,
            expected.clone(),
            &format!("{locale}/{field}/{index}/keyboard={keyboard}"),
          )?;
          window.set_page(6);
          window
            .window()
            .take_snapshot()
            .map_err(|error| error.to_string())?;
          window.set_page(*page);
          window
            .window()
            .take_snapshot()
            .map_err(|error| error.to_string())?;
          window
            .global::<SettingsState>()
            .invoke_edit_request((*field).into());
          expect(
            &edits,
            expected.clone(),
            &format!("{locale}/{field}: navigation restoration"),
          )?;
          // Rehydrate the choice from config rather than relying on the global
          // value surviving navigation. This catches incorrect load mappings.
          let mut restored = snapshot.clone();
          match expected {
            Language(value) => restored.language = value.clone(),
            NotificationDuration(value) => restored.notification_duration_ms = *value,
            NotificationPosition(value) => restored.notification_position = value.clone(),
            ScreenshotFormat(value) => restored.screenshot_format = value.clone(),
            ScreenshotQuality(value) => restored.screenshot_quality = *value,
            GifDuration(value) => restored.gif_max_duration = *value,
            CaptureDelay(value) => restored.capture_delay = *value,
            GifFps(value) => restored.gif_fps = *value,
            VideoQuality(value) => restored.video_quality = value.clone(),
            VideoFps(value) => restored.video_fps = *value,
            SaveLocation(value) => restored.save_location_mode = value.clone(),
            Microphone(choice) => {
              restored.microphone_device = choice.as_ref().map(|choice| choice.id.clone());
              restored.microphone_name = choice.as_ref().map(|choice| choice.name.clone());
            },
            _ => return Err("Unexpected dropdown restoration fixture".into()),
          }
          ui::apply_snapshot(
            window,
            &restored,
            &Presentation {
              locale: restored.language.clone(),
              subfolder_preview: "fixture".into(),
              filename_preview: "fixture".into(),
              screenshots_path: "fixture".into(),
              videos_path: "fixture".into(),
              channel: "installer".into(),
            },
          )?;
          window
            .window()
            .take_snapshot()
            .map_err(|error| error.to_string())?;
          if !edits.borrow().is_empty() {
            return Err(format!("{locale}/{field}: restoration emitted an edit"));
          }
          window
            .global::<SettingsState>()
            .invoke_edit_request((*field).into());
          expect(
            &edits,
            expected.clone(),
            &format!("{locale}/{field}: snapshot restoration"),
          )?;
          count += 1;
        }
      }
    }
    ui::apply_snapshot(window, &snapshot, &presentation)?;
    window.global::<Interaction>().set_editable(true);
    // Action callbacks use the actual production hit areas and focused keys.
    // No external program, file picker or clipboard is invoked by this fixture.
    for (action, page, x, y) in [
      ("download-update", 0, 280.0, 252.0),
      ("import", 0, 280.0, 303.0),
      ("export", 0, 280.0, 351.0),
      ("open-screenshots", 5, 300.0, 257.0),
      ("open-videos", 5, 300.0, 309.0),
      ("pick-folder-together", 5, 560.0, 251.0),
      ("pick-folder-screenshots", 5, 560.0, 257.0),
      ("pick-folder-videos", 5, 560.0, 309.0),
      ("reset-folder-together", 5, 598.0, 251.0),
      ("reset-folder-screenshots", 5, 598.0, 257.0),
      ("reset-folder-videos", 5, 598.0, 309.0),
    ]
    .into_iter()
    .chain(ABOUT_ACTIONS.map(|(action, x, y)| (action, 6, x, y)))
    {
      ui::apply_snapshot(window, &snapshot, &presentation)?;
      if page == 6 {
        window.global::<SettingsState>().set_channel("msix".into());
      }
      window.global::<SettingsState>().set_update_version(
        if action == "download-update" {
          "1.2.0"
        } else {
          ""
        }
        .into(),
      );
      window
        .global::<SettingsState>()
        .set_folder_index(i32::from(action.ends_with("-together")));
      window.set_page(page);
      window.global::<SettingsState>().set_action_error("".into());
      window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      for keyboard in [false, true] {
        actions.borrow_mut().clear();
        click(window.window(), x, y);
        if keyboard {
          if std::mem::take(&mut *actions.borrow_mut()) != [action] {
            return Err(format!("{locale}/{action}: focus click did not dispatch"));
          }
          key(window.window(), "\n".into());
        }
        if std::mem::take(&mut *actions.borrow_mut()) != [action] {
          return Err(format!(
            "{locale}/{action}: action routing failed (keyboard={keyboard})"
          ));
        }
        count += 1;
      }
      window.global::<Interaction>().set_editable(false);
      click(window.window(), x, y);
      key(window.window(), "\n".into());
      if !actions.borrow().is_empty() {
        return Err(format!("{locale}/{action}: disabled action dispatched"));
      }
      window.global::<Interaction>().set_editable(true);
      let before = window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      let state = window.global::<SettingsState>();
      state.set_error_action(action.into());
      state.set_action_error("Injected action failure".into());
      // Match the native presenter: only links handed to the browser stay quiet.
      let folder_failure = FolderTarget::from_pick_action(action).is_some();
      let visible_failure = folder_failure
        || matches!(
          action,
          "import" | "export" | "open-screenshots" | "open-videos" | "logs" | "copy-diagnostics"
        );
      let message = if visible_failure {
        crate::catalog::Catalog::load(locale)?
          .text(match action {
            "import" => "native_import_error",
            "export" => "native_export_error",
            "logs" => "native_open_logs_error",
            "copy-diagnostics" => "native_copy_error",
            _ if folder_failure => "native_folder_error",
            _ => "native_open_folder_error",
          })?
          .to_owned()
      } else {
        String::new()
      };
      state.set_action_message(message.into());
      let failed = window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      if visible_failure && before.as_bytes() == failed.as_bytes() {
        return Err(format!("{locale}/{action}: inline failure was invisible"));
      }
      if !visible_failure && before.as_bytes() != failed.as_bytes() {
        return Err(format!(
          "{locale}/{action}: diagnostic-only failure changed the UI"
        ));
      }
      state.set_action_error("".into());
      state.set_action_message("".into());
      covered_actions.insert(action);
      count += 2;
    }
    window
      .global::<SettingsState>()
      .set_update_version("".into());
    ui::apply_snapshot(window, &snapshot, &presentation)?;
    window.set_page(0);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    *resets.borrow_mut() = 0;
    click(window.window(), 280.0, 399.0);
    key(window.window(), "\n".into());
    if *resets.borrow() != 2 {
      return Err(format!("{locale}: reset did not request confirmation"));
    }
    window.global::<Interaction>().set_editable(false);
    click(window.window(), 280.0, 399.0);
    if *resets.borrow() != 2 {
      return Err(format!("{locale}: disabled reset requested confirmation"));
    }
    window.global::<Interaction>().set_editable(true);
    count += 3;
    window.set_page(1);
    window.global::<SettingsState>().set_preview_cooldown(0);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    for keyboard in [false, true] {
      click(window.window(), 580.0, 266.0);
      if keyboard {
        actions.borrow_mut().clear();
        key(window.window(), "\n".into());
      }
      if std::mem::take(&mut *actions.borrow_mut()) != ["notification"] {
        return Err(format!(
          "{locale}: heading test action failed (keyboard={keyboard})"
        ));
      }
      count += 1;
    }
    covered_actions.insert("notification");
    window.global::<SettingsState>().set_preview_cooldown(3);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    click(window.window(), 580.0, 266.0);
    if !actions.borrow().is_empty() {
      return Err(format!("{locale}: notification test ignored cooldown"));
    }
    window.global::<SettingsState>().set_preview_cooldown(0);
    for (field, page, y, expected) in [
      ("boot", 0, 167.0, AutoStart(true)),
      ("updates", 0, 210.0, CheckForUpdates(false)),
      ("notifications", 1, 112.0, NotificationsEnabled(false)),
      (
        "sound",
        1,
        155.0,
        NotificationSound(!snapshot.notification_sound),
      ),
      (
        "hardware",
        4,
        222.0,
        HardwareEncoding(!snapshot.video_hardware_encoding),
      ),
      ("audio", 4, 265.0, RecordAudio(!snapshot.record_audio)),
      ("cursor", 4, 314.0, ShowCursor(!snapshot.show_cursor)),
      ("clicks", 4, 363.0, ShowClicks(!snapshot.show_clicks)),
      (
        "microphone",
        4,
        457.0,
        RecordMicrophone(!snapshot.record_microphone),
      ),
      (
        "large-png-as-jpg",
        3,
        219.0,
        LargePngAsJpg(!snapshot.screenshot_large_png_as_jpg),
      ),
      (
        "gif-cursor",
        3,
        424.0,
        GifShowCursor(!snapshot.gif_show_cursor),
      ),
      (
        "gif-clicks",
        3,
        473.0,
        GifShowClicks(!snapshot.gif_show_clicks),
      ),
      (
        "magnifier",
        7,
        109.0,
        SelectionMagnifier(!snapshot.selection_magnifier),
      ),
      (
        "copy-text-direct",
        7,
        158.0,
        CopyTextDirect(!snapshot.copy_text_direct),
      ),
    ] {
      covered.insert(field);
      ui::apply_snapshot(window, &snapshot, &presentation)?;
      window.set_page(page);
      window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      click(window.window(), 575.0, y);
      expect(
        &edits,
        expected.clone(),
        &format!("{locale}/{field}: toggle"),
      )?;
      let original = match expected {
        AutoStart(value) => AutoStart(!value),
        CheckForUpdates(value) => CheckForUpdates(!value),
        NotificationsEnabled(value) => NotificationsEnabled(!value),
        NotificationSound(value) => NotificationSound(!value),
        HardwareEncoding(value) => HardwareEncoding(!value),
        RecordAudio(value) => RecordAudio(!value),
        ShowCursor(value) => ShowCursor(!value),
        GifShowCursor(value) => GifShowCursor(!value),
        ShowClicks(value) => ShowClicks(!value),
        GifShowClicks(value) => GifShowClicks(!value),
        RecordMicrophone(value) => RecordMicrophone(!value),
        SelectionMagnifier(value) => SelectionMagnifier(!value),
        CopyTextDirect(value) => CopyTextDirect(!value),
        LargePngAsJpg(value) => LargePngAsJpg(!value),
        _ => return Err("Unexpected toggle fixture".into()),
      };
      // The mouse click focused the real Switch. Space must save its reverse edit.
      key(window.window(), " ".into());
      expect(
        &edits,
        original,
        &format!("{locale}/{field}: keyboard toggle"),
      )?;
      count += 2;
    }
    ui::apply_snapshot(window, &snapshot, &presentation)?;
    window.set_page(5);
    window
      .global::<SettingsState>()
      .set_filename_customization_expanded(false);
    window
      .global::<SettingsState>()
      .set_subfolder_customization_expanded(false);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    click(window.window(), 540.0, 324.0);
    if !window
      .global::<SettingsState>()
      .get_filename_customization_expanded()
    {
      return Err(format!("{locale}: Files customization did not open"));
    }
    key(window.window(), "\n".into());
    if window
      .global::<SettingsState>()
      .get_filename_customization_expanded()
    {
      return Err(format!(
        "{locale}: Files customization did not close with keyboard"
      ));
    }
    key(window.window(), "\n".into());
    if !window
      .global::<SettingsState>()
      .get_filename_customization_expanded()
      || !edits.borrow().is_empty()
    {
      return Err(format!(
        "{locale}: Files customization changed preferences or failed to reopen"
      ));
    }
    count += 3;
    click(window.window(), 602.0, 324.0);
    if std::mem::take(&mut *actions.borrow_mut()) != ["reset-filename"] {
      return Err(format!("{locale}: Filename header reset did not activate"));
    }
    covered_actions.insert("reset-filename");
    key(window.window(), "\n".into());
    if std::mem::take(&mut *actions.borrow_mut()) != ["reset-filename"] {
      return Err(format!(
        "{locale}: Filename header reset did not activate with keyboard"
      ));
    }
    count += 2;
    window
      .global::<SettingsState>()
      .set_filename_customization_expanded(false);
    window
      .global::<SettingsState>()
      .set_subfolder_customization_expanded(false);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    click(window.window(), 540.0, 281.0);
    if !window
      .global::<SettingsState>()
      .get_subfolder_customization_expanded()
      || window
        .global::<SettingsState>()
        .get_filename_customization_expanded()
    {
      return Err(format!(
        "{locale}: Subfolder customization did not open independently"
      ));
    }
    key(window.window(), "\n".into());
    if window
      .global::<SettingsState>()
      .get_subfolder_customization_expanded()
    {
      return Err(format!(
        "{locale}: Subfolder customization did not close with keyboard"
      ));
    }
    key(window.window(), "\n".into());
    if !window
      .global::<SettingsState>()
      .get_subfolder_customization_expanded()
      || !edits.borrow().is_empty()
    {
      return Err(format!(
        "{locale}: Subfolder customization changed preferences or failed to reopen"
      ));
    }
    count += 3;
    click(window.window(), 602.0, 281.0);
    if std::mem::take(&mut *actions.borrow_mut()) != ["reset-subfolder"] {
      return Err(format!("{locale}: Subfolder header reset did not activate"));
    }
    covered_actions.insert("reset-subfolder");
    key(window.window(), "\n".into());
    if std::mem::take(&mut *actions.borrow_mut()) != ["reset-subfolder"] {
      return Err(format!(
        "{locale}: Subfolder header reset did not activate with keyboard"
      ));
    }
    count += 2;
    // At their defaults there is nothing to reset, so the controls stay inert.
    {
      let state = window.global::<SettingsState>();
      state.set_filename_customization_expanded(false);
      state.set_subfolder_customization_expanded(false);
      state.set_filename(DEFAULT_FILENAME_TEMPLATE.into());
      state.set_subfolder(DEFAULT_SUBFOLDER_TEMPLATE.into());
      window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      for (x, y) in [(602.0, 324.0), (602.0, 281.0)] {
        click(window.window(), x, y);
      }
      if !actions.borrow().is_empty() {
        return Err(format!("{locale}: reset at a default value dispatched"));
      }
      // The preview's extension follows the screenshot format.
      let png = window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      state.set_format_index(2);
      let webp = window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      if png.as_bytes() == webp.as_bytes() {
        return Err(format!("{locale}: filename preview ignored the format"));
      }
      count += 2;
    }
    for (field, y, expected) in [
      ("filename", 405.0, Filename("fixturex".into())),
      ("subfolder", 327.0, Subfolder("fixturex".into())),
    ] {
      covered.insert(field);
      ui::apply_snapshot(window, &snapshot, &presentation)?;
      window.set_page(5);
      window
        .global::<SettingsState>()
        .set_filename_customization_expanded(field == "filename");
      window
        .global::<SettingsState>()
        .set_subfolder_customization_expanded(field == "subfolder");
      window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      click(window.window(), 380.0, y);
      key(window.window(), Key::End.into());
      key(window.window(), "x".into());
      expect(&edits, expected, &format!("{locale}/{field}: typing"))?;
      count += 1;
      key(window.window(), " ".into());
      expect(
        &edits,
        if field == "filename" {
          Filename("fixturex ".into())
        } else {
          Subfolder("fixturex ".into())
        },
        &format!("{locale}/{field}: trailing space while typing"),
      )?;
      key(window.window(), "y".into());
      expect(
        &edits,
        if field == "filename" {
          Filename("fixturex y".into())
        } else {
          Subfolder("fixturex y".into())
        },
        &format!("{locale}/{field}: word after space"),
      )?;
      count += 2;
      window
        .global::<SettingsState>()
        .set_filename("%APP%_test".into());
      window
        .global::<SettingsState>()
        .set_subfolder("%YEAR%_test".into());
      window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      window.window().dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(239.0, y),
        button: PointerEventButton::Left,
      });
      for x in [248.0, 264.0, 280.0, 264.0, 248.0] {
        window.window().dispatch_event(WindowEvent::PointerMoved {
          position: slint::LogicalPosition::new(x, y),
        });
        let first = window
          .window()
          .take_snapshot()
          .map_err(|error| error.to_string())?;
        window.window().dispatch_event(WindowEvent::PointerMoved {
          position: slint::LogicalPosition::new(x, y),
        });
        let repeated = window
          .window()
          .take_snapshot()
          .map_err(|error| error.to_string())?;
        if first.as_bytes() != repeated.as_bytes() {
          return Err(format!(
            "{locale}/{field}: selection changed at a stationary pointer"
          ));
        }
      }
      count += 1;
      window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(550.0, y),
      });
      window
        .window()
        .dispatch_event(WindowEvent::PointerReleased {
          position: slint::LogicalPosition::new(550.0, y),
          button: PointerEventButton::Left,
        });
      if field == "filename"
        && locale == "en"
        && let Some(path) = std::env::var_os("FLINCH_TEMPLATE_SELECTION_PREVIEW_OUTPUT")
      {
        let buffer = window
          .window()
          .take_snapshot()
          .map_err(|error| error.to_string())?;
        let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
        let mut encoder = png::Encoder::new(file, buffer.width(), buffer.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
          .write_header()
          .map_err(|error| error.to_string())?
          .write_image_data(buffer.as_bytes())
          .map_err(|error| error.to_string())?;
      }
      key(window.window(), "x".into());
      expect(
        &edits,
        if field == "filename" {
          Filename("x".into())
        } else {
          Subfolder("x".into())
        },
        &format!("{locale}/{field}: mouse selection replacement"),
      )?;
      count += 1;
      if field == "filename" && locale == "en" {
        window
          .global::<SettingsState>()
          .set_filename("%DATETIME%_%APP%_%TYPE%_test".into());
        if let Some(path) = std::env::var_os("FLINCH_TEMPLATE_PREVIEW_OUTPUT") {
          let buffer = window
            .window()
            .take_snapshot()
            .map_err(|error| error.to_string())?;
          let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
          let mut encoder = png::Encoder::new(file, buffer.width(), buffer.height());
          encoder.set_color(png::ColorType::Rgba);
          encoder.set_depth(png::BitDepth::Eight);
          encoder
            .write_header()
            .map_err(|error| error.to_string())?
            .write_image_data(buffer.as_bytes())
            .map_err(|error| error.to_string())?;
        }
      }
      if field == "subfolder" {
        window
          .global::<SettingsState>()
          .set_subfolder("%YEAR%-%MONTH%-%DATETIME%-%APP%".into());
        window
          .window()
          .take_snapshot()
          .map_err(|error| error.to_string())?;
        key(window.window(), Key::End.into());
        key(window.window(), Key::Backspace.into());
        expect(
          &edits,
          Subfolder("%YEAR%-%MONTH%-%DATETIME%-%APP".into()),
          &format!("{locale}/{field}: unsupported token remains editable literal text"),
        )?;
        count += 1;
      }
      if field == "filename" {
        window
          .global::<SettingsState>()
          .set_filename("prefix_%APP%".into());
      } else {
        window
          .global::<SettingsState>()
          .set_subfolder("prefix_%YEAR%".into());
      }
      window
        .window()
        .take_snapshot()
        .map_err(|error| error.to_string())?;
      key(window.window(), Key::End.into());
      key(window.window(), Key::Backspace.into());
      expect(
        &edits,
        if field == "filename" {
          Filename("prefix_".into())
        } else {
          Subfolder("prefix_".into())
        },
        &format!("{locale}/{field}: whole tag deletion"),
      )?;
      count += 1;
    }
    ui::apply_snapshot(window, &snapshot, &presentation)?;
    window.global::<Interaction>().set_hotkeys_editable(true);
    window.set_page(2);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    for (action, y) in SHORTCUT_ROWS {
      click_shortcut_row(window, y);
      let actual = std::mem::take(&mut *captures.borrow_mut());
      if actual != [action] {
        return Err(format!("{locale}: shortcut {action} emitted {actual:?}"));
      }
      count += 1;
    }
    // A Windows startup lookup failure disables only its own control.
    window
      .global::<SettingsState>()
      .set_startup_unavailable(true);
    window.set_page(0);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    click(window.window(), 575.0, 165.0);
    if !edits.borrow().is_empty() {
      return Err(format!(
        "{locale}: unavailable startup control emitted edits"
      ));
    }
    window.set_page(4);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    let before = window.global::<SettingsState>().get_audio();
    click(window.window(), 575.0, 265.0);
    expect(
      &edits,
      RecordAudio(!before),
      "startup failure left audio interactive",
    )?;
    window
      .global::<SettingsState>()
      .set_startup_unavailable(false);
    count += 2;
    // Disabled controls must not mutate or enqueue changes while loading/saving.
    window.set_page(4);
    window
      .window()
      .take_snapshot()
      .map_err(|error| error.to_string())?;
    window.global::<Interaction>().set_editable(false);
    click(window.window(), 540.0, 112.0);
    click(window.window(), 575.0, 265.0);
    if !edits.borrow().is_empty() {
      return Err(format!("{locale}: disabled controls emitted edits"));
    }
    window.global::<Interaction>().set_editable(true);
  }
  if covered != FIELDS.iter().copied().collect() {
    return Err(format!(
      "Settings interaction registry differs from executed cases: {covered:?}"
    ));
  }
  if covered_actions != ACTIONS.iter().copied().collect() {
    return Err(format!(
      "Settings action registry differs from executed cases: {covered_actions:?}"
    ));
  }
  window.hide().map_err(|error| error.to_string())?;
  Ok(count)
}
