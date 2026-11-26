#![windows_subsystem = "windows"]

use flinch_installer::{Phase, State, motion};
use slint::ComponentHandle;
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, AtomicUsize, Ordering},
};

slint::include_modules!();

#[path = "../../../windows/slint_chrome.rs"]
mod chrome;

unsafe extern "C" {
  fn flinch_burn_run(
    callback: unsafe extern "C" fn(*mut std::ffi::c_void, i32, i32, i32),
    context: *mut std::ffi::c_void,
  ) -> i32;
  fn flinch_burn_command(operation: i32) -> i32;
  fn flinch_burn_set_parent(window: *mut std::ffi::c_void) -> i32;
  fn flinch_burn_set_options(scope: i32, folder: *const u16, start_menu: i32, desktop: i32) -> i32;
}

struct Shared {
  state: Mutex<State>,
  pending: AtomicBool,
  ui: slint::Weak<InstallerWindow>,
  catalogs: serde_json::Value,
  language: AtomicUsize,
}

const LANGUAGES: [&str; 5] = ["en", "fr", "de", "es", "it"];
impl Shared {
  fn copy(&self) -> &serde_json::Value {
    &self.catalogs[LANGUAGES[self.language.load(Ordering::Acquire)]]
  }
}

fn word(copy: &serde_json::Value, key: &str) -> slint::SharedString {
  copy[key]
    .as_str()
    .unwrap_or("Missing installer translation")
    .into()
}

fn present(ui: &InstallerWindow, state: &State, copy: &serde_json::Value) {
  ui.set_version(env!("CARGO_PKG_VERSION").into());
  ui.set_window_title(word(copy, "setup"));
  ui.set_minimize_label(word(copy, "minimize"));
  ui.set_window_close_label(word(copy, "close"));
  ui.set_language_label(word(copy, "language"));
  ui.set_language_names(slint::ModelRc::new(slint::VecModel::from(
    [
      "language_en",
      "language_fr",
      "language_de",
      "language_es",
      "language_it",
    ]
    .map(|key| word(copy, key))
    .to_vec(),
  )));
  let busy = state.busy();
  let fresh = state.phase == Phase::Ready && !state.installed && !state.upgrade;
  let optional = state.optional_step && !busy;
  ui.set_busy(busy);
  ui.set_maintenance(state.installed && !state.removal_confirmation && !optional);
  ui.set_finished(matches!(state.phase, Phase::Complete | Phase::Cancelled));
  ui.set_failed(state.phase == Phase::Failed && !optional);
  ui.set_progress(state.progress);
  ui.set_show_options(fresh || optional);
  ui.set_show_preservation(
    state.removal_confirmation || (state.operation == 4 && state.phase == Phase::Complete),
  );
  ui.set_optional_error(optional && state.status < 0);
  ui.set_wizard_page(if optional {
    2
  } else if fresh {
    i32::from(state.folder_step)
  } else {
    -1
  });
  ui.set_just_me_label(word(copy, "just_me"));
  ui.set_everyone_label(word(copy, "everyone"));
  ui.set_folder_label(word(copy, "folder"));
  ui.set_browse_label(word(copy, "browse"));
  ui.set_start_menu_label(word(copy, "start_menu"));
  ui.set_desktop_label(word(copy, "desktop"));
  ui.set_launch_label(word(copy, "launch"));
  ui.set_launch_enabled(!state.restart);
  if state.restart {
    ui.set_launch_app(false);
  }
  ui.set_user_description(word(copy, "user_scope_hint"));
  ui.set_machine_description(word(copy, "machine_scope_hint"));
  if state.existing_scope != 0 {
    ui.set_install_scope(if state.existing_scope == 1 { 1 } else { 0 });
    ui.set_scope_locked(true);
  }
  ui.set_elevation_label(word(
    copy,
    if ui.get_install_scope() == 1 {
      "machine_scope_hint"
    } else {
      "user_scope_hint"
    },
  ));
  let primary = if optional {
    if state.status < 0 { "retry" } else { "finish" }
  } else if state.removal_confirmation {
    "uninstall"
  } else if state.upgrade {
    "update"
  } else {
    "install"
  };
  ui.set_primary_label(word(copy, primary));
  ui.set_finish_label(word(copy, "finish"));
  ui.set_retry_label(word(copy, "retry"));
  ui.set_primary_operation(if optional {
    7
  } else if state.removal_confirmation {
    4
  } else if state.installed {
    8
  } else if fresh && !state.folder_step {
    11
  } else {
    6
  });
  if fresh && !state.folder_step {
    ui.set_primary_label(word(copy, "next"));
  }
  ui.set_repair_label(word(copy, "repair"));
  ui.set_remove_label(word(copy, "uninstall"));
  ui.set_close_label(word(
    copy,
    if busy {
      "cancel"
    } else if state.removal_confirmation || (fresh && state.folder_step) {
      "back"
    } else if fresh {
      "cancel"
    } else {
      "close"
    },
  ));
  ui.set_preservation(word(copy, "preservation"));
  let heading = if optional {
    "shortcuts_heading"
  } else if state.removal_confirmation {
    "uninstall_heading"
  } else {
    match state.phase {
      Phase::Detecting => "detecting",
      Phase::Planning => "planning",
      Phase::Applying => match state.operation {
        4 => "uninstalling",
        8 => "repairing",
        7 => "shortcuts_applying",
        _ => "installing",
      },
      Phase::Complete => match state.operation {
        4 => "uninstalled",
        8 => "repaired",
        7 => "shortcuts_complete",
        _ => "installed",
      },
      Phase::Failed => match state.operation {
        4 => "uninstall_failed",
        8 => "repair_failed",
        _ => "failed",
      },
      Phase::Cancelled => "cancelled",
      _ if state.installed => "maintenance_heading",
      _ if state.upgrade => "update_heading",
      _ if fresh && state.folder_step => "where_heading",
      _ if fresh => "scope_heading",
      _ => "install_heading",
    }
  };
  ui.set_heading(word(
    copy,
    if busy && state.canceling {
      "cancelling"
    } else {
      heading
    },
  ));
  ui.set_stage(if busy && state.canceling {
    word(copy, "canceling")
  } else if busy && state.stage == 1 {
    word(copy, "runtime")
  } else {
    "".into()
  });
  let detail = if let Some(error) = &state.launch_error {
    format!("{}\n{error}", word(copy, "launch_failed")).into()
  } else if optional {
    word(
      copy,
      if state.status < 0 {
        "shortcuts_failed"
      } else if state.restart {
        "shortcuts_restart"
      } else {
        "shortcuts_detail"
      },
    )
  } else if state.removal_confirmation {
    word(copy, "uninstall_detail")
  } else if state.phase == Phase::Complete && state.restart {
    word(copy, "restart")
  } else if state.phase == Phase::Ready && (state.installed || state.upgrade) {
    let location = format!(
      "{} · {}",
      word(
        copy,
        if ui.get_install_scope() == 1 {
          "everyone"
        } else {
          "just_me"
        }
      ),
      ui.get_install_directory()
    );
    if state.installed {
      format!("{}\n{location}", word(copy, "maintenance_detail")).into()
    } else {
      location.into()
    }
  } else {
    "".into()
  };
  ui.set_detail(detail);
  ui.set_error_detail(match flinch_installer::failure_copy(state.status) {
    Some(key) => word(copy, key),
    None => format!(
      "{}  0x{:08X}",
      word(copy, "error_hint"),
      state.status as u32
    )
    .into(),
  });
}

