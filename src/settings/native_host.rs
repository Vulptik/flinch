//! Native Slint application loop and generation-bound settings presenter.

use std::{
  cell::RefCell,
  rc::Rc,
  sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  time::Duration,
};

use super::config_service::CONFIG_SERVICE;
use crate::app::context::AppHandle;
use crate::commands::config::NativeConfigEnvironment;
use crate::{
  config::{CaptureType, Config},
  error::ErrorExt,
};
use flinch_settings::{
  dispatch::CoalescedEvent,
  edit::{EditDraft, FolderTarget, PreferenceEdit, ShortcutAction},
  model::{Section, SettingsSnapshot},
  save::{CloseDecision, EditSession, SaveRequest},
  slint::{self, ComponentHandle, winit_030::WinitWindowAccessor},
  ui::{self, Interaction, Presentation, SettingsState, SettingsWindow},
  windows,
};

struct View {
  window: SettingsWindow,
  controller: Rc<RefCell<Controller>>,
}

enum AfterSave {
  Action(String),
  Capture(ShortcutAction),
}

pub(super) struct Controller {
  invalid_filename: Option<String>,
  invalid_subfolder: Option<String>,
  generation: u64,
  pub(super) app: AppHandle,
  session: EditSession,
  debounce: slint::Timer,
  close_deadline: slint::Timer,
  action_running: bool,
  close_after_action: bool,
  capture_busy: bool,
  capture_cancelled: bool,
  capture_action: Option<ShortcutAction>,
  close_after_capture: bool,
  capture: Option<super::shortcut_capture::CaptureLease<super::shortcut_capture::PlatformCapture>>,
  capture_timer: slint::Timer,
  preview_timer: slint::Timer,
  after_save: Option<AfterSave>,
  update_checked: bool,
  template_preview_time: chrono::DateTime<chrono::Local>,
}

thread_local! { static VIEW: RefCell<Option<View>> = const { RefCell::new(None) }; }
thread_local! { static PREVIEW: RefCell<flinch_settings::preview::PreviewCooldown> = RefCell::new(flinch_settings::preview::PreviewCooldown::default()); }
static GENERATION: AtomicU64 = AtomicU64::new(0);
static PENDING_OPEN: Mutex<CoalescedEvent<(AppHandle, Option<Section>)>> =
  Mutex::new(CoalescedEvent::new());
static QUITTING: AtomicBool = AtomicBool::new(false);
static UI_ACTIVE: AtomicBool = AtomicBool::new(false);
static CONFLICT_REFRESH: AtomicBool = AtomicBool::new(false);
static EXIT_GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn run() -> anyhow::Result<()> {
  #[cfg(debug_assertions)]
  super::native_probe::initialize()?;
  #[cfg(not(debug_assertions))]
  anyhow::ensure!(
    !std::env::args().any(|arg| arg == "--native-settings-smoke"),
    "Native integration probes require a debug build"
  );
  let chrome_failure = Rc::new(RefCell::new(None));
  slint::BackendSelector::new()
    .backend_name("winit".into())
    .renderer_name("skia-software".into())
    .with_winit_window_attributes_hook(crate::app::tray_menu::window_attributes)
    .with_winit_custom_application_handler(super::native_keys::Handler::new(
      windows::ChromeHandler::new(chrome_failure.clone()),
    ))
    .select()?;
  let runtime = crate::app::runtime::initialize()?;
  if let Err(error) = super::placement::initialize(&runtime) {
    tracing::warn!(%error, "Settings placement persistence unavailable");
  }
  UI_ACTIVE.store(true, Ordering::Release);
  let app = crate::app::bootstrap::build_app()?;
  let instance = crate::app::bootstrap::activate(&app)?;
  // The first recording should not pay the worker launch.
  crate::video::controller::prewarm();
  #[cfg(debug_assertions)]
  let probe = super::native_probe::start(app.clone());
  let result = slint::run_event_loop_until_quit();
  UI_ACTIVE.store(false, Ordering::Release);
  if let Some((window, _)) = current() {
    super::placement::remember(&window);
  }
  super::placement::shutdown(&runtime);
  super::text_panel::shutdown();
  VIEW.with(|view| {
    view.borrow_mut().take();
  });
  crate::app::bootstrap::shutdown(&app, instance);
  runtime.shutdown_timeout(Duration::from_secs(5));
  result?;
  #[cfg(debug_assertions)]
  if let Some(probe) = probe {
    probe.finish()?;
  }
  if let Some(error) = chrome_failure.borrow_mut().take() {
    anyhow::bail!(error);
  }
  Ok(())
}

/// Coalesce repeated tray/second-instance opens before submitting one UI event.
pub(crate) fn show(app: &AppHandle, section: Option<Section>) -> Result<(), String> {
  if QUITTING.load(Ordering::Acquire) {
    return Err("Flinch is shutting down".into());
  }
  {
    let mut state = PENDING_OPEN.lock().map_err(|error| error.to_string())?;
    if !state.submit((app.clone(), section)) {
      return Ok(());
    }
  }
  slint::invoke_from_event_loop(|| {
    let request = PENDING_OPEN.lock().map(|mut state| state.take());
    match request {
      Ok(Some((app, section))) => {
        if let Err(error) = show_on_owner(app, section) {
          tracing::error!(%error, "Cannot open native settings");
        }
      },
      Ok(None) => {},
      Err(error) => tracing::error!(%error, "Cannot read native settings open request"),
    }
  })
  .map_err(|error| {
    match PENDING_OPEN.lock() {
      Ok(mut state) => state.submission_failed(),
      Err(error) => tracing::error!(%error, "Cannot clear failed native open submission"),
    }
    error.to_string()
  })
}

pub(super) fn current() -> Option<(SettingsWindow, Rc<RefCell<Controller>>)> {
  VIEW.with(|view| {
    view
      .borrow()
      .as_ref()
      .map(|view| (view.window.clone_strong(), view.controller.clone()))
  })
}

pub(super) fn matching(generation: u64) -> Option<(SettingsWindow, Rc<RefCell<Controller>>)> {
  current().filter(|(_, controller)| controller.borrow().generation == generation)
}

fn show_on_owner(app: AppHandle, section: Option<Section>) -> Result<(), String> {
  show_on_owner_with(app, section, crate::commands::config::load_config)
}

fn show_on_owner_with<
  Fut: std::future::Future<Output = Result<Config, String>> + Send + 'static,
>(
  app: AppHandle,
  section: Option<Section>,
  load: impl FnOnce(AppHandle) -> Fut + Send + 'static,
) -> Result<(), String> {
  let (window, _) = if let Some(view) = current() {
    view
  } else {
    let generation = GENERATION
      .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_add(1)
      })
      .map_err(|_| "Native window generation exhausted")?;
    let window = SettingsWindow::new().map_err(|error| error.to_string())?;
    windows::configure_fonts(&window).map_err(|error| error.to_string())?;
    super::placement::restore(&window)?;
    ui::connect_search(&window)?;
    ui::connect_reset_confirmation(&window);
    let controller = connect_presenter(&window, app.clone(), generation)?;
    load_diagnostics(generation, app.clone());
    update_preview(generation);
    crate::app::runtime::spawn(async move {
      let loaded = load(app).await;
      if let Err(error) = slint::invoke_from_event_loop(move || {
        if let Some((window, _)) = matching(generation) {
          match loaded {
            Ok(config) => {
              if let Err(error) = refresh(&window, &config) {
                display_error(&window, &error);
              } else {
                window.global::<Interaction>().set_editable(true);
                check_updates(generation);
              }
            },
            Err(error) => {
              // The validated current snapshot was already projected above.
              // Loading only reconciles Windows startup status; its failure
              // must not disable unrelated settings or invent startup state.
              tracing::warn!(%error, "Startup status unavailable; other settings remain editable");
              window
                .global::<SettingsState>()
                .set_startup_unavailable(true);
              match text(&window, "native_startup_status_error") {
                Ok(message) => {
                  field_error(&window, PreferenceEdit::AutoStart(false).slot(), &message)
                },
                Err(error) => tracing::error!(%error, "Missing startup status failure message"),
              }
              window.global::<Interaction>().set_editable(true);
              check_updates(generation);
            },
          }
        }
      }) {
        tracing::error!(%error, "Cannot deliver native settings load result");
      }
    });
    (window, controller)
  };
  if let Some(section) = section {
    window.set_page(section.index());
  }
  window.window().set_minimized(false);
  window.show().map_err(|error| error.to_string())?;
  window
    .window()
    .with_winit_window(|window| window.focus_window());
  Ok(())
}

