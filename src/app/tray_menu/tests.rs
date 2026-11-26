//! The custom tray menu on a headless Slint platform with a manual clock: opening, the submenu's
//! hover delays, focus loss, bounded waits for native windows that never appear, prewarm and
//! shutdown. Offscreen adapters only; no native window, foreground change or tray action runs.
use super::*;
use crate::app::headless::{advance, drain};
use flinch_settings::slint::platform::WindowEvent;

fn menu() -> Rc<RefCell<Menu>> {
  current().expect("tray menu exists")
}

fn is_open() -> bool {
  menu().borrow().open
}

fn submenu() -> Option<usize> {
  menu().borrow().submenu
}

fn windows() -> (TrayMenuWindow, TrayRecentWindow) {
  let menu = menu();
  let state = menu.borrow();
  (state.main.clone_strong(), state.recent.clone_strong())
}

fn recent_index() -> i32 {
  let menu = menu();
  let state = menu.borrow();
  state
    .rows
    .iter()
    .position(|row| row.id == "recent")
    .expect("recent captures row") as i32
}

/// Opens while bypassing the reopen guard, which measures real time since the last close.
fn reopen(app: &AppHandle) {
  if let Some(menu) = current() {
    menu.borrow_mut().closed_at = None;
  }
  open(app, POINT { x: 200, y: 200 });
  assert!(is_open());
}

fn write_captures(folder: &std::path::Path) -> Vec<RecentCapture> {
  let captures: Vec<RecentCapture> = [
    ("b.mp4", CaptureKind::Recording),
    ("a.png", CaptureKind::Screenshot),
    (
      "a very long animated capture name that the menu shortens in the middle.gif",
      CaptureKind::Gif,
    ),
  ]
  .into_iter()
  .enumerate()
  .map(|(index, (name, kind))| {
    let path = folder.join(name);
    std::fs::write(&path, b"not media").unwrap();
    RecentCapture {
      path,
      kind,
      saved_at: 1_700_000_000 - index as i64,
    }
  })
  .collect();
  std::fs::write(
    folder.join("recent-captures.json"),
    serde_json::to_vec(&captures).unwrap(),
  )
  .unwrap();
  captures
}

// One owner thread installs the headless platform; the menu, its timers and the event queue
// are all thread-bound, so the scenarios run in sequence.
#[test]
fn headless_tray_menu_opens_navigates_closes_and_bounds_every_native_wait() {
  crate::app::headless::run(tray_menu_scenarios);
}

fn tray_menu_scenarios() {
  let _recent = crate::app::tray::MENU_RECENT_TEST_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let folder = tempfile::tempdir().unwrap();
  let captures = write_captures(folder.path());
  let app = AppHandle::isolated(
    crate::config::Config {
      language: "en".into(),
      ..Default::default()
    },
    PathBuf::new(),
  );
  app.isolate_config_path(folder.path().join("settings.json"));

  without_menu_every_entry_point_is_a_no_op(&app);
  window_hook_marks_only_menu_windows();
  right_click_opens_fills_and_toggles(&app, &captures);
  submenu_follows_hover_delays_and_keyboard(&app);
  delay_submenu_shows_the_choice_and_takes_turns_with_recent();
  thumbnails_update_rows_in_place(&captures);
  dismissal_paths_close_without_running_actions(&app);
  focus_loss_and_missing_native_windows_close_the_menu(&app);
  shutdown_hides_and_forgets_the_menu();
  prewarm_creates_hidden_windows_and_gives_up_on_native_handles(&app);
  failure_keeps_the_native_menu(&app);

  shutdown();
  // Thumbnail deliveries from the Shell worker may still be queued; they find no menu.
  drain();
  crate::app::tray::remember_menu_recent(Vec::new());
}

fn without_menu_every_entry_point_is_a_no_op(app: &AppHandle) {
  assert!(current().is_none());
  assert!(!uses_native_menu());
  refresh(app);
  focus_changed();
  shutdown();
  thumbnail_ready(PathBuf::from("missing.png"), Err("none".into()));
  assert!(current().is_none());
}

fn window_hook_marks_only_menu_windows() {
  let plain = window_attributes(WindowAttributes::default());
  assert!(plain.active);
  CREATING.with(|creating| creating.set(true));
  let menu = window_attributes(WindowAttributes::default());
  CREATING.with(|creating| creating.set(false));
  assert!(!menu.active, "menu windows never take focus by appearing");
}

