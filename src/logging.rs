use std::{ffi::OsStr, fs, path::PathBuf, sync::OnceLock, time::SystemTime};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
  EnvFilter, Registry,
  fmt::{self, time::ChronoLocal},
  layer::SubscriberExt,
  util::SubscriberInitExt,
};

// ============================================================================
// Constants
// ============================================================================

/// Application name derived from Cargo.toml package name at compile time.
const APP_NAME: &str = env!("CARGO_PKG_NAME");

/// Maximum number of log files to retain per category (current and older log names).
const MAX_LOG_FILES: usize = 10;
/// Explicit upper bound for in-memory backend log buffering before records are dropped.
const LOG_BUFFERED_LINES_LIMIT: usize = 2048;

/// Log level filter directives to suppress noisy third-party crates.
/// Format: "crate_path=level"
const NOISE_FILTER_DIRECTIVES: &[&str] = &[
  // Suppress verbose wgpu/naga logs (keep warnings and errors)
  "wgpu_hal=warn",
  "wgpu_core=warn",
  "naga=warn",
  // Suppress symphonia INFO logs — their compile-time source paths embed the
  // developer's cargo registry path (e.g. C:\Users\...) in user-facing log files.
  "symphonia_bundle_mp3=warn",
  "symphonia_core=warn",
  "symphonia_format_ogg=warn",
];

// ============================================================================
// Static State
// ============================================================================

/// Guard that keeps the async file writer alive for the application lifetime.
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// The backend file opened for this process, before Windows filesystem redirection.
static BACKEND_LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

// ============================================================================
// Helper Functions
// ============================================================================

/// Returns the lowercase app name for consistent file/directory naming.
fn app_name_lowercase() -> String {
  APP_NAME.to_lowercase()
}

/// Returns the log directory path, creating it if necessary when `create` is true.
fn get_log_dir(create: bool) -> Result<PathBuf> {
  let log_dir = match crate::probe_profile::folder() {
    Some(profile) => profile,
    None => dirs::config_dir()
      .ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?
      .join(app_name_lowercase()),
  }
  .join("logs");

  if create {
    fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
  }

  Ok(log_dir)
}

/// Resolve and verify the log directory for the native diagnostics presenter.
pub fn support_log_directory() -> Result<PathBuf> {
  let file = BACKEND_LOG_FILE
    .get()
    .ok_or_else(|| anyhow::anyhow!("Backend logging is not initialized"))?;
  physical_log_directory(file)
}

fn physical_log_directory(file: &std::path::Path) -> Result<PathBuf> {
  // On Windows canonicalize queries the opened file handle's final path. Resolve
  // the active file, not a merged AppData directory that may also exist outside MSIX.
  let physical = fs::canonicalize(file).context("Failed to resolve active log file")?;
  let directory = physical
    .parent()
    .ok_or_else(|| anyhow::anyhow!("Active log file has no parent directory"))?;
  let path = directory
    .to_str()
    .ok_or_else(|| anyhow::anyhow!("Log directory is not valid Unicode"))?;
  Ok(shell_directory_path(path))
}

fn shell_directory_path(path: &str) -> PathBuf {
  // Explorer expects ordinary DOS/UNC paths rather than canonicalize's verbatim prefix.
  if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
    PathBuf::from(format!(r"\\{unc}"))
  } else {
    PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
  }
}

/// Generates a timestamped log filename with the given prefix.
/// Format: `{app_name}_{prefix}_{YYYYMMDD_HHMMSS}.log`
fn make_log_filename(prefix: Option<&str>) -> String {
  let app_name = app_name_lowercase();
  let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");

  match prefix {
    Some(p) => format!("{}_{}_{}.log", app_name, p, timestamp),
    None => format!("{}_{}.log", app_name, timestamp),
  }
}

/// Builds an environment filter with default level and noise suppression.
fn build_env_filter() -> EnvFilter {
  // debug → dev builds, info → release
  let default_level = if cfg!(debug_assertions) {
    "debug"
  } else {
    "info"
  };
  let mut filter =
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

  for directive_str in NOISE_FILTER_DIRECTIVES {
    if let Ok(directive) = directive_str.parse() {
      filter = filter.add_directive(directive);
    }
  }

  overlay_diagnostics_filter(
    filter,
    std::env::var("FLINCH_OVERLAY_DIAGNOSTICS").as_deref() == Ok("1"),
  )
}