fn connect_options(ui: &InstallerWindow, shared: &Arc<Shared>) -> Result<(), String> {
  use flinch_installer::options::native;
  let choices = native::load()?;
  ui.set_install_scope(if choices.machine { 1 } else { 0 });
  ui.set_install_directory(choices.directory.into());
  ui.set_start_menu(choices.start_menu);
  ui.set_desktop_shortcut(choices.desktop);
  ui.set_scope_locked(choices.locked);
  let weak = ui.as_weak();
  let copy_scope = shared.clone();
  ui.on_scope_changed(move || {
    if let Some(ui) = weak.upgrade() {
      ui.set_elevation_label(word(
        copy_scope.copy(),
        if ui.get_install_scope() == 1 {
          "machine_scope_hint"
        } else {
          "user_scope_hint"
        },
      ));
      if !ui.get_scope_locked() {
        match native::default_directory(ui.get_install_scope() == 1) {
          Ok(path) => {
            ui.set_install_directory(path.into());
            ui.set_options_error("".into());
          },
          Err(error) => {
            eprintln!("{error}");
            ui.set_options_error(word(copy_scope.copy(), "folder_error"));
          },
        }
      }
    }
  });
  let weak = ui.as_weak();
  let copy_browse = shared.clone();
  ui.on_browse_folder(move || {
    if let Some(ui) = weak.upgrade() {
      let result = native_window(&ui)
        .and_then(|parent| native::browse(parent, &word(copy_browse.copy(), "folder")));
      match result {
        Ok(Some(path)) => {
          ui.set_install_directory(path.into());
          ui.set_options_error("".into());
        },
        Ok(None) => {},
        Err(error) => {
          eprintln!("{error}");
          ui.set_options_error(word(copy_browse.copy(), "folder_error"));
        },
      }
    }
  });
  Ok(())
}

fn connect_language(ui: &InstallerWindow, shared: &Arc<Shared>) {
  ui.set_language_index(shared.language.load(Ordering::Acquire) as i32);
  let shared = shared.clone();
  ui.on_language_selected(move |index| {
    let Ok(index) = usize::try_from(index) else {
      return;
    };
    if index >= LANGUAGES.len() {
      eprintln!("Invalid installer language index {index}");
      return;
    }
    let Some(ui) = shared.ui.upgrade() else {
      return;
    };
    let old_error = ui.get_options_error();
    let error_key = shared.copy().as_object().and_then(|copy| {
      copy
        .iter()
        .find(|(_, value)| value.as_str() == Some(old_error.as_str()))
        .map(|(key, _)| key.clone())
    });
    shared.language.store(index, Ordering::Release);
    if let Some(key) = error_key {
      ui.set_options_error(word(shared.copy(), &key));
    }
    let snapshot = shared.state.lock().map(|state| state.clone());
    match snapshot {
      Ok(state) => present(&ui, &state, shared.copy()),
      Err(error) => eprintln!("Installer language state unavailable: {error}"),
    }
  });
}