fn right_click_opens_fills_and_toggles(app: &AppHandle, captures: &[RecentCapture]) {
  request_open(app, 200, 200);
  assert!(current().is_none(), "opening waits for the owner thread");
  drain();
  assert!(is_open());
  let (main, recent) = windows();
  assert!(main.window().is_visible());
  assert!(!recent.window().is_visible());
  let labels = crate::i18n::tray("en");
  let expected = layout::rows(labels, None, 0);
  assert_eq!(main.get_rows().row_count(), expected.len());
  for (index, row) in expected.iter().enumerate() {
    let shown = main.get_rows().row_data(index).unwrap();
    assert_eq!(shown.id.as_str(), row.id);
    assert_eq!(shown.label.as_str(), row.label);
    assert_eq!(shown.submenu, row.submenu);
  }
  assert_eq!(main.get_highlight(), -1);
  assert_eq!(main.get_submenu_row(), -1);
  assert!(!main.get_shown(), "the reveal waits for the native window");
  assert_eq!(recent.get_empty_label().as_str(), labels.recent_empty);
  assert_eq!(recent.get_folder_label().as_str(), labels.recent_folder);
  {
    let menu = menu();
    let state = menu.borrow();
    assert_eq!(state.captures, captures);
    assert!(state.frame.right > state.frame.left && state.frame.bottom > state.frame.top);
    assert!(state.scale >= 1.0);
  }
  let rows = recent.get_rows();
  assert_eq!(rows.row_count(), captures.len());
  let kinds: Vec<_> = (0..rows.row_count())
    .map(|index| rows.row_data(index).unwrap().kind.to_string())
    .collect();
  assert_eq!(kinds, ["video", "image", "gif"]);
  let first = rows.row_data(0).unwrap();
  assert_eq!(first.name.as_str(), "b.mp4");
  assert_eq!(
    first.time.as_str(),
    crate::recent_captures::saved_label(captures[0].saved_at)
  );
  assert!(!first.has_thumbnail);
  let long = rows.row_data(2).unwrap().name.to_string();
  assert!(long.contains('…'), "{long}");

  // A second right-click while open closes it, and the click right after is swallowed.
  open(app, POINT { x: 200, y: 200 });
  assert!(!is_open());
  assert!(!main.window().is_visible());
  open(app, POINT { x: 200, y: 200 });
  assert!(
    !is_open(),
    "the reopen guard swallows the focus-stealing click"
  );
  reopen(app);
  // An open menu refreshes in place, keeping the rows the same model.
  let model = menu().borrow().recent_rows.clone();
  refresh(app);
  assert!(Rc::ptr_eq(&model, &menu().borrow().recent_rows));
  assert_eq!(recent.get_rows().row_count(), captures.len());
  // A tray state update reaches the open menu on the owner thread, without a tray icon.
  crate::app::tray::update_tray_recording_state(app);
  drain();
  assert!(is_open());
  assert_eq!(menu().borrow().captures, captures);
  assert_eq!(recent.get_rows().row_count(), captures.len());
}

fn delay_submenu_shows_the_choice_and_takes_turns_with_recent() {
  let (main, recent) = windows();
  let choice = menu().borrow().choice.clone_strong();
  let delay = menu()
    .borrow()
    .rows
    .iter()
    .position(|row| row.id == "delay")
    .expect("capture delay row") as i32;
  let labels = crate::i18n::tray("en");
  assert_eq!(
    main
      .get_rows()
      .row_data(delay as usize)
      .unwrap()
      .detail
      .as_str(),
    labels.delay_choices[0],
    "the row shows the current delay"
  );
  let shown: Vec<String> = (0..choice.get_choices().row_count())
    .map(|index| choice.get_choices().row_data(index).unwrap().to_string())
    .collect();
  assert_eq!(shown, labels.delay_choices);
  assert_eq!(choice.get_selected(), 0, "off is checked");
  assert!(choice.get_menu_width() >= 160.0);
  assert_eq!(choice.get_menu_height(), layout::choice_height(4));

  // Resting on the row opens its own submenu, never Recent Captures.
  main.invoke_hover_row(delay);
  advance(SUBMENU_OPEN.as_millis() as u64 + 10);
  assert_eq!(submenu(), Some(delay as usize));
  assert!(choice.window().is_visible());
  assert!(!recent.window().is_visible());
  assert_eq!(choice.get_highlight(), -1);
  // Reaching it cancels a close started on the way, as for Recent Captures.
  main.invoke_hover_row(0);
  choice.invoke_hovered(true);
  advance(SUBMENU_CLOSE.as_millis() as u64 + 10);
  assert_eq!(submenu(), Some(delay as usize));

  // Moving to Recent Captures swaps the submenus; only one is ever open.
  let recent_row = recent_index();
  main.invoke_hover_row(recent_row);
  advance(SUBMENU_OPEN.as_millis() as u64 + 10);
  assert_eq!(submenu(), Some(recent_row as usize));
  assert!(recent.window().is_visible());
  assert!(!choice.window().is_visible());

  // The keyboard lands on the checked choice; Back returns to the menu.
  main.invoke_open_submenu(delay, true);
  assert_eq!(submenu(), Some(delay as usize));
  assert!(!recent.window().is_visible());
  assert_eq!(choice.get_highlight(), choice.get_selected());
  choice.invoke_back();
  assert_eq!(submenu(), None);
  assert!(!choice.window().is_visible());
  assert_eq!(main.get_submenu_row(), -1);
}

