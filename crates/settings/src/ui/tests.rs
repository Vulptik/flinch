use super::*;
use slint::{
  Model,
  platform::{Platform, PlatformError, WindowAdapter, software_renderer::MinimalSoftwareWindow},
};
use std::cell::{Cell, RefCell};

struct Headless;
thread_local! {
  static SOFTWARE_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}
impl Platform for Headless {
  fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
    let adapter = MinimalSoftwareWindow::new(Default::default());
    SOFTWARE_WINDOW.with(|current| *current.borrow_mut() = Some(adapter.clone()));
    Ok(adapter)
  }
}

fn render_text_panel(window: &TextPanelWindow, name: &str) {
  use slint::Rgb8Pixel;
  window.window().request_redraw();
  let mut pixels = vec![Rgb8Pixel::default(); 520 * 380];
  SOFTWARE_WINDOW.with(|current| {
    current
      .borrow()
      .as_ref()
      .expect("headless renderer")
      .draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 520);
      });
  });
  if let Some(directory) = std::env::var_os("FLINCH_SETTINGS_ERROR_PREVIEW_OUTPUT") {
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).expect("preview directory");
    let file = std::fs::File::create(directory.join(format!("{name}.png"))).expect("preview file");
    let mut encoder = png::Encoder::new(file, 520, 380);
    encoder.set_color(png::ColorType::Rgb);
    let bytes = pixels
      .iter()
      .flat_map(|p| [p.r, p.g, p.b])
      .collect::<Vec<_>>();
    encoder
      .write_header()
      .expect("PNG header")
      .write_image_data(&bytes)
      .expect("PNG pixels");
  }
}

fn render_errors(window: &SettingsWindow, name: &str) {
  use slint::Rgb8Pixel;
  window.window().request_redraw();
  let mut pixels = vec![Rgb8Pixel::default(); 640 * 640];
  SOFTWARE_WINDOW.with(|current| {
    current
      .borrow()
      .as_ref()
      .expect("headless renderer")
      .draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 640);
      });
  });
  if let Some(directory) = std::env::var_os("FLINCH_SETTINGS_ERROR_PREVIEW_OUTPUT") {
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).expect("preview directory");
    let file = std::fs::File::create(directory.join(format!("{name}.png"))).expect("preview file");
    let mut encoder = png::Encoder::new(file, 640, 640);
    encoder.set_color(png::ColorType::Rgb);
    let bytes = pixels
      .iter()
      .flat_map(|p| [p.r, p.g, p.b])
      .collect::<Vec<_>>();
    encoder
      .write_header()
      .expect("PNG header")
      .write_image_data(&bytes)
      .expect("PNG pixels");
  }
}

fn defaults() -> SettingsSnapshot {
  serde_json::from_str(include_str!("../../../../resources/defaultConfig.json"))
    .expect("bundled settings")
}

fn presentation(locale: &str, channel: &str) -> Presentation {
  Presentation {
    locale: locale.into(),
    channel: channel.into(),
    filename_preview: "capture.png".into(),
    subfolder_preview: "2026-10".into(),
    screenshots_path: r"C:\isolated\pictures".into(),
    videos_path: r"C:\isolated\videos".into(),
  }
}

// One owner thread installs the headless platform for all scenarios. Only
// offscreen adapters are used; no native event loop or user config is involved.
#[test]
fn headless_production_adapters_preserve_edits_errors_search_and_close_contracts() {
  slint::platform::set_platform(Box::new(Headless)).expect("headless platform");
  snapshot_and_edits();
  invalid_selections();
  error_notice();
  search_and_templates();
  close_and_reset();
  hotkey_rows();
  about_actions();
  text_panel_states();
  screenshot_toggles();
  tray::menu_states();
  tray::recent_states();
  tray::choice_states();
  tray::delay_row_states();
  microphone_picker();
}

