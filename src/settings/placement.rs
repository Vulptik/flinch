//! Owner-thread placement with bounded, serialized background persistence.
use std::{cell::RefCell, path::PathBuf, time::Duration};

use anyhow::Context;
use flinch_settings::slint::{self, ComponentHandle, winit_030::WinitWindowAccessor};
use flinch_settings::ui::SettingsWindow;
use serde::{Deserialize, Serialize};
use windows::Win32::{
  Foundation::{POINT, RECT},
  Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
  },
  UI::{
    HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
    WindowsAndMessaging::GetCursorPos,
  },
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
struct Position {
  x: i32,
  y: i32,
}

struct State {
  saved: Option<Position>,
  sender: tokio::sync::watch::Sender<Option<Position>>,
  worker: tokio::task::JoinHandle<()>,
}
thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }

fn path() -> anyhow::Result<PathBuf> {
  #[cfg(debug_assertions)]
  if let Some(path) = super::native_probe::config_path() {
    return Ok(path.with_file_name("ui-state.json"));
  }
  Ok(crate::config::Config::config_file()?.with_file_name("ui-state.json"))
}

pub(super) fn initialize(runtime: &tokio::runtime::Runtime) -> anyhow::Result<()> {
  initialize_with(runtime, path)
}

fn initialize_with(
  runtime: &tokio::runtime::Runtime,
  resolve: impl FnOnce() -> anyhow::Result<PathBuf> + Send + 'static,
) -> anyhow::Result<()> {
  let (path, saved) = runtime
    .block_on(async {
      tokio::time::timeout(
        Duration::from_secs(2),
        crate::app::runtime::spawn_blocking(move || {
          let path = resolve()?;
          let saved = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
              Ok(position) => Some(position),
              Err(error) => {
                tracing::warn!(%error, "Invalid settings placement; centering window");
                None
              },
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
              tracing::warn!(%error, "Cannot read settings placement; centering window");
              None
            },
          };
          Ok::<_, anyhow::Error>((path, saved))
        }),
      )
      .await
    })
    .context("Placement load timed out")?
    .context("Placement load worker failed")??;
  let (sender, mut receiver) = tokio::sync::watch::channel(None::<Position>);
  let worker = crate::app::runtime::spawn(async move {
    while receiver.changed().await.is_ok() {
      let position = *receiver.borrow_and_update();
      if let Some(position) = position {
        let path = path.clone();
        match crate::app::runtime::spawn_blocking(move || write(&path, position)).await {
          Ok(Ok(())) => {},
          Ok(Err(error)) => tracing::warn!(%error, "Cannot save settings placement"),
          Err(error) => tracing::error!(%error, "Placement save worker failed"),
        }
      }
    }
  });
  STATE.with(|state| {
    *state.borrow_mut() = Some(State {
      saved,
      sender,
      worker,
    })
  });
  Ok(())
}

fn write(path: &std::path::Path, position: Position) -> anyhow::Result<()> {
  use std::os::windows::ffi::OsStrExt;
  use windows::{
    Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    core::PCWSTR,
  };
  let temporary = path.with_extension("json.tmp");
  std::fs::write(&temporary, serde_json::to_vec(&position)?)
    .context("Write placement temporary file")?;
  let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
  let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
  // SAFETY: Both paths are valid NUL-terminated strings for the duration of the call.
  unsafe {
    MoveFileExW(
      PCWSTR(source.as_ptr()),
      PCWSTR(destination.as_ptr()),
      MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
    )
  }
  .context("Replace saved settings placement")
}

fn fit(saved: Option<Position>, work: RECT, size: (i32, i32)) -> Position {
  let max_x = (work.right - size.0).max(work.left);
  let max_y = (work.bottom - size.1).max(work.top);
  match saved {
    Some(p) => Position {
      x: p.x.clamp(work.left, max_x),
      y: p.y.clamp(work.top, max_y),
    },
    None => Position {
      x: (work.left + (work.right - work.left - size.0) / 2).max(work.left),
      y: (work.top + (work.bottom - work.top - size.1) / 2).max(work.top),
    },
  }
}

