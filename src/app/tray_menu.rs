//! Flinch's own tray menu: two Slint windows on the UI owner thread, styled like settings.
//! Actions go through the tray's dispatcher, so they behave exactly like the native menu, which
//! stays as the explicit fallback when these windows cannot be created.

mod fade;
#[cfg(test)]
mod frame_export;
mod layout;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use flinch_settings::slint::{
  self, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
  VecModel,
  winit_030::{
    WinitWindowAccessor,
    winit::{
      raw_window_handle::{HasWindowHandle, RawWindowHandle},
      window::WindowAttributes,
    },
  },
};
use flinch_settings::ui::{
  TrayChoiceWindow, TrayMenuRow, TrayMenuWindow, TrayRecentRow, TrayRecentWindow,
};
use windows::Win32::Foundation::{HWND, POINT, RECT};

use super::context::AppHandle;
use crate::recent_captures::{CaptureKind, RecentCapture};

/// How long the pointer rests on Recent Captures before its submenu opens.
const SUBMENU_OPEN: Duration = Duration::from_millis(150);
/// How long the pointer rests on another row before an open submenu closes. Moving diagonally
/// toward the submenu crosses other rows in less time, so it stays open.
const SUBMENU_CLOSE: Duration = Duration::from_millis(300);
/// The windows are created shortly after startup so even the first open is instant.
const PREWARM_AFTER: Duration = Duration::from_secs(2);
/// Where prewarmed windows are shown once, out of sight, to create them.
const OFFSCREEN: i32 = -32000;
/// A right-click on the tray icon while the menu is open first takes focus from it; the click that
/// follows must not reopen it.
const REOPEN_GUARD: Duration = Duration::from_millis(300);
/// Thumbnails are requested at this logical size, so they stay crisp in the 60x38 tile.
const THUMBNAIL_SIDE: f32 = 96.0;

/// One of the menu's submenu windows, opened from the row of the same kind.
enum Submenu {
  Recent(TrayRecentWindow),
  Delay(TrayChoiceWindow),
}

impl Submenu {
  fn window(&self) -> &slint::Window {
    match self {
      Self::Recent(window) => window.window(),
      Self::Delay(window) => window.window(),
    }
  }

  fn set_shown(&self, shown: bool) {
    match self {
      Self::Recent(window) => window.set_shown(shown),
      Self::Delay(window) => window.set_shown(shown),
    }
  }

  fn set_slide(&self, x: f32, y: f32) {
    match self {
      Self::Recent(window) => {
        window.set_slide_x(x);
        window.set_slide_y(y);
      },
      Self::Delay(window) => {
        window.set_slide_x(x);
        window.set_slide_y(y);
      },
    }
  }

  /// Logical size, set by `fill` before the submenu opens.
  fn size(&self) -> (f32, f32) {
    match self {
      Self::Recent(window) => (window.get_menu_width(), window.get_menu_height()),
      Self::Delay(window) => (window.get_menu_width(), window.get_menu_height()),
    }
  }

  /// The keyboard starts on the first capture, or on the current choice.
  fn reset_highlight(&self, keyboard: bool) {
    match self {
      Self::Recent(window) => {
        window.set_highlight(if keyboard { 0 } else { -1 });
        window.set_part(0);
      },
      Self::Delay(window) => {
        window.set_highlight(if keyboard {
          window.get_selected().max(0)
        } else {
          -1
        });
      },
    }
  }

  fn focus_menu(&self) {
    match self {
      Self::Recent(window) => window.invoke_focus_menu(),
      Self::Delay(window) => window.invoke_focus_menu(),
    }
  }

  fn show(&self) -> Result<(), slint::PlatformError> {
    match self {
      Self::Recent(window) => window.show(),
      Self::Delay(window) => window.show(),
    }
  }

  fn hide(&self) -> Result<(), slint::PlatformError> {
    match self {
      Self::Recent(window) => window.hide(),
      Self::Delay(window) => window.hide(),
    }
  }
}

struct Menu {
  app: AppHandle,
  main: TrayMenuWindow,
  recent: TrayRecentWindow,
  choice: TrayChoiceWindow,
  rows: Vec<layout::Row>,
  captures: Vec<RecentCapture>,
  /// Thumbnails by path and save time; a capture saved again under the same name gets a new one.
  thumbnails: HashMap<(PathBuf, i64), Image>,
  submenu: Option<usize>,
  delay: slint::Timer,
  focus_check: slint::Timer,
  native_wait: slint::Timer,
  warm_wait: slint::Timer,
  /// Whole-window fades: the menu's, and the open submenu's (only one is open at a time).
  fade: fade::Fade,
  submenu_fade: fade::Fade,
  /// Recent rows are updated in place, so a thumbnail that arrives fades in instead of the row
  /// being rebuilt.
  recent_rows: Rc<VecModel<TrayRecentRow>>,
  open: bool,
  closed_at: Option<Instant>,
  frame: RECT,
  work: RECT,
  scale: f32,
}