pub(super) fn connect_presenter(
  window: &SettingsWindow,
  app: AppHandle,
  generation: u64,
) -> Result<Rc<RefCell<Controller>>, String> {
  let controller = Rc::new(RefCell::new(Controller {
    invalid_filename: None,
    invalid_subfolder: None,
    generation,
    app: app.clone(),
    session: EditSession::new(generation),
    debounce: slint::Timer::default(),
    close_deadline: slint::Timer::default(),
    action_running: false,
    close_after_action: false,
    capture_busy: false,
    capture_cancelled: false,
    capture_action: None,
    close_after_capture: false,
    capture: None,
    capture_timer: slint::Timer::default(),
    preview_timer: slint::Timer::default(),
    after_save: None,
    update_checked: false,
    template_preview_time: chrono::Local::now(),
  }));
  ui::connect_edits(window, move |edit| receive_edit(generation, edit));
  super::microphone::attach(generation);
  ui::connect_close_request(window, move |_| close_requested(generation));
  window.on_page_changed(move || {
    cancel_capture(generation, false);
    if let Some((window, controller)) = matching(generation)
      && window.get_page() == 5
    {
      controller.borrow_mut().template_preview_time = chrono::Local::now();
      update_template_draft_previews(&window, None);
    }
  });
  window
    .global::<SettingsState>()
    .on_capture_request(move |action| begin_capture(generation, action.as_str()));
  window.global::<Interaction>().set_hotkeys_editable(true);
  window
    .global::<SettingsState>()
    .on_action(move |action| receive_action(generation, action.as_str()));
  refresh(window, &Config::current(&app))?;
  window.global::<Interaction>().set_editable(false);
  VIEW.with(|view| {
    *view.borrow_mut() = Some(View {
      window: window.clone_strong(),
      controller: controller.clone(),
    })
  });
  Ok(controller)
}

fn refresh(window: &SettingsWindow, config: &Config) -> Result<(), String> {
  let mut snapshot = SettingsSnapshot::from(config);
  snapshot.filename_template = Some(config.effective_filename_template().to_owned());
  snapshot.single_folder_subfolder_template = Some(
    config
      .effective_single_folder_subfolder_template()
      .unwrap_or_default()
      .to_owned(),
  );
  let presentation = Presentation {
    subfolder_preview: subfolder_preview(config),
    locale: config.language.clone(),
    filename_preview: crate::commands::config::preview_filename(
      config.effective_filename_template().to_owned(),
    )?,
    screenshots_path: config
      .resolve_capture_path(CaptureType::Screenshot)
      .to_string_lossy()
      .into_owned(),
    videos_path: config
      .resolve_capture_path(CaptureType::Video)
      .to_string_lossy()
      .into_owned(),
    channel: if cfg!(feature = "msix") {
      "msix"
    } else {
      "installer"
    }
    .into(),
  };
  ui::apply_snapshot(window, &snapshot, &presentation)?;
  update_conflicts(window, &config.language);
  if let Some((_, controller)) = current() {
    let controller = controller.borrow();
    let state = window.global::<SettingsState>();
    if let Some(value) = &controller.invalid_filename {
      state.set_filename(value.into());
    }
    if let Some(value) = &controller.invalid_subfolder {
      state.set_subfolder(value.into());
    }
  }
  update_template_draft_previews(window, None);
  Ok(())
}

fn update_conflicts(window: &SettingsWindow, _language: &str) {
  let Some((_, controller)) = current() else {
    return;
  };
  let app = controller.borrow().app.clone();
  let Some(state) = app.try_state::<crate::shortcuts::ShortcutStateHolder>() else {
    return;
  };
  let statuses = state.get_all();
  use crate::shortcuts::state::{ShortcutStatus, ShortcutType as T};
  let conflicts = [
    T::ScreenshotArea,
    T::ScreenshotFullscreen,
    T::ScreenshotActiveWindow,
    T::Gif,
    T::RecordArea,
    T::RecordFullscreen,
    T::RecordActiveWindow,
    T::ScreenshotEdit,
    T::CopyText,
    T::PickColor,
    T::LastRegion,
  ]
  .map(|key| {
    statuses
      .get(&key)
      .is_some_and(|info| info.status == ShortcutStatus::Conflicted)
  });
  window
    .global::<SettingsState>()
    .set_shortcut_conflicts(std::rc::Rc::new(slint::VecModel::from(conflicts.to_vec())).into());
}
pub(crate) fn shortcuts_changed() {
  if !UI_ACTIVE.load(Ordering::Acquire) || CONFLICT_REFRESH.swap(true, Ordering::AcqRel) {
    return;
  }
  if let Err(error) = slint::invoke_from_event_loop(|| {
    CONFLICT_REFRESH.store(false, Ordering::Release);
    if let Some((window, _)) = current() {
      update_conflicts(&window, "");
    }
  }) {
    CONFLICT_REFRESH.store(false, Ordering::Release);
    tracing::warn!(%error, "Cannot refresh shortcut registration status");
  }
}

fn text(window: &SettingsWindow, key: &str) -> Result<String, String> {
  let locale = flinch_settings::edit::selection(
    &flinch_settings::model::LOCALES,
    window.get_language_index(),
  )?;
  crate::i18n::native_settings(locale, key)
}

fn display_error(window: &SettingsWindow, error: &str) {
  display_action_error(window, "", error);
}

fn display_action_error(window: &SettingsWindow, action: &str, error: &str) {
  tracing::warn!(error, "Native settings operation failed");
  let state = window.global::<SettingsState>();
  state.set_error_action(action.into());
  match text(window, action_error_key(action)) {
    Ok(message) => {
      state.set_action_message(message.clone().into());
      state.set_action_error(message.into());
    },
    Err(error) => tracing::error!(%error, "Missing native action error message"),
  }
}

fn action_error_key(action: &str) -> &'static str {
  match action {
    "import" => "native_import_error",
    "export" => "native_export_error",
    "reset" => "native_reset_error",
    "notification" => "native_notification_error",
    "open-screenshots" | "open-videos" => "native_open_folder_error",
    "logs" => "native_open_logs_error",
    "copy-diagnostics" => "native_copy_error",
    action if FolderTarget::from_pick_action(action).is_some() => "native_folder_error",
    _ => "native_action_error",
  }
}

pub(super) fn field_error(window: &SettingsWindow, slot: usize, message: &str) {
  use slint::Model;
  let state = window.global::<SettingsState>();
  let current = state.get_field_errors();
  let Some(mut errors) = (0..flinch_settings::edit::SLOT_COUNT)
    .map(|index| current.row_data(index))
    .collect::<Option<Vec<_>>>()
  else {
    tracing::error!("Settings inline error model is missing a field slot");
    return;
  };
  let Some(value) = errors.get_mut(slot) else {
    tracing::error!(slot, "Settings inline error slot is out of range");
    return;
  };
  *value = message.into();
  state.set_field_errors(std::rc::Rc::new(slint::VecModel::from(errors)).into());
}

