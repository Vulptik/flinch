//! The last few saved captures, listed in the tray menu to open, copy or show in their folder.
//! Kept in `recent-captures.json` beside the settings, newest first, so the list survives
//! restarts. A file that was moved or deleted drops off the list.

use crate::app::context::AppHandle;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How many captures the tray lists.
pub const LIMIT: usize = 5;
const FILE: &str = "recent-captures.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
  Screenshot,
  Recording,
  Gif,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentCapture {
  pub path: PathBuf,
  pub kind: CaptureKind,
  /// Unix seconds when it was saved.
  pub saved_at: i64,
}

/// Serializes read-modify-write: screenshots, recordings and GIFs can finish at the same time.
static STORE: Mutex<()> = Mutex::new(());

/// Beside the settings file. Tests use their isolated settings path, or keep no list at all.
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

/// Adds a saved capture and refreshes the tray menu. The list is a convenience, so a failure
/// is logged and the capture itself is unaffected.
pub fn record(app: &AppHandle, path: &Path, kind: CaptureKind) {
  let entry = RecentCapture {
    path: path.to_path_buf(),
    kind,
    saved_at: chrono::Utc::now().timestamp(),
  };
  let recorded = store_path(app).and_then(|store| {
    let Some(store) = store else {
      return Ok(());
    };
    let _guard = STORE
      .lock()
      .map_err(|_| anyhow::anyhow!("Recent captures lock poisoned"))?;
    record_in(&store, entry, |path| path.is_file())
  });
  if let Err(error) = recorded {
    tracing::warn!(%error, path = %path.display(), "Failed to add a recent capture");
  }
  crate::app::tray::update_tray_recording_state(app);
}

/// Captures still on disk, newest first.
pub fn list(app: &AppHandle) -> Vec<RecentCapture> {
  let loaded = store_path(app).and_then(|store| {
    let Some(store) = store else {
      return Ok(Vec::new());
    };
    let _guard = STORE
      .lock()
      .map_err(|_| anyhow::anyhow!("Recent captures lock poisoned"))?;
    Ok(load(&store))
  });
  match loaded {
    Ok(entries) => entries
      .into_iter()
      .filter(|entry| entry.path.is_file())
      .collect(),
    Err(error) => {
      tracing::warn!(%error, "Failed to read recent captures");
      Vec::new()
    },
  }
}

/// Drops one capture, for example after it was found missing.
pub fn forget(app: &AppHandle, path: &Path) {
  let forgotten = store_path(app).and_then(|store| {
    let Some(store) = store else {
      return Ok(());
    };
    let _guard = STORE
      .lock()
      .map_err(|_| anyhow::anyhow!("Recent captures lock poisoned"))?;
    let mut entries = load(&store);
    entries.retain(|entry| entry.path != path);
    save(&store, &entries)
  });
  if let Err(error) = forgotten {
    tracing::warn!(%error, "Failed to remove a recent capture");
  }
}

fn record_in(store: &Path, entry: RecentCapture, exists: impl Fn(&Path) -> bool) -> Result<()> {
  let mut entries = load(store);
  entries.retain(|old| old.path != entry.path && exists(&old.path));
  entries.insert(0, entry);
  entries.truncate(LIMIT);
  save(store, &entries)
}

/// A missing file is an empty list; a damaged one is logged and starts over, since the list is
/// rebuilt by the next capture.
fn load(store: &Path) -> Vec<RecentCapture> {
  match std::fs::read(store) {
    Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
      tracing::warn!(%error, "Recent captures list unreadable; starting a new one");
      Vec::new()
    }),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
    Err(error) => {
      tracing::warn!(%error, "Failed to read recent captures list");
      Vec::new()
    },
  }
}

fn save(store: &Path, entries: &[RecentCapture]) -> Result<()> {
  let directory = store
    .parent()
    .context("Recent captures path has no parent directory")?;
  std::fs::create_dir_all(directory).context("Failed to create settings folder")?;
  let mut file =
    tempfile::NamedTempFile::new_in(directory).context("Failed to create recent captures file")?;
  file
    .write_all(&serde_json::to_vec_pretty(entries)?)
    .context("Failed to write recent captures")?;
  file
    .persist(store)
    .context("Failed to replace recent captures")?;
  Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
  Open,
  Copy,
  Show,
  /// Screenshots only: shows it in a pin window.
  Pin,
}