impl Menu {
  /// The submenu a row opens, if it opens one.
  fn submenu_for(&self, row: usize) -> Option<Submenu> {
    let row = self.rows.get(row).filter(|row| row.submenu)?;
    Some(if row.id == "delay" {
      Submenu::Delay(self.choice.clone_strong())
    } else {
      Submenu::Recent(self.recent.clone_strong())
    })
  }

  /// The submenu that is open now.
  fn open_submenu(&self) -> Option<Submenu> {
    self.submenu.and_then(|row| self.submenu_for(row))
  }

  fn submenus(&self) -> [Submenu; 2] {
    [
      Submenu::Recent(self.recent.clone_strong()),
      Submenu::Delay(self.choice.clone_strong()),
    ]
  }
}

thread_local! {
  static MENU: RefCell<Option<Rc<RefCell<Menu>>>> = const { RefCell::new(None) };
  /// The windows could not be created; the native menu serves every later click.
  static FAILED: Cell<bool> = const { Cell::new(false) };
  /// Set only while this module creates its windows, so the shared window hook can tell them apart.
  static CREATING: Cell<bool> = const { Cell::new(false) };
}

/// Every Slint window is built through this hook. Only the menu windows skip the taskbar, and
/// they never take focus by appearing: the menu activates itself, and the prewarm stays unnoticed.
pub(crate) fn window_attributes(attributes: WindowAttributes) -> WindowAttributes {
  use flinch_settings::slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
  if CREATING.with(Cell::get) {
    attributes.with_skip_taskbar(true).with_active(false)
  } else {
    attributes
  }
}

/// Create both windows a little after startup, showing them once out of sight, so the first
/// right-click doesn't pay for window and renderer creation.
pub(crate) fn schedule_prewarm(app: &AppHandle) {
  let app = app.downgrade();
  slint::Timer::single_shot(PREWARM_AFTER, move || {
    if let Some(app) = app.upgrade() {
      prewarm(&app);
    }
  });
}

fn prewarm(app: &AppHandle) {
  if FAILED.with(Cell::get) || current().is_some() {
    return;
  }
  let menu = match create(app) {
    Ok(menu) => menu,
    Err(error) => {
      FAILED.with(|failed| failed.set(true));
      tracing::error!(%error, "Cannot create the tray menu; using the native menu");
      super::tray::use_native_menu(app, false);
      return;
    },
  };
  let (main, submenus) = {
    let Ok(state) = menu.try_borrow() else {
      return;
    };
    (state.main.clone_strong(), state.submenus())
  };
  main
    .window()
    .set_position(slint::PhysicalPosition::new(OFFSCREEN, OFFSCREEN));
  for submenu in &submenus {
    submenu
      .window()
      .set_position(slint::PhysicalPosition::new(OFFSCREEN, OFFSCREEN));
  }
  let shown = main
    .show()
    .and_then(|()| submenus.iter().try_for_each(Submenu::show));
  if let Err(error) = shown {
    tracing::warn!(%error, "Cannot prewarm the tray menu; it will open on first use");
    return;
  }
  settle_prewarm(&menu, 0);
}

/// Hide the prewarmed windows once Windows has created them, unless the menu opened meanwhile.
fn settle_prewarm(menu: &Rc<RefCell<Menu>>, attempt: u32) {
  const ATTEMPTS: u32 = 100;
  const RETRY: Duration = Duration::from_millis(20);
  let Ok(state) = menu.try_borrow() else {
    return;
  };
  let handles = (
    handle(state.main.window()),
    handle(state.recent.window()),
    handle(state.choice.window()),
  );
  let (Some(main), Some(recent), Some(choice)) = handles else {
    if attempt >= ATTEMPTS {
      tracing::warn!("Tray menu windows were not created during prewarm");
      return;
    }
    let weak = Rc::downgrade(menu);
    state
      .warm_wait
      .start(slint::TimerMode::SingleShot, RETRY, move || {
        if let Some(menu) = weak.upgrade() {
          settle_prewarm(&menu, attempt + 1);
        }
      });
    return;
  };
  for hwnd in [main, recent, choice] {
    if let Err(error) = prepare(hwnd) {
      tracing::warn!(%error, "Cannot style a prewarmed tray menu window");
    }
  }
  let hide_main = !state.open;
  // Keep only a submenu the menu opened meanwhile.
  let open = state.open_submenu();
  let hide: Vec<Submenu> = state
    .submenus()
    .into_iter()
    .filter(|submenu| {
      !open
        .as_ref()
        .is_some_and(|open| std::mem::discriminant(open) == std::mem::discriminant(submenu))
    })
    .collect();
  let main = state.main.clone_strong();
  drop(state);
  for submenu in hide {
    if let Err(error) = submenu.hide() {
      tracing::warn!(%error, "Cannot hide a prewarmed submenu");
    }
  }
  if hide_main && let Err(error) = main.hide() {
    tracing::warn!(%error, "Cannot hide the prewarmed tray menu");
  }
}

