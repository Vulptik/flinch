use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const APP_TEMP_DIR_NAME: &str = "Flinch";
const RECORDING_TEMP_ROOT_NAME: &str = "recordings";

pub fn next_available_path(path: &Path, fallback_stem: &str) -> PathBuf {
  next_available_path_with_limit(path, fallback_stem, None)
}

pub fn next_available_path_with_limit(
  path: &Path,
  fallback_stem: &str,
  max_attempts: Option<u32>,
) -> PathBuf {
  if !path.exists() {
    return path.to_path_buf();
  }

  let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
  let stem = path
    .file_stem()
    .map(|s| s.to_string_lossy().to_string())
    .or_else(|| path.file_name().map(|s| s.to_string_lossy().to_string()))
    .unwrap_or_else(|| fallback_stem.to_string());
  let ext = path.extension().map(|e| e.to_string_lossy().to_string());

  let mut attempt = 1u32;
  loop {
    if let Some(limit) = max_attempts
      && attempt > limit
    {
      tracing::error!(
        operation = "next_available_path",
        path = %path.display(),
        attempts = limit,
        "Exhausted filename collision attempts"
      );
      return path.to_path_buf();
    }

    let filename = match &ext {
      Some(ext) if !ext.is_empty() => format!("{stem} ({attempt}).{ext}"),
      _ => format!("{stem} ({attempt})"),
    };
    let candidate = parent.join(filename);
    if !candidate.exists() {
      return candidate;
    }

    attempt = attempt.saturating_add(1);
  }
}

pub fn temp_marker_path(path: &Path, marker: &str, fallback_stem: &str) -> PathBuf {
  let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
  let stem = path
    .file_stem()
    .map(|s| s.to_string_lossy().to_string())
    .or_else(|| path.file_name().map(|s| s.to_string_lossy().to_string()))
    .unwrap_or_else(|| fallback_stem.to_string());
  let ext = path.extension().map(|e| e.to_string_lossy().to_string());

  let file_name = match ext {
    Some(ext) if !ext.is_empty() => format!("{stem}{marker}.{ext}"),
    _ => format!("{stem}{marker}"),
  };

  parent.join(file_name)
}

pub fn recording_temp_root() -> PathBuf {
  temp_base()
    .join(APP_TEMP_DIR_NAME)
    .join(RECORDING_TEMP_ROOT_NAME)
}

#[cfg(not(test))]
fn temp_base() -> PathBuf {
  std::env::temp_dir()
}

// Failure-path tests keep sessions on purpose, as production does for recovery. Each test
// process gets its own base so they never reach the app's startup recovery scan.
#[cfg(test)]
fn temp_base() -> PathBuf {
  static BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
  BASE
    .get_or_init(|| {
      let runs = std::env::temp_dir().join(TEST_TEMP_DIR_NAME);
      prune_test_runs(&runs, SystemTime::now(), TEST_RUN_RETENTION);
      let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
      runs.join(format!("{}-{nanos}", std::process::id()))
    })
    .clone()
}

#[cfg(test)]
const TEST_TEMP_DIR_NAME: &str = "flinch-test-runs";
// Longer than any test process, including the ninety-minute recording timeline.
#[cfg(test)]
const TEST_RUN_RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[cfg(test)]
fn prune_test_runs(runs: &Path, now: SystemTime, retention: std::time::Duration) -> usize {
  let Ok(entries) = std::fs::read_dir(runs) else {
    return 0;
  };
  let mut removed = 0;
  for entry in entries.flatten() {
    let stale = entry
      .metadata()
      .and_then(|metadata| metadata.modified())
      .ok()
      .and_then(|modified| now.duration_since(modified).ok())
      .is_some_and(|age| age > retention);
    if stale && std::fs::remove_dir_all(entry.path()).is_ok() {
      removed += 1;
    }
  }
  removed
}

pub fn recording_session_temp_dir(
  final_output_path: &Path,
  fallback_stem: &str,
  session_prefix: &str,
) -> PathBuf {
  let stem = final_output_path
    .file_stem()
    .map(|s| s.to_string_lossy().to_string())
    .or_else(|| {
      final_output_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
    })
    .unwrap_or_else(|| fallback_stem.to_string());
  let timestamp_nanos = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_nanos())
    .unwrap_or(0);
  let pid = std::process::id();

  recording_temp_root().join(format!(
    "{}-{}-{}-{}",
    sanitize_temp_component(session_prefix, "session"),
    sanitize_temp_component(&stem, fallback_stem),
    pid,
    timestamp_nanos
  ))
}