fn submenu_follows_hover_delays_and_keyboard(app: &AppHandle) {
  let (main, recent) = windows();
  let index = recent_index();
  assert!(is_open());
  // Resting on Recent Captures opens the submenu only after its delay.
  main.invoke_hover_row(index);
  advance(SUBMENU_OPEN.as_millis() as u64 - 10);
  assert_eq!(submenu(), None);
  advance(20);
  assert_eq!(submenu(), Some(index as usize));
  assert!(recent.window().is_visible());
  assert_eq!(main.get_submenu_row(), index);
  assert_eq!(main.get_highlight(), index);
  assert_eq!(
    recent.get_highlight(),
    -1,
    "pointer opening highlights nothing"
  );
  assert!(!recent.get_shown());

  // Crossing another row starts the close; reaching the submenu cancels it.
  main.invoke_hover_row(0);
  recent.invoke_hovered(true);
  advance(SUBMENU_CLOSE.as_millis() as u64 + 10);
  assert_eq!(submenu(), Some(index as usize));
  assert_eq!(main.get_highlight(), index);
  // Hovering the open row again, and leaving the menu, keep it open.
  main.invoke_hover_row(index);
  main.invoke_hover_row(-1);
  recent.invoke_hovered(false);
  advance(SUBMENU_CLOSE.as_millis() as u64 + 10);
  assert_eq!(submenu(), Some(index as usize));
  // Resting on another row closes it.
  main.invoke_hover_row(0);
  advance(SUBMENU_CLOSE.as_millis() as u64 + 10);
  assert_eq!(submenu(), None);
  assert_eq!(main.get_submenu_row(), -1);
  assert!(!recent.window().is_visible());
  // With no submenu, hovering a plain row or leaving the menu starts nothing.
  main.invoke_hover_row(0);
  main.invoke_hover_row(-1);
  advance(SUBMENU_OPEN.as_millis() as u64 + 10);
  assert_eq!(submenu(), None);

  // The keyboard opens it at once with the first row highlighted; again just focuses it.
  main.invoke_open_submenu(index, true);
  assert_eq!(submenu(), Some(index as usize));
  assert_eq!(recent.get_highlight(), 0);
  recent.set_part(1);
  main.invoke_open_submenu(index, false);
  assert_eq!(submenu(), Some(index as usize));
  assert_eq!(recent.get_highlight(), -1);
  assert_eq!(recent.get_part(), 0);
  // Back returns to the menu; closing twice is harmless.
  recent.invoke_back();
  assert_eq!(submenu(), None);
  close_submenu(&menu());
  assert_eq!(submenu(), None);

  // Rows without a submenu, negative indices and a closed menu open nothing.
  open_submenu(&menu(), -1, true);
  open_submenu(&menu(), 0, true);
  assert_eq!(submenu(), None);
  close(&menu());
  open_submenu(&menu(), index, true);
  assert_eq!(submenu(), None);
  reopen(app);
}