/// Open from a tray right-click at a physical screen position. Runs on the UI owner thread.
pub(crate) fn request_open(app: &AppHandle, x: i32, y: i32) {
  let app = app.clone();
  if let Err(error) = slint::invoke_from_event_loop(move || open(&app, POINT { x, y })) {
    tracing::error!(%error, "Cannot schedule the tray menu");
  }
}

/// Whether the native menu has taken over after the custom windows failed.
pub(crate) fn uses_native_menu() -> bool {
  FAILED.with(Cell::get)
}

/// Recording state or recent captures changed: an open menu updates in place.
pub(crate) fn refresh(app: &AppHandle) {
  let Some(menu) = current() else {
    return;
  };
  let open = menu
    .try_borrow()
    .ok()
    .filter(|menu| menu.open)
    .map(|menu| menu.scale);
  if let Some(scale) = open {
    fill(&menu, app, scale);
  }
}

/// Any change of focus between Slint windows. The menu closes once neither of its windows has it.
pub(crate) fn focus_changed() {
  let Some(menu) = current() else {
    return;
  };
  let Ok(state) = menu.try_borrow() else {
    return;
  };
  if !state.open {
    return;
  }
  let weak = Rc::downgrade(&menu);
  // Activation moves between the two menu windows in two steps; look once both have settled.
  state.focus_check.start(
    slint::TimerMode::SingleShot,
    Duration::from_millis(30),
    move || {
      if let Some(menu) = weak.upgrade() {
        let ours = menu
          .try_borrow()
          .map(|state| state.open && owns_foreground(&state))
          .unwrap_or(true);
        if !ours {
          close(&menu);
        }
      }
    },
  );
}

pub(crate) fn shutdown() {
  if let Some(menu) = MENU.with(|slot| slot.borrow_mut().take())
    && let Ok(state) = menu.try_borrow()
  {
    state.delay.stop();
    state.focus_check.stop();
    state.fade.stop();
    state.submenu_fade.stop();
    for submenu in state.submenus() {
      if let Err(error) = submenu.hide() {
        tracing::warn!(%error, "Cannot hide a tray submenu");
      }
    }
    if let Err(error) = state.main.hide() {
      tracing::warn!(%error, "Cannot hide the tray menu");
    }
  }
}

fn current() -> Option<Rc<RefCell<Menu>>> {
  MENU.with(|slot| slot.borrow().clone())
}

fn open(app: &AppHandle, cursor: POINT) {
  if FAILED.with(Cell::get) {
    return;
  }
  let menu = match current() {
    Some(menu) => menu,
    None => match create(app) {
      Ok(menu) => menu,
      Err(error) => {
        FAILED.with(|failed| failed.set(true));
        tracing::error!(%error, "Cannot create the tray menu; using the native menu");
        super::tray::use_native_menu(app, true);
        return;
      },
    },
  };
  {
    let Ok(state) = menu.try_borrow() else {
      return;
    };
    if state.open {
      drop(state);
      close(&menu);
      return;
    }
    if state
      .closed_at
      .is_some_and(|closed| closed.elapsed() < REOPEN_GUARD)
    {
      return;
    }
  }
  let (work, scale) = match monitor(cursor) {
    Ok(monitor) => monitor,
    Err(error) => {
      tracing::error!(%error, "Cannot place the tray menu");
      return;
    },
  };
  fill(&menu, app, scale);
  if let Err(error) = show_main(&menu, cursor, work, scale) {
    tracing::error!(%error, "Cannot show the tray menu");
    close(&menu);
  }
}

