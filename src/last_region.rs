//! Capture Last Region: the last area picked for a screenshot, Capture & Edit, Copy Text, video
//! or GIF, so one shortcut can screenshot it again without the overlay.
//!
//! Kept in `last-region.json` beside the settings so it survives restarts. The area is stored in
//! virtual-screen physical pixels together with the monitor layout it was picked on (position,
//! size and scaling of every monitor). When the layout differs, the area is not reused: replaying
//! it could capture a different monitor or an offset area, so the normal picker opens instead.

use crate::app::context::AppHandle;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE: &str = "last-region.json";

/// An area in virtual-screen physical pixels; `x`/`y` can be negative left of or above the
/// primary monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
}

impl Region {
  /// The area inside a capture whose top-left is at (`origin_x`, `origin_y`), or `None` when any
  /// part of it falls outside.
  pub fn within(self, origin_x: i32, origin_y: i32, width: u32, height: u32) -> Option<Region> {
    if self.width == 0 || self.height == 0 {
      return None;
    }
    let left = i64::from(self.x) - i64::from(origin_x);
    let top = i64::from(self.y) - i64::from(origin_y);
    let fits = left >= 0
      && top >= 0
      && left + i64::from(self.width) <= i64::from(width)
      && top + i64::from(self.height) <= i64::from(height);
    fits.then_some(Region {
      x: left as i32,
      y: top as i32,
      width: self.width,
      height: self.height,
    })
  }

  pub fn center(self) -> (i32, i32) {
    (
      self.x.saturating_add((self.width / 2) as i32),
      self.y.saturating_add((self.height / 2) as i32),
    )
  }
}

/// One monitor of the layout an area was picked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Screen {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
  pub dpi: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
  region: Region,
  layout: Vec<Screen>,
}

/// Serializes read-modify-write between screenshot, video and GIF picks.
static STORE: Mutex<()> = Mutex::new(());

/// Beside the settings file. Tests use their isolated settings path, or keep nothing.
fn store_path(app: &AppHandle) -> Result<Option<PathBuf>> {
  #[cfg(test)]
  let settings = match app.test_config_path() {
    Some(path) => path,
    None => return Ok(None),
  };
  #[cfg(not(test))]
  let settings = {
    let _ = app;
    crate::config::Config::config_file().context("Failed to get config file path")?
  };
  Ok(Some(
    settings
      .parent()
      .context("Configuration path has no parent directory")?
      .join(FILE),
  ))
}

/// The current monitors, sorted so enumeration order never counts as a change.
fn current_layout() -> Vec<Screen> {
  let mut layout: Vec<Screen> = crate::monitors::list_monitors()
    .into_iter()
    .map(|monitor| Screen {
      x: monitor.x,
      y: monitor.y,
      width: monitor.width,
      height: monitor.height,
      dpi: (monitor.dpi_scale * 96.0).round() as u32,
    })
    .collect();
  layout.sort();
  layout
}

/// Remembers a picked area. Reusing it is a convenience, so a failure is logged and the capture
/// itself is unaffected.
pub fn remember(app: &AppHandle, region: Region) {
  if region.width == 0 || region.height == 0 {
    return;
  }
  let saved = Saved {
    region,
    layout: current_layout(),
  };
  let stored = store_path(app).and_then(|store| {
    let Some(store) = store else {
      return Ok(());
    };
    let _guard = STORE
      .lock()
      .map_err(|_| anyhow::anyhow!("Last region lock poisoned"))?;
    save(&store, &saved)
  });
  if let Err(error) = stored {
    tracing::warn!(%error, "Failed to remember the last capture region");
  }
}

/// The last area, when one was picked on the monitor layout in use now.
pub fn recall(app: &AppHandle) -> Option<Region> {
  let loaded = store_path(app).and_then(|store| {
    let Some(store) = store else {
      return Ok(None);
    };
    let _guard = STORE
      .lock()
      .map_err(|_| anyhow::anyhow!("Last region lock poisoned"))?;
    Ok(load(&store))
  });
  match loaded {
    Ok(saved) => saved.and_then(|saved| usable(&saved, &current_layout())),
    Err(error) => {
      tracing::warn!(%error, "Failed to read the last capture region");
      None
    },
  }
}

fn usable(saved: &Saved, layout: &[Screen]) -> Option<Region> {
  if saved.layout.as_slice() != layout {
    tracing::debug!(
      operation = "capture_last_region",
      "Monitor layout changed since the last region was picked"
    );
    return None;
  }
  let on_screen = layout
    .iter()
    .any(|screen| intersects(saved.region, *screen));
  on_screen.then_some(saved.region)
}

fn intersects(region: Region, screen: Screen) -> bool {
  let (left, top) = (i64::from(region.x), i64::from(region.y));
  let (right, bottom) = (
    left + i64::from(region.width),
    top + i64::from(region.height),
  );
  let (screen_left, screen_top) = (i64::from(screen.x), i64::from(screen.y));
  let screen_right = screen_left + i64::from(screen.width);
  let screen_bottom = screen_top + i64::from(screen.height);
  left < screen_right && right > screen_left && top < screen_bottom && bottom > screen_top
}