fn submit_options(ui: &InstallerWindow) -> Result<(), String> {
  let folder = ui.get_install_directory();
  flinch_installer::options::validate_directory(&folder)?;
  let wide: Vec<u16> = folder.encode_utf16().chain(Some(0)).collect();
  // SAFETY: all choices are validated; the terminated folder lives through the synchronous copy.
  let status = unsafe {
    flinch_burn_set_options(
      if ui.get_install_scope() == 1 { 1 } else { 2 },
      wide.as_ptr(),
      i32::from(ui.get_start_menu()),
      i32::from(ui.get_desktop_shortcut()),
    )
  };
  if status < 0 {
    return Err(format!(
      "Configure installer options: 0x{:08X}",
      status as u32
    ));
  }
  Ok(())
}

unsafe extern "C" fn notify(context: *mut std::ffi::c_void, kind: i32, value: i32, detail: i32) {
  // SAFETY: the engine worker retains this Arc until flinch_burn_run has returned,
  // after the SDK has disconnected every native callback.
  let shared = unsafe { &*(context as *const Arc<Shared>) };
  match shared.state.lock() {
    Ok(mut state) => {
      if let Err(error) = state.apply_event(kind, value, detail) {
        eprintln!("Installer event rejected: {error}");
        state.phase = Phase::Failed;
        state.status = -1;
      }
    },
    Err(error) => {
      eprintln!("Installer state lock failed: {error}");
      return;
    },
  }
  if shared.pending.swap(true, Ordering::AcqRel) {
    return;
  }
  let state = shared.clone();
  if let Err(error) = shared.ui.upgrade_in_event_loop(move |ui| {
    state.pending.store(false, Ordering::Release);
    let mut exit_command = None;
    // Release the presentation lock before native activation, window and engine calls.
    let snapshot = state.state.lock().map(|mut snapshot| {
      let activate = snapshot.take_activation_request();
      (snapshot.clone(), activate)
    });
    match snapshot {
      Ok((snapshot, activate)) => {
        present(&ui, &snapshot, state.copy());
        if snapshot.phase == Phase::Closed {
          if let Err(error) = slint::quit_event_loop() {
            eprintln!("Installer UI shutdown failed: {error}");
          }
        } else if snapshot.phase == Phase::Cancelled && snapshot.close_after_cancel {
          exit_command = Some(0);
        } else if snapshot.display {
          let result = (|| -> Result<(), String> {
            match ui.invoke_acquire_instance() {
              1 => {},
              0 => {
                exit_command = Some(0);
                return Ok(());
              },
              _ => return Err("Cannot activate the existing installer instance".into()),
            }
            if !ui.get_placement_ready() {
              flinch_installer::placement::center(ui.window())?;
              apply_window_branding(native_window(&ui)?)?;
              ui.set_placement_ready(true);
            }
            ui.show()
              .map_err(|error| format!("Open installer window: {error}"))?;
            let parent = native_window(&ui)?;
            // SAFETY: the visible Slint HWND remains alive until Burn has disconnected.
            let status = unsafe { flinch_burn_set_parent(parent.0) };
            if status < 0 {
              return Err(format!(
                "Register installer parent window: 0x{:08X}",
                status as u32
              ));
            }
            ui.invoke_start_intro();
            if activate && let Err(error) = activate_installer(&ui) {
              eprintln!("Restore installer after elevation failed: {error}");
            }
            Ok(())
          })();
          if let Err(error) = result {
            eprintln!("Installer window could not open: {error}");
            exit_command = Some(9);
          }
        }
        if let Err(error) = finish_installer(&ui, &state) {
          eprintln!("Finish installer failed: {error}");
        }
      },
      Err(error) => eprintln!("Installer presentation lock failed: {error}"),
    }
    if let Some(command) = exit_command {
      // SAFETY: cancel outside the presentation lock; the bridge validates engine lifetime.
      let status = unsafe { flinch_burn_command(command) };
      if status < 0 {
        eprintln!(
          "Installer display failure cancellation failed: 0x{:08X}",
          status as u32
        );
      }
    }
  }) {
    shared.pending.store(false, Ordering::Release);
    eprintln!("Installer event-loop delivery failed: {error}");
    // SAFETY: the bridge checks engine lifetime and serializes command submission.
    let status = unsafe { flinch_burn_command(9) };
    if status < 0 {
      eprintln!("Installer cancellation failed: 0x{:08X}", status as u32);
    }
  }
}

fn finish_installer(ui: &InstallerWindow, shared: &Shared) -> Result<(), String> {
  let action = shared
    .state
    .lock()
    .map_err(|error| format!("Read installer finish state: {error}"))?
    .take_finish_action();
  let Some(launch) = action else {
    return Ok(());
  };
  if launch {
    let folder = ui.get_install_directory();
    let executable = std::path::Path::new(folder.as_str()).join("flinch.exe");
    // The interactive UI stays unelevated when Burn elevates its MSI engine.
    // Start the installed executable directly, without a shell or elevation verb.
    let result = (|| -> Result<(), String> {
      flinch_installer::options::validate_directory(&folder)?;
      std::process::Command::new(&executable)
        .current_dir(folder.as_str())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Start {}: {error}", executable.display()))
    })();
    if let Err(reason) = result {
      let mut state = shared
        .state
        .lock()
        .map_err(|error| format!("Record installer launch failure: {error}"))?;
      state.launch_failed(reason.clone());
      present(ui, &state, shared.copy());
      return Err(reason);
    }
  }
  // SAFETY: no transaction remains active; the bridge retains the engine.
  let status = unsafe { flinch_burn_command(0) };
  if status < 0 {
    return Err(format!("Close finished installer: 0x{:08X}", status as u32));
  }
  Ok(())
}

