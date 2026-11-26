// src/shortcuts/registration.rs
// Global shortcut registration and setup

use super::normalize::normalize_shortcut;
use super::registry::{
  EmptyLogLevel, ShortcutDefinition, canonicalize_shortcut, shortcut_definitions, shortcut_value,
};
use super::state::{ShortcutStateHolder, ShortcutStatus, ShortcutType};
use crate::app::context::AppHandle;
use crate::app::hotkeys::GlobalShortcutExt;
use crate::config::Config;
use crate::error::rich_display_context;

#[derive(Clone)]
struct ShortcutRegistrationEntry {
  definition: &'static ShortcutDefinition,
  shortcut: String,
}

fn collect_registration_entries(config: &Config) -> Vec<ShortcutRegistrationEntry> {
  shortcut_definitions()
    .iter()
    .map(|definition| ShortcutRegistrationEntry {
      definition,
      shortcut: canonicalize_shortcut(shortcut_value(config, definition.shortcut_type)),
    })
    .collect()
}

/// Result of attempting to register a single shortcut.
struct RegistrationResult {
  shortcut_type: ShortcutType,
  shortcut: String,
  status: ShortcutStatus,
  #[allow(dead_code)]
  error: Option<String>,
}

fn registration_result(
  shortcut_type: ShortcutType,
  shortcut: String,
  status: ShortcutStatus,
  error: Option<String>,
) -> RegistrationResult {
  RegistrationResult {
    shortcut_type,
    shortcut,
    status,
    error,
  }
}

fn try_register_shortcut_impl<F>(
  shortcut: &str,
  shortcut_type: ShortcutType,
  empty_level: EmptyLogLevel,
  register: F,
) -> RegistrationResult
where
  F: FnOnce() -> Result<(), String>,
{
  if shortcut.is_empty() {
    let empty_message = format!(
      "{} shortcut is empty, skipping registration",
      shortcut_type.human_label()
    );
    match empty_level {
      EmptyLogLevel::Warn => tracing::warn!(
        operation = "shortcut_registration",
        shortcut_type = shortcut_type.config_key(),
        "{}",
        empty_message
      ),
      EmptyLogLevel::Debug => tracing::debug!(
        operation = "shortcut_registration",
        shortcut_type = shortcut_type.config_key(),
        "{}",
        empty_message
      ),
    }
    return registration_result(shortcut_type, String::new(), ShortcutStatus::Disabled, None);
  }

  match register() {
    Ok(()) => {
      tracing::debug!(
        operation = "shortcut_registration",
        shortcut_type = shortcut_type.config_key(),
        shortcut = %shortcut,
        "Shortcut registered successfully"
      );
      registration_result(
        shortcut_type,
        shortcut.to_string(),
        ShortcutStatus::Registered,
        None,
      )
    },
    Err(e) => {
      tracing::warn!(
        operation = "shortcut_registration",
        shortcut_type = shortcut_type.config_key(),
        shortcut = %shortcut,
        error = %e,
        "Shortcut registration failed (conflicted)"
      );
      registration_result(
        shortcut_type,
        shortcut.to_string(),
        ShortcutStatus::Conflicted,
        Some(e),
      )
    },
  }
}

/// Try to register an optional shortcut. Returns the registration result.
fn try_register_optional<F>(
  app: &AppHandle,
  shortcut: &str,
  shortcut_type: ShortcutType,
  empty_level: EmptyLogLevel,
  register: F,
) -> RegistrationResult
where
  F: FnOnce(&AppHandle, String) -> Result<(), String>,
{
  try_register_shortcut_impl(shortcut, shortcut_type, empty_level, || {
    register(app, shortcut.to_string())
  })
}

/// Register shortcuts using the native owner-thread hotkey adapter.
/// Returns the list of registration results for all shortcuts.
fn register_plugin_shortcuts(
  app: &AppHandle,
  entries: &[ShortcutRegistrationEntry],
) -> Vec<RegistrationResult> {
  if let Err(e) = app.global_shortcut().unregister_all() {
    tracing::warn!(
        operation = "shortcut_registration",
        step = "unregister_existing",
        error = %e,
        "Failed to unregister existing shortcuts"
    );
  }

  let mut results = Vec::with_capacity(entries.len());

  for entry in entries {
    let result = try_register_optional(
      app,
      &entry.shortcut,
      entry.definition.shortcut_type,
      entry.definition.empty_log_level,
      entry.definition.register_fn,
    );
    results.push(result);
  }

  results
}

