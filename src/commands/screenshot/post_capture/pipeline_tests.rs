use super::*;

fn app(enabled: bool) -> AppHandle {
  AppHandle::isolated(
    Config {
      language: "en".into(),
      notifications_enabled: enabled,
      notification_sound: false,
      ..Default::default()
    },
    std::path::PathBuf::new(),
  )
}
fn result(path: std::path::PathBuf) -> CaptureResult {
  CaptureResult {
    path,
    width: 2,
    height: 2,
    rgba: [10u8, 20, 30, 255].repeat(4),
    format: "png".into(),
    quality: 90,
    source_app: Some("fixture".into()),
    pin: None,
  }
}
fn notices(app: &AppHandle) -> Vec<crate::overlay::notification::NotificationRequest> {
  app
    .try_state::<crate::overlay::notification::NotificationManager>()
    .unwrap()
    .take_test_requests()
}

#[tokio::test]
async fn an_offline_custom_folder_fails_the_capture_and_never_saves_elsewhere() {
  crate::app::runtime::test_runtime();
  let config = Config {
    language: "en".into(),
    save_location_mode: crate::config::SaveLocationMode::SingleFolder,
    custom_single_folder: Some(format!("{}:\\Flinch", crate::config::unused_drive_letter())),
    ..Default::default()
  };
  let folder = config.resolve_capture_path(crate::config::CaptureType::Screenshot);
  assert!(folder.starts_with(config.custom_single_folder.as_deref().unwrap()));
  let app = app(true);
  let titles = crate::i18n::screenshot_notif_titles("en");
  let copied = std::sync::atomic::AtomicBool::new(false);
  let outcome = copy_rgba_and_maybe_save_with(
    &app,
    result(folder.join("capture.png")),
    None,
    &config,
    "fixture",
    &titles,
    |_, _| {
      copied.store(true, std::sync::atomic::Ordering::SeqCst);
      async { Ok(()) }
    },
  )
  .await;
  assert!(outcome.is_err());
  assert!(!copied.load(std::sync::atomic::Ordering::SeqCst));
  let requests = notices(&app);
  assert_eq!(requests.len(), 1);
  assert_eq!(requests[0].title, titles.save_failed);
  assert!(
    crate::commands::open_capture_folder(app.clone(), folder.to_string_lossy().into_owned())
      .await
      .is_err()
  );
}

#[tokio::test]
async fn save_finishes_before_clipboard_and_success_reports_the_real_noncolliding_file() {
  let dir = tempfile::tempdir().unwrap();
  let original = dir.path().join("capture.png");
  std::fs::write(&original, b"preserve this").unwrap();
  let app = app(true);
  let config = Config::current(&app);
  let titles = crate::i18n::screenshot_notif_titles("en");
  let response = copy_rgba_and_maybe_save_with(
    &app,
    result(original.clone()),
    Some("fixture".into()),
    &config,
    "fixture",
    &titles,
    |pixels, path| async move {
      assert_eq!(path.file_name().unwrap(), "capture (1).png");
      let decoded = image::open(&path).unwrap().to_rgba8();
      assert_eq!(decoded.dimensions(), (2, 2));
      assert_eq!(decoded.as_raw(), pixels.as_ref());
      Ok(())
    },
  )
  .await
  .unwrap();
  assert!(response.success && response.saved == Some(true));
  assert_eq!(response.source_app.as_deref(), Some("fixture"));
  assert_eq!(
    response.path.as_deref(),
    dir.path().join("capture (1).png").to_str()
  );
  assert_eq!(std::fs::read(original).unwrap(), b"preserve this");
  let requests = notices(&app);
  assert_eq!(requests.len(), 1);
  assert_eq!(requests[0].title, titles.copied);
  assert!(requests[0].thumbnail.is_some());
}

