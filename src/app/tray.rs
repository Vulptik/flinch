//! Tray and menus stay on the Slint/Win32 owner thread.
use super::{
  context::AppHandle,
  icon::{TrayIconState, build_recording_tray_icon, load_tray_icon},
};
use std::{
  cell::RefCell,
  sync::atomic::{AtomicBool, Ordering},
};
use tray_icon::{
  MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
  menu::{CheckMenuItem, ContextMenu, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};
/// The captures the current menu lists, so a click acts on exactly what was shown.
static MENU_RECENT: std::sync::Mutex<Vec<crate::recent_captures::RecentCapture>> =
  std::sync::Mutex::new(Vec::new());
/// Tests that replace the menu's recent captures hold this, so parallel cases never interleave.
#[cfg(test)]
pub(super) static MENU_RECENT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct Tray {
  icon: TrayIcon,
}
thread_local! { static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) }; }
static UPDATE_PENDING: AtomicBool = AtomicBool::new(false);
pub fn setup_tray_icon(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
  if let Err(error) = super::tray_theme::enable_system_theme() {
    tracing::warn!(%error, "Cannot enable Windows tray menu theme");
  }
  let icon = load_tray_icon()
    .ok_or("Embedded tray artwork could not be decoded")?
    .into_tray()?;
  let icon = TrayIconBuilder::new()
    .with_id("main")
    .with_tooltip("Flinch")
    .with_icon(icon)
    .with_menu_on_left_click(false)
    .build()?;
  if let Err(error) = super::tray_theme::watch_system_theme(icon.window_handle()) {
    tracing::warn!(%error, "Cannot watch Windows tray menu theme changes");
  }
  TRAY.with(|slot| {
    *slot.borrow_mut() = Some(Tray { icon });
  });
  super::tray_menu::schedule_prewarm(app);
  let menu_app = app.downgrade();
  MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
    if let Some(app) = menu_app.upgrade() {
      dispatch(app, event.id.as_ref());
    }
  }));
  let click_app = app.downgrade();
  TrayIconEvent::set_event_handler(Some(move |event| {
    let TrayIconEvent::Click {
      button,
      button_state: MouseButtonState::Up,
      position,
      ..
    } = event
    else {
      return;
    };
    let Some(app) = click_app.upgrade() else {
      return;
    };
    match button {
      MouseButton::Left if crate::state::current_recording_session(&app).is_none() => {
        dispatch(app, "idle-screenshot");
      },
      // The native fallback menu, once in use, opens by itself on right-click.
      MouseButton::Right if !super::tray_menu::uses_native_menu() => {
        super::tray_menu::request_open(&app, position.x.round() as i32, position.y.round() as i32);
      },
      _ => {},
    }
  }));
  Ok(())
}
/// The custom menu could not be created: from now on the tray uses the native menu, shown right
/// away when a click is waiting for it.
pub(super) fn use_native_menu(app: &AppHandle, show: bool) {
  let language = crate::i18n::get_lang(app);
  let paused = crate::state::current_recording_session(app).map(|session| session.paused);
  let recent = crate::recent_captures::list(app);
  let delay = crate::config::Config::current(app).capture_delay;
  match build_menu(crate::i18n::tray(&language), paused, &recent, delay) {
    Ok(menu) => TRAY.with(|slot| {
      if let Some(tray) = slot.borrow().as_ref() {
        tray.icon.set_menu(Some(Box::new(menu)));
        remember_menu_recent(recent);
        if show {
          tray.icon.show_menu();
        }
      }
    }),
    Err(error) => tracing::error!(%error, "Cannot build the native tray menu"),
  }
}