fn template_error(window: &SettingsWindow, error: &str) -> Result<String, String> {
  let (key, value) = template_error_parts(error)?;
  Ok(text(window, key)?.replace("{value}", value))
}

fn template_error_parts(error: &str) -> Result<(&str, &str), String> {
  let (key, value) = if error.starts_with("Unknown token") {
    (
      "native_template_token",
      error
        .split('"')
        .nth(1)
        .ok_or("Template validation error is missing its token")?,
    )
  } else if error.starts_with("Control characters") {
    ("native_template_control", "")
  } else if error.starts_with("Character") {
    (
      "native_template_character",
      error
        .split('\'')
        .nth(1)
        .ok_or("Template validation error is missing its character")?,
    )
  } else if error.contains("too long") {
    ("native_template_long", "")
  } else if error.contains("reserved") {
    (
      "native_template_reserved",
      error
        .split('"')
        .nth(1)
        .ok_or("Reserved-name error is missing its name")?,
    )
  } else if error.contains("dot") {
    ("native_template_edge", "")
  } else if error.contains("produces an empty") {
    ("native_template_name", "")
  } else {
    ("native_template_empty", "")
  };
  Ok((key, value))
}

fn receive_edit(generation: u64, edit: Result<PreferenceEdit, String>) {
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  let result = edit.and_then(|edit| {
    field_error(&window, edit.slot(), "");
    window.global::<SettingsState>().set_focus_invalid(0);
    update_template_draft_previews(&window, Some(&edit));
    let validation = match &edit {
      PreferenceEdit::Filename(value) => Some((
        true,
        value.clone(),
        crate::filename::validate_template(value),
      )),
      PreferenceEdit::Subfolder(value) => Some((
        false,
        value.clone(),
        crate::capture_subfolder::validate_template(value),
      )),
      PreferenceEdit::ResetFilename => Some((true, String::new(), Ok(()))),
      PreferenceEdit::ResetSubfolder => Some((false, String::new(), Ok(()))),
      _ => None,
    };
    if let Some((filename, value, result)) = validation {
      let mut owner = controller.borrow_mut();
      let invalid = result.is_err();
      if filename {
        owner.invalid_filename = invalid.then_some(value);
      } else {
        owner.invalid_subfolder = invalid.then_some(value);
      }
      let blocked = owner.invalid_filename.is_some() || owner.invalid_subfolder.is_some();
      owner.session.set_validation_blocked(blocked);
      let state = window.global::<SettingsState>();
      let message = match &result {
        Err(error) => template_error(&window, error)?,
        Ok(()) => String::new(),
      };
      if filename {
        state.set_filename_error(message.into());
      } else {
        state.set_subfolder_error(message.into());
      }
      if let Err(error) = result {
        tracing::debug!(%error, "Invalid native template draft retained for correction");
        owner.session.remove(&edit);
        return Ok(());
      }
    }
    if let Ok(candidate) =
      Config::try_from((Config::current(&controller.borrow().app), edit.clone()))
    {
      let state = window.global::<SettingsState>();
      state.set_screenshots_path(
        candidate
          .resolve_capture_path(CaptureType::Screenshot)
          .to_string_lossy()
          .as_ref()
          .into(),
      );
      state.set_videos_path(
        candidate
          .resolve_capture_path(CaptureType::Video)
          .to_string_lossy()
          .as_ref()
          .into(),
      );
      state.set_together_folder_custom(candidate.custom_single_folder.is_some());
      state.set_screenshots_folder_custom(candidate.custom_screenshots_folder.is_some());
      state.set_videos_folder_custom(candidate.custom_videos_folder.is_some());
    }
    controller
      .borrow_mut()
      .session
      .edit(edit)
      .map_err(str::to_owned)
  });
  if let Err(error) = result {
    display_error(&window, &error);
    return;
  }
  if controller.borrow().session.is_idle() {
    // Replacing the last failed valid draft with invalid text leaves only its
    // contextual validation error; there is no longer a queued write to retry.
    controller.borrow_mut().session.retry();
    let state = window.global::<SettingsState>();
    state.set_save_error("".into());
    state.set_save_failed(false);
  }
  controller.borrow().debounce.start(
    slint::TimerMode::SingleShot,
    Duration::from_millis(250),
    move || start_save(generation),
  );
}

fn start_save(generation: u64) {
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  let request = controller.borrow_mut().session.start();
  let request = match request {
    Ok(Some(request)) => request,
    Ok(None) => return,
    Err(error) => {
      display_error(&window, error);
      return;
    },
  };
  window.global::<SettingsState>().set_saving(true);
  window.global::<SettingsState>().set_save_failed(false);
  let app = controller.borrow().app.clone();
  crate::app::runtime::spawn(async move {
    let result = CONFIG_SERVICE
      .save(&NativeConfigEnvironment(app.clone()), request.draft.clone())
      .await
      .map_err(|error| error.to_rich_string());
    let result = match result {
      Ok(()) => {
        let config = Config::current(&app);
        crate::commands::set_app_language(app, config.language.clone())
          .await
          .map(|()| config)
      },
      Err(error) => Err(error),
    };
    if let Err(error) =
      slint::invoke_from_event_loop(move || finish_save(generation, request, result))
    {
      tracing::error!(%error, "Cannot deliver native settings save acknowledgment");
    }
  });
}

fn finish_save(generation: u64, request: SaveRequest<EditDraft>, result: Result<Config, String>) {
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  match result {
    Ok(config) => {
      if !controller.borrow_mut().session.saved(&request) {
        return;
      }
      window.global::<SettingsState>().set_saving(false);
      window.global::<SettingsState>().set_save_failed(false);
      for edit in request.draft.edits() {
        field_error(&window, edit.slot(), "");
      }
      if controller.borrow().session.is_idle() {
        window.global::<SettingsState>().set_save_error("".into());
        if let Err(error) = refresh(&window, &config) {
          display_error(&window, &error);
        }
      }
      start_save(generation);
    },
    Err(error) => {
      let slots = request
        .draft
        .edits()
        .map(PreferenceEdit::slot)
        .collect::<Vec<_>>();
      if !controller.borrow_mut().session.failed(request) {
        return;
      }
      if QUITTING.load(Ordering::Acquire) {
        tracing::warn!(%error, "Discarding failed settings draft during explicit app quit");
        controller.borrow_mut().session.request_quit();
        resolve_close(generation);
        return;
      }
      controller.borrow_mut().after_save = None;
      window.global::<SettingsState>().set_saving(false);
      window
        .global::<SettingsState>()
        .set_save_error(error.clone().into());
      window.global::<SettingsState>().set_save_failed(true);
      tracing::warn!(%error, "Settings write failed; draft retained for an explicit retry or edit");
      match text(&window, "native_setting_save_error") {
        Ok(message) => {
          for slot in slots {
            field_error(&window, slot, &message);
          }
        },
        Err(error) => tracing::error!(%error, "Missing inline save failure message"),
      }
      controller.borrow_mut().session.cancel_close();
      window.global::<SettingsState>().set_close_pending(false);
      window.global::<SettingsState>().set_close_timeout(false);
      window
        .global::<SettingsState>()
        .set_confirm_invalid_close(false);
      window.global::<Interaction>().set_editable(true);
    },
  }
  resolve_close(generation);
  let action = if controller.borrow().session.is_idle()
    && !window.global::<SettingsState>().get_close_pending()
  {
    controller.borrow_mut().after_save.take()
  } else {
    None
  };
  if let Some(action) = action {
    match action {
      AfterSave::Action(action) => receive_action(generation, &action),
      AfterSave::Capture(action) => begin_capture(generation, action.id()),
    }
  }
}

