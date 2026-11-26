//! Explicit offscreen export of the custom tray menu: production rows, the Recent Captures
//! submenu filled from given captures, and their Shell thumbnails, on the headless platform, with
//! the production placement on a 1366×768 screen. No tray icon, native window, foreground change or
//! tray action is involved.
use super::*;
use crate::app::headless::{advance, drain, drain_until};
use flinch_settings::slint::LogicalSize;

/// The scene's screen: a 1366×768 Windows 11 desktop with its 48 px bottom taskbar, the pointer on
/// the Flinch tray icon.
const WORK: RECT = RECT {
  left: 0,
  top: 0,
  right: 1366,
  bottom: 720,
};
const TRAY_ICON: POINT = POINT { x: 1199, y: 744 };

fn save(window: &slint::Window, path: &std::path::Path) {
  let buffer = window.take_snapshot().expect("menu snapshot");
  image::save_buffer(
    path,
    buffer.as_bytes(),
    buffer.width(),
    buffer.height(),
    image::ColorType::Rgba8,
  )
  .expect("write menu frame");
}

/// Copies each capture under the production file name, newest first, and lists it as recent.
fn recent_captures(input: &std::path::Path, folder: &std::path::Path) -> Vec<RecentCapture> {
  let mut sources: Vec<PathBuf> = std::fs::read_dir(input)
    .expect("capture folder")
    .map(|entry| entry.expect("capture entry").path())
    .filter(|path| path.extension().is_some_and(|ext| ext == "png"))
    .collect();
  sources.sort();
  assert!(
    (1..=8).contains(&sources.len()),
    "one to eight captures fit the submenu"
  );
  // `FLINCH_EXPORT_CLOCK` (HH:MM) is the backdrop's taskbar clock: the newest capture is a minute
  // older, so the menu never lists captures from after the time on screen.
  let now = match std::env::var("FLINCH_EXPORT_CLOCK") {
    Ok(clock) => {
      let time =
        chrono::NaiveTime::parse_from_str(&clock, "%H:%M").expect("FLINCH_EXPORT_CLOCK is HH:MM");
      chrono::Local::now()
        .date_naive()
        .and_time(time)
        .and_local_timezone(chrono::Local)
        .single()
        .expect("an unambiguous local clock time")
        - chrono::Duration::minutes(1)
    },
    Err(_) => chrono::Local::now(),
  };
  let captures: Vec<RecentCapture> = sources
    .iter()
    .enumerate()
    .map(|(index, source)| {
      let saved = now - chrono::Duration::seconds(40 * index as i64);
      let name = crate::filename::render_template(
        crate::filename::DEFAULT_TEMPLATE,
        &crate::filename::FilenameContext {
          capture_type: "screenshot",
          app_name: None,
          now: saved,
        },
      );
      let path = folder.join(format!("{name}.png"));
      std::fs::copy(source, &path).expect("copy capture");
      RecentCapture {
        path,
        kind: CaptureKind::Screenshot,
        saved_at: saved.timestamp(),
      }
    })
    .collect();
  std::fs::write(
    folder.join("recent-captures.json"),
    serde_json::to_vec(&captures).expect("recent list"),
  )
  .expect("write recent list");
  captures
}

#[test]
#[ignore = "explicit tray menu frame export"]
fn render_tray_menu() {
  crate::app::headless::run(|| {
    let _recent = crate::app::tray::MENU_RECENT_TEST_LOCK
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let input =
      PathBuf::from(std::env::var("FLINCH_EXPORT_INPUT").expect("folder of capture PNGs"));
    let output = PathBuf::from(std::env::var("FLINCH_EXPORT_OUTPUT").expect("output folder"));
    std::fs::create_dir_all(&output).expect("output folder");
    let folder = tempfile::tempdir().expect("isolated settings folder");
    let captures = recent_captures(&input, folder.path());
    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        ..Default::default()
      },
      PathBuf::new(),
    );
    app.isolate_config_path(folder.path().join("settings.json"));

    let menu = create(&app).expect("tray menu");
    fill(&menu, &app, 1.0);
    // The Shell thumbnails arrive from their worker, as when the real menu opens.
    drain_until(|| menu.borrow().thumbnails.len() == captures.len());
    let (main, recent, index, menu_size, row_top) = {
      let state = menu.borrow();
      assert_eq!(state.captures, captures);
      let index = state
        .rows
        .iter()
        .position(|row| row.submenu)
        .expect("recent captures row");
      (
        state.main.clone_strong(),
        state.recent.clone_strong(),
        index,
        (state.main.get_menu_width(), state.main.get_menu_height()),
        layout::row_top(&state.rows, index),
      )
    };
    let recent_size = (recent.get_menu_width(), recent.get_menu_height());
    assert!((0..recent.get_rows().row_count()).all(|row| {
      recent
        .get_rows()
        .row_data(row)
        .is_some_and(|row| row.has_thumbnail)
    }));

    // What `show_main` and `open_submenu` set once their native windows exist.
    main
      .window()
      .set_size(LogicalSize::new(menu_size.0, menu_size.1));
    main.show().expect("offscreen menu");
    main.set_shown(true);
    advance(200);
    save(main.window(), &output.join("menu.png"));
    main.set_highlight(index as i32);
    main.set_submenu_row(index as i32);
    recent
      .window()
      .set_size(LogicalSize::new(recent_size.0, recent_size.1));
    recent.show().expect("offscreen submenu");
    recent.set_shown(true);
    advance(200);
    save(main.window(), &output.join("menu-open.png"));
    save(recent.window(), &output.join("recent.png"));
    recent.set_highlight(0);
    advance(200);
    save(recent.window(), &output.join("recent-hover.png"));

    let size = (
      layout::physical(menu_size.0, 1.0),
      layout::physical(menu_size.1, 1.0),
    );
    let origin = layout::place_menu(TRAY_ICON, size, WORK);
    let frame = RECT {
      left: origin.x,
      top: origin.y,
      right: origin.x + size.0,
      bottom: origin.y + size.1,
    };
    let sub_size = (
      layout::physical(recent_size.0, 1.0),
      layout::physical(recent_size.1, 1.0),
    );
    let sub = layout::place_submenu(frame, layout::physical(row_top, 1.0), 1.0, sub_size, WORK);
    std::fs::write(
      output.join("layout.json"),
      serde_json::to_vec_pretty(&serde_json::json!({
        "pointer": [TRAY_ICON.x, TRAY_ICON.y],
        "menu": [origin.x, origin.y, size.0, size.1],
        "recent": [sub.x, sub.y, sub_size.0, sub_size.1],
      }))
      .expect("layout"),
    )
    .expect("write layout");

    shutdown();
    drain();
    crate::app::tray::remember_menu_recent(Vec::new());
  });
}