/// The picker lists what Windows reports, keeps a choice selected across refreshes and turns
/// the chosen row into an edit; the meter only clamps what it is given.
fn microphone_picker() {
  use PreferenceEdit::*;
  let window = SettingsWindow::new().expect("headless component");
  let edits = Rc::new(RefCell::new(Vec::new()));
  let received = edits.clone();
  connect_edits(&window, move |edit| received.borrow_mut().push(edit));
  apply_snapshot(&window, &defaults(), &presentation("en", "msix")).expect("snapshot");
  let available = [
    ("{mic-a}".to_string(), "Headset".to_string()),
    ("{mic-b}".to_string(), "Desk Microphone".to_string()),
  ];
  refresh_microphones(&window, &available).expect("listed");
  let state = window.global::<SettingsState>();
  assert_eq!(state.get_microphone_options().row_count(), 3);
  assert_eq!(state.get_microphone_index(), 0, "Windows Default first");
  state.set_microphone_index(2);
  state.invoke_edit_request("microphone-index".into());
  assert_eq!(
    edits.borrow_mut().pop(),
    Some(Ok(Microphone(Some(crate::edit::MicrophoneChoice {
      id: "{mic-b}".into(),
      name: "Desk Microphone".into(),
    }))))
  );
  // Unplugged, it stays listed and selected.
  refresh_microphones(&window, &available[..1]).expect("listed again");
  assert_eq!(state.get_microphone_options().row_count(), 3);
  assert_eq!(state.get_microphone_index(), 2);
  assert_eq!(
    state.get_microphone_ids().row_data(2).as_deref(),
    Some("{mic-b}")
  );
  // A selection past the list is an error, never a guess.
  state.set_microphone_index(9);
  state.invoke_edit_request("microphone-index".into());
  assert!(matches!(edits.borrow_mut().pop(), Some(Err(_))));
  state.set_microphone_index(-1);
  state.invoke_edit_request("microphone-index".into());
  assert!(matches!(edits.borrow_mut().pop(), Some(Err(_))));
  set_microphone_level(&window, 1.7, "Blocked");
  assert_eq!(state.get_microphone_level(), 1.0);
  assert_eq!(state.get_microphone_problem(), "Blocked");
  set_microphone_level(&window, -0.5, "");
  assert_eq!(state.get_microphone_level(), 0.0);
  // The saved choice comes back with the next snapshot.
  let mut snapshot = defaults();
  snapshot.microphone_device = Some("{mic-a}".into());
  snapshot.microphone_name = Some("Headset".into());
  apply_snapshot(&window, &snapshot, &presentation("en", "msix")).expect("snapshot");
  assert_eq!(state.get_microphone_index(), 1);
}

#[path = "tray_tests.rs"]
mod tray;

fn about_actions() {
  let window = SettingsWindow::new().expect("headless component");
  window
    .window()
    .set_size(slint::LogicalSize::new(640., 640.));
  window.show().expect("offscreen component");
  window.global::<Interaction>().set_motion_enabled(false);
  let actions = Rc::new(RefCell::new(Vec::new()));
  let received = actions.clone();
  window
    .global::<SettingsState>()
    .on_action(move |action| received.borrow_mut().push(action.to_string()));
  window.global::<Interaction>().set_editable(true);
  window.set_page(6);
  for locale in LOCALES {
    apply_snapshot(&window, &defaults(), &presentation(locale, "msix")).expect("snapshot");
    render_errors(&window, &format!("{locale}-about"));
    for (action, x, y) in crate::interaction_check::ABOUT_ACTIONS {
      crate::interaction_check::click(window.window(), x, y);
      assert_eq!(*actions.borrow(), [action], "{locale}: {action} at {x},{y}");
      actions.borrow_mut().clear();
    }
    apply_snapshot(&window, &defaults(), &presentation(locale, "installer")).expect("snapshot");
    render_errors(&window, &format!("{locale}-about-installer"));
    crate::interaction_check::click(window.window(), 400.0, 492.0);
    assert!(
      actions.borrow().is_empty(),
      "{locale}: installer builds have no Store rating"
    );
    // The shortened update link must still cover its fixture click point.
    window
      .global::<SettingsState>()
      .set_update_version("1.2.0".into());
    window.set_page(0);
    render_errors(&window, &format!("{locale}-update"));
    crate::interaction_check::click(window.window(), 280.0, 252.0);
    assert_eq!(
      *actions.borrow(),
      ["download-update"],
      "{locale}: update link"
    );
    actions.borrow_mut().clear();
    window
      .global::<SettingsState>()
      .set_update_version("".into());
    window.set_page(6);
  }
}

