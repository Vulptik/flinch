use super::*;
use crate::app::headless::drain_until;

#[tokio::test]
async fn settings_action_dispatch_respects_notification_and_update_preferences_without_external_effects()
 {
  for language in ["en", "fr", "de", "es", "it"] {
    let mut config = Config {
      language: language.into(),
      notifications_enabled: false,
      notification_sound: false,
      check_for_updates: false,
      ..Default::default()
    };
    let app = AppHandle::isolated(config.clone(), std::path::PathBuf::new());
    assert!(
      perform_action(app.clone(), "notification")
        .await
        .unwrap_err()
        .contains(&crate::i18n::native_settings(language, "notifications_test_error").unwrap())
    );
    assert!(
      perform_action(app.clone(), "download-update")
        .await
        .unwrap_err()
        .contains("not enabled")
    );
    assert!(
      perform_action(app.clone(), "unknown-action")
        .await
        .unwrap_err()
        .contains("unknown-action")
    );
    assert!(
      app
        .try_state::<crate::overlay::notification::NotificationManager>()
        .unwrap()
        .take_test_requests()
        .is_empty()
    );
    config.notifications_enabled = true;
    app
      .try_state::<crate::state::ConfigState>()
      .unwrap()
      .replace(config);
    assert!(
      perform_action(app.clone(), "notification")
        .await
        .unwrap()
        .is_none()
    );
    let requests = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .unwrap()
      .take_test_requests();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].critical);
    assert_eq!(
      requests[0].title,
      crate::i18n::native_settings(language, "notifications_test_title").unwrap()
    );
    assert_eq!(
      requests[0].body,
      crate::i18n::native_settings(language, "notifications_test_body").unwrap()
    );
  }
}
struct Fixture {
  window: SettingsWindow,
  controller: Rc<RefCell<Controller>>,
  app: AppHandle,
}
impl Fixture {
  fn new() -> Self {
    let app = AppHandle::isolated(
      Config {
        language: "en".into(),
        check_for_updates: false,
        ..Config::default()
      },
      std::path::PathBuf::new(),
    );
    let window = SettingsWindow::new().expect("headless presenter");
    let controller =
      connect_presenter(&window, app.clone(), 10).expect("connect production presenter");
    window.global::<Interaction>().set_editable(true);
    Self {
      window,
      controller,
      app,
    }
  }
  fn request(&self) -> SaveRequest<EditDraft> {
    self.controller.borrow().debounce.stop();
    self
      .controller
      .borrow_mut()
      .session
      .start()
      .expect("start draft")
      .expect("pending draft")
  }
}
impl Drop for Fixture {
  fn drop(&mut self) {
    VIEW.with(|view| {
      view.borrow_mut().take();
    });
  }
}

// Most cases inject synchronous completions. The final worker case pumps only
// the bounded fixture callback queue and persists into its own temporary folder.
// No native window, network, clipboard or real shortcut registration is used.
#[test]
fn headless_presenter_rejects_stale_results_retains_failed_edits_and_owns_close_lifecycle() {
  crate::app::headless::run(presenter_scenarios);
}

fn presenter_scenarios() {
  stale_completion_and_save_recovery();
  validation_and_close();
  active_operations_keep_close_ownership();
  action_results_preserve_drafts_and_complete_close_only_after_success();
  deferred_capture_and_action_slots_are_bounded_and_cancellable();
  shortcut_completions_retain_failed_edits_and_reject_stale_ownership();
  update_and_diagnostic_completions_reject_stale_or_disabled_views();
  bound_presenter_callbacks_keep_drafts_errors_and_capture_cancellation_on_the_owner();
  asynchronous_save_capture_and_action_completion_keep_thread_and_disk_ownership();
  asynchronous_capture_cancellation_rejection_and_failed_save_restore_shortcuts();
  asynchronous_discard_and_retry_keep_valid_preferences_and_close_after_acknowledgment();
  asynchronous_open_coalesces_navigation_and_keeps_preferences_editable_after_startup_status_failure();
  every_slot_shows_its_inline_error();
}

/// Errors for the newest settings (Capture and Edit, Magnifier, Copy Text, Pick Color) used to
/// be dropped because only the first 30 slots were read.
fn every_slot_shows_its_inline_error() {
  use flinch_settings::edit::{SLOT_COUNT, ShortcutAction};
  use slint::Model;
  let f = Fixture::new();
  let state = f.window.global::<SettingsState>();
  assert_eq!(state.get_field_errors().row_count(), SLOT_COUNT);
  let mut slots: Vec<usize> = ShortcutAction::ALL
    .into_iter()
    .map(|action| PreferenceEdit::Shortcut(action, String::new()).slot())
    .collect();
  slots.extend([
    PreferenceEdit::SelectionMagnifier(true).slot(),
    PreferenceEdit::CopyTextDirect(true).slot(),
  ]);
  for slot in slots {
    assert!(slot < SLOT_COUNT, "slot {slot}");
    super::field_error(&f.window, slot, "Problem");
    let errors = state.get_field_errors();
    assert_eq!(errors.row_count(), SLOT_COUNT, "the list keeps every slot");
    assert_eq!(errors.row_data(slot).unwrap(), "Problem", "slot {slot}");
    super::field_error(&f.window, slot, "");
  }
}