pub(super) fn restore(window: &SettingsWindow) -> Result<(), String> {
  use crate::error::ErrorExt;
  restore_inner(window).map_err(|error| error.to_rich_string())
}

fn restore_inner(window: &SettingsWindow) -> anyhow::Result<()> {
  let saved = STATE.with(|state| state.borrow().as_ref().and_then(|state| state.saved));
  let point = saved.map_or(pointer()?, |p| POINT { x: p.x, y: p.y });
  // SAFETY: A valid point and documented nearest-monitor flag are passed.
  let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
  let mut info = MONITORINFO {
    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
    ..Default::default()
  };
  // SAFETY: info is a correctly sized writable monitor structure.
  anyhow::ensure!(
    unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool(),
    "Cannot query settings monitor work area"
  );
  let size = physical_size(
    monitor,
    window.get_initial_width(),
    window.get_initial_height(),
  )?;
  let on_monitor = saved.filter(|p| {
    p.x >= info.rcMonitor.left
      && p.x < info.rcMonitor.right
      && p.y >= info.rcMonitor.top
      && p.y < info.rcMonitor.bottom
  });
  let position = fit(on_monitor, info.rcWork, size);
  window
    .window()
    .set_position(slint::PhysicalPosition::new(position.x, position.y));
  Ok(())
}

fn pointer() -> anyhow::Result<POINT> {
  let mut pointer = POINT::default();
  // SAFETY: The pointer is writable; monitor handles are queried and used only on this thread.
  unsafe { GetCursorPos(&mut pointer) }.context("Read pointer monitor")?;
  Ok(pointer)
}

/// Slint's backend size is not initialized until show; use the UI's declared
/// dimensions to position the native window before its first visible frame.
fn physical_size(monitor: HMONITOR, width: f32, height: f32) -> anyhow::Result<(i32, i32)> {
  let (mut dpi_x, mut dpi_y) = (0, 0);
  // SAFETY: The monitor is live and both DPI outputs are writable.
  unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
    .context("Read settings monitor DPI")?;
  Ok((
    (width * dpi_x as f32 / 96.0).round() as i32,
    (height * dpi_y as f32 / 96.0).round() as i32,
  ))
}

/// Centers a transient window in the work area of the monitor under the pointer.
pub(super) fn center_on_pointer(
  window: &slint::Window,
  width: f32,
  height: f32,
) -> Result<(), String> {
  use crate::error::ErrorExt;
  center_on_pointer_inner(window, width, height).map_err(|error| error.to_rich_string())
}

fn center_on_pointer_inner(window: &slint::Window, width: f32, height: f32) -> anyhow::Result<()> {
  // SAFETY: A valid point and documented nearest-monitor flag are passed.
  let monitor = unsafe { MonitorFromPoint(pointer()?, MONITOR_DEFAULTTONEAREST) };
  let mut info = MONITORINFO {
    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
    ..Default::default()
  };
  // SAFETY: info is a correctly sized writable monitor structure.
  anyhow::ensure!(
    unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool(),
    "Cannot query pointer monitor work area"
  );
  let position = fit(None, info.rcWork, physical_size(monitor, width, height)?);
  window.set_position(slint::PhysicalPosition::new(position.x, position.y));
  Ok(())
}

pub(super) fn remember(window: &SettingsWindow) {
  window.window().with_winit_window(|native| {
    if native.is_minimized() == Some(true) {
      return;
    }
    match native.outer_position() {
      Ok(p) => STATE.with(|state| {
        if let Some(state) = state.borrow_mut().as_mut() {
          let position = Position { x: p.x, y: p.y };
          state.saved = Some(position);
          if state.sender.send(Some(position)).is_err() {
            tracing::error!("Settings placement persistence worker disconnected");
          }
        }
      }),
      Err(error) => tracing::warn!(%error, "Cannot read settings window position"),
    }
  });
}