fn snapshot_and_edits() {
  use PreferenceEdit::*;
  let window = SettingsWindow::new().expect("headless component");
  let edits = Rc::new(RefCell::new(Vec::new()));
  let received = edits.clone();
  connect_edits(&window, move |edit| received.borrow_mut().push(edit));
  let mut snapshot = defaults();
  snapshot.notification_duration_ms = 2500;
  snapshot.show_cursor = false;
  snapshot.gif_show_cursor = true;
  snapshot.selection_magnifier = true;
  snapshot.copy_text_direct = true;
  snapshot.screenshot_large_png_as_jpg = true;
  snapshot.filename_template = Some("%APP%_%DATE%".into());
  for locale in LOCALES {
    apply_snapshot(&window, &snapshot, &presentation(locale, "installer"))
      .expect("project snapshot");
    assert!(
      edits.borrow().is_empty(),
      "projection must never emit edits"
    );
    let state = window.global::<SettingsState>();
    assert_eq!(state.get_filename_preview(), "capture.png");
    assert_eq!(state.get_subfolder_preview(), "2026-10");
    assert_eq!(state.get_screenshots_path(), r"C:\isolated\pictures");
    assert_eq!(state.get_videos_path(), r"C:\isolated\videos");
    let expected = [
      ("language", Language(locale.into())),
      ("boot", AutoStart(snapshot.auto_start)),
      ("updates", CheckForUpdates(snapshot.check_for_updates)),
      (
        "notifications",
        NotificationsEnabled(snapshot.notifications_enabled),
      ),
      ("sound", NotificationSound(snapshot.notification_sound)),
      ("notification-duration-index", NotificationDuration(2500)),
      (
        "notification-position-index",
        NotificationPosition(snapshot.notification_position.clone()),
      ),
      (
        "format-index",
        ScreenshotFormat(snapshot.screenshot_format.clone()),
      ),
      (
        "screenshot-quality-index",
        ScreenshotQuality(snapshot.screenshot_quality),
      ),
      ("gif-fps-index", GifFps(snapshot.gif_fps)),
      ("gif-duration-index", GifDuration(snapshot.gif_max_duration)),
      ("video-fps-index", VideoFps(snapshot.video_fps)),
      (
        "video-quality-index",
        VideoQuality(snapshot.video_quality.clone()),
      ),
      (
        "hardware",
        HardwareEncoding(snapshot.video_hardware_encoding),
      ),
      ("audio", RecordAudio(snapshot.record_audio)),
      ("cursor", ShowCursor(false)),
      ("gif-cursor", GifShowCursor(true)),
      ("clicks", ShowClicks(snapshot.show_clicks)),
      ("gif-clicks", GifShowClicks(snapshot.gif_show_clicks)),
      ("microphone", RecordMicrophone(snapshot.record_microphone)),
      ("microphone-index", Microphone(None)),
      ("capture-delay-index", CaptureDelay(snapshot.capture_delay)),
      ("magnifier", SelectionMagnifier(true)),
      ("copy-text-direct", CopyTextDirect(true)),
      ("large-png-as-jpg", LargePngAsJpg(true)),
      (
        "folder-index",
        SaveLocation(snapshot.save_location_mode.clone()),
      ),
      ("filename", Filename("%APP%_%DATE%".into())),
      (
        "subfolder",
        Subfolder(
          snapshot
            .single_folder_subfolder_template
            .clone()
            .unwrap_or_default(),
        ),
      ),
    ];
    for (field, expected) in expected {
      state.invoke_edit_request(field.into());
      assert_eq!(
        edits.borrow_mut().pop(),
        Some(Ok(expected)),
        "{locale}: {field}"
      );
      assert!(edits.borrow().is_empty(), "exactly one edit per request");
    }
  }
  let state = window.global::<SettingsState>();
  assert_eq!(
    state.get_shortcut_area().iter().collect::<Vec<_>>(),
    snapshot
      .screenshot_area_shortcut
      .split('+')
      .filter(|s| !s.is_empty())
      .map(SharedString::from)
      .collect::<Vec<_>>()
  );
}