pub fn remove_recording_session_temp_dir(temp_output_path: &Path, operation: &str) {
  let Some(session_dir) = temp_output_path.parent() else {
    return;
  };
  let root = recording_temp_root();
  if !session_dir.starts_with(&root) || session_dir == root {
    return;
  }
  if !session_dir.exists() {
    return;
  }

  if let Err(error) = std::fs::remove_dir_all(session_dir) {
    tracing::warn!(
      operation = operation,
      path = %session_dir.display(),
      error = %error,
      "Failed to remove recording temp session directory"
    );
  }

  remove_dir_if_empty_best_effort(&root, operation, "recording temp root");
  if let Some(app_temp_dir) = root.parent() {
    remove_dir_if_empty_best_effort(app_temp_dir, operation, "app temp directory");
  }
}

pub fn remove_file_best_effort(path: &Path, operation: &str, purpose: &str) -> bool {
  match std::fs::remove_file(path) {
    Ok(()) => true,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
    Err(error) => {
      tracing::warn!(
        operation = operation,
        path = %path.display(),
        error = %error,
        purpose,
        "Best-effort file cleanup failed"
      );
      false
    },
  }
}

pub fn remove_dir_if_empty_best_effort(path: &Path, operation: &str, purpose: &str) -> bool {
  match std::fs::remove_dir(path) {
    Ok(()) => true,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
    Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => false,
    Err(error) => {
      tracing::debug!(
        operation = operation,
        path = %path.display(),
        error = %error,
        purpose,
        "Best-effort empty directory cleanup failed"
      );
      false
    },
  }
}