fn asynchronous_discard_and_retry_keep_valid_preferences_and_close_after_acknowledgment() {
  let folder = tempfile::tempdir().unwrap();
  let path = folder.path().join("discard.json");
  {
    let f = Fixture::new();
    f.app.isolate_config_path(path.clone());
    receive_action(10, "discard");
    assert!(
      current().is_some(),
      "discard cannot close without an invalid close request"
    );
    receive_edit(10, Ok(PreferenceEdit::Filename("bad:name".into())));
    receive_edit(10, Ok(PreferenceEdit::Subfolder("bad/name".into())));
    receive_action(10, "retry");
    assert!(f.controller.borrow().invalid_filename.is_some());
    receive_edit(10, Ok(PreferenceEdit::ShowCursor(false)));
    close_requested(10);
    receive_action(10, "discard");
    drain_until(|| current().is_none());
    assert!(
      f.controller.borrow().invalid_filename.is_none()
        && f.controller.borrow().invalid_subfolder.is_none()
    );
    let persisted: Config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(!persisted.show_cursor);
    assert_eq!(
      persisted.filename_template,
      Config::default().filename_template
    );
  }
  {
    let f = Fixture::new();
    f.app
      .isolate_config_path(folder.path().join("missing/preferences.json"));
    receive_edit(10, Ok(PreferenceEdit::ShowCursor(false)));
    start_save(10);
    drain_until(|| f.window.global::<SettingsState>().get_save_failed());
    f.app
      .isolate_config_path(folder.path().join("retried.json"));
    receive_edit(10, Ok(PreferenceEdit::Filename("bad:name".into())));
    assert!(
      !f.window
        .global::<SettingsState>()
        .get_save_error()
        .is_empty()
    );
    receive_action(10, "retry");
    assert!(f.window.global::<SettingsState>().get_saving());
    assert!(
      !f.window
        .global::<SettingsState>()
        .get_save_error()
        .is_empty()
    );
    receive_action(10, "retry"); // An in-flight retry cannot start another worker.
    drain_until(|| f.controller.borrow().session.is_idle());
    assert!(!Config::current(&f.app).show_cursor);
    assert!(!f.window.global::<SettingsState>().get_save_failed());
    assert!(
      f.window
        .global::<SettingsState>()
        .get_save_error()
        .is_empty()
    );
    assert!(f.controller.borrow().invalid_filename.is_some());
    close_requested(10);
    assert!(
      f.window
        .global::<SettingsState>()
        .get_confirm_invalid_close()
    );
    receive_action(10, "discard");
    assert!(current().is_none());
  }
  {
    let f = Fixture::new();
    let missing = folder.path().join("still-missing/preferences.json");
    f.app.isolate_config_path(missing.clone());
    receive_edit(10, Ok(PreferenceEdit::ShowCursor(false)));
    start_save(10);
    drain_until(|| f.window.global::<SettingsState>().get_save_failed());
    let reason = f.window.global::<SettingsState>().get_save_error();
    assert!(
      !reason.is_empty(),
      "save diagnostics retain the service failure"
    );
    close_requested(10);
    assert!(
      f.window
        .global::<SettingsState>()
        .get_confirm_invalid_close()
    );
    receive_action(10, "cancel-close");
    assert_eq!(f.window.global::<SettingsState>().get_save_error(), reason);
    assert!(Config::current(&f.app).show_cursor);
    close_requested(10);
    receive_action(10, "discard");
    assert!(current().is_none());
    assert!(!missing.exists(), "Discard cannot publish a failed draft");
  }
}