/// Runs a tray action on a listed capture. Each failure is reported to the user, and a capture
/// that is no longer on disk leaves the list.
pub async fn act(app: AppHandle, capture: RecentCapture, action: Action) {
  let lang = crate::i18n::get_lang(&app);
  let labels = crate::i18n::tray(&lang);
  if !capture.path.is_file() {
    tracing::warn!(path = %capture.path.display(), "Recent capture is no longer on disk");
    forget(&app, &capture.path);
    crate::app::tray::update_tray_recording_state(&app);
    crate::notifications::show_error(&app, labels.recent, "recent_error_missing");
    return;
  }
  let (result, body_key) = match action {
    Action::Open => (
      crate::app::platform::open_target(capture.path.to_string_lossy().into_owned())
        .await
        .map_err(|error| error.to_string()),
      "recent_error_open",
    ),
    Action::Show => (
      crate::app::platform::reveal_in_folder(capture.path.clone())
        .await
        .map_err(|error| error.to_string()),
      "recent_error_folder",
    ),
    Action::Copy => (copy(&app, &capture).await, "recent_error_copy"),
    Action::Pin => (pin(&app, &capture).await, "notification_error_pin"),
  };
  match result {
    Ok(()) if action == Action::Copy => notify_copied(&app, labels, &capture.path).await,
    Ok(()) => {},
    Err(error) => {
      tracing::error!(%error, ?action, path = %capture.path.display(), "Recent capture action failed");
      crate::notifications::show_error(&app, labels.recent, body_key);
    },
  }
}

/// The copied file's name with its thumbnail, as when it was saved.
async fn notify_copied(app: &AppHandle, labels: &crate::i18n::TrayLabels, path: &Path) {
  let name = path
    .file_name()
    .map(|name| name.to_string_lossy().into_owned())
    .unwrap_or_default();
  crate::notifications::show_file(app, labels.recent_copied, &name, path.to_path_buf()).await;
}

/// The same as right after capture: screenshots as the image plus the file, or the file alone when
/// too large; recordings and GIFs as the file.
async fn copy(app: &AppHandle, capture: &RecentCapture) -> Result<(), String> {
  let large_screenshot = capture.kind == CaptureKind::Screenshot && {
    // Read only the header first: a huge screenshot is never decoded just to be refused.
    let path = capture.path.clone();
    let size = tokio::task::spawn_blocking(move || image::image_dimensions(&path))
      .await
      .map_err(|error| format!("Image size task failed: {error}"))?
      .map_err(|error| format!("Failed to read {}: {error}", capture.path.display()))?;
    !crate::commands::clipboard::fits_clipboard_image(size.0, size.1)
  };
  if capture.kind != CaptureKind::Screenshot || large_screenshot {
    return crate::commands::clipboard::copy_file_to_clipboard_with_retry(
      app,
      capture.path.clone(),
    )
    .await;
  }
  let path = capture.path.clone();
  let image =
    tokio::task::spawn_blocking(move || image::open(&path).map(|image| image.into_rgba8()))
      .await
      .map_err(|error| format!("Image decoding task failed: {error}"))?
      .map_err(|error| format!("Failed to read {}: {error}", capture.path.display()))?;
  let (width, height) = image.dimensions();
  crate::commands::clipboard::copy_screenshot_to_clipboard_with_retry(
    app,
    width,
    height,
    std::sync::Arc::new(image.into_raw()),
    capture.path.clone(),
  )
  .await
}

/// Largest screenshot a pin decodes: about four 4K screens.
const PIN_PIXELS: u64 = 64_000_000;