#[tokio::test]
async fn a_large_png_capture_is_saved_copied_and_reported_as_its_jpg() {
  let dir = tempfile::tempdir().unwrap();
  // An existing JPG of the same name is kept; the switched file takes the next free name.
  std::fs::write(dir.path().join("capture.jpg"), b"preserve this").unwrap();
  let app = app(true);
  let config = Config {
    screenshot_large_png_as_jpg: true,
    ..Config::current(&app)
  };
  let titles = crate::i18n::screenshot_notif_titles("en");
  let (width, height) = (1024u32, 1024u32);
  let mut state = 0x9e37_79b9_7f4a_7c15u64;
  let mut rgba = Vec::with_capacity((width * height * 4) as usize);
  for y in 0..height {
    for x in 0..width {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      let noise = (state >> 56) as u8;
      rgba.extend_from_slice(&[
        ((x / 4) as u8).wrapping_add(noise / 8),
        (y / 4) as u8,
        noise / 2,
        255,
      ]);
    }
  }
  let capture = CaptureResult {
    width,
    height,
    rgba,
    ..result(dir.path().join("capture.png"))
  };
  let expected = dir.path().join("capture (1).jpg");
  let copied = expected.clone();
  let response = copy_rgba_and_maybe_save_with(
    &app,
    capture,
    None,
    &config,
    "fixture",
    &titles,
    |_, path| async move {
      assert_eq!(path, copied);
      Ok(())
    },
  )
  .await
  .unwrap();
  assert_eq!(response.path.as_deref(), expected.to_str());
  assert!(std::fs::read(&expected).unwrap().starts_with(&[0xFF, 0xD8]));
  assert!(!dir.path().join("capture.png").exists());
  assert_eq!(
    std::fs::read(dir.path().join("capture.jpg")).unwrap(),
    b"preserve this"
  );
}

#[tokio::test]
async fn clipboard_failure_retains_the_saved_image_and_reports_saved_instead_of_copied() {
  let dir = tempfile::tempdir().unwrap();
  let app = app(true);
  let config = Config::current(&app);
  let titles = crate::i18n::screenshot_notif_titles("en");
  let path = dir.path().join("nested/capture.png");
  let response = copy_rgba_and_maybe_save_with(
    &app,
    result(path.clone()),
    None,
    &config,
    "fixture",
    &titles,
    |_, _| async { Err("clipboard locked".into()) },
  )
  .await
  .unwrap();
  assert!(response.success && response.saved == Some(true));
  assert_eq!(
    image::open(&path).unwrap().to_rgba8().as_raw(),
    &[10u8, 20, 30, 255].repeat(4)
  );
  let requests = notices(&app);
  assert_eq!(requests.len(), 1);
  assert_eq!(requests[0].title, titles.saved);
  assert!(requests[0].body.contains(path.to_str().unwrap()));
}

#[tokio::test]
async fn save_failure_never_calls_clipboard_or_reports_success_and_bypasses_notification_preferences()
 {
  let dir = tempfile::tempdir().unwrap();
  let blocker = dir.path().join("not-a-directory");
  std::fs::write(&blocker, b"existing data").unwrap();
  for invalid_pixels in [false, true] {
    let app = app(false);
    let config = Config::current(&app);
    let titles = crate::i18n::screenshot_notif_titles("en");
    let path = if invalid_pixels {
      dir.path().join("invalid.png")
    } else {
      blocker.join("capture.png")
    };
    let mut input = result(path.clone());
    if invalid_pixels {
      input.rgba = vec![0; 3];
    }
    let error = copy_rgba_and_maybe_save_with(
      &app,
      input,
      None,
      &config,
      "fixture",
      &titles,
      |_, _| async { panic!("failed save must never publish clipboard data") },
    )
    .await
    .err()
    .unwrap();
    assert!(!error.is_empty());
    assert!(!path.exists());
    assert_eq!(std::fs::read(&blocker).unwrap(), b"existing data");
    let requests = notices(&app);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].critical);
    assert_eq!(requests[0].title, titles.save_failed);
  }
}

#[tokio::test]
async fn unavailable_clipboard_service_keeps_real_saved_capture_without_visible_notification() {
  let dir = tempfile::tempdir().unwrap();
  let app = app(false);
  let config = Config::current(&app);
  let path = dir.path().join("capture.png");
  let response = handle_capture_result(&app, result(path.clone()), &config, "fixture")
    .await
    .unwrap();
  assert!(response.success && path.exists());
  let requests = notices(&app);
  assert_eq!(requests.len(), 1);
  assert!(!requests[0].critical);
  assert!(!requests[0].should_display(false));
}
