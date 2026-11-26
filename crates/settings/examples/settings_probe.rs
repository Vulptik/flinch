use std::{
  cell::{Cell, RefCell},
  fs::File,
  io::Read,
  path::PathBuf,
  rc::Rc,
  time::Duration,
};

use flinch_settings::{
  catalog::Catalog,
  model::SettingsSnapshot,
  ui::{self, Presentation, SettingsWindow},
  windows,
};
use slint::ComponentHandle;

fn probe_polish(
  window: &SettingsWindow,
  directory: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
  use slint::platform::WindowEvent;
  std::fs::create_dir_all(&directory)?;
  window.global::<ui::Interaction>().set_editable(true);
  window.global::<ui::Interaction>().set_motion_enabled(true);
  // A long fixture preview exercises overflow at the production window dimensions.
  window
    .global::<ui::SettingsState>()
    .set_filename_preview("Long capture filename preview\n".repeat(12).into());
  window.global::<ui::SettingsState>().set_folder_index(1);
  window.set_page(0);
  let weak = window.as_weak();
  let stage = Rc::new(Cell::new(0usize));
  let failure = Rc::new(RefCell::new(None::<String>));
  let failures = failure.clone();
  let timer = slint::Timer::default();
  timer.start(
    slint::TimerMode::Repeated,
    Duration::from_millis(300),
    move || {
      let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let window = weak.upgrade().ok_or("Polish window closed")?;
        let current = stage.get();
        stage.set(current + 1);
        match current {
          0 => window.set_page(5),
          1 => window
            .global::<ui::SettingsState>()
            .set_filename_customization_expanded(true),
          2 => window
            .global::<ui::SettingsState>()
            .set_filename_customization_expanded(false),
          3 => {
            let catalog = Catalog::load("en")?;
            let entry = catalog
              .search("filename")?
              .into_iter()
              .find(|entry| entry.id == "files-filename-template")
              .ok_or("Filename search entry missing")?;
            window.invoke_search_select(ui::SearchResult {
              label: entry.label.into(),
              section: "Files".into(),
              page: 5,
              target: entry.id.into(),
            });
          },
          4 => {
            if !window
              .global::<ui::SettingsState>()
              .get_filename_customization_expanded()
              || window.global::<ui::Interaction>().get_target_generation() == 0
            {
              return Err("Search did not expand and reveal its field".into());
            }
            let before = window.global::<ui::SettingsState>().get_filename();
            window
              .window()
              .dispatch_event(WindowEvent::KeyPressed { text: "x".into() });
            window
              .window()
              .dispatch_event(WindowEvent::KeyReleased { text: "x".into() });
            if window.global::<ui::SettingsState>().get_filename() == before {
              return Err("Search did not focus the filename input".into());
            }
          },
          5 => {
            let catalog = Catalog::load("en")?;
            let entry = catalog
              .search("subfolder")?
              .into_iter()
              .find(|entry| entry.id == "files-single-folder-subfolder-template")
              .ok_or("Subfolder search entry missing")?;
            window.invoke_search_select(ui::SearchResult {
              label: entry.label.into(),
              section: "Files".into(),
              page: 5,
              target: entry.id.into(),
            });
          },
          6 => {
            if window.get_content_scroll_y() >= 0.0 {
              return Err("Search did not scroll the lower field into view".into());
            }
            let before = window.global::<ui::SettingsState>().get_subfolder();
            window
              .window()
              .dispatch_event(WindowEvent::KeyPressed { text: "x".into() });
            window
              .window()
              .dispatch_event(WindowEvent::KeyReleased { text: "x".into() });
            if window.global::<ui::SettingsState>().get_subfolder() == before {
              return Err("Search did not focus the subfolder input".into());
            }
            let before_scroll = window.get_content_scroll_y();
            let position =
              slint::LogicalPosition::new(window.get_scroll_thumb_x(), window.get_scroll_thumb_y());
            window
              .window()
              .dispatch_event(WindowEvent::PointerMoved { position });
            window.window().dispatch_event(WindowEvent::PointerPressed {
              position,
              button: slint::platform::PointerEventButton::Left,
            });
            let position = slint::LogicalPosition::new(position.x, position.y - 40.0);
            window
              .window()
              .dispatch_event(WindowEvent::PointerMoved { position });
            window
              .window()
              .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
              });
            if window.get_content_scroll_y() <= before_scroll {
              return Err("Scrollbar drag did not move the page".into());
            }
          },
          7 => {
            window.global::<ui::Interaction>().set_motion_enabled(false);
            window.set_page(4);
          },
          8 => {
            window.hide()?;
            slint::quit_event_loop()?;
            return Ok(());
          },
          _ => return Err("Polish probe exceeded finite stage count".into()),
        }
        let buffer = window.window().take_snapshot()?;
        let mut encoder = png::Encoder::new(
          File::create(directory.join(format!("polish-{current}.png")))?,
          buffer.width(),
          buffer.height(),
        );
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
          .write_header()?
          .write_image_data(buffer.as_bytes())?;
        Ok(())
      })();
      if let Err(error) = result {
        *failures.borrow_mut() = Some(error.to_string());
        if let Err(quit_error) = slint::quit_event_loop() {
          eprintln!("Polish probe cleanup failed: {quit_error}");
        }
      }
    },
  );
  slint::run_event_loop()?;
  timer.stop();
  if let Some(error) = failure.borrow_mut().take() {
    return Err(error.into());
  }
  println!("Settings polish: expansion, search reveal/focus and reduced-motion navigation passed.");
  Ok(())
}