fn invalid_selections() {
  let window = SettingsWindow::new().expect("headless component");
  apply_snapshot(&window, &defaults(), &presentation("en", "installer")).expect("snapshot");
  let state = window.global::<SettingsState>();
  for index in [-1, 99] {
    state.set_format_index(index);
    assert!(read_edit(&window, "format-index").is_err());
    state.set_notification_duration_index(index);
    assert!(read_edit(&window, "notification-duration-index").is_err());
    window.set_language_index(index);
    assert!(read_edit(&window, "language").is_err());
  }
  state.set_notification_duration_index(0);
  for duration in ["NaNs", "infs", "0.5s", "11s", "3", "bad"] {
    state.set_notification_durations(strings([duration.into()]));
    assert!(
      read_edit(&window, "notification-duration-index").is_err(),
      "{duration}"
    );
  }
  assert!(
    read_edit(&window, "missing-field")
      .unwrap_err()
      .contains("missing-field")
  );
  window.set_language_index(0);
  let mut snapshot = defaults();
  snapshot.video_fps = 31;
  let before = state.get_video_fps_index();
  assert!(apply_snapshot(&window, &snapshot, &presentation("en", "installer")).is_err());
  assert_eq!(
    state.get_video_fps_index(),
    before,
    "invalid snapshot must not partially apply"
  );
}

fn error_notice() {
  let window = SettingsWindow::new().expect("headless component");
  window.global::<Interaction>().set_motion_enabled(false);
  window.global::<Interaction>().set_editable(true);
  connect_search(&window).expect("template presentation callbacks");
  window
    .window()
    .set_size(slint::LogicalSize::new(640., 640.));
  window.show().expect("offscreen component");
  let actions = Rc::new(RefCell::new(Vec::new()));
  let received = actions.clone();
  window
    .global::<SettingsState>()
    .on_action(move |action| received.borrow_mut().push(action));
  for locale in LOCALES {
    apply_snapshot(&window, &defaults(), &presentation(locale, "installer")).expect("snapshot");
    let state = window.global::<SettingsState>();
    let catalog = Catalog::load(locale).expect("error copy");
    let validation_message = catalog
      .text("native_template_character")
      .expect("validation copy")
      .replace("{value}", ":");
    state.set_filename_error(validation_message.clone().into());
    state.set_shortcut_conflicts(Rc::new(VecModel::from(vec![true; 9])).into());
    assert!(
      !window.get_save_notice_visible(),
      "{locale}: validation failures stay contextual"
    );
    assert!(state.get_page_has_error().row_data(5).expect("Files"));
    assert!(state.get_page_has_error().row_data(2).expect("Hotkeys"));
    assert!(!state.get_page_has_error().row_data(4).expect("Videos"));
    state.set_filename("bad:name".into());
    window.set_page(5);
    render_errors(&window, &format!("{locale}-validation"));
    window.set_page(0);
    state.set_error_action("import".into());
    state.set_action_error("Cannot import settings".into());
    state.set_action_message(
      catalog
        .text("native_import_error")
        .expect("import copy")
        .into(),
    );
    render_errors(&window, &format!("{locale}-import"));
    window.set_page(1);
    assert!(
      state.get_action_message().is_empty(),
      "{locale}: an action error does not follow you to other pages"
    );
    window.set_page(0);
    let mut errors = vec![SharedString::default(); crate::edit::SLOT_COUNT];
    errors[14] = "video cursor failed".into();
    errors[26] = "GIF cursor failed".into();
    state.set_field_errors(Rc::new(VecModel::from(errors)).into());
    state.set_save_error("write denied by Windows".into());
    assert!(window.get_save_notice_visible());
    assert!(state.get_page_has_error().row_data(4).expect("Videos"));
    assert!(state.get_page_has_error().row_data(3).expect("Screenshots"));
    render_errors(&window, &format!("{locale}-save"));
    crate::interaction_check::click(
      window.window(),
      window.get_save_retry_x(),
      window.get_save_retry_y(),
    );
    assert_eq!(
      actions.borrow_mut().pop().as_deref(),
      Some("retry"),
      "{locale}: Retry dispatches once"
    );
    assert!(actions.borrow().is_empty());
    window.set_page(6);
    assert!(
      window.get_save_notice_visible(),
      "navigation retains unsaved status"
    );
    state.set_save_failed(false);
    state.set_saving(true);
    assert!(
      window.get_save_notice_visible(),
      "retry retains the notice until acknowledgment"
    );
    render_errors(&window, &format!("{locale}-retrying"));
    crate::interaction_check::click(
      window.window(),
      window.get_save_retry_x(),
      window.get_save_retry_y(),
    );
    assert!(
      actions.borrow().is_empty(),
      "retry is disabled while saving"
    );
    state.set_saving(false);
    state.set_save_error("".into());
    assert!(!window.get_save_notice_visible());
    state.set_close_timeout(true);
    assert!(window.get_save_notice_visible(), "a slow close says so");
    render_errors(&window, &format!("{locale}-closing"));
    state.set_close_timeout(false);
    state.set_filename_error("".into());
    state.set_shortcut_conflicts(Rc::new(VecModel::from(vec![false; 9])).into());
    state.set_action_error("".into());
    state.set_action_message("".into());
    state.set_field_errors(Rc::new(VecModel::from(vec![SharedString::default(); 30])).into());
  }
}