fn native_window(ui: &InstallerWindow) -> Result<windows::Win32::Foundation::HWND, String> {
  use raw_window_handle::{HasWindowHandle, RawWindowHandle};
  use slint::winit_030::WinitWindowAccessor;
  ui.window()
    .with_winit_window(|window| {
      match window
        .window_handle()
        .map_err(|error| format!("Read installer HWND: {error}"))?
        .as_raw()
      {
        RawWindowHandle::Win32(handle) => {
          Ok(windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _))
        },
        _ => Err("Installer backend did not create a Win32 window".into()),
      }
    })
    .ok_or("Installer native window is not initialized")?
}

fn apply_window_branding(window: windows::Win32::Foundation::HWND) -> Result<(), String> {
  use windows::{
    Win32::{
      Foundation::{HINSTANCE, LPARAM, WPARAM},
      System::LibraryLoader::GetModuleHandleW,
      UI::{
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        WindowsAndMessaging::{
          ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED, LoadImageW, SM_CXICON, SM_CXSMICON,
          SM_CYICON, SM_CYSMICON, SendMessageW, WM_SETICON,
        },
      },
    },
    core::PCWSTR,
  };
  // SAFETY: owner-thread call for the live installer HWND. Resource 1 is the
  // compiled Flinch ICO; LR_SHARED keeps both icons alive for the process.
  unsafe {
    let module =
      GetModuleHandleW(None).map_err(|error| format!("Read installer module: {error}"))?;
    let dpi = GetDpiForWindow(window);
    for (kind, width, height) in [
      (ICON_BIG, SM_CXICON, SM_CYICON),
      (ICON_SMALL, SM_CXSMICON, SM_CYSMICON),
    ] {
      let icon = LoadImageW(
        Some(HINSTANCE(module.0)),
        PCWSTR(std::ptr::without_provenance(1)),
        IMAGE_ICON,
        GetSystemMetricsForDpi(width, dpi),
        GetSystemMetricsForDpi(height, dpi),
        LR_SHARED,
      )
      .map_err(|error| format!("Load Flinch installer icon: {error}"))?;
      SendMessageW(
        window,
        WM_SETICON,
        Some(WPARAM(kind as usize)),
        Some(LPARAM(icon.0 as isize)),
      );
    }
  }
  Ok(())
}

fn connect_instance(ui: &InstallerWindow) {
  use flinch_installer::instance::Instance;
  let owner = std::cell::RefCell::new(None::<Instance>);
  let outcome = std::cell::Cell::new(None::<i32>);
  let weak = ui.as_weak();
  ui.on_acquire_instance(move || {
    if let Some(outcome) = outcome.get() {
      return outcome;
    }
    let existing = weak.clone();
    let result = Instance::acquire_named(
      "io.github.vulptik.flinch.installer",
      move || {
        if let Err(error) = existing.upgrade_in_event_loop(|ui| {
          if let Err(error) = activate_installer(&ui) {
            eprintln!("Activate installer failed: {error}");
          }
        }) {
          eprintln!("Installer activation delivery failed: {error}");
        }
      },
      None,
    );
    let value = match result {
      Ok(Some(instance)) => {
        *owner.borrow_mut() = Some(instance);
        1
      },
      Ok(None) => 0,
      Err(error) => {
        eprintln!("Installer instance activation failed: {error:#}");
        -1
      },
    };
    outcome.set(Some(value));
    value
  });
}

fn activate_installer(ui: &InstallerWindow) -> Result<(), String> {
  ui.window().set_minimized(false);
  ui.show()
    .map_err(|error| format!("Restore installer window: {error}"))?;
  let window = native_window(ui)?;
  // SAFETY: called on the UI owner thread with its live HWND, after user
  // elevation or a secondary process's foreground permission grant.
  if !unsafe { windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(window) }.as_bool() {
    eprintln!("Windows denied installer foreground activation; flashing its taskbar button.");
    // SAFETY: flash only this owned live window; no global input or focus tricks.
    let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::FlashWindow(window, true) };
  }
  Ok(())
}

fn apply_intro(ui: &InstallerWindow, elapsed: u64) -> Result<(), String> {
  let pose = motion::pose(elapsed);
  let svg = motion::svg(&pose)?;
  let image = slint::Image::load_from_svg_data(svg.as_bytes())
    .map_err(|error| format!("Installer entrance logo failed: {error}"))?;
  ui.set_intro_image(image);
  ui.set_logo_padded(true);
  ui.set_logo_x(pose.x);
  ui.set_logo_y(pose.y);
  ui.set_logo_size(pose.size);
  ui.set_intro_opacity(pose.content);
  ui.set_intro_content_y(pose.content_y);
  ui.set_intro_complete(elapsed >= motion::DURATION_MS);
  Ok(())
}

