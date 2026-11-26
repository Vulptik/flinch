use super::*;

/// A folder mounted as a drive letter, so unplugging needs no hardware.
struct VirtualDrive {
  letter: char,
  folder: std::path::PathBuf,
}

impl VirtualDrive {
  fn name(&self) -> String {
    format!("{}:", self.letter)
  }
  fn plug(&self) {
    let status = std::process::Command::new("subst")
      .arg(self.name())
      .arg(&self.folder)
      .status()
      .unwrap();
    assert!(status.success(), "mount {}", self.name());
  }
  fn unplug(&self) {
    let status = std::process::Command::new("subst")
      .args([self.name().as_str(), "/d"])
      .status()
      .unwrap();
    assert!(status.success(), "unmount {}", self.name());
  }
}

impl Drop for VirtualDrive {
  fn drop(&mut self) {
    if std::path::Path::new(&format!("{}\\", self.name())).exists() {
      self.unplug();
    }
  }
}

fn capture(path: std::path::PathBuf) -> CaptureResult {
  CaptureResult {
    path,
    width: 2,
    height: 2,
    rgba: [10u8, 20, 30, 255].repeat(4),
    format: "png".into(),
    quality: 90,
    source_app: None,
    pin: None,
  }
}

async fn save(app: &AppHandle, config: &Config, folder: &std::path::Path) -> Result<(), String> {
  let titles = crate::i18n::screenshot_notif_titles("en");
  copy_rgba_and_maybe_save_with(
    app,
    capture(folder.join("capture.png")),
    None,
    config,
    "fixture",
    &titles,
    |_, _| async { Ok(()) },
  )
  .await
  .map(|_| ())
}

#[tokio::test]
#[ignore = "mounts a temporary drive letter: cargo xtask test:settings:desktop"]
async fn a_custom_folder_survives_its_drive_being_unplugged_and_plugged_back() {
  crate::app::runtime::test_runtime();
  let backing = tempfile::tempdir().unwrap();
  let drive = VirtualDrive {
    letter: crate::config::unused_drive_letter(),
    folder: backing.path().to_path_buf(),
  };
  drive.plug();
  let root = format!("{}\\", drive.name());
  crate::commands::config::check_capture_folder(std::path::Path::new(&root))
    .expect("a connected drive is accepted");
  let config = Config {
    language: "en".into(),
    save_location_mode: crate::config::SaveLocationMode::SingleFolder,
    custom_single_folder: Some(root.clone()),
    ..Default::default()
  };
  let app = AppHandle::isolated(config.clone(), std::path::PathBuf::new());
  let folder = config.resolve_capture_path(crate::config::CaptureType::Screenshot);
  assert!(folder.starts_with(&root));
  let saved = backing.path().join(folder.strip_prefix(&root).unwrap());
  let files = || std::fs::read_dir(&saved).map_or(0, Iterator::count);

  save(&app, &config, &folder).await.expect("connected save");
  assert_eq!(files(), 1);

  drive.unplug();
  assert!(save(&app, &config, &folder).await.is_err());
  assert_eq!(files(), 1, "an unplugged drive saves nothing");
  let titles = crate::i18n::screenshot_notif_titles("en");
  let requests = app
    .try_state::<crate::overlay::notification::NotificationManager>()
    .unwrap()
    .take_test_requests();
  assert_eq!(
    requests.last().map(|request| request.title.as_str()),
    Some(titles.save_failed)
  );
  assert!(
    crate::commands::open_capture_folder(app.clone(), folder.to_string_lossy().into_owned())
      .await
      .is_err()
  );

  drive.plug();
  save(&app, &config, &folder)
    .await
    .expect("reconnected save");
  assert_eq!(files(), 2, "saving resumes in the same folder");
}