fn screenshot_toggles() {
  use PreferenceEdit::*;
  let window = SettingsWindow::new().expect("headless component");
  window
    .window()
    .set_size(slint::LogicalSize::new(640., 640.));
  window.show().expect("offscreen component");
  window.global::<Interaction>().set_motion_enabled(false);
  window.global::<Interaction>().set_editable(true);
  let edits = Rc::new(RefCell::new(Vec::new()));
  let received = edits.clone();
  connect_edits(&window, move |edit| received.borrow_mut().push(edit));
  let snapshot = defaults();
  for locale in LOCALES {
    apply_snapshot(&window, &snapshot, &presentation(locale, "msix")).expect("snapshot");
    for (page, name, y, expected) in [
      (
        3,
        "screenshots",
        219.0,
        LargePngAsJpg(!snapshot.screenshot_large_png_as_jpg),
      ),
      (
        3,
        "screenshots",
        424.0,
        GifShowCursor(!snapshot.gif_show_cursor),
      ),
      (
        7,
        "tools",
        109.0,
        SelectionMagnifier(!snapshot.selection_magnifier),
      ),
      (
        7,
        "tools",
        158.0,
        CopyTextDirect(!snapshot.copy_text_direct),
      ),
    ] {
      window.set_page(page);
      render_errors(&window, &format!("{locale}-{name}"));
      crate::interaction_check::click(window.window(), 575.0, y);
      assert_eq!(
        edits.borrow_mut().pop(),
        Some(Ok(expected)),
        "{locale} {name} at {y}"
      );
      edits.borrow_mut().clear();
    }
  }
  let state = window.global::<SettingsState>();
  let mut errors = vec![SharedString::default(); crate::edit::SLOT_COUNT];
  errors[31] = "magnifier failed".into();
  state.set_field_errors(Rc::new(VecModel::from(errors)).into());
  assert!(state.get_page_has_error().row_data(7).expect("Tools"));
}