fn connect_intro(ui: &InstallerWindow) -> Result<(), String> {
  let final_image = ui.get_intro_image();
  use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
  };
  let mut enabled = windows::core::BOOL(0);
  // SAFETY: this read-only UI-thread query writes one initialized BOOL.
  let motion_enabled = match unsafe {
    SystemParametersInfoW(
      SPI_GETCLIENTAREAANIMATION,
      0,
      Some((&mut enabled as *mut windows::core::BOOL).cast()),
      SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
  } {
    Ok(()) => enabled.as_bool(),
    Err(error) => {
      eprintln!("Installer animation preference unavailable; entrance motion disabled: {error}");
      false
    },
  };
  apply_intro(
    ui,
    if motion_enabled {
      0
    } else {
      motion::DURATION_MS
    },
  )?;
  let started = std::rc::Rc::new(std::cell::Cell::new(false));
  let timer = std::rc::Rc::new(slint::Timer::default());
  let weak = ui.as_weak();
  ui.on_start_intro(move || {
    if started.replace(true) || !motion_enabled {
      return;
    }
    let start = std::time::Instant::now();
    let mut playback = motion::PlaybackClock::default();
    let view = weak.clone();
    let recovery_image = final_image.clone();
    let stop = std::rc::Rc::downgrade(&timer);
    timer.start(
      slint::TimerMode::Repeated,
      std::time::Duration::from_millis(16),
      move || {
        let Some(ui) = view.upgrade() else {
          if let Some(timer) = stop.upgrade() {
            timer.stop();
          }
          return;
        };
        let elapsed = playback.advance(start.elapsed().as_millis().min(u64::MAX as u128) as u64);
        let failed = if let Err(error) = apply_intro(&ui, elapsed) {
          eprintln!("{error}; entrance stopped and installer controls revealed.");
          ui.set_intro_image(recovery_image.clone());
          ui.set_logo_padded(false);
          ui.set_logo_x(32.0);
          ui.set_logo_y(32.0);
          ui.set_logo_size(36.0);
          ui.set_intro_opacity(1.0);
          ui.set_intro_content_y(0.0);
          ui.set_intro_complete(true);
          true
        } else {
          false
        };
        if (elapsed >= motion::DURATION_MS || failed)
          && let Some(timer) = stop.upgrade()
        {
          timer.stop();
        }
      },
    );
  });
  Ok(())
}

