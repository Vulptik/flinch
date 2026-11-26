// src/config/backup.rs
// Automatic settings backups: one copy per app version, read when config.json cannot be parsed

use super::Config;
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const DIRECTORY: &str = "backups";
const PREFIX: &str = "config-";
const SUFFIX: &str = ".json";
const LIMIT: usize = 5;

fn directory(config_path: &Path) -> Result<PathBuf> {
  Ok(
    config_path
      .parent()
      .context("Configuration path has no parent directory")?
      .join(DIRECTORY),
  )
}

/// Copy the saved bytes on the first launch of `version`. Later launches of the
/// same version keep that copy. Settings that do not parse are never backed up.
pub(super) fn back_up_for_version(config_path: &Path, version: &str) -> Result<Option<PathBuf>> {
  let directory = directory(config_path)?;
  let target = directory.join(format!("{PREFIX}{version}{SUFFIX}"));
  if target
    .try_exists()
    .context("Failed to check settings backup")?
  {
    return Ok(None);
  }
  let bytes = match fs::read(config_path) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(error).context("Failed to read settings for backup"),
  };
  if let Err(error) = serde_json::from_slice::<Config>(&bytes) {
    tracing::warn!(
      operation = "backup_config",
      path = %config_path.display(),
      error = %error,
      "Skipping settings backup because the saved settings do not parse"
    );
    return Ok(None);
  }
  fs::create_dir_all(&directory).context("Failed to create settings backup directory")?;
  let mut file =
    tempfile::NamedTempFile::new_in(&directory).context("Failed to create settings backup")?;
  file
    .write_all(&bytes)
    .context("Failed to write settings backup")?;
  file
    .as_file()
    .sync_all()
    .context("Failed to flush settings backup")?;
  file
    .persist(&target)
    .map_err(|error| error.error)
    .context("Failed to retain settings backup")?;
  prune(&directory)?;
  tracing::info!(
    operation = "backup_config",
    path = %target.display(),
    "Saved settings backup for this version"
  );
  Ok(Some(target))
}

fn prune(directory: &Path) -> Result<()> {
  for stale in newest_first(directory)?.into_iter().skip(LIMIT) {
    fs::remove_file(&stale)
      .with_context(|| format!("Failed to remove old settings backup {}", stale.display()))?;
  }
  Ok(())
}

fn newest_first(directory: &Path) -> Result<Vec<PathBuf>> {
  let entries = match fs::read_dir(directory) {
    Ok(entries) => entries,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(error).context("Failed to list settings backups"),
  };
  let mut backups = Vec::new();
  for entry in entries {
    let entry = entry.context("Failed to list settings backups")?;
    let name = entry.file_name();
    let name = name.to_string_lossy();
    if !name.starts_with(PREFIX) || !name.ends_with(SUFFIX) {
      continue;
    }
    let metadata = entry
      .metadata()
      .context("Failed to read settings backup details")?;
    if metadata.is_file() {
      let modified = metadata
        .modified()
        .context("Failed to read settings backup time")?;
      backups.push((modified, entry.path()));
    }
  }
  backups.sort_by(|left, right| right.cmp(left));
  Ok(backups.into_iter().map(|(_, path)| path).collect())
}