/// Pins a saved screenshot, decoded off the UI thread and centered on the pointer's monitor.
async fn pin(app: &AppHandle, capture: &RecentCapture) -> Result<(), String> {
  if capture.kind != CaptureKind::Screenshot {
    return Err("Only screenshots can be pinned".into());
  }
  let path = capture.path.clone();
  let image = tokio::task::spawn_blocking(move || -> Result<image::RgbaImage, String> {
    let (width, height) = image::image_dimensions(&path)
      .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if u64::from(width) * u64::from(height) > PIN_PIXELS {
      return Err(format!("{width}x{height} is too large to pin"));
    }
    image::open(&path)
      .map(|image| image.into_rgba8())
      .map_err(|error| format!("Failed to read {}: {error}", path.display()))
  })
  .await
  .map_err(|error| format!("Image decoding task failed: {error}"))??;
  let (width, height) = image.dimensions();
  crate::overlay::pin::open(
    app,
    crate::overlay::pin::PinImage {
      rgba: std::sync::Arc::new(image.into_raw()),
      width,
      height,
    },
    crate::overlay::pin::Placement::Centered,
  );
  Ok(())
}

/// The folder the newest capture's kind saves to now, including the current subfolder when one
/// exists; the saving folder before any subfolder otherwise (no subfolder, or none created yet for
/// this period). The screenshots folder when nothing is listed yet.
pub fn captures_folder(
  config: &crate::config::Config,
  newest: Option<&RecentCapture>,
  exists: impl Fn(&Path) -> bool,
) -> PathBuf {
  use crate::config::CaptureType;
  use flinch_settings::edit::FolderTarget;
  let kind = newest.map(|capture| capture.kind);
  let current = config.resolve_capture_path(match kind {
    Some(CaptureKind::Recording) => CaptureType::Video,
    Some(CaptureKind::Gif) => CaptureType::Gif,
    _ => CaptureType::Screenshot,
  });
  if exists(&current) {
    return current;
  }
  let target = match (config.save_location_mode, kind) {
    (crate::config::SaveLocationMode::SingleFolder, _) => FolderTarget::Together,
    (_, Some(CaptureKind::Recording)) => FolderTarget::Videos,
    _ => FolderTarget::Screenshots,
  };
  config.folder_root(target)
}

/// Opens the captures folder from the tray, reporting a folder that cannot be opened.
pub async fn open_folder(app: AppHandle) {
  let newest = list(&app).into_iter().next();
  let config = crate::config::Config::current(&app);
  let folder = match crate::app::runtime::spawn_blocking(move || {
    captures_folder(&config, newest.as_ref(), Path::is_dir)
  })
  .await
  {
    Ok(folder) => folder,
    Err(error) => {
      tracing::error!(%error, "Failed to resolve the captures folder");
      let labels = crate::i18n::tray(&crate::i18n::get_lang(&app));
      crate::notifications::show_error(&app, labels.recent, "recent_error_folder");
      return;
    },
  };
  if let Err(error) = crate::app::platform::open_target(folder.to_string_lossy().into_owned()).await
  {
    tracing::error!(%error, folder = %folder.display(), "Failed to open the captures folder");
    let labels = crate::i18n::tray(&crate::i18n::get_lang(&app));
    crate::notifications::show_error(&app, labels.recent, "recent_error_folder");
  }
}

/// Longest file name a menu shows before shortening it in the middle.
const NAME_LIMIT: usize = 44;

/// The file name as a menu shows it: shortened in the middle when long, so the start (date) and
/// the end (app, type and extension) stay readable, with `&` escaped so Windows does not turn it
/// into a keyboard shortcut.
pub fn menu_name(path: &Path) -> String {
  let name = path
    .file_name()
    .map(|name| name.to_string_lossy().into_owned())
    .unwrap_or_else(|| path.display().to_string());
  let characters: Vec<char> = name.chars().collect();
  let name = if characters.len() > NAME_LIMIT {
    let head = (NAME_LIMIT - 1) / 2;
    let tail = NAME_LIMIT - 1 - head;
    format!(
      "{}…{}",
      characters[..head].iter().collect::<String>(),
      characters[characters.len() - tail..]
        .iter()
        .collect::<String>()
    )
  } else {
    name
  };
  name.replace('&', "&&")
}

