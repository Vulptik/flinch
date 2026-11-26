use super::*;

fn app() -> AppHandle {
  AppHandle::isolated(Config::default(), std::path::PathBuf::new())
}

#[test]
fn configured_modifier_conflict_cannot_silently_register_an_unrequested_plain_printscreen_binding()
{
  let app = app();
  let shortcut: crate::app::hotkeys::Shortcut = "Ctrl+PrintScreen".parse().unwrap();
  app.test_hotkeys().unwrap().reject(
    shortcut,
    "configured combination is owned by another application",
  );
  let definition = shortcut_definitions()
    .iter()
    .find(|entry| entry.shortcut_type == ShortcutType::ScreenshotArea)
    .unwrap();
  let result = (definition.register_fn)(&app, "Ctrl+PrintScreen".into());
  assert!(
    result.is_err(),
    "a different key must not be reported as the configured binding's success"
  );
  assert_eq!(app.test_hotkeys().unwrap().calls(), vec![shortcut]);
  assert!(!app.global_shortcut().is_registered("PrintScreen").unwrap());
}

#[test]
fn every_shortcut_registers_through_the_real_definition_and_state_pipeline_without_os_keys() {
  let app = app();
  let mut config = Config::current(&app);
  config.screenshot_area_shortcut = "Ctrl+Shift+F1".into();
  config.screenshot_fullscreen_shortcut = "Ctrl+Shift+F2".into();
  config.screenshot_active_window_shortcut = "Ctrl+Shift+F3".into();
  config.record_area_shortcut = "Ctrl+Shift+F4".into();
  config.record_fullscreen_shortcut = "Ctrl+Shift+F5".into();
  config.record_active_window_shortcut = "Ctrl+Shift+F6".into();
  config.gif_shortcut = "Ctrl+Shift+F7".into();
  config.screenshot_edit_shortcut = "Ctrl+Shift+F8".into();
  config.copy_text_shortcut = "Ctrl+Shift+F9".into();
  config.color_picker_shortcut = "Ctrl+Shift+F10".into();
  config.last_region_shortcut = "Ctrl+Shift+F11".into();
  app
    .try_state::<crate::state::ConfigState>()
    .unwrap()
    .replace(config.clone());
  setup_global_shortcuts(&app);
  let state = app.try_state::<ShortcutStateHolder>().unwrap();
  assert_eq!(state.get_all().len(), 11);
  assert!(
    state
      .get_all()
      .values()
      .all(|entry| entry.status == ShortcutStatus::Registered)
  );
  for entry in collect_registration_entries(&config) {
    assert!(
      app
        .global_shortcut()
        .is_registered(&entry.shortcut)
        .unwrap()
    );
    test_shortcut_registration(&app, &entry.shortcut).unwrap();
  }
  let before = app.test_hotkeys().unwrap().calls().len();
  state.increment_disable_depth();
  setup_global_shortcuts(&app);
  assert_eq!(app.test_hotkeys().unwrap().calls().len(), before);
  unregister_all_shortcuts(&app).unwrap();
  assert!(
    !app
      .global_shortcut()
      .is_registered("Ctrl+Shift+F1")
      .unwrap()
  );
  for definition in shortcut_definitions() {
    assert!((definition.register_fn)(&app, "invalid+no-such-key".into()).is_err());
    let shortcut: crate::app::hotkeys::Shortcut = "Alt+Shift+F12".parse().unwrap();
    app
      .test_hotkeys()
      .unwrap()
      .reject(shortcut, "synthetic native rejection");
    assert!(
      (definition.register_fn)(&app, "Alt+Shift+F12".into())
        .unwrap_err()
        .contains("synthetic native rejection")
    );
  }
  assert!(test_shortcut_registration(&app, "invalid+no-such-key").is_err());
  test_shortcut_registration(&app, "").unwrap();
  test_shortcut_registration(&app, "Ctrl+Alt+F11").unwrap();
  assert!(!app.global_shortcut().is_registered("Ctrl+Alt+F11").unwrap());
  assert!(
    test_shortcut_registration(&app, "Alt+Shift+F12")
      .unwrap_err()
      .contains("synthetic native rejection")
  );
}