fn preview(
  directory: &std::path::Path,
  copy: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
  use slint::platform::{
    Platform, WindowAdapter,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
  };
  use std::rc::Rc;
  struct Offscreen(Rc<MinimalSoftwareWindow>);
  impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
      Ok(self.0.clone())
    }
  }
  let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
  slint::platform::set_platform(Box::new(Offscreen(window.clone())))?;
  let ui = InstallerWindow::new()?;
  let catalogs: serde_json::Value =
    serde_json::from_str(include_str!("../../../messages/installer-ui.json"))?;
  let initial_language = LANGUAGES
    .iter()
    .position(|locale| catalogs[*locale] == *copy)
    .ok_or("Missing preview language")?;
  let shared = Arc::new(Shared {
    state: Mutex::new(State::default()),
    pending: AtomicBool::new(false),
    ui: ui.as_weak(),
    catalogs,
    language: AtomicUsize::new(initial_language),
  });
  connect_language(&ui, &shared);
  let actions = Rc::new(std::cell::RefCell::new(Vec::new()));
  let sink = actions.clone();
  ui.on_action(move |action| sink.borrow_mut().push(action));
  let browsed = Rc::new(std::cell::Cell::new(0));
  let browser = browsed.clone();
  ui.on_browse_folder(move || browser.set(browser.get() + 1));
  ui.window().set_size(slint::PhysicalSize::new(640, 420));
  ui.show()?;
  std::fs::create_dir_all(directory)?;
  for (name, phase, installed, progress, confirmation, upgrade, stage) in [
    ("install", Phase::Ready, false, 0, false, false, 0),
    ("install-machine", Phase::Ready, false, 0, false, false, 0),
    ("folder", Phase::Ready, false, 0, false, false, 0),
    ("shortcuts", Phase::Complete, false, 100, false, false, 0),
    (
      "shortcuts-failed",
      Phase::Failed,
      false,
      100,
      false,
      false,
      0,
    ),
    ("maintenance", Phase::Ready, true, 0, false, false, 0),
    ("progress", Phase::Applying, false, 58, false, false, 2),
    ("repairing", Phase::Applying, true, 58, false, false, 2),
    ("uninstalling", Phase::Applying, true, 58, false, false, 2),
    ("runtime", Phase::Applying, false, 25, false, false, 1),
    (
      "shortcuts-progress",
      Phase::Applying,
      false,
      58,
      false,
      false,
      2,
    ),
    (
      "repair-complete",
      Phase::Complete,
      true,
      100,
      false,
      false,
      0,
    ),
    (
      "remove-complete",
      Phase::Complete,
      true,
      100,
      false,
      false,
      0,
    ),
    ("repair-failure", Phase::Failed, true, 0, false, false, 0),
    ("remove-failure", Phase::Failed, true, 0, false, false, 0),
    ("complete", Phase::Complete, false, 100, false, false, 0),
    ("failure", Phase::Failed, false, 0, false, false, 0),
    ("confirmation", Phase::Ready, true, 0, true, false, 0),
    ("update", Phase::Ready, false, 0, false, true, 0),
    ("cancelled", Phase::Cancelled, false, 0, false, false, 0),
  ] {
    ui.set_install_scope(i32::from(name == "install-machine"));
    ui.set_install_directory(
      if name == "install-machine" {
        "C:\\Program Files\\Flinch"
      } else {
        "C:\\Users\\Test\\AppData\\Local\\Programs\\Flinch"
      }
      .into(),
    );
    let fixture = State {
      phase,
      folder_step: name == "folder",
      optional_step: name.starts_with("shortcuts"),
      installed,
      progress,
      removal_confirmation: confirmation,
      upgrade,
      stage,
      status: if name.contains("failure") || name == "shortcuts-failed" {
        -2147023293
      } else {
        0
      },
      operation: match name {
        "repairing" | "repair-complete" | "repair-failure" => 8,
        "uninstalling" | "remove-complete" | "remove-failure" => 4,
        "shortcuts-progress" => 7,
        _ => 6,
      },
      ..Default::default()
    };
    *shared.state.lock().map_err(|error| error.to_string())? = fixture.clone();
    present(&ui, &fixture, copy);
    if ui.get_show_preservation() != (confirmation || name == "remove-complete") {
      return Err(format!("Misplaced preservation copy on {name}").into());
    }
    if phase == Phase::Applying && ui.get_detail() != "" {
      return Err(format!("Generic setup text leaked into {name}").into());
    }
    let directory_before = ui.get_install_directory();
    let scope_before = ui.get_install_scope();
    let menu_before = ui.get_start_menu();
    let desktop_before = ui.get_desktop_shortcut();
    ui.set_options_error(word(copy, "folder_invalid"));
    for (index, locale) in LANGUAGES.iter().enumerate() {
      ui.set_language_index(index as i32);
      ui.invoke_language_selected(index as i32);
      let selected = &shared.catalogs[*locale];
      if ui.get_window_title() != word(selected, "setup")
        || ui.get_preservation() != word(selected, "preservation")
        || ui.get_options_error() != word(selected, "folder_invalid")
      {
        return Err(format!("Live language switch failed for {name}/{locale}").into());
      }
      if ui.get_install_directory() != directory_before
        || ui.get_install_scope() != scope_before
        || ui.get_start_menu() != menu_before
        || ui.get_desktop_shortcut() != desktop_before
        || shared
          .state
          .lock()
          .map_err(|error| error.to_string())?
          .phase
          != phase
      {
        return Err("Language switch reset installer state".into());
      }
    }
    ui.set_language_index(initial_language as i32);
    ui.invoke_language_selected(initial_language as i32);
    ui.set_options_error("".into());
    ui.window()
      .dispatch_event(slint::platform::WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(-1.0, -1.0),
      });
    ui.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 640 * 420];
    window.draw_if_needed(|renderer| {
      renderer.render(&mut pixels, 640);
    });
    let image: Vec<u8> = pixels
      .iter()
      .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
      .collect();
    let mut encoder = png::Encoder::new(
      std::fs::File::create(directory.join(format!("{name}.png")))?,
      640,
      420,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&image)?;
    actions.borrow_mut().clear();
    let click_at = |x, y| {
      use slint::platform::{PointerEventButton, WindowEvent};
      let position = slint::LogicalPosition::new(x, y);
      ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
      ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
      });
      ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
      });
    };
    let click = |x| click_at(x, 369.0);
    if name == "install" {
      ui.set_language_index(0);
      ui.invoke_language_selected(0);
      click_at(ui.get_language_x(), ui.get_language_y());
      for key in [slint::platform::Key::DownArrow.into(), "\n".into()] {
        ui.window()
          .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: key });
      }
      if shared.language.load(Ordering::Acquire) != 1 {
        return Err("Language dropdown keyboard selection failed".into());
      }
      ui.set_language_index(initial_language as i32);
      ui.invoke_language_selected(initial_language as i32);
    }
    if name == "install" {
      click_at(ui.get_everyone_x(), ui.get_choice_y());
      if ui.get_install_scope() != 1 {
        return Err("Installer scope selection failed".into());
      }
      ui.set_scope_locked(true);
      click_at(ui.get_just_me_x(), ui.get_user_y());
      if ui.get_install_scope() != 1 {
        return Err("Installed scope was editable".into());
      }
      ui.set_scope_locked(false);
      click_at(ui.get_just_me_x(), ui.get_user_y());
      if ui.get_install_scope() != 0 {
        return Err("User scope selection failed".into());
      }
    }
    if name == "shortcuts" {
      ui.set_start_menu(false);
      ui.set_desktop_shortcut(false);
      click_at(ui.get_menu_x(), ui.get_shortcut_y());
      click_at(ui.get_desktop_x(), ui.get_desktop_y());
      if !ui.get_start_menu() || !ui.get_desktop_shortcut() {
        return Err("Post-install shortcut choices failed".into());
      }
      click(430.0);
      if !actions.borrow().is_empty() {
        return Err("Completion still exposes the redundant Skip action".into());
      }
      ui.set_launch_app(true);
      click_at(ui.get_launch_x(), ui.get_launch_y());
      if ui.get_launch_app() {
        return Err("Launch Flinch could not be deselected".into());
      }
      click_at(ui.get_launch_x(), ui.get_launch_y());
      if !ui.get_launch_app() {
        return Err("Launch Flinch could not be selected".into());
      }
      ui.set_start_menu(false);
      ui.set_desktop_shortcut(false);
    }
    if name == "folder" {
      click_at(ui.get_browse_x(), ui.get_browse_y());
      if browsed.get() != 1 {
        return Err("Installer browse action failed".into());
      }
    }
    let expected: &[i32] = match name {
      "install" | "install-machine" => {
        click(552.0);
        &[11]
      },
      "shortcuts" | "shortcuts-failed" => {
        click(552.0);
        &[7]
      },
      "folder" | "update" => {
        click(552.0);
        &[6]
      },
      "maintenance" => {
        click(552.0);
        click(88.0);
        &[8, 4]
      },
      "progress" | "runtime" | "repairing" | "uninstalling" | "shortcuts-progress" => {
        click(552.0);
        &[9]
      },
      "confirmation" => {
        click(552.0);
        click(430.0);
        &[4, 0]
      },
      _ => {
        click(552.0);
        &[0]
      },
    };
    if actions.borrow().as_slice() != expected {
      return Err(
        format!(
          "Installer {name} input failed: {:?}, expected {expected:?}",
          actions.borrow()
        )
        .into(),
      );
    }
    if ui.get_keyboard_navigation() {
      return Err("Mouse interaction left keyboard decorations visible".into());
    }
    let last = actions.borrow().last().copied();
    ui.window()
      .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "\n".into() });
    ui.window()
      .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: "\n".into() });
    if actions.borrow().len() != expected.len() + 1 || actions.borrow().last().copied() != last {
      return Err(format!("Installer {name} keyboard activation failed").into());
    }
    if !ui.get_keyboard_navigation() {
      return Err("Keyboard navigation highlight was disabled".into());
    }
    actions.borrow_mut().clear();
    click_at(624.0, 16.0);
    let close_operation = if matches!(phase, Phase::Detecting | Phase::Planning | Phase::Applying) {
      9
    } else {
      10
    };
    if actions.borrow().as_slice() != [close_operation] {
      return Err(format!("Installer {name} window close action failed").into());
    }
    ui.window()
      .dispatch_event(slint::platform::WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(-1.0, -1.0),
      });
    ui.window().request_redraw();
    window.draw_if_needed(|renderer| {
      renderer.render(&mut pixels, 640);
    });
    let corner = pixels[8 * 640 + 608];
    if (corner.r, corner.g, corner.b) != (17, 17, 19) {
      return Err(format!("Mouse click left a visible focus border on {name}").into());
    }
  }
  present(
    &ui,
    &State {
      phase: Phase::Ready,
      ..Default::default()
    },
    copy,
  );
  for elapsed in [
    0,
    160,
    300,
    380,
    540,
    700,
    1038,
    1263,
    1638,
    2063,
    2688,
    motion::DURATION_MS,
  ] {
    apply_intro(&ui, elapsed)?;
    ui.window().request_redraw();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 640 * 420];
    window.draw_if_needed(|renderer| {
      renderer.render(&mut pixels, 640);
    });
    let image: Vec<u8> = pixels
      .iter()
      .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
      .collect();
    let mut encoder = png::Encoder::new(
      std::fs::File::create(directory.join(format!("intro-{elapsed:04}ms.png")))?,
      640,
      420,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&image)?;
    actions.borrow_mut().clear();
    use slint::platform::{PointerEventButton, WindowEvent};
    let position = slint::LogicalPosition::new(552.0, 369.0);
    ui.window()
      .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
      position,
      button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
      position,
      button: PointerEventButton::Left,
    });
    let expected: &[i32] = if elapsed < motion::DURATION_MS {
      &[]
    } else {
      &[11]
    };
    if actions.borrow().as_slice() != expected {
      return Err(format!("Installer entrance input gate failed at {elapsed}ms").into());
    }
  }
  Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
  if let Err(error) = tracing_subscriber::fmt()
    .with_writer(std::io::stderr)
    .try_init()
  {
    eprintln!("Installer diagnostic logging could not initialize: {error}");
  }
  let catalogs: serde_json::Value =
    serde_json::from_str(include_str!("../../../messages/installer-ui.json"))?;
  #[link(name = "kernel32")]
  unsafe extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
  }
  let locale = match std::env::var("FLINCH_INSTALLER_LANGUAGE") {
    Ok(locale) => locale,
    Err(std::env::VarError::NotPresent) => {
      // SAFETY: this Windows locale query has no pointer arguments or side effects.
      let language = unsafe { GetUserDefaultUILanguage() } & 0x03ff;
      match language {
        0x0c => "fr",
        0x07 => "de",
        0x0a => "es",
        0x10 => "it",
        _ => "en",
      }
      .into()
    },
    Err(error) => return Err(error.into()),
  };
  let copy = catalogs
    .get(&locale)
    .ok_or("Unsupported installer language")?
    .clone();
  let reference = catalogs["en"]
    .as_object()
    .ok_or("Installer reference catalog is invalid")?;
  let selected = copy.as_object().ok_or("Installer catalog is invalid")?;
  if reference.keys().any(|key| {
    selected
      .get(key)
      .and_then(serde_json::Value::as_str)
      .is_none_or(str::is_empty)
  }) {
    return Err("Installer catalog is incomplete".into());
  }
  let args: Vec<_> = std::env::args().collect();
  if args.get(1).map(String::as_str) == Some("--preview") {
    return preview(
      std::path::Path::new(args.get(2).ok_or("Preview output directory is required")?),
      &copy,
    );
  }
  let chrome_failure = std::rc::Rc::new(std::cell::RefCell::new(None));
  slint::BackendSelector::new()
    .backend_name("winit".into())
    .renderer_name("skia-software".into())
    .with_winit_custom_application_handler(chrome::ChromeHandler::new(chrome_failure.clone()))
    .select()?;
  let ui = InstallerWindow::new()?;
  let shared = Arc::new(Shared {
    state: Mutex::new(State::default()),
    pending: AtomicBool::new(false),
    ui: ui.as_weak(),
    catalogs,
    language: AtomicUsize::new(
      LANGUAGES
        .iter()
        .position(|candidate| *candidate == locale)
        .ok_or("Unsupported installer language")?,
    ),
  });
  connect_intro(&ui)?;
  connect_instance(&ui);
  connect_options(&ui, &shared)?;
  connect_language(&ui, &shared);
  present(&ui, &State::default(), shared.copy());
  let actions = shared.clone();
  ui.on_action(move |operation| {
    let Some(ui) = actions.ui.upgrade() else {
      return;
    };
    let shortcuts = ui.get_start_menu() || ui.get_desktop_shortcut();
    if matches!(operation, 4 | 6 | 8) || (operation == 7 && shortcuts) {
      let Some(ui) = actions.ui.upgrade() else {
        eprintln!("Installer options window is gone");
        return;
      };
      if let Err(error) = submit_options(&ui) {
        eprintln!("{error}");
        if operation == 7 {
          ui.set_detail(word(actions.copy(), "shortcuts_failed"));
          return;
        }
        ui.set_options_error(word(actions.copy(), "folder_invalid"));
        if !ui.get_show_options()
          && let Ok(mut state) = actions.state.lock()
        {
          state.status = -2147024809;
          state.phase = Phase::Failed;
          present(&ui, &state, actions.copy());
        }
        return;
      }
      ui.set_options_error("".into());
    }
    let command = match actions.state.lock() {
      Ok(mut state) => match if operation == 7 {
        state.finish(shortcuts, ui.get_launch_app())
      } else {
        state.command(operation)
      } {
        Ok(command) => command,
        Err(error) => {
          eprintln!("Installer command rejected: {error}");
          return;
        },
      },
      Err(error) => {
        eprintln!("Installer action state unavailable: {error}");
        return;
      },
    };
    // SAFETY: bridge commands validate the operation and retain the engine outside registry locks.
    let status = command.map_or(0, |operation| unsafe { flinch_burn_command(operation) });
    if let Ok(mut state) = actions.state.lock() {
      if status < 0 {
        state.status = status;
        state.phase = Phase::Failed;
      }
      if let Some(ui) = actions.ui.upgrade() {
        present(&ui, &state, actions.copy());
      }
    } else {
      eprintln!("Installer action state unavailable");
    }
    if let Err(error) = finish_installer(&ui, &actions) {
      eprintln!("Finish installer failed: {error}");
    }
  });
  let closing = shared.clone();
  ui.window().on_close_requested(move || {
    // Reserve cancellation before submitting it: completion may arrive immediately.
    let command = match closing.state.lock() {
      Ok(mut state) => {
        let operation = if state.busy() { 9 } else { 0 };
        state.command(operation)
      },
      Err(error) => Err(format!("Installer close state unavailable: {error}")),
    };
    let command = match command {
      Ok(command) => command,
      Err(error) => {
        eprintln!("{error}");
        return slint::CloseRequestResponse::KeepWindowShown;
      },
    };
    // SAFETY: cancellation never terminates Windows Installer or a shared prerequisite process.
    let status = command.map_or(0, |operation| unsafe { flinch_burn_command(operation) });
    if status < 0 {
      eprintln!("Installer close failed: 0x{:08X}", status as u32);
    }
    if let Ok(state) = closing.state.lock()
      && let Some(ui) = closing.ui.upgrade()
    {
      present(&ui, &state, closing.copy());
    }
    slint::CloseRequestResponse::KeepWindowShown
  });
  let worker_state = shared.clone();
  let (finished, completion) = std::sync::mpsc::sync_channel(1);
  std::thread::Builder::new()
    .name("installer-burn".into())
    .spawn(move || {
      // SAFETY: the Arc's address stays stable for the entire synchronous SDK run.
      let status = unsafe {
        flinch_burn_run(
          notify,
          &worker_state as *const Arc<Shared> as *mut std::ffi::c_void,
        )
      };
      if let Err(error) = finished.send(status) {
        eprintln!("Installer completion receiver disconnected: {error}");
      }
      // SAFETY: this is the final event, after all SDK callbacks have stopped.
      unsafe {
        notify(
          &worker_state as *const Arc<Shared> as *mut std::ffi::c_void,
          7,
          status,
          0,
        );
      }
    })?;
  slint::run_event_loop_until_quit()?;
  let chrome_error = chrome_failure.borrow_mut().take();
  if chrome_error.is_some() {
    // SAFETY: request cancellation without terminating a shared installer transaction.
    let cancellation = unsafe { flinch_burn_command(9) };
    if cancellation < 0 {
      eprintln!(
        "Installer chrome failure cancellation failed: 0x{:08X}",
        cancellation as u32
      );
    }
  }
  let status = completion.recv_timeout(std::time::Duration::from_secs(5))?;
  if let Some(error) = chrome_error {
    return Err(format!("Installer window chrome failed: {error}").into());
  }
  if status < 0 {
    return Err(format!("Burn connection failed: 0x{:08X}", status as u32).into());
  }
  Ok(())
}

fn main() {
  if let Err(error) = run() {
    eprintln!("Flinch installer failed: {error}");
    std::process::exit(1);
  }
}