/// Update the shortcut state holder with registration results.
fn update_state_from_results(app: &AppHandle, results: &[RegistrationResult]) {
  if let Some(state) = app.try_state::<ShortcutStateHolder>() {
    state.clear();
    for result in results {
      state.set(result.shortcut_type, result.shortcut.clone(), result.status);
    }
  }
}

/// Count conflicted shortcuts from results.
fn count_conflicts(results: &[RegistrationResult]) -> usize {
  results
    .iter()
    .filter(|r| r.status == ShortcutStatus::Conflicted)
    .count()
}

/// Show a dialog if there are conflicted shortcuts.
/// Only shows once per app session to avoid spamming the user.
fn notify_conflicts_if_any(app: &AppHandle, results: &[RegistrationResult]) {
  notify_conflicts_with(app, results, show_shortcut_conflict_dialog, |app| {
    crate::commands::show_config_window_with_section(app, "hotkeys")
  });
}

fn notify_conflicts_with(
  app: &AppHandle,
  results: &[RegistrationResult],
  dialog: impl FnOnce(&str, &str, &str, &str) -> Result<ShortcutConflictDialogAction, String>,
  open: impl FnOnce(&AppHandle) -> Result<(), String>,
) {
  let conflicted: Vec<_> = results
    .iter()
    .filter(|r| r.status == ShortcutStatus::Conflicted)
    .collect();

  if conflicted.is_empty() {
    return;
  }

  // Only show the dialog once per session
  let should_notify = app
    .try_state::<ShortcutStateHolder>()
    .map(|state| state.try_mark_startup_notification_shown())
    .unwrap_or(false);

  if !should_notify {
    tracing::debug!(
      operation = "shortcut_conflict_dialog",
      conflict_count = conflicted.len(),
      "Skipping dialog (already shown this session)"
    );
    return;
  }

  let lang = crate::i18n::get_lang(app);
  let t = crate::i18n::get(&lang);
  let title = t.shortcut_warning_title;
  let body = t.body_shortcut_conflict_warning;
  let open_settings_label = t.shortcut_warning_open_settings;
  let dismiss_label = t.shortcut_warning_dismiss;

  match dialog(title, body, open_settings_label, dismiss_label) {
    Ok(ShortcutConflictDialogAction::Dismissed) => {
      tracing::info!(
        operation = "shortcut_conflict_dialog",
        conflict_count = conflicted.len(),
        "Shown shortcut conflict warning"
      );
    },
    Ok(ShortcutConflictDialogAction::OpenSettings) => {
      if let Err(error) = open(app) {
        tracing::warn!(
          operation = "shortcut_conflict_dialog",
          conflict_count = conflicted.len(),
          error = %error,
          "User asked to open hotkey settings, but navigation failed"
        );
      }
    },
    Err(error) => {
      tracing::warn!(
        operation = "shortcut_conflict_dialog",
        conflict_count = conflicted.len(),
        error = %error,
        "Failed to show shortcut conflict dialog"
      );
    },
  }
}

enum ShortcutConflictDialogAction {
  OpenSettings,
  Dismissed,
}

#[cfg(target_os = "windows")]
fn show_shortcut_conflict_dialog(
  title: &str,
  body: &str,
  open_settings_label: &str,
  dismiss_label: &str,
) -> Result<ShortcutConflictDialogAction, String> {
  use crate::dialogs::{
    TaskDialogButton, TaskDialogCommonButtons, TaskDialogIcon, TaskDialogOptions, show_task_dialog,
  };

  const OPEN_SETTINGS_BUTTON_ID: i32 = 1001;
  const DISMISS_BUTTON_ID: i32 = 1002;

  let buttons = [
    TaskDialogButton {
      id: OPEN_SETTINGS_BUTTON_ID,
      label: open_settings_label,
    },
    TaskDialogButton {
      id: DISMISS_BUTTON_ID,
      label: dismiss_label,
    },
  ];
  let result = show_task_dialog(
    &TaskDialogOptions::new("Flinch", title, body)
      .with_icon(TaskDialogIcon::Warning)
      .with_common_buttons(TaskDialogCommonButtons::None)
      .with_custom_buttons(&buttons)
      .with_default_button(OPEN_SETTINGS_BUTTON_ID),
  )
  .map_err(|error| rich_display_context("TaskDialogIndirect failed", error))?;

  Ok(if result.button_id == OPEN_SETTINGS_BUTTON_ID {
    ShortcutConflictDialogAction::OpenSettings
  } else {
    ShortcutConflictDialogAction::Dismissed
  })
}