/// The menu label's time: the time alone for today, otherwise the short date too, both in the
/// user's Windows format.
pub fn saved_label(saved_at: i64) -> String {
  use chrono::{Datelike, Local, TimeZone, Timelike};
  let Some(saved) = Local.timestamp_opt(saved_at, 0).single() else {
    return String::new();
  };
  let time = windows::Win32::Foundation::SYSTEMTIME {
    wYear: saved.year() as u16,
    wMonth: saved.month() as u16,
    wDay: saved.day() as u16,
    wHour: saved.hour() as u16,
    wMinute: saved.minute() as u16,
    ..Default::default()
  };
  let clock = format_windows(|buffer| {
    // SAFETY: Windows writes at most buffer.len() UTF-16 units into the live buffer.
    unsafe {
      windows::Win32::Globalization::GetTimeFormatEx(
        windows::core::PCWSTR::null(),
        windows::Win32::Globalization::TIME_NOSECONDS,
        Some(&time),
        None,
        Some(buffer),
      )
    }
  })
  .unwrap_or_else(|| format!("{:02}:{:02}", saved.hour(), saved.minute()));
  if saved.date_naive() == Local::now().date_naive() {
    return clock;
  }
  let date = format_windows(|buffer| {
    // SAFETY: Windows writes at most buffer.len() UTF-16 units into the live buffer.
    unsafe {
      windows::Win32::Globalization::GetDateFormatEx(
        windows::core::PCWSTR::null(),
        windows::Win32::Globalization::DATE_SHORTDATE,
        Some(&time),
        None,
        Some(buffer),
        None,
      )
    }
  })
  .unwrap_or_else(|| saved.format("%Y-%m-%d").to_string());
  format!("{date} {clock}")
}