fn hotkey_rows() {
  let window = SettingsWindow::new().expect("headless component");
  window
    .window()
    .set_size(slint::LogicalSize::new(640., 640.));
  window.show().expect("offscreen component");
  window.global::<Interaction>().set_motion_enabled(false);
  let captures = Rc::new(RefCell::new(Vec::new()));
  let received = captures.clone();
  window
    .global::<SettingsState>()
    .on_capture_request(move |action| received.borrow_mut().push(action.to_string()));
  window.global::<Interaction>().set_editable(true);
  window.global::<Interaction>().set_hotkeys_editable(true);
  window.set_page(2);
  for locale in LOCALES {
    apply_snapshot(&window, &defaults(), &presentation(locale, "msix")).expect("snapshot");
    render_errors(&window, &format!("{locale}-hotkeys"));
    for (action, y) in crate::interaction_check::SHORTCUT_ROWS {
      crate::interaction_check::click_shortcut_row(&window, y);
      assert_eq!(
        *captures.borrow(),
        [action],
        "{locale}: {action} row at {y}"
      );
      captures.borrow_mut().clear();
    }
  }
  let state = window.global::<SettingsState>();
  let mut errors = vec![SharedString::default(); crate::edit::SLOT_COUNT];
  errors[30] = "capture and edit failed".into();
  state.set_field_errors(Rc::new(VecModel::from(errors)).into());
  assert!(state.get_page_has_error().row_data(2).expect("Hotkeys"));
  state.set_field_errors(
    Rc::new(VecModel::from(vec![
      SharedString::default();
      crate::edit::SLOT_COUNT
    ]))
    .into(),
  );
  let mut conflicts = vec![false; 9];
  conflicts[7] = true;
  state.set_shortcut_conflicts(Rc::new(VecModel::from(conflicts)).into());
  assert!(state.get_page_has_error().row_data(2).expect("Hotkeys"));
}

fn search_and_templates() {
  let window = SettingsWindow::new().expect("headless component");
  connect_search(&window).expect("search callbacks");
  for locale in LOCALES {
    for channel in ["installer", "msix"] {
      apply_snapshot(&window, &defaults(), &presentation(locale, channel)).expect("snapshot");
      window.invoke_search_request("".into());
      let targets = window
        .get_search_results()
        .iter()
        .map(|r| r.target.to_string())
        .collect::<Vec<_>>();
      // Catalog search uses a nonempty query; use translated exact labels.
      let catalog = Catalog::load(locale).expect("catalog");
      for (key, target, visible) in [
        (
          "system_check_for_updates",
          "system-check-for-updates",
          channel == "installer",
        ),
        ("about_rate", "about-rate", channel == "msix"),
      ] {
        window.invoke_search_request(catalog.text(key).expect("search label").into());
        assert_eq!(
          window
            .get_search_results()
            .iter()
            .any(|r| r.target == target),
          visible,
          "{locale}/{channel}/{target}"
        );
      }
      assert!(targets.is_empty(), "empty search must clear results");
    }
  }
  let editing = window.global::<TemplateEditing>();
  let blocks = editing.invoke_blocks("前_%APP%_後".into(), false);
  assert_eq!(blocks.row_count(), 3);
  let token = blocks.row_data(1).unwrap();
  assert!(token.tag);
  assert_eq!(token.token, "%APP%");
  assert_eq!(token.prefix, "前_");
  assert_eq!(
    editing.invoke_caret_prefix("前_%APP%_後".into(), 2, false),
    "",
    "caret must not split UTF-8"
  );
  assert_eq!(editing.invoke_snap_cursor("%APP%".into(), 2, 0, false), 5);
  let limited = editing.invoke_limit_edit("safe".into(), "x".repeat(101).into(), 101, false);
  assert!(limited.text.len() <= 100 && limited.cursor <= 100);
  let inserted = editing.invoke_insert_token("name".into(), "%DATE%".into(), 0, 4, false);
  assert_eq!(inserted.text, "%DATE%");
  assert!(inserted.error.is_empty());
  let rejected = editing.invoke_insert_token("name".into(), "%APP%".into(), 4, 4, true);
  assert!(!rejected.error.is_empty());
  assert_eq!(rejected.text, "name");
  let state = window.global::<SettingsState>();
  assert!(
    state.get_filename_error().is_empty(),
    "internal errors stay out of the UI"
  );
  let full = "a".repeat(100);
  let limited = editing.invoke_insert_token(full.as_str().into(), "%APP%".into(), 100, 100, false);
  assert_eq!(limited.text, full);
  assert!(
    !state.get_filename_error().is_empty(),
    "a chip that no longer fits says so"
  );
}