#[cfg(not(target_os = "windows"))]
fn show_shortcut_conflict_dialog(
  _title: &str,
  _body: &str,
  _open_settings_label: &str,
  _dismiss_label: &str,
) -> Result<ShortcutConflictDialogAction, String> {
  Ok(ShortcutConflictDialogAction::Dismissed)
}

/// Retry registration with 500ms then 1000ms delays. Used when initial registration fails.
async fn do_register_with_retries(app: AppHandle, entries: Vec<ShortcutRegistrationEntry>) {
  register_with_retries(
    app,
    entries,
    register_plugin_shortcuts,
    notify_conflicts_if_any,
    |delay| tokio::time::sleep(tokio::time::Duration::from_millis(delay)),
  )
  .await;
}

async fn register_with_retries<Fut: std::future::Future<Output = ()>>(
  app: AppHandle,
  entries: Vec<ShortcutRegistrationEntry>,
  mut register: impl FnMut(&AppHandle, &[ShortcutRegistrationEntry]) -> Vec<RegistrationResult>,
  mut notify: impl FnMut(&AppHandle, &[RegistrationResult]),
  mut delay: impl FnMut(u64) -> Fut,
) {
  let max_attempts = RETRY_DELAYS_MS.len() + 1;

  for (attempt_index, delay_ms) in RETRY_DELAYS_MS.iter().copied().enumerate() {
    let attempt = attempt_index + 2;
    delay(delay_ms).await;

    // A field can begin editing or save new bindings while this timer is pending.
    // Resume registration through the final capture release, using current config.
    if app
      .try_state::<ShortcutStateHolder>()
      .is_some_and(|state| state.is_shortcuts_disabled())
      || !registration_entries_match(
        &entries,
        &collect_registration_entries(&Config::current(&app)),
      )
    {
      tracing::debug!(
        operation = "setup_global_shortcuts",
        "Cancelled paused or stale shortcut registration retry"
      );
      return;
    }

    tracing::debug!(
      operation = "setup_global_shortcuts",
      attempt,
      delay_ms,
      "Retrying shortcut registration"
    );

    let results = register(&app, &entries);
    let conflict_count = count_conflicts(&results);
    update_state_from_results(&app, &results);

    if conflict_count == 0 {
      tracing::info!(
        operation = "setup_global_shortcuts",
        attempt,
        "All shortcuts registered successfully on retry"
      );
      return;
    }

    if attempt == max_attempts {
      tracing::warn!(
        operation = "setup_global_shortcuts",
        max_attempts,
        conflict_count,
        "Some shortcuts remain conflicted after all retry attempts"
      );
      notify(&app, &results);
    } else {
      tracing::warn!(
        operation = "setup_global_shortcuts",
        attempt,
        conflict_count,
        "Retry still has conflicts, will try again"
      );
    }
  }
}

