//! Debug-only native integration harness with isolated preferences.
use crate::config::Config;
use flinch_settings::{
  model::Section,
  slint::{self, ComponentHandle},
  ui::{Interaction, SettingsState},
};
use std::{
  cell::RefCell,
  path::PathBuf,
  rc::Rc,
  sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  },
  time::{Duration, Instant},
};

static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static LOAD_FAIL: AtomicBool = AtomicBool::new(false);
static LOAD_FAILURE_INJECTED: AtomicBool = AtomicBool::new(false);
static QUIT_FAIL: AtomicBool = AtomicBool::new(false);
static QUIT_STARTED: OnceLock<Instant> = OnceLock::new();
static FAIL_WRITE: AtomicBool = AtomicBool::new(false);
static MANIFEST_READS: AtomicUsize = AtomicUsize::new(0);
static CONFIG_PICKER: Mutex<Option<(bool, Option<PathBuf>)>> = Mutex::new(None);

pub(crate) fn pick_configuration(export: bool) -> Result<Option<PathBuf>, String> {
  let (expected, path) = CONFIG_PICKER
    .lock()
    .map_err(|error| error.to_string())?
    .take()
    .ok_or("Configuration picker was not scripted for this probe")?;
  if expected != export {
    return Err("Configuration picker used the wrong mode".into());
  }
  Ok(path)
}

fn script_picker(export: bool, path: Option<PathBuf>) -> anyhow::Result<()> {
  let mut picker = CONFIG_PICKER
    .lock()
    .map_err(|error| anyhow::anyhow!("Lock probe picker: {error}"))?;
  anyhow::ensure!(picker.is_none(), "Previous probe picker was not consumed");
  *picker = Some((export, path));
  Ok(())
}
pub(super) fn update_manifest() -> Vec<u8> {
  MANIFEST_READS.fetch_add(1, Ordering::Relaxed);
  br#"{"version":"9.0.0"}"#.to_vec()
}