fn overlay_diagnostics_filter(filter: EnvFilter, enabled: bool) -> EnvFilter {
  if enabled {
    // The explicit diagnostic opt-in must survive an inherited RUST_LOG=warn/off.
    // Keep the override scoped to diagnostics, not all overlay or application logs.
    match "flinch::overlay::performance=info".parse() {
      Ok(directive) => filter.add_directive(directive),
      Err(error) => {
        eprintln!("Failed to enable overlay diagnostic logging: {error}");
        filter
      },
    }
  } else {
    filter
  }
}

// ============================================================================
// Log Cleanup
// ============================================================================

/// Removes old log files, keeping only the most recent `MAX_LOG_FILES` per category.
/// Older versions wrote a second `frontend` log series; keep pruning it after upgrades.
pub fn cleanup_old_logs() {
  let log_dir = match get_log_dir(false) {
    Ok(dir) if dir.exists() => dir,
    _ => return,
  };
  cleanup_logs_in(&log_dir);
}

fn cleanup_logs_in(log_dir: &std::path::Path) {
  let entries: Vec<_> = match fs::read_dir(log_dir) {
    Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
    Err(_) => return,
  };

  let app_name = app_name_lowercase();
  let frontend_prefix = format!("{}_frontend_", app_name);
  let backend_prefix = format!("{}_", app_name);

  let mut frontend_logs: Vec<(PathBuf, SystemTime)> = Vec::new();
  let mut backend_logs: Vec<(PathBuf, SystemTime)> = Vec::new();

  for entry in entries {
    let path = entry.path();
    if !path.is_file() || path.extension() != Some(OsStr::new("log")) {
      continue;
    }

    let modified = fs::metadata(&path)
      .and_then(|m| m.modified())
      .unwrap_or(SystemTime::UNIX_EPOCH);

    let filename = match path.file_name().and_then(|n| n.to_str()) {
      Some(name) => name.to_string(),
      None => continue,
    };

    if filename.starts_with(&frontend_prefix) {
      frontend_logs.push((path, modified));
    } else if filename.starts_with(&backend_prefix) {
      backend_logs.push((path, modified));
    }
  }

  prune_log_list(&mut backend_logs);
  prune_log_list(&mut frontend_logs);
}

/// Sorts logs by modification time (newest first) and deletes everything past `MAX_LOG_FILES`.
fn prune_log_list(logs: &mut [(PathBuf, SystemTime)]) {
  if logs.len() <= MAX_LOG_FILES {
    return;
  }
  // Most recent first
  logs.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));

  for (path, _) in logs.iter().skip(MAX_LOG_FILES) {
    if let Err(e) = fs::remove_file(path) {
      // Best-effort — don't fail startup over stale logs
      eprintln!("Failed to remove old log file {}: {}", path.display(), e);
    }
  }
}

// ============================================================================
// Backend Logging
// ============================================================================

/// Initializes the logging subsystem.
///
/// - File: RFC3339 timestamps, no ANSI colors, includes filename (all builds)
/// - Stdout: RFC3339 timestamps, ANSI colors, includes file:line (debug builds only)
/// - Default level: DEBUG in debug builds, WARN in release (override with RUST_LOG)
pub fn init_logging() -> Result<()> {
  let log_dir = get_log_dir(true)?;
  init_logging_in(log_dir)
}