fn asynchronous_capture_cancellation_rejection_and_failed_save_restore_shortcuts() {
  use slint::Model;
  let folder = tempfile::tempdir().unwrap();
  let f = Fixture::new();
  f.app
    .isolate_config_path(folder.path().join("preferences.json"));
  let action = ShortcutAction::ScreenshotArea;
  begin_capture(10, action.id());
  cancel_capture(10, false);
  drain_until(|| !f.controller.borrow().capture_busy);
  assert!(f.controller.borrow().capture.is_none());
  assert!(f.window.global::<Interaction>().get_editable());
  let rejected = "Ctrl+Alt+F8";
  f.app.test_hotkeys().unwrap().reject(
    rejected.parse().unwrap(),
    "scripted native shortcut conflict",
  );
  let initial = Config::current(&f.app).screenshot_area_shortcut;
  begin_capture(10, action.id());
  drain_until(|| f.controller.borrow().capture.is_some());
  capture_key(rejected.into());
  drain_until(|| !f.controller.borrow().capture_busy);
  assert_eq!(Config::current(&f.app).screenshot_area_shortcut, initial);
  assert!(
    !f.window
      .global::<SettingsState>()
      .get_field_errors()
      .row_data(PreferenceEdit::Shortcut(action, String::new()).slot())
      .unwrap()
      .is_empty()
  );
  assert!(
    !f.app
      .try_state::<crate::shortcuts::ShortcutStateHolder>()
      .unwrap()
      .is_shortcuts_disabled()
  );
  f.app
    .isolate_config_path(folder.path().join("missing/preferences.json"));
  begin_capture(10, action.id());
  drain_until(|| f.controller.borrow().capture.is_some());
  capture_key("Ctrl+Alt+F9".into());
  drain_until(|| !f.controller.borrow().capture_busy);
  assert_eq!(Config::current(&f.app).screenshot_area_shortcut, initial);
  assert!(
    !f.controller.borrow().session.is_idle(),
    "failed capture save must retain its draft"
  );
  assert!(
    !f.app
      .try_state::<crate::shortcuts::ShortcutStateHolder>()
      .unwrap()
      .is_shortcuts_disabled()
  );
  assert!(f.window.global::<Interaction>().get_editable());
  // Use another presenter to exercise close while the lease worker is still acquiring.
  drop(f);
  let f = Fixture::new();
  begin_capture(10, action.id());
  close_requested(10);
  drain_until(|| current().is_none());
  assert!(f.controller.borrow().capture.is_none());
  assert!(
    !f.app
      .try_state::<crate::shortcuts::ShortcutStateHolder>()
      .unwrap()
      .is_shortcuts_disabled()
  );
}