fn create(app: &AppHandle) -> Result<Rc<RefCell<Menu>>, String> {
  CREATING.with(|creating| creating.set(true));
  let created = (|| {
    let main = TrayMenuWindow::new().map_err(|error| error.to_string())?;
    let recent = TrayRecentWindow::new().map_err(|error| error.to_string())?;
    let choice = TrayChoiceWindow::new().map_err(|error| error.to_string())?;
    // Window adapters are built on first access; build them while the hook can see them.
    let _ = (
      main.window().scale_factor(),
      recent.window().scale_factor(),
      choice.window().scale_factor(),
    );
    Ok::<_, String>((main, recent, choice))
  })();
  CREATING.with(|creating| creating.set(false));
  let (main, recent, choice) = created?;
  flinch_settings::windows::configure_tray_menu(&main, &recent, &choice)
    .map_err(|error| error.to_string())?;
  let recent_rows = Rc::new(VecModel::default());
  recent.set_rows(ModelRc::from(recent_rows.clone()));
  let menu = Rc::new(RefCell::new(Menu {
    app: app.clone(),
    main: main.clone_strong(),
    recent: recent.clone_strong(),
    choice: choice.clone_strong(),
    rows: Vec::new(),
    captures: Vec::new(),
    thumbnails: HashMap::new(),
    submenu: None,
    delay: slint::Timer::default(),
    focus_check: slint::Timer::default(),
    native_wait: slint::Timer::default(),
    warm_wait: slint::Timer::default(),
    fade: fade::Fade::new(),
    submenu_fade: fade::Fade::new(),
    recent_rows: recent_rows.clone(),
    open: false,
    closed_at: None,
    frame: RECT::default(),
    work: RECT::default(),
    scale: 1.0,
  }));
  connect(&menu, &main, &recent);
  connect_choice(&menu, &choice);
  MENU.with(|slot| *slot.borrow_mut() = Some(menu.clone()));
  Ok(menu)
}

fn connect(menu: &Rc<RefCell<Menu>>, main: &TrayMenuWindow, recent: &TrayRecentWindow) {
  let weak = Rc::downgrade(menu);
  main.on_activate(move |id| {
    if let Some(menu) = weak.upgrade() {
      act(&menu, id.to_string());
    }
  });
  let weak = Rc::downgrade(menu);
  main.on_hover_row(move |index| {
    if let Some(menu) = weak.upgrade() {
      hover_row(&menu, index);
    }
  });
  let weak = Rc::downgrade(menu);
  main.on_open_submenu(move |index, keyboard| {
    if let Some(menu) = weak.upgrade() {
      if let Ok(state) = menu.try_borrow() {
        state.delay.stop();
      }
      open_submenu(&menu, index, keyboard);
    }
  });
  let weak = Rc::downgrade(menu);
  main.on_dismiss(move || {
    if let Some(menu) = weak.upgrade() {
      close(&menu);
    }
  });
  let weak = Rc::downgrade(menu);
  main.window().on_close_requested(move || {
    if let Some(menu) = weak.upgrade() {
      close(&menu);
    }
    slint::CloseRequestResponse::KeepWindowShown
  });
  let weak = Rc::downgrade(menu);
  recent.on_act(move |index, action| {
    if let Some(menu) = weak.upgrade() {
      act(&menu, format!("recent-{action}-{index}"));
    }
  });
  let weak = Rc::downgrade(menu);
  recent.on_folder(move || {
    if let Some(menu) = weak.upgrade() {
      act(&menu, "recent-folder".into());
    }
  });
  let weak = Rc::downgrade(menu);
  recent.on_back(move || {
    if let Some(menu) = weak.upgrade() {
      close_submenu(&menu);
    }
  });
  let weak = Rc::downgrade(menu);
  recent.on_hovered(move |hovered| {
    if hovered
      && let Some(menu) = weak.upgrade()
      && let Ok(state) = menu.try_borrow()
    {
      // Reaching the submenu cancels a close started by crossing other rows on the way.
      state.delay.stop();
      if let Some(row) = state.submenu {
        state.main.set_highlight(row as i32);
      }
    }
  });
  let weak = Rc::downgrade(menu);
  recent.window().on_close_requested(move || {
    if let Some(menu) = weak.upgrade() {
      close(&menu);
    }
    slint::CloseRequestResponse::KeepWindowShown
  });
}