/// The newest backup this version can parse. Unusable backups are logged and skipped.
pub(super) fn newest_valid(config_path: &Path) -> Option<(Config, PathBuf)> {
  let backups = directory(config_path).and_then(|directory| newest_first(&directory));
  let backups = match backups {
    Ok(backups) => backups,
    Err(error) => {
      tracing::warn!(
        operation = "restore_config_backup",
        path = %config_path.display(),
        error = %error,
        "Cannot read settings backups"
      );
      return None;
    },
  };
  backups.into_iter().find_map(|path| {
    let parsed = fs::read(&path)
      .map_err(anyhow::Error::from)
      .and_then(|bytes| serde_json::from_slice::<Config>(&bytes).map_err(anyhow::Error::from));
    match parsed {
      Ok(config) => Some((config, path)),
      Err(error) => {
        tracing::warn!(
          operation = "restore_config_backup",
          path = %path.display(),
          error = %error,
          "Skipping unusable settings backup"
        );
        None
      },
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{Duration, SystemTime};

  fn saved(language: &str) -> Vec<u8> {
    serde_json::to_vec(&Config {
      language: language.into(),
      ..Config::default()
    })
    .unwrap()
  }

  fn age(path: &Path, seconds: u64) {
    fs::File::options()
      .write(true)
      .open(path)
      .unwrap()
      .set_modified(SystemTime::now() - Duration::from_secs(seconds))
      .unwrap();
  }

  #[test]
  fn first_launch_of_a_version_keeps_the_exact_bytes_and_later_launches_keep_that_copy() {
    let folder = tempfile::tempdir().unwrap();
    let config = folder.path().join("config.json");
    let original = saved("fr");
    fs::write(&config, &original).unwrap();

    let backup = back_up_for_version(&config, "1.2.0").unwrap().unwrap();
    assert_eq!(backup.file_name().unwrap(), "config-1.2.0.json");
    assert_eq!(fs::read(&backup).unwrap(), original);

    fs::write(&config, saved("de")).unwrap();
    assert!(back_up_for_version(&config, "1.2.0").unwrap().is_none());
    assert_eq!(fs::read(&backup).unwrap(), original);
    assert_eq!(fs::read_dir(backup.parent().unwrap()).unwrap().count(), 1);
  }

  #[test]
  fn missing_or_damaged_settings_are_not_backed_up() {
    let folder = tempfile::tempdir().unwrap();
    let config = folder.path().join("config.json");
    assert!(back_up_for_version(&config, "1.2.0").unwrap().is_none());
    fs::write(&config, b"{broken preferences").unwrap();
    assert!(back_up_for_version(&config, "1.2.0").unwrap().is_none());
    assert!(!folder.path().join(DIRECTORY).exists());
  }

  #[test]
  fn unreadable_settings_report_an_error() {
    let folder = tempfile::tempdir().unwrap();
    assert!(back_up_for_version(folder.path(), "1.2.0").is_err());
  }

  #[test]
  fn only_the_newest_backups_are_kept() {
    let folder = tempfile::tempdir().unwrap();
    let config = folder.path().join("config.json");
    let directory = folder.path().join(DIRECTORY);
    fs::create_dir(&directory).unwrap();
    for version in 0..6_u64 {
      let path = directory.join(format!("config-1.0.{version}.json"));
      fs::write(&path, saved("en")).unwrap();
      age(&path, 100 - version);
    }
    let unrelated = directory.join("notes.txt");
    fs::write(&unrelated, b"kept").unwrap();
    fs::write(&config, saved("en")).unwrap();

    back_up_for_version(&config, "1.2.0").unwrap().unwrap();

    let kept = newest_first(&directory).unwrap();
    assert_eq!(kept.len(), LIMIT);
    assert_eq!(kept[0].file_name().unwrap(), "config-1.2.0.json");
    assert!(!directory.join("config-1.0.0.json").exists());
    assert!(!directory.join("config-1.0.1.json").exists());
    assert!(directory.join("config-1.0.2.json").exists());
    assert!(unrelated.exists());
  }

  #[test]
  fn newest_valid_skips_unusable_backups() {
    let folder = tempfile::tempdir().unwrap();
    let config = folder.path().join("config.json");
    assert!(newest_valid(&config).is_none());
    let directory = folder.path().join(DIRECTORY);
    fs::create_dir(&directory).unwrap();
    let older = directory.join("config-1.0.0.json");
    let usable = directory.join("config-1.1.0.json");
    let damaged = directory.join("config-1.2.0.json");
    fs::write(&older, saved("de")).unwrap();
    fs::write(&usable, saved("fr")).unwrap();
    fs::write(&damaged, b"{broken").unwrap();
    fs::create_dir(directory.join("config-folder.json")).unwrap();
    age(&older, 30);
    age(&usable, 20);
    age(&damaged, 10);

    let (restored, source) = newest_valid(&config).unwrap();
    assert_eq!(restored.language, "fr");
    assert_eq!(source, usable);

    fs::remove_file(&older).unwrap();
    fs::remove_file(&usable).unwrap();
    assert!(newest_valid(&config).is_none());
  }
}