fn subfolder_preview(config: &Config) -> String {
  match config.effective_single_folder_subfolder_template() {
    Some(template) => crate::capture_subfolder::render_template(
      template,
      &crate::capture_subfolder::SubfolderContext {
        now: chrono::Local::now(),
      },
    ),
    None => String::new(),
  }
}

fn update_template_draft_previews(window: &SettingsWindow, edit: Option<&PreferenceEdit>) {
  let state = window.global::<SettingsState>();
  let filename = match edit {
    Some(PreferenceEdit::ResetFilename) => crate::filename::DEFAULT_TEMPLATE.into(),
    _ => state.get_filename(),
  };
  let subfolder = match edit {
    Some(PreferenceEdit::ResetSubfolder) => crate::capture_subfolder::DEFAULT_TEMPLATE.into(),
    _ => state.get_subfolder(),
  };
  let (filename, subfolder) = if let Some((_, controller)) = current() {
    let now = controller.borrow().template_preview_time;
    super::template_draft_previews_at(filename.as_str(), subfolder.as_str(), now)
  } else {
    super::template_draft_previews(filename.as_str(), subfolder.as_str())
  };
  state.set_filename_preview(filename.into());
  state.set_subfolder_preview(subfolder.into());
}

fn update_preview(generation: u64) {
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  let seconds = PREVIEW.with(|preview| preview.borrow().remaining(std::time::Instant::now()));
  let state = window.global::<SettingsState>();
  state.set_preview_cooldown(seconds as i32);
  if seconds == 0 {
    controller.borrow().preview_timer.stop();
  } else if !controller.borrow().preview_timer.running() {
    controller.borrow().preview_timer.start(
      slint::TimerMode::Repeated,
      Duration::from_millis(250),
      move || update_preview(generation),
    );
  }
}

fn check_updates(generation: u64) {
  let Some((_, controller)) = matching(generation) else {
    return;
  };
  if controller.borrow().update_checked {
    return;
  }
  controller.borrow_mut().update_checked = true;
  let app = controller.borrow().app.clone();
  let enabled = Config::current(&app).check_for_updates;
  // Guard before dispatch as well as inside the transport-independent service.
  if cfg!(feature = "msix") || !enabled {
    return;
  }
  crate::app::runtime::spawn(async move {
    let result = super::updates::check(
      &super::updates::WindowsManifest,
      cfg!(feature = "msix"),
      enabled,
      env!("CARGO_PKG_VERSION"),
    )
    .await;
    if let Err(error) = slint::invoke_from_event_loop(move || {
      finish_update_result(generation, &app, result);
    }) {
      tracing::error!(%error, "Cannot deliver native version check result");
    }
  });
}

pub(super) fn close_requested(generation: u64) {
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  controller.borrow_mut().after_save = None;
  if controller.borrow().capture_busy {
    cancel_capture(generation, true);
    return;
  }
  if controller.borrow().action_running {
    controller.borrow_mut().close_after_action = true;
    window.global::<SettingsState>().set_close_pending(true);
    controller.borrow().close_deadline.start(
      slint::TimerMode::SingleShot,
      Duration::from_secs(10),
      move || {
        if let Some((window, controller)) = matching(generation)
          && controller.borrow().action_running
          && controller.borrow().close_after_action
        {
          window.global::<SettingsState>().set_close_timeout(true);

          tracing::warn!(
            "Close is waiting for an active settings action; its worker retains ownership"
          );
        }
      },
    );

    return;
  }
  controller.borrow().debounce.stop();
  if QUITTING.load(Ordering::Acquire) {
    {
      let mut owner = controller.borrow_mut();
      owner.invalid_filename = None;
      owner.invalid_subfolder = None;
      owner.session.request_quit();
    }
    window
      .global::<SettingsState>()
      .set_confirm_invalid_close(false);
  } else {
    controller.borrow_mut().session.request_close();
    if controller.borrow().invalid_filename.is_some()
      || controller.borrow().invalid_subfolder.is_some()
      || window.global::<SettingsState>().get_save_failed()
    {
      window
        .global::<SettingsState>()
        .set_confirm_invalid_close(true);
    }
  }
  controller.borrow().close_deadline.start(slint::TimerMode::SingleShot, Duration::from_secs(10), move || {
    if let Some((window, controller)) = matching(generation)
      && controller.borrow().session.close_decision() == CloseDecision::WaitForSave {
      window.global::<SettingsState>().set_close_timeout(true);
      tracing::warn!("Settings close acknowledgment exceeded ten seconds; transaction remains owned by its worker");
    }
  });
  window.global::<SettingsState>().set_close_pending(true);
  window.global::<Interaction>().set_editable(false);
  start_save(generation);
  resolve_close(generation);
}

pub(crate) fn is_probe() -> bool {
  #[cfg(debug_assertions)]
  {
    super::native_probe::is_active()
  }
  #[cfg(not(debug_assertions))]
  {
    false
  }
}

#[cfg(debug_assertions)]
pub(super) fn probe_view() -> Option<(SettingsWindow, u64, bool)> {
  current().map(|(window, controller)| {
    let controller = controller.borrow();
    (
      window,
      controller.generation,
      controller.session.is_idle() && !controller.capture_busy,
    )
  })
}

fn resolve_close(generation: u64) {
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  if controller.borrow().session.close_decision() != CloseDecision::Close {
    return;
  }
  super::placement::remember(&window);
  if let Err(error) = window.hide() {
    display_error(&window, &error.to_string());
    return;
  }
  VIEW.with(|view| {
    view.borrow_mut().take();
  });
  if QUITTING.load(Ordering::Acquire) {
    finish_exit(&controller.borrow().app);
  }
}

pub(super) fn capture_busy() -> bool {
  current().is_some_and(|(_, controller)| controller.borrow().capture_busy)
}

pub(super) fn cancel_current_capture() {
  let generation = current().map(|(_, controller)| controller.borrow().generation);
  if let Some(generation) = generation {
    cancel_capture(generation, false);
  }
}

fn begin_capture(generation: u64, id: &str) {
  let Some(action) = ShortcutAction::from_id(id) else {
    return;
  };
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  if controller.borrow().capture_busy {
    if window.global::<SettingsState>().get_capture_target() == id {
      cancel_capture(generation, false);
    }
    return;
  }
  if controller.borrow().action_running || !controller.borrow().session.is_idle() {
    if !controller.borrow().action_running {
      let mut owner = controller.borrow_mut();
      owner.after_save = Some(AfterSave::Capture(action));
      owner.session.retry();
      window.global::<SettingsState>().set_save_failed(false);
      window.global::<Interaction>().set_editable(false);
    }
    start_save(generation);
    return;
  }
  field_error(
    &window,
    PreferenceEdit::Shortcut(action, String::new()).slot(),
    "",
  );
  {
    let mut controller = controller.borrow_mut();
    controller.capture_busy = true;
    controller.capture_action = Some(action);
    controller.capture_cancelled = false;
    controller.close_after_capture = false;
  }
  window.global::<Interaction>().set_editable(false);
  let app = controller.borrow().app.clone();
  crate::app::runtime::spawn(async move {
    let acquired = super::shortcut_capture::CAPTURE_SERVICE
      .acquire(super::shortcut_capture::PlatformCapture(app))
      .await;
    if let Err(error) = slint::invoke_from_event_loop(move || {
      let Some((window, controller)) = matching(generation) else {
        return;
      };
      match acquired {
        Ok(lease) => {
          controller.borrow_mut().capture = Some(lease);
          let cancelled = controller.borrow().capture_cancelled;
          if cancelled {
            cancel_capture(generation, false);
            return;
          }
          window
            .global::<SettingsState>()
            .set_capture_target(action.id().into());
          controller.borrow().capture_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_secs(30),
            move || cancel_capture(generation, false),
          );
        },
        Err(error) => {
          controller.borrow_mut().capture_busy = false;
          controller.borrow_mut().capture_action = None;
          shortcut_error(&window, action, &error.to_rich_string());
          window.global::<Interaction>().set_editable(true);
          let close = controller.borrow().close_after_capture;
          if close {
            close_requested(generation);
          }
        },
      }
    }) {
      tracing::error!(%error, "Cannot acknowledge shortcut capture acquisition");
    }
  });
}

