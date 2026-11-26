// src/commands/window.rs
// Window management commands

use crate::app::context::AppHandle;
use crate::error::ErrorExt;

fn is_valid_settings_section(section: &str) -> bool {
  crate::settings::Section::from_id(section).is_some()
}

pub(crate) fn show_config_window_with_section(
  app: &AppHandle,
  section: &str,
) -> Result<(), String> {
  crate::commands::ensure_known_value(
    is_valid_settings_section(section),
    "settings section",
    section,
  )?;

  #[cfg(feature = "native-settings")]
  return crate::settings::native_host::show(app, crate::settings::Section::from_id(section));
}

/// Open a capture output folder in Explorer, creating it first if it doesn't exist.
/// Only accepts absolute local paths; rejects UNC paths and relative paths.
pub async fn open_capture_folder(_app: AppHandle, path: String) -> Result<(), String> {
  crate::commands::ensure_absolute_non_unc_path(&path, "open_capture_folder")?;
  let folder = path.clone();
  crate::app::runtime::spawn_blocking(move || {
    std::fs::create_dir_all(std::path::Path::new(&folder)).map_err(|e| {
      crate::commands::ipc_context("open_capture_folder: failed to create directory", e)
    })?;
    Ok::<_, String>(())
  })
  .await
  .map_err(|e| crate::commands::ipc_context("open_capture_folder worker", e))??;
  crate::app::platform::open_target(path)
    .await
    .map_err(|error| error.to_rich_string())
}

/// Open a URL or file-system path using the OS default handler.
/// Used by the notification window to open saved files and folders.
///
/// Only allows `https://` URLs and absolute local filesystem paths that actually
/// exist on disk, plus the exact Flinch Store review URI. All other URI schemes
/// (e.g. `ms-msdt:`, `search-ms:`) are
/// rejected to prevent OS-level command execution from external targets.
pub async fn open_path(_app: AppHandle, path: String) -> Result<(), String> {
  crate::commands::ensure_safe_open_target(&path, "open_path")?;

  crate::app::platform::open_target(path)
    .await
    .map_err(|error| error.to_rich_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn settings_navigation_accepts_only_current_sections() {
    for section in [
      "general",
      "hotkeys",
      "screenshot",
      "video",
      "notifications",
      "files",
      "about",
    ] {
      assert!(is_valid_settings_section(section));
    }
    for section in ["debug", "stress-test", "unknown"] {
      assert!(!is_valid_settings_section(section));
    }
  }

  #[test]
  fn safe_open_path_allows_https_and_existing_absolute_local_paths() {
    let temp = tempfile::NamedTempFile::new().expect("temp file should be created");
    let temp_path = temp.path().to_string_lossy().into_owned();

    assert!(
      crate::commands::ensure_safe_open_target("https://example.com/file", "open_path").is_ok()
    );
    assert!(crate::commands::ensure_safe_open_target(&temp_path, "open_path").is_ok());
  }

  #[test]
  fn safe_open_path_only_allows_flinch_store_review() {
    assert!(
      crate::commands::ensure_safe_open_target(
        "ms-windows-store://review/?ProductId=9MVHXWLW9N69",
        "open_path"
      )
      .is_ok()
    );
    for target in [
      "ms-windows-store://review/?ProductId=OTHER",
      "ms-windows-store://pdp/?ProductId=9MVHXWLW9N69",
      "ms-windows-store://review/?ProductId=9MVHXWLW9N69&extra=value",
    ] {
      assert!(crate::commands::ensure_safe_open_target(target, "open_path").is_err());
    }
  }

  #[test]
  fn safe_open_path_rejects_unsupported_targets() {
    assert!(
      crate::commands::ensure_safe_open_target("http://example.com/file", "open_path").is_err()
    );
    assert!(crate::commands::ensure_safe_open_target("ms-settings:display", "open_path").is_err());
    assert!(crate::commands::ensure_safe_open_target("relative\\path.txt", "open_path").is_err());
    assert!(
      crate::commands::ensure_safe_open_target(r"\\server\share\file.txt", "open_path").is_err()
    );
  }
}