fn asynchronous_open_coalesces_navigation_and_keeps_preferences_editable_after_startup_status_failure()
 {
  for failure in [false, true] {
    VIEW.with(|slot| slot.borrow_mut().take());
    let app = AppHandle::isolated(
      Config {
        language: "en".into(),
        check_for_updates: false,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    show_on_owner_with(app.clone(), Section::from_id("files"), move |app| {
      std::future::ready(if failure {
        Err("injected startup status failure".into())
      } else {
        Ok(Config::current(&app))
      })
    })
    .unwrap();
    let (window, controller) = current().unwrap();
    assert_eq!(
      window.get_page(),
      Section::from_id("files").unwrap().index()
    );
    drain_until(|| window.global::<Interaction>().get_editable());
    assert_eq!(
      window.global::<SettingsState>().get_startup_unavailable(),
      failure
    );
    show(&app, Section::from_id("general")).unwrap();
    show(&app, Section::from_id("hotkeys")).unwrap();
    show(&app, Section::from_id("about")).unwrap();
    drain_until(|| window.get_page() == Section::from_id("about").unwrap().index());
    assert!(
      Rc::ptr_eq(&controller, &current().unwrap().1),
      "navigation must reuse one presenter"
    );
    drain_until(|| {
      !window
        .global::<SettingsState>()
        .get_windows_build()
        .is_empty()
    });
    let generation = controller.borrow().generation;
    close_requested(generation);
    assert!(current().is_none());
  }
}

fn asynchronous_save_capture_and_action_completion_keep_thread_and_disk_ownership() {
  crate::app::runtime::test_runtime();
  let folder = tempfile::tempdir().unwrap();
  let path = folder.path().join("config.json");
  let f = Fixture::new();
  f.app.isolate_config_path(path.clone());
  flinch_settings::windows::configure_fonts(&f.window).unwrap();
  let mut config = Config::current(&f.app);
  config.screenshot_area_shortcut = "Ctrl+Shift+F1".into();
  config.screenshot_fullscreen_shortcut = "Ctrl+Shift+F2".into();
  config.screenshot_active_window_shortcut = "Ctrl+Shift+F3".into();
  config.record_area_shortcut = "Ctrl+Shift+F4".into();
  config.record_fullscreen_shortcut = "Ctrl+Shift+F5".into();
  config.record_active_window_shortcut = "Ctrl+Shift+F6".into();
  config.gif_shortcut = "Ctrl+Shift+F7".into();
  config.screenshot_edit_shortcut = "Ctrl+Shift+F8".into();
  config.copy_text_shortcut = "Ctrl+Shift+F9".into();
  Config::replace_current(&f.app, config);
  receive_edit(10, Ok(PreferenceEdit::ShowCursor(false)));
  start_save(10);
  drain_until(|| {
    f.controller.borrow().session.is_idle() || f.window.global::<SettingsState>().get_save_failed()
  });
  assert!(
    !f.window.global::<SettingsState>().get_save_failed(),
    "isolated save failed: {}",
    f.window.global::<SettingsState>().get_save_error()
  );
  assert!(!Config::current(&f.app).show_cursor);
  let persisted: Config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
  assert!(!persisted.show_cursor);
  for edit in [Some("Ctrl+Alt+F9"), None, Some("invalid!binding")] {
    begin_capture(10, ShortcutAction::ScreenshotArea.id());
    drain_until(|| f.controller.borrow().capture.is_some());
    assert!(f.controller.borrow().capture_busy);
    if let Some(binding) = edit {
      capture_key(binding.into());
    } else {
      cancel_capture(10, false);
    }
    drain_until(|| !f.controller.borrow().capture_busy);
    assert!(f.controller.borrow().capture.is_none());
    assert!(f.window.global::<Interaction>().get_editable());
  }
  assert_eq!(
    Config::current(&f.app).screenshot_area_shortcut,
    "Ctrl+Alt+F9"
  );
  receive_action(10, "unknown-worker-action");
  drain_until(|| !f.controller.borrow().action_running);
  assert!(
    !f.window
      .global::<SettingsState>()
      .get_action_error()
      .is_empty()
  );
  let state = f.window.global::<SettingsState>();
  state.set_windows_build("".into());
  load_diagnostics(10, f.app.clone());
  drain_until(|| !state.get_windows_build().is_empty());
  assert!(!state.get_displays().is_empty());
  let copied = state.get_diagnostics_copy_generation();
  receive_action(10, "copy-diagnostics");
  drain_until(|| !f.controller.borrow().action_running);
  assert_eq!(
    state.get_diagnostics_copy_generation(),
    copied,
    "missing clipboard cannot report a successful copy"
  );
  assert!(
    !state.get_action_message().is_empty(),
    "a failed copy says so beside the button"
  );
  let mut offline = Config::current(&f.app);
  let drive = crate::config::unused_drive_letter();
  offline.custom_single_folder = Some(format!("{drive}:\\Flinch"));
  offline.custom_screenshots_folder = Some(format!("{drive}:\\Flinch"));
  f.app
    .try_state::<crate::state::ConfigState>()
    .unwrap()
    .replace(offline);
  receive_action(10, "open-screenshots");
  drain_until(|| !f.controller.borrow().action_running);
  assert_eq!(
    state.get_action_message().as_str(),
    crate::i18n::native_settings("en", "native_open_folder_error").unwrap(),
    "an offline folder says so beside its row"
  );
  receive_action(10, "reset");
  drain_until(|| !f.controller.borrow().action_running);
  assert!(Config::current(&f.app).show_cursor);
  // A failed real write retains the draft and leaves the current in-memory value intact.
  f.app
    .isolate_config_path(folder.path().join("missing/config.json"));
  receive_edit(10, Ok(PreferenceEdit::ShowCursor(false)));
  start_save(10);
  drain_until(|| f.window.global::<SettingsState>().get_save_failed());
  assert!(Config::current(&f.app).show_cursor);
  request_exit(&f.app).unwrap();
  request_exit(&f.app).unwrap();
  drain_until(|| current().is_none());
  assert!(
    f.app
      .try_state::<crate::state::AppExitState>()
      .unwrap()
      .should_exit()
  );
  QUITTING.store(false, Ordering::Release);
}

fn bound_presenter_callbacks_keep_drafts_errors_and_capture_cancellation_on_the_owner() {
  let f = Fixture::new();
  let state = f.window.global::<SettingsState>();
  state.set_update_version("2.0.0".into());
  state.invoke_action("dismiss-update".into());
  assert!(state.get_update_version().is_empty());
  state.set_cursor(false);
  state.invoke_edit_request("cursor".into());
  assert!(!f.controller.borrow().session.is_idle());
  let save = f.request();
  state.invoke_capture_request(ShortcutAction::ScreenshotArea.id().into());
  assert!(matches!(
    f.controller.borrow().after_save,
    Some(AfterSave::Capture(ShortcutAction::ScreenshotArea))
  ));
  f.window.set_page(5);
  f.window.invoke_page_changed();
  assert!(f.controller.borrow().after_save.is_none());
  assert!(!state.get_filename_preview().is_empty());
  finish_save(10, save, Err("fixture write failure".into()));
  state.set_filename("bad:name".into());
  state.invoke_edit_request("filename".into());
  assert!(!state.get_filename_error().is_empty());
  // Keep the valid pending edit in flight while exercising close. Complete it
  // explicitly below so this synchronous fixture cannot dispatch a real save.
  f.controller.borrow_mut().session.retry();
  let pending = f.request();
  f.window.invoke_close_request();
  assert!(state.get_confirm_invalid_close());
  state.invoke_action("cancel-close".into());
  assert!(!state.get_confirm_invalid_close());
  assert_eq!(state.get_focus_invalid(), 1);
  finish_save(10, pending, Err("fixture retry failure".into()));
  assert!(!f.controller.borrow().session.is_idle());
  assert!(f.controller.borrow().invalid_filename.is_some());
  state.invoke_action("reset-filename".into());
  assert!(f.controller.borrow().invalid_filename.is_none());
  assert_eq!(state.get_filename(), crate::filename::DEFAULT_TEMPLATE);
  state.set_subfolder("bad:name".into());
  state.invoke_edit_request("subfolder".into());
  assert!(!state.get_subfolder_error().is_empty());
  state.invoke_action("reset-subfolder".into());
  assert!(f.controller.borrow().invalid_subfolder.is_none());
  assert_eq!(
    state.get_subfolder(),
    crate::capture_subfolder::DEFAULT_TEMPLATE
  );
  state.invoke_capture_request("unknown-action".into());
  assert!(!f.controller.borrow().capture_busy);
  f.controller.borrow_mut().capture_busy = true;
  f.controller.borrow_mut().capture_action = Some(ShortcutAction::ScreenshotArea);
  state.set_capture_target(ShortcutAction::ScreenshotArea.id().into());
  state.invoke_capture_request(ShortcutAction::ScreenshotArea.id().into());
  assert!(f.controller.borrow().capture_cancelled);
  assert!(state.get_capture_target().is_empty());
  finish_capture_result(
    10,
    Some(ShortcutAction::ScreenshotArea),
    Config::current(&f.app),
    None,
    Ok(()),
  );
}

fn shortcut_completions_retain_failed_edits_and_reject_stale_ownership() {
  use slint::Model;
  for failed_save in [false, true] {
    let f = Fixture::new();
    let action = ShortcutAction::ScreenshotArea;
    let edit = PreferenceEdit::Shortcut(action, "Ctrl+Alt+F8".into());
    let old_config = Config::current(&f.app);
    let draft = Config::try_from((old_config.clone(), edit.clone())).unwrap();
    f.controller.borrow_mut().capture_busy = true;
    f.window.global::<Interaction>().set_editable(false);
    finish_capture_result(
      9,
      Some(action),
      draft.clone(),
      Some(edit.clone()),
      Err("stale".into()),
    );
    assert!(f.controller.borrow().capture_busy);
    assert!(!f.window.global::<Interaction>().get_editable());
    finish_capture_result(
      10,
      Some(action),
      draft,
      failed_save.then_some(edit),
      Err("registration denied".into()),
    );
    assert!(!f.controller.borrow().capture_busy);
    assert!(f.window.global::<Interaction>().get_editable());
    assert!(
      !f.window
        .global::<SettingsState>()
        .get_field_errors()
        .row_data(PreferenceEdit::Shortcut(action, String::new()).slot())
        .unwrap()
        .is_empty()
    );
    assert_eq!(
      f.window.global::<SettingsState>().get_save_failed(),
      failed_save
    );
    assert_eq!(f.controller.borrow().session.is_idle(), !failed_save);
    assert_eq!(
      Config::current(&f.app).screenshot_area_shortcut,
      old_config.screenshot_area_shortcut
    );
  }
  let f = Fixture::new();
  f.controller.borrow_mut().capture_busy = true;
  f.controller.borrow_mut().close_after_capture = true;
  finish_capture_result(10, None, Config::current(&f.app), None, Ok(()));
  assert!(
    current().is_none(),
    "successful restoration completes a deferred close"
  );
}

fn update_and_diagnostic_completions_reject_stale_or_disabled_views() {
  use slint::Model;
  let f = Fixture::new();
  let state = f.window.global::<SettingsState>();
  state.set_update_version("".into());
  finish_update_result(10, &f.app, Ok(Some("9.9.9".into())));
  assert!(state.get_update_version().is_empty());
  let mut config = Config::current(&f.app);
  config.check_for_updates = true;
  f.app
    .try_state::<crate::state::ConfigState>()
    .unwrap()
    .replace(config);
  state.set_updates(true);
  finish_update_result(9, &f.app, Ok(Some("8.8.8".into())));
  assert!(state.get_update_version().is_empty());
  finish_update_result(10, &f.app, Ok(Some("9.9.9".into())));
  assert_eq!(state.get_update_version(), "9.9.9");
  finish_update_result(10, &f.app, Ok(None));
  assert_eq!(state.get_update_version(), "9.9.9");
  finish_update_result(10, &f.app, Err(anyhow::anyhow!("manifest unavailable")));
  assert!(
    state.get_action_message().is_empty(),
    "background failures stay in the log"
  );
  let info = || Diagnostics {
    windows: "Windows fixture".into(),
    graphics: "GPU fixture".into(),
    graphics_adapters: vec![("GPU A".into(), "1.2".into())],
    displays: "Display fixture".into(),
    display_details: vec![("320 × 200".into(), "HDR enabled".into())],
    copy: "copy fixture".into(),
  };
  finish_diagnostics_result(9, Ok(info()));
  assert_ne!(state.get_windows_build(), "Windows fixture");
  finish_diagnostics_result(10, Ok(info()));
  assert_eq!(state.get_windows_build(), "Windows fixture");
  assert_eq!(state.get_graphics_adapters().row_count(), 1);
  assert_eq!(
    state.get_graphics_adapters().row_data(0).unwrap().driver,
    "1.2"
  );
  assert_eq!(
    state.get_display_details().row_data(0).unwrap().hdr,
    "HDR enabled"
  );
  finish_diagnostics_result(10, Err("deadline exceeded".into()));
  assert!(
    state.get_action_message().is_empty(),
    "background failures stay in the log"
  );
  assert_eq!(state.get_windows_build(), "Windows fixture");
}

#[test]
fn support_diagnostics_formatting_preserves_errors_locales_and_negative_display_origins() {
  let info = serde_json::json!({"windowsBuild":"Windows 11 fixture", "version":"1.1.0", "channel":"msix",
    "graphics":{"gpus":[{"name":"GPU A","driver":"1.2"},{"name":"GPU B","driverError":"driver read denied"},{}],
    "outputs":[{"x":-320,"y":-200,"hdr":true},{"x":0,"y":0,"hdr":false},{"x":320,"y":0,"error":"HDR query denied"}]}});
  let monitors = || {
    [-320, 0, 320]
      .into_iter()
      .map(|x| crate::monitors::MonitorInfo {
        id: x.to_string(),
        name: "fixture".into(),
        x,
        y: if x < 0 { -200 } else { 0 },
        width: 320,
        height: 200,
        is_primary: x == 0,
        dpi_scale: 1.5,
        logical_x: x,
        logical_y: 0,
        logical_width: 213,
        logical_height: 133,
      })
      .collect::<Vec<_>>()
  };
  for language in ["en", "fr", "de", "es", "it"] {
    let unavailable = crate::i18n::native_settings(language, "about_unavailable").unwrap();
    let result = format_diagnostics(language, &unavailable, &info, monitors()).unwrap();
    assert_eq!(result.windows, "Windows 11 fixture");
    assert_eq!(result.graphics_adapters.len(), 3);
    assert_eq!(result.graphics_adapters[1].1, "driver read denied");
    assert_eq!(result.graphics_adapters[2].0, unavailable);
    assert_eq!(result.display_details.len(), 3);
    assert_eq!(
      result.display_details[0].1,
      crate::i18n::native_settings(language, "about_hdr_enabled").unwrap()
    );
    assert_eq!(
      result.display_details[1].1,
      crate::i18n::native_settings(language, "about_hdr_disabled").unwrap()
    );
    assert_eq!(result.display_details[2].1, "HDR query denied");
    assert!(result.displays.contains("150%"));
    assert!(
      result.copy.contains("Flinch 1.1.0")
        && result.copy.contains("MSIX")
        && result.copy.contains("driver read denied")
    );
  }
  let mut failed = info.clone();
  failed["channel"] = "installer".into();
  failed["graphics"] = serde_json::json!({"error":"DXGI unavailable"});
  let result = format_diagnostics("en", "unavailable", &failed, monitors()).unwrap();
  assert_eq!(result.graphics, "DXGI unavailable");
  assert!(result.graphics_adapters.is_empty());
  assert_eq!(result.display_details[0].1, "unavailable");
  for key in ["windowsBuild", "version", "channel"] {
    let mut malformed = info.clone();
    malformed.as_object_mut().unwrap().remove(key);
    assert!(
      format_diagnostics("en", "unavailable", &malformed, vec![]).is_err(),
      "missing {key}"
    );
  }
  let mut malformed = info;
  malformed["graphics"] = serde_json::json!({});
  assert!(format_diagnostics("en", "unavailable", &malformed, vec![]).is_err());
}

fn stale_completion_and_save_recovery() {
  let f = Fixture::new();
  let state = f.window.global::<SettingsState>();
  receive_edit(9, Ok(PreferenceEdit::GifShowCursor(false)));
  assert!(
    f.controller.borrow().session.is_idle(),
    "retired views cannot enqueue edits"
  );
  receive_edit(10, Ok(PreferenceEdit::GifShowCursor(false)));
  let request = f.request();
  f.controller.borrow_mut().after_save = Some(AfterSave::Action("export".into()));
  close_requested(10);
  assert!(
    f.controller.borrow().after_save.is_none(),
    "close cancels deferred actions"
  );
  assert!(state.get_close_pending());
  assert!(!f.window.global::<Interaction>().get_editable());
  assert_eq!(
    f.controller.borrow().session.close_decision(),
    CloseDecision::WaitForSave
  );
  finish_save(9, request.clone(), Err("stale write failure".into()));
  assert!(
    !state.get_save_failed(),
    "stale service result must not affect current view"
  );
  f.controller.borrow_mut().after_save = Some(AfterSave::Action("export".into()));
  finish_save(10, request, Err("isolated write denied".into()));
  assert!(state.get_save_failed());
  assert!(!state.get_close_pending());
  assert!(f.window.global::<Interaction>().get_editable());
  assert!(
    f.controller.borrow().after_save.is_none(),
    "failed writes cancel deferred actions"
  );
  assert!(
    !f.controller.borrow().session.is_idle(),
    "failed draft must survive"
  );
  assert!(
    f.controller.borrow_mut().session.start().unwrap().is_none(),
    "no automatic retry loop"
  );
  receive_edit(10, Ok(PreferenceEdit::ShowCursor(false)));
  let retry = f.request();
  assert_eq!(
    retry.draft.edits().count(),
    2,
    "retry merges old failure with newest edit"
  );
  let mut config = Config::current(&f.app);
  for edit in retry.draft.edits() {
    config = Config::try_from((config, edit.clone())).expect("valid retained edit");
  }
  f.app
    .try_state::<crate::state::ConfigState>()
    .expect("isolated config")
    .replace(config.clone());
  finish_save(10, retry.clone(), Ok(config.clone()));
  assert!(f.controller.borrow().session.is_idle());
  assert!(!state.get_save_failed());
  assert!(!state.get_cursor() && !state.get_gif_cursor());
  finish_save(10, retry, Err("duplicate callback".into()));
  assert!(!state.get_save_failed());
  check_updates(10);
  assert!(
    f.controller.borrow().update_checked,
    "disabled updates finish without dispatching network work"
  );
  close_requested(10);
  assert!(
    current().is_none(),
    "successful final save permits release of the view"
  );
}

fn validation_and_close() {
  let f = Fixture::new();
  let state = f.window.global::<SettingsState>();
  receive_edit(10, Ok(PreferenceEdit::Filename("valid-name".into())));
  let failed = f.request();
  finish_save(10, failed, Err("isolated filename write failure".into()));
  assert!(!state.get_save_error().is_empty());
  state.set_filename("bad:name".into());
  receive_edit(10, Ok(PreferenceEdit::Filename("bad:name".into())));
  assert!(
    state.get_save_error().is_empty(),
    "replacing the last queued write leaves only its validation error"
  );
  assert!(!state.get_save_failed());
  assert_eq!(
    f.controller.borrow().invalid_filename.as_deref(),
    Some("bad:name")
  );
  assert!(!state.get_filename_error().is_empty());
  assert!(
    f.controller.borrow().session.is_idle(),
    "invalid text never enters persistence queue"
  );
  refresh(&f.window, &Config::current(&f.app)).expect("refresh with invalid draft");
  assert_eq!(
    state.get_filename(),
    "bad:name",
    "refresh must preserve invalid text for correction"
  );
  close_requested(10);
  assert!(state.get_confirm_invalid_close());
  assert_eq!(
    f.controller.borrow().session.close_decision(),
    CloseDecision::ResolveFailure
  );
  receive_action(10, "cancel-close");
  assert!(!state.get_confirm_invalid_close() && !state.get_close_pending());
  assert_eq!(state.get_focus_invalid(), 1);
  assert!(f.window.global::<Interaction>().get_editable());
  state.set_together_folder_custom(true);
  receive_action(10, "reset-folder-together");
  assert!(!state.get_together_folder_custom());
  receive_action(10, "reset-filename");
  assert!(f.controller.borrow().invalid_filename.is_none());
  assert!(state.get_filename_error().is_empty());
  let request = f.request();
  let config = Config::try_from((Config::current(&f.app), PreferenceEdit::ResetFilename))
    .expect("reset filename");
  finish_save(10, request, Ok(config));
  assert!(f.controller.borrow().session.is_idle());
  state.set_subfolder("bad/name".into());
  receive_edit(10, Ok(PreferenceEdit::Subfolder("bad/name".into())));
  assert!(f.controller.borrow().invalid_subfolder.is_some());
  close_requested(10);
  receive_action(10, "cancel-close");
  assert_eq!(state.get_focus_invalid(), 2);
  display_action_error(&f.window, "import", "file is inaccessible");
  assert!(!state.get_action_message().is_empty());
  assert!(
    !state.get_action_message().contains("file is inaccessible"),
    "technical reasons stay in the log"
  );
  receive_edit(10, Err("unknown field adapter".into()));
  assert!(!state.get_action_message().contains("unknown field adapter"));
}

fn active_operations_keep_close_ownership() {
  let f = Fixture::new();
  let state = f.window.global::<SettingsState>();
  f.controller.borrow_mut().action_running = true;
  close_requested(10);
  assert!(state.get_close_pending());
  assert!(f.controller.borrow().close_after_action);
  assert!(f.controller.borrow().close_deadline.running());
  receive_action(10, "cancel-close");
  assert!(!state.get_close_pending());
  assert!(!f.controller.borrow().close_after_action);
  assert!(
    !f.window.global::<Interaction>().get_editable(),
    "cancelling close cannot enable controls during an action"
  );
  assert!(!f.controller.borrow().close_deadline.running());
  f.controller.borrow_mut().action_running = false;
  f.controller.borrow_mut().capture_busy = true;
  assert!(capture_busy());
  close_requested(10);
  assert!(f.controller.borrow().capture_cancelled);
  assert!(f.controller.borrow().close_after_capture);
  assert!(
    f.controller.borrow().capture_busy,
    "acquisition worker retains ownership until acknowledgment"
  );
  cancel_current_capture();
  assert!(
    f.controller.borrow().close_after_capture,
    "repeated cancellation cannot lose close intent"
  );
}

fn action_results_preserve_drafts_and_complete_close_only_after_success() {
  {
    let f = Fixture::new();
    let state = f.window.global::<SettingsState>();
    receive_edit(10, Ok(PreferenceEdit::Filename("bad:name".into())));
    f.controller.borrow_mut().action_running = true;
    f.controller.borrow_mut().close_after_action = true;
    state.set_close_pending(true);
    finish_action(
      9,
      "import".into(),
      3000,
      true,
      Ok(Some(Config::current(&f.app))),
    );
    assert!(
      f.controller.borrow().action_running,
      "stale action cannot release ownership"
    );
    finish_action(
      10,
      "import".into(),
      3000,
      false,
      Err("import denied".into()),
    );
    assert!(!f.controller.borrow().action_running && !f.controller.borrow().close_after_action);
    assert!(!state.get_close_pending());
    assert!(!state.get_action_error().is_empty());
    assert!(!state.get_action_error().contains("import denied"));
    assert_eq!(
      f.controller.borrow().invalid_filename.as_deref(),
      Some("bad:name")
    );
    f.controller.borrow_mut().action_running = true;
    finish_action(10, "import".into(), 3000, true, Ok(None));
    assert!(
      f.controller.borrow().invalid_filename.is_some(),
      "cancelled picker retains draft"
    );
    let old = state.get_diagnostics_copy_generation();
    finish_action(10, "copy-diagnostics".into(), 3000, false, Ok(None));
    assert_eq!(
      state.get_diagnostics_copy_generation(),
      old,
      "failed optional copy cannot show a copied checkmark"
    );
    finish_action(10, "copy-diagnostics".into(), 3000, true, Ok(None));
    assert_eq!(state.get_diagnostics_copy_generation(), old.wrapping_add(1));
    state.set_update_version("9.9.9".into());
    finish_action(10, "download-update".into(), 3000, true, Ok(None));
    assert!(state.get_update_version().is_empty());
    f.window.set_confirming_reset(true);
    let mut replacement = Config::current(&f.app);
    replacement.show_cursor = false;
    replacement.gif_show_cursor = false;
    finish_action(10, "reset".into(), 3000, true, Ok(Some(replacement)));
    assert!(!f.window.get_confirming_reset());
    assert!(f.controller.borrow().invalid_filename.is_none());
    assert!(state.get_filename_error().is_empty());
    assert!(!state.get_cursor() && !state.get_gif_cursor());
    finish_action(10, "notification".into(), 5000, true, Ok(None));
    assert!(state.get_preview_cooldown() >= 5);
    receive_action(10, "notification");
    assert!(
      !f.controller.borrow().action_running,
      "cooldown cannot dispatch another preview"
    );
    f.controller.borrow_mut().action_running = true;
    f.controller.borrow_mut().close_after_action = true;
    finish_action(10, "export".into(), 3000, true, Ok(None));
    assert!(
      current().is_none(),
      "successful deferred close releases the presenter"
    );
  }
  PREVIEW
    .with(|preview| *preview.borrow_mut() = flinch_settings::preview::PreviewCooldown::default());
}

fn deferred_capture_and_action_slots_are_bounded_and_cancellable() {
  let f = Fixture::new();
  receive_edit(10, Ok(PreferenceEdit::ShowCursor(false)));
  let request = f.request();
  receive_action(10, "export");
  assert!(
    matches!(&f.controller.borrow().after_save,Some(AfterSave::Action(action))if action=="export")
  );
  receive_action(10, "import");
  assert!(
    matches!(&f.controller.borrow().after_save,Some(AfterSave::Action(action))if action=="import")
  );
  let action = ShortcutAction::ScreenshotArea;
  begin_capture(10, action.id());
  assert!(matches!(
    f.controller.borrow().after_save,
    Some(AfterSave::Capture(_))
  ));
  cancel_current_capture();
  assert!(f.controller.borrow().after_save.is_none());
  assert!(
    !f.controller.borrow().capture_busy,
    "queued capture must not acquire native input"
  );
  finish_save(10, request, Err("save prerequisite failed".into()));
  assert!(f.window.global::<SettingsState>().get_save_failed());
}