fn probe_confirmation(
  window: &SettingsWindow,
  directory: PathBuf,
  reset: bool,
) -> Result<(), Box<dyn std::error::Error>> {
  use slint::platform::{Key, WindowEvent};
  std::fs::create_dir_all(&directory)?;
  window.global::<ui::Interaction>().set_editable(true);
  let actions = Rc::new(RefCell::new(Vec::<String>::new()));
  let received = actions.clone();
  let weak = window.as_weak();
  window
    .global::<ui::SettingsState>()
    .on_action(move |action| {
      received.borrow_mut().push(action.to_string());
      if let Some(window) = weak.upgrade() {
        window
          .global::<ui::SettingsState>()
          .set_confirm_invalid_close(false);
      }
    });
  let weak = window.as_weak();
  let failure = Rc::new(RefCell::new(None));
  let reported = failure.clone();
  let completed = Rc::new(Cell::new(false));
  let finished = completed.clone();
  let timer = slint::Timer::default();
  let mut tick = 0;
  timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
      tick += 1;
      let window = weak.upgrade().ok_or("Discard probe window closed")?;
      let key = |text: slint::SharedString| {
        window.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window.window().dispatch_event(WindowEvent::KeyReleased { text });
      };
      match tick {
        1 | 16 | 31 | 46 => {
          // A pending search reveal must never take focus from the modal confirmation.
          window.global::<ui::Interaction>().set_highlight("Export".into());
          let generation = window.global::<ui::Interaction>().get_search_generation();
          window.global::<ui::Interaction>().set_search_generation(generation + 1);
          if reset { window.global::<ui::SettingsState>().invoke_reset_request(); } else { window.global::<ui::SettingsState>().set_confirm_invalid_close(true); }
        },
        2 => {
          key(Key::Tab.into());
        },
        6 => {
          key("\n".into());
        },
        12 => {
          if *actions.borrow() != [if reset { "reset" } else { "discard" }] { return Err(format!("Tab selection was stolen during entry: {:?}", actions.borrow()).into()); }
        },
        21 => {
          let buffer = window.window().take_snapshot()?;
          let file = File::create(directory.join(if reset { "reset.png" } else { "discard.png" }))?;
          let mut encoder = png::Encoder::new(file, buffer.width(), buffer.height());
          encoder.set_color(png::ColorType::Rgba);
          encoder.set_depth(png::BitDepth::Eight);
          encoder.write_header()?.write_image_data(buffer.as_bytes())?;
          // Repeated input during exit must dispatch exactly one action.
          for _ in 0..3 { key(Key::Escape.into()); }
        },
        27 => {
          if !reset && *actions.borrow() != ["discard", "cancel-close"] || reset && actions.borrow().len() != 1 { return Err(format!("Escape exit failed: {:?}", actions.borrow()).into()); }
        },
        36 => { flinch_settings::interaction_check::click(window.window(), 514.0, 259.0); },
        42 => {
          if !reset && *actions.borrow() != ["discard", "cancel-close", "cancel-close"] || reset && actions.borrow().len() != 1 { return Err(format!("Close cross failed: {:?}", actions.borrow()).into()); }
        },
        51 => {
          if !reset {
            window.global::<ui::SettingsState>().invoke_reset_request();
            if window.get_confirming_reset() { return Err("Reset opened over discard confirmation".into()); }
          }
          // A background click must not navigate or leave an invisible input blocker.
          let page = window.get_page();
          flinch_settings::interaction_check::click(window.window(), 80.0, 125.0);
          if window.get_page() != page { return Err("Discard backdrop allowed navigation".into()); }
          if reset { window.invoke_close_request(); } else { key(Key::Escape.into()); }
        },
        57 => {
          if actions.borrow().len() != if reset { 1 } else { 4 } || window.global::<ui::SettingsState>().get_confirm_invalid_close() || window.get_confirming_reset() { return Err("Discard dialog stuck after repeated reopen".into()); }
          flinch_settings::interaction_check::click(window.window(), 80.0, 125.0);
          if window.get_page() != 1 { return Err("Dismissed discard dialog still blocks input".into()); }
          finished.set(true);
          println!("Embedded confirmation (reset={reset}): entry focus, keyboard confirmation, repeated Escape, close cross, backdrop blocking and repeated reopen passed.");
          slint::quit_event_loop()?;
        },
        _ => {},
      }
      Ok(())
    })();
    if let Err(error) = result {
      *reported.borrow_mut() = Some(error.to_string());
      if let Err(error) = slint::quit_event_loop() { eprintln!("Cannot stop discard probe: {error}"); }
    }
  });
  slint::run_event_loop()?;
  if let Some(error) = failure.borrow_mut().take() {
    return Err(error.into());
  }
  if !completed.get() {
    return Err("Discard probe ended before completing its checks".into());
  }
  Ok(())
}