/// The Capture Delay submenu behaves like Recent Captures: Left or Esc goes back, and reaching
/// it keeps it open. A choice closes the menu and is saved through the tray dispatcher.
fn connect_choice(menu: &Rc<RefCell<Menu>>, choice: &TrayChoiceWindow) {
  let weak = Rc::downgrade(menu);
  choice.on_choose(move |index| {
    if let Some(menu) = weak.upgrade() {
      match usize::try_from(index)
        .ok()
        .and_then(|index| crate::capture_delay::CHOICES.get(index))
      {
        Some(seconds) => act(&menu, format!("delay-{seconds}")),
        None => tracing::warn!(index, "Unknown Capture Delay row"),
      }
    }
  });
  let weak = Rc::downgrade(menu);
  choice.on_back(move || {
    if let Some(menu) = weak.upgrade() {
      close_submenu(&menu);
    }
  });
  let weak = Rc::downgrade(menu);
  choice.on_hovered(move |hovered| {
    if hovered
      && let Some(menu) = weak.upgrade()
      && let Ok(state) = menu.try_borrow()
    {
      state.delay.stop();
      if let Some(row) = state.submenu {
        state.main.set_highlight(row as i32);
      }
    }
  });
  let weak = Rc::downgrade(menu);
  choice.window().on_close_requested(move || {
    if let Some(menu) = weak.upgrade() {
      close(&menu);
    }
    slint::CloseRequestResponse::KeepWindowShown
  });
}

/// Load what the menu shows now: rows for the recording state and recent captures.
fn fill(menu: &Rc<RefCell<Menu>>, app: &AppHandle, scale: f32) {
  let language = crate::i18n::get_lang(app);
  let labels = crate::i18n::tray(&language);
  let paused = crate::state::current_recording_session(app).map(|session| session.paused);
  let delay = crate::config::Config::current(app).capture_delay;
  let rows = layout::rows(labels, paused, delay);
  let captures = crate::recent_captures::list(app);
  super::tray::remember_menu_recent(captures.clone());
  let Ok(mut state) = menu.try_borrow_mut() else {
    return;
  };
  let main = state.main.clone_strong();
  let width = layout::menu_width(&rows, |text| {
    main.set_measure_text(text.into());
    main.get_measured_width()
  });
  main.set_menu_width(width);
  main.set_menu_height(layout::menu_height(&rows));
  let choice_width = layout::choice_width(&labels.delay_choices, |text| {
    main.set_measure_text(text.into());
    main.get_measured_width()
  });
  state.choice.set_menu_width(choice_width);
  state
    .choice
    .set_menu_height(layout::choice_height(labels.delay_choices.len()));
  state.choice.set_choices(ModelRc::new(VecModel::from(
    labels
      .delay_choices
      .iter()
      .map(|label| SharedString::from(*label))
      .collect::<Vec<_>>(),
  )));
  state.choice.set_selected(
    crate::capture_delay::CHOICES
      .iter()
      .position(|choice| *choice == delay)
      .map_or(0, |index| index as i32),
  );
  main.set_rows(ModelRc::new(VecModel::from(
    rows.iter().map(menu_row).collect::<Vec<_>>(),
  )));
  state
    .recent
    .set_menu_height(layout::recent_height(captures.len()));
  if let Some(row) = state.submenu {
    state.main.set_submenu_row(row as i32);
  }
  state.recent.set_empty_label(labels.recent_empty.into());
  state.recent.set_copy_label(labels.recent_copy.into());
  state.recent.set_show_label(labels.recent_show.into());
  state.recent.set_pin_label(labels.pin.into());
  state.recent.set_folder_label(labels.recent_folder.into());
  let keep: Vec<(PathBuf, i64)> = captures
    .iter()
    .map(|capture| (capture.path.clone(), capture.saved_at))
    .collect();
  state.thumbnails.retain(|key, _| keep.contains(key));
  let missing: Vec<PathBuf> = captures
    .iter()
    .filter(|capture| {
      !state
        .thumbnails
        .contains_key(&(capture.path.clone(), capture.saved_at))
    })
    .map(|capture| capture.path.clone())
    .collect();
  state.rows = rows;
  state.captures = captures;
  set_recent_rows(&state);
  let side = layout::physical(THUMBNAIL_SIDE, scale.max(1.0)).max(1) as u32;
  drop(state);
  crate::thumbnails::request(missing, side, |path, result| {
    let pixels = result.map(|pixels| {
      let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(pixels.width, pixels.height);
      buffer.make_mut_bytes().copy_from_slice(&pixels.rgba);
      buffer
    });
    if let Err(error) = slint::invoke_from_event_loop(move || thumbnail_ready(path, pixels)) {
      tracing::warn!(%error, "Cannot deliver a recent capture thumbnail");
    }
  });
}