pub fn setup_global_shortcuts(app: &AppHandle) {
  // If any component (e.g. hotkey settings panel) has requested
  // shortcuts to remain disabled, skip actual registration. The next call to
  // set_global_shortcut_enabled(true) will re-invoke this function once the depth
  // reaches zero, at which point the latest saved config will be used.
  if let Some(state) = app.try_state::<ShortcutStateHolder>()
    && state.is_shortcuts_disabled()
  {
    tracing::debug!(
      operation = "setup_global_shortcuts",
      "Skipping registration: shortcuts are currently disabled by a UI component"
    );
    return;
  }

  tracing::info!(
    operation = "setup_global_shortcuts",
    method = "native-global-hotkey",
    "Setting up global shortcuts"
  );

  let config = Config::current(app);
  let entries = collect_registration_entries(&config);
  for entry in &entries {
    tracing::debug!(
      operation = "setup_global_shortcuts",
      shortcut_type = entry.definition.config_key,
      shortcut = %entry.shortcut,
      "Normalized shortcut before registration"
    );
  }

  let results = register_plugin_shortcuts(app, &entries);
  let conflict_count = count_conflicts(&results);
  update_state_from_results(app, &results);

  if conflict_count == 0 {
    tracing::info!(
      operation = "setup_global_shortcuts",
      "All shortcuts registered successfully"
    );
  } else {
    tracing::warn!(
      operation = "setup_global_shortcuts",
      attempt = 1,
      conflict_count,
      "Initial registration has conflicts, will retry"
    );
    let app_handle = app.clone();
    crate::app::runtime::spawn(async move {
      do_register_with_retries(app_handle, entries).await;
    });
  }
}

/// Unregister all configured shortcuts during shortcut capture.
pub fn unregister_all_shortcuts(app: &AppHandle) -> Result<(), String> {
  app
    .global_shortcut()
    .unregister_all()
    .map_err(|error| rich_display_context("Failed to unregister shortcuts", error))?;
  tracing::debug!(
    operation = "unregister_all_shortcuts",
    "Unregistered all shortcuts"
  );
  Ok(())
}
/// Setup shortcuts with async delay - useful for first launch scenarios
/// This version waits before attempting registration to ensure the app is fully initialized
pub fn setup_global_shortcuts_delayed(app: &AppHandle, delay_ms: u64) {
  let app_handle = app.clone();

  crate::app::runtime::spawn(async move {
    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
    setup_global_shortcuts(&app_handle);
  });
}

/// Test if a shortcut can be registered without keeping it registered.
/// Used by the settings UI to validate shortcuts before saving.
/// Returns Ok(()) if the shortcut can be registered, Err(message) if it conflicts.
pub fn test_shortcut_registration(app: &AppHandle, shortcut: &str) -> Result<(), String> {
  use crate::app::hotkeys::Shortcut;

  if shortcut.is_empty() {
    return Ok(());
  }

  let normalized = normalize_shortcut(shortcut);
  if normalized.is_empty() {
    return Ok(());
  }

  let parsed: Shortcut = normalized
    .parse()
    .map_err(|e| rich_display_context("Invalid shortcut format", e))?;

  let global_shortcut = app.global_shortcut();

  // Check if already registered by us (use string slice form)
  if global_shortcut.is_registered(normalized.as_str())? {
    return Ok(());
  }

  // Try to register temporarily (clone for unregister)
  let parsed_for_unregister = parsed;
  let register_result = global_shortcut.on_shortcut(parsed, |_app, _shortcut, _event| {
    // No-op handler for testing
  });

  match register_result {
    Ok(()) => {
      // Successfully registered, now unregister
      global_shortcut
        .unregister(parsed_for_unregister)
        .map_err(|error| {
          rich_display_context("Cannot remove temporary shortcut validation handler", error)
        })?;
      Ok(())
    },
    Err(e) => Err(format!(
      "Inactive — shortcut already used by another application ({})",
      e
    )),
  }
}

const RETRY_DELAYS_MS: &[u64] = &[500, 1000];

fn registration_entries_match(
  previous: &[ShortcutRegistrationEntry],
  current: &[ShortcutRegistrationEntry],
) -> bool {
  previous.len() == current.len()
    && previous.iter().zip(current).all(|(previous, current)| {
      previous.definition.config_key == current.definition.config_key
        && previous.shortcut == current.shortcut
    })
}

#[cfg(test)]
mod retry_tests;