/// A missing file means nothing was picked yet; a damaged one is logged and ignored, since the
/// next pick rewrites it.
fn load(store: &Path) -> Option<Saved> {
  match std::fs::read(store) {
    Ok(bytes) => match serde_json::from_slice(&bytes) {
      Ok(saved) => Some(saved),
      Err(error) => {
        tracing::warn!(%error, "Last capture region unreadable; ignoring it");
        None
      },
    },
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
    Err(error) => {
      tracing::warn!(%error, "Failed to read the last capture region");
      None
    },
  }
}

fn save(store: &Path, saved: &Saved) -> Result<()> {
  let directory = store
    .parent()
    .context("Last region path has no parent directory")?;
  std::fs::create_dir_all(directory).context("Failed to create settings folder")?;
  let mut file =
    tempfile::NamedTempFile::new_in(directory).context("Failed to create last region file")?;
  file
    .write_all(&serde_json::to_vec_pretty(saved)?)
    .context("Failed to write last region")?;
  file
    .persist(store)
    .context("Failed to replace last region")?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn screen(x: i32, y: i32, width: u32, height: u32, dpi: u32) -> Screen {
    Screen {
      x,
      y,
      width,
      height,
      dpi,
    }
  }

  fn region(x: i32, y: i32, width: u32, height: u32) -> Region {
    Region {
      x,
      y,
      width,
      height,
    }
  }

  fn isolated(folder: &Path) -> AppHandle {
    let app = AppHandle::isolated(crate::config::Config::default(), folder.to_path_buf());
    app.isolate_config_path(folder.join("config.json"));
    app
  }

  #[test]
  fn an_area_converts_into_a_capture_only_when_it_fits_entirely() {
    // A monitor left of the primary one puts the capture origin at negative x.
    let area = region(-1500, 100, 400, 300);
    assert_eq!(
      area.within(-1920, 0, 3840, 1080),
      Some(region(420, 100, 400, 300))
    );
    assert_eq!(area.within(0, 0, 1920, 1080), None);
    assert_eq!(region(1700, 900, 300, 200).within(0, 0, 1920, 1080), None);
    assert_eq!(
      region(1620, 880, 300, 200).within(0, 0, 1920, 1080),
      Some(region(1620, 880, 300, 200))
    );
    assert_eq!(region(10, 10, 0, 50).within(0, 0, 1920, 1080), None);
    assert_eq!(
      region(i32::MAX, 0, u32::MAX, 1).within(0, 0, 1920, 1080),
      None
    );
  }

  #[test]
  fn a_changed_layout_or_an_off_screen_area_is_never_replayed() {
    let layout = vec![
      screen(-1920, 0, 1920, 1080, 96),
      screen(0, 0, 2560, 1440, 144),
    ];
    let saved = Saved {
      region: region(-200, 50, 400, 300),
      layout: layout.clone(),
    };
    assert_eq!(usable(&saved, &layout), Some(saved.region));
    // Scaling changed on one monitor.
    let rescaled = vec![
      screen(-1920, 0, 1920, 1080, 120),
      screen(0, 0, 2560, 1440, 144),
    ];
    assert_eq!(usable(&saved, &rescaled), None);
    // A monitor was unplugged or moved.
    assert_eq!(usable(&saved, &[screen(0, 0, 2560, 1440, 144)]), None);
    let moved = vec![
      screen(2560, 0, 1920, 1080, 96),
      screen(0, 0, 2560, 1440, 144),
    ];
    assert_eq!(usable(&saved, &moved), None);
    // Inside the layout's bounds but in a gap between monitors.
    let gap = Saved {
      region: region(0, 1500, 100, 100),
      layout: layout.clone(),
    };
    assert_eq!(usable(&gap, &layout), None);
  }

  #[test]
  fn the_area_survives_a_restart_and_a_damaged_file_is_ignored() {
    let folder = tempfile::tempdir().unwrap();
    let app = isolated(folder.path());
    assert_eq!(recall(&app), None, "nothing picked yet");

    let primary = crate::monitors::list_monitors()
      .into_iter()
      .find(|monitor| monitor.is_primary);
    let Some(primary) = primary else {
      return; // No monitor on this machine (headless CI): layout matching is covered above.
    };
    let picked = region(primary.x + 10, primary.y + 20, 300, 200);
    remember(&app, picked);
    // A new handle stands in for a restart: only the file carries the area.
    assert_eq!(recall(&isolated(folder.path())), Some(picked));

    remember(&app, region(primary.x, primary.y, 0, 0));
    assert_eq!(
      recall(&app),
      Some(picked),
      "an empty pick keeps the last area"
    );

    std::fs::write(folder.path().join(FILE), b"{not json").unwrap();
    assert_eq!(recall(&app), None);
    remember(&app, picked);
    assert_eq!(
      recall(&app),
      Some(picked),
      "the next pick rewrites a damaged file"
    );
  }

  #[test]
  fn without_a_settings_folder_nothing_is_kept() {
    let folder = tempfile::tempdir().unwrap();
    let app = AppHandle::isolated(
      crate::config::Config::default(),
      folder.path().to_path_buf(),
    );
    remember(&app, region(0, 0, 10, 10));
    assert_eq!(recall(&app), None);
    assert!(!folder.path().join(FILE).exists());
  }
}