fn thumbnail_ready(path: PathBuf, pixels: Result<SharedPixelBuffer<Rgba8Pixel>, String>) {
  let Some(menu) = current() else {
    return;
  };
  let Ok(mut state) = menu.try_borrow_mut() else {
    return;
  };
  match pixels {
    Ok(buffer) => {
      let Some(saved_at) = state
        .captures
        .iter()
        .find(|capture| capture.path == path)
        .map(|capture| capture.saved_at)
      else {
        return;
      };
      state
        .thumbnails
        .insert((path, saved_at), Image::from_rgba8_premultiplied(buffer));
      set_recent_rows(&state);
    },
    Err(error) => tracing::info!(%error, "No Shell thumbnail for a recent capture"),
  }
}

fn set_recent_rows(state: &Menu) {
  let rows = state
    .captures
    .iter()
    .map(|capture| {
      let thumbnail = state
        .thumbnails
        .get(&(capture.path.clone(), capture.saved_at))
        .cloned();
      let name = capture
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
      TrayRecentRow {
        name: layout::fit_middle(&name, state.recent.get_name_width(), |text| {
          state.main.set_measure_text(text.into());
          state.main.get_measured_width()
        })
        .into(),
        time: crate::recent_captures::saved_label(capture.saved_at).into(),
        kind: match capture.kind {
          CaptureKind::Screenshot => "image",
          CaptureKind::Recording => "video",
          CaptureKind::Gif => "gif",
        }
        .into(),
        has_thumbnail: thumbnail.is_some(),
        thumbnail: thumbnail.unwrap_or_default(),
      }
    })
    .collect::<Vec<_>>();
  let model = &state.recent_rows;
  if model.row_count() == rows.len() {
    for (index, row) in rows.into_iter().enumerate() {
      if model.row_data(index).as_ref() != Some(&row) {
        model.set_row_data(index, row);
      }
    }
  } else {
    model.set_vec(rows);
  }
}

fn menu_row(row: &layout::Row) -> TrayMenuRow {
  TrayMenuRow {
    id: row.id.into(),
    label: SharedString::from(row.label.as_str()),
    detail: SharedString::from(row.detail.as_str()),
    icon: row.icon.into(),
    separator: row.separator,
    accent: row.accent,
    submenu: row.submenu,
  }
}