fn cancel_capture(generation: u64, close: bool) {
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  if !controller.borrow().capture_busy {
    if matches!(controller.borrow().after_save, Some(AfterSave::Capture(_))) {
      controller.borrow_mut().after_save = None;
      window.global::<Interaction>().set_editable(true);
    }
    return;
  }
  {
    let mut controller = controller.borrow_mut();
    controller.capture_cancelled = true;
    controller.close_after_capture |= close;
    controller.capture_timer.stop();
  }
  window
    .global::<SettingsState>()
    .set_capture_target("".into());
  let lease = controller.borrow_mut().capture.take();
  if let Some(lease) = lease {
    finish_capture(generation, lease, None);
  }
}

pub(crate) fn capture_key(value: String) {
  let Some((window, controller)) = current() else {
    return;
  };
  let generation = controller.borrow().generation;
  let Some(action) = ShortcutAction::from_id(
    window
      .global::<SettingsState>()
      .get_capture_target()
      .as_str(),
  ) else {
    return;
  };
  let lease = controller.borrow_mut().capture.take();
  if let Some(lease) = lease {
    controller.borrow().capture_timer.stop();
    window
      .global::<SettingsState>()
      .set_capture_target("".into());
    finish_capture(generation, lease, Some((action, value)));
  }
}

fn finish_capture(
  generation: u64,
  lease: super::shortcut_capture::CaptureLease<super::shortcut_capture::PlatformCapture>,
  edit: Option<(ShortcutAction, String)>,
) {
  let Some((_, controller)) = matching(generation) else {
    return;
  };
  let app = controller.borrow().app.clone();
  let capture_action = controller.borrow_mut().capture_action.take();
  crate::app::runtime::spawn(async move {
    // Test and save before restoring registrations: the lease still suppresses
    // capture handlers and configuration publication cannot re-enable them.
    let mut failed_edit = None;
    let saved = if let Some((action, value)) = edit {
      let candidate = Config::try_from((
        Config::current(&app),
        PreferenceEdit::Shortcut(action, value.clone()),
      ));
      match candidate {
        Ok(_) => match if is_probe() {
          Ok(())
        } else {
          crate::shortcuts::test_shortcut_registration(&app, &value)
        } {
          Ok(()) => {
            let edit = PreferenceEdit::Shortcut(action, value);
            let result = CONFIG_SERVICE
              .save(&NativeConfigEnvironment(app.clone()), edit.clone())
              .await;
            if result.is_err() {
              failed_edit = Some(edit);
            }
            result
          },
          Err(error) => Err(anyhow::Error::msg(error)),
        },
        Err(error) => Err(error),
      }
    } else {
      Ok(())
    };
    let restored = lease.release().await;
    let result = match (saved, restored) {
      (Ok(()), Ok(())) => Ok(()),
      (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error.to_rich_string()),
      (Err(saved), Err(restored)) => Err(format!("{saved:#}; {restored:#}")),
    };
    let mut config = Config::current(&app);
    if let Some(edit) = &failed_edit {
      match Config::try_from((config.clone(), edit.clone())) {
        Ok(draft) => config = draft,
        Err(error) => tracing::error!(%error, "Cannot project retained shortcut draft"),
      }
    }
    if let Err(error) = slint::invoke_from_event_loop(move || {
      finish_capture_result(generation, capture_action, config, failed_edit, result);
    }) {
      tracing::error!(%error, "Cannot acknowledge shortcut restoration");
    }
  });
}

fn shortcut_error(window: &SettingsWindow, action: ShortcutAction, error: &str) {
  tracing::warn!(%error, "Shortcut operation failed");
  match text(window, "native_shortcut_error") {
    Ok(message) => field_error(
      window,
      PreferenceEdit::Shortcut(action, String::new()).slot(),
      &message,
    ),
    Err(error) => tracing::error!(%error, "Missing shortcut operation failure message"),
  }
}

fn clear_replaced_template_drafts(
  filename: &mut Option<String>,
  subfolder: &mut Option<String>,
  session: &mut EditSession,
) {
  *filename = None;
  *subfolder = None;
  session.set_validation_blocked(false);
}

fn receive_action(generation: u64, action: &str) {
  let Some((window, controller)) = matching(generation) else {
    return;
  };
  if controller.borrow().capture_busy {
    cancel_capture(generation, false);
    return;
  }
  if action == "notification"
    && PREVIEW.with(|preview| preview.borrow().remaining(std::time::Instant::now())) > 0
  {
    update_preview(generation);
    return;
  }
  if let Some(target) = FolderTarget::from_reset_action(action) {
    controller.borrow_mut().session.retry();
    window.global::<SettingsState>().set_save_failed(false);
    receive_edit(generation, Ok(PreferenceEdit::SaveFolder(target, None)));
    return;
  }
  match action {
    "dismiss-update" => window
      .global::<SettingsState>()
      .set_update_version("".into()),
    "reset-filename" | "reset-subfolder" => {
      let edit = if action == "reset-filename" {
        window
          .global::<SettingsState>()
          .set_filename(crate::filename::DEFAULT_TEMPLATE.into());
        PreferenceEdit::ResetFilename
      } else {
        window
          .global::<SettingsState>()
          .set_subfolder(crate::capture_subfolder::DEFAULT_TEMPLATE.into());
        PreferenceEdit::ResetSubfolder
      };
      controller.borrow_mut().session.retry();
      window.global::<SettingsState>().set_save_failed(false);
      receive_edit(generation, Ok(edit));
    },
    "retry" => {
      if controller.borrow().action_running || window.global::<SettingsState>().get_saving() {
        return;
      }
      controller.borrow().debounce.stop();
      controller.borrow_mut().session.retry();
      window.global::<SettingsState>().set_save_failed(false);
      start_save(generation);
    },
    "cancel-close" => {
      // A late click on the settings confirmation cannot cancel explicit Quit.
      if QUITTING.load(Ordering::Acquire) {
        return;
      }
      controller.borrow().close_deadline.stop();
      controller.borrow_mut().session.cancel_close();
      controller.borrow_mut().close_after_action = false;
      let state = window.global::<SettingsState>();
      state.set_confirm_invalid_close(false);
      window
        .global::<Interaction>()
        .set_editable(!controller.borrow().action_running);
      state.set_focus_invalid(0);
      if controller.borrow().invalid_filename.is_some()
        || controller.borrow().invalid_subfolder.is_some()
      {
        window.set_query("".into());
        window.set_page(5);
        state.set_focus_invalid(if controller.borrow().invalid_filename.is_some() {
          1
        } else {
          2
        });
      }
      state.set_close_pending(false);
      state.set_close_timeout(false);
      window
        .global::<Interaction>()
        .set_editable(!controller.borrow().action_running);
    },
    "discard" => {
      let result = if window.global::<SettingsState>().get_save_failed() {
        controller.borrow_mut().session.discard_and_close()
      } else {
        controller.borrow_mut().session.discard_invalid_and_close()
      };
      if let Err(error) = result {
        display_error(&window, error);
      } else {
        {
          let mut owner = controller.borrow_mut();
          owner.invalid_filename = None;
          owner.invalid_subfolder = None;
        }
        let state = window.global::<SettingsState>();
        state.set_confirm_invalid_close(false);
        state.set_filename_error("".into());
        state.set_subfolder_error("".into());
        start_save(generation);
        resolve_close(generation);
      }
    },
    action => {
      window
        .global::<SettingsState>()
        .set_error_action(action.into());
      window.global::<SettingsState>().set_action_error("".into());
      window
        .global::<SettingsState>()
        .set_action_message("".into());
      if controller.borrow().action_running || !controller.borrow().session.is_idle() {
        if !controller.borrow().action_running {
          let mut owner = controller.borrow_mut();
          owner.after_save = Some(AfterSave::Action(action.to_owned()));
          // This explicit user action may retry a failed prerequisite save once.
          owner.session.retry();
          window.global::<SettingsState>().set_save_failed(false);
          window.global::<Interaction>().set_editable(false);
        }

        start_save(generation);
        return;
      }
      controller.borrow_mut().action_running = true;
      window.global::<Interaction>().set_editable(false);
      let app = controller.borrow().app.clone();
      let action = action.to_owned();
      crate::app::runtime::spawn(async move {
        let duration = Config::current(&app).notification_duration_ms;
        let (result, picked) = match FolderTarget::from_pick_action(&action) {
          Some(target) => picked_folder(target, pick_folder(app, target).await),
          None => (perform_action(app, &action).await, None),
        };
        let operation_succeeded = result.is_ok();
        let result = settings_action_result(&action, result);
        if let Err(error) = slint::invoke_from_event_loop(move || {
          finish_action(generation, action, duration, operation_succeeded, result);
          // Ordinary edit path: the choice queues and saves like any other control,
          // and a template draft being corrected survives it.
          if let Some(edit) = picked {
            receive_edit(generation, Ok(edit));
          }
        }) {
          tracing::error!(%error, "Cannot deliver native settings action result");
        }
      });
    },
  }
}