fn format_windows(call: impl FnOnce(&mut [u16]) -> i32) -> Option<String> {
  let mut buffer = [0u16; 64];
  let written = call(&mut buffer);
  if written <= 1 {
    tracing::warn!("Windows could not format a capture time");
    return None;
  }
  Some(String::from_utf16_lossy(&buffer[..written as usize - 1]))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(name: &str, kind: CaptureKind, saved_at: i64) -> RecentCapture {
    RecentCapture {
      path: PathBuf::from(name),
      kind,
      saved_at,
    }
  }

  #[test]
  fn newest_first_without_duplicates_and_at_most_the_limit() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join(FILE);
    for index in 0..7 {
      record_in(
        &store,
        entry(&format!("{index}.png"), CaptureKind::Screenshot, index),
        |_| true,
      )
      .unwrap();
    }
    record_in(&store, entry("4.png", CaptureKind::Screenshot, 9), |_| true).unwrap();
    let names: Vec<_> = load(&store)
      .into_iter()
      .map(|entry| entry.path.display().to_string())
      .collect();
    assert_eq!(names, ["4.png", "6.png", "5.png", "3.png", "2.png"]);
  }

  #[test]
  fn deleted_files_drop_off_and_every_kind_round_trips() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join(FILE);
    record_in(&store, entry("gone.mp4", CaptureKind::Recording, 1), |_| {
      true
    })
    .unwrap();
    record_in(&store, entry("kept.gif", CaptureKind::Gif, 2), |_| true).unwrap();
    record_in(
      &store,
      entry("new.png", CaptureKind::Screenshot, 3),
      |path| path != Path::new("gone.mp4"),
    )
    .unwrap();
    assert_eq!(
      load(&store),
      [
        entry("new.png", CaptureKind::Screenshot, 3),
        entry("kept.gif", CaptureKind::Gif, 2),
      ]
    );
  }

  #[test]
  fn a_missing_or_damaged_list_starts_empty_and_the_next_capture_repairs_it() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join(FILE);
    assert!(load(&store).is_empty());
    std::fs::write(&store, b"{not json").unwrap();
    assert!(load(&store).is_empty());
    record_in(&store, entry("a.png", CaptureKind::Screenshot, 1), |_| true).unwrap();
    assert_eq!(load(&store).len(), 1);
  }

  #[test]
  fn the_captures_folder_follows_the_newest_capture_and_the_saving_mode() {
    let mut config = crate::config::Config::default();
    config.custom_screenshots_folder = Some(r"C:Shots".into());
    config.custom_videos_folder = Some(r"C:Clips".into());
    config.custom_single_folder = Some(r"C:All".into());
    config.save_location_mode = crate::config::SaveLocationMode::SystemFolders;
    let recording = entry("a.mp4", CaptureKind::Recording, 1);
    let gif = entry("a.gif", CaptureKind::Gif, 1);
    let all = |_: &Path| true;
    assert_eq!(
      captures_folder(&config, None, all),
      PathBuf::from(r"C:Shots")
    );
    assert_eq!(
      captures_folder(&config, Some(&gif), all),
      PathBuf::from(r"C:Shots")
    );
    assert_eq!(
      captures_folder(&config, Some(&recording), all),
      PathBuf::from(r"C:Clips")
    );
    config.save_location_mode = crate::config::SaveLocationMode::SingleFolder;
    config.single_folder_subfolder_template = Some(String::new());
    assert_eq!(
      captures_folder(&config, Some(&recording), all),
      PathBuf::from(r"C:All")
    );
  }

  #[test]
  fn the_captures_folder_opens_the_current_subfolder_when_it_exists() {
    let config = crate::config::Config {
      save_location_mode: crate::config::SaveLocationMode::SingleFolder,
      custom_single_folder: Some(r"C:All".into()),
      single_folder_subfolder_template: Some("Captures".into()),
      ..Default::default()
    };
    let shot = entry("a.png", CaptureKind::Screenshot, 1);
    let subfolder = PathBuf::from(r"C:All").join("Captures");
    // The old behavior opened the root even with the subfolder in place.
    assert_eq!(
      captures_folder(&config, Some(&shot), |path| path == subfolder),
      subfolder
    );
    // A subfolder not created yet (a new month before its first capture) is not created
    // just to be opened; the root opens instead.
    assert_eq!(
      captures_folder(&config, Some(&shot), |_| false),
      PathBuf::from(r"C:All")
    );
    let dated = crate::config::Config {
      single_folder_subfolder_template: None,
      ..config
    };
    let opened = captures_folder(&dated, None, |_| true);
    assert_eq!(opened.parent(), Some(Path::new(r"C:All")));
    assert_ne!(opened, PathBuf::from(r"C:All"));
  }

  #[tokio::test]
  async fn a_missing_capture_leaves_the_isolated_list_and_is_reported() {
    let folder = tempfile::tempdir().unwrap();
    let config = crate::config::Config {
      language: "en".into(),
      ..Default::default()
    };
    let app = AppHandle::isolated(config, PathBuf::new());
    app.isolate_config_path(folder.path().join("config.json"));
    let kept = folder.path().join("kept.png");
    std::fs::write(&kept, b"png").unwrap();
    let gone = folder.path().join("gone.png");
    std::fs::write(&gone, b"png").unwrap();
    record(&app, &kept, CaptureKind::Screenshot);
    record(&app, &gone, CaptureKind::Screenshot);
    assert_eq!(list(&app).len(), 2);
    std::fs::remove_file(&gone).unwrap();
    assert_eq!(list(&app).len(), 1, "a deleted file is never listed");
    act(
      app.clone(),
      entry(&gone.to_string_lossy(), CaptureKind::Screenshot, 2),
      Action::Open,
    )
    .await;
    assert_eq!(load(&folder.path().join(FILE)).len(), 1);
    let missing =
      crate::i18n::native_settings(&crate::i18n::get_lang(&app), "recent_error_missing").unwrap();
    let requests = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .expect("notifications")
      .take_test_requests();
    assert!(
      requests.iter().any(|request| request.body == missing),
      "{:?}",
      requests
        .iter()
        .map(|r| (&r.title, &r.body))
        .collect::<Vec<_>>()
    );
  }

  #[tokio::test]
  async fn only_a_readable_screenshot_can_be_pinned_and_failures_are_reported() {
    let folder = tempfile::tempdir().unwrap();
    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        ..Default::default()
      },
      PathBuf::new(),
    );
    let video = folder.path().join("clip.mp4");
    std::fs::write(&video, b"mp4").unwrap();
    let damaged = folder.path().join("damaged.png");
    std::fs::write(&damaged, b"not a png").unwrap();
    for capture in [
      entry(&video.to_string_lossy(), CaptureKind::Recording, 1),
      entry(&damaged.to_string_lossy(), CaptureKind::Screenshot, 2),
    ] {
      act(app.clone(), capture, Action::Pin).await;
    }
    let failed =
      crate::i18n::native_settings(&crate::i18n::get_lang(&app), "notification_error_pin").unwrap();
    let requests = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .expect("notifications")
      .take_test_requests();
    assert_eq!(
      requests
        .iter()
        .filter(|request| request.body == failed)
        .count(),
      2
    );
  }

  /// The isolated app has no clipboard, so every copy fails before reaching the system one.
  #[tokio::test]
  async fn a_failed_copy_of_every_kind_is_reported_and_keeps_the_capture_listed() {
    let folder = tempfile::tempdir().unwrap();
    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        ..Default::default()
      },
      PathBuf::new(),
    );
    app.isolate_config_path(folder.path().join("config.json"));
    let screenshot = folder.path().join("shot.png");
    image::RgbaImage::from_pixel(8, 6, image::Rgba([1, 2, 3, 255]))
      .save(&screenshot)
      .unwrap();
    let damaged = folder.path().join("damaged.png");
    std::fs::write(&damaged, b"not a png").unwrap();
    let recording = folder.path().join("clip.mp4");
    std::fs::write(&recording, b"mp4").unwrap();
    let gif = folder.path().join("loop.gif");
    std::fs::write(&gif, b"gif").unwrap();
    let cases = [
      (&screenshot, CaptureKind::Screenshot),
      (&damaged, CaptureKind::Screenshot),
      (&recording, CaptureKind::Recording),
      (&gif, CaptureKind::Gif),
    ];
    for (path, kind) in cases {
      record(&app, path, kind);
    }
    let failed =
      crate::i18n::native_settings("en", "recent_error_copy").expect("copy failure text");
    let title = crate::i18n::tray("en").recent;
    let notifications = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .expect("notifications");
    for (path, kind) in cases {
      assert!(
        copy(&app, &entry(&path.to_string_lossy(), kind, 1))
          .await
          .is_err()
      );
      act(
        app.clone(),
        entry(&path.to_string_lossy(), kind, 1),
        Action::Copy,
      )
      .await;
      let requests = notifications.take_test_requests();
      assert_eq!(requests.len(), 1, "{path:?}");
      assert_eq!(requests[0].title, title);
      assert_eq!(requests[0].body, failed);
      assert!(
        requests[0].critical,
        "failures bypass notification preferences"
      );
    }
    let damaged_error = copy(
      &app,
      &entry(&damaged.to_string_lossy(), CaptureKind::Screenshot, 1),
    )
    .await
    .unwrap_err();
    assert!(damaged_error.contains("damaged.png"), "{damaged_error}");
    let unavailable = copy(
      &app,
      &entry(&recording.to_string_lossy(), CaptureKind::Recording, 1),
    )
    .await
    .unwrap_err();
    assert!(
      unavailable.contains("Clipboard is unavailable"),
      "{unavailable}"
    );
    // A failed action never drops a capture that is still on disk.
    assert_eq!(list(&app).len(), cases.len());
    forget(&app, &gif);
    assert_eq!(list(&app).len(), cases.len() - 1);
    assert!(list(&app).iter().all(|capture| capture.path != gif));
  }

  #[test]
  fn without_an_isolated_settings_path_nothing_is_kept() {
    let folder = tempfile::tempdir().unwrap();
    let app = AppHandle::isolated(crate::config::Config::default(), PathBuf::new());
    let path = folder.path().join("a.png");
    std::fs::write(&path, b"png").unwrap();
    record(&app, &path, CaptureKind::Screenshot);
    forget(&app, &path);
    assert!(list(&app).is_empty());
    assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 1);
  }

  #[tokio::test]
  async fn a_copied_capture_notifies_with_its_name_and_thumbnail() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder
      .path()
      .join("2026-11-08_14-32-05_Discord_screenshot.png");
    image::RgbaImage::from_pixel(48, 27, image::Rgba([47, 217, 47, 255]))
      .save(&path)
      .unwrap();
    let app = AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        notifications_enabled: true,
        ..Default::default()
      },
      PathBuf::new(),
    );
    let labels = crate::i18n::tray("en");
    notify_copied(&app, labels, &path).await;
    let requests = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .expect("notifications")
      .take_test_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].title, labels.recent_copied);
    assert_eq!(
      requests[0].body,
      "2026-11-08_14-32-05_Discord_screenshot.png"
    );
    let thumbnail = requests[0]
      .thumbnail
      .as_ref()
      .expect("the copied capture's thumbnail");
    assert_eq!(&thumbnail.rgba[..4], &[47, 217, 47, 255]);
  }

  #[test]
  fn menu_names_keep_both_ends_of_long_names_and_escape_ampersands() {
    assert_eq!(
      menu_name(Path::new(
        "Shots/2026-11-08_14-32-05_Discord_screenshot.png"
      )),
      "2026-11-08_14-32-05_Discord_screenshot.png"
    );
    let long = menu_name(Path::new(
      "Shots/2026-11-08_14-32-05_Visual Studio Code - flinch_screenshot.png",
    ));
    assert_eq!(long.chars().count(), NAME_LIMIT);
    assert!(long.starts_with("2026-11-08_14-32-05_"), "{long}");
    assert!(long.ends_with("_screenshot.png"), "{long}");
    assert!(long.contains('…'));
    assert_eq!(menu_name(Path::new("Tom & Jerry.gif")), "Tom && Jerry.gif");
    assert_eq!(
      menu_name(Path::new("日本語のアプリ.mp4")),
      "日本語のアプリ.mp4"
    );
  }

  /// A settings "folder" that is really a file: every read and write of the list fails, and the
  /// capture itself is never affected; the tray just shows nothing.
  #[test]
  fn an_unwritable_list_is_logged_and_never_blocks_a_capture() {
    let folder = tempfile::tempdir().unwrap();
    let blocker = folder.path().join("not-a-folder");
    std::fs::write(&blocker, b"file").unwrap();
    let app = AppHandle::isolated(crate::config::Config::default(), PathBuf::new());
    app.isolate_config_path(blocker.join("config.json"));
    let capture = folder.path().join("a.png");
    std::fs::write(&capture, b"png").unwrap();
    record(&app, &capture, CaptureKind::Screenshot);
    assert!(list(&app).is_empty());
    forget(&app, &capture);
    assert!(capture.exists());
    assert_eq!(std::fs::read(&blocker).unwrap(), b"file");
    // A store that cannot be read (a folder in its place) is an empty list, not an error.
    let as_folder = folder.path().join("store");
    std::fs::create_dir(&as_folder).unwrap();
    assert!(load(&as_folder).is_empty());
    assert!(save(Path::new(""), &[]).is_err());
  }

  #[test]
  fn names_and_times_fall_back_when_windows_cannot_format_them() {
    assert_eq!(menu_name(Path::new("..")), "..");
    assert_eq!(saved_label(i64::MAX), "");
    assert_eq!(format_windows(|_| 0), None);
    assert_eq!(format_windows(|_| 1), None);
    assert_eq!(
      format_windows(|buffer| {
        buffer[..3].copy_from_slice(&[u16::from(b'1'), u16::from(b'2'), 0]);
        3
      })
      .as_deref(),
      Some("12")
    );
  }

  #[test]
  fn labels_show_the_time_today_and_the_date_before() {
    let now = chrono::Utc::now().timestamp();
    let today = saved_label(now);
    assert!(today.chars().any(|c| c.is_ascii_digit()), "{today}");
    let older = saved_label(now - 3 * 24 * 60 * 60);
    assert!(older.len() > today.len(), "{older} vs {today}");
  }
}