fn thumbnails_update_rows_in_place(captures: &[RecentCapture]) {
  let (_, recent) = windows();
  let pixels = |side| SharedPixelBuffer::<Rgba8Pixel>::new(side, side);
  thumbnail_ready(captures[1].path.clone(), Ok(pixels(4)));
  let rows = recent.get_rows();
  assert!(rows.row_data(1).unwrap().has_thumbnail);
  assert!(!rows.row_data(0).unwrap().has_thumbnail);
  // A capture no longer listed, and a failed thumbnail, change nothing.
  thumbnail_ready(PathBuf::from("gone.png"), Ok(pixels(4)));
  thumbnail_ready(captures[0].path.clone(), Err("no thumbnail".into()));
  assert!(!recent.get_rows().row_data(0).unwrap().has_thumbnail);
  assert_eq!(menu().borrow().thumbnails.len(), 1);
  // Thumbnails of captures that dropped off the list are released on the next fill.
  menu().borrow_mut().thumbnails.insert(
    (PathBuf::from("dropped.png"), 1),
    Image::from_rgba8_premultiplied(pixels(2)),
  );
  let app = menu().borrow().app.clone();
  refresh(&app);
  assert_eq!(menu().borrow().thumbnails.len(), 1);
  assert!(recent.get_rows().row_data(1).unwrap().has_thumbnail);
  // A changed list replaces the rows instead of updating them one by one.
  {
    let menu = menu();
    let mut state = menu.borrow_mut();
    state.captures.truncate(1);
    set_recent_rows(&state);
  }
  assert_eq!(recent.get_rows().row_count(), 1);
  refresh(&app);
  assert_eq!(recent.get_rows().row_count(), captures.len());
}

fn dismissal_paths_close_without_running_actions(app: &AppHandle) {
  let (main, recent) = windows();
  main.invoke_dismiss();
  assert!(!is_open());
  // Closing an already closed menu changes nothing.
  let closed_at = menu().borrow().closed_at;
  close(&menu());
  assert_eq!(menu().borrow().closed_at, closed_at);

  reopen(app);
  main.window().dispatch_event(WindowEvent::CloseRequested);
  assert!(!is_open());
  assert!(!main.window().is_visible());

  reopen(app);
  main.invoke_open_submenu(recent_index(), true);
  recent.window().dispatch_event(WindowEvent::CloseRequested);
  assert!(!is_open());
  assert_eq!(submenu(), None);
  assert!(!recent.window().is_visible());

  // Actions close first and reach the tray dispatcher, which ignores unknown ids.
  reopen(app);
  main.invoke_activate("not-a-tray-action".into());
  assert!(!is_open());
  reopen(app);
  main.invoke_open_submenu(recent_index(), true);
  recent.invoke_act(0, "inspect".into());
  assert!(!is_open());
}

fn focus_loss_and_missing_native_windows_close_the_menu(app: &AppHandle) {
  // The menu never owns the foreground headlessly, so a focus change closes it.
  reopen(app);
  focus_changed();
  advance(10);
  assert!(is_open(), "focus settles before the check");
  advance(30);
  assert!(!is_open());
  // A closed menu ignores focus changes.
  focus_changed();
  advance(40);
  assert!(!is_open());

  // The native window never appears: the bounded wait gives up and closes the menu.
  reopen(app);
  let index = recent_index();
  windows().0.invoke_open_submenu(index, true);
  for _ in 0..60 {
    advance(10);
  }
  assert!(!is_open());
  assert_eq!(submenu(), None);
  // A wait for a menu that closed meanwhile stops at once.
  when_native(&menu(), Part::Menu, 0);
  when_native(&menu(), Part::Submenu, 0);
  assert!(!is_open());
}

fn shutdown_hides_and_forgets_the_menu() {
  let (main, _) = windows();
  let app = menu().borrow().app.clone();
  reopen(&app);
  shutdown();
  assert!(current().is_none());
  assert!(!main.window().is_visible());
  // Late callbacks of the dropped menu find nothing to act on.
  main.invoke_hover_row(0);
  main.invoke_dismiss();
  advance(SUBMENU_CLOSE.as_millis() as u64);
}

fn prewarm_creates_hidden_windows_and_gives_up_on_native_handles(app: &AppHandle) {
  schedule_prewarm(app);
  advance(PREWARM_AFTER.as_millis() as u64 - 10);
  assert!(current().is_none());
  advance(20);
  let (main, recent) = windows();
  assert!(!is_open());
  assert!(main.window().is_visible() && recent.window().is_visible());
  // A second prewarm keeps the existing windows.
  prewarm(app);
  assert!(Rc::ptr_eq(&menu(), &current().unwrap()));
  for _ in 0..110 {
    advance(20);
  }
  // Handles never appeared headlessly; the windows stay as they were, ready to open.
  reopen(app);
  close(&menu());
  shutdown();
}

fn failure_keeps_the_native_menu(app: &AppHandle) {
  FAILED.with(|failed| failed.set(true));
  assert!(uses_native_menu());
  open(app, POINT { x: 0, y: 0 });
  prewarm(app);
  assert!(
    current().is_none(),
    "the native menu serves every later click"
  );
  FAILED.with(|failed| failed.set(false));
}