fn finish_action(
  generation: u64,
  action: String,
  duration: u64,
  operation_succeeded: bool,
  result: Result<Option<Config>, String>,
) {
  if let Some((window, controller)) = matching(generation) {
    controller.borrow_mut().action_running = false;
    window.global::<Interaction>().set_editable(true);
    if action == "download-update" && result.is_ok() {
      window
        .global::<SettingsState>()
        .set_update_version("".into());
    }
    if action == "notification" && result.is_ok() {
      let started = PREVIEW.with(|preview| {
        preview
          .borrow_mut()
          .succeeded(std::time::Instant::now(), duration)
      });
      if let Err(error) = started {
        display_action_error(&window, &action, error);
      }
      update_preview(generation);
    }
    let action_succeeded = result.is_ok();
    if action == "copy-diagnostics" && operation_succeeded {
      let state = window.global::<SettingsState>();
      state
        .set_diagnostics_copy_generation(state.get_diagnostics_copy_generation().wrapping_add(1));
    }
    if action == "reset" {
      window.set_confirming_reset(false);
    }
    match result {
      Ok(Some(config)) => {
        // A successful import/reset replaces the entire settings draft.
        // Cancelled or failed actions retain it for correction.
        {
          let mut owner = controller.borrow_mut();
          let Controller {
            invalid_filename,
            invalid_subfolder,
            session,
            ..
          } = &mut *owner;
          clear_replaced_template_drafts(invalid_filename, invalid_subfolder, session);
        }
        let state = window.global::<SettingsState>();
        state.set_filename_error("".into());
        state.set_subfolder_error("".into());
        state.set_focus_invalid(0);
        if let Err(error) = refresh(&window, &config) {
          display_error(&window, &error);
        }
      },
      Ok(None) => {},
      Err(error) => display_action_error(&window, &action, &error),
    }
    let close_after_action = controller.borrow().close_after_action;
    controller.borrow_mut().close_after_action = false;
    if close_after_action && !action_succeeded && !QUITTING.load(Ordering::Acquire) {
      controller.borrow().close_deadline.stop();
      window.global::<SettingsState>().set_close_pending(false);
      window.global::<SettingsState>().set_close_timeout(false);
    }
    if QUITTING.load(Ordering::Acquire) || (close_after_action && action_succeeded) {
      close_requested(generation);
    }
  }
}

// A link handed to the browser applies nothing and has no control to report beside.
// Complete it normally so it cannot block closing; the log keeps the reason.
fn settings_action_result(
  action: &str,
  result: Result<Option<Config>, String>,
) -> Result<Option<Config>, String> {
  match result {
    Err(reason)
      if matches!(
        action,
        "developer-blog" | "feedback" | "bug" | "rate" | "privacy" | "download-update"
      ) =>
    {
      tracing::info!(action, %reason, "Optional settings action did not complete");
      Ok(None)
    },
    result => result,
  }
}

fn picked_folder(
  target: FolderTarget,
  result: Result<Option<String>, String>,
) -> (Result<Option<Config>, String>, Option<PreferenceEdit>) {
  match result {
    Ok(folder) => (
      Ok(None),
      folder.map(|folder| PreferenceEdit::SaveFolder(target, Some(folder))),
    ),
    Err(error) => (Err(error), None),
  }
}

async fn pick_folder(app: AppHandle, target: FolderTarget) -> Result<Option<String>, String> {
  if is_probe() {
    return Ok(None);
  }
  crate::commands::config::pick_capture_folder(app, target).await
}

async fn perform_action(app: AppHandle, action: &str) -> Result<Option<Config>, String> {
  #[cfg(debug_assertions)]
  if is_probe() && action == "probe-action" {
    tokio::time::sleep(Duration::from_millis(200)).await;
    return Ok(None);
  }
  match action {
    "download-update" => {
      if cfg!(feature = "msix") || !Config::current(&app).check_for_updates {
        return Err("Update downloads are not enabled for this installation".into());
      }
      if !is_probe() {
        crate::commands::open_path(app, super::updates::RELEASES_URL.into()).await?;
      }
    },
    "reset" => return crate::commands::config::reset_config(app).await.map(Some),
    "import" => {
      let config = match crate::commands::config::import_config(app.clone()).await {
        Ok(config) => config,
        Err(error) if error == crate::commands::cancelled() => return Ok(None),
        Err(error) => return Err(error),
      };
      return Ok(Some(config));
    },
    "export" => match crate::commands::config::export_config(app).await {
      Err(error) if error == crate::commands::cancelled() => {},
      result => result?,
    },
    "logs" => crate::commands::open_support_logs().await?,
    "open-screenshots" | "open-videos" => {
      let config = Config::current(&app);
      let capture = if action == "open-videos" {
        CaptureType::Video
      } else {
        CaptureType::Screenshot
      };
      let path = config
        .resolve_capture_path(capture)
        .to_string_lossy()
        .into_owned();
      if is_probe() {
        crate::commands::ensure_absolute_non_unc_path(&path, "native settings folder probe")?;
      } else {
        crate::commands::open_capture_folder(app, path).await?;
      }
    },
    "copy-diagnostics" => {
      let text = collect_diagnostics(app.clone()).await?.copy;
      crate::app::runtime::spawn_blocking(move || {
        let state = app
          .try_state::<crate::state::ClipboardState>()
          .ok_or_else(|| "Clipboard service unavailable".to_owned())?;
        let mut clipboard = state
          .0
          .lock()
          .map_err(|error| crate::commands::ipc_display_context("Lock clipboard", error))?;
        clipboard
          .set_text(text)
          .map_err(|error| crate::commands::ipc_context("Copy support diagnostics", error))
      })
      .await
      .map_err(|error| crate::commands::ipc_context("Copy support diagnostics worker", error))??;
    },
    "rate" => {
      let identity: serde_json::Value =
        serde_json::from_str(include_str!("../../resources/store-identity.json"))
          .map_err(|error| crate::commands::ipc_context("Read Store identity", error))?;
      let id = identity
        .get("storeId")
        .and_then(serde_json::Value::as_str)
        .ok_or("Store product identity is missing")?;
      crate::commands::open_path(app, format!("ms-windows-store://review/?ProductId={id}")).await?;
    },
    "developer-blog" => crate::commands::open_path(app, "https://vulptik.dev".into()).await?,
    "feedback" => {
      crate::commands::open_path(
        app,
        "https://github.com/Vulptik/flinch/issues/new?template=feature_request.yml".into(),
      )
      .await?
    },
    "bug" => {
      crate::commands::open_path(
        app,
        "https://github.com/Vulptik/flinch/issues/new?template=bug_report.yml".into(),
      )
      .await?
    },
    "privacy" => {
      crate::commands::open_path(app, "https://flinch.vulptik.dev/privacy".into()).await?
    },
    "notification" => {
      let config = Config::current(&app);
      if !config.notifications_enabled {
        return Err(crate::i18n::native_settings(
          &config.language,
          "notifications_test_error",
        )?);
      }
      let title = crate::i18n::native_settings(&config.language, "notifications_test_title")?;
      let body = crate::i18n::native_settings(&config.language, "notifications_test_body")?;
      if !is_probe() {
        crate::commands::show_notification(app, title, body).await?;
      }
    },
    _ => {
      return Err(format!(
        "Native settings action is not available yet: {action}"
      ));
    },
  }
  Ok(None)
}