pub(super) fn shutdown(runtime: &tokio::runtime::Runtime) {
  if let Some(state) = STATE.with(|state| state.borrow_mut().take()) {
    drop(state.sender);
    match runtime
      .block_on(async { tokio::time::timeout(Duration::from_secs(2), state.worker).await })
    {
      Ok(Ok(())) => {},
      Ok(Err(error)) => {
        tracing::error!(%error, "Placement persistence worker failed during shutdown")
      },
      Err(error) => tracing::warn!(%error, "Placement persistence shutdown timed out"),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn placement_worker_loads_isolated_files_coalesces_updates_and_drains_before_shutdown() {
    let runtime = crate::app::runtime::test_runtime();
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("placement.json");
    for content in [None, Some("malformed"), Some("{\"x\":-80,\"y\":40}")] {
      if let Some(content) = content {
        std::fs::write(&path, content).unwrap();
      }
      let source = path.clone();
      initialize_with(runtime, move || Ok(source)).unwrap();
      STATE.with(|state| {
        let owner = state.borrow();
        let state = owner.as_ref().unwrap();
        assert_eq!(
          state.saved,
          if content.is_some_and(|s| s.starts_with('{')) {
            Some(Position { x: -80, y: 40 })
          } else {
            None
          }
        );
        for index in 0..8 {
          state
            .sender
            .send(Some(Position {
              x: index,
              y: 100 + index,
            }))
            .unwrap();
        }
      });
      shutdown(runtime);
      assert!(STATE.with(|state| state.borrow().is_none()));
      let saved: Position = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
      assert_eq!(saved, Position { x: 7, y: 107 });
      assert!(!path.with_extension("json.tmp").exists());
    }
    assert!(
      initialize_with(runtime, || anyhow::bail!("isolated path failure"))
        .unwrap_err()
        .to_string()
        .contains("isolated path failure")
    );
    assert!(STATE.with(|state| state.borrow().is_none()));
    shutdown(runtime);
  }
  #[test]
  fn unreadable_and_unwritable_placement_centers_and_keeps_running() {
    let runtime = crate::app::runtime::test_runtime();
    let folder = tempfile::tempdir().unwrap();
    // A folder where the file should be cannot be read: the window is centered instead.
    let unreadable = folder.path().join("placement.json");
    std::fs::create_dir(&unreadable).unwrap();
    let source = unreadable.clone();
    initialize_with(runtime, move || Ok(source)).unwrap();
    assert!(STATE.with(|state| state.borrow().as_ref().unwrap().saved.is_none()));
    // Saving over that folder fails in the worker; it logs and keeps draining.
    STATE.with(|state| {
      let owner = state.borrow();
      owner
        .as_ref()
        .unwrap()
        .sender
        .send(Some(Position { x: 1, y: 2 }))
        .unwrap();
    });
    shutdown(runtime);
    assert!(unreadable.is_dir());
    assert!(
      write(
        &folder.path().join("missing").join("ui-state.json"),
        Position { x: 0, y: 0 }
      )
      .is_err()
    );
    // The production path sits beside the settings file.
    let resolved = path().unwrap();
    assert_eq!(
      resolved.file_name().and_then(|n| n.to_str()),
      Some("ui-state.json")
    );
  }
  #[test]
  fn centers_and_clamps_in_work_area_with_negative_origin() {
    let work = RECT {
      left: -1920,
      top: 40,
      right: 0,
      bottom: 1080,
    };
    assert_eq!(fit(None, work, (640, 640)), Position { x: -1280, y: 240 });
    assert_eq!(
      fit(Some(Position { x: -100, y: 900 }), work, (960, 960)),
      Position { x: -960, y: 120 }
    );
    assert_eq!(fit(None, work, (3000, 2000)), Position { x: -1920, y: 40 });
  }
  #[test]
  fn persistence_replaces_previous_position() {
    let dir =
      tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("temporary placement directory");
    let path = dir.path().join("ui-state.json");
    write(&path, Position { x: 30, y: 40 }).expect("first save");
    let position = Position { x: -900, y: 80 };
    write(&path, position).expect("replace save");
    assert_eq!(
      serde_json::from_slice::<Position>(&std::fs::read(path).expect("read saved position"))
        .expect("valid position"),
      position
    );
  }
}