/// None is idle; Some(false) is recording; Some(true) is paused.
/// Construction and replacement stay on the tray owner thread.
fn build_menu(
  labels: &crate::i18n::TrayLabels,
  paused: Option<bool>,
  recent: &[crate::recent_captures::RecentCapture],
  delay: u32,
) -> anyhow::Result<Menu> {
  let menu = Menu::new();
  let screenshot = MenuItem::with_id("screenshot", labels.screenshot, true, None);
  let last_region = MenuItem::with_id("last-region", labels.last_region, true, None);
  let capture_edit = MenuItem::with_id("capture-edit", labels.capture_edit, true, None);
  let copy_text = MenuItem::with_id("copy-text", labels.copy_text, true, None);
  let pick_color = MenuItem::with_id("pick-color", labels.color_picker, true, None);
  let record = MenuItem::with_id(
    "record",
    if paused.is_some() {
      labels.record_stop
    } else {
      labels.record_start
    },
    true,
    None,
  );
  menu.append_items(&[
    &screenshot,
    &last_region,
    &capture_edit,
    &copy_text,
    &pick_color,
  ])?;
  match paused {
    None => menu.append(&record)?,
    Some(is_paused) => {
      let pause = MenuItem::with_id(
        "pause",
        if is_paused {
          labels.record_resume
        } else {
          labels.record_pause
        },
        true,
        None,
      );
      if is_paused {
        menu.append_items(&[&pause, &record])?;
      } else {
        menu.append_items(&[&record, &pause])?;
      }
    },
  }
  menu.append_items(&[
    &PredefinedMenuItem::separator(),
    &delay_menu(labels, delay)?,
    &recent_menu(labels, recent)?,
    &PredefinedMenuItem::separator(),
    &MenuItem::with_id("hotkeys", labels.hotkeys, true, None),
    &MenuItem::with_id("settings", labels.settings, true, None),
    &MenuItem::with_id("quit", labels.quit, true, None),
  ])?;
  use windows::Win32::UI::WindowsAndMessaging::{
    HMENU, MENUINFO, MIM_STYLE, MNS_CHECKORBMP, SetMenuInfo,
  };
  let info = MENUINFO {
    cbSize: std::mem::size_of::<MENUINFO>() as u32,
    fMask: MIM_STYLE,
    dwStyle: MNS_CHECKORBMP,
    ..Default::default()
  };
  // SAFETY: the popup is owned by menu on this thread. Share the icon/check gutter
  // instead of reserving two columns; Windows still measures and paints the menu.
  unsafe { SetMenuInfo(HMENU(menu.hpopupmenu() as _), &info) }?;
  Ok(menu)
}

/// Capture Delay choices, the current one checked, as in the Slint menu.
fn delay_menu(labels: &crate::i18n::TrayLabels, delay: u32) -> anyhow::Result<Submenu> {
  let submenu = Submenu::new(labels.delay, true);
  for (seconds, label) in crate::capture_delay::CHOICES
    .into_iter()
    .zip(labels.delay_choices)
  {
    submenu.append(&CheckMenuItem::with_id(
      format!("delay-{seconds}"),
      label,
      true,
      seconds == delay,
      None,
    ))?;
  }
  Ok(submenu)
}

/// One submenu per capture, file name on the left and time on the right, each with Open, Copy and
/// Show in Folder; then the captures folder.
fn recent_menu(
  labels: &crate::i18n::TrayLabels,
  recent: &[crate::recent_captures::RecentCapture],
) -> anyhow::Result<Submenu> {
  let submenu = Submenu::new(labels.recent, true);
  if recent.is_empty() {
    submenu.append(&MenuItem::with_id(
      "recent-empty",
      labels.recent_empty,
      false,
      None,
    ))?;
  }
  for (index, capture) in recent.iter().enumerate() {
    let label = format!(
      "{}\t{}",
      crate::recent_captures::menu_name(&capture.path),
      crate::recent_captures::saved_label(capture.saved_at)
    );
    let actions = Submenu::with_items(
      label,
      true,
      &[
        &MenuItem::with_id(
          format!("recent-open-{index}"),
          labels.recent_open,
          true,
          None,
        ),
        &MenuItem::with_id(
          format!("recent-copy-{index}"),
          labels.recent_copy,
          true,
          None,
        ),
        &MenuItem::with_id(
          format!("recent-show-{index}"),
          labels.recent_show,
          true,
          None,
        ),
      ],
    )?;
    if capture.kind == crate::recent_captures::CaptureKind::Screenshot {
      actions.append(&MenuItem::with_id(
        format!("recent-pin-{index}"),
        labels.pin,
        true,
        None,
      ))?;
    }
    submenu.append(&actions)?;
  }
  submenu.append_items(&[
    &PredefinedMenuItem::separator(),
    &MenuItem::with_id("recent-folder", labels.recent_folder, true, None),
  ])?;
  Ok(submenu)
}

pub(super) fn remember_menu_recent(recent: Vec<crate::recent_captures::RecentCapture>) {
  match MENU_RECENT.lock() {
    Ok(mut shown) => *shown = recent,
    Err(error) => tracing::error!(%error, "Recent captures menu state poisoned"),
  }
}

