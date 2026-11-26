//! Interrupted native recordings. Each native session writes a marker naming its destination
//! before the worker starts; a session that never published keeps its temporary folder, and the
//! next launch saves what the engine wrote to that destination (or the next free name beside it).

use flinch_video_engine::artifact::{SESSION_DIRECTORY_PREFIX, move_no_replace};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Lives in the session's temporary folder, beside the engine's session directory.
pub const MARKER: &str = "native-recovery.json";
const PARTIAL: &str = "capture.partial.mp4";
const COLLISION_ATTEMPTS: u32 = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Marker {
  version: u32,
  destination: PathBuf,
  /// Where the engine publishes; present when the app stopped before moving it into place.
  engine_output: PathBuf,
  gif: bool,
  /// The app process that recorded it; that process never recovers its own live sessions.
  process: u32,
  /// Set once a recovery attempt failed, so later launches keep the files without retrying.
  #[serde(default)]
  failed: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Recovered {
  /// Saved at this path; the temporary folder is gone.
  Saved(PathBuf),
  /// Nothing was recorded; the temporary folder is gone.
  Empty,
  /// A process still has the recording open (a worker finishing up); retried next launch.
  InUse,
}

pub fn write_marker(engine_output: &Path, destination: &Path, gif: bool) -> std::io::Result<()> {
  let folder = engine_output
    .parent()
    .ok_or_else(|| std::io::Error::other("Recording temporary path has no folder"))?;
  std::fs::create_dir_all(folder)?;
  let marker = Marker {
    version: 1,
    destination: destination.to_path_buf(),
    engine_output: engine_output.to_path_buf(),
    gif,
    process: std::process::id(),
    failed: None,
  };
  let bytes = serde_json::to_vec_pretty(&marker).map_err(std::io::Error::other)?;
  std::fs::write(folder.join(MARKER), bytes)
}

/// Recovers every marked folder under `root` that has not already failed and was not recorded
/// by `current_process` (whose sessions may still be starting). Each result names its folder; a
/// failure is recorded in the marker before it is returned.
pub fn recover_all(
  root: &Path,
  current_process: u32,
  encode_gif: &dyn Fn(&Path, &Path) -> Result<(), String>,
) -> Vec<(PathBuf, Result<Recovered, String>)> {
  let Ok(entries) = std::fs::read_dir(root) else {
    return Vec::new();
  };
  let mut results = Vec::new();
  for entry in entries.flatten() {
    let folder = entry.path();
    let marker = match read_marker(&folder) {
      Ok(Some(marker)) if marker.failed.is_none() && marker.process != current_process => marker,
      Ok(_) => continue,
      Err(error) => {
        results.push((folder, Err(error)));
        continue;
      },
    };
    let result = recover(&folder, &marker, encode_gif);
    if let Err(reason) = &result
      && let Err(error) = record_failure(&folder, &marker, reason)
    {
      tracing::error!(%error, path = %folder.display(), "Cannot mark failed recovery");
    }
    results.push((folder, result));
  }
  results
}

fn read_marker(folder: &Path) -> Result<Option<Marker>, String> {
  let path = folder.join(MARKER);
  if !path.is_file() {
    return Ok(None);
  }
  let bytes = std::fs::read(&path).map_err(|e| format!("Read {}: {e}", path.display()))?;
  serde_json::from_slice(&bytes)
    .map(Some)
    .map_err(|e| format!("Parse {}: {e}", path.display()))
}

fn record_failure(folder: &Path, marker: &Marker, reason: &str) -> Result<(), String> {
  let mut marker = marker.clone();
  marker.failed = Some(reason.to_owned());
  let bytes = serde_json::to_vec_pretty(&marker).map_err(|e| e.to_string())?;
  std::fs::write(folder.join(MARKER), bytes).map_err(|e| e.to_string())
}

fn recover(
  folder: &Path,
  marker: &Marker,
  encode_gif: &dyn Fn(&Path, &Path) -> Result<(), String>,
) -> Result<Recovered, String> {
  let Some(source) = source(folder, marker)? else {
    remove_folder(folder)?;
    return Ok(Recovered::Empty);
  };
  if in_use(&source)? {
    return Ok(Recovered::InUse);
  }
  let parent = marker
    .destination
    .parent()
    .ok_or_else(|| format!("{} has no folder", marker.destination.display()))?;
  std::fs::create_dir_all(parent).map_err(|e| format!("Create {}: {e}", parent.display()))?;
  let target = crate::capture_artifact::next_available_path_with_limit(
    &marker.destination,
    "recording",
    Some(COLLISION_ATTEMPTS),
  );
  if target.exists() {
    return Err(format!("No free name beside {}", target.display()));
  }
  if marker.gif {
    // The palette pass reads the recovered intermediate; the GIF encoder refuses an existing target.
    let intermediate = folder.join("recovered.mp4");
    if intermediate.exists() {
      std::fs::remove_file(&intermediate)
        .map_err(|e| format!("Remove {}: {e}", intermediate.display()))?;
    }
    flinch_video_engine::recovery::recover(&source, &intermediate)
      .map_err(|e| format!("Recover {}: {e}", source.display()))?;
    encode_gif(&intermediate, &target)?;
  } else {
    // Staged beside the target, so it never appears half written and never crosses volumes.
    // Named after the session, so an attempt cut short by exit is replaced by the next one.
    let session = folder
      .file_name()
      .map(|name| name.to_string_lossy().into_owned())
      .ok_or_else(|| format!("{} has no name", folder.display()))?;
    let staged = parent.join(format!(".flinch-recovering-{session}.mp4"));
    if staged.exists() {
      std::fs::remove_file(&staged).map_err(|e| format!("Remove {}: {e}", staged.display()))?;
    }
    if let Err(error) = flinch_video_engine::recovery::recover(&source, &staged) {
      // The engine creates the destination before it can refuse the source.
      crate::capture_artifact::remove_file_best_effort(
        &staged,
        "recover_recording",
        "staged recovery",
      );
      return Err(format!("Recover {}: {error}", source.display()));
    }
    if let Err(error) = move_no_replace(&staged, &target, "recovered publish") {
      crate::capture_artifact::remove_file_best_effort(
        &staged,
        "recover_recording",
        "staged recovery",
      );
      return Err(format!("Save {}: {error}", target.display()));
    }
  }
  // The recording is saved whatever happens to the folder; a kept folder is marked so no later
  // launch saves it twice.
  if let Err(error) = remove_folder(folder) {
    tracing::warn!(%error, path = %folder.display(), "Recovered recording's temporary folder kept");
    let note = format!("Recovered to {}; {error}", target.display());
    if let Err(error) = record_failure(folder, marker, &note) {
      tracing::error!(%error, path = %folder.display(), "Cannot mark recovered folder");
    }
  }
  Ok(Recovered::Saved(target))
}

/// The engine's published file if the app never moved it, else the newest session's partial.
fn source(folder: &Path, marker: &Marker) -> Result<Option<PathBuf>, String> {
  if marker.engine_output.starts_with(folder) && marker.engine_output.is_file() {
    return Ok(Some(marker.engine_output.clone()));
  }
  let entries = std::fs::read_dir(folder).map_err(|e| format!("Read {}: {e}", folder.display()))?;
  let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
  for entry in entries {
    let entry = entry.map_err(|e| format!("Read {}: {e}", folder.display()))?;
    let is_session = entry
      .file_name()
      .to_str()
      .is_some_and(|name| name.starts_with(SESSION_DIRECTORY_PREFIX));
    let partial = entry.path().join(PARTIAL);
    if !is_session || !partial.is_file() {
      continue;
    }
    let modified = partial
      .metadata()
      .and_then(|metadata| metadata.modified())
      .map_err(|e| format!("Inspect {}: {e}", partial.display()))?;
    if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
      newest = Some((modified, partial));
    }
  }
  Ok(newest.map(|(_, path)| path))
}

/// Opening with no sharing fails while any other handle (a worker still writing) is open.
fn in_use(path: &Path) -> Result<bool, String> {
  use std::os::windows::fs::OpenOptionsExt;
  const ERROR_SHARING_VIOLATION: i32 = 32;
  match std::fs::OpenOptions::new()
    .read(true)
    .share_mode(0)
    .open(path)
  {
    Ok(_) => Ok(false),
    Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(true),
    Err(error) => Err(format!("Open {}: {error}", path.display())),
  }
}

fn remove_folder(folder: &Path) -> Result<(), String> {
  std::fs::remove_dir_all(folder).map_err(|e| format!("Remove {}: {e}", folder.display()))
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
