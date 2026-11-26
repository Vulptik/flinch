use super::*;
use crate::overlay::selection::Selection;

fn app() -> AppHandle {
  AppHandle::isolated(
    crate::config::Config {
      language: "en".into(),
      ..Default::default()
    },
    PathBuf::new(),
  )
}
fn pixels() -> Vec<u8> {
  (0..12).flat_map(|n| [n, n + 20, n + 40, 255]).collect()
}
fn chosen() -> annotations::AnnotatedSelection {
  annotations::AnnotatedSelection {
    selection: Selection {
      x: 1,
      y: 1,
      width: 2,
      height: 2,
      source_app: Some("fixture".into()),
    },
    annotations: vec![],
    pin: false,
  }
}

#[tokio::test]
async fn stale_selection_bounds_return_an_error_instead_of_panicking_a_capture_worker() {
  let worker = tokio::spawn(async {
    let dir = tempfile::tempdir().unwrap();
    let shot = Screenshot::new(dir.path().into(), "capture".into());
    shot
      .capture_with(
        &app(),
        "png",
        90,
        || Ok((0, 0, 4, 3, pixels())),
        |_, _| async {
          let mut selection = chosen();
          selection.selection.x = 4;
          selection.selection.width = 5;
          Ok(Some(selection))
        },
      )
      .await
      .map(|_| ())
  });
  assert!(
    worker
      .await
      .expect("invalid crop must not panic the worker")
      .is_err()
  );
}

#[tokio::test]
async fn area_capture_crops_the_exact_frozen_pixels_and_preserves_metadata() {
  let dir = tempfile::tempdir().unwrap();
  let shot = Screenshot::new(dir.path().into(), "capture".into());
  for (format, ext) in [
    ("png", "png"),
    ("jpg", "jpg"),
    ("jpeg", "jpg"),
    ("webp", "webp"),
  ] {
    let result = shot
      .capture_with(
        &app(),
        format,
        83,
        || Ok((-300, -100, 4, 3, pixels())),
        |input, language| async move {
          assert_eq!(
            (
              input.width,
              input.height,
              input.vd_offset_x,
              input.vd_offset_y
            ),
            (4, 3, -300, -100)
          );
          assert_eq!(input.rgba.as_slice(), pixels());
          assert_eq!(input.quality, 83);
          assert_eq!(language.as_deref(), Some("en"));
          Ok(Some(chosen()))
        },
      )
      .await
      .unwrap();
    let expected: Vec<u8> = [5u8, 6, 9, 10]
      .into_iter()
      .flat_map(|n| [n, n + 20, n + 40, 255])
      .collect();
    assert_eq!(result.rgba, expected);
    assert_eq!((result.width, result.height, result.quality), (2, 2, 83));
    assert_eq!(result.source_app.as_deref(), Some("fixture"));
    assert_eq!(result.path, dir.path().join(format!("capture.{ext}")));
    assert!(
      !result.path.exists(),
      "capture must not publish before post-capture saving"
    );
  }
}