/// The capture and action a recent-capture menu id names, from the list the menu showed.
fn recent_action(
  id: &str,
) -> Option<(
  crate::recent_captures::RecentCapture,
  crate::recent_captures::Action,
)> {
  use crate::recent_captures::Action;
  let (action, index) = [
    ("recent-open-", Action::Open),
    ("recent-copy-", Action::Copy),
    ("recent-show-", Action::Show),
    ("recent-pin-", Action::Pin),
  ]
  .into_iter()
  .find_map(|(prefix, action)| Some((action, id.strip_prefix(prefix)?.parse::<usize>().ok()?)))?;
  match MENU_RECENT.lock() {
    Ok(shown) => shown.get(index).cloned().map(|capture| (capture, action)),
    Err(error) => {
      tracing::error!(%error, "Recent captures menu state poisoned");
      None
    },
  }
}

/// The capture a tray row starts, for skipping its own Capture Delay countdown.
fn capture_trigger(id: &str) -> Option<crate::capture_delay::Trigger> {
  use crate::capture_delay::Trigger;
  Some(match id {
    "screenshot" | "idle-screenshot" => Trigger::Area,
    "last-region" => Trigger::LastRegion,
    "capture-edit" => Trigger::CaptureEdit,
    "copy-text" => Trigger::CopyText,
    "pick-color" => Trigger::PickColor,
    "record" => Trigger::RecordFullscreen,
    _ => return None,
  })
}

/// A Capture Delay choice from either menu: saved like a Settings change, then both menus follow.
/// Returns the saving task, which reports its own failure, or `None` for an unknown choice.
fn set_capture_delay(app: AppHandle, value: &str) -> Option<tokio::task::JoinHandle<()>> {
  let Some(seconds) = value
    .parse::<u32>()
    .ok()
    .filter(|seconds| crate::capture_delay::CHOICES.contains(seconds))
  else {
    tracing::warn!(value, "Unknown Capture Delay choice from the tray");
    return None;
  };
  Some(super::runtime::spawn(async move {
    let mut config = crate::config::Config::current(&app);
    if config.capture_delay == seconds {
      return;
    }
    config.capture_delay = seconds;
    match crate::commands::save_config(app.clone(), config).await {
      Ok(()) => {
        tracing::info!(seconds, "Capture Delay changed from the tray");
        update_tray_recording_state(&app);
      },
      Err(error) => {
        tracing::error!(%error, seconds, "Cannot save the Capture Delay from the tray");
        let labels = crate::i18n::tray(&crate::i18n::get_lang(&app));
        crate::notifications::show_error(
          &app,
          labels.delay_failed,
          "notification_error_capture_delay",
        );
      },
    }
  }))
}