struct Diagnostics {
  windows: String,
  graphics: String,
  graphics_adapters: Vec<(String, String)>,
  displays: String,
  display_details: Vec<(String, String)>,
  copy: String,
}

async fn collect_diagnostics(app: AppHandle) -> Result<Diagnostics, String> {
  let language = Config::current(&app).language;
  let unavailable = crate::i18n::native_settings(&language, "about_unavailable")?;
  let info = crate::commands::get_support_diagnostics(app.clone()).await?;
  let monitors = crate::app::runtime::spawn_blocking(crate::monitors::list_monitors)
    .await
    .map_err(|error| crate::commands::ipc_context("Display diagnostics worker", error))?;
  format_diagnostics(&language, &unavailable, &info, monitors)
}

fn format_diagnostics(
  language: &str,
  unavailable: &str,
  info: &serde_json::Value,
  monitors: Vec<crate::monitors::MonitorInfo>,
) -> Result<Diagnostics, String> {
  let windows = info
    .get("windowsBuild")
    .and_then(serde_json::Value::as_str)
    .ok_or("Windows diagnostics are missing")?
    .to_owned();
  let mut graphics_adapters = Vec::new();
  let graphics = if let Some(error) = info
    .pointer("/graphics/error")
    .and_then(serde_json::Value::as_str)
  {
    error.to_owned()
  } else {
    let gpus = info
      .pointer("/graphics/gpus")
      .and_then(serde_json::Value::as_array)
      .ok_or("Graphics diagnostics are missing")?;
    gpus
      .iter()
      .map(|gpu| {
        let name = gpu
          .get("name")
          .and_then(serde_json::Value::as_str)
          .unwrap_or(unavailable);
        let driver = gpu
          .get("driver")
          .and_then(serde_json::Value::as_str)
          .or_else(|| gpu.get("driverError").and_then(serde_json::Value::as_str))
          .unwrap_or(unavailable);
        graphics_adapters.push((name.to_owned(), driver.to_owned()));
        format!("{name} · {driver}")
      })
      .collect::<Vec<_>>()
      .join("\n")
  };
  let mut display_details = Vec::new();
  let displays = monitors
    .into_iter()
    .map(|monitor| {
      let hdr = info
        .pointer("/graphics/outputs")
        .and_then(serde_json::Value::as_array)
        .and_then(|outputs| {
          outputs.iter().find(|output| {
            output.get("x").and_then(serde_json::Value::as_i64) == Some(i64::from(monitor.x))
              && output.get("y").and_then(serde_json::Value::as_i64) == Some(i64::from(monitor.y))
          })
        });
      let status = match hdr
        .and_then(|output| output.get("hdr"))
        .and_then(serde_json::Value::as_bool)
      {
        Some(true) => crate::i18n::native_settings(language, "about_hdr_enabled"),
        Some(false) => crate::i18n::native_settings(language, "about_hdr_disabled"),
        None => Ok(
          hdr
            .and_then(|output| output.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(unavailable)
            .to_owned(),
        ),
      }?;
      let summary = format!(
        "{} × {} · {}%",
        monitor.width,
        monitor.height,
        (monitor.dpi_scale * 100.0).round()
      );
      display_details.push((summary.clone(), status.clone()));
      Ok::<_, String>(format!("{summary} · HDR: {status}"))
    })
    .collect::<Result<Vec<_>, _>>()?
    .join("\n");
  let version = info
    .get("version")
    .and_then(serde_json::Value::as_str)
    .ok_or("Version diagnostics are missing")?;
  let channel = info
    .get("channel")
    .and_then(serde_json::Value::as_str)
    .ok_or("Channel diagnostics are missing")?;
  let installation = if channel == "msix" {
    "MSIX".to_owned()
  } else {
    crate::i18n::native_settings(language, "about_installation_installer")?
  };
  let mut copy = format!("Flinch {version}");
  for (label, value) in [
    ("about_windows", windows.as_str()),
    ("about_channel", installation.as_str()),
    ("about_gpu", graphics.as_str()),
    ("about_displays", displays.as_str()),
  ] {
    copy.push_str(&format!(
      "\n{}: {value}",
      crate::i18n::native_settings(language, label)?
    ));
  }
  Ok(Diagnostics {
    windows,
    graphics,
    graphics_adapters,
    displays,
    display_details,
    copy,
  })
}

fn load_diagnostics(generation: u64, app: AppHandle) {
  crate::app::runtime::spawn(async move {
    let result = tokio::time::timeout(Duration::from_secs(5), collect_diagnostics(app))
      .await
      .map_err(|_| "Support diagnostics exceeded the five-second deadline".to_owned())
      .and_then(|result| result);
    if let Err(error) = slint::invoke_from_event_loop(move || {
      finish_diagnostics_result(generation, result);
    }) {
      tracing::error!(%error, "Cannot deliver native diagnostics result");
    }
  });
}

pub(crate) fn request_exit(app: &AppHandle) -> Result<(), String> {
  if QUITTING.swap(true, Ordering::AcqRel) {
    return Ok(());
  }
  let exit_generation = EXIT_GENERATION
    .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
      value.checked_add(1)
    })
    .map_err(|_| {
      QUITTING.store(false, Ordering::Release);
      "Shutdown generation exhausted".to_owned()
    })?;
  let app = app.clone();
  slint::invoke_from_event_loop(move || {
    if let Some((_, controller)) = current() {
      let generation = controller.borrow().generation;
      close_requested(generation);
    } else {
      finish_exit(&app);
    }
    let deadline_app = app.clone();
    slint::Timer::single_shot(Duration::from_secs(10), move || {
      if !QUITTING.load(Ordering::Acquire)
        || EXIT_GENERATION.load(Ordering::Acquire) != exit_generation + 1
      {
        return;
      }
      tracing::error!("Explicit quit reached its ten-second settings shutdown budget");
      finish_exit(&deadline_app);
    });
  })
  .map_err(|error| {
    QUITTING.store(false, Ordering::Release);
    error.to_string()
  })
}