fn close_and_reset() {
  let window = SettingsWindow::new().expect("headless component");
  let closes = Rc::new(Cell::new(0));
  let observed = closes.clone();
  connect_close_request(&window, move |_| observed.set(observed.get() + 1));
  connect_reset_confirmation(&window);
  let state = window.global::<SettingsState>();
  window.global::<Interaction>().set_editable(false);
  state.invoke_reset_request();
  assert!(!window.get_confirming_reset());
  window.global::<Interaction>().set_editable(true);
  state.set_confirm_invalid_close(true);
  state.invoke_reset_request();
  assert!(
    !window.get_confirming_reset(),
    "confirmations cannot overlap"
  );
  state.set_confirm_invalid_close(false);
  state.invoke_reset_request();
  assert!(window.get_confirming_reset());
  window.invoke_close_request();
  assert_eq!(
    closes.get(),
    0,
    "close must cancel reset rather than close settings"
  );
  // Modal transition timing remains covered by the explicit desktop probes.
  window.set_confirming_reset(false);
  window.invoke_close_request();
  assert_eq!(closes.get(), 1);
}

fn text_panel_states() {
  let window = TextPanelWindow::new().expect("headless component");
  window
    .window()
    .set_size(slint::LogicalSize::new(520., 380.));
  window.show().expect("offscreen component");
  window.global::<Interaction>().set_motion_enabled(false);
  let copies = Rc::new(RefCell::new(Vec::new()));
  let received = copies.clone();
  window.on_copy_request(move |text| received.borrow_mut().push(text.to_string()));
  let closes = Rc::new(Cell::new(0));
  let received = closes.clone();
  window.on_close_request(move || received.set(received.get() + 1));
  let copy_button = (480.0, 354.0);
  for (index, locale) in LOCALES.iter().enumerate() {
    window.set_language_index(index as i32);
    window.set_reading(true);
    window.set_text("".into());
    render_text_panel(&window, &format!("{locale}-text-reading"));
    crate::interaction_check::click(window.window(), copy_button.0, copy_button.1);
    window.set_reading(false);
    for (notice, name) in [
      (TextPanelNotice::Empty, "empty"),
      (TextPanelNotice::TooLarge, "too-large"),
      (TextPanelNotice::TimedOut, "timed-out"),
      (TextPanelNotice::Failed, "failed"),
    ] {
      window.set_notice(notice);
      render_text_panel(&window, &format!("{locale}-text-{name}"));
      crate::interaction_check::click(window.window(), copy_button.0, copy_button.1);
    }
    assert!(copies.borrow().is_empty(), "{locale}: copied without text");
    window.set_notice(TextPanelNotice::None);
    window.set_text("Capture and Edit\nSet shortcut".into());
    render_text_panel(&window, &format!("{locale}-text-ready"));
    crate::interaction_check::click(window.window(), copy_button.0, copy_button.1);
    assert_eq!(
      copies.borrow_mut().drain(..).collect::<Vec<_>>(),
      ["Capture and Edit\nSet shortcut"],
      "{locale}: copy button"
    );
    window.set_copied(true);
    render_text_panel(&window, &format!("{locale}-text-copied"));
    crate::interaction_check::click(window.window(), copy_button.0, copy_button.1);
    assert!(copies.borrow().is_empty(), "{locale}: copied twice");
    window.set_copied(false);
    window.set_notice(TextPanelNotice::CopyFailed);
    render_text_panel(&window, &format!("{locale}-text-copy-failed"));
    window.set_notice(TextPanelNotice::None);
  }
  crate::interaction_check::click(window.window(), 495.0, 25.0);
  assert_eq!(closes.get(), 1, "close button");
  window
    .window()
    .dispatch_event(slint::platform::WindowEvent::KeyPressed {
      text: slint::platform::Key::Escape.into(),
    });
  assert_eq!(closes.get(), 2, "escape");
}