pub(super) fn dispatch(app: AppHandle, id: &str) {
  if let Some((capture, action)) = recent_action(id) {
    super::runtime::spawn(crate::recent_captures::act(app, capture, action));
    return;
  }
  if let Some(seconds) = id.strip_prefix("delay-") {
    drop(set_capture_delay(app, seconds));
    return;
  }
  if capture_trigger(id).is_some_and(crate::capture_delay::intercept) {
    return;
  }
  match id {
    "recent-folder" => {
      super::runtime::spawn(crate::recent_captures::open_folder(app));
    },
    "settings" => {
      if let Err(error) = crate::settings::native_host::show(&app, None) {
        tracing::error!(%error, "Cannot open settings from tray");
      }
    },
    "hotkeys" => {
      let hotkeys = Some(flinch_settings::model::Section::Hotkeys);
      if let Err(error) = crate::settings::native_host::show(&app, hotkeys) {
        tracing::error!(%error, "Cannot open the Hotkeys settings from tray");
      }
    },
    "quit" => {
      if let Err(error) = crate::settings::native_host::request_exit(&app) {
        tracing::error!(%error, "Cannot quit from tray");
      }
    },
    "record" => {
      super::runtime::spawn(async move {
        crate::recording::coordinator::RecordingCoordinator::toggle_tray_record(app).await;
      });
    },
    "pause" => {
      super::runtime::spawn(async move {
        crate::recording::coordinator::RecordingCoordinator::toggle_tray_pause(app).await;
      });
    },
    "screenshot" | "idle-screenshot" => {
      let idle_only = id == "idle-screenshot";
      super::runtime::spawn(async move {
        // Recheck after scheduling: recording may have started since the tray click.
        if idle_only && crate::state::current_recording_session(&app).is_some() {
          tracing::debug!("Tray left-click screenshot skipped during recording");
          return;
        }
        if let Err(error) = crate::commands::screenshot::capture::take_screenshot(app).await {
          tracing::warn!(%error, "Tray screenshot did not complete");
        }
      });
    },
    "last-region" => {
      super::runtime::spawn(async move {
        if let Err(error) = crate::commands::screenshot::capture::take_last_region(app).await {
          tracing::warn!(%error, "Tray capture last region did not complete");
        }
      });
    },
    "capture-edit" => {
      super::runtime::spawn(async move {
        if let Err(error) = crate::commands::screenshot::capture::take_capture_and_edit(app).await {
          tracing::warn!(%error, "Tray capture and edit did not complete");
        }
      });
    },
    "pick-color" => {
      super::runtime::spawn(async move {
        if let Err(error) = crate::commands::screenshot::capture::take_color_pick(app).await {
          tracing::warn!(%error, "Tray color pick did not complete");
        }
      });
    },
    "copy-text" => {
      super::runtime::spawn(async move {
        if let Err(error) = crate::commands::screenshot::capture::take_copy_text(app).await {
          tracing::warn!(%error, "Tray copy text did not complete");
        }
      });
    },
    _ => tracing::warn!(id, "Unknown tray action"),
  }
}
pub fn apply_tray_labels(app: &AppHandle, _labels: &crate::i18n::TrayLabels, _is_recording: bool) {
  update_tray_recording_state(app);
}
pub fn update_tray_recording_state(app: &AppHandle) {
  if crate::settings::native_host::is_probe() || UPDATE_PENDING.swap(true, Ordering::AcqRel) {
    return;
  }
  let app = app.clone();
  if let Err(error) = flinch_settings::slint::invoke_from_event_loop(move || {
    UPDATE_PENDING.store(false, Ordering::Release);
    let lang = crate::i18n::get_lang(&app);
    let t = crate::i18n::tray(&lang);
    let session = crate::state::current_recording_session(&app);
    super::tray_menu::refresh(&app);
    TRAY.with(|slot| {
      if let Some(tray) = slot.borrow_mut().as_mut() {
        // Publish only a complete menu; failures retain the previous one.
        if super::tray_menu::uses_native_menu() {
          let recent = crate::recent_captures::list(&app);
          let delay = crate::config::Config::current(&app).capture_delay;
          match build_menu(t, session.map(|s| s.paused), &recent, delay) {
            Ok(menu) => {
              tray.icon.set_menu(Some(Box::new(menu)));
              remember_menu_recent(recent);
            },
            Err(error) => tracing::error!(%error, "Cannot update tray menu"),
          }
        }
        let icon = match session {
          Some(s) => Some(build_recording_tray_icon(
            32,
            if s.paused {
              TrayIconState::Paused
            } else {
              TrayIconState::Recording
            },
          )),
          None => load_tray_icon(),
        };
        let result = icon
          .ok_or_else(|| anyhow::anyhow!("Cannot decode tray artwork"))
          .and_then(|icon| icon.into_tray())
          .and_then(|icon| {
            tray.icon.set_icon(Some(icon))?;
            Ok(())
          });
        if let Err(error) = result {
          tracing::error!(%error, "Cannot update tray recording icon");
        }
      }
    });
  }) {
    UPDATE_PENDING.store(false, Ordering::Release);
    tracing::error!(%error, "Cannot schedule tray update");
  }
}
pub fn shutdown() {
  super::tray_menu::shutdown();
  // The library event handlers are OnceLocks; weak references expire when services drop.
  TRAY.with(|slot| {
    slot.borrow_mut().take();
  });
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::UI::WindowsAndMessaging::{
    GetMenuInfo, GetMenuItemInfoW, HMENU, MENUINFO, MENUITEMINFOW, MIIM_BITMAP, MIM_STYLE,
    MNS_CHECKORBMP,
  };

  /// A tray choice is saved like a Settings change; an unknown one or the current value changes
  /// nothing, and a failed save says so.
  #[test]
  fn a_capture_delay_choice_from_the_tray_is_saved_or_its_failure_reported() {
    let runtime = crate::app::runtime::test_runtime();
    let folder = tempfile::tempdir().expect("folder");
    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        notifications_enabled: false,
        notification_sound: false,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let path = folder.path().join("config.json");
    app.isolate_config_path(path.clone());
    let delay = |app: &AppHandle| crate::config::Config::current(app).capture_delay;
    for unknown in ["7", "", "-3", "ten"] {
      assert!(
        set_capture_delay(app.clone(), unknown).is_none(),
        "{unknown}"
      );
    }
    let unchanged = set_capture_delay(app.clone(), "0").expect("task");
    runtime.block_on(unchanged).expect("same value");
    assert!(!path.exists(), "the current value is not saved again");
    let saved = set_capture_delay(app.clone(), "5").expect("task");
    runtime.block_on(saved).expect("saved");
    assert_eq!(delay(&app), 5);
    let on_disk = std::fs::read_to_string(&path).expect("saved config");
    assert!(on_disk.contains("\"capture_delay\": 5"), "{on_disk}");
    // A folder in place of the file: the save fails, the delay stays and the user is told.
    let blocked = folder.path().join("blocked");
    std::fs::create_dir_all(blocked.join("config.json")).expect("blocker");
    app.isolate_config_path(blocked.join("config.json"));
    let failed = set_capture_delay(app.clone(), "10").expect("task");
    runtime.block_on(failed).expect("reported");
    assert_eq!(delay(&app), 5);
    let notices = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .expect("notifications")
      .take_test_requests();
    let body =
      crate::i18n::native_settings("en", "notification_error_capture_delay").expect("copy");
    assert_eq!(
      notices.iter().filter(|notice| notice.body == body).count(),
      1
    );
  }

  #[test]
  fn capture_rows_name_the_capture_their_countdown_belongs_to() {
    use crate::capture_delay::Trigger;
    for (id, trigger) in [
      ("screenshot", Trigger::Area),
      ("idle-screenshot", Trigger::Area),
      ("last-region", Trigger::LastRegion),
      ("capture-edit", Trigger::CaptureEdit),
      ("copy-text", Trigger::CopyText),
      ("pick-color", Trigger::PickColor),
      ("record", Trigger::RecordFullscreen),
    ] {
      assert_eq!(capture_trigger(id), Some(trigger), "{id}");
    }
    for id in [
      "pause",
      "settings",
      "hotkeys",
      "quit",
      "recent-folder",
      "delay-5",
    ] {
      assert_eq!(capture_trigger(id), None, "{id} is not a capture");
    }
  }

  #[test]
  fn native_menu_states_are_text_only_with_one_shared_gutter() {
    for language in ["en", "fr", "de", "es", "it"] {
      for paused in [None, Some(false), Some(true)] {
        let menu = build_menu(crate::i18n::tray(language), paused, &[], 5).expect("native menu");
        let items = menu.items();
        let separator = if paused.is_some() { 7 } else { 6 };
        assert_eq!(items.len(), separator + 7);
        for index in [separator, separator + 3] {
          assert!(matches!(
            items[index],
            tray_icon::menu::MenuItemKind::Predefined(_)
          ));
        }
        let tray_icon::menu::MenuItemKind::Submenu(delay) = &items[separator + 1] else {
          panic!("expected the Capture Delay submenu")
        };
        assert_eq!(delay.text(), crate::i18n::tray(language).delay);
        let choices: Vec<(String, bool)> = delay
          .items()
          .iter()
          .map(|item| match item {
            tray_icon::menu::MenuItemKind::Check(check) => {
              (check.id().as_ref().to_owned(), check.is_checked())
            },
            _ => panic!("Capture Delay choices are check items"),
          })
          .collect();
        assert_eq!(
          choices,
          [
            ("delay-0", false),
            ("delay-3", false),
            ("delay-5", true),
            ("delay-10", false)
          ]
          .map(|(id, checked)| (id.to_owned(), checked))
        );
        assert!(matches!(
          items[separator + 2],
          tray_icon::menu::MenuItemKind::Submenu(_)
        ));
        let expected = if paused == Some(true) {
          ["pause", "record"]
        } else {
          ["record", "pause"]
        };
        for (index, item) in items.iter().enumerate() {
          if (separator..=separator + 3).contains(&index) {
            continue;
          }
          let tray_icon::menu::MenuItemKind::MenuItem(item) = item else {
            panic!("expected text-only action")
          };
          if index == 1 {
            assert_eq!(item.id().as_ref(), "last-region");
            assert_eq!(item.text(), crate::i18n::tray(language).last_region);
          }
          if index == 2 {
            assert_eq!(item.id().as_ref(), "capture-edit");
          }
          if index == 3 {
            assert_eq!(item.id().as_ref(), "copy-text");
          }
          if index == 4 {
            assert_eq!(item.id().as_ref(), "pick-color");
          }
          if index == 5 {
            assert_eq!(item.id().as_ref(), expected[0]);
          }
          if index == 6 && paused.is_some() {
            assert_eq!(item.id().as_ref(), expected[1]);
          }
          if index == separator + 3 {
            assert_eq!(item.id().as_ref(), "hotkeys");
            assert_eq!(item.text(), crate::i18n::tray(language).hotkeys);
          }
          let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            ..Default::default()
          };
          // SAFETY: menu is live on this thread, index exists, info is writable.
          unsafe { GetMenuItemInfoW(HMENU(menu.hpopupmenu() as _), index as u32, true, &mut info) }
            .expect("native bitmap metadata");
          assert!(info.hbmpItem.is_invalid());
        }
        let mut info = MENUINFO {
          cbSize: std::mem::size_of::<MENUINFO>() as u32,
          fMask: MIM_STYLE,
          ..Default::default()
        };
        // SAFETY: menu is live on this thread and info is writable.
        unsafe { GetMenuInfo(HMENU(menu.hpopupmenu() as _), &mut info) }.expect("menu style");
        assert_eq!(info.dwStyle & MNS_CHECKORBMP, MNS_CHECKORBMP);
      }
    }
  }

  #[test]
  fn recent_captures_list_each_capture_with_its_actions_and_clicks_find_what_was_shown() {
    use crate::recent_captures::{Action, CaptureKind, RecentCapture};
    let labels = crate::i18n::tray("en");
    let empty = recent_menu(labels, &[]).expect("empty submenu");
    let items = empty.items();
    assert_eq!(items.len(), 3);
    let tray_icon::menu::MenuItemKind::MenuItem(placeholder) = &items[0] else {
      panic!("expected the empty placeholder")
    };
    assert!(!placeholder.is_enabled());
    let recent = [
      RecentCapture {
        path: "b.mp4".into(),
        kind: CaptureKind::Recording,
        saved_at: chrono::Utc::now().timestamp(),
      },
      RecentCapture {
        path: "a.png".into(),
        kind: CaptureKind::Screenshot,
        saved_at: 0,
      },
    ];
    let listed = recent_menu(labels, &recent).expect("submenu");
    let items = listed.items();
    assert_eq!(items.len(), recent.len() + 2);
    let tray_icon::menu::MenuItemKind::Submenu(first) = &items[0] else {
      panic!("expected a capture submenu")
    };
    let mut native = [0u16; 128];
    // SAFETY: the submenu is live on this thread and the buffer is writable.
    let written = unsafe {
      windows::Win32::UI::WindowsAndMessaging::GetMenuStringW(
        HMENU(listed.hpopupmenu() as _),
        0,
        Some(&mut native),
        windows::Win32::UI::WindowsAndMessaging::MF_BYPOSITION,
      )
    };
    let native = String::from_utf16_lossy(&native[..written as usize]);
    // Windows right-aligns the part after the tab: the file name, then the time.
    let (name, time) = native.split_once('\t').expect("name and time");
    assert_eq!(name, "b.mp4");
    assert_eq!(
      time,
      crate::recent_captures::saved_label(recent[0].saved_at)
    );
    let actions: Vec<_> = first
      .items()
      .iter()
      .map(|item| item.id().0.clone())
      .collect();
    assert_eq!(actions, ["recent-open-0", "recent-copy-0", "recent-show-0"]);
    let tray_icon::menu::MenuItemKind::MenuItem(folder) = &items[3] else {
      panic!("expected the folder action")
    };
    assert_eq!(folder.id().as_ref(), "recent-folder");

    let _recent = MENU_RECENT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    remember_menu_recent(recent.to_vec());
    assert_eq!(
      recent_action("recent-copy-1"),
      Some((recent[1].clone(), Action::Copy))
    );
    assert_eq!(
      recent_action("recent-show-0"),
      Some((recent[0].clone(), Action::Show))
    );
    assert_eq!(recent_action("recent-open-2"), None);
    assert_eq!(recent_action("recent-open-x"), None);
    assert_eq!(recent_action("settings"), None);
    remember_menu_recent(Vec::new());
  }
}