fn sanitize_temp_component(value: &str, fallback: &str) -> String {
  let sanitized: String = value
    .chars()
    .map(|ch| {
      if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
        ch
      } else {
        '_'
      }
    })
    .take(80)
    .collect();
  let trimmed = sanitized.trim_matches('_');
  if trimmed.is_empty() {
    fallback.to_string()
  } else {
    trimmed.to_string()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn temp_marker_path_preserves_extension() {
    let path = PathBuf::from(r"C:\captures\recording.mp4");
    assert_eq!(
      temp_marker_path(&path, ".flinch-part", "recording"),
      PathBuf::from(r"C:\captures\recording.flinch-part.mp4")
    );
  }

  #[test]
  fn recording_session_temp_dir_uses_os_temp_root() {
    let path = PathBuf::from(r"C:\captures\recording.mp4");
    let session_dir = recording_session_temp_dir(&path, "recording", "video");

    assert!(session_dir.starts_with(recording_temp_root()));
    assert!(
      session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("video-recording-"))
    );
  }

  #[test]
  fn test_sessions_never_reach_the_app_recovery_root() {
    let app_root = std::env::temp_dir()
      .join(APP_TEMP_DIR_NAME)
      .join(RECORDING_TEMP_ROOT_NAME);
    let root = recording_temp_root();

    assert!(!root.starts_with(&app_root), "{}", root.display());
    assert!(root.starts_with(std::env::temp_dir().join(TEST_TEMP_DIR_NAME)));
    assert_eq!(root, recording_temp_root());
  }

  #[test]
  fn stale_test_runs_are_pruned_and_recent_ones_kept() {
    let runs = tempfile::tempdir().expect("runs dir");
    let old = runs.path().join("1-1");
    let recent = runs.path().join("2-2");
    std::fs::create_dir_all(old.join("video-existing-1-1")).expect("old run");
    std::fs::create_dir_all(&recent).expect("recent run");
    let hour = std::time::Duration::from_secs(60 * 60);

    assert_eq!(prune_test_runs(runs.path(), SystemTime::now(), hour), 0);
    let later = SystemTime::now() + 2 * hour;
    // FILE_WRITE_ATTRIBUTES with FILE_FLAG_BACKUP_SEMANTICS: a directory cannot be opened for writing.
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
      .access_mode(0x100)
      .custom_flags(0x0200_0000)
      .open(&recent)
      .and_then(|dir| dir.set_modified(later))
      .expect("touch recent run");
    assert_eq!(prune_test_runs(runs.path(), later + hour / 2, hour), 1);
    assert!(!old.exists());
    assert!(recent.exists());
    assert_eq!(
      prune_test_runs(&runs.path().join("missing"), later, hour),
      0
    );
  }

  #[test]
  fn temp_session_component_sanitizer_has_stable_fallbacks() {
    assert_eq!(
      sanitize_temp_component("Code - Insiders", "recording"),
      "Code_-_Insiders"
    );
    assert_eq!(sanitize_temp_component("///", "recording"), "recording");
  }

  #[test]
  fn collisions_get_windows_style_numbers_and_a_limit_returns_the_original() {
    let folder = tempfile::tempdir().expect("folder");
    let image = folder.path().join("image.png");
    assert_eq!(next_available_path(&image, "capture"), image);
    std::fs::write(&image, b"1").expect("first");
    std::fs::write(folder.path().join("image (1).png"), b"2").expect("second");
    assert_eq!(
      next_available_path(&image, "capture"),
      folder.path().join("image (2).png")
    );
    // No extension keeps the bare numbered stem.
    let bare = folder.path().join("capture");
    std::fs::write(&bare, b"3").expect("bare");
    assert_eq!(
      next_available_path(&bare, "capture"),
      folder.path().join("capture (1)")
    );
    // Exhausting the attempts hands back the original path instead of looping forever.
    assert_eq!(
      next_available_path_with_limit(&image, "capture", Some(1)),
      image
    );
    // A path without a file name falls back to the given stem.
    assert_eq!(
      next_available_path(Path::new(".."), "capture"),
      PathBuf::from("capture (1)")
    );
  }

  #[test]
  fn marker_paths_without_an_extension_or_name_use_the_fallback() {
    assert_eq!(
      temp_marker_path(Path::new(r"C:\captures\clip"), ".part", "recording"),
      PathBuf::from(r"C:\captures\clip.part")
    );
    assert_eq!(
      temp_marker_path(Path::new(".."), ".part", "recording"),
      PathBuf::from("recording.part")
    );
    let session = recording_session_temp_dir(Path::new(".."), "recording", "gif");
    assert!(
      session
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("gif-recording-"))
    );
  }

  #[test]
  fn session_cleanup_leaves_anything_outside_its_own_session_alone() {
    let outside = tempfile::tempdir().expect("outside");
    let file = outside.path().join("keep.mp4");
    std::fs::write(&file, b"keep").expect("outside file");
    remove_recording_session_temp_dir(&file, "test_cleanup");
    assert!(
      file.exists(),
      "a folder outside the temp root is never removed"
    );
    // The root itself and a session that is already gone are both no-ops.
    remove_recording_session_temp_dir(&recording_temp_root().join("x.mp4"), "test_cleanup");
    let gone = recording_session_temp_dir(Path::new(r"C:\c\gone.mp4"), "recording", "video");
    remove_recording_session_temp_dir(&gone.join("gone.mp4"), "test_cleanup");
    assert!(!gone.exists());
    remove_recording_session_temp_dir(Path::new(""), "test_cleanup");
  }

  #[test]
  fn best_effort_cleanup_reports_only_real_failures() {
    let folder = tempfile::tempdir().expect("folder");
    let file = folder.path().join("part.mp4");
    std::fs::write(&file, b"part").expect("file");
    assert!(remove_file_best_effort(&file, "test", "part"));
    assert!(remove_file_best_effort(&file, "test", "already gone"));
    // A directory cannot be removed as a file.
    assert!(!remove_file_best_effort(folder.path(), "test", "directory"));

    let empty = folder.path().join("empty");
    std::fs::create_dir(&empty).expect("empty");
    assert!(remove_dir_if_empty_best_effort(&empty, "test", "empty"));
    assert!(remove_dir_if_empty_best_effort(
      &empty,
      "test",
      "already gone"
    ));
    let full = folder.path().join("full");
    std::fs::create_dir(&full).expect("full");
    std::fs::write(full.join("file"), b"x").expect("content");
    assert!(!remove_dir_if_empty_best_effort(&full, "test", "not empty"));
    assert!(full.exists());
    // A file in place of the directory is another failure, reported and kept.
    let not_dir = folder.path().join("not-a-dir");
    std::fs::write(&not_dir, b"x").expect("file");
    assert!(!remove_dir_if_empty_best_effort(&not_dir, "test", "file"));
    assert!(not_dir.exists());
  }

  #[test]
  fn remove_recording_session_temp_dir_removes_only_session_dir() {
    let session_dir = recording_session_temp_dir(
      &PathBuf::from(r"C:\captures\recording.mp4"),
      "recording",
      "video",
    );
    let temp_file = session_dir.join("recording.flinch-part.mp4");
    std::fs::create_dir_all(&session_dir).expect("session temp dir should be created");
    std::fs::write(&temp_file, b"temp").expect("temp file should be written");

    remove_recording_session_temp_dir(&temp_file, "test_cleanup");

    assert!(!session_dir.exists());
  }
}