pub(super) fn initialize() -> anyhow::Result<()> {
  let mut args = std::env::args();
  while let Some(arg) = args.next() {
    if arg == "--native-settings-smoke-load-failure" {
      LOAD_FAIL.store(true, Ordering::Relaxed);
      continue;
    }
    if arg == "--native-settings-smoke-quit-failure" {
      QUIT_FAIL.store(true, Ordering::Relaxed);
      continue;
    }
    if arg != "--native-settings-smoke" {
      continue;
    }
    let directory = PathBuf::from(
      args
        .next()
        .ok_or_else(|| anyhow::anyhow!("Smoke test requires an output directory"))?,
    );
    anyhow::ensure!(
      directory.is_absolute() && !directory.exists(),
      "Smoke output must be a new absolute directory"
    );
    std::fs::create_dir_all(&directory)?;
    DIRECTORY
      .set(directory)
      .map_err(|_| anyhow::anyhow!("Smoke harness initialized twice"))?;
  }
  Ok(())
}
pub(crate) fn is_active() -> bool {
  DIRECTORY.get().is_some()
}
pub(crate) fn config_path() -> Option<PathBuf> {
  DIRECTORY.get().map(|dir| dir.join("config.json"))
}
pub(crate) fn take_load_failure() -> bool {
  LOAD_FAIL.load(Ordering::Relaxed) && !LOAD_FAILURE_INJECTED.swap(true, Ordering::Relaxed)
}
pub(crate) fn take_write_failure() -> bool {
  FAIL_WRITE.swap(false, Ordering::AcqRel)
}
pub(crate) fn configuration() -> Option<Config> {
  is_active().then(|| Config {
    language: "en".into(),
    auto_start: false,
    record_audio: false,
    video_fps: 30,
    ..Config::default()
  })
}
pub(super) struct Probe {
  timer: slint::Timer,
  result: Rc<RefCell<Option<Result<(), String>>>>,
}
impl Probe {
  pub(super) fn finish(self) -> anyhow::Result<()> {
    self.timer.stop();
    if let Some(result) = self.result.borrow_mut().take() {
      return result.map_err(anyhow::Error::msg);
    }
    let started = QUIT_STARTED
      .get()
      .ok_or_else(|| anyhow::anyhow!("Smoke test ended before completion"))?;
    anyhow::ensure!(
      started.elapsed() < Duration::from_secs(8),
      "Explicit quit waited for the shutdown deadline instead of discarding invalid edits"
    );
    let path = config_path().ok_or_else(|| anyhow::anyhow!("Missing probe config path"))?;
    let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(
      config.record_audio != QUIT_FAIL.load(Ordering::Relaxed)
        && config.effective_filename_template() == crate::filename::DEFAULT_TEMPLATE,
      "Explicit quit lost valid changes or persisted invalid text"
    );
    let directory = DIRECTORY
      .get()
      .ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
    let path = directory.join("results.json");
    let mut report: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    report["explicit_quit_discards_invalid_without_prompt"] = true.into();
    report["explicit_quit_discards_failed_write"] = QUIT_FAIL.load(Ordering::Relaxed).into();
    report["startup_failure_keeps_other_settings_editable"] =
      LOAD_FAIL.load(Ordering::Relaxed).into();
    std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    Ok(())
  }
}
pub(super) fn start(app: crate::app::context::AppHandle) -> Option<Probe> {
  if !is_active() {
    return None;
  }
  let result = Rc::new(RefCell::new(None));
  let output = result.clone();
  let timer = slint::Timer::default();
  let started = Instant::now();
  let mut stage = 0;
  let mut cycles = 0;
  let mut generations = Vec::new();
  let mut action_baseline = None;
  timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
    if output.borrow().is_some() { return; }
    let step = (|| -> anyhow::Result<bool> {
      anyhow::ensure!(started.elapsed() < Duration::from_secs(30), "Native smoke test exceeded thirty seconds at stage {stage}");
      match stage {
        0 => { super::native_host::show(&app, Some(Section::General)).map_err(anyhow::Error::msg)?; stage = 1; },
        1 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
            if LOAD_FAIL.load(Ordering::Relaxed) {
              anyhow::ensure!(window.global::<SettingsState>().get_startup_unavailable(), "Startup failure did not isolate the unavailable control");
              for page in 0..8 {
                window.set_page(page);
                anyhow::ensure!(window.global::<Interaction>().get_editable(), "Startup failure disabled page {page}");
              }
              window.set_page(0);
            }
            if !cfg!(feature = "msix") && window.global::<SettingsState>().get_update_version().is_empty() { return Ok(false); }
            if cfg!(feature = "msix") { anyhow::ensure!(window.global::<SettingsState>().get_update_version().is_empty(), "Store build displayed a website update"); }
            let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
            let buffer = window.window().take_snapshot()?;
            let mut encoder = png::Encoder::new(std::fs::File::create(directory.join("general.png"))?, buffer.width(), buffer.height());
            encoder.set_color(png::ColorType::Rgba); encoder.set_depth(png::BitDepth::Eight); encoder.write_header()?.write_image_data(buffer.as_bytes())?;
            window.global::<SettingsState>().invoke_action("dismiss-update".into());
            anyhow::ensure!(window.global::<SettingsState>().get_update_version().is_empty(), "Update dismissal did not clear the notice");
            window.set_page(4);
            FAIL_WRITE.store(true, Ordering::Release);
            let state = window.global::<SettingsState>();
            state.set_updates(false); state.invoke_edit_request("updates".into());
            state.set_audio(true); state.invoke_edit_request("audio".into());
            state.set_video_fps_index(1); state.invoke_edit_request("video-fps-index".into());
            stage = 2;
        },
        2 => if let Some((window, _, _)) = super::native_host::probe_view() && window.global::<SettingsState>().get_save_failed() {
            use slint::Model;
            let errors = window.global::<SettingsState>().get_field_errors();
            anyhow::ensure!(errors.row_data(13).is_some_and(|text| !text.is_empty()) && errors.row_data(10).is_some_and(|text| !text.is_empty()), "Failed save is missing errors beneath its controls");
            render(&window, "save-error.png")?;
            let config = Config::current(&app);
            anyhow::ensure!(!config.record_audio && config.video_fps == 30, "Failed save published preferences");
            window.global::<SettingsState>().invoke_edit_request("audio".into()); stage = 3;
        },
        3 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && !window.global::<SettingsState>().get_save_failed() {
            let config = Config::current(&app);
            anyhow::ensure!(config.record_audio && config.video_fps == 60 && !config.check_for_updates, "Retry lost a draft field");
            let path = config_path().ok_or_else(|| anyhow::anyhow!("Missing probe config path"))?;
          let saved: Config = serde_json::from_slice(&std::fs::read(path)?)?;
            anyhow::ensure!(saved.record_audio && saved.video_fps == 60 && !saved.check_for_updates, "Persisted retry differs from runtime");
            let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
            let buffer = window.window().take_snapshot()?;
            let file = std::fs::File::create(directory.join("videos.png"))?;
            let mut encoder = png::Encoder::new(file, buffer.width(), buffer.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header()?.write_image_data(buffer.as_bytes())?;
            window.set_page(2); stage = 9;
        },
        4 => if super::native_host::probe_view().is_none() {
          if cycles == 20 {
            anyhow::ensure!(MANIFEST_READS.load(Ordering::Relaxed) == usize::from(!cfg!(feature = "msix")), "Disabled update preference requested another manifest on reopen");
            let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
            std::fs::write(directory.join("results.json"), serde_json::to_vec_pretty(&serde_json::json!({"invalid_close_preserves_valid_edits": true, "inline_template_error_and_focus": true, "failed_save_preserved_config": true, "retry_persisted_both_fields": true, "shortcut_retry_persisted": true, "capture_close_restored": true, "action_close_and_cancel": true, "template_resets_persisted": true, "save_location_persisted_and_restored": true, "update_preference_persisted": true, "mock_manifest_reads": MANIFEST_READS.load(Ordering::Relaxed), "preview_waited_for_save": true, "cooldown_survived_navigation_and_reopen": true, "reopen_cycles": cycles, "generations": generations}))?)?;
            super::native_host::show(&app, Some(Section::Files)).map_err(anyhow::Error::msg)?;
            stage = 40;
            return Ok(false);
          }
          super::native_host::show(&app, Some(Section::ALL[cycles % Section::ALL.len()])).map_err(anyhow::Error::msg)?; stage = 5;
        },
        5 => if let Some((window, generation, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
            if cycles == 0 { anyhow::ensure!(window.global::<SettingsState>().get_preview_cooldown() > 0, "Reopening settings cleared preview cooldown"); }
            anyhow::ensure!(!generations.contains(&generation), "Reopened view reused an old generation");
            generations.push(generation); cycles += 1;
            super::native_host::close_requested(generation); stage = 4;
        },
        9 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle {
          let state = window.global::<SettingsState>();
          state.set_cursor(!state.get_cursor());
          state.invoke_edit_request("cursor".into());
          state.invoke_capture_request("gif".into()); stage = 6;
        },
        6 => if let Some((window, _, _)) = super::native_host::probe_view() && window.global::<SettingsState>().get_capture_target() == "gif" {
          FAIL_WRITE.store(true, Ordering::Release);
          super::native_host::capture_key("Ctrl+Shift+F12".into()); stage = 7;
        },
        7 => if let Some((window, _, _)) = super::native_host::probe_view() && window.global::<SettingsState>().get_save_failed() {
          anyhow::ensure!(Config::current(&app).gif_shortcut != "Ctrl+Shift+F12", "Failed shortcut save published config");
          window.global::<SettingsState>().invoke_action("retry".into()); stage = 8;
        },
        8 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && !window.global::<SettingsState>().get_save_failed() {
          anyhow::ensure!(Config::current(&app).gif_shortcut == "Ctrl+Shift+F12", "Shortcut retry lost the draft");
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          let buffer = window.window().take_snapshot()?;
          let mut encoder = png::Encoder::new(std::fs::File::create(directory.join("hotkeys.png"))?, buffer.width(), buffer.height());
          encoder.set_color(png::ColorType::Rgba); encoder.set_depth(png::BitDepth::Eight);
          encoder.write_header()?.write_image_data(buffer.as_bytes())?;
          window.set_page(5);
          let state = window.global::<SettingsState>();
          state.set_filename("Custom_%TYPE%".into()); state.invoke_edit_request("filename".into());
          state.invoke_action("reset-filename".into());
          state.set_subfolder("Custom".into()); state.invoke_edit_request("subfolder".into());
          state.invoke_action("reset-subfolder".into());
          // Exercise the destination choice itself.
          // Directly invoking edit-request would miss a lost UI callback.
          window.window().take_snapshot()?;
          probe_click(window.window(), 500.0, 173.0);
          stage = 10;
        },
        10 => if let Some((window, generation, idle)) = super::native_host::probe_view() && idle {
          let config = Config::current(&app);
          anyhow::ensure!(config.filename_template.is_none() && config.single_folder_subfolder_template.is_none(), "Template resets did not persist default semantics");
          anyhow::ensure!(config.save_location_mode == crate::config::SaveLocationMode::SingleFolder, "Dropdown selection did not reach the configuration service");
          let persisted: Config = serde_json::from_slice(&std::fs::read(config_path().ok_or_else(|| anyhow::anyhow!("Missing probe config path"))?)?)?;
          anyhow::ensure!(persisted.save_location_mode == config.save_location_mode, "Save location was not persisted");
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          let buffer = window.window().take_snapshot()?;
          let mut encoder = png::Encoder::new(std::fs::File::create(directory.join("files.png"))?, buffer.width(), buffer.height());
          encoder.set_color(png::ColorType::Rgba); encoder.set_depth(png::BitDepth::Eight); encoder.write_header()?.write_image_data(buffer.as_bytes())?;
          let state = window.global::<SettingsState>();
          state.set_filename("file:name".into()); state.invoke_edit_request("filename".into());
          anyhow::ensure!(state.get_filename_error() == "“:” isn’t allowed here.", "Template error is not specific");
          super::native_host::close_requested(generation);
          anyhow::ensure!(state.get_confirm_invalid_close(), "Invalid close did not confirm");
          render(&window, "invalid-close.png")?;
          for (index, locale) in ["en", "fr", "de", "es", "it"].iter().enumerate() {
            window.set_language_index(index as i32);
            render(&window, &format!("invalid-close-{locale}.png"))?;
          }
          window.set_language_index(0);
          stage = 24;
        },
        24 => if let Some((window, _, _)) = super::native_host::probe_view() {
          window.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: slint::platform::Key::Escape.into() });
          window.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: slint::platform::Key::Escape.into() });
          stage = 25;
        },
        25 => if let Some((window, generation, _)) = super::native_host::probe_view()
          && !window.global::<SettingsState>().get_confirm_invalid_close() {
          let state = window.global::<SettingsState>();
          anyhow::ensure!(!state.get_confirm_invalid_close() && window.global::<Interaction>().get_editable() && state.get_focus_invalid() == 1, "Keep editing did not restore the invalid field");
          render(&window, "invalid-template.png")?;
          window.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "x".into() });
          window.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: "x".into() });
          anyhow::ensure!(state.get_filename().contains('x'), "Keep editing did not focus the input for typing");
          super::native_host::close_requested(generation);
          render(&window, "invalid-close-cross.png")?;
          stage = 26;
        },
        26 => if let Some((window, _, _)) = super::native_host::probe_view() {
          probe_click(window.window(), 514.0, 259.0);
          stage = 27;
        },
        27 => if let Some((window, generation, _)) = super::native_host::probe_view()
          && !window.global::<SettingsState>().get_confirm_invalid_close() {
          let state = window.global::<SettingsState>();
          anyhow::ensure!(!state.get_confirm_invalid_close() && window.global::<Interaction>().get_editable() && state.get_focus_invalid() == 1, "Dialog cross did not keep editing");
          state.set_audio(false); state.invoke_edit_request("audio".into());
          super::native_host::close_requested(generation);
          render(&window, "invalid-close.png")?;
          stage = 28;
        },
        28 => if let Some((window, _, _)) = super::native_host::probe_view() {
          probe_click(window.window(), 480.0, 364.0);
          stage = 20;
        },
        20 => if super::native_host::probe_view().is_none() {
          anyhow::ensure!(!Config::current(&app).record_audio && Config::current(&app).filename_template.is_none(), "Discard invalid lost valid edits or saved invalid text");
          super::native_host::show(&app, Some(Section::Notifications)).map_err(anyhow::Error::msg)?;
          stage = 21;
        },
        21 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let state = window.global::<SettingsState>();
          state.set_notification_duration_index(3); state.invoke_edit_request("notification-duration-index".into());
          state.invoke_action("notification".into()); stage = 11;
        },
        11 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<SettingsState>().get_preview_cooldown() > 0 {
          anyhow::ensure!(Config::current(&app).notification_duration_ms == 10000, "Preview did not wait for duration persistence");
          window.global::<SettingsState>().invoke_action("notification".into());
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          let buffer = window.window().take_snapshot()?;
          let mut encoder = png::Encoder::new(std::fs::File::create(directory.join("notifications.png"))?, buffer.width(), buffer.height());
          encoder.set_color(png::ColorType::Rgba); encoder.set_depth(png::BitDepth::Eight); encoder.write_header()?.write_image_data(buffer.as_bytes())?;
          window.set_page(2); stage = 12;
        },
        12 => if let Some((window, generation, idle)) = super::native_host::probe_view() && idle {
          anyhow::ensure!(window.global::<SettingsState>().get_preview_cooldown() > 0, "Navigation cleared preview cooldown");
          window.global::<SettingsState>().invoke_action("probe-action".into());
          super::native_host::close_requested(generation); stage = 15;
        },
        15 => if let Some((window, _, _)) = super::native_host::probe_view() {
          window.global::<SettingsState>().invoke_action("cancel-close".into());
          anyhow::ensure!(!window.global::<Interaction>().get_editable(), "Cancel close re-enabled controls during an active action");
          stage = 16;
        },
        16 => if let Some((window, generation, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          anyhow::ensure!(!window.global::<SettingsState>().get_close_pending(), "Cancelled close remained pending after the action");
          window.global::<SettingsState>().invoke_action("probe-action".into());
          super::native_host::close_requested(generation); stage = 13;
        },
        13 => if super::native_host::probe_view().is_none() {
          super::native_host::show(&app, Some(Section::Hotkeys)).map_err(anyhow::Error::msg)?; stage = 14;
        },
        14 => if let Some((window, generation, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          window.set_page(5);
          window.window().take_snapshot()?;
          anyhow::ensure!(window.global::<SettingsState>().get_folder_index() == 1, "Reopened settings lost the persisted save location");
          window.set_page(2);
          window.global::<SettingsState>().invoke_capture_request("video-area".into());
          generations.push(generation); super::native_host::close_requested(generation); stage = 4;
        },
        40 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          action_baseline = Some(Config::current(&app));
          window.set_page(0);
          script_picker(true, Some(directory.join("export.json")))?;
          window.global::<SettingsState>().invoke_action("export".into()); stage = 41;
        },
        41 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          let baseline = action_baseline.as_ref().ok_or_else(|| anyhow::anyhow!("Missing action baseline"))?;
          let exported: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.join("export.json"))?)?;
          anyhow::ensure!(serde_json::to_value(exported)? == serde_json::to_value(baseline)?, "Export lost configuration fields");
          anyhow::ensure!(window.global::<SettingsState>().get_action_error().is_empty(), "Export displayed an error");
          script_picker(true, None)?;
          window.global::<SettingsState>().invoke_action("export".into()); stage = 48;
        },
        48 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          anyhow::ensure!(window.global::<SettingsState>().get_action_error().is_empty(), "Cancelled export displayed an error");
          script_picker(true, Some(directory.clone()))?;
          window.global::<SettingsState>().invoke_action("export".into()); stage = 49;
        },
        49 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          anyhow::ensure!(!window.global::<SettingsState>().get_action_error().is_empty(), "Failed export has no inline error");
          render(&window, "export-error.png")?;
          script_picker(false, None)?;
          window.global::<SettingsState>().invoke_action("import".into()); stage = 42;
        },
        42 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          anyhow::ensure!(window.global::<SettingsState>().get_action_error().is_empty(), "Cancelled import displayed an error");
          std::fs::write(directory.join("invalid-import.json"), b"[]")?;
          script_picker(false, Some(directory.join("invalid-import.json")))?;
          window.global::<SettingsState>().invoke_action("import".into()); stage = 43;
        },
        43 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          let baseline = action_baseline.as_ref().ok_or_else(|| anyhow::anyhow!("Missing action baseline"))?;
          anyhow::ensure!(!window.global::<SettingsState>().get_action_error().is_empty(), "Failed import has no inline error");
          anyhow::ensure!(serde_json::to_value(Config::current(&app))? == serde_json::to_value(baseline)?, "Cancelled or malformed import changed runtime preferences");
          render(&window, "import-error.png")?;
          let mut imported = baseline.clone();
          imported.language = "fr".into(); imported.screenshot_format = crate::config::ScreenshotFormat::Webp; imported.video_fps = 30;
          std::fs::write(directory.join("valid-import.json"), serde_json::to_vec_pretty(&imported)?)?;
          script_picker(false, Some(directory.join("valid-import.json")))?;
          let state = window.global::<SettingsState>();
          state.set_filename("invalid:name".into());
          state.invoke_edit_request("filename".into());
          state.set_subfolder("invalid/folder".into());
          state.invoke_edit_request("subfolder".into());
          state.set_sound(!state.get_sound());
          state.invoke_edit_request("sound".into());
          state.invoke_action("import".into()); stage = 44;
        },
        44 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let state = window.global::<SettingsState>();
          anyhow::ensure!(state.get_action_error().is_empty() && window.get_language_index() == 1 && state.get_format_index() == 2 && state.get_video_fps_index() == 0, "Successful import did not refresh the UI");
          anyhow::ensure!(state.get_filename_error().is_empty() && state.get_subfolder_error().is_empty() && state.get_filename() != "invalid:name" && state.get_subfolder() != "invalid/folder", "Successful import retained obsolete invalid drafts");
          let saved: Config = serde_json::from_slice(&std::fs::read(config_path().ok_or_else(|| anyhow::anyhow!("Missing probe config"))?)?)?;
          anyhow::ensure!(saved.language == "fr" && saved.screenshot_format == crate::config::ScreenshotFormat::Webp && saved.video_fps == 30, "Import did not persist");
          FAIL_WRITE.store(true, Ordering::Release);
          window.set_confirming_reset(true);
          state.invoke_action("reset".into()); stage = 45;
        },
        45 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let state = window.global::<SettingsState>();
          anyhow::ensure!(!window.get_confirming_reset() && !state.get_action_error().is_empty() && window.get_language_index() == 1 && state.get_format_index() == 2, "Failed reset stranded its confirmation or lost the UI/error");
          anyhow::ensure!(Config::current(&app).language == "fr", "Failed reset published defaults");
          state.set_filename("invalid:name".into());
          state.invoke_edit_request("filename".into());
          state.invoke_action("reset".into()); stage = 46;
        },
        46 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let state = window.global::<SettingsState>();
          let mut defaults = Config::default(); defaults.validate();
          anyhow::ensure!(state.get_action_error().is_empty() && serde_json::to_value(Config::current(&app))? == serde_json::to_value(&defaults)? && state.get_format_index() == 0 && state.get_video_fps_index() == 0, "Reset did not persist and refresh defaults");
          anyhow::ensure!(state.get_filename_error().is_empty() && state.get_filename() != "invalid:name", "Successful reset retained the invalid filename draft");
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          script_picker(false, Some(directory.join("export.json")))?;
          state.invoke_action("import".into()); stage = 47;
        },
        47 => if let Some((window, _, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let baseline = action_baseline.as_ref().ok_or_else(|| anyhow::anyhow!("Missing action baseline"))?;
          // Reset template fields are omitted on export and deserialize to the
          // same built-in defaults. Compare the validated import projection.
          let mut expected: Config = serde_json::from_value(serde_json::to_value(baseline)?)?;
          expected.validate();
          anyhow::ensure!(serde_json::to_value(Config::current(&app))? == serde_json::to_value(expected)? && window.get_language_index() == 0, "Export/import restoration lost preferences");
          let directory = DIRECTORY.get().ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
          let path = directory.join("results.json");
          let mut report: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
          report["configuration_actions_refresh_and_failures"] = true.into();
          std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
          window.set_page(5); stage = 22;
        },
        22 => if let Some((window, generation, idle)) = super::native_host::probe_view() && idle && window.global::<Interaction>().get_editable() {
          let state = window.global::<SettingsState>();
          state.set_filename("file:name".into()); state.invoke_edit_request("filename".into());
          super::native_host::close_requested(generation);
          anyhow::ensure!(state.get_confirm_invalid_close(), "Fixture did not open invalid-close dialog");
          state.invoke_action("cancel-close".into());
          state.set_audio(true); state.invoke_edit_request("audio".into());
          FAIL_WRITE.store(QUIT_FAIL.load(Ordering::Relaxed), Ordering::Release);
          super::native_host::close_requested(generation);
          QUIT_STARTED.set(Instant::now()).map_err(|_| anyhow::anyhow!("Quit probe initialized twice"))?;
          super::native_host::request_exit(&app).map_err(anyhow::Error::msg)?;
          state.invoke_action("cancel-close".into());
          anyhow::ensure!(state.get_close_pending(), "A late Keep editing action cancelled explicit Quit");
          stage = 23;
        },
        23 => {},
        _ => anyhow::bail!("Invalid probe stage"),
      }
      Ok(false)
    })();
    let terminal = match step { Ok(false) => return, Ok(true) => Ok(()), Err(error) => Err(error.to_string()) };
    if let Err(error) = &terminal && let Some(directory) = DIRECTORY.get()
      && let Err(write_error) = std::fs::write(directory.join("failure.txt"), error) { tracing::error!(%write_error, "Cannot write probe failure"); }
    *output.borrow_mut() = Some(terminal);
    if let Err(error) = slint::quit_event_loop() { tracing::error!(%error, "Cannot end native smoke loop"); }
  });
  Some(Probe { timer, result })
}

fn probe_click(window: &slint::Window, x: f32, y: f32) {
  use slint::platform::{PointerEventButton, WindowEvent};
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

fn render(window: &flinch_settings::ui::SettingsWindow, name: &str) -> anyhow::Result<()> {
  let directory = DIRECTORY
    .get()
    .ok_or_else(|| anyhow::anyhow!("Missing probe output"))?;
  let buffer = window.window().take_snapshot()?;
  let mut encoder = png::Encoder::new(
    std::fs::File::create(directory.join(name))?,
    buffer.width(),
    buffer.height(),
  );
  encoder.set_color(png::ColorType::Rgba);
  encoder.set_depth(png::BitDepth::Eight);
  encoder
    .write_header()?
    .write_image_data(buffer.as_bytes())?;
  Ok(())
}