fn init_logging_in(log_dir: PathBuf) -> Result<()> {
  let log_filename = make_log_filename(None);
  let log_file_path = log_dir.join(&log_filename);

  // Set up non-blocking file appender
  let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
    .rotation(tracing_appender::rolling::Rotation::NEVER)
    .filename_prefix(&log_filename)
    .build(&log_dir)
    .context("Failed to create backend log file")?;
  let (non_blocking, guard) = NonBlockingBuilder::default()
    .buffered_lines_limit(LOG_BUFFERED_LINES_LIMIT)
    .lossy(true)
    .finish(file_appender);

  // Store guard to prevent premature drop (would stop file logging)
  LOG_GUARD
    .set(guard)
    .map_err(|_| anyhow::anyhow!("Logging already initialized"))?;
  BACKEND_LOG_FILE
    .set(log_file_path.clone())
    .map_err(|_| anyhow::anyhow!("Backend log path already initialized"))?;

  // File layer: no colors, full context
  let file_layer = fmt::layer()
    .with_writer(non_blocking) // Write to file
    .with_ansi(false) // No colors
    .with_timer(ChronoLocal::rfc_3339()) // RFC3339 timestamps
    .with_target(false) // No target
    .with_file(true) // File name
    .with_line_number(false); // No line number

  // Initialize the global subscriber — stdout only in debug builds
  // (release builds have no console window; stdout output is pure overhead)
  #[cfg(debug_assertions)]
  {
    let stdout_layer = fmt::layer()
      .with_writer(std::io::stdout)
      .with_ansi(true)
      .with_timer(ChronoLocal::rfc_3339())
      .with_target(true)
      .with_file(true)
      .with_line_number(true);

    Registry::default()
      .with(build_env_filter())
      .with(file_layer)
      .with(stdout_layer)
      .try_init()
      .context("Failed to initialize logging subscriber")?;
  }

  #[cfg(not(debug_assertions))]
  {
    Registry::default()
      .with(build_env_filter())
      .with(file_layer)
      .try_init()
      .context("Failed to initialize logging subscriber")?;
  }

  tracing::info!(
    operation = "logging_init",
    log_file = %log_file_path.display(),
    buffered_lines_limit = LOG_BUFFERED_LINES_LIMIT,
    "Logging initialized"
  );

  if std::env::var("FLINCH_OVERLAY_DIAGNOSTICS").as_deref() == Ok("1") {
    tracing::info!(target: "flinch::overlay::performance",
      "Overlay diagnostic logging active; verify this record before collecting measurements");
  }

  Ok(())
}

/// Returns the path to the most recently modified log file.
pub fn get_log_file_path() -> Result<PathBuf> {
  let log_dir = get_log_dir(false)?;
  log_file_path_in(&log_dir)
}

fn log_file_path_in(log_dir: &std::path::Path) -> Result<PathBuf> {
  let mut log_files: Vec<PathBuf> = fs::read_dir(log_dir)
    .context("Failed to read log directory")?
    .filter_map(|entry| {
      let path = entry.ok()?.path();
      let is_log = path.is_file() && path.extension() == Some(OsStr::new("log"));
      is_log.then_some(path)
    })
    .collect();

  // Sort by modification time, most recent first
  log_files.sort_by_key(|path| {
    fs::metadata(path)
      .and_then(|m| m.modified())
      .unwrap_or(SystemTime::UNIX_EPOCH)
  });
  log_files.reverse();

  log_files
    .into_iter()
    .next()
    .ok_or_else(|| anyhow::anyhow!("No log files found"))
}