#[cfg(test)]
mod adapter_tests;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registration_retry_detects_changed_bindings() {
    let config = Config::default();
    let entries = collect_registration_entries(&config);
    assert!(registration_entries_match(
      &entries,
      &collect_registration_entries(&config)
    ));
    let mut changed = config;
    changed.screenshot_area_shortcut = "Ctrl+Shift+F10".to_string();
    assert!(!registration_entries_match(
      &entries,
      &collect_registration_entries(&changed)
    ));
  }

  fn make_registration_result(
    shortcut_type: ShortcutType,
    shortcut: &str,
    status: ShortcutStatus,
  ) -> RegistrationResult {
    RegistrationResult {
      shortcut_type,
      shortcut: shortcut.to_string(),
      status,
      error: None,
    }
  }

  #[test]
  fn collect_registration_entries_normalize_config_values() {
    let config = Config {
      screenshot_area_shortcut: " ctrl + shift + 3 ".to_string(),
      screenshot_fullscreen_shortcut: " ctrl + shift + 4 ".to_string(),
      screenshot_active_window_shortcut: "alt + printscreen".to_string(),
      record_area_shortcut: " ctrl + alt + r ".to_string(),
      record_fullscreen_shortcut: " ctrl + alt + f ".to_string(),
      record_active_window_shortcut: " ctrl + alt + w ".to_string(),
      gif_shortcut: " ctrl + alt + g ".to_string(),
      screenshot_edit_shortcut: " ctrl + alt + e ".to_string(),
      copy_text_shortcut: " ctrl + alt + t ".to_string(),
      ..Config::default()
    };

    let entries = collect_registration_entries(&config);
    let get = |shortcut_type| {
      entries
        .iter()
        .find(|entry| entry.definition.shortcut_type == shortcut_type)
        .map(|entry| entry.shortcut.as_str())
        .unwrap()
    };

    assert_eq!(get(ShortcutType::ScreenshotArea), "Ctrl+Shift+3");
    assert_eq!(get(ShortcutType::ScreenshotFullscreen), "Ctrl+Shift+4");
    assert_eq!(get(ShortcutType::ScreenshotActiveWindow), "Alt+PrintScreen");
    assert_eq!(get(ShortcutType::RecordArea), "Ctrl+Alt+R");
    assert_eq!(get(ShortcutType::RecordFullscreen), "Ctrl+Alt+F");
    assert_eq!(get(ShortcutType::RecordActiveWindow), "Ctrl+Alt+W");
    assert_eq!(get(ShortcutType::Gif), "Ctrl+Alt+G");
    assert_eq!(get(ShortcutType::ScreenshotEdit), "Ctrl+Alt+E");
    assert_eq!(get(ShortcutType::CopyText), "Ctrl+Alt+T");
  }

  #[test]
  fn try_register_optional_marks_empty_shortcuts_as_disabled() {
    let result = try_register_shortcut_impl("", ShortcutType::Gif, EmptyLogLevel::Debug, || Ok(()));

    assert_eq!(result.shortcut_type, ShortcutType::Gif);
    assert_eq!(result.shortcut, "");
    assert_eq!(result.status, ShortcutStatus::Disabled);
    assert_eq!(result.error, None);
  }

  #[test]
  fn try_register_optional_marks_successful_registration() {
    let result = try_register_shortcut_impl(
      "Ctrl+Shift+S",
      ShortcutType::ScreenshotArea,
      EmptyLogLevel::Warn,
      || Ok(()),
    );

    assert_eq!(result.status, ShortcutStatus::Registered);
    assert_eq!(result.shortcut, "Ctrl+Shift+S");
    assert_eq!(result.error, None);
  }

  #[test]
  fn try_register_optional_marks_failed_registration_as_conflicted() {
    let result = try_register_shortcut_impl(
      "Ctrl+Shift+S",
      ShortcutType::ScreenshotArea,
      EmptyLogLevel::Warn,
      || Err("already registered".to_string()),
    );

    assert_eq!(result.status, ShortcutStatus::Conflicted);
    assert_eq!(result.error.as_deref(), Some("already registered"));
  }

  fn isolated_app() -> AppHandle {
    AppHandle::isolated(
      Config {
        language: "en".into(),
        ..Config::default()
      },
      std::path::PathBuf::new(),
    )
  }

  /// One conflict warning per session, in the app language; a dialog or navigation failure is
  /// logged and never retried as another dialog.
  #[test]
  fn conflicts_warn_once_per_session_and_failures_are_logged() {
    use std::cell::Cell;
    let conflicted = [make_registration_result(
      ShortcutType::Gif,
      "Ctrl+Shift+G",
      ShortcutStatus::Conflicted,
    )];
    let registered = [make_registration_result(
      ShortcutType::Gif,
      "Ctrl+Shift+G",
      ShortcutStatus::Registered,
    )];
    let shown = Cell::new(0);
    let app = isolated_app();
    notify_conflicts_with(
      &app,
      &registered,
      |_, _, _, _| panic!("no conflict, no dialog"),
      |_| panic!("no navigation"),
    );
    notify_conflicts_with(
      &app,
      &conflicted,
      |title, body, open, dismiss| {
        shown.set(shown.get() + 1);
        let t = crate::i18n::get("en");
        assert_eq!(
          (title, body, open, dismiss),
          (
            t.shortcut_warning_title,
            t.body_shortcut_conflict_warning,
            t.shortcut_warning_open_settings,
            t.shortcut_warning_dismiss
          )
        );
        Err("dialog failed".into())
      },
      |_| panic!("a failed dialog opens nothing"),
    );
    notify_conflicts_with(
      &app,
      &conflicted,
      |_, _, _, _| panic!("already shown this session"),
      |_| panic!("no navigation"),
    );
    assert_eq!(shown.get(), 1);

    let opened = Cell::new(false);
    notify_conflicts_with(
      &isolated_app(),
      &conflicted,
      |_, _, _, _| Ok(ShortcutConflictDialogAction::OpenSettings),
      |_| {
        opened.set(true);
        Err("settings unavailable".into())
      },
    );
    assert!(opened.get());
    notify_conflicts_with(
      &isolated_app(),
      &conflicted,
      |_, _, _, _| Ok(ShortcutConflictDialogAction::Dismissed),
      |_| panic!("dismissed opens nothing"),
    );
    // An empty binding is disabled at either log level without registering anything.
    for level in [EmptyLogLevel::Warn, EmptyLogLevel::Debug] {
      let result = try_register_optional(&app, "", ShortcutType::CopyText, level, |_, _| {
        panic!("an empty shortcut is never registered")
      });
      assert_eq!(result.status, ShortcutStatus::Disabled);
    }
  }

  #[test]
  fn count_conflicts_only_counts_conflicted_shortcuts() {
    let results = vec![
      make_registration_result(
        ShortcutType::ScreenshotArea,
        "Ctrl+Shift+S",
        ShortcutStatus::Registered,
      ),
      make_registration_result(
        ShortcutType::ScreenshotFullscreen,
        "Ctrl+Shift+F",
        ShortcutStatus::Conflicted,
      ),
      make_registration_result(ShortcutType::Gif, "", ShortcutStatus::Disabled),
      make_registration_result(
        ShortcutType::RecordArea,
        "Ctrl+Shift+R",
        ShortcutStatus::Conflicted,
      ),
    ];

    assert_eq!(count_conflicts(&results), 2);
  }

  #[test]
  fn shortcut_registry_covers_each_supported_shortcut_type_once() {
    let definitions = shortcut_definitions();

    assert_eq!(definitions.len(), 11);
    assert_eq!(definitions[0].shortcut_type, ShortcutType::ScreenshotArea);
    assert_eq!(
      definitions[1].shortcut_type,
      ShortcutType::ScreenshotFullscreen
    );
    assert_eq!(
      definitions[2].shortcut_type,
      ShortcutType::ScreenshotActiveWindow
    );
    assert_eq!(definitions[3].shortcut_type, ShortcutType::RecordArea);
    assert_eq!(definitions[4].shortcut_type, ShortcutType::RecordFullscreen);
    assert_eq!(
      definitions[5].shortcut_type,
      ShortcutType::RecordActiveWindow
    );
    assert_eq!(definitions[6].shortcut_type, ShortcutType::Gif);
    assert_eq!(definitions[7].shortcut_type, ShortcutType::ScreenshotEdit);
    assert_eq!(definitions[8].shortcut_type, ShortcutType::CopyText);
    assert_eq!(definitions[9].shortcut_type, ShortcutType::PickColor);
    assert_eq!(definitions[10].shortcut_type, ShortcutType::LastRegion);
  }
}