fn main() {
  if let Err(error) = run() {
    eprintln!("Native settings failed: {error}");
    std::process::exit(1);
  }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
  let mut config_path = None;
  let mut check = false;
  let mut previews = None;
  let mut dialog_probe = None;
  let mut discard_probe = None;
  let mut polish_probe = None;
  let mut args = std::env::args_os().skip(1);
  while let Some(arg) = args.next() {
    match arg.to_str() {
      Some("--probe-polish") => {
        polish_probe = Some(PathBuf::from(
          args
            .next()
            .ok_or("--probe-polish requires an output directory")?,
        ))
      },
      Some("--config") => {
        config_path = Some(PathBuf::from(
          args.next().ok_or("--config requires a file path")?,
        ))
      },
      Some("--check") => check = true,
      Some("--render-previews") => {
        previews = Some(PathBuf::from(
          args
            .next()
            .ok_or("--render-previews requires an output directory")?,
        ))
      },
      Some("--probe-reset-dialog") => {
        dialog_probe = Some(PathBuf::from(
          args
            .next()
            .ok_or("--probe-reset-dialog requires an output directory")?,
        ));
      },
      Some("--probe-discard-dialog") => {
        discard_probe =
          Some(PathBuf::from(args.next().ok_or(
            "--probe-discard-dialog requires an output directory",
          )?));
      },
      _ => return Err("Use --config <file>, --check or --render-previews <directory>".into()),
    }
  }
  if usize::from(check)
    + usize::from(previews.is_some())
    + usize::from(dialog_probe.is_some())
    + usize::from(discard_probe.is_some())
    + usize::from(polish_probe.is_some())
    != 1
  {
    return Err(
      "Choose exactly one diagnostic mode: --check, --render-previews, --probe-reset-dialog or --probe-discard-dialog"
        .into(),
    );
  }
  // This bounded test fixture is read-only and deliberately uses an
  // explicit file. It never locates, repairs or writes the live app's config.
  let source = if let Some(path) = config_path {
    const MAX_SIZE: u64 = 10 * 1024 * 1024;
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_SIZE {
      return Err("Configuration exceeds 10 MiB".into());
    }
    let mut source = String::new();
    file.take(MAX_SIZE + 1).read_to_string(&mut source)?;
    if source.len() as u64 > MAX_SIZE {
      return Err("Configuration exceeds 10 MiB".into());
    }
    source
  } else {
    include_str!("../../../resources/defaultConfig.json").to_owned()
  };
  let snapshot: SettingsSnapshot = serde_json::from_str(&source)?;
  snapshot.validate_projection()?;
  let locale = if snapshot.language.is_empty() {
    "en"
  } else {
    &snapshot.language
  };
  let catalog = Catalog::load(locale)?;
  let failure = Rc::new(RefCell::new(None));
  slint::BackendSelector::new()
    .backend_name("winit".into())
    .renderer_name("skia-software".into())
    .with_winit_custom_application_handler(windows::ChromeHandler::new(failure.clone()))
    .select()?;
  let window = SettingsWindow::new()?;
  windows::configure_fonts(&window)?;
  ui::connect_search(&window)?;
  ui::connect_reset_confirmation(&window);
  let close_failure = failure.clone();
  ui::connect_close_request(&window, move |window| {
    // The read-only runner owns no writes or shortcut suppression leases.
    if let Err(error) = window.hide() {
      eprintln!("Failed to close native settings: {error}");
      *close_failure.borrow_mut() = Some(error.to_string());
      if let Err(error) = slint::quit_event_loop() {
        eprintln!("Failed to stop native settings after close failure: {error}");
      }
    }
  });
  let checked_edits = Rc::new(RefCell::new(Vec::new()));
  if check {
    let checked_edits = checked_edits.clone();
    ui::connect_edits(&window, move |edit| checked_edits.borrow_mut().push(edit));
  }
  ui::apply_snapshot(
    &window,
    &snapshot,
    &Presentation {
      locale: locale.to_owned(),
      subfolder_preview: "fixture".into(),
      filename_preview: catalog.text("about_unavailable")?.to_owned(),
      screenshots_path: catalog.text("about_unavailable")?.to_owned(),
      videos_path: catalog.text("about_unavailable")?.to_owned(),
      channel: catalog.text("about_unavailable")?.to_owned(),
    },
  )?;
  if check {
    if !checked_edits.borrow().is_empty() {
      return Err("Loading native settings incorrectly emitted a user edit".into());
    }
    window
      .global::<ui::SettingsState>()
      .invoke_edit_request("video-fps-index".into());
    window
      .global::<ui::SettingsState>()
      .invoke_edit_request("unknown-field".into());
    let edits = checked_edits.borrow();
    if edits.first()
      != Some(&Ok(flinch_settings::edit::PreferenceEdit::VideoFps(
        snapshot.video_fps,
      )))
      || !matches!(edits.get(1), Some(Err(_)))
    {
      return Err("Native settings edit dispatch failed its boundary check".into());
    }
    drop(edits);
    let interactions = flinch_settings::interaction_check::run(&window)?;
    println!(
      "Native settings: {interactions} production UI interactions passed across five locales."
    );
    return Ok(());
  }
  window.show()?;
  if let Some(directory) = polish_probe {
    return probe_polish(&window, directory);
  }
  if previews.is_some() {
    window.global::<ui::Interaction>().set_motion_enabled(false);
  }
  if let Some(directory) = discard_probe {
    return probe_confirmation(&window, directory, false);
  }
  if let Some(directory) = dialog_probe {
    return probe_confirmation(&window, directory, true);
  }
  let preview_timer = slint::Timer::default();
  if let Some(directory) = previews {
    std::fs::create_dir_all(&directory)?;
    let counter = Rc::new(Cell::new(0usize));
    let finished = Rc::new(Cell::new(false));
    let weak = window.as_weak();
    let preview_failure = failure.clone();
    let preview_finished = finished.clone();
    preview_timer.start(
      slint::TimerMode::Repeated,
      Duration::from_millis(80),
      move || {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
          let window = weak.upgrade().ok_or("Settings preview window closed")?;
          let current = counter.get();
          if current >= 40 {
            return Err("Settings preview counter exceeded its bound".into());
          }
          let locale = flinch_settings::model::LOCALES[current / 8];
          let section = flinch_settings::model::Section::ALL[current % 8];
          if section == flinch_settings::model::Section::About {
            window
              .global::<ui::SettingsState>()
              .set_graphics_adapters(slint::ModelRc::new(slint::VecModel::from(vec![
                ui::GraphicsAdapter {
                  name: "Example Graphics Adapter".into(),
                  driver: "1.0.0.0".into(),
                },
              ])));
          }
          let buffer = window.window().take_snapshot()?;
          let path = directory.join(format!("{locale}-{}.png", section.id()));
          let file = File::create(path)?;
          let mut encoder = png::Encoder::new(file, buffer.width(), buffer.height());
          encoder.set_color(png::ColorType::Rgba);
          encoder.set_depth(png::BitDepth::Eight);
          encoder
            .write_header()?
            .write_image_data(buffer.as_bytes())?;
          if section == flinch_settings::model::Section::Files {
            let state = window.global::<ui::SettingsState>();
            let original_folder = state.get_folder_index();
            for folder in [0, 1] {
              for expanded in [false, true] {
                state.set_folder_index(folder);
                state.set_filename_customization_expanded(expanded);
                state.set_subfolder_customization_expanded(expanded);
                let buffer = window.window().take_snapshot()?;
                let file = File::create(directory.join(format!(
                  "{locale}-files-folder-{folder}-expanded-{expanded}.png"
                )))?;
                let mut encoder = png::Encoder::new(file, buffer.width(), buffer.height());
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder
                  .write_header()?
                  .write_image_data(buffer.as_bytes())?;
              }
            }
            state.set_folder_index(original_folder);
            state.set_filename_customization_expanded(false);
            state.set_subfolder_customization_expanded(false);
          }
          if section == flinch_settings::model::Section::Hotkeys {
            window
              .global::<ui::SettingsState>()
              .set_capture_target("screenshot-area".into());
            let buffer = window.window().take_snapshot()?;
            window
              .global::<ui::SettingsState>()
              .set_capture_target("".into());
            let file = File::create(directory.join(format!("{locale}-hotkeys-capture.png")))?;
            let mut encoder = png::Encoder::new(file, buffer.width(), buffer.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
              .write_header()?
              .write_image_data(buffer.as_bytes())?;
          }
          counter.set(current + 1);
          if current + 1 == 40 {
            preview_finished.set(true);
            slint::quit_event_loop()?;
          } else {
            let next_locale = flinch_settings::model::LOCALES[(current + 1) / 8];
            window.set_language_index(((current + 1) / 8) as i32);
            window.set_page(((current + 1) % 8) as i32);
            apply_preview_copy(&window, next_locale)?;
          }
          Ok(())
        })();
        if let Err(error) = result {
          *preview_failure.borrow_mut() = Some(error.to_string());
          preview_finished.set(true);
          if let Err(quit_error) = slint::quit_event_loop() {
            eprintln!("Cannot stop preview loop: {quit_error}");
          }
        }
      },
    );
    let deadline_failure = failure.clone();
    slint::Timer::single_shot(Duration::from_secs(10), move || {
      if !finished.get() {
        *deadline_failure.borrow_mut() =
          Some("Native settings preview exceeded its ten-second deadline".to_owned());
        if let Err(error) = slint::quit_event_loop() {
          eprintln!("Cannot stop timed-out preview loop: {error}");
        }
      }
    });
    window.set_language_index(0);
    window.set_page(0);
    apply_preview_copy(&window, "en")?;
  }
  slint::run_event_loop()?;
  preview_timer.stop();
  if let Some(error) = failure.borrow_mut().take() {
    return Err(error.into());
  }
  Ok(())
}

fn apply_preview_copy(window: &SettingsWindow, locale: &str) -> Result<(), String> {
  use ui::SettingsState;
  let catalog = Catalog::load(locale)?;
  let unavailable = slint::SharedString::from(catalog.text("about_unavailable")?);
  let state = window.global::<SettingsState>();
  state.set_filename_preview(unavailable.clone());
  state.set_screenshots_path(unavailable.clone());
  state.set_videos_path(unavailable.clone());
  state.set_channel(unavailable);
  Ok(())
}
