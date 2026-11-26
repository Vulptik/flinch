use super::*;
use std::cell::Cell;

fn app() -> AppHandle {
  AppHandle::isolated(
    Config {
      language: "en".into(),
      ..Default::default()
    },
    std::path::PathBuf::new(),
  )
}
fn results(entries: &[ShortcutRegistrationEntry], succeeds: bool) -> Vec<RegistrationResult> {
  entries
    .iter()
    .map(|entry| {
      try_register_shortcut_impl(
        &entry.shortcut,
        entry.definition.shortcut_type,
        entry.definition.empty_log_level,
        || {
          if succeeds {
            Ok(())
          } else {
            Err("synthetic shortcut conflict".into())
          }
        },
      )
    })
    .collect()
}

#[tokio::test]
async fn registration_retry_delays_are_bounded_recover_success_and_publish_final_conflicts_once() {
  for success_on_attempt in [1, 2, 3] {
    let app = app();
    let entries = collect_registration_entries(&Config::current(&app));
    let mut attempts = 0;
    let mut delays = Vec::new();
    let notices = Cell::new(0);
    register_with_retries(
      app.clone(),
      entries,
      |_, entries| {
        attempts += 1;
        results(entries, attempts >= success_on_attempt)
      },
      |_, results| {
        assert!(count_conflicts(results) > 0);
        notices.set(notices.get() + 1);
      },
      |delay| {
        delays.push(delay);
        std::future::ready(())
      },
    )
    .await;
    assert_eq!(attempts, success_on_attempt.min(2));
    assert_eq!(
      delays,
      if success_on_attempt == 1 {
        vec![500]
      } else {
        vec![500, 1000]
      }
    );
    assert_eq!(notices.get(), usize::from(success_on_attempt == 3));
    let state = app.try_state::<ShortcutStateHolder>().unwrap();
    assert_eq!(state.get_all().len(), shortcut_definitions().len());
    assert!(state.get_all().values().any(|info| info.status
      == if success_on_attempt == 3 {
        ShortcutStatus::Conflicted
      } else {
        ShortcutStatus::Registered
      }));
  }
}

#[tokio::test]
async fn editing_or_replacing_bindings_during_retry_delay_cancels_stale_registration() {
  for editing in [false, true] {
    let app = app();
    let entries = collect_registration_entries(&Config::current(&app));
    let state = app.try_state::<ShortcutStateHolder>().unwrap();
    state.set(
      ShortcutType::Gif,
      "original".into(),
      ShortcutStatus::Registered,
    );
    register_with_retries(
      app.clone(),
      entries,
      |_, _| panic!("a stale retry must never reach the OS adapter"),
      |_, _| panic!("a cancelled retry must never display a conflict warning"),
      |_| {
        if editing {
          state.increment_disable_depth();
        } else {
          let mut config = Config::current(&app);
          config.screenshot_area_shortcut = "Ctrl+Alt+F8".into();
          app
            .try_state::<crate::state::ConfigState>()
            .unwrap()
            .replace(config);
        }
        std::future::ready(())
      },
    )
    .await;
    assert_eq!(state.get_all()[&ShortcutType::Gif].shortcut, "original");
  }
}

#[test]
fn conflict_notice_is_localized_once_per_session_and_navigation_failures_do_not_retry_it() {
  for language in ["en", "fr", "de", "es", "it"] {
    for outcome in [0, 1, 2] {
      let app = AppHandle::isolated(
        Config {
          language: language.into(),
          ..Default::default()
        },
        std::path::PathBuf::new(),
      );
      let entries = collect_registration_entries(&Config::current(&app));
      let conflicts = results(&entries, false);
      notify_conflicts_with(
        &app,
        &results(&entries, true),
        |_, _, _, _| panic!("successful registrations need no warning"),
        |_| panic!("no navigation"),
      );
      let calls = Cell::new(0);
      let opened = Cell::new(0);
      notify_conflicts_with(
        &app,
        &conflicts,
        |title, body, open, dismiss| {
          assert_eq!(title, crate::i18n::get(language).shortcut_warning_title);
          assert!(!body.is_empty() && !open.is_empty() && !dismiss.is_empty());
          calls.set(calls.get() + 1);
          match outcome {
            0 => Ok(ShortcutConflictDialogAction::Dismissed),
            1 => Ok(ShortcutConflictDialogAction::OpenSettings),
            _ => Err("dialog creation failed".into()),
          }
        },
        |_| {
          opened.set(opened.get() + 1);
          Err("settings dispatch unavailable".into())
        },
      );
      assert_eq!(calls.get(), 1);
      assert_eq!(opened.get(), usize::from(outcome == 1));
      notify_conflicts_with(
        &app,
        &conflicts,
        |_, _, _, _| panic!("warning must not repeat this session"),
        |_| panic!("no repeated navigation"),
      );
    }
  }
}