// ============================================================================
// Native logging
// ============================================================================

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn logger_initialization_uses_isolated_processes_and_reports_existing_subscriber() {
    use std::os::windows::process::CommandExt;
    for conflict in 0..3 {
      let folder = tempfile::tempdir().unwrap();
      let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
          "--exact",
          "logging::tests::isolated_logger_child",
          "--nocapture",
        ])
        .env("FLINCH_TEST_LOG_PATH", folder.path())
        .env("RUST_LOG", "info")
        .env("FLINCH_TEST_LOG_CONFLICT", conflict.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000)
        .spawn()
        .unwrap();
      let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
      loop {
        if child.try_wait().unwrap().is_some() {
          break;
        }
        if std::time::Instant::now() >= deadline {
          child.kill().unwrap();
          panic!("isolated logging test exceeded deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
      }
      let output = child.wait_with_output().unwrap();
      assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
      );
    }
  }

  #[test]
  fn isolated_logger_child() {
    let Some(path) = std::env::var_os("FLINCH_TEST_LOG_PATH") else {
      return;
    };
    let folder = PathBuf::from(path);
    if std::env::var("FLINCH_TEST_LOG_CONFLICT").as_deref() == Ok("2") {
      let invalid = folder.join("file-is-not-a-directory");
      fs::write(&invalid, b"preserved file").unwrap();
      let result = std::panic::catch_unwind(|| init_logging_in(invalid.clone()));
      assert!(
        result
          .expect("log-file creation failure must return an error instead of panicking")
          .is_err()
      );
      assert_eq!(fs::read(invalid).unwrap(), b"preserved file");
      return;
    }
    if std::env::var("FLINCH_TEST_LOG_CONFLICT").as_deref() == Ok("1") {
      tracing::subscriber::set_global_default(Registry::default()).unwrap();
      let result = std::panic::catch_unwind(|| init_logging_in(folder));
      assert!(
        result
          .expect("logging must return an error instead of panicking on an existing subscriber")
          .is_err()
      );
    } else {
      init_logging_in(folder.clone()).unwrap();
      tracing::info!("isolated logging regression marker");
      assert_eq!(
        support_log_directory().unwrap(),
        shell_directory_path(fs::canonicalize(&folder).unwrap().to_str().unwrap())
      );
      let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
      loop {
        let contents = fs::read_to_string(log_file_path_in(&folder).unwrap()).unwrap();
        if contents.contains("isolated logging regression marker") {
          break;
        }
        assert!(
          std::time::Instant::now() < deadline,
          "file writer failed to publish its record"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
      }
      assert!(
        init_logging_in(folder)
          .unwrap_err()
          .to_string()
          .contains("already initialized")
      );
    }
  }

  #[test]
  fn log_retention_prunes_each_owned_category_and_selects_latest_file_in_an_isolated_directory() {
    let folder = tempfile::tempdir().unwrap();
    assert!(
      log_file_path_in(folder.path())
        .unwrap_err()
        .to_string()
        .contains("No log files")
    );
    for prefix in ["flinch_", "flinch_frontend_", "other_"] {
      for index in 0..12 {
        let path = folder.path().join(format!("{prefix}{index:02}.log"));
        let file = fs::File::create(path).unwrap();
        file
          .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100 + index))
          .unwrap();
      }
    }
    let retained = folder.path().join("unrelated.txt");
    fs::write(&retained, b"preserved").unwrap();
    let directory = folder.path().join("flinch_directory.log");
    fs::create_dir(&directory).unwrap();
    let latest = folder.path().join("flinch_latest.log");
    let file = fs::File::create(&latest).unwrap();
    file
      .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000))
      .unwrap();
    drop(file);
    assert_eq!(log_file_path_in(folder.path()).unwrap(), latest);
    cleanup_logs_in(folder.path());
    for prefix in ["flinch_", "flinch_frontend_", "other_"] {
      for index in 0..12 {
        let path = folder.path().join(format!("{prefix}{index:02}.log"));
        let pruned = if prefix == "flinch_" {
          index < 3
        } else if prefix == "flinch_frontend_" {
          index < 2
        } else {
          false
        };
        assert_eq!(path.exists(), !pruned, "{}", path.display());
      }
    }
    assert!(directory.is_dir());
    assert_eq!(fs::read(retained).unwrap(), b"preserved");
    assert_eq!(log_file_path_in(folder.path()).unwrap(), latest);
    cleanup_logs_in(&folder.path().join("missing"));
    assert!(
      log_file_path_in(&folder.path().join("missing"))
        .unwrap_err()
        .to_string()
        .contains("read log directory")
    );
  }

  #[test]
  fn shell_log_paths_preserve_dos_unc_and_unicode() {
    assert_eq!(
      shell_directory_path(r"\\?\C:\Users\René\logs"),
      PathBuf::from(r"C:\Users\René\logs")
    );
    assert_eq!(
      shell_directory_path(r"\\?\UNC\server\share\logs"),
      PathBuf::from(r"\\server\share\logs")
    );
    assert_eq!(shell_directory_path(r"C:\logs"), PathBuf::from(r"C:\logs"));
  }

  #[test]
  fn support_directory_requires_an_existing_active_file() {
    let folder = tempfile::tempdir().expect("log folder");
    let file = folder.path().join("active.log");
    assert!(physical_log_directory(&file).is_err());
    fs::write(&file, "active backend log").expect("write log");
    let resolved = physical_log_directory(&file).expect("physical directory");
    assert_eq!(
      fs::canonicalize(resolved).expect("resolved directory"),
      fs::canonicalize(folder.path()).expect("original directory")
    );
  }

  #[test]
  fn overlay_diagnostics_reach_file_despite_warn_filter() {
    let output = tempfile::NamedTempFile::new().expect("diagnostic log file");
    let writer = output.reopen().expect("diagnostic log writer");
    let subscriber = fmt::Subscriber::builder()
      .with_env_filter(overlay_diagnostics_filter(EnvFilter::new("warn"), true))
      .with_ansi(false)
      .with_writer(move || writer.try_clone().expect("clone log writer"))
      .finish();
    tracing::subscriber::with_default(subscriber, || {
      tracing::info!(target: "flinch::overlay::performance", "diagnostic probe");
      tracing::info!(target: "flinch::overlay::selection_host", "ordinary info probe");
    });
    let content = fs::read_to_string(output.path()).expect("read diagnostic log");
    assert!(content.contains("diagnostic probe"));
    assert!(!content.contains("ordinary info probe"));
  }

  #[test]
  fn test_app_name_constant() {
    assert!(!APP_NAME.is_empty());
    assert_eq!(APP_NAME, env!("CARGO_PKG_NAME"));
  }

  #[test]
  fn test_app_name_lowercase() {
    let name = app_name_lowercase();
    assert!(!name.is_empty());
    assert_eq!(name, name.to_lowercase());
  }

  #[test]
  fn test_noise_filter_directives() {
    assert!(!NOISE_FILTER_DIRECTIVES.is_empty());

    for directive in NOISE_FILTER_DIRECTIVES {
      assert!(directive.contains('='));
      assert!(!directive.is_empty());
    }
  }

  #[test]
  fn test_make_log_filename_no_prefix() {
    let filename = make_log_filename(None);
    assert!(filename.ends_with(".log"));
    assert!(filename.contains(&app_name_lowercase()));
  }

  #[test]
  fn test_make_log_filename_with_prefix() {
    let filename = make_log_filename(Some("backend"));
    assert!(filename.ends_with(".log"));
    assert!(filename.contains("backend"));
    assert!(filename.contains(&app_name_lowercase()));
  }

  #[test]
  fn test_log_filename_format() {
    let filename = make_log_filename(Some("test"));
    // Format: {app}_test_{timestamp}.log
    let parts: Vec<&str> = filename.split('_').collect();
    assert!(parts.len() >= 3);
  }

  #[test]
  fn test_get_log_dir_no_create() {
    let result = get_log_dir(false);
    // Should succeed even if dir doesn't exist when create=false
    if let Ok(path) = result {
      assert!(path.to_string_lossy().contains("logs"));
    }
  }

  #[test]
  fn test_prune_log_list_under_limit() {
    let now = SystemTime::now();
    let mut logs = vec![
      (PathBuf::from("/tmp/a.log"), now),
      (PathBuf::from("/tmp/b.log"), now),
    ];
    // Should not remove anything when under MAX_LOG_FILES
    prune_log_list(&mut logs);
    assert_eq!(logs.len(), 2);
  }

  #[test]
  fn test_prune_log_list_over_limit() {
    use std::time::Duration;

    let tmp_dir = std::env::temp_dir().join("flinch_prune_test");
    let _ = fs::create_dir_all(&tmp_dir);

    let mut logs = Vec::new();
    // Create MAX_LOG_FILES + 3 files
    for i in 0..(MAX_LOG_FILES + 3) {
      let path = tmp_dir.join(format!("test_{}.log", i));
      let _ = fs::File::create(&path);
      let modified = SystemTime::now() + Duration::from_millis(i as u64 * 10);
      logs.push((path, modified));
    }

    prune_log_list(&mut logs);

    // Only MAX_LOG_FILES newest should remain on disk
    let remaining: Vec<_> = fs::read_dir(&tmp_dir)
      .into_iter()
      .flatten()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension() == Some(OsStr::new("log")))
      .collect();
    assert_eq!(remaining.len(), MAX_LOG_FILES);

    // Cleanup
    let _ = fs::remove_dir_all(&tmp_dir);
  }
}