fn finish_exit(app: &AppHandle) {
  if let Some(state) = app.try_state::<crate::state::AppExitState>() {
    state.allow_exit();
  }
  app.exit(0);
  if let Err(error) = slint::quit_event_loop() {
    tracing::error!(%error, "Cannot stop native settings loop");
  }
}

fn finish_capture_result(
  generation: u64,
  capture_action: Option<ShortcutAction>,
  config: Config,
  failed_edit: Option<PreferenceEdit>,
  result: Result<(), String>,
) {
  if let Some((window, controller)) = matching(generation) {
    controller.borrow_mut().capture_busy = false;
    if let Some(edit) = failed_edit {
      match text(&window, "native_setting_save_error") {
        Ok(message) => field_error(&window, edit.slot(), &message),
        Err(error) => tracing::error!(%error, "Missing shortcut save failure message"),
      }
      let retained = (|| -> Result<(), &'static str> {
        let mut controller = controller.borrow_mut();
        controller.session.edit(edit)?;
        let request = controller
          .session
          .start()?
          .ok_or("Shortcut draft could not start")?;
        if !controller.session.failed(request) {
          return Err("Shortcut draft failure acknowledgment was stale");
        }
        Ok(())
      })();
      if let Err(error) = retained {
        tracing::error!(%error, "Cannot retain failed shortcut draft");
      }
      window
        .global::<SettingsState>()
        .set_save_error(result.as_ref().err().cloned().unwrap_or_default().into());
      window.global::<SettingsState>().set_save_failed(true);
    }
    window.global::<Interaction>().set_editable(true);
    if let Err(error) = refresh(&window, &config) {
      display_error(&window, &error);
    }
    if let Err(error) = result {
      if let Some(action) = capture_action {
        let slot = PreferenceEdit::Shortcut(action, String::new()).slot();
        use slint::Model;
        if window
          .global::<SettingsState>()
          .get_field_errors()
          .row_data(slot)
          .is_some_and(|message| message.is_empty())
        {
          shortcut_error(&window, action, &error);
        } else {
          tracing::warn!(%error, "Shortcut operation failed; inline save error retained");
        }
      } else {
        display_error(&window, &error);
      }
    }
    let close = controller.borrow().close_after_capture;
    if close {
      close_requested(generation);
    }
  }
}

fn finish_update_result(generation: u64, app: &AppHandle, result: anyhow::Result<Option<String>>) {
  if let Some((window, _)) = matching(generation) {
    if !window.global::<SettingsState>().get_updates() || !Config::current(app).check_for_updates {
      return;
    }
    match result {
      Ok(Some(version)) => window
        .global::<SettingsState>()
        .set_update_version(version.into()),
      Ok(None) => {},
      Err(error) => {
        // Background work the user did not ask for: being offline is not their problem.
        tracing::warn!(%error, "Native version check failed");
      },
    }
  }
}

fn finish_diagnostics_result(generation: u64, result: Result<Diagnostics, String>) {
  if let Some((window, _)) = matching(generation) {
    match result {
      Ok(info) => {
        let state = window.global::<SettingsState>();
        state.set_windows_build(info.windows.into());
        state.set_graphics(info.graphics.into());
        state.set_graphics_adapters(slint::ModelRc::new(slint::VecModel::from(
          info
            .graphics_adapters
            .into_iter()
            .map(|(name, driver)| ui::GraphicsAdapter {
              name: name.into(),
              driver: driver.into(),
            })
            .collect::<Vec<_>>(),
        )));
        state.set_displays(info.displays.into());
        state.set_display_details(slint::ModelRc::new(slint::VecModel::from(
          info
            .display_details
            .into_iter()
            .map(|(summary, hdr)| ui::DisplayDetails {
              summary: summary.into(),
              hdr: hdr.into(),
            })
            .collect::<Vec<_>>(),
        )));
      },
      Err(error) => {
        // The About rows already read Unavailable.
        tracing::warn!(%error, "Native settings diagnostics unavailable");
      },
    }
  }
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod template_error_tests {
  use super::{CloseDecision, EditSession, clear_replaced_template_drafts, template_error_parts};

  #[test]
  fn every_visible_action_has_its_own_plain_language_message() {
    for locale in flinch_settings::model::LOCALES {
      for action in [
        "import",
        "export",
        "reset",
        "notification",
        "open-screenshots",
        "open-videos",
        "logs",
        "copy-diagnostics",
        "pick-folder-together",
      ] {
        let key = super::action_error_key(action);
        assert_ne!(key, "native_action_error", "{action}");
        let message = crate::i18n::native_settings(locale, key).expect("localized message");
        assert!(
          !message.to_lowercase().contains("log "),
          "{locale}/{action}"
        );
      }
    }
  }

  #[test]
  fn only_a_chosen_folder_becomes_an_edit() {
    use flinch_settings::edit::{FolderTarget, PreferenceEdit};
    let target = FolderTarget::Videos;
    let (result, edit) = super::picked_folder(target, Ok(Some(r"D:\Clips".into())));
    assert!(matches!(result, Ok(None)));
    assert_eq!(
      edit,
      Some(PreferenceEdit::SaveFolder(target, Some(r"D:\Clips".into())))
    );
    let (result, edit) = super::picked_folder(target, Ok(None));
    assert!(matches!(result, Ok(None)) && edit.is_none());
    let (result, edit) = super::picked_folder(target, Err("not writable".into()));
    assert!(result.is_err() && edit.is_none());
  }

  #[test]
  fn optional_action_failures_complete_quietly_but_settings_failures_remain_errors() {
    for action in ["feedback", "bug", "rate", "privacy", "download-update"] {
      assert!(
        matches!(
          super::settings_action_result(action, Err("OS handoff failed".into())),
          Ok(None)
        ),
        "{action}"
      );
    }
    for action in [
      "import",
      "export",
      "reset",
      "notification",
      "open-screenshots",
      "open-videos",
      "logs",
      "copy-diagnostics",
    ] {
      assert_eq!(
        super::settings_action_result(action, Err("Action failed".into())).err(),
        Some("Action failed".into()),
        "{action}"
      );
    }
  }

  #[test]
  fn successful_configuration_replacement_removes_invalid_close_block() {
    let mut filename = Some("bad:name".into());
    let mut subfolder = Some("bad/folder".into());
    let mut session = EditSession::new(1);
    session.set_validation_blocked(true);
    assert_eq!(session.request_close(), CloseDecision::ResolveFailure);
    session.cancel_close();
    clear_replaced_template_drafts(&mut filename, &mut subfolder, &mut session);
    assert!(filename.is_none() && subfolder.is_none());
    assert_eq!(session.request_close(), CloseDecision::Close);
  }

  #[test]
  fn messages_identify_the_actual_problem_and_reserved_name() {
    for (error, expected) in [
      ("Template cannot be empty.", ("native_template_empty", "")),
      (
        "Template produces an empty filename.",
        ("native_template_name", ""),
      ),
      (
        "Template produces an empty folder name.",
        ("native_template_name", ""),
      ),
      (
        "Control characters are not allowed in Windows filenames.",
        ("native_template_control", ""),
      ),
      (
        "Filename cannot end with a dot.",
        ("native_template_edge", ""),
      ),
      (
        "\"CON.txt\" is a reserved Windows folder name.",
        ("native_template_reserved", "CON.txt"),
      ),
      (
        "Character ':' is not allowed in Windows filenames.",
        ("native_template_character", ":"),
      ),
    ] {
      assert_eq!(template_error_parts(error), Ok(expected));
    }
  }
}