#[tokio::test]
async fn area_cancellation_and_overlay_failures_preserve_typed_causes_without_saving() {
  let dir = tempfile::tempdir().unwrap();
  let shot = Screenshot::new(dir.path().into(), "capture".into());
  let cancelled = shot
    .capture_with(
      &app(),
      "png",
      90,
      || Ok((0, 0, 4, 3, pixels())),
      |_, _| async { Ok(None) },
    )
    .await
    .err()
    .unwrap();
  assert!(matches!(
    cancelled.downcast_ref::<ScreenshotError>(),
    Some(ScreenshotError::Cancelled)
  ));
  let failed = shot
    .capture_with(
      &app(),
      "png",
      90,
      || Ok((0, 0, 4, 3, pixels())),
      |_, _| async { Err(anyhow::anyhow!("native overlay failed")) },
    )
    .await
    .err()
    .unwrap();
  assert!(format!("{failed:#}").contains("native overlay failed"));
  let failed = shot
    .capture_with(
      &app(),
      "png",
      90,
      || Err(anyhow::anyhow!("capture denied")),
      |_, _| async { panic!("overlay must not open after a failed capture") },
    )
    .await
    .err()
    .unwrap();
  assert!(format!("{failed:#}").contains("capture denied"));
  assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn fullscreen_and_active_window_capture_preserve_pixels_paths_and_failure_causes() {
  let dir = tempfile::tempdir().unwrap();
  let shot = Screenshot::new(dir.path().into(), "capture".into());
  for format in ["png", "jpeg", "webp"] {
    let full = shot
      .capture_fullscreen_with(format, 97, || Ok((-300, -100, 4, 3, pixels())))
      .await
      .unwrap();
    assert_eq!(full.rgba, pixels());
    assert_eq!((full.width, full.height, full.quality), (4, 3, 97));
    assert!(full.source_app.is_none());
    let window = shot
      .capture_active_window_with(format, 86, Some("editor".into()), || Ok((4, 3, pixels())))
      .await
      .unwrap();
    assert_eq!(window.rgba, pixels());
    assert_eq!(window.source_app.as_deref(), Some("editor"));
    assert_eq!(window.quality, 86);
  }
  let error = shot
    .capture_fullscreen_with("png", 90, || Err(anyhow::anyhow!("desktop unavailable")))
    .await
    .err()
    .unwrap();
  assert!(format!("{error:#}").contains("desktop unavailable"));
  let error = shot
    .capture_active_window_with("png", 90, None, || {
      Err(anyhow::anyhow!("window unavailable"))
    })
    .await
    .err()
    .unwrap();
  assert!(format!("{error:#}").contains("window unavailable"));
  assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn a_picked_pixel_is_the_single_frozen_pixel_on_a_negative_origin_desktop() {
  let shot = Screenshot::new(PathBuf::new(), String::new());
  let result = shot
    .capture_with(
      &app(),
      "png",
      100,
      || Ok((-1920, -200, 4, 3, pixels())),
      |_, _| async {
        Ok(Some(annotations::AnnotatedSelection {
          selection: Selection {
            x: 3,
            y: 2,
            width: 1,
            height: 1,
            source_app: None,
          },
          annotations: vec![],
          pin: false,
        }))
      },
    )
    .await
    .unwrap();
  assert_eq!((result.width, result.height), (1, 1));
  assert_eq!(result.rgba, [11, 31, 51, 255]);
}

/// Pinning (F3) keeps the crop and says where on the desktop it was, so the pin opens in place.
#[tokio::test]
async fn a_pinned_selection_carries_its_desktop_position() {
  let shot = Screenshot::new(PathBuf::new(), String::new());
  let result = shot
    .capture_with(
      &app(),
      "png",
      100,
      || Ok((-1920, -200, 4, 3, pixels())),
      |_, _| async {
        Ok(Some(annotations::AnnotatedSelection {
          pin: true,
          ..chosen()
        }))
      },
    )
    .await
    .unwrap();
  assert_eq!(result.pin, Some((-1919, -199)));
  assert_eq!((result.width, result.height), (2, 2));
}

fn last(x: i32, y: i32, width: u32, height: u32) -> crate::last_region::Region {
  crate::last_region::Region {
    x,
    y,
    width,
    height,
  }
}

#[tokio::test]
async fn the_last_region_is_cropped_from_the_frozen_desktop_without_the_overlay() {
  let dir = tempfile::tempdir().unwrap();
  let shot = Screenshot::new(dir.path().into(), "capture".into());
  // The desktop starts left of the primary monitor; the region is in virtual-screen pixels.
  let result = shot
    .capture_last_region_with(
      &app(),
      "png",
      90,
      Some(last(-299, -99, 2, 2)),
      || Ok((-300, -100, 4, 3, pixels())),
      |center_x, center_y| {
        assert_eq!((center_x, center_y), (-298, -98));
        Some("Game".into())
      },
      |_, _| async { panic!("a usable last region must not open the overlay") },
    )
    .await
    .unwrap();
  let expected: Vec<u8> = [5u8, 6, 9, 10]
    .into_iter()
    .flat_map(|n| [n, n + 20, n + 40, 255])
    .collect();
  assert_eq!(result.rgba, expected);
  assert_eq!((result.width, result.height), (2, 2));
  assert_eq!(result.source_app.as_deref(), Some("Game"));
}

#[tokio::test]
async fn without_a_fitting_last_region_the_overlay_opens_and_its_pick_is_remembered() {
  let dir = tempfile::tempdir().unwrap();
  let shot = Screenshot::new(dir.path().into(), "capture".into());
  let settings = tempfile::tempdir().unwrap();
  let app = app();
  app.isolate_config_path(settings.path().join("config.json"));
  // Nothing remembered yet, then an area that no longer fits the (smaller) desktop.
  for region in [None, Some(last(3, 0, 2, 2))] {
    let opened = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = opened.clone();
    let result = shot
      .capture_last_region_with(
        &app,
        "png",
        90,
        region,
        || Ok((-300, -100, 4, 3, pixels())),
        |_, _| panic!("no window lookup without a replayed region"),
        move |input, _| {
          seen.store(true, std::sync::atomic::Ordering::SeqCst);
          assert_eq!((input.vd_offset_x, input.vd_offset_y), (-300, -100));
          async { Ok(Some(chosen())) }
        },
      )
      .await
      .unwrap();
    assert!(opened.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!((result.width, result.height), (2, 2));
    assert_eq!(result.source_app.as_deref(), Some("fixture"));
  }
  // The overlay's pick (1,1 inside a desktop at -300,-100) is the next last region, when the
  // monitor layout is unchanged.
  let stored: serde_json::Value = serde_json::from_slice(
    &std::fs::read(settings.path().join("last-region.json")).expect("pick remembered"),
  )
  .unwrap();
  assert_eq!(
    stored["region"],
    serde_json::json!({"x": -299, "y": -99, "width": 2, "height": 2})
  );
}

#[tokio::test]
async fn a_cancelled_fallback_overlay_keeps_the_previous_region() {
  let shot = Screenshot::new(PathBuf::new(), String::new());
  let settings = tempfile::tempdir().unwrap();
  let app = app();
  app.isolate_config_path(settings.path().join("config.json"));
  let cancelled = shot
    .capture_last_region_with(
      &app,
      "png",
      90,
      None,
      || Ok((0, 0, 4, 3, pixels())),
      |_, _| None,
      |_, _| async { Ok(None) },
    )
    .await
    .err()
    .unwrap();
  assert!(matches!(
    cancelled.downcast_ref::<ScreenshotError>(),
    Some(ScreenshotError::Cancelled)
  ));
  assert!(!settings.path().join("last-region.json").exists());
}