fn show_main(
  menu: &Rc<RefCell<Menu>>,
  cursor: POINT,
  work: RECT,
  scale: f32,
) -> anyhow::Result<()> {
  let main = {
    let mut state = menu
      .try_borrow_mut()
      .map_err(|_| anyhow::anyhow!("Tray menu state is busy"))?;
    state.main.set_highlight(-1);
    state.main.set_submenu_row(-1);
    state.submenu = None;
    let size = (
      layout::physical(state.main.get_menu_width(), scale),
      layout::physical(state.main.get_menu_height(), scale),
    );
    let origin = layout::place_menu(cursor, size, work);
    let (slide_x, slide_y) = layout::menu_slide(cursor, origin, work);
    state.main.set_slide_x(slide_x);
    state.main.set_slide_y(slide_y);
    state.main.set_shown(false);
    // A created window is shown invisible; the reveal fades it in once it is on screen.
    if let Some(hwnd) = handle(state.main.window()) {
      state.fade.conceal(hwnd);
    }
    state.frame = RECT {
      left: origin.x,
      top: origin.y,
      right: origin.x + size.0,
      bottom: origin.y + size.1,
    };
    state.work = work;
    state.scale = scale;
    state.open = true;
    state
      .main
      .window()
      .set_position(slint::PhysicalPosition::new(origin.x, origin.y));
    state.main.clone_strong()
  };
  main.show()?;
  when_native(menu, Part::Menu, 0);
  Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Part {
  Menu,
  Submenu,
}

/// Slint creates a window's HWND on the event loop's next turn after its first `show`, so styling,
/// activation and focus wait for it, briefly and a bounded number of times.
fn when_native(menu: &Rc<RefCell<Menu>>, part: Part, attempt: u32) {
  const ATTEMPTS: u32 = 50;
  const RETRY: Duration = Duration::from_millis(10);
  let Ok(state) = menu.try_borrow() else {
    return;
  };
  let still_wanted = state.open && (matches!(part, Part::Menu) || state.submenu.is_some());
  if !still_wanted {
    return;
  }
  let submenu = state.open_submenu();
  let window = match (part, &submenu) {
    (Part::Menu, _) => state.main.window(),
    (Part::Submenu, Some(submenu)) => submenu.window(),
    (Part::Submenu, None) => return,
  };
  let Some(hwnd) = handle(window) else {
    if attempt >= ATTEMPTS {
      drop(state);
      tracing::error!(?part, "Tray menu window was never created");
      close(menu);
      return;
    }
    let weak = Rc::downgrade(menu);
    state
      .native_wait
      .start(slint::TimerMode::SingleShot, RETRY, move || {
        if let Some(menu) = weak.upgrade() {
          when_native(&menu, part, attempt + 1);
        }
      });
    return;
  };
  let main = state.main.clone_strong();
  drop(state);
  if let Err(error) = prepare(hwnd) {
    tracing::warn!(%error, ?part, "Cannot style the tray menu window");
  }
  activate(hwnd);
  // The reveal starts once the window is really on screen, so it is never spent on creation.
  // The whole window fades while its content slides.
  let animate = crate::overlay::motion::windows_animates();
  if let Ok(state) = menu.try_borrow() {
    match part {
      Part::Menu => state.fade.reveal(hwnd, animate),
      Part::Submenu => state.submenu_fade.reveal(hwnd, animate),
    }
  }
  match (part, submenu) {
    (Part::Submenu, Some(submenu)) => {
      submenu.set_shown(true);
      submenu.focus_menu();
    },
    _ => {
      main.set_shown(true);
      main.invoke_focus_menu();
    },
  }
  // If Windows refused the foreground, no focus change will ever close the menu; check now.
  focus_changed();
}

fn hover_row(menu: &Rc<RefCell<Menu>>, index: i32) {
  let Ok(state) = menu.try_borrow() else {
    return;
  };
  let submenu_row = usize::try_from(index)
    .ok()
    .filter(|index| state.rows.get(*index).is_some_and(|row| row.submenu));
  match (submenu_row, state.submenu) {
    (Some(row), Some(open)) if row == open => state.delay.stop(),
    (Some(row), _) => {
      let weak = Rc::downgrade(menu);
      state
        .delay
        .start(slint::TimerMode::SingleShot, SUBMENU_OPEN, move || {
          if let Some(menu) = weak.upgrade() {
            open_submenu(&menu, row as i32, false);
          }
        });
    },
    (None, Some(_)) if index >= 0 => {
      let weak = Rc::downgrade(menu);
      state
        .delay
        .start(slint::TimerMode::SingleShot, SUBMENU_CLOSE, move || {
          if let Some(menu) = weak.upgrade() {
            close_submenu(&menu);
          }
        });
    },
    (None, None) => state.delay.stop(),
    (None, Some(_)) => {},
  }
}

fn open_submenu(menu: &Rc<RefCell<Menu>>, index: i32, keyboard: bool) {
  let Ok(row) = usize::try_from(index) else {
    return;
  };
  // Another row's submenu goes away first, so only one is ever open.
  let other_open = menu
    .try_borrow()
    .is_ok_and(|state| state.submenu.is_some_and(|open| open != row));
  if other_open {
    close_submenu(menu);
  }
  let submenu = {
    let Ok(mut state) = menu.try_borrow_mut() else {
      return;
    };
    if !state.open {
      return;
    }
    let Some(submenu) = state.submenu_for(row) else {
      return;
    };
    submenu.reset_highlight(keyboard);
    if state.submenu == Some(row) {
      drop(state);
      focus(&submenu);
      return;
    }
    let scale = state.scale;
    let (width, height) = submenu.size();
    let size = (
      layout::physical(width, scale),
      layout::physical(height, scale),
    );
    let top = layout::physical(layout::row_top(&state.rows, row), scale);
    let origin = layout::place_submenu(state.frame, top, scale, size, state.work);
    submenu.set_slide(
      if origin.x >= state.frame.right {
        -8.0
      } else {
        8.0
      },
      0.0,
    );
    submenu.set_shown(false);
    if let Some(hwnd) = handle(submenu.window()) {
      state.submenu_fade.conceal(hwnd);
    }
    submenu
      .window()
      .set_position(slint::PhysicalPosition::new(origin.x, origin.y));
    state.submenu = Some(row);
    state.main.set_submenu_row(index);
    state.main.set_highlight(index);
    submenu
  };
  if let Err(error) = submenu.show() {
    tracing::error!(%error, "Cannot show a tray submenu");
    close_submenu(menu);
    return;
  }
  when_native(menu, Part::Submenu, 0);
}

/// Give focus back to the menu before hiding the submenu, so Windows never hands it elsewhere.
fn close_submenu(menu: &Rc<RefCell<Menu>>) {
  let (main, submenu) = {
    let Ok(mut state) = menu.try_borrow_mut() else {
      return;
    };
    state.delay.stop();
    state.submenu_fade.stop();
    let Some(submenu) = state.open_submenu() else {
      state.submenu = None;
      return;
    };
    state.submenu = None;
    state.main.set_submenu_row(-1);
    submenu.set_shown(false);
    (state.main.clone_strong(), submenu)
  };
  if let Some(hwnd) = handle(main.window()) {
    activate(hwnd);
  }
  main.invoke_focus_menu();
  if let Err(error) = submenu.hide() {
    tracing::warn!(%error, "Cannot hide a tray submenu");
  }
}

fn close(menu: &Rc<RefCell<Menu>>) {
  let (main, submenus) = {
    let Ok(mut state) = menu.try_borrow_mut() else {
      return;
    };
    state.delay.stop();
    state.focus_check.stop();
    state.native_wait.stop();
    state.fade.stop();
    state.submenu_fade.stop();
    if !state.open {
      return;
    }
    state.open = false;
    state.submenu = None;
    state.closed_at = Some(Instant::now());
    state.main.set_shown(false);
    let submenus = state.submenus();
    for submenu in &submenus {
      submenu.set_shown(false);
    }
    (state.main.clone_strong(), submenus)
  };
  for submenu in submenus {
    if let Err(error) = submenu.hide() {
      tracing::warn!(%error, "Cannot hide a tray submenu");
    }
  }
  if let Err(error) = main.hide() {
    tracing::warn!(%error, "Cannot hide the tray menu");
  }
}

/// Close first so a capture never sees the menu, then run the tray action.
fn act(menu: &Rc<RefCell<Menu>>, id: String) {
  close(menu);
  let app = match menu.try_borrow() {
    Ok(state) => state.app.clone(),
    Err(_) => return,
  };
  super::tray::dispatch(app, &id);
}

fn focus(submenu: &Submenu) {
  if let Some(hwnd) = handle(submenu.window()) {
    activate(hwnd);
  }
  submenu.focus_menu();
}

fn owns_foreground(state: &Menu) -> bool {
  use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
  // SAFETY: reads the foreground window handle; no window is changed.
  let foreground = unsafe { GetForegroundWindow() };
  [
    state.main.window(),
    state.recent.window(),
    state.choice.window(),
  ]
  .into_iter()
  .filter(|window| window.is_visible())
  .filter_map(handle)
  .any(|hwnd| hwnd == foreground)
}

fn handle(window: &slint::Window) -> Option<HWND> {
  match window.with_winit_window(|window| window.window_handle().map(|handle| handle.as_raw())) {
    Some(Ok(RawWindowHandle::Win32(handle))) => {
      Some(HWND(handle.hwnd.get() as *mut std::ffi::c_void))
    },
    _ => None,
  }
}

/// No taskbar button or Alt+Tab entry, and never in a recording or screenshot.
fn prepare(hwnd: HWND) -> anyhow::Result<()> {
  use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_TOOLWINDOW,
  };
  // SAFETY: the HWND belongs to a live menu window on this owner thread; existing bits are kept.
  unsafe {
    let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if style & WS_EX_TOOLWINDOW.0 as isize == 0 {
      SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_TOOLWINDOW.0 as isize);
    }
  }
  if !crate::overlay::gdi::window::exclude_from_capture(hwnd) {
    tracing::warn!("Cannot exclude the tray menu from captures");
  }
  Ok(())
}

/// The tray click gives this process the right to take the foreground, as native menus do.
fn activate(hwnd: HWND) {
  use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
  // SAFETY: the HWND belongs to a live menu window on this owner thread.
  if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
    tracing::debug!("Windows refused to activate the tray menu");
  }
}

/// Work area and UI scale of the monitor under the cursor.
fn monitor(cursor: POINT) -> anyhow::Result<(RECT, f32)> {
  use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
  };
  use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
  // SAFETY: a valid point and documented nearest-monitor flag.
  let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
  let mut info = MONITORINFO {
    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
    ..Default::default()
  };
  // SAFETY: info is a correctly sized writable monitor structure.
  anyhow::ensure!(
    unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool(),
    "Cannot query the tray monitor's work area"
  );
  let (mut x, mut y) = (96u32, 96u32);
  // SAFETY: both outputs are writable locals.
  unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) }?;
  Ok((info.rcWork, x as f32 / 96.0))
}
